//! ConfigMaps and Secrets.
//!
//! Secrets are watched by metadata only. Their values never reach memory,
//! so the table and inspector show names, labels and annotations, not data.

use std::collections::BTreeMap;
use std::sync::Arc;

use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use k8s_openapi::jiff::Timestamp;
use kube_core::PartialObjectMeta;
use model::Health;
use model::resource::{Details, Field, Kind, Section};
use model::{Column, ColumnType, TableRow};

use crate::Spec;
use crate::common::{self, col, num};

/// The ConfigMaps spec.
pub struct ConfigMaps;

fn key_count(c: &ConfigMap) -> usize {
    c.data.as_ref().map_or(0, BTreeMap::len) + c.binary_data.as_ref().map_or(0, BTreeMap::len)
}

impl Spec for ConfigMaps {
    type Object = ConfigMap;
    const KIND: Kind = Kind::ConfigMap;

    fn columns() -> Vec<Column> {
        vec![
            col("Name", ColumnType::String, "name"),
            col("Keys", ColumnType::Integer, ""),
            col("Age", ColumnType::Date, ""),
        ]
    }

    fn row(c: &ConfigMap, now: Timestamp) -> TableRow {
        common::row(
            &c.metadata,
            vec![
                common::name(&c.metadata),
                num(key_count(c)),
                common::age(&c.metadata, now),
            ],
            Health::Unknown,
        )
    }

    fn details(c: &ConfigMap, now: Timestamp) -> Details {
        let mut out = common::base_details(Kind::ConfigMap, &c.metadata, now);
        let n = key_count(c);
        out.status = format!("{n} key{}", if n == 1 { "" } else { "s" }).into();
        if c.immutable == Some(true) {
            out.badges.push("immutable".into());
        }
        let mut keys: Vec<Field> = c
            .data
            .iter()
            .flatten()
            .map(|(k, v)| Field::new(k.as_str(), size(v.len())))
            .collect();
        keys.extend(
            c.binary_data
                .iter()
                .flatten()
                .map(|(k, v)| Field::new(k.as_str(), format!("{} binary", size(v.0.len())))),
        );
        if !keys.is_empty() {
            out.sections.push(Section {
                title: "Keys".into(),
                fields: keys,
            });
        }
        out
    }
}

fn size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    }
}

/// The Secrets spec, metadata only.
pub struct Secrets;

impl Spec for Secrets {
    type Object = PartialObjectMeta<Secret>;
    const KIND: Kind = Kind::Secret;
    const METADATA_ONLY: bool = true;

    fn columns() -> Vec<Column> {
        vec![
            col("Name", ColumnType::String, "name"),
            col("Labels", ColumnType::Integer, ""),
            col("Age", ColumnType::Date, ""),
        ]
    }

    fn row(s: &PartialObjectMeta<Secret>, now: Timestamp) -> TableRow {
        let meta = &s.metadata;
        common::row(
            meta,
            vec![
                common::name(meta),
                num(meta.labels.as_ref().map_or(0, BTreeMap::len)),
                common::age(meta, now),
            ],
            Health::Unknown,
        )
    }

    fn details(s: &PartialObjectMeta<Secret>, now: Timestamp) -> Details {
        let meta = &s.metadata;
        let mut out = common::base_details(Kind::Secret, meta, now);
        out.status = "Secret".into();
        out.badges.push("metadata only".into());
        let annotations: Vec<Field> = meta
            .annotations
            .iter()
            .flatten()
            .filter(|(k, _)| k.as_str() != "kubectl.kubernetes.io/last-applied-configuration")
            .map(|(k, v)| Field::new(k.as_str(), Arc::<str>::from(v.as_str())))
            .collect();
        if !annotations.is_empty() {
            out.sections.push(Section {
                title: "Annotations".into(),
                fields: annotations,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::ByteString;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    #[test]
    fn configmap_counts_and_lists_keys() {
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some("app".into()),
                namespace: Some("shop".into()),
                ..ObjectMeta::default()
            },
            data: Some(BTreeMap::from([("a.yaml".into(), "x: 1\n".into())])),
            binary_data: Some(BTreeMap::from([(
                "logo.png".into(),
                ByteString(vec![0; 2048]),
            )])),
            ..ConfigMap::default()
        };
        let row = ConfigMaps::row(&cm, Timestamp::UNIX_EPOCH);
        assert_eq!(&*row.cells[1], "2");
        let d = ConfigMaps::details(&cm, Timestamp::UNIX_EPOCH);
        assert_eq!(&*d.status, "2 keys");
        let keys: Vec<(&str, &str)> = d.sections[0]
            .fields
            .iter()
            .map(|f| (&*f.label, &*f.value))
            .collect();
        assert_eq!(keys, [("a.yaml", "5 B"), ("logo.png", "2.0 KiB binary")]);
    }

    #[test]
    fn secrets_show_metadata_only() {
        let s = PartialObjectMeta::<Secret> {
            types: None,
            metadata: ObjectMeta {
                name: Some("db".into()),
                namespace: Some("shop".into()),
                labels: Some(BTreeMap::from([("app".into(), "db".into())])),
                annotations: Some(BTreeMap::from([
                    ("team".into(), "data".into()),
                    (
                        "kubectl.kubernetes.io/last-applied-configuration".into(),
                        "{...}".into(),
                    ),
                ])),
                ..ObjectMeta::default()
            },
            _phantom: std::marker::PhantomData,
        };
        let row = Secrets::row(&s, Timestamp::UNIX_EPOCH);
        let cells: Vec<&str> = row.cells.iter().map(|c| &**c).collect();
        assert_eq!(cells, ["db", "1", "-"]);
        let d = Secrets::details(&s, Timestamp::UNIX_EPOCH);
        assert_eq!(d.sections[0].fields.len(), 1, "last-applied is hidden");
        const { assert!(Secrets::METADATA_ONLY) };
    }
}
