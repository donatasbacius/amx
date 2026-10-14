use crate::model::{Author, AuthorKind, Kind};
use crate::node::{Node, PostInput};
use crate::store::MarkTable;
use crate::sync::{self, PushRequest, SyncRequest};
use crate::tailscale::Identity;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listener {
    Local,
    Tailnet,
}

#[derive(Debug, Clone)]
pub enum Caller {
    Local,
    Tailnet(Identity),
}

impl Caller {
    pub fn human(&self, node: &Node) -> Author {
        match self {
            Caller::Local => node.local_human(),
            Caller::Tailnet(id) => Author {
                kind: AuthorKind::Human,
                name: if id.display_name.is_empty() {
                    id.login.clone()
                } else {
                    id.display_name.clone()
                },
                machine: id.node.clone(),
                login: Some(id.login.clone()),
            },
        }
    }
}

type St = State<Arc<Node>>;

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::BAD_REQUEST, format!("{e:#}"))
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

pub fn router(node: Arc<Node>, listener: Listener) -> Router {
    Router::new()
        .route("/", get(crate::web::index))
        .route("/pr/{owner}/{repo}/{number}", get(crate::web::thread_page).post(web_post))
        .route("/new", post(web_new_request))
        .route("/static/style.css", get(static_css))
        .route("/static/app.js", get(static_js))
        .route("/api/events", get(events))
        .route("/api/hello", get(hello))
        .route("/api/sync", post(api_sync))
        .route("/api/push", post(api_push))
        .route("/mcp", post(mcp).get(mcp_get).delete(mcp_get))
        .route("/api/local/post", post(local_post))
        .route("/api/local/inbox", post(local_inbox))
        .route("/api/local/wait", post(local_wait))
        .route("/api/local/mark", post(local_mark))
        .route("/api/local/threads", get(local_threads))
        .route("/api/local/thread", get(local_thread))
        .route("/api/local/status", get(local_status))
        .layer(middleware::from_fn_with_state(
            (node.clone(), listener),
            identify,
        ))
        .with_state(node)
}

/// Names a browser may legitimately use for this node; anything else is treated as DNS rebinding.
fn host_allowed(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name.parse::<IpAddr>().is_ok()
        || name == "localhost"
        || name.ends_with(".ts.net")
        || (!name.is_empty() && !name.contains('.'))
}

fn origin_matches(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    origin
        .split_once("://")
        .map(|(_, rest)| rest.trim_end_matches('/'))
        .is_some_and(|o| o.eq_ignore_ascii_case(host))
}

fn local_only(path: &str) -> bool {
    path == "/mcp" || path.starts_with("/api/local/")
}

async fn identify(
    State((node, listener)): State<(Arc<Node>, Listener)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut req: Request,
    next: Next,
) -> Response {
    let headers = req.headers();
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !host_allowed(host) {
        return (StatusCode::FORBIDDEN, "host not allowed").into_response();
    }
    if req.method() != axum::http::Method::GET && !origin_matches(headers) {
        return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
    }
    let caller = match listener {
        Listener::Local if addr.ip().is_loopback() => Caller::Local,
        Listener::Local => return (StatusCode::FORBIDDEN, "not a local caller").into_response(),
        Listener::Tailnet => match node.ts.whois(addr.ip()).await {
            Some(id) => {
                if !node.cfg.allow_logins.is_empty()
                    && !node.cfg.allow_logins.iter().any(|l| l.eq_ignore_ascii_case(&id.login))
                {
                    return (StatusCode::FORBIDDEN, "tailnet user not allowed").into_response();
                }
                Caller::Tailnet(id)
            }
            None => {
                return (StatusCode::FORBIDDEN, "caller not identified by tailscale whois").into_response();
            }
        },
    };
    if local_only(req.uri().path()) && !matches!(caller, Caller::Local) {
        return (StatusCode::FORBIDDEN, "local agents only").into_response();
    }
    if let Caller::Tailnet(id) = &caller
        && req.uri().path() == "/api/sync"
    {
        tracing::debug!("sync from {}@{} ({addr})", id.login, id.node);
    }
    req.extensions_mut().insert(caller);
    req.extensions_mut().insert(addr);
    next.run(req).await
}

async fn static_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../assets/style.css"),
    )
}

async fn static_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../assets/app.js"),
    )
}

async fn hello(State(node): St) -> Json<sync::Hello> {
    Json(sync::hello(&node))
}

