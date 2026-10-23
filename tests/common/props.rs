//! The state machine model's safety properties, checked on the relay's real state across one
//! step from `pre` to `s`.

use amx::relay::{Agent, Choice, Model, Ms, Phase, Review, Role, State, Status, Verdict};
use std::collections::{BTreeMap, BTreeSet};

/// The first property the step breaks, as "S1: why".
pub fn check(pre: &State, s: &State, now: Ms) -> Result<(), String> {
    let fail = |prop: &str, why: String| Err(format!("{prop}: {why}"));
    let free: Vec<&Agent> = free(s, now);
    let members = s.reviews.iter().filter(|r| r.phase == Phase::Offering).flat_map(|r| r.active_slots().filter_map(|x| s.agents.get(&x.agent)));
    let freed = wanted(&s.reviews.iter().collect::<Vec<_>>(), free.iter().copied().chain(members).collect());
    for a in s.agents.all() {
        let k = s.reviews.iter().filter(|r| r.phase.active() && r.slot(&a.name).is_some_and(|x| x.active())).count();
        if k > 1 {
            return fail("S1", format!("{} is in {k} teams", a.name));
        }
    }
    for r in &s.reviews {
        let prev = pre.review(&r.id);
        let was = prev.map(|p| p.phase);
        if was == Some(Phase::Offering) && r.phase != Phase::Offering && r.phase.active() {
            let team: Vec<&str> = r.active_slots().map(|x| x.agent.as_str()).collect();
            if !valid_team(s, r, &team) {
                return fail("S2b", format!("{} started with {team:?}", r.id));
            }
            if let Some(x) = team.iter().find(|a| !s.agents.online(a, now)) {
                return fail("S17", format!("{} started with {x} offline", r.id));
            }
            if let Some(why) = needed_by_older(pre, prev.unwrap(), &team, now) {
                return fail("L6", format!("{} started with {team:?}: {why}", r.id));
            }
        }
        if r.phase == Phase::Offering {
            for x in r.active_slots() {
                if !s.agents.online(&x.agent, now) {
                    return fail("F2", format!("{} keeps offline {} in its forming team", r.id, x.agent));
                }
                if x.submission.is_some() {
                    return fail("S2", format!("{} reviewed {} before its team was complete", x.agent, r.id));
                }
                let a = s.agents.get(&x.agent).unwrap();
                if !fillable(r, a, &free) {
                    return fail("F1", format!("{} holds {} though no free reviewer can complete its team", r.id, x.agent));
                }
                if let Some(o) = freed.get(a.name.as_str()) {
                    return fail("F3", format!("{} holds {} while the older {o} would be offered it, were the younger forming teams free", r.id, x.agent));
                }
            }
            for a in s.agents.all() {
                if s.next_actions(&a.name, now).iter().any(|x| x.review == r.id && x.task.as_deref().is_some_and(|t| t.ends_with(":review"))) {
                    return fail("S2", format!("{} has the review task of {}, still forming", a.name, r.id));
                }
            }
        }
        if r.phase == Phase::Failed && was == Some(Phase::Offering) && r.cancelled_by.is_none() && r.slots.iter().any(|x| x.submission.is_some()) {
            return fail("S15", format!("{} failed for want of a team after work was done", r.id));
        }
        if r.phase == Phase::Done && r.params.reviewers >= 2 {
            let models: BTreeSet<Model> = r.active_slots().filter_map(|x| x.model).collect();
            if models.len() < Model::ALL.len() {
                return fail("S19", format!("{} reached a verdict with only {models:?} among the reviewers who completed it", r.id));
            }
        }
        if matches!(r.phase, Phase::Voting(_)) && r.active_slots().any(|x| x.submission.is_none()) {
            return fail("S3", format!("{} revealed findings before every active reviewer submitted", r.id));
        }
        for f in &r.findings {
            for voter in f.votes.keys() {
                if f.by.as_deref() == Some(voter) || r.slot(voter).is_none() || *voter == r.author {
                    return fail("S4", format!("{voter} voted on {}/{}", r.id, f.id));
                }
            }
            let backed = f.current_votes().any(|(v, x)| f.by.as_deref() != Some(v.as_str()) && x.choice != Choice::Disagree);
            if f.outcome.as_ref().is_some_and(|o| o.status == Status::Confirmed) && !backed {
                return fail("S5", format!("{}/{} confirmed on its author's word", r.id, f.id));
            }
        }
        for x in r.slots.iter().filter(|x| x.active()) {
            if prev.and_then(|p| p.slot(&x.agent)).is_some_and(|y| !y.active()) {
                return fail("S8", format!("{} regained its slot in {}", x.agent, r.id));
            }
        }
        if let Some(p) = was.filter(|p| !p.active() && *p != r.phase) {
            return fail("S7", format!("{} went from {p:?} to {:?}", r.id, r.phase));
        }
        if let Some(p) = prev.filter(|p| p.phase == Phase::Done && p.verdict() != r.verdict())
            && !r.log.iter().skip(p.log.len()).any(|e| e.kind == "ruling")
        {
            return fail("S20", format!("{}'s verdict went from {:?} to {:?} with no ruling", r.id, p.verdict(), r.verdict()));
        }
        let results = s.peek_events(&r.author, 0).0.into_iter().filter(|e| e.result && e.review.as_deref() == Some(&r.id)).count();
        let rulings = r.log.iter().filter(|e| e.kind == "ruling").count();
        let retried = s.reviews.iter().any(|c| c.retries > 0 && c.parent.as_deref() == Some(&r.id));
        if results != usize::from(!r.phase.active() && !retried) + rulings {
            return fail("S7", format!("{} ({:?}) has {results} result notices after {rulings} rulings", r.id, r.phase));
        }
    }
    if let Some(why) = retry_check(pre, s) {
        return Err(why);
    }
    let live: Vec<&Review> = s.reviews.iter().filter(|r| r.phase.active()).collect();
    for (i, a) in live.iter().enumerate() {
        if let Some(b) = live[i + 1..].iter().find(|b| a.author == b.author && a.pr == b.pr && a.head_sha == b.head_sha) {
            return fail("S12", format!("{} and {} are the same request", a.id, b.id));
        }
    }
    let logins: BTreeSet<String> = live.iter().map(|r| r.author_login.to_ascii_lowercase()).collect();
    for login in logins {
        let n = live.iter().filter(|r| r.author_login.eq_ignore_ascii_case(&login)).count();
        if n > 5 {
            return fail("S18", format!("{login} has {n} unfinished reviews"));
        }
    }
    if let Some(why) = blind_check(pre, s, now) {
        return fail("H1", why);
    }
    let offers: BTreeMap<&str, Vec<String>> = free.iter().map(|a| (a.name.as_str(), offered(s, &a.name, now))).collect();
    for a in &free {
        let Some(id) = offers[a.name.as_str()].first() else { continue };
        let r = s.review(id).unwrap();
        let older = |y: &&Agent| offers[y.name.as_str()].first().and_then(|o| s.review(o)).is_some_and(|o| o.seq() < r.seq());
        let rest: Vec<&Agent> = free.iter().copied().filter(|y| !older(y)).collect();
        if !fillable(r, a, &rest) {
            return fail("O1", format!("{} is offered {id}, whose team needs a reviewer offered an older review", a.name));
        }
    }
    let want = wanted(&s.reviews.iter().collect::<Vec<_>>(), free.clone());
    for a in &free {
        let shown = &offers[a.name.as_str()];
        let want = want.get(a.name.as_str());
        if shown.len() > 1 || shown.first() != want {
            return fail("S9", format!("{} is offered {shown:?}, should be {want:?} (oldest a team can form for from the reviewers older ones leave)", a.name));
        }
    }
    Ok(())
}

