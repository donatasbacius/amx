use crate::hub::Hub;
use crate::relay::{Choice, FAST_MS, Finding, LogEntry, Model, Ms, OnlineAgent, Phase, Review, Role, Severity, Slot, State, Status, Verdict, rereview};
use crate::text::{bidi, dur, iso, phase_badge, short_sha, split_range, split_rates};
use axum::extract::{Form, Path, Query, State as AxState};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::{Extension, Json};
use crate::server::{Owner, Viewer};
use crate::tailscale::Identity;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write;
use std::sync::Arc;

/// Line icons on a 16px grid, sized and coloured by the text around them (see `.i` in the stylesheet).
macro_rules! icon {
    ($body:literal) => {
        concat!("<svg class=\"i\" viewBox=\"0 0 16 16\" aria-hidden=\"true\">", $body, "</svg>")
    };
}
const CHEVRON: &str = icon!("<path d=\"M6 3.5L10.5 8 6 12.5\"/>");
const EXTERNAL: &str = icon!("<path d=\"M6 3.5h6.5V10M12.5 3.5l-9 9\"/>");
const PERSON: &str = icon!("<circle cx=\"8\" cy=\"5.25\" r=\"2.75\"/><path d=\"M2.75 14c.6-2.9 2.6-4.5 5.25-4.5s4.65 1.6 5.25 4.5\"/>");
const MACHINE: &str = icon!("<rect x=\"1.75\" y=\"2.75\" width=\"12.5\" height=\"8.5\" rx=\"1.5\"/><path d=\"M5.5 14h5M8 11.25V14\"/>");

pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c if bidi(c) => {}
            _ => out.push(c),
        }
    }
    out
}

/// Markdown from agents: raw HTML is shown as text, images become links (so viewing a page
/// never loads anything from elsewhere), links get rel=noopener, only http(s) URLs. Single
/// newlines are kept as line breaks, as agents write plain text.
pub fn markdown(src: &str) -> String {
    use pulldown_cmark::{Event, LinkType, Parser, Tag, TagEnd, html};
    let mut events = Vec::new();
    let mut image = None;
    let src: String = src.chars().filter(|&c| !bidi(c)).collect();
    for e in Parser::new_ext(&src, options()) {
        match e {
            Event::Html(s) | Event::InlineHtml(s) => events.push(Event::Text(s)),
            Event::SoftBreak => events.push(Event::HardBreak),
            Event::Start(Tag::Image { dest_url, title, id, .. }) => {
                image = Some((events.len(), dest_url.clone()));
                events.push(Event::Start(Tag::Link {
                    link_type: LinkType::Inline,
                    dest_url,
                    title,
                    id,
                }));
            }
            Event::End(TagEnd::Image) => {
                if let Some((start, url)) = image.take()
                    && events.len() == start + 1
                {
                    events.push(Event::Text(url));
                }
                events.push(Event::End(TagEnd::Link));
            }
            e => events.push(e),
        }
    }
    let mut raw = String::new();
    html::push_html(&mut raw, events.into_iter());
    ammonia::Builder::default()
        .rm_tags(["img"])
        .url_schemes(["http", "https"].into())
        .link_rel(Some("noopener noreferrer nofollow"))
        .clean(&raw)
        .to_string()
}

fn options() -> pulldown_cmark::Options {
    pulldown_cmark::Options::ENABLE_TABLES | pulldown_cmark::Options::ENABLE_STRIKETHROUGH
}

fn safe_url(u: &str) -> Option<&str> {
    u.starts_with("https://github.com/").then_some(u)
}

/// "14:46 UTC", or "15 Oct 14:46 UTC" on another day than `now`: about as wide as what the page
/// script turns it into, so nothing moves when it runs. The page changes only when the review
/// does, and once a day.
fn utc(ms: Ms, now: Ms) -> String {
    let (iso, today) = (iso(ms), iso(now));
    if iso[..10] == today[..10] {
        return format!("{} UTC", &iso[11..16]);
    }
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let month = iso[5..7].parse::<usize>().ok().and_then(|m| month.get(m.wrapping_sub(1))).unwrap_or(&"");
    format!("{} {month} {} UTC", iso[8..10].trim_start_matches('0'), &iso[11..16])
}

/// Made relative by the page script.
fn time_tag(ms: Ms, now: Ms) -> String {
    format!("<time datetime=\"{}\">{}</time>", iso(ms).replace(' ', "T"), utc(ms, now))
}

/// "a · b · c" with each part and its trailing dot in one span, so a narrow screen wraps between parts.
fn dotted(parts: &[String]) -> String {
    let last = parts.len().saturating_sub(1);
    let spans: Vec<_> = parts
        .iter()
        .enumerate()
        .map(|(i, p)| format!("<span>{p}{}</span>", if i < last { " ·" } else { "" }))
        .collect();
    spans.join(" ")
}

/// How close a queued review's failure is before its time turns amber.
const SOON_MS: Ms = 15 * 60 * 1000;

/// `m` minutes, at least one, as "35m" or "1h28m".
fn mins(m: Ms) -> String {
    match m.max(1) {
        m @ ..60 => format!("{m}m"),
        m => format!("{}h{:02}m", m / 60, m % 60),
    }
}

/// Rounded as the page script rounds `<time>` tags, so it never reads "1m" beside "just now".
fn queued(r: &Review, now: Ms) -> String {
    match now.saturating_sub(r.created) {
        ..45_000 => "queued just now".into(),
        ms => format!("queued {}", mins((ms + 30_000) / 60_000)),
    }
}

/// How long a queued review has waited, or how soon it fails once that is near; a running
/// review's deadline, counted down by the page script; else when it ended.
fn when(r: &Review, now: Ms) -> String {
    match r.phase {
        Phase::Offering if r.deadline_left(now) <= SOON_MS => format!("<span class=\"needs\">fails in {}</span>", mins(r.deadline_left(now).div_ceil(60_000))),
        Phase::Offering => queued(r, now),
        p if p.active() => format!("<span data-deadline=\"{}\">until {}</span>", r.deadline, utc(r.deadline, now)),
        _ => clock_at(r.ended.unwrap_or(r.created), now),
    }
}

/// The viewer's local time, from the page script, as `utc` words it until then.
fn clock_at(ms: Ms, now: Ms) -> String {
    format!("<time class=\"clock\" datetime=\"{}\">{}</time>", iso(ms).replace(' ', "T"), utc(ms, now))
}

/// The review's state as a class and label. An approved review isn't "Approved" while it has
/// findings for someone to decide, unless a re-review took them over (`locked`); `can`: the
/// viewer may decide them.
fn status(r: &Review, locked: bool, can: bool) -> (&'static str, String) {
    match r.phase {
        Phase::Offering => ("queued", "Queued".into()),
        Phase::Reviewing => ("inreview", "Reviewing".into()),
        Phase::Voting(_) if r.replying => ("voting", "Replying".into()),
        Phase::Voting(1) => ("voting", "Voting".into()),
        Phase::Voting(n) => ("voting", format!("Voting, round {n}")),
        Phase::Done if r.verdict() == Some(Verdict::Approve) && !locked && to_do(r).1 > 0 => {
            ("judge", if can { "Decide" } else { "Unresolved" }.into())
        }
        Phase::Done if r.verdict() == Some(Verdict::Approve) => ("approved", "Approved".into()),
        Phase::Done => ("changes", "Changes requested".into()),
        Phase::Failed if r.cancelled_by.is_some() => ("failed", "Cancelled".into()),
        Phase::Failed => ("failed", "Failed".into()),
    }
}

/// Who the page is shown to, in its header: outside `main`, so polling never touches it.
fn viewer_tag(v: &Viewer) -> String {
    match v {
        Viewer::Login(login) => format!("<span class=\"me\">{PERSON} <span class=\"sr\">Viewing as </span>{}</span>", esc(login)),
        Viewer::ThisMachine => format!("<span class=\"me\">{MACHINE} <span class=\"sr\">Viewing on </span>This machine</span>"),
        Viewer::Unknown => format!("<span class=\"me muted\">{PERSON} Not identified</span>"),
    }
}

pub const STYLE: &str = include_str!("../assets/style.css");
pub const SCRIPT: &str = include_str!("../assets/app.js");
const STYLE_V: u32 = version(STYLE.as_bytes());
const SCRIPT_V: u32 = version(SCRIPT.as_bytes());

/// FNV-1a of an asset, put in its URL so browsers can keep it until it changes.
const fn version(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    let mut i = 0;
    while i < bytes.len() {
        h = (h ^ bytes[i] as u32).wrapping_mul(0x01000193);
        i += 1;
    }
    h
}

/// `live` pages poll for changes and show a line while they can't; others load no script.
fn page(title: &str, main: &str, live: bool, viewer: &Viewer) -> String {
    let (script, status, tag) = if live {
        (
            format!("<script src=\"/static/app.js?v={SCRIPT_V:08x}\" defer></script>\n"),
            "<span class=\"live\" role=\"status\" aria-atomic=\"false\"><span id=\"live\"></span><span id=\"said\" class=\"sr\"></span></span>",
            format!(" data-etag='{}'", etag(main)),
        )
    } else {
        Default::default()
    };
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light dark">
<title>{title}</title>
<link rel="stylesheet" href="/static/style.css?v={STYLE_V:08x}">
{script}</head>
<body>
<a class="skip" href="#main">Skip to content</a>
<header class="top"><a class="brand" href="/">amx</a>{status}{me}</header>
<main id="main"{tag}>{main}</main>
</body>
</html>"##,
        title = esc(title),
        me = viewer_tag(viewer),
    )
}

/// The outcome of a finding as a badge class and label, or `None` while it is still open.
fn outcome(f: &Finding) -> Option<(&'static str, String)> {
    let Some(o) = &f.outcome else {
        return f.withdrawn.is_some().then(|| ("closed", "withdrawn".to_string()));
    };
    if let Some(x) = f.ruled() {
        return Some(if x.must_fix { ("changes", "must fix".into()) } else { ("closed", "dismissed".into()) });
    }
    let carried = f.by.is_none();
    Some(match &o.status {
        Status::Confirmed if carried => ("present", "still present".to_string()),
        Status::Confirmed => ("changes", "confirmed".to_string()),
        Status::Rejected if carried => ("approved", "resolved".to_string()),
        Status::Rejected => ("closed", "rejected".to_string()),
        Status::Disputed => ("stale", "disputed".to_string()),
        Status::Unverified => ("stale", "unverified".to_string()),
        Status::Duplicate(t) => ("closed", format!("duplicate of {t}")),
        Status::Withdrawn => ("closed", "withdrawn".to_string()),
    })
}

fn votes(f: &Finding) -> (usize, usize) {
    match &f.outcome {
        Some(o) => (o.agree, o.disagree),
        None => (
            f.current_votes().filter(|(_, v)| v.choice != Choice::Disagree).count(),
            f.current_votes().filter(|(_, v)| v.choice == Choice::Disagree).count(),
        ),
    }
}

