//! Polling the metrics API on the engine's fake API server.
//!
//! The fake server answers the `metrics.k8s.io` paths with canned lists, in
//! order, the last one repeating. Polls run every 100 ms here, not 15 s.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

#[path = "../../engine/tests/support/fake_api.rs"]
mod fake_api;

use std::time::Duration;

use fake_api::FakeApi;
use model::{ContextName, UsageInterest, UsageObject, UsageScope, UsageSnapshot, UsageStatus};
use resource_metrics::{PollConfig, UsagePoller};
use serde_json::json;
use tokio::sync::mpsc;

const NODES: &str = "/apis/metrics.k8s.io/v1beta1/nodes";
const ALL_PODS: &str = "/apis/metrics.k8s.io/v1beta1/pods";
const SHOP_PODS: &str = "/apis/metrics.k8s.io/v1beta1/namespaces/shop/pods";

const FAST: PollConfig = PollConfig {
    interval: Duration::from_millis(100),
    timeout: Duration::from_secs(2),
    window: Duration::from_hours(1),
};

/// A time in seconds since the epoch, as metrics-server writes it.
fn timestamp(secs: i64) -> String {
    k8s_openapi::jiff::Timestamp::from_second(secs)
        .unwrap()
        .to_string()
}

fn nodes(secs: i64, millicores: u32) -> String {
    json!({
        "kind": "NodeMetricsList",
        "apiVersion": "metrics.k8s.io/v1beta1",
        "metadata": {},
        "items": [{
            "metadata": { "name": "node-a" },
            "timestamp": timestamp(secs),
            "window": "20s",
            "usage": { "cpu": format!("{millicores}m"), "memory": "1Gi" },
        }],
    })
    .to_string()
}

fn pods(namespace: &str, secs: i64, millicores: u32) -> String {
    json!({
        "kind": "PodMetricsList",
        "apiVersion": "metrics.k8s.io/v1beta1",
        "metadata": {},
        "items": [{
            "metadata": { "name": "web-1", "namespace": namespace },
            "timestamp": timestamp(secs),
            "window": "15s",
            "containers": [{ "name": "app", "usage": { "cpu": format!("{millicores}m"), "memory": "64Mi" } }],
        }],
    })
    .to_string()
}

fn web_1() -> UsageObject {
    UsageObject::Pod {
        namespace: "shop".into(),
        name: "web-1".into(),
    }
}

fn start(
    api: &FakeApi,
    config: PollConfig,
) -> (UsagePoller, mpsc::UnboundedReceiver<UsageSnapshot>) {
    let mut kube_config = kube::Config::new(api.url().parse().unwrap());
    // Errors should reach the poller at once, not after kube's own retries.
    kube_config.default_retry = false;
    let client = kube::Client::try_from(kube_config).unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let poller = UsagePoller::start(client, ContextName::new("fake"), config, move |snapshot| {
        tx.send(snapshot).is_ok()
    });
    (poller, rx)
}

async fn next(rx: &mut mpsc::UnboundedReceiver<UsageSnapshot>) -> UsageSnapshot {
    tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("a snapshot within 3 s")
        .expect("the poller is running")
}

/// Waits for a snapshot that passes `check`.
async fn until(
    rx: &mut mpsc::UnboundedReceiver<UsageSnapshot>,
    check: impl Fn(&UsageSnapshot) -> bool,
) -> UsageSnapshot {
    loop {
        let snapshot = next(rx).await;
        if check(&snapshot) {
            return snapshot;
        }
    }
}

fn metrics_requests(api: &FakeApi, path: &str) -> usize {
    api.requests().iter().filter(|r| r.path == path).count()
}

#[tokio::test]
async fn polls_nothing_without_subscribers() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let (_poller, _rx) = start(&api, FAST);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(api.requests().len(), 0);
}

#[tokio::test]
async fn a_subscription_polls_at_once_then_on_each_tick() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.respond(NODES, 200, &nodes(1_000, 250));
    api.respond(NODES, 200, &nodes(1_015, 500));
    let config = PollConfig {
        interval: Duration::from_secs(1),
        ..FAST
    };
    let (poller, mut rx) = start(&api, config);

    let started = tokio::time::Instant::now();
    let _nodes = poller.subscribe(UsageInterest::Latest(UsageScope::Nodes));
    let first = next(&mut rx).await;
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "the first poll waits for no tick"
    );
    let node = UsageObject::Node("node-a".into());
    assert_eq!(first.context, ContextName::new("fake"));
    assert_eq!(first.status[&UsageScope::Nodes], UsageStatus::Live);
    assert_eq!(first.latest[&node].cpu.value, 0.25);
    assert_eq!(first.latest[&node].memory.value, 1024.0 * 1024.0 * 1024.0);

    let second = next(&mut rx).await;
    assert_eq!(second.latest[&node].cpu.value, 0.5);
    assert_eq!(metrics_requests(&api, NODES), 2);
}

