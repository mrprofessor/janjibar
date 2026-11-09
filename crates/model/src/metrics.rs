//! Prometheus metrics: where to find Prometheus, what to ask, what comes back.
//!
//! The providers and query names follow Freelens, so its settings and charts
//! map one to one. A [`MetricQuery`] says what to measure; the engine turns
//! it into PromQL for the cluster's provider.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, SystemTime};

/// How a Prometheus install is found and which PromQL it understands.
///
/// Each provider is a known way to deploy Prometheus, with its own service
/// name or labels and its own metric labels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PrometheusProvider {
    /// Service `prometheus` in namespace `lens-metrics`, from the Lens metrics stack.
    Lens,
    /// The `prometheus-community/prometheus` Helm chart.
    Helm,
    /// The old `stable/prometheus` Helm chart, version 14 and earlier.
    Helm14,
    /// Prometheus Operator, including kube-prometheus-stack.
    Operator,
    /// Mirantis Stacklight: service `prometheus-server` in namespace `stacklight`.
    Stacklight,
    /// OpenShift monitoring: service `prometheus-k8s` in `openshift-monitoring`.
    OpenShift,
}

impl PrometheusProvider {
    /// Every provider, in the order auto-detection prefers them.
    ///
    /// This is the order Freelens registers them in. When several match,
    /// the first one wins.
    pub const ALL: [Self; 6] = [
        Self::Helm14,
        Self::Helm,
        Self::Lens,
        Self::OpenShift,
        Self::Operator,
        Self::Stacklight,
    ];

    /// The stable id, as Freelens stores it (`lens`, `helm14`, `operator`).
    pub fn id(self) -> &'static str {
        match self {
            Self::Lens => "lens",
            Self::Helm => "helm",
            Self::Helm14 => "helm14",
            Self::Operator => "operator",
            Self::Stacklight => "stacklight",
            Self::OpenShift => "openshift",
        }
    }

    /// Parses an id from [`id`](Self::id).
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.id() == id)
    }

    /// The name to show, such as "Prometheus Operator".
    pub fn label(self) -> &'static str {
        match self {
            Self::Lens => "Lens",
            Self::Helm => "Helm",
            Self::Helm14 => "Helm 14.x",
            Self::Operator => "Prometheus Operator",
            Self::Stacklight => "Stacklight",
            Self::OpenShift => "OpenShift",
        }
    }
}

impl fmt::Display for PrometheusProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A cluster's Prometheus settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrometheusSettings {
    /// Where Prometheus is.
    pub source: PrometheusSource,
    /// A path in front of `/api/v1`, for a Prometheus served under a prefix
    /// such as `/prometheus`. Empty for none.
    pub path_prefix: String,
    /// How queries are sent.
    pub method: QueryMethod,
    /// A regex of the filesystem mount points that disk charts count.
    pub mountpoints: String,
}

impl PrometheusSettings {
    /// Freelens's default mount points: the root and `/local`.
    pub const DEFAULT_MOUNTPOINTS: &str = "/|/local";
}

impl Default for PrometheusSettings {
    fn default() -> Self {
        Self {
            source: PrometheusSource::Auto,
            path_prefix: String::new(),
            method: QueryMethod::Post,
            mountpoints: Self::DEFAULT_MOUNTPOINTS.to_owned(),
        }
    }
}

/// Where Prometheus is, and which PromQL to send it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PrometheusSource {
    /// Try every provider's lookup and use the first that finds a service.
    #[default]
    Auto,
    /// Look the service up the way this provider does.
    Provider(PrometheusProvider),
    /// A service picked by hand, reached through the API server's proxy.
    Service {
        /// The service.
        service: ServiceRef,
        /// The provider whose queries fit the metrics this Prometheus has.
        queries: PrometheusProvider,
    },
    /// A URL outside the cluster: Prometheus, VictoriaMetrics, Mimir, Thanos.
    Url {
        /// The URL and how to authenticate.
        url: PrometheusUrl,
        /// The provider whose queries fit the metrics this Prometheus has.
        queries: PrometheusProvider,
    },
}

