use crate::config::{Consensus, Timing};
use crate::prurl::PrRef;
use crate::tailscale::Identity;
use crate::text;
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

pub type Ms = u64;

pub fn now_ms() -> Ms {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as Ms)
        .unwrap_or(0)
}

const MAX_EVENTS: usize = 200;
const MAX_EVENT_BYTES: usize = 128 * 1024;
/// Event text one `wait` returns; the rest stays queued for the next call.
const MAX_WAIT_BYTES: usize = 64 * 1024;
/// Characters in each text an agent sends (see `bounded`); longer ones are refused.
const MAX_NOTE: usize = 8000;
/// GitHub's own limit.
const MAX_TITLE: usize = 256;
const MAX_SAY: usize = 4000;
const MAX_FINDING: usize = 2000;
const MAX_REASON: usize = 1000;
const MAX_URL: usize = 400;
/// Characters in one reviewer's findings, their links and its summary together, or in its vote reasons in a round.
const MAX_REVIEWER_TEXT: usize = 16_000;
/// Says, amendments and changed votes, per agent and review.
const SAYS_PER_REVIEW: usize = 50;
const SAYS_PER_MINUTE: usize = 10;
const TALK: &[&str] = &["say", "amend", "revote"];
const CLOSING_REMARKS: usize = 3;
const AMENDS_PER_FINDING: u32 = 3;
/// Offers listed in full in an idle reviewer's next actions.
const OFFERS_SHOWN: usize = 2;
/// Reviews that failed before anyone took them, kept on top of `history`.
const FAILED_KEPT: usize = 10;
/// Unfinished reviews one Tailscale login may have requested at a time.
const REVIEWS_PER_LOGIN: usize = 5;
const AGENTS_PER_LOGIN: usize = 20;
const MAX_AGENTS: usize = 200;
const MAX_FINDINGS: usize = 60;
const FORGET_AGENT_MS: Ms = 24 * 3600 * 1000;
const NO_MODEL: &str = "call `hello` with role \"reviewer\" and model \"claude\" or \"codex\" (the agent program you run in) before you can take reviews.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocking,
    Major,
    Minor,
    Nit,
}

