//! Nodes, Namespaces and Events.

use std::sync::Arc;

use k8s_openapi::api::core::v1::{Event, Namespace, Node};
use k8s_openapi::jiff::Timestamp;
use model::Health;
use model::resource::{Details, Field, Kind, ObjectKey, Section};
use model::{Column, ColumnType, TableRow};

use crate::Spec;
use crate::common::{self, DASH, col, join, num, or_dash};

/// The Nodes spec.
pub struct Nodes;

fn node_status(n: &Node) -> (Arc<str>, Health) {
    let conditions = n
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_deref())
        .unwrap_or_default();
    let ready = conditions.iter().find(|c| c.type_ == "Ready");
    let unschedulable = n.spec.as_ref().and_then(|s| s.unschedulable) == Some(true);
    let pressure: Vec<&str> = conditions
        .iter()
        .filter(|c| c.type_ != "Ready" && c.status == "True")
        .map(|c| c.type_.as_str())
        .collect();
    let (mut text, mut health) = match ready.map(|c| c.status.as_str()) {
        Some("True") => ("Ready".to_owned(), Health::Good),
        Some("False") => ("NotReady".to_owned(), Health::Bad),
        _ => ("Unknown".to_owned(), Health::Unknown),
    };
    if !pressure.is_empty() {
        text = format!("{text}, {}", pressure.join(", "));
        if health == Health::Good {
            health = Health::Pending;
        }
    }
    if unschedulable {
        text.push_str(", SchedulingDisabled");
        if health == Health::Good {
            health = Health::Pending;
        }
    }
    (text.into(), health)
}

fn roles(n: &Node) -> Arc<str> {
    let mut roles: Vec<&str> = n
        .metadata
        .labels
        .iter()
        .flatten()
        .filter_map(|(k, _)| k.strip_prefix("node-role.kubernetes.io/"))
        .filter(|r| !r.is_empty())
        .collect();
    roles.sort_unstable();
    if roles.is_empty() {
        "<none>".into()
    } else {
        roles.join(", ").into()
    }
}

fn internal_ip(n: &Node) -> Arc<str> {
    or_dash(
        n.status
            .as_ref()
            .and_then(|s| s.addresses.as_ref())
            .and_then(|a| a.iter().find(|a| a.type_ == "InternalIP"))
            .map(|a| a.address.as_str()),
    )
}

impl Spec for Nodes {
    type Object = Node;
    const KIND: Kind = Kind::Node;

    fn columns() -> Vec<Column> {
        vec![
            col("Name", ColumnType::String, "name"),
            col("Status", ColumnType::String, "status"),
            col("Roles", ColumnType::String, "muted"),
            col("Version", ColumnType::String, ""),
            col("Internal IP", ColumnType::String, ""),
            col("Age", ColumnType::Date, ""),
        ]
    }

    fn row(n: &Node, now: Timestamp) -> TableRow {
        let (status, health) = node_status(n);
        let info = n.status.as_ref().and_then(|s| s.node_info.as_ref());
        common::row(
            &n.metadata,
            vec![
                common::name(&n.metadata),
                status,
                roles(n),
                or_dash(info.map(|i| i.kubelet_version.as_str())),
                internal_ip(n),
                common::age(&n.metadata, now),
            ],
            health,
        )
    }

