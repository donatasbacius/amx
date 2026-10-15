//! The relay over HTTP: identity, MCP, long-polls and the web page.

use amx::Running;
use amx::config::Config;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

const PR: &str = "https://github.com/rust-lang/rust/pull/1";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn whois_ok(dir: &Path) -> PathBuf {
    write_script(
        dir,
        "ts-ok",
        r#"echo '{"Node":{"Name":"friendbox.example.ts.net."},"UserProfile":{"LoginName":"friend@example.com","DisplayName":"Friend"}}'"#,
    )
}

struct Relay {
    running: Running,
    local: String,
    tailnet: String,
}

async fn start(ts: Option<PathBuf>, allow: Vec<String>) -> Relay {
    let tport = free_port();
    let cfg = Config {
        port: free_port(),
        tailnet: false,
        tailnet_listen: ts.is_some().then(|| format!("127.0.0.1:{tport}").parse().unwrap()),
        tailscale_bin: ts,
        allow_logins: allow,
        ..Config::default()
    };
    let running = amx::start(cfg).await.unwrap();
    let local = format!("http://{}", running.local_addr);
    let tailnet = format!("http://127.0.0.1:{tport}");
    for _ in 0..100 {
        if running.hub.listening.lock().unwrap().len() == 2 || running.hub.cfg.tailnet_listen.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Relay { running, local, tailnet }
}

async fn rpc(base: &str, agent: &str, body: Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .query(&[("agent", agent)])
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

async fn tool(base: &str, agent: &str, name: &str, args: Value) -> (bool, String) {
    let (status, v) = rpc(
        base,
        agent,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let text = v["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
    (v["result"]["isError"].as_bool().unwrap_or(true), text)
}

async fn ok_tool(base: &str, agent: &str, name: &str, args: Value) -> String {
    let (err, text) = tool(base, agent, name, args).await;
    assert!(!err, "{name}: {text}");
    text
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_role_does_not_override_hello_on_requests_or_heartbeats() {
    let relay = start(None, vec![]).await;
    for (initial, chosen) in [("author", "reviewer"), ("reviewer", "author")] {
        let agent = format!("initial-{initial}");
        let client = amx::client::Client::new(&relay.local, Some(agent.clone()), Some(initial.into()));
        let init = client
            .mcp(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
            .await
            .unwrap()
            .unwrap();
        assert!(init["result"]["instructions"].as_str().unwrap().contains(&format!("Your role is {initial}")));

        let hello = client
            .mcp(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"hello","arguments":{"role":chosen}}}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hello["result"]["isError"], false, "{hello}");

        let online = json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"online","arguments":{}}});
        let response = client.mcp(&online).await.unwrap().unwrap();
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains(&format!("{agent} (you) ({chosen},")), "request reset role: {text}");

        client.heartbeat().await.unwrap();
        let text = ok_tool(&relay.local, &agent, "online", json!({})).await;
        assert!(text.contains(&format!("{agent} (you) ({chosen},")), "heartbeat reset role: {text}");
    }
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_role_applies_to_a_new_session() {
    let relay = start(None, vec![]).await;
    let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}});
    let first = amx::client::Client::new(&relay.local, Some("returning".into()), Some("author".into()));
    let init = first.mcp(&initialize).await.unwrap().unwrap();
    assert!(init["result"]["instructions"].as_str().unwrap().contains("Your role is author"));

    let second = amx::client::Client::new(&relay.local, Some("returning".into()), Some("reviewer".into()));
    let init = second.mcp(&initialize).await.unwrap().unwrap();
    assert!(init["result"]["instructions"].as_str().unwrap().contains("Your role is reviewer"), "{init}");
    let text = ok_tool(&relay.local, "returning", "online", json!({})).await;
    assert!(text.contains("returning (you) (reviewer,"), "{text}");
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unidentified_tailnet_callers_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let ts_fail = write_script(tmp.path(), "ts-fail", "echo 'peer not found' >&2; exit 1");
    let relay = start(Some(ts_fail), vec![]).await;
    let http = reqwest::Client::new();
    let page = http.get(format!("{}/", relay.tailnet)).send().await.unwrap();
    assert_eq!(page.status(), 403);
    let (status, _) = rpc(&relay.tailnet, "x", json!({"jsonrpc":"2.0","id":1,"method":"ping"})).await;
    assert_eq!(status, 403);
    let hb = http.post(format!("{}/api/heartbeat?agent=x", relay.tailnet)).send().await.unwrap();
    assert_eq!(hb.status(), 403);
    let local = http.get(format!("{}/", relay.local)).send().await.unwrap();
    assert_eq!(local.status(), 200, "loopback is the relay's owner");
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tailnet_identity_and_browser_checks() {
    let tmp = tempfile::tempdir().unwrap();
    let relay = start(Some(whois_ok(tmp.path())), vec![]).await;
    let (status, v) = rpc(
        &relay.tailnet,
        "friend-claude",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"claude-code"}}}),
    )
    .await;
    assert_eq!(status, 200);
    assert!(v["result"]["instructions"].as_str().unwrap().contains("friend-claude"));
    let online = ok_tool(&relay.tailnet, "friend-claude", "online", json!({})).await;
    assert!(online.contains("friend-claude (you) (author, friend@example.com on friendbox)"), "{online}");

    let (err, text) = tool(&relay.local, "friend-claude", "online", json!({})).await;
    assert!(err && text.contains("in use by another tailnet user"), "{text}");

    let http = reqwest::Client::new();
    let rebind = http.get(format!("{}/", relay.tailnet)).header("Host", "evil.example").send().await.unwrap();
    assert_eq!(rebind.status(), 403);
    let csrf = http
        .post(format!("{}/mcp?agent=x", relay.tailnet))
        .header("Origin", "https://evil.example")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
        .send()
        .await
        .unwrap();
    assert_eq!(csrf.status(), 403);
    let page = http.get(format!("{}/", relay.tailnet)).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("friend@example.com") && page.contains("friend-claude"));
    relay.running.stop().await;

    let relay = start(Some(whois_ok(tmp.path())), vec!["someone-else@example.com".into()]).await;
    let page = http.get(format!("{}/", relay.tailnet)).send().await.unwrap();
    assert_eq!(page.status(), 403, "allow_logins");
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn review_over_mcp_with_long_polls() {
    let tmp = tempfile::tempdir().unwrap();
    let relay = start(Some(whois_ok(tmp.path())), vec![]).await;
    let (author, reviewer) = (relay.local.clone(), relay.tailnet.clone());
    let hello = ok_tool(&reviewer, "friend-rev", "hello", json!({"role":"reviewer"})).await;
    assert!(hello.contains("loop on `wait`"), "{hello}");

    let waiting = tokio::spawn({
        let reviewer = reviewer.clone();
        async move { ok_tool(&reviewer, "friend-rev", "wait", json!({"timeout_seconds": 20})).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let req = ok_tool(
        &author,
        "owner-codex",
        "request_review",
        json!({"pr_url": format!("{PR}/files"), "reviewers": 1, "note": "please look"}),
    )
    .await;
    assert!(req.contains("Review r1 requested; offered to 1 idle reviewer"), "{req}");
    let offer = tokio::time::timeout(Duration::from_secs(5), waiting).await.unwrap().unwrap();
    assert!(offer.contains("Review offer") && offer.contains("accept_review"), "{offer}");

    let acc = ok_tool(&reviewer, "friend-rev", "accept_review", json!({"review_id":"r1"})).await;
    assert!(acc.contains("gh pr diff"), "{acc}");
    let wait = ok_tool(&reviewer, "friend-rev", "wait", json!({"timeout_seconds": 5})).await;
    assert!(wait.contains("submit_findings"), "owed work returns at once: {wait}");
    let sub = ok_tool(
        &reviewer,
        "friend-rev",
        "submit_findings",
        json!({"review_id":"r1","head_sha":"0123456789abcdef","verdict":"changes_requested","summary":"ok",
               "findings":[{"severity":"major","url":"https://github.com/rust-lang/rust/blob/0123456/src/a.rs#L3","text":"<script>alert(1)</script> breaks `Vec<T>`"}]}),
    )
    .await;
    assert!(sub.contains("Submitted 1 finding"), "{sub}");
    let result = ok_tool(&author, "owner-codex", "wait", json!({"timeout_seconds": 5})).await;
    assert!(result.contains("CHANGES REQUESTED") && result.contains("request_rereview"), "{result}");

    let page = reqwest::get(format!("{author}/r/r1")).await.unwrap().text().await.unwrap();
    assert!(!page.contains("<script>alert"), "escaped");
    assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    let status = ok_tool(&author, "owner-codex", "review_status", json!({"review_id":"r1"})).await;
    assert!(status.contains("Transcript"), "{status}");
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_heartbeat_and_obligation() {
    let relay = start(None, vec![]).await;
    let base = relay.local.clone();
    let http = reqwest::Client::new();
    let hb: Value = http
        .post(format!("{base}/api/heartbeat?agent=rev&role=reviewer"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(hb["agent"], "rev");
    let ob: Value = http.get(format!("{base}/api/obligation?agent=rev")).send().await.unwrap().json().await.unwrap();
    assert_eq!(ob["active"], false);
    assert_eq!(ob["role"], "reviewer");

    let watch = tokio::spawn({
        let base = base.clone();
        async move {
            reqwest::get(format!("{base}/api/watch?agent=rev&after=0&timeout=10"))
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    ok_tool(&base, "author", "request_review", json!({"pr_url": PR, "reviewers": 1})).await;
    let w = watch.await.unwrap();
    assert!(w["lines"][0].as_str().unwrap().starts_with("amx r1 [r1] Review offer"), "{w}");
    let ob: Value = http.get(format!("{base}/api/obligation?agent=rev")).send().await.unwrap().json().await.unwrap();
    assert_eq!(ob["active"], true);
    assert!(ob["text"].as_str().unwrap().contains("accept_review"));
    let ob: Value = http.get(format!("{base}/api/obligation?agent=author")).send().await.unwrap().json().await.unwrap();
    assert_eq!(ob["active"], true, "author waits for the result");
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn state_is_saved_while_running_and_restored() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.json");
    let cfg = Config {
        port: 0,
        tailnet: false,
        state: Some(path.clone()),
        ..Config::default()
    };
    let running = amx::start(cfg.clone()).await.unwrap();
    let base = format!("http://{}", running.local_addr);
    ok_tool(&base, "owner-codex", "request_review", json!({"pr_url": PR, "reviewers": 1})).await;
    let mut saved = String::new();
    for _ in 0..50 {
        saved = std::fs::read_to_string(&path).unwrap();
        if saved.contains("\"r1\"") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(saved.contains("\"r1\""), "saved without a shutdown: {saved}");

    let again = amx::start(cfg).await.unwrap();
    let page = reqwest::get(format!("http://{}/r/r1", again.local_addr)).await.unwrap().text().await.unwrap();
    assert!(page.contains("rust-lang/rust#1"), "{page}");
    again.stop().await;
    running.stop().await;
}