impl Severity {
    pub fn parse(s: &str) -> Option<Severity> {
        match s.trim().to_ascii_lowercase().as_str() {
            "blocking" | "blocker" | "critical" => Some(Severity::Blocking),
            "major" | "high" => Some(Severity::Major),
            "minor" | "medium" | "low" => Some(Severity::Minor),
            "nit" | "nitpick" => Some(Severity::Nit),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Blocking => "blocking",
            Severity::Major => "major",
            Severity::Minor => "minor",
            Severity::Nit => "nit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    ChangesRequested,
}

impl Verdict {
    pub fn parse(s: &str) -> Option<Verdict> {
        match s.trim().to_ascii_lowercase().replace([' ', '-'], "_").as_str() {
            "approve" | "approved" => Some(Verdict::Approve),
            "changes_requested" | "request_changes" | "changes" => Some(Verdict::ChangesRequested),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Approve => "approve",
            Verdict::ChangesRequested => "changes_requested",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Reviewer,
    Author,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        match s.trim().to_ascii_lowercase().as_str() {
            "reviewer" => Some(Role::Reviewer),
            "author" => Some(Role::Author),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Reviewer => "reviewer",
            Role::Author => "author",
        }
    }
}

/// The agent program a reviewer says it runs in; self-reported, so it can't be checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Model {
    Claude,
    Codex,
}

impl Model {
    pub const ALL: [Model; 2] = [Model::Claude, Model::Codex];

    pub fn parse(s: &str) -> Option<Model> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Model::Claude),
            "codex" => Some(Model::Codex),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Model::Claude => "claude",
            Model::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentEvent {
    pub seq: u64,
    pub at: Ms,
    pub review: Option<String>,
    pub text: String,
    /// The obligation this event hands out (see `Action::task`); dropped once it is met.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Needs no action; `wait` skips it unless asked for progress.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub progress: bool,
    /// A review's result or failure notice for its author.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub result: bool,
    /// The deadline `text::LEFT` in `text` stands for; rendered as the time left at delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due: Option<Ms>,
}

impl AgentEvent {
    fn new(review: &str, text: String, now: Ms) -> AgentEvent {
        AgentEvent {
            seq: 0,
            at: now,
            review: Some(review.to_string()),
            text,
            task: None,
            progress: false,
            result: false,
            due: None,
        }
    }

    pub fn rendered(mut self, now: Ms) -> AgentEvent {
        if let Some(due) = self.due {
            self.text = self.text.replace(text::LEFT, &text::dur(due.saturating_sub(now)));
        }
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    pub login: String,
    #[serde(default)]
    pub user_id: u64,
    pub node: String,
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    pub first_seen: Ms,
    pub last_seen: Ms,
    /// Requests blocked in `wait` for this agent.
    #[serde(skip)]
    pub waiting: u32,
    events: VecDeque<AgentEvent>,
    next_seq: u64,
    /// Events up to here were received: handed out outside a session, or acknowledged by it.
    delivered: u64,
}

#[derive(Clone)]
pub struct Agents {
    map: BTreeMap<String, Agent>,
    /// Open session event streams per name, which count as activity like a `wait`.
    streams: BTreeMap<String, u32>,
    timing: Timing,
}

impl Agents {
    pub fn get(&self, name: &str) -> Option<&Agent> {
        self.map.get(name)
    }

    pub fn all(&self) -> impl Iterator<Item = &Agent> {
        self.map.values()
    }

    pub fn silent_ms(&self, name: &str, now: Ms) -> Ms {
        match self.map.get(name) {
            Some(a) if a.waiting > 0 || self.streams.contains_key(name) => 0,
            Some(a) => now.saturating_sub(a.last_seen),
            None => Ms::MAX,
        }
    }

    pub fn online(&self, name: &str, now: Ms) -> bool {
        self.silent_ms(name, now) < self.timing.online_secs * 1000
    }

    pub fn stream(&mut self, name: &str, delta: i32, now: Ms) {
        let n = self.streams.entry(name.to_string()).or_default();
        *n = n.saturating_add_signed(delta);
        if *n == 0 {
            self.streams.remove(name);
        }
        if let Some(a) = self.map.get_mut(name) {
            a.last_seen = now;
        }
    }

    fn notify(&mut self, name: &str, review: &str, text: String, now: Ms) {
        self.push(name, AgentEvent::new(review, text, now));
    }

    fn notify_task(&mut self, name: &str, review: &str, task: String, text: String, due: Option<Ms>, now: Ms) {
        self.push(name, AgentEvent { task: Some(task), due, ..AgentEvent::new(review, text, now) });
    }

    fn progress(&mut self, name: &str, review: &str, text: String, now: Ms) {
        self.push(name, AgentEvent { progress: true, ..AgentEvent::new(review, text, now) });
    }

    fn progress_due(&mut self, name: &str, review: &str, text: String, due: Ms, now: Ms) {
        self.push(name, AgentEvent { progress: true, due: Some(due), ..AgentEvent::new(review, text, now) });
    }

    fn outcome(&mut self, name: &str, review: &str, text: String, now: Ms) {
        self.push(name, AgentEvent { result: true, ..AgentEvent::new(review, text, now) });
    }

    /// Queues `ev`; over the caps the oldest events go first. Unread results never do, and a
    /// task that goes is still shown by `next_actions`.
    fn push(&mut self, name: &str, mut ev: AgentEvent) {
        let Some(a) = self.map.get_mut(name) else { return };
        a.next_seq += 1;
        ev.seq = a.next_seq;
        a.events.push_back(ev);
        let delivered = a.delivered;
        let kept = |e: &AgentEvent| e.result && e.seq > delivered;
        let mut bytes: usize = a.events.iter().filter(|e| !kept(e)).map(|e| e.text.len()).sum();
        while a.events.len() > MAX_EVENTS || bytes > MAX_EVENT_BYTES {
            match a.events.iter().position(|e| !kept(e)).filter(|&i| i + 1 < a.events.len()) {
                Some(i) => bytes -= a.events.remove(i).map_or(0, |e| e.text.len()),
                None => break,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "phase", content = "round")]
pub enum Phase {
    Offering,
    Reviewing,
    Voting(u8),
    Done,
    Failed,
}

impl Phase {
    pub fn active(self) -> bool {
        !matches!(self, Phase::Done | Phase::Failed)
    }

    pub fn label(self) -> String {
        match self {
            Phase::Offering => "offering".into(),
            Phase::Reviewing => "independent review".into(),
            Phase::Voting(r) => format!("cross-examination round {r}"),
            Phase::Done => "done".into(),
            Phase::Failed => "failed".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReviewParams {
    pub reviewers: usize,
    pub consensus: Consensus,
    pub offer_secs: u64,
    pub review_secs: u64,
    pub vote_secs: u64,
    #[serde(default = "default_reply_secs")]
    pub reply_secs: u64,
}

fn default_reply_secs() -> u64 {
    Timing::default().reply_secs
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Submission {
    pub at: Ms,
    pub head_sha: String,
    pub verdict: Verdict,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slot {
    pub agent: String,
    pub login: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    pub joined: Ms,
    pub dropped: Option<String>,
    pub submission: Option<Submission>,
}

impl Slot {
    pub fn active(&self) -> bool {
        self.dropped.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Choice {
    Agree,
    Disagree,
    Duplicate(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vote {
    pub choice: Choice,
    /// On an agree vote: the severity the voter thinks is fair, when it differs.
    pub severity: Option<Severity>,
    pub reason: Option<String>,
    pub round: u8,
    pub revision: u32,
    pub at: Ms,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Confirmed,
    Rejected,
    Disputed,
    Duplicate(String),
    Withdrawn,
    /// Nobody else voted on its current revision, as when it had only one reviewer.
    Unverified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub status: Status,
    pub severity: Severity,
    pub downgraded: bool,
    /// Its only two supporters rate it differently, so it takes the lower severity.
    pub severity_split: bool,
    pub agree: usize,
    pub disagree: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    /// `None` for findings carried over from the previous review into a re-review.
    pub by: Option<String>,
    pub severity: Severity,
    pub url: Option<String>,
    pub text: String,
    pub origin: Option<String>,
    pub withdrawn: Option<String>,
    pub revision: u32,
    pub open_round: u8,
    pub votes: BTreeMap<String, Vote>,
    pub outcome: Option<Outcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruling: Option<Ruling>,
}

/// A human's decision on a finding the votes left open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ruling {
    pub by: String,
    pub must_fix: bool,
    pub at: Ms,
}

impl Finding {
    /// Its ruling, which counts only while the votes leave it disputed or unverified.
    pub fn ruled(&self) -> Option<&Ruling> {
        let open = self.outcome.as_ref().is_some_and(|o| matches!(o.status, Status::Disputed | Status::Unverified));
        self.ruling.as_ref().filter(|_| open)
    }

    /// Votes cast on its current revision; an amendment needs fresh ones.
    pub fn current_votes(&self) -> impl Iterator<Item = (&String, &Vote)> {
        self.votes.iter().filter(|(_, v)| v.revision >= self.revision)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub at: Ms,
    pub who: String,
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewResult {
    pub verdict: Verdict,
    pub at: Ms,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub id: String,
    pub pr: PrRef,
    pub author: String,
    pub author_login: String,
    pub note: String,
    /// The PR's title as the author agent gave it, for people only: never in agents' text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub params: ReviewParams,
    pub created: Ms,
    pub ended: Option<Ms>,
    pub phase: Phase,
    pub deadline: Ms,
    pub offered: BTreeSet<String>,
    pub declined: BTreeSet<String>,
    /// The reviewers the author asked for; empty offers it to every idle reviewer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub picked: Vec<String>,
    pub slots: Vec<Slot>,
    pub findings: Vec<Finding>,
    pub log: Vec<LogEntry>,
    pub result: Option<ReviewResult>,
    pub failure: Option<String>,
    pub parent: Option<String>,
    pub iteration: u32,
    pub since_sha: Option<String>,
    pub head_sha: Option<String>,
    next_finding: u32,
    /// In a later voting round: the authors of contested findings may still reply before votes open.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replying: bool,
    /// Who has amended, withdrawn or spoken since the current round started.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub replied: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_by: Option<String>,
    /// The program the review failed for want of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<Model>,
}

impl Review {
    pub fn slot(&self, agent: &str) -> Option<&Slot> {
        self.slots.iter().find(|s| s.agent == agent)
    }

    fn slot_mut(&mut self, agent: &str) -> Option<&mut Slot> {
        self.slots.iter_mut().find(|s| s.agent == agent)
    }

    pub fn active_slots(&self) -> impl Iterator<Item = &Slot> {
        self.slots.iter().filter(|s| s.active())
    }

    pub fn finding(&self, id: &str) -> Option<&Finding> {
        self.findings.iter().find(|f| f.id.eq_ignore_ascii_case(id))
    }

    pub fn revealed(&self) -> bool {
        matches!(self.phase, Phase::Voting(_) | Phase::Done)
            || (self.phase == Phase::Failed && !self.findings.is_empty())
    }

    pub fn participant(&self, agent: &str) -> bool {
        self.author == agent || self.slot(agent).is_some_and(Slot::active)
    }

    pub fn round(&self) -> u8 {
        match self.phase {
            Phase::Voting(r) => r,
            _ => 0,
        }
    }

    /// Findings `agent` still has to vote on in the current round.
    pub fn owed_votes(&self, agent: &str) -> Vec<&Finding> {
        let Phase::Voting(round) = self.phase else {
            return Vec::new();
        };
        if !self.slot(agent).is_some_and(Slot::active) {
            return Vec::new();
        }
        self.findings
            .iter()
            .filter(|f| f.withdrawn.is_none() && f.by.as_deref() != Some(agent) && f.open_round == round)
            .filter(|f| f.votes.get(agent).is_none_or(|v| v.round < round))
            .collect()
    }

    /// `agent`'s own findings reopened in the current round.
    pub fn contested_by(&self, agent: &str) -> Vec<String> {
        let round = self.round();
        self.findings
            .iter()
            .filter(|f| f.by.as_deref() == Some(agent) && f.withdrawn.is_none() && f.open_round == round && round > 1)
            .map(|f| f.id.clone())
            .collect()
    }

    /// Whether `login` may cancel or rule on it: its author's login, or the hub owner (`owner`).
    pub fn may_act(&self, login: &str, owner: bool) -> bool {
        owner || self.author_login.eq_ignore_ascii_case(login)
    }

    /// The verdict of a finished review, from its findings and rulings as they stand now: a
    /// stored one may predate a ruling, or a relay that dropped it.
    pub fn verdict(&self) -> Option<Verdict> {
        self.result.as_ref().map(|_| verdict_of(&self.findings))
    }

    /// Its result as `verdict` and the findings now give it.
    pub fn result_now(&self, web: &str) -> Option<ReviewResult> {
        let at = self.result.as_ref()?.at;
        Some(ReviewResult { verdict: verdict_of(&self.findings), at, text: text::result(self, web) })
    }

    /// Its place in the order reviews were requested.
    pub fn seq(&self) -> u64 {
        self.id.trim_start_matches('r').parse().unwrap_or(0)
    }

    pub fn deadline_left(&self, now: Ms) -> Ms {
        self.deadline.saturating_sub(now)
    }

    /// Whether `login` already holds a slot. Other people take one slot each; the author's own
    /// agents may take any number.
    fn login_taken(&self, login: &str) -> bool {
        !self.author_login.eq_ignore_ascii_case(login) && self.active_slots().any(|s| s.login.eq_ignore_ascii_case(login))
    }

    /// The program a review of two or more reviewers has none of yet; it needs both.
    pub fn missing_model(&self) -> Option<Model> {
        if self.params.reviewers < 2 {
            return None;
        }
        Model::ALL.into_iter().find(|m| !self.active_slots().any(|s| s.model == Some(*m)))
    }

    /// Whether a reviewer running `model` may take a slot: none may without one, and the last slot
    /// is kept for the program the review still lacks.
    pub fn fits(&self, model: Option<Model>) -> bool {
        model.is_some_and(|m| self.lacks(m).is_none())
    }

    /// The program the review would be left without if a `model` reviewer took the next slot.
    fn lacks(&self, model: Model) -> Option<Model> {
        let open = self.params.reviewers.saturating_sub(self.active_slots().count() + 1);
        self.missing_model().filter(|m| *m != model && open == 0)
    }

    /// Refuses `agent` if its name now belongs to a different login than the one that took part.
    fn check_login(&self, agent: &str, login: &str) -> Result<()> {
        let bound = if self.author == agent {
            Some(&self.author_login)
        } else {
            self.slot(agent).map(|s| &s.login)
        };
        if bound.is_some_and(|l| !l.eq_ignore_ascii_case(login)) {
            bail!("`{agent}` on {} was another tailnet user's agent; use your own agent name", self.id);
        }
        Ok(())
    }

    fn log(&mut self, at: Ms, who: &str, kind: &'static str, text: impl Into<String>) {
        self.log.push(LogEntry {
            at,
            who: who.to_string(),
            kind: kind.to_string(),
            text: text.into(),
        });
    }

    fn others(&self, except: &str) -> Vec<String> {
        let mut v: Vec<String> = self
            .active_slots()
            .map(|s| s.agent.clone())
            .filter(|a| a != except)
            .collect();
        if self.author != except {
            v.push(self.author.clone());
        }
        v
    }
}

/// Merges duplicates, then applies the voting rule to every finding. Only a finding someone other than its author
/// backs as it stands can be confirmed.
pub fn decide(findings: &mut [Finding], c: &Consensus) {
    let ids: Vec<String> = findings.iter().map(|f| f.id.clone()).collect();
    let rating: BTreeMap<String, Severity> = findings.iter().map(|f| (f.id.clone(), f.severity)).collect();
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    for f in findings.iter() {
        if f.withdrawn.is_some() {
            continue;
        }
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, v) in f.current_votes() {
            if let Choice::Duplicate(t) = &v.choice {
                *counts.entry(t.as_str()).or_default() += 1;
            }
        }
        let Some((target, n)) = counts.into_iter().max_by_key(|(_, n)| *n) else {
            continue;
        };
        if n * 2 <= f.current_votes().count() {
            continue;
        }
        let mut t = target.to_string();
        let mut seen = HashSet::new();
        while let Some(next) = merged.get(&t) {
            if !seen.insert(t.clone()) {
                break;
            }
            t = next.clone();
        }
        let live = findings.iter().any(|g| g.id == t && g.withdrawn.is_none());
        if t != f.id && live && ids.contains(&t) {
            merged.insert(f.id.clone(), t);
        }
    }
    for f in findings.iter_mut() {
        if f.withdrawn.is_some() {
            f.outcome = Some(Outcome {
                status: Status::Withdrawn,
                severity: f.severity,
                downgraded: false,
                severity_split: false,
                agree: 0,
                disagree: 0,
            });
            continue;
        }
        let mut rated: Vec<Severity> = f.by.iter().map(|_| f.severity).collect();
        let mut disagree = 0;
        for (_, v) in f.current_votes() {
            match &v.choice {
                Choice::Disagree => disagree += 1,
                Choice::Agree => rated.push(v.severity.unwrap_or(f.severity)),
                Choice::Duplicate(t) => rated.push(rating.get(t).copied().unwrap_or(f.severity)),
            }
        }
        let agree = rated.len();
        let backed = f.current_votes().any(|(_, v)| v.choice != Choice::Disagree);
        let total = (agree + disagree) as f64;
        let mut severity = f.severity;
        let mut downgraded = false;
        rated.sort();
        let severity_split = disagree == 0 && agree == 2 && rated[0] != rated[1];
        let status = if let Some(t) = merged.get(&f.id) {
            Status::Duplicate(t.clone())
        } else if backed && agree as f64 > c.confirm_fraction * total {
            severity = rated[agree / 2];
            let unanimous = disagree == 0 && rated.iter().all(|s| *s == Severity::Blocking);
            if severity == Severity::Blocking && c.blocking_unanimous && !unanimous {
                severity = Severity::Major;
                downgraded = true;
            }
            Status::Confirmed
        } else if disagree as f64 > c.confirm_fraction * total {
            Status::Rejected
        } else if disagree == 0 {
            Status::Unverified
        } else {
            Status::Disputed
        };
        f.outcome = Some(Outcome {
            status,
            severity,
            downgraded,
            severity_split,
            agree,
            disagree,
        });
    }
}

/// The re-review of review `id` that hasn't failed, which locks its rulings.
pub fn rereview<'a>(reviews: &'a [Review], id: &str) -> Option<&'a Review> {
    reviews.iter().find(|o| o.parent.as_deref() == Some(id) && o.phase != Phase::Failed)
}

/// Changes are requested by a confirmed blocking or major finding, or one a human ruled must fix.
pub fn verdict_of(findings: &[Finding]) -> Verdict {
    let blocks = findings.iter().any(|f| {
        f.ruled().is_some_and(|r| r.must_fix)
            || f.outcome.as_ref().is_some_and(|o| {
                o.status == Status::Confirmed && matches!(o.severity, Severity::Blocking | Severity::Major)
            })
    });
    if blocks { Verdict::ChangesRequested } else { Verdict::Approve }
}

pub struct NewReview {
    pub pr_url: String,
    pub note: String,
    pub title: Option<String>,
    pub head_sha: Option<String>,
    pub reviewers: Option<usize>,
    /// Offer only to these agents; `reviewers` then defaults to how many there are.
    pub picked: Vec<String>,
}

impl NewReview {
    pub fn new(pr_url: &str) -> NewReview {
        NewReview {
            pr_url: pr_url.into(),
            note: String::new(),
            title: None,
            head_sha: None,
            reviewers: None,
            picked: Vec::new(),
        }
    }
}

pub struct SubmitIn {
    pub head_sha: String,
    pub verdict: String,
    pub summary: String,
    pub findings: Vec<FindingIn>,
}

#[derive(Default)]
pub struct AmendIn {
    pub severity: Option<String>,
    pub url: Option<String>,
    pub text: Option<String>,
}

pub struct FindingIn {
    pub severity: String,
    pub url: Option<String>,
    pub text: String,
}

pub struct VoteIn {
    pub finding: String,
    pub vote: String,
    pub severity: Option<String>,
    pub reason: Option<String>,
    pub duplicate_of: Option<String>,
}

pub struct Action {
    pub review: String,
    pub urgent: bool,
    pub text: String,
    /// Identifies the obligation, so an event that already carries its text can be pointed to.
    pub task: Option<String>,
    /// One line to use instead of `text` when such an event is delivered with it.
    pub short: Option<String>,
}

impl Action {
    fn info(review: &str, text: String) -> Action {
        Action {
            review: review.to_string(),
            urgent: false,
            text,
            task: None,
            short: None,
        }
    }
}

pub fn offer_task(review: &str) -> String {
    format!("{review}:offer")
}

pub fn reply_task(r: &Review) -> String {
    format!("{}:reply:{}", r.id, r.round())
}

pub fn review_task(review: &str) -> String {
    format!("{review}:review")
}

/// Changes whenever the set of owed findings or their revisions change.
pub fn vote_task(r: &Review, owed: &[&Finding]) -> String {
    let ids: Vec<String> = owed.iter().map(|f| format!("{}.{}", f.id, f.revision)).collect();
    format!("{}:vote:{}:{}", r.id, r.round(), ids.join(","))
}

#[derive(Debug)]
/// What a `vote` call recorded, and the votes it skipped because their finding was withdrawn.
pub struct Voted {
    pub recorded: usize,
    pub ignored: Vec<String>,
}

pub struct OnlineAgent {
    pub name: String,
    pub login: String,
    pub node: String,
    pub role: Role,
    pub model: Option<Model>,
    pub busy: Option<String>,
    pub idle_ms: Ms,
}

type Fingerprint = (Vec<(String, bool)>, usize, Vec<(String, Phase, usize)>);

pub const SNAPSHOT_VERSION: u32 = 1;

/// What `--state` keeps across restarts: reviews, agents' bindings and undelivered events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub saved_at: Ms,
    next_review: u64,
    agents: Vec<Agent>,
    pub reviews: Vec<Review>,
}

#[derive(Clone)]
pub struct State {
    pub agents: Agents,
    pub reviews: Vec<Review>,
    pub timing: Timing,
    pub consensus: Consensus,
    pub history: usize,
    /// Base URL of the web page, for links in results.
    pub web_url: String,
    next_review: u64,
}

fn valid_sha(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// A finding's line link: printable ASCII under the PR's repository, nothing an HTML attribute or a terminal would act on.
fn check_url(pr: &PrRef, url: &str) -> Result<String> {
    let url = bounded(url, "finding url", MAX_URL)?;
    let prefix = format!("https://github.com/{}/{}/blob/", pr.owner, pr.repo);
    let rest = url.get(prefix.len()..).filter(|_| url[..prefix.len()].eq_ignore_ascii_case(&prefix));
    let ok = rest.and_then(|rest| {
        let (path, lines) = match rest.split_once('#') {
            Some((path, lines)) => (path, Some(lines)),
            None => (rest, None),
        };
        let path = path.strip_suffix("?plain=1").unwrap_or(path);
        let (sha, path) = path.split_once('/')?;
        let line = |n: &str| (1..=7).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit());
        let lines_ok = match lines.map(|l| l.strip_prefix('L')) {
            None => true,
            Some(Some(l)) => l.split_once("-L").map_or(line(l), |(a, b)| line(a) && line(b)),
            Some(None) => false,
        };
        (valid_sha(sha) && path.split('/').all(path_part) && lines_ok).then_some(())
    });
    if ok.is_none() {
        bail!("finding url must be a line link into the PR's repository: {prefix}<head_sha>/<path>, optionally with ?plain=1 and #L<line> or #L<from>-L<to>; the path may use letters, digits, %XX escapes and . _ - + @ [ ] ( ) ~ , = !");
    }
    Ok(url)
}

/// One segment of a file path in a line link: letters (any script), digits, `%XX` escapes and the
/// punctuation real paths use; no whitespace, `:`, quotes, angle brackets, nor anything that is or
/// decodes to `.`, `..`, a slash, a backslash or a control character.
fn path_part(p: &str) -> bool {
    let mut decoded = Vec::new();
    let mut cs = p.chars();
    while let Some(c) = cs.next() {
        let byte = match c {
            '%' => cs.next().zip(cs.next()).and_then(|(a, b)| Some((a.to_digit(16)? * 16 + b.to_digit(16)?) as u8)),
            c if c.is_ascii() => (c.is_ascii_alphanumeric() || "._-+@[]()~,=!".contains(c)).then_some(c as u8),
            c if c.is_alphanumeric() => Some(b'a'),
            _ => None,
        };
        match byte {
            Some(b) if !b.is_ascii_control() && b != b'/' && b != b'\\' => decoded.push(b),
            _ => return false,
        }
    }
    !matches!(decoded.as_slice(), [] | b"." | b"..")
}

fn need_text(s: &str, what: &str) -> Result<String> {
    let t = bounded(s, what, MAX_FINDING)?;
    if t.is_empty() {
        bail!("{what} must not be empty");
    }
    Ok(t)
}

/// Every text an agent sends goes through here: trimmed, and refused if longer than `max` characters.
fn bounded(s: &str, what: &str, max: usize) -> Result<String> {
    let s = s.trim();
    let n = s.chars().count();
    if n > max {
        bail!("{what} is {n} characters; at most {max} are allowed, so be more concise");
    }
    Ok(s.to_string())
}

/// One line of at most `MAX_TITLE` characters, without control or bidi characters; `None` if empty.
fn one_line(s: &str) -> Option<String> {
    let s: String = s.chars().filter(|&c| !text::bidi(c)).map(|c| if c.is_control() { ' ' } else { c }).collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s: String = s.chars().take(MAX_TITLE).collect();
    (!s.is_empty()).then(|| s.trim_end().to_string())
}

fn finding_size(text: &str, url: Option<&str>) -> usize {
    text.chars().count() + url.map_or(0, |u| u.chars().count())
}

/// What `agent` has put in front of the other reviewers of `r`: its live findings with their links, and its summary.
fn reviewer_text(r: &Review, agent: &str) -> usize {
    let findings = r.findings.iter().filter(|f| f.by.as_deref() == Some(agent) && f.withdrawn.is_none());
    let summary = r.slot(agent).and_then(|s| s.submission.as_ref()).map_or(0, |s| s.summary.chars().count());
    findings.map(|f| finding_size(&f.text, f.url.as_deref())).sum::<usize>() + summary
}

fn check_reviewer_text(n: usize) -> Result<()> {
    if n > MAX_REVIEWER_TEXT {
        bail!("your findings and summary would be {n} characters; at most {MAX_REVIEWER_TEXT} in all, so merge or shorten them");
    }
    Ok(())
}

/// Says, amendments and changed votes share one allowance per agent and review.
fn check_talk(r: &Review, agent: &str, n: usize, now: Ms) -> Result<()> {
    if n == 0 {
        return Ok(());
    }
    let mine: Vec<&LogEntry> = r.log.iter().filter(|e| e.who == agent && TALK.contains(&e.kind.as_str())).collect();
    if mine.len() + n > SAYS_PER_REVIEW {
        bail!("you have had enough discussion in {}; vote, or call `wait`", r.id);
    }
    if mine.iter().filter(|e| now.saturating_sub(e.at) < 60_000).count() + n > SAYS_PER_MINUTE {
        bail!("at most {SAYS_PER_MINUTE} messages a minute; call `wait` and let the others answer");
    }
    Ok(())
}

impl State {
    pub fn new(timing: Timing, consensus: Consensus, history: usize) -> State {
        State {
            agents: Agents {
                map: BTreeMap::new(),
                streams: BTreeMap::new(),
                timing,
            },
            reviews: Vec::new(),
            timing,
            consensus,
            history,
            web_url: String::new(),
            next_review: 0,
        }
    }

    pub fn snapshot(&self, now: Ms) -> Snapshot {
        let agents = self
            .agents
            .all()
            .map(|a| {
                let mut a = a.clone();
                if a.waiting > 0 || self.agents.streams.contains_key(&a.name) {
                    a.last_seen = now;
                }
                a
            })
            .collect();
        Snapshot {
            version: SNAPSHOT_VERSION,
            saved_at: now,
            next_review: self.next_review,
            agents,
            reviews: self.reviews.clone(),
        }
    }

    /// Loads a snapshot as if the relay's clock had stopped while it was down: unfinished
    /// deadlines and agents' silence resume where they were.
    pub fn restore(&mut self, snap: Snapshot, now: Ms) {
        let down = now.saturating_sub(snap.saved_at);
        self.next_review = snap.next_review;
        self.agents.map = snap
            .agents
            .into_iter()
            .map(|mut a| {
                a.last_seen = (a.last_seen + down).min(now);
                (a.name.clone(), a)
            })
            .collect();
        self.reviews = snap.reviews;
        for r in self.reviews.iter_mut().filter(|r| r.phase.active()) {
            r.deadline += down;
        }
    }

    pub fn review(&self, id: &str) -> Option<&Review> {
        let id = id.trim();
        self.reviews.iter().find(|r| r.id.eq_ignore_ascii_case(id))
    }

    fn review_mut(&mut self, id: &str) -> Result<&mut Review> {
        let id = id.trim();
        self.reviews
            .iter_mut()
            .find(|r| r.id.eq_ignore_ascii_case(id))
            .ok_or_else(|| anyhow!("no review {id} (the relay may have restarted)"))
    }

    /// Records that `name` is alive and binds the name to the caller's Tailscale login, with the
    /// role its caller has, if known.
    pub fn touch(&mut self, name: &str, who: &Identity, role: Option<Role>, now: Ms) -> Result<()> {
        if let Some(old) = self
            .agents
            .get(name)
            .filter(|a| a.user_id != 0 && a.user_id == who.user_id && !a.login.eq_ignore_ascii_case(&who.login))
        {
            let old = old.login.clone();
            self.rename_login(&old, who);
        }
        self.check_name(name, who, now)?;
        let timing = self.timing;
        if self.agents.get(name).is_some_and(|a| !a.login.eq_ignore_ascii_case(&who.login)) {
            self.agents.map.remove(name);
        }
        if self.agents.get(name).is_none() {
            self.make_room(&who.login, now)?;
        }
        let a = self.agents.map.entry(name.to_string()).or_insert_with(|| Agent {
            name: name.to_string(),
            login: who.login.clone(),
            user_id: who.user_id,
            node: who.node.clone(),
            role: role.unwrap_or(Role::Author),
            model: None,
            first_seen: now,
            last_seen: now,
            waiting: 0,
            events: VecDeque::new(),
            next_seq: 0,
            delivered: 0,
        });
        if now.saturating_sub(a.last_seen) >= timing.online_secs * 1000 && a.waiting == 0 {
            a.first_seen = now;
        }
        if let Some(r) = role {
            a.role = r;
        }
        a.last_seen = now;
        a.node = who.node.clone();
        if who.user_id != 0 {
            a.user_id = who.user_id;
        }
        Ok(())
    }

    /// Moves everything bound to login `from` over to `to`: a renamed Tailscale login, or `local`
    /// once the relay knows its owner.
    pub fn rename_login(&mut self, from: &str, to: &Identity) {
        let mut logins: Vec<&mut String> = self.agents.map.values_mut().map(|a| &mut a.login).collect();
        for r in &mut self.reviews {
            logins.push(&mut r.author_login);
            logins.extend(r.slots.iter_mut().map(|s| &mut s.login));
        }
        for l in logins.into_iter().filter(|l| l.eq_ignore_ascii_case(from)) {
            l.clone_from(&to.login);
        }
    }

    /// Forgets an offline, unengaged agent if a new one for `login` would exceed the caps.
    fn make_room(&mut self, login: &str, now: Ms) -> Result<()> {
        let mine = |a: &Agent| a.login.eq_ignore_ascii_case(login);
        let per_login = self.agents.all().filter(|a| mine(a)).count() >= AGENTS_PER_LOGIN;
        let total = self.agents.map.len() >= MAX_AGENTS;
        if !per_login && !total {
            return Ok(());
        }
        let victim = self
            .agents
            .all()
            .filter(|a| (!per_login || mine(a)) && a.waiting == 0 && !self.agents.online(&a.name, now) && !self.engaged(&a.name))
            .min_by_key(|a| (self.has_undelivered(&a.name), a.last_seen))
            .map(|a| a.name.clone());
        match victim {
            Some(v) => {
                self.agents.map.remove(&v);
                Ok(())
            }
            None if per_login => bail!("{login} already has {AGENTS_PER_LOGIN} agents on the relay; reuse one of their names"),
            None => bail!("the relay already has {MAX_AGENTS} agents; try again later"),
        }
    }

    /// Refuses `name`, in any letter case, while another login holds it and is online, in an
    /// unfinished review, or has events it hasn't read (such as a result).
    pub fn check_name(&self, name: &str, who: &Identity, now: Ms) -> Result<()> {
        if let Some(holder) = self.agents.all().find(|a| {
            a.name.eq_ignore_ascii_case(name)
                && !a.login.eq_ignore_ascii_case(&who.login)
                && (self.agents.online(&a.name, now) || self.engaged(&a.name) || self.has_undelivered(&a.name))
        }) {
            bail!(
                "agent name `{name}` is in use by another tailnet user ({}); pick a different name, e.g. one prefixed with your own user name",
                holder.login
            );
        }
        Ok(())
    }

    /// Records the program a reviewer says it runs in.
    pub fn set_model(&mut self, name: &str, model: Model) {
        if let Some(a) = self.agents.map.get_mut(name) {
            a.model = Some(model);
        }
    }

    fn login_of(&self, name: &str) -> String {
        self.agents.get(name).map(|a| a.login.clone()).unwrap_or_default()
    }

    pub fn set_waiting(&mut self, name: &str, delta: i32, now: Ms) {
        if let Some(a) = self.agents.map.get_mut(name) {
            a.waiting = a.waiting.saturating_add_signed(delta);
            a.last_seen = now;
        }
    }

    /// Whether `name` is the author of, or an active reviewer on, an unfinished review.
    fn engaged(&self, name: &str) -> bool {
        self.reviews
            .iter()
            .any(|r| r.phase.active() && r.participant(name))
    }

    /// The unfinished review `name` is assigned to as a reviewer.
    pub fn busy_with(&self, name: &str) -> Option<&Review> {
        self.reviews
            .iter()
            .find(|r| r.phase.active() && r.slot(name).is_some_and(Slot::active))
    }

    pub fn online(&self, now: Ms) -> Vec<OnlineAgent> {
        self.agents
            .all()
            .filter(|a| self.agents.online(&a.name, now))
            .map(|a| OnlineAgent {
                name: a.name.clone(),
                login: a.login.clone(),
                node: a.node.clone(),
                role: a.role,
                model: a.model,
                busy: self.busy_with(&a.name).map(|r| r.id.clone()),
                idle_ms: self.agents.silent_ms(&a.name, now),
            })
            .collect()
    }

    /// Undelivered events for `name`, minus tasks it has already done and, unless asked for,
    /// progress, and the seq they reach. They count as delivered at once unless `held`, in which
    /// case only once `ack` is called with that seq.
    pub fn take_events(&mut self, name: &str, progress: bool, held: bool, now: Ms) -> (Vec<AgentEvent>, u64) {
        let open = self.open_tasks(name);
        let Some(a) = self.agents.map.get_mut(name) else {
            return (Vec::new(), 0);
        };
        let mut fresh = Vec::new();
        let mut bytes = 0;
        let mut reached = a.next_seq;
        for e in a.events.iter().filter(|e| e.seq > a.delivered) {
            if (progress || !e.progress) && e.task.as_ref().is_none_or(|t| open.contains(t)) {
                if !fresh.is_empty() && bytes + e.text.len() > MAX_WAIT_BYTES {
                    reached = e.seq - 1;
                    break;
                }
                bytes += e.text.len();
                fresh.push(e.clone().rendered(now));
            }
        }
        if !held {
            a.delivered = reached;
        }
        (fresh, reached)
    }

    /// The agent's client has received its events up to `seq`.
    pub fn ack(&mut self, name: &str, seq: u64) {
        if let Some(a) = self.agents.map.get_mut(name).filter(|a| seq <= a.next_seq) {
            a.delivered = a.delivered.max(seq);
        }
    }

    /// Events not yet received that need the agent; talk on finished reviews doesn't.
    pub fn has_undelivered(&self, name: &str) -> bool {
        let open = self.open_tasks(name);
        self.agents
            .get(name)
            .is_some_and(|a| a.events.iter().any(|e| e.seq > a.delivered && !e.progress && self.live(name, e, &open)))
    }

    /// An open task, its author's result, or anything about an unfinished review `name` is in.
    fn live(&self, name: &str, e: &AgentEvent, open: &HashSet<String>) -> bool {
        match &e.task {
            Some(t) => open.contains(t),
            None => {
                e.result
                    || e.review
                        .as_deref()
                        .and_then(|id| self.review(id))
                        .is_some_and(|r| r.phase.active() && r.participant(name))
            }
        }
    }

    /// A new MCP session under `name`: drops the queued events it no longer needs.
    pub fn new_session(&mut self, name: &str) {
        let open = self.open_tasks(name);
        let Some(a) = self.agents.get(name) else {
            return;
        };
        let keep: HashSet<u64> = a.events.iter().filter(|e| self.live(name, e, &open)).map(|e| e.seq).collect();
        if let Some(a) = self.agents.map.get_mut(name) {
            a.events.retain(|e| keep.contains(&e.seq));
        }
    }

    fn open_tasks(&self, name: &str) -> HashSet<String> {
        self.next_actions(name, 0).into_iter().filter_map(|a| a.task).collect()
    }

    /// Drops queued events for tasks whose full text `name` has just been given in a tool result.
    pub fn forget_tasks(&mut self, name: &str, tasks: &[String]) {
        if let Some(a) = self.agents.map.get_mut(name) {
            let delivered = a.delivered;
            a.events
                .retain(|e| e.seq <= delivered || e.task.as_ref().is_none_or(|t| !tasks.contains(t)));
        }
    }

    /// Events after `after` without marking them delivered.
    pub fn peek_events(&self, name: &str, after: u64) -> (Vec<AgentEvent>, u64) {
        match self.agents.get(name) {
            Some(a) => (
                a.events.iter().filter(|e| e.seq > after).cloned().collect(),
                a.next_seq,
            ),
            None => (Vec::new(), 0),
        }
    }

    /// What `name` must do next, most pressing first.
    pub fn next_actions(&self, name: &str, now: Ms) -> Vec<Action> {
        let mut out = Vec::new();
        let idle = self.busy_with(name).is_none();
        let reviewer = self.agents.get(name).is_some_and(|a| a.role == Role::Reviewer);
        let model = self.agents.get(name).and_then(|a| a.model);
        let mut offers = 0;
        if reviewer && idle && model.is_none() {
            out.push(Action {
                review: String::new(),
                urgent: true,
                text: NO_MODEL.to_string(),
                task: None,
                short: None,
            });
        }
        for r in self.reviews.iter().filter(|r| r.phase.active()) {
            let left = text::dur(r.deadline_left(now));
            let id = &r.id;
            if let Some(slot) = r.slot(name).filter(|s| s.active()) {
                match r.phase {
                    Phase::Offering | Phase::Reviewing if slot.submission.is_none() => {
                        let left = if r.phase == Phase::Offering {
                            format!("at least {}", text::dur(r.params.review_secs * 1000))
                        } else {
                            left.clone()
                        };
                        out.push(Action {
                            review: id.clone(),
                            urgent: true,
                            text: text::review_task(r, &left),
                            task: Some(review_task(id)),
                            short: Some(format!("{id}: review the PR as described above, then call `submit_findings`.")),
                        });
                    }
                    Phase::Offering | Phase::Reviewing => out.push(Action::info(
                        id,
                        format!("{id}: your findings are in; everyone's are revealed once all reviewers have submitted. Call `wait`."),
                    )),
                    Phase::Voting(round) if r.replying => {
                        let mine = r.contested_by(name);
                        if mine.is_empty() || r.replied.contains(name) {
                            out.push(Action::info(
                                id,
                                format!("{id}: round {round} votes open once the authors of contested findings have replied (at most {left}). Call `wait`."),
                            ));
                        } else {
                            out.push(Action {
                                review: id.clone(),
                                urgent: false,
                                text: format!(
                                    "{id}: your findings {} drew objections; call `wait` to read them, then within {left} `amend_finding`, `withdraw_finding`, or `say` why they stand.",
                                    mine.join(", ")
                                ),
                                task: Some(reply_task(r)),
                                short: Some(format!("{id}: reply as described above (within {left}), then call `wait`.")),
                            });
                        }
                    }
                    Phase::Voting(round) => {
                        let owed = r.owed_votes(name);
                        if owed.is_empty() {
                            let voted = r.findings.iter().any(|f| f.open_round == round && f.votes.get(name).is_some_and(|v| v.round == round));
                            out.push(Action::info(
                                id,
                                if voted {
                                    format!("{id}: your votes for round {round} are in. Call `wait` for the other reviewers (you may `say` something meanwhile). Round ends in {left}.")
                                } else {
                                    format!("{id}: nothing for you to vote on in round {round}. Call `wait` (you may `say` something meanwhile). Round ends in {left}.")
                                },
                            ));
                        } else {
                            let ids: Vec<&str> = owed.iter().map(|f| f.id.as_str()).collect();
                            out.push(Action {
                                review: id.clone(),
                                urgent: true,
                                text: text::vote_task(r, &owed, &left),
                                task: Some(vote_task(r, &owed)),
                                short: Some(format!("{id}: vote on {} as described above.", ids.join(", "))),
                            });
                        }
                    }
                    Phase::Done | Phase::Failed => {}
                }
            } else if r.phase == Phase::Offering
                && r.offered.contains(name)
                && !r.declined.contains(name)
                && reviewer
                && idle
                && r.fits(model)
            {
                offers += 1;
                if offers <= OFFERS_SHOWN {
                    out.push(Action {
                        review: id.clone(),
                        urgent: true,
                        text: format!(
                            "{id}: review offer for {} from {}. Call `accept_review` (review_id \"{id}\") to take it or `decline_review` if you can't. Offer closes in {left}.",
                            r.pr.url(),
                            r.author
                        ),
                        task: Some(offer_task(id)),
                        short: Some(format!("{id}: accept or decline the offer above.")),
                    });
                }
            }
            if r.author == name {
                out.push(Action::info(
                    id,
                    format!(
                        "{id}: your review of {} is in {} (ends in {left}). Call `wait`; the consolidated result will arrive as an event.",
                        r.pr.short(),
                        r.phase.label()
                    ),
                ));
            }
        }
        if offers > OFFERS_SHOWN {
            out.push(Action::info(
                "",
                format!("{} more review offers are open; they are shown once you have answered the ones above.", offers - OFFERS_SHOWN),
            ));
        }
        out.sort_by_key(|a| !a.urgent);
        out
    }

    pub fn request_review(&mut self, author: &str, req: NewReview, now: Ms) -> Result<String> {
        let r = self.new_review(author, req, now)?;
        let id = r.id.clone();
        self.reviews.push(r);
        self.advance(now);
        Ok(id)
    }

    fn new_review(&mut self, author: &str, req: NewReview, now: Ms) -> Result<Review> {
        let pr = crate::prurl::parse(&req.pr_url)
            .ok_or_else(|| anyhow!("not a GitHub pull request URL: {}", req.pr_url))?;
        let login = self.agents.get(author).map(|a| a.login.clone()).unwrap_or_default();
        if let Some(r) = self
            .reviews
            .iter()
            .find(|r| r.phase.active() && r.author == author && r.pr == pr)
        {
            bail!("you already have review {} of this PR in progress; call `wait` for its result", r.id);
        }
        let open = self
            .reviews
            .iter()
            .filter(|r| r.phase.active() && r.author_login.eq_ignore_ascii_case(&login))
            .count();
        if open >= REVIEWS_PER_LOGIN {
            bail!("{login} already has {open} unfinished reviews on the relay; call `wait` until one finishes");
        }
        if let Some(sha) = &req.head_sha
            && !valid_sha(sha)
        {
            bail!("head_sha must be a hex commit SHA");
        }
        let c = self.consensus;
        let picked = self.check_picked(author, &req.picked, now)?;
        let reviewers = match req.reviewers {
            Some(n) if !picked.is_empty() && n > picked.len() => {
                bail!("asked for {n} reviewers but named only {}", picked.len())
            }
            Some(n) => n,
            None if picked.is_empty() => c.reviewers,
            None => picked.len(),
        };
        if !(1..=5).contains(&reviewers) {
            bail!("the number of reviewers must be between 1 and 5");
        }
        if reviewers >= 2 && !picked.is_empty() {
            let models: BTreeSet<Model> = picked.iter().filter_map(|p| self.agents.get(p)?.model).collect();
            if let Some(m) = Model::ALL.into_iter().find(|m| !models.contains(m)) {
                bail!("none of the reviewers you named is a {} reviewer: a review with more than one reviewer needs both claude and codex; call `online` to see who runs which", m.as_str());
            }
        }
        let t = self.timing;
        let params = ReviewParams {
            reviewers,
            consensus: c,
            offer_secs: t.offer_secs.clamp(5, 3600),
            review_secs: t.review_secs.clamp(10, 4 * 3600),
            vote_secs: t.vote_secs.clamp(10, 4 * 3600),
            reply_secs: t.reply_secs.min(600),
        };
        self.next_review += 1;
        let mut r = Review {
            id: format!("r{}", self.next_review),
            pr,
            author: author.to_string(),
            author_login: login,
            note: bounded(&req.note, "note", MAX_NOTE)?,
            title: req.title.as_deref().and_then(one_line),
            params,
            created: now,
            ended: None,
            phase: Phase::Offering,
            deadline: now + params.offer_secs * 1000,
            offered: BTreeSet::new(),
            declined: BTreeSet::new(),
            picked,
            slots: Vec::new(),
            findings: Vec::new(),
            log: Vec::new(),
            result: None,
            failure: None,
            parent: None,
            iteration: 1,
            since_sha: None,
            head_sha: req.head_sha,
            next_finding: 0,
            replying: false,
            replied: BTreeSet::new(),
            cancelled_by: None,
            missing: None,
        };
        let from = if r.picked.is_empty() { String::new() } else { format!(" from {}", r.picked.join(", ")) };
        r.log(now, author, "request", format!("requested {reviewers} reviewer(s){from}. {}", r.note));
        Ok(r)
    }

    /// The named reviewers, deduplicated, if all of them are online reviewers.
    fn check_picked(&self, author: &str, names: &[String], now: Ms) -> Result<Vec<String>> {
        let mut picked: Vec<String> = Vec::new();
        let mut bad = Vec::new();
        let author_login = self.login_of(author);
        let mut logins: Vec<String> = Vec::new();
        for name in names.iter().map(|n| n.trim()) {
            if picked.iter().any(|p| p == name) {
                continue;
            }
            match self.agents.get(name) {
                _ if name == author => bad.push(format!("{name} (that's you)")),
                None => bad.push(format!("{name} (unknown agent)")),
                Some(_) if !self.agents.online(name, now) => bad.push(format!("{name} (not online)")),
                Some(a) if a.role != Role::Reviewer => bad.push(format!("{name} (not a reviewer)")),
                Some(a) if a.model.is_none() => bad.push(format!("{name} (hasn't said whether it is claude or codex)")),
                Some(a) if logins.contains(&a.login.to_ascii_lowercase()) => bad.push(format!("{name} (another picked reviewer's Tailscale login)")),
                Some(a) if !a.login.eq_ignore_ascii_case(&author_login) => logins.push(a.login.to_ascii_lowercase()),
                Some(_) => {}
            }
            picked.push(name.to_string());
        }
        if !bad.is_empty() {
            bail!("can't offer the review to {}; call `online` to see who is available", bad.join(", "));
        }
        Ok(picked)
    }

    pub fn accept(&mut self, agent: &str, id: &str, now: Ms) -> Result<()> {
        let busy = self.busy_with(agent).map(|r| r.id.clone());
        let role = self.agents.get(agent).map(|a| a.role);
        let model = self.agents.get(agent).and_then(|a| a.model);
        let login = self.login_of(agent);
        let r = self.review_mut(id)?;
        r.check_login(agent, &login)?;
        if r.slot(agent).is_some_and(Slot::active) {
            return Ok(());
        }
        if r.phase != Phase::Offering {
            bail!("{} is no longer looking for reviewers ({})", r.id, r.phase.label());
        }
        if r.author == agent {
            bail!("you can't review your own request");
        }
        if !r.picked.is_empty() && !r.picked.iter().any(|p| p == agent) {
            bail!("{} was offered only to the reviewers its author picked", r.id);
        }
        if role != Some(Role::Reviewer) {
            bail!("only agents with role reviewer can accept reviews; call `hello` with role \"reviewer\" first");
        }
        if r.slot(agent).is_some() {
            bail!("you were dropped from {}; you can't rejoin it", r.id);
        }
        if let Some(b) = busy {
            bail!("you are already reviewing {b}; finish it first");
        }
        if r.active_slots().count() >= r.params.reviewers {
            bail!("{} already has enough reviewers", r.id);
        }
        if r.login_taken(&login) {
            bail!("another agent of your Tailscale login already reviews {}: each person takes one slot", r.id);
        }
        let Some(model) = model else {
            bail!("call `hello` with role \"reviewer\" and model \"claude\" or \"codex\" (the agent program you run in) first");
        };
        if let Some(m) = r.lacks(model) {
            bail!("{} still needs a {} reviewer: a review with more than one reviewer has both claude and codex. Call `wait`", r.id, m.as_str());
        }
        r.declined.remove(agent);
        r.slots.push(Slot {
            agent: agent.to_string(),
            login,
            model: Some(model),
            joined: now,
            dropped: None,
            submission: None,
        });
        r.log(now, agent, "accept", "accepted the review");
        self.advance(now);
        Ok(())
    }

    pub fn decline(&mut self, agent: &str, id: &str, reason: &str, now: Ms) -> Result<()> {
        let reason = bounded(reason, "reason", MAX_REASON)?;
        let login = self.login_of(agent);
        let r = self.review_mut(id)?;
        r.check_login(agent, &login)?;
        if r.phase != Phase::Offering || r.slot(agent).is_some() {
            bail!("{} has no open offer for you", r.id);
        }
        if !r.declined.insert(agent.to_string()) {
            return Ok(());
        }
        r.log(now, agent, "decline", if reason.is_empty() { "declined".to_string() } else { format!("declined: {reason}") });
        self.advance(now);
        Ok(())
    }

    pub fn submit(&mut self, agent: &str, id: &str, sub: SubmitIn, now: Ms) -> Result<usize> {
        let SubmitIn {
            head_sha,
            verdict,
            summary,
            findings,
        } = sub;
        let login = self.login_of(agent);
        let r = self.review_mut(id)?;
        r.check_login(agent, &login)?;
        let slot = r
            .slot(agent)
            .filter(|s| s.active())
            .ok_or_else(|| anyhow!("you are not an active reviewer on {}", r.id))?;
        if slot.submission.is_some() {
            bail!("you already submitted findings for {}; amend them during cross-examination", r.id);
        }
        if !matches!(r.phase, Phase::Offering | Phase::Reviewing) {
            bail!("{} is not in the independent review phase ({})", r.id, r.phase.label());
        }
        let head_sha = head_sha.trim();
        if !valid_sha(head_sha) {
            bail!("head_sha must be the hex commit SHA you reviewed (gh pr view <url> --json headRefOid)");
        }
        let verdict = Verdict::parse(&verdict).ok_or_else(|| anyhow!("verdict must be approve or changes_requested"))?;
        if findings.len() > MAX_FINDINGS {
            bail!("at most {MAX_FINDINGS} findings; merge the small ones");
        }
        let mut parsed = Vec::new();
        for (i, f) in findings.iter().enumerate() {
            let severity = Severity::parse(&f.severity)
                .ok_or_else(|| anyhow!("finding {}: severity must be blocking, major, minor or nit", i + 1))?;
            let url = f.url.as_deref().map(str::trim).filter(|u| !u.is_empty());
            let url = url.map(|u| check_url(&r.pr, u)).transpose().map_err(|e| anyhow!("finding {}: {e}", i + 1))?;
            let text = need_text(&f.text, &format!("finding {}", i + 1))?;
            parsed.push((severity, url, text));
        }
        let summary = bounded(&summary, "summary", MAX_FINDING)?;
        check_reviewer_text(parsed.iter().map(|(_, u, t)| finding_size(t, u.as_deref())).sum::<usize>() + summary.chars().count())?;
        for (severity, url, text) in parsed {
            r.next_finding += 1;
            r.findings.push(Finding {
                id: format!("F{}", r.next_finding),
                by: Some(agent.to_string()),
                severity,
                url,
                text,
                origin: None,
                withdrawn: None,
                revision: 0,
                open_round: 1,
                votes: BTreeMap::new(),
                outcome: None,
                ruling: None,
            });
        }
        let n = findings.len();
        if let Some(s) = r.slot_mut(agent) {
            s.submission = Some(Submission {
                at: now,
                head_sha: head_sha.to_string(),
                verdict,
                summary,
            });
        }
        r.log(now, agent, "submit", format!("submitted {n} finding(s) at {head_sha}"));
        self.advance(now);
        Ok(n)
    }

    pub fn vote(&mut self, agent: &str, id: &str, votes: Vec<VoteIn>, now: Ms) -> Result<Voted> {
        let login = self.login_of(agent);
        let r = self.review_mut(id)?;
        r.check_login(agent, &login)?;
        let Phase::Voting(round) = r.phase else {
            bail!("{} is not in cross-examination ({}); call `wait`", r.id, r.phase.label());
        };
        if r.replying {
            bail!(
                "round {round} votes open once the authors of contested findings have replied (at most {}); call `wait`",
                text::dur(r.deadline_left(now))
            );
        }
        if !r.slot(agent).is_some_and(Slot::active) {
            bail!("you are not an active reviewer on {}", r.id);
        }
        if votes.is_empty() {
            bail!("no votes given");
        }
        let open: Vec<&str> = r
            .findings
            .iter()
            .filter(|f| f.withdrawn.is_none() && f.by.as_deref() != Some(agent) && f.open_round == round)
            .map(|f| f.id.as_str())
            .collect();
        let open = format!("nothing was recorded; resend only votes on findings open in round {round}: {}", open.join(", "));
        let mut parsed: Vec<(String, Vote, Option<&'static str>)> = Vec::new();
        let mut ignored = Vec::new();
        let mut seen = HashSet::new();
        for v in &votes {
            let f = r.finding(&v.finding).ok_or_else(|| anyhow!("{} has no finding {}", r.id, v.finding))?;
            if !seen.insert(f.id.clone()) {
                bail!("{} is voted on twice; nothing was recorded", f.id);
            }
            if f.by.as_deref() == Some(agent) {
                bail!("{} is your own finding; use amend_finding or withdraw_finding instead", f.id);
            }
            if f.withdrawn.is_some() {
                ignored.push(f.id.clone());
                continue;
            }
            if f.open_round != round {
                bail!("{} is not open for voting in round {round}; {open}", f.id);
            }
            let reason = v.reason.as_deref().map(|s| bounded(s, &format!("your reason on {}", f.id), MAX_REASON)).transpose()?;
            let reason = reason.filter(|s| !s.is_empty());
            let severity = v
                .severity
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Severity::parse(s).ok_or_else(|| anyhow!("{}: severity must be blocking, major, minor or nit", f.id)))
                .transpose()?;
            let choice = match v.vote.trim().to_ascii_lowercase().replace('-', "_").as_str() {
                "agree" => Choice::Agree,
                "disagree" => {
                    if reason.is_none() {
                        bail!("disagreeing with {} needs a reason", f.id);
                    }
                    if severity.is_some() {
                        bail!(
                            "{}: severity goes with agree. If the problem is real but over- or understated, vote agree with the severity you think is fair; disagree only if it is not a real problem",
                            f.id
                        );
                    }
                    Choice::Disagree
                }
                "duplicate" | "duplicate_of" => {
                    let t = v.duplicate_of.as_deref().unwrap_or("");
                    let target = r
                        .finding(t)
                        .filter(|g| g.id != f.id)
                        .ok_or_else(|| anyhow!("{}: duplicate_of must name another finding id", f.id))?;
                    Choice::Duplicate(target.id.clone())
                }
                other => bail!("{}: vote must be agree, disagree or duplicate (got {other})", f.id),
            };
            let severity = severity.filter(|s| choice == Choice::Agree && *s != f.severity);
            let vote = Vote {
                choice,
                severity,
                reason,
                round,
                revision: f.revision,
                at: now,
            };
            let kind = match f.votes.get(agent) {
                Some(old) if old.round == round && old.revision == f.revision => {
                    (old.choice != vote.choice || old.severity != vote.severity || old.reason != vote.reason).then_some("revote")
                }
                _ => Some("vote"),
            };
            parsed.push((f.id.clone(), vote, kind));
        }
        let reasons: usize = r
            .findings
            .iter()
            .filter(|f| !parsed.iter().any(|(id, ..)| *id == f.id))
            .filter_map(|f| f.votes.get(agent).filter(|v| v.round == round))
            .chain(parsed.iter().map(|(_, v, _)| v))
            .filter_map(|v| v.reason.as_ref().map(|s| s.chars().count()))
            .sum();
        if reasons > MAX_REVIEWER_TEXT {
            bail!("your reasons in round {round} would be {reasons} characters; at most {MAX_REVIEWER_TEXT} in all, so be more concise");
        }
        check_talk(r, agent, parsed.iter().filter(|(.., k)| *k == Some("revote")).count(), now)?;
        let recorded = parsed.len();
        for (fid, vote, kind) in parsed {
            let f = r.findings.iter_mut().find(|f| f.id == fid).expect("validated");
            let line = format!("{fid}: {}", text::vote_label(f, &vote))
                + &vote.reason.as_deref().map(|s| format!(" — {s}")).unwrap_or_default();
            f.votes.insert(agent.to_string(), vote);
            if let Some(kind) = kind {
                r.log(now, agent, kind, line);
            }
        }
        self.advance(now);
        Ok(Voted { recorded, ignored })
    }

    pub fn say(&mut self, agent: &str, id: &str, msg: &str, now: Ms) -> Result<()> {
        let msg = bounded(msg, "message", MAX_SAY)?;
        if msg.is_empty() {
            bail!("message must not be empty");
        }
        let login = self.login_of(agent);
        let r = self.review_mut(id)?;
        r.check_login(agent, &login)?;
        let reviewer = r.slot(agent).is_some_and(|s| s.active() || !r.phase.active());
        if !reviewer && r.author != agent {
            bail!("only the author and active reviewers of {} can talk in it", r.id);
        }
        if reviewer && matches!(r.phase, Phase::Offering | Phase::Reviewing) {
            bail!("reviewers can discuss once findings are revealed (cross-examination); work independently until then");
        }
        match r.ended {
            Some(ended) if r.log.iter().filter(|e| e.kind == "say" && e.who == agent && e.at >= ended).count() >= CLOSING_REMARKS => {
                bail!("you have made your {CLOSING_REMARKS} closing remarks on {}", r.id);
            }
            Some(_) => {}
            None => check_talk(r, agent, 1, now)?,
        }
        r.log(now, agent, "say", msg.clone());
        if r.replying {
            r.replied.insert(agent.to_string());
        }
        let to = r.others(agent);
        let text = format!("[{}] {agent} says:\n{}", r.id, text::quote(&msg).trim_end());
        let ev = AgentEvent { progress: r.ended.is_some(), ..AgentEvent::new(&r.id, text, now) };
        for a in to {
            self.agents.push(&a, ev.clone());
        }
        self.advance(now);
        Ok(())
    }

    fn own_finding<'a>(r: &'a mut Review, agent: &str, fid: &str) -> Result<&'a mut Finding> {
        if !matches!(r.phase, Phase::Voting(_)) {
            bail!("findings can be amended or withdrawn only during cross-examination");
        }
        let rid = r.id.clone();
        let f = r
            .findings
            .iter_mut()
            .find(|f| f.id.eq_ignore_ascii_case(fid.trim()))
            .ok_or_else(|| anyhow!("{rid} has no finding {fid}"))?;
        if f.by.as_deref() != Some(agent) {
            bail!("{} is not your finding", f.id);
        }
        if f.withdrawn.is_some() {
            bail!("{} is already withdrawn", f.id);
        }
        Ok(f)
    }

    pub fn amend(&mut self, agent: &str, id: &str, fid: &str, change: AmendIn, now: Ms) -> Result<()> {
        let severity = change.severity.as_deref();
        let url = change.url.as_deref();
        let new_text = change.text.as_deref();
        let login = self.login_of(agent);
        let r = self.review_mut(id)?;
        r.check_login(agent, &login)?;
        let pr = r.pr.clone();
        let round = r.round();
        let replying = r.replying;
        let severity = severity
            .map(|s| Severity::parse(s).ok_or_else(|| anyhow!("severity must be blocking, major, minor or nit")))
            .transpose()?;
        let url = url.map(str::trim).filter(|u| !u.is_empty()).map(|u| check_url(&pr, u)).transpose()?;
        let new_text = new_text.map(|t| need_text(t, "text")).transpose()?;
        if severity.is_none() && url.is_none() && new_text.is_none() {
            bail!("nothing to amend: give severity, url and/or text");
        }
        let total = reviewer_text(r, agent);
        check_talk(r, agent, 1, now)?;
        let f = Self::own_finding(r, agent, fid)?;
        if f.revision >= AMENDS_PER_FINDING {
            bail!("you have amended {} {AMENDS_PER_FINDING} times; withdraw it or `say` why it stands", f.id);
        }
        let after = finding_size(new_text.as_deref().unwrap_or(&f.text), url.as_deref().or(f.url.as_deref()));
        check_reviewer_text(total - finding_size(&f.text, f.url.as_deref()) + after)?;
        if !replying {
            bail!("findings can be amended only in the reply window before a later round's votes open; `say` why it stands or `withdraw_finding`");
        }
        if let Some(s) = severity {
            f.severity = s;
        }
        if url.is_some() {
            f.url = url;
        }
        if let Some(t) = new_text {
            f.text = t;
        }
        f.revision += 1;
        f.open_round = round;
        let line = format!("amended {} [{}]: {}", f.id, f.severity.as_str(), f.text);
        let fid = f.id.clone();
        r.log(now, agent, "amend", line);
        r.replied.insert(agent.to_string());
        let rid = r.id.clone();
        for a in r.others(agent) {
            self.agents.progress(&a, &rid, format!("[{rid}] {agent} amended {fid}; votes on it are reopened."), now);
        }
        self.advance(now);
        Ok(())
    }

    pub fn withdraw(&mut self, agent: &str, id: &str, fid: &str, reason: &str, now: Ms) -> Result<()> {
        let reason = bounded(reason, "reason", MAX_REASON)?;
        let login = self.login_of(agent);
        let r = self.review_mut(id)?;
        r.check_login(agent, &login)?;
        let f = Self::own_finding(r, agent, fid)?;
        f.withdrawn = Some(reason.clone());
        let fid = f.id.clone();
        r.log(now, agent, "withdraw", format!("withdrew {fid}. {reason}"));
        r.replied.insert(agent.to_string());
        let rid = r.id.clone();
        for a in r.others(agent) {
            self.agents.progress(&a, &rid, format!("[{rid}] {agent} withdrew {fid}: no vote needed."), now);
        }
        self.advance(now);
        Ok(())
    }

    pub fn rereview(&mut self, author: &str, id: &str, head_sha: &str, note: &str, now: Ms) -> Result<String> {
        let head_sha = head_sha.trim();
        if !valid_sha(head_sha) {
            bail!("new_head_sha must be the hex SHA of the commit you pushed");
        }
        let prev = self
            .review(id)
            .ok_or_else(|| anyhow!("no review {id} (the relay may have restarted; use request_review)"))?
            .clone();
        if prev.author != author {
            bail!("only {} (the author of {}) can request a re-review", prev.author, prev.id);
        }
        prev.check_login(author, &self.login_of(author))?;
        if prev.phase != Phase::Done {
            bail!("{} has not finished ({})", prev.id, prev.phase.label());
        }
        if let Some(r) = rereview(&self.reviews, &prev.id) {
            bail!("{} is already re-reviewed as {}", prev.id, r.id);
        }
        let reviewed = prev
            .slots
            .iter()
            .filter_map(|s| s.submission.as_ref().map(|x| x.head_sha.clone()))
            .next()
            .or(prev.head_sha.clone());
        let mut req = NewReview::new(&prev.pr.url());
        req.note = note.to_string();
        req.title = prev.title.clone();
        req.head_sha = Some(head_sha.to_string());
        req.reviewers = Some(prev.params.reviewers);
        let mut r = self.new_review(author, req, now)?;
        r.params = prev.params;
        r.deadline = now + r.params.offer_secs * 1000;
        let busy: HashSet<String> = self
            .reviews
            .iter()
            .filter(|r| r.phase.active())
            .flat_map(|r| r.active_slots().map(|s| s.agent.clone()))
            .collect();
        let mut candidates: Vec<(String, String, Model)> = Vec::new();
        for s in prev.slots.iter().filter(|s| {
            s.active()
                && self.agents.online(&s.agent, now)
                && !busy.contains(&s.agent)
                && self.agents.get(&s.agent).is_some_and(|a| a.role == Role::Reviewer && a.login == s.login)
        }) {
            let author = s.login.eq_ignore_ascii_case(&r.author_login);
            let model = self.agents.get(&s.agent).and_then(|a| a.model);
            if let Some(model) = model
                && (author || !candidates.iter().any(|(_, l, _)| l.eq_ignore_ascii_case(&s.login)))
            {
                candidates.push((s.agent.clone(), s.login.clone(), model));
            }
        }
        r.parent = Some(prev.id.clone());
        r.iteration = prev.iteration + 1;
        r.since_sha = reviewed;
        for f in prev.findings.iter() {
            let Some(o) = &f.outcome else { continue };
            if !matches!(o.status, Status::Confirmed | Status::Disputed | Status::Unverified) || f.ruled().is_some_and(|r| !r.must_fix) {
                continue;
            }
            r.next_finding += 1;
            r.findings.push(Finding {
                id: format!("C{}", r.next_finding),
                by: None,
                severity: o.severity,
                url: f.url.clone(),
                text: f.text.clone(),
                origin: Some(format!("{}/{}", prev.id, f.id)),
                withdrawn: None,
                revision: 0,
                open_round: 1,
                votes: BTreeMap::new(),
                outcome: None,
                ruling: f.ruled().cloned(),
            });
        }
        for (agent, login, model) in candidates {
            if r.slots.len() >= r.params.reviewers || !r.fits(Some(model)) {
                continue;
            }
            r.slots.push(Slot {
                agent: agent.clone(),
                login,
                model: Some(model),
                joined: now,
                dropped: None,
                submission: None,
            });
            r.log(now, &agent, "accept", format!("assigned again (reviewed {})", prev.id));
        }
        let left = format!("at least {}", text::dur(r.params.review_secs * 1000));
        for s in &r.slots {
            let text = format!("[{}] You are assigned again (you reviewed {}).\n{}", r.id, prev.id, text::review_task(&r, &left));
            self.agents.notify_task(&s.agent, &r.id, review_task(&r.id), text, None, now);
        }
        let new_id = r.id.clone();
        self.reviews.push(r);
        self.advance(now);
        Ok(new_id)
    }

    /// A human stops an unfinished review; `by` is their name as shown.
    pub fn cancel(&mut self, id: &str, by: &str, now: Ms) -> Result<()> {
        let State { agents, reviews, .. } = self;
        let r = reviews
            .iter_mut()
            .find(|r| r.id.eq_ignore_ascii_case(id.trim()))
            .ok_or_else(|| anyhow!("no review {id}"))?;
        if !r.phase.active() {
            bail!("{} has already ended", r.id);
        }
        r.cancelled_by = Some(by.to_string());
        fail(r, agents, &format!("cancelled by {by}"), now);
        Ok(())
    }

    /// A human rules on a finding the votes left open: `Some(true)` must fix, `Some(false)` dismiss,
    /// `None` undoes. Returns whether anything changed.
    pub fn rule(&mut self, id: &str, fid: &str, by: &str, must_fix: Option<bool>, now: Ms) -> Result<bool> {
        let i = self
            .reviews
            .iter()
            .position(|r| r.id.eq_ignore_ascii_case(id.trim()))
            .ok_or_else(|| anyhow!("no review {id}"))?;
        if let Some(o) = rereview(&self.reviews, &self.reviews[i].id) {
            bail!("{0} re-reviews {1}, so rulings on {1} are locked unless {0} fails; reload the page", o.id, self.reviews[i].id);
        }
        let web = self.web_url.clone();
        let State { agents, reviews, .. } = self;
        let r = &mut reviews[i];
        if r.phase != Phase::Done {
            bail!("{} has not finished", r.id);
        }
        let rid = r.id.clone();
        if must_fix.is_some() && r.log.iter().filter(|e| e.kind == "ruling").count() >= SAYS_PER_REVIEW {
            bail!("{rid} has had {SAYS_PER_REVIEW} rulings; no more are taken");
        }
        let f = r
            .findings
            .iter_mut()
            .find(|f| f.id.eq_ignore_ascii_case(fid.trim()))
            .ok_or_else(|| anyhow!("{rid} has no finding {fid}"))?;
        if !f.outcome.as_ref().is_some_and(|o| matches!(o.status, Status::Disputed | Status::Unverified)) {
            bail!("{} is not disputed or unverified", f.id);
        }
        if f.ruling.as_ref().map(|x| x.must_fix) == must_fix {
            return Ok(false);
        }
        f.ruling = must_fix.map(|must_fix| Ruling { by: by.to_string(), must_fix, at: now });
        let fid = f.id.clone();
        let what = match must_fix {
            Some(true) => "must fix",
            Some(false) => "dismissed",
            None => "ruling undone",
        };
        r.log(now, by, "ruling", format!("{fid}: {what}"));
        r.result = r.result_now(&web);
        let v = match verdict_of(&r.findings) {
            Verdict::Approve => "approve",
            Verdict::ChangesRequested => "changes requested",
        };
        let did = match must_fix {
            Some(true) => format!("ruled {fid} must fix"),
            Some(false) => format!("dismissed {fid}"),
            None => format!("undid the ruling on {fid}"),
        };
        let event = format!("[{rid}] {by} (a human) {did} (verdict: {v}).\n{}", text::next_step(r));
        agents.outcome(&r.author.clone(), &rid, event, now);
        Ok(true)
    }

    /// Applies deadlines and presence; call about once a second. Returns true if anything changed.
    pub fn tick(&mut self, now: Ms) -> bool {
        let before = self.fingerprint(now);
        let stepped = self.advance(now);
        self.trim_history();
        let engaged: HashSet<String> = self
            .reviews
            .iter()
            .filter(|r| r.phase.active())
            .flat_map(|r| std::iter::once(r.author.clone()).chain(r.slots.iter().map(|s| s.agent.clone())))
            .collect();
        let streams = &self.agents.streams;
        self.agents.map.retain(|name, a| {
            engaged.contains(name) || a.waiting > 0 || streams.contains_key(name) || now.saturating_sub(a.last_seen) < FORGET_AGENT_MS
        });
        stepped || before != self.fingerprint(now)
    }

    /// Keeps `history` finished reviews that someone took part in and `FAILED_KEPT` that nobody
    /// did, dropping the oldest of whichever login has the most, so one caller's requests can't
    /// push out everyone else's.
    fn trim_history(&mut self) {
        let unattended = |r: &Review| r.phase == Phase::Failed && r.slots.is_empty();
        for (kind, cap) in [(false, self.history), (true, FAILED_KEPT.min(self.history))] {
            let of_kind = |r: &Review| !r.phase.active() && unattended(r) == kind;
            loop {
                let mut per_login: BTreeMap<String, usize> = BTreeMap::new();
                for r in self.reviews.iter().filter(|r| of_kind(r)) {
                    *per_login.entry(r.author_login.to_ascii_lowercase()).or_default() += 1;
                }
                if per_login.values().sum::<usize>() <= cap {
                    break;
                }
                let most = per_login.values().copied().max().unwrap_or(0);
                let oldest = self
                    .reviews
                    .iter()
                    .position(|r| of_kind(r) && per_login[&r.author_login.to_ascii_lowercase()] == most);
                match oldest {
                    Some(i) => self.reviews.remove(i),
                    None => break,
                };
            }
        }
    }

    fn fingerprint(&self, now: Ms) -> Fingerprint {
        (
            self.agents
                .all()
                .map(|a| (a.name.clone(), self.agents.online(&a.name, now)))
                .collect(),
            self.reviews.len(),
            self.reviews
                .iter()
                .map(|r| (r.id.clone(), r.phase, r.active_slots().count()))
                .collect(),
        )
    }

    fn advance(&mut self, now: Ms) -> bool {
        let mut changed = false;
        for i in 0..self.reviews.len() {
            for _ in 0..8 {
                if !self.step(i, now) {
                    break;
                }
                changed = true;
            }
        }
        changed
    }

    /// One transition of review `i`, if due. Returns whether anything changed.
    fn step(&mut self, i: usize, now: Ms) -> bool {
        let busy: HashSet<String> = self
            .reviews
            .iter()
            .filter(|r| r.phase.active())
            .flat_map(|r| r.active_slots().map(|s| s.agent.clone()))
            .collect();
        let drop_ms = self.timing.drop_after_secs * 1000;
        let web = self.web_url.clone();
        let State { agents, reviews, .. } = self;
        let r = &mut reviews[i];
        match r.phase {
            Phase::Offering => {
                let mut changed = false;
                let eligible: Vec<String> = agents
                    .all()
                    .filter(|a| {
                        a.role == Role::Reviewer
                            && a.name != r.author
                            && agents.online(&a.name, now)
                            && !busy.contains(&a.name)
                            && !r.offered.contains(&a.name)
                            && r.slot(&a.name).is_none()
                            && !r.login_taken(&a.login)
                            && r.fits(a.model)
                            && (r.picked.is_empty() || r.picked.contains(&a.name))
                    })
                    .map(|a| a.name.clone())
                    .collect();
                for a in eligible {
                    r.offered.insert(a.clone());
                    agents.notify_task(&a, &r.id, offer_task(&r.id), text::offer(r, text::LEFT), Some(r.deadline), now);
                    changed = true;
                }
                let n = r.active_slots().count();
                let missing = r.missing_model();
                if n >= r.params.reviewers || (now >= r.deadline && n > 0 && missing.is_none()) {
                    start_reviewing(r, agents, now);
                    return true;
                }
                if let Some(m) = missing.filter(|_| now >= r.deadline && n > 0) {
                    r.missing = Some(m);
                    fail(r, agents, &format!("no {} reviewer accepted before the offer deadline, and a review with more than one reviewer needs both claude and codex", m.as_str()), now);
                    return true;
                }
                if now >= r.deadline {
                    fail(r, agents, "no reviewers available: not enough idle reviewers accepted before the offer deadline", now);
                    return true;
                }
                changed
            }
            Phase::Reviewing => {
                let mut changed = drop_silent(r, agents, drop_ms, now);
                if now >= r.deadline {
                    let late: Vec<String> = r
                        .active_slots()
                        .filter(|s| s.submission.is_none())
                        .map(|s| s.agent.clone())
                        .collect();
                    for a in late {
                        drop_slot(r, agents, &a, "missed the review deadline", now);
                    }
                    changed = true;
                }
                if r.active_slots().all(|s| s.submission.is_some()) {
                    start_voting(r, agents, 1, &web, now);
                    return true;
                }
                changed
            }
            Phase::Voting(round) => {
                let mut changed = drop_silent(r, agents, drop_ms, now);
                if r.replying {
                    let waiting = r
                        .active_slots()
                        .any(|s| !r.replied.contains(&s.agent) && !r.contested_by(&s.agent).is_empty());
                    if now < r.deadline && waiting {
                        return changed;
                    }
                    r.replying = false;
                    open_votes(r, agents, false, now);
                    return true;
                }
                let owing: Vec<String> = r.active_slots().filter(|s| !r.owed_votes(&s.agent).is_empty()).map(|s| s.agent.clone()).collect();
                let late = now >= r.deadline;
                if late {
                    for a in &owing {
                        drop_slot(r, agents, a, &format!("missed the round {round} voting deadline"), now);
                    }
                    changed = true;
                }
                if late || owing.is_empty() {
                    let contested: Vec<usize> = r
                        .findings
                        .iter()
                        .enumerate()
                        .filter(|(_, f)| {
                            let disagree = f.current_votes().any(|(_, v)| v.choice == Choice::Disagree);
                            let agree = f.by.is_some() || f.current_votes().any(|(_, v)| v.choice != Choice::Disagree);
                            let rerated = f.current_votes().any(|(_, v)| v.severity.is_some());
                            f.withdrawn.is_none() && agree && (disagree || rerated)
                        })
                        .map(|(i, _)| i)
                        .collect();
                    if round < r.params.consensus.vote_rounds && !contested.is_empty() {
                        for i in contested {
                            r.findings[i].open_round = round + 1;
                        }
                        start_voting(r, agents, round + 1, &web, now);
                    } else {
                        finish(r, agents, &web, now);
                    }
                    return true;
                }
                changed
            }
            Phase::Done | Phase::Failed => false,
        }
    }
}

fn drop_silent(r: &mut Review, agents: &mut Agents, drop_ms: Ms, now: Ms) -> bool {
    let gone: Vec<String> = r
        .active_slots()
        .filter(|s| agents.silent_ms(&s.agent, now) >= drop_ms)
        .map(|s| s.agent.clone())
        .collect();
    for a in &gone {
        drop_slot(r, agents, a, "went offline", now);
    }
    !gone.is_empty()
}

fn drop_slot(r: &mut Review, agents: &mut Agents, agent: &str, why: &str, now: Ms) {
    if let Some(s) = r.slot_mut(agent) {
        s.dropped = Some(why.to_string());
    }
    r.log(now, agent, "drop", format!("dropped: {why}"));
    agents.notify(agent, &r.id, format!("[{}] You were dropped from the review ({why}). You are free for other reviews; call `wait`.", r.id), now);
    agents.progress(&r.author, &r.id, format!("[{}] {agent} was dropped ({why}); the review continues with the others.", r.id), now);
}

fn start_reviewing(r: &mut Review, agents: &mut Agents, now: Ms) {
    r.phase = Phase::Reviewing;
    r.deadline = now + r.params.review_secs * 1000;
    let names: Vec<String> = r.active_slots().map(|s| s.agent.clone()).collect();
    r.log(now, "relay", "phase", format!("independent review started with {}", names.join(", ")));
    for a in r.offered.clone() {
        if !names.contains(&a) && !r.declined.contains(&a) {
            agents.progress(&a, &r.id, format!("[{}] Offer closed: the review has its reviewers. Call `wait`.", r.id), now);
        }
    }
    agents.progress_due(
        &r.author,
        &r.id,
        format!(
            "[{}] Reviewers assigned: {} (of {} requested). Independent review phase, up to {}. Call `wait` for the result.",
            r.id,
            names.join(", "),
            r.params.reviewers,
            text::LEFT
        ),
        r.deadline,
        now,
    );
}

fn start_voting(r: &mut Review, agents: &mut Agents, round: u8, web: &str, now: Ms) {
    r.phase = Phase::Voting(round);
    r.replied.clear();
    let names: Vec<String> = r.active_slots().map(|s| s.agent.clone()).collect();
    let owed: usize = names.iter().map(|a| r.owed_votes(a).len()).sum();
    if owed == 0 {
        finish(r, agents, web, now);
        return;
    }
    r.log(now, "relay", "phase", format!("cross-examination round {round} started"));
    let authors: Vec<String> = names.into_iter().filter(|a| !r.contested_by(a).is_empty()).collect();
    if authors.is_empty() || r.params.reply_secs == 0 {
        open_votes(r, agents, false, now);
        return;
    }
    r.replying = true;
    r.deadline = now + r.params.reply_secs * 1000;
    for a in &authors {
        let text = format!("[{}] {}", r.id, text::reply_task(r, a, text::LEFT));
        agents.notify_task(a, &r.id, reply_task(r), text, Some(r.deadline), now);
    }
    open_votes(r, agents, true, now);
}

/// Sends the round's vote tasks; while `replying`, only tells the PR author the round has begun.
fn open_votes(r: &mut Review, agents: &mut Agents, replying: bool, now: Ms) {
    let round = r.round();
    let open = r.findings.iter().filter(|f| f.open_round == round && f.withdrawn.is_none()).count();
    let msg = if replying {
        format!("[{}] Cross-examination round {round}: the authors of contested findings may reply before the reviewers vote again on {open} finding(s). Call `wait`.", r.id)
    } else {
        format!("[{}] Cross-examination round {round}: reviewers are voting on {open} finding(s). Call `wait`.", r.id)
    };
    agents.progress(&r.author, &r.id, msg, now);
    if replying {
        return;
    }
    r.deadline = now + r.params.vote_secs * 1000;
    let names: Vec<String> = r.active_slots().map(|s| s.agent.clone()).collect();
    if names.iter().all(|a| r.owed_votes(a).is_empty()) {
        return;
    }
    for a in &names {
        let owed = r.owed_votes(a);
        if !owed.is_empty() {
            let text = format!("[{}] {}", r.id, text::vote_task(r, &owed, text::LEFT));
            agents.notify_task(a, &r.id, vote_task(r, &owed), text, Some(r.deadline), now);
        } else if round == 1 {
            let text = format!(
                "[{}] Round 1 started; nothing for you to vote on (your own findings are being discussed: you may `withdraw_finding` or `say` something). Call `wait`.\n\n{}",
                r.id,
                text::round_findings(r, &[])
            );
            agents.notify(a, &r.id, text, now);
        }
    }
}

fn finish(r: &mut Review, agents: &mut Agents, web: &str, now: Ms) {
    if r.active_slots().next().is_none() {
        fail(r, agents, "all reviewers dropped out", now);
        return;
    }
    let c = r.params.consensus;
    decide(&mut r.findings, &c);
    let verdict = verdict_of(&r.findings);
    r.phase = Phase::Done;
    r.ended = Some(now);
    let body = text::result(r, web);
    r.result = Some(ReviewResult {
        verdict,
        at: now,
        text: body.clone(),
    });
    r.log(now, "relay", "result", format!("consensus: {}", verdict.as_str()));
    agents.outcome(&r.author, &r.id, body, now);
    let names: Vec<String> = r.slots.iter().filter(|s| s.active()).map(|s| s.agent.clone()).collect();
    for a in names {
        agents.progress(
            &a,
            &r.id,
            format!("[{}] Review finished: {}. Thanks; you are free for other reviews. Call `wait`.", r.id, verdict.as_str()),
            now,
        );
    }
}

fn fail(r: &mut Review, agents: &mut Agents, why: &str, now: Ms) {
    r.phase = Phase::Failed;
    r.ended = Some(now);
    r.failure = Some(why.to_string());
    r.log(now, "relay", "failed", why);
    let (author, reviewer) = match &r.cancelled_by {
        Some(by) => (
            format!("[{}] {by} (a human) cancelled the review of {}.\nNEXT: don't request it again unless your human asks.", r.id, r.pr.url()),
            format!("[{}] Review cancelled by {by} (a human). Call `wait`.", r.id),
        ),
        None => (
            format!("[{}] Review of {} failed: {why}. You can try `request_review` again later.", r.id, r.pr.url()),
            format!("[{}] Review cancelled: {why}. Call `wait`.", r.id),
        ),
    };
    agents.outcome(&r.author, &r.id, author, now);
    let names: Vec<String> = r.active_slots().map(|s| s.agent.clone()).collect();
    for a in names {
        agents.notify(&a, &r.id, reviewer.clone(), now);
    }
}
