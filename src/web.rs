use crate::hub::Hub;
use crate::relay::{Choice, Finding, LogEntry, Ms, OnlineAgent, Phase, Review, Role, Severity, State, Status, Verdict};
use crate::text::{bidi, iso, phase_badge, short_sha, split_range, split_rates};
use axum::extract::{Path, State as AxState};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::{Extension, Json};
use crate::tailscale::Identity;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashSet;
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

/// A running review's deadline, counted down by the page script, else when it ended.
fn when(r: &Review, now: Ms) -> String {
    if r.phase.active() {
        format!("<span data-deadline=\"{}\">until {}</span>", r.deadline, utc(r.deadline, now))
    } else {
        time_tag(r.ended.unwrap_or(r.created), now)
    }
}

/// The review's state as a class and label.
fn status(r: &Review) -> (&'static str, String) {
    match r.phase {
        Phase::Offering => ("waiting", "Waiting for reviewers".into()),
        Phase::Reviewing => ("inreview", "Reviewing".into()),
        Phase::Voting(_) if r.replying => ("voting", "Replying".into()),
        Phase::Voting(1) => ("voting", "Voting".into()),
        Phase::Voting(n) => ("voting", format!("Voting, round {n}")),
        Phase::Done if r.result.as_ref().is_some_and(|x| x.verdict == Verdict::Approve) => ("approved", "Approved".into()),
        Phase::Done => ("changes", "Changes requested".into()),
        Phase::Failed => ("failed", "Failed".into()),
    }
}

