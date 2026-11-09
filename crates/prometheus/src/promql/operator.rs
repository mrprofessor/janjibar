//! Queries for the Prometheus Operator and OpenShift.
//!
//! Ported from Freelens's `operator-provider` (MIT). Output matches Freelens
//! character for character, spacing included.
//!
//! node-exporter series here carry `pod` and `namespace` labels but no node
//! name. Per-node and node-filtered queries join them with `kube_pod_info`
//! to find the node.

use model::{ClusterMetric, IngressMetric, MetricQuery, NodeMetric, PodMetric, PvcMetric};

use super::{STATUS_2XX, STATUS_5XX, any_of, bytes_sent, escape, node_filter_additive, pods_regex};

/// Joins node-exporter series to their node for the nodes asked for.
///
/// Without a node list there is no join, so the sum covers every node over
/// the time range, including nodes replaced in the meantime.
fn node_join(nodes: &[String]) -> String {
    if nodes.is_empty() {
        String::new()
    } else {
        format!(
            r#" * on (pod,namespace) group_left(node) max without(pod_ip,host_ip) (kube_pod_info{{node=~"{}"}})"#,
            any_of(nodes)
        )
    }
}

pub(super) fn query(rate: &str, query: &MetricQuery, mountpoints: &str) -> String {
    match query {
        MetricQuery::Cluster { metric, nodes } => {
            cluster(rate, *metric, nodes, &escape(mountpoints))
        }
        MetricQuery::Nodes(metric) => node(rate, *metric, &escape(mountpoints)),
        MetricQuery::Pods { metric, pods } => pod(
            rate,
            *metric,
            &pods_regex(&pods.pods),
            &escape(&pods.namespace),
            &pods.group_by.join(", "),
        ),
        MetricQuery::Pvc {
            metric,
            namespace,
            name,
        } => pvc(*metric, &escape(name), &escape(namespace)),
        MetricQuery::Ingress {
            metric,
            namespace,
            name,
        } => ingress(rate, *metric, &escape(name), &escape(namespace)),
    }
}

fn cluster(rate: &str, metric: ClusterMetric, nodes: &[String], mp: &str) -> String {
    let on = |label: &str| node_filter_additive(nodes, label);
    match metric {
        ClusterMetric::MemoryUsage => {
            let j = node_join(nodes);
            format!(
                "sum(node_memory_MemTotal_bytes{j} - (node_memory_MemFree_bytes{j} + node_memory_Buffers_bytes{j} + node_memory_Cached_bytes{j}))"
            )
        }
        ClusterMetric::WorkloadMemoryUsage => format!(
            r#"sum(container_memory_working_set_bytes{{container!="",image!=""{}}}) by (component)"#,
            on("instance")
        ),
        ClusterMetric::MemoryRequests => format!(
            r#"sum(kube_pod_container_resource_requests{{resource="memory"{}}})"#,
            on("node")
        ),
        ClusterMetric::MemoryLimits => format!(
            r#"sum(kube_pod_container_resource_limits{{resource="memory"{}}})"#,
            on("node")
        ),
        ClusterMetric::MemoryCapacity => format!(
            r#"sum(kube_node_status_capacity{{resource="memory"{}}})"#,
            on("node")
        ),
        ClusterMetric::MemoryAllocatableCapacity => format!(
            r#"sum(kube_node_status_allocatable{{resource="memory"{}}})"#,
            on("node")
        ),
        ClusterMetric::CpuUsage => format!(
            r#"sum(rate(node_cpu_seconds_total{{mode=~"user|system"}}[{rate}]){})"#,
            node_join(nodes)
        ),
        ClusterMetric::CpuRequests => format!(
            r#"sum(kube_pod_container_resource_requests{{resource="cpu"{}}})"#,
            on("node")
        ),
        ClusterMetric::CpuLimits => format!(
            r#"sum(kube_pod_container_resource_limits{{resource="cpu"{}}})"#,
            on("node")
        ),
        ClusterMetric::CpuCapacity => format!(
            r#"sum(kube_node_status_capacity{{resource="cpu"{}}})"#,
            on("node")
        ),
        ClusterMetric::CpuAllocatableCapacity => format!(
            r#"sum(kube_node_status_allocatable{{resource="cpu"{}}})"#,
            on("node")
        ),
        ClusterMetric::PodUsage => format!(
            r#"sum({{__name__=~"kubelet_running_pod_count|kubelet_running_pods"{}}})"#,
            on("node")
        ),
        ClusterMetric::PodCapacity => format!(
            r#"sum(kube_node_status_capacity{{resource="pods"{}}})"#,
            on("node")
        ),
        ClusterMetric::PodAllocatableCapacity => format!(
            r#"sum(kube_node_status_allocatable{{resource="pods"{}}})"#,
            on("node")
        ),
        ClusterMetric::FsSize => format!(
            r#"sum(node_filesystem_size_bytes{{mountpoint=~"{mp}"}}{})"#,
            node_join(nodes)
        ),
        ClusterMetric::FsUsage => {
            let j = node_join(nodes);
            format!(
                r#"sum(node_filesystem_size_bytes{{mountpoint=~"{mp}"}}{j} - node_filesystem_avail_bytes{{mountpoint=~"{mp}"}}{j})"#
            )
        }
    }
}

