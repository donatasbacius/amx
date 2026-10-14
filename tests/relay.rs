//! The review protocol driven by scripted agents on a synthetic clock.

use amx::config::{Consensus, Timing};
use amx::relay::{AmendIn, FindingIn, Ms, NewReview, Phase, Role, State, Status, SubmitIn, Verdict, VoteIn};
use amx::tailscale::Identity;

const PR: &str = "https://github.com/rust-lang/rust/pull/1";
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const SHA2: &str = "89abcdef0123456789abcdef0123456789abcdef";

fn who(login: &str) -> Identity {
    Identity {
        login: login.into(),
        display_name: String::new(),
        node: "box".into(),
    }
}

struct Sim {
    s: State,
    now: Ms,
    alive: Vec<String>,
}

impl Sim {
    fn new() -> Sim {
        let mut s = State::new(Timing::default(), Consensus::default(), 50);
        s.web_url = "http://100.64.0.1:7341".into();
        Sim {
            s,
            now: 1_760_000_000_000,
            alive: Vec::new(),
        }
    }

    fn join(&mut self, name: &str, login: &str, role: Role) {
        self.s.touch(name, &who(login), Some(role), self.now).unwrap();
        self.alive.push(name.into());
        self.s.tick(self.now);
    }

    fn leave(&mut self, name: &str) {
        self.alive.retain(|a| a != name);
    }

    /// Advances the clock in small steps; agents in `alive` keep calling the relay.
    fn advance(&mut self, secs: u64) {
        let end = self.now + secs * 1000;
        while self.now < end {
            self.now = (self.now + 10_000).min(end);
            for a in self.alive.clone() {
                let login = self.s.agents.get(&a).unwrap().login.clone();
                self.s.touch(&a, &who(&login), None, self.now).unwrap();
            }
            self.s.tick(self.now);
        }
    }

    fn events(&mut self, name: &str) -> String {
        self.s
            .take_events(name)
            .into_iter()
            .map(|e| e.text)
            .collect::<Vec<_>>()
            .join("\n---\n")
    }

    fn request(&mut self, author: &str, reviewers: usize) -> String {
        let mut req = NewReview::new(PR);
        req.reviewers = Some(reviewers);
        req.note = "Adds a cache".into();
        self.s.request_review(author, req, self.now).unwrap()
    }

    fn accept(&mut self, agent: &str, id: &str) {
        self.s.accept(agent, id, self.now).unwrap();
    }

    fn submit(&mut self, agent: &str, id: &str, findings: &[(&str, &str)]) {
        let findings: Vec<FindingIn> = findings
            .iter()
            .map(|(sev, text)| FindingIn {
                severity: sev.to_string(),
                url: Some("https://github.com/rust-lang/rust/blob/0123456/src/lib.rs#L10".into()),
                text: text.to_string(),
            })
            .collect();
        let verdict = if findings.is_empty() { "approve" } else { "changes_requested" };
        let sub = SubmitIn {
            head_sha: SHA.into(),
            verdict: verdict.into(),
            summary: format!("{agent} looked"),
            findings,
        };
        self.s.submit(agent, id, sub, self.now).unwrap();
    }

    fn vote(&mut self, agent: &str, id: &str, votes: &[(&str, &str)]) {
        let votes = votes
            .iter()
            .map(|(f, v)| {
                let (vote, dup) = match v.strip_prefix("dup:") {
                    Some(t) => ("duplicate", Some(t.to_string())),
                    None => (*v, None),
                };
                VoteIn {
                    finding: f.to_string(),
                    vote: vote.into(),
                    reason: (vote == "disagree").then(|| format!("{agent} thinks {f} is wrong")),
                    duplicate_of: dup,
                }
            })
            .collect();
        self.s.vote(agent, id, votes, self.now).unwrap();
    }

    fn phase(&self, id: &str) -> Phase {
        self.s.review(id).unwrap().phase
    }

    fn status(&self, id: &str, f: &str) -> Status {
        let r = self.s.review(id).unwrap();
        r.finding(f).unwrap().outcome.clone().unwrap().status
    }

