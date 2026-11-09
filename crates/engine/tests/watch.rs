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

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use engine::{EngineHandle, EngineOptions, KubeconfigSources};
use k8s_openapi::api::apps::v1::{
    Deployment, DeploymentSpec, DeploymentStatus, ReplicaSet, ReplicaSetSpec,
};
use k8s_openapi::api::core::v1::{Node, NodeCondition, NodeStatus, Pod, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use model::resource::{Details, InspectTarget, Kind, ObjectKey, OwnerLink};
use model::{
    Command, ConnectError, ConnectionState, ContextName, Event, GroupVersionResource, Health,
    SubscriptionId, TableSnapshot,
};
use support::fake_api::{CRONTAB_GROUP, FakeApi, collection, sample_pod};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::time::timeout;

const THREE_DAYS: i64 = 3 * 24 * 3600;
const WAIT: Duration = Duration::from_secs(10);

/// An engine whose kubeconfig has one context per fake server.
struct Harness {
    engine: EngineHandle,
    events: mpsc::UnboundedReceiver<Event>,
    dir: TempDir,
}

fn kubeconfig(servers: &[(&str, String)]) -> String {
    let mut yaml = String::from("apiVersion: v1\nkind: Config\nclusters:\n");
    for (name, url) in servers {
        writeln!(
            yaml,
            "  - name: {name}\n    cluster: {{ server: \"{url}\" }}"
        )
        .unwrap();
    }
    yaml.push_str("users:\n  - name: anon\n    user: {}\ncontexts:\n");
    for (name, _) in servers {
        writeln!(
            yaml,
            "  - name: {name}\n    context: {{ cluster: {name}, user: anon }}"
        )
        .unwrap();
    }
    yaml
}

fn harness(servers: &[(&str, &FakeApi)], linger: Duration) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config");
    let urls: Vec<(&str, String)> = servers.iter().map(|(n, a)| (*n, a.url())).collect();
    std::fs::write(&path, kubeconfig(&urls)).unwrap();
    let (engine, events) = engine::start_with(
        &tokio::runtime::Handle::current(),
        KubeconfigSources::new(vec![path]),
        EngineOptions {
            linger,
            ..EngineOptions::default()
        },
    );
    Harness {
        engine,
        events,
        dir,
    }
}

impl Harness {
    fn subscribe(&self, id: u64, context: &str, kind: Kind) {
        self.subscribe_to(id, context, kind.resource());
    }

    fn subscribe_to(&self, id: u64, context: &str, resource: GroupVersionResource) {
        assert!(self.engine.send(Command::Subscribe {
            id: SubscriptionId(id),
            context: context.into(),
            resource,
            namespace: None,
        }));
    }

    fn unsubscribe(&self, id: u64) {
        assert!(self.engine.send(Command::Unsubscribe(SubscriptionId(id))));
    }

