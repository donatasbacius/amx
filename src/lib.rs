pub mod client;
pub mod config;
pub mod gh;
pub mod hooks;
pub mod mcp;
pub mod model;
pub mod node;
pub mod prurl;
pub mod server;
pub mod store;
pub mod sync;
pub mod tailscale;
pub mod text;
pub mod web;

use anyhow::{Context, Result};
use config::Config;
use node::Node;
use server::Listener;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

pub struct Running {
    pub node: Arc<Node>,
    pub local_addr: SocketAddr,
    tasks: Vec<JoinHandle<()>>,
}

impl Running {
    pub async fn stop(self) {
        let _ = self.node.shutdown.send(true);
        for mut t in self.tasks {
            if tokio::time::timeout(Duration::from_millis(500), &mut t).await.is_err() {
                t.abort();
            }
        }
    }
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| crate::tailscale::first_label(s.trim()).to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "machine".into())
}

fn clean_machine(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    s.trim_matches('-').to_string()
}

/// Picked once and remembered, so agent identities (`name@machine`) survive Tailscale outages.
async fn machine_name(cfg: &Config, store: &store::Store, ts: &tailscale::Tailscale) -> Result<String> {
    if let Some(m) = cfg.machine.as_deref().map(clean_machine).filter(|m| !m.is_empty()) {
        store.set_meta("machine", &m)?;
        return Ok(m);
    }
    if let Some(m) = store.meta("machine")? {
        return Ok(m);
    }
    let from_ts = if cfg.tailnet {
        ts.status().await.ok().map(|s| clean_machine(&s.name))
    } else {
        None
    };
    let m = from_ts.filter(|m| !m.is_empty()).unwrap_or_else(|| clean_machine(&hostname()));
    store.set_meta("machine", &m)?;
    Ok(m)
}

fn spawn_server(node: &Arc<Node>, listener: tokio::net::TcpListener, kind: Listener) -> JoinHandle<()> {
    let app = server::router(node.clone(), kind);
    let mut down = node.shutdown.subscribe();
    tokio::spawn(async move {
        let res = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = down.wait_for(|d| *d).await;
        })
        .await;
        if let Err(e) = res {
            tracing::error!("server error: {e}");
        }
    })
}

async fn tailnet_addr(node: &Node) -> Result<SocketAddr> {
    if let Some(a) = node.cfg.tailnet_listen {
        return Ok(a);
    }
    let status = node.ts.status().await?;
    let ip = status
        .ips
        .iter()
        .find(|ip| matches!(ip, IpAddr::V4(_)))
        .copied()
        .context("no Tailscale IPv4 address")?;
    Ok(SocketAddr::new(ip, node.cfg.port))
}

/// Binds the tailnet listener, retrying until Tailscale is up (e.g. at boot).
async fn tailnet_listener_loop(node: Arc<Node>) {
    let mut warned = false;
    loop {
        match tailnet_addr(&node).await {
            Ok(addr) => match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => {
                    tracing::info!("listening on {addr} (tailnet, whois identity)");
                    node.listening.lock().unwrap().push(format!("http://{addr} (tailnet)"));
                    match node.ts.whois(addr.ip()).await {
                        Some(id) => tracing::info!("whois self check: {} on {}", id.login, id.node),
                        None => tracing::warn!("whois of our own address failed; tailnet callers will be refused"),
                    }
                    let h = spawn_server(&node, l, Listener::Tailnet);
                    let _ = h.await;
                    return;
                }
                Err(e) if !warned => {
                    tracing::warn!("cannot bind {addr}: {e}; retrying");
                    warned = true;
                }
                Err(_) => {}
            },
            Err(e) if !warned => {
                tracing::warn!("tailnet listener not started: {e:#}; retrying");
                warned = true;
            }
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn meta_loop(node: Arc<Node>) {
    loop {
        tokio::time::sleep(Duration::from_secs(300)).await;
        if let Ok(threads) = node.threads() {
            for t in threads.iter().filter(|t| t.status != model::Status::Closed) {
                node.refresh_meta(&t.thread).await;
            }
        }
    }
}

pub async fn start(cfg: Config) -> Result<Running> {
    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("creating {}", cfg.data_dir.display()))?;
    let store = store::Store::open(&cfg.data_dir.join("amx.db"))?;
    let probe_ts = tailscale::Tailscale::new(if cfg.tailnet {
        tailscale::find_bin(cfg.tailscale_bin.as_deref())
    } else {
        None
    });
    let machine = machine_name(&cfg, &store, &probe_ts).await?;
    let node = Node::new(cfg, store, machine)?;
    node.peers.set_static(&node.cfg.peers);

    let local_addr = SocketAddr::from(([127, 0, 0, 1], node.cfg.port));
    let local = tokio::net::TcpListener::bind(local_addr)
        .await
        .with_context(|| format!("binding {local_addr}"))?;
    let local_addr = local.local_addr()?;
    node.listening.lock().unwrap().push(format!("http://{local_addr} (local)"));
    tracing::info!(
        "amx node {} ({}) listening on http://{local_addr}",
        node.node_id,
        node.machine
    );

    let mut tasks = vec![spawn_server(&node, local, Listener::Local)];
    if node.cfg.tailnet || node.cfg.tailnet_listen.is_some() {
        tasks.push(tokio::spawn(tailnet_listener_loop(node.clone())));
    }
    if node.cfg.tailnet {
        tasks.push(tokio::spawn(sync::discover_loop(node.clone())));
    }
    tasks.push(tokio::spawn(sync::sync_loop(node.clone())));
    tasks.push(tokio::spawn(meta_loop(node.clone())));
    Ok(Running {
        node,
        local_addr,
        tasks,
    })
}
