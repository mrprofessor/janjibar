//! The built-in kinds the app has typed views for, and the inspector's
//! details.
//!
//! Tables for every kind, typed or not, use [`crate::TableSnapshot`]. A
//! [`Kind`] adds what only typed specs know: actions, owner chains and the
//! inspector.

use std::fmt;
use std::sync::Arc;

use crate::{GroupVersionResource, Scope};

/// The built-in kinds the app has typed views for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Pod,
    Deployment,
    StatefulSet,
    DaemonSet,
    ReplicaSet,
    Job,
    CronJob,
    Service,
    Ingress,
    ConfigMap,
    Secret,
    Node,
    Namespace,
    Event,
    PersistentVolumeClaim,
    PersistentVolume,
    StorageClass,
}

impl Kind {
    /// Every kind.
    pub const ALL: [Self; 17] = [
        Self::Pod,
        Self::Deployment,
        Self::StatefulSet,
        Self::DaemonSet,
        Self::ReplicaSet,
        Self::Job,
        Self::CronJob,
        Self::Service,
        Self::Ingress,
        Self::ConfigMap,
        Self::Secret,
        Self::PersistentVolumeClaim,
        Self::PersistentVolume,
        Self::StorageClass,
        Self::Node,
        Self::Namespace,
        Self::Event,
    ];

    /// The API kind name, like `StatefulSet`.
    pub fn singular(self) -> &'static str {
        match self {
            Self::Pod => "Pod",
            Self::Deployment => "Deployment",
            Self::StatefulSet => "StatefulSet",
            Self::DaemonSet => "DaemonSet",
            Self::ReplicaSet => "ReplicaSet",
            Self::Job => "Job",
            Self::CronJob => "CronJob",
            Self::Service => "Service",
            Self::Ingress => "Ingress",
            Self::ConfigMap => "ConfigMap",
            Self::Secret => "Secret",
            Self::Node => "Node",
            Self::Namespace => "Namespace",
            Self::Event => "Event",
            Self::PersistentVolumeClaim => "PersistentVolumeClaim",
            Self::PersistentVolume => "PersistentVolume",
            Self::StorageClass => "StorageClass",
        }
    }

    /// Label for lists and tabs, like `StatefulSets`.
    pub fn plural(self) -> &'static str {
        match self {
            Self::Pod => "Pods",
            Self::Deployment => "Deployments",
            Self::StatefulSet => "StatefulSets",
            Self::DaemonSet => "DaemonSets",
            Self::ReplicaSet => "ReplicaSets",
            Self::Job => "Jobs",
            Self::CronJob => "CronJobs",
            Self::Service => "Services",
            Self::Ingress => "Ingresses",
            Self::ConfigMap => "ConfigMaps",
            Self::Secret => "Secrets",
            Self::Node => "Nodes",
            Self::Namespace => "Namespaces",
            Self::Event => "Events",
            Self::PersistentVolumeClaim => "PVCs",
            Self::PersistentVolume => "PVs",
            Self::StorageClass => "Storage classes",
        }
    }

    /// The kind for an API kind name, if it is one of ours.
    pub fn from_singular(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.singular() == name)
    }

    /// Whether objects of this kind live in a namespace.
    pub fn namespaced(self) -> bool {
        self.scope() == Scope::Namespaced
    }

    /// Namespaced or cluster-wide.
    pub fn scope(self) -> Scope {
        match self {
            Self::Node | Self::Namespace | Self::PersistentVolume | Self::StorageClass => {
                Scope::Cluster
            }
            _ => Scope::Namespaced,
        }
    }

    /// Where the kind lives in the API, at the version the typed spec reads.
    pub fn resource(self) -> GroupVersionResource {
        let (group, resource) = self.group_and_plural();
        GroupVersionResource {
            group: group.into(),
            version: "v1".into(),
            resource: resource.into(),
        }
    }

    /// The typed kind for an API resource, if there is one. The version is
    /// ignored: a spec reads whatever version the watch is told to use.
    pub fn from_resource(resource: &GroupVersionResource) -> Option<Self> {
        Self::ALL.into_iter().find(|k| {
            let (group, plural) = k.group_and_plural();
            *resource.group == *group && *resource.resource == *plural
        })
    }

    fn group_and_plural(self) -> (&'static str, &'static str) {
        match self {
            Self::Pod => ("", "pods"),
            Self::Deployment => ("apps", "deployments"),
            Self::StatefulSet => ("apps", "statefulsets"),
            Self::DaemonSet => ("apps", "daemonsets"),
            Self::ReplicaSet => ("apps", "replicasets"),
            Self::Job => ("batch", "jobs"),
            Self::CronJob => ("batch", "cronjobs"),
            Self::Service => ("", "services"),
            Self::Ingress => ("networking.k8s.io", "ingresses"),
            Self::ConfigMap => ("", "configmaps"),
            Self::Secret => ("", "secrets"),
            Self::Node => ("", "nodes"),
            Self::Namespace => ("", "namespaces"),
            Self::Event => ("", "events"),
            Self::PersistentVolumeClaim => ("", "persistentvolumeclaims"),
            Self::PersistentVolume => ("", "persistentvolumes"),
            Self::StorageClass => ("storage.k8s.io", "storageclasses"),
        }
    }

    /// Actions the inspector offers for this kind, in display order.
    pub fn actions(self) -> &'static [Action] {
        use Action::{
            Cordon, Delete, Drain, Edit, Forward, Logs, Restart, Scale, Shell, Suspend, Trigger,
        };
        match self {
            Self::Pod => &[Logs, Shell, Forward, Edit, Delete],
            Self::Deployment | Self::StatefulSet => &[Logs, Scale, Restart, Edit, Delete],
            Self::DaemonSet => &[Logs, Restart, Edit, Delete],
            Self::ReplicaSet => &[Scale, Edit, Delete],
            Self::Job => &[Logs, Edit, Delete],
            Self::CronJob => &[Trigger, Suspend, Edit, Delete],
            Self::Service => &[Forward, Edit, Delete],
            Self::Node => &[Shell, Cordon, Drain, Edit],
            Self::Event => &[],
            Self::Ingress
            | Self::ConfigMap
            | Self::Secret
            | Self::Namespace
            | Self::PersistentVolumeClaim
            | Self::PersistentVolume
            | Self::StorageClass => &[Edit, Delete],
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.singular())
    }
}

