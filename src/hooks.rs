use crate::client::Client;
use serde_json::{Value, json};
use std::time::Duration;

const MAX_REASON: usize = 6000;

/// Stop-hook output (same schema for Codex and Claude Code): block stopping while the agent
/// owes the relay something, or always for a looping reviewer when `keep_waiting` is set.
pub fn output(obligation: &Value, keep_waiting: bool) -> Option<Value> {
    let active = obligation["active"].as_bool().unwrap_or(false);
    let reviewer = obligation["role"].as_str() == Some("reviewer");
    let mut reason = if active {
        format!(
            "[amx] You still have an active review obligation on the relay; do not stop.\n{}",
            obligation["text"].as_str().unwrap_or("Call the amx `wait` tool.")
        )
    } else if keep_waiting && reviewer {
        "[amx] You are a looping reviewer: call the amx `wait` tool again and handle what it returns.".to_string()
    } else {
        return None;
    };
    if reason.len() > MAX_REASON {
        let cut = (0..=MAX_REASON).rev().find(|&i| reason.is_char_boundary(i)).unwrap_or(0);
        reason.truncate(cut);
        reason.push('…');
    }
    Some(json!({ "decision": "block", "reason": reason }))
}

/// Never fails the agent's turn: if the relay is unreachable the agent may stop.
pub async fn stop(client: &Client, keep_waiting: bool) -> Option<Value> {
    match client.obligation(Duration::from_secs(8)).await {
        Ok(v) => output(&v, keep_waiting),
        Err(e) => {
            eprintln!("amx hook: {e:#}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_only_with_obligation() {
        let idle = json!({"active": false, "role": "reviewer", "text": "NEXT: wait"});
        assert!(output(&idle, false).is_none());
        assert_eq!(output(&idle, true).unwrap()["decision"], "block");
        let author = json!({"active": false, "role": "author", "text": ""});
        assert!(output(&author, true).is_none());
        let busy = json!({"active": true, "role": "author", "text": "NEXT: call `wait`"});
        let out = output(&busy, false).unwrap();
        assert_eq!(out["decision"], "block");
        assert!(out["reason"].as_str().unwrap().contains("call `wait`"));
    }
}
