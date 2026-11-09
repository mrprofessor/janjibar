//! The watch hub: shared, counted watches per context, kind and scope.
//!
//! Watches are keyed by context, API resource and namespace, like the
//! server-side tables. Kinds with a typed spec in the `kinds` crate are
//! watched here; their rows use the same [`TableSnapshot`] as server-side
//! tables, so the UI draws both the same way. Kinds without a spec get a
//! view in issue #19.
//!
//! A table subscription or the inspector holds a reference to a watch. The
//! first reference starts it. When the last one goes, the watch keeps its
//! store warm for [`DEFAULT_LINGER`] in case the user comes back, then stops.
//! Each watch projects rows (and the inspected object's details) on tokio
//! and sends at most one snapshot per [`SNAPSHOT_WINDOW`], only on change.

use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use futures::stream::BoxStream;
use k8s_openapi::NamespaceResourceScope;
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet};
use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::api::core::v1::{
    ConfigMap, Event as KubeEvent, Namespace, Node, PersistentVolume, PersistentVolumeClaim, Pod,
    Secret, Service,
};
use k8s_openapi::api::networking::v1::Ingress;
use k8s_openapi::api::storage::v1::StorageClass;
use k8s_openapi::jiff::Timestamp;
use kube::config::Kubeconfig;
use kube::core::PartialObjectMeta;
use kube::runtime::reflector::{self, ObjectRef};
use kube::runtime::{WatchStreamExt as _, watcher};
use kube::{Api, Client, Resource, ResourceExt as _};
use model::resource::{Details, InspectTarget, Kind, ObjectKey};
use model::{
    Column, ConnectError, ConnectionState, ContextName, Event, GroupVersionResource,
    SubscriptionId, TableRow, TableSnapshot,
};
use serde::de::DeserializeOwned;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior};

use crate::client::ClientPool;
use crate::errors::connect_error_from_watch;
use crate::owners;
use crate::task::AbortOnDrop;

/// The UI gets at most one snapshot per window per watch (about 30 a second).
pub const SNAPSHOT_WINDOW: Duration = Duration::from_millis(33);

/// How long a watch stays warm after its last subscriber leaves.
pub const DEFAULT_LINGER: Duration = Duration::from_secs(45);

/// How often derived values (ages) are re-checked when nothing else changes.
const AGE_TICK: Duration = Duration::from_secs(1);

/// Rows from the API come in pages of this size, like client-go.
const PAGE_SIZE: u32 = 500;

/// What a watch is for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct WatchKey {
    pub context: ContextName,
    pub resource: GroupVersionResource,
    /// One namespace, or `None` for all (and for cluster-wide kinds).
    pub namespace: Option<Arc<str>>,
}

/// Messages from the hub to one watch task.
#[derive(Debug)]
enum Control {
    /// Start or stop sending table snapshots. Starting sends one at once.
    Tables(bool),
    /// Follow one object for the inspector, or none.
    Inspect(Option<ObjectKey>),
}

struct Entry {
    /// Table subscriptions using this watch.
    tables: usize,
    /// 1 when the inspector follows an object here.
    inspectors: usize,
    /// Bumped on every reference change, so a stale reap is ignored.
    generation: u64,
    control: mpsc::UnboundedSender<Control>,
    _task: AbortOnDrop<()>,
}

impl Entry {
    fn refs(&self) -> usize {
        self.tables + self.inspectors
    }

    fn send(&self, control: Control) {
        // The task only ends when the entry is dropped, so this cannot fail
        // while the entry exists; ignore the impossible error.
        let _ = self.control.send(control);
    }
}

/// A request to stop a watch that has been idle for the linger period.
pub(crate) type Reap = (WatchKey, u64);

/// Owns every watch. Lives inside the engine task; not shared.
pub(crate) struct Hub {
    events: mpsc::UnboundedSender<Event>,
    clients: ClientPool,
    kubeconfig: Arc<Kubeconfig>,
    linger: Duration,
    entries: HashMap<WatchKey, Entry>,
    subscriptions: HashMap<SubscriptionId, WatchKey>,
    inspecting: Option<(InspectTarget, WatchKey)>,
    owners: Option<AbortOnDrop<()>>,
    reaps: mpsc::UnboundedSender<Reap>,
}