    fn verdict(&self, id: &str) -> Verdict {
        self.s.review(id).unwrap().result.as_ref().unwrap().verdict
    }
}

fn setup(reviewers: &[&str]) -> Sim {
    let mut sim = Sim::new();
    sim.join("alice-codex", "alice@example.com", Role::Author);
    for (i, r) in reviewers.iter().enumerate() {
        let login = if i == 0 { "alice@example.com".to_string() } else { format!("friend{i}@example.com") };
        sim.join(r, &login, Role::Reviewer);
    }
    sim
}

#[test]
fn happy_path_two_reviewers() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    assert_eq!(sim.phase(&id), Phase::Offering);
    let offer = sim.events("rev-a");
    assert!(offer.contains("accept_review") && offer.contains(PR), "{offer}");
    assert!(sim.s.next_actions("rev-a", sim.now)[0].urgent);
    sim.accept("rev-a", &id);
    sim.accept("rev-b", &id);
    assert_eq!(sim.phase(&id), Phase::Reviewing);
    let task = sim.events("rev-a");
    assert!(task.contains("gh pr diff") && task.contains("submit_findings"), "{task}");
    assert!(sim.events("alice-codex").contains("Reviewers assigned: rev-a, rev-b"));

    sim.submit("rev-a", &id, &[("major", "cache never invalidated")]);
    sim.submit("rev-b", &id, &[("minor", "missing doc comment")]);
    assert_eq!(sim.phase(&id), Phase::Voting(1));
    let vote_task = sim.events("rev-b");
    assert!(vote_task.contains("F1") && vote_task.contains("cache never invalidated"), "{vote_task}");
    assert_eq!(sim.s.review(&id).unwrap().owed_votes("rev-b").len(), 1);

    sim.vote("rev-a", &id, &[("F2", "agree")]);
    sim.vote("rev-b", &id, &[("F1", "agree")]);
    assert_eq!(sim.phase(&id), Phase::Done);
    assert_eq!(sim.status(&id, "F1"), Status::Confirmed);
    assert_eq!(sim.status(&id, "F2"), Status::Confirmed);
    assert_eq!(sim.verdict(&id), Verdict::ChangesRequested);
    let result = sim.events("alice-codex");
    assert!(result.contains("CHANGES REQUESTED") && result.contains("cache never invalidated"), "{result}");
    assert!(result.contains("request_rereview") && result.contains("http://100.64.0.1:7341/r/r1"));
    assert!(sim.events("rev-a").contains("Review finished"));
    assert!(sim.s.busy_with("rev-a").is_none());
}

#[test]
fn happy_path_three_reviewers_approve() {
    let mut sim = setup(&["rev-a", "rev-b", "rev-c"]);
    let id = sim.request("alice-codex", 3);
    for r in ["rev-a", "rev-b", "rev-c"] {
        sim.accept(r, &id);
    }
    sim.submit("rev-a", &id, &[("nit", "rename x")]);
    sim.submit("rev-b", &id, &[]);
    sim.submit("rev-c", &id, &[("minor", "log level too high")]);
    sim.vote("rev-a", &id, &[("F2", "agree")]);
    sim.vote("rev-b", &id, &[("F1", "agree"), ("F2", "agree")]);
    sim.vote("rev-c", &id, &[("F1", "agree")]);
    assert_eq!(sim.phase(&id), Phase::Done);
    assert_eq!(sim.verdict(&id), Verdict::Approve);
    assert!(sim.events("alice-codex").contains("APPROVE"));
}

