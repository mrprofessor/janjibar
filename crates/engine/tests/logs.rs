//! End-to-end log tests against the fake API server. No cluster needed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

mod support {
    pub mod fake_api;
}

use std::sync::Arc;
use std::time::Duration;

use engine::KubeconfigSources;
use engine::logs::stream_logs;
use k8s_openapi::jiff::{Timestamp, ToSpan as _};
use model::{
    Command, Containers, Event, LineKind, LogBuffer, LogOptions, LogRequest, LogState, LogTabId,
    LogTarget,
};
use support::fake_api::FakeApi;
use tokio::sync::mpsc;
use tokio::time::timeout;

const HOUR: i64 = 3600;
const WAIT: Duration = Duration::from_secs(10);

fn client_for(api: &FakeApi) -> kube::Client {
    let config = kube::Config::new(api.url().parse().unwrap());
    kube::Client::try_from(config).unwrap()
}

fn pod_target(namespace: &str, name: &str) -> LogTarget {
    LogTarget::Pod {
        namespace: namespace.into(),
        name: name.into(),
    }
}

fn request(target: LogTarget, options: LogOptions) -> LogRequest {
    LogRequest {
        tab: LogTabId::new(1),
        context: "fake".into(),
        target,
        containers: Containers::All,
        options,
    }
}

/// What a test has seen of one tab.
struct Tab {
    events: mpsc::UnboundedReceiver<Event>,
    buffer: LogBuffer,
    state: Option<LogState>,
}

