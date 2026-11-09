//! A fake API server for port-forward tests.
//!
//! It serves just what a forward needs: get a pod, get a service, list pods
//! by label selector, and the pod `portforward` WebSocket with the
//! `v4.channel.k8s.io` protocol. Behind the WebSocket, each pod "listens" on
//! the ports it declares and answers every message with
//! `<pod>:<port>:<message>`, so a test can tell which pod got a connection.
//! A port the pod does not declare is refused through the error channel,
//! the way a real kubelet reports it. There is no TLS and no auth.

#![allow(dead_code, reason = "each test uses a subset of the fake")]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures::{SinkExt as _, StreamExt as _};
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, Pod, PodCondition, PodSpec, PodStatus, Service, ServicePort,
    ServiceSpec,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::{Bytes, Message};

/// A running fake. Dropping it does not stop the listener; tests end with
/// the runtime.
#[derive(Clone)]
pub struct FakeForwardApi {
    addr: SocketAddr,
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    /// Port-forward connections opened so far.
    opened: AtomicUsize,
    /// Port-forward connections open right now.
    open: AtomicUsize,
}

#[derive(Default)]
struct State {
    pods: BTreeMap<(String, String), Pod>,
    services: BTreeMap<(String, String), Service>,
}

impl FakeForwardApi {
    /// Starts a server on any free port.
    pub async fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            opened: AtomicUsize::new(0),
            open: AtomicUsize::new(0),
        });
        let accept_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, Arc::clone(&accept_inner)));
            }
        });
        Ok(Self { addr, inner })
    }

    /// The server's base URL.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Port-forward connections opened so far.
    pub fn tunnels_opened(&self) -> usize {
        self.inner.opened.load(Ordering::SeqCst)
    }

    /// Port-forward connections open right now.
    pub fn tunnels_open(&self) -> usize {
        self.inner.open.load(Ordering::SeqCst)
    }

    /// Adds (or replaces) a running pod with `labels` that listens on
    /// `ports`, each a name and a number.
    pub fn add_pod(
        &self,
        namespace: &str,
        name: &str,
        labels: &[(&str, &str)],
        ready: bool,
        ports: &[(&str, u16)],
    ) {
        let pod = Pod {
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                namespace: Some(namespace.to_owned()),
                labels: Some(
                    labels
                        .iter()
                        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                        .collect(),
                ),
                ..ObjectMeta::default()
            },
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: "app".to_owned(),
                    ports: Some(
                        ports
                            .iter()
                            .map(|(port_name, port)| ContainerPort {
                                name: Some((*port_name).to_owned()),
                                container_port: i32::from(*port),
                                ..ContainerPort::default()
                            })
                            .collect(),
                    ),
                    ..Container::default()
                }],
                ..PodSpec::default()
            }),
            status: Some(PodStatus {
                phase: Some("Running".to_owned()),
                conditions: Some(vec![PodCondition {
                    type_: "Ready".to_owned(),
                    status: if ready { "True" } else { "False" }.to_owned(),
                    ..PodCondition::default()
                }]),
                ..PodStatus::default()
            }),
        };
        self.lock()
            .pods
            .insert((namespace.to_owned(), name.to_owned()), pod);
    }

    /// Removes a pod.
    pub fn delete_pod(&self, namespace: &str, name: &str) {
        self.lock()
            .pods
            .remove(&(namespace.to_owned(), name.to_owned()));
    }

    /// Adds a service selecting `selector`, with one port that targets
    /// `target` (a number or a port name).
    pub fn add_service(
        &self,
        namespace: &str,
        name: &str,
        selector: &[(&str, &str)],
        port: u16,
        target: IntOrString,
    ) {
        let service = Service {
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                namespace: Some(namespace.to_owned()),
                ..ObjectMeta::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(
                    selector
                        .iter()
                        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                        .collect(),
                ),
                ports: Some(vec![ServicePort {
                    port: i32::from(port),
                    target_port: Some(target),
                    ..ServicePort::default()
                }]),
                ..ServiceSpec::default()
            }),
            ..Service::default()
        };
        self.lock()
            .services
            .insert((namespace.to_owned(), name.to_owned()), service);
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