/// R1: the hub retries a line of reviews at most twice, each retry of one that reviewers' drops
/// failed, and only once. R2: a retry never starts alongside another unfinished review of its PR
/// by its author's login. R3: a retry reviews the head its failed review had.
fn retry_check(pre: &State, s: &State) -> Option<String> {
    for r in s.reviews.iter().filter(|r| r.retries > 0) {
        let Some(p) = r.parent.as_deref().and_then(|p| s.review(p)) else { continue };
        if r.retries > 2 || r.retries != p.retries + 1 || !p.broke_off() {
            return Some(format!("R1: {} is retry {} of {} ({:?}, retry {})", r.id, r.retries, p.id, p.failure, p.retries));
        }
        if s.reviews.iter().filter(|c| c.retries > 0 && c.parent == r.parent).count() > 1 {
            return Some(format!("R1: {} was retried more than once", p.id));
        }
        if p.head_sha != r.head_sha {
            return Some(format!("R3: {} reviews {:?}, its failed {} {:?}", r.id, r.head_sha, p.id, p.head_sha));
        }
        let rival = s.reviews.iter().find(|o| o.id != r.id && o.phase.active() && o.pr == r.pr && o.author_login.eq_ignore_ascii_case(&r.author_login));
        if let Some(o) = rival.filter(|_| pre.review(&r.id).is_none()) {
            return Some(format!("R2: {} started beside {}, of the same PR and login", r.id, o.id));
        }
    }
    None
}

