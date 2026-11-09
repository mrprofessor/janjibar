//! A tiny fake Kubernetes API server for tests and local UI work.
//!
//! It speaks just enough HTTP/1.1 for kube-rs and the engine:
//!
//! - Discovery: `/api` and `/apis`, aggregated (`apidiscovery.k8s.io/v2`) or
//!   legacy, plus the legacy per-group resource lists.
//! - Pods (`/api/v1/pods`) and a custom resource, `crontabs` in
//!   `stable.example.com/v1`, across all namespaces or in one.
//! - Lists as plain JSON or, when asked with `as=Table`, as
//!   `meta.k8s.io/v1 Table` with printer columns. Pages honour `limit` and
//!   `continue`.
//! - Watches (`watch=true`) stream events with chunked encoding, replayed
//!   from the requested resourceVersion, as objects or as Tables.
//! - Other namespaced core objects, such as `secrets` and `configmaps`, added
//!   with [`FakeApi::add_object`], can be listed and fetched, with label
//!   selectors, paging and metadata-only responses.
//!
//! Every request is logged so tests can check what was asked for. Everything
//! else is a 404. There is no TLS and no auth.

#![allow(
    dead_code,
    reason = "shared by tests and the fake_cluster example; each uses a subset"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use k8s_openapi::api::core::v1::{
    Container, ContainerState, ContainerStateRunning, ContainerStatus, Pod, PodCondition, PodSpec,
    PodStatus,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
use k8s_openapi::jiff::{Timestamp, ToSpan as _};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

/// The custom resource group the fake serves.
pub const CRONTAB_GROUP: &str = "stable.example.com";

/// A running fake API server. Dropping it does not stop the listener task;
/// tests end with the runtime.
#[derive(Clone)]
pub struct FakeApi {
    addr: SocketAddr,
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    events: broadcast::Sender<Change>,
    /// Tells open watches to end with `410 Gone`.
    expire: broadcast::Sender<()>,
    open_watches: AtomicUsize,
    /// List requests served, Table or not.
    lists: AtomicUsize,
    aggregated_discovery: AtomicBool,
    /// Largest page the server returns, whatever the client asks. 0 = no cap.
    page_cap: AtomicUsize,
}

#[derive(Default)]
struct State {
    resource_version: u64,
    pods: BTreeMap<(String, String), Pod>,
    crontabs: BTreeMap<(String, String), CronTab>,
    /// Every event so far, so a watch can replay from its resourceVersion.
    history: Vec<Change>,
    /// Other objects as JSON, by plural, namespace and name.
    objects: BTreeMap<(String, String, String), Value>,
    /// Plurals that answer 403 Forbidden.
    forbidden: BTreeSet<String>,
    /// Every request so far, oldest first.
    requests: Vec<Request>,
}

/// The kinds the fake serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resource {
    Pods,
    CronTabs,
}

/// A stored custom resource.
#[derive(Clone)]
struct CronTab {
    namespace: String,
    name: String,
    spec: String,
    replicas: Option<i64>,
    image: Option<String>,
    created: Timestamp,
    resource_version: u64,
}

/// One recorded watch event, with everything needed to send it either as
/// an object or as a Table row.
#[derive(Clone)]
struct Change {
    rv: u64,
    resource: Resource,
    namespace: String,
    kind: &'static str,
    object: Value,
    cells: Value,
    meta: Value,
}