    /// Waits for the next event that `pick` accepts, skipping others.
    async fn next<T>(&mut self, what: &str, mut pick: impl FnMut(Event) -> Option<T>) -> T {
        let events = &mut self.events;
        timeout(WAIT, async {
            loop {
                let event = events.recv().await.expect("engine stopped");
                if let Event::Table(TableSnapshot {
                    state: ConnectionState::Failed(err),
                    ..
                }) = &event
                {
                    panic!("watch failed: {err}");
                }
                if let Some(found) = pick(event) {
                    return found;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    /// Waits for the next live snapshot of `kind`, skipping `Connecting` ones.
    async fn table(&mut self, kind: Kind) -> TableSnapshot {
        self.next(&format!("{kind:?} rows"), |e| match e {
            Event::Table(t)
                if Kind::from_resource(&t.resource) == Some(kind)
                    && t.state == ConnectionState::Live =>
            {
                Some(t)
            }
            _ => None,
        })
        .await
    }

    async fn details(&mut self) -> Option<Box<Details>> {
        self.next("details", |e| match e {
            Event::Details { details, .. } => Some(details),
            _ => None,
        })
        .await
    }

    async fn connection_failure(&mut self) -> ConnectError {
        self.next("a connection failure", |e| match e {
            Event::Connection {
                state: ConnectionState::Failed(err),
                ..
            } => Some(err),
            _ => None,
        })
        .await
    }
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

fn names(snapshot: &TableSnapshot) -> Vec<String> {
    snapshot
        .rows
        .iter()
        .map(|r| match &r.namespace {
            Some(ns) => format!("{ns}/{}", r.name),
            None => r.name.to_string(),
        })
        .collect()
}

fn meta(namespace: &str, name: &str) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.into()),
        namespace: Some(namespace.into()),
        ..ObjectMeta::default()
    }
}

fn owned_by(kind: &str, name: &str) -> Vec<OwnerReference> {
    vec![OwnerReference {
        api_version: "apps/v1".into(),
        kind: kind.into(),
        name: name.into(),
        uid: format!("uid-{name}"),
        controller: Some(true),
        ..OwnerReference::default()
    }]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_initial_pods_then_live_changes() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_pod("shop", "web-1", THREE_DAYS);
    api.add_pod("shop", "web-0", THREE_DAYS);
    api.add_pod("kube-system", "dns-0", THREE_DAYS);
    let mut h = harness(&[("fake", &api)], Duration::from_secs(45));
    h.subscribe(1, "fake", Kind::Pod);

    let first = h.table(Kind::Pod).await;
    assert_eq!(first.context, ContextName::from("fake"));
    assert_eq!(
        names(&first),
        ["kube-system/dns-0", "shop/web-0", "shop/web-1"]
    );
    assert_eq!(&*first.columns[0].name, "Name");
    assert_eq!(&*first.rows[0].cells[2], "Running");
    assert_eq!(&*first.rows[0].cells[4], "3d");
    assert_eq!(first.rows[0].health, Health::Good);

    api.add_pod("shop", "web-2", 5);
    assert_eq!(h.table(Kind::Pod).await.rows.len(), 4);

    api.delete_pod("shop", "web-0");
    let deleted = h.table(Kind::Pod).await;
    assert_eq!(
        names(&deleted),
        ["kube-system/dns-0", "shop/web-1", "shop/web-2"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bursts_become_few_snapshots() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let mut h = harness(&[("fake", &api)], Duration::from_secs(45));
    h.subscribe(1, "fake", Kind::Pod);
    assert!(h.table(Kind::Pod).await.rows.is_empty());

    for i in 0..300 {
        api.add_pod("load", &format!("pod-{i:03}"), THREE_DAYS);
    }
    let mut snapshots = 0;
    let mut last = 0;
    while last < 300 {
        last = h.table(Kind::Pod).await.rows.len();
        snapshots += 1;
    }
    assert!(
        snapshots <= 10,
        "expected coalescing, got {snapshots} snapshots"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quiet_cluster_sends_no_snapshots() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_pod("shop", "web-0", THREE_DAYS);
    let mut h = harness(&[("fake", &api)], Duration::from_secs(45));
    h.subscribe(1, "fake", Kind::Pod);
    h.table(Kind::Pod).await;

    let events = &mut h.events;
    let quiet = timeout(Duration::from_millis(2500), async {
        loop {
            if let Some(Event::Table(_)) = events.recv().await {
                return;
            }
        }
    })
    .await;
    assert!(quiet.is_err(), "a snapshot was sent with nothing changed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribers_share_one_watch() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_pod("shop", "web-0", THREE_DAYS);
    let mut h = harness(&[("fake", &api)], Duration::from_secs(45));
    h.subscribe(1, "fake", Kind::Pod);
    h.table(Kind::Pod).await;
    wait_until("the first watch", || api.open_watches() == 1).await;

    // A second subscriber gets a snapshot at once, from the same watch.
    h.subscribe(2, "fake", Kind::Pod);
    assert_eq!(names(&h.table(Kind::Pod).await), ["shop/web-0"]);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(api.open_watches(), 1);
    assert_eq!(
        api.full_requests(&collection::<Pod>()),
        2,
        "one list and one watch for both subscribers"
    );

    h.unsubscribe(1);
    api.add_pod("shop", "web-1", THREE_DAYS);
    assert_eq!(
        h.table(Kind::Pod).await.rows.len(),
        2,
        "still live for id 2"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_watch_stays_warm_then_stops() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_pod("shop", "web-0", THREE_DAYS);
    let mut h = harness(&[("fake", &api)], Duration::from_millis(600));
    h.subscribe(1, "fake", Kind::Pod);
    h.table(Kind::Pod).await;
    wait_until("the watch", || api.open_watches() == 1).await;

    h.unsubscribe(1);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(api.open_watches(), 1, "warm during the linger period");

    // Coming back within the period reuses the warm watch: no new list.
    let requests_before = api.full_requests(&collection::<Pod>());
    h.subscribe(2, "fake", Kind::Pod);
    assert_eq!(names(&h.table(Kind::Pod).await), ["shop/web-0"]);
    assert_eq!(api.full_requests(&collection::<Pod>()), requests_before);

    // Leaving again lets it stop once the period is over.
    h.unsubscribe(2);
    wait_until("the watch to stop", || api.open_watches() == 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets_are_watched_by_metadata_only() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.put(&Secret {
        metadata: meta("shop", "db-password"),
        string_data: Some(BTreeMap::from([("password".into(), "hunter2".into())])),
        ..Secret::default()
    });
    let mut h = harness(&[("fake", &api)], Duration::from_secs(45));
    h.subscribe(1, "fake", Kind::Secret);
    assert_eq!(names(&h.table(Kind::Secret).await), ["shop/db-password"]);
    wait_until("the secret watch", || api.open_watches() == 1).await;

    let secrets = collection::<Secret>();
    assert_eq!(
        api.full_requests(&secrets),
        0,
        "never asked for secret bodies"
    );
    assert_eq!(
        api.metadata_requests(&secrets),
        2,
        "metadata list and watch"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_kinds_list_with_their_own_columns() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.put(&Deployment {
        metadata: meta("shop", "web"),
        spec: DeploymentSpec {
            replicas: Some(3),
            ..DeploymentSpec::default()
        },
        status: Some(DeploymentStatus {
            ready_replicas: Some(2),
            updated_replicas: Some(3),
            available_replicas: Some(2),
            ..DeploymentStatus::default()
        }),
    });
    api.put(&Node {
        metadata: ObjectMeta {
            name: Some("worker-1".into()),
            ..ObjectMeta::default()
        },
        status: Some(NodeStatus {
            conditions: Some(vec![NodeCondition {
                type_: "Ready".into(),
                status: "True".into(),
                ..NodeCondition::default()
            }]),
            ..NodeStatus::default()
        }),
        ..Node::default()
    });
    let mut h = harness(&[("fake", &api)], Duration::from_secs(45));

    h.subscribe(1, "fake", Kind::Deployment);
    let deployments = h.table(Kind::Deployment).await;
    let cells: Vec<&str> = deployments.rows[0].cells.iter().map(|c| &**c).collect();
    assert_eq!(&cells[..5], ["web", "2/3", "3", "2", "Progressing"]);
    assert_eq!(deployments.rows[0].namespace.as_deref(), Some("shop"));
    assert_eq!(deployments.rows[0].health, Health::Pending);

    h.subscribe(2, "fake", Kind::Node);
    let nodes = h.table(Kind::Node).await;
    assert_eq!(names(&nodes), ["worker-1"]);
    assert_eq!(&*nodes.rows[0].cells[1], "Ready");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_inspector_follows_an_object_and_its_owners() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.put(&Deployment {
        metadata: meta("shop", "web"),
        spec: DeploymentSpec::default(),
        status: None,
    });
    api.put(&ReplicaSet {
        metadata: ObjectMeta {
            owner_references: Some(owned_by("Deployment", "web")),
            ..meta("shop", "web-7d9f")
        },
        spec: ReplicaSetSpec::default(),
        status: None,
    });
    let mut pod = sample_pod("shop", "web-7d9f-x2k", THREE_DAYS);
    pod.metadata.owner_references = Some(owned_by("ReplicaSet", "web-7d9f"));
    api.put(&pod);

    let mut h = harness(&[("fake", &api)], Duration::from_secs(45));
    h.subscribe(1, "fake", Kind::Pod);
    h.table(Kind::Pod).await;

    let target = InspectTarget {
        context: "fake".into(),
        kind: Kind::Pod,
        namespace: None,
        key: ObjectKey::namespaced("shop", "web-7d9f-x2k"),
    };
    assert!(h.engine.send(Command::Inspect(Some(target.clone()))));
    // Details and owners come from separate tasks, in either order.
    let mut details = None;
    let mut chain = None;
    while details.is_none() || chain.is_none() {
        h.next("details and owners", |e| match e {
            Event::Details { details: d, .. } => {
                details = Some(d);
                Some(())
            }
            Event::Owners {
                target: t,
                chain: c,
            } if t == target => {
                chain = Some(c);
                Some(())
            }
            _ => None,
        })
        .await;
    }
    let details = details.unwrap().expect("the pod exists");
    assert_eq!(&*details.status, "Running");
    assert_eq!(details.containers.len(), 1);
    assert_eq!(&*details.owners[0].kind, "ReplicaSet");
    let link = |kind: &str, name: &str| OwnerLink {
        kind: kind.into(),
        name: name.into(),
    };
    assert_eq!(
        chain.unwrap(),
        [link("Deployment", "web"), link("ReplicaSet", "web-7d9f")]
    );

    // Changes to the object reach the inspector.
    let status = pod.status.as_mut().unwrap();
    status.container_statuses.as_mut().unwrap()[0].restart_count = 4;
    api.put(&pod);
    let updated = h.details().await.expect("still there");
    assert!(updated.badges.iter().any(|b| &**b == "4 restarts"));

    // Deleting it clears the inspector.
    api.delete_pod("shop", "web-7d9f-x2k");
    assert!(h.details().await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selecting_reports_the_connection() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let mut h = harness(&[("fake", &api)], Duration::from_secs(45));
    assert!(h.engine.send(Command::SelectContext("fake".into())));
    h.next("live", |e| match e {
        Event::Connection {
            state: ConnectionState::Live,
            ..
        } => Some(()),
        _ => None,
    })
    .await;
    // Discovery still runs on connect, after the probe.
    let kinds = h
        .next("the kinds", |e| match e {
            Event::Kinds(catalog) => Some(catalog),
            _ => None,
        })
        .await;
    assert!(kinds.find("pods").is_some());
    assert!(kinds.find("crontabs").is_some());

    assert!(h.engine.send(Command::SelectContext("nope".into())));
    assert_eq!(h.connection_failure().await, ConnectError::UnknownContext);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_server_is_reported() {
    // Bind and drop a listener to find a port nobody is listening on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let mut h = harness(&[("down", &api)], Duration::from_secs(45));
    let path = h.dir.path().join("config");
    std::fs::write(
        &path,
        kubeconfig(&[("down", format!("http://127.0.0.1:{port}"))]),
    )
    .unwrap();
    assert!(h.engine.send(Command::ReloadCatalog));
    assert!(h.engine.send(Command::SelectContext("down".into())));
    let err = h.connection_failure().await;
    assert!(matches!(err, ConnectError::Unreachable(_)), "got {err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kinds_without_a_spec_wait_for_issue_19() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_crontab("batch", "nightly", "0 2 * * *", Some(1), THREE_DAYS);
    let mut h = harness(&[("fake", &api)], Duration::from_secs(45));
    let crontabs = GroupVersionResource {
        group: CRONTAB_GROUP.into(),
        version: "v1".into(),
        resource: "crontabs".into(),
    };
    h.subscribe_to(1, "fake", crontabs.clone());
    let events = &mut h.events;
    let state = timeout(WAIT, async {
        loop {
            if let Some(Event::Table(t)) = events.recv().await
                && t.resource == crontabs
            {
                return t.state;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        matches!(&state, ConnectionState::Failed(ConnectError::Other(m)) if m.contains("#19")),
        "got {state:?}"
    );
    assert_eq!(api.open_watches(), 0, "no watch for kinds without a spec");
}
