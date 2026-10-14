use crate::hub::Hub;
use crate::relay::{Action, AmendIn, FindingIn, NewReview, Role, SubmitIn, VoteIn};
use crate::tailscale::Identity;
use crate::text;
use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::time::Duration;

const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Codex's default MCP tool timeout and Claude Code's HTTP first-byte timer are both 60s.
pub const WAIT_DEFAULT: u64 = 45;
pub const WAIT_MAX: u64 = 50;

pub fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn tools() -> Value {
    let rid = json!({ "type": "string", "description": "Review id from the relay, e.g. \"r3\"." });
    json!([
        {
            "name": "hello",
            "description": "Declare your role on the relay. role \"reviewer\": you will be offered other agents' PRs to review and must then loop on `wait`. role \"author\": you only request reviews of your own PRs. Returns the protocol rules and who is online.",
            "inputSchema": { "type": "object", "properties": {
                "role": { "type": "string", "enum": ["reviewer", "author"] }
            }, "required": ["role"] }
        },
        {
            "name": "online",
            "description": "List agents currently online on the relay with their role (reviewer/author), owner and whether they are busy with a review or idle.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "wait",
            "description": "Block until something happens for you (a review offer, a phase change, a vote request, a message, a result) or the timeout passes, then return the new events and a NEXT line telling you exactly what to do. Returns immediately if you already owe an action. Reviewers: call this in a loop forever. Authors: call it after request_review until the result arrives. Also keeps you marked online.",
            "inputSchema": { "type": "object", "properties": {
                "timeout_seconds": { "type": "integer", "minimum": 1, "maximum": WAIT_MAX, "description": format!("Default {WAIT_DEFAULT}, max {WAIT_MAX}.") }
            } }
        },
        {
            "name": "request_review",
            "description": "Ask other online reviewer agents to review a GitHub pull request and reach a consensus. The relay offers it to idle reviewers, runs an independent review round, then cross-examination votes, and sends you one consolidated result (call `wait` to receive it). Push your branch and open the PR first.",
            "inputSchema": { "type": "object", "properties": {
                "pr_url": { "type": "string", "description": "e.g. https://github.com/owner/repo/pull/123" },
                "reviewers": { "type": "integer", "minimum": 1, "maximum": 5, "description": "How many reviewers (default 2). With fewer available, it proceeds with whoever accepted." },
                "note": { "type": "string", "description": "What the PR does and what reviewers should focus on." },
                "head_sha": { "type": "string", "description": "Commit SHA you want reviewed (optional)." },
                "confirm_fraction": { "type": "number", "description": "Advanced: a finding is confirmed when more than this fraction of voters agree (default 0.5)." },
                "blocking_unanimous": { "type": "boolean", "description": "Advanced: blocking findings need unanimous agreement to stay blocking (default true)." },
                "vote_rounds": { "type": "integer", "minimum": 1, "maximum": 3, "description": "Advanced: voting rounds (default 2)." },
                "offer_seconds": { "type": "integer", "description": "Advanced: how long to wait for reviewers to accept (default 120)." },
                "review_seconds": { "type": "integer", "description": "Advanced: independent review deadline (default 1200)." },
                "vote_seconds": { "type": "integer", "description": "Advanced: deadline per voting round (default 600)." }
            }, "required": ["pr_url"] }
        },
        {
            "name": "accept_review",
            "description": "Take a review you were offered (reviewers only, one review at a time). The first N to accept are assigned; you then get the review task from `wait`.",
            "inputSchema": { "type": "object", "properties": { "review_id": rid }, "required": ["review_id"] }
        },
        {
            "name": "decline_review",
            "description": "Decline a review offer, e.g. if you can't access the repository.",
            "inputSchema": { "type": "object", "properties": { "review_id": rid, "reason": { "type": "string" } }, "required": ["review_id"] }
        },
        {
            "name": "submit_findings",
            "description": "Submit your independent review once, after reading the PR read-only with `gh pr view` / `gh pr diff`. Other reviewers' findings stay hidden from you until everyone has submitted. Each finding: severity (blocking = must not merge: bugs, data loss, security; major = fix before merge; minor = worth fixing; nit = taste), url linking the exact line (https://github.com/<owner>/<repo>/blob/<head_sha>/<path>#L<line>), and text explaining the problem and the fix. Use an empty list if the PR is fine.",
            "inputSchema": { "type": "object", "properties": {
                "review_id": rid,
                "head_sha": { "type": "string", "description": "The PR head commit you reviewed (headRefOid)." },
                "verdict": { "type": "string", "enum": ["approve", "changes_requested"], "description": "Your own suggestion; the final verdict comes from the consensus." },
                "summary": { "type": "string", "description": "One or two sentences on the PR overall." },
                "findings": { "type": "array", "items": { "type": "object", "properties": {
                    "severity": { "type": "string", "enum": ["blocking", "major", "minor", "nit"] },
                    "url": { "type": "string" },
                    "text": { "type": "string" }
                }, "required": ["severity", "text"] } }
            }, "required": ["review_id", "head_sha", "verdict", "findings"] }
        },
        {
            "name": "vote",
            "description": "Cross-examination: vote on other reviewers' findings (you must vote on every one the relay lists for you). agree = the problem is real and the severity fair; disagree = wrong or not a problem (reason required, be specific); duplicate = same issue as another finding (give duplicate_of). Verify against the code before agreeing. You can send several votes in one call and change a vote until the round ends.",
            "inputSchema": { "type": "object", "properties": {
                "review_id": rid,
                "votes": { "type": "array", "items": { "type": "object", "properties": {
                    "finding": { "type": "string", "description": "Finding id, e.g. F2 (or C1 for carried-over findings)." },
                    "vote": { "type": "string", "enum": ["agree", "disagree", "duplicate"] },
                    "reason": { "type": "string" },
                    "duplicate_of": { "type": "string" }
                }, "required": ["finding", "vote"] } }
            }, "required": ["review_id", "votes"] }
        },
        {
            "name": "say",
            "description": "Post a message to everyone in a review (its author and reviewers), e.g. to argue for or against a finding or answer a question. Reviewers can talk once findings are revealed; the author can talk any time.",
            "inputSchema": { "type": "object", "properties": { "review_id": rid, "text": { "type": "string" } }, "required": ["review_id", "text"] }
        },
        {
            "name": "amend_finding",
            "description": "During cross-examination, change one of your own findings (text, severity and/or url) in response to objections. Votes on it reopen.",
            "inputSchema": { "type": "object", "properties": {
                "review_id": rid,
                "finding": { "type": "string" },
                "severity": { "type": "string", "enum": ["blocking", "major", "minor", "nit"] },
                "url": { "type": "string" },
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
            "description": "After fixing a finished review's findings and pushing, ask for a re-review. The same reviewers are reused when online (else new ones); they re-check the previous confirmed and disputed findings plus what changed since the reviewed commit, with the same consensus rules.",
            "inputSchema": { "type": "object", "properties": {
                "review_id": rid,
                "new_head_sha": { "type": "string", "description": "The commit you pushed." },
                "note": { "type": "string", "description": "What you changed and why, per finding." }
            }, "required": ["review_id", "new_head_sha"] }
        },
        {
            "name": "review_status",
            "description": "Show one review (phase, deadline, reviewers, findings you are allowed to see, votes, transcript, result), or with no review_id, your reviews in progress and what you must do next.",
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
}

#[derive(Deserialize)]
struct WaitArgs {
    timeout_seconds: Option<u64>,
}

#[derive(Deserialize)]
struct RequestArgs {
    pr_url: String,
    reviewers: Option<usize>,
    #[serde(default)]
    note: String,
    head_sha: Option<String>,
    confirm_fraction: Option<f64>,
    blocking_unanimous: Option<bool>,
    vote_rounds: Option<u8>,
    offer_seconds: Option<u64>,
    review_seconds: Option<u64>,
    vote_seconds: Option<u64>,
}

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
}

#[derive(Deserialize)]
struct StatusArgs {
    review_id: Option<String>,
}

pub fn next_text(actions: &[Action], role: Role, has_events: bool) -> String {
    if actions.is_empty() {
        return match role {
            Role::Reviewer => "NEXT: nothing for you right now. Call `wait` again (keep looping).".into(),
            Role::Author if has_events => "NEXT: nothing pending on the relay.".into(),
            Role::Author => "NEXT: nothing pending. Use `request_review` to get a PR reviewed.".into(),
        };
    }
    let lines: Vec<String> = actions
        .iter()
        .map(|a| {
            if a.text.contains("NEXT:") {
                format!("[{}] {}", a.review, a.text)
            } else {
                format!("NEXT: {}", a.text)
            }
        })
        .collect();
    lines.join("\n\n")
}

fn role_of(hub: &Hub, agent: &str) -> Role {
    hub.read(|s, _| s.agents.get(agent).map(|a| a.role).unwrap_or(Role::Author))
}

fn with_next(hub: &Hub, agent: &str, head: String) -> String {
    let actions = hub.read(|s, now| s.next_actions(agent, now));
    format!("{head}\n\n{}", next_text(&actions, role_of(hub, agent), false))
}

async fn call_tool(hub: &Hub, agent: &str, name: &str, a: Value) -> Result<String> {
    match name {
        "hello" => {
            let r: RoleArgs = args(a)?;
            let role = Role::parse(&r.role).ok_or_else(|| anyhow!("role must be reviewer or author"))?;
            hub.write(|s, now| s.set_role(agent, role, now))?;
            let (intro, online) = hub.read(|s, now| {
                (
                    s.agents.get(agent).map(text::instructions).unwrap_or_default(),
                    text::online(&s.online(now), agent),
                )
            });
            Ok(with_next(hub, agent, format!("{intro}\n\n{online}")))
        }
        "online" => Ok(hub.read(|s, now| text::online(&s.online(now), agent))),
        "wait" => {
            let w: WaitArgs = args(a)?;
            let secs = w.timeout_seconds.unwrap_or(WAIT_DEFAULT).clamp(1, WAIT_MAX);
            let (events, actions) = hub.wait(agent, Duration::from_secs(secs)).await;
            let role = role_of(hub, agent);
            let next = next_text(&actions, role, !events.is_empty());
            Ok(if events.is_empty() {
                format!("No new events.\n\n{next}")
            } else {
                format!("{}\n\n===\n{next}", text::events(&events))
            })
        }
        "request_review" => {
            let r: RequestArgs = args(a)?;
            let req = NewReview {
                pr_url: r.pr_url,
                note: r.note,
                head_sha: r.head_sha,
                reviewers: r.reviewers,
                confirm_fraction: r.confirm_fraction,
                blocking_unanimous: r.blocking_unanimous,
                vote_rounds: r.vote_rounds,
                offer_secs: r.offer_seconds,
                review_secs: r.review_seconds,
                vote_secs: r.vote_seconds,
            };
            let id = hub.write(|s, now| s.request_review(agent, req, now))?;
            let idle = hub.read(|s, now| {
                s.online(now)
                    .iter()
                    .filter(|a| a.role == Role::Reviewer && a.busy.is_none() && a.name != agent)
                    .count()
            });
            Ok(with_next(
                hub,
                agent,
                format!("Review {id} requested; offered to {idle} idle reviewer(s) online now (others are offered it if they come online before the offer closes)."),
            ))
        }
        "accept_review" => {
            let r: IdArgs = args(a)?;
            hub.write(|s, now| s.accept(agent, &r.review_id, now))?;
            Ok(with_next(hub, agent, format!("You accepted {}.", r.review_id)))
        }
        "decline_review" => {
            let r: IdArgs = args(a)?;
            hub.write(|s, now| s.decline(agent, &r.review_id, &r.reason, now))?;
            Ok(with_next(hub, agent, format!("Declined {}.", r.review_id)))
        }
        "submit_findings" => {
            let r: SubmitArgs = args(a)?;
            let findings = r
                .findings
                .into_iter()
                .map(|f| FindingIn {
                    severity: f.severity,
                    url: f.url,
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
            Ok(with_next(hub, agent, format!("Submitted {n} finding(s) for {}.", r.review_id)))
        }
        "vote" => {
            let r: VoteArgs = args(a)?;
            let votes = r
                .votes
                .into_iter()
                .map(|v| VoteIn {
                    finding: v.finding,
                    vote: v.vote,
                    reason: v.reason,
                    duplicate_of: v.duplicate_of,
                })
                .collect();
            let n = hub.write(|s, now| s.vote(agent, &r.review_id, votes, now))?;
            Ok(with_next(hub, agent, format!("Recorded {n} vote(s) on {}.", r.review_id)))
        }
        "say" => {
            let r: SayArgs = args(a)?;
            hub.write(|s, now| s.say(agent, &r.review_id, &r.text, now))?;
            Ok(with_next(hub, agent, format!("Posted to {}.", r.review_id)))
        }
        "amend_finding" => {
            let r: AmendArgs = args(a)?;
            let change = AmendIn {
                severity: r.severity,
                url: r.url,
                text: r.text,
            };
            hub.write(|s, now| s.amend(agent, &r.review_id, &r.finding, change, now))?;
            if !r.reason.is_empty() {
                let _ = hub.write(|s, now| s.say(agent, &r.review_id, &r.reason, now));
            }
            Ok(with_next(hub, agent, format!("Amended {}.", r.finding)))
        }
        "withdraw_finding" => {
            let r: AmendArgs = args(a)?;
            hub.write(|s, now| s.withdraw(agent, &r.review_id, &r.finding, &r.reason, now))?;
            Ok(with_next(hub, agent, format!("Withdrew {}.", r.finding)))
        }
        "request_rereview" => {
            let r: RereviewArgs = args(a)?;
            let id = hub.write(|s, now| s.rereview(agent, &r.review_id, &r.new_head_sha, &r.note, now))?;
            Ok(with_next(hub, agent, format!("Re-review {id} requested (follows {}).", r.review_id)))
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
                    Ok(with_next(hub, agent, head))
                }
            }
        }
        other => Err(anyhow!("unknown tool {other}")),
    }
}

/// Handles one JSON-RPC message for `agent`. Returns `None` for notifications and responses.
pub async fn handle(hub: &Hub, agent: &str, who: &Identity, role: Option<Role>, msg: Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str);
    let (Some(id), Some(method)) = (id, method) else {
        return if msg.is_array() {
            Some(error_response(Value::Null, -32600, "batches are not supported"))
        } else {
            None
        };
    };
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let touched = hub.touch(agent, who, role);
    match method {
        "initialize" => {
            if let Err(e) = touched {
                return Some(error_response(id, -32000, &format!("{e:#}")));
            }
            let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = if PROTOCOL_VERSIONS.contains(&requested) {
                requested
            } else {
                PROTOCOL_VERSIONS[1]
            };
            let instructions = hub.read(|s, _| s.agents.get(agent).map(text::instructions).unwrap_or_default());
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
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            let res = match touched {
                Ok(()) => call_tool(hub, agent, name, args).await,
                Err(e) => Err(e),
            };
            let (text, is_error) = match res {
                Ok(t) => (t, false),
                Err(e) => (format!("Error: {e:#}"), true),
            };
            Some(ok(
                id,
                json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }),
            ))
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
    }
}
