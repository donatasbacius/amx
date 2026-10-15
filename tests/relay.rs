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
            .take_events(name, true)
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
                    severity: None,
                    reason: (vote == "disagree").then(|| format!("{agent} thinks {f} is wrong")),
                    duplicate_of: dup,
                }
            })
            .collect();
        self.s.vote(agent, id, votes, self.now).unwrap();
    }

    /// Lets the reply window of a later voting round run out.
    fn skip_replies(&mut self) {
        self.advance(Timing::default().reply_secs);
    }

    fn task(&self, name: &str) -> String {
        self.s.next_actions(name, self.now).first().map(|a| a.text.clone()).unwrap_or_default()
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
    let task = sim.task("rev-a");
    assert!(task.contains("gh pr diff") && task.contains("submit_findings") && task.contains("at least 20m00s"), "starts on accept: {task}");
    sim.accept("rev-b", &id);
    assert_eq!(sim.phase(&id), Phase::Reviewing);
    assert_eq!(sim.events("rev-a"), "", "the task came with the accept");
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
    sim.skip_replies();
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
    sim.skip_replies();
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
    for round in 1..=2 {
        if round == 2 {
            sim.skip_replies();
        }
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
    let stale = VoteIn {
        finding: "F2".into(),
        vote: "agree".into(),
        severity: None,
        reason: None,
        duplicate_of: None,
    };
    let voted = sim.s.vote("rev-b", &id, vec![stale], sim.now).unwrap();
    assert_eq!((voted.recorded, voted.ignored), (0, vec!["F2".to_string()]), "votes on withdrawn findings are ignored");
    assert!(sim.s.withdraw("rev-b", &id, "F1", "not mine", sim.now).is_err());
    assert_eq!(sim.s.review(&id).unwrap().owed_votes("rev-b").len(), 1);
    sim.vote("rev-b", &id, &[("F2", "agree"), ("F1", "agree")]);
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
    assert!(sim.task("rev-a").contains("submit_findings"));
    assert!(sim.s.accept("rev-a", &other, sim.now).is_err(), "busy");
    sim.submit("rev-a", &id, &[]);
    sim.s.tick(sim.now);
    assert!(sim.events("rev-a").contains(&format!("[{other}] Review offer")), "offered once free");
}

fn pick(names: &[&str], count: Option<usize>) -> NewReview {
    let mut req = NewReview::new(PR);
    req.picked = names.iter().map(|n| n.to_string()).collect();
    req.reviewers = count;
    req
}

#[test]
fn authors_can_pick_reviewers() {
    let mut sim = setup(&["rev-a", "rev-b", "rev-c"]);
    let id = sim.s.request_review("alice-codex", pick(&["rev-b", "rev-c", "rev-b"], None), sim.now).unwrap();
    let r = sim.s.review(&id).unwrap();
    assert_eq!(r.picked, ["rev-b", "rev-c"]);
    assert_eq!(r.params.reviewers, 2, "count defaults to the names given");
    assert!(sim.events("rev-a").is_empty(), "not offered to others");
    assert!(sim.events("rev-b").contains("Review offer"));
    let err = sim.s.accept("rev-a", &id, sim.now).unwrap_err();
    assert!(err.to_string().contains("only to the reviewers its author picked"), "{err}");
    sim.accept("rev-b", &id);
    sim.accept("rev-c", &id);
    assert_eq!(sim.phase(&id), Phase::Reviewing);
}

#[test]
fn picked_reviewers_must_be_online_reviewers() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    sim.join("bob-codex", "friend1@example.com", Role::Author);
    sim.join("rev-gone", "friend2@example.com", Role::Reviewer);
    sim.leave("rev-gone");
    sim.advance(100);
    let err = sim
        .s
        .request_review("alice-codex", pick(&["rev-a", "bob-codex", "rev-gone", "nobody", "alice-codex"], None), sim.now)
        .unwrap_err()
        .to_string();
    for want in ["bob-codex (not a reviewer)", "rev-gone (not online)", "nobody (unknown agent)", "alice-codex (that's you)"] {
        assert!(err.contains(want), "{err}");
    }
    assert!(!err.contains("rev-a"), "{err}");
    let err = sim.s.request_review("alice-codex", pick(&["rev-a"], Some(2)), sim.now).unwrap_err();
    assert!(err.to_string().contains("asked for 2 reviewers but named only 1"), "{err}");
    assert!(sim.s.reviews.is_empty());

    let id = sim.s.request_review("alice-codex", pick(&["rev-a", "rev-b"], Some(1)), sim.now).unwrap();
    sim.accept("rev-b", &id);
    assert_eq!(sim.phase(&id), Phase::Reviewing, "first of the named to accept");
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
    sim.skip_replies();
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
    assert!(sim.s.check_name("shared-name", &who("mallory@example.com"), sim.now).is_err());
    assert!(sim.s.check_name("new-name", &who("mallory@example.com"), sim.now).is_ok());
    assert!(sim.s.agents.get("new-name").is_none(), "checking a name doesn't register it");
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

fn rate(finding: &str, severity: &str) -> VoteIn {
    VoteIn {
        finding: finding.into(),
        vote: "agree".into(),
        severity: Some(severity.into()),
        reason: Some("real, but overstated".into()),
        duplicate_of: None,
    }
}

#[test]
fn overstated_finding_is_confirmed_at_the_severity_its_supporters_back() {
    let mut sim = setup(&["rev-a", "rev-b", "rev-c"]);
    let id = sim.request("alice-codex", 3);
    for r in ["rev-a", "rev-b", "rev-c"] {
        sim.accept(r, &id);
    }
    sim.submit("rev-a", &id, &[("blocking", "no benchmark guards the speedup")]);
    sim.submit("rev-b", &id, &[]);
    sim.submit("rev-c", &id, &[]);
    let mut no = rate("F1", "minor");
    no.vote = "disagree".into();
    let err = sim.s.vote("rev-b", &id, vec![no], sim.now).unwrap_err();
    assert!(err.to_string().contains("severity goes with agree"), "{err}");
    sim.s.vote("rev-b", &id, vec![rate("F1", "minor")], sim.now).unwrap();
    sim.s.vote("rev-c", &id, vec![rate("F1", "minor")], sim.now).unwrap();
    assert_eq!(sim.phase(&id), Phase::Voting(2), "a severity dispute gets a second round");
    assert!(sim.events("rev-a").contains("agree, as minor"));
    sim.skip_replies();
    sim.s.vote("rev-b", &id, vec![rate("F1", "minor")], sim.now).unwrap();
    sim.s.vote("rev-c", &id, vec![rate("F1", "nit")], sim.now).unwrap();
    let r = sim.s.review(&id).unwrap();
    let o = r.finding("F1").unwrap().outcome.clone().unwrap();
    assert_eq!((o.status, o.severity, o.downgraded), (Status::Confirmed, amx::relay::Severity::Minor, false));
    assert_eq!(sim.verdict(&id), Verdict::Approve);
    let result = sim.events("alice-codex");
    assert!(result.contains("Confirmed findings") && result.contains("raised as blocking; the votes put it at minor") && result.contains("rev-c: agree, as nit"), "{result}");
}

#[test]
fn blocking_backed_by_a_majority_but_rated_lower_by_one_becomes_major() {
    let mut sim = setup(&["rev-a", "rev-b", "rev-c"]);
    let id = sim.request("alice-codex", 3);
    for r in ["rev-a", "rev-b", "rev-c"] {
        sim.accept(r, &id);
    }
    sim.submit("rev-a", &id, &[("blocking", "regex is quadratic again")]);
    sim.submit("rev-b", &id, &[]);
    sim.submit("rev-c", &id, &[]);
    for round in 1..=2 {
        if round == 2 {
            sim.skip_replies();
        }
        sim.vote("rev-b", &id, &[("F1", "agree")]);
        sim.s.vote("rev-c", &id, vec![rate("F1", "major")], sim.now).unwrap();
    }
    let o = sim.s.review(&id).unwrap().finding("F1").unwrap().outcome.clone().unwrap();
    assert_eq!((o.severity, o.downgraded), (amx::relay::Severity::Major, true));
    assert_eq!(sim.verdict(&id), Verdict::ChangesRequested);
}

#[test]
fn two_reviewers_split_on_severity_confirm_the_lower() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    sim.accept("rev-b", &id);
    sim.submit("rev-a", &id, &[("blocking", "cache key ignores the locale")]);
    sim.submit("rev-b", &id, &[]);
    sim.s.vote("rev-b", &id, vec![rate("F1", "major")], sim.now).unwrap();
    sim.skip_replies();
    sim.s.vote("rev-b", &id, vec![rate("F1", "major")], sim.now).unwrap();
    assert_eq!(sim.phase(&id), Phase::Done);
    let o = sim.s.review(&id).unwrap().finding("F1").unwrap().outcome.clone().unwrap();
    assert_eq!((o.status, o.severity, o.downgraded, o.severity_split), (Status::Confirmed, amx::relay::Severity::Major, false, true));
    assert_eq!(sim.verdict(&id), Verdict::ChangesRequested, "both agree it is a must-fix");
    let result = sim.events("alice-codex");
    assert!(result.contains("- F1 [major, split: blocking/major (rev-a: blocking, rev-b: major)]"), "{result}");
    assert!(result.contains("rev-b: agree, as major — real, but overstated") && !result.contains("Disputed"), "{result}");
    let page = amx::web::render_review(sim.s.review(&id).unwrap(), sim.now);
    assert!(page.contains("Raised as blocking; rev-b rated it major, so the lower (major) stands."), "{page}");
}

