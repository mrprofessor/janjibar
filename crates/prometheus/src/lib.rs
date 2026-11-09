//! Prometheus for the app: find it in a cluster, build PromQL, run queries.

mod promql;

pub use promql::promql;