/// The first line of a finding's title or text as plain text.
pub fn title(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let line = line.trim_start_matches(['#', '>', '-', '*', ' ']).replace(['*', '`'], "");
    match line.char_indices().nth(100) {
        Some((i, _)) => format!("{}…", line[..i].trim_end()),
        None => line,
    }
}

/// The findings anyone may see now: carried-over ones always, the rest once every reviewer has submitted.
fn visible(r: &Review) -> impl Iterator<Item = &Finding> {
    r.findings.iter().filter(|f| r.revealed() || f.by.is_none())
}

#[derive(Debug, Clone, Serialize)]
pub struct Stage {
    pub name: String,
    /// `done`, `active`, `pending` or `skipped`.
    pub state: &'static str,
    pub start: Option<Ms>,
    pub end: Option<Ms>,
    pub deadline: Option<Ms>,
    pub notes: Vec<String>,
}

fn stage(name: impl Into<String>, state: &'static str, start: Option<Ms>) -> Stage {
    Stage {
        name: name.into(),
        state,
        start,
        end: None,
        deadline: None,
        notes: Vec::new(),
    }
}

/// Offer, independent review, each cross-examination round, then the result, from the review's log.
pub fn timeline(r: &Review) -> Vec<Stage> {
    let starts: Vec<Ms> = r.log.iter().filter(|e| e.kind == "phase").map(|e| e.at).collect();
    let finished = r.ended.or_else(|| r.log.iter().find(|e| e.kind == "result" || e.kind == "failed").map(|e| e.at));
    let mut out = vec![stage("Offer", "done", Some(r.created))];
    match starts.first() {
        Some(&at) => out.push(stage("Independent review", "done", Some(at))),
        None => out.push(stage("Independent review", "pending", None)),
    }
    for (i, &at) in starts.iter().enumerate().skip(1) {
        out.push(stage(format!("Cross-examination round {i}"), "done", Some(at)));
    }
    if starts.len() < 2 {
        out.push(stage("Cross-examination", "pending", None));
    }
    let last = match r.phase {
        Phase::Failed => stage(status(r, false, false).1, "done", finished),
        Phase::Done => stage("Consensus result", "done", finished),
        _ => stage("Consensus result", "pending", None),
    };
    out.push(last);

    let active = match r.phase {
        Phase::Offering => Some(0),
        Phase::Reviewing => Some(1),
        Phase::Voting(_) => out.iter().rposition(|s| s.start.is_some()),
        Phase::Done | Phase::Failed => None,
    };
    let n = out.len();
    for i in 0..n {
        let next = out[i + 1..].iter().find_map(|s| s.start);
        if out[i].start.is_some() {
            out[i].end = next.or(finished);
        } else if !r.phase.active() && i + 1 < n {
            out[i].state = "skipped";
        }
    }
    if let Some(i) = active {
        out[i].state = "active";
        out[i].end = None;
        out[i].deadline = Some(r.deadline);
    }
    for e in r.log.iter().filter(|e| e.kind == "drop") {
        if let Some(s) = out
            .iter_mut()
            .rev()
            .find(|s| s.start.is_some_and(|st| st <= e.at) && s.end.is_none_or(|end| e.at <= end))
        {
            s.notes.push(format!("{} {}", e.who, e.text));
        }
    }
    out
}

/// "a", "a and b", "a, b and c".
fn names<S: AsRef<str>>(v: &[S]) -> String {
    match v {
        [] => String::new(),
        [a] => a.as_ref().to_string(),
        [rest @ .., last] => {
            let rest: Vec<&str> = rest.iter().map(AsRef::as_ref).collect();
            format!("{} and {}", rest.join(", "), last.as_ref())
        }
    }
}

/// Who the review was offered to, as a sentence.
fn asked(r: &Review) -> String {
    let asked: Vec<&String> = r.offered.iter().filter(|a| !r.declined.contains(*a)).collect();
    let declined: Vec<&String> = r.declined.iter().collect();
    match (asked.is_empty(), declined.is_empty()) {
        (true, true) => "Nobody free to ask yet.".into(),
        (false, true) => format!("Asked {}.", names(&asked)),
        (true, false) => format!("{} declined.", names(&declined)),
        (false, false) => format!("Asked {}; {} declined.", names(&asked), names(&declined)),
    }
}

/// Why a review failed, for its index row and its page, from what happened rather than from the
/// relay's message to agents.
fn failure(r: &Review) -> (String, String) {
    let who: Vec<&String> = r.slots.iter().map(|s| &s.agent).collect();
    if let Some(by) = &r.cancelled_by {
        let by = if by == "the hub owner" { "The hub owner" } else { by };
        ("cancelled".into(), format!("{by} cancelled it."))
    } else if r.failure.as_deref().is_some_and(|f| f.starts_with("no full team")) {
        let n = r.params.reviewers;
        let mix = if n >= 2 { ", claude and codex," } else { "" };
        let within = span(r.params.queue_secs);
        (format!("no team within {within}"), format!("No team of {}{mix} joined within {within}.", plural(&n.to_string(), "reviewer")))
    } else if let Some(m) = r.missing.map(|m| m.as_str()) {
        let why = format!("No {m} reviewer accepted in time; every review needs both claude and codex. {} accepted.", names(&who));
        (format!("no {m} reviewer"), why)
    } else if let Some(m) = r.missing_model().filter(|_| r.active_slots().next().is_some()).map(|m| m.as_str()) {
        let gone: Vec<&String> = r.slots.iter().filter(|s| s.dropped.is_some()).map(|s| &s.agent).collect();
        (format!("no {m} reviewer left"), format!("No {m} reviewer was left to finish it, so it gave no verdict. Dropped: {}.", names(&gone)))
    } else if !who.is_empty() {
        ("reviewers dropped out".into(), format!("All reviewers dropped out: {}.", names(&who)))
    } else if r.offered.is_empty() {
        ("no reviewers free".into(), "No reviewers were free.".into())
    } else {
        ("nobody accepted".into(), format!("Nobody accepted in time. {}", asked(r)))
    }
}

/// "2 hours" or "90 minutes".
fn span(secs: u64) -> String {
    if secs.is_multiple_of(3600) { plural(&(secs / 3600).to_string(), "hour") } else { plural(&secs.div_ceil(60).to_string(), "minute") }
}

/// The program a reviewer runs in, after its name.
fn program(m: Option<Model>) -> String {
    m.map(|m| format!(" <span class=\"pg\">{}</span>", m.as_str())).unwrap_or_default()
}

/// Confirmed findings that block, those the reviewers left undecided, and the other confirmed ones.
fn to_do(r: &Review) -> (usize, usize, usize) {
    let (mut fix, mut decide, mut optional) = (0, 0, 0);
    for f in &r.findings {
        let Some(o) = &f.outcome else { continue };
        match (&o.status, o.severity) {
            _ if f.ruled().is_some_and(|x| x.must_fix) => fix += 1,
            _ if f.ruled().is_some() => {}
            (Status::Confirmed, s) if r.blocks(s) => fix += 1,
            (Status::Confirmed, _) => optional += 1,
            (Status::Disputed | Status::Unverified, _) => decide += 1,
            _ => {}
        }
    }
    (fix, decide, optional)
}

/// Reviewers whose contested findings still wait for their reply.
fn repliers(r: &Review) -> Vec<&String> {
    r.active_slots()
        .filter(|s| !r.replied.contains(&s.agent) && !r.contested_by(&s.agent).is_empty())
        .map(|s| &s.agent)
        .collect()
}

/// A PR on the index: its newest review, the newest finished one shown while that runs, and how
/// many reviews it has had.
struct Pr<'a> {
    r: &'a Review,
    last: Option<(&'a Review, bool)>,
    locked: bool,
    n: usize,
}

/// One per PR, newest request first, so a row keeps its place whatever happens to its review.
fn prs(st: &State) -> Vec<Pr<'_>> {
    let mut latest: Vec<&Review> = st.reviews.iter().collect();
    latest.sort_by_key(|r| std::cmp::Reverse((r.created, r.seq())));
    let mut seen = HashSet::new();
    latest.retain(|r| seen.insert(&r.pr));
    latest
        .into_iter()
        .map(|r| {
            let all = st.reviews.iter().filter(|x| x.pr == r.pr);
            Pr {
                r,
                last: all.clone().filter(|x| x.phase == Phase::Done).max_by_key(|x| x.ended).map(|x| (x, rereview(&st.reviews, &x.id).is_some())),
                locked: rereview(&st.reviews, &r.id).is_some(),
                n: all.count(),
            }
        })
        .collect()
}

/// Whether an online reviewer running `m` is idle.
fn idle(online: &[OnlineAgent], m: Model) -> bool {
    online.iter().any(|a| a.role == Role::Reviewer && a.model == Some(m) && a.busy.is_none())
}

/// A program a queued review of two or more reviewers still needs while none of its reviewers is idle.
fn none_idle(r: &Review, online: &[OnlineAgent]) -> Option<Model> {
    let needs = |m: &Model| r.phase == Phase::Offering && r.params.reviewers >= 2 && !r.active_slots().any(|s| s.model == Some(*m));
    Model::ALL.into_iter().find(|m| needs(m) && !idle(online, *m))
}

/// Status, title and time, then the PR, the gist and who asked; `mine` words a call as the
/// viewer's, `may` adds that the viewer can rule on someone else's.
fn review_row(out: &mut String, p: &Pr, mine: bool, may: bool, online: &[OnlineAgent], now: Ms) {
    let r = p.r;
    let (cls, label) = status(r, p.locked, mine);
    let mut meta = vec![esc(&r.pr.short()), esc(&gist(r, mine, p.locked))];
    if let Some(m) = none_idle(r, online) {
        meta.push(format!("no {} reviewer idle", m.as_str()));
    } else if let Some(m) = r.missing_model().filter(|_| r.phase == Phase::Offering && r.active_slots().count() > 0) {
        meta.push(format!("needs a {} reviewer", m.as_str()));
    }
    if let Some((l, locked)) = p.last.filter(|_| r.phase != Phase::Done) {
        meta.push(format!("last: {}", status(l, locked, mine).1.to_lowercase()));
    }
    if p.n > 1 {
        meta.push(format!("{} reviews", p.n));
    }
    meta.push(esc(&r.author));
    if cls == "judge" && may && !mine {
        meta.push("you can rule".into());
    }
    let _ = write!(
        out,
        "<li class=\"rv{call}\"><span class=\"st s-{cls}\">{label}</span><a class=\"rv-pr\" href=\"/r/{id}\" dir=\"auto\" title=\"{pr}\">{pr}<span class=\"sr\">, {label}</span></a>\
         <span class=\"rv-when\">{}</span><span class=\"rv-meta\">{meta}</span></li>",
        when(r, now),
        call = if cls == "judge" && mine { " call" } else { "" },
        id = esc(&r.id),
        pr = esc(&pr_name(r)),
        meta = dotted(&meta),
    );
}
/// The PR's title, else "Pull request #123" under its `owner/repo#123`.
fn pr_name(r: &Review) -> String {
    r.title.clone().unwrap_or_else(|| format!("Pull request #{}", r.pr.number))
}