#[test]
fn review_page_puts_confirmed_findings_first_and_closes_the_rest() {
    let mut sim = setup(&["rev-a", "rev-b", "rev-c"]);
    let id = sim.request("alice-codex", 3);
    for r in ["rev-a", "rev-b", "rev-c"] {
        sim.accept(r, &id);
    }
    sim.submit("rev-a", &id, &[("nit", "should use a BTreeMap"), ("blocking", "Lock held across await\n\nThe guard lives until the end of `drain()`.")]);
    sim.submit("rev-b", &id, &[]);
    sim.submit("rev-c", &id, &[]);
    for r in ["rev-b", "rev-c"] {
        sim.vote(r, &id, &[("F1", "disagree"), ("F2", "agree")]);
    }
    sim.skip_replies();
    for r in ["rev-b", "rev-c"] {
        sim.vote(r, &id, &[("F1", "disagree")]);
    }
    assert_eq!(sim.verdict(&id), Verdict::ChangesRequested);
    let page = amx::web::render_review(sim.s.review(&id).unwrap(), sim.now);
    let (f1, f2, closed) = (page.find("id=\"F1\"").unwrap(), page.find("id=\"F2\"").unwrap(), page.find(">Closed <").unwrap());
    assert!(f2 < closed && closed < f1, "{page}");
    assert!(page.contains("Changes requested") && page.contains("1 confirmed (1 blocking) · 1 rejected"), "{page}");
    assert!(page.contains("<span class=\"ft\">Lock held across await</span>") && !page.contains("<p>Lock held across await</p>"), "the headline is not repeated: {page}");
    assert!(page.contains("<span class=\"ft\">should use a BTreeMap</span>") && !page.contains("<p>should use a BTreeMap</p>"), "{page}");
    assert!(page.contains("rev-c</b> agrees") && !page.contains("agrees, as blocking"), "{page}");
    assert!(!page.contains("summary-table") && !page.contains("local"), "{page}");
}

