//! Finding and querying Prometheus on the engine's fake API server.
//!
//! The fake server holds Service objects for detection and answers the
//! service proxy path with canned Prometheus responses. No cluster needed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

#[path = "../../engine/tests/support/fake_api.rs"]
mod fake_api;

use std::time::{Duration, SystemTime};

use fake_api::{FakeApi, Request};
use model::{
    ClusterMetric, MetricQuery, PodMetric, PodSelection, PodSet, PrometheusProvider,
    PrometheusSettings, PrometheusSource, PrometheusUrl, QueryMethod, ServiceRef, TimeRange,
};
use prometheus::{Endpoint, MissReason, Prometheus, QueryError, Retry, SetupError};
use serde_json::{Value, json};

const HELM_PROXY: &str =
    "/api/v1/namespaces/monitoring/services/prometheus-server:80/proxy/api/v1/query_range";

/// A one-hour window at a fixed time, so request parameters are known.
#[expect(
    clippy::duration_suboptimal_units,
    reason = "a Unix time reads best in seconds"
)]
fn range() -> TimeRange {
    let end = SystemTime::UNIX_EPOCH + Duration::from_secs(1_762_650_000);
    TimeRange {
        start: end - Duration::from_hours(1),
        end,
        step: Duration::from_secs(60),
    }
}

/// A client for the fake server. `retry` keeps kube's own retry of 503s.
fn client(api: &FakeApi, retry: bool) -> kube::Client {
    let mut config = kube::Config::new(api.url().parse().unwrap());
    config.default_retry = retry;
    kube::Client::try_from(config).unwrap()
}

fn service(namespace: &str, name: &str, labels: Value, port: u16) -> Value {
    let mut object = json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": { "name": name, "namespace": namespace },
        "spec": { "ports": [{ "name": "http", "port": port }] },
    });
    object["metadata"]["labels"] = labels;
    object
}

fn helm_service() -> Value {
    service(
        "monitoring",
        "prometheus-server",
        json!({
            "app.kubernetes.io/name": "prometheus",
            "app.kubernetes.io/component": "server",
            "app.kubernetes.io/managed-by": "Helm",
        }),
        80,
    )
}

fn matrix(series: &Value) -> String {
    json!({ "status": "success", "data": { "resultType": "matrix", "result": series } }).to_string()
}

fn cpu() -> MetricQuery {
    MetricQuery::Cluster {
        metric: ClusterMetric::CpuUsage,
        nodes: Vec::new(),
    }
}

fn proxied(api: &FakeApi, path: &str) -> Vec<Request> {
    api.requests()
        .into_iter()
        .filter(|r| r.path == path)
        .collect()
}

#[tokio::test]
async fn detects_helm_and_queries_through_the_service_proxy() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_object("services", helm_service());
    api.respond(
        HELM_PROXY,
        200,
        &matrix(
            &json!([{ "metric": {}, "values": [[1_762_646_400, "0.75"], [1_762_646_460, "1.5"]] }]),
        ),
    );

    let prometheus = Prometheus::connect(&client(&api, true), &PrometheusSettings::default())
        .await
        .unwrap();
    assert_eq!(prometheus.provider(), PrometheusProvider::Helm);
    assert!(prometheus.auto_detected());
    assert_eq!(
        prometheus.endpoint(),
        &Endpoint::Service(ServiceRef {
            namespace: "monitoring".to_owned(),
            name: "prometheus-server".to_owned(),
            port: 80,
            https: false,
        })
    );

    let series = prometheus.query(&cpu(), &range()).await.unwrap();
    let values: Vec<f64> = series[0].samples.iter().map(|s| s.value).collect();
    assert_eq!(values, [0.75, 1.5]);

    let requests = proxied(&api, HELM_PROXY);
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(
        request.form_param("query").as_deref(),
        Some(r#"sum(rate(node_cpu_seconds_total{mode=~"user|system"}[5m]))"#)
    );
    assert_eq!(request.form_param("start").as_deref(), Some("1762646400"));
    assert_eq!(request.form_param("end").as_deref(), Some("1762650000"));
    assert_eq!(request.form_param("step").as_deref(), Some("60"));
}

#[tokio::test]
async fn detection_prefers_freelens_order() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_object("services", helm_service());
    api.add_object(
        "services",
        service("lens-metrics", "prometheus", json!({}), 80),
    );
    api.add_object(
        "services",
        service(
            "monitoring",
            "prometheus-operated",
            json!({ "operated-prometheus": "true" }),
            9090,
        ),
    );

    let prometheus = Prometheus::connect(&client(&api, true), &PrometheusSettings::default())
        .await
        .unwrap();
    // Freelens tries Helm 14, Helm, Lens, OpenShift, Operator, Stacklight.
    assert_eq!(prometheus.provider(), PrometheusProvider::Helm);
}