#[test]
fn findings_hidden_until_all_submit() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    sim.accept("rev-b", &id);
    sim.submit("rev-a", &id, &[("major", "SECRET-FINDING-TEXT")]);
    let r = sim.s.review(&id).unwrap();
    assert!(!r.revealed());
    let b_view = amx::text::status(r, "rev-b", sim.now, "http://x");
    assert!(!b_view.contains("SECRET-FINDING-TEXT"), "{b_view}");
    assert!(b_view.contains("1 finding(s) from other reviewers hidden"));
    assert!(amx::text::status(r, "rev-a", sim.now, "http://x").contains("SECRET-FINDING-TEXT"));
    assert!(!amx::web::render_review(r, sim.now).contains("SECRET-FINDING-TEXT"));
    assert!(!sim.events("rev-b").contains("SECRET-FINDING-TEXT"));
    assert!(sim.s.vote("rev-b", &id, vec![], sim.now).is_err());
    assert!(sim.s.say("rev-a", &id, "psst", sim.now).is_err(), "no talking before reveal");
    sim.submit("rev-b", &id, &[]);
    assert!(sim.events("rev-b").contains("SECRET-FINDING-TEXT"));
}

#[test]
fn disagreement_then_agreement_in_round_two() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    sim.accept("rev-b", &id);
    sim.submit("rev-a", &id, &[("major", "race on shutdown")]);
    sim.submit("rev-b", &id, &[]);
    sim.vote("rev-b", &id, &[("F1", "disagree")]);
    assert_eq!(sim.phase(&id), Phase::Voting(2));
    let a = sim.events("rev-a");
    assert!(a.contains("rev-b thinks F1 is wrong") && a.contains("amend_finding"), "{a}");
    sim.s.say("rev-a", &id, "Look at line 10: the lock is dropped first.", sim.now).unwrap();
    assert!(sim.events("rev-b").contains("the lock is dropped first"));
    assert!(sim.events("alice-codex").contains("the lock is dropped first"), "author sees the discussion");
    sim.vote("rev-b", &id, &[("F1", "agree")]);
    assert_eq!(sim.phase(&id), Phase::Done);
    assert_eq!(sim.status(&id, "F1"), Status::Confirmed);
    assert_eq!(sim.verdict(&id), Verdict::ChangesRequested);
}

#[test]
fn blocking_needs_unanimity() {
    let mut sim = setup(&["rev-a", "rev-b", "rev-c"]);
    let id = sim.request("alice-codex", 3);
    for r in ["rev-a", "rev-b", "rev-c"] {
        sim.accept(r, &id);
    }
    sim.submit("rev-a", &id, &[("blocking", "SQL injection")]);
    sim.submit("rev-b", &id, &[]);
    sim.submit("rev-c", &id, &[]);
    sim.vote("rev-b", &id, &[("F1", "agree")]);
    sim.vote("rev-c", &id, &[("F1", "disagree")]);
    assert_eq!(sim.phase(&id), Phase::Voting(2));
    sim.vote("rev-b", &id, &[("F1", "agree")]);
    sim.vote("rev-c", &id, &[("F1", "disagree")]);
    let r = sim.s.review(&id).unwrap();
    let o = r.finding("F1").unwrap().outcome.clone().unwrap();
    assert_eq!(o.status, Status::Confirmed);
    assert_eq!(o.severity, amx::relay::Severity::Major);
    assert!(o.downgraded);
    assert_eq!(sim.verdict(&id), Verdict::ChangesRequested);
    assert!(sim.events("alice-codex").contains("downgraded from blocking"));
}

#[test]
fn duplicates_are_merged() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    sim.accept("rev-b", &id);
    sim.submit("rev-a", &id, &[("major", "unbounded cache growth")]);
    sim.submit("rev-b", &id, &[("major", "memory leak in cache")]);
    sim.vote("rev-a", &id, &[("F2", "dup:F1")]);
    sim.vote("rev-b", &id, &[("F1", "agree")]);
    assert_eq!(sim.status(&id, "F1"), Status::Confirmed);
    assert_eq!(sim.status(&id, "F2"), Status::Duplicate("F1".into()));
    let result = sim.events("alice-codex");
    assert!(result.contains("merged duplicate F2"), "{result}");
}

