use crate::hub::Hub;
use crate::relay::Role;
use crate::tailscale::Identity;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
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
        .route("/r/{id}/cancel", post(crate::web::cancel))
        .route("/r/{id}/f/{fid}", post(crate::web::rule))
        .route("/static/style.css", get(static_css))
        .route("/static/app.js", get(static_js))
        .route("/api/reviews", get(crate::web::api_reviews))
        .route("/api/reviews/{id}", get(crate::web::api_review))
        .route("/setup", get(setup))
        .route("/mcp", get(mcp_get).post(mcp).delete(mcp_delete))
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
        .layer(middleware::from_fn_with_state((hub.clone(), listener), identify))
        .layer(middleware::map_response(no_framing))
        .with_state(hub)
}

/// No page of the relay may be shown inside another site's frame, where a click could be stolen.
async fn no_framing(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("frame-ancestors 'none'"));
    r
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

/// A header a reverse proxy adds, which a loopback request from the owner's own browser lacks:
/// any naming a forwarded request or a client address, `Via`, or Tailscale's.
fn proxy_header(h: &str) -> bool {
    h == "via" || h.contains("forwarded") || h.ends_with("-ip") || h.starts_with("tailscale-")
}

/// Marks a request from the relay's owner: its Tailscale login over the tailnet, or loopback with
/// no proxy header, where no login names the person.
#[derive(Clone, Copy)]
pub struct Owner {
    pub loopback: bool,
}

/// Who a page is shown to: a tailnet login as Tailscale reports it, a browser on the relay's own
/// machine, or a caller the relay can't name (proxied, or before Tailscale names the owner).
#[derive(Clone)]
pub enum Viewer {
    Login(String),
    ThisMachine,
    Unknown,
}

