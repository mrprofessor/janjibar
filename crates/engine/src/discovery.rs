//! API discovery: which kinds a context serves, grouped by API group.
//!
//! Aggregated discovery (`apidiscovery.k8s.io/v2`, Kubernetes 1.26 and
//! later) answers in two requests. Older servers get the legacy walk: one
//! request per group version, made concurrently. kube-rs's `Discovery` does
//! the same walk but drops short names and categories, so we call the same
//! client endpoints and keep them.

use std::collections::HashSet;
use std::sync::Arc;

use futures::StreamExt as _;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{APIGroup, APIResourceList};
use kube::Client;
use kube::core::discovery::v2::{
    ACCEPT_AGGREGATED_DISCOVERY_V2, APIGroupDiscovery, APIResourceDiscovery,
};
use model::{
    ConnectError, ContextName, DiscoveryProblem, Event, GroupVersionResource, KindCatalog,
    KindGroup, KindInfo, Scope, Verb, Verbs,
};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::errors::connect_error_from_request;

/// How many group versions the legacy walk fetches at once.
///
/// Clusters with many CRDs have over a hundred group versions. kubectl
/// fetches them all at once; a bound keeps us from opening that many
/// connections in a burst.
const LEGACY_CONCURRENCY: usize = 16;

/// The `kind` an aggregated discovery response carries. Older servers ignore
/// the aggregated `Accept` type and send an `APIGroupList` instead.
const AGGREGATED_KIND: &str = "APIGroupDiscoveryList";

/// Lists every kind the context serves that can be listed.
///
/// Uses aggregated discovery when the server supports it, and the legacy
/// per-group endpoints otherwise. A group that fails to load becomes a
/// [`DiscoveryProblem`]; the rest still load.
pub async fn discover(context: &ContextName, client: &Client) -> Result<KindCatalog, ConnectError> {
    let (core, apis) = tokio::try_join!(aggregated(client, "/api"), aggregated(client, "/apis"))
        .map_err(|e| connect_error_from_request(&e))?;
    let catalog = if let (Some(core), Some(apis)) = (core, apis) {
        from_aggregated(context, core, apis)
    } else {
        tracing::debug!(%context, "no aggregated discovery, using the legacy endpoints");
        legacy(context, client).await?
    };
    tracing::debug!(
        %context,
        groups = catalog.groups.len(),
        kinds = catalog.kinds().count(),
        problems = catalog.problems.len(),
        "discovery done"
    );
    Ok(catalog)
}

/// Runs discovery and sends the result as [`Event::Kinds`]. Failures are
/// logged; the pod watch reports connection problems to the UI.
pub(crate) async fn send_kinds(
    context: ContextName,
    client: Client,
    events: mpsc::UnboundedSender<Event>,
) {
    match discover(&context, &client).await {
        Ok(catalog) => {
            // A send error means the UI is gone; nothing to do.
            let _ = events.send(Event::Kinds(catalog));
        }
        Err(err) => tracing::warn!(%context, "discovery failed: {err}"),
    }
}

#[derive(Deserialize)]
struct AggregatedList {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    items: Vec<APIGroupDiscovery>,
}

/// Fetches one aggregated discovery document. `None` means the server does
/// not support aggregated discovery.
async fn aggregated(client: &Client, path: &str) -> kube::Result<Option<Vec<APIGroupDiscovery>>> {
    let request = http::Request::get(path)
        .header(http::header::ACCEPT, ACCEPT_AGGREGATED_DISCOVERY_V2)
        .body(Vec::new())
        .map_err(kube::Error::HttpError)?;
    let list = match client.request::<AggregatedList>(request).await {
        Ok(list) => list,
        // Some proxies answer an unknown media type with 406 or 415.
        Err(kube::Error::Api(status)) if matches!(status.code, 404 | 406 | 415) => return Ok(None),
        Err(err) => return Err(err),
    };
    Ok((list.kind == AGGREGATED_KIND).then_some(list.items))
}

