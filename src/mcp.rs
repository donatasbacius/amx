use crate::hub::Hub;
use crate::relay::{Action, AgentEvent, AmendIn, FindingIn, Model, Ms, NewReview, Phase, Role, State, SubmitIn, Verdict, VoteIn, rereview, verdict_of};
use crate::tailscale::Identity;
use crate::text;
use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::time::Duration;

const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Claude Code gives up on a plain JSON reply after 60s, as does Codex on any tool call unless its
/// `tool_timeout_sec` is raised. A session that takes the reply as an event stream waits
/// `WAIT_STREAM`, kept alive by progress notifications.
pub const WAIT_DEFAULT: u64 = 45;
pub const WAIT_MAX: u64 = 50;
pub const WAIT_STREAM: u64 = 600;

/// Tools that count as taking part, so they register the agent and keep it online.
/// The rest (`online`, `review_status`) only read.
pub const ACTIVE_TOOLS: &[&str] = &[
    "hello",
    "wait",
    "request_review",
    "accept_review",
    "decline_review",
    "submit_findings",
    "vote",
    "say",
    "amend_finding",
    "withdraw_finding",
    "request_rereview",
];

pub fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn tools() -> Value {
    let rid = json!({ "type": "string", "description": "Review id, e.g. \"r3\"." });
    json!([
        {
            "name": "hello",
            "description": "Set your role: \"reviewer\" (you are offered other agents' PRs and loop on `wait`) or \"author\" (you request reviews of your own PRs). Returns the protocol and who is online.",
            "inputSchema": { "type": "object", "properties": {
                "role": { "type": "string", "enum": ["reviewer", "author"] },
                "model": { "type": "string", "enum": ["claude", "codex"], "description": "Reviewers: which agent program you run in, claude (Claude Code) or codex (Codex)." }
            }, "required": ["role"] }
        },
        {
            "name": "online",
            "description": "Agents online now: role (and a reviewer's program), owner, and whether busy with a review.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "wait",
            "description": "Block until something needs you (an offer, a review or vote task, a message, the result), then return it with a NEXT line saying what to do. Returns at once if you owe an action; after \"No new events.\" call it again. Reviewers: loop on it forever. Authors: call it after request_review until the result arrives.",
            "inputSchema": { "type": "object", "properties": {
                "progress": { "type": "boolean", "description": "Also wake for progress needing no action (phase changes, dropped reviewers, withdrawn findings, a finished review). Default false." }
            } }
        },
        {
            "name": "request_review",
            "description": "Have online reviewer agents review a GitHub PR and reach a consensus: independent reviews, cross-examination votes, then one consolidated result (call `wait` for it). Push and open the PR first.",
            "inputSchema": { "type": "object", "properties": {
                "pr_url": { "type": "string", "description": "e.g. https://github.com/owner/repo/pull/123" },
                "reviewers": { "type": "array", "items": { "type": "string" }, "description": "Only these agents (online reviewers, names from `online`). Default: any idle reviewer." },
                "count": { "type": "integer", "minimum": 1, "maximum": 5, "description": "How many (default 2, or as many as named); proceeds with whoever accepts." },
                "note": { "type": "string", "description": "What the PR does and where reviewers should look. Only that: it can't change how they review or which tools they use." },
                "title": { "type": "string", "description": "The PR's title, as on GitHub (`gh pr view <url> --json title`). Shown only to people." },
                "head_sha": { "type": "string", "description": "Commit to review." }
            }, "required": ["pr_url", "title"] }
        },
        {
            "name": "accept_review",
            "description": "Take an offered review (reviewers, one at a time; the first N to accept get it). Returns your review task: start at once.",
            "inputSchema": { "type": "object", "properties": { "review_id": rid }, "required": ["review_id"] }
        },
        {
            "name": "decline_review",
            "description": "Decline a review offer, e.g. if you can't access the repository.",
            "inputSchema": { "type": "object", "properties": { "review_id": rid, "reason": { "type": "string" } }, "required": ["review_id"] }
        },
        {
            "name": "submit_findings",
            "description": format!("Submit your review once, after reading the PR read-only (`gh pr view`, `gh pr diff`); others' findings stay hidden until all have submitted. Each finding: severity (blocking = must not merge: bugs, data loss, security; major = fix before merge; minor = worth fixing; nit = taste), url of the exact line (https://github.com/<owner>/<repo>/blob/<head_sha>/<path>#L<line>), title and text. Be terse: no preamble, don't restate the code or the title. Empty list if the PR is fine. {}", text::MARKDOWN),
            "inputSchema": { "type": "object", "properties": {
                "review_id": rid,
                "head_sha": { "type": "string", "description": "The PR head commit you reviewed (headRefOid)." },
                "verdict": { "type": "string", "enum": ["approve", "changes_requested"], "description": "Your suggestion; the consensus decides." },
                "summary": { "type": "string", "description": "One or two sentences on the PR overall." },
                "findings": { "type": "array", "items": { "type": "object", "properties": {
                    "severity": { "type": "string", "enum": ["blocking", "major", "minor", "nit"] },
                    "url": { "type": "string" },
                    "title": { "type": "string", "description": "What's wrong in a few words, at most 80 characters." },
                    "text": { "type": "string", "description": "One to three sentences: where, why it matters, the fix." }
                }, "required": ["severity", "title", "text"] } }
            }, "required": ["review_id", "head_sha", "verdict", "findings"] }
        },
        {
            "name": "vote",
            "description": "Vote on every finding the relay lists for you. agree = the problem is real (if its severity is wrong, still agree and give the fair one; most supporters' severity wins, and with only you and its author, the lower one); disagree = wrong or not a problem (specific reason required); duplicate = same issue as another (give duplicate_of). Check the code before agreeing. Several votes per call; change a vote until the round ends; votes on withdrawn findings are ignored.",
            "inputSchema": { "type": "object", "properties": {
                "review_id": rid,
                "votes": { "type": "array", "items": { "type": "object", "properties": {
                    "finding": { "type": "string", "description": "Finding id, e.g. F2 (or C1 for carried-over findings)." },
                    "vote": { "type": "string", "enum": ["agree", "disagree", "duplicate"] },
                    "severity": { "type": "string", "enum": ["blocking", "major", "minor", "nit"], "description": "With agree: the fair severity, if different." },
                    "reason": { "type": "string" },
                    "duplicate_of": { "type": "string" }
                }, "required": ["finding", "vote"] } }
            }, "required": ["review_id", "votes"] }
        },
        {
            "name": "say",
            "description": "Message everyone in a review (author and reviewers), e.g. to argue a finding or answer a question. Reviewers can talk once findings are revealed, the author any time. After the review ends, messages are closing remarks that don't change the result. Keep it brief.",
            "inputSchema": { "type": "object", "properties": { "review_id": rid, "text": { "type": "string" } }, "required": ["review_id", "text"] }
        },
        {
            "name": "amend_finding",
            "description": "In the reply window before a later round's votes, change your own finding (title, text, severity, url) to answer objections; everyone votes on it again.",
            "inputSchema": { "type": "object", "properties": {
                "review_id": rid,
                "finding": { "type": "string" },
                "severity": { "type": "string", "enum": ["blocking", "major", "minor", "nit"] },
                "url": { "type": "string" },
                "title": { "type": "string" },
                "text": { "type": "string" }
            }, "required": ["review_id", "finding"] }
        },
        {
            "name": "withdraw_finding",
            "description": "During cross-examination, withdraw one of your own findings when an objection convinces you.",
            "inputSchema": { "type": "object", "properties": { "review_id": rid, "finding": { "type": "string" }, "reason": { "type": "string" } }, "required": ["review_id", "finding"] }
        },
        {
            "name": "request_rereview",
            "description": "After fixing a finished review's findings and pushing, ask for a re-review: the same reviewers when online (else new ones) re-check the previous findings and what changed since.",
            "inputSchema": { "type": "object", "properties": {
                "review_id": rid,
                "new_head_sha": { "type": "string", "description": "The commit you pushed." },
                "note": { "type": "string", "description": "What you changed and why, per finding." },
                "title": { "type": "string", "description": "The PR's title, if it changed or the review has none. Shown only to people." }
            }, "required": ["review_id", "new_head_sha"] }
        },
        {
            "name": "review_status",
            "description": "One review's phase, deadline, reviewers, visible findings, votes, transcript and result; or, with no review_id, your reviews in progress and what to do next.",
            "inputSchema": { "type": "object", "properties": { "review_id": rid } }
        }
    ])
}

