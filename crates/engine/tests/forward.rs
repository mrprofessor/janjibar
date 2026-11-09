//! Port-forwards end to end: engine commands in, bytes through a fake
//! kubelet, events out. No cluster needed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

mod support {
    pub mod fake_forward;
}

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use engine::{EngineHandle, EngineOptions, KubeconfigSources};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use model::{Command, Event, ForwardRow, ForwardSpec, ForwardState, ForwardTarget};
use support::fake_forward::FakeForwardApi;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

struct Setup {
    engine: EngineHandle,
    events: mpsc::UnboundedReceiver<Event>,
    dir: tempfile::TempDir,
}

impl Setup {
    fn forwards_file(&self) -> PathBuf {
        self.dir.path().join("forwards.json")
    }
}

fn write_kubeconfig(dir: &Path, api: &FakeForwardApi) -> PathBuf {
    let path = dir.join("config");
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
    path
}

/// Starts an engine whose forwards file lives in `dir`.
async fn start_in(dir: tempfile::TempDir, api: &FakeForwardApi) -> Setup {
    let kubeconfig = write_kubeconfig(dir.path(), api);
    let (engine, mut events) = engine::start_with(
        &tokio::runtime::Handle::current(),
        KubeconfigSources::new(vec![kubeconfig]),
        EngineOptions {
            forwards_file: Some(dir.path().join("forwards.json")),
            ..EngineOptions::default()
        },
    );
    // Wait for the catalog so the context is known before any command.
    timeout(WAIT, async {
        loop {
            if let Some(Event::Catalog(_)) = events.recv().await {
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for the catalog");
    Setup {
        engine,
        events,
        dir,
    }
}

async fn start(api: &FakeForwardApi) -> Setup {
    start_in(tempfile::tempdir().unwrap(), api).await
}

fn pod_spec(name: &str, port: u16) -> ForwardSpec {
    ForwardSpec {
        context: "fake".into(),
        namespace: "shop".into(),
        target: ForwardTarget::Pod(name.into()),
        port,
        local_port: 0,
    }
}

fn service_spec(name: &str, port: u16) -> ForwardSpec {
    ForwardSpec {
        target: ForwardTarget::Service(name.into()),
        ..pod_spec(name, port)
    }
}

/// Waits for a forwards snapshot that satisfies `ready`.
async fn forwards_until(
    events: &mut mpsc::UnboundedReceiver<Event>,
    what: &str,
    ready: impl Fn(&[ForwardRow]) -> bool,
) -> Arc<[ForwardRow]> {
    timeout(WAIT, async {
        loop {
            if let Event::Forwards(rows) = events.recv().await.expect("engine stopped")
                && ready(&rows)
            {
                return rows;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Starts a forward and waits until it listens. Returns its row.
async fn listening(setup: &mut Setup, spec: ForwardSpec) -> ForwardRow {
    assert!(setup.engine.send(Command::StartForward(spec)));
    let rows = forwards_until(&mut setup.events, "a listening forward", |rows| {
        rows.iter()
            .any(|r| r.is_running() || matches!(r.state, ForwardState::Failed(_)))
    })
    .await;
    let row = rows.first().unwrap().clone();
    assert_eq!(row.state, ForwardState::Listening, "{row:?}");
    row
}

/// Sends `message` through the forward and reads until `expected` arrives.
async fn exchange(addr: SocketAddr, message: &str, expected: &str) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(message.as_bytes()).await.unwrap();
    let mut got = Vec::new();
    let read = timeout(WAIT, async {
        let mut buf = [0u8; 1024];
        while !String::from_utf8_lossy(&got).contains(expected) {
            let n = stream.read(&mut buf).await.unwrap();
            assert!(
                n > 0,
                "closed early; got {:?}",
                String::from_utf8_lossy(&got)
            );
            got.extend_from_slice(&buf[..n]);
        }
    })
    .await;
    assert!(
        read.is_ok(),
        "timed out; got {:?}",
        String::from_utf8_lossy(&got)
    );
}

/// Opens a connection and waits for the forward to close it.
async fn closed_without_reply(addr: SocketAddr) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    // Writing may fail once the forward has given up; that is the point.
    let _ = stream.write_all(b"hello").await;
    let mut buf = Vec::new();
    let read = timeout(WAIT, stream.read_to_end(&mut buf)).await;
    assert!(read.is_ok(), "the connection stayed open");
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

fn saved(path: &Path) -> serde_json::Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pod_forwards_carry_bytes_both_ways() {
    let api = FakeForwardApi::start().await.unwrap();
    api.add_pod("shop", "web-0", &[("app", "web")], true, &[("http", 8080)]);
    let mut setup = start(&api).await;

    let row = listening(&mut setup, pod_spec("web-0", 8080)).await;
    let addr = row.local_addr.unwrap();
    assert!(addr.ip().is_loopback());
    assert_eq!(row.spec.local_port, addr.port(), "the picked port is kept");

    exchange(addr, "ping", "web-0:8080:ping").await;
    let rows = forwards_until(&mut setup.events, "the connection to finish", |rows| {
        rows[0].total == 1 && rows[0].active == 0
    })
    .await;
    assert_eq!(rows[0].last_pod.as_deref(), Some("web-0"));
    assert_eq!(rows[0].last_error, None);
    assert_eq!(api.tunnels_opened(), 1);

    // Each connection gets its own port-forward.
    exchange(addr, "again", "web-0:8080:again").await;
    wait_until("a second tunnel", || api.tunnels_opened() == 2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_forwards_follow_ready_pods() {
    let api = FakeForwardApi::start().await.unwrap();
    let labels = [("app", "gateway")];
    api.add_pod("shop", "gateway-a", &labels, true, &[("http", 8080)]);
    api.add_pod(
        "shop",
        "gateway-0-starting",
        &labels,
        false,
        &[("http", 8080)],
    );
    api.add_pod(
        "shop",
        "other",
        &[("app", "other")],
        true,
        &[("http", 8080)],
    );
    // Port 80 targets the named port `http`, like the kind cluster's gateway.
    api.add_service(
        "shop",
        "gateway",
        &labels,
        80,
        IntOrString::String("http".into()),
    );
    let mut setup = start(&api).await;

    let addr = listening(&mut setup, service_spec("gateway", 80))
        .await
        .local_addr
        .unwrap();
    exchange(addr, "one", "gateway-a:8080:one").await;

    // The pod is replaced; the next connection finds the new one.
    api.delete_pod("shop", "gateway-a");
    api.add_pod("shop", "gateway-b", &labels, true, &[("http", 8080)]);
    exchange(addr, "two", "gateway-b:8080:two").await;
    forwards_until(&mut setup.events, "the new pod in the row", |rows| {
        rows[0].last_pod.as_deref() == Some("gateway-b")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_service_without_ready_pods_says_so() {
    let api = FakeForwardApi::start().await.unwrap();
    let labels = [("app", "gateway")];
    api.add_pod("shop", "gateway-a", &labels, false, &[("http", 8080)]);
    api.add_service("shop", "gateway", &labels, 80, IntOrString::Int(8080));
    let mut setup = start(&api).await;

    let addr = listening(&mut setup, service_spec("gateway", 80))
        .await
        .local_addr
        .unwrap();
    closed_without_reply(addr).await;
    let rows = forwards_until(&mut setup.events, "the error in the row", |rows| {
        rows[0].last_error.is_some()
    })
    .await;
    assert_eq!(
        rows[0].last_error.as_deref(),
        Some("no ready pod behind service gateway")
    );
    // The forward keeps listening for the next try.
    assert_eq!(rows[0].state, ForwardState::Listening);
    assert_eq!(api.tunnels_opened(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_pod_port_reports_the_pods_error() {
    let api = FakeForwardApi::start().await.unwrap();
    api.add_pod("shop", "web-0", &[("app", "web")], true, &[("http", 8080)]);
    let mut setup = start(&api).await;

    let addr = listening(&mut setup, pod_spec("web-0", 9999))
        .await
        .local_addr
        .unwrap();
    closed_without_reply(addr).await;
    let rows = forwards_until(&mut setup.events, "the error in the row", |rows| {
        rows[0].last_error.is_some()
    })
    .await;
    let error = rows[0].last_error.as_deref().unwrap();
    assert!(error.contains("connection refused"), "{error}");
    assert!(error.contains("9999"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_a_forward_closes_its_port_and_connections() {
    let api = FakeForwardApi::start().await.unwrap();
    api.add_pod("shop", "web-0", &[("app", "web")], true, &[("http", 8080)]);
    let mut setup = start(&api).await;
    let row = listening(&mut setup, pod_spec("web-0", 8080)).await;
    let addr = row.local_addr.unwrap();

    // Keep one connection open across the stop.
    let mut open = TcpStream::connect(addr).await.unwrap();
    open.write_all(b"hold").await.unwrap();
    wait_until("the tunnel to open", || api.tunnels_open() == 1).await;

    assert!(setup.engine.send(Command::StopForward(row.id)));
    forwards_until(&mut setup.events, "no forwards", <[ForwardRow]>::is_empty).await;
    wait_until("the tunnel to close", || api.tunnels_open() == 0).await;
    let mut buf = Vec::new();
    let drained = timeout(WAIT, open.read_to_end(&mut buf)).await;
    assert!(drained.is_ok(), "the open connection was not closed");
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "the port is still open"
    );

    let file = setup.forwards_file();
    wait_until("the file to forget the forward", || {
        saved(&file)["clusters"]
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_are_saved_and_start_again_when_the_cluster_connects() {
    let api = FakeForwardApi::start().await.unwrap();
    api.add_pod("shop", "web-0", &[("app", "web")], true, &[("http", 8080)]);
    let mut first = start(&api).await;
    let port = listening(&mut first, pod_spec("web-0", 8080))
        .await
        .spec
        .local_port;

    let file = first.forwards_file();
    wait_until("the forward to be saved", || {
        saved(&file)["clusters"]["fake"][0]["localPort"] == port
    })
    .await;
    assert_eq!(
        saved(&file)["clusters"]["fake"][0],
        serde_json::json!({ "namespace": "shop", "pod": "web-0", "port": 8080, "localPort": port })
    );

    // Stop the first engine and wait for its port to close.
    let Setup { engine, dir, .. } = first;
    drop(engine);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    timeout(WAIT, async {
        while TcpStream::connect(addr).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the first engine kept the port open");

    // A new engine reads the file; the forward waits for its cluster.
    let mut second = start_in(dir, &api).await;
    let rows = forwards_until(&mut second.events, "the saved forward", |rows| {
        !rows.is_empty()
    })
    .await;
    assert_eq!(rows[0].state, ForwardState::Waiting);
    assert_eq!(rows[0].spec.local_port, port);

    assert!(second.engine.send(Command::SelectContext("fake".into())));
    forwards_until(&mut second.events, "the forward to listen again", |rows| {
        rows[0].is_running()
    })
    .await;
    exchange(addr, "back", "web-0:8080:back").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_taken_local_port_fails_cleanly() {
    let api = FakeForwardApi::start().await.unwrap();
    api.add_pod("shop", "web-0", &[("app", "web")], true, &[("http", 8080)]);
    let mut setup = start(&api).await;
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();

    let spec = ForwardSpec {
        local_port: port,
        ..pod_spec("web-0", 8080)
    };
    assert!(setup.engine.send(Command::StartForward(spec)));
    let rows = forwards_until(&mut setup.events, "the failure", |rows| {
        matches!(
            rows.first().map(|r| &r.state),
            Some(ForwardState::Failed(_))
        )
    })
    .await;
    assert_eq!(
        rows[0].state,
        ForwardState::Failed(format!("port {port} is already in use on 127.0.0.1"))
    );
    assert_eq!(rows[0].local_addr, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forward_for_an_unknown_context_fails() {
    let api = FakeForwardApi::start().await.unwrap();
    let mut setup = start(&api).await;
    let spec = ForwardSpec {
        context: "gone".into(),
        ..pod_spec("web-0", 8080)
    };
    assert!(setup.engine.send(Command::StartForward(spec)));
    let rows = forwards_until(&mut setup.events, "the failure", |rows| {
        matches!(
            rows.first().map(|r| &r.state),
            Some(ForwardState::Failed(_))
        )
    })
    .await;
    assert_eq!(
        rows[0].state,
        ForwardState::Failed("this context is no longer in your kubeconfig".to_owned())
    );
}
