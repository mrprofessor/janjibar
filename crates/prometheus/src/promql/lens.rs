//! Queries for Lens, Helm 14, Helm and Stacklight.
//!
//! Ported from Freelens's `lens-provider`, `helm-provider` and
//! `stacklight-provider` (MIT). The three are the same apart from the
//! fields of [`LensLike`]. Output matches Freelens character for character,
//! spacing included, so a diff against upstream stays readable.

use model::{ClusterMetric, IngressMetric, MetricQuery, NodeMetric, PodMetric, PvcMetric};

use super::{
    STATUS_2XX, STATUS_5XX, bytes_sent, escape, node_filter, node_filter_additive, pods_regex,
};

/// What differs between the providers in this family.
pub(super) struct LensLike {
    /// The label that names the node on node-exporter series.
    node: &'static str,
    /// What whole-cluster memory use is grouped by.
    cluster_memory_by: &'static str,
    /// Freelens writes `sum (` in one node query for Lens and Stacklight.
    nodes_memory_sum: &'static str,
}

/// The Lens metrics stack (also the old Helm 14 chart's queries).
pub(super) const LENS: LensLike = LensLike {
    node: "kubernetes_node",
    cluster_memory_by: "kubernetes_name",
    nodes_memory_sum: "sum ",
};

/// The `prometheus-community/prometheus` chart.
pub(super) const HELM: LensLike = LensLike {
    node: "node",
    cluster_memory_by: "component",
    nodes_memory_sum: "sum",
};

/// Mirantis Stacklight.
pub(super) const STACKLIGHT: LensLike = LensLike {
    node: "node",
    cluster_memory_by: "kubernetes_name",
    nodes_memory_sum: "sum ",
};