#[test]
fn two_reviewers_agreeing_on_severity_confirm_it() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    sim.accept("rev-b", &id);
    sim.submit("rev-a", &id, &[("blocking", "cache key ignores the locale")]);
    sim.submit("rev-b", &id, &[]);
    sim.s.vote("rev-b", &id, vec![rate("F1", "blocking")], sim.now).unwrap();
    assert_eq!(sim.phase(&id), Phase::Done);
    let o = sim.s.review(&id).unwrap().finding("F1").unwrap().outcome.clone().unwrap();
    assert_eq!((o.status, o.severity, o.downgraded, o.severity_split), (Status::Confirmed, amx::relay::Severity::Blocking, false, false));
    assert_eq!(sim.verdict(&id), Verdict::ChangesRequested);
}

#[test]
fn three_or_more_reviewers_split_on_severity_still_resolve_by_majority() {
    let mut sim = setup(&["rev-a", "rev-b", "rev-c"]);
    let id = sim.request("alice-codex", 3);
    for r in ["rev-a", "rev-b", "rev-c"] {
        sim.accept(r, &id);
    }
    sim.submit("rev-a", &id, &[("blocking", "cache key ignores the locale")]);
    sim.submit("rev-b", &id, &[]);
    sim.submit("rev-c", &id, &[]);
    for round in 1..=2 {
        if round == 2 {
            sim.skip_replies();
        }
        sim.s.vote("rev-b", &id, vec![rate("F1", "nit")], sim.now).unwrap();
        sim.s.vote("rev-c", &id, vec![rate("F1", "minor")], sim.now).unwrap();
    }
    let o = sim.s.review(&id).unwrap().finding("F1").unwrap().outcome.clone().unwrap();
    assert_eq!((o.status, o.severity, o.severity_split), (Status::Confirmed, amx::relay::Severity::Minor, false));

    let mut sim = setup(&["rev-a", "rev-b", "rev-c", "rev-d"]);
    let id = sim.request("alice-codex", 4);
    for r in ["rev-a", "rev-b", "rev-c", "rev-d"] {
        sim.accept(r, &id);
    }
    sim.submit("rev-a", &id, &[("blocking", "cache key ignores the locale")]);
    for r in ["rev-b", "rev-c", "rev-d"] {
        sim.submit(r, &id, &[]);
    }
    for round in 1..=2 {
        if round == 2 {
            sim.skip_replies();
        }
        sim.vote("rev-b", &id, &[("F1", "agree")]);
        sim.s.vote("rev-c", &id, vec![rate("F1", "nit")], sim.now).unwrap();
        sim.s.vote("rev-d", &id, vec![rate("F1", "nit")], sim.now).unwrap();
    }
    let o = sim.s.review(&id).unwrap().finding("F1").unwrap().outcome.clone().unwrap();
    assert_eq!((o.status, o.severity, o.severity_split), (Status::Confirmed, amx::relay::Severity::Nit, false), "a tie takes the lower severity");
    assert_eq!(sim.verdict(&id), Verdict::Approve);
}

