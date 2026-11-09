//! Reading `metrics.k8s.io/v1beta1`, the API metrics-server serves.
//!
//! The responses are small and typed here rather than through a crate that
//! pins its own k8s-openapi. A pod's usage is the sum of its containers, as
//! `kubectl top pod` shows it.

use std::collections::BTreeMap;
use std::error::Error as _;
use std::sync::Arc;
use std::time::SystemTime;

use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
use model::{Sample, Usage, UsageObject, UsageScope, UsageStatus};
use serde::Deserialize;

use crate::quantity::parse_quantity;

/// Where the API lives.
const GROUP_PATH: &str = "/apis/metrics.k8s.io/v1beta1";

#[derive(Deserialize)]
struct List<T> {
    /// Missing or `null` when nothing matched.
    #[serde(default = "Option::default")]
    items: Option<Vec<T>>,
}

#[derive(Deserialize)]
struct NodeMetrics {
    metadata: ObjectMeta,
    timestamp: Option<Time>,
    #[serde(default)]
    usage: BTreeMap<String, Quantity>,
}

#[derive(Deserialize)]
struct PodMetrics {
    metadata: ObjectMeta,
    timestamp: Option<Time>,
    #[serde(default)]
    containers: Vec<ContainerMetrics>,
}

#[derive(Deserialize)]
struct ContainerMetrics {
    #[serde(default)]
    usage: BTreeMap<String, Quantity>,
}

/// The list path for a scope.
pub(crate) fn path(scope: &UsageScope) -> String {
    match scope {
        UsageScope::Nodes => format!("{GROUP_PATH}/nodes"),
        UsageScope::Pods(None) => format!("{GROUP_PATH}/pods"),
        UsageScope::Pods(Some(namespace)) => format!("{GROUP_PATH}/namespaces/{namespace}/pods"),
    }
}

/// Lists the usage of everything in `scope`.
pub(crate) async fn fetch(
    client: &kube::Client,
    scope: &UsageScope,
) -> Result<Vec<(UsageObject, Usage)>, kube::Error> {
    let request = http::Request::get(path(scope))
        .header(http::header::ACCEPT, "application/json")
        .body(Vec::new())
        .map_err(kube::Error::HttpError)?;
    let text = client.request_text(request).await?;
    parse(scope, &text).map_err(kube::Error::SerdeError)
}

/// Parses a list response for `scope`.
pub(crate) fn parse(
    scope: &UsageScope,
    text: &str,
) -> Result<Vec<(UsageObject, Usage)>, serde_json::Error> {
    // Without a timestamp (some adapters omit it) the sample is "now".
    let time = |t: Option<Time>| t.map_or_else(SystemTime::now, |t| SystemTime::from(t.0));
    Ok(match scope {
        UsageScope::Nodes => serde_json::from_str::<List<NodeMetrics>>(text)?
            .items
            .unwrap_or_default()
            .into_iter()
            .filter_map(|node| {
                let name = node.metadata.name?;
                let time = time(node.timestamp);
                let usage = usage(
                    time,
                    sum(&[&node.usage], "cpu"),
                    sum(&[&node.usage], "memory"),
                );
                Some((UsageObject::Node(name.into()), usage))
            })
            .collect(),
        UsageScope::Pods(_) => serde_json::from_str::<List<PodMetrics>>(text)?
            .items
            .unwrap_or_default()
            .into_iter()
            .filter_map(|pod| {
                let namespace: Arc<str> = pod.metadata.namespace?.into();
                let name: Arc<str> = pod.metadata.name?.into();
                let time = time(pod.timestamp);
                let containers: Vec<&BTreeMap<String, Quantity>> =
                    pod.containers.iter().map(|c| &c.usage).collect();
                let usage = usage(time, sum(&containers, "cpu"), sum(&containers, "memory"));
                Some((UsageObject::Pod { namespace, name }, usage))
            })
            .collect(),
    })
}

fn usage(time: SystemTime, cpu: f64, memory: f64) -> Usage {
    Usage {
        cpu: Sample { time, value: cpu },
        memory: Sample {
            time,
            value: memory,
        },
    }
}

/// Adds up one resource across usage maps. Missing or unreadable entries
/// count as zero.
fn sum(maps: &[&BTreeMap<String, Quantity>], resource: &str) -> f64 {
    maps.iter()
        .filter_map(|map| map.get(resource))
        .filter_map(|quantity| {
            let value = parse_quantity(&quantity.0);
            if value.is_none() {
                tracing::debug!(resource, quantity = %quantity.0, "unreadable quantity");
            }
            value
        })
        .sum()
}

