//! Every interleaving of the actors' actions and the deadlines they race, within small bounds:
//! a breadth-first search over the relay's real state, checking the state machine model's
//! safety properties across every step. `EXPLORE=large` raises the bounds (about seven minutes and
//! 4 GB with `--release`).

#[path = "common/props.rs"]
mod props;

use amx::config::{Consensus, Timing};
use amx::relay::{FindingIn, Model, Ms, NewReview, Phase, Role, State, SubmitIn, VoteIn, rereview};
use amx::tailscale::Identity;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::Write;

const SHAS: [&str; 2] = ["0123456789abcdef0123456789abcdef01234567", "89abcdef0123456789abcdef0123456789abcdef"];
const AUTHORS: [(&str, &str); 2] = [("al-author", "al@example.com"), ("bo-author", "bo@example.com")];
const REVIEWERS: [(&str, &str, Model); 4] = [
    ("cy-claude", "cy@example.com", Model::Claude),
    ("dee-codex", "dee@example.com", Model::Codex),
    ("eve-claude", "eve@example.com", Model::Claude),
    ("al-codex", "al@example.com", Model::Codex),
];

/// Interchangeable reviewers.
const TWINS: [&str; 2] = ["cy-claude", "eve-claude"];

struct Bounds {
    name: &'static str,
    authors: usize,
    reviewers: usize,
    /// Requests in all, each of a new PR; re-reviews come on top.
    requests: usize,
    rereviews: usize,
    cancels: usize,
    /// Times reviewers go offline, in all.
    absences: usize,
    counts: &'static [usize],
    submissions: &'static [&'static [&'static str]],
    withdraw: bool,
    /// Also requests repeated while under way and accepts of reviews never offered.
    strays: bool,
}

const BASE: Bounds = Bounds {
    name: "",
    authors: 1,
    reviewers: 3,
    requests: 1,
    rereviews: 0,
    cancels: 1,
    absences: 0,
    counts: &[2],
    submissions: &[&[]],
    withdraw: false,
    strays: false,
};

const SMALL: [Bounds; 3] = [
    Bounds { name: "two requests queue for three reviewers", requests: 2, ..BASE },
    Bounds { name: "a review and its re-review", reviewers: 2, rereviews: 1, cancels: 0, submissions: &[&["major"]], ..BASE },
    Bounds { name: "a reviewer goes offline", absences: 1, ..BASE },
];

const LARGE: [Bounds; 3] = [
    Bounds { name: "two requests queue for three reviewers", requests: 2, absences: 1, counts: &[2, 3], strays: true, ..BASE },
    Bounds { name: "two authors and four reviewers", authors: 2, reviewers: 4, requests: 2, ..BASE },
    Bounds {
        name: "a review and its re-review",
        reviewers: 2,
        rereviews: 1,
        submissions: &[&[], &["major"], &["minor"]],
        withdraw: true,
        ..BASE
    },
];

/// States one search may keep before it gives up, a few kilobytes each.
const MAX_STATES: usize = 1_500_000;

#[derive(Debug)]
enum Act {
    Hello(&'static str),
    Online(&'static str),
    Offline(&'static str),
    Request(&'static str, usize, usize),
    Rereview(String),
    Cancel(String),
    Accept(&'static str, String),
    Decline(&'static str, String),
    Submit(&'static str, String, &'static [&'static str]),
    Vote(&'static str, String, Vec<(String, bool)>),
    Say(&'static str, String),
    Withdraw(&'static str, String, String),
    Wait(u64),
}

fn who(name: &str) -> Identity {
    let login = AUTHORS.iter().map(|(n, l)| (*n, *l)).chain(REVIEWERS.iter().map(|(n, l, _)| (*n, *l))).find(|(n, _)| *n == name).unwrap().1;
    Identity {
        login: login.into(),
        display_name: String::new(),
        node: "box".into(),
        user_id: 0,
    }
}

/// Authors and online reviewers stay in `wait`; a reviewer that goes offline stops calling.
fn fresh(b: &Bounds) -> (State, Ms) {
    let now = 1_760_000_000_000;
    let consensus = Consensus { reviewers: 2, queue_secs: 900, ..Consensus::default() };
    let mut s = State::new(Timing::default(), consensus, 50);
    for (name, _) in &AUTHORS[..b.authors] {
        s.touch(name, &who(name), Some(Role::Author), now).unwrap();
        s.set_waiting(name, 1, now);
    }
    s.tick(now);
    (s, now)
}

/// The times at which something may happen without anyone acting.
fn deadlines(s: &State) -> Vec<Ms> {
    let t = s.timing;
    let mut v = Vec::new();
    for a in s.agents.all() {
        v.extend(s.offer_lapses(&a.name));
        if a.waiting > 0 {
            continue;
        }
        v.push(a.last_seen + t.online_secs * 1000);
        if s.busy_with(&a.name).is_some() {
            v.push(a.last_seen + t.drop_after_secs * 1000);
        }
    }
    for r in s.reviews.iter().filter(|r| r.phase.active()) {
        v.push(r.deadline);
        if r.phase == Phase::Offering {
            v.extend(r.active_slots().map(|x| x.joined + r.params.offer_secs * 1000).min());
            v.extend(r.declined.iter().filter_map(|a| r.declined_until(a)).filter(|t| *t < r.deadline));
        }
    }
    v
}

fn actions(s: &State, now: Ms, absences: usize, b: &Bounds) -> Vec<Act> {
    let mut v = Vec::new();
    let requested = s.reviews.iter().filter(|r| r.parent.is_none()).count();
    for (a, _) in &AUTHORS[..b.authors] {
        if requested < b.requests {
            v.extend(b.counts.iter().map(|&c| Act::Request(a, requested + 1, c)));
        }
        if b.strays {
            let again = s.reviews.iter().filter(|r| r.phase.active() && r.author == *a && r.parent.is_none());
            v.extend(again.map(|r| Act::Request(a, r.pr.number as usize, r.params.reviewers)));
        }
    }
    if s.reviews.iter().filter(|r| r.parent.is_some()).count() < b.rereviews {
        let done = s.reviews.iter().filter(|r| r.phase == Phase::Done && rereview(&s.reviews, &r.id).is_none());
        v.extend(done.map(|r| Act::Rereview(r.id.clone())));
    }
    if s.reviews.iter().filter(|r| r.cancelled_by.is_some()).count() < b.cancels {
        v.extend(s.reviews.iter().filter(|r| r.phase.active()).map(|r| Act::Cancel(r.id.clone())));
    }
    for (n, ..) in &REVIEWERS[..b.reviewers] {
        let Some(a) = s.agents.get(n) else {
            v.push(Act::Hello(n));
            break;
        };
        if a.waiting == 0 {
            v.push(Act::Online(n));
            continue;
        }
        if absences < b.absences {
            v.push(Act::Offline(n));
        }
        let todo = s.next_actions(n, now);
        for r in s.reviews.iter().filter(|r| r.phase == Phase::Offering && r.slot(n).is_none()) {
            if b.strays || todo.iter().any(|x| x.review == r.id && x.task.as_deref().is_some_and(|t| t.ends_with(":offer"))) {
                v.push(Act::Accept(n, r.id.clone()));
            }
        }
        for x in todo {
            let (Some(task), id) = (x.task, x.review) else { continue };
            if task.ends_with(":offer") {
                v.push(Act::Decline(n, id));
            } else if task.ends_with(":review") {
                v.extend(b.submissions.iter().map(|f| Act::Submit(n, id.clone(), f)));
            } else if task.contains(":vote:") {
                let owed: Vec<String> = s.review(&id).unwrap().owed_votes(n).iter().map(|f| f.id.clone()).collect();
                for bits in 0u32..1 << owed.len() {
                    v.push(Act::Vote(n, id.clone(), owed.iter().enumerate().map(|(i, f)| (f.clone(), bits & 1 << i != 0)).collect()));
                }
            } else {
                v.push(Act::Say(n, id.clone()));
                if b.withdraw {
                    v.extend(s.review(&id).unwrap().contested_by(n).into_iter().map(|f| Act::Withdraw(n, id.clone(), f)));
                }
            }
        }
    }
    v.extend(deadlines(s).into_iter().filter(|t| *t > now).min().map(|t| Act::Wait(t - now)));
    v
}

/// Applies `act` the way the MCP tools do, then the relay's tick.
fn apply(s: &mut State, now: &mut Ms, act: &Act) {
    let t = *now;
    let pr = |n: usize| format!("https://github.com/example-org/app/pull/{n}");
    let touch = |s: &mut State, n: &str| s.touch(n, &who(n), None, t).unwrap();
    let _ = match act {
        Act::Hello(n) => {
            s.touch(n, &who(n), Some(Role::Reviewer), t).unwrap();
            s.set_model(n, REVIEWERS.iter().find(|r| r.0 == *n).unwrap().2);
            s.set_waiting(n, 1, t);
            Ok(())
        }
        Act::Online(n) => {
            touch(s, n);
            s.set_waiting(n, 1, t);
            Ok(())
        }
        Act::Offline(n) => {
            s.set_waiting(n, -1, t);
            Ok(())
        }
        Act::Request(a, n, c) => {
            let mut req = NewReview::new(&pr(*n));
            req.title = Some("Add a cache".into());
            req.reviewers = Some(*c);
            req.head_sha = Some(SHAS[0].into());
            s.request_review(a, req, t).map(drop)
        }
        Act::Rereview(id) => {
            let author = s.review(id).unwrap().author.clone();
            s.rereview(&author, id, SHAS[1], "fixed", None, t).map(drop)
        }
        Act::Cancel(id) => s.cancel(id, "a human", t),
        Act::Accept(n, id) => {
            touch(s, n);
            s.accept(n, id, t)
        }
        Act::Decline(n, id) => {
            touch(s, n);
            s.decline(n, id, "no access", t)
        }
        Act::Submit(n, id, severities) => {
            touch(s, n);
            let findings = severities
                .iter()
                .enumerate()
                .map(|(i, sev)| FindingIn { severity: sev.to_string(), url: None, title: Some(format!("issue {i}")), text: format!("{n} found {i}") })
                .collect();
            s.submit(n, id, SubmitIn { head_sha: SHAS[0].into(), verdict: "approve".into(), summary: "looked".into(), checked: "src/lib.rs".into(), findings }, t).map(drop)
        }
        Act::Vote(n, id, votes) => {
            touch(s, n);
            let votes = votes
                .iter()
                .map(|(f, agree)| VoteIn {
                    finding: f.clone(),
                    vote: if *agree { "agree" } else { "disagree" }.into(),
                    severity: None,
                    reason: (!agree).then(|| "not real".into()),
                    duplicate_of: None,
                })
                .collect();
            s.vote(n, id, votes, t).map(drop)
        }
        Act::Say(n, id) => {
            touch(s, n);
            s.say(n, id, "it stands", t)
        }
        Act::Withdraw(n, id, f) => {
            touch(s, n);
            s.withdraw(n, id, f, "fair point", t)
        }
        Act::Wait(ms) => {
            *now += ms;
            Ok(())
        }
    };
    s.tick(*now);
}

/// What can still matter: phases, teams, findings and votes, presence, offers, and how far off
/// each pending deadline is; not review ids, timestamps or text. Two reviewers of the same
/// program and of logins nobody else has are interchangeable, so the key is the lesser of the
/// one with their names as they are and the one with them swapped.
fn key(s: &State, now: Ms) -> String {
    let t = s.timing;
    let left = |at: Ms| at.checked_sub(now).filter(|d| *d > 0);
    let pos = |id: &str| s.reviews.iter().position(|r| r.id == id);
    let forming = s.reviews.iter().any(|r| r.phase == Phase::Offering);
    let mut agents = Vec::new();
    for a in s.agents.all() {
        let offer = forming.then(|| s.next_actions(&a.name, now)).into_iter().flatten().find(|x| x.task.as_deref().is_some_and(|t| t.ends_with(":offer"))).map(|x| (pos(&x.review), s.offer_lapses(&a.name).map(left)));
        let quiet = (a.waiting == 0).then(|| (left(a.last_seen + t.online_secs * 1000), s.busy_with(&a.name).map(|_| left(a.last_seen + t.drop_after_secs * 1000))));
        agents.push(format!("{} {:?} {:?} {quiet:?} {offer:?}", a.name, a.role, a.model));
    }
    let mut k = String::new();
    for r in &s.reviews {
        let parent = r.parent.as_deref().map(pos);
        write!(k, "\n{} {} {:?} {} {parent:?} {:?}", r.author, r.pr.number, r.head_sha, r.params.reviewers, r.phase).unwrap();
        if r.phase == Phase::Done && rereview(&s.reviews, &r.id).is_none() {
            for x in r.active_slots() {
                write!(k, " {}", x.agent).unwrap();
            }
            for f in &r.findings {
                write!(k, " [{:?}]", f.outcome.as_ref().map(|o| (&o.status, o.severity))).unwrap();
            }
        }
        if !r.phase.active() {
            continue;
        }
        for x in &r.slots {
            write!(k, " {}:{}{}", x.agent, x.dropped.is_some(), x.submission.is_some()).unwrap();
        }
        for f in &r.findings {
            let votes: Vec<_> = f.votes.iter().map(|(by, v)| (by, &v.choice, v.severity, v.round, v.revision)).collect();
            write!(k, " [{:?} {:?} {} {} {} {votes:?}]", f.by, f.severity, f.withdrawn.is_some(), f.revision, f.open_round).unwrap();
        }
        let team_due = r.active_slots().map(|x| x.joined + r.params.offer_secs * 1000).min().filter(|_| r.phase == Phase::Offering).map(left);
        let declined: Vec<_> = r.declined.iter().map(|a| (a, r.declined_until(a).filter(|t| *t < r.deadline).map(left))).collect();
        write!(k, " {} {:?} {declined:?} {:?} {:?} {team_due:?}", r.replying, r.replied, r.lapsed, left(r.deadline)).unwrap();
    }
    let [x, y] = TWINS;
    let named = |swap: bool| {
        let name = |s: &str| if swap { s.replace(x, "\0").replace(y, x).replace('\0', y) } else { s.to_string() };
        let mut agents: Vec<String> = agents.iter().map(|a| name(a)).collect();
        agents.sort();
        agents.join("|") + &name(&k)
    };
    named(false).min(named(true))
}

/// The outcomes a step reaches, so the search can't pass by never getting anywhere.
fn reached(pre: &State, s: &State, seen: &mut BTreeMap<&'static str, usize>) {
    let mut hit = |what: &'static str| *seen.entry(what).or_default() += 1;
    for r in &s.reviews {
        let prev = pre.review(&r.id);
        let was = prev.map(|p| p.phase);
        if was.is_some_and(|p| p.active()) && !r.phase.active() {
            hit(match r.phase {
                Phase::Done if r.parent.is_some() => "re-review done",
                Phase::Done => "done",
                _ if r.cancelled_by.is_some() => "cancelled",
                _ if r.failure.as_deref().is_some_and(|f| f.starts_with("no full team")) => "queue timeout",
                _ => "dropped out",
            });
        }
        if was == Some(Phase::Offering) && matches!(r.phase, Phase::Reviewing) {
            hit("started");
        }
        if r.phase == Phase::Voting(2) && was != Some(r.phase) {
            hit("second round");
        }
        let logged = prev.map_or(0, |p| p.log.len());
        for e in r.log.iter().skip(logged) {
            match e.kind.as_str() {
                "release" if e.text.contains("older review") => hit("released for an older review"),
                "release" if e.text.contains("didn't fill") => hit("team timer"),
                "release" if e.text.contains("offline") => hit("released offline"),
                "release" => hit("released"),
                "drop" => hit("dropped"),
                "decline" => hit("declined"),
                "lapse" => hit("lapsed"),
                _ => {}
            }
        }
    }
}

fn trace(trail: &[(usize, String)], mut at: usize, last: &Act) -> String {
    let mut steps = vec![format!("{last:?}")];
    while at != 0 {
        steps.push(trail[at].1.clone());
        at = trail[at].0;
    }
    steps.iter().rev().enumerate().map(|(i, s)| format!("{:3}. {s}", i + 1)).collect::<Vec<_>>().join("\n")
}

/// Every state reachable within `b`, checking the properties across every step; the shortest
/// trace to a violation fails the test.
fn explore(b: &Bounds, seen: &mut BTreeMap<&'static str, usize>) {
    let started = std::time::Instant::now();
    let (s, now) = fresh(b);
    let mut index: HashMap<String, usize> = HashMap::from([(format!("0{}", key(&s, now)), 0)]);
    let mut trail: Vec<(usize, String)> = vec![(0, String::new())];
    let mut todo = VecDeque::from([(s, now, 0, 0, props::Churn::default())]);
    let mut steps = 0;
    while let Some((s, now, absences, at, churn)) = todo.pop_front() {
        for act in actions(&s, now, absences, b) {
            let (mut next, mut then, mut churn) = (s.clone(), now, churn.clone());
            apply(&mut next, &mut then, &act);
            steps += 1;
            if let Err(why) = props::check(&s, &next, then).and_then(|_| churn.check(&s, &next, then)) {
                panic!("{}: {why}\nshortest trace:\n{}", b.name, trace(&trail, at, &act));
            }
            reached(&s, &next, seen);
            let absences = absences + usize::from(matches!(act, Act::Offline(_)));
            let k = format!("{absences}{}", key(&next, then));
            if !index.contains_key(&k) {
                assert!(index.len() < MAX_STATES, "{}: more than {MAX_STATES} states; lower the bounds", b.name);
                index.insert(k, trail.len());
                trail.push((at, format!("{act:?}")));
                todo.push_back((next, then, absences, trail.len() - 1, churn));
            }
        }
    }
    println!("{}: {} states, {steps} steps in {:.1?}", b.name, index.len(), started.elapsed());
}

#[test]
fn every_interleaving_keeps_the_queue_rules() {
    let large = std::env::var("EXPLORE").is_ok_and(|v| v == "large");
    let mut seen = BTreeMap::new();
    for b in if large { &LARGE[..] } else { &SMALL[..] } {
        explore(b, &mut seen);
    }
    println!("reached {seen:?}");
    let outcomes = [
        "done",
        "re-review done",
        "cancelled",
        "queue timeout",
        "dropped out",
        "started",
        "second round",
        "released",
        "released for an older review",
        "released offline",
        "team timer",
        "dropped",
        "declined",
        "lapsed",
    ];
    for what in outcomes {
        assert!(seen.contains_key(what), "the search never reached {what}: {seen:?}");
    }
}
