use crate::hub::Hub;
use crate::relay::{Choice, Finding, Ms, Phase, Review, State, Status, Verdict};
use crate::tailscale::Identity;
use crate::text::{dur, iso, phase_badge, severity_split, short_sha};
use axum::Extension;
use axum::extract::{Path, State as AxState};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
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
        r#"<!doctype html>
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
<header class="top"><a class="brand" href="/">amx</a><span class="where">review relay · you are <b>{who}</b></span><span id="live" class="live" title="live updates">●</span></header>
<main id="main">{main}</main>
</body>
</html>"#,
        title = esc(title),
        who = esc(&who.login),
    )
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
    let _ = write!(
        out,
        "<li class=\"thread\"><div class=\"row1\">{phase}{deadline}<a class=\"ref\" href=\"/r/{id}\">{id}</a>\
         <a class=\"gh\" href=\"{url}\" rel=\"noopener noreferrer\" target=\"_blank\">{short} ↗</a></div>\
         <div class=\"meta\">by <b>{author}</b>{reviewers} · {n} finding{s} · {time}</div></li>",
        phase = phase_tag(r),
        deadline = deadline_tag(r, now),
        id = esc(&r.id),
        url = esc(&r.pr.url()),
        short = esc(&r.pr.short()),
        author = esc(&r.author),
        reviewers = if reviewers.is_empty() { String::new() } else { format!(" · reviewers {}", reviewers.join(", ")) },
        n = r.findings.len(),
        s = if r.findings.len() == 1 { "" } else { "s" },
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
        out.push_str("<div class=\"tablewrap\"><table class=\"agents\"><thead><tr><th>Agent</th><th>Role</th><th>Owner</th><th>State</th></tr></thead><tbody>");
        for a in &online {
            let state = match &a.busy {
                Some(id) => format!("busy with <a href=\"/r/{0}\">{0}</a>", esc(id)),
                None => "idle".into(),
            };
            let _ = write!(
                out,
                "<tr><td><b>{}</b></td><td><span class=\"role r-{role}\">{role}</span></td><td>{}<span class=\"muted\"> on {}</span></td><td>{state}</td></tr>",
                esc(&a.name),
                esc(&a.login),
                esc(&a.node),
                role = a.role.as_str(),
            );
        }
        out.push_str("</tbody></table></div>");
    }
    let (active, done): (Vec<&Review>, Vec<&Review>) = st.reviews.iter().rev().partition(|r| r.phase.active());
    for (label, list, empty) in [
        ("Active reviews", active, "No reviews in progress."),
        ("Recent reviews", done, "Nothing finished yet."),
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
    let Some(o) = &f.outcome else {
        if f.withdrawn.is_some() {
            return "<span class=\"badge s-closed\">withdrawn</span>".into();
        }
        return String::new();
    };
    let carried = f.by.is_none();
    let (cls, label) = match &o.status {
        Status::Confirmed if carried => ("changes", "still present".to_string()),
        Status::Confirmed => ("changes", "confirmed".to_string()),
        Status::Rejected if carried => ("approved", "resolved".to_string()),
        Status::Rejected => ("closed", "rejected".to_string()),
        Status::Disputed if o.severity_split => ("stale", "disputed severity".to_string()),
        Status::Disputed => ("stale", "disputed".to_string()),
        Status::Duplicate(t) => ("closed", format!("duplicate of {t}")),
        Status::Withdrawn => ("closed", "withdrawn".to_string()),
    };
    let mut s = format!("<span class=\"badge s-{cls}\">{}</span>", esc(&label));
    if o.downgraded {
        s.push_str("<span class=\"badge s-stale\">blocking → major (not unanimous)</span>");
    } else if o.severity != f.severity {
        let _ = write!(s, "<span class=\"badge s-stale\">{} → {} (by vote)</span>", f.severity.as_str(), o.severity.as_str());
    }
    s
}

