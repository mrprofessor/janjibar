//! Shells for a terminal pane: `exec` into a pod, a root shell on a node,
//! and a kubeconfig pinned to one context for a local shell.
//!
//! The UI asks through [`EngineHandle::open_shell`] and
//! [`EngineHandle::pin_kubeconfig`]. The engine loop owns the clients and
//! the merged kubeconfig, so the requests go through it; each shell then
//! runs in its own task and talks to the terminal over its
//! [`ShellSession`].

mod exec;
mod kubeconfig;
mod node;

pub(crate) use node::HelperPods;

use std::sync::Arc;

use kube::config::Kubeconfig;
use model::{
    ConnectError, ContextName, PinnedKubeconfig, ShellError, ShellEvent, ShellSession, ShellTarget,
};
use tokio::sync::{mpsc, oneshot};

use crate::client::ClientPool;
use crate::{Engine, EngineHandle};

/// How many output events may wait for the terminal before the shell is
/// held back. Each holds one read, at most [`READ_BUFFER`] bytes.
const EVENT_QUEUE: usize = 64;

/// How much program output one read takes.
pub(crate) const READ_BUFFER: usize = 32 * 1024;

/// A shell request for the engine loop.
#[derive(Debug)]
pub(crate) enum Request {
    /// Start a shell and serve it on these channel ends.
    Open { target: ShellTarget, io: ShellIo },
    /// Write a kubeconfig pinned to `context` and send it back.
    Pin {
        context: ContextName,
        reply: oneshot::Sender<Result<PinnedKubeconfig, ShellError>>,
    },
    /// Delete every node shell's helper pod, then reply.
    CloseAll { reply: oneshot::Sender<()> },
}

/// The engine's ends of a [`ShellSession`].
#[derive(Debug)]
pub(crate) struct ShellIo {
    /// Input from the terminal. `None` from `recv` means the terminal is
    /// gone and the shell must stop.
    pub(crate) input: mpsc::UnboundedReceiver<model::ShellInput>,
    /// Events for the terminal.
    pub(crate) events: mpsc::Sender<ShellEvent>,
}

impl ShellIo {
    /// Sends an event. Returns false when the terminal is gone.
    pub(crate) async fn send(&self, event: ShellEvent) -> bool {
        self.events.send(event).await.is_ok()
    }

    /// Reports a failure. The terminal may already be gone, which is fine:
    /// nobody is left to tell.
    pub(crate) async fn fail(&self, error: ShellError) {
        let _ = self.events.send(ShellEvent::Failed(error)).await;
    }
}

impl EngineHandle {
    /// Starts a shell on `target` and returns the terminal's end.
    ///
    /// It returns at once. Progress, output and the exit arrive as
    /// [`ShellEvent`]s; if the engine has stopped, the events channel is
    /// closed. Drop the session to end the shell.
    pub fn open_shell(&self, target: ShellTarget) -> ShellSession {
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE);
        let io = ShellIo {
            input: input_rx,
            events: events_tx,
        };
        // A send error means the engine stopped; the session's events
        // channel is then closed, which the terminal reports.
        let _ = self.shells.send(Request::Open { target, io });
        ShellSession {
            input: input_tx,
            events: events_rx,
        }
    }

    /// Writes a kubeconfig that holds only `context` and makes it current,
    /// for a local shell to use as `KUBECONFIG`.
    ///
    /// The file is deleted when the returned [`PinnedKubeconfig`] is
    /// dropped. The receiver fails if the engine has stopped.
    pub fn pin_kubeconfig(
        &self,
        context: ContextName,
    ) -> oneshot::Receiver<Result<PinnedKubeconfig, ShellError>> {
        let (reply, receiver) = oneshot::channel();
        // A send error drops `reply`, so the receiver reports it.
        let _ = self.shells.send(Request::Pin { context, reply });
        receiver
    }

    /// Deletes every node shell's helper pod, for an app that is quitting.
    ///
    /// The runtime stops when the app exits, and with it any cleanup still
    /// in flight, so a privileged pod would outlive the app until its
    /// deadline. Wait on the receiver, with a timeout, before exiting. It
    /// resolves once the deletes are answered, or fails if the engine has
    /// stopped. Pod shells need nothing: their websocket closes with the
    /// process.
    pub fn close_shells(&self) -> oneshot::Receiver<()> {
        let (reply, receiver) = oneshot::channel();
        // A send error drops `reply`, so the receiver reports it.
        let _ = self.shells.send(Request::CloseAll { reply });
        receiver
    }
}

