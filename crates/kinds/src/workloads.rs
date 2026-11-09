//! Deployments, StatefulSets, DaemonSets, ReplicaSets, Jobs and CronJobs.

use std::sync::Arc;

use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet};
use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::jiff::Timestamp;
use model::Health;
use model::resource::{Details, Field, Kind, Section};
use model::{Column, ColumnType, TableRow};

use crate::Spec;
use crate::age::human_age;
use crate::common::{self, DASH, col, num, or_dash, ratio, readiness};

fn name_col() -> Column {
    col("Name", ColumnType::String, "name")
}

/// The Deployments spec.
pub struct Deployments;

struct DeployView {
    desired: i32,
    ready: i32,
    updated: i32,
    available: i32,
    status: &'static str,
    health: Health,
}

fn deploy_view(d: &Deployment) -> DeployView {
    let desired = Some(&d.spec).and_then(|s| s.replicas).unwrap_or(1);
    let st = d.status.as_ref();
    let ready = st.and_then(|s| s.ready_replicas).unwrap_or(0);
    let updated = st.and_then(|s| s.updated_replicas).unwrap_or(0);
    let available = st.and_then(|s| s.available_replicas).unwrap_or(0);
    let conditions = st.and_then(|s| s.conditions.as_deref()).unwrap_or_default();
    let stalled = conditions.iter().any(|c| {
        c.type_.as_deref() == Some("Progressing")
            && c.status.as_deref() == Some("False")
            && c.reason.as_deref() == Some("ProgressDeadlineExceeded")
    });
    let replica_failure = conditions.iter().any(|c| {
        c.type_.as_deref() == Some("ReplicaFailure") && c.status.as_deref() == Some("True")
    });
    let (status, health) = if desired == 0 {
        ("Scaled to 0", Health::Done)
    } else if stalled {
        ("Stalled", Health::Bad)
    } else if replica_failure {
        ("ReplicaFailure", Health::Bad)
    } else if available >= desired && updated >= desired {
        ("Available", Health::Good)
    } else if ready == 0 {
        ("Unavailable", Health::Bad)
    } else {
        ("Progressing", Health::Pending)
    };
    DeployView {
        desired,
        ready,
        updated,
        available,
        status,
        health,
    }
}

impl Spec for Deployments {
    type Object = Deployment;
    const KIND: Kind = Kind::Deployment;

    fn columns() -> Vec<Column> {
        let mut cols = vec![name_col()];
        cols.extend([
            col("Ready", ColumnType::String, ""),
            col("Up-to-date", ColumnType::Integer, ""),
            col("Available", ColumnType::Integer, ""),
            col("Status", ColumnType::String, "status"),
            col("Age", ColumnType::Date, ""),
        ]);
        cols
    }

    fn row(d: &Deployment, now: Timestamp) -> TableRow {
        let v = deploy_view(d);
        common::row(
            &d.metadata,
            vec![
                common::name(&d.metadata),
                ratio(v.ready, v.desired),
                num(v.updated),
                num(v.available),
                v.status.into(),
                common::age(&d.metadata, now),
            ],
            v.health,
        )
    }

    fn details(d: &Deployment, now: Timestamp) -> Details {
        let v = deploy_view(d);
        let mut out = common::base_details(Kind::Deployment, &d.metadata, now);
        out.status = v.status.into();
        out.health = v.health;
        out.badges
            .push(format!("{}/{} ready", v.ready, v.desired).into());
        let spec = Some(&d.spec);
        let strategy = spec.and_then(|s| s.strategy.as_ref());
        let strategy_text = if strategy.and_then(|s| s.type_.as_deref()) == Some("Recreate") {
            "Recreate".to_owned()
        } else {
            let ru = strategy.and_then(|s| s.rolling_update.as_ref());
            let fmt = |v: Option<&k8s_openapi::apimachinery::pkg::util::intstr::IntOrString>| {
                v.map_or_else(|| "25%".to_owned(), intstr)
            };
            format!(
                "RollingUpdate, max surge {}, max unavailable {}",
                fmt(ru.and_then(|r| r.max_surge.as_ref())),
                fmt(ru.and_then(|r| r.max_unavailable.as_ref()))
            )
        };
        let mut fields = vec![
            Field::new(
                "Replicas",
                format!(
                    "{} desired, {} updated, {} ready, {} available",
                    v.desired, v.updated, v.ready, v.available
                ),
            ),
            Field::new("Strategy", strategy_text),
            Field::new("Selector", common::selector(spec.map(|s| &s.selector))),
        ];
        fields.append(&mut out.fields);
        out.fields = fields;
        out.containers = common::template_containers(spec.and_then(|s| s.template.spec.as_ref()));
        out.sections.extend(common::conditions(
            d.status
                .as_ref()
                .and_then(|s| s.conditions.as_ref())
                .into_iter()
                .flatten()
                .map(|c| {
                    (
                        c.type_.as_deref().unwrap_or_default(),
                        c.status.as_deref().unwrap_or_default(),
                        c.reason.as_deref(),
                        c.message.as_deref(),
                    )
                }),
        ));
        out
    }
}

