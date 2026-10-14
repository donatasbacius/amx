use crate::hub::Hub;
use crate::relay::Role;
use crate::tailscale::Identity;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use futures_util::Stream;
use serde::Deserialize;
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

type St = State<Arc<Hub>>;

pub fn router(hub: Arc<Hub>, listener: Listener) -> Router {
    Router::new()
        .route("/", get(crate::web::index))
        .route("/r/{id}", get(crate::web::review_page))
        .route("/static/style.css", get(static_css))
        .route("/static/app.js", get(static_js))
        .route("/api/events", get(events))
        .route("/api/heartbeat", post(heartbeat))
        .route("/api/watch", get(watch))
        .route("/api/obligation", get(obligation))
        .route("/mcp", post(mcp).get(mcp_get).delete(mcp_get))
        .layer(middleware::from_fn_with_state((hub.clone(), listener), identify))
        .with_state(hub)
}

/// Names a browser may legitimately use for the relay; anything else is treated as DNS rebinding.
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

async fn identify(
    State((hub, listener)): State<(Arc<Hub>, Listener)>,
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
    let who = match listener {
        Listener::Local if addr.ip().is_loopback() => hub.owner.lock().unwrap().clone(),
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

async fn events(State(hub): St) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    use futures_util::StreamExt;
    let rx = hub.subscribe();
    let mut down = hub.shutdown.subscribe();
    let stream = tokio_stream::wrappers::WatchStream::from_changes(rx)
        .map(|v| Ok(Event::default().event("change").data(v.to_string())))
        .take_until(async move {
            let _ = down.wait_for(|d| *d).await;
        });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20)))
}

#[derive(Deserialize)]
pub struct AgentQuery {
    agent: Option<String>,
    role: Option<String>,
    after: Option<u64>,
    timeout: Option<u64>,
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

fn agent_of(q: &AgentQuery, headers: &HeaderMap, who: &Identity) -> Result<String, Reject> {
    let given = q
        .agent
        .clone()
        .or_else(|| headers.get("x-amx-agent").and_then(|v| v.to_str().ok()).map(String::from))
        .map(|s| s.trim().to_string());
    match given {
        Some(n) if valid_agent(&n) => Ok(n),
        Some(_) => Err((StatusCode::BAD_REQUEST, "agent name: 1-64 letters, digits, - _ .".into())),
        None => {
            let ua = headers
                .get("x-amx-client")
                .or_else(|| headers.get(header::USER_AGENT))
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            Ok(default_agent(&who.login, ua))
        }
    }
}

fn role_of(q: &AgentQuery) -> Result<Option<Role>, Reject> {
    match q.role.as_deref().filter(|r| !r.is_empty()) {
        None => Ok(None),
        Some(r) => Role::parse(r)
            .map(Some)
            .ok_or_else(|| (StatusCode::BAD_REQUEST, "role must be reviewer or author".into())),
    }
}

async fn mcp(
    State(hub): St,
    Extension(who): Extension<Identity>,
    Query(q): Query<AgentQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let agent = match agent_of(&q, &headers, &who) {
        Ok(a) => a,
        Err(r) => return r.into_response(),
    };
    let role = match role_of(&q) {
        Ok(r) => r,
        Err(r) => return r.into_response(),
    };
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return Json(crate::mcp::error_response(serde_json::Value::Null, -32700, &format!("parse error: {e}")))
                .into_response();
        }
    };
    match crate::mcp::handle(&hub, &agent, &who, role, value).await {
        Some(resp) => {
            let mut r = Json(resp).into_response();
            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            r
        }
        None => StatusCode::ACCEPTED.into_response(),
    }
}

async fn mcp_get() -> impl IntoResponse {
    (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")])
}

fn named(hub: &Hub, q: &AgentQuery, headers: &HeaderMap, who: &Identity, touch: bool) -> Result<String, Reject> {
    let agent = agent_of(q, headers, who)?;
    if touch {
        let role = role_of(q)?;
        hub.touch(&agent, who, role)
            .map_err(|e| (StatusCode::CONFLICT, format!("{e:#}")))?;
    }
    Ok(agent)
}

async fn heartbeat(State(hub): St, Extension(who): Extension<Identity>, Query(q): Query<AgentQuery>, headers: HeaderMap) -> Response {
    match named(&hub, &q, &headers, &who, true) {
        Ok(agent) => Json(json!({ "agent": agent })).into_response(),
        Err(r) => r.into_response(),
    }
}

/// Long-polls an agent's events without consuming them, for `amx watch`.
async fn watch(State(hub): St, Extension(who): Extension<Identity>, Query(q): Query<AgentQuery>, headers: HeaderMap) -> Response {
    let agent = match named(&hub, &q, &headers, &who, true) {
        Ok(a) => a,
        Err(r) => return r.into_response(),
    };
    let timeout = Duration::from_secs(q.timeout.unwrap_or(50).clamp(1, 300));
    let (events, last) = hub.watch_events(&agent, q.after.unwrap_or(0), timeout).await;
    let lines: Vec<String> = events.iter().map(crate::text::event_line).collect();
    Json(json!({ "agent": agent, "last": last, "lines": lines })).into_response()
}

/// What the agent still owes, for the Codex Stop hook. Does not count as activity.
async fn obligation(State(hub): St, Extension(who): Extension<Identity>, Query(q): Query<AgentQuery>, headers: HeaderMap) -> Response {
    let agent = match named(&hub, &q, &headers, &who, false) {
        Ok(a) => a,
        Err(r) => return r.into_response(),
    };
    let (actions, pending, role) = hub.read(|s, now| {
        (
            s.next_actions(&agent, now),
            s.has_undelivered(&agent),
            s.agents.get(&agent).map(|a| a.role),
        )
    });
    let role = role.unwrap_or(Role::Author);
    let mut text = crate::mcp::next_text(&actions, role, false);
    if pending {
        text = format!("You have undelivered amx events: call `wait` now.\n\n{text}");
    }
    Json(json!({
        "agent": agent,
        "active": !actions.is_empty() || pending,
        "role": role.as_str(),
        "text": text,
    }))
    .into_response()
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

    #[test]
    fn default_names() {
        assert_eq!(default_agent("alice@example.com", "claude-code/2.1"), "alice-claude");
        assert_eq!(default_agent("bob.smith@github", "codex_cli_rs/0.160"), "bob-smith-codex");
        assert_eq!(default_agent("", "curl/8"), "agent");
    }
}