#[test]
fn closing_remarks_after_the_result_do_not_change_it() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    sim.accept("rev-b", &id);
    sim.submit("rev-a", &id, &[("major", "off by one")]);
    sim.submit("rev-b", &id, &[]);
    sim.vote("rev-b", &id, &[("F1", "agree")]);
    assert_eq!(sim.phase(&id), Phase::Done);
    assert!(sim.events("alice-codex").contains("closing remark"));
    sim.s.say("alice-codex", &id, "Thanks, fixing the off-by-one now.", sim.now).unwrap();
    sim.s.say("rev-a", &id, "You're welcome.", sim.now).unwrap();
    assert!(sim.events("rev-b").contains("fixing the off-by-one"));
    assert!(sim.s.say("rev-c", &id, "drive-by", sim.now).is_err(), "outsiders still can't talk");
    let r = sim.s.review(&id).unwrap();
    assert_eq!(r.phase, Phase::Done);
    assert_eq!(sim.verdict(&id), Verdict::ChangesRequested);
    assert!(amx::web::render_review(r, sim.now).contains("fixing the off-by-one"));
    assert!(sim.s.next_actions("alice-codex", sim.now).is_empty() && sim.s.busy_with("rev-a").is_none());
}

fn reload(s: &State, saved: Ms, now: Ms) -> State {
    let json = serde_json::to_string(&s.snapshot(saved)).unwrap();
    let mut fresh = State::new(Timing::default(), Consensus::default(), 50);
    fresh.web_url = s.web_url.clone();
    fresh.restore(serde_json::from_str(&json).unwrap(), now);
    fresh
}

