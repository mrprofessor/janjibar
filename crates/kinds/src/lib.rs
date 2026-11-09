//! Columns, rows and inspector details for each built-in kind.
//!
//! Every kind has a [`Spec`]: the columns its table shows, how one object
//! becomes a [`TableRow`], and what the inspector shows for it. Rows use the
//! same types as the server-side tables, so the UI draws typed and untyped
//! kinds the same way. Like server-side tables, typed rows carry no
//! namespace column: the UI adds one from [`TableRow::namespace`]. Specs are plain
//! functions over typed `k8s-openapi` objects, so they are cheap to test and
//! have no I/O. The engine runs them on tokio; the UI only sees the results.
//!
//! This crate never depends on GPUI or on a Kubernetes client.

pub mod age;
mod cluster;
mod common;
mod config;
mod network;
pub mod pod;
mod storage;
mod workloads;

use k8s_openapi::jiff::Timestamp;
use model::resource::{Details, Kind};
use model::{Column, TableRow};

pub use cluster::{Events, Namespaces, Nodes, event_subject};
pub use config::{ConfigMaps, Secrets};
pub use network::{Ingresses, Services};
pub use pod::Pods;
pub use storage::{PersistentVolumeClaims, PersistentVolumes, StorageClasses};
pub use workloads::{CronJobs, DaemonSets, Deployments, Jobs, ReplicaSets, StatefulSets};

/// How one kind is shown.
pub trait Spec: 'static {
    /// What the watch stores: the typed object, or `PartialObjectMeta` for
    /// kinds watched by metadata only.
    type Object: Clone + Send + Sync + 'static;

    /// The kind.
    const KIND: Kind;

    /// Watch metadata only, so object bodies (like secret values) never
    /// reach memory.
    const METADATA_ONLY: bool = false;

    /// The table's columns.
    fn columns() -> Vec<Column>;

    /// One table row. `now` is used for ages.
    fn row(obj: &Self::Object, now: Timestamp) -> TableRow;

    /// What the inspector shows.
    fn details(obj: &Self::Object, now: Timestamp) -> Details;

    /// A timestamp (Unix seconds) to sort newest first, for kinds where
    /// recency matters more than names. `None` sorts by namespace and name.
    fn recency(_obj: &Self::Object) -> Option<i64> {
        None
    }
}

/// Projects and sorts rows for a spec: newest first for kinds with a
/// [`Spec::recency`], otherwise by namespace and name.
pub fn rows<'a, S: Spec>(
    objects: impl IntoIterator<Item = &'a S::Object>,
    now: Timestamp,
) -> Vec<TableRow> {
    let mut keyed: Vec<(Option<i64>, TableRow)> = objects
        .into_iter()
        .map(|obj| (S::recency(obj), S::row(obj, now)))
        .collect();
    keyed.sort_unstable_by(|(ra, a), (rb, b)| {
        rb.cmp(ra)
            .then_with(|| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)))
    });
    keyed.into_iter().map(|(_, row)| row).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn every_kind_leads_with_its_name_and_has_no_namespace_column() {
        let all: Vec<(Kind, Vec<Column>)> = vec![
            (Pods::KIND, Pods::columns()),
            (Deployments::KIND, Deployments::columns()),
            (StatefulSets::KIND, StatefulSets::columns()),
            (DaemonSets::KIND, DaemonSets::columns()),
            (ReplicaSets::KIND, ReplicaSets::columns()),
            (Jobs::KIND, Jobs::columns()),
            (CronJobs::KIND, CronJobs::columns()),
            (Services::KIND, Services::columns()),
            (Ingresses::KIND, Ingresses::columns()),
            (ConfigMaps::KIND, ConfigMaps::columns()),
            (Secrets::KIND, Secrets::columns()),
            (Nodes::KIND, Nodes::columns()),
            (Namespaces::KIND, Namespaces::columns()),
            (
                PersistentVolumeClaims::KIND,
                PersistentVolumeClaims::columns(),
            ),
            (PersistentVolumes::KIND, PersistentVolumes::columns()),
            (StorageClasses::KIND, StorageClasses::columns()),
        ];
        for (kind, columns) in &all {
            assert_eq!(
                &*columns[0].format, "name",
                "{kind:?} should lead with its name"
            );
            assert!(
                columns.iter().all(|c| &*c.name != "Namespace"),
                "{kind:?}: the UI adds the namespace column"
            );
            assert!(columns.iter().all(|c| c.priority == 0));
        }
        // Every kind except Event is covered above; Event leads with time.
        let mut kinds: Vec<Kind> = all.iter().map(|(k, _)| *k).collect();
        kinds.push(Events::KIND);
        kinds.sort();
        let mut expected = Kind::ALL.to_vec();
        expected.sort();
        assert_eq!(kinds, expected);
    }

    #[test]
    fn rows_sort_by_recency_then_key() {
        struct Fake;
        impl Spec for Fake {
            type Object = (i64, &'static str);
            const KIND: Kind = Kind::Event;
            fn columns() -> Vec<Column> {
                Vec::new()
            }
            fn row(obj: &Self::Object, _: Timestamp) -> TableRow {
                TableRow {
                    name: obj.1.into(),
                    namespace: None,
                    uid: "".into(),
                    created: None,
                    cells: Arc::from([]),
                    details: Arc::from([]),
                    health: model::Health::Unknown,
                }
            }
            fn details(_: &Self::Object, _: Timestamp) -> Details {
                unreachable!("not used")
            }
            fn recency(obj: &Self::Object) -> Option<i64> {
                Some(obj.0)
            }
        }
        let objs = [(1, "old"), (5, "new-b"), (5, "new-a"), (3, "mid")];
        let names: Vec<String> = rows::<Fake>(objs.iter(), Timestamp::UNIX_EPOCH)
            .iter()
            .map(|r| r.name.to_string())
            .collect();
        assert_eq!(names, ["new-a", "new-b", "mid", "old"]);
    }
}
