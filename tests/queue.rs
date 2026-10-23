//! The review queue driven by random actors on a synthetic clock, checking the state machine
//! model's safety properties on the relay's real state after every step.

#[path = "common/props.rs"]
mod props;

use amx::config::{Consensus, Timing};
use amx::relay::{FindingIn, Model, Ms, NewReview, Phase, Role, State, Status, SubmitIn, VoteIn};
use amx::tailscale::Identity;
use std::collections::{BTreeMap, BTreeSet};

const SHAS: [&str; 2] = ["0123456789abcdef0123456789abcdef01234567", "89abcdef0123456789abcdef0123456789abcdef"];
const AUTHORS: [(&str, &str); 2] = [("al-author", "al@example.com"), ("bo-author", "bo@example.com")];
/// Reviewers sharing a login with each other and with an author.
const REVIEWERS: [(&str, &str, Model); 6] = [
    ("al-claude", "al@example.com", Model::Claude),
    ("al-codex", "al@example.com", Model::Codex),
    ("cy-claude", "cy@example.com", Model::Claude),
    ("cy-codex", "cy@example.com", Model::Codex),
    ("dee-codex", "dee@example.com", Model::Codex),
    ("eve-claude", "eve@example.com", Model::Claude),
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<'a, T>(&mut self, v: &'a [T]) -> Option<&'a T> {
        (!v.is_empty()).then(|| &v[self.below(v.len())])
    }
}

fn who(login: &str) -> Identity {
    Identity {
        login: login.into(),
        display_name: String::new(),
        node: "box".into(),
        user_id: 0,
    }
}

struct World {
    seed: u64,
    s: State,
    now: Ms,
    rng: Rng,
    alive: BTreeSet<&'static str>,
    log: Vec<String>,
    churn: props::Churn,
}

impl World {
    fn new(seed: u64) -> World {
        let consensus = Consensus { reviewers: 2, queue_secs: 900, ..Consensus::default() };
        let mut w = World {
            seed,
            s: State::new(Timing::default(), consensus, 50),
            now: 1_760_000_000_000,
            rng: Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
            alive: BTreeSet::new(),
            log: Vec::new(),
            churn: props::Churn::default(),
        };
        for (name, login) in AUTHORS {
            w.s.touch(name, &who(login), Some(Role::Author), w.now).unwrap();
            w.alive.insert(name);
        }
        for (name, login, model) in REVIEWERS {
            w.s.touch(name, &who(login), Some(Role::Reviewer), w.now).unwrap();
            w.s.set_model(name, model);
            if w.rng.chance(70) {
                w.alive.insert(name);
            }
        }
        w.s.tick(w.now);
        w
    }

