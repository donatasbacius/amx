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
    let api = http.get(format!("{}/api/reviews", relay.tailnet)).send().await.unwrap();
    assert_eq!(api.status(), 403);
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
async fn json_api_lists_reviews_and_hides_unrevealed_findings() {
    let relay = start(None, vec![]).await;
    let base = relay.local.clone();
    let http = reqwest::Client::new();
    let get = |path: &str| http.get(format!("{base}{path}")).send();
    for rev in ["rev-a", "rev-b"] {
        ok_tool(&base, rev, "hello", json!({"role":"reviewer"})).await;
    }
    ok_tool(&base, "author", "request_review", json!({"pr_url": PR, "reviewers": ["rev-a", "rev-b"]})).await;
    for rev in ["rev-a", "rev-b"] {
        ok_tool(&base, rev, "accept_review", json!({"review_id":"r1"})).await;
    }
    let finding = json!([{"severity":"major","text":"SECRET until revealed\nsecond line"}]);
    ok_tool(&base, "rev-a", "submit_findings", json!({"review_id":"r1","head_sha":"abc1234","verdict":"changes_requested","summary":"PRIVATE","findings":finding})).await;

    let r = get("/api/reviews/r1").await.unwrap();
    assert_eq!(r.headers()["cache-control"], "no-store");
    let body = r.text().await.unwrap();
    assert!(!body.contains("SECRET") && !body.contains("PRIVATE"), "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((v["phase"].as_str(), v["revealed"].as_bool(), v["hidden_findings"].as_u64()), (Some("reviewing"), Some(false), Some(1)));
    assert_eq!(v["timeline"][1]["state"], "active");
    assert!(v["deadline_left_ms"].as_u64().unwrap() > 0);

    ok_tool(&base, "rev-b", "submit_findings", json!({"review_id":"r1","head_sha":"abc1234","verdict":"approve","findings":[]})).await;
    ok_tool(&base, "rev-b", "vote", json!({"review_id":"r1","votes":[{"finding":"F1","vote":"agree","severity":"major"}]})).await;
    let v: Value = get("/api/reviews/r1").await.unwrap().json().await.unwrap();
    assert_eq!(v["phase"], "done");
    assert_eq!(v["verdict"], "changes_requested");
    assert_eq!(v["findings"][0]["votes"]["rev-b"]["choice"], "Agree");
    assert_eq!(v["findings"][0]["votes"]["rev-b"]["severity"], "major", "the severity the vote backs");
    assert!(v["log"].as_array().unwrap().iter().any(|e| e["text"] == "F1: agree (major)"), "{}", v["log"]);
    assert_eq!(v["summary"][0], json!({"id":"F1","title":"SECRET until revealed","severity":"major","severity_split":null,"by":"rev-a","origin":null,"outcome":"confirmed","agree":2,"disagree":0}));
    assert!(v["result"]["text"].as_str().unwrap().contains("CHANGES REQUESTED"));
    assert!(v["log"].as_array().unwrap().iter().any(|e| e["kind"] == "result"));
    let stages: Vec<&str> = v["timeline"].as_array().unwrap().iter().map(|s| s["state"].as_str().unwrap()).collect();
    assert_eq!(stages, ["done", "done", "done", "done"]);
    assert!(v.get("next_finding").is_none());

    ok_tool(&base, "author", "request_review", json!({"pr_url": "https://github.com/rust-lang/rust/pull/2", "reviewers": 1})).await;
    let list: Value = get("/api/reviews").await.unwrap().json().await.unwrap();
    let ids: Vec<&str> = list["reviews"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["r2", "r1"], "newest first");
    assert_eq!(list["reviews"][0]["active"], true);
    assert_eq!(list["reviews"][1]["verdict"], "changes_requested");

    let missing = get("/api/reviews/r9").await.unwrap();
    assert_eq!(missing.status(), 404);
    let page = get("/r/r1").await.unwrap().text().await.unwrap();
    assert!(page.contains("id=\"F1\"") && page.contains("Changes requested") && page.contains(">Result<") && !page.contains("<pre>"), "{page}");
    let partial = http.get(format!("{base}/r/r1")).header("X-Amx-Partial", "1").send().await.unwrap();
    assert_eq!(partial.headers()["cache-control"], "no-store");
    assert!(!partial.text().await.unwrap().contains("<html"));
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
        let client = amx::client::Client::new(&base, Some("rev".into()), None);
        async move { client.watch(0, 10).await.unwrap() }
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

#[tokio::test(flavor = "multi_thread")]
async fn read_only_calls_do_not_register_the_caller() {
    let relay = start(None, vec![]).await;
    let base = relay.local.clone();
    for body in [
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":2,"method":"ping"}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    ] {
        rpc(&base, "probe", body).await;
    }
    let online = ok_tool(&base, "probe", "online", json!({})).await;
    assert!(online.contains("No agents online.") && online.contains("You (`probe`) are not online"), "{online}");
    ok_tool(&base, "probe", "review_status", json!({})).await;
    assert!(relay.running.hub.read(|s, _| s.agents.get("probe").is_none()));

    rpc(&base, "probe", json!({"jsonrpc":"2.0","id":3,"method":"initialize","params":{}})).await;
    let online = ok_tool(&base, "probe", "online", json!({})).await;
    assert!(online.contains("probe (you) (author,") && !online.contains("not online"), "{online}");
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn authors_name_reviewers_and_links_use_the_public_url() {
    let cfg = Config {
        port: 0,
        tailnet: false,
        public_url: Some("http://amx.example.com:7341/".into()),
        ..Config::default()
    };
    let running = amx::start(cfg).await.unwrap();
    let base = format!("http://{}", running.local_addr);
    for rev in ["rev-a", "rev-b"] {
        ok_tool(&base, rev, "hello", json!({"role":"reviewer"})).await;
    }
    let (err, text) = tool(&base, "author", "request_review", json!({"pr_url": PR, "reviewers": ["rev-a", "nobody"]})).await;
    assert!(err && text.contains("nobody (unknown agent)"), "{text}");
    let req = ok_tool(&base, "author", "request_review", json!({"pr_url": PR, "reviewers": ["rev-b"]})).await;
    assert!(req.contains("Review r1 requested from rev-b; 1 of them idle now"), "{req}");
    let (err, text) = tool(&base, "rev-a", "accept_review", json!({"review_id":"r1"})).await;
    assert!(err && text.contains("picked"), "{text}");
    ok_tool(&base, "rev-b", "accept_review", json!({"review_id":"r1"})).await;

    let status = ok_tool(&base, "author", "review_status", json!({"review_id":"r1"})).await;
    assert!(status.contains("http://amx.example.com:7341/r/r1"), "{status}");
    let page = reqwest::Client::new()
        .get(format!("{base}/"))
        .header("Host", "amx.example.com:7341")
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200, "the public name passes the host check");
    running.stop().await;
}

#[test]
fn tailnet_links_prefer_the_magicdns_name() {
    let addr = "192.0.2.7:7341".parse().unwrap();
    assert_eq!(amx::tailnet_url(addr, Some("relay.example.ts.net")), "http://relay.example.ts.net:7341");
    assert_eq!(amx::tailnet_url(addr, None), "http://192.0.2.7:7341");
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_points_to_tasks_instead_of_repeating_them() {
    let relay = start(None, vec![]).await;
    let base = relay.local.clone();
    for rev in ["rev-a", "rev-b"] {
        ok_tool(&base, rev, "hello", json!({"role":"reviewer"})).await;
    }
    ok_tool(&base, "author", "request_review", json!({"pr_url": PR, "reviewers": ["rev-a", "rev-b"]})).await;
    let offer = ok_tool(&base, "rev-a", "wait", json!({"timeout_seconds": 5})).await;
    assert!(offer.ends_with("NEXT: r1: accept or decline the offer above.") && offer.matches(PR).count() == 1, "{offer}");

    let acc = ok_tool(&base, "rev-a", "accept_review", json!({"review_id":"r1"})).await;
    assert!(acc.contains("gh pr diff") && acc.contains("at least 20m00s"), "starts at once: {acc}");
    ok_tool(&base, "rev-b", "accept_review", json!({"review_id":"r1"})).await;
    let findings = json!([{"severity":"major","text":"cache never invalidated"},{"severity":"minor","text":"typo in docs"}]);
    ok_tool(&base, "rev-a", "submit_findings", json!({"review_id":"r1","head_sha":"abc1234","verdict":"changes_requested","findings":findings})).await;
    let idle = ok_tool(&base, "rev-a", "wait", json!({"timeout_seconds": 1})).await;
    assert!(idle.starts_with("No new events."), "no second copy of the task: {idle}");
    let last = ok_tool(&base, "rev-b", "submit_findings", json!({"review_id":"r1","head_sha":"abc1234","verdict":"approve","findings":[{"severity":"nit","text":"rename x"}]})).await;
    assert!(last.contains("You must vote on: F1, F2.") && last.matches("cache never invalidated").count() == 1, "{last}");
    assert!(!last.contains("finding: \"F3\""), "the example never names your own finding: {last}");
    let owed = ok_tool(&base, "rev-b", "wait", json!({})).await;
    assert_eq!(owed, "NEXT: r1: vote on F1, F2 as described above.", "a task already given is only pointed to");

    let vote = ok_tool(&base, "rev-a", "wait", json!({"timeout_seconds": 5})).await;
    assert_eq!(vote.matches("rename x").count(), 1, "{vote}");
    assert!(vote.contains("You must vote on: F3.") && vote.ends_with("NEXT: r1: vote on F3 as described above."), "{vote}");
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}});
    assert_eq!(rpc(&base, "rev-a", init).await.0, 200);
    let again = ok_tool(&base, "rev-a", "wait", json!({})).await;
    assert!(again.contains("You must vote on: F3.") && again.contains("rename x"), "a new session gets the task in full: {again}");

    ok_tool(&base, "rev-a", "withdraw_finding", json!({"review_id":"r1","finding":"F2","reason":"fixed upstream"})).await;
    let voted = ok_tool(&base, "rev-b", "vote", json!({"review_id":"r1","votes":[{"finding":"F1","vote":"agree"},{"finding":"F2","vote":"agree"}]})).await;
    assert!(voted.contains("Recorded 1 vote(s) on r1. Ignored your votes on F2: withdrawn by their authors."), "{voted}");

    let quiet = ok_tool(&base, "author", "wait", json!({"timeout_seconds": 1})).await;
    assert!(quiet.starts_with("No new events.") && quiet.lines().count() == 3, "progress doesn't wake the author: {quiet}");
    ok_tool(&base, "rev-a", "vote", json!({"review_id":"r1","votes":[{"finding":"F3","vote":"agree"}]})).await;
    let result = ok_tool(&base, "author", "wait", json!({"timeout_seconds": 5})).await;
    assert!(result.starts_with("[r1] Review result") && !result.contains("===") && !result.contains("nothing pending"), "{result}");
    let finished = ok_tool(&base, "rev-b", "wait", json!({"timeout_seconds": 1, "progress": true})).await;
    assert!(finished.contains("Review finished"), "{finished}");
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closing_reply_window_wakes_waiting_reviewers() {
    let mut cfg = Config {
        port: 0,
        tailnet: false,
        ..Config::default()
    };
    cfg.timing.reply_secs = 1;
    let running = amx::start(cfg).await.unwrap();
    let base = format!("http://{}", running.local_addr);
    for rev in ["rev-a", "rev-b"] {
        ok_tool(&base, rev, "hello", json!({"role":"reviewer"})).await;
    }
    ok_tool(&base, "author", "request_review", json!({"pr_url": PR, "reviewers": ["rev-a", "rev-b"]})).await;
    for (rev, text) in [("rev-a", "race on shutdown"), ("rev-b", "leaks a file handle")] {
        ok_tool(&base, rev, "accept_review", json!({"review_id":"r1"})).await;
        let findings = json!([{"severity":"major","text":text}]);
        ok_tool(&base, rev, "submit_findings", json!({"review_id":"r1","head_sha":"abc1234","verdict":"changes_requested","findings":findings})).await;
    }
    ok_tool(&base, "rev-a", "vote", json!({"review_id":"r1","votes":[{"finding":"F2","vote":"disagree","reason":"no"}]})).await;
    let reply = ok_tool(&base, "rev-b", "vote", json!({"review_id":"r1","votes":[{"finding":"F1","vote":"disagree","reason":"no"}]})).await;
    assert!(reply.contains("contested"), "{reply}");
    let started = std::time::Instant::now();
    let wait = ok_tool(&base, "rev-b", "wait", json!({"timeout_seconds": 20})).await;
    assert!(started.elapsed() < Duration::from_secs(10), "woken when the window closed: {wait}");
    assert!(wait.contains("You must vote on: F1.") && !wait.contains("contested"), "{wait}");
    running.stop().await;
}

fn agent_in(path: &Path, name: &str) -> Value {
    let v: Value = serde_json::from_slice(&std::fs::read(path).unwrap_or_default()).unwrap_or_default();
    v["agents"].as_array().into_iter().flatten().find(|a| a["name"] == name).cloned().unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn delivered_events_are_saved_before_wait_returns() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.json");
    let cfg = Config {
        port: 0,
        tailnet: false,
        state: Some(path.clone()),
        ..Config::default()
    };
    let running = amx::start(cfg).await.unwrap();
    let base = format!("http://{}", running.local_addr);
    ok_tool(&base, "rev", "hello", json!({"role":"reviewer"})).await;
    ok_tool(&base, "author", "request_review", json!({"pr_url": PR, "reviewers": ["rev"]})).await;
    for _ in 0..50 {
        if agent_in(&path, "rev")["next_seq"] == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(agent_in(&path, "rev")["delivered"], 0);
    let offer = ok_tool(&base, "rev", "wait", json!({"timeout_seconds": 5})).await;
    assert!(offer.contains("Review offer"), "{offer}");
    assert_eq!(agent_in(&path, "rev")["delivered"], 1, "a crash now must not deliver the offer again");
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn downtime_after_a_crash_is_counted_from_the_last_heartbeat() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.json");
    let alive = path.with_file_name("state.json.alive");
    let cfg = Config {
        port: 0,
        tailnet: false,
        state: Some(path.clone()),
        ..Config::default()
    };
    let running = amx::start(cfg.clone()).await.unwrap();
    let base = format!("http://{}", running.local_addr);
    ok_tool(&base, "author", "request_review", json!({"pr_url": PR, "reviewers": 1})).await;
    assert!(alive.exists(), "the relay records that it is alive");
    running.stop().await;

    let mut v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let saved = v["saved_at"].as_u64().unwrap();
    v["saved_at"] = json!(saved - 600_000);
    std::fs::write(&path, v.to_string()).unwrap();
    std::fs::write(&alive, saved.to_string()).unwrap();
    let again = amx::start(cfg).await.unwrap();
    let r: Value = reqwest::get(format!("http://{}/api/reviews/r1", again.local_addr)).await.unwrap().json().await.unwrap();
    let left = r["deadline_left_ms"].as_u64().unwrap();
    assert!(left <= 120_000, "a quiet spell before the crash is not downtime: {left}");
    again.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_without_a_method_are_invalid() {
    let relay = start(None, vec![]).await;
    for body in [json!({"jsonrpc":"2.0","id":1}), json!({"jsonrpc":"2.0","id":2,"method":5})] {
        let (status, v) = rpc(&relay.local, "someone", body.clone()).await;
        assert_eq!((status, v["error"]["code"].as_i64()), (200, Some(-32600)), "{body} -> {v}");
    }
    let (status, _) = rpc(&relay.local, "someone", json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
    assert_eq!(status, 202);
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn other_sites_cannot_register_or_keep_agents_alive() {
    let relay = start(None, vec![]).await;
    let base = relay.local.clone();
    let http = reqwest::Client::new();
    let url = format!("{base}/api/watch?agent=csrf-ghost&role=reviewer&timeout=1");
    let tries = [
        http.get(&url),
        http.get(&url).header("Origin", "http://evil.example.com").header("X-Amx-Client", "x"),
        http.get(&url).header("Sec-Fetch-Site", "cross-site").header("X-Amx-Client", "x"),
        http.post(format!("{base}/api/heartbeat?agent=csrf-ghost&role=reviewer")).header("Sec-Fetch-Site", "cross-site"),
        http.post(format!("{base}/mcp?agent=csrf-ghost"))
            .header("Sec-Fetch-Site", "same-site")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})),
    ];
    for req in tries {
        assert_eq!(req.send().await.unwrap().status(), 403);
    }
    assert!(relay.running.hub.read(|s, _| s.agents.get("csrf-ghost").is_none()));
    let link = http.get(format!("{base}/")).header("Sec-Fetch-Site", "cross-site").send().await.unwrap();
    assert_eq!(link.status(), 200, "links from elsewhere still open the page");

    let watcher = amx::client::Client::new(&base, Some("cli-watch".into()), Some("reviewer".into()));
    let w = watcher.watch(0, 1).await.unwrap();
    assert_eq!(w["agent"], "cli-watch");
    let same = http.get(format!("{base}/api/watch?agent=page&timeout=1")).header("Sec-Fetch-Site", "same-origin").header("X-Amx-Client", "x");
    assert_eq!(same.send().await.unwrap().status(), 200);
    relay.running.stop().await;
}

async fn connect_from(ip: [u8; 4], to: std::net::SocketAddr) -> tokio::net::TcpStream {
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.bind((ip, 0).into()).unwrap();
    sock.connect(to).await.unwrap()
}

/// Whether the relay closes the connection within `within` without being sent anything more.
async fn closed(conn: &mut tokio::net::TcpStream, within: Duration) -> bool {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 256];
    matches!(tokio::time::timeout(within, conn.read(&mut buf)).await, Ok(Ok(0) | Err(_)))
}

async fn get_on(conn: &mut tokio::net::TcpStream, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    conn.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), conn.read(&mut buf)).await.unwrap().unwrap();
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_and_excess_connections_are_cut_off_before_identification() {
    let mut cfg = Config {
        port: 0,
        tailnet: false,
        ..Config::default()
    };
    cfg.conn_limits = amx::server::ConnLimits {
        max: 8,
        per_ip: 3,
        header_timeout: Duration::from_secs(2),
    };
    let running = amx::start(cfg).await.unwrap();
    let addr = running.local_addr;

    let mut idle = Vec::new();
    for _ in 0..4 {
        idle.push(connect_from([127, 0, 0, 1], addr).await);
    }
    for _ in 0..3 {
        idle.push(connect_from([127, 0, 0, 2], addr).await);
    }
    let mut over_ip = connect_from([127, 0, 0, 2], addr).await;
    assert!(closed(&mut over_ip, Duration::from_secs(1)).await, "4th connection from one address");
    idle.push(connect_from([127, 0, 0, 3], addr).await);
    let mut over_all = connect_from([127, 0, 0, 4], addr).await;
    assert!(closed(&mut over_all, Duration::from_secs(1)).await, "9th connection overall");
    for (i, c) in idle.iter_mut().enumerate() {
        assert!(!closed(c, Duration::from_millis(20)).await, "connection {i} admitted (127.0.0.1 has no per-address cap)");
    }

    assert!(get_on(&mut idle[0], "/api/reviews").await.starts_with("HTTP/1.1 200"), "admitted connections work");
    let mut slow = idle.pop().unwrap();
    use tokio::io::AsyncWriteExt;
    slow.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n").await.unwrap();
    for (i, c) in idle.iter_mut().enumerate() {
        assert!(closed(c, Duration::from_secs(5)).await, "idle connection {i} (keep-alive after a request included)");
    }
    assert!(closed(&mut slow, Duration::from_secs(5)).await, "headers never finished");

    let mut fresh = None;
    for _ in 0..50 {
        let mut c = connect_from([127, 0, 0, 1], addr).await;
        if !closed(&mut c, Duration::from_millis(50)).await {
            fresh = Some(c);
            break;
        }
    }
    assert!(get_on(fresh.as_mut().expect("slots freed"), "/api/reviews").await.starts_with("HTTP/1.1 200"));
    running.stop().await;
}