/// A Kubernetes service that serves the Prometheus HTTP API.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServiceRef {
    /// The service's namespace.
    pub namespace: String,
    /// The service's name.
    pub name: String,
    /// The service port.
    pub port: u16,
    /// Whether the port speaks HTTPS.
    pub https: bool,
}

impl fmt::Display for ServiceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}:{}", self.namespace, self.name, self.port)
    }
}

/// A Prometheus-compatible endpoint outside the cluster.
#[derive(Clone, PartialEq, Eq)]
pub struct PrometheusUrl {
    /// The base URL, such as `https://mimir.example.com/prometheus`. Queries
    /// go to `{url}{path_prefix}/api/v1/query_range`.
    pub url: String,
    /// A bearer token sent as `Authorization: Bearer ...`.
    pub bearer_token: Option<String>,
    /// Extra headers, such as `X-Scope-OrgID` for a Mimir tenant or a
    /// basic `Authorization` header.
    pub headers: Vec<(String, String)>,
}

impl fmt::Debug for PrometheusUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Tokens and header values can be credentials; show names only.
        let header_names: Vec<&str> = self.headers.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("PrometheusUrl")
            .field("url", &self.url)
            .field("bearer_token", &self.bearer_token.as_ref().map(|_| "<redacted>"))
            .field("headers", &header_names)
            .finish()
    }
}

/// The HTTP method for `query_range` requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum QueryMethod {
    /// A form-encoded POST. Long queries never hit URL limits. The default.
    #[default]
    Post,
    /// A GET with the query in the URL, for proxies that only allow GET.
    Get,
}

/// One chart's worth of data to fetch.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MetricQuery {
    /// Whole-cluster totals.
    Cluster {
        /// What to measure.
        metric: ClusterMetric,
        /// Only these nodes. Empty means every node, including nodes that
        /// were replaced during the time range.
        nodes: Vec<String>,
    },
    /// One series per node.
    Nodes(NodeMetric),
    /// Pods in one namespace.
    Pods {
        /// What to measure.
        metric: PodMetric,
        /// Which pods, and how to group them.
        pods: PodSelection,
    },
    /// One persistent volume claim.
    Pvc {
        /// What to measure.
        metric: PvcMetric,
        /// The claim's namespace.
        namespace: String,
        /// The claim's name.
        name: String,
    },
    /// One ingress, from ingress-nginx metrics.
    Ingress {
        /// What to measure.
        metric: IngressMetric,
        /// The ingress's namespace.
        namespace: String,
        /// The ingress's name.
        name: String,
    },
}

/// Which pods a pod query covers.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PodSelection {
    /// The pods' namespace.
    pub namespace: String,
    /// The pods.
    pub pods: PodSet,
    /// Labels to group by, such as `pod` and `namespace`. Empty sums
    /// everything into one series.
    pub group_by: Vec<String>,
}

/// A set of pods, matched by name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PodSet {
    /// These pods.
    Named(Vec<String>),
    /// The pods of these workloads, matched by the names their controller
    /// gives them.
    OwnedBy {
        /// The workloads' kind.
        kind: WorkloadKind,
        /// The workloads' names.
        names: Vec<String>,
    },
    /// Every pod in the namespace.
    All,
}

/// A workload kind whose pods are named after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WorkloadKind {
    /// Pods are `{name}-{replicaset hash}-{suffix}`.
    Deployment,
    /// Pods are `{name}-{ordinal}`.
    StatefulSet,
    /// Pods are `{name}-{suffix}`.
    DaemonSet,
    /// Pods are `{name}-{suffix}`.
    ReplicaSet,
    /// Pods are `{name}-{suffix}`.
    Job,
}

/// Declares a metric enum with Freelens's query name for each variant.
macro_rules! metric_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $key:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)+
        }

        impl $name {
            /// Every metric of this kind.
            pub const ALL: &[Self] = &[$(Self::$variant,)+];

            /// Freelens's name for this query, such as `memoryUsage`.
            pub fn key(self) -> &'static str {
                match self {
                    $(Self::$variant => $key,)+
                }
            }
        }
    };
}

