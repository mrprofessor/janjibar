//! End-to-end engine tests against a fake API server. No cluster needed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

mod support {
    pub mod fake_api;
}

use std::time::Duration;

use engine::{KubeconfigSources, pods::watch_pods};
use model::{Command, ConnectionState, ContextName, Event, PodSnapshot};
use support::fake_api::FakeApi;
use tokio::sync::mpsc;
use tokio::time::timeout;

const THREE_DAYS: i64 = 3 * 24 * 3600;
const WAIT: Duration = Duration::from_secs(10);

fn client_for(api: &FakeApi) -> kube::Client {
    let config = kube::Config::new(api.url().parse().unwrap());
    kube::Client::try_from(config).unwrap()
}

/// Waits for the next pod snapshot, skipping other events.
async fn next_pods(events: &mut mpsc::UnboundedReceiver<Event>) -> PodSnapshot {
    timeout(WAIT, async {
        loop {
            match events.recv().await.expect("engine stopped") {
                Event::Pods(snapshot) => return snapshot,
                Event::Connection {
                    state: ConnectionState::Failed(err),
                    ..
                } => panic!("connection failed: {err}"),
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for pods")
}

async fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let waited = timeout(WAIT, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "timed out waiting for {what}");
}

fn names(snapshot: &PodSnapshot) -> Vec<String> {
    snapshot
        .rows
        .iter()
        .map(|r| format!("{}/{}", r.namespace, r.name))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_initial_pods_then_live_changes() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_pod("shop", "web-1", THREE_DAYS);
    api.add_pod("shop", "web-0", THREE_DAYS);
    api.add_pod("kube-system", "dns-0", THREE_DAYS);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(watch_pods("fake".into(), client_for(&api), tx));

    let first = next_pods(&mut rx).await;
    assert_eq!(first.context, ContextName::from("fake"));
    assert_eq!(
        names(&first),
        ["kube-system/dns-0", "shop/web-0", "shop/web-1"]
    );
    assert_eq!(&*first.rows[0].age, "3d");
    assert_eq!(&*first.rows[0].status, "Running");

    api.add_pod("shop", "web-2", 5);
    let added = next_pods(&mut rx).await;
    assert_eq!(added.rows.len(), 4);
    let new = added.rows.iter().find(|r| &*r.name == "web-2").unwrap();
    assert!(
        new.age.ends_with('s'),
        "young pods show seconds: {}",
        new.age
    );

    api.delete_pod("shop", "web-0");
    let deleted = next_pods(&mut rx).await;
    assert_eq!(
        names(&deleted),
        ["kube-system/dns-0", "shop/web-1", "shop/web-2"]
    );

    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bursts_become_few_snapshots() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(watch_pods("fake".into(), client_for(&api), tx));
    assert!(next_pods(&mut rx).await.rows.is_empty());

    for i in 0..300 {
        api.add_pod("load", &format!("pod-{i:03}"), THREE_DAYS);
    }

    let mut snapshots = 0;
    let mut last = 0;
    while last < 300 {
        last = next_pods(&mut rx).await.rows.len();
        snapshots += 1;
    }
    // 300 changes arrive within a few windows of 33 ms each.
    assert!(
        snapshots <= 10,
        "expected coalescing, got {snapshots} snapshots"
    );

    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quiet_cluster_sends_no_snapshots() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_pod("shop", "web-0", THREE_DAYS);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(watch_pods("fake".into(), client_for(&api), tx));
    next_pods(&mut rx).await;

    // Ages are re-checked every second, but "3d" does not change, so the
    // UI must not be woken.
    let quiet = timeout(Duration::from_millis(2500), async {
        loop {
            if let Some(Event::Pods(_)) = rx.recv().await {
                return;
            }
        }
    })
    .await;
    assert!(quiet.is_err(), "a snapshot was sent with nothing changed");

    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_context_stops_the_old_watch() {
    let a = FakeApi::start("127.0.0.1:0").await.unwrap();
    let b = FakeApi::start("127.0.0.1:0").await.unwrap();
    a.add_pod("default", "from-a", THREE_DAYS);
    b.add_pod("default", "from-b", THREE_DAYS);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config");
    std::fs::write(
        &path,
        format!(
            "apiVersion: v1
kind: Config
clusters:
  - name: a
    cluster: {{ server: \"{}\" }}
  - name: b
    cluster: {{ server: \"{}\" }}
users:
  - name: anon
    user: {{}}
contexts:
  - name: a
    context: {{ cluster: a, user: anon }}
  - name: b
    context: {{ cluster: b, user: anon }}
",
            a.url(),
            b.url()
        ),
    )
    .unwrap();

    let (engine, mut events) = engine::start(
        &tokio::runtime::Handle::current(),
        KubeconfigSources::new(vec![path]),
    );
    match timeout(WAIT, events.recv()).await.unwrap() {
        Some(Event::Catalog(catalog)) => assert_eq!(catalog.contexts.len(), 2),
        other => panic!("expected the catalog first, got {other:?}"),
    }

    assert!(engine.send(Command::SelectContext("a".into())));
    let snap = next_pods(&mut events).await;
    assert_eq!(names(&snap), ["default/from-a"]);
    // The watch request follows the initial list, so it may lag the snapshot.
    wait_until("context a to open a watch", || a.open_watches() == 1).await;

    assert!(engine.send(Command::SelectContext("b".into())));
    let snap = next_pods(&mut events).await;
    assert_eq!(names(&snap), ["default/from-b"]);

    // The old watch connection must close once its task is dropped.
    wait_until("context a to close its watch", || a.open_watches() == 0).await;
    wait_until("context b to open a watch", || b.open_watches() == 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_context_fails_cleanly() {
    let (engine, mut events) = engine::start(
        &tokio::runtime::Handle::current(),
        KubeconfigSources::new(vec![]),
    );
    assert!(matches!(
        timeout(WAIT, events.recv()).await.unwrap(),
        Some(Event::Catalog(c)) if c.is_empty()
    ));
    assert!(engine.send(Command::SelectContext("nope".into())));
    match timeout(WAIT, events.recv()).await.unwrap() {
        Some(Event::Connection {
            state: ConnectionState::Failed(model::ConnectError::UnknownContext),
            ..
        }) => {}
        other => panic!("expected UnknownContext, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_server_is_reported() {
    // Bind and drop a listener to find a port nobody is listening on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let config = kube::Config::new(format!("http://127.0.0.1:{port}").parse().unwrap());
    let client = kube::Client::try_from(config).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(watch_pods("down".into(), client, tx));
    let state = timeout(WAIT, async {
        loop {
            if let Some(Event::Connection { state, .. }) = rx.recv().await {
                return state;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        matches!(
            state,
            ConnectionState::Failed(model::ConnectError::Unreachable(_))
        ),
        "got {state:?}"
    );
    task.abort();
}