/// The PR's title cut at a word to fit a browser tab, else its short form.
fn tab_name(r: &Review) -> String {
    match &r.title {
        Some(t) if t.chars().count() > 60 => {
            let cut: String = t.chars().take(56).collect();
            format!("{}…", cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head).trim_end())
        }
        Some(t) => t.clone(),
        None => r.pr.short(),
    }
}

/// Whether online agent `a` waits in a queued review's forming team.
fn in_team(st: &State, a: &OnlineAgent) -> bool {
    a.busy.as_deref().and_then(|id| st.review(id)).is_some_and(|r| r.phase == Phase::Offering)
}

/// The parts of "10 reviewers idle · 1 in a team · 6 reviewing · 2 authors" that aren't zero.
fn availability(st: &State, online: &[OnlineAgent]) -> Vec<String> {
    let reviewers = online.iter().filter(|a| a.role == Role::Reviewer);
    let idle = reviewers.clone().filter(|a| a.busy.is_none()).count();
    let team = reviewers.clone().filter(|a| in_team(st, a)).count();
    let reviewing = reviewers.filter(|a| a.busy.is_some()).count() - team;
    let authors = online.len() - idle - team - reviewing;
    let plural = |n: usize, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
    let parts = [
        (idle, plural(idle, "reviewer") + " idle"),
        (team, format!("{team} in a team")),
        (reviewing, format!("{reviewing} reviewing")),
        (authors, plural(authors, "author")),
    ];
    parts.into_iter().filter(|(n, _)| *n > 0).map(|(_, s)| s).collect()
}

/// Each program that reviews waiting for reviewers still lack (one of two or more needs both)
/// while none of its reviewers is idle, with those reviews.
fn short_of<'a>(st: &'a State, online: &[OnlineAgent]) -> Vec<(Model, Vec<&'a Review>)> {
    Model::ALL
        .into_iter()
        .filter(|m| !idle(online, *m))
        .map(|m| {
            let waiting = st.reviews.iter().filter(|r| r.phase == Phase::Offering && r.params.reviewers >= 2 && !r.active_slots().any(|s| s.model == Some(m)));
            (m, waiting.collect::<Vec<_>>())
        })
        .filter(|(_, waiting)| !waiting.is_empty())
        .collect()
}

/// Who is online, reviewers by program first, and what each program is short of.
fn agents_panel(out: &mut String, st: &State, online: &[OnlineAgent], short: &[(Model, Vec<&Review>)]) {
    let _ = write!(out, "<section id=\"agents\" class=\"panel\"><h2 class=\"ph\">Agents <span class=\"count\">{}</span></h2>", online.len());
    if online.is_empty() {
        out.push_str("<p class=\"empty\">No agents online.</p></section>");
        return;
    }
    out.push_str("<p class=\"prog\">");
    for m in Model::ALL {
        let of = online.iter().filter(|a| a.role == Role::Reviewer && a.model == Some(m));
        let idle = of.clone().filter(|a| a.busy.is_none()).count();
        let _ = write!(out, "<span><b>{}</b> {idle} idle, {} busy</span>", m.as_str(), of.count() - idle);
    }
    out.push_str("</p><ul class=\"agents\">");
    for a in online {
        let what = match (&a.busy, a.role) {
            (Some(id), _) => match st.review(id) {
                Some(r) if r.phase == Phase::Offering => format!("in a team for <a href=\"/r/{}\">{}#{}</a>", esc(&r.id), esc(&r.pr.repo), r.pr.number),
                Some(r) => format!("on <a href=\"/r/{}\">{}#{}</a>", esc(&r.id), esc(&r.pr.repo), r.pr.number),
                None => format!("on {}", esc(id)),
            },
            (None, Role::Reviewer) => "idle".into(),
            (None, Role::Author) => "author".into(),
        };
        let model = a.model.filter(|_| a.role == Role::Reviewer);
        let _ = write!(out, "<li><b>{}</b>{}<span class=\"role\">{what}</span></li>", esc(&a.name), program(model));
    }
    out.push_str("</ul>");
    for (m, waiting) in short {
        let links: Vec<String> = waiting.iter().map(|r| format!("<a href=\"/r/{}\">{}#{}</a>", esc(&r.id), esc(&r.pr.repo), r.pr.number)).collect();
        let verb = if links.len() == 1 { "waits" } else { "wait" };
        let _ = write!(out, "<p class=\"warn\">No {} reviewer is idle: {} {verb} for one.</p>", m.as_str(), names(&links));
    }
    out.push_str("</section>");
}

/// Rows on a page of the index.
const PER_PAGE: usize = 20;

/// Who the index is shown to, and what: `filter` is a `?f=` key, `page` a `?page=` value.
/// `may`: whether the viewer may act on a review; `mine`: whether its PR is theirs.
pub struct View<'a> {
    pub filter: Option<&'a str>,
    pub page: Option<&'a str>,
    pub connect: Option<String>,
    pub may: &'a dyn Fn(&Review) -> bool,
    pub mine: &'a dyn Fn(&Review) -> bool,
}

impl View<'_> {
    /// Every PR, for a viewer who may act on nothing.
    pub fn anyone() -> View<'static> {
        View { filter: None, page: None, connect: None, may: &|_| false, mine: &|_| false }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Filter {
    All,
    Call,
    Running,
    Queued,
    Finished,
}

impl Filter {
    const ALL: [Filter; 5] = [Filter::All, Filter::Call, Filter::Running, Filter::Queued, Filter::Finished];

    fn parse(key: Option<&str>) -> Filter {
        Filter::ALL.into_iter().find(|f| Some(f.parts().0) == key).unwrap_or(Filter::All)
    }

    /// Its `?f=` key and label.
    fn parts(self) -> (&'static str, &'static str) {
        match self {
            Filter::All => ("", "All"),
            Filter::Call => ("call", "Decide"),
            Filter::Running => ("running", "Running"),
            Filter::Queued => ("queued", "Queued"),
            Filter::Finished => ("finished", "Finished"),
        }
    }

    /// Whether PR `p` is in it; `mine`: whether the PR is the viewer's.
    fn has(self, p: &Pr, mine: bool) -> bool {
        match self {
            Filter::All => true,
            Filter::Call => mine && status(p.r, p.locked, true).0 == "judge",
            Filter::Running => matches!(p.r.phase, Phase::Reviewing | Phase::Voting(_)),
            Filter::Queued => p.r.phase == Phase::Offering,
            Filter::Finished => !p.r.phase.active(),
        }
    }

    /// Page `page` of the index with this filter.
    fn href(self, page: usize) -> String {
        let mut q: Vec<String> = Vec::new();
        if self != Filter::All {
            q.push(format!("f={}", self.parts().0));
        }
        if page > 1 {
            q.push(format!("page={page}"));
        }
        if q.is_empty() { "/".into() } else { format!("/?{}", q.join("&amp;")) }
    }
}

/// The page `asked` names, or the nearest of `pages`: the last for a number past it, the first
/// for anything that isn't a page number.
fn page_number(asked: Option<&str>, pages: usize) -> usize {
    let digits = asked.map(str::trim).filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    digits.map_or(1, |p| p.parse().unwrap_or(usize::MAX)).clamp(1, pages)
}

/// "‹ Newer · Page 2 of 5 · Older ›", with only the links that lead somewhere.
fn pager(filter: Filter, page: usize, pages: usize) -> String {
    let mut parts = Vec::new();
    if page > 1 {
        parts.push(format!("<a href=\"{}\" rel=\"prev\"><span aria-hidden=\"true\">‹ </span>Newer</a>", filter.href(page - 1)));
    }
    parts.push(format!("<span aria-current=\"page\">Page {page} of {pages}</span>"));
    if page < pages {
        parts.push(format!("<a href=\"{}\" rel=\"next\">Older<span aria-hidden=\"true\"> ›</span></a>", filter.href(page + 1)));
    }
    format!("<nav class=\"foot pager\" aria-label=\"Pages\">{}</nav>", parts.join("<span aria-hidden=\"true\">·</span>"))
}

fn status_key() -> String {
    format!(
        "<details id=\"key\" class=\"side-d\"><summary>{CHEVRON}What the statuses mean</summary><dl class=\"key\">\
         <dt><span class=\"st s-judge\">Decide</span></dt><dd>Approved, but some findings are disputed or unverified and wait for your ruling: Must fix or Dismiss on the review page.</dd>\
         <dt><span class=\"st s-judge\">Unresolved</span></dt><dd>Approved, but some findings are disputed or unverified and wait for the author's ruling.</dd>\
         <dt><span class=\"st s-changes\">Changes requested</span></dt><dd>The author's agent fixes and asks for a re-review.</dd>\
         <dt><span class=\"st s-failed\">Failed</span></dt><dd>No result; request it again.</dd>\
         <dt><span class=\"st s-queued\">Queued</span></dt><dd>Waiting for a full team of reviewers; it starts by itself.</dd>\
         <dt><span class=\"st s-voting\">Running</span></dt><dd>Review, then votes; times are deadlines.</dd></dl></details>"
    )
}

/// One row per PR, newest request first, under filters with their counts, beside the agents online.
pub fn render_index(st: &State, now: Ms, view: &View) -> String {
    index_page(st, now, view).0
}