fn from_aggregated(
    context: &ContextName,
    core: Vec<APIGroupDiscovery>,
    apis: Vec<APIGroupDiscovery>,
) -> KindCatalog {
    let mut problems = Vec::new();
    let groups = core
        .into_iter()
        .chain(apis)
        .filter_map(|group| {
            let name = group.metadata.and_then(|m| m.name).unwrap_or_default();
            let versions = group
                .versions
                .into_iter()
                .filter_map(|v| {
                    let version = v.version.unwrap_or_default();
                    // A stale version belongs to an aggregated API server
                    // that is down. Listing it would fail, so leave it out.
                    if v.freshness.as_deref() == Some("Stale") {
                        problems.push(DiscoveryProblem {
                            group_version: group_version(&name, &version),
                            message: "the API server for this group is not responding".to_owned(),
                        });
                        return None;
                    }
                    let resources = v.resources.into_iter().map(entry_from_v2).collect();
                    Some((version, resources))
                })
                .collect();
            build_group(&name, versions)
        })
        .collect();
    KindCatalog {
        context: context.clone(),
        groups,
        problems,
    }
}

async fn legacy(context: &ContextName, client: &Client) -> Result<KindCatalog, ConnectError> {
    let (core, apis) = tokio::try_join!(client.list_core_api_versions(), client.list_api_groups())
        .map_err(|e| connect_error_from_request(&e))?;

    // One entry per group: the name and its versions, preferred first.
    let mut groups: Vec<(String, Vec<String>)> = vec![(String::new(), core.versions)];
    groups.extend(apis.groups.into_iter().map(|g| {
        let versions = versions_by_preference(&g);
        (g.name, versions)
    }));

    let requests: Vec<(usize, String, String)> = groups
        .iter()
        .enumerate()
        .flat_map(|(i, (name, versions))| {
            versions.iter().map(move |v| (i, name.clone(), v.clone()))
        })
        .collect();
    let mut fetched: Vec<Vec<(String, kube::Result<APIResourceList>)>> =
        groups.iter().map(|_| Vec::new()).collect();
    let mut results = futures::stream::iter(requests)
        .map(|(i, name, version)| async move {
            let result = if name.is_empty() {
                client.list_core_api_resources(&version).await
            } else {
                client
                    .list_api_group_resources(&group_version(&name, &version))
                    .await
            };
            (i, version, result)
        })
        // `buffered` keeps request order, so versions stay in preference order.
        .buffered(LEGACY_CONCURRENCY);
    while let Some((i, version, result)) = results.next().await {
        if let Some(slot) = fetched.get_mut(i) {
            slot.push((version, result));
        }
    }

    let mut problems = Vec::new();
    let kind_groups = groups
        .into_iter()
        .zip(fetched)
        .filter_map(|((name, _), lists)| {
            let versions = lists
                .into_iter()
                .filter_map(|(version, result)| match result {
                    Ok(list) => Some((version, entries_from_list(list))),
                    Err(err) => {
                        problems.push(DiscoveryProblem {
                            group_version: group_version(&name, &version),
                            message: connect_error_from_request(&err).to_string(),
                        });
                        None
                    }
                })
                .collect();
            build_group(&name, versions)
        })
        .collect();
    Ok(KindCatalog {
        context: context.clone(),
        groups: kind_groups,
        problems,
    })
}

/// A group's versions with the preferred one first, then the server's order.
fn versions_by_preference(group: &APIGroup) -> Vec<String> {
    let preferred = group.preferred_version.as_ref().map(|p| p.version.clone());
    preferred
        .iter()
        .cloned()
        .chain(
            group
                .versions
                .iter()
                .map(|v| v.version.clone())
                .filter(|v| Some(v) != preferred.as_ref()),
        )
        .collect()
}

/// One resource from either discovery format.
struct Entry {
    plural: String,
    kind: String,
    singular: String,
    scope: Scope,
    verbs: Vec<String>,
    short_names: Vec<String>,
    categories: Vec<String>,
}

fn entry_from_v2(r: APIResourceDiscovery) -> Entry {
    Entry {
        plural: r.resource.unwrap_or_default(),
        kind: r.response_kind.and_then(|k| k.kind).unwrap_or_default(),
        singular: r.singular_resource.unwrap_or_default(),
        scope: scope(r.scope.as_deref() == Some("Namespaced")),
        verbs: r.verbs,
        short_names: r.short_names,
        categories: r.categories,
    }
}

