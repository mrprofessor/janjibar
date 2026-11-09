//! PromQL for each provider, ported from Freelens.
//!
//! Freelens has four families of queries. Lens, Helm 14, Helm and Stacklight
//! share one shape and differ in a few label names ([`lens`]). The Prometheus
//! Operator and OpenShift share another ([`operator`]). Each provider also
//! has its own rate window.
//!
//! Every value put inside a PromQL string is escaped. Names that go into a
//! regex (pods, nodes) are regex-escaped first, so a dot in a node name
//! matches only a dot. Freelens inserts them as they are.

mod lens;
mod operator;

use model::{MetricQuery, PodSet, PrometheusProvider, WorkloadKind};

/// The PromQL for `query` on a cluster whose metrics follow `provider`.
///
/// `mountpoints` is a regex of the filesystem mount points disk queries
/// count, such as [`model::PrometheusSettings::DEFAULT_MOUNTPOINTS`].
pub fn promql(provider: PrometheusProvider, query: &MetricQuery, mountpoints: &str) -> String {
    let rate = rate_window(provider);
    match provider {
        PrometheusProvider::Lens | PrometheusProvider::Helm14 => {
            lens::query(&lens::LENS, rate, query, mountpoints)
        }
        PrometheusProvider::Helm => lens::query(&lens::HELM, rate, query, mountpoints),
        PrometheusProvider::Stacklight => lens::query(&lens::STACKLIGHT, rate, query, mountpoints),
        PrometheusProvider::Operator | PrometheusProvider::OpenShift => {
            operator::query(rate, query, mountpoints)
        }
    }
}

/// The window `rate()` looks back over, per provider, as Freelens sets it.
///
/// The Helm charts scrape every minute by default, so a one-minute window
/// could hold a single sample and `rate()` needs two.
fn rate_window(provider: PrometheusProvider) -> &'static str {
    match provider {
        PrometheusProvider::Helm | PrometheusProvider::Helm14 => "5m",
        PrometheusProvider::Lens
        | PrometheusProvider::Operator
        | PrometheusProvider::Stacklight
        | PrometheusProvider::OpenShift => "1m",
    }
}

/// Escapes text for the inside of a double-quoted PromQL string.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

/// Escapes a literal name for use in a regex (RE2 syntax).
fn regex_escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if r"\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A regex matching any of `names` exactly, escaped for a PromQL string.
fn any_of(names: &[String]) -> String {
    let alternation: Vec<String> = names.iter().map(|n| regex_escape(n)).collect();
    escape(&alternation.join("|"))
}

/// The pod-name regex for a pod set, escaped for a PromQL string.
fn pods_regex(pods: &PodSet) -> String {
    match pods {
        PodSet::Named(names) => any_of(names),
        PodSet::OwnedBy { kind, names } => {
            // The suffixes each controller appends, as Freelens matches them.
            let suffix = match kind {
                WorkloadKind::Deployment => "-[[:alnum:]]{9,}-[[:alnum:]]{5}",
                WorkloadKind::StatefulSet => "-[[:digit:]]+",
                WorkloadKind::DaemonSet | WorkloadKind::ReplicaSet | WorkloadKind::Job => {
                    "-[[:alnum:]]{5}"
                }
            };
            let patterns: Vec<String> = names
                .iter()
                .map(|n| format!("{}{suffix}", regex_escape(n)))
                .collect();
            escape(&patterns.join("|"))
        }
        PodSet::All => ".*".to_owned(),
    }
}

