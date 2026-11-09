//! The tokio side of the app: kubeconfig catalog, clients and watches.
//!
//! This crate never depends on GPUI. It talks to the UI only through the
//! plain data types in `model`: the UI sends [`Command`]s, the engine sends
//! [`Event`]s.

pub mod age;
pub mod catalog;
mod client;
mod errors;
pub mod pods;
pub mod shell;
pub mod shell_env;

use std::sync::Arc;

use kube::config::Kubeconfig;
use model::{Command, ConnectError, ConnectionState, ContextName, Event};
use tokio::sync::mpsc;

pub use catalog::KubeconfigSources;
use client::ClientPool;
use pods::AbortOnDrop;

/// The UI's handle to a running engine. Dropping it stops the engine.
#[derive(Clone, Debug)]
pub struct EngineHandle {
    commands: mpsc::UnboundedSender<Command>,
    shells: mpsc::UnboundedSender<shell::Request>,
}

impl EngineHandle {
    /// Sends a command. Returns false if the engine has stopped.
    pub fn send(&self, command: Command) -> bool {
        self.commands.send(command).is_ok()
    }
}

/// Starts the engine on `runtime` and returns its handle and event stream.
///
/// The engine loads the catalog right away and sends [`Event::Catalog`].
/// It stops when every [`EngineHandle`] is dropped.
pub fn start(
    runtime: &tokio::runtime::Handle,
    sources: KubeconfigSources,
) -> (EngineHandle, mpsc::UnboundedReceiver<Event>) {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (shell_tx, shell_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    runtime.spawn(run(sources, command_rx, shell_rx, event_tx));
    (
        EngineHandle {
            commands: command_tx,
            shells: shell_tx,
        },
        event_rx,
    )
}

struct Engine {
    sources: KubeconfigSources,
    events: mpsc::UnboundedSender<Event>,
    kubeconfig: Arc<Kubeconfig>,
    clients: ClientPool,
    catalog: model::Catalog,
    /// The selected context's connection and pod watch. Replacing it drops
    /// the old task, which stops the old watch.
    active: Option<AbortOnDrop<()>>,
    /// Node shells' helper pods that are up.
    helpers: shell::HelperPods,
}

async fn run(
    sources: KubeconfigSources,
    mut commands: mpsc::UnboundedReceiver<Command>,
    mut shells: mpsc::UnboundedReceiver<shell::Request>,
    events: mpsc::UnboundedSender<Event>,
) {
    let mut engine = Engine {
        sources,
        events,
        kubeconfig: Arc::default(),
        clients: ClientPool::default(),
        catalog: model::Catalog::default(),
        active: None,
        helpers: shell::HelperPods::default(),
    };
    engine.reload().await;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::ReloadCatalog) => engine.reload().await,
                Some(Command::SelectContext(name)) => engine.select(name),
                None => break,
            },
            Some(request) = shells.recv() => engine.shell(request),
        }
    }
    tracing::debug!("engine stopped: every handle was dropped");
}

impl Engine {
    async fn reload(&mut self) {
        let sources = self.sources.clone();
        // Reading files is blocking I/O; keep it off the async workers.
        let loaded = match tokio::task::spawn_blocking(move || catalog::load(&sources)).await {
            Ok(loaded) => loaded,
            Err(err) => {
                tracing::error!("loading kubeconfig failed: {err}");
                return;
            }
        };
        for problem in &loaded.catalog.problems {
            tracing::warn!(path = %problem.path.display(), "{}", problem.message);
        }
        tracing::info!(
            contexts = loaded.catalog.contexts.len(),
            files = loaded.catalog.sources.len(),
            "kubeconfig loaded"
        );
        self.kubeconfig = Arc::new(loaded.merged);
        self.clients.clear();
        self.catalog = loaded.catalog.clone();
        // A send error means the UI is gone; the loop ends on its own.
        let _ = self.events.send(Event::Catalog(loaded.catalog));
    }

    fn select(&mut self, context: ContextName) {
        tracing::info!(%context, "context selected");
        // Stop the previous watch before starting the next.
        self.active = None;
        let events = self.events.clone();
        if self.catalog.get(&context).is_none() {
            let _ = events.send(Event::Connection {
                context,
                state: ConnectionState::Failed(ConnectError::UnknownContext),
            });
            return;
        }
        let clients = self.clients.clone();
        let kubeconfig = Arc::clone(&self.kubeconfig);
        let task = tokio::spawn(async move {
            let _ = events.send(Event::Connection {
                context: context.clone(),
                state: ConnectionState::Connecting,
            });
            match clients.get(&context, &kubeconfig).await {
                Ok(client) => pods::watch_pods(context, client, events).await,
                Err(err) => {
                    tracing::warn!(%context, "could not connect: {err}");
                    let _ = events.send(Event::Connection {
                        context,
                        state: ConnectionState::Failed(err),
                    });
                }
            }
        });
        self.active = Some(AbortOnDrop(task));
    }
}
