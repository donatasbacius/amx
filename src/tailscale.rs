use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::process::Command;

const MAC_APP_CLI: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";
const TIMEOUT: Duration = Duration::from_secs(5);

/// Service managers (launchd, systemd) start us with a minimal PATH, so also look where
/// Homebrew and Linuxbrew install tools.
const EXTRA_DIRS: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/home/linuxbrew/.linuxbrew/bin",
];

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(EXTRA_DIRS.iter().map(PathBuf::from))
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// `tailscale` on PATH (Linux packages, Homebrew, macOS standalone CLI), else the binary
/// inside the macOS app bundle, which works as a CLI when invoked directly.
pub fn find_bin(over: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = over {
        return Some(p.to_path_buf());
    }
    which("tailscale").or_else(|| {
        let app = Path::new(MAC_APP_CLI);
        app.is_file().then(|| app.to_path_buf())
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub login: String,
    pub display_name: String,
    pub node: String,
}

#[derive(Debug, Clone)]
pub struct SelfStatus {
    pub name: String,
    pub ips: Vec<IpAddr>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WhoisJson {
    node: WhoisNode,
    user_profile: WhoisUser,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WhoisNode {
    #[serde(default)]
    name: String,
    #[serde(default)]
    computed_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WhoisUser {
    #[serde(default)]
    login_name: String,
    #[serde(default)]
    display_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StatusJson {
    #[serde(rename = "Self")]
    me: Option<StatusNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StatusNode {
    #[serde(default)]
    host_name: String,
    #[serde(rename = "DNSName", default)]
    dns_name: String,
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Vec<IpAddr>,
}

pub fn first_label(dns: &str) -> &str {
    dns.split('.').next().unwrap_or(dns)
}

impl StatusNode {
    fn name(&self) -> String {
        let label = first_label(&self.dns_name);
        if label.is_empty() {
            self.host_name.clone()
        } else {
            label.to_string()
        }
    }
}

pub fn parse_whois(json: &[u8]) -> Result<Identity> {
    let w: WhoisJson = serde_json::from_slice(json).context("parsing whois")?;
    if w.user_profile.login_name.is_empty() {
        bail!("whois returned no login");
    }
    let node = if w.node.computed_name.is_empty() {
        first_label(&w.node.name).to_string()
    } else {
        w.node.computed_name
    };
    Ok(Identity {
        login: w.user_profile.login_name,
        display_name: w.user_profile.display_name,
        node,
    })
}

pub fn parse_status(json: &[u8]) -> Result<SelfStatus> {
    let s: StatusJson = serde_json::from_slice(json).context("parsing status")?;
    let me = s.me.ok_or_else(|| anyhow!("tailscale status has no Self"))?;
    Ok(SelfStatus {
        name: me.name(),
        ips: me.tailscale_ips,
    })
}

pub struct Tailscale {
    bin: Option<PathBuf>,
    cache: Mutex<HashMap<IpAddr, (Instant, Option<Identity>)>>,
}

impl Tailscale {
    pub fn bin(&self) -> Option<&Path> {
        self.bin.as_deref()
    }

    pub fn new(bin: Option<PathBuf>) -> Self {
        Tailscale {
            bin,
            cache: Mutex::new(HashMap::new()),
        }
    }

    async fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let bin = self.bin.as_ref().ok_or_else(|| anyhow!("tailscale CLI not found"))?;
        let out = tokio::time::timeout(
            TIMEOUT,
            Command::new(bin).args(args).kill_on_drop(true).output(),
        )
        .await
        .map_err(|_| anyhow!("tailscale {} timed out", args[0]))??;
        if !out.status.success() {
            bail!(
                "tailscale {} failed: {}",
                args[0],
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }

    pub async fn status(&self) -> Result<SelfStatus> {
        parse_status(&self.run(&["status", "--json"]).await?)
    }

    /// Cached: identities for 5 minutes, failures for 10 seconds.
    pub async fn whois(&self, ip: IpAddr) -> Option<Identity> {
        if let Some((at, id)) = self.cache.lock().unwrap().get(&ip) {
            let ttl = if id.is_some() { 300 } else { 10 };
            if at.elapsed() < Duration::from_secs(ttl) {
                return id.clone();
            }
        }
        let res = match self.run(&["whois", "--json", &ip.to_string()]).await {
            Ok(out) => parse_whois(&out).map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let id = match res {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::info!("whois {ip}: {e}");
                None
            }
        };
        self.cache
            .lock()
            .unwrap()
            .insert(ip, (Instant::now(), id.clone()));
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_whois_output() {
        let json = br#"{"Node":{"ID":1,"Name":"alpha.example.ts.net.","Hostinfo":{"Hostname":"alpha"}},
            "UserProfile":{"ID":2,"LoginName":"someone@example.com","DisplayName":"Some One"},"CapMap":{}}"#;
        let id = parse_whois(json).unwrap();
        assert_eq!(id.login, "someone@example.com");
        assert_eq!(id.node, "alpha");
        assert!(parse_whois(br#"{"Node":{},"UserProfile":{}}"#).is_err());
        assert!(parse_whois(b"peer not found").is_err());
    }

    #[test]
    fn parses_status_output() {
        let json = br#"{"Self":{"HostName":"alpha","DNSName":"alpha.x.ts.net.","TailscaleIPs":["100.64.0.1","fd7a::1"],"Online":true},
            "Peer":{"k":{"HostName":"Some Mac","DNSName":"beta.x.ts.net.","TailscaleIPs":["100.64.0.2"],"Online":false}}}"#;
        let s = parse_status(json).unwrap();
        assert_eq!(s.name, "alpha");
        assert_eq!(s.ips.len(), 2);
    }
}
