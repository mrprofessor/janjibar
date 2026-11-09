//! Pod rows: projection from `Pod` objects, and the watch that streams them.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use k8s_openapi::api::core::v1::{ContainerStatus, Pod, PodStatus};
use k8s_openapi::jiff::Timestamp;
use kube::runtime::{WatchStreamExt as _, reflector, watcher};
use kube::{Api, Client, ResourceExt as _};
use model::{ConnectionState, ContextName, Event, Health, PodRow, PodSnapshot, Ready};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior};

use crate::age::human_age;
use crate::errors::connect_error_from_watch;

/// The UI gets at most one snapshot per window (about 30 a second).
pub const SNAPSHOT_WINDOW: Duration = Duration::from_millis(33);

/// How often ages are re-checked when nothing else changes.
const AGE_TICK: Duration = Duration::from_secs(1);

/// Projects a pod into a table row, the way `kubectl get pods` shows it.
pub fn project(pod: &Pod, now: Timestamp) -> PodRow {
    let summary = summarize(pod);
    let age = pod.metadata.creation_timestamp.as_ref().map_or_else(
        || "-".to_owned(),
        |t| human_age(now.as_second() - t.0.as_second()),
    );
    PodRow {
        namespace: pod.namespace().unwrap_or_default().into(),
        name: pod.name_any().into(),
        ready: summary.ready,
        health: health(&summary.status),
        status: summary.status.into(),
        restarts: summary.restarts,
        age: age.into(),
        node: pod
            .spec
            .as_ref()
            .and_then(|s| s.node_name.as_deref())
            .unwrap_or_default()
            .into(),
    }
}

/// Projects and sorts every pod, by namespace then name.
pub fn project_all<'a>(pods: impl IntoIterator<Item = &'a Pod>, now: Timestamp) -> Vec<PodRow> {
    let mut rows: Vec<PodRow> = pods.into_iter().map(|p| project(p, now)).collect();
    rows.sort_unstable_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    rows
}

struct Summary {
    ready: Ready,
    status: String,
    restarts: u32,
}

/// A port of `printPod` from kubectl's `pkg/printers/internalversion`.
fn summarize(pod: &Pod) -> Summary {
    let spec = pod.spec.as_ref();
    let status = pod.status.as_ref();
    let init_specs = spec
        .and_then(|s| s.init_containers.as_deref())
        .unwrap_or_default();
    let is_sidecar = |name: &str| {
        init_specs
            .iter()
            .any(|c| c.name == name && c.restart_policy.as_deref() == Some("Always"))
    };

    let mut total = spec.map_or(0, |s| s.containers.len());
    total += init_specs
        .iter()
        .filter(|c| c.restart_policy.as_deref() == Some("Always"))
        .count();
    let mut ready = 0usize;
    let mut restarts = 0u32;
    let mut sidecar_restarts = 0u32;

    let mut reason = status
        .and_then(|s| s.reason.clone())
        .filter(|r| !r.is_empty())
        .or_else(|| status.and_then(|s| s.phase.clone()))
        .unwrap_or_default();

    if status
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|cs| {
            cs.iter().any(|c| {
                c.type_ == "PodScheduled" && c.reason.as_deref() == Some("SchedulingGated")
            })
        })
    {
        "SchedulingGated".clone_into(&mut reason);
    }

    let init_statuses = status
        .and_then(|s| s.init_container_statuses.as_deref())
        .unwrap_or_default();
    let mut initializing = false;
    for (i, c) in init_statuses.iter().enumerate() {
        let count = to_u32(c.restart_count);
        restarts += count;
        let state = c.state.as_ref();
        let terminated = state.and_then(|s| s.terminated.as_ref());
        let waiting = state.and_then(|s| s.waiting.as_ref());
        if is_sidecar(&c.name) {
            sidecar_restarts += count;
        }
        if terminated.is_some_and(|t| t.exit_code == 0) {
            continue;
        }
        if is_sidecar(&c.name) && c.started == Some(true) {
            if c.ready {
                ready += 1;
            }
            continue;
        }
        initializing = true;
        reason = if let Some(t) = terminated {
            match t.reason.as_deref().filter(|r| !r.is_empty()) {
                Some(r) => format!("Init:{r}"),
                None if t.signal.unwrap_or(0) != 0 => {
                    format!("Init:Signal:{}", t.signal.unwrap_or(0))
                }
                None => format!("Init:ExitCode:{}", t.exit_code),
            }
        } else if let Some(r) = waiting
            .and_then(|w| w.reason.as_deref())
            .filter(|r| !r.is_empty() && *r != "PodInitializing")
        {
            format!("Init:{r}")
        } else {
            format!("Init:{i}/{}", init_specs.len())
        };
        break;
    }

    if !initializing || is_initialized(status) {
        restarts = sidecar_restarts;
        let mut has_running = false;
        let statuses = status
            .and_then(|s| s.container_statuses.as_deref())
            .unwrap_or_default();
        for c in statuses.iter().rev() {
            restarts += to_u32(c.restart_count);
            if let Some(r) = container_reason(c) {
                reason = r;
            } else if c.ready && c.state.as_ref().and_then(|s| s.running.as_ref()).is_some() {
                has_running = true;
                ready += 1;
            }
        }
        if reason == "Completed" && has_running {
            reason = if has_condition(status, "Ready") {
                "Running".to_owned()
            } else {
                "NotReady".to_owned()
            };
        }
    }

    if pod.metadata.deletion_timestamp.is_some() {
        if status.and_then(|s| s.reason.as_deref()) == Some("NodeLost") {
            "Unknown".clone_into(&mut reason);
        } else {
            let phase = status.and_then(|s| s.phase.as_deref());
            if !matches!(phase, Some("Succeeded" | "Failed")) {
                "Terminating".clone_into(&mut reason);
            }
        }
    }

    if reason.is_empty() {
        "Unknown".clone_into(&mut reason);
    }

    Summary {
        ready: Ready {
            ready: u32::try_from(ready).unwrap_or(u32::MAX),
            total: u32::try_from(total).unwrap_or(u32::MAX),
        },
        status: reason,
        restarts,
    }
}