/// Serves one connection: plain requests, or one port-forward.
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
        let parts: Vec<&str> = path.split('/').collect();
        let (status, body) = match parts.as_slice() {
            [
                "",
                "api",
                "v1",
                "namespaces",
                ns,
                "pods",
                name,
                "portforward",
            ] => {
                let port = param(query, "ports").and_then(|p| p.parse::<u16>().ok());
                let pod = inner
                    .lock()
                    .pods
                    .get(&((*ns).to_owned(), (*name).to_owned()))
                    .cloned();
                match (pod, port, header(&head, "sec-websocket-key")) {
                    (Some(pod), Some(port), Some(key)) => {
                        port_forward(stream, &inner, &pod, port, &key).await;
                        return;
                    }
                    (None, _, _) => not_found(),
                    _ => ("400 Bad Request", status_body(400, "BadRequest")),
                }
            }
            ["", "api", "v1", "namespaces", ns, "pods", name] => {
                match inner
                    .lock()
                    .pods
                    .get(&((*ns).to_owned(), (*name).to_owned()))
                {
                    Some(pod) => ("200 OK", json!(pod).to_string()),
                    None => not_found(),
                }
            }
            ["", "api", "v1", "namespaces", ns, "pods"] => {
                let selector = param(query, "labelSelector").unwrap_or_default();
                let state = inner.lock();
                let items: Vec<&Pod> = state
                    .pods
                    .iter()
                    .filter(|((pod_ns, _), pod)| pod_ns == ns && matches_labels(pod, &selector))
                    .map(|(_, pod)| pod)
                    .collect();
                let body = json!({
                    "kind": "PodList",
                    "apiVersion": "v1",
                    "metadata": { "resourceVersion": "1" },
                    "items": items,
                });
                ("200 OK", body.to_string())
            }
            ["", "api", "v1", "namespaces", ns, "services", name] => {
                match inner
                    .lock()
                    .services
                    .get(&((*ns).to_owned(), (*name).to_owned()))
                {
                    Some(service) => ("200 OK", json!(service).to_string()),
                    None => not_found(),
                }
            }
            _ => not_found(),
        };
        if write_response(&mut stream, status, &body).await.is_err() {
            return;
        }
    }
}

/// Upgrades to a WebSocket and plays the pod's side of one port-forward.
async fn port_forward(mut stream: TcpStream, inner: &Inner, pod: &Pod, port: u16, key: &str) {
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\nSec-WebSocket-Protocol: v4.channel.k8s.io\r\n\r\n",
        derive_accept_key(key.as_bytes())
    );
    if stream.write_all(response.as_bytes()).await.is_err() {
        return;
    }
    inner.opened.fetch_add(1, Ordering::SeqCst);
    inner.open.fetch_add(1, Ordering::SeqCst);
    let _open = Counter(&inner.open);
    let mut ws = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;

    // Each channel starts with the port number, little endian.
    let [lo, hi] = port.to_le_bytes();
    for channel in [0u8, 1] {
        if ws.send(binary(&[channel, lo, hi])).await.is_err() {
            return;
        }
    }
    let name = pod.metadata.name.clone().unwrap_or_default();
    let listens = pod
        .spec
        .iter()
        .flat_map(|s| s.containers.iter())
        .flat_map(|c| c.ports.iter().flatten())
        .any(|p| p.container_port == i32::from(port));
    if !listens {
        let message = format!(
            "error forwarding port {port} to pod {name}: failed to connect to localhost:{port}: connection refused"
        );
        let mut frame = vec![1u8];
        frame.extend_from_slice(message.as_bytes());
        // The client may already be gone; the test checks what it saw.
        let _ = ws.send(binary(&frame)).await;
        let _ = ws.close(None).await;
        return;
    }

    while let Some(Ok(message)) = ws.next().await {
        match message {
            Message::Binary(data) if data.first() == Some(&0) && data.len() > 1 => {
                let mut reply = vec![0u8];
                reply.extend_from_slice(format!("{name}:{port}:").as_bytes());
                reply.extend_from_slice(&data[1..]);
                if ws.send(binary(&reply)).await.is_err() {
                    return;
                }
            }
            Message::Close(_) => {
                let _ = ws.close(None).await;
                return;
            }
            _ => {}
        }
    }
}

fn binary(bytes: &[u8]) -> Message {
    Message::Binary(Bytes::copy_from_slice(bytes))
}

/// Decrements a counter when dropped.
struct Counter<'a>(&'a AtomicUsize);

impl Drop for Counter<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn matches_labels(pod: &Pod, selector: &str) -> bool {
    let labels = pod.metadata.labels.clone().unwrap_or_default();
    selector.split(',').filter(|s| !s.is_empty()).all(|term| {
        let (key, value) = term.split_once('=').unwrap_or((term, ""));
        labels.get(key).map(String::as_str) == Some(value)
    })
}

fn not_found() -> (&'static str, String) {
    ("404 Not Found", status_body(404, "NotFound"))
}

fn status_body(code: u16, reason: &str) -> String {
    json!({
        "kind": "Status",
        "apiVersion": "v1",
        "status": "Failure",
        "message": format!("fake api: {reason}"),
        "reason": reason,
        "code": code,
    })
    .to_string()
}

fn param(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
        .map(percent_decode)
}

fn header(head: &str, name: &str) -> Option<String> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
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
                if let Some(decoded) = decoded {
                    out.push(decoded);
                } else {
                    out.push(b'%');
                    out.extend(hex);
                }
            }
            b'+' => out.push(b' '),
            other => out.push(other),
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
