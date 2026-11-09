//! Live tables for any kind, with columns from the server.
//!
//! [`watch_table`] lists and watches one kind in the server's Table format
//! and sends [`TableSnapshot`]s, batched the same way as the pod watch: at
//! most one per [`SNAPSHOT_WINDOW`], and only when something changed.

mod stream;
mod wire;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use k8s_openapi::jiff::Timestamp;
use kube::Client;
use model::{
    Column, ConnectError, ConnectionState, ContextName, Event, GroupVersionResource, Health,
    KindInfo, Scope, TableRow, TableSnapshot,
};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior};

use crate::age::human_age;
use crate::hub::SNAPSHOT_WINDOW;
use crate::task::AbortOnDrop;

/// How often ages are re-checked when nothing else changes.
const AGE_TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// What to list: one kind, across all namespaces or in one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableQuery {
    /// The kind's group, version and plural name.
    pub resource: GroupVersionResource,
    /// Namespaced or cluster-wide.
    pub scope: Scope,
    /// One namespace, or `None` for all. Always `None` for cluster-wide kinds.
    pub namespace: Option<Arc<str>>,
}

impl TableQuery {
    /// Lists `kind` across all namespaces, or cluster-wide.
    pub fn all(kind: &KindInfo) -> Self {
        Self {
            resource: kind.resource.clone(),
            scope: kind.scope,
            namespace: None,
        }
    }

    /// Lists `kind` in one namespace. Cluster-wide kinds ignore the namespace.
    pub fn in_namespace(kind: &KindInfo, namespace: impl Into<Arc<str>>) -> Self {
        let namespace = match kind.scope {
            Scope::Namespaced => Some(namespace.into()),
            Scope::Cluster => None,
        };
        Self {
            namespace,
            ..Self::all(kind)
        }
    }

    /// The collection's URL path, like `/apis/apps/v1/namespaces/shop/deployments`.
    pub fn path(&self) -> String {
        let r = &self.resource;
        let base = if r.group.is_empty() {
            format!("/api/{}", r.version)
        } else {
            format!("/apis/{}/{}", r.group, r.version)
        };
        match &self.namespace {
            Some(ns) => format!("{base}/namespaces/{ns}/{}", r.resource),
            None => format!("{base}/{}", r.resource),
        }
    }
}

/// One object as the server described it, before projection.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredRow {
    namespace: Option<Arc<str>>,
    name: Arc<str>,
    uid: Arc<str>,
    created: Option<Timestamp>,
    /// Every cell, in the server's column order.
    cells: Vec<Arc<str>>,
}

impl StoredRow {
    fn key(&self) -> RowKey {
        (self.namespace.clone(), Arc::clone(&self.name))
    }
}

/// Rows sort by namespace, then name.
type RowKey = (Option<Arc<str>>, Arc<str>);

/// The server's columns, split into the list and details views.
#[derive(Debug, Default)]
struct Layout {
    /// Every column, in the server's order. Kept to spot changes.
    all: Vec<Column>,
    /// Priority-0 columns.
    list: Arc<[Column]>,
    /// The rest.
    details: Arc<[Column]>,
    /// Indexes into `all` for `list`.
    list_cells: Vec<usize>,
    /// Indexes into `all` for `details`.
    detail_cells: Vec<usize>,
    /// The index of the age column, if there is one.
    age: Option<usize>,
}

impl Layout {
    fn new(all: Vec<Column>) -> Self {
        let (list_cells, detail_cells): (Vec<usize>, Vec<usize>) =
            (0..all.len()).partition(|&i| all.get(i).is_some_and(|c| c.priority == 0));
        let pick = |cells: &[usize]| -> Arc<[Column]> {
            cells.iter().filter_map(|&i| all.get(i).cloned()).collect()
        };
        Self {
            list: pick(&list_cells),
            details: pick(&detail_cells),
            age: all.iter().position(wire::is_creation_age),
            list_cells,
            detail_cells,
            all,
        }
    }
}

