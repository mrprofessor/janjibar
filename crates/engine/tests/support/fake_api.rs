//! A tiny fake Kubernetes API server for tests and local UI work.
//!
//! It speaks just enough HTTP/1.1 for kube-rs to list and watch pods:
//! `GET /api/v1/pods` returns a `PodList`, and the same path with
//! `watch=true` streams watch events with chunked encoding.
//!
//! Other namespaced core objects, such as `secrets` and `configmaps`, added
//! with [`FakeApi::add_object`] can be listed and fetched, with label selectors,
//! paging and metadata-only responses. Every request is logged so tests can
//! check what was asked for. Everything else is a 404. There is no TLS and
//! no auth.

#![allow(
    dead_code,
    reason = "shared by tests and the fake_cluster example; each uses a subset"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use k8s_openapi::api::core::v1::{
    Container, ContainerState, ContainerStateRunning, ContainerStatus, Pod, PodCondition, PodSpec,
    PodStatus,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
use k8s_openapi::jiff::{Timestamp, ToSpan as _};
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

/// A running fake API server. Dropping it does not stop the listener task;
/// tests end with the runtime.
#[derive(Clone)]
pub struct FakeApi {
    addr: SocketAddr,
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    events: broadcast::Sender<(u64, String)>,
    open_watches: AtomicUsize,
}

#[derive(Default)]
struct State {
    resource_version: u64,
    pods: BTreeMap<(String, String), Pod>,
    /// Every event so far, so a watch can replay from its resourceVersion.
    history: Vec<(u64, String)>,
    /// Other objects as JSON, by plural, namespace and name.
    objects: BTreeMap<(String, String, String), Value>,
    /// Plurals that answer 403 Forbidden.
    forbidden: BTreeSet<String>,
    /// Every request so far, oldest first.
    requests: Vec<Request>,
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
}

impl FakeApi {
    /// Starts a server on `addr` (use port 0 for any free port).
    pub async fn start(addr: &str) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let (events, _) = broadcast::channel(4096);
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            events,
            open_watches: AtomicUsize::new(0),
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

    /// Number of pods the server holds.
    pub fn pod_count(&self) -> usize {
        self.lock().pods.len()
    }

    /// Adds (or replaces) a running pod created `age` ago.
    pub fn add_pod(&self, namespace: &str, name: &str, age_seconds: i64) {
        let mut state = self.lock();
        state.resource_version += 1;
        let rv = state.resource_version;
        let pod = running_pod(namespace, name, age_seconds, rv);
        state
            .pods
            .insert((namespace.to_owned(), name.to_owned()), pod.clone());
        self.record(&mut state, rv, "ADDED", &pod);
    }

    /// Deletes a pod if it exists.
    pub fn delete_pod(&self, namespace: &str, name: &str) {
        let mut state = self.lock();
        let Some(mut pod) = state.pods.remove(&(namespace.to_owned(), name.to_owned())) else {
            return;
        };
        state.resource_version += 1;
        let rv = state.resource_version;
        pod.metadata.resource_version = Some(rv.to_string());
        self.record(&mut state, rv, "DELETED", &pod);
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

    fn record(&self, state: &mut State, rv: u64, kind: &str, pod: &Pod) {
        let line = serde_json::json!({ "type": kind, "object": pod }).to_string();
        state.history.push((rv, line.clone()));
        // No receivers just means no watch is open yet.
        let _ = self.inner.events.send((rv, line));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
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

/// Serves one connection: any number of plain requests, or one watch.
async fn serve(mut stream: TcpStream, inner: Arc<Inner>) {
    let mut buf = Vec::new();
    loop {
        let Some(head) = read_head(&mut stream, &mut buf).await else {
            return;
        };
        let target = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_owned();
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let request = Request {
            path: path.to_owned(),
            query: query.to_owned(),
            accept: header(&head, "accept"),
        };
        inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .requests
            .push(request.clone());
        if path != "/api/v1/pods" {
            let (status, body) = objects_response(&inner, &request);
            if write_response(&mut stream, status, &body).await.is_err() {
                return;
            }
            continue;
        }
        if query
            .split('&')
            .any(|kv| kv == "watch=true" || kv == "watch=1")
        {
            let from = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("resourceVersion="))
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            watch(stream, &inner, from).await;
            return;
        }
        let body = {
            let state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
            serde_json::json!({
                "kind": "PodList",
                "apiVersion": "v1",
                "metadata": { "resourceVersion": state.resource_version.to_string() },
                "items": state.pods.values().collect::<Vec<_>>(),
            })
            .to_string()
        };
        if write_response(&mut stream, "200 OK", &body).await.is_err() {
            return;
        }
    }
}

async fn watch(mut stream: TcpStream, inner: &Inner, from: u64) {
    // Subscribe before reading history so nothing falls in between.
    let mut live = inner.events.subscribe();
    let replay: Vec<(u64, String)> = {
        let state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .history
            .iter()
            .filter(|(rv, _)| *rv > from)
            .cloned()
            .collect()
    };
    let head =
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n";
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    inner.open_watches.fetch_add(1, Ordering::SeqCst);
    let _guard = WatchGuard(&inner.open_watches);
    let mut last = from;
    for (rv, line) in replay {
        if write_chunk(&mut stream, &line).await.is_err() {
            return;
        }
        last = rv;
    }
    let mut probe = [0u8; 1];
    loop {
        tokio::select! {
            event = live.recv() => {
                let Ok((rv, line)) = event else { return };
                if rv <= last {
                    continue;
                }
                last = rv;
                if write_chunk(&mut stream, &line).await.is_err() {
                    return;
                }
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