/// The index and its tab title.
fn index_page(st: &State, now: Ms, view: &View) -> (String, String) {
    let prs = prs(st);
    let mut online = st.online(now);
    online.sort_by_key(|a| (a.role != Role::Reviewer, a.busy.is_none(), a.name.clone()));
    let short = short_of(st, &online);
    let filter = Filter::parse(view.filter);
    let has = |f: Filter, p: &Pr| f.has(p, (view.mine)(p.r));

    let mut out = String::from("<div class=\"cols\"><div class=\"list\">");
    let summary = if online.is_empty() {
        "No agents online".to_string()
    } else {
        let mut parts = availability(st, &online);
        parts.extend(short.iter().map(|(m, _)| format!("no {} idle", m.as_str())));
        format!("Agents: {}", dotted(&parts))
    };
    let _ = write!(out, "<a class=\"ag-sum\" href=\"#agents\">{summary}</a><h1 class=\"sr\">Pull requests</h1>");
    if prs.is_empty() {
        out.push_str("<p class=\"empty\">No reviews yet.</p>");
    } else {
        out.push_str("<nav class=\"filters\" aria-label=\"Filter\">");
        for f in Filter::ALL {
            let n = prs.iter().filter(|p| has(f, p)).count();
            if n == 0 && f != Filter::All && f != filter {
                continue;
            }
            let _ = write!(
                out,
                "<a{call} href=\"{}\"{current}>{} <span class=\"count\">{n}</span></a>",
                f.href(1),
                f.parts().1,
                call = if f == Filter::Call && n > 0 { " class=\"f-call\"" } else { "" },
                current = if f == filter { " aria-current=\"page\"" } else { "" },
            );
        }
        out.push_str("</nav>");
        let list: Vec<&Pr> = prs.iter().filter(|p| has(filter, p)).collect();
        let pages = list.len().div_ceil(PER_PAGE).max(1);
        let page = page_number(view.page, pages);
        if list.is_empty() {
            out.push_str("<p class=\"empty\">None right now.</p>");
        } else {
            out.push_str("<ul class=\"reviews\">");
            for p in list.iter().skip((page - 1) * PER_PAGE).take(PER_PAGE) {
                review_row(&mut out, p, (view.mine)(p.r), (view.may)(p.r), &online, now);
            }
            out.push_str("</ul>");
        }
        if pages > 1 {
            out.push_str(&pager(filter, page, pages));
        }
    }
    out.push_str("</div><aside class=\"side\">");
    agents_panel(&mut out, st, &online, &short);
    out.push_str(view.connect.as_deref().unwrap_or(""));
    out.push_str(&status_key());
    out.push_str("</aside></div>");
    (out, index_title(&prs))
}

/// The index's tab title: how many PRs have a review running and how many queued.
fn index_title(prs: &[Pr]) -> String {
    let mut parts = vec!["amx".to_string()];
    for f in [Filter::Running, Filter::Queued] {
        let n = prs.iter().filter(|p| f.has(p, false)).count();
        if n > 0 {
            parts.push(format!("{n} {}", f.parts().0));
        }
    }
    parts.join(" · ")
}

/// The connect commands for both clients; `/setup` has the rest.
fn connect(url: &str, login: &str, open: bool) -> String {
    format!(
        "<details id=\"connect\" class=\"side-d connect\"{open}><summary>{CHEVRON}Connect an agent</summary>\
         <h3>Claude Code</h3><pre><code>{add}</code></pre>\
         <h3>Codex</h3><p>In <code>~/.codex/config.toml</code>, replace any <code>[mcp_servers.amx]</code> table with:</p><pre><code>{toml}</code></pre>\
         <p>Agents are named <code>{claude}</code> and <code>{codex}</code>. <a href=\"/setup\">Full setup</a> covers reviewers, other names and a repo's own config.</p></details>",
        open = if open { " open" } else { "" },
        add = esc(&crate::text::claude_add(url)),
        toml = esc(&crate::text::codex_toml(url)),
        claude = esc(&crate::server::default_agent(login, "claude")),
        codex = esc(&crate::server::default_agent(login, "codex")),
    )
}

/// What a finding asks of the human, in page order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    /// Confirmed and blocking, or still present.
    Fix,
    /// Disputed, unverified or undecided, or still being voted on.
    Judge,
    /// Confirmed, not blocking.
    Optional,
    /// Carried over and voted down.
    Fixed,
    /// Rejected, duplicate or withdrawn.
    Closed,
}

fn rank(r: &Review, f: &Finding) -> Rank {
    if f.withdrawn.is_some() {
        return Rank::Closed;
    }
    match f.outcome.as_ref().map(|o| (&o.status, o.severity)) {
        Some((Status::Confirmed, s)) if r.blocks(s) => Rank::Fix,
        Some((Status::Confirmed, _)) => Rank::Optional,
        Some((Status::Disputed | Status::Unverified, _)) | None => Rank::Judge,
        Some((Status::Rejected, _)) if f.by.is_none() => Rank::Fixed,
        Some(_) => Rank::Closed,
    }
}

fn severity(f: &Finding) -> Severity {
    f.outcome.as_ref().map(|o| o.severity).unwrap_or(f.severity)
}

fn severity_note(f: &Finding) -> Option<String> {
    let o = f.outcome.as_ref()?;
    let rater = f.current_votes().find(|(_, v)| v.choice == Choice::Agree && v.severity.is_some_and(|s| s != f.severity));
    if let (Some(_), Some((who, v)), true) = (&f.by, rater, split_range(f).is_some()) {
        let rated = v.severity.unwrap_or(f.severity).as_str();
        return Some(format!("Raised as {}; {who} rated it {rated}, and the lower rating counts.", f.severity.as_str()));
    }
    if let Some([(a, ra), (b, rb)]) = split_range(f).and(split_rates(f)) {
        return Some(format!("Rated {ra} by {a} and {rb} by {b}; the lower rating counts."));
    }
    let raised = f.severity.as_str();
    if o.downgraded {
        Some(format!("Raised as {raised}; not every voter agreed, so {}.", o.severity.as_str()))
    } else if o.severity != f.severity {
        Some(format!("Raised as {raised}; voters rated it {}.", o.severity.as_str()))
    } else {
        None
    }
}

/// The finding's heading line: its first line if that starts a paragraph, otherwise the first
/// paragraph's first line.
fn heading(text: &str) -> Option<&str> {
    use pulldown_cmark::{Event, Parser, Tag};
    let text = text.trim();
    let mut depth = 0;
    for (e, at) in Parser::new_ext(text, options()).into_offset_iter() {
        match e {
            Event::Start(Tag::Paragraph) if depth == 0 => {
                return text[at.start..].lines().next().map(str::trim_end);
            }
            Event::Start(_) => depth += 1,
            Event::End(_) => depth -= 1,
            _ => {}
        }
    }
    None
}

/// A line of agent text as inline markdown without links, as it sits in a `<summary>`, or as plain
/// text where it would render as a block.
fn inline(line: &str) -> String {
    let html = markdown(line);
    match html.trim_end().strip_prefix("<p>").and_then(|h| h.strip_suffix("</p>")) {
        Some(h) if !h.contains("<p>") => ammonia::Builder::default().rm_tags(["a"]).clean(h).to_string(),
        _ => esc(line),
    }
}

/// "2 of 3 agree", counting the finding's author as the consensus does.
fn agreement(f: &Finding) -> String {
    let (agree, disagree) = votes(f);
    let agree = agree + usize::from(f.outcome.is_none() && f.by.is_some());
    format!("{agree} of {} agree", agree + disagree)
}

/// One button of a ruling form; `fid` names it for screen readers.
fn rule_button(class: &str, value: &str, label: &str, fid: &str) -> String {
    format!("<button class=\"{class}\" name=\"ruling\" value=\"{value}\">{label}<span class=\"sr\"> {}</span></button>", esc(fid))
}

/// `rule`: the viewer may rule on this review's open findings now.
fn render_finding(out: &mut String, r: &Review, f: &Finding, rule: bool) {
    let rank = rank(r, f);
    let sev = severity(f);
    let disputed = matches!(f.outcome.as_ref().map(|o| &o.status), Some(Status::Disputed | Status::Unverified));
    let open = match rank {
        Rank::Fix => true,
        Rank::Judge => f.outcome.is_some() || !r.phase.active() || r.blocks(sev),
        _ => false,
    };
    let mark = match outcome(f) {
        Some((_, label)) if label == "confirmed" => String::new(),
        Some((_, label)) if label == "resolved" => "<span class=\"badge s-approved\">fixed</span>".into(),
        Some((cls, label)) => format!("<span class=\"badge s-{cls}\">{}</span>", esc(&label)),
        None if r.phase == Phase::Offering && f.by.is_none() => "<span class=\"badge s-queued\">to re-check</span>".into(),
        None if r.phase.active() && f.by.is_none() => "<span class=\"badge s-waiting\">re-checking</span>".into(),
        None if r.phase.active() => String::new(),
        None if r.phase == Phase::Failed => "<span class=\"badge s-closed\">not voted on</span>".into(),
        None => "<span class=\"badge s-undecided\">undecided</span>".into(),
    };
    let new = if r.parent.is_some() && f.by.is_some() { "<span class=\"badge s-new\">new</span>" } else { "" };
    let head = f.title.as_deref().or_else(|| heading(&f.text)).map_or_else(|| format!("Finding {}", esc(&f.id)), inline);
    let _ = write!(
        out,
        "<details class=\"f{dim}\" id=\"{id}\"{open}><summary><span class=\"sev sev-{sev}\">{sev}</span>\
         <span class=\"ft{wrap}\" dir=\"auto\">{head}</span><span class=\"fx\">{new}{mark}<span class=\"fid\">{id}</span></span>{CHEVRON}</summary><div class=\"fb\">",
        dim = if rank >= Rank::Fixed { " dim" } else { "" },
        wrap = if f.title.is_some() { " w" } else { "" },
        id = esc(&f.id),
        open = if open { " open" } else { "" },
        sev = sev.as_str(),
    );
    let _ = write!(out, "<div class=\"md\" dir=\"auto\">{}</div>", markdown(f.text.trim()));
    let mut facts = vec![match (&f.by, &f.origin) {
        (Some(b), _) => format!("Raised by <b>{}</b>", esc(b)),
        (None, Some(o)) => {
            let href = o.replacen('/', "#", 1);
            format!("Carried over from <a href=\"/r/{}\">{}</a>", esc(&href), esc(o))
        }
        _ => "Carried over".into(),
    }];
    if let Some(u) = f.url.as_deref().and_then(safe_url) {
        let label = u.rsplit('/').next().unwrap_or(u);
        facts.push(format!("<a href=\"{}\" rel=\"noopener noreferrer\" target=\"_blank\"><code>{}</code></a>", esc(u), esc(label)));
    }
    if f.revision > 0 {
        facts.push("amended".into());
    }
    let _ = write!(out, "<p class=\"facts\">{}</p>", facts.join(" · "));
    if let Some(note) = severity_note(f) {
        let _ = write!(out, "<p class=\"facts\">{}</p>", esc(&note));
    }
    let action = format!("/r/{}/f/{}", esc(&r.id), esc(&f.id));
    match f.ruled().or(f.ruling.as_ref().filter(|_| f.outcome.is_none())) {
        Some(x) => {
            let undo = if rule { format!(" · <form method=\"post\" action=\"{action}\">{}</form>", rule_button("btn", "undo", "Undo", &format!("ruling on {}", f.id))) } else { String::new() };
            let what = if x.must_fix { "Ruled must fix" } else { "Dismissed" };
            let _ = write!(out, "<div class=\"facts ruled\">{what} by <b>{}</b>{undo}</div>", esc(&x.by));
        }
        None if rule && disputed => {
            let why = if f.outcome.as_ref().is_some_and(|o| o.status == Status::Unverified) { "Nobody else checked it" } else { "Reviewers didn't settle it" };
            let _ = write!(
                out,
                "<form class=\"rule\" method=\"post\" action=\"{action}\"><span class=\"q\">{why}. Your call:</span>{}{}</form>",
                rule_button("btn btn-fix", "must_fix", "Must fix", &f.id),
                rule_button("btn", "dismiss", "Dismiss", &f.id),
            );
        }
        None => {}
    }
    if let Some(w) = &f.withdrawn {
        let _ = write!(out, "<p class=\"facts\">Withdrawn: {}</p>", esc(w));
    }
    if !f.votes.is_empty() && rank != Rank::Fixed {
        let reasons = match f.votes.values().filter(|v| v.reason.is_some()).count() {
            0 => String::new(),
            n => format!("<span class=\"muted\">· {}</span>", plural(&n.to_string(), "reason")),
        };
        let _ = write!(
            out,
            "<details class=\"vbox\"{}><summary>{CHEVRON}{}{reasons}</summary><ul class=\"votes\">",
            if disputed { " open" } else { "" },
            agreement(f)
        );
        let rounds = f.votes.values().any(|v| v.round > 1);
        for (who, v) in &f.votes {
            let (cls, what) = match &v.choice {
                Choice::Agree => match v.severity.filter(|&s| s != f.severity) {
                    Some(s) => ("agree", format!("agrees, as {}", s.as_str())),
                    None => ("agree", "agrees".to_string()),
                },
                Choice::Disagree => ("disagree", "disagrees".to_string()),
                Choice::Duplicate(t) => ("dup", format!("duplicate of {t}")),
            };
            let what = if v.revision < f.revision { format!("{what} (before it was amended; not counted)") } else { what };
            let round = if rounds { format!(" <span class=\"muted\">· round {}</span>", v.round) } else { String::new() };
            let reason = v.reason.as_deref().map(|r| format!("<div class=\"md\" dir=\"auto\">{}</div>", markdown(r))).unwrap_or_default();
            let _ = write!(out, "<li class=\"v-{cls}\"><b>{}</b> {}{round}{reason}</li>", esc(who), esc(&what));
        }
        out.push_str("</ul></details>");
    }
    out.push_str("</div></details>");
}

