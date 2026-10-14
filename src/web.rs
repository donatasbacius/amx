use crate::model::{AuthorKind, Finding, Kind, Message, Status, hlc_ms};
use crate::node::{Node, ThreadSummary, ThreadView};
use crate::prurl::PrRef;
use crate::server::Caller;
use axum::Extension;
use axum::extract::{Path, State};
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

/// Markdown from other machines: raw HTML is sanitised away, links get rel=noopener.
pub fn markdown(src: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    // Raw HTML is shown as text: reviews often mention things like `Vec<String>` unquoted.
    let events = Parser::new_ext(src, opts).map(|e| match e {
        pulldown_cmark::Event::Html(s) | pulldown_cmark::Event::InlineHtml(s) => {
            pulldown_cmark::Event::Text(s)
        }
        e => e,
    });
    let mut raw = String::new();
    html::push_html(&mut raw, events);
    ammonia::Builder::default()
        .url_schemes(["http", "https", "mailto"].into())
        .link_rel(Some("noopener noreferrer nofollow"))
        .clean(&raw)
        .to_string()
}

fn safe_url(u: &str) -> Option<&str> {
    (u.starts_with("https://") || u.starts_with("http://")).then_some(u)
}

pub fn finding_url(f: &Finding, pr: Option<&PrRef>, head: Option<&str>) -> Option<String> {
    if let Some(u) = f.url.as_deref().and_then(safe_url) {
        return Some(u.to_string());
    }
    let (pr, head, path) = (pr?, head?, f.path.as_deref()?);
    let path: String = path
        .trim_start_matches('/')
        .split('/')
        .map(|seg| {
            seg.bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/");
    let line = f.line.map(|l| format!("#L{l}")).unwrap_or_default();
    let head: String = head.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    Some(format!(
        "https://github.com/{}/{}/blob/{head}/{path}{line}",
        pr.owner, pr.repo
    ))
}

fn time_tag(hlc: i64) -> String {
    let ms = hlc_ms(hlc);
    let iso = crate::text::iso(ms);
    format!(
        "<time datetime=\"{}\" title=\"{iso}\">{iso}</time>",
        iso.replace(' ', "T")
    )
}

fn badge(status: Status) -> String {
    format!(
        "<span class=\"badge s-{}\">{}</span>",
        status.css(),
        status.label()
    )
}

fn page(title: &str, who: &str, machine: &str, main: &str, extra: &str) -> String {
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
<header class="top"><a class="brand" href="/">amx</a><span class="where">node <b>{machine}</b> · you are <b>{who}</b></span><span id="live" class="live" title="live updates">●</span></header>
<main id="main">{main}</main>
{extra}
</body>
</html>"#,
        title = esc(title),
        machine = esc(machine),
        who = esc(who),
    )
}

fn pr_path(pr: &PrRef) -> String {
    format!("/pr/{}/{}/{}", pr.owner, pr.repo, pr.number)
}

pub fn render_index(threads: &[ThreadSummary]) -> String {
    let mut out = String::new();
    let (open, closed): (Vec<_>, Vec<_>) = threads.iter().partition(|t| t.status != Status::Closed);
    let _ = write!(out, "<h1>Review threads</h1>");
    if threads.is_empty() {
        out.push_str("<p class=\"empty\">No threads yet. Agents create them with <code>request_review</code>, or use the form below.</p>");
    }
    for (label, list) in [("Open", open), ("Closed", closed)] {
        if list.is_empty() {
            continue;
        }
        let _ = write!(out, "<h2>{label} <span class=\"count\">{}</span></h2><ul class=\"threads\">", list.len());
        for t in list {
            let Some(pr) = crate::prurl::parse(&t.thread) else { continue };
            let _ = write!(
                out,
                "<li class=\"thread\"><div class=\"row1\">{badge}{stale}<a class=\"ref\" href=\"{path}\">{short}</a>\
                 <a class=\"gh\" href=\"{url}\" rel=\"noopener noreferrer\" target=\"_blank\" title=\"Open on GitHub\">GitHub ↗</a></div>\
                 <a class=\"title\" href=\"{path}\">{title}</a>\
                 <div class=\"meta\">{n} message{s} · last {kind} by {last} · {time}{to}</div></li>",
                badge = badge(t.status),
                stale = if t.latest_review_stale { "<span class=\"badge s-stale\">review older than latest push</span>" } else { "" },
                path = esc(&pr_path(&pr)),
                short = esc(&t.short),
                url = esc(&t.thread),
                title = esc(t.title.as_deref().unwrap_or("(title unknown)")),
                n = t.messages,
                s = if t.messages == 1 { "" } else { "s" },
                kind = t.last_kind.as_str(),
                last = esc(&t.last_author),
                time = time_tag(t.last_hlc),
                to = t.requested_to.as_deref().map(|to| format!(" · for <b>{}</b>", esc(to))).unwrap_or_default(),
            );
        }
        out.push_str("</ul>");
    }
    out
}

fn new_request_form() -> String {
    r#"<section class="compose"><details><summary>Request a review</summary>
<form method="post" action="/new">
<label>Pull request URL<input name="pr_url" type="url" required placeholder="https://github.com/owner/repo/pull/123"></label>
<label>For (optional agent name or login)<input name="to" placeholder="anyone"></label>
<label>Note<textarea name="body" rows="3" placeholder="What should the reviewer focus on? Markdown."></textarea></label>
<button type="submit">Request review</button>
</form></details></section>"#
        .to_string()
}

fn render_message(out: &mut String, m: &Message, pr: Option<&PrRef>, stale: bool) {
    let human = m.author.kind == AuthorKind::Human;
    let _ = write!(
        out,
        "<article class=\"msg k-{kind}{h}\" id=\"m-{id}\"><div class=\"mhead\"><span class=\"kind\">{kind}</span>\
         <span class=\"author\">{author}</span>{role}<a class=\"when\" href=\"#m-{id}\">{time}</a></div>",
        kind = m.kind.as_str(),
        h = if human { " human" } else { "" },
        id = esc(&m.id),
        author = esc(&m.author.display()),
        role = if human { "<span class=\"role\">human</span>" } else { "" },
        time = time_tag(m.hlc),
    );
    let mut facts = Vec::new();
    if let Some(to) = &m.fields.to {
        facts.push(format!("for <b>{}</b>", esc(to)));
    }
    if let Some(v) = m.fields.verdict {
        let cls = match v {
            crate::model::Verdict::Approve => "approved",
            crate::model::Verdict::RequestChanges => "changes",
            crate::model::Verdict::Comment => "commented",
        };
        facts.push(format!("<span class=\"badge s-{cls}\">{}</span>", v.label()));
    }
    if let Some(sha) = &m.fields.head_sha
        && m.kind == Kind::Review
    {
        let short: String = sha.chars().take(7).collect();
        let link = pr
            .map(|p| format!("{}/commits/{}", p.url(), esc(sha)))
            .unwrap_or_default();
        facts.push(format!("at <a href=\"{link}\" rel=\"noopener noreferrer\"><code>{}</code></a>", esc(&short)));
        if stale {
            facts.push("<span class=\"badge s-stale\">older than latest push</span>".into());
        }
    }
    if let Some(p) = &m.fields.in_reply_to {
        let tail = p.get(p.len().saturating_sub(6)..).unwrap_or(p);
        facts.push(format!("in reply to <a href=\"#m-{}\">…{}</a>", esc(p), esc(tail)));
    }
    if !facts.is_empty() {
        let _ = write!(out, "<div class=\"facts\">{}</div>", facts.join(" · "));
    }
    if !m.body.is_empty() {
        let _ = write!(out, "<div class=\"body md\">{}</div>", markdown(&m.body));
    }
    if !m.fields.findings.is_empty() {
        out.push_str("<div class=\"findings-wrap\"><table class=\"findings\"><thead><tr><th>Severity</th><th>Location</th><th>Finding</th></tr></thead><tbody>");
        for f in &m.fields.findings {
            let sev = f.severity.as_deref().unwrap_or("note");
            let sev_cls: String = sev.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
            let loc = match (&f.path, f.line) {
                (Some(p), Some(l)) => format!("{p}:{l}"),
                (Some(p), None) => p.clone(),
                _ if f.url.is_some() => "link".into(),
                _ => String::new(),
            };
            let loc_html = match finding_url(f, pr, m.fields.head_sha.as_deref()) {
                Some(u) if !loc.is_empty() => format!(
                    "<a href=\"{}\" rel=\"noopener noreferrer\" target=\"_blank\"><code>{}</code></a>",
                    esc(&u),
                    esc(&loc)
                ),
                _ => format!("<code>{}</code>", esc(&loc)),
            };
            let _ = write!(
                out,
                "<tr><td><span class=\"sev sev-{}\">{}</span></td><td class=\"loc\">{loc_html}</td><td class=\"md\">{}</td></tr>",
                esc(&sev_cls.to_ascii_lowercase()),
                esc(sev),
                markdown(&f.text)
            );
        }
        out.push_str("</tbody></table></div>");
    }
    out.push_str("</article>");
}

pub fn render_thread(v: &ThreadView) -> String {
    let s = &v.summary;
    let pr = crate::prurl::parse(&s.thread);
    let mut out = String::new();
    let _ = write!(
        out,
        "<nav class=\"crumbs\"><a href=\"/\">← all threads</a></nav>\
         <h1>{title}</h1><div class=\"prline\">{badge}{stale}<a class=\"ref\" href=\"{url}\" rel=\"noopener noreferrer\" target=\"_blank\">{short} ↗</a>",
        title = esc(s.title.as_deref().unwrap_or(&s.short)),
        badge = badge(s.status),
        stale = if s.latest_review_stale { "<span class=\"badge s-stale\">latest review older than latest push</span>" } else { "" },
        url = esc(&s.thread),
        short = esc(&s.short),
    );
    if let Some(meta) = &v.meta {
        if let Some(state) = &meta.state {
            let _ = write!(out, "<span class=\"prstate\">{}</span>", esc(&state.to_ascii_lowercase()));
        }
        if let Some(a) = &meta.author {
            let _ = write!(out, "<span class=\"prauthor\">by {}</span>", esc(a));
        }
        if let Some(h) = &meta.head_sha {
            let _ = write!(out, "<span class=\"prhead\">head <code>{}</code></span>", esc(&h.chars().take(7).collect::<String>()));
        }
    }
    out.push_str("</div><div class=\"msgs\">");
    for m in &v.messages {
        render_message(&mut out, m, pr.as_ref(), v.stale_reviews.contains(&m.id));
    }
    out.push_str("</div>");
    out
}

fn reply_form(pr: &PrRef) -> String {
    format!(
        r#"<section class="compose"><form method="post" action="{}">
<label>Message<textarea name="body" rows="4" placeholder="Reply in Markdown"></textarea></label>
<div class="formrow"><label>Type<select name="kind"><option value="reply">Reply</option><option value="request">Request (re-)review</option><option value="close">Close thread</option></select></label>
<label>For (optional)<input name="to" placeholder="agent name or login"></label>
<button type="submit">Post</button></div>
</form></section>"#,
        esc(&pr_path(pr))
    )
}

pub fn error_page(msg: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><link rel=stylesheet href=/static/style.css><main id=main><h1>Could not post</h1><p class=error>{}</p><p><a href=\"javascript:history.back()\">Back</a></p></main>",
        esc(msg)
    )
}

