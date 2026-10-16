use crate::hub::Hub;
use crate::relay::Role;
use crate::tailscale::Identity;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listener {
    Local,
    Tailnet,
}

type St = State<Arc<Hub>>;

/// Connection caps; fixed outside tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConnLimits {
    /// Open connections from other machines, over all listeners.
    pub max: usize,
    /// Per peer address of another machine.
    pub per_ip: usize,
    /// From this machine (loopback or its own Tailscale address), counted apart from `max`.
    pub own: usize,
    /// Time a client gets to send a request's headers; also closes idle keep-alive connections.
    pub header_timeout: Duration,
}

impl Default for ConnLimits {
    fn default() -> Self {
        ConnLimits {
            max: 512,
            per_ip: 64,
            own: 256,
            header_timeout: Duration::from_secs(15),
        }
    }
}

/// Open connections per peer address, shared by all listeners.
#[derive(Default)]
pub struct Conns {
    open: Mutex<HashMap<IpAddr, usize>>,
}

pub struct ConnSlot {
    conns: Arc<Conns>,
    ip: IpAddr,
}

/// Where this machine's own connections are counted.
const OWN: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

impl Conns {
    pub fn admit(self: &Arc<Self>, ip: IpAddr, own: bool, limits: &ConnLimits) -> Option<ConnSlot> {
        let ip = if own { OWN } else { ip };
        let mut open = self.open.lock().unwrap();
        let others: usize = open.iter().filter(|(k, _)| **k != OWN).map(|(_, n)| n).sum();
        let n = open.get(&ip).copied().unwrap_or(0);
        if (own && n >= limits.own) || (!own && (n >= limits.per_ip || others >= limits.max)) {
            return None;
        }
        *open.entry(ip).or_default() += 1;
        Some(ConnSlot { conns: self.clone(), ip })
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        let mut open = self.conns.open.lock().unwrap();
        if let Some(n) = open.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                open.remove(&self.ip);
            }
        }
    }
}

/// Serves HTTP/1 until shutdown. Connections over the caps are closed at once, and clients that
/// don't send a request's headers within `header_timeout_secs` (idle keep-alive included) are cut
/// off, all before the caller is identified.
pub async fn serve(hub: Arc<Hub>, listener: tokio::net::TcpListener, kind: Listener) {
    use hyper_util::rt::{TokioIo, TokioTimer};
    use tower::ServiceExt;
    let app = router(hub.clone(), kind);
    let limits = hub.cfg.conn_limits;
    let me = listener.local_addr().map(|a| a.ip()).ok();
    let mut down = hub.shutdown.subscribe();
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        let accepted = tokio::select! {
            r = listener.accept() => r,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => continue,
            _ = down.wait_for(|d| *d) => break,
        };
        let (stream, addr) = match accepted {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!("accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let own = addr.ip().is_loopback() || Some(addr.ip()) == me;
        let Some(slot) = hub.conns.admit(addr.ip(), own, &limits) else {
            tracing::debug!("refused a connection from {addr}: too many open");
            continue;
        };
        let app = app.clone();
        let mut down = hub.shutdown.subscribe();
        tasks.spawn(async move {
            let _slot = slot;
            let svc = hyper::service::service_fn(move |mut req: Request<hyper::body::Incoming>| {
                req.extensions_mut().insert(ConnectInfo(addr));
                app.clone().oneshot(req.map(axum::body::Body::new))
            });
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(limits.header_timeout)
                .serve_connection(TokioIo::new(stream), svc);
            tokio::pin!(conn);
            let stopping = tokio::select! {
                _ = conn.as_mut() => false,
                _ = down.wait_for(|d| *d) => true,
            };
            if stopping {
                conn.as_mut().graceful_shutdown();
                let _ = conn.await;
            }
        });
    }
    while tasks.join_next().await.is_some() {}
}

pub fn router(hub: Arc<Hub>, listener: Listener) -> Router {
    Router::new()
        .route("/", get(crate::web::index))
        .route("/r/{id}", get(crate::web::review_page))
        .route("/static/style.css", get(static_css))
        .route("/static/app.js", get(static_js))
        .route("/api/reviews", get(crate::web::api_reviews))
        .route("/api/reviews/{id}", get(crate::web::api_review))
        .route("/api/hook/stop", get(stop_hook))
        .route("/setup", get(setup))
        .route("/setup/codex.toml", get(codex_toml))
        .route("/mcp", get(mcp_get).post(mcp).delete(mcp_delete))
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
        .layer(middleware::from_fn_with_state((hub.clone(), listener), identify))
        .with_state(hub)
}

fn host_name(host: &str) -> String {
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
    };
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// The host name in an http(s) URL.
pub fn url_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"))?;
    let name = host_name(rest.split(['/', '?', '#']).next().unwrap_or(""));
    (!name.is_empty()).then_some(name)
}