impl Hub {
    pub(crate) fn new(
        events: mpsc::UnboundedSender<Event>,
        clients: ClientPool,
        linger: Duration,
    ) -> (Self, mpsc::UnboundedReceiver<Reap>) {
        let (reaps, reap_rx) = mpsc::unbounded_channel();
        let hub = Self {
            events,
            clients,
            kubeconfig: Arc::default(),
            linger,
            entries: HashMap::new(),
            subscriptions: HashMap::new(),
            inspecting: None,
            owners: None,
            reaps,
        };
        (hub, reap_rx)
    }

    /// New watches use this kubeconfig. Running watches keep their client.
    pub(crate) fn set_kubeconfig(&mut self, kubeconfig: Arc<Kubeconfig>) {
        self.kubeconfig = kubeconfig;
    }

    pub(crate) fn subscribe(&mut self, id: SubscriptionId, key: WatchKey) {
        if let Some(old) = self.subscriptions.remove(&id) {
            self.release(&old, true);
        }
        self.acquire(&key, true).send(Control::Tables(true));
        self.subscriptions.insert(id, key);
    }

    pub(crate) fn unsubscribe(&mut self, id: SubscriptionId) {
        if let Some(key) = self.subscriptions.remove(&id) {
            self.release(&key, true);
        }
    }

    pub(crate) fn inspect(&mut self, target: Option<InspectTarget>) {
        self.owners = None;
        if let Some((_, key)) = self.inspecting.take() {
            if let Some(entry) = self.entries.get(&key) {
                entry.send(Control::Inspect(None));
            }
            self.release(&key, false);
        }
        let Some(target) = target else {
            return;
        };
        let key = WatchKey {
            context: target.context.clone(),
            resource: target.kind.resource(),
            namespace: target.namespace.clone(),
        };
        self.acquire(&key, false)
            .send(Control::Inspect(Some(target.key.clone())));
        self.owners = Some(self.spawn_owners(target.clone()));
        self.inspecting = Some((target, key));
    }

    /// Stops a watch if it is still idle and nobody came back since.
    pub(crate) fn reap(&mut self, (key, generation): Reap) {
        let idle = self
            .entries
            .get(&key)
            .is_some_and(|e| e.refs() == 0 && e.generation == generation);
        if idle {
            tracing::debug!(context = %key.context, resource = %key.resource, "watch stopped after idling");
            self.entries.remove(&key);
        }
    }

    fn acquire(&mut self, key: &WatchKey, table: bool) -> &Entry {
        let entry = self.entries.entry(key.clone()).or_insert_with(|| {
            start_entry(key.clone(), &self.events, &self.clients, &self.kubeconfig)
        });
        entry.generation += 1;
        if table {
            entry.tables += 1;
        } else {
            entry.inspectors += 1;
        }
        entry
    }

    fn release(&mut self, key: &WatchKey, table: bool) {
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        entry.generation += 1;
        if table {
            entry.tables = entry.tables.saturating_sub(1);
            if entry.tables == 0 {
                entry.send(Control::Tables(false));
            }
        } else {
            entry.inspectors = entry.inspectors.saturating_sub(1);
        }
        if entry.refs() == 0 {
            let reaps = self.reaps.clone();
            let reap = (key.clone(), entry.generation);
            let linger = self.linger;
            tokio::spawn(async move {
                tokio::time::sleep(linger).await;
                // The engine may have stopped; then there is nothing to reap.
                let _ = reaps.send(reap);
            });
        }
    }

    fn spawn_owners(&self, target: InspectTarget) -> AbortOnDrop<()> {
        let events = self.events.clone();
        let clients = self.clients.clone();
        let kubeconfig = Arc::clone(&self.kubeconfig);
        AbortOnDrop(tokio::spawn(async move {
            let Ok(client) = clients.get(&target.context, &kubeconfig).await else {
                return;
            };
            let chain = owners::resolve(&client, target.kind, &target.key).await;
            // A send error means the UI is gone.
            let _ = events.send(Event::Owners { target, chain });
        }))
    }
}