#[test]
fn split_vote_is_disputed_and_majority_rejects() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    sim.accept("rev-b", &id);
    sim.submit("rev-a", &id, &[("major", "should use a BTreeMap")]);
    sim.submit("rev-b", &id, &[]);
    sim.vote("rev-b", &id, &[("F1", "disagree")]);
    sim.vote("rev-b", &id, &[("F1", "disagree")]);
    assert_eq!(sim.status(&id, "F1"), Status::Disputed);
    assert_eq!(sim.verdict(&id), Verdict::Approve, "disputed findings don't block");
    let result = sim.events("alice-codex");
    assert!(result.contains("Disputed") && result.contains("rev-b thinks F1 is wrong"), "{result}");

    let mut sim = setup(&["rev-a", "rev-b", "rev-c"]);
    let id = sim.request("alice-codex", 3);
    for r in ["rev-a", "rev-b", "rev-c"] {
        sim.accept(r, &id);
    }
    sim.submit("rev-a", &id, &[("major", "wrong")]);
    sim.submit("rev-b", &id, &[]);
    sim.submit("rev-c", &id, &[]);
    for _ in 0..2 {
        sim.vote("rev-b", &id, &[("F1", "disagree")]);
        sim.vote("rev-c", &id, &[("F1", "disagree")]);
    }
    assert_eq!(sim.status(&id, "F1"), Status::Rejected);
    assert_eq!(sim.verdict(&id), Verdict::Approve);
}

#[test]
fn amend_and_withdraw() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    sim.accept("rev-b", &id);
    sim.submit("rev-a", &id, &[("blocking", "panics on empty input"), ("nit", "typo")]);
    sim.submit("rev-b", &id, &[]);
    sim.vote("rev-b", &id, &[("F1", "disagree"), ("F2", "disagree")]);
    assert_eq!(sim.phase(&id), Phase::Voting(2));
    let change = AmendIn {
        severity: Some("minor".into()),
        text: Some("returns an error on empty input; message is unclear".into()),
        ..Default::default()
    };
    sim.s.amend("rev-a", &id, "F1", change, sim.now).unwrap();
    sim.s.withdraw("rev-a", &id, "F2", "fair, it's a word", sim.now).unwrap();
    assert!(sim.s.withdraw("rev-b", &id, "F1", "not mine", sim.now).is_err());
    assert_eq!(sim.s.review(&id).unwrap().owed_votes("rev-b").len(), 1);
    sim.vote("rev-b", &id, &[("F1", "agree")]);
    assert_eq!(sim.status(&id, "F1"), Status::Confirmed);
    assert_eq!(sim.status(&id, "F2"), Status::Withdrawn);
    assert_eq!(sim.verdict(&id), Verdict::Approve);
}

#[test]
fn missed_deadline_drops_reviewer() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    sim.accept("rev-b", &id);
    sim.submit("rev-a", &id, &[("major", "off by one")]);
    sim.advance(1201);
    let r = sim.s.review(&id).unwrap();
    assert_eq!(r.slot("rev-b").unwrap().dropped.as_deref(), Some("missed the review deadline"));
    assert_eq!(sim.phase(&id), Phase::Done, "a single remaining reviewer has nobody to vote");
    assert!(sim.events("rev-b").contains("You were dropped"));
    let result = sim.events("alice-codex");
    assert!(result.contains("rev-b (dropped: missed the review deadline)"), "{result}");
    assert!(result.contains("off by one"));
}

#[test]
fn voting_deadline_drops_silent_voter() {
    let mut sim = setup(&["rev-a", "rev-b", "rev-c"]);
    let id = sim.request("alice-codex", 3);
    for r in ["rev-a", "rev-b", "rev-c"] {
        sim.accept(r, &id);
    }
    sim.submit("rev-a", &id, &[("major", "leak")]);
    sim.submit("rev-b", &id, &[]);
    sim.submit("rev-c", &id, &[]);
    sim.vote("rev-b", &id, &[("F1", "agree")]);
    sim.advance(601);
    assert_eq!(sim.phase(&id), Phase::Done);
    let r = sim.s.review(&id).unwrap();
    assert!(r.slot("rev-c").unwrap().dropped.is_some());
    assert_eq!(sim.status(&id, "F1"), Status::Confirmed);
}