/// What a failed list means for the view.
pub(crate) fn status_from(err: &kube::Error) -> UsageStatus {
    match err {
        kube::Error::Api(status) if status.code == 403 => {
            UsageStatus::Forbidden(status.message.clone())
        }
        // No such API group: metrics-server is not installed. 503 means it is
        // registered but its pods are not answering.
        kube::Error::Api(status) if matches!(status.code, 404 | 503) => {
            UsageStatus::Unavailable(status.message.clone())
        }
        kube::Error::Api(status) => {
            UsageStatus::Failed(format!("{}: {}", status.code, status.message))
        }
        other => UsageStatus::Failed(root_cause(other)),
    }
}

/// The innermost error message, which usually says what happened.
fn root_cause(err: &kube::Error) -> String {
    let mut message = err.to_string();
    let mut current = err.source();
    while let Some(e) = current {
        message = e.to_string();
        current = e.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A trimmed `kubectl get --raw /apis/metrics.k8s.io/v1beta1/pods` answer.
    const PODS: &str = r#"{
        "kind": "PodMetricsList",
        "apiVersion": "metrics.k8s.io/v1beta1",
        "metadata": {},
        "items": [{
            "metadata": { "name": "web-1", "namespace": "shop", "creationTimestamp": "2026-11-09T01:00:15Z" },
            "timestamp": "2026-11-09T01:00:00Z",
            "window": "15.012s",
            "containers": [
                { "name": "app", "usage": { "cpu": "250000000n", "memory": "100Mi" } },
                { "name": "envoy", "usage": { "cpu": "5m", "memory": "28Mi" } }
            ]
        }]
    }"#;

    const NODES: &str = r#"{
        "kind": "NodeMetricsList",
        "apiVersion": "metrics.k8s.io/v1beta1",
        "metadata": {},
        "items": [{
            "metadata": { "name": "node-a", "labels": { "kubernetes.io/os": "linux" } },
            "timestamp": "2026-11-09T01:00:00Z",
            "window": "20.03s",
            "usage": { "cpu": "1234567890n", "memory": "2097152Ki" }
        }]
    }"#;

    #[expect(
        clippy::duration_suboptimal_units,
        reason = "a Unix time reads best in seconds"
    )]
    fn at_one_am() -> SystemTime {
        // 2026-11-09T01:00:00Z
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_794_186_000)
    }

    #[test]
    fn a_pod_is_the_sum_of_its_containers() {
        let items = parse(&UsageScope::Pods(None), PODS).unwrap();
        let (object, usage) = &items[0];
        assert_eq!(
            object,
            &UsageObject::Pod {
                namespace: "shop".into(),
                name: "web-1".into(),
            }
        );
        assert!((usage.cpu.value - 0.255).abs() < 1e-9);
        assert_eq!(usage.memory.value, 128.0 * 1024.0 * 1024.0);
        assert_eq!(usage.cpu.time, at_one_am());
    }

    #[test]
    fn reads_nodes() {
        let items = parse(&UsageScope::Nodes, NODES).unwrap();
        let (object, usage) = &items[0];
        assert_eq!(object, &UsageObject::Node("node-a".into()));
        assert!((usage.cpu.value - 1.234_567_89).abs() < 1e-9);
        assert_eq!(usage.memory.value, 2.0 * 1024.0 * 1024.0 * 1024.0);
        assert_eq!(usage.memory.time, at_one_am());
    }

    #[test]
    fn an_empty_list_has_no_items() {
        for text in [
            r#"{"kind":"PodMetricsList","metadata":{},"items":[]}"#,
            r#"{"kind":"PodMetricsList","metadata":{},"items":null}"#,
            r#"{"kind":"PodMetricsList","metadata":{}}"#,
        ] {
            assert_eq!(parse(&UsageScope::Pods(None), text).unwrap(), [], "{text}");
        }
    }

    #[test]
    fn paths_follow_the_scope() {
        assert_eq!(
            path(&UsageScope::Nodes),
            "/apis/metrics.k8s.io/v1beta1/nodes"
        );
        assert_eq!(
            path(&UsageScope::Pods(None)),
            "/apis/metrics.k8s.io/v1beta1/pods"
        );
        assert_eq!(
            path(&UsageScope::Pods(Some("shop".into()))),
            "/apis/metrics.k8s.io/v1beta1/namespaces/shop/pods"
        );
    }
}