#[test]
fn state_survives_a_snapshot_and_the_review_carries_on() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let done = sim.request("alice-codex", 2);
    sim.accept("rev-a", &done);
    sim.accept("rev-b", &done);
    sim.submit("rev-a", &done, &[("major", "cache never invalidated")]);
    sim.submit("rev-b", &done, &[]);
    sim.vote("rev-b", &done, &[("F1", "agree")]);
    assert_eq!(sim.phase(&done), Phase::Done);
    sim.s.say("alice-codex", &done, "Thanks", sim.now).unwrap();
    for a in ["alice-codex", "rev-a", "rev-b"] {
        sim.events(a);
    }

    let mut req = NewReview::new("https://github.com/rust-lang/rust/pull/2");
    req.reviewers = Some(2);
    let open = sim.s.request_review("alice-codex", req, sim.now).unwrap();
    sim.accept("rev-a", &open);
    sim.accept("rev-b", &open);
    sim.submit("rev-a", &open, &[("minor", "missing doc comment")]);
    sim.events("rev-a");
    assert!(!sim.s.peek_events("alice-codex", 0).0.is_empty());

    let mut s = reload(&sim.s, sim.now, sim.now);
    for id in [&done, &open] {
        assert_eq!(format!("{:?}", s.review(id).unwrap()), format!("{:?}", sim.s.review(id).unwrap()));
    }
    assert_eq!(s.reviews.len(), 2);
    for a in sim.s.agents.all() {
        let b = s.agents.get(&a.name).unwrap();
        assert_eq!((&b.login, b.role, b.last_seen), (&a.login, a.role, a.last_seen));
        assert_eq!(s.peek_events(&a.name, 0), sim.s.peek_events(&a.name, 0));
        assert_eq!(s.has_undelivered(&a.name), sim.s.has_undelivered(&a.name));
    }
    assert_eq!(s.agents.all().count(), sim.s.agents.all().count());
    assert!(s.touch("rev-a", &who("mallory@example.com"), None, sim.now).is_err(), "name stays bound to its login");
    let next = s.request_review("rev-a", NewReview::new("https://github.com/rust-lang/rust/pull/3"), sim.now).unwrap();
    assert_eq!(next, "r3");

    let later = sim.now + 3_600_000;
    let shifted = reload(&sim.s, sim.now, later);
    let (before, after) = (sim.s.review(&open).unwrap(), shifted.review(&open).unwrap());
    assert_eq!(after.deadline, before.deadline + 3_600_000, "time left is kept across the outage");
    assert_eq!(shifted.review(&done).unwrap().deadline, sim.s.review(&done).unwrap().deadline);
    assert!(shifted.agents.online("rev-b", later));

    sim.s = reload(&sim.s, sim.now, sim.now);
    sim.submit("rev-b", &open, &[]);
    assert_eq!(sim.phase(&open), Phase::Voting(1));
    sim.vote("rev-b", &open, &[("F1", "agree")]);
    assert_eq!(sim.phase(&open), Phase::Done);
    assert_eq!(sim.verdict(&open), Verdict::Approve);
    let result = sim.events("alice-codex");
    assert!(result.contains("Reviewers assigned") && result.contains("APPROVE"), "{result}");
}

#[test]
fn reviewers_lost_in_a_restart_are_dropped_after_the_usual_silence() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    sim.accept("rev-b", &id);
    sim.submit("rev-a", &id, &[]);
    let now = sim.now + 86_400_000;
    sim.s = reload(&sim.s, sim.now, now);
    sim.now = now;
    sim.leave("rev-b");
    sim.advance(290);
    assert!(sim.s.review(&id).unwrap().slot("rev-b").unwrap().active());
    sim.advance(20);
    let r = sim.s.review(&id).unwrap();
    assert!(r.slot("rev-b").unwrap().dropped.is_some());
    assert_eq!(r.phase, Phase::Done);
}

#[test]
fn reviewers_start_on_accept() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    assert_eq!(sim.phase(&id), Phase::Offering);
    sim.submit("rev-a", &id, &[("minor", "typo in the error message")]);
    assert!(sim.task("rev-a").contains("your findings are in"), "{}", sim.task("rev-a"));
    sim.accept("rev-b", &id);
    assert_eq!(sim.phase(&id), Phase::Reviewing);
    assert_eq!(sim.s.review(&id).unwrap().finding("F1").unwrap().by.as_deref(), Some("rev-a"));
    sim.submit("rev-b", &id, &[]);
    assert_eq!(sim.phase(&id), Phase::Voting(1));
}