fn node(rate: &str, metric: NodeMetric, mp: &str) -> String {
    match metric {
        NodeMetric::MemoryUsage => {
            "sum((node_memory_MemTotal_bytes - (node_memory_MemFree_bytes + node_memory_Buffers_bytes + node_memory_Cached_bytes)) * on (pod, namespace) group_left(node) max without(pod_ip,host_ip) (kube_pod_info)) by (node)"
                .to_owned()
        }
        NodeMetric::WorkloadMemoryUsage => {
            r#"sum(container_memory_working_set_bytes{container!="POD", container!=""}) by (node)"#
                .to_owned()
        }
        NodeMetric::MemoryCapacity => {
            r#"sum(kube_node_status_capacity{resource="memory"}) by (node)"#.to_owned()
        }
        NodeMetric::MemoryAllocatableCapacity => {
            r#"sum(kube_node_status_allocatable{resource="memory"}) by (node)"#.to_owned()
        }
        NodeMetric::CpuUsage => format!(
            r#"sum(rate(node_cpu_seconds_total{{mode=~"user|system"}}[{rate}]) * on (pod, namespace) group_left(node) max without(pod_ip,host_ip) (kube_pod_info)) by (node)"#
        ),
        // Freelens reads allocatable CPU for capacity too.
        NodeMetric::CpuCapacity | NodeMetric::CpuAllocatableCapacity => {
            r#"sum(kube_node_status_allocatable{resource="cpu"}) by (node)"#.to_owned()
        }
        NodeMetric::FsSize => format!(
            r#"sum(node_filesystem_size_bytes{{mountpoint=~"{mp}"}} * on (pod,namespace) group_left(node) max without(pod_ip,host_ip) (kube_pod_info)) by (node)"#
        ),
        NodeMetric::FsUsage => format!(
            r#"sum((node_filesystem_size_bytes{{mountpoint=~"{mp}"}} - node_filesystem_avail_bytes{{mountpoint=~"{mp}"}}) * on (pod, namespace) group_left(node) max without(pod_ip,host_ip) (kube_pod_info)) by (node)"#
        ),
    }
}

fn pod(rate: &str, metric: PodMetric, pods: &str, ns: &str, by: &str) -> String {
    match metric {
        PodMetric::CpuUsage => format!(
            r#"sum(rate(container_cpu_usage_seconds_total{{container!="",image!="", pod=~"{pods}", namespace="{ns}"}}[{rate}])) by ({by})"#
        ),
        PodMetric::CpuRequests => format!(
            r#"sum(kube_pod_container_resource_requests{{pod=~"{pods}", resource="cpu", namespace="{ns}"}}) by ({by})"#
        ),
        PodMetric::CpuLimits => format!(
            r#"sum(kube_pod_container_resource_limits{{pod=~"{pods}", resource="cpu", namespace="{ns}"}}) by ({by})"#
        ),
        PodMetric::MemoryUsage => format!(
            r#"sum(container_memory_working_set_bytes{{container!="",image!="", pod=~"{pods}", namespace="{ns}"}}) by ({by})"#
        ),
        PodMetric::MemoryRequests => format!(
            r#"sum(kube_pod_container_resource_requests{{pod=~"{pods}", resource="memory", namespace="{ns}"}}) by ({by})"#
        ),
        PodMetric::MemoryLimits => format!(
            r#"sum(kube_pod_container_resource_limits{{pod=~"{pods}", resource="memory", namespace="{ns}"}}) by ({by})"#
        ),
        PodMetric::FsUsage => format!(
            r#"sum(container_fs_usage_bytes{{container!="",image!="", pod=~"{pods}", namespace="{ns}"}}) by ({by})"#
        ),
        PodMetric::FsWrites => format!(
            r#"sum(rate(container_fs_writes_bytes_total{{container!="",image!="", pod=~"{pods}", namespace="{ns}"}}[{rate}])) by ({by})"#
        ),
        PodMetric::FsReads => format!(
            r#"sum(rate(container_fs_reads_bytes_total{{container!="",image!="", pod=~"{pods}", namespace="{ns}"}}[{rate}])) by ({by})"#
        ),
        PodMetric::NetworkReceive => format!(
            r#"sum(rate(container_network_receive_bytes_total{{pod=~"{pods}", namespace="{ns}"}}[{rate}])) by ({by})"#
        ),
        PodMetric::NetworkTransmit => format!(
            r#"sum(rate(container_network_transmit_bytes_total{{pod=~"{pods}", namespace="{ns}"}}[{rate}])) by ({by})"#
        ),
    }
}

fn pvc(metric: PvcMetric, pvc: &str, ns: &str) -> String {
    match metric {
        PvcMetric::DiskUsage => format!(
            r#"sum(kubelet_volume_stats_used_bytes{{persistentvolumeclaim="{pvc}", namespace="{ns}"}}) by (persistentvolumeclaim, namespace)"#
        ),
        PvcMetric::DiskCapacity => format!(
            r#"sum(kubelet_volume_stats_capacity_bytes{{persistentvolumeclaim="{pvc}", namespace="{ns}"}}) by (persistentvolumeclaim, namespace)"#
        ),
    }
}

fn ingress(rate: &str, metric: IngressMetric, ingress: &str, ns: &str) -> String {
    match metric {
        IngressMetric::BytesSentSuccess => bytes_sent(rate, ingress, ns, STATUS_2XX),
        IngressMetric::BytesSentFailure => bytes_sent(rate, ingress, ns, STATUS_5XX),
        IngressMetric::RequestDurationSeconds => format!(
            r#"sum(rate(nginx_ingress_controller_request_duration_seconds_sum{{ingress="{ingress}", namespace="{ns}"}}[{rate}])) by (ingress, namespace)"#
        ),
        IngressMetric::ResponseDurationSeconds => format!(
            r#"sum(rate(nginx_ingress_controller_response_duration_seconds_sum{{ingress="{ingress}", namespace="{ns}"}}[{rate}])) by (ingress, namespace)"#
        ),
    }
}