metric_enum! {
    /// Whole-cluster metrics.
    ClusterMetric {
        /// Node memory in use, from node-exporter.
        MemoryUsage => "memoryUsage",
        /// Container working sets, summed.
        WorkloadMemoryUsage => "workloadMemoryUsage",
        /// Memory requests, summed.
        MemoryRequests => "memoryRequests",
        /// Memory limits, summed.
        MemoryLimits => "memoryLimits",
        /// Node memory capacity.
        MemoryCapacity => "memoryCapacity",
        /// Node allocatable memory.
        MemoryAllocatableCapacity => "memoryAllocatableCapacity",
        /// Node CPU seconds in user and system mode, as a rate.
        CpuUsage => "cpuUsage",
        /// CPU requests, summed.
        CpuRequests => "cpuRequests",
        /// CPU limits, summed.
        CpuLimits => "cpuLimits",
        /// Node CPU capacity.
        CpuCapacity => "cpuCapacity",
        /// Node allocatable CPU.
        CpuAllocatableCapacity => "cpuAllocatableCapacity",
        /// Running pods.
        PodUsage => "podUsage",
        /// Node pod capacity.
        PodCapacity => "podCapacity",
        /// Node allocatable pods.
        PodAllocatableCapacity => "podAllocatableCapacity",
        /// Filesystem size on the chosen mount points.
        FsSize => "fsSize",
        /// Filesystem space in use on the chosen mount points.
        FsUsage => "fsUsage",
    }
}

metric_enum! {
    /// Per-node metrics.
    NodeMetric {
        /// Memory in use, from node-exporter.
        MemoryUsage => "memoryUsage",
        /// Container working sets on the node.
        WorkloadMemoryUsage => "workloadMemoryUsage",
        /// Memory capacity.
        MemoryCapacity => "memoryCapacity",
        /// Allocatable memory.
        MemoryAllocatableCapacity => "memoryAllocatableCapacity",
        /// CPU seconds in user and system mode, as a rate.
        CpuUsage => "cpuUsage",
        /// CPU capacity. Freelens reads allocatable CPU here; so do we.
        CpuCapacity => "cpuCapacity",
        /// Allocatable CPU.
        CpuAllocatableCapacity => "cpuAllocatableCapacity",
        /// Filesystem size on the chosen mount points.
        FsSize => "fsSize",
        /// Filesystem space in use on the chosen mount points.
        FsUsage => "fsUsage",
    }
}

metric_enum! {
    /// Pod and workload metrics.
    PodMetric {
        /// CPU seconds, as a rate.
        CpuUsage => "cpuUsage",
        /// CPU requests.
        CpuRequests => "cpuRequests",
        /// CPU limits.
        CpuLimits => "cpuLimits",
        /// Working set memory.
        MemoryUsage => "memoryUsage",
        /// Memory requests.
        MemoryRequests => "memoryRequests",
        /// Memory limits.
        MemoryLimits => "memoryLimits",
        /// Container filesystem usage.
        FsUsage => "fsUsage",
        /// Filesystem bytes written, as a rate.
        FsWrites => "fsWrites",
        /// Filesystem bytes read, as a rate.
        FsReads => "fsReads",
        /// Network bytes received, as a rate.
        NetworkReceive => "networkReceive",
        /// Network bytes sent, as a rate.
        NetworkTransmit => "networkTransmit",
    }
}

metric_enum! {
    /// Persistent volume claim metrics, from the kubelet.
    PvcMetric {
        /// Bytes used.
        DiskUsage => "diskUsage",
        /// Bytes available in total.
        DiskCapacity => "diskCapacity",
    }
}

metric_enum! {
    /// Ingress metrics, from ingress-nginx.
    IngressMetric {
        /// Bytes sent with a 2xx status, as a rate.
        BytesSentSuccess => "bytesSentSuccess",
        /// Bytes sent with a 5xx status, as a rate.
        BytesSentFailure => "bytesSentFailure",
        /// Request time, as a rate.
        RequestDurationSeconds => "requestDurationSeconds",
        /// Upstream response time, as a rate.
        ResponseDurationSeconds => "responseDurationSeconds",
    }
}

