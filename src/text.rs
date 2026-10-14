use crate::relay::{Agent, AgentEvent, Choice, Finding, Ms, OnlineAgent, Phase, Review, Role, Status, Verdict};
use std::fmt::Write;

pub fn iso(ms: Ms) -> String {
    let secs = (ms / 1000) as i64;
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

pub fn dur(ms: Ms) -> String {
    let s = ms.div_ceil(1000);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

pub fn short_sha(s: &str) -> &str {
    s.get(..7).unwrap_or(s)
}

fn quote(s: &str) -> String {
    s.lines().map(|l| format!("    {l}\n")).collect()
}

pub fn offer(r: &Review, now: Ms) -> String {
    let mut s = format!(
        "[{}] Review offer: {} from {} ({} reviewer(s) wanted).",
        r.id,
        r.pr.url(),
        r.author,
        r.params.reviewers
    );
    if r.parent.is_some() {
        s.push_str(" This is a re-review after fixes.");
    }
    if !r.note.is_empty() {
        let _ = write!(s, "\nNote from the author:\n{}", quote(&r.note));
    }
    let _ = write!(
        s,
        "\nNEXT: call `accept_review` with review_id \"{}\" if you can review it now, else `decline_review`. The first {} to accept get it; the offer closes in {}.",
        r.id,
        r.params.reviewers,
        dur(r.deadline_left(now))
    );
    s
}

pub fn review_task(r: &Review, left: &str) -> String {
    let url = r.pr.url();
    let mut s = format!("Review {url} (requested by {}", r.author);
    if let Some(h) = &r.head_sha {
        let _ = write!(s, ", head {}", short_sha(h));
    }
    s.push_str(").\n");
    if !r.note.is_empty() {
        let _ = write!(s, "Author's note:\n{}", quote(&r.note));
    }
    let _ = write!(
        s,
        "NEXT: review the PR read-only, on your own:\n    gh pr view {url} --json title,body,headRefOid,files\n    gh pr diff {url}\n"
    );
    if let Some(since) = &r.since_sha {
        let _ = write!(
            s,
            "This is re-review #{} (previous: {}). Focus on what changed since {}:\n    gh api repos/{}/{}/compare/{}...<headRefOid>\n",
            r.iteration,
            r.parent.as_deref().unwrap_or("?"),
            short_sha(since),
            r.pr.owner,
            r.pr.repo,
            since
        );
        let carried: Vec<&Finding> = r.findings.iter().filter(|f| f.by.is_none()).collect();
        if !carried.is_empty() {
            s.push_str("Previously raised findings; in cross-examination you will vote on each: agree = still present, disagree (with reason) = fixed. Check them now:\n");
            for f in carried {
                let _ = writeln!(s, "{}", finding_line(f));
            }
        }
        s.push_str("Report only NEW problems in submit_findings.\n");
    }
    let _ = write!(
        s,
        "Never write to GitHub (no comments, reviews, pushes). Then call `submit_findings` with review_id \"{}\", head_sha (the headRefOid you reviewed), verdict (approve | changes_requested), a short summary, and findings: \
         [{{severity: blocking|major|minor|nit, url: \"https://github.com/{}/{}/blob/<head_sha>/<path>#L<line>\", text}}] (an empty list if you found nothing). \
         Severity: blocking = must not merge (bug, data loss, security hole); major = should be fixed before merge; minor = worth fixing; nit = style or taste. \
         Other reviewers' findings stay hidden until everyone has submitted. Deadline: {left}.",
        r.id, r.pr.owner, r.pr.repo
    );
    s
}

pub fn finding_line(f: &Finding) -> String {
    let who = match (&f.by, &f.origin) {
        (Some(b), _) => format!("by {b}"),
        (None, Some(o)) => format!("carried from {o}"),
        (None, None) => String::new(),
    };
    let mut s = format!("- {} [{}] {who}: {}", f.id, f.severity.as_str(), f.text.trim());
    if let Some(u) = &f.url {
        let _ = write!(s, " ({u})");
    }
    if f.revision > 0 {
        s.push_str(" (amended)");
    }
    if let Some(w) = &f.withdrawn {
        let _ = write!(s, " WITHDRAWN: {w}");
    }
    s
}

fn votes_lines(f: &Finding) -> String {
    let mut s = String::new();
    for (who, v) in &f.votes {
        let what = match &v.choice {
            Choice::Agree => "agree".to_string(),
            Choice::Disagree => "disagree".to_string(),
            Choice::Duplicate(t) => format!("duplicate of {t}"),
        };
        let _ = writeln!(
            s,
            "    {who}: {what}{} (round {})",
            v.reason.as_deref().map(|r| format!(" — {r}")).unwrap_or_default(),
            v.round
        );
    }
    s
}

/// Every finding in the review, with votes cast so far.
pub fn findings_for(r: &Review, round: u8) -> String {
    if r.findings.is_empty() {
        return "No findings.".into();
    }
    let mut s = String::from("All findings:\n");
    for f in &r.findings {
        let _ = writeln!(s, "{}", finding_line(f));
        if round > 1 || f.revision > 0 {
            s.push_str(&votes_lines(f));
        }
    }
    let summaries: Vec<String> = r
        .slots
        .iter()
        .filter_map(|sl| {
            sl.submission.as_ref().map(|x| {
                format!(
                    "- {} suggested {} at {}{}",
                    sl.agent,
                    x.verdict.as_str(),
                    short_sha(&x.head_sha),
                    if x.summary.is_empty() { String::new() } else { format!(": {}", x.summary) }
                )
            })
        })
        .collect();
    if !summaries.is_empty() {
        let _ = write!(s, "Reviewer summaries:\n{}\n", summaries.join("\n"));
    }
    s
}

pub fn vote_task(r: &Review, agent: &str, owed: &[&Finding], left: &str) -> String {
    let round = r.round();
    let mut s = if round == 1 {
        format!(
            "Cross-examination round 1 of {}: all findings are revealed. Check each finding below against the code before you vote.\n",
            r.params.consensus.vote_rounds
        )
    } else {
        format!(
            "Cross-examination round {round}: these findings drew disagreement. Read the reasons, re-check the code, and vote again (you may change your mind).\n"
        )
    };
    s.push_str("You must vote on:\n");
    for f in owed {
        let _ = writeln!(s, "{}", finding_line(f));
        if round > 1 || f.revision > 0 {
            s.push_str(&votes_lines(f));
        }
        if f.by.is_none() {
            s.push_str("    (carried over: agree = still present at the new head, disagree = fixed)\n");
        }
    }
    let _ = writeln!(
        s,
        "NEXT: call `vote` with review_id \"{}\" and votes like [{{finding: \"{}\", vote: \"agree\"}}, {{finding: \"F2\", vote: \"disagree\", reason: \"why it is wrong\"}}, {{finding: \"F3\", vote: \"duplicate\", duplicate_of: \"F1\"}}]. \
         A disagree vote needs a concrete reason. Use `say` to discuss with the other reviewers. Round ends in {left}; reviewers who have not voted by then are dropped.",
        r.id,
        owed.first().map(|f| f.id.as_str()).unwrap_or("F1")
    );
    let mine: Vec<&Finding> = r
        .findings
        .iter()
        .filter(|f| f.by.as_deref() == Some(agent) && f.withdrawn.is_none() && f.open_round == round)
        .collect();
    if !mine.is_empty() && round > 1 {
        let ids: Vec<&str> = mine.iter().map(|f| f.id.as_str()).collect();
        let _ = writeln!(
            s,
            "Your own findings {} are disputed: you may `amend_finding` (clarify, change severity) or `withdraw_finding` if the objection is right.",
            ids.join(", ")
        );
    }
    s.push('\n');
    s.push_str(&findings_for(r, round));
    s
}

fn outcome_line(f: &Finding, carried_label: bool) -> String {
    let o = f.outcome.as_ref().expect("decided");
    let mut s = format!("- {} [{}", f.id, o.severity.as_str());
    if o.downgraded {
        s.push_str(", downgraded from blocking: not unanimous");
    }
    let _ = write!(s, "] {}", f.text.trim());
    if let Some(u) = &f.url {
        let _ = write!(s, " ({u})");
    }
    match (&f.by, &f.origin) {
        (Some(b), _) => {
            let _ = write!(s, " — by {b}");
        }
        (None, Some(o)) if carried_label => {
            let _ = write!(s, " — from {o}");
        }
        _ => {}
    }
    let _ = write!(s, "; {} agree, {} disagree", o.agree, o.disagree);
    s
}

pub fn result(r: &Review, web: &str) -> String {
    let verdict = crate::relay::verdict_of(&r.findings);
    let mut s = format!(
        "[{}] Review result for {}: {}\n",
        r.id,
        r.pr.url(),
        match verdict {
            Verdict::Approve => "APPROVE",
            Verdict::ChangesRequested => "CHANGES REQUESTED",
        }
    );
    s.push_str("Reviewers:\n");
    for sl in &r.slots {
        let _ = write!(s, "- {}", sl.agent);
        if let Some(x) = &sl.submission {
            let _ = write!(s, " suggested {} at {}", x.verdict.as_str(), short_sha(&x.head_sha));
            if !x.summary.is_empty() {
                let _ = write!(s, ": {}", x.summary.lines().next().unwrap_or(""));
            }
        }
        if let Some(d) = &sl.dropped {
            let _ = write!(s, " (dropped: {d})");
        }
        s.push('\n');
    }
    let by_status = |pred: &dyn Fn(&Status) -> bool| -> Vec<&Finding> {
        r.findings
            .iter()
            .filter(|f| f.outcome.as_ref().is_some_and(|o| pred(&o.status)))
            .collect()
    };
    let (new_confirmed, carried_open): (Vec<&Finding>, Vec<&Finding>) =
        by_status(&|s| *s == Status::Confirmed).into_iter().partition(|f| f.by.is_some());
    if !carried_open.is_empty() {
        s.push_str("\nPreviously raised and STILL PRESENT:\n");
        for f in &carried_open {
            let _ = writeln!(s, "{}", outcome_line(f, true));
        }
    }
    let resolved: Vec<&Finding> = by_status(&|s| *s == Status::Rejected).into_iter().filter(|f| f.by.is_none()).collect();
    if !resolved.is_empty() {
        s.push_str("\nPreviously raised and now resolved:\n");
        for f in resolved {
            let _ = writeln!(s, "- {} {}", f.id, f.text.lines().next().unwrap_or(""));
        }
    }
    if new_confirmed.is_empty() && carried_open.is_empty() {
        s.push_str("\nNo confirmed findings.\n");
    } else if !new_confirmed.is_empty() {
        s.push_str("\nConfirmed findings:\n");
        for f in &new_confirmed {
            let _ = writeln!(s, "{}", outcome_line(f, false));
            for d in by_status(&|st| *st == Status::Duplicate(f.id.clone())) {
                let _ = writeln!(s, "    merged duplicate {} by {}: {}", d.id, d.by.as_deref().unwrap_or("?"), d.text.lines().next().unwrap_or(""));
            }
        }
    }
    let disputed = by_status(&|s| *s == Status::Disputed);
    if !disputed.is_empty() {
        s.push_str("\nDisputed (reviewers split; use your judgement):\n");
        for f in disputed {
            let _ = writeln!(s, "{}", outcome_line(f, true));
            s.push_str(&votes_lines(f));
        }
    }
    let rejected: Vec<&Finding> = by_status(&|s| *s == Status::Rejected).into_iter().filter(|f| f.by.is_some()).collect();
    if !rejected.is_empty() {
        s.push_str("\nRejected by the other reviewers (no action needed):\n");
        for f in rejected {
            let _ = writeln!(s, "{}", outcome_line(f, false));
            s.push_str(&votes_lines(f));
        }
    }
    let withdrawn = by_status(&|s| *s == Status::Withdrawn);
    if !withdrawn.is_empty() {
        let ids: Vec<&str> = withdrawn.iter().map(|f| f.id.as_str()).collect();
        let _ = writeln!(s, "\nWithdrawn by their authors: {}", ids.join(", "));
    }
    let _ = write!(
        s,
        "\nTranscript: {web}/r/{} (or `review_status` with review_id \"{}\").\n",
        r.id, r.id
    );
    match verdict {
        Verdict::ChangesRequested => {
            let _ = write!(
                s,
                "NEXT: fix the confirmed findings (and any disputed ones you agree with), push, then call `request_rereview` with review_id \"{}\", new_head_sha and a note on what you changed.",
                r.id
            );
        }
        Verdict::Approve => {
            s.push_str("NEXT: nothing required. Address minor findings or nits if you like; after pushing changes you can `request_rereview`.");
        }
    }
    s
}

/// A review as seen by `viewer`: others' findings stay hidden until everyone has submitted.
pub fn status(r: &Review, viewer: &str, now: Ms, web: &str) -> String {
    let mut s = format!(
        "{} {} by {} — {}",
        r.id,
        r.pr.url(),
        r.author,
        r.phase.label()
    );
    if r.phase.active() {
        let _ = write!(s, ", phase ends in {}", dur(r.deadline_left(now)));
    }
    if let Some(p) = &r.parent {
        let _ = write!(s, ", re-review of {p}");
    }
    s.push('\n');
    if !r.note.is_empty() {
        let _ = write!(s, "Note:\n{}", quote(&r.note));
    }
    for sl in &r.slots {
        let state = match (&sl.dropped, &sl.submission) {
            (Some(d), _) => format!("dropped ({d})"),
            (None, Some(_)) => "submitted".into(),
            (None, None) => "reviewing".into(),
        };
        let _ = writeln!(s, "reviewer {}: {state}", sl.agent);
    }
    if r.revealed() {
        s.push_str(&findings_for(r, 2));
    } else {
        let mine: Vec<&Finding> = r.findings.iter().filter(|f| f.by.as_deref() == Some(viewer) || f.by.is_none()).collect();
        let hidden = r.findings.len() - mine.len();
        for f in mine {
            let _ = writeln!(s, "{}", finding_line(f));
        }
        if hidden > 0 {
            let _ = writeln!(s, "({hidden} finding(s) from other reviewers hidden until everyone has submitted)");
        }
    }
    if let Some(res) = &r.result {
        let _ = write!(s, "\n{}\n", res.text);
    }
    if let Some(f) = &r.failure {
        let _ = writeln!(s, "Failed: {f}");
    }
    if !r.log.is_empty() && r.revealed() {
        s.push_str("\nTranscript:\n");
        for e in &r.log {
            let _ = writeln!(s, "{} {} {}: {}", iso(e.at), e.who, e.kind, e.text);
        }
    }
    let _ = write!(s, "Web: {web}/r/{}", r.id);
    s
}

pub fn online(agents: &[OnlineAgent], me: &str) -> String {
    if agents.is_empty() {
        return "No agents online.".into();
    }
    let mut s = format!("{} agent(s) online:\n", agents.len());
    for a in agents {
        let _ = writeln!(
            s,
            "- {}{} ({}, {} on {}) — {}",
            a.name,
            if a.name == me { " (you)" } else { "" },
            a.role.as_str(),
            a.login,
            a.node,
            match &a.busy {
                Some(r) => format!("busy with {r}"),
                None => "idle".into(),
            }
        );
    }
    s
}

pub fn events(evs: &[AgentEvent]) -> String {
    evs.iter().map(|e| e.text.as_str()).collect::<Vec<_>>().join("\n\n---\n\n")
}

pub fn event_line(e: &AgentEvent) -> String {
    let first = e.text.lines().next().unwrap_or("");
    format!("amx {} {first}", e.review.as_deref().unwrap_or("-"))
}

pub fn instructions(agent: &Agent) -> String {
    let role_text = match agent.role {
        Role::Reviewer => "Your role is reviewer: loop on `wait` forever. When it returns, do exactly what its NEXT line says (accept an offer, review the PR and submit_findings, vote, ...), then call `wait` again. Never end your turn while a review is in progress.",
        Role::Author => "Your role is author: call `request_review` with a PR URL, then call `wait` repeatedly until the consolidated result arrives. Fix confirmed findings, push, and `request_rereview`.",
    };
    format!(
        "amx is a relay where coding agents review GitHub pull requests together while they are online. You are `{}`. {role_text} \
         The relay runs each review as a fixed protocol: offer -> independent review (findings hidden from other reviewers) -> cross-examination (vote agree/disagree/duplicate on every other finding, up to 2 rounds, discuss with `say`) -> consensus result to the author. \
         Every phase has a deadline; reviewers who miss one or go silent are dropped. Fetch PRs yourself read-only (`gh pr view`, `gh pr diff`); never comment, review or push on GitHub. \
         Every tool result ends with what to do next; follow it. Call `hello` to change role.",
        agent.name
    )
}

pub fn phase_badge(p: Phase) -> &'static str {
    match p {
        Phase::Offering => "offering",
        Phase::Reviewing => "reviewing",
        Phase::Voting(_) => "voting",
        Phase::Done => "done",
        Phase::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn iso_dates() {
        assert_eq!(super::iso(0), "1970-01-01 00:00:00Z");
        assert_eq!(super::iso(1_760_400_000_000), "2025-10-14 00:00:00Z");
    }

    #[test]
    fn durations() {
        assert_eq!(super::dur(0), "0s");
        assert_eq!(super::dur(59_001), "1m00s");
        assert_eq!(super::dur(1_200_000), "20m00s");
        assert_eq!(super::dur(7_260_000), "2h01m");
    }
}
