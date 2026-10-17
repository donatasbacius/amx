//! The binary: the relay process.

use serde_json::Value;
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

fn curl(url: &str, headers: &[&str], body: &str) -> Command {
    let mut c = Command::new("curl");
    c.args(["-sS", "-H", "content-type: application/json", "-H", "user-agent: claude-code/2.1", "-d", body]);
    for h in headers {
        c.args(["-H", h]);
    }
    c.arg(format!("{url}/mcp"));
    c
}

#[test]
fn a_crash_during_a_long_wait_loses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    let state = state.to_str().unwrap();
    let port = free_port();
    let (relay, url) = relay_with(port, &["--state", state]);
    let init = curl(&url, &[], r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#).arg("-D").arg("-").output().unwrap();
    let head = String::from_utf8(init.stdout).unwrap();
    let sid = head.lines().find_map(|l| l.strip_prefix("mcp-session-id: ")).expect("a session id").trim().to_string();
    let session = format!("mcp-session-id: {sid}");
    let headers = [session.as_str(), "accept: application/json, text/event-stream"];
    let hello = curl(&url, &headers, r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"hello","arguments":{"role":"reviewer"}}}"#).output().unwrap();
    assert!(String::from_utf8_lossy(&hello.stdout).contains("You are `local-claude`"));

    let wait = |id: u32| format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"wait","arguments":{{}},"_meta":{{"progressToken":{id}}}}}}}"#);
    let first = curl(&url, &headers, &wait(2)).stdout(Stdio::piped()).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    drop(relay);
    let cut = String::from_utf8(first.wait_with_output().unwrap().stdout).unwrap();
    assert!(!cut.contains("\"result\""), "a crash ends the wait without an answer for the client to take as one: {cut}");

    let (_relay, url) = relay_with(port, &["--state", state]);
    let retry = curl(&url, &headers, &wait(3)).stdout(Stdio::piped()).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(request_review(&url, "https://github.com/rust-lang/rust/pull/9")["result"]["isError"], false);
    let again = String::from_utf8(retry.wait_with_output().unwrap().stdout).unwrap();
    assert!(again.contains("Review offer"), "the same session, and name, after the restart: {again}");
}