fn intstr(v: &k8s_openapi::apimachinery::pkg::util::intstr::IntOrString) -> String {
    use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
    match v {
        IntOrString::Int(n) => n.to_string(),
        IntOrString::String(s) => s.clone(),
    }
}

/// The StatefulSets spec.
pub struct StatefulSets;

fn sts_status(s: &StatefulSet) -> (i32, i32, &'static str, Health) {
    let desired = Some(&s.spec).and_then(|s| s.replicas).unwrap_or(1);
    let st = s.status.as_ref();
    let ready = st.and_then(|s| s.ready_replicas).unwrap_or(0);
    let updated = st.and_then(|s| s.updated_replicas).unwrap_or(0);
    let (text, health) = if desired == 0 {
        ("Scaled to 0", Health::Done)
    } else if ready >= desired && updated >= desired {
        ("Ready", Health::Good)
    } else if ready >= desired {
        ("Updating", Health::Pending)
    } else {
        ("Not ready", readiness(ready, desired))
    };
    (desired, ready, text, health)
}

impl Spec for StatefulSets {
    type Object = StatefulSet;
    const KIND: Kind = Kind::StatefulSet;

    fn columns() -> Vec<Column> {
        let mut cols = vec![name_col()];
        cols.extend([
            col("Ready", ColumnType::String, ""),
            col("Status", ColumnType::String, "status"),
            col("Service", ColumnType::String, "muted"),
            col("Age", ColumnType::Date, ""),
        ]);
        cols
    }

    fn row(s: &StatefulSet, now: Timestamp) -> TableRow {
        let (desired, ready, text, health) = sts_status(s);
        common::row(
            &s.metadata,
            vec![
                common::name(&s.metadata),
                ratio(ready, desired),
                text.into(),
                or_dash(Some(&s.spec).and_then(|s| s.service_name.as_deref())),
                common::age(&s.metadata, now),
            ],
            health,
        )
    }

    fn details(s: &StatefulSet, now: Timestamp) -> Details {
        let (desired, ready, text, health) = sts_status(s);
        let mut out = common::base_details(Kind::StatefulSet, &s.metadata, now);
        out.status = text.into();
        out.health = health;
        out.badges.push(format!("{ready}/{desired} ready").into());
        let spec = Some(&s.spec);
        let st = s.status.as_ref();
        let mut fields = vec![
            Field::new(
                "Replicas",
                format!(
                    "{desired} desired, {} current, {} updated, {ready} ready",
                    st.and_then(|s| s.current_replicas).unwrap_or(0),
                    st.and_then(|s| s.updated_replicas).unwrap_or(0),
                ),
            ),
            Field::new(
                "Service",
                or_dash(spec.and_then(|s| s.service_name.as_deref())),
            ),
            Field::new(
                "Update strategy",
                or_dash(
                    spec.and_then(|s| s.update_strategy.as_ref())
                        .and_then(|u| u.type_.as_deref()),
                ),
            ),
            Field::new(
                "Pod management",
                or_dash(spec.and_then(|s| s.pod_management_policy.as_deref())),
            ),
            Field::new("Selector", common::selector(spec.map(|s| &s.selector))),
        ];
        let claims = common::join(
            spec.and_then(|s| s.volume_claim_templates.as_ref())
                .into_iter()
                .flatten()
                .map(|c| c.metadata.name.clone().unwrap_or_default()),
        );
        if &*claims != DASH {
            fields.push(Field::new("Volume claims", claims));
        }
        fields.append(&mut out.fields);
        out.fields = fields;
        out.containers = common::template_containers(spec.and_then(|s| s.template.spec.as_ref()));
        out
    }
}