impl FakeApi {
    /// Starts a server on `addr` (use port 0 for any free port).
    pub async fn start(addr: &str) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let (events, _) = broadcast::channel(4096);
        let (expire, _) = broadcast::channel(16);
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            events,
            expire,
            open_watches: AtomicUsize::new(0),
            lists: AtomicUsize::new(0),
            aggregated_discovery: AtomicBool::new(true),
            page_cap: AtomicUsize::new(0),
        });
        let accept_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, Arc::clone(&accept_inner)));
            }
        });
        Ok(Self { addr, inner })
    }

    /// The server's base URL, for a kubeconfig or `kube::Config::new`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Watch connections currently open.
    pub fn open_watches(&self) -> usize {
        self.inner.open_watches.load(Ordering::SeqCst)
    }

    /// List requests served so far.
    pub fn list_requests(&self) -> usize {
        self.inner.lists.load(Ordering::SeqCst)
    }

    /// Ends every open watch with a `410 Gone` error event, as a server does
    /// when the watch's resourceVersion is too old.
    pub fn expire_watches(&self) {
        // No receivers just means no watch is open.
        let _ = self.inner.expire.send(());
    }

    /// Turns aggregated discovery off, to act like a server older than 1.26.
    pub fn set_aggregated_discovery(&self, on: bool) {
        self.inner.aggregated_discovery.store(on, Ordering::SeqCst);
    }

    /// Caps list pages at `cap` items, so clients must follow `continue`.
    pub fn set_page_cap(&self, cap: usize) {
        self.inner.page_cap.store(cap, Ordering::SeqCst);
    }

    /// Number of pods the server holds.
    pub fn pod_count(&self) -> usize {
        self.lock().pods.len()
    }

    /// Adds (or replaces) a running pod created `age` ago.
    pub fn add_pod(&self, namespace: &str, name: &str, age_seconds: i64) {
        let mut state = self.lock();
        let rv = next_rv(&mut state);
        let pod = running_pod(namespace, name, age_seconds, rv);
        state
            .pods
            .insert((namespace.to_owned(), name.to_owned()), pod.clone());
        self.record(&mut state, pod_change(rv, "ADDED", &pod));
    }

    /// Adds one restart to a pod's container, if the pod exists.
    pub fn restart_pod(&self, namespace: &str, name: &str) {
        let mut state = self.lock();
        let rv = next_rv(&mut state);
        let Some(pod) = state.pods.get_mut(&(namespace.to_owned(), name.to_owned())) else {
            return;
        };
        pod.metadata.resource_version = Some(rv.to_string());
        if let Some(status) = pod
            .status
            .as_mut()
            .and_then(|s| s.container_statuses.as_mut())
            .and_then(|c| c.first_mut())
        {
            status.restart_count += 1;
        }
        let change = pod_change(rv, "MODIFIED", pod);
        self.record(&mut state, change);
    }

    /// Deletes a pod if it exists.
    pub fn delete_pod(&self, namespace: &str, name: &str) {
        let mut state = self.lock();
        let Some(mut pod) = state.pods.remove(&(namespace.to_owned(), name.to_owned())) else {
            return;
        };
        let rv = next_rv(&mut state);
        pod.metadata.resource_version = Some(rv.to_string());
        self.record(&mut state, pod_change(rv, "DELETED", &pod));
    }

    /// Adds (or replaces) a `CronTab` created `age_seconds` ago.
    pub fn add_crontab(
        &self,
        namespace: &str,
        name: &str,
        spec: &str,
        replicas: Option<i64>,
        age_seconds: i64,
    ) {
        let mut state = self.lock();
        let rv = next_rv(&mut state);
        let crontab = CronTab {
            namespace: namespace.to_owned(),
            name: name.to_owned(),
            spec: spec.to_owned(),
            replicas,
            image: Some("busybox:1.36".to_owned()),
            created: Timestamp::now() - age_seconds.seconds(),
            resource_version: rv,
        };
        state
            .crontabs
            .insert((namespace.to_owned(), name.to_owned()), crontab.clone());
        self.record(&mut state, crontab_change(rv, "ADDED", &crontab));
    }

    /// Changes a `CronTab`'s replicas, if it exists.
    pub fn set_crontab_replicas(&self, namespace: &str, name: &str, replicas: i64) {
        let mut state = self.lock();
        let rv = next_rv(&mut state);
        let Some(crontab) = state
            .crontabs
            .get_mut(&(namespace.to_owned(), name.to_owned()))
        else {
            return;
        };
        crontab.replicas = Some(replicas);
        crontab.resource_version = rv;
        let change = crontab_change(rv, "MODIFIED", crontab);
        self.record(&mut state, change);
    }

    /// Deletes a `CronTab` if it exists.
    pub fn delete_crontab(&self, namespace: &str, name: &str) {
        let mut state = self.lock();
        let Some(mut crontab) = state
            .crontabs
            .remove(&(namespace.to_owned(), name.to_owned()))
        else {
            return;
        };
        let rv = next_rv(&mut state);
        crontab.resource_version = rv;
        self.record(&mut state, crontab_change(rv, "DELETED", &crontab));
    }

    /// Adds (or replaces) a namespaced core object, such as a Secret or
    /// `ConfigMap`. `plural` is its URL segment (`secrets`, `configmaps`).
    /// The namespace and name come from the object's metadata.
    pub fn add_object(&self, plural: &str, mut object: Value) {
        let mut state = self.lock();
        state.resource_version += 1;
        object["metadata"]["resourceVersion"] = state.resource_version.to_string().into();
        let field = |key: &str| {
            object["metadata"][key]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        };
        let key = (plural.to_owned(), field("namespace"), field("name"));
        state.objects.insert(key, object);
    }

    /// Makes every request for `plural` answer 403 Forbidden.
    pub fn forbid(&self, plural: &str) {
        self.lock().forbidden.insert(plural.to_owned());
    }

    /// Every request so far, oldest first.
    pub fn requests(&self) -> Vec<Request> {
        self.lock().requests.clone()
    }

    fn record(&self, state: &mut State, change: Change) {
        state.history.push(change.clone());
        // No receivers just means no watch is open yet.
        let _ = self.inner.events.send(change);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.lock()
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn next_rv(state: &mut State) -> u64 {
    state.resource_version += 1;
    state.resource_version
}

fn running_pod(namespace: &str, name: &str, age_seconds: i64, rv: u64) -> Pod {
    let created = Timestamp::now() - age_seconds.seconds();
    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            uid: Some(format!("uid-{namespace}-{name}")),
            resource_version: Some(rv.to_string()),
            creation_timestamp: Some(Time(created)),
            ..ObjectMeta::default()
        },
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "app".to_owned(),
                image: Some("example/app:1".to_owned()),
                ..Container::default()
            }],
            node_name: Some("fake-node-1".to_owned()),
            ..PodSpec::default()
        }),
        status: Some(PodStatus {
            phase: Some("Running".to_owned()),
            conditions: Some(vec![
                condition("Initialized"),
                condition("Ready"),
                condition("PodScheduled"),
            ]),
            container_statuses: Some(vec![ContainerStatus {
                name: "app".to_owned(),
                image: "example/app:1".to_owned(),
                image_id: String::new(),
                ready: true,
                restart_count: 0,
                started: Some(true),
                state: Some(ContainerState {
                    running: Some(ContainerStateRunning {
                        started_at: Some(Time(created)),
                    }),
                    ..ContainerState::default()
                }),
                ..ContainerStatus::default()
            }]),
            ..PodStatus::default()
        }),
    }
}