/// Whether an owner login can be matched over the tailnet: not `local`, which stands in for an owner
/// Tailscale hasn't named, nor `tagged-devices`, which every tagged node shares.
fn names_a_person(login: &str) -> bool {
    !login.eq_ignore_ascii_case("local") && !login.eq_ignore_ascii_case("tagged-devices")
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
    let human = req.method() == axum::http::Method::POST && path.starts_with("/r/");
    if human && (!headers.contains_key(header::ORIGIN) || from_another_site(headers)) {
        return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
    }
    let read_only = req.method() == axum::http::Method::GET
        && (path == "/" || ["/r/", "/static/", "/api/reviews"].iter().any(|p| path.starts_with(p)));
    if listener == Listener::Local && headers.keys().any(|h| proxy_header(h.as_str())) {
        if read_only {
            req.extensions_mut().insert(Viewer::Unknown);
            return next.run(req).await;
        }
        return (StatusCode::FORBIDDEN, "a proxied request can't act as the relay's owner; open the relay by its own address").into_response();
    }
    let owner = hub.owner.lock().unwrap().clone();
    let who = match listener {
        Listener::Local if addr.ip().is_loopback() => match owner {
            Some(owner) => {
                req.extensions_mut().insert(Owner { loopback: true });
                req.extensions_mut().insert(Viewer::ThisMachine);
                owner
            }
            None if read_only => {
                req.extensions_mut().insert(Viewer::Unknown);
                return next.run(req).await;
            }
            None => return (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "10")], OWNER_UNKNOWN).into_response(),
        },
        Listener::Local => return (StatusCode::FORBIDDEN, "not a local caller").into_response(),
        Listener::Tailnet => match hub.ts.whois(addr.ip()).await {
            Some(id) => {
                let allow = &hub.cfg.allow_logins;
                if !allow.is_empty() && !allow.iter().any(|l| l.eq_ignore_ascii_case(&id.login)) {
                    return (StatusCode::FORBIDDEN, "tailnet user not allowed").into_response();
                }
                if owner.is_some_and(|o| names_a_person(&o.login) && o.login.eq_ignore_ascii_case(&id.login)) {
                    req.extensions_mut().insert(Owner { loopback: false });
                }
                req.extensions_mut().insert(Viewer::Login(id.login.clone()));
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

/// The agent name and role the caller chose, if any.
fn chosen(q: &AgentQuery, headers: &HeaderMap) -> Result<(Option<String>, Option<Role>), Reject> {
    let given = q.agent.clone().or_else(|| headers.get("x-amx-agent").and_then(|v| v.to_str().ok()).map(String::from));
    let role = match q.role.as_deref().filter(|r| !r.is_empty()) {
        None => None,
        Some(r) => Some(Role::parse(r).ok_or_else(|| (StatusCode::BAD_REQUEST, "role must be reviewer or author".into()))?),
    };
    match given.map(|s| s.trim().to_string()) {
        Some(n) if !valid_agent(&n) => Err((StatusCode::BAD_REQUEST, "agent name: 1-64 letters, digits, - _ .".into())),
        n => Ok((n, role)),
    }
}

fn default_for(headers: &HeaderMap, who: &Identity) -> String {
    default_agent(&who.login, headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or(""))
}

/// An MCP session: its agent name and role, and its waits by request id: running, or answered
/// with the `amx/seq` it has received once it sends its next request.
pub struct Session {
    /// A chosen name from the start; a default one from its first call that acts.
    agent: Option<String>,
    role: Option<Role>,
    waits: HashMap<String, Option<u64>>,
    streams: usize,
    streamed: bool,
    used: Instant,
}

impl Session {
    /// Whether it still holds its name: while its event stream is open, or for a client that
    /// never opened one, while it keeps calling.
    fn live(&self, online: Duration) -> bool {
        self.streams > 0 || (!self.streamed && self.used.elapsed() < online)
    }
}

const SESSIONS_PER_LOGIN: usize = 64;
const KEEPALIVE: Duration = Duration::from_secs(20);

fn session_id(headers: &HeaderMap) -> Option<&str> {
    let id = headers.get("mcp-session-id")?.to_str().ok()?;
    (!id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_graphic())).then_some(id)
}

fn new_session_id() -> String {
    use std::hash::{BuildHasher, RandomState};
    let n = || RandomState::new().hash_one(Instant::now());
    format!("{:016x}{:016x}", n(), n())
}

/// Scoped to the caller, so nobody else can act in its session.
fn session_key(who: &Identity, id: &str) -> String {
    format!("{}\0{}\0{id}", who.user_id, who.login)
}

/// Moves `agent`'s count of open event streams.
fn streams(hub: &Hub, agent: Option<&str>, delta: i32) {
    if let Some(a) = agent.filter(|_| delta != 0) {
        hub.write(|s, now| s.agents.stream(a, delta, now));
    }
}

/// The caller's session `id`, recreated if the relay doesn't know it. Over the cap, the login's
/// least recently used session ends.
fn session(hub: &Hub, who: &Identity, id: &str, chosen: Option<&str>, role: Option<Role>) -> String {
    let key = session_key(who, id);
    let mine = session_key(who, "");
    let mut all = hub.sessions.lock().unwrap();
    if !all.contains_key(&key) {
        let own = || all.iter().filter(|(k, _)| k.starts_with(&mine));
        if own().count() >= SESSIONS_PER_LOGIN
            && let Some(old) = own().min_by_key(|(_, s)| s.used).map(|(k, _)| k.clone())
            && let Some(s) = all.remove(&old)
        {
            streams(hub, s.agent.as_deref(), -(s.streams as i32));
        }
    }
    let s = all.entry(key.clone()).or_insert_with(|| Session {
        agent: chosen.map(String::from),
        role,
        waits: HashMap::new(),
        streams: 0,
        streamed: false,
        used: Instant::now(),
    });
    s.used = Instant::now();
    key
}

/// Whether a live session holds `name`. Refused while a session of another login holds it, so a
/// running session keeps its name.
fn held(hub: &Hub, who: &Identity, name: &str) -> Result<bool, String> {
    let (online, mine) = (Duration::from_secs(hub.cfg.timing.online_secs), session_key(who, ""));
    let all = hub.sessions.lock().unwrap();
    let holders = || all.iter().filter(|(_, s)| s.agent.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(name)));
    if holders().any(|(k, _)| !k.starts_with(&mine)) {
        return Err(format!("agent name `{name}` is in use by another tailnet user; pick a different name"));
    }
    Ok(holders().any(|(_, s)| s.live(online)))
}

/// The role a call gives its session: `hello`'s, or author for a review request.
fn role_given(msg: &Value) -> Option<Role> {
    match msg["params"]["name"].as_str().filter(|_| msg["method"] == "tools/call")? {
        "hello" => Role::parse(msg["params"]["arguments"]["role"].as_str()?),
        "request_review" | "request_rereview" => Some(Role::Author),
        _ => None,
    }
}