fn since_link(r: &Review) -> Option<String> {
    let (old, new) = (r.since_sha.as_ref()?, r.head_sha.as_ref()?);
    Some(format!(
        "<a href=\"https://github.com/{}/{}/compare/{}...{}\" rel=\"noopener noreferrer\" target=\"_blank\">changes since <code>{}</code></a>",
        esc(&r.pr.owner),
        esc(&r.pr.repo),
        esc(old),
        esc(new),
        esc(short_sha(old)),
    ))
}

/// One list in page order: what to fix, what to judge, the optional, then the fixed and dismissed, dimmed.
/// While reviewers work, how many findings they have is hidden too, so nothing here depends on it.
fn render_findings(out: &mut String, r: &Review, rule: bool) {
    let hiding = !r.revealed() && r.phase == Phase::Reviewing;
    let mut shown: Vec<(usize, &Finding)> = visible(r).enumerate().collect();
    if shown.is_empty() && !hiding {
        return;
    }
    shown.sort_by_key(|&(i, f)| (rank(r, f), severity(f), i));
    let n = shown.iter().filter(|(_, f)| rank(r, f) < Rank::Fixed).count();
    let count = if n > 0 { format!(" <span class=\"count\">{n}</span>") } else { String::new() };
    let since = since_link(r).map(|l| format!(" <span class=\"aside\">{l}</span>")).unwrap_or_default();
    let _ = write!(out, "<h2 id=\"findings\">Findings{count}{since}</h2>");
    if hiding {
        let what = if shown.is_empty() { "Reviewers' findings" } else { "New findings" };
        let _ = write!(out, "<p class=\"empty\">{what} stay hidden until every reviewer submits.</p>");
    }
    if !shown.is_empty() {
        out.push_str("<div class=\"findings\">");
        for (_, f) in shown {
            render_finding(out, r, f, rule);
        }
        out.push_str("</div>");
    }
}

fn waiting_for(who: Vec<&String>, to: &str) -> String {
    if who.is_empty() { "Waiting for the deadline".into() } else { format!("Waiting for {} to {to}", names(&who)) }
}

/// `can`: the viewer may cancel it while it runs.
/// `locked`: a re-review has taken over this review's open findings.
/// `q`: the relay's queue, to say whether a queued review's missing program has a reviewer idle.
fn render_verdict(out: &mut String, r: &Review, q: Option<&Queue>, old: bool, can: bool, locked: bool, now: Ms) {
    let (cls, label) = status(r, locked, can);
    let detail = match r.phase {
        Phase::Done => {
            let (fix, decide, optional) = to_do(r);
            let fixed = r.findings.iter().filter(|f| rank(r, f) == Rank::Fixed).count();
            let mut parts = Vec::new();
            if fix > 0 {
                parts.push(format!("{fix} to fix"));
            }
            if decide > 0 {
                let what = format!("{decide} {}", to_decide(can, locked, fix));
                parts.push(if locked || old { what } else { format!("<span class=\"needs\">{what}</span>") });
            }
            if parts.is_empty() {
                parts.push(if r.findings.is_empty() { "No findings" } else { "Nothing to fix" }.into());
            }
            if optional > 0 {
                parts.push(format!("{optional} optional"));
            }
            if fixed > 0 {
                parts.push(format!("{fixed} fixed"));
            }
            dotted(&parts)
        }
        Phase::Failed => {
            let again = match &r.parent {
                _ if r.broke_off() => "The author can request a re-review.".into(),
                Some(p) => format!("The author can re-review it from {p}."),
                None => "The author can request it again.".into(),
            };
            let sent: Vec<&String> = r.slots.iter().filter(|s| s.submission.is_some()).map(|s| &s.agent).collect();
            let lost = if sent.is_empty() { String::new() } else { format!(" What {} submitted was discarded with it.", names(&sent)) };
            esc(&format!("{} {again}{lost}", failure(r).1))
        }
        Phase::Offering => {
            let joined = r.active_slots().count();
            let left = plural(&r.params.reviewers.saturating_sub(joined).to_string(), "reviewer");
            let why = match (q.and_then(|q| none_idle(r, &q.online)), r.missing_model()) {
                (Some(m), _) => format!("Waiting for a {} reviewer: none is idle.", m.as_str()),
                (None, Some(m)) if joined + 1 == r.params.reviewers => format!("Waiting for a {} reviewer to join.", m.as_str()),
                _ if joined == 0 => format!("Waiting for {left} to join."),
                _ => format!("Waiting for {left} more to join."),
            };
            esc(&format!("{why} Nothing to do: it starts by itself."))
        }
        Phase::Reviewing => esc(&waiting_for(r.active_slots().filter(|s| s.submission.is_none()).map(|s| &s.agent).collect(), "submit")),
        Phase::Voting(_) if r.replying => esc(&waiting_for(repliers(r), "reply")),
        Phase::Voting(_) => esc(&waiting_for(r.active_slots().filter(|s| !r.owed_votes(&s.agent).is_empty()).map(|s| &s.agent).collect(), "vote")),
    };
    let cancel = if can && r.phase.active() {
        let who: Vec<&String> = r.active_slots().map(|s| &s.agent).collect();
        let stop = match who.len() {
            0 => "The".to_string(),
            1 => format!("{} stops, and the", esc(who[0])),
            _ => format!("{} stop, and the", esc(&names(&who))),
        };
        format!(
            "<details class=\"act\" id=\"cancel\"><summary class=\"btn\"><span class=\"c0\">Cancel review</span><span class=\"c1\">Keep running</span></summary>\
             <form method=\"post\" action=\"/r/{}/cancel\"><p id=\"cancel-what\">{stop} PR is free for a new review. You can't undo this.</p>\
             <button class=\"btn btn-fix\" aria-describedby=\"cancel-what\">Cancel review</button></form></details>",
            esc(&r.id)
        )
    } else {
        String::new()
    };
    let time = if r.phase == Phase::Offering {
        let fails = format!("fails at {}", clock_tag(r.deadline));
        let fails = if r.deadline_left(now) <= SOON_MS { format!("<span class=\"needs\">{fails}</span>") } else { fails };
        format!("{} · {fails}", queued(r, now))
    } else {
        when(r, now)
    };
    let _ = write!(
        out,
        "<section class=\"verdict v-{cls}{old}\" aria-label=\"Status\"><p class=\"vt\">{label}</p><p class=\"vd\">{detail}</p><p class=\"vw\">{time}</p>{cancel}</section>",
        old = if old { " v-old" } else { "" },
    );
}

/// A queued review's team: who has joined, then each open seat with who is asked to take it,
/// or why nobody is. Without the relay's queue `q` the seats are only "open".
fn render_team(out: &mut String, r: &Review, q: Option<&Queue>) {
    let team: Vec<&String> = r.active_slots().map(|s| &s.agent).collect();
    let _ = write!(out, "<h2>Team <span class=\"count\">{} of {}</span></h2><ul class=\"reviewers\">", team.len(), r.params.reviewers);
    for s in r.active_slots() {
        let _ = write!(out, "<li><div class=\"who\"><b>{}</b>{} <span class=\"badge s-queued\"><span>joined {}</span></span></div></li>", esc(&s.agent), program(s.model), clock_tag(s.joined));
    }
    let mut seats: Vec<Option<Model>> = Model::ALL.into_iter().filter(|_| r.params.reviewers >= 2).filter(|m| !r.active_slots().any(|s| s.model == Some(*m))).map(Some).collect();
    seats.resize(r.params.reviewers.saturating_sub(team.len()), None);
    let online: &[OnlineAgent] = q.map_or(&[], |q| &q.online);
    let free: Vec<&OnlineAgent> = online.iter().filter(|a| a.role == Role::Reviewer && a.busy.is_none()).collect();
    let offered = |a: &OnlineAgent| q.and_then(|q| q.offers.get(&a.name)).map(String::as_str);
    let mut asked: Vec<&OnlineAgent> = free.iter().copied().filter(|a| offered(a) == Some(r.id.as_str())).collect();
    let mut named: Vec<&str> = Vec::new();
    let n = seats.len();
    for (i, m) in seats.into_iter().enumerate() {
        let fits = |a: &OnlineAgent| m.is_none() || a.model == m;
        let label = m.map_or("claude or codex", Model::as_str);
        let take = if m.is_some() || i + 1 == n { usize::MAX } else { 1 };
        let mut asking = Vec::new();
        while asking.len() < take && let Some(at) = asked.iter().position(|a| fits(a)) {
            asking.push(format!("<b>{}</b>", esc(&asked.remove(at).name)));
        }
        let line = if !asking.is_empty() {
            let until = match r.team_due().filter(|_| i == 0) {
                Some(t) => format!(" <span class=\"why\">until {}, else {} {} released and it waits on</span>", clock_tag(t), esc(&names(&team)), if team.len() == 1 { "is" } else { "are" }),
                None => String::new(),
            };
            format!("asking {}{until}", names(&asking))
        } else if q.is_none() {
            "open".into()
        } else {
            let idle: Vec<&&OnlineAgent> = free.iter().filter(|a| fits(a) && offered(a) != Some(r.id.as_str()) && !named.contains(&a.name.as_str())).collect();
            let may_join = |a: &&&OnlineAgent| a.name != r.author && !r.declined.contains(&a.name) && !r.lapsed.contains(&a.name) && (r.picked.is_empty() || r.picked.contains(&a.name));
            let pick = idle.iter().copied().find(may_join).or(idle.first().copied());
            named.extend(pick.map(|a| a.name.as_str()));
            match pick {
                None => "none idle".into(),
                Some(a) if r.lapsed.contains(&a.name) => format!("<b>{}</b> didn't answer its offer; it's asked again when it next waits", esc(&a.name)),
                Some(a) if r.declined.contains(&a.name) => format!("<b>{}</b> declined; asked again from {}", esc(&a.name), clock_tag(r.declined_until(&a.name).unwrap_or_default())),
                Some(a) if !may_join(&a) => format!("<b>{}</b> is idle but can't take it", esc(&a.name)),
                Some(a) if offered(a).is_some_and(|o| o != r.id) => format!("<b>{}</b> is asked for {} first", esc(&a.name), esc(offered(a).unwrap_or_default())),
                Some(a) => format!("<b>{}</b> is idle, asked once a full team can form", esc(&a.name)),
            }
        };
        let _ = write!(out, "<li><div class=\"who\"><span class=\"muted\">{label}:</span> {line}</div></li>");
    }
    out.push_str("</ul>");
}

