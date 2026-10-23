//! What people do from the web page: cancel a review, rule on findings. Requests from 127.0.0.2
//! reach the tailnet listener as another login (stranger@example.com) than the author's
//! (friend@example.com), from 127.0.0.3 as local@evil.example and from 127.0.0.4 as
//! boss@example.com; loopback callers are the owner.

use amx::Running;
use amx::config::Config;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const SHA2: &str = "89abcdef0123456789abcdef0123456789abcdef";

fn pr(n: u32) -> String {
    format!("https://github.com/example-org/demo/pull/{n}")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn whois(dir: &Path) -> PathBuf {
    let p = dir.join("ts");
    let id = |login: &str| format!(r#"echo '{{"Node":{{"Name":"box.example.ts.net."}},"UserProfile":{{"LoginName":"{login}","DisplayName":"x"}}}}'"#);
    let body = format!(
        "#!/bin/sh\ncase \"$3\" in\n127.0.0.2) {} ;;\n127.0.0.3) {} ;;\n127.0.0.4) {} ;;\n127.0.0.5) {} ;;\n127.0.0.6) {} ;;\n*) {} ;;\nesac\n",
        id("stranger@example.com"),
        id("local@evil.example"),
        id("boss@example.com"),
        id("tagged-devices"),
        id("local"),
        id("friend@example.com")
    );
    std::fs::write(&p, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

struct Hub {
    running: Running,
    cfg: Config,
    local: String,
    tailnet: String,
}

async fn start(dir: &Path, vote_rounds: u8) -> Hub {
    let tport = free_port();
    let mut cfg = Config {
        port: free_port(),
        tailnet: false,
        tailnet_listen: Some(SocketAddr::from(([127, 0, 0, 1], tport))),
        tailscale_bin: Some(whois(dir)),
        state: Some(dir.join("state.json")),
        ..Config::default()
    };
    cfg.consensus.vote_rounds = vote_rounds;
    run(cfg).await
}

async fn run(cfg: Config) -> Hub {
    let running = amx::start(cfg.clone()).await.unwrap();
    let local = format!("http://{}", running.local_addr);
    let tailnet = format!("http://{}", cfg.tailnet_listen.unwrap());
    for _ in 0..100 {
        if running.hub.listening.lock().unwrap().len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Hub { running, cfg, local, tailnet }
}

impl Hub {
    async fn restart(self) -> Hub {
        let cfg = self.cfg.clone();
        self.running.stop().await;
        run(cfg).await
    }

    /// An agent of the author's login, over the tailnet.
    async fn friend(&self, agent: &str, tool: &str, args: Value) -> String {
        call(&self.tailnet, agent, tool, args).await
    }

    /// An agent of the owner, over loopback.
    async fn owner(&self, agent: &str, tool: &str, args: Value) -> String {
        call(&self.local, agent, tool, args).await
    }

    /// A reviewer: rv2 belongs to another person (stranger@example.com) than rv1 (the owner).
    async fn reviewer(&self, agent: &str, tool: &str, args: Value) -> String {
        if agent == "rv2" { call_as(browser("stranger"), &self.tailnet, agent, tool, args).await } else { self.owner(agent, tool, args).await }
    }

    async fn api(&self, id: &str) -> Value {
        reqwest::get(format!("{}/api/reviews/{id}", self.local)).await.unwrap().json().await.unwrap()
    }
}

async fn call(base: &str, agent: &str, name: &str, args: Value) -> String {
    call_as(Client::new(), base, agent, name, args).await
}

async fn call_as(client: Client, base: &str, agent: &str, name: &str, args: Value) -> String {
    let v: Value = client
        .post(format!("{base}/mcp"))
        .query(&[("agent", agent)])
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = v["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
    assert_eq!(v["result"]["isError"], false, "{name}: {text}");
    text
}

/// A browser of `who` ("friend", "stranger", "evil" or "owner") posting a form from the relay's own page.
fn browser(who: &str) -> Client {
    let b = Client::builder().redirect(reqwest::redirect::Policy::none());
    let b = match who {
        "stranger" => b.local_address(std::net::IpAddr::from([127, 0, 0, 2])),
        "evil" => b.local_address(std::net::IpAddr::from([127, 0, 0, 3])),
        "boss" => b.local_address(std::net::IpAddr::from([127, 0, 0, 4])),
        "tagged" => b.local_address(std::net::IpAddr::from([127, 0, 0, 5])),
        "local" => b.local_address(std::net::IpAddr::from([127, 0, 0, 6])),
        _ => b,
    };
    b.build().unwrap()
}

impl Hub {
    fn base(&self, who: &str) -> &str {
        if who == "owner" { &self.local } else { &self.tailnet }
    }

    async fn post(&self, who: &str, path: &str, form: &[(&str, &str)]) -> (StatusCode, String) {
        let base = self.base(who);
        let body: Vec<String> = form.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let r = browser(who)
            .post(format!("{base}{path}"))
            .header("origin", base)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body.join("&"))
            .send()
            .await
            .unwrap();
        (r.status(), r.text().await.unwrap())
    }

    async fn page(&self, who: &str, id: &str) -> String {
        browser(who).get(format!("{}/r/{id}", self.base(who))).send().await.unwrap().text().await.unwrap()
    }
}

/// Waits until `agent` has received an event containing `want`; returns everything it got.
async fn wait_for(base: &str, agent: &str, want: &str) -> String {
    let mut all = String::new();
    for _ in 0..10 {
        all += &call(base, agent, "wait", json!({"timeout_seconds": 2})).await;
        if all.contains(want) {
            return all;
        }
    }
    panic!("{agent} never got {want:?}: {all}");
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_take_the_hubs_rules_and_old_reviews_keep_theirs() {
    let tmp = tempfile::tempdir().unwrap();
    let hub = start(tmp.path(), 2).await;
    let tools: Value = Client::new()
        .post(format!("{}/mcp", hub.local))
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let props = tools["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "request_review").unwrap()["inputSchema"]["properties"].clone();
    let names: Vec<&String> = props.as_object().unwrap().keys().collect();
    assert_eq!(names, ["count", "head_sha", "note", "pr_url", "reviewers", "title"]);
    let schema = |name: &str| tools["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == name).unwrap()["inputSchema"].clone();
    assert_eq!(schema("request_review")["required"], json!(["pr_url", "title"]));
    assert!(schema("request_rereview")["properties"]["title"].is_object() && schema("request_rereview")["required"] == json!(["review_id", "new_head_sha"]));

    let out = hub.friend("fa", "request_review", json!({"title": "Add a cache", "pr_url": pr(1), "confirm_fraction": 0.99, "vote_rounds": 1, "review_seconds": 9})).await;
    assert!(out.contains(" Ignored confirm_fraction, vote_rounds, review_seconds: the hub sets consensus rules."), "{out}");
    let plain = hub.friend("fa", "request_review", json!({"title": "Add a cache", "pr_url": pr(2)})).await;
    let toml = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/contrib/config.toml")).unwrap();
    assert!(!toml.contains("override per request"), "{toml}");
    assert!(!plain.contains("Ignored"), "{plain}");
    let again = hub.friend("fa", "request_review", json!({"title": "Add a cache", "pr_url": pr(2), "count": 3})).await;
    assert!(again.contains("You already requested this review as r2; it is not queued twice, and the count and reviewers you gave now are ignored: it keeps 2 reviewer(s)."), "{again}");
    let same = hub.friend("fa", "request_review", json!({"title": "Add a cache", "pr_url": pr(2), "count": 2})).await;
    assert!(same.contains("it is not queued twice.") && !same.contains("ignored"), "{same}");
    let index = browser("owner").get(format!("{}/?f=queued&page=9&n=1&show=all", hub.base("owner"))).send().await.unwrap().text().await.unwrap();
    assert!(index.contains("aria-current=\"page\">Queued") && index.matches("<li class=\"rv").count() == 2 && !index.contains("pager"), "a page past the last, and old parameters, still list them: {index}");
    let params = &hub.api("r1").await["params"];
    assert_eq!((params["consensus"]["confirm_fraction"].as_f64(), params["consensus"]["vote_rounds"].as_u64(), params["review_secs"].as_u64()), (Some(0.5), Some(2), Some(1200)));

    // A review saved by an older relay with its own rules keeps them: they are what it was decided under.
    let hub = {
        hub.running.stop().await;
        let path = tmp.path().join("state.json");
        let mut snap: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        snap["reviews"][0]["params"]["consensus"]["vote_rounds"] = json!(1);
        snap["reviews"][0]["params"]["vote_secs"] = json!(14400);
        std::fs::write(&path, snap.to_string()).unwrap();
        run(hub.cfg.clone()).await
    };
    let params = &hub.api("r1").await["params"];
    assert_eq!((params["consensus"]["vote_rounds"].as_u64(), params["vote_secs"].as_u64()), (Some(1), Some(14400)));
    let page = hub.page("owner", "r1").await;
    assert!(page.contains("demo#1") && !page.contains("voting round"), "{page}");
    hub.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(not(target_os = "linux"), ignore = "binds 127.0.0.2-6, which only Linux routes by default")]
async fn the_author_or_owner_cancels_a_running_review_which_frees_the_pr_and_the_slot() {
    let tmp = tempfile::tempdir().unwrap();
    let hub = start(tmp.path(), 2).await;
    hub.owner("rv", "hello", json!({"role": "reviewer", "model": "claude"})).await;
    hub.reviewer("rv2", "hello", json!({"role": "reviewer", "model": "codex"})).await;
    hub.friend("fa", "request_review", json!({"pr_url": pr(1), "count": 2, "title": "Drop the <v1> export"})).await;
    for n in 2..=5 {
        hub.friend("fa", "request_review", json!({"title": "Add a cache", "pr_url": pr(n), "count": 2})).await;
    }
    hub.owner("rv", "accept_review", json!({"review_id": "r1"})).await;
    hub.reviewer("rv2", "accept_review", json!({"review_id": "r1"})).await;
    assert_eq!(hub.api("r1").await["phase"], "reviewing");
    let full = Client::new()
        .post(format!("{}/mcp?agent=fa", hub.tailnet))
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"request_review","arguments":{"pr_url": pr(6), "title": "Add a cache"}}}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(full.contains("already has 5 unfinished reviews"), "{full}");

    // Without JS: a disclosure whose summary is the quiet button and turns into "Keep running";
    // opened, what happens and the red button follow it, in the order they are read and tabbed.
    let friend_page = hub.page("friend", "r1").await;
    let control = "<details class=\"act\" id=\"cancel\"><summary class=\"btn\"><span class=\"c0\">Cancel review</span><span class=\"c1\">Keep running</span></summary>\
                   <form method=\"post\" action=\"/r/r1/cancel\"><p id=\"cancel-what\">rv and rv2 stop, and the PR is free for a new review. You can't undo this.</p>\
                   <button class=\"btn btn-fix\" aria-describedby=\"cancel-what\">Cancel review</button></form></details></section>";
    assert!(friend_page.contains(control), "{friend_page}");
    let waiting = hub.page("friend", "r2").await;
    assert!(waiting.contains("<p id=\"cancel-what\">The PR is free for a new review.") && waiting.contains(">Keep running</span>"), "nobody has accepted r2 yet: {waiting}");
    let stranger_page = hub.page("stranger", "r1").await;
    assert!(stranger_page.contains("demo#1") && !stranger_page.contains("Cancel review"), "{stranger_page}");

    let (code, body) = hub.post("stranger", "/r/r1/cancel", &[]).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("Only the review&#39;s author or the hub owner") || body.contains("Only the review's author or the hub owner"), "{body}");
    let http = browser("friend");
    let url = format!("{}/r/r1/cancel", hub.tailnet);
    let no_origin = http.post(&url).send().await.unwrap();
    assert_eq!(no_origin.status(), StatusCode::FORBIDDEN);
    let elsewhere = http.post(&url).header("origin", "http://box2.example.ts.net:3000").send().await.unwrap();
    assert_eq!(elsewhere.status(), StatusCode::FORBIDDEN);
    let other_port = http.post(&url).header("origin", "http://127.0.0.1:1").send().await.unwrap();
    assert_eq!(other_port.status(), StatusCode::FORBIDDEN);
    let cross = http.post(&url).header("origin", &hub.tailnet).header("sec-fetch-site", "cross-site").send().await.unwrap();
    assert_eq!(cross.status(), StatusCode::FORBIDDEN);
    for h in ["forwarded", "x-forwarded-for", "x-real-ip", "true-client-ip", "cf-connecting-ip", "via", "tailscale-user-name"] {
        let proxied = browser("owner").post(format!("{}/r/r1/cancel", hub.local)).header("origin", &hub.local).header(h, "x").send().await.unwrap();
        assert_eq!(proxied.status(), StatusCode::FORBIDDEN, "{h}");
        let page = browser("owner").get(format!("{}/r/r1", hub.local)).header(h, "x").send().await.unwrap().text().await.unwrap();
        assert!(page.contains("demo#1") && !page.contains("Cancel review"), "no controls behind a proxy ({h}): {page}");
        let index = browser("owner").get(format!("{}/", hub.local)).header(h, "x").send().await.unwrap().text().await.unwrap();
        assert!(index.contains("demo#1") && !index.contains("Connect an agent"), "a proxied caller is nobody ({h}): {index}");
        let setup = browser("owner").get(format!("{}/setup", hub.local)).header(h, "x").send().await.unwrap();
        assert_eq!(setup.status(), StatusCode::FORBIDDEN, "{h}");
        let mcp = browser("owner").post(format!("{}/mcp?agent=rv", hub.local)).header(h, "x").header("accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "wait", "arguments": {"timeout_seconds": 1}}})).send().await.unwrap();
        assert_eq!(mcp.status(), StatusCode::FORBIDDEN, "a proxied agent can't use the owner's agents ({h})");
    }
    assert!(browser("owner").get(format!("{}/", hub.local)).send().await.unwrap().text().await.unwrap().contains("Connect an agent"));
    assert!(hub.page("owner", "r1").await.contains("action=\"/r/r1/cancel\""));
    let (code, body) = hub.post("friend", "/r/r1/f/F1", &[("ruling", "must_fix")]).await;
    assert!(code == StatusCode::CONFLICT && body.contains("r1 has not finished"), "{code}: {body}");
    assert_eq!(hub.api("r1").await["phase"], "reviewing", "nothing refused changed anything");

    // A tailnet login named local is not the hub owner.
    let evil = call_as(browser("evil"), &hub.tailnet, "ea", "request_review", json!({"title": "Add a cache", "pr_url": pr(7), "count": 2})).await;
    let ev_id = evil.split_whitespace().nth(1).unwrap().to_string();
    let (code, _) = hub.post("evil", "/r/r3/cancel", &[]).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _) = hub.post("evil", &format!("/r/{ev_id}/cancel"), &[]).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    assert_eq!(hub.api(&ev_id).await["cancelled_by"], "local");
    assert!(hub.page("evil", &ev_id).await.contains("<p class=\"vt\">Cancelled</p><p class=\"vd\">local cancelled it."));

    // A cancel that can't be saved is not done, and nobody is told of it.
    use std::os::unix::fs::PermissionsExt;
    let mode = |m| std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(m)).unwrap();
    mode(0o555);
    let (code, body) = hub.post("friend", "/r/r3/cancel", &[]).await;
    mode(0o755);
    assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(body.contains("couldn&#39;t save") && !body.contains(tmp.path().to_str().unwrap()) && !body.contains("state.json"), "{body}");
    assert!(body.contains("<h1>Not done</h1><p>The relay couldn&#39;t save it. Try again.</p>"), "the heading says it once: {body}");
    assert_eq!(hub.api("r3").await["phase"], "offering", "rolled back");

    // The relay's own Tailscale login is its owner over the tailnet too.
    let (code, _) = hub.post("boss", "/r/r4/cancel", &[]).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "not the owner yet");
    hub.running.hub.owner.lock().unwrap().as_mut().unwrap().login = "boss@example.com".into();
    assert!(hub.page("boss", "r4").await.contains("action=\"/r/r4/cancel\""));
    let (code, _) = hub.post("boss", "/r/r4/cancel", &[]).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    assert_eq!(hub.api("r4").await["cancelled_by"], "boss", "an owner Tailscale names is named like anyone");
    assert!(hub.page("boss", "r4").await.contains("boss cancelled it."));
    hub.running.hub.owner.lock().unwrap().as_mut().unwrap().login = "tagged-devices".into();
    let (code, _) = hub.post("tagged", "/r/r1/cancel", &[]).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "a tagged node is not the owner of a relay on a tagged node");
    hub.running.hub.owner.lock().unwrap().as_mut().unwrap().login = "local".into();
    let (code, _) = hub.post("local", "/r/r1/cancel", &[]).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "a tailnet login named exactly local is not the owner");

    let (code, _) = hub.post("friend", "/r/r1/cancel", &[]).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    let (again, body) = hub.post("friend", "/r/r1/cancel", &[]).await;
    assert_eq!(again, StatusCode::SEE_OTHER, "a double submit lands on the page it cancelled: {body}");
    assert_eq!(hub.post("owner", "/r/r1/cancel", &[]).await.0, StatusCode::SEE_OTHER);
    assert_eq!(hub.api("r1").await["cancelled_by"], "friend", "the first cancel stands");
    let (code, _) = hub.post("owner", "/r/r2/cancel", &[]).await;
    assert_eq!(code, StatusCode::SEE_OTHER, "the owner may cancel anyone's review");

    let rv = wait_for(&hub.local, "rv", "cancelled").await;
    assert!(rv.contains("[r1] Review cancelled by friend (a human). Call `wait`."), "{rv}");
    let fa = wait_for(&hub.tailnet, "fa", "[r2]").await;
    assert!(fa.contains(&format!("[r1] friend (a human) cancelled the review of {}.\nNEXT: don't request it again unless your human asks.", pr(1))), "{fa}");
    assert!(fa.contains("[r2] the hub owner (a human) cancelled the review"), "{fa}");
    assert!(!fa.contains("try `request_review` again") && !fa.contains("[r3]") && !fa.contains("nothing pending"), "{fa}");
    let online = hub.owner("rv", "online", json!({})).await;
    assert!(online.contains("rv (you) (reviewer") && online.contains("idle"), "the reviewer is free: {online}");

    let hub = hub.restart().await;
    let page = hub.page("friend", "r1").await;
    assert!(page.contains("<title>Cancelled · Drop the &lt;v1&gt; export</title>") && page.contains("<p class=\"vt\">Cancelled</p>"), "{page}");
    assert!(page.contains("friend cancelled it. The author can request it again.") && !page.contains("id=\"cancel\""), "{page}");
    let index = browser("friend").get(format!("{}/", hub.tailnet)).send().await.unwrap().text().await.unwrap();
    assert!(index.contains("Drop the &lt;v1&gt; export<span class=\"sr\">, Cancelled</span>") && index.contains("<span class=\"st s-failed\">Cancelled</span>") && index.contains(">Add a cache<span") && index.contains("<span>example-org/demo#1 ·</span> <span>by friend ·</span>"), "{index}");
    assert_eq!(hub.api("r1").await["title"], "Drop the <v1> export");
    assert_eq!(hub.api("r1").await["timeline"].as_array().unwrap().last().unwrap()["name"], "Cancelled");
    assert!(page.contains("<b>rv</b> <span class=\"pg\">claude</span> <span class=\"badge s-closed\">stopped</span>") && !page.contains(">reviewing<"), "{page}");
    assert!(hub.page("owner", "r2").await.contains("The hub owner cancelled it."));
    let r1 = hub.api("r1").await;
    assert_eq!((r1["phase"].as_str(), r1["cancelled_by"].as_str()), (Some("failed"), Some("friend")));
    hub.friend("fa", "request_review", json!({"title": "Add a cache", "pr_url": pr(1), "count": 2})).await;
    let r = Client::new().get(format!("{}/r/r1", hub.local)).send().await.unwrap();
    assert_eq!(r.headers()["x-frame-options"], "DENY");
    assert_eq!(r.headers()["content-security-policy"], "frame-ancestors 'none'");
    hub.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(not(target_os = "linux"), ignore = "binds 127.0.0.2-6, which only Linux routes by default")]
async fn rulings_set_the_verdict_reach_the_author_survive_a_restart_and_settle_the_rereview() {
    let tmp = tempfile::tempdir().unwrap();
    let hub = start(tmp.path(), 1).await;
    for (r, model) in [("rv1", "claude"), ("rv2", "codex")] {
        hub.reviewer(r, "hello", json!({"role": "reviewer", "model": model})).await;
    }
    hub.friend("fa", "request_review", json!({"title": "Add a cache", "pr_url": pr(1), "count": 2})).await;
    for r in ["rv1", "rv2"] {
        hub.reviewer(r, "accept_review", json!({"review_id": "r1"})).await;
    }
    let findings = json!([
        {"severity": "major", "title": "Short title", "text": "The retry loop has no backoff."},
        {"severity": "minor", "title": "Short title", "text": "Retries reuse one idempotency key."},
    ]);
    hub.reviewer("rv1", "submit_findings", json!({"review_id": "r1", "head_sha": SHA, "verdict": "changes_requested", "findings": findings})).await;
    hub.reviewer("rv2", "submit_findings", json!({"review_id": "r1", "head_sha": SHA, "verdict": "approve", "checked": "src/lib.rs", "findings": []})).await;
    let votes = json!([{"finding": "F1", "vote": "disagree", "reason": "It backs off."}, {"finding": "F2", "vote": "disagree", "reason": "Keys differ."}]);
    hub.reviewer("rv2", "vote", json!({"review_id": "r1", "votes": votes})).await;
    let r1 = hub.api("r1").await;
    assert_eq!((r1["phase"].as_str(), r1["verdict"].as_str()), (Some("done"), Some("approve")), "{r1}");
    wait_for(&hub.tailnet, "fa", "Review result").await;

    let page = hub.page("friend", "r1").await;
    assert!(page.contains("value=\"must_fix\">Must fix<span class=\"sr\"> F1</span>"), "{page}");
    assert!(page.contains("<p class=\"vt\">Decide</p><p class=\"vd\"><span><span class=\"needs\">2 to decide</span>") && page.contains("<title>Decide · "), "{page}");
    let index = |who: &'static str, h: Option<&'static str>| {
        let url = format!("{}/", hub.base(who));
        async move {
            let req = browser(who).get(url);
            let req = if let Some(h) = h { req.header(h, "x") } else { req };
            req.send().await.unwrap().text().await.unwrap()
        }
    };
    let yours = "<li class=\"rv call\"><span class=\"st s-judge\">Decide</span>";
    let authors = "<span class=\"st s-judge\">Unresolved</span>";
    assert!(index("friend", None).await.contains(yours));
    let owner = index("owner", None).await;
    assert!(owner.contains(authors) && owner.contains("<span>fa ·</span> <span>you can rule</span>") && !owner.contains("f-call") && !owner.contains("rv call"), "the owner's own PRs only: {owner}");
    let page = hub.page("stranger", "r1").await;
    assert!(page.contains("<p class=\"vt\">Unresolved</p>") && page.contains("<title>Unresolved · "), "{page}");
    assert!(index("stranger", None).await.contains(authors));
    assert!(index("owner", Some("via")).await.contains(authors), "not identified");
    // A cancel that arrives after the review ended says how it ended.
    let (code, body) = hub.post("friend", "/r/r1/cancel", &[]).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("r1 had already ended: it was approved at ") && body.contains(" UTC."), "{body}");
    assert_eq!(hub.api("r1").await["verdict"], "approve");
    assert!(!hub.page("stranger", "r1").await.contains("Must fix"));
    let (code, _) = hub.post("stranger", "/r/r1/f/F1", &[("ruling", "must_fix")]).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert!(hub.api("r1").await["findings"][0].get("ruling").is_none());

    let (code, _) = hub.post("friend", "/r/r1/f/F2", &[("ruling", "must_fix")]).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    assert_eq!(hub.api("r1").await["verdict"], "changes_requested", "must fix blocks whatever its severity");
    let ev = wait_for(&hub.tailnet, "fa", "ruled F2").await;
    assert!(ev.contains("[r1] friend (a human) ruled F2 must fix (verdict: changes requested).\nNEXT: fix F2, push, then call `request_rereview` with review_id \"r1\"."), "{ev}");
    let page = hub.page("friend", "r1").await;
    assert!(page.contains("<details class=\"f\" id=\"F2\" open>") && page.contains("value=\"undo\""), "a ruled finding stays open with its Undo: {page}");
    let (code, _) = hub.post("friend", "/r/r1/f/F2", &[("ruling", "must_fix")]).await;
    assert_eq!(code, StatusCode::SEE_OTHER, "the same ruling again changes nothing");
    let (code, _) = hub.post("owner", "/r/r1/f/F2", &[("ruling", "undo")]).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    let ev = wait_for(&hub.tailnet, "fa", "undid").await;
    assert!(ev.contains("[r1] the hub owner (a human) undid the ruling on F2 (verdict: approve).\nNEXT: nothing to fix; judge F1, F2 yourself."), "{ev}");
    assert!(!ev.contains("ruled F2 must fix"), "no event for a ruling that changed nothing: {ev}");
    hub.post("friend", "/r/r1/f/F1", &[("ruling", "must_fix")]).await;
    wait_for(&hub.tailnet, "fa", "ruled F1").await;
    // Read later, a dismissal still points at what is left to fix.
    hub.post("friend", "/r/r1/f/F2", &[("ruling", "dismiss")]).await;
    let ev = wait_for(&hub.tailnet, "fa", "dismissed F2").await;
    assert!(ev.contains("[r1] friend (a human) dismissed F2 (verdict: changes requested).\nNEXT: fix F1, push, then call `request_rereview` with review_id \"r1\"."), "{ev}");
    assert!(!ev.contains("nothing pending"), "{ev}");
    let said = hub.reviewer("rv2", "say", json!({"review_id": "r1", "text": "Thanks."})).await;
    assert!(said.contains("r1"), "{said}");
    let ev = call(&hub.tailnet, "fa", "wait", json!({"timeout_seconds": 2, "progress": true})).await;
    assert!(ev.contains("Thanks.") && ev.contains("NEXT: fix F1, push") && !ev.contains("nothing pending"), "a remark after a ruling: {ev}");
    let (code, body) = hub.post("friend", "/r/r1/f/F9", &[("ruling", "dismiss")]).await;
    assert_eq!(code, StatusCode::NOT_FOUND, "{body}");
    let (code, _) = hub.post("friend", "/r/r1/f/F1", &[("ruling", "maybe")]).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);

    let hub = hub.restart().await;
    let r1 = hub.api("r1").await;
    assert_eq!(r1["verdict"], "changes_requested");
    assert_eq!((r1["findings"][0]["ruling"]["by"].as_str(), r1["findings"][0]["ruling"]["must_fix"].as_bool()), (Some("friend"), Some(true)));
    assert_eq!(r1["findings"][1]["ruling"]["must_fix"], false);
    let rulings: Vec<&str> = r1["log"].as_array().unwrap().iter().filter(|e| e["kind"] == "ruling").map(|e| e["text"].as_str().unwrap()).collect();
    assert_eq!(rulings, ["F2: must fix", "F2: ruling undone", "F1: must fix", "F2: dismissed"]);
    let page = hub.page("friend", "r1").await;
    assert!(page.contains("Ruled must fix by <b>friend</b>") && page.contains("Dismissed by <b>friend</b>"), "{page}");
    assert!(page.contains("<span class=\"badge s-changes\">must fix</span>") && page.contains("Changes requested"), "{page}");
    let status = hub.friend("fa", "review_status", json!({"review_id": "r1"})).await;
    assert!(status.contains("ruled must fix by friend") && status.contains("CHANGES REQUESTED"), "{status}");

    // The verdict follows the rulings a state file holds, whatever verdict it stored: an older
    // relay drops rulings when it saves, and a hand edit can leave them out of step.
    let edit = |hub: Hub, f: fn(&mut Value)| async move {
        hub.running.stop().await;
        let path = hub.cfg.state.clone().unwrap();
        let mut snap: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        f(&mut snap["reviews"][0]);
        std::fs::write(&path, snap.to_string()).unwrap();
        run(hub.cfg.clone()).await
    };
    let hub = edit(hub, |r| {
        for f in r["findings"].as_array_mut().unwrap() {
            f.as_object_mut().unwrap().remove("ruling");
        }
    })
    .await;
    let r1 = hub.api("r1").await;
    assert_eq!((r1["verdict"].as_str(), r1["result"]["verdict"].as_str()), (Some("approve"), Some("approve")), "{r1}");
    assert!(hub.page("friend", "r1").await.contains("<p class=\"vt\">Decide</p>"), "approve, with the reviewers' split left to decide");
    let (code, _) = hub.post("friend", "/r/r1/f/F1", &[("ruling", "must_fix")]).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    wait_for(&hub.tailnet, "fa", "ruled F1 must fix (verdict: changes requested)").await;
    hub.post("friend", "/r/r1/f/F2", &[("ruling", "dismiss")]).await;
    let hub = edit(hub, |r| {
        r["result"]["verdict"] = json!("approve");
        r["result"]["text"] = json!("[r1] Review result: APPROVE");
    })
    .await;
    let r1 = hub.api("r1").await;
    assert_eq!((r1["verdict"].as_str(), r1["result"]["verdict"].as_str()), (Some("changes_requested"), Some("changes_requested")), "{r1}");
    assert!(r1["result"]["text"].as_str().unwrap().contains("ruled must fix by friend"), "{r1}");
    let list: Value = reqwest::get(format!("{}/api/reviews", hub.local)).await.unwrap().json().await.unwrap();
    assert_eq!(list["reviews"].as_array().unwrap().iter().find(|r| r["id"] == "r1").unwrap()["verdict"], "changes_requested");
    let page = hub.page("friend", "r1").await;
    assert!(page.contains("<p class=\"vt\">Changes requested</p>") && !page.contains("Approved"), "{page}");
    let status = hub.friend("fa", "review_status", json!({"review_id": "r1"})).await;
    assert!(status.contains("CHANGES REQUESTED") && !status.contains("APPROVE"), "{status}");

    for (r, model) in [("rv1", "claude"), ("rv2", "codex")] {
        hub.reviewer(r, "hello", json!({"role": "reviewer", "model": model})).await;
    }
    hub.friend("fa", "request_rereview", json!({"review_id": "r1", "new_head_sha": SHA2, "note": "Added backoff."})).await;
    let page = hub.page("friend", "r2").await;
    assert!(page.contains("<h2>Discussion <span class=\"count\">1</span></h2><ol class=\"talk\"><li><div class=\"th\"><b>fa</b> <time class=\"clock\"") && page.contains("<p>Added backoff.</p>"), "the note opens the discussion: {page}");
    let all = wait_for(&hub.local, "rv1", "carried from r1/F1").await;
    let task = &all[all.find("[r2] Your team is complete").unwrap()..];
    assert!(task.contains("C1 [major] carried from r1/F1, ruled must fix by friend:"), "{task}");
    assert!(task.contains("Findings ruled must fix are real (a human decided); vote only on whether they are still present."), "{task}");
    assert!(!task.contains("idempotency"), "a dismissed finding is not carried: {task}");
    let r2 = hub.api("r2").await;
    assert_eq!(r2["findings"].as_array().unwrap().len(), 1);
    assert_eq!(r2["findings"][0]["ruling"]["must_fix"], true);

    let (code, body) = hub.post("friend", "/r/r1/f/F2", &[("ruling", "undo")]).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("r2 re-reviews r1, so rulings on r1 are locked unless r2 fails; reload the page"), "{body}");
    assert!(!hub.page("friend", "r1").await.contains("Undo"), "no controls once a re-review runs");

    // The reviewers split on whether it is fixed, so the ruling stands.
    for r in ["rv1", "rv2"] {
        hub.reviewer(r, "submit_findings", json!({"review_id": "r2", "head_sha": SHA2, "verdict": "approve", "checked": "src/lib.rs", "findings": []})).await;
    }
    hub.reviewer("rv1", "vote", json!({"review_id": "r2", "votes": [{"finding": "C1", "vote": "agree"}]})).await;
    hub.reviewer("rv2", "vote", json!({"review_id": "r2", "votes": [{"finding": "C1", "vote": "disagree", "reason": "Fixed."}]})).await;
    assert_eq!(hub.api("r2").await["verdict"], "changes_requested");
    hub.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(not(target_os = "linux"), ignore = "binds 127.0.0.2-6, which only Linux routes by default")]
