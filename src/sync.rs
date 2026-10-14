use crate::model::{Message, now_ms};
use crate::node::Node;
use crate::store::VersionVector;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const PROTOCOL: u32 = 1;
const PAGE: usize = 500;
const MAX_PAGES: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub app: String,
    pub protocol: u32,
    pub node_id: String,
    pub machine: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRequest {
    pub node_id: String,
    pub machine: String,
    pub vv: VersionVector,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponse {
    pub node_id: String,
    pub machine: String,
    pub vv: VersionVector,
    pub messages: Vec<Message>,
    pub more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushRequest {
    pub node_id: String,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushResponse {
    pub inserted: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerSource {
    Static,
    Tailnet,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerState {
    pub url: String,
    pub source: PeerSource,
    pub node_id: Option<String>,
    pub machine: Option<String>,
    pub last_ok: Option<i64>,
    pub last_err: Option<String>,
    pub vv: VersionVector,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundPeer {
    pub node_id: String,
    pub machine: String,
    pub addr: String,
    pub last_seen: i64,
}

#[derive(Default)]
pub struct Peers {
    outbound: Mutex<BTreeMap<String, PeerState>>,
    inbound: Mutex<BTreeMap<String, InboundPeer>>,
}

impl Peers {
    pub fn set_static(&self, urls: &[String]) {
        let mut out = self.outbound.lock().unwrap();
        for u in urls {
            let url = u.trim_end_matches('/').to_string();
            out.entry(url.clone()).or_insert_with(|| new_peer(url, PeerSource::Static));
        }
    }

    fn set_discovered(&self, urls: Vec<String>) {
        let mut out = self.outbound.lock().unwrap();
        out.retain(|u, p| p.source == PeerSource::Static || urls.contains(u));
        for url in urls {
            out.entry(url.clone()).or_insert_with(|| new_peer(url, PeerSource::Tailnet));
        }
    }

    pub fn outbound(&self) -> Vec<PeerState> {
        self.outbound.lock().unwrap().values().cloned().collect()
    }

    pub fn inbound(&self) -> Vec<InboundPeer> {
        self.inbound.lock().unwrap().values().cloned().collect()
    }

    pub fn saw_inbound(&self, node_id: &str, machine: &str, addr: &str) {
        self.inbound.lock().unwrap().insert(
            node_id.to_string(),
            InboundPeer {
                node_id: node_id.to_string(),
                machine: machine.to_string(),
                addr: addr.to_string(),
                last_seen: now_ms(),
            },
        );
    }

    fn update(&self, url: &str, f: impl FnOnce(&mut PeerState)) {
        if let Some(p) = self.outbound.lock().unwrap().get_mut(url) {
            f(p);
        }
    }
}

fn new_peer(url: String, source: PeerSource) -> PeerState {
    PeerState {
        url,
        source,
        node_id: None,
        machine: None,
        last_ok: None,
        last_err: None,
        vv: VersionVector::new(),
    }
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(20))
        .build()
        .expect("http client")
}

pub fn hello(node: &Node) -> Hello {
    Hello {
        app: "amx".into(),
        protocol: PROTOCOL,
        node_id: node.node_id.clone(),
        machine: node.machine.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
    }
}

pub fn handle_sync(node: &Node, req: &SyncRequest) -> Result<SyncResponse> {
    let (messages, more) = node.store.missing_for(&req.vv, PAGE)?;
    Ok(SyncResponse {
        node_id: node.node_id.clone(),
        machine: node.machine.clone(),
        vv: node.store.version_vector()?,
        messages,
        more,
    })
}

pub fn handle_push(node: &Node, req: &PushRequest) -> Result<PushResponse> {
    let new = node.store.insert_remote(&req.messages)?;
    node.on_new_messages(&new);
    Ok(PushResponse { inserted: new.len() })
}

/// One anti-entropy exchange: pull what the peer has that we lack, then push what it lacks,
/// so a link that only works in one direction still converges both ways.
pub async fn sync_with(node: &Node, http: &reqwest::Client, url: &str) -> Result<(usize, usize)> {
    let mut pulled = 0;
    let mut theirs;
    let mut pages = 0;
    loop {
        let req = SyncRequest {
            node_id: node.node_id.clone(),
            machine: node.machine.clone(),
            vv: node.store.version_vector()?,
        };
        let resp: SyncResponse = http
            .post(format!("{url}/api/sync"))
            .json(&req)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if resp.node_id == node.node_id {
            bail!("peer is this node");
        }
        let new = node.store.insert_remote(&resp.messages)?;
        pulled += new.len();
        node.on_new_messages(&new);
        node.peers.update(url, |p| {
            p.node_id = Some(resp.node_id.clone());
            p.machine = Some(resp.machine.clone());
        });
        theirs = resp.vv;
        pages += 1;
        if !resp.more || pages >= MAX_PAGES {
            break;
        }
    }
    let mut pushed = 0;
    for _ in 0..MAX_PAGES {
        let (msgs, more) = node.store.missing_for(&theirs, PAGE)?;
        if msgs.is_empty() {
            break;
        }
        for m in &msgs {
            let e = theirs.entry(m.origin.clone()).or_insert(0);
            *e = (*e).max(m.seq);
        }
        let resp: PushResponse = http
            .post(format!("{url}/api/push"))
            .json(&PushRequest {
                node_id: node.node_id.clone(),
                messages: msgs,
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        pushed += resp.inserted;
        if !more {
            break;
        }
    }
    node.peers.update(url, |p| {
        p.last_ok = Some(now_ms());
        p.last_err = None;
        p.vv = theirs;
    });
    Ok((pulled, pushed))
}

pub async fn sync_all(node: &Arc<Node>, http: &reqwest::Client) {
    let peers = node.peers.outbound();
    let jobs = peers.into_iter().map(|p| {
        let node = node.clone();
        let http = http.clone();
        async move {
            match sync_with(&node, &http, &p.url).await {
                Ok((pulled, pushed)) if pulled + pushed > 0 => {
                    tracing::info!("synced with {}: pulled {pulled}, pushed {pushed}", p.url)
                }
                Ok(_) => {}
                Err(e) => {
                    let msg = format!("{e:#}");
                    if p.last_err.as_deref() != Some(msg.as_str()) {
                        tracing::warn!("sync with {} failed: {msg}", p.url);
                    }
                    node.peers.update(&p.url, |s| s.last_err = Some(msg));
                }
            }
        }
    });
    futures_util::future::join_all(jobs).await;
}

pub async fn sync_loop(node: Arc<Node>) {
    let http = client();
    let interval = Duration::from_secs(node.cfg.sync_interval_secs.max(1));
    loop {
        sync_all(&node, &http).await;
        tokio::select! {
            _ = node.sync_now.notified() => {}
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

async fn probe(http: &reqwest::Client, url: &str) -> Option<Hello> {
    let h: Hello = http
        .get(format!("{url}/api/hello"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    (h.app == "amx").then_some(h)
}

/// Peers are online tailnet machines that answer amx's hello on the configured port.
pub async fn discover_loop(node: Arc<Node>) {
    let http = client();
    let interval = Duration::from_secs(node.cfg.discover_interval_secs.max(5));
    loop {
        match node.ts.status().await {
            Ok(status) => {
                let cands: Vec<String> = status
                    .peers
                    .iter()
                    .filter(|p| p.online)
                    .filter_map(|p| p.ips.iter().find(|ip| matches!(ip, IpAddr::V4(_))))
                    .map(|ip| format!("http://{ip}:{}", node.cfg.port))
                    .collect();
                let probes = cands.into_iter().map(|url| {
                    let http = http.clone();
                    async move { probe(&http, &url).await.map(|h| (url, h)) }
                });
                let found: Vec<String> = futures_util::future::join_all(probes)
                    .await
                    .into_iter()
                    .flatten()
                    .filter(|(_, h)| h.node_id != node.node_id)
                    .map(|(url, _)| url)
                    .collect();
                let before: Vec<String> = node.peers.outbound().into_iter().map(|p| p.url).collect();
                if found.iter().any(|u| !before.contains(u)) {
                    node.sync_now.notify_one();
                }
                node.peers.set_discovered(found);
            }
            Err(e) => tracing::warn!("tailscale status: {e:#}"),
        }
        tokio::time::sleep(interval).await;
    }
}
