//! CPU and memory for pods and nodes from the metrics API (`metrics.k8s.io`),
//! for clusters with or without Prometheus.
//!
//! metrics-server (or any adapter serving `metrics.k8s.io`) reports current
//! usage only. A [`UsagePoller`] polls it every 15 seconds while a view holds
//! a [`UsageSubscription`], keeps an hour of history per pod and node for
//! small charts, and sends one [`model::UsageSnapshot`] per round.
//!
//! The engine holds one poller per connected context and a subscription per
//! table or chart that needs usage:
//!
//! ```no_run
//! # async fn example(client: kube::Client, events: tokio::sync::mpsc::UnboundedSender<model::UsageSnapshot>) {
//! use model::{ContextName, UsageInterest, UsageScope};
//! use resource_metrics::{PollConfig, UsagePoller};
//!
//! let poller = UsagePoller::start(client, ContextName::new("prod"), PollConfig::default(), move |snapshot| {
//!     events.send(snapshot).is_ok()
//! });
//! let pods_table = poller.subscribe(UsageInterest::Latest(UsageScope::Pods(None)));
//! // Polling stops when the last subscription is dropped.
//! drop(pods_table);
//! # }
//! ```
//!
//! This crate never depends on GPUI.

mod fetch;
mod history;
mod poller;
mod quantity;

pub use poller::{PollConfig, UsagePoller, UsageSubscription};
pub use quantity::parse_quantity;