/// The DaemonSets spec.
pub struct DaemonSets;

struct DsView {
    desired: i32,
    current: i32,
    ready: i32,
    updated: i32,
    available: i32,
    text: &'static str,
    health: Health,
}

fn ds_view(d: &DaemonSet) -> DsView {
    let st = d.status.as_ref();
    let desired = st.map_or(0, |s| s.desired_number_scheduled);
    let current = st.map_or(0, |s| s.current_number_scheduled);
    let ready = st.map_or(0, |s| s.number_ready);
    let updated = st.and_then(|s| s.updated_number_scheduled).unwrap_or(0);
    let available = st.and_then(|s| s.number_available).unwrap_or(0);
    let (text, health) = if desired == 0 {
        ("No nodes", Health::Done)
    } else if ready >= desired && updated >= desired {
        ("Ready", Health::Good)
    } else if ready >= desired {
        ("Updating", Health::Pending)
    } else {
        ("Not ready", readiness(ready, desired))
    };
    DsView {
        desired,
        current,
        ready,
        updated,
        available,
        text,
        health,
    }
}

impl Spec for DaemonSets {
    type Object = DaemonSet;
    const KIND: Kind = Kind::DaemonSet;

    fn columns() -> Vec<Column> {
        let mut cols = vec![name_col()];
        cols.extend([
            col("Desired", ColumnType::Integer, ""),
            col("Current", ColumnType::Integer, ""),
            col("Ready", ColumnType::Integer, ""),
            col("Up-to-date", ColumnType::Integer, ""),
            col("Available", ColumnType::Integer, ""),
            col("Status", ColumnType::String, "status"),
            col("Age", ColumnType::Date, ""),
        ]);
        cols
    }

    fn row(d: &DaemonSet, now: Timestamp) -> TableRow {
        let v = ds_view(d);
        common::row(
            &d.metadata,
            vec![
                common::name(&d.metadata),
                num(v.desired),
                num(v.current),
                num(v.ready),
                num(v.updated),
                num(v.available),
                v.text.into(),
                common::age(&d.metadata, now),
            ],
            v.health,
        )
    }

    fn details(d: &DaemonSet, now: Timestamp) -> Details {
        let v = ds_view(d);
        let mut out = common::base_details(Kind::DaemonSet, &d.metadata, now);
        out.status = v.text.into();
        out.health = v.health;
        out.badges
            .push(format!("{}/{} ready", v.ready, v.desired).into());
        let spec = Some(&d.spec);
        let misscheduled = d.status.as_ref().map_or(0, |s| s.number_misscheduled);
        let mut fields = vec![
            Field::new(
                "Nodes",
                format!(
                    "{} desired, {} current, {} ready, {} updated, {} available, {misscheduled} misscheduled",
                    v.desired, v.current, v.ready, v.updated, v.available
                ),
            ),
            Field::new(
                "Update strategy",
                or_dash(
                    spec.and_then(|s| s.update_strategy.as_ref())
                        .and_then(|u| u.type_.as_deref()),
                ),
            ),
            Field::new(
                "Node selector",
                common::pairs(
                    spec.and_then(|s| s.template.spec.as_ref())
                        .and_then(|p| p.node_selector.as_ref()),
                ),
            ),
            Field::new("Selector", common::selector(spec.map(|s| &s.selector))),
        ];
        fields.append(&mut out.fields);
        out.fields = fields;
        out.containers = common::template_containers(spec.and_then(|s| s.template.spec.as_ref()));
        out
    }
}

/// The ReplicaSets spec.
pub struct ReplicaSets;

fn rs_status(r: &ReplicaSet) -> (i32, i32, i32, &'static str, Health) {
    let desired = Some(&r.spec).and_then(|s| s.replicas).unwrap_or(1);
    let st = r.status.as_ref();
    let current = st.map_or(0, |s| s.replicas);
    let ready = st.and_then(|s| s.ready_replicas).unwrap_or(0);
    let (text, health) = if desired == 0 {
        ("Scaled to 0", Health::Done)
    } else if ready >= desired {
        ("Ready", Health::Good)
    } else {
        ("Not ready", readiness(ready, desired))
    };
    (desired, current, ready, text, health)
}

