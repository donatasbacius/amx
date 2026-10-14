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
    let port = free_port();
    let child = Command::new(BIN)
        .args(["relay", "--no-tailnet", "--port", &port.to_string()])
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
