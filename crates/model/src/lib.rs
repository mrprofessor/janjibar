//! Plain data that crosses the bridge between the engine and the UI.
//!
//! Nothing here knows about GPUI or kube-rs. Types are cheap to clone and
//! safe to send between threads.

pub mod brand;
mod catalog;
mod event;
mod forward;
mod helm;
mod kinds;
mod log;
mod metrics;
mod pod;
mod table;

pub use catalog::{Catalog, ContextInfo, ContextName, SourceProblem};
pub use event::{Command, ConnectError, ConnectionState, Event};
pub use forward::{
    ForwardId, ForwardParseError, ForwardRow, ForwardSpec, ForwardState, ForwardTarget,
};
pub use helm::{HelmChart, HelmRelease, HelmReleaseDetails, HelmRevision, HelmStatus, HelmStorage};
pub use kinds::{
    DiscoveryProblem, GroupVersionResource, KindCatalog, KindGroup, KindInfo, Scope, Verb, Verbs,
};
pub use log::{
    Clock, Containers, DEFAULT_LOG_CAPACITY, DEFAULT_TAIL_LINES, LineKind, LogBuffer, LogChange,
    LogChunk, LogLevel, LogLine, LogOptions, LogRequest, LogSource, LogState, LogTabId, LogTarget,
};
pub use metrics::{
    ClusterMetric, IngressMetric, MetricQuery, MetricSeries, NodeMetric, PodMetric, PodSelection,
    PodSet, PrometheusProvider, PrometheusSettings, PrometheusSource, PrometheusUrl, PvcMetric,
    QueryMethod, Sample, ServiceRef, TimeRange, Usage, UsageHistory, UsageInterest, UsageObject,
    UsageScope, UsageSnapshot, UsageStatus, WorkloadKind,
};
pub use pod::{Health, PodRow, PodSnapshot, Ready};
pub use table::{Column, ColumnType, TableRow, TableSnapshot};