fn condition(kind: &str) -> PodCondition {
    PodCondition {
        type_: kind.to_owned(),
        status: "True".to_owned(),
        ..PodCondition::default()
    }
}

/// A rough age for Table cells. The engine recomputes ages from the
/// creation time, so this only has to look plausible.
fn age_cell(created: Option<Timestamp>) -> String {
    let Some(created) = created else {
        return "<unknown>".to_owned();
    };
    let secs = (Timestamp::now().as_second() - created.as_second()).max(0);
    match secs {
        0..120 => format!("{secs}s"),
        120..7200 => format!("{}m", secs / 60),
        7200..172_800 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

fn pod_change(rv: u64, kind: &'static str, pod: &Pod) -> Change {
    Change {
        rv,
        resource: Resource::Pods,
        namespace: pod.metadata.namespace.clone().unwrap_or_default(),
        kind,
        object: serde_json::to_value(pod).unwrap_or_default(),
        cells: pod_cells(pod),
        meta: serde_json::to_value(&pod.metadata).unwrap_or_default(),
    }
}

/// The cells `kubectl get pods` shows, simplified for always-running pods.
fn pod_cells(pod: &Pod) -> Value {
    let statuses = pod
        .status
        .as_ref()
        .and_then(|s| s.container_statuses.as_deref())
        .unwrap_or_default();
    let ready = statuses.iter().filter(|c| c.ready).count();
    let restarts: i32 = statuses.iter().map(|c| c.restart_count).sum();
    let phase = pod
        .status
        .as_ref()
        .and_then(|s| s.phase.clone())
        .unwrap_or_default();
    let node = pod
        .spec
        .as_ref()
        .and_then(|s| s.node_name.clone())
        .unwrap_or_else(|| "<none>".to_owned());
    json!([
        pod.metadata.name,
        format!("{ready}/{}", statuses.len()),
        phase,
        restarts.to_string(),
        age_cell(pod.metadata.creation_timestamp.as_ref().map(|t| t.0)),
        "<none>",
        node,
        "<none>",
        "<none>",
    ])
}

fn crontab_meta(c: &CronTab) -> Value {
    json!({
        "name": c.name,
        "namespace": c.namespace,
        "uid": format!("uid-{}-{}", c.namespace, c.name),
        "resourceVersion": c.resource_version.to_string(),
        "generation": 1,
        "creationTimestamp": c.created.to_string(),
    })
}

fn crontab_object(c: &CronTab) -> Value {
    json!({
        "apiVersion": format!("{CRONTAB_GROUP}/v1"),
        "kind": "CronTab",
        "metadata": crontab_meta(c),
        "spec": { "cronSpec": c.spec, "replicas": c.replicas, "image": c.image },
    })
}

/// Cells from the CRD's printer columns: Name, Spec, Replicas, Image, Age.
fn crontab_cells(c: &CronTab) -> Value {
    json!([
        c.name,
        c.spec,
        c.replicas,
        c.image,
        age_cell(Some(c.created))
    ])
}

fn crontab_change(rv: u64, kind: &'static str, c: &CronTab) -> Change {
    Change {
        rv,
        resource: Resource::CronTabs,
        namespace: c.namespace.clone(),
        kind,
        object: crontab_object(c),
        cells: crontab_cells(c),
        meta: crontab_meta(c),
    }
}

fn columns(resource: Resource) -> Value {
    let col = |name: &str, ty: &str, format: &str, priority: i32, description: &str| json!({ "name": name, "type": ty, "format": format, "description": description, "priority": priority });
    let name = col(
        "Name",
        "string",
        "name",
        0,
        "Name must be unique within a namespace.",
    );
    match resource {
        Resource::Pods => json!([
            name,
            col(
                "Ready",
                "string",
                "",
                0,
                "The aggregate readiness state of this pod for accepting traffic."
            ),
            col(
                "Status",
                "string",
                "",
                0,
                "The aggregate status of the containers in this pod."
            ),
            col(
                "Restarts",
                "string",
                "",
                0,
                "The number of times the containers in this pod have been restarted."
            ),
            col(
                "Age",
                "string",
                "",
                0,
                "CreationTimestamp is a timestamp representing the server time when this object was created."
            ),
            col("IP", "string", "", 1, "podIP address allocated to the pod."),
            col(
                "Node",
                "string",
                "",
                1,
                "NodeName indicates in which node this pod is scheduled."
            ),
            col(
                "Nominated Node",
                "string",
                "",
                1,
                "nominatedNodeName is set when this pod preempts other pods on the node."
            ),
            col(
                "Readiness Gates",
                "string",
                "",
                1,
                "If specified, all readiness gates will be evaluated for pod readiness."
            ),
        ]),
        Resource::CronTabs => json!([
            name,
            col(
                "Spec",
                "string",
                "",
                0,
                "The cron spec defining the interval a CronJob is run"
            ),
            col(
                "Replicas",
                "integer",
                "",
                0,
                "The number of jobs launched by the CronJob"
            ),
            col("Image", "string", "", 1, "The container image the job runs"),
            col("Age", "date", "", 0, ""),
        ]),
    }
}

/// One request the server received.
#[derive(Clone, Debug)]
pub struct Request {
    /// The path, such as `/api/v1/secrets`.
    pub path: String,
    /// The raw query string, still percent-encoded.
    pub query: String,
    /// The `Accept` header, empty if none.
    pub accept: String,
}

impl Request {
    fn parse(head: &str) -> Self {
        let target = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        Self {
            path: path.to_owned(),
            query: query.to_owned(),
            accept: header(head, "accept"),
        }
    }

    /// True when the client asked for metadata only.
    pub fn wants_metadata(&self) -> bool {
        self.accept.contains("as=PartialObjectMetadata")
    }

    /// A decoded query parameter.
    pub fn param(&self, key: &str) -> Option<String> {
        self.query
            .split('&')
            .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
            .map(percent_decode)
    }

    fn is_watch(&self) -> bool {
        matches!(self.param("watch").as_deref(), Some("true" | "1"))
    }

    fn wants_table(&self) -> bool {
        self.accept.contains("as=Table")
    }

    fn wants_aggregated(&self) -> bool {
        self.accept.contains("as=APIGroupDiscoveryList")
    }

    fn include_object(&self) -> bool {
        self.param("includeObject").as_deref() != Some("None")
    }
}

/// Where a request goes.
enum Route {
    Api,
    Apis,
    CoreV1,
    CronTabsV1,
    Collection {
        resource: Resource,
        namespace: Option<String>,
    },
    /// Stored objects, or a 404.
    Other,
}

fn route(path: &str) -> Route {
    let crontab_base = format!("/apis/{CRONTAB_GROUP}/v1");
    let parts: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    match parts.as_slice() {
        ["", "api"] => Route::Api,
        ["", "apis"] => Route::Apis,
        ["", "api", "v1"] => Route::CoreV1,
        ["", "api", "v1", "pods"] => Route::Collection {
            resource: Resource::Pods,
            namespace: None,
        },
        ["", "api", "v1", "namespaces", ns, "pods"] => Route::Collection {
            resource: Resource::Pods,
            namespace: Some((*ns).to_owned()),
        },
        _ if path == crontab_base => Route::CronTabsV1,
        _ => match path
            .strip_prefix(&crontab_base)
            .map(|rest| rest.trim_start_matches('/').split('/').collect::<Vec<_>>())
        {
            Some(rest) if rest == ["crontabs"] => Route::Collection {
                resource: Resource::CronTabs,
                namespace: None,
            },
            Some(rest) if rest.len() == 3 && rest[0] == "namespaces" && rest[2] == "crontabs" => {
                Route::Collection {
                    resource: Resource::CronTabs,
                    namespace: Some(rest[1].to_owned()),
                }
            }
            _ => Route::Other,
        },
    }
}

/// Serves one connection: any number of plain requests, or one watch.
async fn serve(mut stream: TcpStream, inner: Arc<Inner>) {
    let mut buf = Vec::new();
    loop {
        let Some(head) = read_head(&mut stream, &mut buf).await else {
            return;
        };
        let request = Request::parse(&head);
        inner.lock().requests.push(request.clone());
        let aggregated =
            request.wants_aggregated() && inner.aggregated_discovery.load(Ordering::SeqCst);
        let body = match route(&request.path) {
            Route::Api if aggregated => aggregated_core(),
            Route::Api => json!({
                "kind": "APIVersions",
                "versions": ["v1"],
                "serverAddressByClientCIDRs": [{ "clientCIDR": "0.0.0.0/0", "serverAddress": "127.0.0.1:6443" }],
            }),
            Route::Apis if aggregated => aggregated_groups(),
            Route::Apis => legacy_groups(),
            Route::CoreV1 => legacy_core_resources(),
            Route::CronTabsV1 => legacy_crontab_resources(),
            Route::Collection {
                resource,
                namespace,
            } => {
                if request.is_watch() {
                    let from = request
                        .param("resourceVersion")
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(0);
                    let watch = Watch {
                        resource,
                        namespace,
                        table: request.wants_table(),
                        include_object: request.include_object(),
                    };
                    serve_watch(stream, &inner, &watch, from).await;
                    return;
                }
                list(&inner, &request, resource, namespace.as_deref())
            }
            Route::Other => {
                let (status, body) = objects_response(&inner, &request);
                if write_response(&mut stream, status, &body).await.is_err() {
                    return;
                }
                continue;
            }
        };
        if write_response(&mut stream, "200 OK", &body.to_string())
            .await
            .is_err()
        {
            return;
        }
    }
}

fn list(inner: &Inner, request: &Request, resource: Resource, namespace: Option<&str>) -> Value {
    inner.lists.fetch_add(1, Ordering::SeqCst);
    let state = inner.lock();
    let in_namespace = |ns: &str| namespace.is_none_or(|want| want == ns);
    // (object, cells, meta) for every matching item, in name order.
    let all: Vec<(Value, Value, Value)> = match resource {
        Resource::Pods => state
            .pods
            .iter()
            .filter(|((ns, _), _)| in_namespace(ns))
            .map(|(_, pod)| {
                (
                    serde_json::to_value(pod).unwrap_or_default(),
                    pod_cells(pod),
                    serde_json::to_value(&pod.metadata).unwrap_or_default(),
                )
            })
            .collect(),
        Resource::CronTabs => state
            .crontabs
            .iter()
            .filter(|((ns, _), _)| in_namespace(ns))
            .map(|(_, c)| (crontab_object(c), crontab_cells(c), crontab_meta(c)))
            .collect(),
    };

    let start = request
        .param("continue")
        .and_then(|c| c.parse::<usize>().ok())
        .unwrap_or(0);
    let mut limit = request
        .param("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(usize::MAX);
    let cap = inner.page_cap.load(Ordering::SeqCst);
    if cap > 0 {
        limit = limit.min(cap);
    }
    let end = start.saturating_add(limit).min(all.len());
    let page = all.get(start..end).unwrap_or_default();
    let mut metadata = json!({ "resourceVersion": state.resource_version.to_string() });
    if end < all.len() {
        metadata["continue"] = json!(end.to_string());
    }

    if request.wants_table() {
        let rows: Vec<Value> = page
            .iter()
            .map(|(_, cells, meta)| table_row(cells, meta, request.include_object()))
            .collect();
        return json!({
            "kind": "Table",
            "apiVersion": "meta.k8s.io/v1",
            "metadata": metadata,
            "columnDefinitions": columns(resource),
            "rows": rows,
        });
    }
    let (kind, api_version) = match resource {
        Resource::Pods => ("PodList", "v1".to_owned()),
        Resource::CronTabs => ("CronTabList", format!("{CRONTAB_GROUP}/v1")),
    };
    json!({
        "kind": kind,
        "apiVersion": api_version,
        "metadata": metadata,
        "items": page.iter().map(|(object, _, _)| object).collect::<Vec<_>>(),
    })
}

fn table_row(cells: &Value, meta: &Value, include_object: bool) -> Value {
    if include_object {
        json!({
            "cells": cells,
            "object": { "kind": "PartialObjectMetadata", "apiVersion": "meta.k8s.io/v1", "metadata": meta },
        })
    } else {
        json!({ "cells": cells })
    }
}

/// What one watch connection streams.
struct Watch {
    resource: Resource,
    namespace: Option<String>,
    table: bool,
    include_object: bool,
}

impl Watch {
    fn wants(&self, change: &Change) -> bool {
        change.resource == self.resource
            && self
                .namespace
                .as_deref()
                .is_none_or(|ns| ns == change.namespace)
    }

    /// Renders an event. As a Table, only the first event carries the column
    /// definitions; later ones send `null`, like a real API server.
    fn line(&self, change: &Change, first: bool) -> String {
        let object = if self.table {
            json!({
                "kind": "Table",
                "apiVersion": "meta.k8s.io/v1",
                "metadata": { "resourceVersion": change.rv.to_string() },
                "columnDefinitions": if first { columns(self.resource) } else { Value::Null },
                "rows": [table_row(&change.cells, &change.meta, self.include_object)],
            })
        } else {
            change.object.clone()
        };
        json!({ "type": change.kind, "object": object }).to_string()
    }
}

async fn serve_watch(mut stream: TcpStream, inner: &Inner, watch: &Watch, from: u64) {
    // Subscribe before reading history so nothing falls in between.
    let mut live = inner.events.subscribe();
    let mut expire = inner.expire.subscribe();
    let replay: Vec<Change> = inner
        .lock()
        .history
        .iter()
        .filter(|c| c.rv > from && watch.wants(c))
        .cloned()
        .collect();
    // The connection closes when the watch ends, so say so; otherwise the
    // client may reuse it for its next request and fail.
    let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    inner.open_watches.fetch_add(1, Ordering::SeqCst);
    let _guard = WatchGuard(&inner.open_watches);
    let mut last = from;
    let mut first = true;
    for change in replay {
        if write_chunk(&mut stream, &watch.line(&change, first))
            .await
            .is_err()
        {
            return;
        }
        first = false;
        last = change.rv;
    }
    let mut probe = [0u8; 1];
    loop {
        tokio::select! {
            event = live.recv() => {
                let Ok(change) = event else { return };
                if change.rv <= last || !watch.wants(&change) {
                    continue;
                }
                last = change.rv;
                if write_chunk(&mut stream, &watch.line(&change, first)).await.is_err() {
                    return;
                }
                first = false;
            }
            _ = expire.recv() => {
                let gone = json!({
                    "type": "ERROR",
                    "object": {
                        "kind": "Status", "apiVersion": "v1", "metadata": {}, "status": "Failure",
                        "message": format!("too old resource version: {last}"),
                        "reason": "Expired", "code": 410,
                    },
                });
                // The watch ends either way; a write error changes nothing.
                let _ = write_chunk(&mut stream, &gone.to_string()).await;
                let _ = stream.write_all(b"0\r\n\r\n").await;
                return;
            }
            // A read of 0 bytes (or an error) means the client hung up.
            read = stream.read(&mut probe) => {
                if matches!(read, Ok(0) | Err(_)) {
                    return;
                }
            }
        }
    }
}

const ALL_VERBS: [&str; 8] = [
    "create",
    "delete",
    "deletecollection",
    "get",
    "list",
    "patch",
    "update",
    "watch",
];

fn aggregated_core() -> Value {
    json!({
        "kind": "APIGroupDiscoveryList",
        "apiVersion": "apidiscovery.k8s.io/v2",
        "metadata": {},
        "items": [{
            "metadata": { "creationTimestamp": null },
            "versions": [{
                "version": "v1",
                "freshness": "Current",
                "resources": [
                    {
                        "resource": "bindings",
                        "responseKind": { "group": "", "version": "", "kind": "Binding" },
                        "scope": "Namespaced",
                        "singularResource": "binding",
                        "verbs": ["create"],
                    },
                    {
                        "resource": "pods",
                        "responseKind": { "group": "", "version": "", "kind": "Pod" },
                        "scope": "Namespaced",
                        "singularResource": "pod",
                        "verbs": ALL_VERBS,
                        "shortNames": ["po"],
                        "categories": ["all"],
                        "subresources": [
                            { "subresource": "log", "responseKind": { "group": "", "version": "", "kind": "Pod" }, "verbs": ["get"] },
                            { "subresource": "status", "responseKind": { "group": "", "version": "", "kind": "Pod" }, "verbs": ["get", "patch", "update"] },
                        ],
                    },
                ],
            }],
        }],
    })
}

fn aggregated_groups() -> Value {
    json!({
        "kind": "APIGroupDiscoveryList",
        "apiVersion": "apidiscovery.k8s.io/v2",
        "metadata": {},
        "items": [{
            "metadata": { "name": CRONTAB_GROUP, "creationTimestamp": null },
            "versions": [{
                "version": "v1",
                "freshness": "Current",
                "resources": [{
                    "resource": "crontabs",
                    "responseKind": { "group": CRONTAB_GROUP, "version": "v1", "kind": "CronTab" },
                    "scope": "Namespaced",
                    "singularResource": "crontab",
                    "verbs": ALL_VERBS,
                    "shortNames": ["ct"],
                    "categories": ["all"],
                }],
            }],
        }],
    })
}

fn legacy_groups() -> Value {
    let version = json!({ "groupVersion": format!("{CRONTAB_GROUP}/v1"), "version": "v1" });
    json!({
        "kind": "APIGroupList",
        "apiVersion": "v1",
        "groups": [{ "name": CRONTAB_GROUP, "versions": [version], "preferredVersion": version }],
    })
}

fn legacy_core_resources() -> Value {
    json!({
        "kind": "APIResourceList",
        "groupVersion": "v1",
        "resources": [
            { "name": "bindings", "singularName": "binding", "namespaced": true, "kind": "Binding", "verbs": ["create"] },
            { "name": "pods", "singularName": "pod", "namespaced": true, "kind": "Pod", "verbs": ALL_VERBS, "shortNames": ["po"], "categories": ["all"] },
            { "name": "pods/log", "singularName": "", "namespaced": true, "kind": "Pod", "verbs": ["get"] },
        ],
    })
}

fn legacy_crontab_resources() -> Value {
    json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": format!("{CRONTAB_GROUP}/v1"),
        "resources": [
            { "name": "crontabs", "singularName": "crontab", "namespaced": true, "kind": "CronTab", "verbs": ALL_VERBS, "shortNames": ["ct"], "categories": ["all"] },
        ],
    })
}

struct WatchGuard<'a>(&'a AtomicUsize);

