use crate::hub::Hub;
use crate::relay::{Choice, Finding, Ms, Phase, Review, State, Status, Verdict};
use crate::tailscale::Identity;
use crate::text::{dur, iso, phase_badge, severity_split, short_sha, split_note, split_range, vote_label};
use axum::extract::{Path, State as AxState};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::{Extension, Json};
use serde::Serialize;
use serde_json::{Value, json};
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
            _ => out.push(c),
        }
    }
    out
}

/// Markdown from agents: raw HTML is shown as text, links get rel=noopener, only http(s) URLs.
pub fn markdown(src: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let events = Parser::new_ext(src, opts).map(|e| match e {
        pulldown_cmark::Event::Html(s) | pulldown_cmark::Event::InlineHtml(s) => pulldown_cmark::Event::Text(s),
        e => e,
    });
    let mut raw = String::new();
    html::push_html(&mut raw, events);
    ammonia::Builder::default()
        .url_schemes(["http", "https"].into())
        .link_rel(Some("noopener noreferrer nofollow"))
        .clean(&raw)
        .to_string()
}

fn safe_url(u: &str) -> Option<&str> {
    u.starts_with("https://github.com/").then_some(u)
}

fn time_tag(ms: Ms) -> String {
    let iso = iso(ms);
    format!("<time datetime=\"{}\" title=\"{iso}\">{iso}</time>", iso.replace(' ', "T"))
}

fn deadline_tag(r: &Review, now: Ms) -> String {
    if !r.phase.active() {
        return String::new();
    }
    format!(
        "<span class=\"deadline\">ends in <span data-deadline=\"{}\">{}</span></span>",
        r.deadline,
        dur(r.deadline_left(now))
    )
}

fn phase_tag(r: &Review) -> String {
    let (cls, label) = match (r.phase, r.result.as_ref().map(|x| x.verdict)) {
        (Phase::Done, Some(Verdict::Approve)) => ("approved", "approved".to_string()),
        (Phase::Done, _) => ("changes", "changes requested".to_string()),
        (p, _) => (phase_badge(p), r.phase.label()),
    };
    format!("<span class=\"badge s-{cls}\">{}</span>", esc(&label))
}

fn page(title: &str, who: &Identity, main: &str) -> String {
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
<header class="top"><a class="brand" href="/">amx</a><span class="where">review relay · you are <b>{who}</b></span><span id="live" class="live" role="status"><span class="dot" aria-hidden="true">●</span> <span class="lt">static</span></span></header>
<main id="main">{main}</main>
</body>
</html>"##,
        title = esc(title),
        who = esc(&who.login),
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
        Status::Disputed if o.severity_split => ("stale", "disputed severity".to_string()),
        Status::Disputed => ("stale", "disputed".to_string()),
        Status::Duplicate(t) => ("closed", format!("duplicate of {t}")),
        Status::Withdrawn => ("closed", "withdrawn".to_string()),
    })
}