/// An action on one object. Wiring lands with the operate milestone; for
/// now the inspector shows them and reports clicks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Logs,
    Shell,
    Forward,
    Edit,
    Scale,
    Restart,
    Trigger,
    Suspend,
    Cordon,
    Drain,
    Delete,
}

impl Action {
    /// Button label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Logs => "Logs",
            Self::Shell => "Shell",
            Self::Forward => "Forward",
            Self::Edit => "Edit",
            Self::Scale => "Scale",
            Self::Restart => "Restart",
            Self::Trigger => "Trigger",
            Self::Suspend => "Suspend",
            Self::Cordon => "Cordon",
            Self::Drain => "Drain",
            Self::Delete => "Delete",
        }
    }

    /// The one-key shortcut shown on the button, if any.
    pub fn key(self) -> Option<&'static str> {
        match self {
            Self::Logs => Some("L"),
            Self::Shell => Some("S"),
            Self::Forward => Some("F"),
            Self::Edit => Some("E"),
            Self::Restart => Some("R"),
            _ => None,
        }
    }

    /// Whether the action destroys or disrupts something.
    pub fn destructive(self) -> bool {
        matches!(self, Self::Delete | Self::Drain)
    }
}

/// Identifies one object of a known kind within a context.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectKey {
    /// Namespace, or `None` for cluster-scoped objects.
    pub namespace: Option<Arc<str>>,
    /// Name.
    pub name: Arc<str>,
}

impl ObjectKey {
    /// A key for a namespaced object.
    pub fn namespaced(namespace: impl Into<Arc<str>>, name: impl Into<Arc<str>>) -> Self {
        Self {
            namespace: Some(namespace.into()),
            name: name.into(),
        }
    }

    /// A key for a cluster-scoped object.
    pub fn cluster(name: impl Into<Arc<str>>) -> Self {
        Self {
            namespace: None,
            name: name.into(),
        }
    }
}

impl fmt::Display for ObjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.namespace {
            Some(ns) => write!(f, "{ns}/{}", self.name),
            None => f.write_str(&self.name),
        }
    }
}

/// How a status should be coloured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Health {
    /// Running, ready, bound, active.
    Good,
    /// Starting, pending, progressing, terminating.
    Pending,
    /// Failing: crash loops, image pull errors, failed jobs, not ready nodes.
    Bad,
    /// Finished successfully.
    Done,
    /// Not known, or not a state at all.
    Unknown,
}