impl Drop for WatchGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Answers a list or get of a stored object.
fn objects_response(inner: &Inner, request: &Request) -> (&'static str, String) {
    let segments: Vec<&str> = request.path.split('/').skip(1).collect();
    let (namespace, plural, name) = match segments.as_slice() {
        ["api", "v1", plural] => (None, *plural, None),
        ["api", "v1", "namespaces", ns, plural] => (Some(*ns), *plural, None),
        ["api", "v1", "namespaces", ns, plural, name] => (Some(*ns), *plural, Some(*name)),
        _ => return failure(404, "NotFound"),
    };
    let state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
    if state.forbidden.contains(plural) {
        return failure(403, "Forbidden");
    }
    let shape = |object: &Value| {
        if request.wants_metadata() {
            serde_json::json!({
                "kind": "PartialObjectMetadata",
                "apiVersion": "meta.k8s.io/v1",
                "metadata": object["metadata"],
            })
        } else {
            object.clone()
        }
    };

    if let Some(name) = name {
        let key = (
            plural.to_owned(),
            namespace.unwrap_or_default().to_owned(),
            name.to_owned(),
        );
        return match state.objects.get(&key) {
            Some(object) => ("200 OK", shape(object).to_string()),
            None => failure(404, "NotFound"),
        };
    }

    let selector = request.param("labelSelector").unwrap_or_default();
    let matching: Vec<&Value> = state
        .objects
        .iter()
        .filter(|((p, ns, _), object)| {
            p == plural && namespace.is_none_or(|n| n == ns) && matches_labels(object, &selector)
        })
        .map(|(_, object)| object)
        .collect();
    let start = request
        .param("continue")
        .and_then(|c| c.parse::<usize>().ok())
        .unwrap_or(0)
        .min(matching.len());
    let end = request
        .param("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .map_or(matching.len(), |limit| (start + limit).min(matching.len()));
    let mut metadata = serde_json::json!({ "resourceVersion": state.resource_version.to_string() });
    if end < matching.len() {
        metadata["continue"] = end.to_string().into();
    }
    let (kind, api_version) = if request.wants_metadata() {
        ("PartialObjectMetadataList", "meta.k8s.io/v1")
    } else {
        ("List", "v1")
    };
    let body = serde_json::json!({
        "kind": kind,
        "apiVersion": api_version,
        "metadata": metadata,
        "items": matching[start..end].iter().map(|o| shape(o)).collect::<Vec<_>>(),
    });
    ("200 OK", body.to_string())
}

/// Equality-only label selectors, like `owner=helm,name=web`.
fn matches_labels(object: &Value, selector: &str) -> bool {
    selector.split(',').filter(|s| !s.is_empty()).all(|term| {
        let (key, value) = term
            .split_once("==")
            .or_else(|| term.split_once('='))
            .unwrap_or((term, ""));
        object["metadata"]["labels"][key].as_str() == Some(value)
    })
}

fn failure(code: u16, reason: &str) -> (&'static str, String) {
    let status = if code == 403 {
        "403 Forbidden"
    } else {
        "404 Not Found"
    };
    let body = serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "status": "Failure",
        "message": format!("fake api: {reason}"),
        "reason": reason,
        "code": code,
    });
    (status, body.to_string())
}

fn header(head: &str, name: &str) -> String {
    head.lines()
        .skip(1)
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_default()
}

fn percent_decode(text: &str) -> String {
    let mut out = Vec::with_capacity(text.len());
    let mut bytes = text.bytes();
    while let Some(b) = bytes.next() {
        match b {
            b'%' => {
                let hex: Vec<u8> = bytes.by_ref().take(2).collect();
                let decoded = std::str::from_utf8(&hex)
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match decoded {
                    Some(d) if hex.len() == 2 => out.push(d),
                    _ => {
                        out.push(b'%');
                        out.extend(hex);
                    }
                }
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reads one request head. Request bodies are not expected (GET only).
async fn read_head(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Option<String> {
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).into_owned();
            buf.drain(..end + 4);
            return Some(head);
        }
        let mut chunk = [0u8; 4096];
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

async fn write_response(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await
}

async fn write_chunk(stream: &mut TcpStream, line: &str) -> std::io::Result<()> {
    let payload = format!("{line}\n");
    let chunk = format!("{:x}\r\n{payload}\r\n", payload.len());
    stream.write_all(chunk.as_bytes()).await?;
    stream.flush().await
}
