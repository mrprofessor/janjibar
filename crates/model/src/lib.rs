//! Plain data that crosses the bridge between the engine and the UI.
//!
//! Nothing here knows about GPUI or kube-rs. Types are cheap to clone and
//! safe to send between threads.

pub mod brand;
mod catalog;
mod event;
mod helm;
mod pod;
mod shell;

pub use catalog::{Catalog, ContextInfo, ContextName, SourceProblem};
pub use event::{Command, ConnectError, ConnectionState, Event};
pub use helm::{HelmChart, HelmRelease, HelmReleaseDetails, HelmRevision, HelmStatus, HelmStorage};
pub use pod::{Health, PodRow, PodSnapshot, Ready};
pub use shell::{
    NodeShellOptions, PinnedKubeconfig, ShellError, ShellEvent, ShellExit, ShellInput,
    ShellSession, ShellTarget,
};