/// The time window and resolution of a range query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeRange {
    /// The first sample's time.
    pub start: SystemTime,
    /// The last sample's time.
    pub end: SystemTime,
    /// The time between samples.
    pub step: Duration,
}

impl TimeRange {
    /// The window of length `span` that ends now, with [`step_for`](Self::step_for).
    pub fn last(span: Duration) -> Self {
        let end = SystemTime::now();
        Self {
            start: end.checked_sub(span).unwrap_or(SystemTime::UNIX_EPOCH),
            end,
            step: Self::step_for(span),
        }
    }

    /// Freelens's step for a window: coarser as the window grows, so a chart
    /// never has more than a few hundred points.
    pub fn step_for(span: Duration) -> Duration {
        const HOUR: u64 = 3_600;
        const DAY: u64 = 24 * HOUR;
        let secs = span.as_secs();
        let step = match secs {
            s if s <= 4 * HOUR => 60,
            s if s <= DAY => 10 * 60,
            s if s <= 4 * DAY => HOUR,
            s if s <= 14 * DAY => 3 * HOUR,
            s if s <= 30 * DAY => 6 * HOUR,
            _ => 12 * HOUR,
        };
        Duration::from_secs(step)
    }
}

/// One series of a range query result.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MetricSeries {
    /// The series' labels, such as `pod` and `namespace`. Empty when the
    /// query summed everything.
    pub labels: BTreeMap<String, String>,
    /// Samples, oldest first.
    pub samples: Vec<Sample>,
}

/// One value at one time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    /// When the value was taken.
    pub time: SystemTime,
    /// The value. May be NaN or infinite, as Prometheus allows.
    pub value: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_round_trip() {
        for provider in PrometheusProvider::ALL {
            assert_eq!(PrometheusProvider::from_id(provider.id()), Some(provider));
        }
        assert_eq!(PrometheusProvider::from_id("victoria"), None);
    }

    #[test]
    fn step_grows_with_the_window() {
        let hours = |h: u64| Duration::from_secs(h * 3_600);
        assert_eq!(TimeRange::step_for(hours(1)), Duration::from_secs(60));
        assert_eq!(TimeRange::step_for(hours(4)), Duration::from_secs(60));
        assert_eq!(TimeRange::step_for(hours(5)), Duration::from_secs(600));
        assert_eq!(TimeRange::step_for(hours(24)), Duration::from_secs(600));
        assert_eq!(TimeRange::step_for(hours(25)), hours(1));
        assert_eq!(TimeRange::step_for(hours(14 * 24)), hours(3));
        assert_eq!(TimeRange::step_for(hours(30 * 24)), hours(6));
        assert_eq!(TimeRange::step_for(hours(31 * 24)), hours(12));
    }

    #[test]
    fn last_ends_now() {
        let range = TimeRange::last(Duration::from_secs(3_600));
        let span = range.end.duration_since(range.start).unwrap_or_default();
        assert_eq!(span, Duration::from_secs(3_600));
        assert_eq!(range.step, Duration::from_secs(60));
    }

    #[test]
    fn url_debug_hides_credentials() {
        let url = PrometheusUrl {
            url: "https://mimir.example.com".to_owned(),
            bearer_token: Some("s3cret".to_owned()),
            headers: vec![("X-Scope-OrgID".to_owned(), "tenant-a".to_owned())],
        };
        let text = format!("{url:?}");
        assert!(!text.contains("s3cret"), "{text}");
        assert!(!text.contains("tenant-a"), "{text}");
        assert!(text.contains("X-Scope-OrgID"), "{text}");
    }

    #[test]
    fn metric_keys_are_unique_per_kind() {
        fn unique(keys: &[&str]) -> bool {
            let mut sorted = keys.to_vec();
            sorted.sort_unstable();
            sorted.windows(2).all(|w| w[0] != w[1])
        }
        assert!(unique(&ClusterMetric::ALL.iter().map(|m| m.key()).collect::<Vec<_>>()));
        assert!(unique(&NodeMetric::ALL.iter().map(|m| m.key()).collect::<Vec<_>>()));
        assert!(unique(&PodMetric::ALL.iter().map(|m| m.key()).collect::<Vec<_>>()));
    }
}