/// The review each of `pool` should be offered: the oldest a team can form for from the
/// reviewers older ones leave. A review's own forming members in `pool` count only for older ones.
fn wanted<'a>(reviews: &[&Review], mut pool: Vec<&'a Agent>) -> BTreeMap<&'a str, String> {
    let mut want = BTreeMap::new();
    for r in reviews {
        pool.retain(|a| r.phase != Phase::Offering || r.slot(&a.name).is_none_or(|x| !x.active()));
        let takers: Vec<&Agent> = pool.iter().copied().filter(|a| can_join(r, a) && fillable(r, a, &pool)).collect();
        pool.retain(|a| !takers.iter().any(|t| t.name == a.name));
        want.extend(takers.iter().map(|a| (a.name.as_str(), r.id.clone())));
    }
    want
}

/// H1: no findings count or content is visible to other reviewers before every reviewer has
/// submitted. Whatever the others have submitted, the web pages, the JSON API and `review_status`
/// show each viewer exactly what they would if the others had submitted nothing but their head.
/// Checked on the reviews the step changed.
fn blind_check(pre: &State, s: &State, now: Ms) -> Option<String> {
    let changed = |r: &Review| pre.review(&r.id).is_none_or(|p| p.log.len() != r.log.len());
    let hidden = |r: &Review| !r.revealed() && r.slots.iter().any(|x| x.submission.is_some()) && changed(r);
    if !s.reviews.iter().any(hidden) {
        return None;
    }
    let mut seen = s.clone();
    seen.reviews.retain(hidden);
    let mut b = seen.clone();
    for r in &mut b.reviews {
        *r = blind(r, "");
    }
    if amx::web::render_index(&seen, now, &amx::web::View::anyone()) != amx::web::render_index(&b, now, &amx::web::View::anyone()) {
        return Some("the index".into());
    }
    for r in s.reviews.iter().filter(|r| hidden(r)) {
        let shown = |x: &Review| {
            let page = amx::web::render_review_page(x, &[], None, now, &|_| false);
            (page, amx::web::review_json(x, &[], now, ""), amx::web::list_json(x, now))
        };
        if shown(r) != shown(&blind(r, "")) {
            return Some(format!("{}'s page or JSON", r.id));
        }
        for viewer in r.slots.iter().map(|x| x.agent.as_str()).chain([r.author.as_str()]) {
            if amx::text::status(r, viewer, now, "") != amx::text::status(&blind(r, viewer), viewer, now, "") {
                return Some(format!("review_status of {} for {viewer}", r.id));
            }
        }
    }
    None
}

/// `r` as if reviewers other than `viewer` had submitted no findings, summary or note.
fn blind(r: &Review, viewer: &str) -> Review {
    let mut b = r.clone();
    b.findings.retain(|f| f.by.is_none() || f.by.as_deref() == Some(viewer));
    for sub in b.slots.iter_mut().filter(|x| x.agent != viewer).filter_map(|x| x.submission.as_mut()) {
        sub.summary.clear();
        sub.verdict = Verdict::Approve;
        sub.checked = None;
    }
    for e in b.log.iter_mut().filter(|e| e.kind == "submit" && e.who != viewer) {
        let sha = e.text.split_once(" at ").map_or("", |(_, t)| t.split("; ").next().unwrap_or(t)).to_string();
        e.text = format!("submitted 0 finding(s) at {sha}");
    }
    b
}

/// C1's memory along one run: what teams could form, and how often each review released each
/// reviewer since that last changed.
#[derive(Clone, Default)]
pub struct Churn {
    teams: String,
    released: BTreeMap<(String, String), usize>,
}