fn render_reviewers(out: &mut String, r: &Review) {
    if r.slots.is_empty() {
        return;
    }
    let _ = write!(out, "<h2>Reviewers <span class=\"count\">{}</span></h2><ul class=\"reviewers\">", r.slots.len());
    for s in &r.slots {
        let state = match (&s.dropped, &s.submission) {
            (Some(d), _) => format!("<span class=\"badge s-failed\">dropped</span> <span class=\"muted\">{}</span>", esc(d)),
            (None, Some(x)) => {
                let (cls, label) = match x.verdict {
                    _ if !r.revealed() => ("submitted", "submitted"),
                    Verdict::Approve => ("approved", "approves"),
                    Verdict::ChangesRequested => ("changes", "requests changes"),
                };
                let sha = if r.head_sha.as_deref().is_none_or(|h| short_sha(h) == short_sha(&x.head_sha)) {
                    String::new()
                } else {
                    format!(" <span class=\"muted\">at</span> <code>{}</code>", esc(short_sha(&x.head_sha)))
                };
                format!("<span class=\"badge s-{cls}\">{label}</span>{sha}{}", took(s))
            }
            (None, None) if r.phase == Phase::Failed => "<span class=\"badge s-closed\">stopped</span>".into(),
            (None, None) => "<span class=\"badge s-waiting\">reviewing</span>".into(),
        };
        let mut summary = s
            .submission
            .as_ref()
            .filter(|x| !x.summary.is_empty() && r.revealed())
            .map(|x| format!("<div class=\"md\" dir=\"auto\">{}</div>", markdown(&x.summary)))
            .unwrap_or_default();
        if let Some(c) = s.submission.as_ref().and_then(|x| x.checked.as_deref()).filter(|_| r.revealed()) {
            let _ = write!(summary, "<p class=\"facts\" dir=\"auto\">Checked: {}</p>", esc(c));
        }
        let _ = write!(out, "<li><div class=\"who\"><b>{}</b>{} {state}</div>{summary}</li>", esc(&s.agent), program(s.model));
    }
    out.push_str("</ul>");
}

/// " in 12s" after a reviewer's submission, from when it got the task; amber under `FAST_MS`.
fn took(s: &Slot) -> String {
    match s.took() {
        Some(t) if t < FAST_MS => format!(" <span class=\"took needs\" title=\"Submitted {0} after getting the task\">in {0}, very fast</span>", dur(t)),
        Some(t) => format!(" <span class=\"took muted\">in {}</span>", dur(t)),
        None => String::new(),
    }
}

/// Who decides the findings the reviewers left open: the viewer, if they may rule on the review,
/// else its author; nobody once a re-review has carried them over (`locked`).
fn to_decide(can: bool, locked: bool, fix: usize) -> &'static str {
    match (locked, fix, can) {
        (true, _, _) => "carried to the re-review",
        (false, 0, _) => "to decide",
        (false, _, true) => "for you to decide",
        (false, _, false) => "for the author to decide",
    }
}

/// A review in a few words, for its index row and the PR's history; `can` and `locked` as for `status`.
fn gist(r: &Review, can: bool, locked: bool) -> String {
    match r.phase {
        Phase::Offering => format!("{} of {} joined", r.active_slots().count(), r.params.reviewers),
        Phase::Reviewing => format!("{} of {} submitted", r.active_slots().filter(|s| s.submission.is_some()).count(), r.active_slots().count()),
        Phase::Voting(_) if r.replying => format!("{} to reply", repliers(r).len()),
        Phase::Voting(n) => {
            let voted = |a: &String| r.findings.iter().any(|f| f.votes.get(a).is_some_and(|v| v.round == n));
            let owing = r.active_slots().filter(|s| !r.owed_votes(&s.agent).is_empty()).count();
            let done = r.active_slots().filter(|s| r.owed_votes(&s.agent).is_empty() && voted(&s.agent)).count();
            format!("{done} of {} voted", done + owing)
        }
        Phase::Failed => match &r.cancelled_by {
            Some(by) => format!("by {by}"),
            None => failure(r).0,
        },
        Phase::Done => {
            let (fix, decide, _) = to_do(r);
            match (fix, decide) {
                (0, 0) if r.findings.is_empty() => "no findings".into(),
                (0, 0) => "nothing to fix".into(),
                (f, 0) => format!("{f} to fix"),
                (f, d) => {
                    let what = format!("{d} {}", to_decide(can, locked, f));
                    if f == 0 { what } else { format!("{f} to fix · {what}") }
                }
            }
        }
    }
}

/// On an old review, one line naming the PR's latest review and, if that one hasn't finished, the last result.
fn render_newer(out: &mut String, r: &Review, prs: &[Review], may: &dyn Fn(&Review) -> bool) {
    let Some(latest) = prs.last().filter(|l| l.id != r.id) else {
        return;
    };
    let last = prs
        .iter()
        .filter(|x| x.phase == Phase::Done && latest.phase.active())
        .max_by_key(|x| x.ended)
        .map(|x| format!("&nbsp;· last result: {}", status(x, rereview(prs, &x.id).is_some(), may(x)).1.to_lowercase()))
        .unwrap_or_default();
    let _ = write!(
        out,
        "<p class=\"newer\">Newer review: <a href=\"/r/{0}\">{0}</a> ({1}){last}</p>",
        esc(&latest.id),
        status(latest, false, may(latest)).1.to_lowercase(),
    );
}

fn render_history(out: &mut String, r: &Review, prs: &[Review], may: &dyn Fn(&Review) -> bool, now: Ms) {
    if prs.len() < 2 {
        return;
    }
    out.push_str("<h2>Reviews of this PR</h2><ol class=\"history\">");
    for x in prs.iter().rev() {
        let (current, id) = if x.id == r.id {
            (" aria-current=\"page\"", format!("<b>{}</b>", esc(&x.id)))
        } else {
            ("", format!("<a href=\"/r/{0}\">{0}</a>", esc(&x.id)))
        };
        let locked = rereview(prs, &x.id).is_some();
        let (cls, label) = status(x, locked, may(x));
        let _ = write!(
            out,
            "<li{current}><span class=\"h-id\">{id}</span><span class=\"st s-{cls}\">{label}</span><span class=\"h-gist\">{}</span><span class=\"h-when\">{}</span></li>",
            esc(&gist(x, may(x), locked)),
            when(x, now),
        );
    }
    out.push_str("</ol>");
}

/// The page as seen on the day the review was requested.
pub fn render_review(r: &Review) -> String {
    render_review_page(r, &[], None, r.created, &|_| false)
}

/// What a queued review's page shows of the relay: who is online and the review each free
/// reviewer is offered, taken under the state's lock so the page renders outside it.
pub struct Queue {
    online: Vec<OnlineAgent>,
    offers: BTreeMap<String, String>,
}

impl Queue {
    pub fn of(st: &State, now: Ms) -> Queue {
        let online = st.online(now);
        let offers = online.iter().filter_map(|a| Some((a.name.clone(), st.offered(&a.name)?.to_string()))).collect();
        Queue { online, offers }
    }
}

/// `prs` are all reviews of the same PR, this one included, oldest first; `q` the relay's queue,
/// for who could join a queued review. `may`: whether the viewer may cancel or rule on a review.
pub fn render_review_page(r: &Review, prs: &[Review], q: Option<&Queue>, now: Ms, may: &dyn Fn(&Review) -> bool) -> String {
    let can = may(r);
    let mut meta = vec![format!("{} by <b>{}</b>", esc(&r.id), esc(&r.author))];
    if let Some(h) = &r.head_sha {
        meta.push(format!("<code>{}</code>", esc(short_sha(h))));
    }
    meta.push(format!("requested {}", time_tag(r.created, now)));
    let mut out = String::new();
    render_newer(&mut out, r, prs, may);
    let _ = write!(
        out,
        "<h1><span class=\"ref\">{short}</span><a href=\"{url}\" rel=\"noopener noreferrer\" target=\"_blank\" dir=\"auto\">{name}<span class=\"ext\">{EXTERNAL}</span></a></h1>\
         <p class=\"meta\">{meta}</p>",
        url = esc(&r.pr.url()),
        short = esc(&r.pr.short()),
        name = esc(&pr_name(r)),
        meta = dotted(&meta),
    );
    let locked = rereview(prs, &r.id).is_some();
    render_verdict(&mut out, r, q, prs.last().is_some_and(|l| l.id != r.id), can, locked, now);
    render_findings(&mut out, r, can && r.phase == Phase::Done && !locked);
    render_discussion(&mut out, r);
    if r.phase == Phase::Offering {
        render_team(&mut out, r, q);
    } else {
        render_reviewers(&mut out, r);
    }
    render_history(&mut out, r, prs, may, now);
    render_transcript(&mut out, r);
    out
}

/// Filled in with the viewer's local time by the page script.
fn clock_tag(ms: Ms) -> String {
    let iso = iso(ms);
    format!("<time class=\"clock\" datetime=\"{}\">{} UTC</time>", iso.replace(' ', "T"), &iso[11..16])
}

