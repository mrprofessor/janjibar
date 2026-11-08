//! Janjibar spike: prove GPUI + kube-rs + tokio coexist.
//!
//! - tokio runtime runs on its own threads (kube-rs needs it).
//! - a kube-rs reflector watches every Pod in the current context.
//! - snapshots are batched (every 250 ms at most) and pushed over a channel.
//! - a GPUI entity receives them on the foreground executor and re-renders a
//!   virtualized `uniform_list`.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{
    App, Bounds, Context, SharedString, Window, WindowBounds, WindowOptions, div, prelude::*, px,
    rgb, size, uniform_list,
};
use gpui_platform::application;
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Api, Client,
    runtime::{WatchStreamExt, reflector, watcher},
};

#[derive(Clone)]
struct PodRow {
    namespace: SharedString,
    name: SharedString,
    ready: SharedString,
    phase: SharedString,
    restarts: i32,
    age: SharedString,
    node: SharedString,
}

enum Msg {
    Context(String),
    Snapshot { rows: Vec<PodRow>, first_ms: Option<u128> },
    Error(String),
}

fn age(ts: Option<&k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>) -> String {
    let Some(ts) = ts else { return "-".into() };
    let secs = (k8s_openapi::jiff::Timestamp::now() - ts.0)
        .total(k8s_openapi::jiff::Unit::Second)
        .unwrap_or(0.0) as i64;
    match secs {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s if s < 172800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

fn to_row(p: &Pod) -> PodRow {
    let status = p.status.as_ref();
    let cs = status.and_then(|s| s.container_statuses.as_ref());
    let total = p.spec.as_ref().map(|s| s.containers.len()).unwrap_or(0);
    let ready = cs.map(|c| c.iter().filter(|c| c.ready).count()).unwrap_or(0);
    let restarts = cs.map(|c| c.iter().map(|c| c.restart_count).sum()).unwrap_or(0);
    // Prefer a waiting reason (CrashLoopBackOff etc.) over the bare phase.
    let waiting = cs.and_then(|c| {
        c.iter()
            .find_map(|c| c.state.as_ref()?.waiting.as_ref()?.reason.clone())
    });
    let phase = waiting
        .or_else(|| status.and_then(|s| s.phase.clone()))
        .unwrap_or_else(|| "Unknown".into());
    PodRow {
        namespace: p.metadata.namespace.clone().unwrap_or_default().into(),
        name: p.metadata.name.clone().unwrap_or_default().into(),
        ready: format!("{ready}/{total}").into(),
        phase: phase.into(),
        restarts,
        age: age(p.metadata.creation_timestamp.as_ref()).into(),
        node: p
            .spec
            .as_ref()
            .and_then(|s| s.node_name.clone())
            .unwrap_or_default()
            .into(),
    }
}

/// Runs on tokio. Owns the watch; GPUI never touches kube types directly.
async fn watch_pods(tx: tokio::sync::mpsc::UnboundedSender<Msg>) -> anyhow::Result<()> {
    let started = Instant::now();
    let config = kube::Config::infer().await?;
    let _ = tx.send(Msg::Context(config.cluster_url.to_string()));
    let client = Client::try_from(config)?;
    let api: Api<Pod> = Api::all(client);

    let (reader, writer) = reflector::store::<Pod>();
    let version = Arc::new(AtomicU64::new(0));
    let v = version.clone();
    tokio::spawn(async move {
        reflector(writer, watcher(api, watcher::Config::default()))
            .applied_objects()
            .for_each(|_| {
                v.fetch_add(1, Ordering::Relaxed);
                async {}
            })
            .await;
    });

    reader.wait_until_ready().await?;
    let mut first = Some(started.elapsed().as_millis());
    let mut seen = u64::MAX;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tick.tick().await;
        let now = version.load(Ordering::Relaxed);
        if now == seen && first.is_none() {
            continue; // nothing changed, do not wake the UI
        }
        seen = now;
        let mut rows: Vec<PodRow> = reader.state().iter().map(|p| to_row(p)).collect();
        rows.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
        if tx.send(Msg::Snapshot { rows, first_ms: first.take() }).is_err() {
            return Ok(()); // window closed
        }
    }
}

struct PodList {
    context: SharedString,
    status: SharedString,
    rows: Vec<PodRow>,
}

impl PodList {
    fn new(mut rx: tokio::sync::mpsc::UnboundedReceiver<Msg>, cx: &mut Context<Self>) -> Self {
        // tokio's mpsc is executor-agnostic, so the GPUI foreground executor
        // can await it directly. This is the whole bridge.
        cx.spawn(async move |this, cx| {
            while let Some(msg) = rx.recv().await {
                let alive = this.update(cx, |this, cx| {
                    match msg {
                        Msg::Context(c) => this.context = c.into(),
                        Msg::Snapshot { rows, first_ms } => {
                            if let Some(ms) = first_ms {
                                eprintln!("first snapshot: {} pods in {ms} ms", rows.len());
                            }
                            eprintln!("snapshot: {} pods", rows.len());
                            this.status = format!("{} pods, live", rows.len()).into();
                            this.rows = rows;
                        }
                        Msg::Error(e) => this.status = format!("error: {e}").into(),
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            context: "connecting...".into(),
            status: "loading".into(),
            rows: vec![],
        }
    }
}

const COLS: [(&str, f32); 7] = [
    ("NAMESPACE", 160.),
    ("NAME", 360.),
    ("READY", 64.),
    ("STATUS", 150.),
    ("RESTARTS", 80.),
    ("AGE", 64.),
    ("NODE", 200.),
];

fn status_color(phase: &str) -> u32 {
    match phase {
        "Running" | "Succeeded" | "Completed" => 0x3fb27f,
        "Pending" | "ContainerCreating" | "PodInitializing" => 0xd9a441,
        _ => 0xe0605a,
    }
}

impl Render for PodList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = div()
            .flex()
            .px_3()
            .py_1()
            .text_xs()
            .text_color(rgb(0x8a93a6))
            .border_b_1()
            .border_color(rgb(0x2a2f3a))
            .children(COLS.iter().map(|(n, w)| div().w(px(*w)).child(*n)));

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x14171d))
            .text_color(rgb(0xd6dbe4))
            .text_sm()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .bg(rgb(0x1b1f27))
                    .child(format!("Pods  ·  {}", self.context))
                    .child(div().text_color(rgb(0x8a93a6)).child(self.status.clone())),
            )
            .child(header)
            .child(
                uniform_list(
                    "pods",
                    self.rows.len(),
                    cx.processor(|this, range: std::ops::Range<usize>, _window, _cx| {
                        range
                            .map(|ix| {
                                let r = &this.rows[ix];
                                let cells: [SharedString; 7] = [
                                    r.namespace.clone(),
                                    r.name.clone(),
                                    r.ready.clone(),
                                    r.phase.clone(),
                                    r.restarts.to_string().into(),
                                    r.age.clone(),
                                    r.node.clone(),
                                ];
                                let color = status_color(&r.phase);
                                div()
                                    .id(ix)
                                    .flex()
                                    .px_3()
                                    .py_1()
                                    .hover(|s| s.bg(rgb(0x222733)))
                                    .children(cells.into_iter().enumerate().map(
                                        move |(i, c)| {
                                            let cell = div()
                                                .w(px(COLS[i].1))
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .child(c);
                                            if i == 3 { cell.text_color(rgb(color)) } else { cell }
                                        },
                                    ))
                            })
                            .collect()
                    }),
                )
                .flex_1(),
            )
    }
}

fn main() {
    // tokio lives on its own worker threads for the whole app lifetime.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let err_tx = tx.clone();
    rt.spawn(async move {
        if let Err(e) = watch_pods(tx).await {
            let _ = err_tx.send(Msg::Error(e.to_string()));
        }
    });

    application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1120.), px(640.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| PodList::new(rx, cx)),
        )
        .unwrap();
        cx.activate(true);
    });
    drop(rt);
}
