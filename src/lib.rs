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
    let app = server::router(hub.clone(), kind);
    let mut down = hub.shutdown.subscribe();
    tokio::spawn(async move {
        let res = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(async move {
                let _ = down.wait_for(|d| *d).await;
            })
            .await;
        if let Err(e) = res {
            tracing::error!("server error: {e}");
        }
    })
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

/// Binds the tailnet listener, retrying until Tailscale is up (e.g. at boot).
async fn tailnet_listener_loop(hub: Arc<Hub>) {
    let mut warned = false;
    loop {
        match tailnet_addr(&hub).await {
            Ok((addr, dns_name)) => match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => {
                    tracing::info!("listening on http://{addr} (tailnet, callers identified with tailscale whois)");
                    hub.listening.lock().unwrap().push(format!("http://{addr}"));
                    if hub.cfg.tailnet_listen.is_none() {
                        if hub.cfg.public_url.is_none() {
                            hub.set_web_url(&tailnet_url(addr, dns_name.as_deref()));
                        }
                        match hub.ts.whois(addr.ip()).await {
                            Some(id) => {
                                tracing::info!("relay owner (loopback callers): {} on {}", id.login, id.node);
                                *hub.owner.lock().unwrap() = id;
                            }
                            None => tracing::warn!("whois of our own address failed; tailnet callers will be refused"),
                        }
                    }
                    let h = spawn_server(&hub, l, Listener::Tailnet);
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

pub async fn start(cfg: Config) -> Result<Running> {
    if let Some(u) = &cfg.public_url
        && server::url_host(u).is_none()
    {
        anyhow::bail!("public_url must be an http:// or https:// URL, got {u}");
    }
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
    if hub.cfg.state.is_some() {
        tasks.push(tokio::spawn(persist_loop(hub.clone())));
    }
    if hub.cfg.tailnet || hub.cfg.tailnet_listen.is_some() {
        tasks.push(tokio::spawn(tailnet_listener_loop(hub.clone())));
    }
    Ok(Running { hub, local_addr, tasks })
}
