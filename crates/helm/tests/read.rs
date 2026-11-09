//! Reading releases from the engine's fake API server. No cluster needed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

#[path = "../../engine/tests/support/fake_api.rs"]
mod fake_api;

use std::io::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use fake_api::{FakeApi, Request};
use flate2::Compression;
use flate2::write::GzEncoder;
use model::{HelmRevision, HelmStatus, HelmStorage};
use serde_json::{Value, json};

fn client_for(api: &FakeApi) -> kube::Client {
    let config = kube::Config::new(api.url().parse().unwrap());
    kube::Client::try_from(config).unwrap()
}

/// Helm's text for a payload: base64 of gzip of JSON.
fn helm_text(release: &Value) -> String {
    let mut gz = GzEncoder::new(Vec::new(), Compression::best());
    gz.write_all(release.to_string().as_bytes()).unwrap();
    STANDARD.encode(gz.finish().unwrap())
}

fn payload(namespace: &str, name: &str, revision: u32, status: &str) -> Value {
    json!({
        "name": name,
        "namespace": namespace,
        "version": revision,
        "info": {
            "last_deployed": "2026-11-09T10:00:00Z",
            "status": status,
            "description": format!("revision {revision}"),
        },
        "chart": { "metadata": { "name": name, "version": "1.0.0", "appVersion": "2.0" } },
        "config": { "replicas": revision },
        "manifest": "kind: Service\n",
    })
}

fn labels(name: &str, revision: u32, status: &str) -> Value {
    json!({
        "owner": "helm",
        "name": name,
        "version": revision.to_string(),
        "status": status,
        "modifiedAt": "1794182400",
    })
}

/// A Secret as Helm's secrets driver writes it, with a real payload.
fn helm_secret(namespace: &str, name: &str, revision: u32, status: &str) -> Value {
    let text = helm_text(&payload(namespace, name, revision, status));
    json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "type": "helm.sh/release.v1",
        "metadata": {
            "namespace": namespace,
            "name": helm::object_name(name, revision),
            "labels": labels(name, revision, status),
        },
        // The API base64-encodes Secret data on the wire.
        "data": { "release": STANDARD.encode(text) },
    })
}

/// A `ConfigMap` as Helm's configmap driver writes it.
fn helm_config_map(namespace: &str, name: &str, revision: u32, status: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "namespace": namespace,
            "name": helm::object_name(name, revision),
            "labels": labels(name, revision, status),
        },
        "data": { "release": helm_text(&payload(namespace, name, revision, status)) },
    })
}

/// Requests for Secrets, then for `ConfigMaps`. Both kinds are listed at
/// once, so their arrival order is not fixed.
fn storage_requests(api: &FakeApi) -> Vec<Request> {
    let requests = api.requests();
    let about = |kind: &'static str| {
        requests
            .iter()
            .filter(move |r| r.path.split('/').any(|segment| segment == kind))
            .cloned()
    };
    about("secrets").chain(about("configmaps")).collect()
}

fn summary(rev: &HelmRevision) -> (String, String, u32, HelmStatus, HelmStorage) {
    (
        rev.namespace.to_string(),
        rev.name.to_string(),
        rev.revision,
        rev.status,
        rev.storage,
    )
}

fn revision(namespace: &str, name: &str, revision: u32, storage: HelmStorage) -> HelmRevision {
    HelmRevision {
        namespace: namespace.into(),
        name: name.into(),
        revision,
        status: HelmStatus::Unknown,
        updated: None,
        storage,
    }
}

