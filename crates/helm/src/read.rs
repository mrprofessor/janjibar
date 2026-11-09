//! Reading release records from the cluster.

use std::fmt::Debug;
use std::sync::Arc;

use k8s_openapi::NamespaceResourceScope;
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use kube::api::ListParams;
use kube::{Api, Client, Resource};
use model::{HelmReleaseDetails, HelmRevision, HelmStorage};
use serde::de::DeserializeOwned;

use crate::decode::{DecodeError, decode_config_map, decode_secret};
use crate::records::{OWNER_SELECTOR, object_name, revision_from_meta, sort_newest_first};

/// Records per list request, the same page size as client-go.
const PAGE_SIZE: u32 = 500;

/// Why releases could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// A list of release records failed.
    #[error("could not list Helm release {}", plural(*.storage))]
    List {
        /// The kind of object being listed.
        storage: HelmStorage,
        /// The API error.
        #[source]
        source: kube::Error,
    },
    /// Fetching one record failed.
    #[error("could not read `{namespace}/{object}`")]
    Get {
        /// The record's namespace.
        namespace: String,
        /// The record's object name.
        object: String,
        /// The API error.
        #[source]
        source: kube::Error,
    },
    /// The record is gone, usually because Helm pruned old revisions.
    #[error("`{namespace}/{object}` no longer exists")]
    NotFound {
        /// The record's namespace.
        namespace: String,
        /// The record's object name.
        object: String,
    },
    /// The record exists but its payload is unreadable.
    #[error("`{namespace}/{object}` is not a readable Helm release")]
    Decode {
        /// The record's namespace.
        namespace: String,
        /// The record's object name.
        object: String,
        /// What was wrong with the payload.
        #[source]
        source: DecodeError,
    },
}

fn plural(storage: HelmStorage) -> &'static str {
    match storage {
        HelmStorage::Secret => "Secrets",
        HelmStorage::ConfigMap => "ConfigMaps",
    }
}

/// Lists every stored revision of every release, from labels only.
///
/// Reads Secrets and `ConfigMaps` labelled `owner=helm` in `namespace`, or in
/// all namespaces when it is `None`. Only metadata is requested, so no
/// payload crosses the wire. Pass the result to [`crate::releases`] for one
/// row per release.
///
/// Secrets are Helm's default storage, so failing to list them is an error.
/// `ConfigMap` storage is rare and many roles may not list `ConfigMaps`
/// cluster-wide, so a 403 there is logged and skipped rather than hiding
/// the Secret-backed releases.
pub async fn list_revisions(
    client: &Client,
    namespace: Option<&str>,
) -> Result<Vec<HelmRevision>, ReadError> {
    let (secrets, config_maps) = futures::join!(
        list_storage(client, HelmStorage::Secret, namespace),
        list_storage(client, HelmStorage::ConfigMap, namespace),
    );
    let mut revisions = secrets?;
    match config_maps {
        Ok(more) => revisions.extend(more),
        Err(ReadError::List {
            source: kube::Error::Api(status),
            ..
        }) if status.is_forbidden() => {
            tracing::warn!(
                namespace = namespace.unwrap_or("(all)"),
                "skipping ConfigMap-stored Helm releases: {}",
                status.message
            );
        }
        Err(err) => return Err(err),
    }
    Ok(revisions)
}

/// Lists the revisions held in one kind of storage, from labels only.
pub async fn list_storage(
    client: &Client,
    storage: HelmStorage,
    namespace: Option<&str>,
) -> Result<Vec<HelmRevision>, ReadError> {
    match storage {
        HelmStorage::Secret => {
            list_meta(
                &scoped::<Secret>(client, namespace),
                storage,
                OWNER_SELECTOR,
            )
            .await
        }
        HelmStorage::ConfigMap => {
            list_meta(
                &scoped::<ConfigMap>(client, namespace),
                storage,
                OWNER_SELECTOR,
            )
            .await
        }
    }
}

/// Lists every stored revision of one release, newest first, from labels
/// only.
pub async fn history(
    client: &Client,
    storage: HelmStorage,
    namespace: &str,
    name: &str,
) -> Result<Vec<HelmRevision>, ReadError> {
    let selector = format!("{OWNER_SELECTOR},name={name}");
    let mut revisions = match storage {
        HelmStorage::Secret => {
            list_meta(
                &scoped::<Secret>(client, Some(namespace)),
                storage,
                &selector,
            )
            .await?
        }
        HelmStorage::ConfigMap => {
            list_meta(
                &scoped::<ConfigMap>(client, Some(namespace)),
                storage,
                &selector,
            )
            .await?
        }
    };
    // The `name` label and the object name always agree when Helm wrote
    // the record; trust the object name, which is what `get` fetches.
    revisions.retain(|r| &*r.name == name);
    sort_newest_first(&mut revisions);
    Ok(revisions)
}

/// Fetches one revision's record and decodes its payload.
///
/// Fields an old payload leaves out (namespace, name, revision) are filled
/// in from `revision`.
pub async fn get(
    client: &Client,
    revision: &HelmRevision,
) -> Result<HelmReleaseDetails, ReadError> {
    let namespace = &*revision.namespace;
    let object = object_name(&revision.name, revision.revision);
    let fetch_error = |source| ReadError::Get {
        namespace: namespace.to_owned(),
        object: object.clone(),
        source,
    };
    let decoded = match revision.storage {
        HelmStorage::Secret => Api::<Secret>::namespaced(client.clone(), namespace)
            .get_opt(&object)
            .await
            .map_err(fetch_error)?
            .map(|secret| decode_secret(&secret)),
        HelmStorage::ConfigMap => Api::<ConfigMap>::namespaced(client.clone(), namespace)
            .get_opt(&object)
            .await
            .map_err(fetch_error)?
            .map(|config_map| decode_config_map(&config_map)),
    };
    let Some(decoded) = decoded else {
        return Err(ReadError::NotFound {
            namespace: namespace.to_owned(),
            object,
        });
    };
    let mut details = decoded.map_err(|source| ReadError::Decode {
        namespace: namespace.to_owned(),
        object: object.clone(),
        source,
    })?;
    if details.namespace.is_empty() {
        details.namespace = Arc::clone(&revision.namespace);
    }
    if details.name.is_empty() {
        details.name = Arc::clone(&revision.name);
    }
    if details.revision == 0 {
        details.revision = revision.revision;
    }
    Ok(details)
}

fn scoped<K>(client: &Client, namespace: Option<&str>) -> Api<K>
where
    K: Resource<Scope = NamespaceResourceScope>,
    K::DynamicType: Default,
{
    match namespace {
        Some(namespace) => Api::namespaced(client.clone(), namespace),
        None => Api::all(client.clone()),
    }
}

/// Lists metadata only, following continue tokens to the end.
async fn list_meta<K>(
    api: &Api<K>,
    storage: HelmStorage,
    selector: &str,
) -> Result<Vec<HelmRevision>, ReadError>
where
    K: Resource + Clone + DeserializeOwned + Debug,
{
    let mut params = ListParams::default().labels(selector).limit(PAGE_SIZE);
    let mut revisions = Vec::new();
    loop {
        let page = api
            .list_metadata(&params)
            .await
            .map_err(|source| ReadError::List { storage, source })?;
        revisions.extend(
            page.items
                .iter()
                .filter_map(|item| revision_from_meta(&item.metadata, storage)),
        );
        match page.metadata.continue_ {
            Some(token) if !token.is_empty() => params.continue_token = Some(token),
            _ => return Ok(revisions),
        }
    }
}