#[test]
fn reviewer_going_offline_is_dropped() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    sim.accept("rev-b", &id);
    sim.leave("rev-b");
    sim.advance(200);
    assert!(sim.s.review(&id).unwrap().slot("rev-b").unwrap().active(), "silent while working is fine");
    sim.advance(120);
    let r = sim.s.review(&id).unwrap();
    assert_eq!(r.slot("rev-b").unwrap().dropped.as_deref(), Some("went offline"));
    assert_eq!(r.phase, Phase::Reviewing);
    sim.submit("rev-a", &id, &[]);
    assert_eq!(sim.phase(&id), Phase::Done);
    assert_eq!(sim.verdict(&id), Verdict::Approve);
}

#[test]
fn fewer_reviewers_than_requested() {
    let mut sim = setup(&["rev-a"]);
    let id = sim.request("alice-codex", 3);
    sim.accept("rev-a", &id);
    assert_eq!(sim.phase(&id), Phase::Offering);
    sim.join("rev-late", "friend9@example.com", Role::Reviewer);
    assert!(sim.events("rev-late").contains("Review offer"), "reviewers coming online get the offer");
    sim.s.decline("rev-late", &id, "no access", sim.now).unwrap();
    sim.advance(121);
    assert_eq!(sim.phase(&id), Phase::Reviewing);
    assert!(sim.events("alice-codex").contains("rev-a (of 3 requested)"));
}

#[test]
fn no_reviewers_fails_back_to_author() {
    let mut sim = setup(&[]);
    let id = sim.request("alice-codex", 2);
    sim.advance(121);
    assert_eq!(sim.phase(&id), Phase::Failed);
    assert!(sim.events("alice-codex").contains("no reviewers available"));
}

#[test]
fn offers_respect_roles_and_busy_reviewers() {
    let mut sim = setup(&["rev-a"]);
    sim.join("bob-codex", "friend1@example.com", Role::Author);
    let id = sim.request("alice-codex", 1);
    assert!(sim.s.accept("alice-codex", &id, sim.now).is_err(), "not your own");
    assert!(sim.s.accept("bob-codex", &id, sim.now).is_err(), "authors don't review");
    assert!(sim.events("bob-codex").is_empty());
    sim.accept("rev-a", &id);
    let other = sim.request("bob-codex", 1);
    assert!(sim.events("rev-a").contains("You are assigned"));
    assert!(sim.s.accept("rev-a", &other, sim.now).is_err(), "busy");
    sim.submit("rev-a", &id, &[]);
    sim.s.tick(sim.now);
    assert!(sim.events("rev-a").contains(&format!("[{other}] Review offer")), "offered once free");
}

#[test]
fn rereview_rechecks_previous_findings() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    sim.accept("rev-b", &id);
    sim.submit("rev-a", &id, &[("major", "cache never invalidated")]);
    sim.submit("rev-b", &id, &[("nit", "naming")]);
    sim.vote("rev-a", &id, &[("F2", "disagree")]);
    sim.vote("rev-b", &id, &[("F1", "agree")]);
    sim.vote("rev-a", &id, &[("F2", "disagree")]);
    assert_eq!(sim.status(&id, "F2"), Status::Disputed);
    assert_eq!(sim.verdict(&id), Verdict::ChangesRequested);
    sim.events("alice-codex");
    assert!(sim.s.rereview("rev-a", &id, SHA2, "", sim.now).is_err(), "only the author");

    let id2 = sim.s.rereview("alice-codex", &id, SHA2, "invalidate on write", sim.now).unwrap();
    let r2 = sim.s.review(&id2).unwrap();
    assert_eq!(r2.phase, Phase::Reviewing, "same reviewers reassigned");
    assert_eq!(r2.findings.len(), 2, "confirmed and disputed findings carried");
    assert_eq!(r2.since_sha.as_deref(), Some(SHA));
    let task = sim.events("rev-b");
    assert!(task.contains("re-review") && task.contains("cache never invalidated") && task.contains("compare/"), "{task}");
    sim.submit("rev-a", &id2, &[]);
    sim.submit("rev-b", &id2, &[]);
    sim.vote("rev-a", &id2, &[("C1", "disagree"), ("C2", "disagree")]);
    sim.vote("rev-b", &id2, &[("C1", "disagree"), ("C2", "disagree")]);
    assert_eq!(sim.phase(&id2), Phase::Done);
    assert_eq!(sim.status(&id2, "C1"), Status::Rejected);
    assert_eq!(sim.verdict(&id2), Verdict::Approve);
    let result = sim.events("alice-codex");
    assert!(result.contains("now resolved") && result.contains("APPROVE"), "{result}");
    assert!(sim.s.rereview("alice-codex", &id, SHA2, "", sim.now).is_err(), "one re-review per review");
}