#[tokio::test]
async fn lists_releases_from_labels_without_fetching_payloads() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_object("secrets", helm_secret("shop", "web", 1, "superseded"));
    api.add_object("secrets", helm_secret("shop", "web", 2, "deployed"));
    api.add_object("secrets", helm_secret("data", "pg", 1, "deployed"));
    api.add_object(
        "configmaps",
        helm_config_map("ingress", "edge", 1, "superseded"),
    );
    api.add_object(
        "configmaps",
        helm_config_map("ingress", "edge", 2, "failed"),
    );
    // Not Helm's: no owner label, and a name Helm would never write.
    api.add_object(
        "secrets",
        json!({ "metadata": { "namespace": "shop", "name": "db-password" }, "data": {} }),
    );
    api.add_object(
        "secrets",
        json!({ "metadata": { "namespace": "shop", "name": "lookalike", "labels": { "owner": "helm" } } }),
    );

    let revisions = helm::list_revisions(&client_for(&api), None).await.unwrap();
    let mut found: Vec<_> = revisions.iter().map(summary).collect();
    found.sort_by(|a, b| (&a.0, &a.1, a.2).cmp(&(&b.0, &b.1, b.2)));
    assert_eq!(
        found,
        [
            (
                "data".into(),
                "pg".into(),
                1,
                HelmStatus::Deployed,
                HelmStorage::Secret
            ),
            (
                "ingress".into(),
                "edge".into(),
                1,
                HelmStatus::Superseded,
                HelmStorage::ConfigMap
            ),
            (
                "ingress".into(),
                "edge".into(),
                2,
                HelmStatus::Failed,
                HelmStorage::ConfigMap
            ),
            (
                "shop".into(),
                "web".into(),
                1,
                HelmStatus::Superseded,
                HelmStorage::Secret
            ),
            (
                "shop".into(),
                "web".into(),
                2,
                HelmStatus::Deployed,
                HelmStorage::Secret
            ),
        ]
    );
    assert!(revisions.iter().all(|r| r.updated.is_some()));

    let releases: Vec<_> = helm::releases(&revisions)
        .iter()
        .map(|r| (summary(&r.latest), r.stored_revisions))
        .collect();
    assert_eq!(
        releases,
        [
            (
                (
                    "data".into(),
                    "pg".into(),
                    1,
                    HelmStatus::Deployed,
                    HelmStorage::Secret
                ),
                1
            ),
            (
                (
                    "ingress".into(),
                    "edge".into(),
                    2,
                    HelmStatus::Failed,
                    HelmStorage::ConfigMap
                ),
                2
            ),
            (
                (
                    "shop".into(),
                    "web".into(),
                    2,
                    HelmStatus::Deployed,
                    HelmStorage::Secret
                ),
                2
            ),
        ]
    );

    // One metadata-only list per storage kind, and nothing else: no
    // payload was fetched.
    let requests = storage_requests(&api);
    let paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, ["/api/v1/secrets", "/api/v1/configmaps"]);
    for request in &requests {
        assert!(request.wants_metadata(), "payload requested: {request:?}");
        assert_eq!(
            request.param("labelSelector").as_deref(),
            Some("owner=helm")
        );
    }
}

#[tokio::test]
async fn lists_one_namespace() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_object("secrets", helm_secret("shop", "web", 1, "deployed"));
    api.add_object("secrets", helm_secret("data", "pg", 1, "deployed"));

    let revisions = helm::list_revisions(&client_for(&api), Some("shop"))
        .await
        .unwrap();
    assert_eq!(revisions.len(), 1);
    assert_eq!(&*revisions[0].name, "web");

    let paths: Vec<String> = storage_requests(&api).into_iter().map(|r| r.path).collect();
    assert_eq!(
        paths,
        [
            "/api/v1/namespaces/shop/secrets",
            "/api/v1/namespaces/shop/configmaps"
        ]
    );
}

#[tokio::test]
async fn pages_through_long_lists() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    for i in 0..1100 {
        api.add_object(
            "secrets",
            json!({ "metadata": {
                "namespace": "load",
                "name": helm::object_name(&format!("app-{i:04}"), 1),
                "labels": labels(&format!("app-{i:04}"), 1, "deployed"),
            } }),
        );
    }

    let revisions = helm::list_storage(&client_for(&api), HelmStorage::Secret, None)
        .await
        .unwrap();
    assert_eq!(revisions.len(), 1100);
    assert_eq!(helm::releases(&revisions).len(), 1100);

    let requests = storage_requests(&api);
    let tokens: Vec<Option<String>> = requests.iter().map(|r| r.param("continue")).collect();
    assert_eq!(tokens, [None, Some("500".into()), Some("1000".into())]);
    assert!(
        requests
            .iter()
            .all(|r| r.param("limit").as_deref() == Some("500"))
    );
}

#[tokio::test]
async fn history_is_one_release_newest_first() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    for (rev, status) in [(3, "deployed"), (1, "superseded"), (2, "superseded")] {
        api.add_object("secrets", helm_secret("shop", "web", rev, status));
    }
    api.add_object("secrets", helm_secret("shop", "web-admin", 5, "deployed"));
    api.add_object("secrets", helm_secret("other", "web", 9, "deployed"));

    let history = helm::history(&client_for(&api), HelmStorage::Secret, "shop", "web")
        .await
        .unwrap();
    let rows: Vec<_> = history.iter().map(|r| (r.revision, r.status)).collect();
    assert_eq!(
        rows,
        [
            (3, HelmStatus::Deployed),
            (2, HelmStatus::Superseded),
            (1, HelmStatus::Superseded),
        ]
    );

    let requests = storage_requests(&api);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/api/v1/namespaces/shop/secrets");
    assert!(requests[0].wants_metadata());
    assert_eq!(
        requests[0].param("labelSelector").as_deref(),
        Some("owner=helm,name=web")
    );
}