/// `live` pages poll for changes and show a line while they can't; others load no script.
fn page(title: &str, main: &str, live: bool) -> String {
    let (script, status, tag) = if live {
        (
            "<script src=\"/static/app.js\" defer></script>\n",
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
<link rel="stylesheet" href="/static/style.css">
{script}</head>
<body>
<a class="skip" href="#main">Skip to content</a>
<header class="top"><a class="brand" href="/">amx</a>{status}</header>
<main id="main"{tag}>{main}</main>
</body>
</html>"##,
        title = esc(title),
    )
}

/// The outcome of a finding as a badge class and label, or `None` while it is still open.
fn outcome(f: &Finding) -> Option<(&'static str, String)> {
    let Some(o) = &f.outcome else {
        return f.withdrawn.is_some().then(|| ("closed", "withdrawn".to_string()));
    };
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

/// The first line of a finding as plain text.
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
        Phase::Failed => stage("Failed", "done", finished),
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
fn failure(r: &Review) -> (&'static str, String) {
    if !r.slots.is_empty() {
        let who: Vec<&String> = r.slots.iter().map(|s| &s.agent).collect();
        ("reviewers dropped out", format!("All reviewers dropped out: {}.", names(&who)))
    } else if r.offered.is_empty() {
        ("no reviewers free", "No reviewers were free.".into())
    } else {
        ("nobody accepted", format!("Nobody accepted in time. {}", asked(r)))
    }
}

/// Confirmed blocking and major findings, those the reviewers left undecided, and confirmed minors and nits.
fn to_do(r: &Review) -> (usize, usize, usize) {
    let (mut fix, mut decide, mut optional) = (0, 0, 0);
    for o in r.findings.iter().filter_map(|f| f.outcome.as_ref()) {
        match (&o.status, o.severity) {
            (Status::Confirmed, Severity::Blocking | Severity::Major) => fix += 1,
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

/// `last` is the newest finished review of the same PR, shown unless this one has a result.
fn review_row(out: &mut String, r: &Review, last: Option<&Review>, now: Ms) {
    let what = gist(r);
    let (cls, label) = status(r);
    let mut meta = vec![format!("<span class=\"st s-{cls}\">{label}</span>"), esc(&what)];
    if let Some(l) = last.filter(|_| r.phase != Phase::Done) {
        meta.push(format!("last: {}", status(l).1.to_lowercase()));
    }
    meta.push(esc(&r.author));
    let _ = write!(
        out,
        "<li class=\"rv\"><div class=\"rv-top\"><a class=\"rv-pr\" href=\"/r/{id}\">{pr}<span class=\"sr\">, {label}</span></a>\
         <span class=\"rv-when\">{}</span></div><div class=\"rv-meta\">{meta}</div></li>",
        when(r, now),
        id = esc(&r.id),
        pr = esc(&r.pr.short()),
        meta = dotted(&meta),
    );
}

/// "10 reviewers idle · 6 reviewing · 2 authors", leaving out the parts that are zero.
fn availability(online: &[OnlineAgent]) -> String {
    let reviewers = online.iter().filter(|a| a.role == Role::Reviewer);
    let idle = reviewers.clone().filter(|a| a.busy.is_none()).count();
    let reviewing = reviewers.filter(|a| a.busy.is_some()).count();
    let authors = online.len() - idle - reviewing;
    let plural = |n: usize, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
    let parts = [
        (idle, plural(idle, "reviewer") + " idle"),
        (reviewing, format!("{reviewing} reviewing")),
        (authors, plural(authors, "author")),
    ];
    dotted(&parts.into_iter().filter(|(n, _)| *n > 0).map(|(_, s)| s).collect::<Vec<_>>())
}

/// One flat list, one row per PR, newest request first: rows only move when a review is requested.
pub fn render_index(st: &State, now: Ms) -> String {
    let mut out = String::from("<h1 class=\"sr\">Pull requests</h1>");
    let mut reviews: Vec<&Review> = st.reviews.iter().collect();
    reviews.sort_by_key(|r| std::cmp::Reverse(r.created));
    let last = |r: &Review| {
        st.reviews
            .iter()
            .filter(|x| x.pr == r.pr && x.phase == Phase::Done)
            .max_by_key(|x| x.ended)
    };
    let mut seen = HashSet::new();
    reviews.retain(|r| seen.insert(&r.pr));
    if reviews.is_empty() {
        out.push_str("<p class=\"empty reviews-empty\">No reviews yet.</p>");
    } else {
        out.push_str("<ul class=\"reviews\">");
        for r in reviews {
            review_row(&mut out, r, last(r), now);
        }
        out.push_str("</ul>");
    }
    out.push_str("<h2 class=\"sr\">Online agents</h2>");
    let mut online = st.online(now);
    if online.is_empty() {
        out.push_str("<p class=\"online empty\">No agents online.</p>");
        return out;
    }
    online.sort_by_key(|a| (a.role != Role::Reviewer, a.busy.is_none(), a.name.clone()));
    let _ = write!(
        out,
        "<details id=\"agents\" class=\"online\"><summary>{CHEVRON}{}</summary><ul class=\"agents\">",
        availability(&online)
    );
    for a in &online {
        let what = match (&a.busy, a.role) {
            (Some(id), _) => match st.review(id) {
                Some(r) => format!("on <a href=\"/r/{}\">{}#{}</a>", esc(&r.id), esc(&r.pr.repo), r.pr.number),
                None => format!("on {}", esc(id)),
            },
            (None, Role::Reviewer) => "idle".into(),
            (None, Role::Author) => "author".into(),
        };
        let _ = write!(out, "<li><b>{}</b><span class=\"role\">{what}</span></li>", esc(&a.name));
    }
    out.push_str("</ul></details>");
    out
}

/// How many PRs have a review running, for the index's tab title.
fn running(st: &State) -> usize {
    st.reviews.iter().filter(|r| r.phase.active()).map(|r| &r.pr).collect::<HashSet<_>>().len()
}

/// The connect instructions as separate blocks; `/setup` keeps its plain-text form.
fn connect(url: &str, login: &str, open: bool) -> String {
    let claude = esc(&crate::server::default_agent(login, "claude"));
    format!(
        "<h2 class=\"sr\">Connect an agent</h2><details id=\"connect\" class=\"connect\"{open}><summary>{CHEVRON}Connect an agent</summary>\
         <h3>Claude Code</h3><pre><code>{add}</code></pre>\
         <h3>Codex</h3><p>In <code>~/.codex/config.toml</code>, replace any <code>[mcp_servers.amx]</code> table with:</p><pre><code>{toml}</code></pre>\
         <h3>Then</h3><p>Agents are named <code>{claude}</code> and <code>{codex}</code>; a second session becomes <code>{claude}-2</code>. \
         To pick a name, use <code>{u}/mcp?agent=NAME</code>.</p>\
         <p>To make a reviewer, tell the agent: \u{201c}Call amx hello with role reviewer, then loop on amx wait and do what it says.\u{201d}</p>\
         <p>For everyone on a repo, commit this as <code>.mcp.json</code> (Claude Code), or the table above as <code>.codex/config.toml</code> (Codex):</p>\
         <pre><code>{json}</code></pre></details>",
        open = if open { " open" } else { "" },
        u = esc(url),
        add = esc(&crate::text::claude_add(url)),
        toml = esc(&crate::text::codex_toml(url)),
        json = esc(&crate::text::mcp_json(url)),
        codex = esc(&crate::server::default_agent(login, "codex")),
    )
}

/// What a finding asks of the human, in page order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    /// Confirmed blocking or major, or still present.
    Fix,
    /// Disputed, unverified or undecided, or still being voted on.
    Judge,
    /// Confirmed minor or nit.
    Optional,
    /// Carried over and voted down.
    Fixed,
    /// Rejected, duplicate or withdrawn.
    Closed,
}

fn rank(f: &Finding) -> Rank {
    if f.withdrawn.is_some() {
        return Rank::Closed;
    }
    match f.outcome.as_ref().map(|o| (&o.status, o.severity)) {
        Some((Status::Confirmed, Severity::Blocking | Severity::Major)) => Rank::Fix,
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

/// The finding's heading line and the rest of its text. A first line that starts a paragraph is the
/// heading; otherwise the first paragraph's first line is, and the body is the whole text.
fn heading(text: &str) -> (Option<&str>, &str) {
    use pulldown_cmark::{Event, Parser, Tag};
    let text = text.trim();
    let mut depth = 0;
    for (e, at) in Parser::new_ext(text, options()).into_offset_iter() {
        match e {
            Event::Start(Tag::Paragraph) if depth == 0 => {
                let line = text[at.start..].lines().next().unwrap_or("").trim_end();
                return (Some(line), if at.start == 0 { text[line.len()..].trim() } else { text });
            }
            Event::Start(_) => depth += 1,
            Event::End(_) => depth -= 1,
            _ => {}
        }
    }
    (None, text)
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

fn render_finding(out: &mut String, r: &Review, f: &Finding) {
    let rank = rank(f);
    let sev = severity(f);
    let disputed = matches!(f.outcome.as_ref().map(|o| &o.status), Some(Status::Disputed | Status::Unverified));
    let open = match rank {
        Rank::Fix => true,
        Rank::Judge => f.outcome.is_some() || !r.phase.active() || sev <= Severity::Major,
        _ => false,
    };
    let mark = match outcome(f) {
        Some((_, label)) if label == "confirmed" => String::new(),
        Some((_, label)) if label == "resolved" => "<span class=\"badge s-approved\">fixed</span>".into(),
        Some((cls, label)) => format!("<span class=\"badge s-{cls}\">{}</span>", esc(&label)),
        None if r.phase.active() && f.by.is_none() => "<span class=\"badge s-waiting\">re-checking</span>".into(),
        None if r.phase.active() => String::new(),
        None => "<span class=\"badge s-undecided\">undecided</span>".into(),
    };
    let new = if r.parent.is_some() && f.by.is_some() { "<span class=\"badge s-new\">new</span>" } else { "" };
    let raised = match &f.outcome {
        Some(o) if o.severity != f.severity => format!("<span class=\"was\">raised {}</span>", f.severity.as_str()),
        _ => String::new(),
    };
    let (head, body) = heading(&f.text);
    let head = head.map_or_else(|| format!("Finding {}", esc(&f.id)), inline);
    let _ = write!(
        out,
        "<details class=\"f{dim}\" id=\"{id}\"{open}><summary><span class=\"sev sev-{sev}\">{sev}</span>\
         <span class=\"ft\" dir=\"auto\">{head}</span><span class=\"fx\">{raised}{new}{mark}<span class=\"fid\">{id}</span></span>{CHEVRON}</summary><div class=\"fb\">",
        dim = if rank >= Rank::Fixed { " dim" } else { "" },
        id = esc(&f.id),
        open = if open { " open" } else { "" },
        sev = sev.as_str(),
    );
    if !body.is_empty() {
        let _ = write!(out, "<div class=\"md\" dir=\"auto\">{}</div>", markdown(body));
    }
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
fn render_findings(out: &mut String, r: &Review) {
    if r.findings.is_empty() {
        return;
    }
    let mut shown: Vec<(usize, &Finding)> = visible(r).enumerate().collect();
    shown.sort_by_key(|&(i, f)| (rank(f), severity(f), i));
    let hidden = r.findings.len() - shown.len();
    let n = shown.iter().filter(|(_, f)| rank(f) < Rank::Fixed).count() + hidden;
    let count = if n > 0 { format!(" <span class=\"count\">{n}</span>") } else { String::new() };
    let since = since_link(r).map(|l| format!(" <span class=\"aside\">{l}</span>")).unwrap_or_default();
    let _ = write!(out, "<h2 id=\"findings\">Findings{count}{since}</h2>");
    if hidden > 0 {
        let _ = write!(out, "<p class=\"empty\">{hidden} hidden until every reviewer submits.</p>");
    }
    if !shown.is_empty() {
        out.push_str("<div class=\"findings\">");
        for (_, f) in shown {
            render_finding(out, r, f);
        }
        out.push_str("</div>");
    }
}

fn waiting_for(who: Vec<&String>, to: &str) -> String {
    if who.is_empty() { "Waiting for the deadline".into() } else { format!("Waiting for {} to {to}", names(&who)) }
}

fn render_verdict(out: &mut String, r: &Review, old: bool, now: Ms) {
    let (cls, label) = status(r);
    let detail = match r.phase {
        Phase::Done => {
            let (fix, decide, optional) = to_do(r);
            let fixed = r.findings.iter().filter(|f| rank(f) == Rank::Fixed).count();
            let mut parts = Vec::new();
            if fix > 0 {
                parts.push(format!("{fix} to fix"));
            }
            if decide > 0 {
                parts.push(format!("<span class=\"needs\">{decide} for you to decide</span>"));
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
        Phase::Failed => esc(&format!("{} The author can request it again.", failure(r).1)),
        Phase::Offering => {
            let left = r.params.reviewers.saturating_sub(r.active_slots().count());
            esc(&format!("Waiting for {} to accept. {}", plural(&left.to_string(), "reviewer"), asked(r)))
        }
        Phase::Reviewing => esc(&waiting_for(r.active_slots().filter(|s| s.submission.is_none()).map(|s| &s.agent).collect(), "submit")),
        Phase::Voting(_) if r.replying => esc(&waiting_for(repliers(r), "reply")),
        Phase::Voting(_) => esc(&waiting_for(r.active_slots().filter(|s| !r.owed_votes(&s.agent).is_empty()).map(|s| &s.agent).collect(), "vote")),
    };
    let _ = write!(
        out,
        "<section class=\"verdict v-{cls}{old}\" aria-label=\"Status\"><p class=\"vt\">{label}</p><p class=\"vd\">{detail}</p><p class=\"vw\">{}</p></section>",
        when(r, now),
        old = if old { " v-old" } else { "" },
    );
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
                format!("<span class=\"badge s-{cls}\">{label}</span>{sha}")
            }
            (None, None) if r.phase == Phase::Offering => "<span class=\"badge s-waiting\">accepted</span>".into(),
            (None, None) => "<span class=\"badge s-waiting\">reviewing</span>".into(),
        };
        let summary = s
            .submission
            .as_ref()
            .filter(|x| !x.summary.is_empty() && r.revealed())
            .map(|x| format!("<div class=\"md\" dir=\"auto\">{}</div>", markdown(&x.summary)))
            .unwrap_or_default();
        let _ = write!(out, "<li><div class=\"who\"><b>{}</b> {state}</div>{summary}</li>", esc(&s.agent));
    }
    out.push_str("</ul>");
}

/// A review in a few words, for its index row and the PR's history.
fn gist(r: &Review) -> String {
    match r.phase {
        Phase::Offering => format!("{} of {} accepted", r.active_slots().count(), r.params.reviewers),
        Phase::Reviewing => format!("{} of {} submitted", r.active_slots().filter(|s| s.submission.is_some()).count(), r.active_slots().count()),
        Phase::Voting(_) if r.replying => format!("{} to reply", repliers(r).len()),
        Phase::Voting(n) => {
            let voted = |a: &String| r.findings.iter().any(|f| f.votes.get(a).is_some_and(|v| v.round == n));
            let owing = r.active_slots().filter(|s| !r.owed_votes(&s.agent).is_empty()).count();
            let done = r.active_slots().filter(|s| r.owed_votes(&s.agent).is_empty() && voted(&s.agent)).count();
            format!("{done} of {} voted", done + owing)
        }
        Phase::Failed => failure(r).0.into(),
        Phase::Done => {
            let (fix, decide, _) = to_do(r);
            match (fix, decide) {
                (0, 0) if r.findings.is_empty() => "no findings".into(),
                (0, 0) => "nothing to fix".into(),
                (f, 0) => format!("{f} to fix"),
                (0, d) => format!("{d} to decide"),
                (f, d) => format!("{f} to fix · {d} to decide"),
            }
        }
    }
}

/// On an old review, one line naming the PR's latest review and, if that one hasn't finished, the last result.
fn render_newer(out: &mut String, r: &Review, prs: &[Review]) {
    let Some(latest) = prs.last().filter(|l| l.id != r.id) else {
        return;
    };
    let last = prs
        .iter()
        .filter(|x| x.phase == Phase::Done && latest.phase.active())
        .max_by_key(|x| x.ended)
        .map(|x| format!("&nbsp;· last result: {}", status(x).1.to_lowercase()))
        .unwrap_or_default();
    let _ = write!(
        out,
        "<p class=\"newer\">Newer review: <a href=\"/r/{0}\">{0}</a> ({1}){last}</p>",
        esc(&latest.id),
        status(latest).1.to_lowercase(),
    );
}

fn render_history(out: &mut String, r: &Review, prs: &[Review], now: Ms) {
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
        let (cls, label) = status(x);
        let _ = write!(
            out,
            "<li{current}><span class=\"h-id\">{id}</span><span class=\"st s-{cls}\">{label}</span><span class=\"h-gist\">{}</span><span class=\"h-when\">{}</span></li>",
            esc(&gist(x)),
            when(x, now),
        );
    }
    out.push_str("</ol>");
}

/// Consensus settings that differ from the defaults, in a few words each.
fn unusual(r: &Review) -> Vec<String> {
    let (c, d) = (r.params.consensus, crate::config::Consensus::default());
    let mut v = Vec::new();
    if c.vote_rounds != d.vote_rounds {
        v.push(format!("{} voting round{}", c.vote_rounds, if c.vote_rounds == 1 { "" } else { "s" }));
    }
    if c.confirm_fraction != d.confirm_fraction {
        v.push(format!("confirmed above {:.0}%", c.confirm_fraction * 100.0));
    }
    if c.blocking_unanimous != d.blocking_unanimous {
        v.push("blocking by majority".into());
    }
    v
}

/// The page as seen on the day the review was requested.
pub fn render_review(r: &Review) -> String {
    render_review_page(r, &[], r.created)
}

/// `prs` are all reviews of the same PR, this one included, oldest first.
pub fn render_review_page(r: &Review, prs: &[Review], now: Ms) -> String {
    let mut meta = vec![format!("{} by <b>{}</b>", esc(&r.id), esc(&r.author))];
    if let Some(h) = &r.head_sha {
        meta.push(format!("<code>{}</code>", esc(short_sha(h))));
    }
    meta.extend(unusual(r));
    meta.push(format!("requested {}", time_tag(r.created, now)));
    let mut out = String::new();
    render_newer(&mut out, r, prs);
    let _ = write!(
        out,
        "<h1><a href=\"{url}\" rel=\"noopener noreferrer\" target=\"_blank\">{short}<span class=\"ext\">{EXTERNAL}</span></a></h1>\
         <p class=\"meta\">{meta}</p>",
        url = esc(&r.pr.url()),
        short = esc(&r.pr.short()),
        meta = dotted(&meta),
    );
    render_verdict(&mut out, r, prs.last().is_some_and(|l| l.id != r.id), now);
    render_findings(&mut out, r);
    if !r.note.is_empty() {
        let _ = write!(out, "<div class=\"note\"><span class=\"note-by\">{}:</span><div class=\"md\" dir=\"auto\">{}</div></div>", esc(&r.author), markdown(&r.note));
    }
    render_discussion(&mut out, r);
    render_reviewers(&mut out, r);
    render_history(&mut out, r, prs, now);
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
fn render_discussion(out: &mut String, r: &Review) {
    let says: Vec<&LogEntry> = r.log.iter().filter(|e| e.kind == "say").collect();
    if !says.is_empty() {
        let _ = write!(out, "<h2>Discussion <span class=\"count\">{}</span></h2><ol class=\"talk\">", says.len());
        for e in says {
            let _ = write!(
                out,
                "<li><div class=\"th\"><b>{}</b> {}</div><div class=\"md\" dir=\"auto\">{}</div></li>",
                esc(&e.who),
                clock_tag(e.at),
                markdown(&e.text)
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
        "submit" => {
            let (n, sha) = t.strip_prefix("submitted ")?.split_once(" finding(s) at ")?;
            Some(format!("<b>{}</b> submitted {} at <code>{}</code>", esc(&e.who), plural(n, "finding"), esc(short_sha(sha))))
        }
        _ => None,
    }
}

fn plural(n: &str, noun: &str) -> String {
    if n == "1" { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

fn summary_json(r: &Review, f: &Finding) -> Value {
    let (agree, disagree) = votes(f);
    json!({
        "id": f.id,
        "title": title(&f.text),
        "severity": f.outcome.as_ref().map(|o| o.severity).unwrap_or(f.severity),
        "severity_split": split_range(f),
        "by": f.by,
        "origin": f.origin,
        "outcome": outcome(f).map(|(_, l)| l).unwrap_or_else(|| if r.phase.active() { "open".into() } else { "undecided".into() }),
        "agree": agree,
        "disagree": disagree,
    })
}

fn list_json(r: &Review, now: Ms) -> Value {
    json!({
        "id": r.id,
        "pr": r.pr.url(),
        "repo": r.pr.short(),
        "author": r.author,
        "phase": phase_badge(r.phase),
        "round": r.round(),
        "phase_label": r.phase.label(),
        "active": r.phase.active(),
        "created": r.created,
        "ended": r.ended,
        "deadline": r.phase.active().then_some(r.deadline),
        "deadline_left_ms": r.phase.active().then(|| r.deadline_left(now)),
        "verdict": r.result.as_ref().map(|x| x.verdict),
        "failure": r.failure,
        "parent": r.parent,
        "reviewers": r.slots.iter().map(|s| &s.agent).collect::<Vec<_>>(),
        "findings": r.findings.len(),
        "path": format!("/r/{}", r.id),
    })
}

/// The whole review as JSON, with what the page hides before every reviewer has submitted left out.
pub fn review_json(r: &Review, rereviews: &[String], now: Ms) -> Value {
    let mut v = serde_json::to_value(r).unwrap_or(Value::Null);
    let Some(m) = v.as_object_mut() else {
        return v;
    };
    m.remove("next_finding");
    if !r.revealed() {
        let shown: Vec<Value> = visible(r).filter_map(|f| serde_json::to_value(f).ok()).collect();
        m.insert("findings".into(), Value::Array(shown));
        if let Some(slots) = m.get_mut("slots").and_then(Value::as_array_mut) {
            for s in slots {
                if let Some(sub) = s.get_mut("submission").and_then(Value::as_object_mut) {
                    sub.insert("summary".into(), Value::String(String::new()));
                    sub.insert("verdict".into(), Value::Null);
                }
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
    m.insert("hidden_findings".into(), json!(r.findings.len() - visible(r).count()));
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
    v.sort_by_key(|x| x.created);
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

pub async fn index(AxState(hub): AxState<Arc<Hub>>, who: Option<Extension<Identity>>, headers: HeaderMap) -> Response {
    let (mut main, running, quiet) =
        hub.read(|s, now| (render_index(s, now), running(s), s.reviews.is_empty() || s.online(now).is_empty()));
    if let Some(Extension(who)) = who {
        main.push_str(&connect(&hub.web_url(), &who.login, quiet));
    }
    let title = if running == 0 { "amx".to_string() } else { format!("amx · {running} running") };
    let main = titled(&title, &main);
    if partial(&headers) {
        return fragment(&headers, main);
    }
    Html(page(&title, &main, true)).into_response()
}

pub async fn review_page(
    AxState(hub): AxState<Arc<Hub>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some((r, prs, now)) = hub.read(|s, now| s.review(&id).map(|r| (r.clone(), siblings(s, r), now))) else {
        let main = format!(
            "<h1>No review {}</h1><p class=\"empty\">The relay may have restarted. <a href=\"/\">All reviews</a></p>",
            esc(&id)
        );
        let body = if partial(&headers) { titled("amx", &main) } else { page("amx", &main, false) };
        return no_store((StatusCode::NOT_FOUND, Html(body)).into_response());
    };
    let title = format!("{} · {}", status(&r).1, r.pr.short());
    let main = titled(&title, &render_review_page(&r, &prs, now));
    if partial(&headers) {
        return fragment(&headers, main);
    }
    Html(page(&title, &main, true)).into_response()
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
        Some((r, children, now)) => no_store(Json(review_json(&r, &children, now)).into_response()),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": format!("no review {id}") }))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(heading("Off by one\n\nThe loop ends early."), (Some("Off by one"), "The loop ends early."));
        assert_eq!(heading("Uses `a.b()` twice. Cache it."), (Some("Uses `a.b()` twice. Cache it."), ""));
        assert_eq!(heading("No stop here\nand more"), (Some("No stop here"), "and more"));
        let link = "See [foo. bar](https://example.invalid/x) for the problem. Second sentence.";
        assert_eq!(heading(link), (Some(link), ""), "never cut at a full stop");
        assert_eq!(heading("Use e.g. a mutex here. The rest of the text."), (Some("Use e.g. a mutex here. The rest of the text."), ""));
    }

    #[test]
    fn headings_never_split_a_block() {
        let fence = "```rust\nfn main() { let x = 1; }\n```\nCode block first.";
        assert_eq!(heading(fence), (Some("Code block first."), fence), "the first plain line, the body unchanged");
        let list = "1. ordered first\n2. ordered second";
        assert_eq!(heading(list), (None, list));
        let table = "| a | b |\n|---|---|\n| 1 | 2 |";
        assert_eq!(heading(table), (None, table));
        let quoted = "> quoted\n\n## Title\n\n- item\n\nThen prose.";
        assert_eq!(heading(quoted), (Some("Then prose."), quoted));
        assert_eq!(heading("Setext title\n---\nbody"), (Some("body"), "Setext title\n---\nbody"));
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
