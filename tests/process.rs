mod common;

use common::{PR, free_port};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_amx");

fn spawn(dir: &Path, name: &str, port: u16, peer: u16) -> Child {
    Command::new(BIN)
        .args(["serve", "--no-tailnet", "--sync-interval", "1", "--machine", name])
        .args(["--port", &port.to_string()])
        .args(["--data-dir", &dir.join(name).to_string_lossy()])
        .args(["--peer", &format!("http://127.0.0.1:{peer}")])
        .args(["--gh-bin", "/nonexistent/gh"])
        .env("XDG_CONFIG_HOME", dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn cli(port: u16, args: &[&str]) -> Output {
    Command::new(BIN)
        .env("AMX_NODE", format!("http://127.0.0.1:{port}"))
        .args(args)
        .output()
        .unwrap()
}

fn count(port: u16) -> Option<i64> {
    let out = cli(port, &["status", "--json"]);
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v["messages"].as_i64()
}

fn wait_count(port: u16, n: i64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if count(port) == Some(n) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("node on {port} never reached {n} messages (has {:?})", count(port));
}

#[test]
fn killed_process_catches_up_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let (pa, pb) = (free_port(), free_port());
    let mut a = spawn(tmp.path(), "a", pa, pb);
    let mut b = spawn(tmp.path(), "b", pb, pa);
    let post = |port: u16, agent: &str, body: &str| {
        let out = cli(port, &["post", "reply", PR, "-m", body, "--agent", agent]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    wait_count(pa, 0);
    wait_count(pb, 0);
    post(pa, "alice", "one");
    wait_count(pb, 1);

    b.kill().unwrap();
    b.wait().unwrap();
    for i in 0..5 {
        post(pa, "alice", &format!("while b is dead {i}"));
    }
    std::thread::sleep(Duration::from_millis(1500));
    b = spawn(tmp.path(), "b", pb, pa);
    wait_count(pb, 6);
    post(pb, "bob", "back");
    wait_count(pa, 7);

    let out = cli(pb, &["inbox", "--agent", "alice", "--json"]);
    assert!(out.status.success());
    let thread = cli(pa, &["thread", PR]);
    let text = String::from_utf8_lossy(&thread.stdout);
    assert!(text.contains("bob@b replied") && text.contains("while b is dead 4"), "{text}");

    a.kill().unwrap();
    b.kill().unwrap();
    let _ = (a.wait(), b.wait());
}

#[test]
fn hook_output_and_unreachable_node() {
    let tmp = tempfile::tempdir().unwrap();
    let p = free_port();
    let mut a = spawn(tmp.path(), "a", p, free_port());
    wait_count(p, 0);
    let run_hook = |event: &str, stdin: &str| {
        let mut child = Command::new(BIN)
            .env("AMX_NODE", format!("http://127.0.0.1:{p}"))
            .args(["hook", "codex", event, "--agent", "bob"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    };
    let out = run_hook("stop", r#"{"hook_event_name":"Stop","stop_hook_active":false}"#);
    assert!(out.status.success());
    assert!(out.stdout.is_empty(), "no mail, no output");
    let o = cli(p, &["post", "request", PR, "--to", "bob", "-m", "review please", "--agent", "alice"]);
    assert!(o.status.success());
    let out = run_hook("stop", "{}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["decision"], "block");
    assert!(v["reason"].as_str().unwrap().contains("review please"));
    let again = run_hook("stop", "{}");
    assert!(again.stdout.is_empty(), "each item is surfaced once");
    let start = run_hook("session-start", "{}");
    let v: serde_json::Value = serde_json::from_slice(&start.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
    let ups = run_hook("", r#"{"hook_event_name":"UserPromptSubmit","prompt":"hi"}"#);
    assert!(ups.stdout.is_empty());
    a.kill().unwrap();
    let _ = a.wait();
    let down = run_hook("stop", "{}");
    assert!(down.status.success() && down.stdout.is_empty(), "node down must not break the agent");
}