    fn login(name: &str) -> &'static str {
        AUTHORS.iter().map(|(n, l)| (*n, *l)).chain(REVIEWERS.iter().map(|(n, l, _)| (*n, *l))).find(|(n, _)| *n == name).unwrap().1
    }

    fn touch_alive(&mut self) {
        for a in self.alive.clone() {
            self.s.touch(a, &who(Self::login(a)), None, self.now).unwrap();
        }
    }

    /// One random action, then the relay's tick, checking the properties across it.
    fn step(&mut self) {
        let pre = self.s.clone();
        let roll = self.rng.below(100);
        let what = match roll {
            0..=14 => self.request(),
            15..=17 => self.rereview(),
            18..=64 => self.reviewer_acts(),
            65..=74 => {
                let (name, ..) = REVIEWERS[self.rng.below(REVIEWERS.len())];
                if !self.alive.remove(name) {
                    self.alive.insert(name);
                    self.s.touch(name, &who(Self::login(name)), None, self.now).unwrap();
                }
                format!("{name} online: {}", self.alive.contains(name))
            }
            75..=76 => {
                let live: Vec<String> = self.s.reviews.iter().filter(|r| r.phase.active()).map(|r| r.id.clone()).collect();
                match self.rng.pick(&live).cloned() {
                    Some(id) => format!("cancel {id}: {:?}", self.s.cancel(&id, "a human", self.now).is_ok()),
                    None => "cancel nothing".into(),
                }
            }
            77..=78 => self.rule(),
            79 => {
                let json = serde_json::to_string(&self.s.snapshot(self.now)).unwrap();
                let mut fresh = State::new(self.s.timing, self.s.consensus, self.s.history);
                fresh.restore(serde_json::from_str(&json).unwrap(), self.now);
                self.s = fresh;
                "restart".into()
            }
            _ => {
                let secs = if self.rng.chance(10) { 600 } else { 1 + self.rng.below(120) as u64 };
                return self.advance(secs);
            }
        };
        self.log.push(what);
        self.s.tick(self.now);
        self.check(&pre);
    }

    fn advance(&mut self, secs: u64) {
        self.log.push(format!("advance {secs}s"));
        let end = self.now + secs * 1000;
        while self.now < end {
            let pre = self.s.clone();
            self.now = (self.now + 10_000).min(end);
            self.touch_alive();
            self.s.tick(self.now);
            self.check(&pre);
        }
    }

    fn request(&mut self) -> String {
        let (author, _) = AUTHORS[self.rng.below(2)];
        let mut req = NewReview::new(&format!("https://github.com/example-org/app/pull/{}", 1 + self.rng.below(4)));
        req.title = Some("Add a cache".into());
        req.reviewers = Some([2, 2, 2, 3][self.rng.below(4)]);
        req.head_sha = self.rng.chance(70).then(|| SHAS[self.rng.below(2)].to_string());
        if self.rng.chance(15) {
            let names: Vec<String> = REVIEWERS.iter().filter(|_| self.rng.chance(50)).map(|(n, ..)| n.to_string()).collect();
            req.reviewers = None;
            req.picked = names;
        }
        let what = format!("{author} requests {} x{:?} picked {:?}", req.pr_url, req.reviewers, req.picked);
        let res = self.s.request_review(author, req, self.now);
        format!("{what}: {res:?}")
    }

    fn rereview(&mut self) -> String {
        let done: Vec<(String, String)> = self.s.reviews.iter().filter(|r| r.phase == Phase::Done).map(|r| (r.author.clone(), r.id.clone())).collect();
        let Some((author, id)) = self.rng.pick(&done).cloned() else { return "no re-review".into() };
        let sha = SHAS[self.rng.below(2)];
        let res = self.s.rereview(&author, &id, sha, "fixed", None, self.now);
        format!("{author} re-reviews {id}: {res:?}")
    }

    /// A human rules on, or undoes the ruling on, an open finding of a finished review.
    fn rule(&mut self) -> String {
        let open: Vec<(String, String)> = self
            .s
            .reviews
            .iter()
            .filter(|r| r.phase == Phase::Done)
            .flat_map(|r| r.findings.iter().filter(|f| f.outcome.as_ref().is_some_and(|o| matches!(o.status, Status::Disputed | Status::Unverified))).map(|f| (r.id.clone(), f.id.clone())))
            .collect();
        let Some((id, fid)) = self.rng.pick(&open).cloned() else { return "no ruling".into() };
        let must_fix = [Some(true), Some(false), None][self.rng.below(3)];
        format!("rule {id}/{fid} {must_fix:?}: {:?}", self.s.rule(&id, &fid, "a human", must_fix, self.now))
    }

    fn reviewer_acts(&mut self) -> String {
        let alive: Vec<&str> = REVIEWERS.iter().map(|(n, ..)| *n).filter(|n| self.alive.contains(n)).collect();
        let Some(&name) = self.rng.pick(&alive) else { return "nobody acts".into() };
        self.s.touch(name, &who(Self::login(name)), None, self.now).unwrap();
        if self.rng.chance(5) {
            let any: Vec<String> = self.s.reviews.iter().filter(|r| r.phase == Phase::Offering).map(|r| r.id.clone()).collect();
            if let Some(id) = self.rng.pick(&any).cloned() {
                return format!("{name} accepts {id} unasked: {:?}", self.s.accept(name, &id, self.now));
            }
        }
        let Some(action) = self.s.next_actions(name, self.now).into_iter().find(|a| a.task.is_some()) else {
            self.s.set_waiting(name, 1, self.now);
            self.s.set_waiting(name, -1, self.now);
            return format!("{name} waits");
        };
        let (id, task) = (action.review.clone(), action.task.clone().unwrap());
        let res = if task.ends_with(":offer") {
            if self.rng.chance(85) {
                self.s.accept(name, &id, self.now).map(|_| "accepted".to_string())
            } else {
                self.s.decline(name, &id, "no access", self.now).map(|_| "declined".to_string())
            }
        } else if task.ends_with(":review") {
            if self.rng.chance(20) {
                return format!("{name} stays silent on {id}");
            }
            let n = self.rng.below(3);
            let findings = (0..n)
                .map(|i| FindingIn {
                    severity: ["major", "minor"][self.rng.below(2)].into(),
                    url: None,
                    title: Some(format!("issue {i}")),
                    text: format!("{name} found {i}"),
                })
                .collect();
            let sub = SubmitIn { head_sha: SHAS[0].into(), verdict: "approve".into(), summary: "looked".into(), checked: "src/lib.rs".into(), findings };
            self.s.submit(name, &id, sub, self.now).map(|n| format!("submitted {n}"))
        } else if task.contains(":vote:") {
            let owed: Vec<String> = self.s.review(&id).unwrap().owed_votes(name).iter().map(|f| f.id.clone()).collect();
            let votes = owed
                .into_iter()
                .map(|f| {
                    let agree = self.rng.chance(60);
                    VoteIn {
                        finding: f,
                        vote: if agree { "agree" } else { "disagree" }.into(),
                        severity: None,
                        reason: Some(if agree { "checked" } else { "not real" }.into()),
                        duplicate_of: None,
                    }
                })
                .collect();
            self.s.vote(name, &id, votes, self.now).map(|v| format!("voted {}", v.recorded))
        } else {
            self.s.say(name, &id, "it stands", self.now).map(|_| "replied".to_string())
        };
        format!("{name} on {task}: {res:?}")
    }

    fn check(&mut self, pre: &State) {
        if let Err(why) = props::check(pre, &self.s, self.now).and_then(|_| self.churn.check(pre, &self.s, self.now)) {
            let tail: Vec<&String> = self.log.iter().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect();
            panic!("{why} (seed {})\nlast steps:\n{}", self.seed, tail.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n"));
        }
    }
}