/// The rows of one table, kept current by [`stream::run`].
#[derive(Debug, Default)]
struct Store {
    layout: Arc<Layout>,
    rows: BTreeMap<RowKey, Arc<StoredRow>>,
}

impl Store {
    /// Replaces everything, after a list.
    fn replace(&mut self, columns: Vec<Column>, rows: Vec<StoredRow>) {
        self.set_columns(columns);
        self.rows = rows.into_iter().map(|r| (r.key(), Arc::new(r))).collect();
    }

    /// Adds or updates rows, after a watch event.
    fn apply(&mut self, columns: Vec<Column>, rows: Vec<StoredRow>) {
        self.set_columns(columns);
        for row in rows {
            self.rows.insert(row.key(), Arc::new(row));
        }
    }

    /// Removes rows, after a watch event.
    fn delete(&mut self, rows: &[StoredRow]) {
        for row in rows {
            self.rows.remove(&row.key());
        }
    }

    /// Takes new columns when the server sent some that differ. Watch events
    /// after the first carry none, which means "unchanged".
    fn set_columns(&mut self, columns: Vec<Column>) {
        if !columns.is_empty() && columns != self.layout.all {
            self.layout = Arc::new(Layout::new(columns));
        }
    }
}

fn lock(store: &Mutex<Store>) -> MutexGuard<'_, Store> {
    // Every update leaves the store whole, so a poisoned lock is still usable.
    store.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Projects stored rows for display: splits cells into list and details,
/// and keeps the age column current.
fn project(layout: &Layout, rows: &[Arc<StoredRow>], now: Timestamp) -> Vec<TableRow> {
    let empty: Arc<str> = Arc::from("");
    rows.iter()
        .map(|row| {
            let age: Option<Arc<str>> = layout
                .age
                .and(row.created)
                .map(|created| human_age(now.as_second() - created.as_second()).into());
            let cell = |i: &usize| -> Arc<str> {
                if Some(*i) == layout.age
                    && let Some(age) = &age
                {
                    return Arc::clone(age);
                }
                Arc::clone(row.cells.get(*i).unwrap_or(&empty))
            };
            TableRow {
                name: Arc::clone(&row.name),
                namespace: row.namespace.clone(),
                uid: Arc::clone(&row.uid),
                created: row.created.map(SystemTime::from),
                cells: layout.list_cells.iter().map(cell).collect(),
                details: layout.detail_cells.iter().map(cell).collect(),
                // The server does not say what a status means.
                health: Health::Unknown,
            }
        })
        .collect()
}