    fn details(n: &Node, now: Timestamp) -> Details {
        let (status, health) = node_status(n);
        let mut out = common::base_details(Kind::Node, &n.metadata, now);
        out.status = status;
        out.health = health;
        let st = n.status.as_ref();
        let info = st.and_then(|s| s.node_info.as_ref());
        let quantity = |map: Option<
            &std::collections::BTreeMap<
                String,
                k8s_openapi::apimachinery::pkg::api::resource::Quantity,
            >,
        >|
         -> Arc<str> {
            let Some(map) = map else {
                return DASH.into();
            };
            join(
                ["cpu", "memory", "pods"]
                    .into_iter()
                    .filter_map(|k| map.get(k).map(|q| format!("{k} {}", q.0))),
            )
        };
        let mut fields = vec![
            Field::new("Roles", roles(n)),
            Field::new("Internal IP", internal_ip(n)),
            Field::new("Kubelet", or_dash(info.map(|i| i.kubelet_version.as_str()))),
            Field::new("OS image", or_dash(info.map(|i| i.os_image.as_str()))),
            Field::new("Kernel", or_dash(info.map(|i| i.kernel_version.as_str()))),
            Field::new(
                "Runtime",
                or_dash(info.map(|i| i.container_runtime_version.as_str())),
            ),
            Field::new("Capacity", quantity(st.and_then(|s| s.capacity.as_ref()))),
            Field::new(
                "Allocatable",
                quantity(st.and_then(|s| s.allocatable.as_ref())),
            ),
            Field::new(
                "Pod CIDR",
                or_dash(n.spec.as_ref().and_then(|s| s.pod_cidr.as_deref())),
            ),
        ];
        fields.append(&mut out.fields);
        out.fields = fields;
        let taints: Vec<Field> = n
            .spec
            .as_ref()
            .and_then(|s| s.taints.as_ref())
            .into_iter()
            .flatten()
            .map(|t| {
                let value = match t.value.as_deref() {
                    Some(v) if !v.is_empty() => format!("{v}:{}", t.effect),
                    _ => t.effect.clone(),
                };
                Field::new(t.key.as_str(), value)
            })
            .collect();
        if !taints.is_empty() {
            out.sections.push(Section {
                title: "Taints".into(),
                fields: taints,
            });
        }
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

/// The Namespaces spec.
pub struct Namespaces;

fn ns_status(n: &Namespace) -> (Arc<str>, Health) {
    match n.status.as_ref().and_then(|s| s.phase.as_deref()) {
        Some("Active") | None => ("Active".into(), Health::Good),
        Some("Terminating") => ("Terminating".into(), Health::Pending),
        Some(other) => (other.into(), Health::Unknown),
    }
}

impl Spec for Namespaces {
    type Object = Namespace;
    const KIND: Kind = Kind::Namespace;

    fn columns() -> Vec<Column> {
        vec![
            col("Name", ColumnType::String, "name"),
            col("Status", ColumnType::String, "status"),
            col("Age", ColumnType::Date, ""),
        ]
    }

    fn row(n: &Namespace, now: Timestamp) -> TableRow {
        let (status, health) = ns_status(n);
        common::row(
            &n.metadata,
            vec![
                common::name(&n.metadata),
                status,
                common::age(&n.metadata, now),
            ],
            health,
        )
    }

    fn details(n: &Namespace, now: Timestamp) -> Details {
        let (status, health) = ns_status(n);
        let mut out = common::base_details(Kind::Namespace, &n.metadata, now);
        out.status = status;
        out.health = health;
        out
    }
}

/// The Events spec. Newest first.
pub struct Events;

/// When the event last happened, as Unix seconds.
fn last_seen(e: &Event) -> Option<i64> {
    e.last_timestamp
        .as_ref()
        .map(|t| t.0)
        .or_else(|| e.event_time.as_ref().map(|t| t.0))
        .or_else(|| e.metadata.creation_timestamp.as_ref().map(|t| t.0))
        .map(Timestamp::as_second)
}

fn event_object(e: &Event) -> Arc<str> {
    let o = &e.involved_object;
    format!(
        "{}/{}",
        o.kind.as_deref().unwrap_or("?").to_lowercase(),
        o.name.as_deref().unwrap_or("?")
    )
    .into()
}

fn event_health(e: &Event) -> Health {
    if e.type_.as_deref() == Some("Warning") {
        Health::Bad
    } else {
        Health::Unknown
    }
}

impl Spec for Events {
    type Object = Event;
    const KIND: Kind = Kind::Event;

    fn columns() -> Vec<Column> {
        vec![
            col("Last seen", ColumnType::Date, ""),
            col("Type", ColumnType::String, "status"),
            col("Reason", ColumnType::String, ""),
            col("Object", ColumnType::String, "name"),
            col("Message", ColumnType::String, "muted"),
            col("Count", ColumnType::Integer, ""),
        ]
    }

    fn row(e: &Event, now: Timestamp) -> TableRow {
        let seen = last_seen(e).map_or_else(
            || DASH.into(),
            |t| crate::age::human_age(now.as_second() - t).into(),
        );
        common::row(
            &e.metadata,
            vec![
                seen,
                or_dash(e.type_.as_deref()),
                or_dash(e.reason.as_deref()),
                event_object(e),
                or_dash(e.message.as_deref().map(|m| m.lines().next().unwrap_or(m))),
                num(e.count.unwrap_or(1)),
            ],
            event_health(e),
        )
    }

    fn details(e: &Event, now: Timestamp) -> Details {
        let mut out = common::base_details(Kind::Event, &e.metadata, now);
        out.status = or_dash(e.reason.as_deref());
        out.health = event_health(e);
        if let Some(n) = e.count.filter(|n| *n > 1) {
            out.badges.push(format!("{n} times").into());
        }
        let source = e
            .source
            .as_ref()
            .and_then(|s| s.component.as_deref())
            .or(e.reporting_component.as_deref());
        let mut fields = vec![
            Field::new("Type", or_dash(e.type_.as_deref())),
            Field::new("Object", event_object(e)),
            Field::new("Message", or_dash(e.message.as_deref())),
            Field::new("Source", or_dash(source)),
            Field::new(
                "Last seen",
                last_seen(e).map_or_else(
                    || DASH.into(),
                    |t| format!("{} ago", crate::age::human_age(now.as_second() - t)),
                ),
            ),
        ];
        fields.append(&mut out.fields);
        out.fields = fields;
        out
    }

    fn recency(e: &Event) -> Option<i64> {
        last_seen(e)
    }
}

/// The key of the object an event is about, when it is one of ours.
pub fn event_subject(e: &Event) -> Option<(Kind, ObjectKey)> {
    let o = &e.involved_object;
    let kind = Kind::from_singular(o.kind.as_deref()?)?;
    let name: Arc<str> = o.name.as_deref()?.into();
    Some((
        kind,
        ObjectKey {
            namespace: o.namespace.as_deref().map(Arc::from),
            name,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{NodeCondition, NodeSpec, NodeStatus, ObjectReference};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
    use std::collections::BTreeMap;

    fn now() -> Timestamp {
        "2026-11-09T12:00:00Z".parse().unwrap()
    }

    fn node(ready: &str, extra: &[(&str, &str)], unschedulable: bool) -> Node {
        let mut conditions = vec![NodeCondition {
            type_: "Ready".into(),
            status: ready.into(),
            ..NodeCondition::default()
        }];
        for (kind, status) in extra {
            conditions.push(NodeCondition {
                type_: (*kind).into(),
                status: (*status).into(),
                ..NodeCondition::default()
            });
        }
        Node {
            metadata: ObjectMeta {
                name: Some("worker-1".into()),
                labels: Some(BTreeMap::from([
                    ("node-role.kubernetes.io/worker".into(), String::new()),
                    ("node-role.kubernetes.io/infra".into(), String::new()),
                ])),
                ..ObjectMeta::default()
            },
            spec: Some(NodeSpec {
                unschedulable: Some(unschedulable),
                ..NodeSpec::default()
            }),
            status: Some(NodeStatus {
                conditions: Some(conditions),
                ..NodeStatus::default()
            }),
        }
    }

    #[test]
    fn node_states() {
        let (s, h) = node_status(&node("True", &[("DiskPressure", "False")], false));
        assert_eq!((&*s, h), ("Ready", Health::Good));
        let (s, h) = node_status(&node("True", &[("DiskPressure", "True")], false));
        assert_eq!((&*s, h), ("Ready, DiskPressure", Health::Pending));
        let (s, h) = node_status(&node("True", &[], true));
        assert_eq!((&*s, h), ("Ready, SchedulingDisabled", Health::Pending));
        let (s, h) = node_status(&node("False", &[], false));
        assert_eq!((&*s, h), ("NotReady", Health::Bad));
        assert_eq!(&*roles(&node("True", &[], false)), "infra, worker");
    }

    fn event(name: &str, last: &str, kind: &str) -> Event {
        Event {
            metadata: ObjectMeta {
                name: Some(name.into()),
                namespace: Some("faults".into()),
                ..ObjectMeta::default()
            },
            involved_object: ObjectReference {
                kind: Some("Pod".into()),
                name: Some("crashloop-0".into()),
                namespace: Some("faults".into()),
                ..ObjectReference::default()
            },
            type_: Some(kind.into()),
            reason: Some("BackOff".into()),
            message: Some("Back-off restarting failed container\nmore".into()),
            count: Some(7),
            last_timestamp: Some(Time(last.parse().unwrap())),
            ..Event::default()
        }
    }

    #[test]
    fn events_sort_newest_first_and_show_one_line() {
        let events = [
            event("a", "2026-11-09T11:00:00Z", "Normal"),
            event("b", "2026-11-09T11:59:00Z", "Warning"),
        ];
        let rows = crate::rows::<Events>(events.iter(), now());
        assert_eq!(&*rows[0].name, "b");
        let cells: Vec<&str> = rows[0].cells.iter().map(|c| &**c).collect();
        assert_eq!(
            cells,
            [
                "60s",
                "Warning",
                "BackOff",
                "pod/crashloop-0",
                "Back-off restarting failed container",
                "7"
            ]
        );
        assert_eq!(rows[0].health, Health::Bad);
        assert_eq!(
            event_subject(&events[0]),
            Some((Kind::Pod, ObjectKey::namespaced("faults", "crashloop-0")))
        );
    }
}
