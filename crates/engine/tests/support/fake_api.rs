//! A tiny fake Kubernetes API server for tests and local UI work.
//!
//! It speaks just enough HTTP/1.1 for kube-rs to list and watch pods:
//! `GET /api/v1/pods` returns a `PodList`, and the same path with
//! `watch=true` streams watch events with chunked encoding. Everything else
//! is a 404. There is no TLS and no auth.

#![allow(
    dead_code,
    reason = "shared by tests and the fake_cluster example; each uses a subset"
)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use k8s_openapi::api::core::v1::{
    Container, ContainerState, ContainerStateRunning, ContainerStatus, Pod, PodCondition, PodSpec,
    PodStatus,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
use k8s_openapi::jiff::{Timestamp, ToSpan as _};
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
        if path != "/api/v1/pods" {
            let body = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}"#;
            if write_response(&mut stream, "404 Not Found", body)
                .await
                .is_err()
            {
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
