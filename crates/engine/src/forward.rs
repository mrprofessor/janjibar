//! Port-forwards, native to the engine.
//!
//! Each forward owns a `TcpListener` on 127.0.0.1. Every accepted
//! connection opens its own kube-rs port-forward, and services resolve to a
//! ready pod at that moment, so a forward keeps working when pods restart.
//! Forwards are saved per cluster and start again when their cluster
//! connects. The UI gets [`Event::Forwards`], batched like the other
//! snapshots.

mod resolve;
pub mod saved;
mod tunnel;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use kube::Client;
use model::{ConnectError, ContextName, Event, ForwardId, ForwardRow, ForwardSpec, ForwardState};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::pods::{AbortOnDrop, SNAPSHOT_WINDOW};

/// Every forward the engine knows, running or not. Cheap to clone.
#[derive(Clone)]
pub(crate) struct Forwards {
    shared: Arc<Shared>,
}

struct Shared {
    registry: Mutex<Registry>,
    /// Bumped on every change; the reporter sends a snapshot.
    changed: watch::Sender<u64>,
    /// The specs to save; the saver writes the latest.
    save: watch::Sender<Vec<ForwardSpec>>,
    next_id: AtomicU64,
}

#[derive(Default)]
struct Registry {
    entries: BTreeMap<ForwardId, Entry>,
}

struct Entry {
    row: ForwardRow,
    /// The listener task while it runs. Dropping it closes the port and
    /// every connection.
    task: Option<AbortOnDrop<()>>,
}

impl Forwards {
    /// Loads the forwards saved at `path`, if any, and starts reporting
    /// changes as [`Event::Forwards`]. Saved forwards wait for their cluster
    /// to connect; call [`Forwards::announce`] to send them the first time.
    /// With no path, forwards live in memory only.
    pub(crate) async fn load(path: Option<PathBuf>, events: mpsc::UnboundedSender<Event>) -> Self {
        let saved = match path.clone() {
            Some(path) => tokio::task::spawn_blocking(move || saved::load(&path))
                .await
                .unwrap_or_else(|err| {
                    tracing::warn!("reading saved port-forwards was interrupted: {err}");
                    Ok(Vec::new())
                })
                .unwrap_or_else(|err| {
                    tracing::warn!("saved port-forwards ignored: {err}");
                    Vec::new()
                }),
            None => Vec::new(),
        };
        let (changed, changes) = watch::channel(0);
        let (save, saves) = watch::channel(saved.clone());
        let shared = Arc::new(Shared {
            registry: Mutex::new(Registry::default()),
            changed,
            save,
            next_id: AtomicU64::new(1),
        });
        let forwards = Self { shared };
        {
            let mut registry = forwards.shared.lock();
            for spec in saved {
                let id = forwards.shared.next_id();
                registry.entries.insert(id, Entry::new(id, spec));
            }
        }
        tokio::spawn(report(Arc::downgrade(&forwards.shared), changes, events));
        if let Some(path) = path {
            tokio::spawn(write_saved(path, saves));
        }
        forwards
    }

    /// Sends the current forwards, if there are any. The engine calls it
    /// after the first catalog, so the UI knows the clusters first.
    pub(crate) fn announce(&self) {
        self.shared.bump();
    }

    /// Adds a forward, saves it and starts it once `client` resolves.
    ///
    /// Asking again for a forward that already exists with a fixed local
    /// port does not add a second one; a failed one gets another try.
    pub(crate) fn start<F>(&self, spec: ForwardSpec, client: F)
    where
        F: Future<Output = Result<Client, ConnectError>> + Send + 'static,
    {
        let existing = {
            let registry = self.shared.lock();
            registry
                .entries
                .values()
                .find(|e| spec.local_port != 0 && e.row.spec == spec)
                .map(|e| (e.row.id, e.row.state.clone()))
        };
        let id = match existing {
            Some((_, ForwardState::Starting | ForwardState::Listening)) => return,
            Some((id, _)) => id,
            None => {
                let id = self.shared.next_id();
                let mut registry = self.shared.lock();
                registry.entries.insert(id, Entry::new(id, spec));
                self.shared.save(&registry);
                id
            }
        };
        self.launch(id, client);
    }

    /// Starts every waiting or failed forward of `context` with `client`.
    /// Called when the cluster connects.
    pub(crate) fn resume(&self, context: &ContextName, client: &Client) {
        let ids: Vec<ForwardId> = self
            .shared
            .lock()
            .entries
            .values()
            .filter(|e| {
                &e.row.spec.context == context
                    && matches!(e.row.state, ForwardState::Waiting | ForwardState::Failed(_))
            })
            .map(|e| e.row.id)
            .collect();
        for id in ids {
            let client = client.clone();
            self.launch(id, async move { Ok(client) });
        }
    }

    /// Stops a forward, closes its port and forgets it.
    pub(crate) fn stop(&self, id: ForwardId) {
        let removed = {
            let mut registry = self.shared.lock();
            let removed = registry.entries.remove(&id);
            if removed.is_some() {
                self.shared.save(&registry);
            }
            removed
        };
        if let Some(entry) = removed {
            tracing::info!(target = %entry.row.spec.target, port = entry.row.spec.port, "port-forward stopped");
            // Dropping the entry aborts its listener outside the lock.
            drop(entry);
            self.shared.bump();
        }
    }