async fn api_sync(
    State(node): St,
    axum::Extension(addr): axum::Extension<SocketAddr>,
    Json(req): Json<SyncRequest>,
) -> ApiResult<sync::SyncResponse> {
    node.peers.saw_inbound(&req.node_id, &req.machine, &addr.to_string());
    Ok(Json(sync::handle_sync(&node, &req)?))
}

async fn api_push(State(node): St, Json(req): Json<PushRequest>) -> ApiResult<sync::PushResponse> {
    Ok(Json(sync::handle_push(&node, &req)?))
}

async fn events(State(node): St) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    use futures_util::StreamExt;
    let rx = node.subscribe();
    let mut down = node.shutdown.subscribe();
    let stream = tokio_stream::wrappers::WatchStream::from_changes(rx)
        .map(|v| Ok(Event::default().event("change").data(v.to_string())))
        .take_until(async move {
            let _ = down.wait_for(|d| *d).await;
        });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20)))
}

#[derive(Deserialize)]
struct AgentQuery {
    agent: Option<String>,
}

/// Agent name from `?agent=`, the `X-Amx-Agent` header, or a guess from the client's User-Agent.
fn agent_name(q: &AgentQuery, headers: &HeaderMap) -> String {
    let given = q
        .agent
        .clone()
        .or_else(|| headers.get("x-amx-agent").and_then(|v| v.to_str().ok()).map(String::from));
    if let Some(n) = given.map(|s| s.trim().to_string()).filter(|s| valid_agent(s)) {
        return n;
    }
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    crate::mcp::default_agent_for(&ua)
}

pub fn valid_agent(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

async fn mcp(State(node): St, Query(q): Query<AgentQuery>, headers: HeaderMap, body: Bytes) -> Response {
    let agent = agent_name(&q, &headers);
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return Json(crate::mcp::error_response(
                serde_json::Value::Null,
                -32700,
                &format!("parse error: {e}"),
            ))
            .into_response();
        }
    };
    match crate::mcp::handle(&node, &agent, value).await {
        Some(resp) => {
            let mut r = Json(resp).into_response();
            r.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            r
        }
        None => StatusCode::ACCEPTED.into_response(),
    }
}