fn start_entry(
    key: WatchKey,
    events: &mpsc::UnboundedSender<Event>,
    clients: &ClientPool,
    kubeconfig: &Arc<Kubeconfig>,
) -> Entry {
    tracing::debug!(context = %key.context, resource = %key.resource, namespace = ?key.namespace, "watch started");
    let (control, control_rx) = mpsc::unbounded_channel();
    let kind = Kind::from_resource(&key.resource);
    let ctx = WatchCtx {
        key,
        events: events.clone(),
        clients: clients.clone(),
        kubeconfig: Arc::clone(kubeconfig),
        control: control_rx,
    };
    Entry {
        tables: 0,
        inspectors: 0,
        generation: 0,
        control,
        _task: AbortOnDrop(spawn_watch(kind, ctx)),
    }
}

/// Everything a watch task needs.
struct WatchCtx {
    key: WatchKey,
    events: mpsc::UnboundedSender<Event>,
    clients: ClientPool,
    kubeconfig: Arc<Kubeconfig>,
    control: mpsc::UnboundedReceiver<Control>,
}

type WatchStream<K> = BoxStream<'static, Result<watcher::Event<K>, watcher::Error>>;

fn watcher_config() -> watcher::Config {
    watcher::Config::default().page_size(PAGE_SIZE)
}

/// A watch with managed fields stripped. For `PartialObjectMeta<K>` the
/// requests are metadata-only.
fn full<K>(api: Api<K>) -> WatchStream<K>
where
    K: Resource + Clone + DeserializeOwned + Debug + Send + 'static,
{
    watcher(api, watcher_config())
        .default_backoff()
        .modify(|obj| obj.managed_fields_mut().clear())
        .boxed()
}

fn namespaced<K>(client: Client, namespace: Option<&str>) -> Api<K>
where
    K: Resource<Scope = NamespaceResourceScope, DynamicType = ()>,
{
    match namespace {
        None => Api::all(client),
        Some(ns) => Api::namespaced(client, ns),
    }
}

fn spawn_watch(kind: Option<Kind>, ctx: WatchCtx) -> tokio::task::JoinHandle<()> {
    use kinds::{
        ConfigMaps, CronJobs, DaemonSets, Deployments, Events, Ingresses, Jobs, Namespaces, Nodes,
        PersistentVolumeClaims, PersistentVolumes, Pods, ReplicaSets, Secrets, Services,
        StatefulSets, StorageClasses,
    };
    let Some(kind) = kind else {
        return tokio::spawn(no_spec(ctx));
    };
    match kind {
        Kind::Pod => tokio::spawn(run::<Pods>(ctx, |c, s| full(namespaced::<Pod>(c, s)))),
        Kind::Deployment => tokio::spawn(run::<Deployments>(ctx, |c, s| {
            full(namespaced::<Deployment>(c, s))
        })),
        Kind::StatefulSet => tokio::spawn(run::<StatefulSets>(ctx, |c, s| {
            full(namespaced::<StatefulSet>(c, s))
        })),
        Kind::DaemonSet => tokio::spawn(run::<DaemonSets>(ctx, |c, s| {
            full(namespaced::<DaemonSet>(c, s))
        })),
        Kind::ReplicaSet => tokio::spawn(run::<ReplicaSets>(ctx, |c, s| {
            full(namespaced::<ReplicaSet>(c, s))
        })),
        Kind::Job => tokio::spawn(run::<Jobs>(ctx, |c, s| full(namespaced::<Job>(c, s)))),
        Kind::CronJob => tokio::spawn(run::<CronJobs>(ctx, |c, s| {
            full(namespaced::<CronJob>(c, s))
        })),
        Kind::Service => tokio::spawn(run::<Services>(ctx, |c, s| {
            full(namespaced::<Service>(c, s))
        })),
        Kind::Ingress => tokio::spawn(run::<Ingresses>(ctx, |c, s| {
            full(namespaced::<Ingress>(c, s))
        })),
        Kind::ConfigMap => tokio::spawn(run::<ConfigMaps>(ctx, |c, s| {
            full(namespaced::<ConfigMap>(c, s))
        })),
        // `Api<PartialObjectMeta<_>>` makes metadata-only requests, so
        // secret values never arrive.
        Kind::Secret => tokio::spawn(run::<Secrets>(ctx, |c, s| {
            full(namespaced::<PartialObjectMeta<Secret>>(c, s))
        })),
        Kind::Event => tokio::spawn(run::<Events>(ctx, |c, s| {
            full(namespaced::<KubeEvent>(c, s))
        })),
        Kind::PersistentVolumeClaim => tokio::spawn(run::<PersistentVolumeClaims>(ctx, |c, s| {
            full(namespaced::<PersistentVolumeClaim>(c, s))
        })),
        Kind::Node => tokio::spawn(run::<Nodes>(ctx, |c, _| full(Api::<Node>::all(c)))),
        Kind::Namespace => tokio::spawn(run::<Namespaces>(ctx, |c, _| {
            full(Api::<Namespace>::all(c))
        })),
        Kind::PersistentVolume => tokio::spawn(run::<PersistentVolumes>(ctx, |c, _| {
            full(Api::<PersistentVolume>::all(c))
        })),
        Kind::StorageClass => tokio::spawn(run::<StorageClasses>(ctx, |c, _| {
            full(Api::<StorageClass>::all(c))
        })),
    }
}