/// The reason a regular container shows, if it is not simply running.
fn container_reason(c: &ContainerStatus) -> Option<String> {
    let state = c.state.as_ref()?;
    if let Some(r) = state
        .waiting
        .as_ref()
        .and_then(|w| w.reason.as_deref())
        .filter(|r| !r.is_empty())
    {
        return Some(r.to_owned());
    }
    let t = state.terminated.as_ref()?;
    Some(match t.reason.as_deref().filter(|r| !r.is_empty()) {
        Some(r) => r.to_owned(),
        None if t.signal.unwrap_or(0) != 0 => format!("Signal:{}", t.signal.unwrap_or(0)),
        None => format!("ExitCode:{}", t.exit_code),
    })
}

fn is_initialized(status: Option<&PodStatus>) -> bool {
    has_condition(status, "Initialized")
}

fn has_condition(status: Option<&PodStatus>, kind: &str) -> bool {
    status
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|cs| cs.iter().any(|c| c.type_ == kind && c.status == "True"))
}

fn to_u32(n: i32) -> u32 {
    u32::try_from(n).unwrap_or(0)
}

/// Classifies a status string for colouring.
pub fn health(status: &str) -> Health {
    match status {
        "Running" => Health::Good,
        "Completed" | "Succeeded" => Health::Done,
        "Pending" | "ContainerCreating" | "PodInitializing" | "Terminating" | "SchedulingGated" => {
            Health::Pending
        }
        "Unknown" | "" => Health::Unknown,
        s if s.starts_with("Init:") => {
            let rest = &s["Init:".len()..];
            // `Init:0/2` is progress; anything else (`Init:Error`,
            // `Init:CrashLoopBackOff`) is a failure.
            if rest
                .split_once('/')
                .is_some_and(|(a, b)| a.parse::<u32>().is_ok() && b.parse::<u32>().is_ok())
            {
                Health::Pending
            } else {
                Health::Bad
            }
        }
        _ => Health::Bad,
    }
}

