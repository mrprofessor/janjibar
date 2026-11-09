//! Release records: object names, labels, and grouping into releases.
//!
//! Helm writes one object per revision, named
//! `sh.helm.release.v1.<name>.v<revision>` and labelled `owner=helm`,
//! `name`, `status` and `version`. Newer Helm also sets `createdAt` and
//! `modifiedAt` (Unix seconds). Everything here works on that metadata
//! alone, so no payload is read.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use model::{HelmRelease, HelmRevision, HelmStatus, HelmStorage};

/// The label selector that finds Helm's records, the same one Helm uses.
pub const OWNER_SELECTOR: &str = "owner=helm";

/// The prefix of every record's object name. The `v1` is the storage
/// format, which Helm 3 and Helm 4 share.
const NAME_PREFIX: &str = "sh.helm.release.v1.";

/// The object name Helm stores `revision` of `release` under.
pub fn object_name(release: &str, revision: u32) -> String {
    format!("{NAME_PREFIX}{release}.v{revision}")
}

/// Splits a record's object name into release name and revision.
///
/// Release names may contain dots, so the revision is whatever follows the
/// last `.v`. Returns `None` for names Helm would not write.
pub fn parse_object_name(object: &str) -> Option<(&str, u32)> {
    let rest = object.strip_prefix(NAME_PREFIX)?;
    let (name, revision) = rest.rsplit_once(".v")?;
    if name.is_empty() || revision.is_empty() || !revision.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((name, revision.parse().ok()?))
}

/// Reads one revision from a record's metadata.
///
/// Returns `None` when the object is not a Helm record: wrong name, or no
/// `owner=helm` label. A missing or unknown `status` label gives
/// [`HelmStatus::Unknown`].
pub fn revision_from_meta(meta: &ObjectMeta, storage: HelmStorage) -> Option<HelmRevision> {
    let (name, revision) = parse_object_name(meta.name.as_deref()?)?;
    let label = |key: &str| {
        meta.labels
            .as_ref()
            .and_then(|labels| labels.get(key))
            .map(String::as_str)
    };
    if label("owner") != Some("helm") {
        return None;
    }
    let updated = label("modifiedAt")
        .or_else(|| label("createdAt"))
        .and_then(unix_seconds)
        .or_else(|| {
            meta.creation_timestamp
                .as_ref()
                .map(|t| SystemTime::from(t.0))
        });
    Some(HelmRevision {
        namespace: meta.namespace.as_deref().unwrap_or_default().into(),
        name: name.into(),
        revision,
        status: label("status").map_or(HelmStatus::Unknown, HelmStatus::parse),
        updated,
        storage,
    })
}

