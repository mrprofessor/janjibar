//! Discovery and server-side tables against a fake API server. No cluster needed.

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

use engine::KubeconfigSources;
use engine::discovery::discover;
use engine::tables::{TableQuery, watch_table};
use model::{
    Command, ConnectError, ConnectionState, Event, GroupVersionResource, KindCatalog, KindInfo,
    Scope, TableSnapshot, Verbs,
};
use support::fake_api::FakeApi;
use tokio::sync::mpsc;
use tokio::time::timeout;

const THREE_DAYS: i64 = 3 * 24 * 3600;
const WAIT: Duration = Duration::from_secs(10);

fn client_for(api: &FakeApi) -> kube::Client {
    let config = kube::Config::new(api.url().parse().unwrap());
    kube::Client::try_from(config).unwrap()
}

async fn kinds(api: &FakeApi) -> KindCatalog {
    discover(&"fake".into(), &client_for(api)).await.unwrap()
}

/// Waits for the next table snapshot, skipping other events.
async fn next_table(events: &mut mpsc::UnboundedReceiver<Event>) -> TableSnapshot {
    timeout(WAIT, async {
        loop {
            if let Event::Table(snapshot) = events.recv().await.expect("watch stopped") {
                return snapshot;
            }
        }
    })
    .await
    .expect("timed out waiting for a table snapshot")
}

/// Waits for a live snapshot, failing on a failed one.
async fn next_live(events: &mut mpsc::UnboundedReceiver<Event>) -> TableSnapshot {
    loop {
        let snapshot = next_table(events).await;
        match &snapshot.state {
            ConnectionState::Live => return snapshot,
            ConnectionState::Failed(err) => panic!("table watch failed: {err}"),
            _ => {}
        }
    }
}

/// Starts a table watch for the `fake` context.
fn start(
    api: &FakeApi,
    query: TableQuery,
) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedReceiver<Event>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(watch_table("fake".into(), client_for(api), query, tx));
    (task, rx)
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
        .map(|r| format!("{}/{}", r.namespace.as_deref().unwrap_or("-"), r.name))
        .collect()
}

fn column_names(columns: &[model::Column]) -> Vec<&str> {
    columns.iter().map(|c| &*c.name).collect()
}

fn cells(snapshot: &TableSnapshot, name: &str) -> Vec<String> {
    let row = snapshot
        .rows
        .iter()
        .find(|r| &*r.name == name)
        .unwrap_or_else(|| panic!("no row {name}"));
    row.cells.iter().map(ToString::to_string).collect()
}