impl Churn {
    /// C1: while the same teams could form, a review releases a reviewer at most twice, so a
    /// silent reviewer can't keep another joining and being released. A decline that runs out, or
    /// a `wait` from a reviewer whose offer lapsed, changes who can.
    pub fn check(&mut self, pre: &State, s: &State, now: Ms) -> Result<(), String> {
        let started = |a: &Agent| s.reviews.iter().any(|r| matches!(r.phase, Phase::Reviewing | Phase::Voting(_)) && r.slot(&a.name).is_some_and(|x| x.active()));
        let available: Vec<&str> = s
            .agents
            .all()
            .filter(|a| a.role == Role::Reviewer && a.model.is_some() && s.agents.online(&a.name, now) && !started(a))
            .map(|a| a.name.as_str())
            .collect();
        let forming: Vec<(&str, &BTreeSet<String>, &BTreeSet<String>)> = s.reviews.iter().filter(|r| r.phase == Phase::Offering).map(|r| (r.id.as_str(), &r.declined, &r.lapsed)).collect();
        let teams = format!("{available:?} {forming:?}");
        if teams != self.teams {
            self.teams = teams;
            self.released.clear();
        }
        for r in &s.reviews {
            let logged = pre.review(&r.id).map_or(0, |p| p.log.len());
            for e in r.log.iter().skip(logged).filter(|e| e.kind == "release") {
                let n = self.released.entry((r.id.clone(), e.who.clone())).or_default();
                *n += 1;
                if *n > 2 {
                    return Err(format!("C1: {} released {} {n} times while the same teams could form", r.id, e.who));
                }
            }
        }
        Ok(())
    }
}

fn offered(s: &State, name: &str, now: Ms) -> Vec<String> {
    s.next_actions(name, now).into_iter().filter(|x| x.task.as_deref().is_some_and(|t| t.ends_with(":offer"))).map(|x| x.review).collect()
}

/// Online reviewers with a program that are in no unfinished review.
fn free(s: &State, now: Ms) -> Vec<&Agent> {
    s.agents
        .all()
        .filter(|a| a.role == Role::Reviewer && a.model.is_some() && s.agents.online(&a.name, now) && s.busy_with(&a.name).is_none())
        .collect()
}

fn own(r: &Review, login: &str) -> bool {
    r.author_login.eq_ignore_ascii_case(login)
}

/// The README's rules for who may take a place in a forming team.
fn can_join(r: &Review, a: &Agent) -> bool {
    let n = r.active_slots().count();
    let models: BTreeSet<Model> = r.active_slots().filter_map(|x| x.model).collect();
    let last = n + 1 == r.params.reviewers;
    let leaves_out = r.params.reviewers >= 2 && last && Model::ALL.iter().any(|m| !models.contains(m) && a.model != Some(*m));
    r.phase == Phase::Offering
        && a.role == Role::Reviewer
        && a.model.is_some()
        && a.name != r.author
        && r.slot(&a.name).is_none()
        && !r.declined.contains(&a.name)
        && !r.lapsed.contains(&a.name)
        && (r.picked.is_empty() || r.picked.contains(&a.name))
        && n < r.params.reviewers
        && (own(r, &a.login) || !r.active_slots().any(|x| x.login.eq_ignore_ascii_case(&a.login)))
        && !leaves_out
}

/// Some valid team containing the members and `a` can be drawn from `free`.
fn fillable(r: &Review, a: &Agent, free: &[&Agent]) -> bool {
    let mut base: Vec<&str> = r.active_slots().map(|x| x.agent.as_str()).collect();
    if !base.contains(&a.name.as_str()) {
        base.push(&a.name);
    }
    let pool: Vec<&Agent> = free.iter().copied().filter(|y| y.name != a.name && can_join(r, y)).collect();
    let s_agents: BTreeMap<&str, (&str, Option<Model>)> = r
        .active_slots()
        .map(|x| (x.agent.as_str(), (x.login.as_str(), x.model)))
        .chain([(a.name.as_str(), (a.login.as_str(), a.model))])
        .chain(pool.iter().map(|y| (y.name.as_str(), (y.login.as_str(), y.model))))
        .collect();
    (0u32..1 << pool.len()).any(|bits| {
        let team: Vec<&str> = base.iter().copied().chain(pool.iter().enumerate().filter(|(i, _)| bits & 1 << i != 0).map(|(_, y)| y.name.as_str())).collect();
        valid(r, &team.iter().map(|n| s_agents[n]).collect::<Vec<_>>())
    })
}

