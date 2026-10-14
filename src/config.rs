use anyhow::{Context, Result};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 7341;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub port: u16,
    pub data_dir: PathBuf,
    /// Display name for this machine; defaults to the Tailscale machine name, then the hostname.
    pub machine: Option<String>,
    /// Extra peers to sync with, e.g. "http://100.64.0.2:7341".
    pub peers: Vec<String>,
    /// Listen on the Tailscale address and discover peers from `tailscale status`.
    pub tailnet: bool,
    pub tailscale_bin: Option<PathBuf>,
    pub gh_bin: PathBuf,
    pub sync_interval_secs: u64,
    pub discover_interval_secs: u64,
    /// Tailscale logins allowed to connect over the tailnet. Empty allows any identified caller.
    pub allow_logins: Vec<String>,
    /// Testing only: listen here with tailnet (whois) identity rules instead of the Tailscale IP.
    pub tailnet_listen: Option<SocketAddr>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: DEFAULT_PORT,
            data_dir: data_home().join("amx"),
            machine: None,
            peers: Vec::new(),
            tailnet: true,
            tailscale_bin: None,
            gh_bin: PathBuf::from("gh"),
            sync_interval_secs: 5,
            discover_interval_secs: 60,
            allow_logins: Vec::new(),
            tailnet_listen: None,
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

pub fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"))
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

    pub fn local_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_partial_config() {
        let c: Config = toml::from_str("port = 9000\npeers = [\"http://1.2.3.4:9000\"]").unwrap();
        assert_eq!(c.port, 9000);
        assert_eq!(c.peers.len(), 1);
        assert!(c.tailnet);
        assert!(toml::from_str::<Config>("prot = 1").is_err());
    }
}