/// Names a browser may legitimately use for the relay; anything else is treated as DNS rebinding.
fn host_allowed(host: &str, public_url: Option<&str>) -> bool {
    let name = host_name(host);
    name.parse::<IpAddr>().is_ok()
        || name == "localhost"
        || name.ends_with(".ts.net")
        || (!name.is_empty() && !name.contains('.'))
        || public_url.and_then(url_host).is_some_and(|h| h == name)
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

/// A browser request made by another site's page. Browsers send this header on every request,
/// so requests without it (and without an `Origin`) come from agents' own clients.
fn from_another_site(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("cross-site") || v.eq_ignore_ascii_case("same-site"))
}

const OWNER_UNKNOWN: &str =
    "the relay is waiting for Tailscale to identify this machine's owner; start Tailscale, or run the relay with --no-tailnet";

async fn identify(
    State((hub, listener)): State<(Arc<Hub>, Listener)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut req: Request,
    next: Next,
) -> Response {
    let headers = req.headers();
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !host_allowed(host, hub.cfg.public_url.as_deref()) {
        return (StatusCode::FORBIDDEN, "host not allowed").into_response();
    }
    let path = req.uri().path();
    let agent_api = path == "/mcp";
    if (req.method() != axum::http::Method::GET || agent_api) && !origin_matches(headers) {
        return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
    }
    if agent_api && from_another_site(headers) {
        return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
    }
    let read_only = req.method() == axum::http::Method::GET
        && (path == "/" || ["/r/", "/static/", "/api/reviews"].iter().any(|p| path.starts_with(p)));
    let owner = hub.owner.lock().unwrap().clone();
    let who = match listener {
        Listener::Local if addr.ip().is_loopback() => match owner {
            Some(owner) => owner,
            None if read_only => return next.run(req).await,
            None => return (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "10")], OWNER_UNKNOWN).into_response(),
        },
        Listener::Local => return (StatusCode::FORBIDDEN, "not a local caller").into_response(),
        Listener::Tailnet => match hub.ts.whois(addr.ip()).await {
            Some(id) => {
                let allow = &hub.cfg.allow_logins;
                if !allow.is_empty() && !allow.iter().any(|l| l.eq_ignore_ascii_case(&id.login)) {
                    return (StatusCode::FORBIDDEN, "tailnet user not allowed").into_response();
                }
                id
            }
            None => {
                return (StatusCode::FORBIDDEN, "caller not identified by tailscale whois").into_response();
            }
        },
    };
    req.extensions_mut().insert(who);
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

#[derive(Deserialize)]
pub struct AgentQuery {
    agent: Option<String>,
    role: Option<String>,
    keep_waiting: Option<String>,
}

