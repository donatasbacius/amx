use crate::store::PrMeta;
use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;

pub struct Gh {
    bin: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrView {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    head_ref_oid: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    author: Option<Login>,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

pub fn parse_view(json: &[u8]) -> Result<PrMeta> {
    let v: PrView = serde_json::from_slice(json)?;
    Ok(PrMeta {
        title: v.title,
        head_sha: v.head_ref_oid,
        state: v.state,
        author: v.author.map(|a| a.login),
        fetched_at: crate::model::now_ms(),
        error: None,
    })
}

impl Gh {
    pub fn new(bin: PathBuf) -> Self {
        let bare = bin.components().count() == 1;
        let bin = if bare {
            bin.to_str().and_then(crate::tailscale::which).unwrap_or(bin)
        } else {
            bin
        };
        Gh { bin }
    }

    /// Read-only `gh pr view`; amx never runs a mutating gh command.
    pub async fn view(&self, url: &str, timeout: Duration) -> Result<PrMeta> {
        let out = tokio::time::timeout(
            timeout,
            Command::new(&self.bin)
                .args(["pr", "view", url, "--json", "title,headRefOid,state,author"])
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| anyhow!("gh pr view timed out"))??;
        if !out.status.success() {
            bail!("gh pr view failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        parse_view(&out.stdout)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_pr_view() {
        let m = super::parse_view(
            br#"{"author":{"id":"x","is_bot":false,"login":"octo","name":""},"headRefOid":"abc","state":"OPEN","title":"Fix it"}"#,
        )
        .unwrap();
        assert_eq!(m.title.as_deref(), Some("Fix it"));
        assert_eq!(m.head_sha.as_deref(), Some("abc"));
        assert_eq!(m.author.as_deref(), Some("octo"));
    }
}
