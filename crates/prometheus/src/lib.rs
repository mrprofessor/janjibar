//! Prometheus for the app: find it in a cluster, build PromQL, run queries.
//!
//! The providers, their service lookups and their PromQL are ported from
//! Freelens (MIT, see `NOTICE`). In-cluster Prometheus is reached through
//! the API server's service proxy with the context's own client. A URL
//! outside the cluster (Prometheus, VictoriaMetrics, Mimir, Thanos) works
//! too.
//!
//! ```no_run
//! # async fn example(cluster: kube::Client) -> Result<(), Box<dyn std::error::Error>> {
//! use std::time::Duration;
//! use model::{ClusterMetric, MetricQuery, PrometheusSettings, TimeRange};
//!
//! let prometheus = prometheus::Prometheus::connect(&cluster, &PrometheusSettings::default()).await?;
//! let query = MetricQuery::Cluster { metric: ClusterMetric::CpuUsage, nodes: Vec::new() };
//! let series = prometheus.query(&query, &TimeRange::last(Duration::from_secs(3_600))).await?;
//! # Ok(())
//! # }
//! ```
//!
//! This crate never depends on GPUI.

mod detect;
mod promql;
mod query;
mod response;

pub use detect::{Miss, MissReason, NotFound, detect, find_service};
pub use promql::promql;
pub use query::{Endpoint, Prometheus, QueryError, Retry, SetupError};