#[test]
fn done_tasks_and_progress_are_not_delivered() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    sim.accept("rev-b", &id);
    assert!(sim.s.take_events("rev-a", false).is_empty(), "the offer was answered");
    assert!(!sim.s.has_undelivered("rev-b"));
    sim.submit("rev-a", &id, &[("major", "cache never invalidated")]);
    sim.submit("rev-b", &id, &[("minor", "missing doc comment")]);
    assert!(sim.s.has_undelivered("rev-b"));
    let task = sim.task("rev-b");
    let next = task.lines().find(|l| l.starts_with("NEXT:")).unwrap();
    assert!(next.contains("finding: \"F1\"") && !next.contains("\"F2\""), "never suggests a vote on your own finding: {next}");
    assert_eq!(task.matches("cache never invalidated").count(), 1, "{task}");
    sim.vote("rev-b", &id, &[("F1", "agree")]);
    assert!(sim.s.take_events("rev-b", false).is_empty(), "the vote task is done");
    sim.vote("rev-a", &id, &[("F2", "agree")]);
    assert_eq!(sim.phase(&id), Phase::Done);
    let author: Vec<String> = sim.s.take_events("alice-codex", false).into_iter().map(|e| e.text).collect();
    assert_eq!(author.len(), 1, "authors wake for the result only: {author:?}");
    assert!(author[0].contains("Review result"));
    assert!(sim.s.take_events("rev-a", false).is_empty(), "\"Review finished\" is progress");
}

#[test]
fn finding_authors_reply_before_round_two_votes() {
    let mut sim = setup(&["rev-a", "rev-b", "rev-c"]);
    let id = sim.request("alice-codex", 3);
    for r in ["rev-a", "rev-b", "rev-c"] {
        sim.accept(r, &id);
    }
    sim.submit("rev-a", &id, &[("major", "race on shutdown")]);
    sim.submit("rev-b", &id, &[("major", "leaks a file handle")]);
    sim.submit("rev-c", &id, &[]);
    sim.vote("rev-a", &id, &[("F2", "agree")]);
    sim.vote("rev-b", &id, &[("F1", "disagree")]);
    sim.vote("rev-c", &id, &[("F1", "agree"), ("F2", "agree")]);
    assert_eq!(sim.phase(&id), Phase::Voting(2));
    let r = sim.s.review(&id).unwrap();
    assert!(r.replying && r.deadline_left(sim.now) == Timing::default().reply_secs * 1000);
    let a = sim.events("rev-a");
    assert!(a.contains("your findings F1 drew objections") && a.contains("rev-b thinks F1 is wrong") && !a.contains("leaks a file handle"), "{a}");
    assert!(sim.events("rev-c").is_empty(), "voters wait for the reply");
    let err = sim.s.vote("rev-c", &id, vec![VoteIn { finding: "F1".into(), vote: "agree".into(), severity: None, reason: None, duplicate_of: None }], sim.now).unwrap_err();
    assert!(err.to_string().contains("once the authors of contested findings have replied"), "{err}");
    sim.advance(30);
    assert_eq!(sim.phase(&id), Phase::Voting(2));
    sim.s.say("rev-a", &id, "Line 10 drops the lock before joining.", sim.now).unwrap();
    assert!(!sim.s.review(&id).unwrap().replying, "votes open once every author has replied");
    let c = sim.events("rev-c");
    assert!(c.contains("Line 10 drops the lock") && c.contains("You must vote on: F1."), "{c}");
    sim.vote("rev-b", &id, &[("F1", "agree")]);
    sim.vote("rev-c", &id, &[("F1", "agree")]);
    assert_eq!(sim.phase(&id), Phase::Done);
    assert_eq!(sim.status(&id, "F1"), Status::Confirmed);
}

#[test]
fn reply_window_closes_on_its_own() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    sim.accept("rev-b", &id);
    sim.submit("rev-a", &id, &[("major", "race on shutdown")]);
    sim.submit("rev-b", &id, &[]);
    sim.vote("rev-b", &id, &[("F1", "disagree")]);
    sim.advance(Timing::default().reply_secs - 10);
    assert!(sim.s.review(&id).unwrap().replying);
    sim.advance(10);
    assert!(!sim.s.review(&id).unwrap().replying);
    assert!(sim.events("rev-b").contains("Cross-examination round 2"));
}