impl Engine {
    /// Serves a shell request. Never blocks the loop: the work runs in its
    /// own task.
    pub(crate) fn shell(&self, request: Request) {
        match request {
            Request::Open { target, io } => {
                let known = self.catalog.get(target.context()).is_some();
                let clients = self.clients.clone();
                let helpers = self.helpers.clone();
                let kubeconfig = Arc::clone(&self.kubeconfig);
                tokio::spawn(async move {
                    if !known {
                        io.fail(ShellError::Connect(ConnectError::UnknownContext))
                            .await;
                        return;
                    }
                    // Boxed: a shell future holds the read buffer and the
                    // pod watch, too big for the stack of every caller.
                    Box::pin(run(target, io, &clients, &kubeconfig, &helpers)).await;
                });
            }
            Request::Pin { context, reply } => {
                let kubeconfig = Arc::clone(&self.kubeconfig);
                tokio::spawn(kubeconfig::pin(kubeconfig, context, reply));
            }
            Request::CloseAll { reply } => {
                let helpers = self.helpers.clone();
                tokio::spawn(async move {
                    helpers.delete_all().await;
                    // The asker may have given up waiting; that is fine.
                    let _ = reply.send(());
                });
            }
        }
    }
}

/// Connects and runs one shell until it ends or the terminal goes away.
async fn run(
    target: ShellTarget,
    io: ShellIo,
    clients: &ClientPool,
    kubeconfig: &Kubeconfig,
    helpers: &HelperPods,
) {
    let client = match clients.get(target.context(), kubeconfig).await {
        Ok(client) => client,
        Err(err) => {
            io.fail(ShellError::Connect(err)).await;
            return;
        }
    };
    match target {
        ShellTarget::Pod {
            namespace,
            pod,
            container,
            ..
        } => {
            let shell = exec::Exec {
                namespace: &namespace,
                pod: &pod,
                container: container.as_deref(),
                command: exec::shell_command(),
                pending: Vec::new(),
            };
            shell.run(client, io).await;
        }
        ShellTarget::Node { node, options, .. } => {
            Box::pin(node::run(client, &node, &options, helpers, io)).await;
        }
    }
}

/// Maps an error from an API call made for a shell.
pub(crate) fn shell_error(err: &kube::Error) -> ShellError {
    match err {
        kube::Error::Api(status) => match status.code {
            403 => ShellError::Forbidden(status.message.clone()),
            404 => ShellError::NotFound(status.message.clone()),
            _ => ShellError::Other(status.message.clone()),
        },
        other => match crate::errors::from_request(other) {
            ConnectError::Other(message) => ShellError::Other(message),
            connect => ShellError::Connect(connect),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_error(code: u16, message: &str) -> kube::Error {
        kube::Error::Api(
            kube::core::Status::failure(message, "Reason")
                .with_code(code)
                .boxed(),
        )
    }

    #[test]
    fn api_errors_map_to_what_the_user_can_act_on() {
        assert_eq!(
            shell_error(&api_error(403, "pods \"web\" is forbidden")),
            ShellError::Forbidden("pods \"web\" is forbidden".to_owned())
        );
        assert_eq!(
            shell_error(&api_error(404, "pods \"web\" not found")),
            ShellError::NotFound("pods \"web\" not found".to_owned())
        );
        assert_eq!(
            shell_error(&api_error(400, "container nope is not valid")),
            ShellError::Other("container nope is not valid".to_owned())
        );
    }
}