    fn launch<F>(&self, id: ForwardId, client: F)
    where
        F: Future<Output = Result<Client, ConnectError>> + Send + 'static,
    {
        let mut registry = self.shared.lock();
        let Some(entry) = registry.entries.get_mut(&id) else {
            return;
        };
        entry.row.state = ForwardState::Starting;
        let reporter = Reporter {
            shared: Arc::downgrade(&self.shared),
            id,
        };
        let spec = entry.row.spec.clone();
        // Replacing the old task, if any, stops it first.
        entry.task = Some(AbortOnDrop(tokio::spawn(async move {
            match client.await {
                Ok(client) => tunnel::run(reporter, spec, client).await,
                Err(err) => reporter.failed(err.to_string()),
            }
        })));
        drop(registry);
        self.shared.bump();
    }
}

impl Entry {
    fn new(id: ForwardId, spec: ForwardSpec) -> Self {
        Self {
            row: ForwardRow {
                id,
                spec,
                local_addr: None,
                state: ForwardState::Waiting,
                active: 0,
                total: 0,
                last_pod: None,
                last_error: None,
            },
            task: None,
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Registry> {
        // Every update leaves the registry whole, so a poisoned lock is usable.
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn next_id(&self) -> ForwardId {
        ForwardId::new(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    fn save(&self, registry: &Registry) {
        let specs = registry
            .entries
            .values()
            .map(|e| e.row.spec.clone())
            .collect();
        self.save.send_replace(specs);
    }

    /// Every row, by local port, then id.
    fn rows(&self) -> Arc<[ForwardRow]> {
        let mut rows: Vec<ForwardRow> = self
            .lock()
            .entries
            .values()
            .map(|e| e.row.clone())
            .collect();
        rows.sort_unstable_by_key(|r| (r.spec.local_port, r.id));
        rows.into()
    }
}

/// How a running forward reports back. It holds the manager weakly, so a
/// forward's tasks never keep the manager alive.
#[derive(Clone)]
struct Reporter {
    shared: Weak<Shared>,
    id: ForwardId,
}

impl Reporter {
    /// Applies `f` to the row. `f` returns true when it changed the spec,
    /// which then gets saved.
    fn update(&self, f: impl FnOnce(&mut ForwardRow) -> bool) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        {
            let mut registry = shared.lock();
            let Some(entry) = registry.entries.get_mut(&self.id) else {
                return;
            };
            if f(&mut entry.row) {
                shared.save(&registry);
            }
        }
        shared.bump();
    }

    fn listening(&self, addr: SocketAddr) {
        self.update(|row| {
            row.local_addr = Some(addr);
            row.state = ForwardState::Listening;
            // Keep the port the system picked, so it survives restarts.
            let picked = row.spec.local_port == 0;
            if picked {
                row.spec.local_port = addr.port();
            }
            picked
        });
    }

    fn failed(&self, message: String) {
        tracing::warn!(id = %self.id, "port-forward failed: {message}");
        self.update(|row| {
            row.local_addr = None;
            row.state = ForwardState::Failed(message);
            false
        });
    }

    fn opened(&self) {
        self.update(|row| {
            row.active = row.active.saturating_add(1);
            row.total = row.total.saturating_add(1);
            false
        });
    }

    fn closed(&self, outcome: Result<String, String>) {
        self.update(|row| {
            row.active = row.active.saturating_sub(1);
            match outcome {
                Ok(pod) => {
                    row.last_pod = Some(pod.into());
                    row.last_error = None;
                }
                Err(message) => row.last_error = Some(message.into()),
            }
            false
        });
    }
}

/// Sends [`Event::Forwards`] after changes, at most once per
/// [`SNAPSHOT_WINDOW`], and only when the rows differ.
async fn report(
    shared: Weak<Shared>,
    mut changes: watch::Receiver<u64>,
    events: mpsc::UnboundedSender<Event>,
) {
    let mut last: Arc<[ForwardRow]> = Arc::from([]);
    let mut last_sent: Option<Instant> = None;
    // Ends when the manager is dropped and its sender with it.
    while changes.changed().await.is_ok() {
        if let Some(sent) = last_sent {
            let wait = SNAPSHOT_WINDOW.saturating_sub(sent.elapsed());
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }
        changes.borrow_and_update();
        let Some(rows) = shared.upgrade().map(|s| s.rows()) else {
            return;
        };
        last_sent = Some(Instant::now());
        if rows == last {
            continue;
        }
        last = Arc::clone(&rows);
        if events.send(Event::Forwards(rows)).is_err() {
            return;
        }
    }
}

/// Writes the latest specs whenever they change. Writes happen one at a
/// time, in order, so the file always ends up with the newest list.
async fn write_saved(path: PathBuf, mut saves: watch::Receiver<Vec<ForwardSpec>>) {
    while saves.changed().await.is_ok() {
        let specs = saves.borrow_and_update().clone();
        let target = path.clone();
        match tokio::task::spawn_blocking(move || saved::save(&target, &specs)).await {
            Ok(Ok(())) => tracing::debug!(path = %path.display(), "port-forwards saved"),
            Ok(Err(err)) => tracing::warn!("could not save port-forwards: {err}"),
            Err(err) => tracing::warn!("saving port-forwards was interrupted: {err}"),
        }
    }
}
