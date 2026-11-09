//! Finds Prometheus for a kubeconfig context and runs a few queries.
//!
//! ```sh
//! cargo run -p prometheus --example promql -- --context kind-platform
//! cargo run -p prometheus --example promql -- --context NAME --provider operator
//! cargo run -p prometheus --example promql -- --context NAME --url http://localhost:9090
//! cargo run -p prometheus --example promql -- --context NAME --query 'up'
//! ```
//!
//! Prints the provider and endpoint it found, or why each provider missed,
//! then the last value of each series.

#![allow(clippy::print_stdout, reason = "an example prints its findings")]

use std::error::Error;
use std::time::Duration;

use kube::config::KubeConfigOptions;
use model::{
    ClusterMetric, MetricQuery, MetricSeries, PrometheusProvider, PrometheusSettings,
    PrometheusSource, PrometheusUrl, TimeRange,
};
use prometheus::{Prometheus, QueryError, SetupError};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut context = None;
    let mut settings = PrometheusSettings::default();
    let mut raw = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args.next().ok_or_else(|| format!("{arg} needs a value"))?;
        match arg.as_str() {
            "--context" => context = Some(value),
            "--provider" => {
                let provider = PrometheusProvider::from_id(&value)
                    .ok_or_else(|| format!("unknown provider {value}"))?;
                settings.source = PrometheusSource::Provider(provider);
            }
            "--url" => {
                settings.source = PrometheusSource::Url {
                    url: PrometheusUrl {
                        url: value,
                        bearer_token: std::env::var("PROMETHEUS_TOKEN").ok(),
                        headers: Vec::new(),
                    },
                    queries: PrometheusProvider::Operator,
                };
            }
            "--query" => raw = Some(value),
            other => return Err(format!("unknown flag {other}").into()),
        }
    }

    let options = KubeConfigOptions {
        context,
        ..KubeConfigOptions::default()
    };
    let config = kube::Config::from_kubeconfig(&options).await?;
    // Exec auth plugins run while the client is built; keep them off the runtime.
    let cluster = tokio::task::spawn_blocking(move || kube::Client::try_from(config)).await??;

    let prometheus = match Prometheus::connect(&cluster, &settings).await {
        Ok(prometheus) => prometheus,
        Err(SetupError::NotFound(not_found)) => {
            println!("no Prometheus found:");
            for miss in &not_found.misses {
                println!("  {miss}");
            }
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };
    println!(
        "found {} at {} (auto-detected: {})",
        prometheus.provider(),
        prometheus.endpoint(),
        prometheus.auto_detected()
    );

    let range = TimeRange::last(Duration::from_mins(15));
    if let Some(promql) = raw {
        print(&promql, prometheus.query_range(&promql, &range).await);
        return Ok(());
    }
    let queries: Vec<MetricQuery> = [
        ClusterMetric::CpuUsage,
        ClusterMetric::MemoryUsage,
        ClusterMetric::PodUsage,
    ]
    .into_iter()
    .map(|metric| MetricQuery::Cluster {
        metric,
        nodes: Vec::new(),
    })
    .collect();
    let results = prometheus.query_many(&queries, &range).await;
    for (query, result) in queries.iter().zip(results) {
        print(&prometheus.promql(query), result);
    }
    Ok(())
}

fn print(promql: &str, result: Result<Vec<MetricSeries>, QueryError>) {
    println!("\n{promql}");
    match result {
        Ok(series) if series.is_empty() => println!("  no series"),
        Ok(series) => {
            for s in series {
                let last = s.samples.last().map_or(f64::NAN, |sample| sample.value);
                println!("  {:?}: {} samples, last {last}", s.labels, s.samples.len());
            }
        }
        Err(err) => println!("  error: {err}"),
    }
}
