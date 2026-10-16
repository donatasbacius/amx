use crate::hub::Hub;
use crate::relay::{Choice, Finding, Ms, Phase, Review, Severity, State, Status, Verdict};
use crate::text::{bidi, dur, iso, phase_badge, short_sha, split_note, split_range};
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
    use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd, html};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let mut events = Vec::new();
    let mut image = None;
    let src: String = src.chars().filter(|&c| !bidi(c)).collect();
    for e in Parser::new_ext(&src, opts) {
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

fn safe_url(u: &str) -> Option<&str> {
    u.starts_with("https://github.com/").then_some(u)
}

/// Times and countdowns are rendered as timestamps and made relative by the page script, so the
/// page only changes when the review does.
fn time_tag(ms: Ms) -> String {
    let iso = iso(ms);
    format!("<time datetime=\"{}\">{iso}</time>", iso.replace(' ', "T"))
}

fn countdown(deadline: Ms) -> String {
    format!("<span class=\"left\" data-deadline=\"{deadline}\">until {}</span>", iso(deadline))
}

/// The review's state as a pill class and label.
fn status(r: &Review) -> (&'static str, String) {
    match r.phase {
        Phase::Offering => ("waiting", "Finding reviewers".into()),
        Phase::Reviewing => ("inreview", "Reviewing".into()),
        Phase::Voting(n) => ("voting", format!("Voting, round {n} of {}", r.params.consensus.vote_rounds.max(n))),
        Phase::Done if r.result.as_ref().is_some_and(|x| x.verdict == Verdict::Approve) => ("approved", "Approved".into()),
        Phase::Done => ("changes", "Changes requested".into()),
        Phase::Failed => ("failed", "Failed".into()),
    }
}

fn pill(r: &Review) -> String {
    let (cls, label) = status(r);
    format!("<span class=\"pill s-{cls}\">{label}</span>")
}

fn page(title: &str, main: &str) -> String {
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light dark">
<title>{title}</title>
<link rel="stylesheet" href="/static/style.css">
<script src="/static/app.js" defer></script>
</head>
<body>
<a class="skip" href="#main">Skip to content</a>
<header class="top"><a class="brand" href="/">amx</a><span id="live" class="live" role="status" aria-live="polite"></span></header>
<main id="main" data-etag='{tag}'>{main}</main>
</body>
</html>"##,
        title = esc(title),
        tag = etag(main),
    )
}

