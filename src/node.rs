use crate::config::Config;
use crate::gh::Gh;
use crate::model::{
    Author, AuthorKind, Fields, Finding, Hlc, Kind, Message, Status, Verdict, new_id, now_ms,
    thread_status,
};
use crate::store::{MarkTable, PrMeta, Store};
use crate::sync::Peers;
use crate::tailscale::Tailscale;
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, watch};

const META_TTL_MS: i64 = 5 * 60 * 1000;
const POST_GH_TIMEOUT: Duration = Duration::from_secs(3);

pub struct Node {
    pub store: Store,
    pub cfg: Config,
    pub node_id: String,
    pub machine: String,
    pub local_user: String,
    pub hlc: Hlc,
    pub gh: Gh,
    pub ts: Tailscale,
    pub peers: Peers,
    pub sync_now: Notify,
    pub listening: Mutex<Vec<String>>,
    pub shutdown: watch::Sender<bool>,
    changes: watch::Sender<u64>,
    gh_inflight: Mutex<HashSet<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PostInput {
    pub kind: Option<Kind>,
    pub pr_url: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub verdict: Option<Verdict>,
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub head_sha: Option<String>,
    #[serde(default)]
    pub in_reply_to: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    AddressedToYou,
    YourThread,
    OpenRequest,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::AddressedToYou => "addressed to you",
            Reason::YourThread => "on a thread you are in",
            Reason::OpenRequest => "open request, anyone can take it",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxItem {
    pub reason: Reason,
    pub title: Option<String>,
    pub message: Message,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub thread: String,
    pub short: String,
    pub title: Option<String>,
    pub status: Status,
    pub messages: usize,
    pub last_hlc: i64,
    pub last_author: String,
    pub last_kind: Kind,
    pub participants: Vec<String>,
    pub requested_to: Option<String>,
    pub pr_state: Option<String>,
    pub head_sha: Option<String>,
    pub latest_review_stale: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadView {
    pub summary: ThreadSummary,
    pub meta: Option<PrMeta>,
    pub messages: Vec<Message>,
    /// IDs of reviews whose head SHA differs from the PR's current head.
    pub stale_reviews: Vec<String>,
}

/// Unread items for `me`: anything addressed to it, anything by others on threads it has
/// posted in, and unaddressed requests on threads that are not closed.
pub fn inbox_for(msgs: &[Message], me: &Author, read: &HashSet<String>) -> Vec<(Message, Reason)> {
    let is_me = |a: &Author| a.kind == me.kind && a.id().eq_ignore_ascii_case(&me.id());
    let mut by_thread: BTreeMap<&str, Vec<&Message>> = BTreeMap::new();
    for m in msgs {
        by_thread.entry(m.thread.as_str()).or_default().push(m);
    }
    let mine: HashSet<&str> = msgs
        .iter()
        .filter(|m| is_me(&m.author))
        .map(|m| m.thread.as_str())
        .collect();
    let closed: HashSet<&str> = by_thread
        .iter()
        .filter(|(_, ms)| {
            let owned: Vec<Message> = ms.iter().map(|m| (*m).clone()).collect();
            thread_status(&owned) == Status::Closed
        })
        .map(|(t, _)| *t)
        .collect();
    let mut out = Vec::new();
    for m in msgs {
        if is_me(&m.author) || read.contains(&m.id) {
            continue;
        }
        let to = m.fields.to.as_deref().filter(|t| !t.trim().is_empty());
        let reason = if to.is_some_and(|t| me.matches(t)) {
            Reason::AddressedToYou
        } else if mine.contains(m.thread.as_str()) {
            Reason::YourThread
        } else if m.kind == Kind::Request && to.is_none() && !closed.contains(m.thread.as_str()) {
            Reason::OpenRequest
        } else {
            continue;
        };
        out.push((m.clone(), reason));
    }
    out
}

pub fn summarize(thread: &str, msgs: &[Message], meta: Option<&PrMeta>) -> Option<ThreadSummary> {
    let last = msgs.last()?;
    let mut participants: Vec<String> = Vec::new();
    for m in msgs {
        let d = m.author.display();
        if !participants.contains(&d) {
            participants.push(d);
        }
    }
    let title = meta
        .and_then(|m| m.title.clone())
        .or_else(|| msgs.iter().rev().find_map(|m| m.fields.title.clone()));
    let requested_to = msgs
        .iter()
        .rev()
        .find(|m| m.kind == Kind::Request)
        .and_then(|m| m.fields.to.clone());
    let latest_review = msgs.iter().rev().find(|m| m.kind == Kind::Review);
    Some(ThreadSummary {
        thread: thread.to_string(),
        short: crate::prurl::parse(thread).map(|p| p.short()).unwrap_or_default(),
        title,
        status: thread_status(msgs),
        messages: msgs.len(),
        last_hlc: last.hlc,
        last_author: last.author.display(),
        last_kind: last.kind,
        participants,
        requested_to,
        pr_state: meta.and_then(|m| m.state.clone()),
        head_sha: meta.and_then(|m| m.head_sha.clone()),
        latest_review_stale: latest_review.is_some_and(|r| is_stale(r, meta)),
    })
}

pub fn is_stale(review: &Message, meta: Option<&PrMeta>) -> bool {
    match (review.fields.head_sha.as_deref(), meta.and_then(|m| m.head_sha.as_deref())) {
        (Some(reviewed), Some(current)) => !reviewed.eq_ignore_ascii_case(current),
        _ => false,
    }
}

fn clean_opt(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn is_http_url(s: &str) -> bool {
    s.starts_with("https://") || s.starts_with("http://")
}

impl Node {
    pub fn new(cfg: Config, store: Store, machine: String) -> Result<Arc<Node>> {
        let node_id = store.node_id()?;
        let hlc = Hlc::new(store.max_hlc()?);
        let ts_bin = if cfg.tailnet || cfg.tailnet_listen.is_some() {
            crate::tailscale::find_bin(cfg.tailscale_bin.as_deref())
        } else {
            None
        };
        let local_user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "local".into());
        Ok(Arc::new(Node {
            gh: Gh::new(cfg.gh_bin.clone()),
            ts: Tailscale::new(ts_bin),
            peers: Peers::default(),
            store,
            node_id,
            machine,
            local_user,
            hlc,
            sync_now: Notify::new(),
            listening: Mutex::new(Vec::new()),
            shutdown: watch::channel(false).0,
            changes: watch::channel(0).0,
            gh_inflight: Mutex::new(HashSet::new()),
            cfg,
        }))
    }

    pub fn agent(&self, name: &str) -> Author {
        Author::agent(name, &self.machine)
    }

    pub fn local_human(&self) -> Author {
        Author {
            kind: AuthorKind::Human,
            name: self.local_user.clone(),
            machine: self.machine.clone(),
            login: None,
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub fn on_new_messages(&self, msgs: &[Message]) {
        if msgs.is_empty() {
            return;
        }
        for m in msgs {
            self.hlc.observe(m.hlc);
        }
        self.changes.send_modify(|v| *v += 1);
        self.sync_now.notify_one();
    }

    pub fn notify_local_change(&self) {
        self.changes.send_modify(|v| *v += 1);
    }

    async fn meta_for_post(&self, thread: &str) -> Option<PrMeta> {
        let cached = self.store.pr_meta(thread).ok().flatten();
        if let Some(m) = &cached
            && m.error.is_none()
            && now_ms() - m.fetched_at < META_TTL_MS
        {
            return cached;
        }
        match self.gh.view(thread, POST_GH_TIMEOUT).await {
            Ok(m) => {
                let _ = self.store.set_pr_meta(thread, &m);
                self.notify_local_change();
                Some(m)
            }
            Err(e) => {
                tracing::info!("gh lookup for {thread} failed: {e}");
                cached
            }
        }
    }

    pub async fn post(&self, author: Author, input: PostInput) -> Result<Message> {
        let kind = input.kind.ok_or_else(|| anyhow!("kind is required"))?;
        let thread = crate::prurl::canonical(&input.pr_url).ok_or_else(|| {
            anyhow!(
                "not a GitHub pull request URL: {:?} (expected https://github.com/<owner>/<repo>/pull/<n>)",
                input.pr_url
            )
        })?;
        let body = input.body.trim().to_string();
        let mut fields = Fields {
            to: clean_opt(input.to),
            ..Default::default()
        };
        match kind {
            Kind::Review => {
                fields.verdict = Some(
                    input
                        .verdict
                        .ok_or_else(|| anyhow!("a review needs a verdict: approve, request_changes or comment"))?,
                );
                if body.is_empty() {
                    bail!("a review needs a summary");
                }
                for f in &input.findings {
                    if f.text.trim().is_empty() {
                        bail!("every finding needs text");
                    }
                    if f.url.as_deref().is_some_and(|u| !is_http_url(u)) {
                        bail!("finding url must be an http(s) link: {:?}", f.url);
                    }
                }
                fields.findings = input.findings;
            }
            Kind::Reply if body.is_empty() => bail!("a reply needs text"),
            _ if input.verdict.is_some() || !input.findings.is_empty() => {
                bail!("verdict and findings are only allowed on reviews")
            }
            _ => {}
        }
        if let Some(parent) = clean_opt(input.in_reply_to) {
            match self.store.get(&parent)? {
                Some(p) if p.thread == thread => fields.in_reply_to = Some(parent),
                _ => bail!("in_reply_to {parent} is not a message on this thread"),
            }
        }
        if body.len() > crate::model::MAX_BODY {
            bail!("body is too long");
        }
        if matches!(kind, Kind::Request | Kind::Review) {
            let meta = self.meta_for_post(&thread).await;
            if kind == Kind::Request {
                fields.title = meta.as_ref().and_then(|m| m.title.clone());
            }
            fields.head_sha = clean_opt(input.head_sha).or_else(|| meta.and_then(|m| m.head_sha));
        }
        let msg = self.store.insert_local(&self.node_id, |seq| Message {
            id: new_id(),
            origin: self.node_id.clone(),
            seq,
            hlc: self.hlc.now(),
            thread: thread.clone(),
            kind,
            author,
            body,
            fields,
        })?;
        self.changes.send_modify(|v| *v += 1);
        self.sync_now.notify_one();
        Ok(msg)
    }

    pub fn inbox(&self, me: &Author, only_undelivered: bool) -> Result<Vec<InboxItem>> {
        let msgs = self.store.all()?;
        let mut skip = self.store.marks(MarkTable::Read, &me.id())?;
        if only_undelivered {
            skip.extend(self.store.marks(MarkTable::Delivered, &me.id())?);
        }
        Ok(inbox_for(&msgs, me, &skip)
            .into_iter()
            .map(|(m, reason)| InboxItem {
                reason,
                title: self.title_for(&m.thread, &msgs),
                message: m,
            })
            .collect())
    }

    fn title_for(&self, thread: &str, msgs: &[Message]) -> Option<String> {
        self.store
            .pr_meta(thread)
            .ok()
            .flatten()
            .and_then(|m| m.title)
            .or_else(|| {
                msgs.iter()
                    .filter(|m| m.thread == thread)
                    .find_map(|m| m.fields.title.clone())
            })
    }

    pub fn mark(&self, table: MarkTable, me: &Author, ids: &[String]) -> Result<usize> {
        let n = self.store.mark(table, &me.id(), ids)?;
        if table == MarkTable::Read && n > 0 {
            self.notify_local_change();
        }
        Ok(n)
    }

    /// Long-polls until `me` has unread items or the timeout passes.
    pub async fn wait_inbox(&self, me: &Author, timeout: Duration, only_undelivered: bool) -> Result<Vec<InboxItem>> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut rx = self.subscribe();
        loop {
            rx.borrow_and_update();
            let items = self.inbox(me, only_undelivered)?;
            if !items.is_empty() {
                return Ok(items);
            }
            if tokio::time::timeout_at(deadline, rx.changed()).await.is_err() {
                return Ok(Vec::new());
            }
        }
    }

    pub fn threads(self: &Arc<Self>) -> Result<Vec<ThreadSummary>> {
        let msgs = self.store.all()?;
        let mut by_thread: BTreeMap<String, Vec<Message>> = BTreeMap::new();
        for m in msgs {
            by_thread.entry(m.thread.clone()).or_default().push(m);
        }
        let mut out = Vec::new();
        for (thread, ms) in by_thread {
            let meta = self.store.pr_meta(&thread)?;
            self.refresh_meta_if_stale(&thread, meta.as_ref());
            if let Some(s) = summarize(&thread, &ms, meta.as_ref()) {
                out.push(s);
            }
        }
        out.sort_by_key(|t| std::cmp::Reverse(t.last_hlc));
        Ok(out)
    }

    pub fn thread(self: &Arc<Self>, url: &str) -> Result<Option<ThreadView>> {
        let thread = crate::prurl::canonical(url).ok_or_else(|| anyhow!("not a GitHub pull request URL: {url:?}"))?;
        let msgs = self.store.thread(&thread)?;
        let meta = self.store.pr_meta(&thread)?;
        self.refresh_meta_if_stale(&thread, meta.as_ref());
        let Some(summary) = summarize(&thread, &msgs, meta.as_ref()) else {
            return Ok(None);
        };
        let stale_reviews = msgs
            .iter()
            .filter(|m| m.kind == Kind::Review && is_stale(m, meta.as_ref()))
            .map(|m| m.id.clone())
            .collect();
        Ok(Some(ThreadView {
            summary,
            meta,
            messages: msgs,
            stale_reviews,
        }))
    }

    pub fn refresh_meta_if_stale(self: &Arc<Self>, thread: &str, meta: Option<&PrMeta>) {
        if meta.is_some_and(|m| now_ms() - m.fetched_at < META_TTL_MS) {
            return;
        }
        if !self.gh_inflight.lock().unwrap().insert(thread.to_string()) {
            return;
        }
        let node = self.clone();
        let thread = thread.to_string();
        tokio::spawn(async move {
            node.refresh_meta(&thread).await;
            node.gh_inflight.lock().unwrap().remove(&thread);
        });
    }

    pub async fn refresh_meta(&self, thread: &str) {
        let meta = match self.gh.view(thread, Duration::from_secs(20)).await {
            Ok(m) => m,
            Err(e) => {
                let mut old = self.store.pr_meta(thread).ok().flatten().unwrap_or_default();
                old.fetched_at = now_ms();
                old.error = Some(e.to_string());
                old
            }
        };
        let before = self.store.pr_meta(thread).ok().flatten();
        let _ = self.store.set_pr_meta(thread, &meta);
        if before.map(|b| (b.head_sha, b.title, b.state)) != Some((meta.head_sha, meta.title, meta.state)) {
            self.notify_local_change();
        }
    }
}
