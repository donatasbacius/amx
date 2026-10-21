//! The relay over HTTP: identity, MCP, long-polls and the web page.

use amx::Running;
use amx::config::Config;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
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
    start_on("127.0.0.1", ts, allow).await
}

async fn start_on(tailnet_ip: &str, ts: Option<PathBuf>, allow: Vec<String>) -> Relay {
    let tport = free_port();
    let cfg = Config {
        port: free_port(),
        tailnet: false,
        tailnet_listen: ts.is_some().then(|| SocketAddr::new(tailnet_ip.parse().unwrap(), tport)),
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

async fn rpc_as(base: &str, query: &str, body: Value) -> Value {
    reqwest::Client::new().post(format!("{base}/mcp?{query}")).json(&body).send().await.unwrap().json().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_role_does_not_override_hello_on_requests() {
    let relay = start(None, vec![]).await;
    for (initial, chosen) in [("author", "reviewer"), ("reviewer", "author")] {
        let agent = format!("initial-{initial}");
        let q = format!("agent={agent}&role={initial}");
        let init = rpc_as(&relay.local, &q, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})).await;
        assert!(init["result"]["instructions"].as_str().unwrap().contains(&format!("Your role is {initial}")));
        let hello = rpc_as(&relay.local, &q, json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"hello","arguments":{"role":chosen,"model":"codex"}}})).await;
        assert_eq!(hello["result"]["isError"], false, "{hello}");
        let online = rpc_as(&relay.local, &q, json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"online","arguments":{}}})).await;
        let text = online["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains(&format!("{agent} (you) ({chosen},")), "request reset role: {text}");
    }
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shared_name_stays_a_reviewer_while_a_reviewer_session_holds_it() {
    let relay = start(None, vec![]).await;
    let base = relay.local.clone();
    let (rev, _) = initialize(&base, "agent=team&role=reviewer").await;
    session_rpc(&base, &rev, call(0, "hello", json!({"role":"reviewer","model":"claude"})), false).await;
    let _stream = open_stream(&base, &rev).await;
    session_rpc(&base, &rev, call(1, "wait", json!({"timeout_seconds": 1})), false).await;
    let (author, note) = initialize(&base, "agent=team&role=author").await;
    assert!(note.contains("Another session is also connected as `team`"), "{note}");
    session_rpc(&base, &author, call(1, "hello", json!({"role":"author"})), false).await;
    let online = text_of(&session_rpc(&base, &author, call(2, "online", json!({})), false).await);
    assert!(online.contains("team (you) (reviewer,"), "{online}");
    let started = std::time::Instant::now();
    session_rpc(&base, &rev, call(2, "wait", json!({"timeout_seconds": 1})), false).await;
    assert!(started.elapsed() >= Duration::from_secs(1), "the reviewer session doesn't spin");
    ok_tool(&base, "alice", "request_review", json!({"pr_url": PR, "reviewers": ["team"]})).await;
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
    let hello = ok_tool(&relay.tailnet, "friend-claude", "hello", json!({"role":"author"})).await;
    assert!(hello.contains("You are `friend-claude`."), "{hello}");
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
    let main = &page[page.find("<main").unwrap()..];
    assert!(!main.contains("friend@example.com") && main.contains("friend-claude"), "the login is only in the header: {page}");
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
    let hello = ok_tool(&reviewer, "friend-rev", "hello", json!({"role":"reviewer","model":"claude"})).await;
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
    assert!(result.contains("NEEDS YOUR CALL") && result.contains("only their author vouches for F1: check them against the code yourself"), "a sole reviewer's finding is unverified: {result}");

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
    for (rev, model) in [("rev-a", "claude"), ("rev-b", "codex")] {
        ok_tool(&base, rev, "hello", json!({"role":"reviewer","model":model})).await;
    }
    ok_tool(&base, "author", "request_review", json!({"pr_url": PR, "reviewers": ["rev-a", "rev-b"]})).await;
    for rev in ["rev-a", "rev-b"] {
        ok_tool(&base, rev, "accept_review", json!({"review_id":"r1"})).await;
    }
    let index = get("/").await.unwrap().text().await.unwrap();
    assert!(index.contains("Agents: <span>2 reviewing ·</span> <span>1 author</span></a>") && index.contains("on <a href=\"/r/r1\">rust#1</a>"), "{index}");
    let finding = json!([{"severity":"major","text":"SECRET until revealed\nsecond line"}]);
    ok_tool(&base, "rev-a", "submit_findings", json!({"review_id":"r1","head_sha":"abc1234","verdict":"changes_requested","summary":"PRIVATE","findings":finding})).await;

    let r = get("/api/reviews/r1").await.unwrap();
    assert_eq!(r.headers()["cache-control"], "no-store");
    let body = r.text().await.unwrap();
    assert!(!body.contains("SECRET") && !body.contains("PRIVATE") && !body.contains("changes_requested"), "{body}");
    let page = get("/r/r1").await.unwrap().text().await.unwrap();
    assert!(page.contains(">submitted<") && !page.contains("requests changes"), "{page}");
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
    ok_tool(&base, "author", "request_review", json!({"pr_url": PR, "reviewers": 1})).await;
    let page = get("/r/r1").await.unwrap().text().await.unwrap();
    assert!(page.contains("<p class=\"newer\">Newer review: <a href=\"/r/r3\">r3</a> (waiting for reviewers)&nbsp;· last result: changes requested</p>"), "{page}");
    assert!(page.contains("<li aria-current=\"page\"><span class=\"h-id\"><b>r1</b>") && page.contains("<a href=\"/r/r3\">r3</a></span>"), "{page}");
    let index = get("/").await.unwrap().text().await.unwrap();
    assert!(page.contains("<span class=\"h-gist\">0 of 1 accepted</span>"), "the history says what the index row says: {page}");
    assert!(index.contains("<span>0 of 1 accepted ·</span> <span>last: changes requested ·</span> <span>2 reviews ·</span> <span>author</span>") && !index.contains("href=\"/r/r1\""), "one row per PR: {index}");
    assert!(index.contains("<title>amx · 2 running</title>") && index.contains("#2<span class=\"sr\">, Waiting for reviewers</span></a>"), "{index}");
    assert!(index.contains("<span>2 reviewers idle ·</span> <span>1 author</span></a>"), "zero counts left out: {index}");

    let missing = get("/api/reviews/r9").await.unwrap();
    assert_eq!(missing.status(), 404);
    let gone = get("/r/r9").await.unwrap();
    assert_eq!(gone.status(), 404);
    let gone = gone.text().await.unwrap();
    assert!(gone.contains("<a href=\"/\">All reviews</a>"), "{gone}");
    assert!(!gone.contains("app.js") && !gone.contains("data-etag") && !gone.contains("id=\"live\""), "a page that can't change doesn't poll: {gone}");
    let polled = http.get(format!("{base}/r/r9")).header("X-Amx-Partial", "1").send().await.unwrap();
    assert_eq!(polled.status(), 404);
    let polled = polled.text().await.unwrap();
    assert!(polled.contains("No review r9") && !polled.contains("<html"), "an open page whose review is gone shows so: {polled}");
    let page = get("/r/r1").await.unwrap().text().await.unwrap();
    assert!(page.contains("<title>Changes requested · rust-lang/rust#1</title>"), "{page}");
    assert!(page.contains("id=\"F1\"") && page.contains("Changes requested") && !page.contains("<pre>"), "{page}");
    assert!(page.contains("<h2 class=\"sr\">Transcript</h2><details class=\"more\" id=\"transcript\">"), "{page}");
    for page in [&page, &index] {
        well_formed_for_screen_readers(page);
    }
    let partial = http.get(format!("{base}/r/r1")).header("X-Amx-Partial", "1").send().await.unwrap();
    assert_eq!(partial.headers()["cache-control"], "no-store");
    let tag = partial.headers()["etag"].clone();
    assert!(!partial.text().await.unwrap().contains("<html"));
    let unchanged = http.get(format!("{base}/r/r1")).header("X-Amx-Partial", "1").header("If-None-Match", tag).send().await.unwrap();
    assert_eq!(unchanged.status(), 304, "the page's polls only fetch a changed page");

    for path in ["/", "/r/r2"] {
        let full = get(path).await.unwrap().text().await.unwrap();
        let first = http.get(format!("{base}{path}")).header("X-Amx-Partial", "1").send().await.unwrap();
        let tag = first.headers()["etag"].to_str().unwrap().to_string();
        assert!(full.contains(&format!("data-etag='{tag}'")), "the page seeds its first poll's tag: {full}");
        assert!(full.contains("<span class=\"live\" role=\"status\" aria-atomic=\"false\"><span id=\"live\"></span><span id=\"said\" class=\"sr\"></span></span>"), "silent while live: {full}");
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let again = http.get(format!("{base}{path}")).header("X-Amx-Partial", "1").header("If-None-Match", &tag).send().await.unwrap();
        assert_eq!(again.status(), 304, "{path}: countdowns tick in the browser, not in the page");
    }
    relay.running.stop().await;
}

/// No heading inside a disclosure's summary, where screen readers drop it, and no id used twice,
/// as the page script keeps each finding's open state and focus by id.
fn well_formed_for_screen_readers(page: &str) {
    for s in page.split("<summary").skip(1) {
        let s = &s[..s.find("</summary>").expect("closed summary")];
        assert!(!["<h1", "<h2", "<h3", "<h4"].iter().any(|h| s.contains(h)), "heading in a summary: {s}");
    }
    let ids: Vec<&str> = page.split(" id=\"").skip(1).map(|s| &s[..s.find('"').unwrap()]).collect();
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len(), "{ids:?}");
    assert!(page.split("<details class=\"f").skip(1).all(|d| d.split('>').next().unwrap().contains(" id=\"")), "{page}");
}