fn assert_discovered(catalog: &KindCatalog) {
    let groups: Vec<&str> = catalog.groups.iter().map(|g| &*g.name).collect();
    assert_eq!(groups, ["", "stable.example.com"]);
    // Bindings can only be created; pods/log is a subresource.
    let core: Vec<&str> = catalog.groups[0].kinds.iter().map(|k| &*k.kind).collect();
    assert_eq!(core, ["Pod"]);

    let pods = catalog.find("po").unwrap();
    assert_eq!(pods.resource.api_version(), "v1");
    assert_eq!(pods.scope, Scope::Namespaced);
    assert!(pods.verbs.contains(model::Verb::Watch));

    let crontabs = catalog.find("ct").unwrap();
    assert_eq!(&*crontabs.kind, "CronTab");
    assert_eq!(crontabs.resource.api_version(), "stable.example.com/v1");
    assert_eq!(&*crontabs.categories, [std::sync::Arc::from("all")]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aggregated_discovery_finds_pods_and_custom_resources() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let catalog = kinds(&api).await;
    assert_eq!(catalog.context, "fake".into());
    assert_discovered(&catalog);
    assert_eq!(catalog.problems.len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_falls_back_to_legacy_endpoints() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.set_aggregated_discovery(false);
    assert_discovered(&kinds(&api).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pod_rows_appear_and_update_through_the_generic_path() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_pod("shop", "web-1", THREE_DAYS);
    api.add_pod("shop", "web-0", THREE_DAYS);
    api.add_pod("kube-system", "dns-0", THREE_DAYS);
    let catalog = kinds(&api).await;
    let (task, mut rx) = start(&api, TableQuery::all(catalog.find("pods").unwrap()));

    let connecting = next_table(&mut rx).await;
    assert_eq!(connecting.state, ConnectionState::Connecting);
    assert_eq!(connecting.rows.len(), 0);

    let first = next_live(&mut rx).await;
    assert_eq!(first.context, "fake".into());
    assert_eq!(&*first.resource.resource, "pods");
    assert_eq!(first.namespace, None);
    assert_eq!(
        column_names(&first.columns),
        ["Name", "Ready", "Status", "Restarts", "Age"]
    );
    assert_eq!(
        column_names(&first.detail_columns),
        ["IP", "Node", "Nominated Node", "Readiness Gates"]
    );
    assert_eq!(
        names(&first),
        ["kube-system/dns-0", "shop/web-0", "shop/web-1"]
    );
    assert_eq!(
        cells(&first, "web-0"),
        ["web-0", "1/1", "Running", "0", "3d"]
    );
    let web0 = first.rows.iter().find(|r| &*r.name == "web-0").unwrap();
    assert_eq!(&*web0.uid, "uid-shop-web-0");
    assert_eq!(&*web0.details[1], "fake-node-1");

    api.add_pod("shop", "web-2", 5);
    let added = next_live(&mut rx).await;
    assert_eq!(added.rows.len(), 3 + 1);
    let age = &cells(&added, "web-2")[4];
    assert!(age.ends_with('s'), "young pods show seconds: {age}");

    // Later watch events carry no column definitions; the columns stay.
    api.restart_pod("shop", "web-1");
    let restarted = next_live(&mut rx).await;
    assert_eq!(cells(&restarted, "web-1")[3], "1");
    assert_eq!(restarted.columns.len(), 5);

    api.delete_pod("shop", "web-0");
    let deleted = next_live(&mut rx).await;
    assert_eq!(
        names(&deleted),
        ["kube-system/dns-0", "shop/web-1", "shop/web-2"]
    );

    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_resources_get_their_printer_columns() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_crontab("batch-jobs", "nightly", "0 2 * * *", Some(1), 5 * 24 * 3600);
    api.add_crontab(
        "batch-jobs",
        "every-minute",
        "* * * * */1",
        None,
        THREE_DAYS,
    );
    api.add_crontab("other", "elsewhere", "@hourly", Some(2), THREE_DAYS);
    let catalog = kinds(&api).await;
    let crontabs = catalog.find("crontabs.stable.example.com").unwrap();
    let (task, mut rx) = start(&api, TableQuery::in_namespace(crontabs, "batch-jobs"));

    let first = next_live(&mut rx).await;
    assert_eq!(first.namespace.as_deref(), Some("batch-jobs"));
    assert_eq!(
        column_names(&first.columns),
        ["Name", "Spec", "Replicas", "Age"]
    );
    assert_eq!(column_names(&first.detail_columns), ["Image"]);
    assert_eq!(
        names(&first),
        ["batch-jobs/every-minute", "batch-jobs/nightly"]
    );
    assert_eq!(
        cells(&first, "nightly"),
        ["nightly", "0 2 * * *", "1", "5d"]
    );
    // An unset printer column prints like kubectl.
    assert_eq!(cells(&first, "every-minute")[2], "<none>");

    // Changes in another namespace are not part of this table.
    api.add_crontab("other", "noise", "@daily", Some(1), 10);
    api.set_crontab_replicas("batch-jobs", "every-minute", 3);
    let updated = next_live(&mut rx).await;
    assert_eq!(cells(&updated, "every-minute")[2], "3");
    assert_eq!(updated.rows.len(), 2);

    api.delete_crontab("batch-jobs", "nightly");
    let deleted = next_live(&mut rx).await;
    assert_eq!(names(&deleted), ["batch-jobs/every-minute"]);

    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_follow_continue_tokens() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.set_page_cap(2);
    for i in 0..5 {
        api.add_pod("paged", &format!("pod-{i}"), THREE_DAYS);
    }
    let catalog = kinds(&api).await;
    let (task, mut rx) = start(&api, TableQuery::all(catalog.find("po").unwrap()));
    let first = next_live(&mut rx).await;
    assert_eq!(first.rows.len(), 5);
    assert_eq!(first.columns.len(), 5);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_watch_lists_again_and_keeps_going() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_pod("shop", "web-0", THREE_DAYS);
    let catalog = kinds(&api).await;
    let (task, mut rx) = start(&api, TableQuery::all(catalog.find("po").unwrap()));
    assert_eq!(names(&next_live(&mut rx).await), ["shop/web-0"]);
    wait_until("the table watch to open", || api.open_watches() == 1).await;
    let lists = api.list_requests();

    api.expire_watches();
    wait_until("a fresh list", || api.list_requests() > lists).await;
    wait_until("the watch to reopen", || api.open_watches() == 1).await;

    api.add_pod("shop", "web-1", THREE_DAYS);
    let snapshot = loop {
        let snapshot = next_table(&mut rx).await;
        // A 410 is routine: no error state may reach the UI on the way.
        assert_eq!(snapshot.state, ConnectionState::Live);
        if snapshot.rows.len() == 2 {
            break snapshot;
        }
    };
    assert_eq!(names(&snapshot), ["shop/web-0", "shop/web-1"]);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bursts_become_few_table_snapshots() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let catalog = kinds(&api).await;
    let (task, mut rx) = start(&api, TableQuery::all(catalog.find("ct").unwrap()));
    assert_eq!(next_live(&mut rx).await.rows.len(), 0);
    wait_until("the table watch to open", || api.open_watches() == 1).await;

    for i in 0..300 {
        api.add_crontab(
            "load",
            &format!("ct-{i:03}"),
            "@hourly",
            Some(1),
            THREE_DAYS,
        );
    }

    let mut snapshots = 0;
    let mut last = 0;
    while last < 300 {
        last = next_live(&mut rx).await.rows.len();
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
async fn a_quiet_table_sends_no_snapshots() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_pod("shop", "web-0", THREE_DAYS);
    let catalog = kinds(&api).await;
    let (task, mut rx) = start(&api, TableQuery::all(catalog.find("po").unwrap()));
    next_live(&mut rx).await;

    // Ages are re-checked every second, but "3d" does not change, so the
    // UI must not be woken.
    let quiet = timeout(Duration::from_millis(2500), async {
        loop {
            if let Some(Event::Table(_)) = rx.recv().await {
                return;
            }
        }
    })
    .await;
    assert!(quiet.is_err(), "a snapshot was sent with nothing changed");
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kind_the_server_does_not_serve_fails_cleanly() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let missing = KindInfo {
        resource: GroupVersionResource {
            group: "example.com".into(),
            version: "v1".into(),
            resource: "widgets".into(),
        },
        kind: "Widget".into(),
        singular: "widget".into(),
        scope: Scope::Namespaced,
        verbs: Verbs::default(),
        short_names: std::sync::Arc::from([]),
        categories: std::sync::Arc::from([]),
    };
    let (task, mut rx) = start(&api, TableQuery::all(&missing));
    let state = loop {
        let snapshot = next_table(&mut rx).await;
        if let ConnectionState::Failed(err) = snapshot.state {
            break err;
        }
    };
    assert!(
        matches!(&state, ConnectError::Other(msg) if msg.starts_with("404")),
        "got {state:?}"
    );
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_engine_sends_kinds_after_connecting() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let dir = tempfile::tempdir().unwrap();
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

    let (engine, mut events) = engine::start(
        &tokio::runtime::Handle::current(),
        KubeconfigSources::new(vec![path]),
    );
    assert!(engine.send(Command::SelectContext("fake".into())));
    let catalog = timeout(WAIT, async {
        loop {
            if let Event::Kinds(catalog) = events.recv().await.expect("engine stopped") {
                return catalog;
            }
        }
    })
    .await
    .expect("timed out waiting for kinds");
    assert_eq!(catalog.context, "fake".into());
    assert_discovered(&catalog);
}
