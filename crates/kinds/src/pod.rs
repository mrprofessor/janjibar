//! Pods: kubectl-style status, the table row and the inspector.

use std::fmt;
use std::sync::Arc;

use k8s_openapi::api::core::v1::{ContainerStatus, Pod, PodStatus};
use k8s_openapi::jiff::Timestamp;
use model::Health;
use model::resource::{ContainerInfo, Details, Field, Kind};
use model::{Column, ColumnType, TableRow};

use crate::Spec;
use crate::common::{self, DASH, col, or_dash};

/// Ready containers out of total, like kubectl's `READY` column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Ready {
    /// Containers reporting ready.
    pub ready: u32,
    /// Containers in the pod (including restartable init containers).
    pub total: u32,
}

impl fmt::Display for Ready {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.ready, self.total)
    }
}

/// A pod as `kubectl get pods` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// Namespace.
    pub namespace: Arc<str>,
    /// Name.
    pub name: Arc<str>,
    /// Ready containers out of total.
    pub ready: Ready,
    /// A phase, or a reason like `CrashLoopBackOff`.
    pub status: Arc<str>,
    /// Colour class for `status`.
    pub health: Health,
    /// Container restarts, summed.
    pub restarts: u32,
    /// kubectl-style age.
    pub age: Arc<str>,
    /// Node name, empty when not scheduled.
    pub node: Arc<str>,
}

/// Projects a pod the way `kubectl get pods` shows it.
pub fn project(pod: &Pod, now: Timestamp) -> View {
    let summary = summarize(pod);
    View {
        namespace: common::namespace(&pod.metadata),
        name: common::name(&pod.metadata),
        ready: summary.ready,
        health: health(&summary.status),
        status: summary.status.into(),
        restarts: summary.restarts,
        age: common::age(&pod.metadata, now),
        node: pod
            .spec
            .as_ref()
            .and_then(|s| s.node_name.as_deref())
            .unwrap_or_default()
            .into(),
    }
}

/// The Pods spec.
pub struct Pods;

impl Spec for Pods {
    type Object = Pod;
    const KIND: Kind = Kind::Pod;

    fn columns() -> Vec<Column> {
        vec![
            col("Name", ColumnType::String, "name"),
            col("Ready", ColumnType::String, ""),
            col("Status", ColumnType::String, "status"),
            col("Restarts", ColumnType::Integer, ""),
            col("Age", ColumnType::Date, ""),
            col("Node", ColumnType::String, "muted"),
        ]
    }

    fn row(pod: &Pod, now: Timestamp) -> TableRow {
        let v = project(pod, now);
        common::row(
            &pod.metadata,
            vec![
                v.name,
                v.ready.to_string().into(),
                v.status,
                common::num(v.restarts),
                v.age,
                v.node,
            ],
            v.health,
        )
    }

    fn details(pod: &Pod, now: Timestamp) -> Details {
        let v = project(pod, now);
        let mut d = common::base_details(Kind::Pod, &pod.metadata, now);
        d.status = Arc::clone(&v.status);
        d.health = v.health;
        d.badges.push(format!("{} ready", v.ready).into());
        if v.restarts > 0 {
            let s = if v.restarts == 1 { "" } else { "s" };
            d.badges.push(format!("{} restart{s}", v.restarts).into());
        }
        let status = pod.status.as_ref();
        let spec = pod.spec.as_ref();
        let mut fields = vec![
            Field::new("Node", or_dash(spec.and_then(|s| s.node_name.as_deref()))),
            Field::new("Pod IP", or_dash(status.and_then(|s| s.pod_ip.as_deref()))),
            Field::new(
                "Host IP",
                or_dash(status.and_then(|s| s.host_ip.as_deref())),
            ),
            Field::new("QoS", or_dash(status.and_then(|s| s.qos_class.as_deref()))),
            Field::new(
                "Service account",
                or_dash(spec.and_then(|s| s.service_account_name.as_deref())),
            ),
        ];
        if let Some(pc) = spec.and_then(|s| s.priority_class_name.as_deref()) {
            fields.push(Field::new("Priority class", pc));
        }
        fields.append(&mut d.fields);
        d.fields = fields;
        d.containers = containers(pod);
        d.sections.extend(common::conditions(
            status
                .and_then(|s| s.conditions.as_ref())
                .into_iter()
                .flatten()
                .map(|c| {
                    (
                        c.type_.as_str(),
                        c.status.as_str(),
                        c.reason.as_deref(),
                        c.message.as_deref(),
                    )
                }),
        ));
        d
    }
}