#[test]
fn snapshots_from_0_4_still_load() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    let mut v = serde_json::to_value(sim.s.snapshot(sim.now)).unwrap();
    for r in v["reviews"].as_array_mut().unwrap() {
        r["params"].as_object_mut().unwrap().remove("reply_secs");
    }
    for a in v["agents"].as_array_mut().unwrap() {
        for e in a["events"].as_array_mut().unwrap() {
            e.as_object_mut().unwrap().remove("task");
        }
    }
    let text = v.to_string();
    assert!(!text.contains("reply_secs") && !text.contains("\"task\"") && !text.contains("\"progress\"") && !text.contains("replying"), "{text}");
    let mut s = State::new(Timing::default(), Consensus::default(), 50);
    s.restore(serde_json::from_str(&text).unwrap(), sim.now);
    assert_eq!(s.review(&id).unwrap().params.reply_secs, 90);
    assert!(s.next_actions("rev-b", sim.now)[0].text.contains("accept_review"));
}

#[test]
fn a_new_session_drops_events_about_reviews_it_is_no_longer_in() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    sim.accept("rev-b", &id);
    sim.submit("rev-a", &id, &[("major", "off by one")]);
    sim.submit("rev-b", &id, &[]);
    sim.vote("rev-b", &id, &[("F1", "agree")]);
    assert_eq!(sim.phase(&id), Phase::Done);
    sim.s.say("rev-a", &id, "Glad to help.", sim.now).unwrap();
    assert!(sim.s.has_undelivered("alice-codex"), "the result is owed to the author");
    sim.s.new_session("alice-codex");
    let author = sim.events("alice-codex");
    assert!(author.contains("Review result") && !author.contains("Glad to help"), "{author}");

    sim.s.say("alice-codex", &id, "Thanks, fixing it.", sim.now).unwrap();
    assert!(!sim.s.has_undelivered("rev-a"), "closing remarks don't keep a reviewer from stopping");
    let id2 = sim.s.rereview("alice-codex", &id, SHA2, "fixed", sim.now).unwrap();
    assert!(sim.s.has_undelivered("rev-a"));
    sim.s = reload(&sim.s, sim.now, sim.now);
    sim.s.new_session("rev-a");
    let a = sim.events("rev-a");
    assert!(a.contains(&format!("[{id2}] You are assigned again")) && !a.contains("fixing it"), "a reviewer that reconnects keeps its task: {a}");
    sim.s.new_session("rev-b");
    let b = sim.events("rev-b");
    assert!(b.contains("assigned again") && !b.contains("fixing it") && !b.contains("Review finished"), "{b}");
}

#[test]
fn snapshots_from_0_5_still_load() {
    let mut sim = setup(&["rev-a"]);
    let id = sim.request("alice-codex", 1);
    sim.accept("rev-a", &id);
    sim.submit("rev-a", &id, &[]);
    assert_eq!(sim.phase(&id), Phase::Done);
    let mut v = serde_json::to_value(sim.s.snapshot(sim.now)).unwrap();
    assert!(v.to_string().contains("\"result\":true"));
    for a in v["agents"].as_array_mut().unwrap() {
        for e in a["events"].as_array_mut().unwrap() {
            e.as_object_mut().unwrap().remove("result");
        }
    }
    let mut s = State::new(Timing::default(), Consensus::default(), 50);
    s.restore(serde_json::from_value(v).unwrap(), sim.now);
    assert!(s.take_events("alice-codex", false)[0].text.contains("Review result"));
}

#[test]
fn transcript_shows_the_severity_an_agree_vote_backs() {
    let mut sim = setup(&["rev-a", "rev-b"]);
    let id = sim.request("alice-codex", 2);
    sim.accept("rev-a", &id);
    sim.accept("rev-b", &id);
    sim.submit("rev-a", &id, &[("minor", "bidi characters pass through")]);
    sim.submit("rev-b", &id, &[]);
    sim.s.vote("rev-b", &id, vec![rate("F1", "minor")], sim.now).unwrap();
    assert_eq!(sim.phase(&id), Phase::Done, "rating it as raised is plain agreement");
    let r = sim.s.review(&id).unwrap();
    assert!(r.finding("F1").unwrap().votes["rev-b"].severity.is_none());
    assert!(r.log.iter().any(|e| e.text == "F1: agree (minor) — real, but overstated"), "{:?}", r.log);
    assert!(amx::web::render_review(r, sim.now).contains("agree (minor)"));
}
