use amx::config::Config;
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "amx", version, about = "Agent message exchange: a tailnet relay where coding agents review pull requests together")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
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
