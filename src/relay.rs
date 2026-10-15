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
const MAX_TEXT: usize = 8000;
const MAX_FINDINGS: usize = 60;
const FORGET_AGENT_MS: Ms = 24 * 3600 * 1000;

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    pub login: String,
    pub node: String,
    pub role: Role,
    pub first_seen: Ms,
    pub last_seen: Ms,
    /// Requests currently blocked in `wait` (or a watcher) for this agent.
    #[serde(skip)]
    pub waiting: u32,
    events: VecDeque<AgentEvent>,
    next_seq: u64,
    delivered: u64,
}

pub struct Agents {
    map: BTreeMap<String, Agent>,
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
            Some(a) if a.waiting > 0 => 0,
            Some(a) => now.saturating_sub(a.last_seen),
            None => Ms::MAX,
        }
    }

    pub fn online(&self, name: &str, now: Ms) -> bool {
        self.silent_ms(name, now) < self.timing.online_secs * 1000
    }

    fn notify(&mut self, name: &str, review: &str, text: String, now: Ms) {
        self.push(name, review, text, None, false, now);
    }

    fn notify_task(&mut self, name: &str, review: &str, task: String, text: String, now: Ms) {
        self.push(name, review, text, Some(task), false, now);
    }

    fn progress(&mut self, name: &str, review: &str, text: String, now: Ms) {
        self.push(name, review, text, None, true, now);
    }

    fn push(&mut self, name: &str, review: &str, text: String, task: Option<String>, progress: bool, now: Ms) {
        if let Some(a) = self.map.get_mut(name) {
            a.next_seq += 1;
            a.events.push_back(AgentEvent {
                seq: a.next_seq,
                at: now,
                review: Some(review.to_string()),
                text,
                task,
                progress,
            });
            while a.events.len() > MAX_EVENTS {
                a.events.pop_front();
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
            .filter(|f| match f.votes.get(agent) {
                None => true,
                Some(v) => v.round < round || v.revision < f.revision,
            })
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

    pub fn deadline_left(&self, now: Ms) -> Ms {
        self.deadline.saturating_sub(now)
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

/// Merges duplicates, then applies the voting rule to every finding.
pub fn decide(findings: &mut [Finding], c: &Consensus) {
    let ids: Vec<String> = findings.iter().map(|f| f.id.clone()).collect();
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    for f in findings.iter() {
        if f.withdrawn.is_some() {
            continue;
        }
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for v in f.votes.values() {
            if let Choice::Duplicate(t) = &v.choice {
                *counts.entry(t.as_str()).or_default() += 1;
            }
        }
        let Some((target, n)) = counts.into_iter().max_by_key(|(_, n)| *n) else {
            continue;
        };
        if n * 2 <= f.votes.len() {
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
        for v in f.votes.values() {
            match v.choice {
                Choice::Disagree => disagree += 1,
                _ => rated.push(v.severity.unwrap_or(f.severity)),
            }
        }
        let agree = rated.len();
        let total = (agree + disagree) as f64;
        let mut severity = f.severity;
        let mut downgraded = false;
        rated.sort();
        let severity_split = disagree == 0 && agree == 2 && rated[0] != rated[1];
        let status = if let Some(t) = merged.get(&f.id) {
            Status::Duplicate(t.clone())
        } else if total > 0.0 && agree as f64 > c.confirm_fraction * total {
            severity = rated[agree / 2];
            let unanimous = disagree == 0 && rated.iter().all(|s| *s == Severity::Blocking);
            if severity == Severity::Blocking && c.blocking_unanimous && !unanimous {
                severity = Severity::Major;
                downgraded = true;
            }
            Status::Confirmed
        } else if total > 0.0 && disagree as f64 > c.confirm_fraction * total {
            Status::Rejected
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

pub fn verdict_of(findings: &[Finding]) -> Verdict {
    let blocks = findings.iter().any(|f| {
        f.outcome.as_ref().is_some_and(|o| {
            o.status == Status::Confirmed && matches!(o.severity, Severity::Blocking | Severity::Major)
        })
    });
    if blocks { Verdict::ChangesRequested } else { Verdict::Approve }
}

pub struct NewReview {
    pub pr_url: String,
    pub note: String,
    pub head_sha: Option<String>,
    pub reviewers: Option<usize>,
    /// Offer only to these agents; `reviewers` then defaults to how many there are.
    pub picked: Vec<String>,
    pub confirm_fraction: Option<f64>,
    pub blocking_unanimous: Option<bool>,
    pub vote_rounds: Option<u8>,
    pub offer_secs: Option<u64>,
    pub review_secs: Option<u64>,
    pub vote_secs: Option<u64>,
}

impl NewReview {
    pub fn new(pr_url: &str) -> NewReview {
        NewReview {
            pr_url: pr_url.into(),
            note: String::new(),
            head_sha: None,
            reviewers: None,
            picked: Vec::new(),
            confirm_fraction: None,
            blocking_unanimous: None,
            vote_rounds: None,
            offer_secs: None,
            review_secs: None,
            vote_secs: None,
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

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        return s.to_string();
    }
    let cut = (0..=max).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
    format!("{}…", &s[..cut])
}

fn valid_sha(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn check_url(pr: &PrRef, url: &str) -> Result<()> {
    let prefix = format!("https://github.com/{}/{}/", pr.owner, pr.repo);
    let lower = url.trim().to_ascii_lowercase();
    if !lower.starts_with(&prefix) || url.chars().any(char::is_whitespace) {
        bail!("finding url must be a line link into the PR's repository, e.g. {prefix}blob/<head_sha>/<path>#L<line> (got {url})");
    }
    Ok(())
}

fn need_text(s: &str, what: &str) -> Result<String> {
    let t = s.trim();
    if t.is_empty() {
        bail!("{what} must not be empty");
    }
    Ok(clip(t, MAX_TEXT))
}

impl State {
    pub fn new(timing: Timing, consensus: Consensus, history: usize) -> State {
        State {
            agents: Agents {
                map: BTreeMap::new(),
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
                if a.waiting > 0 {
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

    /// Records that `name` is alive and binds the name to the caller's Tailscale login.
    /// The supplied role is an initial default; only `set_role` changes an existing agent's role.
    pub fn touch(&mut self, name: &str, who: &Identity, role: Option<Role>, now: Ms) -> Result<()> {
        self.check_name(name, who, now)?;
        let timing = self.timing;
        if self.agents.get(name).is_some_and(|a| !a.login.eq_ignore_ascii_case(&who.login)) {
            self.agents.map.remove(name);
        }
        let a = self.agents.map.entry(name.to_string()).or_insert_with(|| Agent {
            name: name.to_string(),
            login: who.login.clone(),
            node: who.node.clone(),
            role: role.unwrap_or(Role::Author),
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
        a.last_seen = now;
        a.node = who.node.clone();
        Ok(())
    }

    /// Refuses `name` while another login holds it and is online or in an unfinished review.
    pub fn check_name(&self, name: &str, who: &Identity, now: Ms) -> Result<()> {
        if let Some(holder) = self.agents.get(name).map(|a| &a.login)
            && !holder.eq_ignore_ascii_case(&who.login)
            && (self.agents.online(name, now) || self.engaged(name))
        {
            bail!(
                "agent name `{name}` is in use by another tailnet user ({holder}); pick a different name, e.g. one prefixed with your own user name"
            );
        }
        Ok(())
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
                busy: self.busy_with(&a.name).map(|r| r.id.clone()),
                idle_ms: self.agents.silent_ms(&a.name, now),
            })
            .collect()
    }

    pub fn set_role(&mut self, name: &str, role: Role, now: Ms) -> Result<()> {
        let a = self.agents.map.get_mut(name).ok_or_else(|| anyhow!("unknown agent {name}"))?;
        a.role = role;
        self.advance(now);
        Ok(())
    }

    /// New events for `name`, minus tasks it has already done and, unless asked for, progress.
    pub fn take_events(&mut self, name: &str, progress: bool) -> Vec<AgentEvent> {
        let open = self.open_tasks(name);
        let Some(a) = self.agents.map.get_mut(name) else {
            return Vec::new();
        };
        let fresh: Vec<AgentEvent> = a
            .events
            .iter()
            .filter(|e| e.seq > a.delivered && (progress || !e.progress))
            .filter(|e| e.task.as_ref().is_none_or(|t| open.contains(t)))
            .cloned()
            .collect();
        a.delivered = a.next_seq;
        fresh
    }

    pub fn has_undelivered(&self, name: &str) -> bool {
        let open = self.open_tasks(name);
        self.agents.get(name).is_some_and(|a| {
            a.events
                .iter()
                .any(|e| e.seq > a.delivered && !e.progress && e.task.as_ref().is_none_or(|t| open.contains(t)))
        })
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

    /// Events after `after` without marking them delivered (for watchers).
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
                                    "{id}: your findings {} are contested. Before round {round} votes open (in {left}), `amend_finding`, `withdraw_finding`, or `say` why they stand. Then call `wait`.",
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
            {
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
        if let Some(sha) = &req.head_sha
            && !valid_sha(sha)
        {
            bail!("head_sha must be a hex commit SHA");
        }
        let mut c = self.consensus;
        if let Some(f) = req.confirm_fraction {
            if !(0.0..1.0).contains(&f) {
                bail!("confirm_fraction must be in [0, 1)");
            }
            c.confirm_fraction = f;
        }
        if let Some(b) = req.blocking_unanimous {
            c.blocking_unanimous = b;
        }
        if let Some(v) = req.vote_rounds {
            c.vote_rounds = v.clamp(1, 3);
        }
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
        let t = self.timing;
        let params = ReviewParams {
            reviewers,
            consensus: c,
            offer_secs: req.offer_secs.unwrap_or(t.offer_secs).clamp(5, 3600),
            review_secs: req.review_secs.unwrap_or(t.review_secs).clamp(10, 4 * 3600),
            vote_secs: req.vote_secs.unwrap_or(t.vote_secs).clamp(10, 4 * 3600),
            reply_secs: t.reply_secs.min(600),
        };
        self.next_review += 1;
        let mut r = Review {
            id: format!("r{}", self.next_review),
            pr,
            author: author.to_string(),
            author_login: login,
            note: clip(&req.note, MAX_TEXT),
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
        };
        let from = if r.picked.is_empty() { String::new() } else { format!(" from {}", r.picked.join(", ")) };
        r.log(now, author, "request", format!("requested {reviewers} reviewer(s){from}. {}", r.note));
        Ok(r)
    }

    /// The named reviewers, deduplicated, if all of them are online reviewers.
    fn check_picked(&self, author: &str, names: &[String], now: Ms) -> Result<Vec<String>> {
        let mut picked: Vec<String> = Vec::new();
        let mut bad = Vec::new();
        for name in names.iter().map(|n| n.trim()) {
            if picked.iter().any(|p| p == name) {
                continue;
            }
            match self.agents.get(name) {
                _ if name == author => bad.push(format!("{name} (that's you)")),
                None => bad.push(format!("{name} (unknown agent)")),
                Some(_) if !self.agents.online(name, now) => bad.push(format!("{name} (not online)")),
                Some(a) if a.role != Role::Reviewer => bad.push(format!("{name} (not a reviewer)")),
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
        let login = self.agents.get(agent).map(|a| a.login.clone()).unwrap_or_default();
        let r = self.review_mut(id)?;
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
        r.declined.remove(agent);
        r.slots.push(Slot {
            agent: agent.to_string(),
            login,
            joined: now,
            dropped: None,
            submission: None,
        });
        r.log(now, agent, "accept", "accepted the review");
        self.advance(now);
        Ok(())
    }

    pub fn decline(&mut self, agent: &str, id: &str, reason: &str, now: Ms) -> Result<()> {
        let r = self.review_mut(id)?;
        if r.phase != Phase::Offering || r.slot(agent).is_some() {
            bail!("{} has no open offer for you", r.id);
        }
        r.declined.insert(agent.to_string());
        let reason = clip(reason, 500);
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
        let r = self.review_mut(id)?;
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
            if let Some(u) = url {
                check_url(&r.pr, u).map_err(|e| anyhow!("finding {}: {e}", i + 1))?;
            }
            parsed.push((severity, url.map(String::from), need_text(&f.text, "finding text")?));
        }
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
            });
        }
        let n = findings.len();
        let summary = clip(&summary, MAX_TEXT);
        if let Some(s) = r.slot_mut(agent) {
            s.submission = Some(Submission {
                at: now,
                head_sha: head_sha.to_string(),
                verdict,
                summary,
            });
        }
        r.log(now, agent, "submit", format!("submitted {n} finding(s), suggests {} at {head_sha}", verdict.as_str()));
        self.advance(now);
        Ok(n)
    }

    pub fn vote(&mut self, agent: &str, id: &str, votes: Vec<VoteIn>, now: Ms) -> Result<Voted> {
        let r = self.review_mut(id)?;
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
        let mut parsed = Vec::new();
        let mut ignored = Vec::new();
        for v in &votes {
            let f = r.finding(&v.finding).ok_or_else(|| anyhow!("{} has no finding {}", r.id, v.finding))?;
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
            let reason = v.reason.as_deref().map(|s| clip(s, MAX_TEXT)).filter(|s| !s.is_empty());
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
            parsed.push((f.id.clone(), choice, severity, reason));
        }
        let recorded = parsed.len();
        for (fid, choice, severity, reason) in parsed {
            let line = match (&choice, severity) {
                (Choice::Agree, Some(s)) => format!("{fid}: agree, as {}", s.as_str()),
                (Choice::Agree, None) => format!("{fid}: agree"),
                (Choice::Disagree, _) => format!("{fid}: disagree"),
                (Choice::Duplicate(t), _) => format!("{fid}: duplicate of {t}"),
            } + &reason.as_deref().map(|s| format!(" — {s}")).unwrap_or_default();
            let f = r.findings.iter_mut().find(|f| f.id == fid).expect("validated");
            f.votes.insert(
                agent.to_string(),
                Vote {
                    choice,
                    severity,
                    reason,
                    round,
                    revision: f.revision,
                    at: now,
                },
            );
            r.log(now, agent, "vote", line);
        }
        self.advance(now);
        Ok(Voted { recorded, ignored })
    }

    pub fn say(&mut self, agent: &str, id: &str, msg: &str, now: Ms) -> Result<()> {
        let msg = need_text(msg, "message")?;
        let r = self.review_mut(id)?;
        let reviewer = r.slot(agent).is_some_and(Slot::active);
        if !reviewer && r.author != agent {
            bail!("only the author and active reviewers of {} can talk in it", r.id);
        }
        if reviewer && matches!(r.phase, Phase::Offering | Phase::Reviewing) {
            bail!("reviewers can discuss once findings are revealed (cross-examination); work independently until then");
        }
        r.log(now, agent, "say", msg.clone());
        if r.replying {
            r.replied.insert(agent.to_string());
        }
        let to = r.others(agent);
        let rid = r.id.clone();
        for a in to {
            self.agents.notify(&a, &rid, format!("[{rid}] {agent} says:\n{msg}"), now);
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
        let r = self.review_mut(id)?;
        let pr = r.pr.clone();
        let round = r.round();
        let severity = severity
            .map(|s| Severity::parse(s).ok_or_else(|| anyhow!("severity must be blocking, major, minor or nit")))
            .transpose()?;
        let url = url.map(str::trim).filter(|u| !u.is_empty());
        if let Some(u) = url {
            check_url(&pr, u)?;
        }
        let new_text = new_text.map(|t| need_text(t, "text")).transpose()?;
        if severity.is_none() && url.is_none() && new_text.is_none() {
            bail!("nothing to amend: give severity, url and/or text");
        }
        let f = Self::own_finding(r, agent, fid)?;
        if let Some(s) = severity {
            f.severity = s;
        }
        if let Some(u) = url {
            f.url = Some(u.to_string());
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
        let author = r.author.clone();
        let replying = r.replying;
        for a in r.others(agent) {
            let text = format!("[{rid}] {agent} amended {fid}; votes on it are reopened.");
            if a == author || replying {
                self.agents.progress(&a, &rid, text, now);
            } else {
                self.agents.notify(&a, &rid, text, now);
            }
        }
        self.advance(now);
        Ok(())
    }

    pub fn withdraw(&mut self, agent: &str, id: &str, fid: &str, reason: &str, now: Ms) -> Result<()> {
        let r = self.review_mut(id)?;
        let f = Self::own_finding(r, agent, fid)?;
        let reason = clip(reason, 1000);
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
        if prev.phase != Phase::Done {
            bail!("{} has not finished ({})", prev.id, prev.phase.label());
        }
        if let Some(r) = self.reviews.iter().find(|r| r.parent.as_deref() == Some(&prev.id)) {
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
        req.head_sha = Some(head_sha.to_string());
        req.reviewers = Some(prev.params.reviewers);
        req.confirm_fraction = Some(prev.params.consensus.confirm_fraction);
        req.blocking_unanimous = Some(prev.params.consensus.blocking_unanimous);
        req.vote_rounds = Some(prev.params.consensus.vote_rounds);
        req.offer_secs = Some(prev.params.offer_secs);
        req.review_secs = Some(prev.params.review_secs);
        req.vote_secs = Some(prev.params.vote_secs);
        let mut r = self.new_review(author, req, now)?;
        r.params.reply_secs = prev.params.reply_secs;
        let busy: HashSet<String> = self
            .reviews
            .iter()
            .filter(|r| r.phase.active())
            .flat_map(|r| r.active_slots().map(|s| s.agent.clone()))
            .collect();
        let candidates: Vec<(String, String)> = prev
            .slots
            .iter()
            .filter(|s| {
                self.agents.online(&s.agent, now)
                    && !busy.contains(&s.agent)
                    && self.agents.get(&s.agent).is_some_and(|a| a.role == Role::Reviewer && a.login == s.login)
            })
            .map(|s| (s.agent.clone(), s.login.clone()))
            .collect();
        r.parent = Some(prev.id.clone());
        r.iteration = prev.iteration + 1;
        r.since_sha = reviewed;
        for f in prev.findings.iter() {
            let Some(o) = &f.outcome else { continue };
            if !matches!(o.status, Status::Confirmed | Status::Disputed) {
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
            });
        }
        for (agent, login) in candidates.into_iter().take(r.params.reviewers) {
            r.slots.push(Slot {
                agent: agent.clone(),
                login,
                joined: now,
                dropped: None,
                submission: None,
            });
            r.log(now, &agent, "accept", format!("assigned again (reviewed {})", prev.id));
        }
        let left = format!("at least {}", text::dur(r.params.review_secs * 1000));
        for s in &r.slots {
            let text = format!("[{}] You are assigned again (you reviewed {}).\n{}", r.id, prev.id, text::review_task(&r, &left));
            self.agents.notify_task(&s.agent, &r.id, review_task(&r.id), text, now);
        }
        let new_id = r.id.clone();
        self.reviews.push(r);
        self.advance(now);
        Ok(new_id)
    }

    /// Applies deadlines and presence; call about once a second. Returns true if anything changed.
    pub fn tick(&mut self, now: Ms) -> bool {
        let before = self.fingerprint(now);
        self.advance(now);
        let finished: Vec<usize> = self
            .reviews
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.phase.active())
            .map(|(i, _)| i)
            .collect();
        if finished.len() > self.history {
            let drop: HashSet<usize> = finished[..finished.len() - self.history].iter().copied().collect();
            let mut i = 0;
            self.reviews.retain(|_| {
                i += 1;
                !drop.contains(&(i - 1))
            });
        }
        let engaged: HashSet<String> = self
            .reviews
            .iter()
            .filter(|r| r.phase.active())
            .flat_map(|r| std::iter::once(r.author.clone()).chain(r.slots.iter().map(|s| s.agent.clone())))
            .collect();
        self.agents.map.retain(|name, a| {
            engaged.contains(name) || a.waiting > 0 || now.saturating_sub(a.last_seen) < FORGET_AGENT_MS
        });
        before != self.fingerprint(now)
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

    fn advance(&mut self, now: Ms) {
        for i in 0..self.reviews.len() {
            for _ in 0..8 {
                if !self.step(i, now) {
                    break;
                }
            }
        }
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
                            && (r.picked.is_empty() || r.picked.contains(&a.name))
                    })
                    .map(|a| a.name.clone())
                    .collect();
                for a in eligible {
                    r.offered.insert(a.clone());
                    agents.notify_task(&a, &r.id, offer_task(&r.id), text::offer(r, now), now);
                    changed = true;
                }
                let n = r.active_slots().count();
                if n >= r.params.reviewers || (now >= r.deadline && n > 0) {
                    start_reviewing(r, agents, now);
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
                let submitted = r.slots.iter().filter(|s| s.submission.is_some()).count();
                let pending = r.active_slots().filter(|s| s.submission.is_none()).count();
                if pending == 0 {
                    if submitted == 0 {
                        fail(r, agents, "all reviewers dropped out before submitting findings", now);
                    } else {
                        start_voting(r, agents, 1, &web, now);
                    }
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
                let owing = |r: &Review| -> Vec<String> {
                    r.active_slots()
                        .filter(|s| !r.owed_votes(&s.agent).is_empty())
                        .map(|s| s.agent.clone())
                        .collect()
                };
                if now >= r.deadline {
                    for a in owing(r) {
                        drop_slot(r, agents, &a, &format!("missed the round {round} voting deadline"), now);
                    }
                    changed = true;
                }
                if owing(r).is_empty() {
                    let contested: Vec<usize> = r
                        .findings
                        .iter()
                        .enumerate()
                        .filter(|(_, f)| {
                            let disagree = f.votes.values().any(|v| v.choice == Choice::Disagree);
                            let agree = f.by.is_some() || f.votes.values().any(|v| v.choice != Choice::Disagree);
                            let rerated = f.votes.values().any(|v| v.severity.is_some());
                            f.withdrawn.is_none() && agree && (disagree || rerated)
                        })
                        .map(|(i, _)| i)
                        .collect();
                    let voters = r.active_slots().count();
                    if round < r.params.consensus.vote_rounds && !contested.is_empty() && voters > 0 {
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
    let left = text::dur(r.deadline_left(now));
    agents.progress(
        &r.author,
        &r.id,
        format!(
            "[{}] Reviewers assigned: {} (of {} requested). Independent review phase, up to {left}. Call `wait` for the result.",
            r.id,
            names.join(", "),
            r.params.reviewers
        ),
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
    let left = text::dur(r.deadline_left(now));
    for a in &authors {
        let text = format!("[{}] {}", r.id, text::reply_task(r, a, &left));
        agents.notify_task(a, &r.id, reply_task(r), text, now);
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
    let left = text::dur(r.deadline_left(now));
    for a in &names {
        let owed = r.owed_votes(a);
        if !owed.is_empty() {
            let text = format!("[{}] {}", r.id, text::vote_task(r, &owed, &left));
            agents.notify_task(a, &r.id, vote_task(r, &owed), text, now);
        } else if round == 1 {
            let text = format!(
                "[{}] Round 1 started; nothing for you to vote on (your own findings are being discussed: you may `amend_finding` or `withdraw_finding`, or `say` something). Call `wait`.\n\n{}",
                r.id,
                text::round_findings(r, &[])
            );
            agents.notify(a, &r.id, text, now);
        }
    }
}

fn finish(r: &mut Review, agents: &mut Agents, web: &str, now: Ms) {
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
    agents.notify(&r.author, &r.id, body, now);
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
    agents.notify(&r.author, &r.id, format!("[{}] Review of {} failed: {why}. You can try `request_review` again later.", r.id, r.pr.url()), now);
    let names: Vec<String> = r.active_slots().map(|s| s.agent.clone()).collect();
    for a in names {
        agents.notify(&a, &r.id, format!("[{}] Review cancelled: {why}. Call `wait`.", r.id), now);
    }
}