impl Spec for ReplicaSets {
    type Object = ReplicaSet;
    const KIND: Kind = Kind::ReplicaSet;

    fn columns() -> Vec<Column> {
        let mut cols = vec![name_col()];
        cols.extend([
            col("Desired", ColumnType::Integer, ""),
            col("Current", ColumnType::Integer, ""),
            col("Ready", ColumnType::Integer, ""),
            col("Status", ColumnType::String, "status"),
            col("Age", ColumnType::Date, ""),
        ]);
        cols
    }

    fn row(r: &ReplicaSet, now: Timestamp) -> TableRow {
        let (desired, current, ready, text, health) = rs_status(r);
        common::row(
            &r.metadata,
            vec![
                common::name(&r.metadata),
                num(desired),
                num(current),
                num(ready),
                text.into(),
                common::age(&r.metadata, now),
            ],
            health,
        )
    }

    fn details(r: &ReplicaSet, now: Timestamp) -> Details {
        let (desired, current, ready, text, health) = rs_status(r);
        let mut out = common::base_details(Kind::ReplicaSet, &r.metadata, now);
        out.status = text.into();
        out.health = health;
        out.badges.push(format!("{ready}/{desired} ready").into());
        let spec = Some(&r.spec);
        let mut fields = vec![
            Field::new(
                "Replicas",
                format!("{desired} desired, {current} current, {ready} ready"),
            ),
            Field::new("Selector", common::selector(spec.map(|s| &s.selector))),
        ];
        fields.append(&mut out.fields);
        out.fields = fields;
        out.containers = common::template_containers(
            spec.and_then(|s| s.template.as_ref())
                .and_then(|t| t.spec.as_ref()),
        );
        out
    }
}

/// The Jobs spec.
pub struct Jobs;

fn job_status(j: &Job) -> (&'static str, Health) {
    let st = j.status.as_ref();
    let has = |kind: &str| {
        st.and_then(|s| s.conditions.as_ref())
            .is_some_and(|cs| cs.iter().any(|c| c.type_ == kind && c.status == "True"))
    };
    if has("Complete") {
        ("Complete", Health::Done)
    } else if has("Failed") {
        ("Failed", Health::Bad)
    } else if j.spec.as_ref().and_then(|s| s.suspend) == Some(true) {
        ("Suspended", Health::Pending)
    } else if st.and_then(|s| s.active).unwrap_or(0) > 0 {
        ("Running", Health::Good)
    } else {
        ("Pending", Health::Pending)
    }
}

fn job_duration(j: &Job, now: Timestamp) -> Arc<str> {
    let st = j.status.as_ref();
    let Some(start) = st.and_then(|s| s.start_time.as_ref()) else {
        return DASH.into();
    };
    let end = st
        .and_then(|s| s.completion_time.as_ref())
        .map_or(now, |t| t.0);
    human_age(end.as_second() - start.0.as_second()).into()
}

impl Spec for Jobs {
    type Object = Job;
    const KIND: Kind = Kind::Job;

    fn columns() -> Vec<Column> {
        let mut cols = vec![name_col()];
        cols.extend([
            col("Completions", ColumnType::String, ""),
            col("Duration", ColumnType::Date, ""),
            col("Status", ColumnType::String, "status"),
            col("Age", ColumnType::Date, ""),
        ]);
        cols
    }

    fn row(j: &Job, now: Timestamp) -> TableRow {
        let (text, health) = job_status(j);
        let completions = j.spec.as_ref().and_then(|s| s.completions).unwrap_or(1);
        let succeeded = j.status.as_ref().and_then(|s| s.succeeded).unwrap_or(0);
        common::row(
            &j.metadata,
            vec![
                common::name(&j.metadata),
                ratio(succeeded, completions),
                job_duration(j, now),
                text.into(),
                common::age(&j.metadata, now),
            ],
            health,
        )
    }