fn entries_from_list(list: APIResourceList) -> Vec<Entry> {
    list.resources
        .into_iter()
        // `pods/log`, `deployments/scale`: subresources, not kinds.
        .filter(|r| !r.name.contains('/'))
        .map(|r| Entry {
            plural: r.name,
            kind: r.kind,
            singular: r.singular_name,
            scope: scope(r.namespaced),
            verbs: r.verbs,
            short_names: r.short_names.unwrap_or_default(),
            categories: r.categories.unwrap_or_default(),
        })
        .collect()
}

fn scope(namespaced: bool) -> Scope {
    if namespaced {
        Scope::Namespaced
    } else {
        Scope::Cluster
    }
}

/// Builds a group from its versions, most preferred first.
///
/// Each resource appears once, at the first version that serves it, the way
/// `kubectl api-resources` picks. Resources that cannot be listed are left
/// out. Returns `None` when nothing is left.
fn build_group(name: &str, versions: Vec<(String, Vec<Entry>)>) -> Option<KindGroup> {
    let preferred_version: Arc<str> = versions.first()?.0.as_str().into();
    let group: Arc<str> = name.into();
    let mut seen = HashSet::new();
    let mut kinds = Vec::new();
    for (version, entries) in versions {
        let version: Arc<str> = version.into();
        for entry in entries {
            let verbs: Verbs = entry.verbs.iter().filter_map(|v| Verb::parse(v)).collect();
            if !verbs.contains(Verb::List) || entry.kind.is_empty() {
                continue;
            }
            if !seen.insert(entry.plural.clone()) {
                continue;
            }
            kinds.push(KindInfo {
                resource: GroupVersionResource {
                    group: Arc::clone(&group),
                    version: Arc::clone(&version),
                    resource: entry.plural.into(),
                },
                kind: entry.kind.into(),
                singular: entry.singular.into(),
                scope: entry.scope,
                verbs,
                short_names: entry.short_names.into_iter().map(Into::into).collect(),
                categories: entry.categories.into_iter().map(Into::into).collect(),
            });
        }
    }
    if kinds.is_empty() {
        return None;
    }
    kinds.sort_unstable_by(|a, b| a.kind.cmp(&b.kind));
    Some(KindGroup {
        name: group,
        preferred_version,
        kinds,
    })
}