fn unix_seconds(text: &str) -> Option<SystemTime> {
    let seconds = text.parse::<u64>().ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// Groups revisions into releases, one per namespace, name and storage.
///
/// Each release carries its highest revision, whatever its status, the way
/// Helm picks the current one. The list is sorted by namespace, then name.
pub fn releases(revisions: &[HelmRevision]) -> Vec<HelmRelease> {
    let mut by_key: BTreeMap<(Arc<str>, Arc<str>, HelmStorage), HelmRelease> = BTreeMap::new();
    for revision in revisions {
        let key = (
            Arc::clone(&revision.namespace),
            Arc::clone(&revision.name),
            revision.storage,
        );
        by_key
            .entry(key)
            .and_modify(|release| {
                release.stored_revisions += 1;
                if revision.revision > release.latest.revision {
                    release.latest = revision.clone();
                }
            })
            .or_insert_with(|| HelmRelease {
                latest: revision.clone(),
                stored_revisions: 1,
            });
    }
    by_key.into_values().collect()
}

/// Sorts revisions newest first.
pub fn sort_newest_first(revisions: &mut [HelmRevision]) {
    revisions.sort_unstable_by_key(|r| std::cmp::Reverse(r.revision));
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use k8s_openapi::jiff::Timestamp;

    use super::*;

    fn meta(namespace: &str, object: &str, labels: &[(&str, &str)]) -> ObjectMeta {
        ObjectMeta {
            name: Some(object.to_owned()),
            namespace: Some(namespace.to_owned()),
            labels: Some(
                labels
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect::<BTreeMap<_, _>>(),
            ),
            ..ObjectMeta::default()
        }
    }

    fn helm_labels<'a>(
        name: &'a str,
        version: &'a str,
        status: &'a str,
    ) -> Vec<(&'a str, &'a str)> {
        vec![
            ("owner", "helm"),
            ("name", name),
            ("version", version),
            ("status", status),
        ]
    }

    fn revision(namespace: &str, name: &str, revision: u32, status: HelmStatus) -> HelmRevision {
        HelmRevision {
            namespace: namespace.into(),
            name: name.into(),
            revision,
            status,
            updated: None,
            storage: HelmStorage::Secret,
        }
    }

    #[test]
    fn object_names_round_trip() {
        assert_eq!(object_name("web", 3), "sh.helm.release.v1.web.v3");
        assert_eq!(
            parse_object_name("sh.helm.release.v1.web.v3"),
            Some(("web", 3))
        );
    }

    #[test]
    fn release_names_may_contain_dots_and_v() {
        assert_eq!(
            parse_object_name("sh.helm.release.v1.api.v2.v14"),
            Some(("api.v2", 14))
        );
    }

    #[test]
    fn foreign_names_are_rejected() {
        for name in [
            "my-secret",
            "sh.helm.release.v1.web",
            "sh.helm.release.v1.web.v",
            "sh.helm.release.v1.web.vX",
            "sh.helm.release.v1.web.v+3",
            "sh.helm.release.v1..v3",
            "sh.helm.release.v2.web.v3",
            "sh.helm.release.v1.web.v99999999999",
        ] {
            assert_eq!(parse_object_name(name), None, "{name}");
        }
    }

    #[test]
    fn reads_a_revision_from_labels() {
        let meta = meta(
            "shop",
            "sh.helm.release.v1.web.v7",
            &[
                ("owner", "helm"),
                ("name", "web"),
                ("version", "7"),
                ("status", "pending-upgrade"),
                ("createdAt", "1700000000"),
                ("modifiedAt", "1700000300"),
            ],
        );
        let rev = revision_from_meta(&meta, HelmStorage::Secret).unwrap();
        assert_eq!(&*rev.namespace, "shop");
        assert_eq!(&*rev.name, "web");
        assert_eq!(rev.revision, 7);
        assert_eq!(rev.status, HelmStatus::PendingUpgrade);
        assert!(rev.status.is_pending());
        assert_eq!(
            rev.updated,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_300))
        );
        assert_eq!(rev.storage, HelmStorage::Secret);
    }

    #[test]
    fn reads_the_labels_of_real_shaped_records() {
        let secret: k8s_openapi::api::core::v1::Secret =
            serde_json::from_str(include_str!("../tests/fixtures/helm3-secret.json")).unwrap();
        let rev = revision_from_meta(&secret.metadata, HelmStorage::Secret).unwrap();
        assert_eq!(
            (&*rev.namespace, &*rev.name, rev.revision, rev.status),
            ("payments", "ledger", 3, HelmStatus::Deployed)
        );
        assert_eq!(
            rev.updated,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_739_910))
        );

        let config_map: k8s_openapi::api::core::v1::ConfigMap =
            serde_json::from_str(include_str!("../tests/fixtures/helm4-configmap.json")).unwrap();
        let rev = revision_from_meta(&config_map.metadata, HelmStorage::ConfigMap).unwrap();
        assert_eq!(
            (&*rev.namespace, &*rev.name, rev.revision, rev.status),
            ("ingress", "edge", 2, HelmStatus::Failed)
        );
    }

    #[test]
    fn older_records_fall_back_to_creation_time() {
        let mut meta = meta(
            "shop",
            "sh.helm.release.v1.web.v1",
            &helm_labels("web", "1", "deployed"),
        );
        let created: Timestamp = "2024-05-01T10:00:00Z".parse().unwrap();
        meta.creation_timestamp = Some(Time(created));
        let rev = revision_from_meta(&meta, HelmStorage::ConfigMap).unwrap();
        assert_eq!(rev.updated, Some(SystemTime::from(created)));
        assert_eq!(rev.storage, HelmStorage::ConfigMap);
    }

    #[test]
    fn unknown_or_missing_status_is_unknown() {
        let odd = meta(
            "a",
            "sh.helm.release.v1.x.v1",
            &helm_labels("x", "1", "exploded"),
        );
        let none = meta("a", "sh.helm.release.v1.x.v1", &[("owner", "helm")]);
        for m in [odd, none] {
            let rev = revision_from_meta(&m, HelmStorage::Secret).unwrap();
            assert_eq!(rev.status, HelmStatus::Unknown);
        }
    }

    #[test]
    fn objects_without_the_owner_label_are_skipped() {
        let meta = meta("a", "sh.helm.release.v1.x.v1", &[("name", "x")]);
        assert_eq!(revision_from_meta(&meta, HelmStorage::Secret), None);
    }

    #[test]
    fn statuses_round_trip_through_helm_words() {
        for status in [
            HelmStatus::Unknown,
            HelmStatus::Deployed,
            HelmStatus::Uninstalled,
            HelmStatus::Superseded,
            HelmStatus::Failed,
            HelmStatus::Uninstalling,
            HelmStatus::PendingInstall,
            HelmStatus::PendingUpgrade,
            HelmStatus::PendingRollback,
        ] {
            assert_eq!(HelmStatus::parse(status.as_str()), status);
        }
    }

    #[test]
    fn releases_keep_the_highest_revision() {
        let revisions = [
            revision("shop", "web", 9, HelmStatus::Superseded),
            revision("shop", "web", 11, HelmStatus::Failed),
            revision("shop", "web", 10, HelmStatus::Superseded),
            revision("data", "pg", 1, HelmStatus::Deployed),
            revision("auth", "web", 2, HelmStatus::Deployed),
        ];
        let list = releases(&revisions);
        let rows: Vec<_> = list
            .iter()
            .map(|r| {
                (
                    &*r.latest.namespace,
                    &*r.latest.name,
                    r.latest.revision,
                    r.latest.status,
                    r.stored_revisions,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("auth", "web", 2, HelmStatus::Deployed, 1),
                ("data", "pg", 1, HelmStatus::Deployed, 1),
                ("shop", "web", 11, HelmStatus::Failed, 3),
            ]
        );
    }

    #[test]
    fn the_same_name_in_two_storages_is_two_releases() {
        let mut in_config_map = revision("shop", "web", 1, HelmStatus::Deployed);
        in_config_map.storage = HelmStorage::ConfigMap;
        let list = releases(&[
            revision("shop", "web", 4, HelmStatus::Deployed),
            in_config_map,
        ]);
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn history_sorts_newest_first() {
        let mut revisions = vec![
            revision("a", "x", 2, HelmStatus::Superseded),
            revision("a", "x", 10, HelmStatus::Deployed),
            revision("a", "x", 9, HelmStatus::Superseded),
        ];
        sort_newest_first(&mut revisions);
        let numbers: Vec<u32> = revisions.iter().map(|r| r.revision).collect();
        assert_eq!(numbers, [10, 9, 2]);
    }
}
