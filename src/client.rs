use crate::node::{InboxItem, PostInput, ThreadSummary, ThreadView};
use crate::server::StatusReport;
use anyhow::{Result, anyhow};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::time::Duration;

pub struct Client {
    base: String,
    http: reqwest::Client,
}

impl Client {
    pub fn new(base: &str) -> Client {
        Client {
            base: base.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .build()
                .expect("http client"),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    async fn parse<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(String::from))
                .unwrap_or(text);
            return Err(anyhow!("{status}: {msg}"));
        }
        Ok(serde_json::from_str(&text)?)
    }

    fn unreachable(&self, e: reqwest::Error) -> anyhow::Error {
        anyhow!("cannot reach amx node at {} ({e}); is `amx serve` running?", self.base)
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: Value, timeout: Duration) -> Result<T> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        Self::parse(resp).await
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .timeout(Duration::from_secs(30))
            .query(query)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        Self::parse(resp).await
    }

    pub async fn post_message(&self, agent: Option<&str>, human: bool, input: &PostInput) -> Result<crate::model::Message> {
        let mut body = serde_json::to_value(input)?;
        body["agent"] = json!(agent);
        body["human"] = json!(human);
        self.post("/api/local/post", body, Duration::from_secs(30)).await
    }

    pub async fn inbox(&self, agent: &str, undelivered: bool, mark_delivered: bool, mark_read: bool) -> Result<Vec<InboxItem>> {
        self.post(
            "/api/local/inbox",
            json!({ "agent": agent, "undelivered": undelivered, "mark_delivered": mark_delivered, "mark_read": mark_read }),
            Duration::from_secs(10),
        )
        .await
    }

    pub async fn wait(&self, agent: &str, timeout_secs: u64, undelivered: bool, mark_delivered: bool) -> Result<Vec<InboxItem>> {
        self.post(
            "/api/local/wait",
            json!({ "agent": agent, "timeout_secs": timeout_secs, "undelivered": undelivered, "mark_delivered": mark_delivered }),
            Duration::from_secs(timeout_secs + 15),
        )
        .await
    }

    pub async fn mark(&self, agent: &str, ids: &[String], delivered: bool) -> Result<Value> {
        self.post(
            "/api/local/mark",
            json!({ "agent": agent, "ids": ids, "delivered": delivered }),
            Duration::from_secs(10),
        )
        .await
    }

    pub async fn threads(&self) -> Result<Vec<ThreadSummary>> {
        self.get("/api/local/threads", &[]).await
    }

    pub async fn thread(&self, url: &str) -> Result<Option<ThreadView>> {
        self.get("/api/local/thread", &[("url", url)]).await
    }

    pub async fn status(&self) -> Result<StatusReport> {
        self.get("/api/local/status", &[]).await
    }

    /// Forwards one MCP JSON-RPC message; `None` for notifications.
    pub async fn mcp(&self, agent: &str, msg: &Value) -> Result<Option<Value>> {
        let resp = self
            .http
            .post(format!("{}/mcp", self.base))
            .query(&[("agent", agent)])
            .timeout(Duration::from_secs(700))
            .json(msg)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        if resp.status() == reqwest::StatusCode::ACCEPTED {
            return Ok(None);
        }
        Ok(Some(Self::parse(resp).await?))
    }
}
