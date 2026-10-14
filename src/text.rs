use crate::model::{Kind, Message, hlc_ms};
use crate::node::{InboxItem, ThreadSummary, ThreadView};
use std::fmt::Write;

pub fn iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        format!("{}…", flat.chars().take(max).collect::<String>())
    } else {
        flat
    }
}

pub fn headline(m: &Message) -> String {
    let who = m.author.display();
    match m.kind {
        Kind::Request => match &m.fields.to {
            Some(to) => format!("{who} requested a review from {to}"),
            None => format!("{who} requested a review (anyone)"),
        },
        Kind::Claim => format!("{who} is reviewing"),
        Kind::Review => format!(
            "{who} reviewed: {} ({} finding{})",
            m.fields.verdict.map(|v| v.label()).unwrap_or("?"),
            m.fields.findings.len(),
            if m.fields.findings.len() == 1 { "" } else { "s" }
        ),
        Kind::Reply => format!("{who} replied"),
        Kind::Close => format!("{who} closed the thread"),
    }
}

/// One line per item, for `amx watch` and hook context.
pub fn item_line(item: &InboxItem) -> String {
    let m = &item.message;
    let short = crate::prurl::parse(&m.thread).map(|p| p.short()).unwrap_or_default();
    let title = item
        .title
        .as_deref()
        .map(|t| format!(" \"{}\"", one_line(t, 60)))
        .unwrap_or_default();
    let body = if m.body.is_empty() {
        String::new()
    } else {
        format!(": {}", one_line(&m.body, 140))
    };
    format!(
        "amx {} {short}{title} | {}{body} | {} | id {}",
        m.kind.as_str(),
        headline(m),
        m.thread,
        m.id
    )
}

pub fn message_block(out: &mut String, m: &Message, stale: bool) {
    let _ = writeln!(
        out,
        "### [{}] {} — {} (id {})",
        m.kind.as_str(),
        headline(m),
        iso(hlc_ms(m.hlc)),
        m.id
    );
    if let Some(parent) = &m.fields.in_reply_to {
        let _ = writeln!(out, "in reply to {parent}");
    }
    if let Some(sha) = &m.fields.head_sha {
        let _ = writeln!(
            out,
            "head {sha}{}",
            if stale { " (OLDER than the PR's latest push)" } else { "" }
        );
    }
    if !m.body.is_empty() {
        let _ = writeln!(out, "\n{}", m.body);
    }
    if !m.fields.findings.is_empty() {
        let _ = writeln!(out, "\nFindings:");
        for (i, f) in m.fields.findings.iter().enumerate() {
            let loc = match (&f.path, f.line) {
                (Some(p), Some(l)) => format!("{p}:{l}"),
                (Some(p), None) => p.clone(),
                _ => String::new(),
            };
            let _ = writeln!(
                out,
                "{}. [{}] {} {}{}",
                i + 1,
                f.severity.as_deref().unwrap_or("note"),
                loc,
                f.text.trim(),
                f.url.as_deref().map(|u| format!(" ({u})")).unwrap_or_default()
            );
        }
    }
    out.push('\n');
}

pub fn inbox(agent: &str, items: &[InboxItem]) -> String {
    if items.is_empty() {
        return format!("No unread amx messages for {agent}.");
    }
    let mut out = format!("{} unread amx message(s) for {agent}:\n\n", items.len());
    for item in items {
        let m = &item.message;
        let _ = writeln!(
            out,
            "## {}{} — {}",
            m.thread,
            item.title.as_deref().map(|t| format!(" \"{t}\"")).unwrap_or_default(),
            item.reason.label()
        );
        message_block(&mut out, m, false);
    }
    out.push_str("Call mark_read with these ids once handled (or thread to read and mark a whole thread).");
    out
}

pub fn summary_line(t: &ThreadSummary) -> String {
    format!(
        "{} [{}{}]{} — {} msgs, last: {} by {} at {}{}",
        t.thread,
        t.status.label(),
        if t.latest_review_stale { ", review older than latest push" } else { "" },
        t.title.as_deref().map(|s| format!(" \"{s}\"")).unwrap_or_default(),
        t.messages,
        t.last_kind.as_str(),
        t.last_author,
        iso(hlc_ms(t.last_hlc)),
        t.requested_to.as_deref().map(|to| format!(", requested from {to}")).unwrap_or_default()
    )
}

pub fn thread(v: &ThreadView) -> String {
    let s = &v.summary;
    let mut out = format!(
        "# {}{}\nstatus: {}\n",
        s.thread,
        s.title.as_deref().map(|t| format!(" \"{t}\"")).unwrap_or_default(),
        s.status.label()
    );
    if let Some(meta) = &v.meta {
        let _ = writeln!(
            out,
            "PR: state {}, author {}, current head {}",
            meta.state.as_deref().unwrap_or("?"),
            meta.author.as_deref().unwrap_or("?"),
            meta.head_sha.as_deref().unwrap_or("?")
        );
    }
    out.push('\n');
    for m in &v.messages {
        message_block(&mut out, m, v.stale_reviews.contains(&m.id));
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn iso_dates() {
        assert_eq!(super::iso(0), "1970-01-01 00:00:00Z");
        assert_eq!(super::iso(1_760_400_000_000), "2025-10-14 00:00:00Z");
    }
}
