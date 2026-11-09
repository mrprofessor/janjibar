//! The Helm releases feed end to end, against the fake API server.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

mod support {
    pub mod fake_api;
}

use std::io::Write as _;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use engine::{EngineHandle, KubeconfigSources};
use flate2::Compression;
use flate2::write::GzEncoder;
use model::{
    Command, ContextName, Event, HelmChartInfo, HelmCommand, HelmEvent, HelmReleaseKey,
    HelmSnapshot, HelmStatus, HelmStorage,
};
use serde_json::{Value, json};
use support::fake_api::FakeApi;
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

/// Helm's text for a payload: base64 of gzip of JSON.
fn helm_text(namespace: &str, name: &str, revision: u32, status: &str) -> String {
    let release = json!({
        "name": name,
        "namespace": namespace,
        "version": revision,
        "info": { "status": status, "description": format!("revision {revision}") },
        "chart": { "metadata": { "name": format!("{name}-chart"), "version": format!("1.{revision}.0"), "appVersion": "2.0" } },
        "config": { "replicas": revision },
        "manifest": "kind: Service\n",
    });
    let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
    gz.write_all(release.to_string().as_bytes()).unwrap();
    STANDARD.encode(gz.finish().unwrap())
}

fn meta(namespace: &str, name: &str, revision: u32, status: &str) -> Value {
    json!({
        "namespace": namespace,
        "name": format!("sh.helm.release.v1.{name}.v{revision}"),
        "labels": {
            "owner": "helm",
            "name": name,
            "version": revision.to_string(),
            "status": status,
            "modifiedAt": "1794182400",
        },
    })
}

fn add_secret(api: &FakeApi, namespace: &str, name: &str, revision: u32, status: &str) {
    let text = helm_text(namespace, name, revision, status);
    api.add_object(
        "secrets",
        json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "type": "helm.sh/release.v1",
            "metadata": meta(namespace, name, revision, status),
            "data": { "release": STANDARD.encode(text) },
        }),
    );
}

fn add_config_map(api: &FakeApi, namespace: &str, name: &str, revision: u32, status: &str) {
    api.add_object(
        "configmaps",
        json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": meta(namespace, name, revision, status),
            "data": { "release": helm_text(namespace, name, revision, status) },
        }),
    );
}