/// Seeded, so a failure names a run that replays exactly; `QUEUE_SEEDS=5000` runs longer.
#[test]
fn random_actors_never_break_the_queue_rules() {
    let seeds: u64 = std::env::var("QUEUE_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(100);
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for seed in 1..=seeds {
        let mut w = World::new(seed);
        for _ in 0..300 {
            w.step();
        }
        for r in &w.s.reviews {
            let kind = match r.phase {
                Phase::Done => "done",
                Phase::Failed if r.cancelled_by.is_some() => "cancelled",
                Phase::Failed if r.failure.as_deref().is_some_and(|f| f.starts_with("no full team")) => "queue timeout",
                Phase::Failed => "dropped out",
                _ => "unfinished",
            };
            *seen.entry(kind).or_default() += 1;
            if r.parent.is_some() && r.retries == 0 {
                *seen.entry("re-review").or_default() += 1;
            }
            if r.retries > 0 {
                *seen.entry(if r.phase == Phase::Done { "retry done" } else { "retried" }).or_default() += 1;
            }
            if r.retries == 2 && r.broke_off() {
                *seen.entry("retry cap").or_default() += 1;
            }
            if r.params.reviewers >= 3 && r.phase == Phase::Done && r.slots.len() == 2 {
                *seen.entry("started with fewer").or_default() += 1;
            }
            for e in r.log.iter().filter(|e| e.kind == "release") {
                let why = if e.text.contains("older review") { "released for an older review" } else { "released" };
                *seen.entry(why).or_default() += 1;
            }
            if r.log.iter().any(|e| e.kind == "ruling") {
                *seen.entry("ruled").or_default() += 1;
            }
            if r.log.iter().any(|e| e.kind == "lapse") {
                *seen.entry("lapsed").or_default() += 1;
            }
            let declined = |e: &&amx::relay::LogEntry| e.kind == "decline";
            if r.log.iter().filter(declined).any(|d| r.log.iter().any(|e| e.kind == "accept" && e.who == d.who && e.at > d.at)) {
                *seen.entry("asked again after declining").or_default() += 1;
            }
        }
    }
    for kind in ["done", "cancelled", "queue timeout", "dropped out", "re-review", "retried", "retry done", "retry cap", "started with fewer", "released", "released for an older review", "lapsed", "asked again after declining", "ruled"] {
        assert!(seen.get(kind).is_some_and(|n| *n > 0), "no run reached {kind}: {seen:?}");
    }
}