pub fn valid_agent(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `alice-claude` for alice@example.com using Claude Code, when no name is given.
pub fn default_agent(login: &str, client: &str) -> String {
    let user: String = login
        .split('@')
        .next()
        .unwrap_or("")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let c = client.to_ascii_lowercase();
    let kind = if c.contains("claude") {
        "claude"
    } else if c.contains("codex") {
        "codex"
    } else {
        "agent"
    };
    let user = user.trim_matches('-');
    if user.is_empty() { kind.to_string() } else { format!("{user}-{kind}") }
}

type Reject = (StatusCode, String);

/// The agent name the caller chose, if any.
fn chosen(q: &AgentQuery, headers: &HeaderMap) -> Result<Option<String>, Reject> {
    let given = q.agent.clone().or_else(|| headers.get("x-amx-agent").and_then(|v| v.to_str().ok()).map(String::from));
    match given.map(|s| s.trim().to_string()) {
        Some(n) if !valid_agent(&n) => Err((StatusCode::BAD_REQUEST, "agent name: 1-64 letters, digits, - _ .".into())),
        n => Ok(n),
    }
}

fn default_for(headers: &HeaderMap, who: &Identity) -> String {
    default_agent(&who.login, headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or(""))
}

fn agent_of(q: &AgentQuery, headers: &HeaderMap, who: &Identity) -> Result<String, Reject> {
    Ok(chosen(q, headers)?.unwrap_or_else(|| default_for(headers, who)))
}

fn role_of(q: &AgentQuery) -> Result<Option<Role>, Reject> {
    match q.role.as_deref().filter(|r| !r.is_empty()) {
        None => Ok(None),
        Some(r) => Role::parse(r)
            .map(Some)
            .ok_or_else(|| (StatusCode::BAD_REQUEST, "role must be reviewer or author".into())),
    }
}

/// An MCP session: the agent name it was given, and its waits by request id: running, or answered
/// with the `amx/seq` it has received once it sends its next request.
pub struct Session {
    agent: String,
    waits: HashMap<String, Option<u64>>,
    streams: usize,
    streamed: bool,
    /// Has called a tool; a client reconnecting may initialize sessions it never uses.
    called: bool,
    used: Instant,
}

impl Session {
    /// Whether it still holds its name: while its event stream is open, or for a client that
    /// never opened one, while it keeps calling.
    fn live(&self, online: Duration) -> bool {
        self.streams > 0 || (self.called && !self.streamed && self.used.elapsed() < online)
    }
}

const MAX_SESSIONS: usize = 4096;
const SESSION_IDLE: Duration = Duration::from_secs(3600);
const KEEPALIVE: Duration = Duration::from_secs(20);

fn session_id(headers: &HeaderMap) -> Option<&str> {
    let id = headers.get("mcp-session-id")?.to_str().ok()?;
    (!id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_graphic())).then_some(id)
}

/// Random, then the agent name, so a session keeps its name when the relay restarts.
fn new_session_id(agent: &str) -> String {
    use std::hash::{BuildHasher, RandomState};
    let n = || RandomState::new().hash_one(Instant::now());
    format!("{:016x}{:016x}-{agent}", n(), n())
}

/// Scoped to the caller, so nobody else can act in its session.
fn session_key(who: &Identity, id: &str) -> String {
    format!("{}\0{}\0{id}", who.user_id, who.login)
}

/// The session `id` of the caller and its agent, recreated if the relay doesn't know it.
fn session(hub: &Hub, who: &Identity, id: &str, chosen: Option<&str>, default: &str) -> Result<(String, String), Reject> {
    let key = session_key(who, id);
    let mut all = hub.sessions.lock().unwrap();
    if !all.contains_key(&key) && all.len() >= MAX_SESSIONS {
        all.retain(|_, s| s.streams > 0 || s.used.elapsed() < SESSION_IDLE);
        if all.len() >= MAX_SESSIONS {
            return Err((StatusCode::SERVICE_UNAVAILABLE, "too many MCP sessions; try again later".into()));
        }
    }
    let named = id.get(33..).filter(|n| valid_agent(n));
    let s = all.entry(key.clone()).or_insert_with(|| Session {
        agent: chosen.or(named).unwrap_or(default).to_string(),
        waits: HashMap::new(),
        streams: 0,
        streamed: false,
        called: false,
        used: Instant::now(),
    });
    s.used = Instant::now();
    Ok((key, s.agent.clone()))
}