/// Lists and watches one kind as Tables and sends [`Event::Table`]
/// snapshots until the future is dropped or the receiver goes away.
///
/// The first snapshot is [`ConnectionState::Connecting`] with no rows. Errors
/// before the first list are [`ConnectionState::Failed`]; later ones are
/// [`ConnectionState::Retrying`] and keep the rows. Either way the watch keeps
/// retrying with backoff.
pub async fn watch_table(
    context: ContextName,
    client: Client,
    query: TableQuery,
    events: mpsc::UnboundedSender<Event>,
) {
    let store = Arc::new(Mutex::new(Store::default()));
    let (changed_tx, mut changed) = watch::channel(0u64);
    let (error_tx, mut errors) = mpsc::unbounded_channel();
    let _driver = AbortOnDrop(tokio::spawn(stream::run(
        client,
        query.clone(),
        Arc::clone(&store),
        changed_tx,
        error_tx,
    )));

    let mut ages = tokio::time::interval(AGE_TICK);
    ages.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut state = ConnectionState::Connecting;
    let mut shown: (Arc<Layout>, Arc<[TableRow]>) = (Arc::default(), Arc::from([]));
    let mut has_data = false;
    let mut last_sent: Option<Instant> = None;

    let send = |state: &ConnectionState, shown: &(Arc<Layout>, Arc<[TableRow]>)| {
        let snapshot = TableSnapshot {
            context: context.clone(),
            resource: query.resource.clone(),
            namespace: query.namespace.clone(),
            state: state.clone(),
            columns: Arc::clone(&shown.0.list),
            detail_columns: Arc::clone(&shown.0.details),
            rows: Arc::clone(&shown.1),
        };
        events.send(Event::Table(snapshot)).is_ok()
    };

    if !send(&state, &shown) {
        return;
    }
    loop {
        // All three branches are cancel safe: `watch::Receiver::changed`,
        // `mpsc::UnboundedReceiver::recv` and `Interval::tick`.
        let data_changed = tokio::select! {
            res = changed.changed() => {
                if res.is_err() {
                    let msg = "the watch ended unexpectedly".to_owned();
                    send(&ConnectionState::Failed(ConnectError::Other(msg)), &shown);
                    return;
                }
                true
            }
            Some(err) = errors.recv() => {
                tracing::warn!(%context, resource = %query.resource, "table watch error: {err}");
                state = if has_data {
                    ConnectionState::Retrying(err.to_string())
                } else {
                    ConnectionState::Failed(err.to_connect_error())
                };
                if !send(&state, &shown) {
                    return;
                }
                continue;
            }
            _ = ages.tick(), if has_data => false,
        };

        // Coalesce: wait out the rest of the window so a burst of changes
        // becomes one snapshot.
        if let Some(sent) = last_sent {
            let wait = SNAPSHOT_WINDOW.saturating_sub(sent.elapsed());
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }
        changed.borrow_and_update();

        let (layout, stored) = {
            let store = lock(&store);
            let rows: Vec<Arc<StoredRow>> = store.rows.values().cloned().collect();
            (Arc::clone(&store.layout), rows)
        };
        let rows: Arc<[TableRow]> = project(&layout, &stored, Timestamp::now()).into();
        last_sent = Some(Instant::now());

        // Only fresh data proves the watch is healthy again; an age tick
        // while retrying does not.
        let state_changed = data_changed && state != ConnectionState::Live;
        if data_changed {
            has_data = true;
            state = ConnectionState::Live;
        }
        let same_rows = Arc::ptr_eq(&layout, &shown.0) && shown.1 == rows;
        if same_rows && !state_changed {
            continue;
        }
        shown = (layout, rows);
        tracing::debug!(%context, resource = %query.resource, rows = shown.1.len(), "table snapshot sent");
        if !send(&state, &shown) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{ColumnType, Verbs};

    fn now() -> Timestamp {
        "2026-11-09T01:00:00Z".parse().unwrap()
    }

    fn kind(group: &str, resource: &str, scope: Scope) -> KindInfo {
        KindInfo {
            resource: GroupVersionResource {
                group: group.into(),
                version: "v1".into(),
                resource: resource.into(),
            },
            kind: "Thing".into(),
            singular: "".into(),
            scope,
            verbs: Verbs::default(),
            short_names: Arc::from([]),
            categories: Arc::from([]),
        }
    }

    fn recorded_pods() -> (Arc<Layout>, Vec<Arc<StoredRow>>) {
        let page = wire::parse_page(include_str!("../tests/fixtures/tables/pods.json")).unwrap();
        let mut store = Store::default();
        store.replace(page.columns, page.rows);
        let rows = store.rows.values().cloned().collect();
        (store.layout, rows)
    }

    #[test]
    fn paths_follow_the_api_layout() {
        let pods = kind("", "pods", Scope::Namespaced);
        assert_eq!(TableQuery::all(&pods).path(), "/api/v1/pods");
        assert_eq!(
            TableQuery::in_namespace(&pods, "faults").path(),
            "/api/v1/namespaces/faults/pods"
        );
        let crontabs = kind("stable.example.com", "crontabs", Scope::Namespaced);
        assert_eq!(
            TableQuery::in_namespace(&crontabs, "jobs").path(),
            "/apis/stable.example.com/v1/namespaces/jobs/crontabs"
        );
        let nodes = kind("", "nodes", Scope::Cluster);
        let query = TableQuery::in_namespace(&nodes, "faults");
        assert_eq!(query.namespace, None);
        assert_eq!(query.path(), "/api/v1/nodes");
    }

    #[test]
    fn only_priority_zero_columns_go_in_the_list() {
        let (layout, rows) = recorded_pods();
        let list: Vec<&str> = layout.list.iter().map(|c| &*c.name).collect();
        assert_eq!(list, ["Name", "Ready", "Status", "Restarts", "Age"]);
        let details: Vec<&str> = layout.details.iter().map(|c| &*c.name).collect();
        assert_eq!(details, ["IP", "Node", "Nominated Node", "Readiness Gates"]);

        let projected = project(&layout, &rows, now());
        let crashloop = &projected[0];
        let cells: Vec<&str> = crashloop.cells.iter().map(AsRef::as_ref).collect();
        // Created at 00:57:30, so the age is recomputed to 2m30s at 01:00.
        assert_eq!(
            cells,
            [
                "crashloop-54594bf7cc-tb6vb",
                "0/1",
                "CrashLoopBackOff",
                "3 (47s ago)",
                "2m30s"
            ]
        );
        let details: Vec<&str> = crashloop.details.iter().map(AsRef::as_ref).collect();
        assert_eq!(
            details,
            ["10.244.1.6", "platform-worker", "<none>", "<none>"]
        );
        assert_eq!(crashloop.namespace.as_deref(), Some("faults"));
        assert_eq!(&*crashloop.uid, "acecc5e1-68d3-43dc-9b1f-549831f338f5");
        assert!(crashloop.created.is_some());
    }

    #[test]
    fn rows_sort_by_namespace_then_name() {
        let mut store = Store::default();
        let row = |ns: Option<&str>, name: &str| StoredRow {
            namespace: ns.map(Into::into),
            name: name.into(),
            uid: "".into(),
            created: None,
            cells: vec![name.into()],
        };
        store.replace(
            wire::fallback_columns(),
            vec![
                row(Some("b"), "x"),
                row(Some("a"), "y"),
                row(Some("a"), "b"),
            ],
        );
        store.apply(Vec::new(), vec![row(Some("a"), "c")]);
        store.delete(&[row(Some("b"), "x")]);
        let names: Vec<String> = store
            .rows
            .values()
            .map(|r| format!("{}/{}", r.namespace.as_deref().unwrap_or(""), r.name))
            .collect();
        assert_eq!(names, ["a/b", "a/c", "a/y"]);
    }

    #[test]
    fn events_without_columns_keep_the_layout() {
        let (layout, _) = recorded_pods();
        let mut store = Store {
            layout: Arc::clone(&layout),
            rows: BTreeMap::new(),
        };
        store.apply(Vec::new(), Vec::new());
        assert!(Arc::ptr_eq(&store.layout, &layout));
        store.apply(layout.all.clone(), Vec::new());
        assert!(
            Arc::ptr_eq(&store.layout, &layout),
            "same columns keep the layout"
        );
        store.apply(wire::fallback_columns(), Vec::new());
        assert_eq!(store.layout.list.len(), 2);
    }

    #[test]
    fn short_rows_get_empty_cells() {
        let layout = Layout::new(vec![
            Column {
                name: "Name".into(),
                ty: ColumnType::String,
                format: "name".into(),
                priority: 0,
                description: "".into(),
            },
            Column {
                name: "Extra".into(),
                ty: ColumnType::String,
                format: "".into(),
                priority: 0,
                description: "".into(),
            },
        ]);
        let row = Arc::new(StoredRow {
            namespace: None,
            name: "n".into(),
            uid: "".into(),
            created: None,
            cells: vec!["n".into()],
        });
        let projected = project(&layout, &[row], now());
        let cells: Vec<&str> = projected[0].cells.iter().map(AsRef::as_ref).collect();
        assert_eq!(cells, ["n", ""]);
    }
}