#[tokio::test(flavor = "multi_thread")]
async fn setup() {
    let relay = start(None, vec![]).await;
    let base = relay.local.clone();
    let setup = reqwest::get(format!("{base}/setup")).await.unwrap().text().await.unwrap();
    for line in [
        format!("claude mcp add --scope user --transport http amx {base}/mcp"),
        format!("replace any [mcp_servers.amx] table with\n\n[mcp_servers.amx]\nurl = \"{base}/mcp\"\ndefault_tools_approval_mode = \"approve\"\ntool_timeout_sec = 660\n"),
        "local-claude and local-codex".into(),
    ] {
        assert!(setup.contains(&line), "{line} in {setup}");
    }
    assert!(!setup.contains("hook"), "{setup}");
    assert_eq!(reqwest::get(format!("{base}/api/hook/stop?agent=rev")).await.unwrap().status(), 404);
    let page = reqwest::get(format!("{base}/")).await.unwrap().text().await.unwrap();
    assert!(page.contains("Connect an agent</summary>") && page.contains("claude mcp add"), "{page}");
    assert!(page.contains("local-claude</code> and <code>local-codex</code>. <a href=\"/setup\">Full setup</a>"), "the rest is on /setup: {page}");
    assert!(page.contains("No agents online") && page.contains("<details id=\"connect\" class=\"side-d connect\" open>"), "open while nobody is online: {page}");
    assert!(page.contains("<span class=\"sr\">Viewing on </span>This machine</span></header>"), "--no-tailnet: {page}");
    assert!(!page.contains(">local<") && !page.contains("Viewing as"), "its placeholder login is never shown: {page}");
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
        json!({"jsonrpc":"2.0","id":3,"method":"initialize","params":{}}),
    ] {
        rpc(&base, "probe", body).await;
    }
    let online = ok_tool(&base, "probe", "online", json!({})).await;
    assert!(online.contains("No agents online.") && online.contains("You (`probe`) are not online"), "{online}");
    ok_tool(&base, "probe", "review_status", json!({})).await;
    assert!(relay.running.hub.read(|s, _| s.agents.get("probe").is_none()));
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
    for (rev, model) in [("rev-a", "claude"), ("rev-b", "codex")] {
        ok_tool(&base, rev, "hello", json!({"role":"reviewer","model":model})).await;
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
async fn owed_tasks_are_given_in_full_once_per_result() {
    let relay = start(None, vec![]).await;
    let base = relay.local.clone();
    for (rev, model) in [("rev-a", "claude"), ("rev-b", "codex")] {
        ok_tool(&base, rev, "hello", json!({"role":"reviewer","model":model})).await;
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
    let said = ok_tool(&base, "rev-b", "say", json!({"review_id":"r1","text":"Looking at F1."})).await;
    assert!(said.ends_with("NEXT: r1: vote on F1, F2 as described above.") && !said.contains("cache never"), "a task owed before the call is pointed to: {said}");
    ok_tool(&base, "author", "wait", json!({"timeout_seconds": 1})).await;
    let status = ok_tool(&base, "rev-b", "review_status", json!({})).await;
    assert!(status.ends_with("NEXT: r1: vote on F1, F2 as described above."), "{status}");
    for _ in 0..2 {
        let owed = ok_tool(&base, "rev-b", "wait", json!({})).await;
        assert!(owed.contains("You must vote on: F1, F2.") && owed.matches("cache never invalidated").count() == 1, "an owed task is given in full whenever asked: {owed}");
    }

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
    for (rev, model) in [("rev-a", "claude"), ("rev-b", "codex")] {
        ok_tool(&base, rev, "hello", json!({"role":"reviewer","model":model})).await;
    }
    ok_tool(&base, "author", "request_review", json!({"pr_url": PR, "reviewers": ["rev-a", "rev-b"]})).await;
    for (rev, text) in [("rev-a", "race on shutdown"), ("rev-b", "leaks a file handle")] {
        ok_tool(&base, rev, "accept_review", json!({"review_id":"r1"})).await;
        let findings = json!([{"severity":"major","text":text}]);
        ok_tool(&base, rev, "submit_findings", json!({"review_id":"r1","head_sha":"abc1234","verdict":"changes_requested","findings":findings})).await;
    }
    ok_tool(&base, "rev-a", "vote", json!({"review_id":"r1","votes":[{"finding":"F2","vote":"disagree","reason":"closed in Drop"}]})).await;
    let reply = ok_tool(&base, "rev-b", "vote", json!({"review_id":"r1","votes":[{"finding":"F1","vote":"disagree","reason":"no"}]})).await;
    assert!(reply.contains("your findings F2 drew objections; call `wait` to read them"), "{reply}");
    let objections = ok_tool(&base, "rev-b", "wait", json!({"timeout_seconds": 20})).await;
    assert!(objections.contains("closed in Drop"), "the reviewer whose vote opened the window sees the objections: {objections}");
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
    ok_tool(&base, "rev", "hello", json!({"role":"reviewer","model":"claude"})).await;
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

/// Starts an MCP session as Claude Code would; returns its id and the instructions.
async fn initialize(base: &str, query: &str) -> (String, String) {
    let r = reqwest::Client::new()
        .post(format!("{base}/mcp?{query}"))
        .header("User-Agent", "claude-code/2.1")
        .json(&json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}))
        .send()
        .await
        .unwrap();
    let sid = r.headers()["mcp-session-id"].to_str().unwrap().to_string();
    let v: Value = r.json().await.unwrap();
    (sid, v["result"]["instructions"].as_str().unwrap().to_string())
}

/// One request in a session; the reply's last message, whether it came as JSON or an event stream.
async fn session_rpc(base: &str, sid: &str, body: Value, stream: bool) -> Value {
    let accept = if stream { "application/json, text/event-stream" } else { "application/json" };
    let r = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .header("User-Agent", "claude-code/2.1")
        .header("Mcp-Session-Id", sid)
        .header("Accept", accept)
        .json(&body)
        .send()
        .await
        .unwrap();
    let text = r.text().await.unwrap();
    let data = text.lines().filter_map(|l| l.strip_prefix("data: ")).next_back().map(String::from).unwrap_or(text);
    serde_json::from_str(&data).unwrap_or(Value::Null)
}

fn call(id: u64, name: &str, args: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}})
}