async fn every_page_names_its_viewer_as_tailscale_reports_them_and_is_not_cached() {
    let tmp = tempfile::tempdir().unwrap();
    let hub = start(tmp.path(), 2).await;
    hub.friend("fa", "request_review", json!({"title": "Add a cache", "pr_url": pr(1), "count": 2})).await;
    hub.running.hub.owner.lock().unwrap().as_mut().unwrap().login = "boss@example.com".into();
    let get = |who: &'static str, path: &'static str, proxy: Option<&'static str>| {
        let url = format!("{}{path}", hub.base(who));
        async move {
            let mut req = browser(who).get(url);
            if let Some(h) = proxy {
                req = req.header(h, "x");
            }
            let r = req.send().await.unwrap();
            assert_eq!(r.headers()["cache-control"], "no-store", "{who} {path}");
            r.text().await.unwrap()
        }
    };
    let me = |page: &str| page.split("<span class=\"me").nth(1).map(|s| s[..s.find("</span></header>").unwrap()].to_string()).unwrap();

    for path in ["/", "/r/r1", "/r/r9"] {
        let friend = get("friend", path, None).await;
        assert!(me(&friend).ends_with("<span class=\"sr\">Viewing as </span>friend@example.com"), "{friend}");
        assert!(friend.find("class=\"me").unwrap() < friend.find("<main").unwrap(), "outside what polling replaces");
        let tagged = me(&get("tagged", path, None).await);
        assert!(tagged.ends_with("Viewing as </span>tagged-devices"), "{tagged}");
        let owner = me(&get("owner", path, None).await);
        assert!(owner.starts_with("\"><svg") && owner.ends_with("<span class=\"sr\">Viewing on </span>This machine") && !owner.contains("boss"), "{owner}");
        for h in ["x-forwarded-for", "via", "tailscale-user-login"] {
            let proxied = get("owner", path, Some(h)).await;
            assert!(me(&proxied).ends_with(" muted\"><svg class=\"i\" viewBox=\"0 0 16 16\" aria-hidden=\"true\"><circle cx=\"8\" cy=\"5.25\" r=\"2.75\"/><path d=\"M2.75 14c.6-2.9 2.6-4.5 5.25-4.5s4.65 1.6 5.25 4.5\"/></svg> Not identified"), "{h}: {proxied}");
            assert!(!proxied.contains("boss"), "nothing of the owner's behind a proxy ({h}): {proxied}");
        }
    }
    let (code, refused) = hub.post("stranger", "/r/r1/cancel", &[]).await;
    assert!(code == StatusCode::FORBIDDEN && refused.contains("Viewing as </span>stranger@example.com</span>"), "{refused}");

    hub.running.hub.owner.lock().unwrap().as_mut().unwrap().login = "local".into();
    let unnamed = me(&get("owner", "/r/r1", None).await);
    assert!(unnamed.ends_with("Viewing on </span>This machine") && !unnamed.contains("local"), "a placeholder login is never shown: {unnamed}");
    *hub.running.hub.owner.lock().unwrap() = None;
    for path in ["/", "/r/r1"] {
        assert!(me(&get("owner", path, None).await).ends_with("Not identified"), "before Tailscale names the owner");
    }
    hub.running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_review_0_15_0_approved_with_a_confirmed_minor_reads_approved_over_http() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("state.json"), include_str!("fixtures/state-0.15.0.json")).unwrap();
    let mut hub = start(tmp.path(), 2).await;
    for _ in 0..2 {
        let r1 = hub.api("r1").await;
        assert_eq!((r1["verdict"].as_str(), r1["result"]["verdict"].as_str()), (Some("approve"), Some("approve")), "{r1}");
        assert!(r1["result"]["text"].as_str().unwrap().contains(": DECIDE ("), "{r1}");
        let list: Value = reqwest::get(format!("{}/api/reviews", hub.local)).await.unwrap().json().await.unwrap();
        assert_eq!(list["reviews"][0]["verdict"], "approve");
        let page = hub.page("owner", "r1").await;
        assert!(page.contains("<p class=\"vt\">Decide</p>") && !page.contains(" to fix"), "{page}");
        let index = browser("owner").get(format!("{}/", hub.local)).send().await.unwrap().text().await.unwrap();
        assert!(index.contains("Decide</span>") && !index.contains("to fix"), "{index}");
        let status = hub.owner("al-author", "review_status", json!({"review_id": "r1"})).await;
        assert!(status.contains(": DECIDE (") && !status.contains("CHANGES REQUESTED"), "{status}");
        hub = hub.restart().await;
    }
    let (code, _) = hub.post("owner", "/r/r1/f/F2", &[("ruling", "dismiss")]).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    let ev = wait_for(&hub.local, "al-author", "dismissed F2").await;
    assert!(ev.contains("dismissed F2 (verdict: approve).\nNEXT: nothing to fix.") && !ev.contains("NEXT: fix"), "{ev}");
    assert_eq!(hub.api("r1").await["verdict"], "approve");
    let page = browser("owner").get(format!("{}/r/r9", hub.local)).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("r9 isn't in the hub's history any more (older reviews are trimmed)."), "{page}");
    hub.running.stop().await;
}