/// Count members, or for three or more at least two; both programs when two or more; one per
/// login other than the author's.
fn valid(r: &Review, team: &[(&str, Option<Model>)]) -> bool {
    let c = r.params.reviewers;
    let n = team.len();
    if !(n == c || (c >= 3 && (2..c).contains(&n))) {
        return false;
    }
    if c >= 2 && !Model::ALL.iter().all(|m| team.iter().any(|(_, x)| *x == Some(*m))) {
        return false;
    }
    let logins: Vec<String> = team.iter().map(|(l, _)| l.to_ascii_lowercase()).filter(|l| !own(r, l)).collect();
    logins.iter().collect::<BTreeSet<_>>().len() == logins.len()
}

fn valid_team(s: &State, r: &Review, team: &[&str]) -> bool {
    let members: Vec<(&str, Option<Model>)> = team.iter().map(|n| (s.agents.get(n).unwrap().login.as_str(), r.slot(n).unwrap().model)).collect();
    valid(r, &members)
}

/// L6, as the model words it: an older waiting review could have completed its forming team
/// with these members plus the reviewers free for it, and can't without them. A reviewer is
/// free for a review when online, allowed, on no running review, and not spoken for by an even
/// older review that can start. (The model lets the older review swap out its own forming
/// members too; the relay never does, which matters only when one other person runs both
/// programs.)
fn needed_by_older(pre: &State, started: &Review, team: &[&str], now: Ms) -> Option<String> {
    let reviewers: Vec<&Agent> = pre.agents.all().filter(|a| a.role == Role::Reviewer && a.model.is_some()).collect();
    let allowed = |r: &Review, a: &Agent| (r.picked.is_empty() || r.picked.contains(&a.name)) && !r.declined.contains(&a.name) && !r.lapsed.contains(&a.name) && r.slot(&a.name).is_none_or(|x| x.active());
    let running = |a: &Agent| pre.reviews.iter().any(|r| matches!(r.phase, Phase::Reviewing | Phase::Voting(_)) && r.slot(&a.name).is_some_and(|x| x.active()));
    let can_start = |r: &Review| {
        let pool: Vec<&Agent> = reviewers.iter().copied().filter(|a| (r.slot(&a.name).is_some_and(|x| x.active())) || (pre.agents.online(&a.name, now) && !running(a) && allowed(r, a))).collect();
        any_team(r, &pool, &[])
    };
    let target = |a: &Agent| -> Option<String> {
        let forming = pre.reviews.iter().find(|r| r.phase == Phase::Offering && r.slot(&a.name).is_some_and(|x| x.active()));
        forming.map(|r| r.id.clone()).or_else(|| {
            pre.next_actions(&a.name, now).into_iter().find(|x| x.task.as_deref().is_some_and(|t| t.ends_with(":offer"))).map(|x| x.review)
        })
    };
    let seq = |id: &str| pre.review(id).map_or(u64::MAX, Review::seq);
    for older in pre.reviews.iter().filter(|o| o.phase == Phase::Offering && o.seq() < started.seq()) {
        let free_for: Vec<&Agent> = reviewers
            .iter()
            .copied()
            .filter(|a| {
                team.contains(&a.name.as_str())
                    || (pre.agents.online(&a.name, now)
                        && allowed(older, a)
                        && !running(a)
                        && !target(a).is_some_and(|c| seq(&c) < older.seq() && pre.review(&c).is_some_and(can_start)))
            })
            .filter(|a| allowed(older, a))
            .collect();
        if any_team(older, &free_for, &[]) && !any_team(older, &free_for, team) {
            return Some(format!("the older {} needed them", older.id));
        }
    }
    None
}

/// Some valid team for `r` that keeps its forming members, from `pool` minus `without`.
fn any_team(r: &Review, pool: &[&Agent], without: &[&str]) -> bool {
    let pool: Vec<&Agent> = pool.iter().copied().filter(|a| !without.contains(&a.name.as_str())).collect();
    let must = |a: &Agent| r.slot(&a.name).is_some_and(|x| x.active());
    (1u32..1 << pool.len()).any(|bits| {
        let picked = |i: usize| bits & 1 << i != 0;
        pool.iter().enumerate().all(|(i, a)| picked(i) || !must(a))
            && valid(r, &pool.iter().enumerate().filter(|(i, _)| picked(*i)).map(|(_, a)| (a.login.as_str(), a.model)).collect::<Vec<_>>())
    })
}
