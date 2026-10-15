use anyhow::{Result, anyhow};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::time::Duration;

/// HTTP client for a relay, used by `amx mcp`, `amx watch` and `amx hook`.
pub struct Client {
    base: String,
    agent: Option<String>,
    role: Option<String>,
    client_name: Option<String>,
    http: reqwest::Client,
}

impl Client {
    pub fn new(base: &str, agent: Option<String>, role: Option<String>) -> Client {
        let base = base.trim_end_matches('/');
        let base = base.strip_suffix("/mcp").unwrap_or(base);
        Client {
            base: base.to_string(),
            agent,
            role,
            client_name: None,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .build()
                .expect("http client"),
        }
    }

    pub fn set_client_name(&mut self, name: &str) {
        self.client_name = Some(name.to_string());
    }

    fn query(&self) -> Vec<(&'static str, String)> {
        let mut q = Vec::new();
        if let Some(a) = &self.agent {
            q.push(("agent", a.clone()));
        }
        if let Some(r) = &self.role {
            q.push(("role", r.clone()));
        }
        q
    }

    fn request(&self, method: reqwest::Method, path: &str, timeout: Duration) -> reqwest::RequestBuilder {
        let rb = self
            .http
            .request(method, format!("{}{path}", self.base))
            .timeout(timeout)
            .query(&self.query());
        let client = self.client_name.as_deref().unwrap_or(concat!("amx/", env!("CARGO_PKG_VERSION")));
        rb.header("x-amx-client", client)
    }

    fn unreachable(&self, e: reqwest::Error) -> anyhow::Error {
        anyhow!("cannot reach the amx relay at {} ({e})", self.base)
    }

    async fn parse<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(String::from))
                .unwrap_or(text);
            return Err(anyhow!("{status}: {}", msg.trim()));
        }
        Ok(serde_json::from_str(&text)?)
    }

    /// Forwards one MCP JSON-RPC message; `None` for notifications.
    pub async fn mcp(&self, msg: &Value) -> Result<Option<Value>> {
        let resp = self
            .request(reqwest::Method::POST, "/mcp", Duration::from_secs(120))
            .json(msg)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        if resp.status() == reqwest::StatusCode::ACCEPTED {
            return Ok(None);
        }
        Ok(Some(Self::parse(resp).await?))
    }

    pub async fn heartbeat(&self) -> Result<Value> {
        let resp = self
            .request(reqwest::Method::POST, "/api/heartbeat", Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        Self::parse(resp).await
    }

    pub async fn watch(&self, after: u64, timeout_secs: u64) -> Result<Value> {
        let resp = self
            .request(reqwest::Method::GET, "/api/watch", Duration::from_secs(timeout_secs + 20))
            .query(&[("after", after), ("timeout", timeout_secs)])
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        Self::parse(resp).await
    }

    pub async fn obligation(&self, timeout: Duration) -> Result<Value> {
        let resp = self
            .request(reqwest::Method::GET, "/api/obligation", timeout)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        Self::parse(resp).await
    }
}