/// The session's agent and the role to record for it. A session without a chosen name gets one
/// on its first call that acts (`active`), once its role is known: a free default name whose last
/// holder had that role, else one never used. A session that hasn't said its role takes the one
/// its free names had, unless they differ. A name shared with a live reviewer session stays a
/// reviewer.
fn named(hub: &Hub, who: &Identity, key: &str, default: &str, given: Option<Role>, active: bool) -> Result<(String, Option<Role>, String), String> {
    let online = Duration::from_secs(hub.cfg.timing.online_secs);
    let mine = session_key(who, "");
    let mut all = hub.sessions.lock().unwrap();
    let Some(s) = all.get(key) else { return Ok((String::new(), given, String::new())) };
    let mut role = given.or(s.role);
    let mut note = String::new();
    let others = || all.iter().filter(|(k, _)| *k != key);
    let name = match s.agent.clone() {
        Some(name) => name,
        None if !active => String::new(),
        None => {
            let free: Vec<String> = (1..=20)
                .map(|n| if n == 1 { default.to_string() } else { format!("{default}-{n}") })
                .filter(|n| {
                    !others().any(|(k, o)| {
                        o.agent.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(n)) && (o.live(online) || !k.starts_with(&mine))
                    })
                })
                .collect();
            let (name, r) = hub.read(|st, _| {
                let free: Vec<(&String, Option<Role>)> = free
                    .iter()
                    .filter_map(|n| match st.agents.all().find(|a| a.name.eq_ignore_ascii_case(n)) {
                        Some(a) if !a.login.eq_ignore_ascii_case(&who.login) => None,
                        a => Some((n, a.map(|a| a.role))),
                    })
                    .collect();
                let mut had = free.iter().filter_map(|(_, r)| *r);
                let role = match (role, had.next()) {
                    (Some(r), _) => r,
                    (None, None) => Role::Author,
                    (None, Some(r)) if had.all(|o| o == r) => r,
                    _ => return Err("call `hello` with your role (reviewer or author) first".to_string()),
                };
                let name = free.iter().find(|(_, r)| *r == Some(role)).or_else(|| free.iter().find(|(_, r)| r.is_none())).or(free.first());
                Ok((name.map_or(default.to_string(), |(n, _)| n.to_string()), role))
            })?;
            if name != default {
                note = format!("You are `{name}`.");
            }
            role = Some(r);
            hub.write(|st, _| st.new_session(&name));
            name
        }
    };
    let shared = others().any(|(_, o)| o.agent.as_ref() == Some(&name) && o.role == Some(Role::Reviewer) && o.live(online));
    let s = all.get_mut(key).expect("found");
    if s.agent.is_none() && !name.is_empty() {
        s.agent = Some(name.clone());
        streams(hub, s.agent.as_deref(), s.streams as i32);
    }
    s.role = role;
    let role = if shared && role == Some(Role::Author) { None } else { role };
    Ok((name, role, note))
}

