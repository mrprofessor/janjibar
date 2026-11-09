//! Helm releases as the UI shows them, and the messages that carry them.
//!
//! The engine watches Helm's records, decodes each release's chart in the
//! background and sends [`HelmSnapshot`]s. The UI asks for one revision's
//! details with [`HelmCommand::LoadDetails`].

use std::sync::Arc;

use crate::{ContextName, HelmChart, HelmReleaseDetails, HelmStatus, HelmStorage};

/// Names a release: where it lives and how Helm stores it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HelmReleaseKey {
    /// The release's namespace.
    pub namespace: Arc<str>,
    /// The release name.
    pub name: Arc<str>,
    /// The kind of object that holds its revisions.
    pub storage: HelmStorage,
}

/// What the list knows about a release's chart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum HelmChartInfo {
    /// The latest revision is still being decoded in the background.
    #[default]
    Loading,
    /// Decoded from the latest revision.
    Known(HelmChart),
    /// The latest revision could not be read. The text says why.
    Unreadable(Arc<str>),
}

/// One stored revision in a release's history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelmHistoryEntry {
    /// The revision number.
    pub revision: u32,
    /// Its status label.
    pub status: HelmStatus,
    /// kubectl-style age of Helm's last write to it, like `3d4h`. `-` when
    /// unknown.
    pub age: Arc<str>,
}

/// One row of the Helm releases table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelmRow {
    /// Which release this is.
    pub key: HelmReleaseKey,
    /// The latest revision.
    pub revision: u32,
    /// The latest revision's status.
    pub status: HelmStatus,
    /// The latest revision's age.
    pub age: Arc<str>,
    /// The latest revision's chart and app version.
    pub chart: HelmChartInfo,
    /// Every stored revision, newest first. Never empty.
    pub history: Arc<[HelmHistoryEntry]>,
}

/// Every Helm release of one context at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelmSnapshot {
    /// The context the rows belong to.
    pub context: ContextName,
    /// One row per release, sorted by namespace, name, then storage.
    pub rows: Arc<[HelmRow]>,
    /// Whether the first full list has arrived. Until then `rows` is empty.
    pub loaded: bool,
    /// Why the watch is failing, if it is. Rows stay on screen when it
    /// fails after loading.
    pub problem: Option<Arc<str>>,
}

/// What the UI asks of the engine about Helm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelmCommand {
    /// Watch the selected context's releases until [`HelmCommand::Close`].
    /// The watch follows context changes.
    Open,
    /// Stop watching releases.
    Close,
    /// Fetch and decode one revision. A newer request replaces one that has
    /// not finished.
    LoadDetails {
        /// The context the release is in.
        context: ContextName,
        /// The release.
        release: HelmReleaseKey,
        /// The revision to decode.
        revision: u32,
    },
}

/// What the engine tells the UI about Helm.
#[derive(Clone, Debug)]
pub enum HelmEvent {
    /// New rows or a new state for the selected context's releases.
    Releases(HelmSnapshot),
    /// The answer to [`HelmCommand::LoadDetails`].
    Details {
        /// The context the release is in.
        context: ContextName,
        /// The release.
        release: HelmReleaseKey,
        /// The revision that was decoded.
        revision: u32,
        /// The decoded revision, or why it could not be read.
        result: Result<HelmReleaseDetails, String>,
    },
}
