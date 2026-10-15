use amx::client::Client;
use amx::config::Config;
use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

#[derive(Parser)]
#[command(name = "amx", version, about = "Agent message exchange: a tailnet relay where coding agents review pull requests together")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct Target {
    /// Relay URL, e.g. http://100.64.0.1:7341
    #[arg(long, env = "AMX_RELAY")]
    relay: String,
    /// Agent name, unique per person, e.g. alice-claude
    #[arg(long, env = "AMX_AGENT")]
    agent: Option<String>,
    /// reviewer or author
    #[arg(long, env = "AMX_ROLE")]
    role: Option<String>,
}

impl Target {
    fn client(&self) -> Client {
        Client::new(&self.relay, self.agent.clone(), self.role.clone())
    }

    fn named(&self) -> Result<Client> {
        match &self.agent {
            Some(a) if amx::server::valid_agent(a) => Ok(self.client()),
            _ => bail!("--agent (or AMX_AGENT) is required: letters, digits, - _ ."),
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum HookClient {
    Codex,
    Claude,
}

#[derive(Clone, Copy, ValueEnum)]
enum HookEvent {
    Stop,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the relay
    Relay {
        /// Config file (default ~/.config/amx/config.toml)
        #[arg(long, env = "AMX_CONFIG")]
        config: Option<PathBuf>,
        #[arg(long)]
        port: Option<u16>,
        /// Loopback only: no Tailscale listener
        #[arg(long)]
        no_tailnet: bool,
        #[arg(long)]
        tailscale_bin: Option<PathBuf>,
        /// Base URL for transcript links (default: the MagicDNS name, else the Tailscale IP)
        #[arg(long)]
        public_url: Option<String>,
        /// Keep reviews in this JSON file across restarts (default: memory only)
        #[arg(long)]
        state: Option<PathBuf>,
        /// TESTING ONLY: extra listener that applies tailnet (whois) identity rules
        #[arg(long, hide = true)]
        tailnet_listen: Option<SocketAddr>,
    },
    /// MCP server on stdio that forwards to a relay (for clients that prefer stdio)
    Mcp {
        #[command(flatten)]
        target: Target,
        /// Seconds a `wait` may block, repeating the relay's 45s long-poll while nothing happens
        /// (any timeout_seconds it passes is ignored); keep it under the client's tool timeout
        #[arg(long, env = "AMX_WAIT_MAX", default_value_t = amx::mcp::WAIT_PROXY_DEFAULT)]
        wait_max: u64,
        /// TESTING ONLY: seconds per long-poll on the relay
        #[arg(long, hide = true, default_value_t = amx::mcp::WAIT_DEFAULT)]
        wait_step: u64,
    },
    /// Print one line per new relay event for an agent (for Claude Code's Monitor tool)
    Watch {
        #[command(flatten)]
        target: Target,
    },
    /// Agent hook: `amx hook codex stop` blocks stopping while a review needs the agent
    Hook {
        client: HookClient,
        event: HookEvent,
        #[command(flatten)]
        target: Target,
        /// Reviewers: also block stopping when idle, so the agent keeps looping on `wait`
        #[arg(long)]
        keep_waiting: bool,
    },
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("amx: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Relay {
            config,
            port,
            no_tailnet,
            tailscale_bin,
            public_url,
            state,
            tailnet_listen,
        } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_env("AMX_LOG").unwrap_or_else(|_| "amx=info".into()),
                )
                .with_writer(std::io::stderr)
                .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
                .init();
            let mut cfg = Config::load(config.as_deref())?;
            if let Some(p) = port {
                cfg.port = p;
            }
            if no_tailnet {
                cfg.tailnet = false;
            }
            cfg.tailnet_listen = tailnet_listen.or(cfg.tailnet_listen);
            cfg.tailscale_bin = tailscale_bin.or(cfg.tailscale_bin);
            cfg.public_url = public_url.or(cfg.public_url);
            cfg.state = state.or(cfg.state);
            let running = amx::start(cfg).await?;
            shutdown_signal().await;
            tracing::info!("shutting down");
            running.stop().await;
        }
        Cmd::Mcp { target, wait_max, wait_step } => mcp_stdio(target, wait_max, wait_step).await?,
        Cmd::Watch { target } => watch(target.named()?).await,
        Cmd::Hook {
            client: _,
            event: HookEvent::Stop,
            target,
            keep_waiting,
        } => {
            let mut stdin = String::new();
            let _ = std::io::stdin().read_to_string(&mut stdin);
            if let Some(out) = amx::hooks::stop(&target.named()?, keep_waiting).await {
                println!("{out}");
            }
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn watch(client: Client) {
    let mut after = match client.watch(u64::MAX, 1).await {
        Ok(v) => v["last"].as_u64().unwrap_or(0),
        Err(_) => 0,
    };
    let mut down = false;
    loop {
        match client.watch(after, 50).await {
            Ok(v) => {
                if std::mem::take(&mut down) {
                    eprintln!("amx watch: relay is back");
                }
                let last = v["last"].as_u64().unwrap_or(after);
                for l in v["lines"].as_array().into_iter().flatten() {
                    println!("{}", l.as_str().unwrap_or(""));
                }
                // A relay restart resets sequence numbers.
                after = last;
            }
            Err(e) => {
                if !down {
                    eprintln!("amx watch: {e:#}; retrying");
                    down = true;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

type Out = Arc<tokio::sync::Mutex<tokio::io::Stdout>>;

async fn mcp_stdio(target: Target, wait_max: u64, wait_step: u64) -> Result<()> {
    let stdout: Out = Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut client = Arc::new(target.client());
    let mut beating = false;
    let waits: Arc<std::sync::Mutex<HashMap<String, tokio::sync::watch::Sender<bool>>>> = Default::default();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let err = amx::mcp::error_response(Value::Null, -32700, &e.to_string());
                write_line(&stdout, &err).await;
                continue;
            }
        };
        let method = msg.get("method").and_then(|m| m.as_str());
        if method == Some("initialize")
            && let Some(name) = msg["params"]["clientInfo"]["name"].as_str()
            && let Some(c) = Arc::get_mut(&mut client)
        {
            c.set_client_name(name);
        }
        if method == Some("notifications/cancelled")
            && let Some(tx) = waits.lock().unwrap().remove(&msg["params"]["requestId"].to_string())
        {
            let _ = tx.send(true);
        }
        if !beating {
            beating = true;
            let c = client.clone();
            tokio::spawn(async move {
                loop {
                    if let Err(e) = c.heartbeat().await {
                        eprintln!("amx mcp: heartbeat: {e:#}");
                    }
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            });
        }
        let long = method == Some("tools/call") && msg["params"]["name"] == "wait" && wait_max > 0;
        let cancel = long.then(|| {
            let (tx, rx) = tokio::sync::watch::channel(false);
            waits.lock().unwrap().insert(msg["id"].to_string(), tx);
            rx
        });
        let (client, stdout, waits) = (client.clone(), stdout.clone(), waits.clone());
        tokio::spawn(async move {
            let id = msg.get("id").cloned();
            let res = match cancel {
                Some(rx) => {
                    let r = long_wait(&client, msg, (wait_max, wait_step), rx, &stdout).await;
                    waits.lock().unwrap().remove(&id.clone().unwrap_or_default().to_string());
                    r
                }
                None => client.mcp(&msg).await,
            };
            match res {
                Ok(Some(resp)) => write_line(&stdout, &resp).await,
                Ok(None) => {}
                Err(e) => {
                    if let Some(id) = id {
                        write_line(&stdout, &amx::mcp::error_response(id, -32603, &format!("{e:#}"))).await;
                    }
                }
            }
        });
    }
    Ok(())
}

/// Repeats `wait` on the relay while it returns no events, up to `max` seconds in all, so an
/// idle agent spends one tool call per real event. Returns `None` if the client cancelled it.
async fn long_wait(
    client: &Client,
    mut msg: Value,
    (max, step): (u64, u64),
    mut cancel: tokio::sync::watch::Receiver<bool>,
    out: &Out,
) -> Result<Option<Value>> {
    let start = std::time::Instant::now();
    let token = msg["params"]["_meta"]["progressToken"].clone();
    loop {
        let left = max.saturating_sub(start.elapsed().as_secs());
        let round = std::time::Instant::now();
        msg["params"]["arguments"]["timeout_seconds"] = json!(left.min(step).clamp(1, amx::mcp::WAIT_MAX));
        let resp = tokio::select! {
            r = client.mcp(&msg) => r?,
            _ = cancel.wait_for(|c| *c) => return Ok(None),
        };
        let idle = resp
            .as_ref()
            .and_then(|r| r["result"]["content"][0]["text"].as_str())
            .is_some_and(|t| t.starts_with("No new events."));
        if !idle || start.elapsed().as_secs() + 1 >= max {
            return Ok(resp);
        }
        if round.elapsed() < Duration::from_secs(1) {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        if !token.is_null() {
            let note = json!({"jsonrpc": "2.0", "method": "notifications/progress",
                "params": {"progressToken": token, "progress": start.elapsed().as_secs(), "message": "waiting for the relay"}});
            write_line(out, &note).await;
        }
    }
}

async fn write_line(out: &tokio::sync::Mutex<tokio::io::Stdout>, v: &Value) {
    let mut line = v.to_string();
    line.push('\n');
    let mut out = out.lock().await;
    let _ = out.write_all(line.as_bytes()).await;
    let _ = out.flush().await;
}
