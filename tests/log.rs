//! The relay's own log, in a test binary of its own: tracing caches whether anyone listens, so
//! tests running alongside without a subscriber could hide its lines.

use amx::config::{Consensus, Timing};
use amx::relay::{FindingIn, Model, NewReview, Role, State, SubmitIn, VoteIn};
use amx::tailscale::Identity;
use std::sync::{Arc, Mutex};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn who(login: &str) -> Identity {
    Identity { login: login.into(), display_name: String::new(), node: "box".into(), user_id: 0 }
}

fn request(s: &mut State, now: u64) -> String {
    let mut req = NewReview::new("https://github.com/example-org/widgets/pull/7");
    req.title = Some("Add a cache".into());
    s.request_review("alice", req, now).unwrap()
}

#[test]
fn each_review_that_ends_is_logged_on_one_line_without_text() {
    let lines = Lines::default();
    let sub = tracing_subscriber::fmt().with_writer({
        let lines = lines.clone();
        move || lines.clone()
    });
    tracing::subscriber::with_default(sub.with_ansi(false).without_time().finish(), || {
        let mut s = State::new(Timing::default(), Consensus::default(), 50);
        let mut now = 1_760_000_000_000;
        s.touch("alice", &who("alice@example.com"), Some(Role::Author), now).unwrap();
        for (name, login, model) in [("rev-a", "a@example.com", Model::Claude), ("rev-b", "b@example.com", Model::Codex)] {
            s.touch(name, &who(login), Some(Role::Reviewer), now).unwrap();
            s.set_model(name, model);
        }
        let id = request(&mut s, now);
        s.accept("rev-a", &id, now).unwrap();
        s.accept("rev-b", &id, now).unwrap();
        now += 60_000;
        let finding = FindingIn { severity: "major".into(), url: None, title: Some("SECRET-TITLE".into()), text: "SECRET-FINDING".into() };
        let sub = |findings: Vec<FindingIn>| SubmitIn { head_sha: SHA.into(), verdict: "approve".into(), summary: "SECRET-SUMMARY".into(), checked: "SECRET-CHECKED".into(), findings };
        s.submit("rev-a", &id, sub(vec![finding]), now).unwrap();
        s.submit("rev-b", &id, sub(vec![]), now).unwrap();
        now += 30_000;
        let vote = VoteIn { finding: "F1".into(), vote: "agree".into(), severity: None, reason: Some("SECRET-REASON".into()), duplicate_of: None };
        s.vote("rev-b", &id, vec![vote], now).unwrap();
        let id = request(&mut s, now);
        s.cancel(&id, "alice", now).unwrap();
    });
    let out = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    let ended: Vec<&str> = out.lines().map(str::trim).filter(|l| l.contains("review ended")).collect();
    assert_eq!(
        ended,
        [
            "INFO amx::relay: review ended review=r1 outcome=changes_requested reviewers=2 dropped=0 claude=1 codex=1 rounds=1 queued_secs=0 review_secs=60 voting_secs=30",
            "INFO amx::relay: review ended review=r2 outcome=cancelled reviewers=0 dropped=0 claude=0 codex=0 rounds=0 queued_secs=0",
        ],
        "{out}"
    );
    assert!(!out.contains("SECRET") && !out.contains("example-org") && !out.contains("Add a cache"), "{out}");
}