#[tokio::test]
async fn get_decodes_one_revision_on_demand() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_object("secrets", helm_secret("shop", "web", 1, "superseded"));
    api.add_object("secrets", helm_secret("shop", "web", 2, "deployed"));
    api.add_object(
        "configmaps",
        helm_config_map("ingress", "edge", 4, "failed"),
    );
    let client = client_for(&api);

    let details = helm::get(&client, &revision("shop", "web", 2, HelmStorage::Secret))
        .await
        .unwrap();
    assert_eq!((&*details.namespace, &*details.name), ("shop", "web"));
    assert_eq!(details.revision, 2);
    assert_eq!(details.status, HelmStatus::Deployed);
    assert_eq!(details.chart.to_string(), "web-1.0.0");
    assert_eq!(&*details.description, "revision 2");
    assert_eq!(&*details.values, "replicas: 2\n");

    let edge = helm::get(
        &client,
        &revision("ingress", "edge", 4, HelmStorage::ConfigMap),
    )
    .await
    .unwrap();
    assert_eq!(edge.status, HelmStatus::Failed);

    // Exactly the two records asked for, each fetched whole.
    let requests = storage_requests(&api);
    let paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/api/v1/namespaces/shop/secrets/sh.helm.release.v1.web.v2",
            "/api/v1/namespaces/ingress/configmaps/sh.helm.release.v1.edge.v4",
        ]
    );
    assert!(requests.iter().all(|r| !r.wants_metadata()));
}

#[tokio::test]
async fn sparse_payloads_take_identity_from_the_record() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_object(
        "secrets",
        json!({
            "metadata": {
                "namespace": "shop",
                "name": helm::object_name("web", 7),
                "labels": labels("web", 7, "deployed"),
            },
            "data": { "release": STANDARD.encode(helm_text(&json!({ "info": { "status": "deployed" } }))) },
        }),
    );
    let details = helm::get(
        &client_for(&api),
        &revision("shop", "web", 7, HelmStorage::Secret),
    )
    .await
    .unwrap();
    assert_eq!(
        (&*details.namespace, &*details.name, details.revision),
        ("shop", "web", 7)
    );
}

#[tokio::test]
async fn corrupt_payloads_name_the_record() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let mut secret = helm_secret("shop", "web", 1, "deployed");
    secret["data"]["release"] = STANDARD.encode("definitely not helm").into();
    api.add_object("secrets", secret);

    let err = helm::get(
        &client_for(&api),
        &revision("shop", "web", 1, HelmStorage::Secret),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "`shop/sh.helm.release.v1.web.v1` is not a readable Helm release"
    );
    let helm::ReadError::Decode { source, .. } = err else {
        panic!("expected a decode error, got {err:?}");
    };
    assert!(matches!(source, helm::DecodeError::Base64(_)), "{source:?}");
}

#[tokio::test]
async fn pruned_revisions_are_not_found() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    let err = helm::get(
        &client_for(&api),
        &revision("shop", "web", 1, HelmStorage::Secret),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, helm::ReadError::NotFound { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "`shop/sh.helm.release.v1.web.v1` no longer exists"
    );
}

#[tokio::test]
async fn forbidden_config_maps_do_not_hide_secret_releases() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_object("secrets", helm_secret("shop", "web", 1, "deployed"));
    api.forbid("configmaps");

    let revisions = helm::list_revisions(&client_for(&api), None).await.unwrap();
    assert_eq!(revisions.len(), 1);
}

#[tokio::test]
async fn forbidden_secrets_are_an_error() {
    let api = FakeApi::start("127.0.0.1:0").await.unwrap();
    api.add_object(
        "configmaps",
        helm_config_map("ingress", "edge", 1, "deployed"),
    );
    api.forbid("secrets");

    let err = helm::list_revisions(&client_for(&api), None)
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            helm::ReadError::List {
                storage: HelmStorage::Secret,
                source: kube::Error::Api(status),
            } if status.is_forbidden()
        ),
        "{err:?}"
    );
    assert_eq!(err.to_string(), "could not list Helm release Secrets");
}
