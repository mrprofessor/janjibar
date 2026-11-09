//! The tokio side of the app: kubeconfig catalog, clients and watches.
//!
//! This crate never depends on GPUI. It talks to the UI only through the
//! plain data types in `model`: the UI sends [`Command`]s, the engine sends
//! [`Event`]s.

pub mod catalog;
mod client;
pub mod discovery;
mod errors;
pub mod forward;
mod hub;
pub mod logs;
mod owners;
pub mod shell_env;
pub mod tables;
mod task;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kube::config::Kubeconfig;
use model::{Command, ConnectError, ConnectionState, ContextName, Event, ForwardSpec, LogTabId};
use tokio::sync::mpsc;

pub use catalog::KubeconfigSources;
use client::ClientPool;
use forward::Forwards;
pub use hub::{DEFAULT_LINGER, SNAPSHOT_WINDOW};
pub use kinds::age;
use task::AbortOnDrop;

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

/// Engine settings beyond the kubeconfig. The defaults suit tests: nothing
/// is saved, and watches linger for the usual 45 s.
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// Where port-forwards are saved. `None` keeps them in memory only.
    pub forwards_file: Option<PathBuf>,
    /// How long a watch stays warm after its last subscriber leaves.
    pub linger: Duration,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            forwards_file: None,
            linger: DEFAULT_LINGER,
        }
    }
}

impl EngineOptions {
    /// The app's settings: forwards saved under `~/.config`.
    pub fn from_env() -> Self {
        Self {
            forwards_file: forward::saved::default_path(),
            ..Self::default()
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
    hub: hub::Hub,
    /// Connecting to the selected context. Replaced on the next selection.
    connecting: Option<AbortOnDrop<()>>,
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
    let clients = ClientPool::default();
    let (hub, mut reaps) = hub::Hub::new(events.clone(), clients.clone(), options.linger);
    let mut engine = Engine {
        sources,
        events,
        kubeconfig: Arc::default(),
        clients,
        catalog: model::Catalog::default(),
        hub,
        connecting: None,
        logs: HashMap::new(),
        forwards,
    };
    engine.reload().await;
    engine.forwards.announce();
    loop {
        // Both branches are cancel safe (mpsc `recv`).
        tokio::select! {
            command = commands.recv() => match command {
                Some(command) => engine.handle(command).await,
                None => break,
            },
            Some(reap) = reaps.recv() => engine.hub.reap(reap),
        }
    }
    tracing::debug!("engine stopped: every handle was dropped");
}

impl Engine {
    async fn handle(&mut self, command: Command) {
        match command {
            Command::ReloadCatalog => self.reload().await,
            Command::SelectContext(name) => self.select(name),
            Command::Subscribe {
                id,
                context,
                resource,
                namespace,
            } => self.hub.subscribe(
                id,
                hub::WatchKey {
                    context,
                    resource,
                    namespace,
                },
            ),
            Command::Unsubscribe(id) => self.hub.unsubscribe(id),
            Command::Inspect(target) => self.hub.inspect(target),
            Command::OpenLogs(request) => {
                let tab = request.tab;
                let task = logs::open(
                    request,
                    self.clients.clone(),
                    Arc::clone(&self.kubeconfig),
                    self.events.clone(),
                );
                self.logs.insert(tab, task);
            }
            Command::CloseLogs(tab) => {
                self.logs.remove(&tab);
            }
            Command::StartForward(spec) => self.start_forward(spec),
            Command::StopForward(id) => self.forwards.stop(id),
        }
    }

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
        self.hub.set_kubeconfig(Arc::clone(&self.kubeconfig));
        self.clients.clear();
        self.catalog = loaded.catalog.clone();
        // A send error means the UI is gone; the loop ends on its own.
        let _ = self.events.send(Event::Catalog(loaded.catalog));
    }

    /// Builds the context's client, probes the API server to report the
    /// connection, resumes its saved port-forwards, then sends the kinds it
    /// serves. Watches start separately, through subscriptions.
    fn select(&mut self, context: ContextName) {
        tracing::info!(%context, "context selected");
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
            let send = |state| {
                // A send error means the UI is gone.
                let _ = events.send(Event::Connection {
                    context: context.clone(),
                    state,
                });
            };
            send(ConnectionState::Connecting);
            let client = match clients.get(&context, &kubeconfig).await {
                Ok(client) => client,
                Err(err) => {
                    tracing::warn!(%context, "could not connect: {err}");
                    send(ConnectionState::Failed(err));
                    return;
                }
            };
            // Saved forwards start once a client exists. Each one reports its
            // own failures, so they do not wait for the probe.
            forwards.resume(&context, &client);
            if let Err(err) = client.apiserver_version().await {
                let err = errors::connect_error_from_request(&err);
                tracing::warn!(%context, "could not connect: {err}");
                send(ConnectionState::Failed(err));
                return;
            }
            send(ConnectionState::Live);
            // The kinds this context serves. Untyped kinds get a view in #19.
            discovery::send_kinds(context.clone(), client, events.clone()).await;
        });
        self.connecting = Some(AbortOnDrop(task));
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