/// `{label=~"a|b"}` for metrics with no other matchers, or nothing when
/// every node counts.
fn node_filter(nodes: &[String], label: &str) -> String {
    if nodes.is_empty() {
        String::new()
    } else {
        format!(r#"{{{label}=~"{}"}}"#, any_of(nodes))
    }
}

/// `,label=~"a|b"` to add to existing matchers, or nothing when every node
/// counts.
fn node_filter_additive(nodes: &[String], label: &str) -> String {
    if nodes.is_empty() {
        String::new()
    } else {
        format!(r#",{label}=~"{}""#, any_of(nodes))
    }
}

/// ingress-nginx bytes sent for responses whose status matches `statuses`.
fn bytes_sent(rate: &str, ingress: &str, namespace: &str, statuses: &str) -> String {
    format!(
        r#"sum(rate(nginx_ingress_controller_bytes_sent_sum{{ingress="{ingress}",namespace="{namespace}",status=~"{statuses}"}}[{rate}])) by (ingress, namespace)"#
    )
}

/// Status regexes for [`bytes_sent`], already escaped for a PromQL string.
const STATUS_2XX: &str = r"^2\\d*";
const STATUS_5XX: &str = r"^5\\d*";

#[cfg(test)]
mod tests {
    use model::{ClusterMetric, NodeMetric, PodMetric, PodSelection};

    use super::*;

    fn pods(names: &[&str]) -> PodSelection {
        PodSelection {
            namespace: "shop".to_owned(),
            pods: PodSet::Named(names.iter().map(|&n| n.to_owned()).collect()),
            group_by: vec!["pod".to_owned(), "namespace".to_owned()],
        }
    }

    #[test]
    fn dots_in_node_names_match_only_dots() {
        let query = MetricQuery::Cluster {
            metric: ClusterMetric::CpuRequests,
            nodes: vec!["ip-10-0-0-1.ec2.internal".to_owned()],
        };
        let text = promql(PrometheusProvider::Operator, &query, "/");
        assert!(
            text.contains(r#"node=~"ip-10-0-0-1\\.ec2\\.internal""#),
            "{text}"
        );
    }

    #[test]
    fn quotes_cannot_break_out_of_a_string() {
        let query = MetricQuery::Pvc {
            metric: model::PvcMetric::DiskUsage,
            namespace: "shop".to_owned(),
            name: r#"x"} or vector(1) or {a=""#.to_owned(),
        };
        let text = promql(PrometheusProvider::Lens, &query, "/");
        assert!(
            text.contains(r#"persistentvolumeclaim="x\"} or vector(1) or {a=\"""#),
            "{text}"
        );
    }

    #[test]
    fn mountpoints_are_a_regex_kept_intact() {
        let query = MetricQuery::Nodes(NodeMetric::FsSize);
        let text = promql(PrometheusProvider::Helm, &query, r"/|/mnt/\d+");
        assert!(text.contains(r#"mountpoint=~"/|/mnt/\\d+""#), "{text}");
    }

    #[test]
    fn workload_pods_match_controller_suffixes() {
        let regex = |kind| {
            pods_regex(&PodSet::OwnedBy {
                kind,
                names: vec!["web".to_owned(), "api".to_owned()],
            })
        };
        assert_eq!(
            regex(WorkloadKind::Deployment),
            "web-[[:alnum:]]{9,}-[[:alnum:]]{5}|api-[[:alnum:]]{9,}-[[:alnum:]]{5}"
        );
        assert_eq!(
            regex(WorkloadKind::StatefulSet),
            "web-[[:digit:]]+|api-[[:digit:]]+"
        );
        assert_eq!(
            regex(WorkloadKind::Job),
            "web-[[:alnum:]]{5}|api-[[:alnum:]]{5}"
        );
    }

    #[test]
    fn empty_group_by_sums_everything() {
        let mut selection = pods(&["web-1"]);
        selection.group_by.clear();
        let query = MetricQuery::Pods {
            metric: PodMetric::MemoryUsage,
            pods: selection,
        };
        let text = promql(PrometheusProvider::Lens, &query, "/");
        assert!(text.ends_with(" by ()"), "{text}");
    }

    #[test]
    fn helm_charts_use_a_five_minute_rate() {
        let query = MetricQuery::Pods {
            metric: PodMetric::CpuUsage,
            pods: pods(&["web-1"]),
        };
        assert!(promql(PrometheusProvider::Helm, &query, "/").contains("[5m]"));
        assert!(promql(PrometheusProvider::Helm14, &query, "/").contains("[5m]"));
        assert!(promql(PrometheusProvider::Lens, &query, "/").contains("[1m]"));
        assert!(promql(PrometheusProvider::OpenShift, &query, "/").contains("[1m]"));
    }
}