#[tokio::test]
async fn a_chosen_provider_only_looks_its_own_way() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_object("services", helm_service());
    api.add_object(
        "services",
        service(
            "monitoring",
            "prometheus-operated",
            json!({ "operated-prometheus": "true" }),
            9090,
        ),
    );
    let settings = PrometheusSettings {
        source: PrometheusSource::Provider(PrometheusProvider::Operator),
        ..PrometheusSettings::default()
    };
    let prometheus = Prometheus::connect(&client(&api, true), &settings)
        .await
        .unwrap();
    assert_eq!(prometheus.provider(), PrometheusProvider::Operator);
    assert!(!prometheus.auto_detected());
    let Endpoint::Service(service) = prometheus.endpoint() else {
        panic!("expected a service");
    };
    assert_eq!(service.name, "prometheus-operated");
    assert_eq!(service.port, 9090);
}

#[tokio::test]
async fn names_every_provider_when_nothing_is_found() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_object(
        "services",
        service("monitoring", "grafana", json!({ "app": "grafana" }), 80),
    );

    let err = Prometheus::connect(&client(&api, true), &PrometheusSettings::default())
        .await
        .unwrap_err();
    let SetupError::NotFound(not_found) = err else {
        panic!("expected NotFound, got {err:?}");
    };
    let providers: Vec<PrometheusProvider> = not_found.misses.iter().map(|m| m.provider).collect();
    assert_eq!(providers, PrometheusProvider::ALL);
    assert!(
        not_found
            .misses
            .iter()
            .all(|m| matches!(m.reason, MissReason::NotFound(_))),
        "{not_found}"
    );
}

#[tokio::test]
async fn reports_when_services_are_forbidden() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.forbid("services");
    let settings = PrometheusSettings {
        source: PrometheusSource::Provider(PrometheusProvider::Helm),
        ..PrometheusSettings::default()
    };
    let err = Prometheus::connect(&client(&api, true), &settings)
        .await
        .unwrap_err();
    let SetupError::NotFound(not_found) = err else {
        panic!("expected NotFound, got {err:?}");
    };
    assert!(
        matches!(not_found.misses[0].reason, MissReason::Forbidden(_)),
        "{not_found}"
    );
}

#[tokio::test]
async fn a_picked_service_uses_its_scheme_prefix_and_get() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let path = "/api/v1/namespaces/openshift-monitoring/services/https:prometheus-k8s:9091/proxy/prom/api/v1/query_range";
    api.respond(path, 200, &matrix(&json!([])));
    let settings = PrometheusSettings {
        source: PrometheusSource::Service {
            service: ServiceRef {
                namespace: "openshift-monitoring".to_owned(),
                name: "prometheus-k8s".to_owned(),
                port: 9091,
                https: true,
            },
            queries: PrometheusProvider::OpenShift,
        },
        path_prefix: "prom/".to_owned(),
        method: QueryMethod::Get,
        ..PrometheusSettings::default()
    };

    let prometheus = Prometheus::connect(&client(&api, true), &settings)
        .await
        .unwrap();
    // No detection happens: the server sees only the query.
    let series = prometheus.query(&cpu(), &range()).await.unwrap();
    assert_eq!(series, []);

    let requests = api.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].path, path);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].body, "");
    assert_eq!(
        requests[0].param("query").as_deref(),
        Some(r#"sum(rate(node_cpu_seconds_total{mode=~"user|system"}[1m]))"#)
    );
}

#[tokio::test]
async fn a_direct_url_sends_its_token_and_headers() {
    // The fake server stands in for Mimir outside the cluster.
    let mimir = FakeApi::start("127.0.0.1:0").await.unwrap();
    let path = "/mimir/prometheus/api/v1/query_range";
    mimir.respond(
        path,
        200,
        &matrix(&json!([{ "metric": { "pod": "web-1" }, "values": [[1_762_650_000, "42"]] }])),
    );
    // The cluster is unreachable; a URL source must not need it.
    let cluster = {
        let config = kube::Config::new("http://127.0.0.1:1".parse().unwrap());
        kube::Client::try_from(config).unwrap()
    };
    let settings = PrometheusSettings {
        source: PrometheusSource::Url {
            url: PrometheusUrl {
                url: format!("{}/mimir", mimir.url()),
                bearer_token: Some("t0ken".to_owned()),
                headers: vec![("X-Scope-OrgID".to_owned(), "tenant-a".to_owned())],
            },
            queries: PrometheusProvider::Operator,
        },
        path_prefix: "/prometheus".to_owned(),
        ..PrometheusSettings::default()
    };

    let prometheus = Prometheus::connect(&cluster, &settings).await.unwrap();
    let query = MetricQuery::Pods {
        metric: PodMetric::MemoryUsage,
        pods: PodSelection {
            namespace: "shop".to_owned(),
            pods: PodSet::Named(vec!["web-1".to_owned()]),
            group_by: vec!["pod".to_owned()],
        },
    };
    let series = prometheus.query(&query, &range()).await.unwrap();
    assert_eq!(series[0].labels["pod"], "web-1");
    assert_eq!(series[0].samples[0].value, 42.0);

    let requests = mimir.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].path, path);
    assert_eq!(requests[0].header("authorization"), Some("Bearer t0ken"));
    assert_eq!(requests[0].header("x-scope-orgid"), Some("tenant-a"));
    assert_eq!(
        requests[0].form_param("query").as_deref(),
        Some(
            r#"sum(container_memory_working_set_bytes{container!="",image!="", pod=~"web-1", namespace="shop"}) by (pod)"#
        )
    );
}

