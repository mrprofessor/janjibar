//! Helpers shared by the specs.

use std::collections::BTreeMap;
use std::sync::Arc;

use k8s_openapi::api::core::v1::{Container, PodSpec, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, Time};
use k8s_openapi::jiff::Timestamp;
use model::resource::{ContainerInfo, Details, Field, Kind, ObjectKey, OwnerLink};
use model::{Column, ColumnType, Health, TableRow};

use crate::age::human_age;

/// Shown for missing values.
pub(crate) const DASH: &str = "-";

/// A column for a typed spec. See [`model::Column::format`] for the hints.
pub(crate) fn col(name: &str, ty: ColumnType, format: &str) -> Column {
    Column {
        name: name.into(),
        ty,
        format: format.into(),
        priority: 0,
        description: "".into(),
    }
}

/// A table row from an object's metadata and its cells.
pub(crate) fn row(meta: &ObjectMeta, cells: Vec<Arc<str>>, health: Health) -> TableRow {
    TableRow {
        name: name(meta),
        namespace: meta.namespace.as_deref().map(Arc::from),
        uid: meta.uid.as_deref().unwrap_or_default().into(),
        created: meta
            .creation_timestamp
            .as_ref()
            .map(|t| std::time::SystemTime::from(t.0)),
        cells: cells.into(),
        details: Arc::from([]),
        health,
    }
}

/// The object's key.
pub(crate) fn key(meta: &ObjectMeta) -> ObjectKey {
    ObjectKey {
        namespace: meta.namespace.as_deref().map(Arc::from),
        name: meta.name.as_deref().unwrap_or_default().into(),
    }
}

/// kubectl-style time since `t`.
pub(crate) fn since(t: &Time, now: Timestamp) -> String {
    human_age(now.as_second() - t.0.as_second())
}

/// The object's age, or `-`.
pub(crate) fn age(meta: &ObjectMeta, now: Timestamp) -> Arc<str> {
    meta.creation_timestamp
        .as_ref()
        .map_or_else(|| DASH.into(), |t| since(t, now).into())
}

/// The name, or the empty string.
pub(crate) fn name(meta: &ObjectMeta) -> Arc<str> {
    meta.name.as_deref().unwrap_or_default().into()
}

/// The namespace, or the empty string.
pub(crate) fn namespace(meta: &ObjectMeta) -> Arc<str> {
    meta.namespace.as_deref().unwrap_or_default().into()
}

/// Display text for an optional string.
pub(crate) fn or_dash(value: Option<&str>) -> Arc<str> {
    match value {
        Some(v) if !v.is_empty() => v.into(),
        _ => DASH.into(),
    }
}

/// Joins values with `, `, or `-` when there are none.
pub(crate) fn join<I, S>(values: I) -> Arc<str>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let joined = values
        .into_iter()
        .filter(|v| !v.as_ref().is_empty())
        .map(|v| v.as_ref().to_owned())
        .collect::<Vec<_>>()
        .join(", ");
    if joined.is_empty() {
        DASH.into()
    } else {
        joined.into()
    }
}

/// A number as display text.
pub(crate) fn num(n: impl std::fmt::Display) -> Arc<str> {
    n.to_string().into()
}

/// `ready/total`.
pub(crate) fn ratio(ready: i32, total: i32) -> Arc<str> {
    format!("{ready}/{total}").into()
}

/// Labels sorted by key.
pub(crate) fn labels(meta: &ObjectMeta) -> Vec<(Arc<str>, Arc<str>)> {
    meta.labels
        .iter()
        .flatten()
        .map(|(k, v)| (Arc::from(k.as_str()), Arc::from(v.as_str())))
        .collect()
}

/// Direct owners, controller first.
pub(crate) fn owners(meta: &ObjectMeta) -> Vec<OwnerLink> {
    let mut refs: Vec<_> = meta.owner_references.iter().flatten().collect();
    refs.sort_by_key(|r| r.controller != Some(true));
    refs.into_iter()
        .map(|r| OwnerLink {
            kind: r.kind.as_str().into(),
            name: r.name.as_str().into(),
        })
        .collect()
}

/// A label selector as `key=value, key in (a, b)`.
pub(crate) fn selector(sel: Option<&LabelSelector>) -> Arc<str> {
    let Some(sel) = sel else {
        return DASH.into();
    };
    let mut parts: Vec<String> = sel
        .match_labels
        .iter()
        .flatten()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    for expr in sel.match_expressions.iter().flatten() {
        let values = expr.values.as_deref().unwrap_or_default().join(", ");
        parts.push(match expr.operator.as_str() {
            "In" => format!("{} in ({values})", expr.key),
            "NotIn" => format!("{} notin ({values})", expr.key),
            "Exists" => expr.key.clone(),
            "DoesNotExist" => format!("!{}", expr.key),
            other => format!("{} {other} ({values})", expr.key),
        });
    }
    join(parts)
}

/// A plain `key=value` map, sorted.
pub(crate) fn pairs(map: Option<&BTreeMap<String, String>>) -> Arc<str> {
    join(
        map.iter()
            .flat_map(|m| m.iter())
            .map(|(k, v)| format!("{k}={v}")),
    )
}

/// Details with the fields every object has. Specs add to it.
pub(crate) fn base_details(kind: Kind, meta: &ObjectMeta, now: Timestamp) -> Details {
    let mut fields = Vec::new();
    if let Some(created) = &meta.creation_timestamp {
        fields.push(Field::new(
            "Created",
            format!("{} ago", since(created, now)),
        ));
    }
    Details {
        kind,
        key: key(meta),
        status: DASH.into(),
        health: Health::Unknown,
        badges: Vec::new(),
        fields,
        sections: Vec::new(),
        containers: Vec::new(),
        labels: labels(meta),
        owners: owners(meta),
    }
}