/// A new session's name and what to tell it about that: a default name another live session
/// holds gets the next free suffix, and a chosen one is shared, with a warning.
fn name_session(hub: &Hub, who: &Identity, chosen: Option<String>, default: String) -> (String, String) {
    let online = Duration::from_secs(hub.cfg.timing.online_secs);
    let taken = |name: &str| {
        hub.sessions.lock().unwrap().values().any(|s| s.live(online) && s.agent.eq_ignore_ascii_case(name))
    };
    if let Some(name) = chosen {
        let note = if taken(&name) {
            format!("Another session is also connected as `{name}`: you share its events and tasks. To work apart, connect with a different `?agent=` name.")
        } else {
            String::new()
        };
        return (name, note);
    }
    let free = (1..=20)
        .map(|n| if n == 1 { default.clone() } else { format!("{default}-{n}") })
        .find(|n| !taken(n) && hub.check_name(n, who).is_ok());
    match free {
        Some(name) if name != default => {
            let note = format!("Another session holds `{default}`, so you are `{name}`.");
            (name, note)
        }
        _ => (default, String::new()),
    }
}

/// Acknowledges what the session's earlier waits handed out when its client sends a request, and
/// notes a wait it starts; a cancelled wait's events stay queued, even if it is answered later.
fn session_seen(hub: &Hub, key: &str, agent: &str, msg: &Value) {
    let acked = {
        let mut all = hub.sessions.lock().unwrap();
        let Some(s) = all.get_mut(key) else { return };
        if msg["method"] == "notifications/cancelled" {
            s.waits.remove(&msg["params"]["requestId"].to_string());
            None
        } else if let Some(id) = msg.get("id") {
            s.called |= msg["method"] == "tools/call";
            let acked = s.waits.values().flatten().copied().max();
            s.waits.retain(|_, n| n.is_none());
            if msg["method"] == "tools/call" && msg["params"]["name"] == "wait" {
                s.waits.insert(id.to_string(), None);
            }
            acked
        } else {
            None
        }
    };
    if let Some(n) = acked {
        hub.write(|s, _| s.ack(agent, n));
    }
}

/// Records the events the reply to wait `id` handed out, to be acknowledged later, unless the
/// client has cancelled it.
fn answered(hub: &Hub, key: &str, id: &str, seq: u64) {
    if let Some(wait) = hub.sessions.lock().unwrap().get_mut(key).and_then(|s| s.waits.get_mut(id)) {
        *wait = Some(seq);
    }
}

fn wants_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"))
}

fn sse_message(v: &Value) -> String {
    format!("event: message\ndata: {v}\n\n")
}

/// An event stream of what `rx` sends; `done` gets the first seq sent along once the client has
/// taken all of it, and is never called if the stream breaks first.
fn sse(rx: mpsc::Receiver<(String, Option<u64>)>, done: impl FnOnce(u64) + Send + 'static) -> Response {
    let body = futures_util::stream::unfold((rx, None, Some(done)), |(mut rx, seq, mut done)| async move {
        match rx.recv().await {
            Some((s, n)) => Some((Ok::<_, std::convert::Infallible>(Bytes::from(s)), (rx, seq.or(n), done))),
            None => {
                if let (Some(n), Some(f)) = (seq, done.take()) {
                    f(n);
                }
                None
            }
        }
    });
    (
        [(header::CONTENT_TYPE, "text/event-stream"), (header::CACHE_CONTROL, "no-store")],
        axum::body::Body::from_stream(body),
    )
        .into_response()
}