/// Answers subscriptions to a kind no typed spec covers yet. Server-side
/// tables will serve them (issue #19).
async fn no_spec(mut ctx: WatchCtx) {
    let key = ctx.key.clone();
    let msg = format!(
        "{} has no view yet; untyped kinds come with issue #19",
        key.resource
    );
    while let Some(control) = ctx.control.recv().await {
        if let Control::Tables(true) = control {
            let snapshot = TableSnapshot {
                context: key.context.clone(),
                resource: key.resource.clone(),
                namespace: key.namespace.clone(),
                state: ConnectionState::Failed(ConnectError::Other(msg.clone())),
                columns: Arc::from([]),
                detail_columns: Arc::from([]),
                rows: Arc::from([]),
            };
            if ctx.events.send(Event::Table(snapshot)).is_err() {
                return;
            }
        }
    }
}

/// Runs one watch until the hub drops it.
///
/// Snapshots follow the server-side table rules: the first one is
/// [`ConnectionState::Connecting`] with no rows; errors before the first
/// list are [`ConnectionState::Failed`]; later ones are
/// [`ConnectionState::Retrying`] and keep the rows. The watch keeps retrying
/// with backoff either way. Snapshots go out only while a table subscribes.
async fn run<S>(
    mut ctx: WatchCtx,
    make: impl FnOnce(Client, Option<&str>) -> WatchStream<S::Object>,
) where
    S: kinds::Spec,
    S::Object: Resource<DynamicType = ()> + Debug,
{
    let key = ctx.key.clone();
    let events = ctx.events.clone();
    let columns: Arc<[Column]> = S::columns().into();
    let table = |state: &ConnectionState, rows: &Arc<[TableRow]>| {
        let snapshot = TableSnapshot {
            context: key.context.clone(),
            resource: key.resource.clone(),
            namespace: key.namespace.clone(),
            state: state.clone(),
            columns: Arc::clone(&columns),
            detail_columns: Arc::from([]),
            rows: Arc::clone(rows),
        };
        events.send(Event::Table(snapshot)).is_ok()
    };
    let connection = |state: ConnectionState| {
        events
            .send(Event::Connection {
                context: key.context.clone(),
                state,
            })
            .is_ok()
    };

    let mut state = ConnectionState::Connecting;
    let mut rows: Arc<[TableRow]> = Arc::from([]);
    let mut tables = false;

    let client = match ctx.clients.get(&key.context, &ctx.kubeconfig).await {
        Ok(client) => client,
        Err(err) => {
            // Without a client there is nothing to watch. Answer every
            // subscriber with the failure until the hub drops us.
            state = ConnectionState::Failed(err);
            while let Some(control) = ctx.control.recv().await {
                if let Control::Tables(true) = control
                    && !table(&state, &rows)
                {
                    return;
                }
            }
            return;
        }
    };

    let (store, writer) = reflector::store::<S::Object>();
    let (changed_tx, mut changed) = watch::channel(0u64);
    let (error_tx, mut errors) = mpsc::unbounded_channel::<watcher::Error>();
    let stream = make(client, key.namespace.as_deref()).reflect(writer);
    // The driver owns the writer, so the store only updates while it runs.
    // It is aborted when this task ends.
    let _driver = AbortOnDrop(tokio::spawn(async move {
        let mut stream = std::pin::pin!(stream);
        while let Some(item) = stream.next().await {
            match item {
                Ok(
                    watcher::Event::InitDone | watcher::Event::Apply(_) | watcher::Event::Delete(_),
                ) => {
                    changed_tx.send_modify(|n| *n = n.wrapping_add(1));
                }
                Ok(_) => {}
                Err(err) => {
                    if error_tx.send(err).is_err() {
                        break;
                    }
                }
            }
        }
    }));

    let mut ages = tokio::time::interval(AGE_TICK);
    ages.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut inspect: Option<ObjectKey> = None;
    // `None` until details were sent for the current target.
    let mut last_details: Option<Option<Details>> = None;
    // Any error so far: when the watch then succeeds, the cluster is back.
    let mut saw_error = false;
    let mut last_sent: Option<Instant> = None;

    loop {
        // Every branch is cancel safe: watch `changed`, mpsc `recv`, `tick`.
        let mut force = false;
        tokio::select! {
            res = changed.changed() => {
                if res.is_err() {
                    let msg = "the watch ended unexpectedly".to_owned();
                    table(&ConnectionState::Failed(ConnectError::Other(msg)), &rows);
                    return;
                }
            }
            Some(err) = errors.recv() => {
                tracing::warn!(context = %key.context, resource = %key.resource, "watch error: {err}");
                saw_error = true;
                let mapped = connect_error_from_watch(&err);
                let ready = *changed.borrow() > 0;
                if ready {
                    state = ConnectionState::Retrying(err.to_string());
                    if matches!(mapped, ConnectError::Unreachable(_) | ConnectError::Auth(_)) {
                        connection(ConnectionState::Retrying(err.to_string()));
                    }
                } else {
                    state = ConnectionState::Failed(mapped);
                }
                if tables && !table(&state, &rows) {
                    return;
                }
                continue;
            }
            control = ctx.control.recv() => match control {
                None => return,
                Some(Control::Tables(on)) => {
                    tables = on;
                    if !on {
                        continue;
                    }
                    force = true;
                }
                Some(Control::Inspect(target)) => {
                    inspect = target;
                    last_details = None;
                    if inspect.is_none() {
                        continue;
                    }
                }
            },
            _ = ages.tick(), if *changed.borrow() > 0 && (tables || inspect.is_some()) => {}
        }

        if *changed.borrow() == 0 {
            // No list has completed yet. A new subscriber still hears where
            // things stand.
            if force && !table(&state, &rows) {
                return;
            }
            continue;
        }
        // Coalesce: wait out the rest of the window so a burst of changes
        // becomes one snapshot.
        if let Some(sent) = last_sent {
            let wait = SNAPSHOT_WINDOW.saturating_sub(sent.elapsed());
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }
        changed.borrow_and_update();
        last_sent = Some(Instant::now());

        if state != ConnectionState::Live {
            state = ConnectionState::Live;
            force = true;
            // The select probe reports the first connection; a watch only
            // speaks up when it recovers from an error.
            if saw_error {
                saw_error = false;
                connection(ConnectionState::Live);
            }
        }
        let now = Timestamp::now();
        if tables {
            let objects = store.state();
            let fresh: Arc<[TableRow]> =
                kinds::rows::<S>(objects.iter().map(AsRef::as_ref), now).into();
            if force || fresh != rows {
                rows = fresh;
                if !table(&state, &rows) {
                    return;
                }
            }
        }
        if let Some(target) = &inspect {
            let mut obj_ref = ObjectRef::<S::Object>::new(&target.name);
            if let Some(ns) = &target.namespace {
                obj_ref = obj_ref.within(ns);
            }
            let details = store.get(&obj_ref).map(|obj| S::details(&obj, now));
            if last_details.as_ref() != Some(&details) {
                last_details = Some(details.clone());
                let event = Event::Details {
                    target: InspectTarget {
                        context: key.context.clone(),
                        kind: S::KIND,
                        namespace: key.namespace.clone(),
                        key: target.clone(),
                    },
                    details: details.map(Box::new),
                };
                if events.send(event).is_err() {
                    return;
                }
            }
        }
    }
}