/// Messages in full, then every entry as one quiet line. The log text is also what agents read,
/// so only lines the relay itself wrote are reworded, and only here.
/// The author's note, as of the request, then every message.
fn render_discussion(out: &mut String, r: &Review) {
    let note = (!r.note.is_empty()).then_some((&r.author, r.created, &r.note));
    let says: Vec<_> = note.into_iter().chain(r.log.iter().filter(|e| e.kind == "say").map(|e| (&e.who, e.at, &e.text))).collect();
    if !says.is_empty() {
        let _ = write!(out, "<h2>Discussion <span class=\"count\">{}</span></h2><ol class=\"talk\">", says.len());
        for (who, at, text) in says {
            let _ = write!(
                out,
                "<li><div class=\"th\"><b>{}</b> {}</div><div class=\"md\" dir=\"auto\">{}</div></li>",
                esc(who),
                clock_tag(at),
                markdown(text)
            );
        }
        out.push_str("</ol>");
    }
}

fn render_transcript(out: &mut String, r: &Review) {
    let _ = write!(
        out,
        "<h2 class=\"sr\">Transcript</h2><details class=\"more\" id=\"transcript\"><summary>{CHEVRON}Transcript <span class=\"count\">{}</span></summary><ol class=\"log\">",
        r.log.len()
    );
    let mut i = 0;
    while i < r.log.len() {
        let e = &r.log[i];
        let mut n = 1;
        let body = if e.kind == "vote" || e.kind == "revote" {
            n = r.log[i..].iter().take_while(|x| x.who == e.who && x.kind == e.kind && x.at == e.at).count();
            let items: String = r.log[i..i + n].iter().map(|x| vote_item(&x.text)).collect();
            let verb = if e.kind == "vote" { "voted" } else { "changed a vote" };
            format!("<b>{}</b> {verb}<ul class=\"vl\">{items}</ul>", esc(&e.who))
        } else {
            relay_line(r, e).unwrap_or_else(|| format!("<b>{}</b> <div class=\"md\" dir=\"auto\">{}</div>", esc(&e.who), markdown(&e.text)))
        };
        let _ = write!(out, "<li>{}<div class=\"et\">{body}</div></li>", clock_tag(e.at));
        i += n;
    }
    out.push_str("</ol></details>");
}

fn vote_item(line: &str) -> String {
    let Some((fid, rest)) = line.split_once(": ") else {
        return format!("<li><div class=\"md\" dir=\"auto\">{}</div></li>", markdown(line));
    };
    let (label, reason) = rest.split_once(" — ").unwrap_or((rest, ""));
    let reason = if reason.is_empty() { String::new() } else { format!(" <div class=\"md why\" dir=\"auto\">{}</div>", markdown(reason)) };
    format!("<li><b>{}</b> {}{reason}</li>", esc(fid), esc(label))
}

fn relay_line(r: &Review, e: &LogEntry) -> Option<String> {
    let t = e.text.as_str();
    match e.kind.as_str() {
        "phase" => {
            if let Some(names) = t.strip_prefix("independent review started with ") {
                return Some(format!("Reviewing: {}", esc(names)));
            }
            let n: u8 = t.strip_prefix("cross-examination round ")?.strip_suffix(" started")?.parse().ok()?;
            let replies = r.params.reply_secs > 0 && r.findings.iter().any(|f| f.by.is_some() && f.open_round >= n);
            Some(match n {
                1 => "Voting".into(),
                _ if replies => format!("Replying, then voting, round {n}"),
                _ => format!("Voting, round {n}"),
            })
        }
        "result" => Some(match t.strip_prefix("consensus: ")? {
            "approve" => "Result: approved".into(),
            v => format!("Result: {}", esc(&v.replace('_', " "))),
        }),
        "request" => {
            let (n, rest) = t.strip_prefix("requested ")?.split_once(" reviewer(s)")?;
            n.parse::<u32>().ok()?;
            Some(format!("<b>{}</b> <div class=\"md\" dir=\"auto\">{}</div>", esc(&e.who), markdown(&format!("requested {}{rest}", plural(n, "reviewer")))))
        }
        "task" => Some(format!("<b>{}</b> got the review task", esc(&e.who))),
        "submit" if !r.revealed() => Some(format!("<b>{}</b> submitted at <code>{}</code>", esc(&e.who), esc(uncounted(t)?))),
        "submit" => {
            let (n, rest) = t.strip_prefix("submitted ")?.split_once(" finding(s) at ")?;
            let (sha, checked) = rest.split_once("; checked: ").unwrap_or((rest, ""));
            let checked = if checked.is_empty() || !r.revealed() { String::new() } else { format!(" · checked: <span dir=\"auto\">{}</span>", esc(checked)) };
            Some(format!("<b>{}</b> submitted {} at <code>{}</code>{checked}", esc(&e.who), plural(n, "finding"), esc(short_sha(sha))))
        }
        _ => None,
    }
}

/// The short head of "submitted 2 finding(s) at <sha>; checked: …", all that is shown of it before findings are revealed.
fn uncounted(t: &str) -> Option<&str> {
    let (_, rest) = t.strip_prefix("submitted ")?.split_once(" finding(s) at ")?;
    Some(short_sha(rest.split_once("; ").map_or(rest, |(sha, _)| sha)))
}

fn plural(n: &str, noun: &str) -> String {
    if n == "1" { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

fn summary_json(r: &Review, f: &Finding) -> Value {
    let (agree, disagree) = votes(f);
    json!({
        "id": f.id,
        "title": title(f.title.as_deref().unwrap_or(&f.text)),
        "severity": f.outcome.as_ref().map(|o| o.severity).unwrap_or(f.severity),
        "severity_split": split_range(f),
        "by": f.by,
        "origin": f.origin,
        "outcome": outcome(f).map(|(_, l)| l).unwrap_or_else(|| if r.phase.active() { "open".into() } else { "undecided".into() }),
        "agree": agree,
        "disagree": disagree,
    })
}

/// A review's row in `/api/reviews`.
pub fn list_json(r: &Review, now: Ms) -> Value {
    json!({
        "id": r.id,
        "pr": r.pr.url(),
        "repo": r.pr.short(),
        "title": r.title,
        "author": r.author,
        "phase": phase_badge(r.phase),
        "round": r.round(),
        "phase_label": r.phase.label(),
        "active": r.phase.active(),
        "created": r.created,
        "ended": r.ended,
        "deadline": r.phase.active().then_some(r.deadline),
        "deadline_left_ms": r.phase.active().then(|| r.deadline_left(now)),
        "verdict": r.verdict(),
        "failure": r.failure,
        "parent": r.parent,
        "reviewers": r.slots.iter().map(|s| &s.agent).collect::<Vec<_>>(),
        "findings": visible(r).count(),
        "path": format!("/r/{}", r.id),
    })
}

/// The whole review as JSON, with what the page hides before every reviewer has submitted left out.
pub fn review_json(r: &Review, rereviews: &[String], now: Ms, web: &str) -> Value {
    let mut v = serde_json::to_value(r).unwrap_or(Value::Null);
    let Some(m) = v.as_object_mut() else {
        return v;
    };
    m.remove("next_finding");
    m.insert("result".into(), json!(r.result_now(web)));
    if !r.revealed() {
        let shown: Vec<Value> = visible(r).filter_map(|f| serde_json::to_value(f).ok()).collect();
        m.insert("findings".into(), Value::Array(shown));
        if let Some(slots) = m.get_mut("slots").and_then(Value::as_array_mut) {
            for s in slots {
                if let Some(sub) = s.get_mut("submission").and_then(Value::as_object_mut) {
                    sub.insert("summary".into(), Value::String(String::new()));
                    sub.insert("verdict".into(), Value::Null);
                    sub.remove("checked");
                }
            }
        }
        for e in m.get_mut("log").and_then(Value::as_array_mut).into_iter().flatten().filter(|e| e["kind"] == "submit") {
            if let Some(sha) = e["text"].as_str().and_then(uncounted).map(str::to_string) {
                e["text"] = Value::String(format!("submitted at {sha}"));
            }
        }
    }
    for f in m.get_mut("findings").and_then(Value::as_array_mut).into_iter().flatten() {
        let sev = f["severity"].clone();
        for v in f.get_mut("votes").and_then(Value::as_object_mut).into_iter().flat_map(|o| o.values_mut()) {
            if v["choice"] == "Agree" && v["severity"].is_null() {
                v["severity"] = sev.clone();
            }
        }
    }
    let base = list_json(r, now);
    for k in ["phase", "round", "phase_label", "active", "deadline", "deadline_left_ms", "verdict", "path"] {
        m.insert(k.into(), base[k].clone());
    }
    m.insert("pr".into(), json!({ "url": r.pr.url(), "repo": r.pr.short(), "number": r.pr.number }));
    m.insert("revealed".into(), json!(r.revealed()));
    m.insert("summary".into(), visible(r).map(|f| summary_json(r, f)).collect());
    m.insert("timeline".into(), json!(timeline(r)));
    m.insert("rereviews".into(), json!(rereviews));
    v
}

fn rereviews(st: &State, id: &str) -> Vec<String> {
    st.reviews
        .iter()
        .filter(|c| c.parent.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(id)))
        .map(|c| c.id.clone())
        .collect()
}

fn siblings(st: &State, r: &Review) -> Vec<Review> {
    let mut v: Vec<Review> = st.reviews.iter().filter(|x| x.pr == r.pr).cloned().collect();
    v.sort_by_key(Review::seq);
    v
}

fn snapshot(hub: &Hub, id: &str) -> Option<(Review, Vec<String>, Ms)> {
    hub.read(|s, now| s.review(id).map(|r| (r.clone(), rereviews(s, &r.id), now)))
}

/// Wraps a page's main part with its tab title, which the page script copies on each poll.
fn titled(title: &str, main: &str) -> String {
    format!("<div data-title=\"{}\">{main}</div>", esc(title))
}

fn partial(headers: &HeaderMap) -> bool {
    headers.contains_key("x-amx-partial")
}

fn etag(main: &str) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    main.hash(&mut h);
    format!("\"{:x}\"", h.finish())
}

/// A page's main part for the page script, which polls it; 304 if it hasn't changed.
fn fragment(headers: &HeaderMap, main: String) -> Response {
    let tag = etag(&main);
    let mut r = if headers.get(header::IF_NONE_MATCH).is_some_and(|v| v.as_bytes() == tag.as_bytes()) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        Html(main).into_response()
    };
    r.headers_mut().insert(header::ETAG, HeaderValue::from_str(&tag).expect("hex"));
    no_store(r)
}