/// Watches every pod in a context and sends snapshots until the receiver
/// is gone or the task is dropped.
///
/// Snapshots go out at most once per [`SNAPSHOT_WINDOW`], and only when
/// the rows changed. Ages are re-checked every second, so a snapshot also
/// goes out when an age string changes.
pub async fn watch_pods(
    context: ContextName,
    client: Client,
    events: mpsc::UnboundedSender<Event>,
) {
    let api: Api<Pod> = Api::all(client);
    let (store, writer) = reflector::store::<Pod>();
    let (changed_tx, mut changed) = watch::channel(0u64);
    let (error_tx, mut errors) = mpsc::unbounded_channel::<watcher::Error>();

    let stream = watcher(api, watcher::Config::default().page_size(500))
        .default_backoff()
        .modify(|pod: &mut Pod| pod.managed_fields_mut().clear())
        .reflect(writer);

    // Drive the stream in its own task. It owns the writer, so the store only
    // updates while this task runs. It ends when `driver` is dropped.
    let driver = tokio::spawn(async move {
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
    });
    let _driver = AbortOnDrop(driver);

    let mut ages = tokio::time::interval(AGE_TICK);
    ages.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_rows: Option<Arc<[PodRow]>> = None;
    let mut last_sent: Option<Instant> = None;
    let mut ready = false;
    let mut retrying = false;

    let send_state = |state: ConnectionState| {
        events
            .send(Event::Connection {
                context: context.clone(),
                state,
            })
            .is_ok()
    };

    loop {
        // All three branches are cancel safe: `watch::Receiver::changed`,
        // `mpsc::Receiver::recv` and `Interval::tick`.
        tokio::select! {
            res = changed.changed() => {
                if res.is_err() {
                    let msg = "the watch ended unexpectedly".to_owned();
                    send_state(ConnectionState::Failed(model::ConnectError::Other(msg)));
                    return;
                }
            }
            Some(err) = errors.recv() => {
                tracing::warn!(%context, "pod watch error: {err}");
                let state = if ready {
                    retrying = true;
                    ConnectionState::Retrying(err.to_string())
                } else {
                    ConnectionState::Failed(connect_error_from_watch(&err))
                };
                if !send_state(state) {
                    return;
                }
                continue;
            }
            _ = ages.tick(), if ready => {}
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

        let state = store.state();
        let rows: Arc<[PodRow]> =
            project_all(state.iter().map(AsRef::as_ref), Timestamp::now()).into();
        last_sent = Some(Instant::now());

        if !ready || retrying {
            ready = true;
            retrying = false;
            if !send_state(ConnectionState::Live) {
                return;
            }
        }
        if last_rows.as_deref() == Some(&*rows) {
            continue;
        }
        last_rows = Some(Arc::clone(&rows));
        tracing::debug!(%context, rows = rows.len(), "pod snapshot sent");
        let snapshot = PodSnapshot {
            context: context.clone(),
            rows,
        };
        if events.send(Event::Pods(snapshot)).is_err() {
            return;
        }
    }
}

/// Aborts a tokio task when dropped, so a watch stops with its owner.
pub(crate) struct AbortOnDrop<T>(pub(crate) tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        Container, ContainerState, ContainerStateRunning, ContainerStateTerminated,
        ContainerStateWaiting, PodCondition, PodSpec,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};

    fn now() -> Timestamp {
        "2026-11-09T12:00:00Z".parse().unwrap()
    }

    fn container(name: &str) -> Container {
        Container {
            name: name.to_owned(),
            ..Container::default()
        }
    }

    fn running(name: &str, ready: bool, restarts: i32) -> ContainerStatus {
        ContainerStatus {
            name: name.to_owned(),
            ready,
            restart_count: restarts,
            state: Some(ContainerState {
                running: Some(ContainerStateRunning::default()),
                ..ContainerState::default()
            }),
            ..ContainerStatus::default()
        }
    }

    fn waiting(name: &str, reason: &str, restarts: i32) -> ContainerStatus {
        ContainerStatus {
            name: name.to_owned(),
            restart_count: restarts,
            state: Some(ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some(reason.to_owned()),
                    ..ContainerStateWaiting::default()
                }),
                ..ContainerState::default()
            }),
            ..ContainerStatus::default()
        }
    }

    fn terminated(name: &str, reason: Option<&str>, exit_code: i32) -> ContainerStatus {
        ContainerStatus {
            name: name.to_owned(),
            state: Some(ContainerState {
                terminated: Some(ContainerStateTerminated {
                    reason: reason.map(str::to_owned),
                    exit_code,
                    ..ContainerStateTerminated::default()
                }),
                ..ContainerState::default()
            }),
            ..ContainerStatus::default()
        }
    }

    fn condition(kind: &str, status: &str) -> PodCondition {
        PodCondition {
            type_: kind.to_owned(),
            status: status.to_owned(),
            ..PodCondition::default()
        }
    }

    fn pod(containers: &[&str], phase: &str, statuses: Vec<ContainerStatus>) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some("web-0".to_owned()),
                namespace: Some("shop".to_owned()),
                creation_timestamp: Some(Time("2026-11-06T08:00:00Z".parse().unwrap())),
                ..ObjectMeta::default()
            },
            spec: Some(PodSpec {
                containers: containers.iter().map(|n| container(n)).collect(),
                node_name: Some("node-a".to_owned()),
                ..PodSpec::default()
            }),
            status: Some(PodStatus {
                phase: Some(phase.to_owned()),
                container_statuses: Some(statuses),
                conditions: Some(vec![
                    condition("Initialized", "True"),
                    condition("Ready", "True"),
                ]),
                ..PodStatus::default()
            }),
        }
    }

    #[test]
    fn running_pod() {
        let p = pod(
            &["app", "envoy"],
            "Running",
            vec![running("app", true, 0), running("envoy", true, 2)],
        );
        let row = project(&p, now());
        assert_eq!(&*row.name, "web-0");
        assert_eq!(&*row.namespace, "shop");
        assert_eq!(row.ready, Ready { ready: 2, total: 2 });
        assert_eq!(&*row.status, "Running");
        assert_eq!(row.health, Health::Good);
        assert_eq!(row.restarts, 2);
        assert_eq!(&*row.age, "3d4h");
        assert_eq!(&*row.node, "node-a");
    }

    #[test]
    fn waiting_reason_beats_phase() {
        let p = pod(
            &["app", "envoy"],
            "Running",
            vec![
                waiting("app", "CrashLoopBackOff", 14),
                running("envoy", true, 0),
            ],
        );
        let row = project(&p, now());
        assert_eq!(&*row.status, "CrashLoopBackOff");
        assert_eq!(row.health, Health::Bad);
        assert_eq!(row.ready, Ready { ready: 1, total: 2 });
        assert_eq!(row.restarts, 14);
    }

    #[test]
    fn terminated_reason_and_exit_codes() {
        let p = pod(
            &["app"],
            "Running",
            vec![terminated("app", Some("OOMKilled"), 137)],
        );
        assert_eq!(&*project(&p, now()).status, "OOMKilled");

        let p = pod(&["app"], "Running", vec![terminated("app", None, 3)]);
        assert_eq!(&*project(&p, now()).status, "ExitCode:3");
    }

    #[test]
    fn completed_job_pod() {
        let mut p = pod(
            &["job"],
            "Succeeded",
            vec![terminated("job", Some("Completed"), 0)],
        );
        p.status.as_mut().unwrap().conditions = Some(vec![condition("Initialized", "True")]);
        let row = project(&p, now());
        assert_eq!(&*row.status, "Completed");
        assert_eq!(row.health, Health::Done);
        assert_eq!(row.ready, Ready { ready: 0, total: 1 });
    }

    #[test]
    fn completed_with_a_running_container_is_running_or_not_ready() {
        let p = pod(
            &["main", "helper"],
            "Running",
            vec![
                terminated("main", Some("Completed"), 0),
                running("helper", true, 0),
            ],
        );
        // Statuses are walked in reverse, so "main" sets the reason last.
        assert_eq!(&*project(&p, now()).status, "Running");

        let mut p = p;
        p.status.as_mut().unwrap().conditions = Some(vec![condition("Initialized", "True")]);
        assert_eq!(&*project(&p, now()).status, "NotReady");
    }

    #[test]
    fn pending_pod_without_statuses() {
        let mut p = pod(&["app"], "Pending", vec![]);
        p.metadata.creation_timestamp = Some(Time("2026-11-09T11:59:19Z".parse().unwrap()));
        let row = project(&p, now());
        assert_eq!(&*row.status, "Pending");
        assert_eq!(row.health, Health::Pending);
        assert_eq!(row.ready, Ready { ready: 0, total: 1 });
        assert_eq!(&*row.age, "41s");
    }

    #[test]
    fn container_creating() {
        let p = pod(
            &["app"],
            "Pending",
            vec![waiting("app", "ContainerCreating", 0)],
        );
        let row = project(&p, now());
        assert_eq!(&*row.status, "ContainerCreating");
        assert_eq!(row.health, Health::Pending);
    }

    #[test]
    fn init_containers_show_progress_and_failures() {
        let mut p = pod(
            &["app"],
            "Pending",
            vec![waiting("app", "PodInitializing", 0)],
        );
        let spec = p.spec.as_mut().unwrap();
        spec.init_containers = Some(vec![container("migrate"), container("seed")]);
        let status = p.status.as_mut().unwrap();
        status.conditions = Some(vec![condition("Initialized", "False")]);
        status.init_container_statuses = Some(vec![
            terminated("migrate", Some("Completed"), 0),
            running("seed", false, 0),
        ]);
        let row = project(&p, now());
        assert_eq!(&*row.status, "Init:1/2");
        assert_eq!(row.health, Health::Pending);

        p.status.as_mut().unwrap().init_container_statuses = Some(vec![
            terminated("migrate", Some("Completed"), 0),
            waiting("seed", "CrashLoopBackOff", 4),
        ]);
        let row = project(&p, now());
        assert_eq!(&*row.status, "Init:CrashLoopBackOff");
        assert_eq!(row.health, Health::Bad);
        assert_eq!(row.restarts, 4);

        p.status.as_mut().unwrap().init_container_statuses =
            Some(vec![terminated("migrate", None, 2)]);
        assert_eq!(&*project(&p, now()).status, "Init:ExitCode:2");
    }

    #[test]
    fn sidecars_count_towards_ready_and_total() {
        let mut p = pod(&["app"], "Running", vec![running("app", true, 0)]);
        let mut sidecar = container("proxy");
        sidecar.restart_policy = Some("Always".to_owned());
        p.spec.as_mut().unwrap().init_containers = Some(vec![sidecar]);
        let mut proxy = running("proxy", true, 3);
        proxy.started = Some(true);
        p.status.as_mut().unwrap().init_container_statuses = Some(vec![proxy]);
        let row = project(&p, now());
        assert_eq!(row.ready, Ready { ready: 2, total: 2 });
        assert_eq!(row.restarts, 3);
        assert_eq!(&*row.status, "Running");
    }

    #[test]
    fn terminating_pod() {
        let mut p = pod(&["app"], "Running", vec![running("app", true, 0)]);
        p.metadata.deletion_timestamp = Some(Time(now()));
        let row = project(&p, now());
        assert_eq!(&*row.status, "Terminating");
        assert_eq!(row.health, Health::Pending);
    }

    #[test]
    fn scheduling_gated() {
        let mut p = pod(&["app"], "Pending", vec![]);
        p.status.as_mut().unwrap().conditions = Some(vec![PodCondition {
            type_: "PodScheduled".to_owned(),
            status: "False".to_owned(),
            reason: Some("SchedulingGated".to_owned()),
            ..PodCondition::default()
        }]);
        assert_eq!(&*project(&p, now()).status, "SchedulingGated");
    }

    #[test]
    fn evicted_pod_uses_status_reason() {
        let mut p = pod(&["app"], "Failed", vec![]);
        p.status.as_mut().unwrap().reason = Some("Evicted".to_owned());
        let row = project(&p, now());
        assert_eq!(&*row.status, "Evicted");
        assert_eq!(row.health, Health::Bad);
    }

    #[test]
    fn missing_fields_do_not_panic() {
        let row = project(&Pod::default(), now());
        assert_eq!(&*row.status, "Unknown");
        assert_eq!(row.health, Health::Unknown);
        assert_eq!(&*row.age, "-");
        assert_eq!(row.ready, Ready { ready: 0, total: 0 });
    }

    #[test]
    fn rows_sort_by_namespace_then_name() {
        let mk = |ns: &str, name: &str| {
            let mut p = pod(&["c"], "Running", vec![running("c", true, 0)]);
            p.metadata.namespace = Some(ns.to_owned());
            p.metadata.name = Some(name.to_owned());
            p
        };
        let pods = [mk("b", "a"), mk("a", "z"), mk("a", "b")];
        let rows = project_all(pods.iter(), now());
        let keys: Vec<_> = rows
            .iter()
            .map(|r| format!("{}/{}", r.namespace, r.name))
            .collect();
        assert_eq!(keys, ["a/b", "a/z", "b/a"]);
    }

    #[test]
    fn health_classes() {
        assert_eq!(health("Running"), Health::Good);
        assert_eq!(health("Init:0/3"), Health::Pending);
        assert_eq!(health("Init:Error"), Health::Bad);
        assert_eq!(health("ImagePullBackOff"), Health::Bad);
        assert_eq!(health("Unknown"), Health::Unknown);
    }
}