fn partial(headers: &HeaderMap) -> bool {
    headers.contains_key("x-amx-partial")
}

pub async fn index(
    State(node): State<Arc<Node>>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response {
    let threads = match node.threads() {
        Ok(t) => t,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, esc(&e.to_string())).into_response(),
    };
    let main = render_index(&threads);
    if partial(&headers) {
        return Html(main).into_response();
    }
    let who = caller.human(&node).display();
    Html(page("amx · reviews", &who, &node.machine, &main, &new_request_form())).into_response()
}

pub async fn thread_page(
    State(node): State<Arc<Node>>,
    Extension(caller): Extension<Caller>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
    headers: HeaderMap,
) -> Response {
    let url = format!("https://github.com/{owner}/{repo}/pull/{number}");
    let Some(pr) = crate::prurl::parse(&url) else {
        return (StatusCode::NOT_FOUND, "not a PR").into_response();
    };
    let view = match node.thread(&url) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, esc(&e.to_string())).into_response(),
    };
    let main = match &view {
        Some(v) => render_thread(v),
        None => format!(
            "<nav class=\"crumbs\"><a href=\"/\">← all threads</a></nav><h1>{}</h1><p class=\"empty\">No messages on this PR yet.</p>",
            esc(&pr.short())
        ),
    };
    if partial(&headers) {
        return Html(main).into_response();
    }
    let who = caller.human(&node).display();
    let title = view
        .as_ref()
        .and_then(|v| v.summary.title.clone())
        .map(|t| format!("{} · {t}", pr.short()))
        .unwrap_or_else(|| pr.short());
    Html(page(&title, &who, &node.machine, &main, &reply_form(&pr))).into_response()
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
    fn finding_links() {
        let pr = crate::prurl::parse("https://github.com/a/b/pull/1").unwrap();
        let f = Finding {
            path: Some("src/my file.rs".into()),
            line: Some(12),
            text: "x".into(),
            ..Default::default()
        };
        assert_eq!(
            finding_url(&f, Some(&pr), Some("abc123")).unwrap(),
            "https://github.com/a/b/blob/abc123/src/my%20file.rs#L12"
        );
        let bad = Finding {
            url: Some("javascript:alert(1)".into()),
            text: "x".into(),
            ..Default::default()
        };
        assert_eq!(finding_url(&bad, Some(&pr), None), None);
    }
}
