//! The state machine model's safety properties, checked on the relay's real state across one
//! step from `pre` to `s`.

use amx::relay::{Agent, Choice, Model, Ms, Phase, Review, Role, State, Status, Verdict};
use std::collections::{BTreeMap, BTreeSet};

/// The first property the step breaks, as "S1: why".
pub fn check(pre: &State, s: &State, now: Ms) -> Result<(), String> {
    let fail = |prop: &str, why: String| Err(format!("{prop}: {why}"));
    let pre_held: BTreeMap<String, String> = pre.agents.all().filter_map(|a| Some((a.name.clone(), offered(pre, &a.name, now).into_iter().next()?))).collect();
    let (_, _, pre_kept) = wanted(pre, free(pre, now), &pre_held, now);
    let free: Vec<&Agent> = free(s, now);
    let members = s.reviews.iter().filter(|r| r.phase == Phase::Offering).flat_map(|r| r.active_slots().filter_map(|x| s.agents.get(&x.agent)));
    let held: BTreeMap<String, String> = s.agents.all().filter_map(|a| Some((a.name.clone(), offered(s, &a.name, now).into_iter().next()?))).collect();
    let (freed, ..) = wanted(s, free.iter().copied().chain(members).collect(), &held, now);
    for a in s.agents.all() {
        let k = s.reviews.iter().filter(|r| r.phase.active() && r.slot(&a.name).is_some_and(|x| x.active())).count();
        if k > 1 {
            return fail("S1", format!("{} is in {k} teams", a.name));
        }
    }
    for r in &s.reviews {
        let prev = pre.review(&r.id);
        let was = prev.map(|p| p.phase);
        let logged = prev.map_or(0, |p| p.log.len());
        for e in r.log.iter().skip(logged).filter(|e| e.kind == "accept" && !e.text.starts_with("assigned again")) {
            if let Some(o) = pre_kept.get(e.who.as_str()).filter(|o| pre.review(o).is_some_and(|o| o.seq() < r.seq())) {
                return fail("R7", format!("{} joined {} while the older {o} could still take it in its turns", e.who, r.id));
            }
        }
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
        let expired = r.log.iter().filter(|e| e.kind == "requeue").count();
        if results != usize::from(!r.phase.active() && !retried) + rulings + expired {
            return fail("S7", format!("{} ({:?}) has {results} result notices after {rulings} rulings", r.id, r.phase));
        }
    }
    if let Some(why) = retry_check(pre, s).or_else(|| requeue_check(pre, s, now)).or_else(|| rotation_check(pre, s, now, &free, &held, &|a| s.agents.turn(a), "R5")) {
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
    let (want, ..) = wanted(s, free.clone(), &held, now);
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
        if s.reviews.iter().filter(|c| c.retries > 0 && c.requeued.is_none() && c.parent == r.parent).count() > 1 {
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

fn timed_out(r: &Review) -> bool {
    r.phase == Phase::Failed && r.cancelled_by.is_none() && r.failure.as_deref().is_some_and(|f| f.starts_with("no full team"))
}

/// Q1: the hub waits to queue again only a review that found no team in time, for at most a day,
/// and not once the author's login has asked for another review of the PR. Q2: it queues one
/// again at most once per line (the reviews it retries back to one the author asked for), at
/// the same PR, head and author, never beside another unfinished review of the PR by that
/// login, and only when a team it can start with is free. Q3: once a step is over, no review
/// that follows no earlier review still waits while it could be queued again and a team could
/// start it (counting offers that lapsed on reviews the step trimmed from history).
fn requeue_check(pre: &State, s: &State, now: Ms) -> Option<String> {
    let lapsed = |a: &Agent| s.reviews.iter().chain(pre.reviews.iter().filter(|o| s.review(&o.id).is_none())).any(|o| o.lapsed.contains(&a.name));
    let held: BTreeMap<String, String> = s.agents.all().filter_map(|a| Some((a.name.clone(), offered(s, &a.name, now).into_iter().next()?))).collect();
    let (_, spare, _) = wanted(s, free(s, now), &held, now);
    let pool: Vec<&Agent> = spare.into_iter().filter(|a| !s.agents.gone(&a.name) && !lapsed(a)).collect();
    for r in s.reviews.iter().filter(|r| r.requeue_until.is_some() && r.parent.is_none()) {
        let open = s.reviews.iter().filter(|o| o.phase.active() && o.author_login.eq_ignore_ascii_case(&r.author_login)).count();
        let picked_online = r.picked.iter().all(|p| s.agents.get(p).is_some_and(|a| a.role == Role::Reviewer && a.model.is_some() && s.agents.online(p, now)));
        if open >= 5 || !picked_online {
            continue;
        }
        let mut y = r.clone();
        y.phase = Phase::Offering;
        y.slots.clear();
        y.declined.retain(|a| r.declined_until(a).is_some_and(|t| now < t));
        let team: Vec<&Agent> = pool.iter().copied().filter(|a| can_join(&y, a)).collect();
        if any_team(&y, &team, &[]) {
            return Some(format!("Q3: {} waits to be queued again though {:?} are free", r.id, team.iter().map(|a| &a.name).collect::<Vec<_>>()));
        }
    }
    let find = |id: &str| s.reviews.iter().find(|r| r.id == id);
    let newer = |r: &Review| s.reviews.iter().any(|o| o.id != r.id && o.pr == r.pr && o.author_login.eq_ignore_ascii_case(&r.author_login) && (o.phase.active() || o.seq() > r.seq()));
    for r in &s.reviews {
        if let Some(until) = r.requeue_until {
            if !timed_out(r) || r.ended.is_none_or(|e| until > e + 24 * 3600 * 1000) || now >= until {
                return Some(format!("Q1: {} waits to be queued again ({:?}, until {until})", r.id, r.failure));
            }
            if newer(r) {
                return Some(format!("Q1: {} waits to be queued again after a newer review of its PR", r.id));
            }
            if s.reviews.iter().any(|y| y.requeued.as_deref() == Some(&r.id)) {
                return Some(format!("Q1: {} waits to be queued again but already was", r.id));
            }
        }
        let Some(x) = r.requeued.as_deref() else { continue };
        if s.reviews.iter().filter(|y| y.requeued.as_deref() == Some(x)).count() > 1 {
            return Some(format!("Q2: {x} was queued again more than once"));
        }
        let mut line = find(x);
        while let Some(l) = line {
            if l.requeued.is_some() {
                return Some(format!("Q2: {} queued again {x}, in a line already queued again at {}", r.id, l.id));
            }
            line = l.parent.as_deref().filter(|_| l.retries > 0).and_then(find);
        }
        if pre.review(&r.id).is_some() {
            continue;
        }
        let Some(x) = find(x).or_else(|| pre.review(x)) else { return Some(format!("Q2: {} queued again a review that is gone", r.id)) };
        if !timed_out(x) || (x.pr.clone(), &x.head_sha, &x.author) != (r.pr.clone(), &r.head_sha, &r.author) {
            return Some(format!("Q2: {} queued again {} ({:?}) with another PR, head or author", r.id, x.id, x.failure));
        }
        if let Some(o) = s.reviews.iter().find(|o| o.id != r.id && o.phase.active() && o.pr == r.pr && o.author_login.eq_ignore_ascii_case(&r.author_login)) {
            return Some(format!("Q2: {} queued beside {}, of the same PR and login", r.id, o.id));
        }
        let pool: Vec<&Agent> = s
            .agents
            .all()
            .filter(|a| {
                r.slot(&a.name).is_some_and(|x| x.active())
                    || (a.role == Role::Reviewer && a.model.is_some() && s.agents.online(&a.name, now) && s.busy_with(&a.name).is_none() && a.name != r.author && (r.picked.is_empty() || r.picked.contains(&a.name)))
            })
            .collect();
        if !any_team(r, &pool, &[]) {
            return Some(format!("Q2: {} was queued again with no team free to start it", r.id));
        }
    }
    None
}

/// Whether queued review `r` is still in its first 30s of offers, when its seats go by turn.
fn rotating(s: &State, r: &Review, now: Ms) -> bool {
    s.offered_since(&r.id).is_none_or(|t| now < t + 30_000)
}

/// The review each of `pool` should be offered: the oldest a team can form for from the
/// reviewers older ones leave, or, for a review still rotating, those it is offered (`held`),
/// which `rotation_check` judges; a rotating review leaves younger ones none it could take. A
/// review's own forming members in `pool` count only for older ones. Also returns those of `pool`
/// no review takes or keeps, and those a rotating review keeps without offering it.
fn wanted<'a>(s: &State, mut pool: Vec<&'a Agent>, held: &BTreeMap<String, String>, now: Ms) -> (BTreeMap<&'a str, String>, Vec<&'a Agent>, BTreeMap<&'a str, String>) {
    let mut want = BTreeMap::new();
    let mut kept = BTreeMap::new();
    for r in &s.reviews {
        pool.retain(|a| r.phase != Phase::Offering || r.slot(&a.name).is_none_or(|x| !x.active()));
        let usable: Vec<&Agent> = pool.iter().copied().filter(|a| can_join(r, a) && fillable(r, a, &pool)).collect();
        let takers: Vec<&Agent> = if r.phase == Phase::Offering && rotating(s, r, now) {
            pool.iter().copied().filter(|a| held.get(&a.name) == Some(&r.id)).collect()
        } else {
            usable.clone()
        };
        pool.retain(|a| !takers.iter().chain(&usable).any(|t| t.name == a.name));
        want.extend(takers.iter().map(|a| (a.name.as_str(), r.id.clone())));
        kept.extend(usable.iter().filter(|a| !want.contains_key(a.name.as_str())).map(|a| (a.name.as_str(), r.id.clone())));
    }
    (want, pool, kept)
}

/// R5: a review in its first 30s of offers is offered to no more reviewers than it has open
/// seats, enough with its members for a team it can start with, and a reviewer newly offered it
/// comes no later than any free reviewer of its program that could take that seat and isn't
/// offered an older review: those that let an offer lapse last (whether or not lapses on reviews
/// the step trimmed count), then by their person's seats in the review, then by turn.
fn rotation_check(pre: &State, s: &State, now: Ms, free: &[&Agent], held: &BTreeMap<String, String>, turn: &dyn Fn(&Agent) -> (u64, u64), tag: &str) -> Option<String> {
    let seq = |id: &str| s.review(id).map_or(u64::MAX, Review::seq);
    let lapsed = |a: &Agent| s.reviews.iter().any(|o| o.lapsed.contains(&a.name));
    let trimmed = |a: &Agent| lapsed(a) || pre.reviews.iter().any(|o| s.review(&o.id).is_none() && o.lapsed.contains(&a.name));
    let t = turn;
    for r in s.reviews.iter().filter(|r| r.phase == Phase::Offering && rotating(s, r, now)) {
        let to: Vec<&Agent> = free.iter().copied().filter(|a| held.get(&a.name) == Some(&r.id)).collect();
        let open = r.params.reviewers.saturating_sub(r.active_slots().count());
        if to.len() > open {
            return Some(format!("{tag}: {} has {open} open seat(s) but is offered to {}", r.id, to.len()));
        }
        let members: Vec<&Agent> = r.active_slots().filter_map(|x| s.agents.get(&x.agent)).collect();
        if !to.is_empty() && !any_team(r, &members.iter().copied().chain(to.iter().copied()).collect::<Vec<_>>(), &[]) {
            return Some(format!("{tag}: {} is offered to {:?}, who can't make a team it can start with", r.id, to.iter().map(|a| &a.name).collect::<Vec<_>>()));
        }
        let older = |f: &Agent| s.reviews.iter().any(|o| o.phase == Phase::Offering && o.seq() < r.seq() && can_join(o, f) && fillable(o, f, free));
        let open_to = |f: &Agent| held.get(&f.name).is_none_or(|o| seq(o) > r.seq()) && !older(f);
        let seats = |a: &Agent, among: &[&Agent]| among.iter().filter(|x| x.login.eq_ignore_ascii_case(&a.login)).count();
        let both: Vec<&Agent> = members.iter().chain(to.iter()).copied().collect();
        let ahead = |f: &Agent, a: &Agent| {
            let (sf, sa) = (seats(f, &both), seats(a, &members));
            (lapsed(f), sf, t(f)) < (lapsed(a), sa, t(a)) && (trimmed(f), sf, t(f)) < (trimmed(a), sa, t(a))
        };
        for a in to.iter().filter(|a| pre.offered(&a.name) != Some(r.id.as_str())) {
            let ahead = free.iter().find(|f| {
                f.model == a.model
                    && held.get(&f.name) != Some(&r.id)
                    && open_to(f)
                    && can_join(r, f)
                    && fillable(r, f, free)
                    && ahead(f, a)
            });
            if let Some(f) = ahead {
                return Some(format!("{tag}: {} is offered {} ahead of {}, whose turn came earlier", a.name, r.id, f.name));
            }
        }
    }
    None
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

/// R6's memory along one run: the order people (by login and program) and their agents last had
/// their turn in, as the steps show it (not the relay's own count), and how many reviews in a row
/// each reviewer was passed over for.
#[derive(Clone, Default)]
pub struct Passed {
    people: BTreeMap<(String, Option<Model>), u64>,
    turns: BTreeMap<String, u64>,
    next: u64,
    times: BTreeMap<String, usize>,
}

impl Passed {
    /// R6: a free reviewer that hasn't let an offer lapse is passed over, never offered a review
    /// that starts with a reviewer of its program who accepted it and had its turn later, at most
    /// `MAX_PASSED` times in a row. Turns go by person first, then by agent. A reviewer and its
    /// person have their turn when it starts a review, declines an offer it has or lets one lapse,
    /// and it starts counting over; a new agent, and a new person, start at the back; turns taken
    /// in one step tie. R8: R5's turn order holds by these turns too, so the relay's own count
    /// can't hide a missed turn.
    pub fn check(&mut self, pre: &State, s: &State, now: Ms) -> Result<(), String> {
        const MAX_PASSED: usize = 2;
        let person = |a: &Agent| (a.login.to_ascii_lowercase(), a.model);
        self.next += 1;
        for a in s.agents.all() {
            if !self.people.contains_key(&person(a)) {
                self.people.insert(person(a), self.next);
            }
            if pre.agents.get(&a.name).is_none() || !self.turns.contains_key(&a.name) {
                self.turns.insert(a.name.clone(), self.next);
            }
        }
        let mut took: Vec<String> = Vec::new();
        let lapsed = |a: &Agent| s.reviews.iter().chain(&pre.reviews).any(|o| o.lapsed.contains(&a.name));
        let pre_free = free(pre, now);
        for r in &s.reviews {
            let p = pre.review(&r.id);
            for e in r.log.iter().skip(p.map_or(0, |p| p.log.len())) {
                if e.kind == "lapse" || (e.kind == "decline" && pre.offered(&e.who) == Some(r.id.as_str())) {
                    took.push(e.who.clone());
                }
                if e.kind == "phase" && e.text.starts_with("independent review started with ") {
                    took.extend(e.text["independent review started with ".len()..].split(", ").map(String::from));
                }
            }
            if p.is_none_or(|p| p.phase != Phase::Offering || r.phase != Phase::Reviewing) {
                continue;
            }
            let turn = |a: &Agent| (self.people.get(&person(a)).copied().unwrap_or(u64::MAX), self.turns.get(&a.name).copied().unwrap_or(u64::MAX));
            let accepted: Vec<&Agent> = r
                .active_slots()
                .filter(|x| r.log.iter().any(|e| e.kind == "accept" && e.who == x.agent && !e.text.starts_with("assigned again")))
                .filter_map(|x| pre.agents.get(&x.agent))
                .collect();
            for f in pre_free.iter().filter(|f| s.busy_with(&f.name).is_none() && s.agents.online(&f.name, now) && !lapsed(f)) {
                let older = offered(pre, &f.name, now).first().and_then(|o| pre.review(o)).is_some_and(|o| o.seq() < r.seq())
                    || pre.reviews.iter().any(|o| o.phase == Phase::Offering && o.seq() < r.seq() && can_join(o, f) && fillable(o, f, &pre_free));
                let allowed = f.name != r.author
                    && (r.picked.is_empty() || r.picked.contains(&f.name))
                    && !r.declined.contains(&f.name)
                    && !r.offered.contains(&f.name);
                if !older && allowed && accepted.iter().any(|a| a.model == f.model && turn(a) > turn(f)) {
                    let n = self.times.entry(f.name.clone()).or_default();
                    *n += 1;
                    if *n > MAX_PASSED {
                        return Err(format!("R6: {} was passed over {n} times in a row, last for {}", f.name, r.id));
                    }
                }
            }
        }
        self.next += 1;
        for name in took {
            if let Some(a) = s.agents.get(&name) {
                self.people.insert(person(a), self.next);
            }
            self.turns.insert(name.clone(), self.next);
            self.times.remove(&name);
        }
        let held: BTreeMap<String, String> = s.agents.all().filter_map(|a| Some((a.name.clone(), offered(s, &a.name, now).into_iter().next()?))).collect();
        let turn = |a: &Agent| (self.people.get(&person(a)).copied().unwrap_or(u64::MAX), self.turns.get(&a.name).copied().unwrap_or(u64::MAX));
        match rotation_check(pre, s, now, &free(s, now), &held, &turn, "R8") {
            Some(why) => Err(why),
            None => Ok(()),
        }
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

/// Count members, or for three or more at least two; both programs when two or more.
fn valid(r: &Review, team: &[(&str, Option<Model>)]) -> bool {
    let c = r.params.reviewers;
    let n = team.len();
    if !(n == c || (c >= 3 && (2..c).contains(&n))) {
        return false;
    }
    c < 2 || Model::ALL.iter().all(|m| team.iter().any(|(_, x)| *x == Some(*m)))
}

fn valid_team(s: &State, r: &Review, team: &[&str]) -> bool {
    let members: Vec<(&str, Option<Model>)> = team.iter().map(|n| (s.agents.get(n).unwrap().login.as_str(), r.slot(n).unwrap().model)).collect();
    valid(r, &members)
}

/// L6, as the model words it: an older waiting review could have completed its forming team
/// with these members plus the reviewers free for it, and can't without them. A reviewer is
/// free for a review when online, allowed, on no running review, and not spoken for (offered or
/// kept in its turns) by an even older review that can start. An older review still in its first
/// 30s of offers needs only those it offers, which F3 covers. (The model lets the older review
/// swap out its own forming members too; the relay never does.)
fn needed_by_older(pre: &State, started: &Review, team: &[&str], now: Ms) -> Option<String> {
    let reviewers: Vec<&Agent> = pre.agents.all().filter(|a| a.role == Role::Reviewer && a.model.is_some()).collect();
    let allowed = |r: &Review, a: &Agent| (r.picked.is_empty() || r.picked.contains(&a.name)) && !r.declined.contains(&a.name) && !r.lapsed.contains(&a.name) && r.slot(&a.name).is_none_or(|x| x.active());
    let running = |a: &Agent| pre.reviews.iter().any(|r| matches!(r.phase, Phase::Reviewing | Phase::Voting(_)) && r.slot(&a.name).is_some_and(|x| x.active()));
    let can_start = |r: &Review| {
        let pool: Vec<&Agent> = reviewers.iter().copied().filter(|a| (r.slot(&a.name).is_some_and(|x| x.active())) || (pre.agents.online(&a.name, now) && !running(a) && allowed(r, a))).collect();
        any_team(r, &pool, &[])
    };
    let held: BTreeMap<String, String> = pre.agents.all().filter_map(|a| Some((a.name.clone(), offered(pre, &a.name, now).into_iter().next()?))).collect();
    let (_, _, kept) = wanted(pre, free(pre, now), &held, now);
    let target = |a: &Agent| -> Option<String> {
        let forming = pre.reviews.iter().find(|r| r.phase == Phase::Offering && r.slot(&a.name).is_some_and(|x| x.active()));
        forming.map(|r| r.id.clone()).or_else(|| held.get(&a.name).cloned()).or_else(|| kept.get(a.name.as_str()).cloned())
    };
    let seq = |id: &str| pre.review(id).map_or(u64::MAX, Review::seq);
    for older in pre.reviews.iter().filter(|o| o.phase == Phase::Offering && o.seq() < started.seq() && !rotating(pre, o, now)) {
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
