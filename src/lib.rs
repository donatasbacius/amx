pub mod client;
pub mod config;
pub mod hooks;
pub mod hub;
pub mod mcp;
pub mod persist;
pub mod prurl;
pub mod relay;
pub mod server;
pub mod tailscale;
pub mod text;
pub mod web;

use anyhow::{Context, Result};
use config::Config;
use hub::Hub;
use server::Listener;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

pub struct Running {
    pub hub: Arc<Hub>,
    pub local_addr: SocketAddr,
    tasks: Vec<JoinHandle<()>>,
}

impl Running {
    pub async fn stop(self) {
        let _ = self.hub.shutdown.send(true);
        for mut t in self.tasks {
            if tokio::time::timeout(Duration::from_millis(500), &mut t).await.is_err() {
                t.abort();
            }
        }
        if let Err(e) = self.hub.save() {
            tracing::error!("{e:#}");
        }
    }
}

fn spawn_server(hub: &Arc<Hub>, listener: tokio::net::TcpListener, kind: Listener) -> JoinHandle<()> {
    tokio::spawn(server::serve(hub.clone(), listener, kind))
}

/// Where links to the web page point when `public_url` isn't set.
pub fn tailnet_url(addr: SocketAddr, dns_name: Option<&str>) -> String {
    match dns_name {
        Some(name) => format!("http://{name}:{}", addr.port()),
        None => format!("http://{addr}"),
    }
}

async fn tailnet_addr(hub: &Hub) -> Result<(SocketAddr, Option<String>)> {
    if let Some(a) = hub.cfg.tailnet_listen {
        return Ok((a, None));
    }
    let status = hub.ts.status().await?;
    let ip = status
        .ips
        .iter()
        .find(|ip| matches!(ip, IpAddr::V4(_)))
        .copied()
        .context("no Tailscale IPv4 address")?;
    Ok((SocketAddr::new(ip, hub.cfg.port), status.dns_name))
}

/// Learns the relay owner with whois of our own address, then binds the tailnet listener.
/// Anything bound to `local` (from `--no-tailnet`) becomes the owner's.
async fn tailnet_listener(hub: &Hub) -> Result<tokio::net::TcpListener> {
    let (addr, dns_name) = tailnet_addr(hub).await?;
    if hub.cfg.tailnet_listen.is_none() && hub.owner.lock().unwrap().is_none() {
        let id = hub.ts.whois(addr.ip()).await.context("whois of our own address failed")?;
        tracing::info!("relay owner (loopback callers): {} on {}", id.login, id.node);
        hub.write(|st, _| st.rename_login("local", &id));
        *hub.owner.lock().unwrap() = Some(id);
    }
    let l = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("cannot bind {addr}"))?;
    tracing::info!("listening on http://{addr} (tailnet, callers identified with tailscale whois)");
    hub.listening.lock().unwrap().push(format!("http://{addr}"));
    if hub.cfg.tailnet_listen.is_none() && hub.cfg.public_url.is_none() {
        hub.set_web_url(&tailnet_url(addr, dns_name.as_deref()));
    }
    Ok(l)
}

/// Retries until Tailscale is up (e.g. at boot), warning every 5 minutes; loopback callers wait for
/// the owner until then, except to read the web page.
async fn tailnet_listener_loop(hub: Arc<Hub>) {
    let mut warned: Option<tokio::time::Instant> = None;
    let mut pause = Duration::from_secs(1);
    loop {
        match tailnet_listener(&hub).await {
            Ok(l) => {
                let _ = spawn_server(&hub, l, Listener::Tailnet).await;
                return;
            }
            Err(e) if warned.is_none_or(|t| t.elapsed() >= Duration::from_secs(300)) => {
                tracing::warn!("tailnet listener not started: {e:#}; retrying");
                warned = Some(tokio::time::Instant::now());
            }
            Err(_) => {}
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_secs(10));
    }
}

async fn tick_loop(hub: Arc<Hub>) {
    let mut down = hub.shutdown.subscribe();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(500)) => hub.tick(),
            _ = down.wait_for(|d| *d) => return,
        }
    }
}

/// Saves at most once a second while the state keeps changing.
async fn persist_loop(hub: Arc<Hub>) {
    let mut rx = hub.subscribe();
    let mut down = hub.shutdown.subscribe();
    loop {
        tokio::select! {
            r = rx.changed() => if r.is_err() { return },
            _ = down.wait_for(|d| *d) => return,
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = down.wait_for(|d| *d) => return,
        }
        rx.borrow_and_update();
        if let Err(e) = hub.save() {
            tracing::error!("{e:#}");
        }
    }
}

async fn alive_loop(hub: Arc<Hub>, path: std::path::PathBuf) {
    let mut down = hub.shutdown.subscribe();
    let mut failing = false;
    loop {
        match persist::mark_alive(&path, relay::now_ms()) {
            Ok(()) => failing = false,
            Err(e) if !failing => {
                tracing::error!("{e:#}");
                failing = true;
            }
            Err(_) => {}
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            _ = down.wait_for(|d| *d) => return,
        }
    }
}

pub async fn start(cfg: Config) -> Result<Running> {
    cfg.validate()?;
    let ts = tailscale::Tailscale::new(if cfg.tailnet || cfg.tailnet_listen.is_some() {
        tailscale::find_bin(cfg.tailscale_bin.as_deref())
    } else {
        None
    });
    let hub = Hub::new(cfg, ts);
    if let Some(path) = &hub.cfg.state {
        if let Some(snap) = persist::load(path)? {
            let n = snap.reviews.len();
            hub.write(|st, now| st.restore(snap, now));
            tracing::info!("restored {n} review(s) from {}", path.display());
        }
        hub.save()?;
    }
    let local_addr = SocketAddr::from(([127, 0, 0, 1], hub.cfg.port));
    let local = tokio::net::TcpListener::bind(local_addr)
        .await
        .with_context(|| format!("binding {local_addr}"))?;
    let local_addr = local.local_addr()?;
    hub.listening.lock().unwrap().push(format!("http://{local_addr}"));
    hub.set_web_url(hub.cfg.public_url.as_deref().unwrap_or(&format!("http://{local_addr}")));
    tracing::info!("amx relay listening on http://{local_addr} (this machine's owner)");
    let mut tasks = vec![spawn_server(&hub, local, Listener::Local), tokio::spawn(tick_loop(hub.clone()))];
    if let Some(path) = hub.cfg.state.clone() {
        tasks.push(tokio::spawn(persist_loop(hub.clone())));
        tasks.push(tokio::spawn(alive_loop(hub.clone(), path)));
    }
    if hub.cfg.tailnet || hub.cfg.tailnet_listen.is_some() {
        tasks.push(tokio::spawn(tailnet_listener_loop(hub.clone())));
    }
    Ok(Running { hub, local_addr, tasks })
}
