//! Messages between the UI and the engine.

use std::fmt;

use crate::{
    Catalog, ContextName, HelmCommand, HelmEvent, KindCatalog, LogChunk, LogRequest, LogState,
    LogTabId, PodSnapshot, TableSnapshot,
};

/// What the UI asks the engine to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Re-read the kubeconfig files.
    ReloadCatalog,
    /// Connect to a context and watch its pods. Stops the previous watch.
    SelectContext(ContextName),
    /// Open a log tab and start streaming into it.
    OpenLogs(LogRequest),
    /// Close a log tab and stop its streams.
    CloseLogs(LogTabId),
    /// Helm releases: watch them, or load one revision.
    Helm(HelmCommand),
}

/// What the engine tells the UI.
#[derive(Clone, Debug)]
pub enum Event {
    /// A fresh catalog. Sent at startup and after every reload.
    Catalog(Catalog),
    /// The connection state of a context changed.
    Connection {
        /// The context.
        context: ContextName,
        /// Its new state.
        state: ConnectionState,
    },
    /// New pod rows for the selected context.
    Pods(PodSnapshot),
    /// The kinds a context serves, sent after it connects.
    Kinds(KindCatalog),
    /// New rows or a new state for a watched kind.
    Table(TableSnapshot),
    /// New lines for a log tab, at most one chunk per tab every 33 ms.
    Logs(LogChunk),
    /// A log tab's state changed.
    LogState {
        /// The tab.
        tab: LogTabId,
        /// Its new state.
        state: LogState,
    },
    /// Helm releases of the selected context, or one decoded revision.
    Helm(HelmEvent),
}

/// Where a context's connection stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// Building the client or waiting for the first list.
    Connecting,
    /// Watching, with data on screen.
    Live,
    /// Was live, now retrying after an error. Rows stay on screen.
    Retrying(String),
    /// Could not connect.
    Failed(ConnectError),
}

/// Why a context could not be reached. Each variant gets its own empty state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectError {
    /// The context is not in the catalog any more.
    UnknownContext,
    /// The kubeconfig entry is broken: missing cluster, bad URL, bad cert.
    Config(String),
    /// The exec auth plugin is not on PATH.
    AuthPluginMissing {
        /// The command kubeconfig asks for.
        command: String,
        /// The PATH that was searched.
        path: String,
    },
    /// The exec auth plugin ran and failed, or credentials were rejected.
    Auth(String),
    /// The API server could not be reached.
    Unreachable(String),
    /// The user may not list pods here.
    Forbidden(String),
    /// Anything else.
    Other(String),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownContext => f.write_str("this context is no longer in your kubeconfig"),
            Self::Config(msg) => write!(f, "kubeconfig problem: {msg}"),
            Self::AuthPluginMissing { command, .. } => {
                write!(f, "auth plugin `{command}` was not found on PATH")
            }
            Self::Auth(msg) => write!(f, "authentication failed: {msg}"),
            Self::Unreachable(msg) => write!(f, "cluster unreachable: {msg}"),
            Self::Forbidden(msg) => write!(f, "not allowed to list pods: {msg}"),
            Self::Other(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for ConnectError {}