fn no_store(mut r: Response) -> Response {
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

#[derive(Deserialize)]
pub struct IndexQuery {
    f: Option<String>,
    page: Option<String>,
}

pub async fn index(
    AxState(hub): AxState<Arc<Hub>>,
    Query(q): Query<IndexQuery>,
    who: Option<Extension<Identity>>,
    owner: Option<Extension<Owner>>,
    Extension(viewer): Extension<Viewer>,
    headers: HeaderMap,
) -> Response {
    let login = who.as_ref().map(|Extension(w)| w.login.as_str());
    let may = |r: &Review| login.is_some_and(|l| r.may_act(l, owner.is_some()));
    let mine = |r: &Review| login.is_some_and(|l| r.author_login.eq_ignore_ascii_case(l));
    let (main, title) = hub.read(|s, now| {
        let quiet = s.reviews.is_empty() || s.online(now).is_empty();
        let connect = login.map(|l| connect(&s.web_url, l, quiet));
        let view = View { filter: q.f.as_deref(), page: q.page.as_deref(), connect, may: &may, mine: &mine };
        index_page(s, now, &view)
    });
    let main = titled(&title, &main);
    if partial(&headers) {
        return fragment(&headers, main);
    }
    no_store(Html(page(&title, &main, true, &viewer)).into_response())
}

pub async fn review_page(
    AxState(hub): AxState<Arc<Hub>>,
    Path(id): Path<String>,
    who: Option<Extension<Identity>>,
    owner: Option<Extension<Owner>>,
    Extension(viewer): Extension<Viewer>,
    headers: HeaderMap,
) -> Response {
    let taken = hub.read(|s, now| s.review(&id).map(|r| (r.clone(), siblings(s, r), (r.phase == Phase::Offering).then(|| Queue::of(s, now)), now)));
    let Some((r, prs, q, now)) = taken else {
        let main = format!(
            "<h1>No review {0}</h1><p class=\"empty\">{0} isn't in the hub's history any more (older reviews are trimmed). <a href=\"/\">All reviews</a></p>",
            esc(&id)
        );
        let body = if partial(&headers) { titled("amx", &main) } else { page("amx", &main, false, &viewer) };
        return no_store((StatusCode::NOT_FOUND, Html(body)).into_response());
    };
    let may = |r: &Review| who.as_ref().is_some_and(|Extension(w)| r.may_act(&w.login, owner.is_some()));
    let title = format!("{} · {}", status(&r, rereview(&prs, &r.id).is_some(), may(&r)).1, tab_name(&r));
    let main = titled(&title, &render_review_page(&r, &prs, q.as_ref(), now, &may));
    if partial(&headers) {
        return fragment(&headers, main);
    }
    no_store(Html(page(&title, &main, true, &viewer)).into_response())
}

/// A refused action, as a page that doesn't poll.
fn refused(code: StatusCode, id: &str, why: &str, viewer: &Viewer) -> Response {
    let main = format!("<h1>Not done</h1><p>{}</p><p><a href=\"/r/{1}\">Back to {1}</a></p>", esc(why), esc(id));
    no_store((code, Html(page("amx", &main, false, viewer))).into_response())
}

/// Runs a person's action on review `id` (and its finding `fid`) if they may act on it, saves
/// it, and sends them back to its page. The hub owner is told apart by how the request came.
fn act(hub: &Hub, id: &str, fid: Option<&str>, who: &Identity, owner: Option<Owner>, viewer: &Viewer, f: impl FnOnce(&mut State, &str, Ms) -> anyhow::Result<()>) -> Response {
    let has = |r: &Review| r.phase != Phase::Done || fid.is_none_or(|fid| r.findings.iter().any(|f| f.id.eq_ignore_ascii_case(fid.trim())));
    let Some((rid, may, found)) = hub.read(|s, _| s.review(id).map(|r| (r.id.clone(), r.may_act(&who.login, owner.is_some()), has(r)))) else {
        return refused(StatusCode::NOT_FOUND, id, &format!("There is no review {id}."), viewer);
    };
    if !may {
        return refused(StatusCode::FORBIDDEN, &rid, "Only the review's author or the hub owner can do this.", viewer);
    }
    if !found {
        return refused(StatusCode::NOT_FOUND, &rid, &format!("{rid} has no finding {}.", fid.unwrap_or("")), viewer);
    }
    let by = if owner.is_some_and(|o| o.loopback) { "the hub owner" } else { who.login.split('@').next().unwrap_or("") };
    match hub.write_saved(|s, now| f(s, by, now)) {
        Ok(Err(e)) => return refused(StatusCode::CONFLICT, &rid, &format!("{e:#}."), viewer),
        Err(e) => {
            tracing::error!("{e:#}");
            return refused(StatusCode::INTERNAL_SERVER_ERROR, &rid, "The relay couldn't save it. Try again.", viewer);
        }
        Ok(Ok(())) => {}
    }
    no_store((StatusCode::SEE_OTHER, [(header::LOCATION, format!("/r/{rid}"))]).into_response())
}

pub async fn cancel(
    AxState(hub): AxState<Arc<Hub>>,
    Path(id): Path<String>,
    Extension(who): Extension<Identity>,
    owner: Option<Extension<Owner>>,
    Extension(viewer): Extension<Viewer>,
) -> Response {
    act(&hub, &id, None, &who, owner.map(|Extension(o)| o), &viewer, |s, by, now| match s.review(&id) {
        Some(r) if r.cancelled_by.is_some() => Ok(()),
        Some(r) if !r.phase.active() => {
            let how = match (r.phase, r.verdict()) {
                (Phase::Done, Some(Verdict::Approve)) => "it was approved",
                (Phase::Done, _) => "changes were requested",
                _ => "it failed",
            };
            anyhow::bail!("{} had already ended: {how} at {}", r.id, utc(r.ended.unwrap_or(r.created), now))
        }
        _ => s.cancel(&id, by, now),
    })
}

#[derive(Deserialize)]
pub struct RuleForm {
    ruling: String,
}

pub async fn rule(
    AxState(hub): AxState<Arc<Hub>>,
    Path((id, fid)): Path<(String, String)>,
    Extension(who): Extension<Identity>,
    owner: Option<Extension<Owner>>,
    Extension(viewer): Extension<Viewer>,
    Form(form): Form<RuleForm>,
) -> Response {
    let must_fix = match form.ruling.as_str() {
        "must_fix" => Some(true),
        "dismiss" => Some(false),
        "undo" => None,
        _ => return refused(StatusCode::BAD_REQUEST, &id, "A ruling is must_fix, dismiss or undo.", &viewer),
    };
    act(&hub, &id, Some(&fid), &who, owner.map(|Extension(o)| o), &viewer, |s, by, now| s.rule(&id, &fid, by, must_fix, now).map(|_| ()))
}

pub async fn api_reviews(AxState(hub): AxState<Arc<Hub>>) -> Response {
    let list = hub.read(|s, now| {
        let mut rs: Vec<&Review> = s.reviews.iter().collect();
        rs.sort_by_key(|r| std::cmp::Reverse(r.created));
        rs.into_iter().map(|r| list_json(r, now)).collect::<Vec<_>>()
    });
    no_store(Json(json!({ "reviews": list })).into_response())
}

pub async fn api_review(AxState(hub): AxState<Arc<Hub>>, Path(id): Path<String>) -> Response {
    match snapshot(&hub, &id) {
        Some((r, children, now)) => no_store(Json(review_json(&r, &children, now, &hub.web_url())).into_response()),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": format!("no review {id}") }))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_versions_follow_their_content() {
        assert_eq!(version(b""), 0x811c9dc5);
        assert_eq!(version(b"a"), 0xe40c292c);
        assert_ne!(version(b"body{}"), version(b"body{ }"));
    }

    #[test]
    fn escapes_html() {
        assert_eq!(esc("<a href=\"x\">'&'</a>"), "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;");
        assert_eq!(esc("\u{202E}live\u{2066}"), "live");
        assert_eq!(markdown("**\u{202E}evil**"), "<p><strong>evil</strong></p>\n");
    }

    #[test]
    fn markdown_keeps_line_breaks() {
        assert_eq!(markdown("a\nb"), "<p>a<br>\nb</p>\n");
    }

    #[test]
    fn markdown_is_sanitised() {
        let h = markdown("**hi** <script>alert(1)</script> [x](javascript:alert(1)) <img src=x onerror=alert(1)>\n\n[ok](https://github.com/a/b)");
        assert!(h.contains("<strong>hi</strong>"));
        assert!(!h.contains("<script"));
        assert!(!h.contains("javascript:"));
        assert!(!h.contains("<img"));
        assert!(h.contains("href=\"https://github.com/a/b\""));
        assert!(h.contains("noopener"));
        assert!(markdown("returns Vec<String> now").contains("Vec&lt;String&gt;"));
        let h = markdown("see ![pixel](https://tracker.example.invalid/p.png?leak=1) and ![](https://x.example/a.png)");
        assert!(!h.contains("<img"), "{h}");
        assert!(h.contains(r#"href="https://tracker.example.invalid/p.png?leak=1""#) && h.contains(">pixel</a>"), "{h}");
        assert!(h.contains(">https://x.example/a.png</a>"), "{h}");
    }

    #[test]
    fn titles_are_first_lines() {
        assert_eq!(title("\n## **Off by one** in `len`\nmore"), "Off by one in len");
        let long = "x".repeat(150);
        assert_eq!(title(&long).chars().count(), 101);
    }

    #[test]
    fn headings_take_the_first_line_when_it_starts_a_paragraph() {
        assert_eq!(heading("Off by one\n\nThe loop ends early."), Some("Off by one"));
        assert_eq!(heading("Uses `a.b()` twice. Cache it."), Some("Uses `a.b()` twice. Cache it."));
        assert_eq!(heading("No stop here\nand more"), Some("No stop here"));
        let link = "See [foo. bar](https://example.invalid/x) for the problem. Second sentence.";
        assert_eq!(heading(link), Some(link), "never cut at a full stop");
        assert_eq!(heading("Use e.g. a mutex here. The rest of the text."), Some("Use e.g. a mutex here. The rest of the text."));
    }

    #[test]
    fn headings_never_split_a_block() {
        let fence = "```rust\nfn main() { let x = 1; }\n```\nCode block first.";
        assert_eq!(heading(fence), Some("Code block first."), "the first plain line");
        let list = "1. ordered first\n2. ordered second";
        assert_eq!(heading(list), None);
        let table = "| a | b |\n|---|---|\n| 1 | 2 |";
        assert_eq!(heading(table), None);
        let quoted = "> quoted\n\n## Title\n\n- item\n\nThen prose.";
        assert_eq!(heading(quoted), Some("Then prose."));
        assert_eq!(heading("Setext title\n---\nbody"), Some("body"));
    }

    #[test]
    fn headings_keep_the_agents_characters() {
        assert_eq!(inline("Expiry is `ttl * 1000` but **ttl** is ms"), "Expiry is <code>ttl * 1000</code> but <strong>ttl</strong> is ms");
        assert_eq!(inline("ttl * 1000 overflows"), "ttl * 1000 overflows");
        assert_eq!(inline("# not a heading"), "# not a heading");
        assert_eq!(inline("<b>x</b>"), "&lt;b&gt;x&lt;/b&gt;");
        assert_eq!(inline("[Link title](https://github.com/a/b#L1) as title"), "Link title as title", "no links inside a summary");
    }
}
