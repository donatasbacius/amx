//! The binary: relay process, stdio MCP proxy, Stop hook and watch.

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_amx");

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Kill(Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn relay() -> (Kill, String) {
    relay_with(free_port(), &[])
}

fn relay_with(port: u16, extra: &[&str]) -> (Kill, String) {
    let child = Command::new(BIN)
        .args(["relay", "--no-tailnet", "--port", &port.to_string()])
        .args(extra)
        .env("XDG_CONFIG_HOME", "/nonexistent")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "relay did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    (Kill(child), url)
}

#[test]
fn missing_explicit_config_is_an_error() {
    let out = Command::new(BIN)
        .args(["relay", "--no-tailnet", "--config", "/nonexistent/amx.toml"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

fn request_review(url: &str, pr: &str) -> Value {
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"request_review","arguments":{{"pr_url":"{pr}","reviewers":1}}}}}}"#
    );
    let out = Command::new("curl")
        .args(["-sS", "-H", "content-type: application/json", "-d", &body])
        .arg(format!("{url}/mcp?agent=owner-claude"))
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}

fn get(url: &str) -> String {
    let out = Command::new("curl").args(["-sS", url]).output().unwrap();
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn state_file_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("amx/state.json");
    let state = state.to_str().unwrap();
    let port = free_port();
    let (mut relay, url) = relay_with(port, &["--state", state]);
    let r = request_review(&url, "https://github.com/rust-lang/rust/pull/77");
    assert_eq!(r["result"]["isError"], false, "{r}");
    let status = Command::new("kill").args(["-TERM", &relay.0.id().to_string()]).status().unwrap();
    assert!(status.success());
    assert!(relay.0.wait().unwrap().success());
    let saved = std::fs::read_to_string(state).unwrap();
    assert!(saved.contains("\"version\":1") && saved.contains("\"number\":77"), "{saved}");

    let (_relay, url) = relay_with(port, &["--state", state]);
    let page = get(&format!("{url}/r/r1"));
    assert!(page.contains("rust-lang/rust#77"), "{page}");
    let r = request_review(&url, "https://github.com/rust-lang/rust/pull/78");
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("r2"), "{r}");
}

#[test]
fn unreadable_state_file_is_refused_and_kept() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    std::fs::write(&state, "{\"version\":1,\"reviews\":[").unwrap();
    let out = Command::new(BIN)
        .args(["relay", "--no-tailnet", "--port", &free_port().to_string(), "--state"])
        .arg(&state)
        .env("XDG_CONFIG_HOME", "/nonexistent")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains(state.to_str().unwrap()) && err.contains("not valid"), "{err}");
    assert_eq!(std::fs::read_to_string(&state).unwrap(), "{\"version\":1,\"reviews\":[");
}

fn hook(url: &str, agent: &str, extra: &[&str]) -> String {
    let mut child = Command::new(BIN)
        .args(["hook", "codex", "stop", "--relay", url, "--agent", agent])
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(br#"{"hook_event_name":"Stop","stop_hook_active":false}"#).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn stdio_proxy_hook_and_watch() {
    let (_relay, url) = relay();
    std::thread::sleep(Duration::from_millis(300));

    let mut proxy = Kill(
        Command::new(BIN)
            .args(["mcp", "--relay", &format!("{url}/mcp"), "--agent", "rev-codex", "--role", "reviewer"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdin = proxy.0.stdin.take().unwrap();
    let mut lines = BufReader::new(proxy.0.stdout.take().unwrap()).lines();
    let mut call = |line: &str| -> Value {
        writeln!(stdin, "{line}").unwrap();
        serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap()
    };
    let init = call(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"codex-mcp-client"}}}"#);
    assert!(init["result"]["instructions"].as_str().unwrap().contains("rev-codex"));
    let tools = call(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 13);

    let mut watcher = Kill(
        Command::new(BIN)
            .args(["watch", "--relay", &url, "--agent", "rev-codex"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut watch_lines = BufReader::new(watcher.0.stdout.take().unwrap()).lines();
    std::thread::sleep(Duration::from_millis(500));

    assert_eq!(hook(&url, "rev-codex", &[]), "", "idle reviewer may stop");
    assert!(hook(&url, "rev-codex", &["--keep-waiting"]).contains("\"decision\":\"block\""));

    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"request_review","arguments":{"pr_url":"https://github.com/rust-lang/rust/pull/1","reviewers":1}}}"#;
    let out = Command::new("curl")
        .args(["-sS", "-H", "content-type: application/json", "-d", body])
        .arg(format!("{url}/mcp?agent=owner-claude"))
        .output()
        .unwrap();
    let r: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(r["result"]["isError"], false, "{r}");

    let line = watch_lines.next().unwrap().unwrap();
    assert!(line.starts_with("amx r1 [r1] Review offer"), "{line}");
    let block = hook(&url, "rev-codex", &[]);
    let v: Value = serde_json::from_str(&block).unwrap();
    assert_eq!(v["decision"], "block");
    assert!(v["reason"].as_str().unwrap().contains("accept_review"));

    let w = call(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"wait","arguments":{"timeout_seconds":5}}}"#);
    assert!(w["result"]["content"][0]["text"].as_str().unwrap().contains("Review offer"), "{w}");
    assert_eq!(hook("http://127.0.0.1:1", "rev-codex", &[]), "", "unreachable relay never blocks");
}

#[test]
fn stdio_proxy_keeps_waiting_while_idle() {
    let (_relay, url) = relay();
    let mut proxy = Kill(
        Command::new(BIN)
            .args(["mcp", "--relay", &url, "--agent", "rev-long", "--role", "reviewer", "--wait-max", "8", "--wait-step", "1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdin = proxy.0.stdin.take().unwrap();
    let mut lines = BufReader::new(proxy.0.stdout.take().unwrap()).lines();
    let mut next = || -> Value { serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap() };
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#).unwrap();
    assert!(next()["result"]["instructions"].is_string());

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"wait","arguments":{{}}}}}}"#).unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","method":"notifications/cancelled","params":{{"requestId":2}}}}"#).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let started = Instant::now();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"wait","arguments":{{}},"_meta":{{"progressToken":"t3"}}}}}}"#
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(2500));
    let r = request_review(&url, "https://github.com/rust-lang/rust/pull/5");
    assert_eq!(r["result"]["isError"], false, "{r}");
    let mut progress = 0;
    let resp = loop {
        let v = next();
        if v["method"] == "notifications/progress" {
            assert_eq!(v["params"]["progressToken"], "t3");
            progress += 1;
            continue;
        }
        break v;
    };
    assert_eq!(resp["id"], 3, "the cancelled wait sends nothing: {resp}");
    assert!(resp["result"]["content"][0]["text"].as_str().unwrap().contains("Review offer"), "{resp}");
    assert!(progress >= 1 && started.elapsed() > Duration::from_secs(2), "one call spans several relay long-polls");
}
