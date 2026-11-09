//! Parking: an idle terminal is saved as a snapshot and its live state
//! freed, then restored the moment it is needed.
//!
//! A terminal with full scrollback holds about 10 MB of pages. Its
//! ghostty-vt snapshot, which keeps the scrollback compressed, is about
//! half a megabyte, and restoring it takes about a millisecond. So a
//! terminal that has had no output, input, resize or paint for
//! [`ParkOptions::idle`] (60 seconds by default) is parked: its snapshot
//! goes to a [`ParkStore`] and the terminal and its render state are
//! dropped. The pty or stream behind it stays open. Output, input, a
//! resize, a paint, a reveal, a copy, or a color change restores it first.
//!
//! The approach comes from the Superlogical server, which parks idle
//! terminals the same way on top of libghostty.
//!
//! The snapshot holds the screen and the scrollback, which can hold
//! secrets. [`MemoryStore`], the default, keeps it in this process.
//! [`DirStore`] writes it to files only the user can read. An application
//! that wants it encrypted, in a database, or elsewhere implements
//! [`ParkStore`].

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where parked snapshots are kept. Calls come from the terminal's engine
/// thread, one terminal at a time per key.
pub trait ParkStore: Send + Sync + 'static {
    /// Keeps the snapshot of terminal `key`, replacing any earlier one.
    fn save(&self, key: u64, snapshot: &[u8]) -> io::Result<()>;
    /// Returns the snapshot of terminal `key` and forgets it.
    fn take(&self, key: u64) -> io::Result<Vec<u8>>;
    /// Forgets the snapshot of terminal `key`, if any. Called when a parked
    /// terminal closes.
    fn remove(&self, key: u64);
}

/// Keeps snapshots in this process' memory. The default store.
#[derive(Debug, Default)]
pub struct MemoryStore {
    snapshots: Mutex<HashMap<u64, Vec<u8>>>,
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Vec<u8>>> {
        self.snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The bytes held for every parked terminal.
    pub fn len_bytes(&self) -> usize {
        self.lock().values().map(Vec::len).sum()
    }
}

impl ParkStore for MemoryStore {
    fn save(&self, key: u64, snapshot: &[u8]) -> io::Result<()> {
        self.lock().insert(key, snapshot.to_vec());
        Ok(())
    }

    fn take(&self, key: u64) -> io::Result<Vec<u8>> {
        self.lock()
            .remove(&key)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no parked snapshot"))
    }

    fn remove(&self, key: u64) {
        self.lock().remove(&key);
    }
}

/// Keeps snapshots as files in a directory, one per parked terminal,
/// readable and writable by the user only (mode 0600 on Unix). A file is
/// removed when its terminal is restored or closes.
#[derive(Debug, Clone)]
pub struct DirStore {
    dir: PathBuf,
}

impl DirStore {
    /// A store in `dir`, which is created when it does not exist.
    pub fn new(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// The directory the snapshots go to.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, key: u64) -> PathBuf {
        self.dir
            .join(format!("terminal-{}-{key}.snapshot", std::process::id()))
    }
}

impl ParkStore for DirStore {
    fn save(&self, key: u64, snapshot: &[u8]) -> io::Result<()> {
        use std::io::Write as _;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(self.path(key))?;
        file.write_all(snapshot)?;
        file.sync_data()
    }

    fn take(&self, key: u64) -> io::Result<Vec<u8>> {
        let path = self.path(key);
        let snapshot = std::fs::read(&path)?;
        let _ = std::fs::remove_file(path);
        Ok(snapshot)
    }

    fn remove(&self, key: u64) {
        let _ = std::fs::remove_file(self.path(key));
    }
}

/// When a terminal parks and where its snapshot goes.
///
/// The default parks after 60 seconds idle, the Superlogical server's
/// threshold, in a [`MemoryStore`].
#[derive(Clone)]
#[non_exhaustive]
pub struct ParkOptions {
    pub(crate) enabled: bool,
    pub(crate) idle: Duration,
    pub(crate) store: Arc<dyn ParkStore>,
}

impl Default for ParkOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            idle: Duration::from_secs(60),
            store: Arc::new(MemoryStore::default()),
        }
    }
}

impl ParkOptions {
    /// Whether idle terminals park at all.
    #[must_use]
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// How long a terminal stays idle before it parks.
    #[must_use]
    pub fn with_idle(mut self, idle: Duration) -> Self {
        self.idle = idle;
        self
    }

    /// Where snapshots go.
    #[must_use]
    pub fn with_store(mut self, store: impl ParkStore) -> Self {
        self.store = Arc::new(store);
        self
    }

    /// Where snapshots go, shared with other terminals or kept by the
    /// application to inspect.
    #[must_use]
    pub fn with_shared_store(mut self, store: Arc<dyn ParkStore>) -> Self {
        self.store = store;
        self
    }

    /// Whether idle terminals park.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// How long a terminal stays idle before it parks.
    pub fn idle(&self) -> Duration {
        self.idle
    }

    /// Where snapshots go.
    pub fn store(&self) -> &Arc<dyn ParkStore> {
        &self.store
    }
}

impl fmt::Debug for ParkOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParkOptions")
            .field("enabled", &self.enabled)
            .field("idle", &self.idle)
            .finish_non_exhaustive()
    }
}

impl PartialEq for ParkOptions {
    fn eq(&self, other: &Self) -> bool {
        self.enabled == other.enabled
            && self.idle == other.idle
            && Arc::ptr_eq(&self.store, &other.store)
    }
}

impl Eq for ParkOptions {}

/// A key no other terminal in this process uses.
pub(crate) fn next_key() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_memory_store_keeps_and_forgets_snapshots() {
        let store = MemoryStore::new();
        store.save(1, b"screen").unwrap();
        assert_eq!(store.len_bytes(), 6);
        assert_eq!(store.take(1).unwrap(), b"screen");
        assert!(store.take(1).is_err(), "taken once");
        store.save(2, b"x").unwrap();
        store.remove(2);
        assert_eq!(store.len_bytes(), 0);
    }

    #[test]
    fn a_dir_store_writes_private_files_and_removes_them() {
        let dir = std::env::temp_dir().join(format!("term-park-{}", std::process::id()));
        let store = DirStore::new(&dir).unwrap();
        store.save(7, b"scrollback").unwrap();
        let path = store.path(7);
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "only the user can read it");
        }
        assert_eq!(store.take(7).unwrap(), b"scrollback");
        assert!(!path.exists(), "a restore removes the file");
        store.save(8, b"x").unwrap();
        store.remove(8);
        assert!(!store.path(8).exists(), "a close removes the file");
        let _ = std::fs::remove_dir_all(dir);
    }
}
