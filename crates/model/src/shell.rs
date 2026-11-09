//! Shells: a terminal attached to a pod, a node, or a local shell pinned to
//! a context.
//!
//! The engine runs the Kubernetes side; a terminal pane draws the bytes. A
//! [`ShellSession`] is the pair of channels between them: [`ShellInput`]
//! goes to the engine, [`ShellEvent`]s come back. Dropping the session's
//! input sender ends the shell, and the engine cleans up after it.

use std::fmt;
use std::path::{Path, PathBuf};

use tokio::sync::{mpsc, oneshot};

use crate::{ConnectError, ContextName};

/// Where a shell runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellTarget {
    /// A shell in a container of a running pod, through `exec`.
    Pod {
        /// The context.
        context: ContextName,
        /// The pod's namespace.
        namespace: String,
        /// The pod.
        pod: String,
        /// The container; `None` is the pod's default container.
        container: Option<String>,
    },
    /// A root shell on a node, through a short-lived privileged pod that
    /// enters the host's namespaces with `nsenter`.
    Node {
        /// The context.
        context: ContextName,
        /// The node.
        node: String,
        /// The image and namespace of the helper pod.
        options: NodeShellOptions,
    },
}

impl ShellTarget {
    /// The context the shell runs in.
    pub fn context(&self) -> &ContextName {
        match self {
            Self::Pod { context, .. } | Self::Node { context, .. } => context,
        }
    }
}

/// How a node shell's helper pod is made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeShellOptions {
    /// The image. It needs `nsenter` and `sleep`, which busybox has.
    pub image: String,
    /// Where the pod is created. It must allow privileged pods.
    pub namespace: String,
}

impl NodeShellOptions {
    /// The default image: small, multi-arch, and its busybox has `nsenter`.
    pub const DEFAULT_IMAGE: &str = "docker.io/library/busybox:1.37";
    /// The default namespace. Pod security admission usually leaves it
    /// privileged.
    pub const DEFAULT_NAMESPACE: &str = "kube-system";
}

impl Default for NodeShellOptions {
    fn default() -> Self {
        Self {
            image: Self::DEFAULT_IMAGE.to_owned(),
            namespace: Self::DEFAULT_NAMESPACE.to_owned(),
        }
    }
}

/// What the terminal sends to a shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellInput {
    /// Keys, pastes and replies to terminal queries, already encoded.
    Bytes(Vec<u8>),
    /// The terminal's grid changed.
    Resize {
        /// Columns.
        columns: u16,
        /// Rows.
        rows: u16,
    },
}

/// What a shell sends to the terminal, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellEvent {
    /// A line about getting the shell ready, such as starting a helper pod.
    /// Not program output; the terminal shows it dimmed.
    Progress(String),
    /// Program output, with stderr merged in by the TTY.
    Output(Vec<u8>),
    /// The shell ended. Nothing follows.
    Exited(ShellExit),
    /// The shell could not start or broke off. Nothing follows.
    Failed(ShellError),
}

/// How a shell ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellExit {
    /// The exit code, when the server reported one.
    pub code: Option<i32>,
    /// The server's message, when the exit was not a plain success.
    pub message: Option<String>,
}

/// Why a shell could not start or broke off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellError {
    /// The context could not be reached.
    Connect(ConnectError),
    /// The pod, container or node does not exist.
    NotFound(String),
    /// The user may not exec into the pod, or create the helper pod.
    Forbidden(String),
    /// The container has no `sh`.
    NoShell(String),
    /// The node shell's helper pod could not start.
    PodFailed(String),
    /// The helper pod took too long to start.
    Timeout(String),
    /// Anything else, such as a dropped connection.
    Other(String),
}

impl fmt::Display for ShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(err) => fmt::Display::fmt(err, f),
            Self::NotFound(msg) => write!(f, "not found: {msg}"),
            Self::Forbidden(msg) => write!(f, "not allowed: {msg}"),
            Self::NoShell(msg) => write!(
                f,
                "this container has no shell; pick another container or use a node shell ({msg})"
            ),
            Self::PodFailed(msg) => write!(f, "the node shell pod could not start: {msg}"),
            Self::Timeout(msg) => write!(f, "timed out: {msg}"),
            Self::Other(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for ShellError {}

/// The terminal's end of a running shell.
///
/// Send [`ShellInput`] on `input`, read [`ShellEvent`]s from `events`. The
/// engine stops the shell when `input` is dropped, and for a node shell
/// deletes its helper pod. `events` is bounded, so a terminal that reads
/// slowly holds the program back instead of filling memory.
#[derive(Debug)]
pub struct ShellSession {
    /// Input for the shell.
    pub input: mpsc::UnboundedSender<ShellInput>,
    /// Output and status from the shell.
    pub events: mpsc::Receiver<ShellEvent>,
}

/// A kubeconfig file that holds one context, its cluster and its user,
/// with that context current. A local shell with `KUBECONFIG` set to it
/// talks to that cluster only, whatever the user's own kubeconfig says.
///
/// The file holds credentials, so only the user can read it, and the
/// engine deletes it when this value is dropped.
#[derive(Debug)]
pub struct PinnedKubeconfig {
    path: PathBuf,
    context: ContextName,
    /// The engine deletes the file when this sender is dropped.
    _release: oneshot::Sender<()>,
}

impl PinnedKubeconfig {
    /// Wraps a written file. `release` is dropped with the value, which
    /// tells the engine to delete the file.
    pub fn new(path: PathBuf, context: ContextName, release: oneshot::Sender<()>) -> Self {
        Self {
            path,
            context,
            _release: release,
        }
    }

    /// The file, for `KUBECONFIG`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The context it pins.
    pub fn context(&self) -> &ContextName {
        &self.context
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_say_what_to_do() {
        let err = ShellError::NoShell("exec: \"sh\": executable file not found".to_owned());
        assert!(err.to_string().starts_with("this container has no shell"));
        let err = ShellError::Connect(ConnectError::UnknownContext);
        assert_eq!(err.to_string(), ConnectError::UnknownContext.to_string());
    }

    #[test]
    fn a_target_knows_its_context() {
        let target = ShellTarget::Node {
            context: ContextName::new("kind"),
            node: "worker".to_owned(),
            options: NodeShellOptions::default(),
        };
        assert_eq!(target.context().as_str(), "kind");
    }
}
