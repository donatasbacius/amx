use crate::client::Client;
use crate::node::InboxItem;
use serde_json::{Value, json};

const MAX_CONTEXT: usize = 12_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    SessionStart,
    UserPromptSubmit,
    Stop,
}

impl Event {
    pub fn parse(s: &str) -> Option<Event> {
        match s.to_ascii_lowercase().replace(['-', '_'], "").as_str() {
            "sessionstart" => Some(Event::SessionStart),
            "userpromptsubmit" => Some(Event::UserPromptSubmit),
            "stop" => Some(Event::Stop),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Event::SessionStart => "SessionStart",
            Event::UserPromptSubmit => "UserPromptSubmit",
            Event::Stop => "Stop",
        }
    }
}

fn context_text(agent: &str, items: &[InboxItem]) -> String {
    let mut text = format!(
        "[amx] New pull-request review mail for {agent}. Handle it with the amx MCP tools \
         (thread to read a whole PR thread, post_review / reply / claim_review to respond, mark_read when done). \
         Fetch PR code yourself with read-only gh commands.\n\n{}",
        crate::text::inbox(agent, items)
    );
    if text.len() > MAX_CONTEXT {
        let cut = (0..=MAX_CONTEXT).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
        text.truncate(cut);
        text.push_str("\n… (truncated; call the amx inbox tool for the rest)");
    }
    text
}

/// Builds the hook's stdout JSON. Items are marked delivered so each one is surfaced once;
/// they stay unread until the agent marks them read.
pub fn output(event: Event, agent: &str, items: &[InboxItem]) -> Option<Value> {
    if items.is_empty() {
        return None;
    }
    let text = context_text(agent, items);
    Some(match event {
        Event::Stop => json!({ "decision": "block", "reason": text }),
        _ => json!({
            "hookSpecificOutput": { "hookEventName": event.name(), "additionalContext": text }
        }),
    })
}

/// Never fails the agent's turn: any problem (node down, bad input) yields no output.
pub async fn run(client: &Client, agent: &str, event: Option<Event>, stdin: &str) -> Option<Value> {
    let input: Value = serde_json::from_str(stdin).unwrap_or(Value::Null);
    let event = event.or_else(|| input.get("hook_event_name").and_then(Value::as_str).and_then(Event::parse))?;
    let undelivered = event != Event::SessionStart;
    let items = match client.inbox(agent, undelivered, false, false).await {
        Ok(items) => items,
        Err(e) => {
            eprintln!("amx hook: {e:#}");
            return None;
        }
    };
    let out = output(event, agent, &items)?;
    let ids: Vec<String> = items.iter().map(|i| i.message.id.clone()).collect();
    if let Err(e) = client.mark(agent, &ids, true).await {
        eprintln!("amx hook: {e:#}");
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, Fields, Kind, Message, new_id};
    use crate::node::Reason;

    fn item() -> InboxItem {
        InboxItem {
            reason: Reason::AddressedToYou,
            title: Some("Fix bug".into()),
            message: Message {
                id: new_id(),
                origin: "o".into(),
                seq: 1,
                hlc: 1,
                thread: "https://github.com/a/b/pull/1".into(),
                kind: Kind::Request,
                author: Author::agent("alice", "box"),
                body: "please look".into(),
                fields: Fields::default(),
            },
        }
    }

    #[test]
    fn event_names() {
        assert_eq!(Event::parse("session-start"), Some(Event::SessionStart));
        assert_eq!(Event::parse("UserPromptSubmit"), Some(Event::UserPromptSubmit));
        assert_eq!(Event::parse("stop"), Some(Event::Stop));
        assert_eq!(Event::parse("PreToolUse"), None);
    }

    #[test]
    fn outputs_match_codex_hook_schema() {
        assert!(output(Event::Stop, "me", &[]).is_none());
        let stop = output(Event::Stop, "me", &[item()]).unwrap();
        assert_eq!(stop["decision"], "block");
        assert!(stop["reason"].as_str().unwrap().contains("please look"));
        let ups = output(Event::UserPromptSubmit, "me", &[item()]).unwrap();
        assert_eq!(ups["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        assert!(ups["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("a/b/pull/1"));
    }
}
