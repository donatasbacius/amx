mod common;

use amx::config::Config;
use amx::model::{Finding, Kind, Status, Verdict};
use amx::node::{PostInput, Reason};
use amx::store::MarkTable;
use common::*;
use serde_json::{Value, json};

fn input(kind: Kind, body: &str) -> PostInput {
    PostInput {
        kind: Some(kind),
        pr_url: PR.into(),
        body: body.into(),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn inbox_semantics_across_nodes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = TestNode::start(tmp.path(), "a").await;
    let b = TestNode::start(tmp.path(), "b").await;
    a.peer_with(&b);
    let (alice, bob, carol) = (a.agent("alice"), b.agent("bob"), b.agent("carol"));

    let req = a
        .post_input("alice", PostInput { to: Some("bob".into()), ..input(Kind::Request, "please") })
        .await;
    eventually("b has request", 5, || b.count() == 1).await;
    let bi = b.node().inbox(&bob, false).unwrap();
    assert_eq!(bi.len(), 1);
    assert_eq!(bi[0].reason, Reason::AddressedToYou);
    assert!(b.node().inbox(&carol, false).unwrap().is_empty(), "addressed to someone else");
    assert!(a.node().inbox(&alice, false).unwrap().is_empty(), "own messages never appear");

    let other = PostInput {
        pr_url: "https://github.com/rust-lang/rust/pull/2".into(),
        ..input(Kind::Request, "anyone?")
    };
    a.post_input("alice", other).await;
    eventually("b has 2", 5, || b.count() == 2).await;
    let ci = b.node().inbox(&carol, false).unwrap();
    assert_eq!(ci.len(), 1);
    assert_eq!(ci[0].reason, Reason::OpenRequest);

    b.node().mark(MarkTable::Read, &bob, std::slice::from_ref(&req.id)).unwrap();
    let review = b
        .post_input(
            "bob",
            PostInput {
                verdict: Some(Verdict::RequestChanges),
                findings: vec![Finding { severity: Some("major".into()), path: Some("src/lib.rs".into()), line: Some(3), url: None, text: "bug".into() }],
                ..input(Kind::Review, "needs work")
            },
        )
        .await;
    eventually("a has review", 5, || a.count() == 3).await;
    let ai = a.node().inbox(&alice, false).unwrap();
    assert_eq!(ai.len(), 1);
    assert_eq!(ai[0].message.id, review.id);
    assert_eq!(ai[0].reason, Reason::YourThread);

    a.post("alice", Kind::Reply, "fixed").await;
    eventually("b has reply", 5, || b.count() == 4).await;
    let bi = b.node().inbox(&bob, false).unwrap();
    assert_eq!(bi.len(), 2, "reply on bob's thread plus the open request on pull/2: {bi:?}");
    // Read marks are per agent and per node.
    assert_eq!(a.node().inbox(&alice, false).unwrap().len(), 1);
    a.node().mark(MarkTable::Read, &alice, std::slice::from_ref(&review.id)).unwrap();
    assert!(a.node().inbox(&alice, false).unwrap().is_empty());

    a.post_input("alice", PostInput { pr_url: "https://github.com/rust-lang/rust/pull/2".into(), ..input(Kind::Close, "") }).await;
    eventually("b has close", 5, || b.count() == 5).await;
    assert!(
        b.node().inbox(&carol, false).unwrap().is_empty(),
        "open requests on closed threads drop out"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_returns_when_mail_arrives() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = TestNode::start(tmp.path(), "a").await;
    let b = TestNode::start(tmp.path(), "b").await;
    a.peer_with(&b);
    let node = b.node().clone();
    let bob = b.agent("bob");
    let waiter = tokio::spawn(async move {
        let t = std::time::Instant::now();
        let items = node.wait_inbox(&bob, std::time::Duration::from_secs(10), false).await.unwrap();
        (items.len(), t.elapsed())
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    a.post_input("alice", PostInput { to: Some("bob".into()), ..input(Kind::Request, "x") }).await;
    let (n, took) = waiter.await.unwrap();
    assert_eq!(n, 1);
    assert!(took < std::time::Duration::from_secs(3));
    let empty = b.node().wait_inbox(&b.agent("zed"), std::time::Duration::from_millis(200), false).await.unwrap();
    assert!(empty.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_review_detection_with_stubbed_gh() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("a");
    std::fs::create_dir_all(&dir).unwrap();
    let gh = gh_stub(&dir);
    std::fs::write(dir.join("head"), "1111111").unwrap();
    let running = amx::start(Config { gh_bin: gh, ..config(&dir, free_port()) }).await.unwrap();
    let node = running.node.clone();

    let req = node.post(node.agent("alice"), input(Kind::Request, "")).await.unwrap();
    assert_eq!(req.fields.title.as_deref(), Some("Stub PR"));
    assert_eq!(req.fields.head_sha.as_deref(), Some("1111111"));
    let review = node
        .post(node.agent("bob"), PostInput { verdict: Some(Verdict::Approve), ..input(Kind::Review, "lgtm") })
        .await
        .unwrap();
    assert_eq!(review.fields.head_sha.as_deref(), Some("1111111"));
    let v = node.thread(PR).unwrap().unwrap();
    assert!(v.stale_reviews.is_empty());
    assert_eq!(v.summary.status, Status::Approved);

    std::fs::write(dir.join("head"), "2222222").unwrap();
    node.refresh_meta(PR).await;
    let v = node.thread(PR).unwrap().unwrap();
    assert_eq!(v.stale_reviews, vec![review.id.clone()]);
    assert!(v.summary.latest_review_stale);

    // gh failing must not block or fail posting, and keeps the last known metadata.
    std::fs::remove_file(dir.join("head")).unwrap();
    node.refresh_meta(PR).await;
    let t = std::time::Instant::now();
    let r2 = node
        .post(node.agent("carol"), PostInput { verdict: Some(Verdict::Comment), head_sha: Some("2222222".into()), ..input(Kind::Review, "ok now") })
        .await
        .unwrap();
    assert!(t.elapsed() < std::time::Duration::from_secs(4));
    let v = node.thread(PR).unwrap().unwrap();
    assert!(!v.stale_reviews.contains(&r2.id));
    assert!(!v.summary.latest_review_stale);
    assert_eq!(v.meta.unwrap().head_sha.as_deref(), Some("2222222"));
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn posting_without_gh_works_and_validates() {
    let tmp = tempfile::tempdir().unwrap();
    let a = TestNode::start(tmp.path(), "a").await;
    let n = a.node();
    let me = a.agent("alice");
    let m = n.post(me.clone(), PostInput { pr_url: "https://github.com/Rust-Lang/rust/pull/1/files#diff-1".into(), ..input(Kind::Request, "") }).await.unwrap();
    assert_eq!(m.thread, PR);
    assert!(m.fields.head_sha.is_none());
    for bad in [
        PostInput { pr_url: "https://github.com/a/b/issues/1".into(), ..input(Kind::Reply, "x") },
        input(Kind::Reply, "   "),
        input(Kind::Review, "no verdict"),
        PostInput { verdict: Some(Verdict::Approve), ..input(Kind::Reply, "x") },
        PostInput { in_reply_to: Some("01JUNKJUNKJUNKJUNKJUNKJUNK".into()), ..input(Kind::Reply, "x") },
        PostInput {
            verdict: Some(Verdict::Comment),
            findings: vec![Finding { url: Some("javascript:alert(1)".into()), text: "x".into(), ..Default::default() }],
            ..input(Kind::Review, "x")
        },
    ] {
        assert!(n.post(me.clone(), bad.clone()).await.is_err(), "{bad:?}");
    }
    assert_eq!(a.count(), 1);
}

async fn rpc(url: &str, body: Value) -> Value {
    reqwest::Client::new().post(url).json(&body).send().await.unwrap().json().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_over_http() {
    let tmp = tempfile::tempdir().unwrap();
    let a = TestNode::start(tmp.path(), "a").await;
    let url = format!("{}/mcp?agent=alice", a.url());
    let init = rpc(&url, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}})).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "amx");
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let tools = rpc(&url, json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).await;
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for t in ["request_review", "claim_review", "post_review", "reply", "inbox", "mark_read", "wait", "threads", "thread", "close_thread"] {
        assert!(names.contains(&t), "{t}");
    }
    let call = |id: i64, name: &str, args: Value| json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}});
    let r = rpc(&url, call(3, "request_review", json!({"pr_url": PR, "to": "bob", "note": "pls"}))).await;
    assert_eq!(r["result"]["isError"], false, "{r}");
    let bob_url = format!("{}/mcp?agent=bob", a.url());
    let r = rpc(&bob_url, call(4, "inbox", json!({}))).await;
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("alice@a requested a review from bob") && text.contains("pls"), "{text}");
    let r = rpc(&bob_url, call(5, "post_review", json!({"pr_url": PR, "verdict": "request_changes", "summary": "s", "findings": [{"severity":"major","path":"x.rs","line":2,"text":"t"}]}))).await;
    assert_eq!(r["result"]["isError"], false, "{r}");
    let r = rpc(&bob_url, call(6, "post_review", json!({"pr_url": PR, "verdict": "maybe", "summary": "s"}))).await;
    assert_eq!(r["result"]["isError"], true);
    let r = rpc(&bob_url, call(7, "thread", json!({"pr_url": PR}))).await;
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("changes requested"));
    let r = rpc(&bob_url, call(8, "inbox", json!({}))).await;
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().starts_with("No unread"), "thread marks read");
    let r = rpc(&url, call(9, "wait", json!({"timeout_seconds": 1}))).await;
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("request_changes") || r["result"]["content"][0]["text"].as_str().unwrap().contains("request changes"));
    let r = rpc(&url, json!({"jsonrpc":"2.0","id":10,"method":"nope"})).await;
    assert_eq!(r["error"]["code"], -32601);
}

