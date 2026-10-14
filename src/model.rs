use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_BODY: usize = 64 * 1024;
pub const MAX_FINDINGS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorKind {
    Agent,
    Human,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    pub kind: AuthorKind,
    pub name: String,
    pub machine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
}

impl Author {
    pub fn agent(name: &str, machine: &str) -> Self {
        Author {
            kind: AuthorKind::Agent,
            name: name.to_string(),
            machine: machine.to_string(),
            login: None,
        }
    }

    /// `agent@machine`; for humans the login is shown when there is one.
    pub fn display(&self) -> String {
        match (&self.kind, &self.login) {
            (AuthorKind::Human, Some(login)) => login.clone(),
            _ => format!("{}@{}", self.name, self.machine),
        }
    }

    pub fn id(&self) -> String {
        format!("{}@{}", self.name, self.machine)
    }

    /// `to` may be `name`, `name@machine`, or a human login.
    pub fn matches(&self, to: &str) -> bool {
        let to = to.trim();
        !to.is_empty()
            && (to.eq_ignore_ascii_case(&self.name)
                || to.eq_ignore_ascii_case(&self.id())
                || self.login.as_deref().is_some_and(|l| l.eq_ignore_ascii_case(to)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Request,
    Claim,
    Review,
    Reply,
    Close,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Request => "request",
            Kind::Claim => "claim",
            Kind::Review => "review",
            Kind::Reply => "reply",
            Kind::Close => "close",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    RequestChanges,
    Comment,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Approve => "approve",
            Verdict::RequestChanges => "request changes",
            Verdict::Comment => "comment",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Finding {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Fields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
}

/// Immutable once created. `origin`+`seq` gives a per-node gapless sequence used by sync;
/// `hlc` orders messages across nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub origin: String,
    pub seq: i64,
    pub hlc: i64,
    pub thread: String,
    pub kind: Kind,
    pub author: Author,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub fields: Fields,
}

impl Message {
    pub fn time_ms(&self) -> i64 {
        hlc_ms(self.hlc)
    }

    pub fn validate(&self) -> Result<(), String> {
        if ulid::Ulid::from_string(&self.id).is_err() {
            return Err("id is not a ULID".into());
        }
        if self.origin.is_empty() || self.origin.len() > 64 {
            return Err("bad origin".into());
        }
        if self.seq < 1 || self.hlc < 0 {
            return Err("bad seq/hlc".into());
        }
        if crate::prurl::canonical(&self.thread).as_deref() != Some(self.thread.as_str()) {
            return Err(format!("thread is not a canonical PR URL: {}", self.thread));
        }
        if self.body.len() > MAX_BODY {
            return Err("body too large".into());
        }
        if self.fields.findings.len() > MAX_FINDINGS {
            return Err("too many findings".into());
        }
        if self.author.name.is_empty() || self.author.name.len() > 100 || self.author.machine.len() > 100 {
            return Err("bad author".into());
        }
        if self.kind == Kind::Review && self.fields.verdict.is_none() {
            return Err("review without verdict".into());
        }
        Ok(())
    }
}

const COUNTER_BITS: u32 = 12;

pub fn hlc_ms(hlc: i64) -> i64 {
    hlc >> COUNTER_BITS
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Hybrid logical clock packed into an i64: milliseconds << 12 | counter.
#[derive(Default)]
pub struct Hlc {
    last: Mutex<i64>,
}

impl Hlc {
    pub fn new(last: i64) -> Self {
        Hlc {
            last: Mutex::new(last),
        }
    }

    pub fn now(&self) -> i64 {
        self.tick(now_ms())
    }

    fn tick(&self, wall_ms: i64) -> i64 {
        let mut last = self.last.lock().unwrap();
        let wall = wall_ms << COUNTER_BITS;
        *last = if wall > *last { wall } else { *last + 1 };
        *last
    }

    pub fn observe(&self, remote: i64) {
        let mut last = self.last.lock().unwrap();
        // Ignore clocks implausibly far ahead so one bad node can't drag everyone's clock.
        if remote > *last && hlc_ms(remote) < now_ms() + 24 * 3600 * 1000 {
            *last = remote;
        }
    }
}

pub fn new_id() -> String {
    ulid::Ulid::generate().to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Waiting,
    InReview,
    ChangesRequested,
    Commented,
    Approved,
    Closed,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Waiting => "waiting",
            Status::InReview => "in review",
            Status::ChangesRequested => "changes requested",
            Status::Commented => "commented",
            Status::Approved => "approved",
            Status::Closed => "closed",
        }
    }

    pub fn css(self) -> &'static str {
        match self {
            Status::Waiting => "waiting",
            Status::InReview => "inreview",
            Status::ChangesRequested => "changes",
            Status::Commented => "commented",
            Status::Approved => "approved",
            Status::Closed => "closed",
        }
    }
}

/// Messages must be in thread order. A new request reopens a closed or reviewed thread.
pub fn thread_status(msgs: &[Message]) -> Status {
    let mut status = Status::Waiting;
    for m in msgs {
        status = match m.kind {
            Kind::Request => Status::Waiting,
            Kind::Claim if status != Status::Closed => Status::InReview,
            Kind::Review => match m.fields.verdict {
                Some(Verdict::Approve) => Status::Approved,
                Some(Verdict::RequestChanges) => Status::ChangesRequested,
                _ => Status::Commented,
            },
            Kind::Close => Status::Closed,
            _ => status,
        };
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hlc_is_monotonic_even_when_wall_clock_stalls_or_goes_back() {
        let c = Hlc::default();
        let a = c.tick(1000);
        let b = c.tick(1000);
        let d = c.tick(500);
        let e = c.tick(2000);
        assert!(a < b && b < d && d < e);
        assert_eq!(hlc_ms(a), 1000);
        assert_eq!(hlc_ms(e), 2000);
    }

    #[test]
    fn hlc_observes_remote_and_stays_ahead() {
        let c = Hlc::default();
        let local = c.tick(1000);
        let remote = (5000 << COUNTER_BITS) + 7;
        c.observe(remote);
        let next = c.tick(1001);
        assert!(next > remote && next > local);
        c.observe(i64::MAX);
        assert!(c.tick(1002) < i64::MAX - 1);
    }

    #[test]
    fn ids_are_unique_and_sortable_by_creation() {
        let a = new_id();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_id();
        assert_ne!(a, b);
        assert!(a < b);
    }

    fn msg(kind: Kind, verdict: Option<Verdict>) -> Message {
        Message {
            id: new_id(),
            origin: "n".into(),
            seq: 1,
            hlc: 1,
            thread: "https://github.com/a/b/pull/1".into(),
            kind,
            author: Author::agent("x", "m"),
            body: String::new(),
            fields: Fields {
                verdict,
                ..Default::default()
            },
        }
    }

    #[test]
    fn status_follows_thread() {
        use Kind::*;
        let s = |v: Vec<Message>| thread_status(&v);
        assert_eq!(s(vec![msg(Request, None)]), Status::Waiting);
        assert_eq!(s(vec![msg(Request, None), msg(Claim, None)]), Status::InReview);
        assert_eq!(
            s(vec![msg(Request, None), msg(Review, Some(Verdict::RequestChanges)), msg(Reply, None)]),
            Status::ChangesRequested
        );
        assert_eq!(
            s(vec![msg(Review, Some(Verdict::RequestChanges)), msg(Request, None)]),
            Status::Waiting
        );
        assert_eq!(s(vec![msg(Review, Some(Verdict::Approve))]), Status::Approved);
        assert_eq!(s(vec![msg(Request, None), msg(Close, None)]), Status::Closed);
        assert_eq!(s(vec![msg(Close, None), msg(Claim, None)]), Status::Closed);
    }

    #[test]
    fn author_matching() {
        let a = Author::agent("alice-claude-1", "box1");
        assert!(a.matches("alice-claude-1"));
        assert!(a.matches("Alice-Claude-1@BOX1"));
        assert!(!a.matches("alice-claude-1@box2"));
        assert!(!a.matches(""));
        assert_eq!(a.display(), "alice-claude-1@box1");
    }

    #[test]
    fn validate_rejects_bad_messages() {
        let mut m = msg(Kind::Reply, None);
        assert!(m.validate().is_ok());
        m.thread = "https://github.com/a/b/pull/1/files".into();
        assert!(m.validate().is_err());
        let mut m = msg(Kind::Review, None);
        assert!(m.validate().is_err());
        m.fields.verdict = Some(Verdict::Comment);
        m.id = "nope".into();
        assert!(m.validate().is_err());
    }
}
