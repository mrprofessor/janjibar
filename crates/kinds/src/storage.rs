//! PersistentVolumeClaims, PersistentVolumes and StorageClasses.

use std::sync::Arc;

use k8s_openapi::api::core::v1::{PersistentVolume, PersistentVolumeClaim};
use k8s_openapi::api::storage::v1::StorageClass;
use k8s_openapi::jiff::Timestamp;
use model::Health;
use model::resource::{Details, Field, Kind};
use model::{Column, ColumnType, TableRow};

use crate::Spec;
use crate::common::{self, DASH, col, join, or_dash};

fn phase_health(phase: &str) -> Health {
    match phase {
        "Bound" | "Available" => Health::Good,
        "Pending" | "Released" => Health::Pending,
        "Failed" | "Lost" => Health::Bad,
        _ => Health::Unknown,
    }
}

fn access_modes(modes: Option<&Vec<String>>) -> Arc<str> {
    join(modes.into_iter().flatten().map(|m| match m.as_str() {
        "ReadWriteOnce" => "RWO",
        "ReadOnlyMany" => "ROX",
        "ReadWriteMany" => "RWX",
        "ReadWriteOncePod" => "RWOP",
        other => other,
    }))
}

/// The PersistentVolumeClaims spec.
pub struct PersistentVolumeClaims;

fn pvc_capacity(c: &PersistentVolumeClaim) -> Arc<str> {
    c.status
        .as_ref()
        .and_then(|s| s.capacity.as_ref())
        .and_then(|m| m.get("storage"))
        .or_else(|| {
            c.spec
                .as_ref()
                .and_then(|s| s.resources.as_ref())
                .and_then(|r| r.requests.as_ref())
                .and_then(|m| m.get("storage"))
        })
        .map_or_else(|| DASH.into(), |q| q.0.as_str().into())
}

impl Spec for PersistentVolumeClaims {
    type Object = PersistentVolumeClaim;
    const KIND: Kind = Kind::PersistentVolumeClaim;

    fn columns() -> Vec<Column> {
        vec![
            col("Name", ColumnType::String, "name"),
            col("Status", ColumnType::String, "status"),
            col("Volume", ColumnType::String, "muted"),
            col("Capacity", ColumnType::String, ""),
            col("Access", ColumnType::String, ""),
            col("Storage class", ColumnType::String, ""),
            col("Age", ColumnType::Date, ""),
        ]
    }

    fn row(c: &PersistentVolumeClaim, now: Timestamp) -> TableRow {
        let phase = c
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or("Pending");
        let spec = c.spec.as_ref();
        common::row(
            &c.metadata,
            vec![
                common::name(&c.metadata),
                phase.into(),
                or_dash(spec.and_then(|s| s.volume_name.as_deref())),
                pvc_capacity(c),
                access_modes(spec.and_then(|s| s.access_modes.as_ref())),
                or_dash(spec.and_then(|s| s.storage_class_name.as_deref())),
                common::age(&c.metadata, now),
            ],
            phase_health(phase),
        )
    }

    fn details(c: &PersistentVolumeClaim, now: Timestamp) -> Details {
        let row = Self::row(c, now);
        let mut out = common::base_details(Kind::PersistentVolumeClaim, &c.metadata, now);
        out.status = Arc::clone(&row.cells[2]);
        out.health = row.health;
        let spec = c.spec.as_ref();
        let mut fields = vec![
            Field::new("Volume", Arc::clone(&row.cells[3])),
            Field::new("Capacity", Arc::clone(&row.cells[4])),
            Field::new("Access modes", Arc::clone(&row.cells[5])),
            Field::new("Storage class", Arc::clone(&row.cells[6])),
            Field::new(
                "Volume mode",
                or_dash(spec.and_then(|s| s.volume_mode.as_deref())),
            ),
        ];
        fields.append(&mut out.fields);
        out.fields = fields;
        out
    }
}

/// The PersistentVolumes spec.
pub struct PersistentVolumes;

fn pv_claim(v: &PersistentVolume) -> Arc<str> {
    v.spec
        .as_ref()
        .and_then(|s| s.claim_ref.as_ref())
        .map_or_else(
            || DASH.into(),
            |c| {
                format!(
                    "{}/{}",
                    c.namespace.as_deref().unwrap_or("?"),
                    c.name.as_deref().unwrap_or("?")
                )
                .into()
            },
        )
}

impl Spec for PersistentVolumes {
    type Object = PersistentVolume;
    const KIND: Kind = Kind::PersistentVolume;

    fn columns() -> Vec<Column> {
        vec![
            col("Name", ColumnType::String, "name"),
            col("Capacity", ColumnType::String, ""),
            col("Access", ColumnType::String, ""),
            col("Reclaim", ColumnType::String, ""),
            col("Status", ColumnType::String, "status"),
            col("Claim", ColumnType::String, "muted"),
            col("Storage class", ColumnType::String, ""),
            col("Age", ColumnType::Date, ""),
        ]
    }