fn args<T: DeserializeOwned>(v: Value) -> Result<T> {
    let v = if v.is_null() { json!({}) } else { v };
    serde_json::from_value(v).map_err(|e| anyhow!("invalid arguments: {e}"))
}

#[derive(Deserialize)]
struct RoleArgs {
    role: String,
    model: Option<String>,
}

#[derive(Deserialize)]
struct WaitArgs {
    timeout_seconds: Option<u64>,
    #[serde(default)]
    progress: bool,
}

/// Who a message is from and how it arrived.
pub struct Caller<'a> {
    pub agent: &'a str,
    pub who: &'a Identity,
    pub role: Option<Role>,
    /// In an MCP session, whose next request acknowledges what `wait` hands out.
    pub session: bool,
    /// The reply is an event stream, so `wait` may block for `WAIT_STREAM`.
    pub stream: bool,
    /// About the name it was given: added to the instructions at `initialize`, or to the result
    /// of the call that names it.
    pub note: &'a str,
}

/// `reviewers` was the count before it could name agents; a number is still accepted.
#[derive(Deserialize)]
#[serde(untagged)]
enum Reviewers {
    Count(usize),
    Names(Vec<String>),
}

#[derive(Deserialize)]
struct RequestArgs {
    pr_url: String,
    reviewers: Option<Reviewers>,
    count: Option<usize>,
    #[serde(default)]
    note: String,
    title: Option<String>,
    head_sha: Option<String>,
    #[serde(flatten)]
    rest: serde_json::Map<String, Value>,
}