fn votes(f: &Finding) -> (usize, usize) {
    match &f.outcome {
        Some(o) => (o.agree, o.disagree),
        None => (
            f.votes.values().filter(|v| v.choice != Choice::Disagree).count(),
            f.votes.values().filter(|v| v.choice == Choice::Disagree).count(),
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

fn render_timeline(out: &mut String, r: &Review, now: Ms) {
    out.push_str("<h2 id=\"timeline\">Timeline</h2><ol class=\"timeline\">");
    for s in timeline(r) {
        let mut info = String::new();
        if let Some(at) = s.start {
            info.push_str(&time_tag(at));
        }
        if let (Some(a), Some(b)) = (s.start, s.end)
            && s.name != "Consensus result"
            && s.name != "Failed"
        {
            let _ = write!(info, " · took {}", dur(b.saturating_sub(a)));
        }
        if let Some(d) = s.deadline {
            let _ = write!(
                info,
                " · <span class=\"deadline\">ends in <span data-deadline=\"{d}\">{}</span></span>",
                dur(d.saturating_sub(now))
            );
        }
        if s.state == "skipped" {
            info.push_str("skipped");
        }
        let notes: String = s.notes.iter().map(|n| format!("<div class=\"note\">{}</div>", esc(n))).collect();
        let current = if s.state == "active" { " aria-current=\"step\"" } else { "" };
        let _ = write!(
            out,
            "<li class=\"st-{state}\"{current}><span class=\"name\">{name}</span><span class=\"info\">{info}</span>{notes}</li>",
            state = s.state,
            name = esc(&s.name),
        );
    }
    out.push_str("</ol>");
}

fn review_row(out: &mut String, r: &Review, now: Ms) {
    let reviewers: Vec<String> = r
        .slots
        .iter()
        .map(|s| {
            if s.dropped.is_some() {
                format!("<s>{}</s>", esc(&s.agent))
            } else {
                esc(&s.agent)
            }
        })
        .collect();
    let n = r.findings.len();
    let confirmed = visible(r)
        .filter(|f| f.outcome.as_ref().is_some_and(|o| o.status == Status::Confirmed))
        .count();
    let _ = write!(
        out,
        "<li class=\"thread\"><div class=\"row1\">{phase}{deadline}<a class=\"ref\" href=\"/r/{id}\">{id}</a>\
         <a class=\"gh\" href=\"{url}\" rel=\"noopener noreferrer\" target=\"_blank\">{short} ↗</a></div>\
         <div class=\"meta\">by <b>{author}</b>{reviewers} · {n} finding{s}{confirmed} · {time}</div></li>",
        phase = phase_tag(r),
        deadline = deadline_tag(r, now),
        id = esc(&r.id),
        url = esc(&r.pr.url()),
        short = esc(&r.pr.short()),
        author = esc(&r.author),
        reviewers = if reviewers.is_empty() { String::new() } else { format!(" · reviewers {}", reviewers.join(", ")) },
        s = if n == 1 { "" } else { "s" },
        confirmed = if confirmed > 0 { format!(", {confirmed} confirmed") } else { String::new() },
        time = time_tag(r.created),
    );
}

pub fn render_index(st: &State, now: Ms) -> String {
    let mut out = String::from("<h1>Review relay</h1>");
    let online = st.online(now);
    let _ = write!(out, "<h2>Online agents <span class=\"count\">{}</span></h2>", online.len());
    if online.is_empty() {
        out.push_str("<p class=\"empty\">No agents online. Agents appear here while they call the relay.</p>");
    } else {
        out.push_str("<div class=\"tablewrap\"><table class=\"grid agents\"><thead><tr><th scope=\"col\">Agent</th><th scope=\"col\">Role</th><th scope=\"col\">Owner</th><th scope=\"col\">State</th></tr></thead><tbody>");
        for a in &online {
            let state = match &a.busy {
                Some(id) => format!("busy with <a href=\"/r/{0}\">{0}</a>", esc(id)),
                None => "idle".into(),
            };
            let _ = write!(
                out,
                "<tr><th scope=\"row\">{}</th><td><span class=\"role r-{role}\">{role}</span></td><td>{}<span class=\"muted\"> on {}</span></td><td>{state}</td></tr>",
                esc(&a.name),
                esc(&a.login),
                esc(&a.node),
                role = a.role.as_str(),
            );
        }
        out.push_str("</tbody></table></div>");
    }
    let mut reviews: Vec<&Review> = st.reviews.iter().collect();
    reviews.sort_by_key(|r| std::cmp::Reverse(r.created));
    let (active, done): (Vec<&Review>, Vec<&Review>) = reviews.into_iter().partition(|r| r.phase.active());
    for (label, list, empty) in [
        ("Active reviews", active, "No reviews in progress."),
        ("Finished reviews", done, "Nothing finished yet."),
    ] {
        let _ = write!(out, "<h2>{label} <span class=\"count\">{}</span></h2>", list.len());
        if list.is_empty() {
            let _ = write!(out, "<p class=\"empty\">{empty}</p>");
            continue;
        }
        out.push_str("<ul class=\"threads\">");
        for r in list {
            review_row(&mut out, r, now);
        }
        out.push_str("</ul>");
    }
    out
}

fn status_badge(f: &Finding) -> String {
    let Some((cls, label)) = outcome(f) else {
        return String::new();
    };
    let mut s = format!("<span class=\"badge s-{cls}\">{}</span>", esc(&label));
    if let Some(o) = &f.outcome {
        if let Some(range) = split_range(f) {
            let _ = write!(s, "<span class=\"badge s-stale\">severity split {range}: the lower stands</span>");
        } else if o.downgraded {
            s.push_str("<span class=\"badge s-stale\">blocking → major (not unanimous)</span>");
        } else if o.severity != f.severity {
            let _ = write!(s, "<span class=\"badge s-stale\">{} → {} (by vote)</span>", f.severity.as_str(), o.severity.as_str());
        }
    }
    s
}

fn raised_by(f: &Finding) -> String {
    match (&f.by, &f.origin) {
        (Some(b), _) => esc(b),
        (None, Some(o)) => format!("carried from {}", esc(o)),
        _ => String::new(),
    }
}

fn render_summary(out: &mut String, r: &Review, findings: &[&Finding]) {
    out.push_str(
        "<div class=\"tablewrap\"><table class=\"grid summary-table\"><thead><tr><th scope=\"col\">ID</th><th scope=\"col\">Finding</th>\
         <th scope=\"col\">Severity</th><th scope=\"col\">Raised by</th><th scope=\"col\">Outcome</th><th scope=\"col\">Votes</th></tr></thead><tbody>",
    );
    for f in findings {
        let sev = f.outcome.as_ref().map(|o| o.severity).unwrap_or(f.severity);
        let status = match outcome(f) {
            Some((cls, label)) => format!("<span class=\"badge s-{cls}\">{}</span>", esc(&label)),
            None if r.phase.active() => "<span class=\"muted\">open</span>".into(),
            None => "<span class=\"muted\">undecided</span>".into(),
        };
        let (agree, disagree) = votes(f);
        let _ = write!(
            out,
            "<tr><th scope=\"row\"><a class=\"fid\" href=\"#{id}\">{id}</a></th><td class=\"ftitle\">{title}</td>\
             <td><span class=\"sev sev-{sev}\">{sev}</span>{split}</td><td>{by}</td><td>{status}</td>\
             <td class=\"num\"><span title=\"agree (counting who raised it)\">{agree} ✓</span> <span title=\"disagree\">{disagree} ✗</span></td></tr>",
            id = esc(&f.id),
            title = esc(&title(&f.text)),
            sev = sev.as_str(),
            split = split_range(f).map(|x| format!(" <span class=\"muted\">(split: {x})</span>")).unwrap_or_default(),
            by = raised_by(f),
        );
    }
    out.push_str("</tbody></table></div>");
}

fn render_finding(out: &mut String, f: &Finding) {
    let sev = f.outcome.as_ref().map(|o| o.severity).unwrap_or(f.severity);
    let by = match &f.by {
        Some(_) => format!("by <b>{}</b>", raised_by(f)),
        None => raised_by(f),
    };
    let link = f
        .url
        .as_deref()
        .and_then(safe_url)
        .map(|u| {
            let label = u.rsplit('/').next().unwrap_or(u);
            format!(" · <a href=\"{}\" rel=\"noopener noreferrer\" target=\"_blank\"><code>{}</code></a>", esc(u), esc(label))
        })
        .unwrap_or_default();
    let _ = write!(
        out,
        "<details class=\"finding{w}\" id=\"{id}\" open><summary class=\"mhead\"><span class=\"fid\">{id}</span><span class=\"sev sev-{sev}\">{sev}</span>{badge}<span class=\"by\">{by}{amended}</span>{link}</summary>\
         <div class=\"body md\">{text}</div>",
        w = if f.withdrawn.is_some() { " withdrawn" } else { "" },
        id = esc(&f.id),
        sev = sev.as_str(),
        badge = status_badge(f),
        amended = if f.revision > 0 { " · amended" } else { "" },
        text = markdown(&f.text),
    );
    if let Some(w) = &f.withdrawn {
        let _ = write!(out, "<div class=\"facts\">withdrawn: {}</div>", esc(w));
    }
    if let Some(split) = split_note(f) {
        let _ = write!(out, "<div class=\"facts\">severity {}</div>", esc(&split));
    } else if let Some(split) = severity_split(f) {
        let _ = write!(out, "<div class=\"facts\">disputed: {}</div>", esc(&split));
    }
    if !f.votes.is_empty() {
        out.push_str("<ul class=\"votes\">");
        for (who, v) in &f.votes {
            let cls = match &v.choice {
                Choice::Agree => "agree",
                Choice::Disagree => "disagree",
                Choice::Duplicate(_) => "dup",
            };
            let what = vote_label(f, v);
            let _ = write!(
                out,
                "<li class=\"v-{cls}\"><b>{}</b> {}<span class=\"muted\"> · round {}</span>{}</li>",
                esc(who),
                esc(&what),
                v.round,
                v.reason.as_deref().map(|r| format!("<div class=\"md\">{}</div>", markdown(r))).unwrap_or_default()
            );
        }
        out.push_str("</ul>");
    }
    out.push_str("</details>");
}

fn render_verdict(out: &mut String, r: &Review) {
    if let Some(res) = &r.result {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for f in &r.findings {
            if let Some((_, label)) = outcome(f) {
                match counts.iter_mut().find(|(l, _)| *l == label) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((label, 1)),
                }
            }
        }
        let counts = if counts.is_empty() {
            "no findings".to_string()
        } else {
            counts.iter().map(|(l, n)| format!("{n} {l}")).collect::<Vec<_>>().join(" · ")
        };
        let ok = res.verdict == Verdict::Approve;
        let _ = write!(
            out,
            "<section class=\"verdict {cls}\" aria-label=\"Result\"><strong>{label}</strong><span>{counts}</span><span class=\"muted\">sent to {author} {time}</span></section>",
            cls = if ok { "ok" } else { "bad" },
            label = if ok { "Approved" } else { "Changes requested" },
            counts = esc(&counts),
            author = esc(&r.author),
            time = time_tag(res.at),
        );
    }
    if let Some(f) = &r.failure {
        let _ = write!(out, "<section class=\"verdict bad\" aria-label=\"Result\"><strong>Failed</strong><span>{}</span></section>", esc(f));
    }
}

pub fn render_review(r: &Review, now: Ms) -> String {
    render_review_page(r, &[], now)
}

/// `rereviews` are the ids of later reviews of the same PR started from this one.
pub fn render_review_page(r: &Review, rereviews: &[String], now: Ms) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "<nav class=\"crumbs\" aria-label=\"Breadcrumb\"><a href=\"/\">← relay</a></nav><h1>{id} · {short}</h1>\
         <div class=\"prline\">{phase}{deadline}<a class=\"ref\" href=\"{url}\" rel=\"noopener noreferrer\" target=\"_blank\">{url_text} ↗</a>\
         <span>by <b>{author}</b></span>{parent}{children}{head}</div>",
        id = esc(&r.id),
        short = esc(&r.pr.short()),
        phase = phase_tag(r),
        deadline = deadline_tag(r, now),
        url = esc(&r.pr.url()),
        url_text = esc(&r.pr.short()),
        author = esc(&r.author),
        parent = r
            .parent
            .as_deref()
            .map(|p| format!("<span>re-review of <a href=\"/r/{0}\">{0}</a></span>", esc(p)))
            .unwrap_or_default(),
        children = if rereviews.is_empty() {
            String::new()
        } else {
            let links: Vec<String> = rereviews.iter().map(|c| format!("<a href=\"/r/{0}\">{0}</a>", esc(c))).collect();
            format!("<span>re-reviewed in {}</span>", links.join(", "))
        },
        head = r
            .head_sha
            .as_deref()
            .map(|h| format!("<span>head <code>{}</code></span>", esc(short_sha(h))))
            .unwrap_or_default(),
    );
    render_verdict(&mut out, r);

    let shown: Vec<&Finding> = visible(r).collect();
    let hidden = r.findings.len() - shown.len();
    let _ = write!(out, "<h2 id=\"findings\">Findings <span class=\"count\">{}</span></h2>", r.findings.len());
    if hidden > 0 {
        let _ = write!(
            out,
            "<p class=\"empty\">{hidden} finding(s) submitted so far; hidden until every reviewer has submitted.</p>"
        );
    } else if shown.is_empty() {
        out.push_str("<p class=\"empty\">No findings.</p>");
    }
    if !shown.is_empty() {
        render_summary(&mut out, r, &shown);
    }

    render_timeline(&mut out, r, now);

    if !r.note.is_empty() {
        let _ = write!(out, "<section class=\"card\"><h2 class=\"label\">Author's note</h2><div class=\"md\">{}</div></section>", markdown(&r.note));
    }
    let c = r.params.consensus;
    let _ = write!(
        out,
        "<h2>Reviewers <span class=\"count\">{}/{}</span></h2><div class=\"facts\">confirm when more than {:.0}% agree · blocking needs unanimity: {} · up to {} voting round(s)</div><ul class=\"reviewers\">",
        r.active_slots().count(),
        r.params.reviewers,
        c.confirm_fraction * 100.0,
        if c.blocking_unanimous { "yes" } else { "no" },
        c.vote_rounds
    );
    if r.slots.is_empty() {
        let _ = write!(out, "<li class=\"muted\">offered to {} agent(s), {} declined</li>", r.offered.len(), r.declined.len());
    }
    for s in &r.slots {
        let state = match (&s.dropped, &s.submission) {
            (Some(d), _) => format!("<span class=\"badge s-closed\">dropped: {}</span>", esc(d)),
            (None, Some(x)) => format!(
                "<span class=\"badge s-inreview\">submitted</span> suggests <b>{}</b> at <code>{}</code>",
                x.verdict.as_str(),
                esc(short_sha(&x.head_sha))
            ),
            (None, None) => "<span class=\"badge s-waiting\">reviewing</span>".into(),
        };
        let summary = s
            .submission
            .as_ref()
            .filter(|x| !x.summary.is_empty() && r.revealed())
            .map(|x| format!("<div class=\"md summary\">{}</div>", markdown(&x.summary)))
            .unwrap_or_default();
        let _ = write!(out, "<li><b>{}</b> <span class=\"muted\">{}</span> {state}{summary}</li>", esc(&s.agent), esc(&s.login));
    }
    out.push_str("</ul>");

    if !shown.is_empty() {
        out.push_str("<h2>Finding details</h2><div class=\"findings-list\">");
        for f in &shown {
            render_finding(&mut out, f);
        }
        out.push_str("</div>");
    }

    let _ = write!(
        out,
        "<details class=\"transcript\" id=\"transcript\"><summary><h2>Transcript <span class=\"count\">{}</span></h2></summary><ol class=\"log\">",
        r.log.len()
    );
    for e in &r.log {
        let _ = write!(
            out,
            "<li class=\"k-{kind}\"><span class=\"when\">{time}</span> <b>{who}</b> <span class=\"kind\">{kind}</span> <div class=\"md\">{text}</div></li>",
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

fn snapshot(hub: &Hub, id: &str) -> Option<(Review, Vec<String>, Ms)> {
    hub.read(|s, now| s.review(id).map(|r| (r.clone(), rereviews(s, &r.id), now)))
}

fn partial(headers: &HeaderMap) -> bool {
    headers.contains_key("x-amx-partial")
}

fn no_store(mut r: Response) -> Response {
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

pub async fn index(AxState(hub): AxState<Arc<Hub>>, Extension(who): Extension<Identity>, headers: HeaderMap) -> Response {
    let main = hub.read(render_index);
    if partial(&headers) {
        return no_store(Html(main).into_response());
    }
    Html(page("amx relay", &who, &main)).into_response()
}

pub async fn review_page(
    AxState(hub): AxState<Arc<Hub>>,
    Extension(who): Extension<Identity>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some((r, children, now)) = snapshot(&hub, &id) else {
        let main = format!(
            "<nav class=\"crumbs\" aria-label=\"Breadcrumb\"><a href=\"/\">← relay</a></nav><h1>No review {}</h1><p class=\"empty\">The relay may have restarted.</p>",
            esc(&id)
        );
        return (StatusCode::NOT_FOUND, Html(page("amx relay", &who, &main))).into_response();
    };
    let main = render_review_page(&r, &children, now);
    if partial(&headers) {
        return no_store(Html(main).into_response());
    }
    Html(page(&format!("{} · {}", r.id, r.pr.short()), &who, &main)).into_response()
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
    }

    #[test]
    fn titles_are_first_lines() {
        assert_eq!(title("\n## **Off by one** in `len`\nmore"), "Off by one in len");
        let long = "x".repeat(150);
        assert_eq!(title(&long).chars().count(), 101);
    }
}
