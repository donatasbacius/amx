mod common;

use amx::model::Kind;
use amx::sync::{PushRequest, PushResponse};
use common::*;

fn same(a: &TestNode, b: &TestNode) -> bool {
    a.node().store.version_vector().unwrap() == b.node().store.version_vector().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn three_nodes_converge() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = TestNode::start(tmp.path(), "a").await;
    let mut b = TestNode::start(tmp.path(), "b").await;
    let c = TestNode::start(tmp.path(), "c").await;
    a.peer_with(&b);
    a.peer_with(&c);
    b.peer_with(&c);

    a.post("alice", Kind::Request, "please review").await;
    b.post("bob", Kind::Claim, "").await;
    c.post("carol", Kind::Reply, "hi").await;
    for i in 0..10 {
        [&a, &b, &c][i % 3].post("x", Kind::Reply, &format!("m{i}")).await;
    }
    eventually("13 messages everywhere", 15, || {
        a.count() == 13 && b.count() == 13 && c.count() == 13 && same(&a, &b) && same(&b, &c)
    })
    .await;
    let ids = |n: &TestNode| n.node().store.all().unwrap().into_iter().map(|m| m.id).collect::<Vec<_>>();
    assert_eq!(ids(&a), ids(&b));
    assert_eq!(ids(&b), ids(&c));
}

#[tokio::test(flavor = "multi_thread")]
async fn push_makes_delivery_fast() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = TestNode::start(tmp.path(), "a").await;
    let b = TestNode::start(tmp.path(), "b").await;
    a.peer_with(&b);
    eventually("first round", 5, || a.node().peers.outbound()[0].last_ok.is_some()).await;
    let t = std::time::Instant::now();
    a.post("alice", Kind::Request, "fast?").await;
    eventually("b has it", 5, || b.count() == 1).await;
    assert!(t.elapsed() < std::time::Duration::from_millis(900), "took {:?}", t.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn transitive_relay_without_direct_link() {
    let tmp = tempfile::tempdir().unwrap();
    // Only A and C know about B; B knows nobody. A and C never talk directly.
    let mut a = TestNode::start(tmp.path(), "a").await;
    let b = TestNode::start(tmp.path(), "b").await;
    let mut c = TestNode::start(tmp.path(), "c").await;
    a.peer_with(&b);
    c.peer_with(&b);

    a.post("alice", Kind::Request, "from a").await;
    eventually("c got a's message via b", 10, || c.count() == 1).await;
    c.post("carol", Kind::Reply, "from c").await;
    eventually("a got c's reply via b", 10, || a.count() == 2).await;
    assert!(same(&a, &c) && same(&a, &b));
    assert!(b.node().peers.outbound().is_empty());
    assert_eq!(b.node().peers.inbound().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn node_down_for_a_while_catches_up() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = TestNode::start(tmp.path(), "a").await;
    let mut b = TestNode::start(tmp.path(), "b").await;
    let mut c = TestNode::start(tmp.path(), "c").await;
    a.peer_with(&b);
    b.peer_with(&c);
    a.post("alice", Kind::Request, "before").await;
    eventually("all have 1", 10, || b.count() == 1 && c.count() == 1).await;

    b.down().await;
    for i in 0..30 {
        a.post("alice", Kind::Reply, &format!("while b is down {i}")).await;
    }
    c.post("carol", Kind::Reply, "c while b is down").await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(c.count(), 2, "c can only learn a's messages through b");
    let err = a.node().peers.outbound()[0].last_err.clone();
    assert!(err.is_some(), "a should record the failed sync");

    b.up().await;
    eventually("everyone has 32", 15, || a.count() == 32 && b.count() == 32 && c.count() == 32).await;
    b.post("bob", Kind::Reply, "back").await;
    eventually("everyone has 33", 10, || a.count() == 33 && c.count() == 33).await;
    let _ = &mut c;
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_delivery_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let a = TestNode::start(tmp.path(), "a").await;
    let b = TestNode::start(tmp.path(), "b").await;
    for i in 0..5 {
        a.post("alice", Kind::Reply, &format!("m{i}")).await;
    }
    let msgs = a.node().store.all().unwrap();
    let http = reqwest::Client::new();
    let push = |msgs: Vec<amx::model::Message>| {
        let http = http.clone();
        let url = format!("{}/api/push", b.url());
        async move {
            http.post(url)
                .json(&PushRequest { node_id: "test".into(), messages: msgs })
                .send()
                .await
                .unwrap()
                .json::<PushResponse>()
                .await
                .unwrap()
                .inserted
        }
    };
    assert_eq!(push(msgs[..3].to_vec()).await, 3);
    assert_eq!(push(msgs.clone()).await, 2);
    assert_eq!(push(msgs.clone()).await, 0);
    let mut reversed = msgs.clone();
    reversed.reverse();
    assert_eq!(push(reversed).await, 0);
    assert_eq!(b.count(), 5);
    // A gap (seq 7 without 6) is refused rather than leaving a hole in the version vector.
    let mut bogus = msgs[4].clone();
    bogus.id = amx::model::new_id();
    bogus.seq = 7;
    assert_eq!(push(vec![bogus]).await, 0);
    assert_eq!(b.node().store.version_vector().unwrap()[&a.node().node_id], 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn large_backlog_pages() {
    let tmp = tempfile::tempdir().unwrap();
    let a = TestNode::start(tmp.path(), "a").await;
    for i in 0..1200 {
        a.post("alice", Kind::Reply, &format!("m{i}")).await;
    }
    let mut b = TestNode::start(tmp.path(), "b").await;
    b.peer_with(&a);
    eventually("b has all 1200", 20, || b.count() == 1200).await;
}
