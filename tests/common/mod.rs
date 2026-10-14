#![allow(dead_code)]

use amx::Running;
use amx::config::Config;
use amx::model::{Author, Kind, Message, Verdict};
use amx::node::PostInput;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    p
}

/// A gh stand-in whose head SHA is read from `<dir>/head` (missing file means gh fails).
pub fn gh_stub(dir: &Path) -> PathBuf {
    write_script(
        dir,
        "gh",
        &format!(
            r#"[ -f "{0}/head" ] || {{ echo "gh: not logged in" >&2; exit 1; }}
printf '{{"title":"Stub PR","headRefOid":"%s","state":"OPEN","author":{{"login":"octo"}}}}' "$(cat "{0}/head")""#,
            dir.display()
        ),
    )
}

pub fn config(dir: &Path, port: u16) -> Config {
    Config {
        port,
        data_dir: dir.join("data"),
        machine: Some(dir.file_name().unwrap().to_string_lossy().into_owned()),
        tailnet: false,
        gh_bin: dir.join("no-gh"),
        sync_interval_secs: 1,
        ..Config::default()
    }
}

pub struct TestNode {
    pub dir: PathBuf,
    pub port: u16,
    pub peers: Vec<String>,
    pub running: Option<Running>,
}

impl TestNode {
    pub async fn start(root: &Path, name: &str) -> TestNode {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let mut n = TestNode {
            dir,
            port: free_port(),
            peers: Vec::new(),
            running: None,
        };
        n.up().await;
        n
    }

    pub async fn up(&mut self) {
        let cfg = Config {
            peers: self.peers.clone(),
            ..config(&self.dir, self.port)
        };
        self.running = Some(amx::start(cfg).await.unwrap());
    }

    pub async fn down(&mut self) {
        if let Some(r) = self.running.take() {
            r.stop().await;
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn node(&self) -> &std::sync::Arc<amx::node::Node> {
        &self.running.as_ref().expect("node running").node
    }

    pub fn peer_with(&mut self, other: &TestNode) {
        self.peers.push(other.url());
        if let Some(r) = &self.running {
            r.node.peers.set_static(&[other.url()]);
        }
    }

    pub fn agent(&self, name: &str) -> Author {
        self.node().agent(name)
    }

    pub async fn post(&self, agent: &str, kind: Kind, body: &str) -> Message {
        self.post_input(
            agent,
            PostInput {
                kind: Some(kind),
                pr_url: PR.into(),
                body: body.into(),
                verdict: (kind == Kind::Review).then_some(Verdict::Comment),
                ..Default::default()
            },
        )
        .await
    }

    pub async fn post_input(&self, agent: &str, input: PostInput) -> Message {
        self.node().post(self.agent(agent), input).await.unwrap()
    }

    pub fn count(&self) -> i64 {
        self.node().store.count().unwrap()
    }
}

pub const PR: &str = "https://github.com/rust-lang/rust/pull/1";

pub async fn eventually<F: Fn() -> bool>(what: &str, secs: u64, f: F) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for: {what}");
}