async fn mcp_get() -> impl IntoResponse {
    (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")])
}

#[derive(Deserialize)]
struct LocalPost {
    agent: Option<String>,
    #[serde(default)]
    human: bool,
    #[serde(flatten)]
    input: PostInput,
}

fn require_agent(a: Option<&str>) -> Result<String, ApiError> {
    match a.map(str::trim) {
        Some(a) if valid_agent(a) => Ok(a.to_string()),
        _ => Err(ApiError(
            StatusCode::BAD_REQUEST,
            "agent name required (letters, digits, - _ .); set AMX_AGENT or --agent".into(),
        )),
    }
}

async fn local_post(State(node): St, Json(p): Json<LocalPost>) -> ApiResult<crate::model::Message> {
    let author = if p.human {
        node.local_human()
    } else {
        node.agent(&require_agent(p.agent.as_deref())?)
    };
    Ok(Json(node.post(author, p.input).await?))
}

#[derive(Deserialize)]
struct InboxReq {
    agent: String,
    #[serde(default)]
    undelivered: bool,
    #[serde(default)]
    mark_delivered: bool,
    #[serde(default)]
    mark_read: bool,
    timeout_secs: Option<u64>,
}

fn finish_inbox(node: &Node, me: &Author, req: &InboxReq, items: &[crate::node::InboxItem]) -> Result<(), ApiError> {
    let ids: Vec<String> = items.iter().map(|i| i.message.id.clone()).collect();
    if req.mark_delivered {
        node.mark(MarkTable::Delivered, me, &ids)?;
    }
    if req.mark_read {
        node.mark(MarkTable::Read, me, &ids)?;
    }
    Ok(())
}

async fn local_inbox(State(node): St, Json(req): Json<InboxReq>) -> ApiResult<Vec<crate::node::InboxItem>> {
    let me = node.agent(&require_agent(Some(&req.agent))?);
    let items = node.inbox(&me, req.undelivered)?;
    finish_inbox(&node, &me, &req, &items)?;
    Ok(Json(items))
}

async fn local_wait(State(node): St, Json(req): Json<InboxReq>) -> ApiResult<Vec<crate::node::InboxItem>> {
    let me = node.agent(&require_agent(Some(&req.agent))?);
    let timeout = Duration::from_secs(req.timeout_secs.unwrap_or(60).min(3600));
    let items = node.wait_inbox(&me, timeout, req.undelivered).await?;
    finish_inbox(&node, &me, &req, &items)?;
    Ok(Json(items))
}

#[derive(Deserialize)]
struct MarkReq {
    agent: String,
    ids: Vec<String>,
    #[serde(default)]
    delivered: bool,
}

async fn local_mark(State(node): St, Json(req): Json<MarkReq>) -> ApiResult<serde_json::Value> {
    let me = node.agent(&require_agent(Some(&req.agent))?);
    let table = if req.delivered { MarkTable::Delivered } else { MarkTable::Read };
    let n = node.mark(table, &me, &req.ids)?;
    Ok(Json(json!({ "marked": n })))
}

async fn local_threads(State(node): St) -> ApiResult<Vec<crate::node::ThreadSummary>> {
    Ok(Json(node.threads()?))
}

#[derive(Deserialize)]
struct UrlQuery {
    url: String,
}

async fn local_thread(State(node): St, Query(q): Query<UrlQuery>) -> ApiResult<Option<crate::node::ThreadView>> {
    Ok(Json(node.thread(&q.url)?))
}

#[derive(Serialize, Deserialize)]
pub struct StatusReport {
    pub node_id: String,
    pub machine: String,
    pub version: String,
    pub messages: i64,
    pub vv: crate::store::VersionVector,
    pub peers: Vec<sync::PeerState>,
    pub inbound: Vec<sync::InboundPeer>,
    pub listening: Vec<String>,
    pub tailscale_bin: Option<String>,
}

async fn local_status(State(node): St) -> ApiResult<StatusReport> {
    Ok(Json(status_report(&node)?))
}

pub fn status_report(node: &Node) -> anyhow::Result<StatusReport> {
    Ok(StatusReport {
        node_id: node.node_id.clone(),
        machine: node.machine.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        messages: node.store.count()?,
        vv: node.store.version_vector()?,
        peers: node.peers.outbound(),
        inbound: node.peers.inbound(),
        listening: node.listening.lock().unwrap().clone(),
        tailscale_bin: node.ts.bin().map(|p| p.display().to_string()),
    })
}

#[derive(Deserialize)]
pub struct WebPostForm {
    kind: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    to: String,
    #[serde(default)]
    pr_url: String,
}

fn form_kind(s: &str) -> Option<Kind> {
    match s {
        "reply" => Some(Kind::Reply),
        "request" => Some(Kind::Request),
        "close" => Some(Kind::Close),
        _ => None,
    }
}

async fn web_post(
    State(node): St,
    axum::Extension(caller): axum::Extension<Caller>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
    Form(f): Form<WebPostForm>,
) -> Response {
    let url = format!("https://github.com/{owner}/{repo}/pull/{number}");
    let input = PostInput {
        kind: form_kind(&f.kind),
        pr_url: url,
        body: f.body,
        to: Some(f.to),
        ..Default::default()
    };
    post_and_redirect(&node, caller, input).await
}

async fn web_new_request(
    State(node): St,
    axum::Extension(caller): axum::Extension<Caller>,
    Form(f): Form<WebPostForm>,
) -> Response {
    let input = PostInput {
        kind: Some(Kind::Request),
        pr_url: f.pr_url,
        body: f.body,
        to: Some(f.to),
        ..Default::default()
    };
    post_and_redirect(&node, caller, input).await
}

async fn post_and_redirect(node: &Arc<Node>, caller: Caller, input: PostInput) -> Response {
    let author = caller.human(node);
    match node.post(author, input).await {
        Ok(m) => {
            let p = crate::prurl::parse(&m.thread).expect("canonical thread");
            Redirect::to(&format!("/pr/{}/{}/{}#m-{}", p.owner, p.repo, p.number, m.id)).into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Html(crate::web::error_page(&format!("{e:#}"))),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_check() {
        for ok in [
            "127.0.0.1:7341",
            "localhost:7341",
            "100.64.0.1:7341",
            "[fd7a::1]:7341",
            "alpha:7341",
            "alpha.example.ts.net:7341",
            "alpha.example.ts.net.",
        ] {
            assert!(host_allowed(ok), "{ok}");
        }
        for bad in ["evil.com", "evil.com:7341", "", "127.0.0.1.evil.com"] {
            assert!(!host_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn origin_check() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("127.0.0.1:7341"));
        assert!(origin_matches(&h));
        h.insert(header::ORIGIN, HeaderValue::from_static("http://127.0.0.1:7341"));
        assert!(origin_matches(&h));
        h.insert(header::ORIGIN, HeaderValue::from_static("https://evil.com"));
        assert!(!origin_matches(&h));
        h.insert(header::ORIGIN, HeaderValue::from_static("null"));
        assert!(!origin_matches(&h));
    }
}