fn render_finding(out: &mut String, f: &Finding) {
    let sev = f.outcome.as_ref().map(|o| o.severity).unwrap_or(f.severity);
    let by = match (&f.by, &f.origin) {
        (Some(b), _) => format!("by <b>{}</b>", esc(b)),
        (None, Some(o)) => format!("carried from {}", esc(o)),
        _ => String::new(),
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
        "<article class=\"finding{w}\" id=\"{id}\"><div class=\"mhead\"><span class=\"fid\">{id}</span><span class=\"sev sev-{sev}\">{sev}</span>{badge}<span class=\"by\">{by}{amended}</span>{link}</div>\
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
    if let Some(split) = severity_split(f) {
        let _ = write!(out, "<div class=\"facts\">disputed: {}</div>", esc(&split));
    }
    if !f.votes.is_empty() {
        out.push_str("<ul class=\"votes\">");
        for (who, v) in &f.votes {
            let (cls, what) = match &v.choice {
                Choice::Agree => ("agree", v.severity.map_or("agree".to_string(), |s| format!("agree, as {}", s.as_str()))),
                Choice::Disagree => ("disagree", "disagree".to_string()),
                Choice::Duplicate(t) => ("dup", format!("duplicate of {t}")),
            };
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
    out.push_str("</article>");
}

pub fn render_review(r: &Review, now: Ms) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "<nav class=\"crumbs\"><a href=\"/\">← relay</a></nav><h1>{id} · {short}</h1>\
         <div class=\"prline\">{phase}{deadline}<a class=\"ref\" href=\"{url}\" rel=\"noopener noreferrer\" target=\"_blank\">{url_text} ↗</a>\
         <span>by <b>{author}</b></span>{parent}{head}</div>",
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
        head = r
            .head_sha
            .as_deref()
            .map(|h| format!("<span>head <code>{}</code></span>", esc(short_sha(h))))
            .unwrap_or_default(),
    );
    if !r.note.is_empty() {
        let _ = write!(out, "<section class=\"card\"><div class=\"label\">Author's note</div><div class=\"md\">{}</div></section>", markdown(&r.note));
    }
    if let Some(res) = &r.result {
        let _ = write!(
            out,
            "<section class=\"card result {}\"><div class=\"label\">Consensus result sent to {}</div><pre>{}</pre></section>",
            if res.verdict == Verdict::Approve { "ok" } else { "bad" },
            esc(&r.author),
            esc(&res.text)
        );
    }
    if let Some(f) = &r.failure {
        let _ = write!(out, "<section class=\"card result bad\"><div class=\"label\">Failed</div><p>{}</p></section>", esc(f));
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
    let _ = write!(out, "<h2>Findings <span class=\"count\">{}</span></h2>", r.findings.len());
    if !r.revealed() {
        let carried: Vec<&Finding> = r.findings.iter().filter(|f| f.by.is_none()).collect();
        let hidden = r.findings.len() - carried.len();
        let _ = write!(
            out,
            "<p class=\"empty\">{hidden} finding(s) submitted so far; hidden until every reviewer has submitted.</p>"
        );
        for f in carried {
            render_finding(&mut out, f);
        }
    } else if r.findings.is_empty() {
        out.push_str("<p class=\"empty\">No findings.</p>");
    } else {
        out.push_str("<div class=\"findings-list\">");
        for f in &r.findings {
            render_finding(&mut out, f);
        }
        out.push_str("</div>");
    }
    out.push_str("<h2>Transcript</h2><ol class=\"log\">");
    for e in &r.log {
        let _ = write!(
            out,
            "<li class=\"k-{kind}\"><span class=\"when\">{time}</span> <b>{who}</b> <span class=\"kind\">{kind}</span> <div class=\"md\">{text}</div></li>",
            time = time_tag(e.at),
            who = esc(&e.who),
            kind = esc(e.kind),
            text = markdown(&e.text),
        );
    }
    out.push_str("</ol>");
    out
}

fn partial(headers: &HeaderMap) -> bool {
    headers.contains_key("x-amx-partial")
}

pub async fn index(AxState(hub): AxState<Arc<Hub>>, Extension(who): Extension<Identity>, headers: HeaderMap) -> Response {
    let main = hub.read(render_index);
    if partial(&headers) {
        return Html(main).into_response();
    }
    Html(page("amx relay", &who, &main)).into_response()
}

pub async fn review_page(
    AxState(hub): AxState<Arc<Hub>>,
    Extension(who): Extension<Identity>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let found = hub.read(|s, now| s.review(&id).map(|r| (render_review(r, now), format!("{} · {}", r.id, r.pr.short()))));
    let Some((main, title)) = found else {
        let main = format!(
            "<nav class=\"crumbs\"><a href=\"/\">← relay</a></nav><h1>No review {}</h1><p class=\"empty\">The relay keeps reviews in memory; it may have restarted.</p>",
            esc(&id)
        );
        return (StatusCode::NOT_FOUND, Html(page("amx relay", &who, &main))).into_response();
    };
    if partial(&headers) {
        return Html(main).into_response();
    }
    Html(page(&title, &who, &main)).into_response()
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
}