async fn mcp(
    State(hub): St,
    Extension(who): Extension<Identity>,
    Query(q): Query<AgentQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (chosen, role) = match (chosen(&q, &headers), role_of(&q)) {
        (Ok(c), Ok(r)) => (c, r),
        (Err(e), _) | (_, Err(e)) => return e.into_response(),
    };
    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return Json(crate::mcp::error_response(Value::Null, -32700, &format!("parse error: {e}"))).into_response(),
    };
    let default = default_for(&headers, &who);
    let initialize = value["method"] == "initialize";
    let (agent, key, note, new_id) = if initialize {
        let (agent, note) = name_session(&hub, &who, chosen, default);
        let id = new_session_id(&agent);
        match session(&hub, &who, &id, Some(&agent), &agent) {
            Ok((key, _)) => (agent, Some(key), note, Some(id)),
            Err(e) => return e.into_response(),
        }
    } else if let Some(id) = session_id(&headers) {
        match session(&hub, &who, id, chosen.as_deref(), &default) {
            Ok((key, agent)) => (agent, Some(key), String::new(), None),
            Err(e) => return e.into_response(),
        }
    } else {
        (chosen.unwrap_or(default), None, String::new(), None)
    };
    if let Some(key) = &key {
        session_seen(&hub, key, &agent, &value);
    }
    let id = value.get("id").map(Value::to_string).unwrap_or_default();
    let stream = key.is_some() && value["method"] == "tools/call" && value["params"]["name"] == "wait" && wants_stream(&headers);
    let seq_of = |resp: &Value| resp["result"]["_meta"]["amx/seq"].as_u64();
    if stream {
        let token = value["params"]["_meta"]["progressToken"].clone();
        let (tx, rx) = mpsc::channel(4);
        let key = key.unwrap_or_default();
        let done = {
            let (hub, id) = (hub.clone(), id.clone());
            move |n| answered(&hub, &key, &id, n)
        };
        tokio::spawn(async move {
            let c = crate::mcp::Caller { agent: &agent, who: &who, role, session: true, stream: true, note: "" };
            let call = crate::mcp::handle(&hub, &c, value);
            tokio::pin!(call);
            let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + KEEPALIVE, KEEPALIVE);
            let mut n = 0;
            let resp = loop {
                tokio::select! {
                    r = &mut call => break r,
                    _ = tx.closed() => return,
                    _ = beat.tick() => {
                        n += 1;
                        let note = if token.is_null() {
                            ": waiting\n\n".to_string()
                        } else {
                            sse_message(&json!({"jsonrpc": "2.0", "method": "notifications/progress",
                                "params": {"progressToken": token, "progress": n, "message": "waiting for the relay"}}))
                        };
                        // A client that has gone away drops the wait; what it was handed stays queued.
                        if tx.send((note, None)).await.is_err() {
                            return;
                        }
                    }
                }
            };
            if let Some(resp) = resp {
                let _ = tx.send((sse_message(&resp), seq_of(&resp))).await;
            }
        });
        return sse(rx, done);
    }
    let c = crate::mcp::Caller { agent: &agent, who: &who, role, session: key.is_some(), stream: false, note: &note };
    match crate::mcp::handle(&hub, &c, value).await {
        Some(resp) => {
            if let (Some(key), Some(n)) = (&key, seq_of(&resp)) {
                answered(&hub, key, &id, n);
            }
            let mut r = Json(resp).into_response();
            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            if let Some(v) = new_id.and_then(|id| HeaderValue::from_str(&id).ok()) {
                r.headers_mut().insert("mcp-session-id", v);
            }
            r
        }
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// A session's event stream. Clients keep it open while they run, so it keeps the agent online
/// while it reads code between calls; the relay sends nothing on it but keepalives.
async fn mcp_get(State(hub): St, Extension(who): Extension<Identity>, Query(q): Query<AgentQuery>, headers: HeaderMap) -> Response {
    let Some(id) = session_id(&headers).filter(|_| wants_stream(&headers)) else {
        return (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")]).into_response();
    };
    let chosen = match chosen(&q, &headers) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let (key, agent) = match session(&hub, &who, id, chosen.as_deref(), &default_for(&headers, &who)) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = hub.touch(&agent, &who, None) {
        return (StatusCode::CONFLICT, format!("{e:#}")).into_response();
    }
    if let Some(s) = hub.sessions.lock().unwrap().get_mut(&key) {
        s.streams += 1;
        s.streamed = true;
    }
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        {
            let _online = hub.waiting(&agent);
            let mut down = hub.shutdown.subscribe();
            while tx.send((": alive\n\n".into(), None)).await.is_ok() {
                tokio::select! {
                    _ = tokio::time::sleep(KEEPALIVE) => {}
                    _ = tx.closed() => break,
                    _ = down.wait_for(|d| *d) => break,
                }
            }
        }
        if let Some(s) = hub.sessions.lock().unwrap().get_mut(&key) {
            s.streams -= 1;
            s.used = Instant::now();
        }
    });
    sse(rx, |_| {})
}

/// Ends a session, which frees its name.
async fn mcp_delete(State(hub): St, Extension(who): Extension<Identity>, headers: HeaderMap) -> StatusCode {
    if let Some(id) = session_id(&headers) {
        hub.sessions.lock().unwrap().remove(&session_key(&who, id));
    }
    StatusCode::OK
}