fn group_version(group: &str, version: &str) -> String {
    if group.is_empty() {
        version.to_owned()
    } else {
        format!("{group}/{version}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aggregated_fixture(json: &str) -> Vec<APIGroupDiscovery> {
        let list: AggregatedList = serde_json::from_str(json).unwrap();
        assert_eq!(list.kind, AGGREGATED_KIND);
        list.items
    }

    /// Discovery recorded from a kind cluster (Kubernetes 1.34). Two groups
    /// were added by hand at the end of `/apis`: a stale `metrics.k8s.io` and
    /// `stable.example.com`, a custom resource group with two versions.
    fn aggregated_catalog() -> KindCatalog {
        from_aggregated(
            &"kind".into(),
            aggregated_fixture(include_str!(
                "../tests/fixtures/discovery/aggregated-api.json"
            )),
            aggregated_fixture(include_str!(
                "../tests/fixtures/discovery/aggregated-apis.json"
            )),
        )
    }

    fn group<'a>(catalog: &'a KindCatalog, name: &str) -> &'a KindGroup {
        catalog.groups.iter().find(|g| &*g.name == name).unwrap()
    }

    #[test]
    fn aggregated_discovery_groups_kinds_in_server_order() {
        let catalog = aggregated_catalog();
        let names: Vec<&str> = catalog.groups.iter().map(|g| &*g.name).collect();
        assert_eq!(names.first(), Some(&""));
        assert!(catalog.groups[0].is_core());
        assert_eq!(
            names.get(1..4),
            Some(&["apiregistration.k8s.io", "apps", "events.k8s.io"][..])
        );
        assert_eq!(names.last(), Some(&"stable.example.com"));
        // Token and access reviews can only be created, so their groups are empty.
        assert!(!names.contains(&"authentication.k8s.io"));
        assert!(!names.contains(&"authorization.k8s.io"));

        let core: Vec<&str> = group(&catalog, "").kinds.iter().map(|k| &*k.kind).collect();
        // Bindings can only be created, so they are not listed.
        assert_eq!(
            core,
            [
                "ComponentStatus",
                "ConfigMap",
                "Endpoints",
                "Event",
                "LimitRange",
                "Namespace",
                "Node",
                "PersistentVolume",
                "PersistentVolumeClaim",
                "Pod",
                "PodTemplate",
                "ReplicationController",
                "ResourceQuota",
                "Secret",
                "Service",
                "ServiceAccount",
            ]
        );
    }

    #[test]
    fn the_preferred_version_wins_when_several_serve_a_kind() {
        let catalog = aggregated_catalog();
        let hpa = catalog.find("hpa").unwrap();
        assert_eq!(hpa.resource.api_version(), "autoscaling/v2");
        assert_eq!(&*group(&catalog, "autoscaling").preferred_version, "v2");
        let count = catalog
            .kinds()
            .filter(|k| &*k.kind == "HorizontalPodAutoscaler")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn aggregated_discovery_keeps_scope_verbs_and_short_names() {
        let catalog = aggregated_catalog();
        let pods = catalog.find("po").unwrap();
        assert_eq!(pods.resource.api_version(), "v1");
        assert_eq!(&*pods.resource.resource, "pods");
        assert_eq!(&*pods.singular, "pod");
        assert_eq!(pods.scope, Scope::Namespaced);
        assert!(pods.verbs.contains(Verb::Watch));
        assert!(pods.verbs.contains(Verb::DeleteCollection));
        assert_eq!(&*pods.categories, [Arc::from("all")]);

        let nodes = catalog.find("no").unwrap();
        assert_eq!(nodes.scope, Scope::Cluster);

        let crd = catalog.find("ct").unwrap();
        assert_eq!(&*crd.kind, "CronTab");
        assert_eq!(crd.resource.api_version(), "stable.example.com/v1");
    }

    #[test]
    fn a_kind_only_in_an_older_version_still_appears_once() {
        let catalog = aggregated_catalog();
        let stable = group(&catalog, "stable.example.com");
        assert_eq!(&*stable.preferred_version, "v1");
        let kinds: Vec<(&str, &str)> = stable
            .kinds
            .iter()
            .map(|k| (&*k.kind, &*k.resource.version))
            .collect();
        // CronTab is served by v1 and v1beta1; v1 wins. Legacy only exists in v1beta1.
        assert_eq!(kinds, [("CronTab", "v1"), ("LegacyTab", "v1beta1")]);
    }

    #[test]
    fn stale_versions_become_problems() {
        let catalog = aggregated_catalog();
        assert!(catalog.groups.iter().all(|g| &*g.name != "metrics.k8s.io"));
        assert_eq!(
            catalog.problems,
            [DiscoveryProblem {
                group_version: "metrics.k8s.io/v1beta1".to_owned(),
                message: "the API server for this group is not responding".to_owned(),
            }]
        );
    }

    #[test]
    fn events_resolve_to_the_core_group_first() {
        let catalog = aggregated_catalog();
        assert_eq!(&*catalog.find("events").unwrap().resource.group, "");
        assert_eq!(
            &*catalog.find("events.events.k8s.io").unwrap().resource.group,
            "events.k8s.io"
        );
    }

    #[test]
    fn legacy_resource_lists_skip_subresources() {
        let list: APIResourceList = serde_json::from_str(include_str!(
            "../tests/fixtures/discovery/legacy-apps-v1.json"
        ))
        .unwrap();
        let group = build_group("apps", vec![("v1".to_owned(), entries_from_list(list))]).unwrap();
        let kinds: Vec<&str> = group.kinds.iter().map(|k| &*k.kind).collect();
        assert_eq!(
            kinds,
            [
                "ControllerRevision",
                "DaemonSet",
                "Deployment",
                "ReplicaSet",
                "StatefulSet"
            ]
        );
        let deploy = group
            .kinds
            .iter()
            .find(|k| &*k.kind == "Deployment")
            .unwrap();
        assert_eq!(&*deploy.short_names, [Arc::from("deploy")]);
        assert_eq!(&*deploy.singular, "deployment");
    }

    #[test]
    fn preferred_version_comes_first() {
        let group: APIGroup = serde_json::from_value(serde_json::json!({
            "name": "autoscaling",
            "versions": [
                { "groupVersion": "autoscaling/v1", "version": "v1" },
                { "groupVersion": "autoscaling/v2", "version": "v2" },
            ],
            "preferredVersion": { "groupVersion": "autoscaling/v2", "version": "v2" },
        }))
        .unwrap();
        assert_eq!(versions_by_preference(&group), ["v2", "v1"]);
    }
}