/// The outcome of a finding as a badge class and label, or `None` while it is still open.
fn outcome(f: &Finding) -> Option<(&'static str, String)> {
    let Some(o) = &f.outcome else {
        return f.withdrawn.is_some().then(|| ("closed", "withdrawn".to_string()));
    };
    let carried = f.by.is_none();
    Some(match &o.status {
        Status::Confirmed if carried => ("changes", "still present".to_string()),
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

fn step_name(name: &str) -> String {
    match name {
        "Independent review" => "Review".into(),
        "Cross-examination" => "Voting".into(),
        "Consensus result" => "Result".into(),
        n => n.strip_prefix("Cross-examination round ").map_or_else(|| n.to_string(), |k| format!("Voting round {k}")),
    }
}

fn render_timeline(out: &mut String, r: &Review) {
    out.push_str("<h2 id=\"progress\">Progress</h2><ol class=\"steps\">");
    for s in timeline(r) {
        let info = match (s.state, s.start, s.end, s.deadline) {
            (_, _, _, Some(d)) => countdown(d),
            ("skipped", ..) => "skipped".into(),
            (_, Some(a), Some(b), _) if s.name != "Consensus result" && s.name != "Failed" => dur(b.saturating_sub(a)),
            (_, Some(a), None, _) if !r.phase.active() => time_tag(a),
            _ => String::new(),
        };
        let title = s.start.map(|a| format!(" title=\"started {}\"", iso(a))).unwrap_or_default();
        let current = if s.state == "active" { " aria-current=\"step\"" } else { "" };
        let notes: String = s.notes.iter().map(|n| format!("<span class=\"note\">{}</span>", esc(n))).collect();
        let _ = write!(
            out,
            "<li class=\"st-{state}\"{current}{title}><span class=\"name\">{name}</span><span class=\"info\">{info}</span>{notes}</li>",
            state = if s.name == "Failed" { "failed" } else { s.state },
            name = esc(&step_name(&s.name)),
        );
    }
    out.push_str("</ol>");
}

fn confirmed(r: &Review) -> usize {
    visible(r)
        .filter(|f| f.outcome.as_ref().is_some_and(|o| o.status == Status::Confirmed))
        .count()
}

fn review_row(out: &mut String, r: &Review, count: usize) {
    let n = visible(r).count();
    let what = match r.phase {
        Phase::Offering => format!("offered to {}", r.offered.len()),
        Phase::Reviewing => format!(
            "{} of {} submitted",
            r.active_slots().filter(|s| s.submission.is_some()).count(),
            r.active_slots().count()
        ),
        Phase::Failed => format!("{}, author must ask again", r.failure.as_deref().map_or("failed", |f| f.split(':').next().unwrap_or(f))),
        _ if n == 0 => "no findings".into(),
        Phase::Voting(_) => format!("{n} finding{}", if n == 1 { "" } else { "s" }),
        Phase::Done => match confirmed(r) {
            0 => "none confirmed".into(),
            c if r.result.as_ref().is_some_and(|x| x.verdict == Verdict::ChangesRequested) => format!("{c} to fix"),
            c => format!("{c} of {n} confirmed"),
        },
    };
    let more = if count > 1 { format!(" · {count} reviews") } else { String::new() };
    let when = if r.phase.active() { countdown(r.deadline) } else { time_tag(r.ended.unwrap_or(r.created)) };
    let _ = write!(
        out,
        "<li><a class=\"rv\" href=\"/r/{id}\">{pill}<span class=\"rv-pr\">{pr}</span>\
         <span class=\"rv-meta\">{id} · {author} · {what}{more}</span><span class=\"rv-when\">{when}</span></a></li>",
        id = esc(&r.id),
        pill = pill(r),
        pr = esc(&r.pr.short()),
        author = esc(&r.author),
        what = esc(&what),
    );
}

pub fn render_index(st: &State, now: Ms) -> String {
    let mut out = String::from("<h1>Pull requests</h1>");
    let mut reviews: Vec<&Review> = st.reviews.iter().collect();
    reviews.sort_by_key(|r| std::cmp::Reverse(r.created));
    let count = |r: &Review| st.reviews.iter().filter(|x| x.pr == r.pr).count();
    let mut seen = HashSet::new();
    reviews.retain(|r| seen.insert(&r.pr));
    reviews.sort_by_key(|r| std::cmp::Reverse(r.ended.unwrap_or(r.created)));
    let (active, done): (Vec<&Review>, Vec<&Review>) = reviews.into_iter().partition(|r| r.phase.active());
    if active.is_empty() && done.is_empty() {
        out.push_str("<p class=\"empty\">No reviews yet.</p>");
    }
    for (label, list) in [("In progress", active), ("Finished", done)] {
        if list.is_empty() {
            continue;
        }
        let _ = write!(out, "<h2>{label} <span class=\"count\">{}</span></h2><ul class=\"reviews\">", list.len());
        for r in list {
            review_row(&mut out, r, count(r));
        }
        out.push_str("</ul>");
    }
    let online = st.online(now);
    let _ = write!(out, "<h2>Online agents <span class=\"count\">{}</span></h2>", online.len());
    if online.is_empty() {
        out.push_str("<p class=\"empty\">Nobody online.</p>");
    } else {
        out.push_str("<ul class=\"agents\">");
        for a in &online {
            let busy = a
                .busy
                .as_deref()
                .map(|id| format!(" <a href=\"/r/{0}\">on {0}</a>", esc(id)))
                .unwrap_or_default();
            let _ = write!(
                out,
                "<li><b>{name}</b> <span class=\"role\">{role}</span>{busy}</li>",
                name = esc(&a.name),
                role = a.role.as_str(),
            );
        }
        out.push_str("</ul>");
    }
    out
}

/// Sort and group order: what needs attention first, dismissed findings last.
fn rank(f: &Finding) -> u8 {
    match f.outcome.as_ref().map(|o| &o.status) {
        Some(Status::Confirmed) => 0,
        Some(Status::Disputed | Status::Unverified) => 1,
        None if f.withdrawn.is_none() => 2,
        Some(Status::Rejected) => 3,
        Some(Status::Duplicate(_)) => 4,
        _ => 5,
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
        return Some(format!("Raised as {}; {who} rated it {rated}, so the lower ({}) stands.", f.severity.as_str(), o.severity.as_str()));
    }
    if let Some(split) = split_note(f) {
        return Some(format!("Severity {split}; the lower stands."));
    }
    let raised = f.severity.as_str();
    if o.downgraded {
        Some(format!("Raised as {raised}; not unanimous, so {}.", o.severity.as_str()))
    } else if o.severity != f.severity {
        Some(format!("Raised as {raised}; voters rated it {}.", o.severity.as_str()))
    } else {
        None
    }
}

/// The finding's heading and what is left to show under it: a short first line set apart by a blank
/// line, else the first sentence, which is not repeated below unless it had to be shortened.
fn heading(text: &str) -> (String, &str) {
    let text = text.trim();
    if let Some((first, rest)) = text.split_once("\n\n").or_else(|| text.split_once("\r\n\r\n"))
        && !first.contains('\n')
        && first.chars().count() <= 100
    {
        return (title(first), rest.trim());
    }
    let para = text.split("\n\n").next().unwrap_or(text);
    let end = para
        .match_indices(['.', '?', '!'])
        .map(|(i, _)| i + 1)
        .find(|&i| para[i..].starts_with([' ', '\n']) && para[..i].matches('`').count().is_multiple_of(2))
        .unwrap_or(para.len());
    let sentence = &text[..end];
    if sentence.contains('\n') || sentence.chars().count() > 100 {
        return (title(text), text);
    }
    (title(sentence), text[end..].trim())
}

fn render_finding(out: &mut String, r: &Review, f: &Finding, open: bool) {
    let sev = severity(f).as_str();
    let badge = match outcome(f) {
        Some((cls, label)) => {
            let cls = if cls == "changes" { "confirmed" } else { cls };
            format!("<span class=\"badge s-{cls}\">{}</span>", esc(&label))
        }
        None if r.phase.active() => String::new(),
        None => "<span class=\"badge s-closed\">undecided</span>".into(),
    };
    let (agree, disagree) = votes(f);
    let tally = if agree + disagree == 0 {
        String::new()
    } else {
        format!(
            "<span class=\"tally\" title=\"{agree} agree, {disagree} disagree\"><span class=\"sr\">{agree} agree, {disagree} disagree</span>\
             <span aria-hidden=\"true\">✓{agree} ✗{disagree}</span></span>"
        )
    };
    let (head, body) = heading(&f.text);
    let _ = write!(
        out,
        "<details class=\"f\" id=\"{id}\"{open}><summary><span class=\"sev sev-{sev}\">{sev}</span>\
         <span class=\"ft\">{head}</span><span class=\"fx\">{badge}{tally}<span class=\"fid\">{fid}</span></span></summary><div class=\"fb\">",
        id = esc(&f.id),
        fid = esc(&f.origin.as_ref().map_or(f.id.clone(), |o| format!("{} was {o}", f.id))),
        open = if open { " open" } else { "" },
        head = esc(&head),
    );
    if !body.is_empty() {
        let _ = write!(out, "<div class=\"md prose\">{}</div>", markdown(body));
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
    if !f.votes.is_empty() {
        let rounds = f.votes.values().any(|v| v.round > 1);
        out.push_str("<ul class=\"votes\">");
        for (who, v) in &f.votes {
            let (cls, what) = match &v.choice {
                Choice::Agree => match v.severity.filter(|&s| s != f.severity) {
                    Some(s) => ("agree", format!("agrees, as {}", s.as_str())),
                    None => ("agree", "agrees".to_string()),
                },
                Choice::Disagree => ("disagree", "disagrees".to_string()),
                Choice::Duplicate(t) => ("dup", format!("duplicate of {t}")),
            };
            let what = if v.revision < f.revision { format!("{what} (on an earlier revision, not counted)") } else { what };
            let round = if rounds { format!(" <span class=\"muted\">· round {}</span>", v.round) } else { String::new() };
            let reason = v.reason.as_deref().map(|r| format!("<div class=\"md prose\">{}</div>", markdown(r))).unwrap_or_default();
            let _ = write!(out, "<li class=\"v-{cls}\"><b>{}</b> {}{round}{reason}</li>", esc(who), esc(&what));
        }
        out.push_str("</ul>");
    }
    out.push_str("</div></details>");
}

fn render_findings(out: &mut String, r: &Review) {
    let mut shown: Vec<(usize, &Finding)> = visible(r).enumerate().collect();
    shown.sort_by_key(|(i, f)| (rank(f), severity(f), *i));
    let hidden = r.findings.len() - shown.len();
    if r.findings.is_empty() && matches!(r.phase, Phase::Done | Phase::Failed) {
        return;
    }
    let _ = write!(out, "<h2 id=\"findings\">Findings <span class=\"count\">{}</span></h2>", r.findings.len());
    if hidden > 0 {
        let _ = write!(out, "<p class=\"empty\">{hidden} hidden until every reviewer has submitted.</p>");
    } else if shown.is_empty() {
        out.push_str("<p class=\"empty\">None yet.</p>");
    }
    let (main, closed): (Vec<&Finding>, Vec<&Finding>) = shown.into_iter().map(|(_, f)| f).partition(|f| rank(f) < 3);
    let open = main.len() <= 4;
    if !main.is_empty() {
        out.push_str("<div class=\"findings\">");
        for f in &main {
            render_finding(out, r, f, open);
        }
        out.push_str("</div>");
    }
    if !closed.is_empty() {
        if !main.is_empty() {
            let _ = write!(out, "<h3 class=\"sub\">Closed <span class=\"count\">{}</span></h3>", closed.len());
        }
        out.push_str("<div class=\"findings dim\">");
        for f in &closed {
            render_finding(out, r, f, false);
        }
        out.push_str("</div>");
    }
}

fn render_verdict(out: &mut String, r: &Review) {
    let (cls, label) = status(r);
    let detail = match r.phase {
        Phase::Done => {
            let mut sorted: Vec<&Finding> = r.findings.iter().collect();
            sorted.sort_by_key(|f| (rank(f), severity(f)));
            let mut counts: Vec<(String, Vec<Severity>)> = Vec::new();
            for f in sorted {
                if let Some((_, l)) = outcome(f) {
                    match counts.iter_mut().find(|(x, _)| *x == l) {
                        Some((_, v)) => v.push(severity(f)),
                        None => counts.push((l, vec![severity(f)])),
                    }
                }
            }
            if counts.is_empty() {
                "No findings".to_string()
            } else {
                let parts: Vec<String> = counts
                    .iter()
                    .map(|(l, sevs)| {
                        let mut s = format!("{} {l}", sevs.len());
                        if l == "confirmed" || l == "still present" {
                            let mut by: Vec<(Severity, usize)> = Vec::new();
                            for &x in sevs {
                                match by.iter_mut().find(|(y, _)| *y == x) {
                                    Some((_, n)) => *n += 1,
                                    None => by.push((x, 1)),
                                }
                            }
                            let by: Vec<String> = by.iter().map(|(x, n)| format!("{n} {}", x.as_str())).collect();
                            let _ = write!(s, " ({})", by.join(", "));
                        }
                        s
                    })
                    .collect();
                parts.join(" · ")
            }
        }
        Phase::Failed => {
            let f = r.failure.as_deref().unwrap_or("");
            let mut c = f.chars();
            c.next().map(|x| x.to_uppercase().chain(c).collect()).unwrap_or_default()
        }
        Phase::Offering => format!("Offered to {}", r.offered.len()),
        Phase::Reviewing => format!(
            "{} of {} reviewers submitted",
            r.active_slots().filter(|s| s.submission.is_some()).count(),
            r.active_slots().count()
        ),
        Phase::Voting(_) if r.replying => "Waiting for replies on contested findings".into(),
        Phase::Voting(_) => {
            let owing = r.active_slots().filter(|s| !r.owed_votes(&s.agent).is_empty()).count();
            format!("{owing} of {} reviewers still to vote", r.active_slots().count())
        }
    };
    let when = match (&r.result, r.phase.active()) {
        (_, true) => countdown(r.deadline),
        (Some(res), _) => time_tag(res.at),
        (None, _) => r.ended.map(time_tag).unwrap_or_default(),
    };
    let _ = write!(
        out,
        "<section class=\"verdict v-{cls}\" aria-label=\"Status\"><p class=\"vt\">{label}</p><p class=\"vd\">{detail}</p><p class=\"vw\">{when}</p></section>",
        detail = esc(&detail),
    );
}

fn render_reviewers(out: &mut String, r: &Review) {
    let _ = write!(out, "<h2>Reviewers <span class=\"count\">{}</span></h2><ul class=\"reviewers\">", r.active_slots().count());
    if r.slots.is_empty() {
        let declined = if r.declined.is_empty() { String::new() } else { format!("; {} declined", r.declined.len()) };
        let n = r.offered.len();
        let _ = write!(out, "<li class=\"muted\">Offered to {n} agent{}{declined}.</li>", if n == 1 { "" } else { "s" });
    }
    for s in &r.slots {
        let state = match (&s.dropped, &s.submission) {
            (Some(d), _) => format!("<span class=\"badge s-closed\">dropped</span> <span class=\"muted\">{}</span>", esc(d)),
            (None, Some(x)) => {
                let (cls, label) = match x.verdict {
                    _ if !r.revealed() => ("waiting", "submitted"),
                    Verdict::Approve => ("approved", "approves"),
                    Verdict::ChangesRequested => ("changes", "requests changes"),
                };
                let sha = if r.head_sha.as_deref().map(short_sha) == Some(short_sha(&x.head_sha)) {
                    String::new()
                } else {
                    format!(" <span class=\"muted\">at</span> <code>{}</code>", esc(short_sha(&x.head_sha)))
                };
                format!("<span class=\"badge s-{cls}\">{label}</span>{sha}")
            }
            (None, None) => "<span class=\"badge s-waiting\">reviewing</span>".into(),
        };
        let login = if s.login == "local" { String::new() } else { format!(" <span class=\"muted\">{}</span>", esc(&s.login)) };
        let summary = s
            .submission
            .as_ref()
            .filter(|x| !x.summary.is_empty() && r.revealed())
            .map(|x| format!("<div class=\"md prose\">{}</div>", markdown(&x.summary)))
            .unwrap_or_default();
        let _ = write!(out, "<li><div class=\"who\"><b>{}</b>{login} {state}</div>{summary}</li>", esc(&s.agent));
    }
    out.push_str("</ul>");
}

pub fn render_review(r: &Review) -> String {
    render_review_page(r, &[])
}

/// `others` are the other reviews of the same PR, oldest first, as id and status.
pub fn render_review_page(r: &Review, others: &[(String, String)]) -> String {
    let mut meta = vec![format!("{} by <b>{}</b>", esc(&r.id), esc(&r.author))];
    if let Some(h) = &r.head_sha {
        meta.push(format!("<code>{}</code>", esc(short_sha(h))));
    }
    if let (Some(old), Some(new)) = (&r.since_sha, &r.head_sha) {
        meta.push(format!(
            "<a href=\"https://github.com/{}/{}/compare/{}...{}\" rel=\"noopener noreferrer\" target=\"_blank\">changes since <code>{}</code></a>",
            esc(&r.pr.owner),
            esc(&r.pr.repo),
            esc(old),
            esc(new),
            esc(short_sha(old)),
        ));
    }
    if let Some(p) = &r.parent {
        meta.push(format!("re-review of <a href=\"/r/{0}\">{0}</a>", esc(p)));
    }
    if !others.is_empty() {
        let links: Vec<String> = others.iter().map(|(id, st)| format!("<a href=\"/r/{0}\">{0}</a> ({1})", esc(id), esc(st))).collect();
        meta.push(format!("other reviews of this PR: {}", links.join(", ")));
    }
    meta.push(format!("requested {}", time_tag(r.created)));
    let mut out = format!(
        "<h1><a href=\"{url}\" rel=\"noopener noreferrer\" target=\"_blank\">{short}<span class=\"ext\" aria-hidden=\"true\"> ↗</span></a></h1>\
         <p class=\"meta\">{meta}</p>",
        url = esc(&r.pr.url()),
        short = esc(&r.pr.short()),
        meta = meta.join(" · "),
    );
    render_verdict(&mut out, r);
    render_findings(&mut out, r);
    render_timeline(&mut out, r);
    if !r.note.is_empty() {
        let _ = write!(out, "<h2>Author's note</h2><div class=\"md prose note\">{}</div>", markdown(&r.note));
    }
    render_reviewers(&mut out, r);

    let c = r.params.consensus;
    let _ = write!(
        out,
        "<details class=\"more\" id=\"rules\"><summary><h2>Consensus rules</h2></summary><p class=\"prose\">A finding is confirmed when more than {:.0}% of its author and voters agree \
         and a reviewer other than its author backs its current revision, or its author was the only reviewer; it is rejected when more than {:.0}% disagree, \
         and disputed otherwise. One nobody else voted on as it stands is unverified. Disputed and unverified findings don't affect the verdict. {} \
         Findings can be amended only in the reply window before a round's votes. Up to {} voting round{}. A review whose reviewers all dropped out fails.</p></details>",
        c.confirm_fraction * 100.0,
        c.confirm_fraction * 100.0,
        if c.blocking_unanimous { "A blocking finding stays blocking only if every voter agrees it is blocking." } else { "" },
        c.vote_rounds,
        if c.vote_rounds == 1 { "" } else { "s" },
    );
    let _ = write!(
        out,
        "<details class=\"more\" id=\"transcript\"><summary><h2>Transcript <span class=\"count\">{}</span></h2></summary><ol class=\"log\">",
        r.log.len()
    );
    for e in &r.log {
        let _ = write!(
            out,
            "<li class=\"k-{kind}\"><div class=\"lh\"><b>{who}</b> <span class=\"kind\">{kind}</span> <span class=\"when\">{time}</span></div><div class=\"md prose\">{text}</div></li>",
            time = time_tag(e.at),
            who = esc(&e.who),
            kind = esc(&e.kind),
            text = markdown(&e.text),
        );
    }
    out.push_str("</ol></details>");
    out
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

fn others(st: &State, r: &Review) -> Vec<(String, String)> {
    let mut v: Vec<&Review> = st.reviews.iter().filter(|x| x.pr == r.pr && x.id != r.id).collect();
    v.sort_by_key(|x| x.created);
    v.into_iter().map(|x| (x.id.clone(), status(x).1.to_lowercase())).collect()
}

fn snapshot(hub: &Hub, id: &str) -> Option<(Review, Vec<String>, Ms)> {
    hub.read(|s, now| s.review(id).map(|r| (r.clone(), rereviews(s, &r.id), now)))
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
    let mut main = hub.read(render_index);
    if let Some(Extension(who)) = who {
        let setup = crate::text::setup(&hub.web_url(), &who.login);
        let _ = write!(main, "<details id=\"connect\"><summary>Connect an agent</summary><pre>{}</pre></details>", esc(&setup));
    }
    if partial(&headers) {
        return fragment(&headers, main);
    }
    Html(page("amx", &main)).into_response()
}

pub async fn review_page(
    AxState(hub): AxState<Arc<Hub>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some((r, others)) = hub.read(|s, _| s.review(&id).map(|r| (r.clone(), others(s, r)))) else {
        let main = format!(
            "<h1>No review {}</h1><p class=\"empty\">The relay may have restarted. <a href=\"/\">All reviews</a></p>",
            esc(&id)
        );
        return (StatusCode::NOT_FOUND, Html(page("amx", &main))).into_response();
    };
    let main = render_review_page(&r, &others);
    if partial(&headers) {
        return fragment(&headers, main);
    }
    Html(page(&format!("{} · {}", r.pr.short(), r.id), &main)).into_response()
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
    fn headings_take_a_set_apart_line_or_the_first_sentence() {
        assert_eq!(heading("Off by one\n\nThe loop ends early."), ("Off by one".into(), "The loop ends early."));
        assert_eq!(heading("Uses `a.b()` twice. Cache it."), ("Uses a.b() twice.".into(), "Cache it."));
        assert_eq!(heading("Only one sentence here."), ("Only one sentence here.".into(), ""));
        let long = format!("{} end. More.", "word ".repeat(30));
        assert_eq!(heading(&long).1, long, "a shortened heading keeps the full text below");
    }
}