/// Starts an engine whose only context, `fake`, points at `api`, connects
/// to it and opens the Helm view.
fn open_helm(
    api: &FakeApi,
    dir: &tempfile::TempDir,
) -> (EngineHandle, mpsc::UnboundedReceiver<Event>) {
    let path = dir.path().join("config");
    std::fs::write(
        &path,
        format!(
            "apiVersion: v1
kind: Config
clusters:
  - name: fake
    cluster: {{ server: \"{}\" }}
users:
  - name: anon
    user: {{}}
contexts:
  - name: fake
    context: {{ cluster: fake, user: anon }}
",
            api.url()
        ),
    )
    .unwrap();
    let (engine, events) = engine::start(
        &tokio::runtime::Handle::current(),
        KubeconfigSources::new(vec![path]),
    );
    assert!(engine.send(Command::SelectContext("fake".into())));
    assert!(engine.send(Command::Helm(HelmCommand::Open)));
    (engine, events)
}

/// Waits for a Helm snapshot that passes `done`, skipping others.
async fn helm_until(
    events: &mut mpsc::UnboundedReceiver<Event>,
    what: &str,
    done: impl Fn(&HelmSnapshot) -> bool,
) -> HelmSnapshot {
    let found = timeout(WAIT, async {
        loop {
            if let Event::Helm(HelmEvent::Releases(snapshot)) =
                events.recv().await.expect("engine stopped")
                && done(&snapshot)
            {
                return snapshot;
            }
        }
    })
    .await;
    found.unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

fn charts_known(snapshot: &HelmSnapshot) -> bool {
    snapshot.loaded
        && snapshot
            .rows
            .iter()
            .all(|r| matches!(r.chart, HelmChartInfo::Known(_)))
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

fn key(namespace: &str, name: &str, storage: HelmStorage) -> HelmReleaseKey {
    HelmReleaseKey {
        namespace: namespace.into(),
        name: name.into(),
        storage,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn releases_load_from_labels_and_charts_fill_in() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    add_secret(&api, "shop", "web", 1, "superseded");
    add_secret(&api, "shop", "web", 2, "deployed");
    add_secret(&api, "data", "pg", 1, "deployed");
    add_config_map(&api, "ingress", "edge", 3, "failed");
    let dir = tempfile::tempdir().unwrap();
    let (_engine, mut events) = open_helm(&api, &dir);

    let snapshot = helm_until(&mut events, "decoded charts", charts_known).await;
    assert_eq!(snapshot.context, ContextName::new("fake"));
    assert_eq!(snapshot.problem, None);
    let rows: Vec<_> = snapshot
        .rows
        .iter()
        .map(|r| {
            let HelmChartInfo::Known(chart) = &r.chart else {
                unreachable!()
            };
            (
                r.key.clone(),
                r.revision,
                r.status,
                chart.to_string(),
                r.history.iter().map(|h| h.revision).collect::<Vec<_>>(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (
                key("data", "pg", HelmStorage::Secret),
                1,
                HelmStatus::Deployed,
                "pg-chart-1.1.0".to_owned(),
                vec![1]
            ),
            (
                key("ingress", "edge", HelmStorage::ConfigMap),
                3,
                HelmStatus::Failed,
                "edge-chart-1.3.0".to_owned(),
                vec![3]
            ),
            (
                key("shop", "web", HelmStorage::Secret),
                2,
                HelmStatus::Deployed,
                "web-chart-1.2.0".to_owned(),
                vec![2, 1]
            ),
        ]
    );

    // Lists and watches asked for metadata only. Payloads were fetched for
    // the latest revisions alone.
    let requests = api.requests();
    let helm_requests: Vec<_> = requests
        .iter()
        .filter(|r| r.path.contains("secrets") || r.path.contains("configmaps"))
        .collect();
    for r in &helm_requests {
        let is_get = r.path.contains("/sh.helm.release.v1.");
        assert_eq!(!is_get, r.wants_metadata(), "{r:?}");
        if !is_get {
            assert_eq!(r.param("labelSelector").as_deref(), Some("owner=helm"));
        }
    }
    let mut gets: Vec<&str> = helm_requests
        .iter()
        .map(|r| r.path.as_str())
        .filter(|p| p.contains("/sh.helm.release.v1."))
        .collect();
    gets.sort_unstable();
    assert_eq!(
        gets,
        [
            "/api/v1/namespaces/data/secrets/sh.helm.release.v1.pg.v1",
            "/api/v1/namespaces/ingress/configmaps/sh.helm.release.v1.edge.v3",
            "/api/v1/namespaces/shop/secrets/sh.helm.release.v1.web.v2",
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upgrade_shows_up_live() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    add_secret(&api, "shop", "web", 1, "deployed");
    let dir = tempfile::tempdir().unwrap();
    let (_engine, mut events) = open_helm(&api, &dir);
    helm_until(&mut events, "first list", charts_known).await;
    // Pods, Helm Secrets and Helm ConfigMaps.
    wait_until("three watches", || api.open_watches() == 3).await;

    add_secret(&api, "shop", "web", 2, "pending-upgrade");
    add_secret(&api, "shop", "web", 1, "superseded");
    let upgraded = helm_until(&mut events, "revision 2", |s| {
        s.rows.first().is_some_and(|r| r.revision == 2)
    })
    .await;
    assert_eq!(upgraded.rows[0].status, HelmStatus::PendingUpgrade);
    let history: Vec<_> = upgraded.rows[0]
        .history
        .iter()
        .map(|h| (h.revision, h.status))
        .collect();
    assert_eq!(
        history,
        [(2, HelmStatus::PendingUpgrade), (1, HelmStatus::Superseded)]
    );

    // The new latest revision is decoded too.
    let decoded = helm_until(&mut events, "chart of revision 2", charts_known).await;
    let HelmChartInfo::Known(chart) = &decoded.rows[0].chart else {
        unreachable!()
    };
    assert_eq!(&*chart.version, "1.2.0");

    // Uninstalling without history deletes every record.
    api.delete_object("secrets", "shop", "sh.helm.release.v1.web.v1");
    api.delete_object("secrets", "shop", "sh.helm.release.v1.web.v2");
    helm_until(&mut events, "no releases", |s| s.rows.is_empty()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn details_are_decoded_on_request() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    add_secret(&api, "shop", "web", 1, "superseded");
    add_secret(&api, "shop", "web", 2, "deployed");
    let dir = tempfile::tempdir().unwrap();
    let (engine, mut events) = open_helm(&api, &dir);
    helm_until(&mut events, "first list", charts_known).await;

    let web = key("shop", "web", HelmStorage::Secret);
    assert!(engine.send(Command::Helm(HelmCommand::LoadDetails {
        context: "fake".into(),
        release: web.clone(),
        revision: 1,
    })));
    let reply = timeout(WAIT, async {
        loop {
            if let Event::Helm(HelmEvent::Details {
                context,
                release,
                revision,
                result,
            }) = events.recv().await.expect("engine stopped")
            {
                return (context, release, revision, result);
            }
        }
    })
    .await
    .expect("details arrive");
    assert_eq!(reply.0, ContextName::new("fake"));
    assert_eq!(reply.1, web);
    assert_eq!(reply.2, 1);
    let details = reply.3.expect("revision 1 decodes");
    assert_eq!(details.revision, 1);
    assert_eq!(&*details.description, "revision 1");
    assert_eq!(&*details.values, "replicas: 1\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_stops_the_watch() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    add_secret(&api, "shop", "web", 1, "deployed");
    let dir = tempfile::tempdir().unwrap();
    let (engine, mut events) = open_helm(&api, &dir);
    helm_until(&mut events, "first list", |s| s.loaded).await;
    wait_until("pod and Helm watches", || api.open_watches() == 3).await;

    assert!(engine.send(Command::Helm(HelmCommand::Close)));
    wait_until("only the pod watch", || api.open_watches() == 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forbidden_secrets_are_reported_and_config_maps_are_optional() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    add_config_map(&api, "ingress", "edge", 1, "deployed");
    api.forbid("secrets");
    let dir = tempfile::tempdir().unwrap();
    let (_engine, mut events) = open_helm(&api, &dir);
    let failed = helm_until(&mut events, "a problem", |s| s.problem.is_some()).await;
    assert!(!failed.loaded);
    assert_eq!(
        failed.problem.as_deref(),
        Some("not allowed to watch Secrets across all namespaces")
    );

    // The other way round, releases in Secrets still load.
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    add_secret(&api, "shop", "web", 1, "deployed");
    api.forbid("configmaps");
    let dir = tempfile::tempdir().unwrap();
    let (_engine, mut events) = open_helm(&api, &dir);
    let loaded = helm_until(&mut events, "Secret releases", |s| s.loaded).await;
    assert_eq!(loaded.problem, None);
    assert_eq!(loaded.rows.len(), 1);
}
