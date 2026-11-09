//! Messages between the UI and the engine.

use std::fmt;
use std::sync::Arc;

use crate::resource::{Details, InspectTarget, OwnerLink};
use crate::{
    Catalog, ContextName, ForwardId, ForwardRow, ForwardSpec, GroupVersionResource, KindCatalog,
    LogChunk, LogRequest, LogState, LogTabId, TableSnapshot,
};

/// Names one table subscription. The UI picks the numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubscriptionId(pub u64);

/// What the UI asks the engine to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Re-read the kubeconfig files.
    ReloadCatalog,
    /// Connect to a context: build its client and report the connection.
    SelectContext(ContextName),
    /// Start receiving table snapshots for a kind, in one namespace or all.
    /// Watches are shared: subscribers to the same context, kind and
    /// namespace share one.
    Subscribe {
        /// Chosen by the UI; used to unsubscribe.
        id: SubscriptionId,
        /// The context.
        context: ContextName,
        /// The kind to list.
        resource: GroupVersionResource,
        /// One namespace, or `None` for all (and for cluster-wide kinds).
        namespace: Option<Arc<str>>,
    },
    /// Stop a subscription. The watch stays warm for a while in case the UI
    /// comes back.
    Unsubscribe(SubscriptionId),
    /// Follow one object in the inspector, or stop following.
    Inspect(Option<InspectTarget>),
    /// Open a log tab and start streaming into it.
    OpenLogs(LogRequest),
    /// Close a log tab and stop its streams.
    CloseLogs(LogTabId),
    /// Start a port-forward and save it for its cluster.
    StartForward(ForwardSpec),
    /// Stop a port-forward and forget it.
    StopForward(ForwardId),
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
    /// The kinds a context serves, sent after it connects.
    Kinds(KindCatalog),
    /// New rows or a new state for a watched kind. Typed kinds and
    /// server-side tables arrive the same way.
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
    /// Every port-forward, across all clusters, sorted by local port.
    Forwards(Arc<[ForwardRow]>),
    /// New details for the inspected object; `None` once it is gone.
    Details {
        /// The object.
        target: InspectTarget,
        /// What to show.
        details: Option<Box<Details>>,
    },
    /// The owner chain of the inspected object, root first.
    Owners {
        /// The object.
        target: InspectTarget,
        /// Owners, outermost first.
        chain: Vec<OwnerLink>,
    },
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
    /// The user may not list or watch this kind here.
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
            Self::Forbidden(msg) => write!(f, "not allowed: {msg}"),
            Self::Other(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for ConnectError {}
