use amx::client::Client;
use amx::config::Config;
use amx::model::{Finding, Kind, Verdict};
use amx::node::PostInput;
use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use std::collections::HashSet;
use std::io::Read;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

#[derive(Parser)]
#[command(name = "amx", version, about = "Agent message exchange: share PR reviews between coding agents over a tailnet")]
struct Cli {
    /// Config file (default ~/.config/amx/config.toml)
    #[arg(long, global = true, env = "AMX_CONFIG")]
    config: Option<PathBuf>,
    /// Local node URL (default http://127.0.0.1:<port from config>)
    #[arg(long, global = true, env = "AMX_NODE")]
    node: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct AgentArg {
    /// Agent name, e.g. alice-claude-1; shown as name@machine
    #[arg(long, env = "AMX_AGENT")]
    agent: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the node daemon
    Serve {
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Extra peer URL to sync with (repeatable)
        #[arg(long = "peer")]
        peers: Vec<String>,
        /// Do not use Tailscale: no tailnet listener, no discovery (local testing)
        #[arg(long)]
        no_tailnet: bool,
        #[arg(long)]
        machine: Option<String>,
        /// TESTING ONLY: extra listener that applies tailnet (whois) identity rules
        #[arg(long, hide = true)]
        tailnet_listen: Option<SocketAddr>,
        #[arg(long)]
        tailscale_bin: Option<PathBuf>,
        #[arg(long)]
        gh_bin: Option<PathBuf>,
        #[arg(long)]
        sync_interval: Option<u64>,
    },
    /// MCP server on stdio (proxies to the local node)
    Mcp {
        #[command(flatten)]
        agent: AgentArg,
    },
    /// Print one line per new inbox item, forever (for Claude Code's Monitor tool)
    Watch {
        #[command(flatten)]
        agent: AgentArg,
        /// Skip items that are already unread when watch starts
        #[arg(long)]
        new_only: bool,
    },
    /// Show unread messages
    Inbox {
        #[command(flatten)]
        agent: AgentArg,
        #[arg(long)]
        mark_read: bool,
        #[arg(long)]
        json: bool,
    },
    /// Mark messages read
    Read {
        #[command(flatten)]
        agent: AgentArg,
        ids: Vec<String>,
        /// Mark the whole current inbox read
        #[arg(long)]
        all: bool,
    },
    /// Post a message to a PR thread
    Post {
        #[command(flatten)]
        agent: AgentArg,
        kind: PostKind,
        pr_url: String,
        /// Text (Markdown); "-" reads stdin
        #[arg(short = 'm', long)]
        body: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long, value_enum)]
        verdict: Option<VerdictArg>,
        /// JSON file with an array of findings [{severity,path,line,url,text}]; "-" for stdin
        #[arg(long)]
        findings: Option<PathBuf>,
        #[arg(long)]
        head_sha: Option<String>,
        #[arg(long)]
        in_reply_to: Option<String>,
        /// Post as the local human user instead of an agent
        #[arg(long)]
        human: bool,
    },
    /// List threads
    Threads {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show one thread
    Thread {
        pr_url: String,
        #[arg(long)]
        json: bool,
    },
    /// Node, peers and sync state
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Agent hook helpers: `amx hook codex stop` (reads hook JSON on stdin)
    Hook {
        client: HookClient,
        /// session-start | user-prompt-submit | stop (default: from stdin hook_event_name)
        event: Option<String>,
        #[command(flatten)]
        agent: AgentArg,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum PostKind {
    Request,
    Claim,
    Review,
    Reply,
    Close,
}

#[derive(Clone, Copy, ValueEnum)]
enum VerdictArg {
    Approve,
    RequestChanges,
    Comment,
}

#[derive(Clone, Copy, ValueEnum)]
enum HookClient {
    Codex,
    Claude,
}

fn agent_or(a: &AgentArg, default: Option<&str>) -> Result<String> {
    match a.agent.as_deref().map(str::trim).filter(|s| !s.is_empty()).or(default) {
        Some(n) if amx::server::valid_agent(n) => Ok(n.to_string()),
        Some(n) => bail!("invalid agent name {n:?}: use letters, digits, - _ ."),
        None => bail!("agent name required: pass --agent or set AMX_AGENT (e.g. AMX_AGENT=alice-claude-1)"),
    }
}

fn read_arg(s: &str) -> Result<String> {
    if s == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        Ok(buf)
    } else {
        Ok(s.to_string())
    }
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
    let mut cfg = Config::load(cli.config.as_deref())?;
    let client = Client::new(cli.node.as_deref().unwrap_or(&cfg.local_url()));
    match cli.cmd {
        Cmd::Serve {
            port,
            data_dir,
            peers,
            no_tailnet,
            machine,
            tailnet_listen,
            tailscale_bin,
            gh_bin,
            sync_interval,
        } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_env("AMX_LOG")
                        .unwrap_or_else(|_| "amx=info".into()),
                )
                .with_writer(std::io::stderr)
                .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
                .init();
            if let Some(p) = port {
                cfg.port = p;
            }
            if let Some(d) = data_dir {
                cfg.data_dir = d;
            }
            cfg.peers.extend(peers);
            if no_tailnet {
                cfg.tailnet = false;
            }
            cfg.machine = machine.or(cfg.machine);
            cfg.tailnet_listen = tailnet_listen.or(cfg.tailnet_listen);
            cfg.tailscale_bin = tailscale_bin.or(cfg.tailscale_bin);
            if let Some(g) = gh_bin {
                cfg.gh_bin = g;
            }
            if let Some(s) = sync_interval {
                cfg.sync_interval_secs = s;
            }
            let running = amx::start(cfg).await?;
            shutdown_signal().await;
            tracing::info!("shutting down");
            running.stop().await;
        }
        Cmd::Mcp { agent } => mcp_stdio(&client, agent.agent).await?,
        Cmd::Watch { agent, new_only } => {
            let agent = agent_or(&agent, None)?;
            watch(&client, &agent, new_only).await?;
        }
        Cmd::Inbox { agent, mark_read, json } => {
            let agent = agent_or(&agent, None)?;
            let items = client.inbox(&agent, false, false, mark_read).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                println!("{}", amx::text::inbox(&agent, &items));
            }
        }
        Cmd::Read { agent, ids, all } => {
            let agent = agent_or(&agent, None)?;
            let ids = if all {
                client.inbox(&agent, false, false, false).await?.into_iter().map(|i| i.message.id).collect()
            } else {
                ids
            };
            let r = client.mark(&agent, &ids, false).await?;
            println!("marked {} read", r["marked"]);
        }
        Cmd::Post {
            agent,
            kind,
            pr_url,
            body,
            to,
            verdict,
            findings,
            head_sha,
            in_reply_to,
            human,
        } => {
            let agent = if human { None } else { Some(agent_or(&agent, None)?) };
            let findings: Vec<Finding> = match findings {
                Some(p) => {
                    let text = if p.as_os_str() == "-" {
                        read_arg("-")?
                    } else {
                        std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?
                    };
                    serde_json::from_str(&text).context("findings must be a JSON array of {severity,path,line,url,text}")?
                }
                None => Vec::new(),
            };
            let input = PostInput {
                kind: Some(match kind {
                    PostKind::Request => Kind::Request,
                    PostKind::Claim => Kind::Claim,
                    PostKind::Review => Kind::Review,
                    PostKind::Reply => Kind::Reply,
                    PostKind::Close => Kind::Close,
                }),
                pr_url,
                body: body.as_deref().map(read_arg).transpose()?.unwrap_or_default(),
                to,
                verdict: verdict.map(|v| match v {
                    VerdictArg::Approve => Verdict::Approve,
                    VerdictArg::RequestChanges => Verdict::RequestChanges,
                    VerdictArg::Comment => Verdict::Comment,
                }),
                findings,
                head_sha,
                in_reply_to,
            };
            let m = client.post_message(agent.as_deref(), human, &input).await?;
            println!("posted {} {} on {}", m.kind.as_str(), m.id, m.thread);
        }
        Cmd::Threads { all, json } => {
            let threads: Vec<_> = client
                .threads()
                .await?
                .into_iter()
                .filter(|t| all || t.status != amx::model::Status::Closed)
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&threads)?);
            } else if threads.is_empty() {
                println!("no threads");
            } else {
                for t in &threads {
                    println!("{}", amx::text::summary_line(t));
                }
            }
        }
        Cmd::Thread { pr_url, json } => {
            let v = client.thread(&pr_url).await?.ok_or_else(|| anyhow!("no thread for {pr_url}"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}", amx::text::thread(&v));
            }
        }
        Cmd::Status { json } => {
            let s = client.status().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                print_status(&s);
            }
        }
        Cmd::Hook { client: which, event, agent } => {
            let default = match which {
                HookClient::Codex => "codex",
                HookClient::Claude => "claude",
            };
            let agent = agent_or(&agent, Some(default))?;
            let event = match event.as_deref().filter(|e| !e.is_empty()) {
                Some(e) => Some(amx::hooks::Event::parse(e).ok_or_else(|| anyhow!("unknown hook event {e}"))?),
                None => None,
            };
            let mut stdin = String::new();
            let _ = std::io::stdin().read_to_string(&mut stdin);
            if let Some(out) = amx::hooks::run(&client, &agent, event, &stdin).await {
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

async fn watch(client: &Client, agent: &str, new_only: bool) -> Result<()> {
    let mut seen: HashSet<String> = HashSet::new();
    if new_only {
        for i in client.inbox(agent, false, false, false).await? {
            seen.insert(i.message.id);
        }
    }
    let mut down_since: Option<std::time::Instant> = None;
    loop {
        match client.wait(agent, 300, false, false).await {
            Ok(items) => {
                if down_since.take().is_some() {
                    eprintln!("amx watch: node is back");
                }
                let fresh: Vec<_> = items.iter().filter(|i| !seen.contains(&i.message.id)).collect();
                for i in &fresh {
                    println!("{}", amx::text::item_line(i));
                    seen.insert(i.message.id.clone());
                }
                if fresh.is_empty() {
                    // Everything unread was already printed; wait for the next change.
                    wait_for_change(client, agent, &seen).await;
                }
            }
            Err(e) => {
                if down_since.is_none() {
                    eprintln!("amx watch: {e:#}; retrying");
                    down_since = Some(std::time::Instant::now());
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }
}

async fn wait_for_change(client: &Client, agent: &str, seen: &HashSet<String>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        match client.inbox(agent, false, false, false).await {
            Ok(items) if items.iter().any(|i| !seen.contains(&i.message.id)) => return,
            Ok(_) => {}
            Err(_) => return,
        }
    }
}

fn print_status(s: &amx::server::StatusReport) {
    println!("node {} ({}) amx {}", s.machine, s.node_id, s.version);
    for l in &s.listening {
        println!("  listening {l}");
    }
    if let Some(t) = &s.tailscale_bin {
        println!("  tailscale CLI {t}");
    }
    println!("  {} messages from {} origin(s)", s.messages, s.vv.len());
    if s.peers.is_empty() {
        println!("  no outbound peers");
    }
    let now = amx::model::now_ms();
    for p in &s.peers {
        let (mut they_lack, mut we_lack) = (0i64, 0i64);
        for (o, &n) in &s.vv {
            they_lack += (n - p.vv.get(o).copied().unwrap_or(0)).max(0);
        }
        for (o, &n) in &p.vv {
            we_lack += (n - s.vv.get(o).copied().unwrap_or(0)).max(0);
        }
        let state = match (&p.last_err, p.last_ok) {
            (Some(e), _) => format!("ERROR {e}"),
            (None, Some(t)) => format!("ok {}s ago, they lack {they_lack}, we lack {we_lack}", (now - t) / 1000),
            (None, None) => "not contacted yet".into(),
        };
        println!(
            "  peer {} [{:?}] {}: {state}",
            p.url,
            p.source,
            p.machine.as_deref().unwrap_or("?")
        );
    }
    for i in &s.inbound {
        println!("  inbound from {} ({}) {}s ago", i.machine, i.addr, (now - i.last_seen) / 1000);
    }
}

async fn mcp_stdio(client: &Client, agent: Option<String>) -> Result<()> {
    let stdout = Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let agent = Arc::new(tokio::sync::RwLock::new(agent.filter(|a| amx::server::valid_agent(a))));
    let client = Arc::new(Client::new(client.base()));
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_line(&stdout, &amx::mcp::error_response(serde_json::Value::Null, -32700, &e.to_string())).await;
                continue;
            }
        };
        if msg.get("method").and_then(|m| m.as_str()) == Some("initialize") {
            let mut a = agent.write().await;
            if a.is_none() {
                let client_name = msg["params"]["clientInfo"]["name"].as_str().unwrap_or("");
                *a = Some(amx::mcp::default_agent_for(client_name));
            }
        }
        let name = agent.read().await.clone().unwrap_or_else(|| "agent".into());
        let (client, stdout) = (client.clone(), stdout.clone());
        tokio::spawn(async move {
            let id = msg.get("id").cloned();
            match client.mcp(&name, &msg).await {
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

async fn write_line(out: &tokio::sync::Mutex<tokio::io::Stdout>, v: &serde_json::Value) {
    let mut line = v.to_string();
    line.push('\n');
    let mut out = out.lock().await;
    let _ = out.write_all(line.as_bytes()).await;
    let _ = out.flush().await;
}
