//! The inspector's owner chain: Deployment, then ReplicaSet, then Pod.
//!
//! Walks controller owner references upwards with metadata-only reads, so it
//! costs one small request per level and works without any watch running.

use std::fmt::Debug;
use std::sync::Arc;

use k8s_openapi::NamespaceResourceScope;
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet};
use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::api::core::v1::{
    ConfigMap, Event as KubeEvent, Namespace, Node, PersistentVolume, PersistentVolumeClaim, Pod,
    Secret, Service,
};
use k8s_openapi::api::networking::v1::Ingress;
use k8s_openapi::api::storage::v1::StorageClass;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::{Api, Client, Resource};
use model::resource::{Kind, ObjectKey, OwnerLink};
use serde::de::DeserializeOwned;

/// Chains longer than this are cut; real ones are two or three deep.
const MAX_DEPTH: usize = 6;

/// Resolves the owners of an object, outermost first. Unknown owner kinds
/// end the chain after being listed. Errors end it quietly: the chain is a
/// convenience, not something to fail over.
pub(crate) async fn resolve(client: &Client, kind: Kind, key: &ObjectKey) -> Vec<OwnerLink> {
    let mut chain = Vec::new();
    let mut current = Some((kind, key.clone()));
    while let Some((kind, key)) = current.take() {
        if chain.len() >= MAX_DEPTH {
            break;
        }
        let Some(owner) = controller(owner_refs(client, kind, &key).await) else {
            break;
        };
        let next_kind = Kind::from_singular(&owner.kind);
        chain.push(OwnerLink {
            kind: owner.kind.as_str().into(),
            name: owner.name.as_str().into(),
        });
        if let Some(next) = next_kind {
            let name: Arc<str> = owner.name.as_str().into();
            let namespace = if next.namespaced() {
                key.namespace.clone()
            } else {
                None
            };
            current = Some((next, ObjectKey { namespace, name }));
        }
    }
    chain.reverse();
    chain
}

/// The controlling owner, or the first owner when none is marked.
fn controller(refs: Vec<OwnerReference>) -> Option<OwnerReference> {
    let mut refs = refs;
    let ix = refs
        .iter()
        .position(|r| r.controller == Some(true))
        .unwrap_or(0);
    (ix < refs.len()).then(|| refs.swap_remove(ix))
}

async fn owner_refs(client: &Client, kind: Kind, key: &ObjectKey) -> Vec<OwnerReference> {
    let ns = key.namespace.as_deref().unwrap_or_default();
    let name = &*key.name;
    let c = client.clone();
    match kind {
        Kind::Pod => ns_refs::<Pod>(c, ns, name).await,
        Kind::Deployment => ns_refs::<Deployment>(c, ns, name).await,
        Kind::StatefulSet => ns_refs::<StatefulSet>(c, ns, name).await,
        Kind::DaemonSet => ns_refs::<DaemonSet>(c, ns, name).await,
        Kind::ReplicaSet => ns_refs::<ReplicaSet>(c, ns, name).await,
        Kind::Job => ns_refs::<Job>(c, ns, name).await,
        Kind::CronJob => ns_refs::<CronJob>(c, ns, name).await,
        Kind::Service => ns_refs::<Service>(c, ns, name).await,
        Kind::Ingress => ns_refs::<Ingress>(c, ns, name).await,
        Kind::ConfigMap => ns_refs::<ConfigMap>(c, ns, name).await,
        Kind::Secret => ns_refs::<Secret>(c, ns, name).await,
        Kind::Event => ns_refs::<KubeEvent>(c, ns, name).await,
        Kind::PersistentVolumeClaim => ns_refs::<PersistentVolumeClaim>(c, ns, name).await,
        Kind::Node => cluster_refs::<Node>(c, name).await,
        Kind::Namespace => cluster_refs::<Namespace>(c, name).await,
        Kind::PersistentVolume => cluster_refs::<PersistentVolume>(c, name).await,
        Kind::StorageClass => cluster_refs::<StorageClass>(c, name).await,
    }
}

async fn ns_refs<K>(client: Client, ns: &str, name: &str) -> Vec<OwnerReference>
where
    K: Resource<Scope = NamespaceResourceScope, DynamicType = ()>
        + Clone
        + DeserializeOwned
        + Debug,
{
    refs_from(Api::<K>::namespaced(client, ns), name).await
}

async fn cluster_refs<K>(client: Client, name: &str) -> Vec<OwnerReference>
where
    K: Resource<DynamicType = ()> + Clone + DeserializeOwned + Debug,
{
    refs_from(Api::<K>::all(client), name).await
}

async fn refs_from<K>(api: Api<K>, name: &str) -> Vec<OwnerReference>
where
    K: Resource<DynamicType = ()> + Clone + DeserializeOwned + Debug,
{
    match api.get_metadata(name).await {
        Ok(meta) => meta.metadata.owner_references.unwrap_or_default(),
        Err(err) => {
            tracing::debug!("owner lookup for {name} stopped: {err}");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(kind: &str, name: &str, controller: bool) -> OwnerReference {
        OwnerReference {
            kind: kind.into(),
            name: name.into(),
            controller: Some(controller),
            ..OwnerReference::default()
        }
    }

    #[test]
    fn picks_the_controller() {
        let refs = vec![
            owner("ConfigMap", "a", false),
            owner("ReplicaSet", "b", true),
        ];
        assert_eq!(controller(refs).map(|r| r.name), Some("b".to_owned()));
        let refs = vec![owner("Widget", "w", false)];
        assert_eq!(controller(refs).map(|r| r.name), Some("w".to_owned()));
        assert!(controller(Vec::new()).is_none());
    }
}