    fn details(j: &Job, now: Timestamp) -> Details {
        let (text, health) = job_status(j);
        let mut out = common::base_details(Kind::Job, &j.metadata, now);
        out.status = text.into();
        out.health = health;
        let spec = j.spec.as_ref();
        let st = j.status.as_ref();
        let failed = st.and_then(|s| s.failed).unwrap_or(0);
        if failed > 0 {
            out.badges.push(format!("{failed} failed").into());
        }
        let mut fields = vec![
            Field::new(
                "Completions",
                format!(
                    "{}/{}",
                    st.and_then(|s| s.succeeded).unwrap_or(0),
                    spec.and_then(|s| s.completions).unwrap_or(1)
                ),
            ),
            Field::new(
                "Parallelism",
                num(spec.and_then(|s| s.parallelism).unwrap_or(1)),
            ),
            Field::new(
                "Pods",
                format!(
                    "{} active, {} succeeded, {failed} failed",
                    st.and_then(|s| s.active).unwrap_or(0),
                    st.and_then(|s| s.succeeded).unwrap_or(0)
                ),
            ),
            Field::new("Duration", job_duration(j, now)),
            Field::new(
                "Backoff limit",
                num(spec.and_then(|s| s.backoff_limit).unwrap_or(6)),
            ),
        ];
        fields.append(&mut out.fields);
        out.fields = fields;
        out.containers = common::template_containers(spec.and_then(|s| s.template.spec.as_ref()));
        out.sections.extend(common::conditions(
            st.and_then(|s| s.conditions.as_ref())
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
        out
    }
}

/// The CronJobs spec.
pub struct CronJobs;

fn cron_status(c: &CronJob) -> (&'static str, Health, usize) {
    let active = c
        .status
        .as_ref()
        .and_then(|s| s.active.as_ref())
        .map_or(0, Vec::len);
    if Some(&c.spec).and_then(|s| s.suspend) == Some(true) {
        ("Suspended", Health::Pending, active)
    } else if active > 0 {
        ("Running", Health::Good, active)
    } else {
        ("Scheduled", Health::Done, active)
    }
}

impl Spec for CronJobs {
    type Object = CronJob;
    const KIND: Kind = Kind::CronJob;

    fn columns() -> Vec<Column> {
        let mut cols = vec![name_col()];
        cols.extend([
            col("Schedule", ColumnType::String, ""),
            col("Active", ColumnType::Integer, ""),
            col("Last schedule", ColumnType::Date, ""),
            col("Status", ColumnType::String, "status"),
            col("Age", ColumnType::Date, ""),
        ]);
        cols
    }

    fn row(c: &CronJob, now: Timestamp) -> TableRow {
        let (text, health, active) = cron_status(c);
        let last = c
            .status
            .as_ref()
            .and_then(|s| s.last_schedule_time.as_ref())
            .map_or_else(|| DASH.into(), |t| common::since(t, now).into());
        common::row(
            &c.metadata,
            vec![
                common::name(&c.metadata),
                c.spec.schedule.as_str().into(),
                num(active),
                last,
                text.into(),
                common::age(&c.metadata, now),
            ],
            health,
        )
    }

    fn details(c: &CronJob, now: Timestamp) -> Details {
        let (text, health, active) = cron_status(c);
        let mut out = common::base_details(Kind::CronJob, &c.metadata, now);
        out.status = text.into();
        out.health = health;
        let spec = Some(&c.spec);
        let st = c.status.as_ref();
        let ago = |t: Option<&k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>| -> Arc<str> {
            t.map_or_else(
                || DASH.into(),
                |t| format!("{} ago", common::since(t, now)).into(),
            )
        };
        let mut fields = vec![
            Field::new(
                "Schedule",
                spec.map_or_else(|| DASH.into(), |s| Arc::from(s.schedule.as_str())),
            ),
            Field::new(
                "Time zone",
                or_dash(spec.and_then(|s| s.time_zone.as_deref())),
            ),
            Field::new(
                "Suspend",
                if spec.and_then(|s| s.suspend) == Some(true) {
                    "Yes"
                } else {
                    "No"
                },
            ),
            Field::new(
                "Concurrency",
                or_dash(spec.and_then(|s| s.concurrency_policy.as_deref())),
            ),
            Field::new(
                "Last schedule",
                ago(st.and_then(|s| s.last_schedule_time.as_ref())),
            ),
            Field::new(
                "Last success",
                ago(st.and_then(|s| s.last_successful_time.as_ref())),
            ),
        ];
        fields.append(&mut out.fields);
        out.fields = fields;
        if active > 0 {
            out.sections.push(Section {
                title: "Active jobs".into(),
                fields: st
                    .and_then(|s| s.active.as_ref())
                    .into_iter()
                    .flatten()
                    .map(|r| Field::new("Job", r.name.as_deref().unwrap_or_default()))
                    .collect(),
            });
        }
        out.containers = common::template_containers(
            spec.and_then(|s| s.job_template.spec.as_ref())
                .and_then(|j| j.template.spec.as_ref()),
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::apps::v1::{DeploymentCondition, DeploymentSpec, DeploymentStatus};
    use k8s_openapi::api::batch::v1::{JobCondition, JobSpec, JobStatus};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};

    fn now() -> Timestamp {
        "2026-11-09T12:00:00Z".parse().unwrap()
    }

    fn meta(name: &str) -> ObjectMeta {
        ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some("shop".to_owned()),
            creation_timestamp: Some(Time("2026-11-08T12:00:00Z".parse().unwrap())),
            ..ObjectMeta::default()
        }
    }

    fn deployment(desired: i32, ready: i32, updated: i32, available: i32) -> Deployment {
        Deployment {
            metadata: meta("web"),
            spec: DeploymentSpec {
                replicas: Some(desired),
                ..DeploymentSpec::default()
            },
            status: Some(DeploymentStatus {
                ready_replicas: Some(ready),
                updated_replicas: Some(updated),
                available_replicas: Some(available),
                ..DeploymentStatus::default()
            }),
        }
    }

    fn cells(row: &TableRow) -> Vec<&str> {
        row.cells.iter().map(|c| &**c).collect()
    }

    #[test]
    fn deployment_rows() {
        let row = Deployments::row(&deployment(3, 3, 3, 3), now());
        assert_eq!(cells(&row), ["web", "3/3", "3", "3", "Available", "24h"]);
        assert_eq!(row.health, Health::Good);
        assert_eq!(row.cells.len(), Deployments::columns().len());

        let rolling = Deployments::row(&deployment(3, 2, 1, 2), now());
        assert_eq!(cells(&rolling)[4], "Progressing");
        assert_eq!(rolling.health, Health::Pending);

        let down = Deployments::row(&deployment(3, 0, 3, 0), now());
        assert_eq!(cells(&down)[4], "Unavailable");
        assert_eq!(down.health, Health::Bad);

        let zero = Deployments::row(&deployment(0, 0, 0, 0), now());
        assert_eq!(cells(&zero)[4], "Scaled to 0");
    }

    #[test]
    fn stalled_deployment_is_bad() {
        let mut d = deployment(2, 1, 2, 1);
        d.status.as_mut().unwrap().conditions = Some(vec![DeploymentCondition {
            type_: Some("Progressing".into()),
            status: Some("False".into()),
            reason: Some("ProgressDeadlineExceeded".into()),
            ..DeploymentCondition::default()
        }]);
        let details = Deployments::details(&d, now());
        assert_eq!(&*details.status, "Stalled");
        assert_eq!(details.health, Health::Bad);
        let conditions = &details.sections[0];
        assert_eq!(
            &*conditions.fields[0].value,
            "False, ProgressDeadlineExceeded"
        );
        assert!(details.fields.iter().any(|f| &*f.label == "Strategy"
            && &*f.value == "RollingUpdate, max surge 25%, max unavailable 25%"));
    }

    #[test]
    fn job_states_and_duration() {
        let mut j = Job {
            metadata: meta("migrate"),
            spec: Some(JobSpec {
                completions: Some(1),
                ..JobSpec::default()
            }),
            status: Some(JobStatus {
                start_time: Some(Time("2026-11-09T11:58:00Z".parse().unwrap())),
                active: Some(1),
                ..JobStatus::default()
            }),
        };
        let row = Jobs::row(&j, now());
        assert_eq!(cells(&row), ["migrate", "0/1", "2m", "Running", "24h"]);

        let status = j.status.as_mut().unwrap();
        status.active = None;
        status.succeeded = Some(1);
        status.completion_time = Some(Time("2026-11-09T11:59:30Z".parse().unwrap()));
        status.conditions = Some(vec![JobCondition {
            type_: "Complete".into(),
            status: "True".into(),
            ..JobCondition::default()
        }]);
        let row = Jobs::row(&j, now());
        assert_eq!(cells(&row), ["migrate", "1/1", "90s", "Complete", "24h"]);
        assert_eq!(row.health, Health::Done);
    }
}