    fn row(v: &PersistentVolume, now: Timestamp) -> TableRow {
        let spec = v.spec.as_ref();
        let phase = v
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or("Pending");
        common::row(
            &v.metadata,
            vec![
                common::name(&v.metadata),
                spec.and_then(|s| s.capacity.as_ref())
                    .and_then(|m| m.get("storage"))
                    .map_or_else(|| DASH.into(), |q| q.0.as_str().into()),
                access_modes(spec.and_then(|s| s.access_modes.as_ref())),
                or_dash(spec.and_then(|s| s.persistent_volume_reclaim_policy.as_deref())),
                phase.into(),
                pv_claim(v),
                or_dash(spec.and_then(|s| s.storage_class_name.as_deref())),
                common::age(&v.metadata, now),
            ],
            phase_health(phase),
        )
    }

    fn details(v: &PersistentVolume, now: Timestamp) -> Details {
        let row = Self::row(v, now);
        let mut out = common::base_details(Kind::PersistentVolume, &v.metadata, now);
        out.status = Arc::clone(&row.cells[4]);
        out.health = row.health;
        let mut fields = vec![
            Field::new("Capacity", Arc::clone(&row.cells[1])),
            Field::new("Access modes", Arc::clone(&row.cells[2])),
            Field::new("Reclaim policy", Arc::clone(&row.cells[3])),
            Field::new("Claim", Arc::clone(&row.cells[5])),
            Field::new("Storage class", Arc::clone(&row.cells[6])),
        ];
        fields.append(&mut out.fields);
        out.fields = fields;
        out
    }
}

/// The StorageClasses spec.
pub struct StorageClasses;

fn is_default(s: &StorageClass) -> bool {
    s.metadata.annotations.as_ref().is_some_and(|a| {
        a.get("storageclass.kubernetes.io/is-default-class")
            .is_some_and(|v| v == "true")
    })
}

impl Spec for StorageClasses {
    type Object = StorageClass;
    const KIND: Kind = Kind::StorageClass;

    fn columns() -> Vec<Column> {
        vec![
            col("Name", ColumnType::String, "name"),
            col("Provisioner", ColumnType::String, ""),
            col("Reclaim", ColumnType::String, ""),
            col("Binding", ColumnType::String, "muted"),
            col("Expansion", ColumnType::String, ""),
            col("Age", ColumnType::Date, ""),
        ]
    }

    fn row(s: &StorageClass, now: Timestamp) -> TableRow {
        let mut name = common::name(&s.metadata);
        if is_default(s) {
            name = format!("{name} (default)").into();
        }
        common::row(
            &s.metadata,
            vec![
                name,
                s.provisioner.as_str().into(),
                or_dash(s.reclaim_policy.as_deref().or(Some("Delete"))),
                or_dash(s.volume_binding_mode.as_deref().or(Some("Immediate"))),
                if s.allow_volume_expansion == Some(true) {
                    "Yes"
                } else {
                    "No"
                }
                .into(),
                common::age(&s.metadata, now),
            ],
            Health::Unknown,
        )
    }

    fn details(s: &StorageClass, now: Timestamp) -> Details {
        let row = Self::row(s, now);
        let mut out = common::base_details(Kind::StorageClass, &s.metadata, now);
        out.status = Arc::clone(&row.cells[1]);
        if is_default(s) {
            out.badges.push("default".into());
        }
        let mut fields = vec![
            Field::new("Provisioner", Arc::clone(&row.cells[1])),
            Field::new("Reclaim policy", Arc::clone(&row.cells[2])),
            Field::new("Binding mode", Arc::clone(&row.cells[3])),
            Field::new("Expansion", Arc::clone(&row.cells[4])),
            Field::new("Parameters", common::pairs(s.parameters.as_ref())),
        ];
        fields.append(&mut out.fields);
        out.fields = fields;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{PersistentVolumeClaimSpec, PersistentVolumeClaimStatus};
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    use std::collections::BTreeMap;

    #[test]
    fn pvc_row() {
        let pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some("data-postgres-0".into()),
                namespace: Some("loaner".into()),
                ..ObjectMeta::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(vec!["ReadWriteOnce".into()]),
                storage_class_name: Some("standard".into()),
                volume_name: Some("pvc-123".into()),
                ..PersistentVolumeClaimSpec::default()
            }),
            status: Some(PersistentVolumeClaimStatus {
                phase: Some("Bound".into()),
                capacity: Some(BTreeMap::from([("storage".into(), Quantity("1Gi".into()))])),
                ..PersistentVolumeClaimStatus::default()
            }),
        };
        let row = PersistentVolumeClaims::row(&pvc, Timestamp::UNIX_EPOCH);
        let cells: Vec<&str> = row.cells.iter().map(|c| &**c).collect();
        assert_eq!(
            cells,
            [
                "data-postgres-0",
                "Bound",
                "pvc-123",
                "1Gi",
                "RWO",
                "standard",
                "-"
            ]
        );
        assert_eq!(row.health, Health::Good);
    }

    #[test]
    fn default_storage_class_is_marked() {
        let sc = StorageClass {
            metadata: ObjectMeta {
                name: Some("standard".into()),
                annotations: Some(BTreeMap::from([(
                    "storageclass.kubernetes.io/is-default-class".into(),
                    "true".into(),
                )])),
                ..ObjectMeta::default()
            },
            provisioner: "rancher.io/local-path".into(),
            ..StorageClass::default()
        };
        let row = StorageClasses::row(&sc, Timestamp::UNIX_EPOCH);
        assert_eq!(&*row.cells[0], "standard (default)");
        assert_eq!(&*row.name, "standard");
    }
}