/// Init containers, then regular ones, with their live state.
fn containers(pod: &Pod) -> Vec<ContainerInfo> {
    let Some(spec) = pod.spec.as_ref() else {
        return Vec::new();
    };
    let status = pod.status.as_ref();
    let find = |list: Option<&Vec<ContainerStatus>>, name: &str| {
        list.and_then(|l| l.iter().find(|c| c.name == name))
            .cloned()
    };
    let init = spec.init_containers.iter().flatten().map(|c| {
        let st = find(
            status.and_then(|s| s.init_container_statuses.as_ref()),
            &c.name,
        );
        (c, st, true)
    });
    let main = spec.containers.iter().map(|c| {
        let st = find(status.and_then(|s| s.container_statuses.as_ref()), &c.name);
        (c, st, false)
    });
    init.chain(main)
        .map(|(c, st, init)| {
            let (state, health) = st
                .as_ref()
                .map_or_else(|| ("Waiting".to_owned(), Health::Unknown), container_state);
            ContainerInfo {
                name: c.name.as_str().into(),
                image: or_dash(c.image.as_deref()),
                init,
                state: state.into(),
                health,
                restarts: st.as_ref().map_or(0, |s| to_u32(s.restart_count)),
                resources: common::resources(c.resources.as_ref()),
            }
        })
        .collect()
}

/// A container's state as one line, like `Exited 137, OOMKilled`.
fn container_state(c: &ContainerStatus) -> (String, Health) {
    let last = c
        .last_state
        .as_ref()
        .and_then(|s| s.terminated.as_ref())
        .map(|t| match t.reason.as_deref() {
            Some(r) => format!(" (last: {r}, exit {})", t.exit_code),
            None => format!(" (last: exit {})", t.exit_code),
        })
        .unwrap_or_default();
    let Some(state) = c.state.as_ref() else {
        return (DASH.to_owned(), Health::Unknown);
    };
    if state.running.is_some() {
        return if c.ready {
            ("Running, ready".to_owned(), Health::Good)
        } else {
            (format!("Running, not ready{last}"), Health::Pending)
        };
    }
    if let Some(w) = &state.waiting {
        let reason = w.reason.as_deref().unwrap_or("Waiting");
        let health = match reason {
            "ContainerCreating" | "PodInitializing" => Health::Pending,
            _ => Health::Bad,
        };
        return (format!("{reason}{last}"), health);
    }
    if let Some(t) = &state.terminated {
        let reason = t.reason.as_deref().unwrap_or("Exited");
        return if t.exit_code == 0 {
            (format!("Exited 0, {reason}"), Health::Done)
        } else {
            (format!("Exited {}, {reason}", t.exit_code), Health::Bad)
        };
    }
    (DASH.to_owned(), Health::Unknown)
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
        let rows = crate::rows::<Pods>(pods.iter(), now());
        let keys: Vec<String> = rows
            .iter()
            .map(|r| format!("{}/{}", r.namespace.as_deref().unwrap_or_default(), r.name))
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

    #[test]
    fn pod_row_cells_follow_columns() {
        let p = pod(&["app"], "Running", vec![running("app", true, 2)]);
        let row = Pods::row(&p, now());
        let cells: Vec<&str> = row.cells.iter().map(|c| &**c).collect();
        assert_eq!(cells, ["web-0", "1/1", "Running", "2", "3d4h", "node-a"]);
        assert_eq!(row.cells.len(), Pods::columns().len());
        assert_eq!(&*Pods::columns()[2].format, "status");
    }

    #[test]
    fn pod_details_show_containers_and_badges() {
        let mut oom = terminated("app", Some("OOMKilled"), 137);
        oom.restart_count = 14;
        let p = pod(
            &["app", "envoy"],
            "Running",
            vec![oom, running("envoy", true, 0)],
        );
        let d = Pods::details(&p, now());
        assert_eq!(&*d.status, "OOMKilled");
        assert_eq!(d.health, Health::Bad);
        assert!(d.badges.iter().any(|b| &**b == "14 restarts"));
        assert_eq!(d.containers.len(), 2);
        assert_eq!(&*d.containers[0].state, "Exited 137, OOMKilled");
        assert_eq!(d.containers[0].restarts, 14);
        assert_eq!(&*d.containers[1].state, "Running, ready");
        assert!(
            d.fields
                .iter()
                .any(|f| &*f.label == "Node" && &*f.value == "node-a")
        );
        assert!(
            d.fields
                .iter()
                .any(|f| &*f.label == "Created" && &*f.value == "3d4h ago")
        );
        assert_eq!(d.sections[0].title.as_ref(), "Conditions");
    }

    #[test]
    fn crash_loop_container_shows_last_termination() {
        let mut c = waiting("app", "CrashLoopBackOff", 3);
        c.last_state = Some(ContainerState {
            terminated: Some(ContainerStateTerminated {
                reason: Some("Error".to_owned()),
                exit_code: 1,
                ..ContainerStateTerminated::default()
            }),
            ..ContainerState::default()
        });
        let (state, health) = container_state(&c);
        assert_eq!(state, "CrashLoopBackOff (last: Error, exit 1)");
        assert_eq!(health, Health::Bad);
    }
}