/// A Stop hook for Codex or Claude Code, run with curl: blocks stopping while the agent owes the
/// relay something (or always for a looping reviewer with `keep_waiting`), else says nothing.
async fn stop_hook(State(hub): St, Extension(who): Extension<Identity>, Query(q): Query<AgentQuery>, headers: HeaderMap) -> Response {
    let agent = match agent_of(&q, &headers, &who) {
        Ok(a) => a,
        Err(r) => return r.into_response(),
    };
    if let Err(e) = hub.check_name(&agent, &who) {
        return (StatusCode::CONFLICT, format!("{e:#}")).into_response();
    }
    let (actions, pending, role) =
        hub.read(|s, now| (s.next_actions(&agent, now), s.has_undelivered(&agent), s.agents.get(&agent).map(|a| a.role)));
    let role = role.unwrap_or(Role::Author);
    let reason = if !actions.is_empty() || pending {
        let mut text = crate::mcp::next_text(&actions, role, &[], &[]);
        if pending {
            text = format!("You have undelivered amx events: call `wait` now.\n\n{text}");
        }
        format!("[amx] You still have an active review obligation on the relay; do not stop.\n{text}")
    } else if q.keep_waiting.is_some() && role == Role::Reviewer {
        "[amx] You are a looping reviewer: call the amx `wait` tool again and handle what it returns.".into()
    } else {
        return StatusCode::NO_CONTENT.into_response();
    };
    let reason: String = reason.chars().take(6000).collect();
    Json(json!({ "decision": "block", "reason": reason })).into_response()
}

/// How to connect each client, with this relay's address and the caller's default names.
async fn setup(State(hub): St, Extension(who): Extension<Identity>) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], crate::text::setup(&hub.web_url(), &who.login))
}

async fn codex_toml(State(hub): St) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], crate::text::codex_toml(&hub.web_url()))
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
            assert!(host_allowed(ok, None), "{ok}");
        }
        for bad in ["evil.com", "evil.com:7341", "", "127.0.0.1.evil.com"] {
            assert!(!host_allowed(bad, None), "{bad}");
        }
        let public = Some("https://amx.example.com/");
        assert!(host_allowed("amx.example.com", public));
        assert!(host_allowed("AMX.example.com:443", public));
        assert!(!host_allowed("evil.com", public));
        assert_eq!(url_host("http://[fd7a::1]:7341/r/r1").as_deref(), Some("fd7a::1"));
        assert_eq!(url_host("amx.example.com"), None);
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

    #[test]
    fn connections_are_capped_per_address_and_in_total_and_this_machines_apart() {
        let conns = Arc::new(Conns::default());
        let limits = ConnLimits {
            max: 3,
            per_ip: 2,
            own: 4,
            header_timeout: Duration::from_secs(1),
        };
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let a = conns.admit(ip("100.64.0.1"), false, &limits).unwrap();
        let _a2 = conns.admit(ip("100.64.0.1"), false, &limits).unwrap();
        assert!(conns.admit(ip("100.64.0.1"), false, &limits).is_none(), "per address");
        let _b = conns.admit(ip("100.64.0.2"), false, &limits).unwrap();
        assert!(conns.admit(ip("100.64.0.3"), false, &limits).is_none(), "in total");
        let own: Vec<_> = ["127.0.0.1", "::1", "127.0.0.2", "100.64.0.9"].map(|s| conns.admit(ip(s), true, &limits)).into();
        assert!(own.iter().all(Option::is_some), "this machine's own are outside the other caps");
        assert!(conns.admit(ip("127.0.0.3"), true, &limits).is_none(), "but share a cap of their own");
        drop(a);
        assert!(conns.admit(ip("100.64.0.3"), false, &limits).is_some(), "a closed connection frees its slot");
    }

    #[test]
    fn default_names() {
        assert_eq!(default_agent("alice@example.com", "claude-code/2.1"), "alice-claude");
        assert_eq!(default_agent("bob.smith@github", "codex_cli_rs/0.160"), "bob-smith-codex");
        assert_eq!(default_agent("", "curl/8"), "agent");
    }
}