impl Tab {
    fn open(api: &FakeApi, request: LogRequest) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(stream_logs(request, client_for(api), tx));
        let tab = Self {
            events,
            buffer: LogBuffer::default(),
            state: None,
        };
        (tab, task)
    }

    /// Applies events until `done` holds.
    async fn until(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let waited = timeout(WAIT, async {
            while !done(self) {
                match self.events.recv().await.expect("tab stopped") {
                    Event::Logs(chunk) => {
                        let sorted = chunk.lines.windows(2).all(|w| w[0].time <= w[1].time);
                        assert!(sorted, "chunks arrive sorted by time");
                        self.buffer.push(chunk.lines);
                    }
                    Event::LogState { state, .. } => self.state = Some(state),
                    _ => {}
                }
            }
        })
        .await;
        assert!(
            waited.is_ok(),
            "timed out waiting for {what}; state {:?}, lines {:?}",
            self.state,
            self.texts()
        );
    }

    fn texts(&self) -> Vec<String> {
        self.buffer.iter().map(|l| l.text.to_string()).collect()
    }

    fn has(&self, text: &str) -> bool {
        self.buffer.iter().any(|l| &*l.text == text)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follows_a_pod_and_picks_up_after_a_restart() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_pod("shop", "web-0", HOUR);
    let base = Timestamp::now() - 10.seconds();
    for (i, text) in ["boot", "INFO ready", "level=warn heap high"]
        .iter()
        .enumerate()
    {
        let at = base + i64::try_from(i).unwrap().seconds();
        api.log_at("shop", "web-0", "app", at, text);
    }

    let (mut tab, task) = Tab::open(
        &api,
        request(pod_target("shop", "web-0"), LogOptions::default()),
    );
    tab.until("the tail", |t| t.buffer.len() == 3).await;
    assert_eq!(tab.texts(), ["boot", "INFO ready", "level=warn heap high"]);
    let levels: Vec<LineKind> = tab.buffer.iter().map(|l| l.kind).collect();
    assert_eq!(
        levels,
        [
            LineKind::Text(None),
            LineKind::Text(Some(model::LogLevel::Info)),
            LineKind::Text(Some(model::LogLevel::Warn)),
        ]
    );
    let first = tab.buffer.get(0).unwrap();
    assert_eq!(&*first.source.pod, "web-0");
    assert_eq!(&*first.source.container, "app");
    tab.until("streaming", |t| {
        t.state
            == Some(LogState::Streaming {
                pods: 1,
                containers: 1,
            })
    })
    .await;

    // Follow: new lines arrive while the stream stays open.
    wait_until("the stream to open", || api.open_log_streams() == 1).await;
    api.log("shop", "web-0", "app", "live 1");
    tab.until("a live line", |t| t.has("live 1")).await;

    // The container crashes and comes back with a new ID.
    api.crash("shop", "web-0", "app");
    wait_until("the stream to end", || api.open_log_streams() == 0).await;
    api.start_container("shop", "web-0", "app");
    tab.until("the restart marker", |t| {
        t.buffer
            .iter()
            .any(|l| l.kind == LineKind::Restarted { restarts: 1 })
    })
    .await;
    wait_until("the stream to reopen", || api.open_log_streams() == 1).await;
    api.log("shop", "web-0", "app", "after restart");
    tab.until("a line from the new container", |t| t.has("after restart"))
        .await;

    let texts = tab.texts();
    let marker = "container restarted (1), stream picked up again";
    assert_eq!(
        texts,
        [
            "boot",
            "INFO ready",
            "level=warn heap high",
            "live 1",
            marker,
            "after restart"
        ],
        "the marker sits between the old container's lines and the new one's"
    );
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_stream_reconnects_without_repeating_lines() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_pod("shop", "web-0", HOUR);
    let (mut tab, task) = Tab::open(
        &api,
        request(pod_target("shop", "web-0"), LogOptions::default()),
    );
    tab.until("streaming", |t| {
        matches!(t.state, Some(LogState::Streaming { .. }))
    })
    .await;
    wait_until("the stream to open", || api.open_log_streams() == 1).await;
    for text in ["a", "b", "c"] {
        api.log("shop", "web-0", "app", text);
    }
    tab.until("three lines", |t| t.buffer.len() == 3).await;

    let requests = api.log_requests();
    api.cut_log_streams("shop", "web-0", "app");
    // The reconnect replays from the start of the last line's second.
    wait_until("a reconnect", || {
        api.log_requests() > requests && api.open_log_streams() == 1
    })
    .await;
    api.log("shop", "web-0", "app", "d");
    tab.until("the next line", |t| t.has("d")).await;
    assert_eq!(tab.texts(), ["a", "b", "c", "d"]);
    assert!(
        tab.buffer
            .iter()
            .all(|l| !matches!(l.kind, LineKind::Restarted { .. })),
        "a dropped connection is not a restart"
    );
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merges_a_workload_by_time_and_follows_pods_as_they_come_and_go() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let web = [("app", "web")];
    api.add_pod_with("shop", "web-6b9-aaaaa", HOUR, &web, &["app"]);
    api.add_pod_with("shop", "web-6b9-bbbbb", HOUR, &web, &["app"]);
    api.add_pod_with("shop", "other-0", HOUR, &[("app", "other")], &["app"]);
    api.add_pod_with("kube-system", "web-elsewhere", HOUR, &web, &["app"]);
    let base = Timestamp::now() - 60.seconds();
    let at = |s: i64| base + s.seconds();
    api.log_at("shop", "web-6b9-aaaaa", "app", at(1), "a1");
    api.log_at("shop", "web-6b9-aaaaa", "app", at(3), "a3");
    api.log_at("shop", "web-6b9-bbbbb", "app", at(2), "b2");
    api.log_at("shop", "web-6b9-bbbbb", "app", at(4), "b4");
    api.log_at("shop", "other-0", "app", at(2), "not mine");
    api.log_at(
        "kube-system",
        "web-elsewhere",
        "app",
        at(2),
        "wrong namespace",
    );

    let target = LogTarget::Selector {
        namespace: "shop".into(),
        selector: "app=web".into(),
        title: "deploy/web".into(),
    };
    let (mut tab, task) = Tab::open(&api, request(target, LogOptions::default()));
    tab.until("both pods' tails", |t| t.buffer.len() == 4).await;
    assert_eq!(tab.texts(), ["a1", "b2", "a3", "b4"]);
    let tags: Vec<&str> = tab.buffer.iter().map(|l| l.source.short_pod()).collect();
    assert_eq!(tags, ["aaaaa", "bbbbb", "aaaaa", "bbbbb"]);
    tab.until("two pods", |t| {
        t.state
            == Some(LogState::Streaming {
                pods: 2,
                containers: 2,
            })
    })
    .await;
    wait_until("two streams", || api.open_log_streams() == 2).await;

    // A new pod matches the selector: it gets a stream.
    api.add_pod_with("shop", "web-6b9-ccccc", 0, &web, &["app"]);
    tab.until("three pods", |t| {
        t.state
            == Some(LogState::Streaming {
                pods: 3,
                containers: 3,
            })
    })
    .await;
    wait_until("three streams", || api.open_log_streams() == 3).await;
    api.log("shop", "web-6b9-ccccc", "app", "hello from c");
    tab.until("the new pod's line", |t| t.has("hello from c"))
        .await;
    let last = tab.buffer.iter().last().unwrap();
    assert_eq!(&*last.source.pod, "web-6b9-ccccc");

    // A pod goes away: its stream closes and the count drops.
    api.delete_pod("shop", "web-6b9-aaaaa");
    tab.until("two pods again", |t| {
        t.state
            == Some(LogState::Streaming {
                pods: 2,
                containers: 2,
            })
    })
    .await;
    wait_until("two streams again", || api.open_log_streams() == 2).await;
    assert!(!tab.has("not mine") && !tab.has("wrong namespace"));
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_the_previous_container_once() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_pod("faults", "crashloop", HOUR);
    api.log("faults", "crashloop", "app", "panic: nil map");
    api.restart("faults", "crashloop", "app");
    api.log("faults", "crashloop", "app", "starting again");

    let options = LogOptions {
        follow: false,
        previous: true,
        ..LogOptions::default()
    };
    let (mut tab, task) = Tab::open(&api, request(pod_target("faults", "crashloop"), options));
    tab.until("done", |t| t.state == Some(LogState::Done)).await;
    assert_eq!(tab.texts(), ["panic: nil map"]);
    assert_eq!(
        tab.buffer.get(0).unwrap().kind,
        LineKind::Text(Some(model::LogLevel::Error))
    );
    timeout(WAIT, task).await.unwrap().unwrap();
    assert_eq!(api.open_log_streams(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_huge_line_is_cut_while_reading_and_marked() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_pod("shop", "web-1", HOUR);
    // About 80 KiB of colour codes, then text. Cleaning alone would strip the
    // codes and keep "TAIL"; the reader must stop before it and mark the cut.
    let huge = format!("{}TAIL", "\u{1b}[0m".repeat(20_000));
    api.log("shop", "web-1", "app", &huge);
    api.log("shop", "web-1", "app", "after");

    let options = LogOptions {
        follow: false,
        ..LogOptions::default()
    };
    let (mut tab, task) = Tab::open(&api, request(pod_target("shop", "web-1"), options));
    tab.until("done", |t| t.state == Some(LogState::Done)).await;
    assert_eq!(tab.texts(), ["…", "after"]);
    timeout(WAIT, task).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_pod_reports_no_pods() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let (mut tab, task) = Tab::open(
        &api,
        request(pod_target("shop", "ghost"), LogOptions::default()),
    );
    tab.until("no pods", |t| t.state == Some(LogState::NoPods))
        .await;
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_engine_opens_and_closes_log_tabs() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_pod("shop", "web-0", HOUR);
    api.log("shop", "web-0", "app", "hello");

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
    let (engine, events) = engine::start(
        &tokio::runtime::Handle::current(),
        KubeconfigSources::new(vec![path]),
    );
    let mut tab = Tab {
        events,
        buffer: LogBuffer::default(),
        state: None,
    };
    let tab_id = LogTabId::new(42);
    assert!(engine.send(Command::OpenLogs(LogRequest {
        tab: tab_id,
        ..request(pod_target("shop", "web-0"), LogOptions::default())
    })));
    tab.until("the first line", |t| t.has("hello")).await;
    assert!(Arc::strong_count(&tab.buffer.get(0).unwrap().source) >= 1);
    wait_until("the stream to open", || api.open_log_streams() == 1).await;
    wait_until("the pod watch to open", || api.open_watches() == 1).await;

    assert!(engine.send(Command::CloseLogs(tab_id)));
    wait_until("the stream to close", || api.open_log_streams() == 0).await;
    wait_until("the pod watch to close", || api.open_watches() == 0).await;
}
