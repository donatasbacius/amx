use crate::model::{Finding, Kind, Verdict};
use crate::node::{Node, PostInput};
use crate::store::MarkTable;
use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const WAIT_DEFAULT: u64 = 50;
const WAIT_MAX: u64 = 600;

pub fn default_agent_for(client: &str) -> String {
    let c = client.to_ascii_lowercase();
    if c.contains("claude") {
        "claude".into()
    } else if c.contains("codex") {
        "codex".into()
    } else {
        "agent".into()
    }
}

pub fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

const INSTRUCTIONS: &str = "amx shares pull-request reviews between coding agents on different machines (and humans via a web page). \
Threads are keyed by GitHub PR URL. Code is never sent over amx: fetch PRs yourself with read-only `gh pr view`/`gh pr diff`. \
Typical flow: request_review -> another agent claim_review -> post_review -> reply/close. \
Check inbox when you start work and after finishing a task; use wait to block until something arrives. \
Never post reviews or comments to GitHub itself unless your user asks; amx is the channel.";

fn tools() -> Value {
    let pr = json!({ "type": "string", "description": "GitHub pull request URL, e.g. https://github.com/owner/repo/pull/123 (variants like /files or #diff-... are normalised)." });
    json!([
        {
            "name": "request_review",
            "description": "Ask another agent (or anyone) to review a GitHub pull request. Opens or re-opens the thread for that PR. Use after you push changes you want reviewed, including re-review requests after addressing feedback.",
            "inputSchema": { "type": "object", "properties": {
                "pr_url": pr,
                "to": { "type": "string", "description": "Optional reviewer: an agent name (e.g. bob-claude-1), agent@machine, or a human's Tailscale login. Omit to let any agent pick it up." },
                "note": { "type": "string", "description": "What to focus on, context, or what changed since the last review. Markdown." }
            }, "required": ["pr_url"] }
        },
        {
            "name": "claim_review",
            "description": "Tell others you are reviewing this PR now, so they don't duplicate the work. Informational only, nothing is locked. Call it before you start a review you picked up from your inbox.",
            "inputSchema": { "type": "object", "properties": {
                "pr_url": pr,
                "note": { "type": "string", "description": "Optional note, e.g. expected time." }
            }, "required": ["pr_url"] }
        },
        {
            "name": "post_review",
            "description": "Post your review of a PR to its amx thread (not to GitHub). Fetch the PR yourself first (e.g. `gh pr view <url> --json headRefOid,title` and `gh pr diff <url>`). Put each concrete problem in findings with a GitHub link to the exact line: https://github.com/<owner>/<repo>/blob/<head_sha>/<path>#L<line> (or #L10-L20).",
            "inputSchema": { "type": "object", "properties": {
                "pr_url": pr,
                "verdict": { "type": "string", "enum": ["approve", "request_changes", "comment"], "description": "approve = good to merge; request_changes = must fix before merge; comment = observations only." },
                "summary": { "type": "string", "description": "Overall review in Markdown: what the PR does, main concerns, what you checked." },
                "head_sha": { "type": "string", "description": "The PR head commit you reviewed (headRefOid). Lets readers see when a review is older than the latest push. Looked up automatically if omitted." },
                "findings": { "type": "array", "description": "Specific issues, most severe first.", "items": { "type": "object", "properties": {
                    "severity": { "type": "string", "enum": ["blocker", "major", "minor", "nit", "question"] },
                    "path": { "type": "string", "description": "File path in the repo." },
                    "line": { "type": "integer", "description": "Line number in the PR head version of the file." },
                    "url": { "type": "string", "description": "GitHub link to the line (blob URL with #L<line>, or the PR files view #diff-... anchor)." },
                    "text": { "type": "string", "description": "What is wrong and how to fix it." }
                }, "required": ["text"] } }
            }, "required": ["pr_url", "verdict", "summary"] }
        },
        {
            "name": "reply",
            "description": "Reply on a PR thread: answer review findings, say what you changed, ask a question. To ask for a fresh review after pushing fixes, use request_review instead.",
            "inputSchema": { "type": "object", "properties": {
                "pr_url": pr,
                "text": { "type": "string", "description": "Markdown." },
                "in_reply_to": { "type": "string", "description": "Optional id of the message you are answering." },
                "to": { "type": "string", "description": "Optional agent/human to notify specifically." }
            }, "required": ["pr_url", "text"] }
        },
        {
            "name": "close_thread",
            "description": "Mark a PR thread as done (merged, abandoned, or no further review needed). A later request_review re-opens it.",
            "inputSchema": { "type": "object", "properties": {
                "pr_url": pr,
                "note": { "type": "string" }
            }, "required": ["pr_url"] }
        },
        {
            "name": "inbox",
            "description": "List your unread amx messages: anything addressed to you, new messages on threads you have posted in, and open review requests nobody was named for. Returns full message text. Items stay unread until you call mark_read (or read the thread with the thread tool).",
            "inputSchema": { "type": "object", "properties": {
                "mark_read": { "type": "boolean", "description": "Mark the returned items read immediately. Default false." }
            } }
        },
        {
            "name": "mark_read",
            "description": "Mark inbox items as read so they stop appearing in inbox and wait.",
            "inputSchema": { "type": "object", "properties": {
                "ids": { "type": "array", "items": { "type": "string" }, "description": "Message ids from inbox." },
                "all": { "type": "boolean", "description": "Mark everything currently in your inbox read." }
            } }
        },
        {
            "name": "wait",
            "description": "Block until you have unread amx messages, then return them (same format as inbox). Returns early as soon as something arrives; returns 'No unread' on timeout. Use when you are waiting for a review or reply.",
            "inputSchema": { "type": "object", "properties": {
                "timeout_seconds": { "type": "integer", "description": "Max seconds to wait (default 50, max 600). Keep under your client's tool timeout." }
            } }
        },
        {
            "name": "threads",
            "description": "List PR review threads with status (waiting, in review, changes requested, commented, approved, closed), last activity, and whether the latest review is older than the PR's latest push.",
            "inputSchema": { "type": "object", "properties": {
                "include_closed": { "type": "boolean", "description": "Include closed threads. Default false." }
            } }
        },
        {
            "name": "thread",
            "description": "Read a PR thread in full (requests, reviews with findings, replies) and mark its messages read for you.",
            "inputSchema": { "type": "object", "properties": { "pr_url": pr }, "required": ["pr_url"] }
        }
    ])
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Args {
    pr_url: String,
    to: Option<String>,
    note: Option<String>,
    text: Option<String>,
    summary: Option<String>,
    verdict: Option<Verdict>,
    head_sha: Option<String>,
    findings: Vec<Finding>,
    in_reply_to: Option<String>,
    mark_read: bool,
    ids: Vec<String>,
    all: bool,
    timeout_seconds: Option<u64>,
    include_closed: bool,
}

async fn call_tool(node: &Arc<Node>, agent: &str, name: &str, args: Value) -> Result<String> {
    let args: Args = serde_json::from_value(if args.is_null() { json!({}) } else { args })
        .map_err(|e| anyhow!("invalid arguments: {e}"))?;
    let me = node.agent(agent);
    let post = |kind: Kind, body: Option<String>| PostInput {
        kind: Some(kind),
        pr_url: args.pr_url.clone(),
        body: body.unwrap_or_default(),
        to: args.to.clone(),
        ..Default::default()
    };
    let posted = |m: crate::model::Message| {
        format!("Posted {} {} on {} as {}.", m.kind.as_str(), m.id, m.thread, m.author.display())
    };
    match name {
        "request_review" => Ok(posted(node.post(me, post(Kind::Request, args.note.clone())).await?)),
        "claim_review" => Ok(posted(node.post(me, post(Kind::Claim, args.note.clone())).await?)),
        "close_thread" => Ok(posted(node.post(me, post(Kind::Close, args.note.clone())).await?)),
        "reply" => {
            let mut input = post(Kind::Reply, args.text.clone());
            input.in_reply_to = args.in_reply_to.clone();
            Ok(posted(node.post(me, input).await?))
        }
        "post_review" => {
            let mut input = post(Kind::Review, args.summary.clone());
            input.to = None;
            input.verdict = args.verdict;
            input.findings = args.findings.clone();
            input.head_sha = args.head_sha.clone();
            let m = node.post(me, input).await?;
            Ok(format!(
                "{}{}",
                posted(m.clone()),
                m.fields
                    .head_sha
                    .map(|s| format!(" Reviewed head {s}."))
                    .unwrap_or_default()
            ))
        }
        "inbox" => {
            let items = node.inbox(&me, false)?;
            if args.mark_read {
                let ids: Vec<String> = items.iter().map(|i| i.message.id.clone()).collect();
                node.mark(MarkTable::Read, &me, &ids)?;
            }
            Ok(crate::text::inbox(&me.display(), &items))
        }
        "mark_read" => {
            let ids = if args.all {
                node.inbox(&me, false)?.into_iter().map(|i| i.message.id).collect()
            } else {
                args.ids.clone()
            };
            let n = node.mark(MarkTable::Read, &me, &ids)?;
            Ok(format!("Marked {n} message(s) read."))
        }
        "wait" => {
            let secs = args.timeout_seconds.unwrap_or(WAIT_DEFAULT).clamp(1, WAIT_MAX);
            let items = node.wait_inbox(&me, Duration::from_secs(secs), false).await?;
            Ok(crate::text::inbox(&me.display(), &items))
        }
        "threads" => {
            let all = node.threads()?;
            let shown: Vec<_> = all
                .iter()
                .filter(|t| args.include_closed || t.status != crate::model::Status::Closed)
                .collect();
            if shown.is_empty() {
                return Ok("No threads.".into());
            }
            Ok(shown.iter().map(|t| crate::text::summary_line(t)).collect::<Vec<_>>().join("\n"))
        }
        "thread" => {
            let view = node
                .thread(&args.pr_url)?
                .ok_or_else(|| anyhow!("no amx thread for {}", args.pr_url))?;
            let ids: Vec<String> = view.messages.iter().map(|m| m.id.clone()).collect();
            node.mark(MarkTable::Read, &me, &ids)?;
            Ok(crate::text::thread(&view))
        }
        other => Err(anyhow!("unknown tool {other}")),
    }
}

/// Handles one JSON-RPC message. Returns `None` for notifications and responses.
pub async fn handle(node: &Arc<Node>, agent: &str, msg: Value) -> Option<Value> {
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
    match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = if PROTOCOL_VERSIONS.contains(&requested) {
                requested
            } else {
                PROTOCOL_VERSIONS[1]
            };
            Some(ok(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "amx", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": format!("{INSTRUCTIONS} You are `{agent}@{}`.", node.machine)
                }),
            ))
        }
        "ping" => Some(ok(id, json!({}))),
        "tools/list" => Some(ok(id, json!({ "tools": tools() }))),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            let (text, is_error) = match call_tool(node, agent, name, args).await {
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
        for t in super::tools().as_array().unwrap() {
            assert_eq!(t["inputSchema"]["type"], "object");
            assert!(t["description"].as_str().unwrap().len() > 40);
        }
    }

    #[test]
    fn agent_guess() {
        assert_eq!(super::default_agent_for("claude-code/2.1"), "claude");
        assert_eq!(super::default_agent_for("codex_cli_rs/0.160"), "codex");
        assert_eq!(super::default_agent_for("curl/8"), "agent");
    }
}
