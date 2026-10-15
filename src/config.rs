use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 7341;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub port: u16,
    /// Also listen on this machine's Tailscale IPv4 address.
    pub tailnet: bool,
    pub tailscale_bin: Option<PathBuf>,
    /// Tailscale logins allowed to connect over the tailnet. Empty allows any identified caller.
    pub allow_logins: Vec<String>,
    /// Testing only: listen here with tailnet (whois) identity rules instead of the Tailscale IP.
    pub tailnet_listen: Option<SocketAddr>,
    pub timing: Timing,
    pub consensus: Consensus,
    /// Finished reviews kept in memory for the web page and `review_status`.
    pub history: usize,
    /// Keep reviews and agents' pending events in this JSON file across restarts.
    pub state: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Timing {
    /// An agent is online while it has called the relay within this many seconds.
    pub online_secs: u64,
    /// An assigned reviewer silent this long is dropped from its review.
    pub drop_after_secs: u64,
    pub offer_secs: u64,
    pub review_secs: u64,
    pub vote_secs: u64,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            online_secs: 90,
            drop_after_secs: 300,
            offer_secs: 120,
            review_secs: 1200,
            vote_secs: 600,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Consensus {
    pub reviewers: usize,
    /// A finding is confirmed when more than this fraction of its voters (author included) agree,
    /// and rejected when more than this fraction disagree; anything else is disputed.
    pub confirm_fraction: f64,
    /// Blocking findings stay blocking only if nobody disagrees; otherwise they become major.
    pub blocking_unanimous: bool,
    pub vote_rounds: u8,
}

impl Default for Consensus {
    fn default() -> Self {
        Consensus {
            reviewers: 2,
            confirm_fraction: 0.5,
            blocking_unanimous: true,
            vote_rounds: 2,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: DEFAULT_PORT,
            tailnet: true,
            tailscale_bin: None,
            allow_logins: Vec::new(),
            tailnet_listen: None,
            timing: Timing::default(),
            consensus: Consensus::default(),
            history: 50,
            state: None,
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// XDG-style paths on every platform, including macOS, so docs and scripts stay identical.
pub fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
}

pub fn default_path() -> PathBuf {
    config_home().join("amx/config.toml")
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Config> {
        let explicit = path.is_some();
        let path = path.map(PathBuf::from).unwrap_or_else(default_path);
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_partial_config() {
        let c: Config = toml::from_str("port = 9000\nstate = \"/var/lib/amx/state.json\"\n[timing]\nvote_secs = 60\n[consensus]\nreviewers = 3").unwrap();
        assert_eq!(c.port, 9000);
        assert_eq!(c.timing.vote_secs, 60);
        assert_eq!(c.timing.offer_secs, 120);
        assert_eq!(c.consensus.reviewers, 3);
        assert_eq!(c.state.as_deref(), Some(Path::new("/var/lib/amx/state.json")));
        assert!(c.tailnet);
        assert!(toml::from_str::<Config>("prot = 1").is_err());
    }
}