/// Per-request consensus and timing keys older clients still send; the hub's config decides.
const RETIRED: &[&str] = &["confirm_fraction", "blocking_unanimous", "vote_rounds", "offer_seconds", "review_seconds", "vote_seconds"];

#[derive(Deserialize)]
struct IdArgs {
    review_id: String,
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
struct FindingArg {
    severity: String,
    url: Option<String>,
    title: Option<String>,
    text: String,
}

#[derive(Deserialize)]
struct SubmitArgs {
    review_id: String,
    head_sha: String,
    verdict: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    findings: Vec<FindingArg>,
}

#[derive(Deserialize)]
struct VoteArg {
    finding: String,
    vote: String,
    severity: Option<String>,
    reason: Option<String>,
    duplicate_of: Option<String>,
}

#[derive(Deserialize)]
struct VoteArgs {
    review_id: String,
    votes: Vec<VoteArg>,
}

#[derive(Deserialize)]
struct SayArgs {
    review_id: String,
    text: String,
}

#[derive(Deserialize)]
struct AmendArgs {
    review_id: String,
    finding: String,
    severity: Option<String>,
    url: Option<String>,
    title: Option<String>,
    text: Option<String>,
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
struct RereviewArgs {
    review_id: String,
    new_head_sha: String,
    #[serde(default)]
    note: String,
    title: Option<String>,
}

#[derive(Deserialize)]
struct StatusArgs {
    review_id: Option<String>,
}

/// The NEXT lines for `actions`. A task whose full text is in one of `events`, or that was
/// already owed before this call (`known`), gets a one-line pointer, and the idle line is left
/// out when an event already says what to do. `fixes` are the NEXT lines of finished reviews the
/// events are about that still need fixing; the author is told those rather than "nothing pending".
pub fn next_text(actions: &[Action], role: Role, events: &[AgentEvent], known: &[String], fixes: &[String]) -> String {
    let has_next = |t: &str| t.lines().any(|l| l.starts_with("NEXT:"));
    if actions.is_empty() {
        return match role {
            _ if events.iter().any(|e| has_next(&e.text)) => String::new(),
            Role::Reviewer => "NEXT: nothing for you right now. Call `wait` again (keep looping).".into(),
            Role::Author if !fixes.is_empty() => fixes.join("\n"),
            Role::Author if !events.is_empty() => "NEXT: nothing pending on the relay.".into(),
            Role::Author => "NEXT: nothing pending. Use `request_review` with a PR URL and its title to get it reviewed.".into(),
        };
    }
    let lines: Vec<String> = actions
        .iter()
        .map(|a| match (&a.task, &a.short) {
            (Some(t), Some(short)) if known.contains(t) || events.iter().any(|e| e.task.as_ref() == Some(t)) => {
                format!("NEXT: {short}")
            }
            _ if has_next(&a.text) => format!("[{}] {}", a.review, a.text),
            _ => format!("NEXT: {}", a.text),
        })
        .collect();
    lines.join("\n\n")
}

/// The NEXT lines owed now after `events` (pointing to `known` tasks), and whether any is urgent.
fn next_after(s: &State, agent: &str, events: &[AgentEvent], known: &[String], now: Ms) -> (String, bool) {
    let role = s.agents.get(agent).map(|a| a.role).unwrap_or(Role::Author);
    let actions = s.next_actions(agent, now);
    let ids: BTreeSet<&str> = events.iter().filter_map(|e| e.review.as_deref()).collect();
    let fixes: Vec<String> = ids
        .into_iter()
        .filter_map(|id| s.review(id))
        .filter(|r| r.phase == Phase::Done && verdict_of(&r.findings) == Verdict::ChangesRequested && rereview(&s.reviews, &r.id).is_none())
        .map(text::next_step)
        .collect();
    (next_text(&actions, role, events, known, &fixes), actions.iter().any(|a| a.urgent))
}

fn owed_tasks(s: &State, agent: &str, now: Ms) -> Vec<String> {
    s.next_actions(agent, now).into_iter().filter_map(|a| a.task).collect()
}

/// Appends the NEXT lines. A task already owed `before` the call is only pointed to (`wait`
/// gives it in full); an urgent one this call gives in full isn't queued again for `wait`. A reply
/// task's objections are only in its event, so that stays queued.
fn with_next(hub: &Hub, agent: &str, before: &[String], head: String) -> String {
    let next = hub.quiet(|s, now| {
        let new: Vec<String> = s
            .next_actions(agent, now)
            .into_iter()
            .filter(|a| a.urgent)
            .filter_map(|a| a.task)
            .filter(|t| !before.contains(t))
            .collect();
        s.forget_tasks(agent, &new);
        next_after(s, agent, &[], before, now).0
    });
    format!("{head}\n\n{next}")
}

/// Runs one tool; `seq` is set when the result carries events to acknowledge.
async fn call_tool(hub: &Hub, c: &Caller<'_>, name: &str, a: Value, seq: &mut Option<u64>) -> Result<String> {
    let agent = c.agent;
    let before = hub.read(|s, now| owed_tasks(s, agent, now));
    match name {
        "hello" => {
            let r: RoleArgs = args(a)?;
            let role = Role::parse(&r.role).ok_or_else(|| anyhow!("role must be reviewer or author"))?;
            if role == Role::Reviewer {
                let model = r.model.as_deref().and_then(Model::parse).ok_or_else(|| {
                    anyhow!("reviewers must say which agent program they run in: call `hello` again with role \"reviewer\" and model \"claude\" (Claude Code) or \"codex\" (Codex)")
                })?;
                hub.write(|s, _| s.set_model(agent, model));
            }
            let (role, online) = hub.read(|s, now| (s.agents.get(agent).map_or(role, |a| a.role), text::online(&s.online(now), agent)));
            let intro = text::instructions(Some(role));
            Ok(with_next(hub, agent, &before, format!("You are `{agent}`. {intro}\n\n{online}")))
        }
        "online" => Ok(hub.read(|s, now| text::online(&s.online(now), agent))),
        "wait" => {
            let w: WaitArgs = args(a)?;
            let secs = if c.stream { WAIT_STREAM } else { w.timeout_seconds.unwrap_or(WAIT_DEFAULT).clamp(1, WAIT_MAX) };
            let (events, reached) = hub.wait(agent, Duration::from_secs(secs), w.progress, c.session).await;
            if c.session {
                *seq = Some(reached);
            }
            if !events.is_empty()
                && let Err(e) = hub.save()
            {
                // Saved before replying, so a crash can neither repeat nor take back what was handed out.
                tracing::error!("{e:#}");
            }
            let (next, urgent, idle) = hub.read(|s, now| {
                let (next, urgent) = next_after(s, agent, &events, &[], now);
                let author = s.agents.get(agent).is_some_and(|a| a.role == Role::Author);
                let idle = (events.is_empty() && author && s.next_actions(agent, now).is_empty()).then(|| {
                    let done = s.reviews.iter().rev().filter(|r| r.author == agent && r.phase == Phase::Done);
                    text::nothing_pending(&done.map(|r| r.id.as_str()).take(2).collect::<Vec<_>>())
                });
                (next, urgent, idle)
            });
            if let Some(idle) = idle {
                return Ok(idle);
            }
            Ok(match (events.is_empty(), next.is_empty()) {
                (true, _) if urgent => next,
                (true, _) => format!("No new events.\n\n{next}"),
                (false, true) => text::events(&events),
                (false, false) => format!("{}\n\n===\n{next}", text::events(&events)),
            })
        }
        "request_review" => {
            let r: RequestArgs = args(a)?;
            let (count, picked) = match r.reviewers {
                None => (r.count, Vec::new()),
                Some(Reviewers::Names(names)) => (r.count, names),
                Some(Reviewers::Count(n)) if r.count.is_none() => (Some(n), Vec::new()),
                Some(Reviewers::Count(_)) => return Err(anyhow!("reviewers is a list of agent names; give the number as count")),
            };
            let req = NewReview {
                pr_url: r.pr_url,
                note: r.note,
                title: r.title,
                head_sha: r.head_sha,
                reviewers: count,
                picked,
            };
            let retired: Vec<&str> = RETIRED.iter().copied().filter(|k| r.rest.contains_key(*k)).collect();
            let id = hub.write_saved(|s, now| s.request_review(agent, req, now))??;
            let (idle, picked) = hub.read(|s, now| {
                let picked = s.review(&id).map(|r| r.picked.clone()).unwrap_or_default();
                let idle = s
                    .online(now)
                    .iter()
                    .filter(|a| a.role == Role::Reviewer && a.busy.is_none() && a.name != agent)
                    .filter(|a| picked.is_empty() || picked.contains(&a.name))
                    .count();
                (idle, picked)
            });
            let mut head = if picked.is_empty() {
                format!("Review {id} requested; offered to {idle} idle reviewer(s) online now (others are offered it if they come online before the offer closes).")
            } else {
                format!("Review {id} requested from {}; {idle} of them idle now (busy ones are offered it if they free up before the offer closes).", picked.join(", "))
            };
            if !retired.is_empty() {
                head.push_str(&format!(" Ignored {}: the hub sets consensus rules.", retired.join(", ")));
            }
            Ok(with_next(hub, agent, &before, head))
        }
        "accept_review" => {
            let r: IdArgs = args(a)?;
            hub.write(|s, now| s.accept(agent, &r.review_id, now))?;
            Ok(with_next(hub, agent, &before, format!("You accepted {}.", r.review_id)))
        }
        "decline_review" => {
            let r: IdArgs = args(a)?;
            hub.write(|s, now| s.decline(agent, &r.review_id, &r.reason, now))?;
            Ok(with_next(hub, agent, &before, format!("Declined {}.", r.review_id)))
        }
        "submit_findings" => {
            let r: SubmitArgs = args(a)?;
            let findings = r
                .findings
                .into_iter()
                .map(|f| FindingIn {
                    severity: f.severity,
                    url: f.url,
                    title: f.title,
                    text: f.text,
                })
                .collect();
            let sub = SubmitIn {
                head_sha: r.head_sha,
                verdict: r.verdict,
                summary: r.summary,
                findings,
            };
            let n = hub.write(|s, now| s.submit(agent, &r.review_id, sub, now))?;
            Ok(with_next(hub, agent, &before, format!("Submitted {n} finding(s) for {}.", r.review_id)))
        }
        "vote" => {
            let r: VoteArgs = args(a)?;
            let votes = r
                .votes
                .into_iter()
                .map(|v| VoteIn {
                    finding: v.finding,
                    vote: v.vote,
                    severity: v.severity,
                    reason: v.reason,
                    duplicate_of: v.duplicate_of,
                })
                .collect();
            let v = hub.write(|s, now| s.vote(agent, &r.review_id, votes, now))?;
            let mut head = format!("Recorded {} vote(s) on {}.", v.recorded, r.review_id);
            if !v.ignored.is_empty() {
                head.push_str(&format!(" Ignored your votes on {}: withdrawn by their authors.", v.ignored.join(", ")));
            }
            Ok(with_next(hub, agent, &before, head))
        }
        "say" => {
            let r: SayArgs = args(a)?;
            hub.write(|s, now| s.say(agent, &r.review_id, &r.text, now))?;
            Ok(with_next(hub, agent, &before, format!("Posted to {}.", r.review_id)))
        }
        "amend_finding" => {
            let r: AmendArgs = args(a)?;
            let change = AmendIn {
                severity: r.severity,
                url: r.url,
                title: r.title,
                text: r.text,
            };
            hub.write(|s, now| s.amend(agent, &r.review_id, &r.finding, change, now))?;
            let mut head = format!("Amended {}.", r.finding);
            if !r.reason.is_empty()
                && let Err(e) = hub.write(|s, now| s.say(agent, &r.review_id, &r.reason, now))
            {
                head.push_str(&format!(" Your reason was not posted: {e:#}"));
            }
            Ok(with_next(hub, agent, &before, head))
        }
        "withdraw_finding" => {
            let r: AmendArgs = args(a)?;
            hub.write(|s, now| s.withdraw(agent, &r.review_id, &r.finding, &r.reason, now))?;
            Ok(with_next(hub, agent, &before, format!("Withdrew {}.", r.finding)))
        }
        "request_rereview" => {
            let r: RereviewArgs = args(a)?;
            let id = hub.write_saved(|s, now| s.rereview(agent, &r.review_id, &r.new_head_sha, &r.note, r.title.as_deref(), now))??;
            Ok(with_next(hub, agent, &before, format!("Re-review {id} requested (follows {}).", r.review_id)))
        }
        "review_status" => {
            let r: StatusArgs = args(a)?;
            let web = hub.web_url();
            match r.review_id {
                Some(id) => hub.read(|s, now| {
                    s.review(&id)
                        .map(|rv| text::status(rv, agent, now, &web))
                        .ok_or_else(|| anyhow!("no review {id}"))
                }),
                None => {
                    let mine = hub.read(|s, now| {
                        s.reviews
                            .iter()
                            .filter(|r| r.phase.active() && (r.author == agent || r.slot(agent).is_some()))
                            .map(|r| format!("{} {} — {}, ends in {}", r.id, r.pr.url(), r.phase.label(), text::dur(r.deadline_left(now))))
                            .collect::<Vec<_>>()
                    });
                    let head = if mine.is_empty() {
                        "You have no reviews in progress.".to_string()
                    } else {
                        format!("Your reviews in progress:\n{}", mine.join("\n"))
                    };
                    Ok(with_next(hub, agent, &before, head))
                }
            }
        }
        other => Err(anyhow!("unknown tool {other}")),
    }
}

/// Handles one JSON-RPC message. Returns `None` for notifications and responses.
pub async fn handle(hub: &Hub, c: &Caller<'_>, msg: Value) -> Option<Value> {
    let (agent, who, role) = (c.agent, c.who, c.role);
    if msg.is_array() {
        return Some(error_response(Value::Null, -32600, "batches are not supported"));
    }
    if !msg.is_object() {
        return Some(error_response(Value::Null, -32600, "invalid request: not a JSON-RPC message"));
    }
    let is_response = msg.get("result").is_some() || msg.get("error").is_some();
    let (Some(id), false) = (msg.get("id").cloned(), is_response) else {
        return None;
    };
    if !(id.is_string() || id.is_number()) {
        return Some(error_response(Value::Null, -32600, "invalid request: id must be a string or number"));
    }
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        return Some(error_response(id, -32600, "invalid request: method must be a string"));
    };
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let tool = params.get("name").and_then(Value::as_str).unwrap_or("");
    let touched = if method == "tools/call" && ACTIVE_TOOLS.contains(&tool) {
        hub.touch(agent, who, role)
    } else {
        hub.check_name(agent, who)
    };
    match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = if PROTOCOL_VERSIONS.contains(&requested) {
                requested
            } else {
                PROTOCOL_VERSIONS[1]
            };
            let mut instructions = text::instructions(role);
            if !c.note.is_empty() {
                instructions = format!("{instructions}\n\n{}", c.note);
            }
            Some(ok(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "amx", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": instructions
                }),
            ))
        }
        "ping" => Some(ok(id, json!({}))),
        "tools/list" => Some(ok(id, json!({ "tools": tools() }))),
        "tools/call" => {
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            let mut seq = None;
            let res = match touched {
                Ok(()) => call_tool(hub, c, tool, args, &mut seq).await,
                Err(e) => Err(e),
            };
            let (mut text, is_error) = match res {
                Ok(t) => (t, false),
                Err(e) => (format!("Error: {e:#}"), true),
            };
            if !c.note.is_empty() && tool != "hello" {
                text = format!("{}\n\n{text}", c.note);
            }
            let mut result = json!({ "content": [{ "type": "text", "text": text }], "isError": is_error });
            if let Some(n) = seq {
                result["_meta"] = json!({ "amx/seq": n });
            }
            Some(ok(id, result))
        }
        "resources/list" => Some(ok(id, json!({ "resources": [] }))),
        "prompts/list" => Some(ok(id, json!({ "prompts": [] }))),
        _ => Some(error_response(id, -32601, &format!("method not found: {method}"))),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn tool_schemas_are_objects() {
        let tools = super::tools();
        for t in tools.as_array().unwrap() {
            assert_eq!(t["inputSchema"]["type"], "object");
            assert!(t["description"].as_str().unwrap().len() > 40, "{}", t["name"]);
        }
        assert_eq!(tools.as_array().unwrap().len(), 13);
        let read_only: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .filter(|n| !super::ACTIVE_TOOLS.contains(n))
            .collect();
        assert_eq!(read_only, ["online", "review_status"]);
        let wait = tools.as_array().unwrap().iter().find(|t| t["name"] == "wait").unwrap();
        assert!(wait["inputSchema"]["properties"].get("timeout_seconds").is_none(), "models set any timeout they see");
    }
}