/// What a new session that chose `name` is told: that it shares the name, if a live session holds it.
fn shared_note(hub: &Hub, who: &Identity, name: &str) -> Result<String, String> {
    hub.check_name(name, who).map_err(|e| format!("{e:#}"))?;
    Ok(match held(hub, who, name)? {
        true => format!("Another session is also connected as `{name}`: you share its events and tasks. To work apart, connect with a different `?agent=` name."),
        false => String::new(),
    })
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
    if let Some(n) = acked.filter(|_| !agent.is_empty()) {
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
    let (chosen, role) = match chosen(&q, &headers) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return Json(crate::mcp::error_response(Value::Null, -32700, &format!("parse error: {e}"))).into_response(),
    };
    let default = default_for(&headers, &who);
    let active = value["method"] == "tools/call" && value["params"]["name"].as_str().is_some_and(|t| crate::mcp::ACTIVE_TOOLS.contains(&t));
    let named = if value["method"] == "initialize" {
        let id = new_session_id();
        chosen
            .as_deref()
            .map_or(Ok(String::new()), |n| shared_note(&hub, &who, n))
            .map(|note| (chosen.clone().unwrap_or_default(), role, note, Some(session(&hub, &who, &id, chosen.as_deref(), role)), Some(id)))
    } else if let Some(id) = session_id(&headers) {
        let key = session(&hub, &who, id, chosen.as_deref(), role);
        named(&hub, &who, &key, &default, role_given(&value), active).map(|(a, r, note)| (a, r, note, Some(key), None))
    } else {
        Ok((chosen.unwrap_or(default), role_given(&value).or(role), String::new(), None, None))
    };
    let named = named.and_then(|n| if active { held(&hub, &who, &n.0).map(|_| n) } else { Ok(n) });
    let (agent, role, note, key, new_id) = match named {
        Ok(n) => n,
        Err(e) => return Json(crate::mcp::error_response(value.get("id").cloned().unwrap_or(Value::Null), -32000, &e)).into_response(),
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
            let c = crate::mcp::Caller { agent: &agent, who: &who, role, session: true, stream: true, note: &note };
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

/// Counts an event stream of session `key` opening or closing, and with it its agent's.
fn stream(hub: &Hub, key: &str, delta: i32) {
    if let Some(s) = hub.sessions.lock().unwrap().get_mut(key) {
        s.streams = s.streams.saturating_add_signed(delta as isize);
        s.streamed = true;
        s.used = Instant::now();
        streams(hub, s.agent.as_deref(), delta);
    }
}

/// A session's event stream. Clients keep it open while they run, so it keeps the agent online
/// while it reads code between calls; the relay sends nothing on it but keepalives.
async fn mcp_get(State(hub): St, Extension(who): Extension<Identity>, Query(q): Query<AgentQuery>, headers: HeaderMap) -> Response {
    let Some(id) = session_id(&headers).filter(|_| wants_stream(&headers)) else {
        return (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")]).into_response();
    };
    let (chosen, role) = match chosen(&q, &headers) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let key = session(&hub, &who, id, chosen.as_deref(), role);
    stream(&hub, &key, 1);
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        let mut down = hub.shutdown.subscribe();
        while tx.send((": alive\n\n".into(), None)).await.is_ok() {
            tokio::select! {
                _ = tokio::time::sleep(KEEPALIVE) => {}
                _ = tx.closed() => break,
                _ = down.wait_for(|d| *d) => break,
            }
        }
        stream(&hub, &key, -1);
    });
    sse(rx, |_| {})
}

/// Ends a session, which frees its name.
async fn mcp_delete(State(hub): St, Extension(who): Extension<Identity>, headers: HeaderMap) -> StatusCode {
    if let Some(id) = session_id(&headers) {
        let s = hub.sessions.lock().unwrap().remove(&session_key(&who, id));
        if let Some(s) = s {
            streams(&hub, s.agent.as_deref(), -(s.streams as i32));
        }
    }
    StatusCode::OK
}

/// How to connect each client, with this relay's address and the caller's default names.
async fn setup(State(hub): St, Extension(who): Extension<Identity>) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], crate::text::setup(&hub.web_url(), &who.login))
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
    fn proxy_headers() {
        for h in [
            "forwarded",
            "x-forwarded-for",
            "x-original-forwarded-for",
            "x-real-ip",
            "client-ip",
            "true-client-ip",
            "cf-connecting-ip",
            "x-cluster-client-ip",
            "via",
            "tailscale-user-login",
        ] {
            assert!(proxy_header(h), "{h}");
        }
        for h in [
            "host", "origin", "referer", "user-agent", "accept", "accept-encoding", "accept-language", "cookie",
            "connection", "content-type", "content-length", "cache-control", "pragma", "priority", "dnt",
            "upgrade-insecure-requests", "sec-ch-ua", "sec-ch-ua-mobile", "sec-ch-ua-platform", "sec-fetch-site",
            "sec-fetch-mode", "sec-fetch-dest", "sec-fetch-user", "sec-gpc", "if-none-match", "if-modified-since",
            "authorization", "mcp-session-id", "mcp-protocol-version", "last-event-id", "x-amx-agent", "traceparent",
        ] {
            assert!(!proxy_header(h), "{h}");
        }
    }

    #[test]
    fn default_names() {
        assert_eq!(default_agent("alice@example.com", "claude-code/2.1"), "alice-claude");
        assert_eq!(default_agent("bob.smith@github", "codex_cli_rs/0.160"), "bob-smith-codex");
        assert_eq!(default_agent("", "curl/8"), "agent");
    }
}