/// A label and a value in the inspector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    /// Label.
    pub label: Arc<str>,
    /// Value.
    pub value: Arc<str>,
}

impl Field {
    /// A field.
    pub fn new(label: impl Into<Arc<str>>, value: impl Into<Arc<str>>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

/// A titled list of fields, like a node's conditions or a service's ports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    /// Title.
    pub title: Arc<str>,
    /// Entries.
    pub fields: Vec<Field>,
}

/// One container, from a pod or a pod template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerInfo {
    /// Name.
    pub name: Arc<str>,
    /// Image.
    pub image: Arc<str>,
    /// Whether it is an init container (or a sidecar).
    pub init: bool,
    /// State line, like `Running, ready` or `Exited 137, OOMKilled`.
    pub state: Arc<str>,
    /// Colour for the state.
    pub health: Health,
    /// Restarts.
    pub restarts: u32,
    /// Requests and limits, like `limits cpu 500m, memory 512Mi`.
    pub resources: Option<Arc<str>>,
}

/// An owner in the inspector's owner chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerLink {
    /// API kind name.
    pub kind: Arc<str>,
    /// Name.
    pub name: Arc<str>,
}

/// What the inspector shows for one object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Details {
    /// The object's kind.
    pub kind: Kind,
    /// The object.
    pub key: ObjectKey,
    /// Status text and colour.
    pub status: Arc<str>,
    /// Colour for `status`.
    pub health: Health,
    /// Short facts next to the status, like `14 restarts`.
    pub badges: Vec<Arc<str>>,
    /// Key facts, like node, IP and creation time.
    pub fields: Vec<Field>,
    /// Kind-specific lists.
    pub sections: Vec<Section>,
    /// Containers, for pods and pod templates.
    pub containers: Vec<ContainerInfo>,
    /// Labels, sorted by key.
    pub labels: Vec<(Arc<str>, Arc<str>)>,
    /// Direct owners from the object's metadata.
    pub owners: Vec<OwnerLink>,
}

/// The object the inspector follows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InspectTarget {
    /// The context.
    pub context: crate::ContextName,
    /// The kind.
    pub kind: Kind,
    /// The namespace of the table it was picked from (`None` for all), so
    /// the inspector shares the table's watch.
    pub namespace: Option<Arc<str>>,
    /// The object.
    pub key: ObjectKey,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips_its_names() {
        for kind in Kind::ALL {
            assert_eq!(Kind::from_singular(kind.singular()), Some(kind));
            assert_eq!(Kind::from_resource(&kind.resource()), Some(kind));
        }
        assert_eq!(Kind::from_singular("Widget"), None);
    }

    #[test]
    fn resources_follow_the_api() {
        let r = Kind::Deployment.resource();
        assert_eq!(r.api_version(), "apps/v1");
        assert_eq!(&*r.resource, "deployments");
        assert_eq!(Kind::Pod.resource().api_version(), "v1");
        let crd = GroupVersionResource {
            group: "stable.example.com".into(),
            version: "v1".into(),
            resource: "crontabs".into(),
        };
        assert_eq!(Kind::from_resource(&crd), None);
        // Any version of a typed kind maps to it.
        let beta = GroupVersionResource {
            version: "v1beta1".into(),
            ..Kind::CronJob.resource()
        };
        assert_eq!(Kind::from_resource(&beta), Some(Kind::CronJob));
    }

    #[test]
    fn cluster_scoped_kinds() {
        let cluster: Vec<Kind> = Kind::ALL.into_iter().filter(|k| !k.namespaced()).collect();
        assert_eq!(
            cluster,
            [
                Kind::PersistentVolume,
                Kind::StorageClass,
                Kind::Node,
                Kind::Namespace
            ]
        );
    }

    #[test]
    fn keys_sort_by_namespace_then_name() {
        let mut keys = [
            ObjectKey::namespaced("b", "a"),
            ObjectKey::namespaced("a", "z"),
            ObjectKey::cluster("n"),
            ObjectKey::namespaced("a", "b"),
        ];
        keys.sort();
        let shown: Vec<String> = keys.iter().map(ToString::to_string).collect();
        assert_eq!(shown, ["n", "a/b", "a/z", "b/a"]);
    }
}