#[tokio::test]
async fn dropping_the_last_subscription_stops_polling() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.respond(NODES, 200, &nodes(1_000, 250));
    let (poller, mut rx) = start(&api, FAST);

    let nodes = poller.subscribe(UsageInterest::Latest(UsageScope::Nodes));
    next(&mut rx).await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(
        metrics_requests(&api, NODES) >= 2,
        "polls repeat while subscribed"
    );

    drop(nodes);
    // Let a poll already in flight finish.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let after_drop = metrics_requests(&api, NODES);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(metrics_requests(&api, NODES), after_drop);
}

#[tokio::test]
async fn all_pods_cover_a_namespace() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.respond(ALL_PODS, 200, &pods("shop", 1_000, 100));
    let (poller, mut rx) = start(&api, FAST);

    let _all = poller.subscribe(UsageInterest::Latest(UsageScope::Pods(None)));
    next(&mut rx).await;
    let shop = UsageScope::Pods(Some("shop".into()));
    let _shop = poller.subscribe(UsageInterest::Latest(shop.clone()));
    let snapshot = until(&mut rx, |s| s.status.contains_key(&shop)).await;

    assert_eq!(snapshot.status[&shop], UsageStatus::Live);
    assert_eq!(snapshot.latest[&web_1()].cpu.value, 0.1);
    assert_eq!(metrics_requests(&api, SHOP_PODS), 0);
}

#[tokio::test]
async fn history_builds_up_across_polls() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    // The second answer repeats the first sample, as metrics-server does
    // between scrapes. It must not add a point.
    api.respond(SHOP_PODS, 200, &pods("shop", 1_000, 100));
    api.respond(SHOP_PODS, 200, &pods("shop", 1_000, 100));
    api.respond(SHOP_PODS, 200, &pods("shop", 1_015, 300));
    let (poller, mut rx) = start(&api, FAST);

    let _chart = poller.subscribe(UsageInterest::History(web_1()));
    let snapshot = until(&mut rx, |s| {
        s.history.first().is_some_and(|h| h.cpu.samples.len() == 2)
    })
    .await;

    let history = &snapshot.history[0];
    assert_eq!(history.object, web_1());
    let cpu: Vec<f64> = history.cpu.samples.iter().map(|s| s.value).collect();
    assert!(
        (cpu[0] - 0.1).abs() < 1e-6 && (cpu[1] - 0.3).abs() < 1e-6,
        "{cpu:?}"
    );
    assert_eq!(history.memory.samples[1].value, 64.0 * 1024.0 * 1024.0);
    assert_eq!(history.cpu.labels["pod"], "web-1");
    assert!(metrics_requests(&api, SHOP_PODS) >= 3);
    assert_eq!(metrics_requests(&api, ALL_PODS), 0);
}

#[tokio::test]
async fn a_cluster_without_metrics_server_is_unavailable() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let (poller, mut rx) = start(&api, FAST);

    let _pods = poller.subscribe(UsageInterest::Latest(UsageScope::Pods(None)));
    let snapshot = next(&mut rx).await;
    assert!(
        matches!(
            snapshot.status[&UsageScope::Pods(None)],
            UsageStatus::Unavailable(_)
        ),
        "{snapshot:?}"
    );
    assert!(snapshot.latest.is_empty());
}

#[tokio::test]
async fn forbidden_node_metrics_are_reported() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.respond(
        NODES,
        403,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"nodes.metrics.k8s.io is forbidden","reason":"Forbidden","code":403}"#,
    );
    let (poller, mut rx) = start(&api, FAST);

    let _nodes = poller.subscribe(UsageInterest::Latest(UsageScope::Nodes));
    let snapshot = next(&mut rx).await;
    assert_eq!(
        snapshot.status[&UsageScope::Nodes],
        UsageStatus::Forbidden("nodes.metrics.k8s.io is forbidden".to_owned())
    );
}

#[tokio::test]
async fn stops_when_nobody_listens() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.respond(NODES, 200, &nodes(1_000, 250));
    let client = kube::Client::try_from(kube::Config::new(api.url().parse().unwrap())).unwrap();
    // A listener that is already gone.
    let poller = UsagePoller::start(client, ContextName::new("fake"), FAST, |_| false);

    let _nodes = poller.subscribe(UsageInterest::Latest(UsageScope::Nodes));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(metrics_requests(&api, NODES), 1);
}
