//! Reads Helm releases straight from the cluster, without the helm binary.
//!
//! Helm 3 and Helm 4 store each revision of a release as one Secret (or
//! `ConfigMap`) named `sh.helm.release.v1.<name>.v<revision>` and labelled
//! `owner=helm`. Its `release` data is base64 of gzip of JSON.
//!
//! - [`list_revisions`] reads labels only, so the release list loads without
//!   moving a single payload. [`releases`] turns it into one row per release.
//! - [`history`] lists one release's revisions, newest first, from labels.
//! - [`get`] fetches one record and decodes it on demand.
//!
//! Writes (install, upgrade, rollback, uninstall) belong to the helm CLI and
//! are not here. This crate never depends on GPUI; it returns `model` types.

mod decode;
mod read;
mod records;

pub use decode::{DATA_KEY, DecodeError, decode, decode_config_map, decode_secret};
pub use read::{ReadError, get, history, list_revisions, list_storage};
pub use records::{OWNER_SELECTOR, object_name, parse_object_name, releases, revision_from_meta};