/// Requests and limits as one line.
pub(crate) fn resources(req: Option<&ResourceRequirements>) -> Option<Arc<str>> {
    let req = req?;
    let fmt = |map: Option<&BTreeMap<String, Quantity>>| -> Option<String> {
        let map = map?;
        let parts: Vec<String> = ["cpu", "memory"]
            .into_iter()
            .filter_map(|k| map.get(k).map(|q| format!("{k} {}", q.0)))
            .collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    };
    let mut out = Vec::new();
    if let Some(r) = fmt(req.requests.as_ref()) {
        out.push(format!("requests {r}"));
    }
    if let Some(l) = fmt(req.limits.as_ref()) {
        out.push(format!("limits {l}"));
    }
    (!out.is_empty()).then(|| out.join("; ").into())
}

/// Containers of a pod template, for workload inspectors.
pub(crate) fn template_containers(spec: Option<&PodSpec>) -> Vec<ContainerInfo> {
    let Some(spec) = spec else {
        return Vec::new();
    };
    let init = spec.init_containers.iter().flatten().map(|c| (c, true));
    let main = spec.containers.iter().map(|c| (c, false));
    init.chain(main)
        .map(|(c, init)| template_container(c, init))
        .collect()
}

fn template_container(c: &Container, init: bool) -> ContainerInfo {
    ContainerInfo {
        name: c.name.as_str().into(),
        image: or_dash(c.image.as_deref()),
        init,
        state: if init { "Init container" } else { "Container" }.into(),
        health: Health::Unknown,
        restarts: 0,
        resources: resources(c.resources.as_ref()),
    }
}

/// A "Conditions" section from `(type, status, reason, message)` tuples.
/// Reasons and messages are shown only for conditions that are not `True`.
pub(crate) fn conditions<'a>(
    items: impl IntoIterator<Item = (&'a str, &'a str, Option<&'a str>, Option<&'a str>)>,
) -> Option<model::resource::Section> {
    let fields: Vec<Field> = items
        .into_iter()
        .map(|(kind, status, reason, message)| {
            let mut value = status.to_owned();
            if status != "True" {
                if let Some(r) = reason.filter(|r| !r.is_empty()) {
                    value = format!("{value}, {r}");
                }
                if let Some(m) = message.filter(|m| !m.is_empty()) {
                    value = format!("{value}: {m}");
                }
            }
            Field::new(kind, value)
        })
        .collect();
    (!fields.is_empty()).then(|| model::resource::Section {
        title: "Conditions".into(),
        fields,
    })
}

/// Health for a ready count against a desired count.
pub(crate) fn readiness(ready: i32, desired: i32) -> Health {
    if desired == 0 {
        Health::Done
    } else if ready >= desired {
        Health::Good
    } else if ready == 0 {
        Health::Bad
    } else {
        Health::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{
        LabelSelectorRequirement, OwnerReference,
    };

    #[test]
    fn selector_formats_labels_and_expressions() {
        let sel = LabelSelector {
            match_labels: Some(BTreeMap::from([("app".into(), "web".into())])),
            match_expressions: Some(vec![
                LabelSelectorRequirement {
                    key: "tier".into(),
                    operator: "In".into(),
                    values: Some(vec!["a".into(), "b".into()]),
                },
                LabelSelectorRequirement {
                    key: "canary".into(),
                    operator: "DoesNotExist".into(),
                    values: None,
                },
            ]),
        };
        assert_eq!(&*selector(Some(&sel)), "app=web, tier in (a, b), !canary");
        assert_eq!(&*selector(None), "-");
    }

    #[test]
    fn owners_put_the_controller_first() {
        let meta = ObjectMeta {
            owner_references: Some(vec![
                OwnerReference {
                    kind: "ConfigMap".into(),
                    name: "other".into(),
                    ..OwnerReference::default()
                },
                OwnerReference {
                    kind: "ReplicaSet".into(),
                    name: "web-7d9f".into(),
                    controller: Some(true),
                    ..OwnerReference::default()
                },
            ]),
            ..ObjectMeta::default()
        };
        let owners = owners(&meta);
        let kinds: Vec<&str> = owners.iter().map(|o| &*o.kind).collect();
        assert_eq!(kinds, ["ReplicaSet", "ConfigMap"]);
    }

    #[test]
    fn resources_line() {
        let req = ResourceRequirements {
            requests: Some(BTreeMap::from([("cpu".into(), Quantity("100m".into()))])),
            limits: Some(BTreeMap::from([
                ("cpu".into(), Quantity("500m".into())),
                ("memory".into(), Quantity("512Mi".into())),
            ])),
            ..ResourceRequirements::default()
        };
        assert_eq!(
            resources(Some(&req)).as_deref(),
            Some("requests cpu 100m; limits cpu 500m, memory 512Mi")
        );
        assert_eq!(resources(Some(&ResourceRequirements::default())), None);
    }

    #[test]
    fn readiness_classes() {
        assert_eq!(readiness(3, 3), Health::Good);
        assert_eq!(readiness(1, 3), Health::Pending);
        assert_eq!(readiness(0, 3), Health::Bad);
        assert_eq!(readiness(0, 0), Health::Done);
    }

    #[test]
    fn join_skips_empties() {
        assert_eq!(&*join(["a", "", "b"]), "a, b");
        assert_eq!(&*join(Vec::<String>::new()), "-");
    }
}