#[tokio::test]
async fn rejects_a_bad_url() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let settings = PrometheusSettings {
        source: PrometheusSource::Url {
            url: PrometheusUrl {
                url: "mimir.example.com".to_owned(),
                bearer_token: None,
                headers: Vec::new(),
            },
            queries: PrometheusProvider::Operator,
        },
        ..PrometheusSettings::default()
    };
    let err = Prometheus::connect(&client(&api, true), &settings)
        .await
        .unwrap_err();
    assert!(matches!(err, SetupError::InvalidUrl { .. }), "{err:?}");
}

/// Connects straight to the Helm service, with fast retries.
async fn helm(api: &FakeApi, kube_retry: bool, retry: Retry) -> Prometheus {
    api.add_object("services", helm_service());
    let settings = PrometheusSettings {
        source: PrometheusSource::Provider(PrometheusProvider::Helm),
        ..PrometheusSettings::default()
    };
    Prometheus::connect(&client(api, kube_retry), &settings)
        .await
        .unwrap()
        .with_retry(retry)
}

const FAST_RETRY: Retry = Retry {
    attempts: 3,
    delay: Duration::from_millis(1),
};

#[tokio::test]
async fn a_bad_query_fails_at_once() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let prometheus = helm(&api, true, FAST_RETRY).await;
    api.respond(
        HELM_PROXY,
        400,
        r#"{"status":"error","errorType":"bad_data","error":"1:5: parse error"}"#,
    );

    let err = prometheus.query(&cpu(), &range()).await.unwrap_err();
    assert_eq!(
        err,
        QueryError::Prometheus {
            status: 400,
            kind: "bad_data".to_owned(),
            message: "1:5: parse error".to_owned(),
        }
    );
    assert_eq!(proxied(&api, HELM_PROXY).len(), 1);
}

#[tokio::test]
async fn a_server_error_is_retried() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let prometheus = helm(&api, true, FAST_RETRY).await;
    api.respond(HELM_PROXY, 500, "oops");
    api.respond(HELM_PROXY, 200, &matrix(&json!([])));

    let series = prometheus.query(&cpu(), &range()).await.unwrap();
    assert_eq!(series, []);
    assert_eq!(proxied(&api, HELM_PROXY).len(), 2);
}

#[tokio::test]
async fn kube_retries_unavailable_on_its_own() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let no_retry = Retry {
        attempts: 1,
        delay: Duration::ZERO,
    };
    let prometheus = helm(&api, true, no_retry).await;
    api.respond(HELM_PROXY, 503, "{}");
    api.respond(HELM_PROXY, 200, &matrix(&json!([])));

    prometheus.query(&cpu(), &range()).await.unwrap();
    assert_eq!(proxied(&api, HELM_PROXY).len(), 2);
}

#[tokio::test]
async fn a_service_without_pods_says_so() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let prometheus = helm(&api, false, FAST_RETRY).await;
    api.respond(
        HELM_PROXY,
        503,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"no endpoints available for service \"prometheus-server\"","reason":"ServiceUnavailable","code":503}"#,
    );

    let err = prometheus.query(&cpu(), &range()).await.unwrap_err();
    assert_eq!(
        err,
        QueryError::Http {
            status: 503,
            message: r#"no endpoints available for service "prometheus-server""#.to_owned(),
        }
    );
}

#[tokio::test]
async fn query_many_sends_each_promql_once() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let prometheus = helm(&api, true, FAST_RETRY).await;
    api.respond(HELM_PROXY, 200, &matrix(&json!([])));
    let memory = MetricQuery::Cluster {
        metric: ClusterMetric::MemoryUsage,
        nodes: Vec::new(),
    };

    let results = prometheus
        .query_many(&[cpu(), memory, cpu()], &range())
        .await;
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(Result::is_ok));
    assert_eq!(proxied(&api, HELM_PROXY).len(), 2);
}