pub(super) fn query(d: &LensLike, rate: &str, query: &MetricQuery, mountpoints: &str) -> String {
    match query {
        MetricQuery::Cluster { metric, nodes } => {
            cluster(d, rate, *metric, nodes, &escape(mountpoints))
        }
        MetricQuery::Nodes(metric) => node(d, rate, *metric, &escape(mountpoints)),
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

fn cluster(d: &LensLike, rate: &str, metric: ClusterMetric, nodes: &[String], mp: &str) -> String {
    let n = d.node;
    let on = |label: &str| node_filter_additive(nodes, label);
    match metric {
        ClusterMetric::MemoryUsage => {
            let nf = node_filter(nodes, n);
            format!(
                "sum(node_memory_MemTotal_bytes{nf} - (node_memory_MemFree_bytes{nf} + node_memory_Buffers_bytes{nf} + node_memory_Cached_bytes{nf})) by ({})",
                d.cluster_memory_by
            )
        }
        ClusterMetric::WorkloadMemoryUsage => format!(
            r#"sum(container_memory_working_set_bytes{{container!="POD",container!="",image!=""{}}}) by (component)"#,
            on("instance")
        ),
        ClusterMetric::MemoryRequests => format!(
            r#"sum(kube_pod_container_resource_requests{{resource="memory"{}}}) by (component)"#,
            on("node")
        ),
        ClusterMetric::MemoryLimits => format!(
            r#"sum(kube_pod_container_resource_limits{{resource="memory"{}}}) by (component)"#,
            on("node")
        ),
        ClusterMetric::MemoryCapacity => format!(
            r#"sum(kube_node_status_capacity{{resource="memory"{}}}) by (component)"#,
            on("node")
        ),
        ClusterMetric::MemoryAllocatableCapacity => format!(
            r#"sum(kube_node_status_allocatable{{resource="memory"{}}}) by (component)"#,
            on("node")
        ),
        ClusterMetric::CpuUsage => format!(
            r#"sum(rate(node_cpu_seconds_total{{mode=~"user|system"{}}}[{rate}]))"#,
            on(n)
        ),
        ClusterMetric::CpuRequests => format!(
            r#"sum(kube_pod_container_resource_requests{{resource="cpu"{}}}) by (component)"#,
            on("node")
        ),
        ClusterMetric::CpuLimits => format!(
            r#"sum(kube_pod_container_resource_limits{{resource="cpu"{}}}) by (component)"#,
            on("node")
        ),
        ClusterMetric::CpuCapacity => format!(
            r#"sum(kube_node_status_capacity{{resource="cpu"{}}}) by (component)"#,
            on("node")
        ),
        ClusterMetric::CpuAllocatableCapacity => format!(
            r#"sum(kube_node_status_allocatable{{resource="cpu"{}}}) by (component)"#,
            on("node")
        ),
        ClusterMetric::PodUsage => format!(
            r#"sum({{__name__=~"kubelet_running_pod_count|kubelet_running_pods"{}}})"#,
            on("instance")
        ),
        ClusterMetric::PodCapacity => format!(
            r#"sum(kube_node_status_capacity{{resource="pods"{}}}) by (component)"#,
            on("node")
        ),
        ClusterMetric::PodAllocatableCapacity => format!(
            r#"sum(kube_node_status_allocatable{{resource="pods"{}}}) by (component)"#,
            on("node")
        ),
        ClusterMetric::FsSize => format!(
            r#"sum(node_filesystem_size_bytes{{mountpoint=~"{mp}"{}}}) by ({n})"#,
            on(n)
        ),
        ClusterMetric::FsUsage => {
            let nf = on(n);
            format!(
                r#"sum(node_filesystem_size_bytes{{mountpoint=~"{mp}"{nf}}} - node_filesystem_avail_bytes{{mountpoint=~"{mp}"{nf}}}) by ({n})"#
            )
        }
    }
}

fn node(d: &LensLike, rate: &str, metric: NodeMetric, mp: &str) -> String {
    let n = d.node;
    match metric {
        NodeMetric::MemoryUsage => format!(
            "{}(node_memory_MemTotal_bytes - (node_memory_MemFree_bytes + node_memory_Buffers_bytes + node_memory_Cached_bytes)) by ({n})",
            d.nodes_memory_sum
        ),
        NodeMetric::WorkloadMemoryUsage => {
            r#"sum(container_memory_working_set_bytes{container!="POD",container!=""}) by (instance)"#
                .to_owned()
        }
        NodeMetric::MemoryCapacity => {
            r#"sum(kube_node_status_capacity{resource="memory"}) by (node)"#.to_owned()
        }
        NodeMetric::MemoryAllocatableCapacity => {
            r#"sum(kube_node_status_allocatable{resource="memory"}) by (node)"#.to_owned()
        }
        NodeMetric::CpuUsage => {
            format!(r#"sum(rate(node_cpu_seconds_total{{mode=~"user|system"}}[{rate}])) by({n})"#)
        }
        // Freelens reads allocatable CPU for capacity too.
        NodeMetric::CpuCapacity | NodeMetric::CpuAllocatableCapacity => {
            r#"sum(kube_node_status_allocatable{resource="cpu"}) by (node)"#.to_owned()
        }
        NodeMetric::FsSize => {
            format!(r#"sum(node_filesystem_size_bytes{{mountpoint=~"{mp}"}}) by ({n})"#)
        }
        NodeMetric::FsUsage => format!(
            r#"sum(node_filesystem_size_bytes{{mountpoint=~"{mp}"}} - node_filesystem_avail_bytes{{mountpoint=~"{mp}"}}) by ({n})"#
        ),
    }
}

fn pod(rate: &str, metric: PodMetric, pods: &str, ns: &str, by: &str) -> String {
    match metric {
        PodMetric::CpuUsage => format!(
            r#"sum(rate(container_cpu_usage_seconds_total{{container!="POD",container!="",image!="",pod=~"{pods}",namespace="{ns}"}}[{rate}])) by ({by})"#
        ),
        PodMetric::CpuRequests => format!(
            r#"sum(kube_pod_container_resource_requests{{pod=~"{pods}",resource="cpu",namespace="{ns}"}}) by ({by})"#
        ),
        PodMetric::CpuLimits => format!(
            r#"sum(kube_pod_container_resource_limits{{pod=~"{pods}",resource="cpu",namespace="{ns}"}}) by ({by})"#
        ),
        PodMetric::MemoryUsage => format!(
            r#"sum(container_memory_working_set_bytes{{container!="POD",container!="",image!="",pod=~"{pods}",namespace="{ns}"}}) by ({by})"#
        ),
        PodMetric::MemoryRequests => format!(
            r#"sum(kube_pod_container_resource_requests{{pod=~"{pods}",resource="memory",namespace="{ns}"}}) by ({by})"#
        ),
        PodMetric::MemoryLimits => format!(
            r#"sum(kube_pod_container_resource_limits{{pod=~"{pods}",resource="memory",namespace="{ns}"}}) by ({by})"#
        ),
        PodMetric::FsUsage => format!(
            r#"sum(container_fs_usage_bytes{{container!="POD",container!="",image!="",pod=~"{pods}",namespace="{ns}"}}) by ({by})"#
        ),
        PodMetric::FsWrites => format!(
            r#"sum(rate(container_fs_writes_bytes_total{{container!="",image!="", pod=~"{pods}", namespace="{ns}"}}[{rate}])) by ({by})"#
        ),
        PodMetric::FsReads => format!(
            r#"sum(rate(container_fs_reads_bytes_total{{container!="",image!="", pod=~"{pods}", namespace="{ns}"}}[{rate}])) by ({by})"#
        ),
        PodMetric::NetworkReceive => format!(
            r#"sum(rate(container_network_receive_bytes_total{{pod=~"{pods}",namespace="{ns}"}}[{rate}])) by ({by})"#
        ),
        PodMetric::NetworkTransmit => format!(
            r#"sum(rate(container_network_transmit_bytes_total{{pod=~"{pods}",namespace="{ns}"}}[{rate}])) by ({by})"#
        ),
    }
}

fn pvc(metric: PvcMetric, pvc: &str, ns: &str) -> String {
    match metric {
        PvcMetric::DiskUsage => format!(
            r#"sum(kubelet_volume_stats_used_bytes{{persistentvolumeclaim="{pvc}",namespace="{ns}"}}) by (persistentvolumeclaim, namespace)"#
        ),
        PvcMetric::DiskCapacity => format!(
            r#"sum(kubelet_volume_stats_capacity_bytes{{persistentvolumeclaim="{pvc}",namespace="{ns}"}}) by (persistentvolumeclaim, namespace)"#
        ),
    }
}

fn ingress(rate: &str, metric: IngressMetric, ingress: &str, ns: &str) -> String {
    match metric {
        IngressMetric::BytesSentSuccess => bytes_sent(rate, ingress, ns, STATUS_2XX),
        IngressMetric::BytesSentFailure => bytes_sent(rate, ingress, ns, STATUS_5XX),
        IngressMetric::RequestDurationSeconds => format!(
            r#"sum(rate(nginx_ingress_controller_request_duration_seconds_sum{{ingress="{ingress}",namespace="{ns}"}}[{rate}])) by (ingress, namespace)"#
        ),
        IngressMetric::ResponseDurationSeconds => format!(
            r#"sum(rate(nginx_ingress_controller_response_duration_seconds_sum{{ingress="{ingress}",namespace="{ns}"}}[{rate}])) by (ingress, namespace)"#
        ),
    }
}
