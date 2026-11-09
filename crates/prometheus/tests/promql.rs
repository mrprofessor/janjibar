//! Every query for every provider matches Freelens's output.
//!
//! `fixtures/freelens-promql.json` comes from Freelens's own templates (see
//! `fixtures/README.md`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

use model::{
    ClusterMetric, IngressMetric, MetricQuery, NodeMetric, PodMetric, PodSelection, PodSet,
    PrometheusProvider, PrometheusSettings, PvcMetric,
};
use serde_json::Value;

/// Builds the query the fixture expanded, with the same options.
fn query_for(category: &str, key: &str, nodes: bool) -> MetricQuery {
    fn find<T: Copy>(all: &[T], key: &str, name: impl Fn(T) -> &'static str) -> T {
        all.iter()
            .copied()
            .find(|m| name(*m) == key)
            .unwrap_or_else(|| panic!("no metric named {key}"))
    }
    match category {
        "cluster" => MetricQuery::Cluster {
            metric: find(ClusterMetric::ALL, key, ClusterMetric::key),
            nodes: if nodes {
                vec!["node-a".to_owned(), "node-b".to_owned()]
            } else {
                Vec::new()
            },
        },
        "nodes" => MetricQuery::Nodes(find(NodeMetric::ALL, key, NodeMetric::key)),
        "pods" => MetricQuery::Pods {
            metric: find(PodMetric::ALL, key, PodMetric::key),
            pods: PodSelection {
                namespace: "shop".to_owned(),
                pods: PodSet::Named(vec!["web-1".to_owned(), "web-2".to_owned()]),
                group_by: vec!["pod".to_owned(), "namespace".to_owned()],
            },
        },
        "pvc" => MetricQuery::Pvc {
            metric: find(PvcMetric::ALL, key, PvcMetric::key),
            namespace: "shop".to_owned(),
            name: "data-0".to_owned(),
        },
        "ingress" => MetricQuery::Ingress {
            metric: find(IngressMetric::ALL, key, IngressMetric::key),
            namespace: "shop".to_owned(),
            name: "web".to_owned(),
        },
        other => panic!("unknown category {other}"),
    }
}

#[test]
fn every_query_matches_freelens() {
    let fixture: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/freelens-promql.json")).unwrap();
    let mut mismatches = Vec::new();
    for entry in &fixture {
        let provider = PrometheusProvider::from_id(entry["provider"].as_str().unwrap()).unwrap();
        let category = entry["category"].as_str().unwrap();
        let key = entry["query"].as_str().unwrap();
        let nodes = entry["nodes"].as_bool().unwrap();
        let expected = entry["promql"].as_str().unwrap();
        let query = query_for(category, key, nodes);
        let actual = prometheus::promql(provider, &query, PrometheusSettings::DEFAULT_MOUNTPOINTS);
        if actual != expected {
            mismatches.push(format!(
                "{provider:?} {category}.{key} nodes={nodes}\n  expected: {expected}\n  actual:   {actual}"
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn the_fixture_covers_every_provider_and_metric() {
    let fixture: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/freelens-promql.json")).unwrap();
    let per_provider = ClusterMetric::ALL.len()
        + NodeMetric::ALL.len()
        + PodMetric::ALL.len()
        + PvcMetric::ALL.len()
        + IngressMetric::ALL.len();
    for provider in PrometheusProvider::ALL {
        let count = fixture
            .iter()
            .filter(|e| e["provider"] == provider.id() && e["nodes"] == false)
            .count();
        assert_eq!(count, per_provider, "{provider:?}");
    }
}