#[tokio::test(flavor = "multi_thread")]
async fn tailnet_listener_requires_whois() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("a");
    std::fs::create_dir_all(&dir).unwrap();
    let ts_fail = write_script(&dir, "ts-fail", "echo 'peer not found' >&2; exit 1");
    let ts_ok = write_script(
        &dir,
        "ts-ok",
        r#"echo '{"Node":{"Name":"friendbox.example.ts.net."},"UserProfile":{"LoginName":"friend@example.com","DisplayName":"Friend"}}'"#,
    );
    let http = reqwest::Client::new();
    for (bin, allowed) in [(ts_fail, false), (ts_ok, true)] {
        let tport = free_port();
        let cfg = Config {
            tailnet_listen: Some(format!("127.0.0.1:{tport}").parse().unwrap()),
            tailscale_bin: Some(bin),
            ..config(&dir, free_port())
        };
        let running = amx::start(cfg).await.unwrap();
        let base = format!("http://127.0.0.1:{tport}");
        eventually("tailnet listener up", 5, || running.node.listening.lock().unwrap().len() == 2).await;
        let page = http.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(page.status().is_success(), allowed, "GET / allowed={allowed}");
        let sync = http
            .post(format!("{base}/api/sync"))
            .json(&json!({"node_id":"x","machine":"y","vv":{}}))
            .send()
            .await
            .unwrap();
        assert_eq!(sync.status().is_success(), allowed);
        let mcp = http.post(format!("{base}/mcp")).json(&json!({"jsonrpc":"2.0","id":1,"method":"ping"})).send().await.unwrap();
        assert_eq!(mcp.status(), 403, "MCP is never served to tailnet callers");
        let local = http.post(format!("{base}/api/local/post")).json(&json!({})).send().await.unwrap();
        assert_eq!(local.status(), 403);
        if allowed {
            let r = http
                .post(format!("{base}/pr/rust-lang/rust/1"))
                .header("Origin", &base)
                .form(&[("kind", "reply"), ("body", "hello from a human"), ("to", "")])
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success(), "{}", r.status());
            let msgs = running.node.store.all().unwrap();
            let m = msgs.last().unwrap();
            assert_eq!(m.author.login.as_deref(), Some("friend@example.com"));
            assert_eq!(m.author.machine, "friendbox");
            let csrf = http
                .post(format!("{base}/pr/rust-lang/rust/1"))
                .header("Origin", "https://evil.example")
                .form(&[("kind", "reply"), ("body", "csrf")])
                .send()
                .await
                .unwrap();
            assert_eq!(csrf.status(), 403);
            let rebind = http.get(format!("{base}/")).header("Host", "evil.example").send().await.unwrap();
            assert_eq!(rebind.status(), 403);
        }
        running.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn web_pages_escape_remote_content() {
    let tmp = tempfile::tempdir().unwrap();
    let a = TestNode::start(tmp.path(), "a").await;
    a.post_input(
        "evil",
        PostInput {
            verdict: Some(Verdict::RequestChanges),
            findings: vec![Finding {
                severity: Some("<b>major</b>".into()),
                path: Some("src/\"x\".rs".into()),
                line: Some(4),
                url: Some("https://github.com/rust-lang/rust/blob/abc/src/x.rs#L4".into()),
                text: "<img src=x onerror=alert(1)> bad".into(),
            }],
            ..input(Kind::Review, "**bold** [ok](https://example.com)\n\n<script>alert('x')</script>")
        },
    )
    .await;
    let http = reqwest::Client::new();
    let index = http.get(format!("{}/", a.url())).send().await.unwrap().text().await.unwrap();
    assert!(index.contains("rust-lang/rust#1"));
    assert!(index.contains("changes requested"));
    let page = http.get(format!("{}/pr/rust-lang/rust/1", a.url())).send().await.unwrap().text().await.unwrap();
    assert!(!page.contains("<script>alert"));
    assert!(!page.contains("<img"));
    assert!(page.contains("&lt;script&gt;"));
    assert!(!page.contains("<b>major</b>"));
    assert!(page.contains("<strong>bold</strong>"));
    assert!(page.contains("href=\"https://github.com/rust-lang/rust/blob/abc/src/x.rs#L4\""));
    assert!(page.contains("evil@a"));
    let css = http.get(format!("{}/static/style.css", a.url())).send().await.unwrap();
    assert_eq!(css.headers()["content-type"], "text/css; charset=utf-8");
}
