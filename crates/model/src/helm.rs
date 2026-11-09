//! Helm releases, read from the cluster without the helm binary.
//!
//! Helm stores each revision of a release as one Secret (the default) or
//! `ConfigMap`. A [`HelmRevision`] comes from that object's name and labels
//! only, so listing is cheap. [`HelmReleaseDetails`] needs the payload.

use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

/// Which kind of object holds a release's revisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HelmStorage {
    /// Secrets of type `helm.sh/release.v1`. Helm's default driver.
    Secret,
    /// `ConfigMaps`, from `HELM_DRIVER=configmap`.
    ConfigMap,
}

/// A revision's state, as Helm records it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum HelmStatus {
    /// Helm's own `unknown`, or a value this app does not know.
    #[default]
    Unknown,
    /// The live revision.
    Deployed,
    /// Uninstalled, kept with `--keep-history`.
    Uninstalled,
    /// Replaced by a newer revision.
    Superseded,
    /// The install, upgrade or rollback failed.
    Failed,
    /// An uninstall is running.
    Uninstalling,
    /// An install is running.
    PendingInstall,
    /// An upgrade is running.
    PendingUpgrade,
    /// A rollback is running.
    PendingRollback,
}

impl HelmStatus {
    /// Parses Helm's status word, such as `pending-upgrade`.
    pub fn parse(word: &str) -> Self {
        match word {
            "deployed" => Self::Deployed,
            "uninstalled" => Self::Uninstalled,
            "superseded" => Self::Superseded,
            "failed" => Self::Failed,
            "uninstalling" => Self::Uninstalling,
            "pending-install" => Self::PendingInstall,
            "pending-upgrade" => Self::PendingUpgrade,
            "pending-rollback" => Self::PendingRollback,
            _ => Self::Unknown,
        }
    }

    /// Helm's word for the status, as `helm list` prints it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Deployed => "deployed",
            Self::Uninstalled => "uninstalled",
            Self::Superseded => "superseded",
            Self::Failed => "failed",
            Self::Uninstalling => "uninstalling",
            Self::PendingInstall => "pending-install",
            Self::PendingUpgrade => "pending-upgrade",
            Self::PendingRollback => "pending-rollback",
        }
    }

    /// True while an operation runs. Helm refuses another one until it ends.
    pub fn is_pending(self) -> bool {
        matches!(
            self,
            Self::Uninstalling
                | Self::PendingInstall
                | Self::PendingUpgrade
                | Self::PendingRollback
        )
    }
}

impl fmt::Display for HelmStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One stored revision of a release, read from its object's name and labels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelmRevision {
    /// The release's namespace.
    pub namespace: Arc<str>,
    /// The release name.
    pub name: Arc<str>,
    /// The revision number, starting at 1.
    pub revision: u32,
    /// The status label.
    pub status: HelmStatus,
    /// When Helm last wrote the record: its `modifiedAt` or `createdAt`
    /// label, else the object's creation time.
    pub updated: Option<SystemTime>,
    /// The kind of object that holds it.
    pub storage: HelmStorage,
}

/// A release in the list: its newest revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelmRelease {
    /// The newest stored revision.
    pub latest: HelmRevision,
    /// How many revisions are stored (Helm keeps 10 by default).
    pub stored_revisions: u32,
}

/// The chart a revision was installed from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HelmChart {
    /// Chart name, such as `ingress-nginx`.
    pub name: Arc<str>,
    /// Chart version, such as `4.11.3`.
    pub version: Arc<str>,
    /// The app version the chart declares. Empty when it declares none.
    pub app_version: Arc<str>,
}

/// `name-version`, like the CHART column of `helm list`.
impl fmt::Display for HelmChart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.name, self.version)
    }
}

/// One revision, decoded from its payload.
///
/// `Debug` redacts `values`, `manifest` and `notes`: they are rendered from
/// user values and often hold passwords and tokens.
#[derive(Clone, PartialEq, Eq)]
pub struct HelmReleaseDetails {
    /// The release's namespace.
    pub namespace: Arc<str>,
    /// The release name.
    pub name: Arc<str>,
    /// The revision number.
    pub revision: u32,
    /// The status Helm recorded in the payload.
    pub status: HelmStatus,
    /// The chart it was installed from.
    pub chart: HelmChart,
    /// When the first revision was deployed.
    pub first_deployed: Option<SystemTime>,
    /// When this revision was deployed, shown as UPDATED by `helm list`.
    pub updated: Option<SystemTime>,
    /// Helm's log line, such as `Upgrade complete`.
    pub description: Arc<str>,
    /// Rendered `NOTES.txt`. Empty when the chart has none.
    pub notes: Arc<str>,
    /// User-supplied values as YAML, like `helm get values`. Empty when the
    /// user set none.
    pub values: Arc<str>,
    /// The rendered manifest, without hooks.
    pub manifest: Arc<str>,
}

impl fmt::Debug for HelmReleaseDetails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HelmReleaseDetails")
            .field("namespace", &self.namespace)
            .field("name", &self.name)
            .field("revision", &self.revision)
            .field("status", &self.status)
            .field("chart", &self.chart)
            .field("first_deployed", &self.first_deployed)
            .field("updated", &self.updated)
            .field("description", &self.description)
            .field("notes", &Redacted(&self.notes))
            .field("values", &Redacted(&self.values))
            .field("manifest", &Redacted(&self.manifest))
            .finish()
    }
}

/// Shows only the size of text that may hold secrets.
struct Redacted<'a>(&'a str);

impl fmt::Debug for Redacted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted, {} bytes>", self.0.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_values_manifest_and_notes() {
        let details = HelmReleaseDetails {
            namespace: "shop".into(),
            name: "web".into(),
            revision: 2,
            status: HelmStatus::Deployed,
            chart: HelmChart::default(),
            first_deployed: None,
            updated: None,
            description: "Upgrade complete".into(),
            notes: "admin password: hunter2".into(),
            values: "dbPassword: hunter2\n".into(),
            manifest: "kind: Secret\nstringData:\n  token: hunter2\n".into(),
        };
        let shown = format!("{details:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("values: <redacted, 20 bytes>"), "{shown}");
        assert!(shown.contains("name: \"web\""), "{shown}");
        assert!(shown.contains("Upgrade complete"), "{shown}");
    }
}