fn text_of(v: &Value) -> String {
    v["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn events_stay_until_the_session_acknowledges_them() {
    let relay = start(None, vec![]).await;
    let base = relay.local.clone();
    let (sid, _) = initialize(&base, "agent=rev").await;
    ok_tool(&base, "rev", "hello", json!({"role":"reviewer","model":"claude"})).await;
    ok_tool(&base, "author", "request_review", json!({"pr_url": PR, "reviewers": ["rev"]})).await;
    ok_tool(&base, "rev", "accept_review", json!({"review_id":"r1"})).await;
    ok_tool(&base, "author", "say", json!({"review_id":"r1","text":"Mind the cache."})).await;
    let cancel = |id: u64| json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}});
    let owed = || relay.running.hub.read(|s, _| s.has_undelivered("rev"));
    let first = session_rpc(&base, &sid, call(1, "wait", json!({})), true).await;
    assert!(text_of(&first).contains("Mind the cache."), "{first}");
    assert!(owed(), "handed out is not yet received");
    session_rpc(&base, &sid, cancel(1), false).await;
    let again = session_rpc(&base, &sid, call(2, "wait", json!({})), true).await;
    assert!(text_of(&again).contains("Mind the cache."), "cancelled, so delivered again: {again}");
    let next = session_rpc(&base, &sid, call(3, "wait", json!({})), false).await;
    assert!(!text_of(&next).contains("Mind the cache.") && text_of(&next).contains("submit_findings"), "{next}");
    assert!(!owed(), "acknowledged by the next request");
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wait_cancelled_while_it_runs_is_not_taken_as_received() {
    let relay = start(None, vec![]).await;
    let base = relay.local.clone();
    let (sid, _) = initialize(&base, "agent=rev&role=reviewer").await;
    session_rpc(&base, &sid, call(0, "hello", json!({"role":"reviewer","model":"claude"})), false).await;
    let running = tokio::spawn({
        let (base, sid) = (base.clone(), sid.clone());
        async move { session_rpc(&base, &sid, call(1, "wait", json!({"timeout_seconds": 5})), false).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    session_rpc(&base, &sid, json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}), false).await;
    ok_tool(&base, "author", "request_review", json!({"pr_url": PR, "reviewers": ["rev"]})).await;
    assert!(text_of(&running.await.unwrap()).contains("Review offer"), "answered after the client gave up on it");
    let next = session_rpc(&base, &sid, call(2, "wait", json!({})), false).await;
    assert!(text_of(&next).contains("Review offer"), "so it comes again: {next}");
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_stream_returns_its_events() {
    let relay = start(None, vec![]).await;
    let base = relay.local.clone();
    let (sid, _) = initialize(&base, "agent=rev&role=reviewer").await;
    session_rpc(&base, &sid, call(0, "hello", json!({"role":"reviewer","model":"claude"})), false).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .header("Mcp-Session-Id", &sid)
        .header("Accept", "application/json, text/event-stream")
        .json(&call(1, "wait", json!({})))
        .send()
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(resp);
    ok_tool(&base, "author", "request_review", json!({"pr_url": PR, "reviewers": ["rev"]})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let next = session_rpc(&base, &sid, call(2, "wait", json!({})), true).await;
    assert!(text_of(&next).contains("Review offer"), "{next}");
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_streamed_wait_sends_progress_until_an_event() {
    let relay = start(None, vec![]).await;
    let base = relay.local.clone();
    let (sid, _) = initialize(&base, "agent=rev&role=reviewer").await;
    session_rpc(&base, &sid, call(0, "hello", json!({"role":"reviewer","model":"claude"})), false).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .header("Mcp-Session-Id", &sid)
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"wait","arguments":{"timeout_seconds":1},"_meta":{"progressToken":7}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    tokio::time::sleep(Duration::from_secs(21)).await;
    ok_tool(&base, "author", "request_review", json!({"pr_url": PR, "reviewers": 1})).await;
    let body = tokio::time::timeout(Duration::from_secs(5), resp.text()).await.unwrap().unwrap();
    assert!(body.contains("notifications/progress") && body.contains("Review offer"), "outlasts timeout_seconds: {body}");
    relay.running.stop().await;
}

async fn open_stream(base: &str, sid: &str) -> reqwest::Response {
    let get = reqwest::Client::new().get(format!("{base}/mcp")).header("Mcp-Session-Id", sid).header("Accept", "text/event-stream");
    get.send().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_get_default_names_of_their_role() {
    let relay = start(None, vec![]).await;
    let base = relay.local.clone();
    let named = |sid: String, name: &'static str, args: Value| {
        let base = base.clone();
        async move { text_of(&session_rpc(&base, &sid, call(1, name, args), false).await) }
    };
    let (_unused, _) = initialize(&base, "").await;
    let (first, _) = initialize(&base, "").await;
    let hello = named(first.clone(), "hello", json!({"role":"reviewer","model":"claude"})).await;
    assert!(hello.starts_with("You are `local-claude`."), "a session never used holds no name: {hello}");
    reqwest::Client::new().delete(format!("{base}/mcp")).header("Mcp-Session-Id", &first).send().await.unwrap();
    let (author, _) = initialize(&base, "").await;
    let req = named(author, "request_review", json!({"pr_url": PR})).await;
    assert!(req.starts_with("You are `local-claude-2`.\n\nReview r1"), "a free reviewer's name isn't an author's: {req}");
    let (reviewer, _) = initialize(&base, "").await;
    let hello = named(reviewer, "hello", json!({"role":"reviewer","model":"claude"})).await;
    assert!(hello.starts_with("You are `local-claude`."), "{hello}");

    let (streaming, _) = initialize(&base, "").await;
    let stream = open_stream(&base, &streaming).await;
    let hello = named(streaming, "hello", json!({"role":"author"})).await;
    assert!(hello.starts_with("You are `local-claude-3`."), "{hello}");
    let (waiting, _) = initialize(&base, "").await;
    let wait = named(waiting, "wait", json!({"timeout_seconds": 1})).await;
    assert!(wait.starts_with("You are `local-claude-4`."), "an open event stream holds the name: {wait}");
    drop(stream);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (_, _) = initialize(&base, "").await;
    let (next, _) = initialize(&base, "").await;
    let hello = named(next, "hello", json!({"role":"author"})).await;
    assert!(hello.starts_with("You are `local-claude-3`."), "and frees it as soon as it closes: {hello}");

    let (pair, intro) = initialize(&base, "agent=pair").await;
    assert!(!intro.contains("Another session"), "{intro}");
    session_rpc(&base, &pair, call(1, "online", json!({})), false).await;
    let (_, intro) = initialize(&base, "agent=pair").await;
    assert!(intro.contains("Another session is also connected as `pair`"), "a chosen name is shared, with a warning: {intro}");
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn after_a_restart_sessions_get_back_the_names_of_their_role() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        port: 0,
        tailnet: false,
        state: Some(tmp.path().join("state.json")),
        ..Config::default()
    };
    let running = amx::start(cfg.clone()).await.unwrap();
    let base = format!("http://{}", running.local_addr);
    let (rev, _) = initialize(&base, "").await;
    session_rpc(&base, &rev, call(1, "hello", json!({"role":"reviewer","model":"claude"})), false).await;
    let (author, _) = initialize(&base, "role=reviewer").await;
    let req = session_rpc(&base, &author, call(1, "request_review", json!({"pr_url": PR, "reviewers": ["local-claude"]})), false).await;
    assert!(text_of(&req).starts_with("You are `local-claude-2`."), "{req}");
    session_rpc(&base, &rev, call(2, "accept_review", json!({"review_id":"r1"})), false).await;
    session_rpc(&base, &rev, call(3, "submit_findings", json!({"review_id":"r1","head_sha":"abc1234","verdict":"approve","findings":[]})), false).await;
    running.stop().await;

    let running = amx::start(cfg).await.unwrap();
    let base = format!("http://{}", running.local_addr);
    let (author, _) = initialize(&base, "").await;
    let (rev, _) = initialize(&base, "").await;
    let wait = session_rpc(&base, &author, call(1, "wait", json!({})), false).await;
    assert!(wait["error"]["message"].as_str().unwrap().contains("call `hello` with your role"), "{wait}");
    let hello = session_rpc(&base, &author, call(2, "hello", json!({"role":"author"})), false).await;
    assert!(text_of(&hello).starts_with("You are `local-claude-2`."), "{hello}");
    let hello = session_rpc(&base, &rev, call(1, "hello", json!({"role":"reviewer","model":"claude"})), false).await;
    assert!(text_of(&hello).starts_with("You are `local-claude`."), "{hello}");
    let result = session_rpc(&base, &author, call(3, "wait", json!({})), false).await;
    assert!(text_of(&result).contains("Review result"), "a reviewer session's request still gets its result: {result}");
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn logins_with_the_same_user_name_keep_apart() {
    let tmp = tempfile::tempdir().unwrap();
    let ts = write_script(
        tmp.path(),
        "ts-ip",
        r#"case "$3" in ::1) l=alice@example.com; id=2;; *) l=alice@other.org; id=7;; esac
echo "{\"Node\":{\"Name\":\"box.example.ts.net.\"},\"UserProfile\":{\"ID\":$id,\"LoginName\":\"$l\"}}""#,
    );
    let relay = start_on("::", Some(ts), vec![]).await;
    let port = relay.running.hub.cfg.tailnet_listen.unwrap().port();
    let post = |ip: &'static str, query: &'static str, sid: Option<String>, body: Value| {
        let ip: IpAddr = ip.parse().unwrap();
        let url = format!("http://{}/mcp{query}", SocketAddr::new(ip, port));
        async move {
            let http = reqwest::Client::builder().local_address(ip).build().unwrap();
            let mut req = http.post(url).header("User-Agent", "claude-code/2.1").json(&body);
            if let Some(sid) = &sid {
                req = req.header("Mcp-Session-Id", sid);
            }
            let r = req.send().await.unwrap();
            let sid = r.headers().get("mcp-session-id").map(|v| v.to_str().unwrap().to_string()).or(sid);
            (sid.unwrap_or_default(), r.json::<Value>().await.unwrap())
        }
    };
    let init = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{}});
    let hello = call(1, "hello", json!({"role":"author"}));
    let (mine, _) = post("::1", "", None, init.clone()).await;
    assert!(text_of(&post("::1", "", Some(mine.clone()), hello.clone()).await.1).starts_with("You are `alice-claude`."));
    for _ in 0..2 {
        let (theirs, _) = post("127.0.0.1", "", None, init.clone()).await;
        let (_, v) = post("127.0.0.1", "", Some(theirs.clone()), hello.clone()).await;
        assert!(text_of(&v).starts_with("You are `alice-claude-2`."), "{v}");
        reqwest::Client::new().delete(format!("{}/mcp", relay.tailnet)).header("Mcp-Session-Id", &theirs).send().await.unwrap();
    }
    let (_, v) = post("127.0.0.1", "?agent=alice-claude", None, init.clone()).await;
    assert!(v["error"]["message"].as_str().unwrap().contains("in use by another tailnet user"), "{v}");
    let (_, v) = post("::1", "", Some(mine), hello.clone()).await;
    assert!(text_of(&v).starts_with("You are `alice-claude`."), "never locked out of its own name: {v}");

    for _ in 0..70 {
        post("::1", "", None, init.clone()).await;
    }
    let (theirs, _) = post("127.0.0.1", "", None, init.clone()).await;
    assert!(!theirs.is_empty(), "one login's sessions don't crowd out another's");
    let sessions = relay.running.hub.sessions.lock().unwrap().keys().filter(|k| k.contains("alice@example.com")).count();
    assert_eq!(sessions, 64);
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_author_with_nothing_in_progress_is_answered_at_once() {
    let relay = start(None, vec![]).await;
    let base = relay.local.clone();
    let started = std::time::Instant::now();
    let idle = ok_tool(&base, "author", "wait", json!({"timeout_seconds": 20})).await;
    assert!(idle.starts_with("Nothing in progress.") && idle.contains("call `request_review`"), "{idle}");
    ok_tool(&base, "rev", "hello", json!({"role":"reviewer","model":"claude"})).await;
    ok_tool(&base, "author", "request_review", json!({"pr_url": PR, "reviewers": ["rev"]})).await;
    ok_tool(&base, "rev", "accept_review", json!({"review_id":"r1"})).await;
    ok_tool(&base, "rev", "submit_findings", json!({"review_id":"r1","head_sha":"abc1234","verdict":"approve","findings":[]})).await;
    let result = ok_tool(&base, "author", "wait", json!({"timeout_seconds": 20})).await;
    assert!(result.contains("Review result"), "{result}");
    let idle = ok_tool(&base, "author", "wait", json!({"timeout_seconds": 20})).await;
    assert!(idle.contains("to check fixes for r1, call `request_rereview`"), "{idle}");
    assert!(started.elapsed() < Duration::from_secs(5), "never long-polled");
    let started = std::time::Instant::now();
    ok_tool(&base, "rev", "wait", json!({"timeout_seconds": 2})).await;
    assert!(started.elapsed() >= Duration::from_secs(2), "reviewers wait for offers");
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_acknowledgement_from_before_a_restart_acknowledges_nothing() {
    let relay = start(None, vec![]).await;
    let (sid, _) = initialize(&relay.local, "agent=rev&role=reviewer").await;
    session_rpc(&relay.local, &sid, call(0, "hello", json!({"role":"reviewer","model":"claude"})), false).await;
    ok_tool(&relay.local, "author", "request_review", json!({"pr_url": PR, "reviewers": ["rev"]})).await;
    let offer = session_rpc(&relay.local, &sid, call(1, "wait", json!({})), false).await;
    assert!(text_of(&offer).contains("Review offer"), "{offer}");
    relay.running.stop().await;

    let relay = start(None, vec![]).await;
    ok_tool(&relay.local, "rev", "hello", json!({"role":"reviewer","model":"claude"})).await;
    ok_tool(&relay.local, "author", "request_review", json!({"pr_url": format!("{PR}0"), "reviewers": ["rev"]})).await;
    let after = reqwest::Client::new().post(format!("{}/mcp?agent=rev", relay.local)).header("Mcp-Session-Id", &sid).json(&call(2, "wait", json!({})));
    let after: Value = after.send().await.unwrap().json().await.unwrap();
    assert!(text_of(&after).contains(&format!("{PR}0")), "the new relay's offer is not taken as received: {after}");
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_agents_change_nothing() {
    let relay = start(None, vec![]).await;
    let base = relay.local.clone();
    ok_tool(&base, "rev", "hello", json!({"role":"reviewer","model":"claude"})).await;
    ok_tool(&base, "author", "hello", json!({"role":"author"})).await;
    let version = || *relay.running.hub.subscribe().borrow();
    let before = version();
    for _ in 0..2 {
        ok_tool(&base, "rev", "wait", json!({"timeout_seconds": 1})).await;
        ok_tool(&base, "author", "review_status", json!({})).await;
    }
    assert_eq!(version(), before, "nothing to save or refresh");
    relay.running.stop().await;
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
    for body in [json!({"jsonrpc":"2.0","id":1}), json!({"jsonrpc":"2.0","id":2,"method":5}), json!({"jsonrpc":"2.0","id":null,"method":"ping"})] {
        let (status, v) = rpc(&relay.local, "someone", body.clone()).await;
        assert_eq!((status, v["error"]["code"].as_i64()), (200, Some(-32600)), "{body} -> {v}");
    }
    let (status, _) = rpc(&relay.local, "someone", json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
    assert_eq!(status, 202);
    let huge = json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"say","arguments":{"review_id":"r1","message":"x".repeat(300_000)}}});
    assert_eq!(rpc(&relay.local, "someone", huge).await.0, 413);
    relay.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn other_sites_cannot_register_or_keep_agents_alive() {
    let relay = start(None, vec![]).await;
    let base = relay.local.clone();
    let http = reqwest::Client::new();
    let url = format!("{base}/mcp?agent=csrf-ghost&role=reviewer");
    let tries = [
        http.get(&url).header("Origin", "http://evil.example.com").header("Mcp-Session-Id", "x").header("Accept", "text/event-stream"),
        http.get(&url).header("Sec-Fetch-Site", "cross-site").header("Mcp-Session-Id", "x").header("Accept", "text/event-stream"),
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
    relay.running.stop().await;
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
async fn idle_connections_are_cut_off_and_loopback_has_its_own_cap() {
    let mut cfg = Config {
        port: 0,
        tailnet: false,
        ..Config::default()
    };
    cfg.conn_limits = amx::server::ConnLimits {
        max: 3,
        per_ip: 3,
        own: 5,
        header_timeout: Duration::from_secs(2),
    };
    let running = amx::start(cfg).await.unwrap();
    let addr = running.local_addr;

    let mut idle = Vec::new();
    for _ in 0..5 {
        idle.push(tokio::net::TcpStream::connect(addr).await.unwrap());
    }
    for (i, c) in idle.iter_mut().enumerate() {
        assert!(!closed(c, Duration::from_millis(20)).await, "connection {i} admitted (loopback is over neither max nor per_ip)");
    }
    let mut over = tokio::net::TcpStream::connect(addr).await.unwrap();
    assert!(closed(&mut over, Duration::from_secs(1)).await, "loopback still has a cap");
    drop(over);

    assert!(get_on(&mut idle[0], "/api/reviews").await.starts_with("HTTP/1.1 200"), "admitted connections work");
    let mut slow = idle.pop().unwrap();
    use tokio::io::AsyncWriteExt;
    slow.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n").await.unwrap();
    for (i, c) in idle.iter_mut().enumerate() {
        assert!(closed(c, Duration::from_secs(5)).await, "idle connection {i} (keep-alive after a request included)");
    }
    assert!(closed(&mut slow, Duration::from_secs(5)).await, "headers never finished");
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn loopback_waits_for_the_owner_then_takes_over_local_names() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        port: 0,
        tailnet: false,
        state: Some(tmp.path().join("state.json")),
        ..Config::default()
    };
    let running = amx::start(cfg.clone()).await.unwrap();
    ok_tool(&format!("http://{}", running.local_addr), "own-au", "request_review", json!({"pr_url": PR, "reviewers": 1})).await;
    running.stop().await;

    let down = write_script(tmp.path(), "ts-down", "echo 'Tailscale is stopped.' >&2; exit 1");
    let starting = amx::start(Config { tailnet: true, tailscale_bin: Some(down), ..cfg.clone() }).await.unwrap();
    let (status, v) = rpc(&format!("http://{}", starting.local_addr), "own-au", json!({"jsonrpc":"2.0","id":1,"method":"ping"})).await;
    assert_eq!(status, 503);
    assert!(v.as_str().unwrap().contains("waiting for Tailscale"), "{v}");
    let early = format!("http://{}", starting.local_addr);
    let mcp = reqwest::Client::new().post(format!("{early}/mcp?agent=own-au")).json(&json!({})).send().await.unwrap();
    assert_eq!((mcp.status().as_u16(), mcp.headers().get("retry-after").map(|v| v.to_str().unwrap())), (503, Some("10")));
    for path in ["/", "/r/r1", "/api/reviews", "/static/app.js"] {
        assert_eq!(reqwest::get(format!("{early}{path}")).await.unwrap().status(), 200, "{path} is read-only");
    }
    assert_eq!(reqwest::get(format!("{early}/setup")).await.unwrap().status(), 503);
    starting.stop().await;

    let up = write_script(
        tmp.path(),
        "ts-up",
        r#"case "$1" in
status) echo '{"Self":{"HostName":"relaybox","TailscaleIPs":["127.0.0.1"]}}' ;;
*) echo '{"Node":{"Name":"relaybox."},"UserProfile":{"ID":7,"LoginName":"owner@example.com"}}' ;;
esac"#,
    );
    let running = amx::start(Config { tailnet: true, tailscale_bin: Some(up), ..cfg }).await.unwrap();
    let base = format!("http://{}", running.local_addr);
    for _ in 0..100 {
        if rpc(&base, "own-au", json!({"jsonrpc":"2.0","id":1,"method":"ping"})).await.0 == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let online = ok_tool(&base, "own-au", "online", json!({})).await;
    assert!(online.contains("own-au (you) (author, owner@example.com on"), "{online}");
    assert!(running.hub.read(|s, _| s.reviews[0].author_login == "owner@example.com"), "local reviews are the owner's");
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_config_is_refused_at_start() {
    for (toml, key) in [
        ("[consensus]\nconfirm_fraction = -0.5", "confirm_fraction"),
        ("[consensus]\nconfirm_fraction = 1.0", "confirm_fraction"),
        ("[consensus]\nreviewers = 0", "reviewers"),
        ("[consensus]\nvote_rounds = 9", "vote_rounds"),
        ("public_url = \"relay.example\"", "public_url"),
    ] {
        let cfg = Config { port: 0, tailnet: false, ..toml::from_str(toml).unwrap() };
        let err = amx::start(cfg).await.err().expect(toml).to_string();
        assert!(err.contains(key), "{err}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reviewers_say_which_program_they_run_in() {
    let relay = start(None, vec![]).await;
    let base = relay.local.clone();
    let (err, text) = tool(&base, "rev", "hello", json!({"role":"reviewer"})).await;
    assert!(err && text.contains("model \"claude\" (Claude Code) or \"codex\" (Codex)"), "{text}");
    let online = ok_tool(&base, "rev", "online", json!({})).await;
    assert!(!online.contains("rev (you) (reviewer"), "a refused hello makes no reviewer: {online}");
    ok_tool(&base, "rev", "hello", json!({"role":"reviewer","model":"codex"})).await;
    ok_tool(&base, "author", "hello", json!({"role":"author"})).await;
    let online = ok_tool(&base, "author", "online", json!({})).await;
    assert!(online.contains("- rev (reviewer, codex, "), "{online}");
    relay.running.stop().await;
}
