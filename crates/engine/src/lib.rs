//! The tokio side of the app: kubeconfig catalog, clients and watches.
//!
//! This crate never depends on GPUI. It talks to the UI only through the
//! plain data types in `model`: the UI sends [`Command`]s, the engine sends
//! [`Event`]s.

pub mod age;
pub mod catalog;
mod client;
pub mod discovery;
mod errors;
pub mod forward;
pub mod logs;
pub mod pods;
pub mod shell_env;
pub mod tables;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use kube::config::Kubeconfig;
use model::{Command, ConnectError, ConnectionState, ContextName, Event, ForwardSpec, LogTabId};
use tokio::sync::mpsc;

pub use catalog::KubeconfigSources;
use client::ClientPool;
use forward::Forwards;
use pods::AbortOnDrop;

/// The UI's handle to a running engine. Dropping it stops the engine.
#[derive(Clone, Debug)]
pub struct EngineHandle {
    commands: mpsc::UnboundedSender<Command>,
}

impl EngineHandle {
    /// Sends a command. Returns false if the engine has stopped.
    pub fn send(&self, command: Command) -> bool {
        self.commands.send(command).is_ok()
    }
}

/// Engine settings beyond the kubeconfig.
#[derive(Clone, Debug, Default)]
pub struct EngineOptions {
    /// Where port-forwards are saved. `None` keeps them in memory only.
    pub forwards_file: Option<PathBuf>,
}

impl EngineOptions {
    /// The app's settings: forwards saved under `~/.config`.
    pub fn from_env() -> Self {
        Self {
            forwards_file: forward::saved::default_path(),
        }
    }
}

/// Starts the engine with default options: nothing is read or written
/// besides the kubeconfig. See [`start_with`].
pub fn start(
    runtime: &tokio::runtime::Handle,
    sources: KubeconfigSources,
) -> (EngineHandle, mpsc::UnboundedReceiver<Event>) {
    start_with(runtime, sources, EngineOptions::default())
}

/// Starts the engine on `runtime` and returns its handle and event stream.
///
/// The engine loads the catalog right away and sends [`Event::Catalog`].
/// Saved port-forwards, if any, arrive as [`Event::Forwards`] and start when
/// their cluster connects. It stops when every [`EngineHandle`] is dropped.
pub fn start_with(
    runtime: &tokio::runtime::Handle,
    sources: KubeconfigSources,
    options: EngineOptions,
) -> (EngineHandle, mpsc::UnboundedReceiver<Event>) {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    runtime.spawn(run(sources, options, command_rx, event_tx));
    (
        EngineHandle {
            commands: command_tx,
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
    /// Open log tabs. Removing one drops its task and stops its streams.
    logs: HashMap<LogTabId, AbortOnDrop<()>>,
    /// Port-forwards across all clusters. They keep running when another
    /// context is selected.
    forwards: Forwards,
}

async fn run(
    sources: KubeconfigSources,
    options: EngineOptions,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<Event>,
) {
    let forwards = Forwards::load(options.forwards_file, events.clone()).await;
    let mut engine = Engine {
        sources,
        events,
        kubeconfig: Arc::default(),
        clients: ClientPool::default(),
        catalog: model::Catalog::default(),
        active: None,
        logs: HashMap::new(),
        forwards,
    };
    engine.reload().await;
    engine.forwards.announce();
    while let Some(command) = commands.recv().await {
        match command {
            Command::ReloadCatalog => engine.reload().await,
            Command::SelectContext(name) => engine.select(name),
            Command::OpenLogs(request) => {
                let tab = request.tab;
                let kubeconfig = Arc::clone(&engine.kubeconfig);
                let task = logs::open(
                    request,
                    engine.clients.clone(),
                    kubeconfig,
                    engine.events.clone(),
                );
                engine.logs.insert(tab, task);
            }
            Command::CloseLogs(tab) => {
                engine.logs.remove(&tab);
            }
            Command::StartForward(spec) => engine.start_forward(spec),
            Command::StopForward(id) => engine.forwards.stop(id),
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
        let forwards = self.forwards.clone();
        let task = tokio::spawn(async move {
            let _ = events.send(Event::Connection {
                context: context.clone(),
                state: ConnectionState::Connecting,
            });
            match clients.get(&context, &kubeconfig).await {
                Ok(client) => {
                    forwards.resume(&context, &client);
                    let kinds =
                        discovery::send_kinds(context.clone(), client.clone(), events.clone());
                    tokio::join!(kinds, pods::watch_pods(context, client, events));
                }
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

    /// Starts a forward, connecting to its cluster if needed.
    fn start_forward(&self, spec: ForwardSpec) {
        let known = self.catalog.get(&spec.context).is_some();
        let clients = self.clients.clone();
        let kubeconfig = Arc::clone(&self.kubeconfig);
        let context = spec.context.clone();
        self.forwards.start(spec, async move {
            if !known {
                return Err(ConnectError::UnknownContext);
            }
            clients.get(&context, &kubeconfig).await
        });
    }
}