#[test]
fn rereview_with_previous_reviewers_gone_offers_new_ones() {
    let mut sim = setup(&["rev-a"]);
    let id = sim.request("alice-codex", 1);
    sim.accept("rev-a", &id);
    sim.submit("rev-a", &id, &[("major", "bug")]);
    assert_eq!(sim.verdict(&id), Verdict::ChangesRequested);
    sim.leave("rev-a");
    sim.advance(100);
    sim.join("rev-new", "friend5@example.com", Role::Reviewer);
    let id2 = sim.s.rereview("alice-codex", &id, SHA2, "fixed", sim.now).unwrap();
    assert_eq!(sim.phase(&id2), Phase::Offering);
    assert!(sim.events("rev-new").contains("re-review"));
    sim.accept("rev-new", &id2);
    sim.submit("rev-new", &id2, &[]);
    assert_eq!(sim.phase(&id2), Phase::Voting(1), "the carried finding needs a vote even with one reviewer");
    sim.vote("rev-new", &id2, &[("C1", "agree")]);
    assert_eq!(sim.status(&id2, "C1"), Status::Confirmed);
    assert!(sim.events("alice-codex").contains("STILL PRESENT"));
}

#[test]
fn agent_names_are_bound_to_logins() {
    let mut sim = Sim::new();
    sim.join("shared-name", "alice@example.com", Role::Reviewer);
    let err = sim.s.touch("shared-name", &who("mallory@example.com"), None, sim.now).unwrap_err();
    assert!(err.to_string().contains("in use by another tailnet user"), "{err}");
    sim.s.touch("shared-name", &who("alice@example.com"), None, sim.now).unwrap();
    sim.leave("shared-name");
    sim.advance(91);
    sim.s.touch("shared-name", &who("mallory@example.com"), None, sim.now).unwrap();
    assert_eq!(sim.s.agents.get("shared-name").unwrap().login, "mallory@example.com");
}

#[test]
fn input_validation() {
    let mut sim = setup(&["rev-a"]);
    assert!(sim.s.request_review("alice-codex", NewReview::new("https://example.com/x"), sim.now).is_err());
    let id = sim.request("alice-codex", 1);
    assert!(sim.s.request_review("alice-codex", NewReview::new(PR), sim.now).is_err(), "duplicate request");
    sim.accept("rev-a", &id);
    let bad_url = SubmitIn {
        head_sha: SHA.into(),
        verdict: "approve".into(),
        summary: String::new(),
        findings: vec![FindingIn {
            severity: "major".into(),
            url: Some("https://evil.example/x".into()),
            text: "x".into(),
        }],
    };
    assert!(sim.s.submit("rev-a", &id, bad_url, sim.now).is_err());
    let bad_sha = SubmitIn {
        head_sha: "main".into(),
        verdict: "approve".into(),
        summary: String::new(),
        findings: vec![],
    };
    assert!(sim.s.submit("rev-a", &id, bad_sha, sim.now).is_err());
    assert_eq!(sim.phase(&id), Phase::Reviewing);
}
