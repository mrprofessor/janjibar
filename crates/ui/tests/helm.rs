//! View tests for the Helm releases view, with a fake engine.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

#[allow(
    dead_code,
    reason = "the harness is shared; this file uses only part of it"
)]
mod support {
    pub mod harness;
}

use std::sync::Arc;

use gpui_kit::TestAppContext;
use model::{
    Command, ConnectionState, ContextName, Event, HelmChart, HelmChartInfo, HelmCommand, HelmEvent,
    HelmHistoryEntry, HelmReleaseDetails, HelmReleaseKey, HelmRow, HelmSnapshot, HelmStatus,
    HelmStorage,
};
use support::harness::{Harness, catalog};

const CONTEXT: &str = "kind-platform";

fn key(namespace: &str, name: &str) -> HelmReleaseKey {
    HelmReleaseKey {
        namespace: namespace.into(),
        name: name.into(),
        storage: HelmStorage::Secret,
    }
}

/// A deployed release whose revisions run from 1 to `latest`.
fn release(namespace: &str, name: &str, latest: u32) -> HelmRow {
    let history: Arc<[HelmHistoryEntry]> = (1..=latest)
        .rev()
        .map(|revision| HelmHistoryEntry {
            revision,
            status: if revision == latest {
                HelmStatus::Deployed
            } else {
                HelmStatus::Superseded
            },
            age: format!("{revision}d").into(),
        })
        .collect();
    HelmRow {
        key: key(namespace, name),
        revision: latest,
        status: HelmStatus::Deployed,
        age: "1d".into(),
        chart: HelmChartInfo::Known(HelmChart {
            name: name.into(),
            version: format!("1.{latest}.0").into(),
            app_version: "2.0".into(),
        }),
        history,
    }
}

fn releases(context: &str, rows: Vec<HelmRow>) -> Event {
    Event::Helm(HelmEvent::Releases(HelmSnapshot {
        context: ContextName::new(context),
        rows: Arc::from(rows),
        loaded: true,
        problem: None,
    }))
}

fn details(release: &HelmReleaseKey, revision: u32, values: &str) -> Event {
    Event::Helm(HelmEvent::Details {
        context: ContextName::new(CONTEXT),
        release: release.clone(),
        revision,
        result: Ok(HelmReleaseDetails {
            namespace: Arc::clone(&release.namespace),
            name: Arc::clone(&release.name),
            revision,
            status: HelmStatus::Deployed,
            chart: HelmChart::default(),
            first_deployed: None,
            updated: None,
            description: "Upgrade complete".into(),
            notes: "".into(),
            values: values.into(),
            manifest: "kind: Service\n".into(),
        }),
    })
}

fn load(release: &HelmReleaseKey, revision: u32) -> Command {
    Command::Helm(HelmCommand::LoadDetails {
        context: ContextName::new(CONTEXT),
        release: release.clone(),
        revision,
    })
}

/// A connected workspace with the Helm view open.
fn open_helm(cx: &mut TestAppContext) -> Harness<'_> {
    let mut h = Harness::new(cx);
    h.send(Event::Catalog(catalog(&[CONTEXT])));
    h.click("cluster-kind-platform");
    h.send(Event::Connection {
        context: ContextName::new(CONTEXT),
        state: ConnectionState::Live,
    });
    h.take_commands();
    h.click("sidebar-helm");
    assert_eq!(h.take_commands(), [Command::Helm(HelmCommand::Open)]);
    h
}

#[gpui_kit::test]
fn the_helm_view_runs_the_watch_only_while_it_shows(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    h.send(Event::Catalog(catalog(&[CONTEXT])));
    // No context yet: the entry does nothing.
    h.click("sidebar-helm");
    assert_eq!(h.take_commands(), []);

    h.click("cluster-kind-platform");
    h.take_commands();
    h.click("sidebar-helm");
    assert_eq!(h.take_commands(), [Command::Helm(HelmCommand::Open)]);
    assert!(h.bounds("pod-default/web-0").is_none());

    h.click("sidebar-Pods");
    assert_eq!(h.take_commands(), [Command::Helm(HelmCommand::Close)]);
}

#[gpui_kit::test]
fn releases_show_and_a_click_loads_details(cx: &mut TestAppContext) {
    let mut h = open_helm(cx);
    h.send(releases(
        CONTEXT,
        vec![release("data", "pg", 1), release("shop", "web", 3)],
    ));
    let pg = h.bounds("helm-data/pg").expect("pg drawn");
    let web = h.bounds("helm-shop/web").expect("web drawn");
    assert!(pg.origin.y < web.origin.y, "rows keep the engine's order");
    assert!(h.bounds("helm-details").is_none());

    let web_key = key("shop", "web");
    h.click("helm-shop/web");
    assert_eq!(h.take_commands(), [load(&web_key, 3)]);
    assert!(h.bounds("helm-details").is_some());
    assert!(h.bounds("helm-text-0").is_none(), "nothing until it loads");

    h.send(details(
        &web_key,
        3,
        "image:\n  tag: \"2.0\"\nreplicas: 3\n",
    ));
    assert!(h.bounds("helm-text-0").is_some());
    assert!(h.bounds("helm-text-2").is_some());
    assert!(h.bounds("helm-text-3").is_none());

    // Picking an older revision in the history loads it.
    h.click("helm-tab-history");
    assert!(h.bounds("helm-history-3").is_some());
    h.click("helm-history-1");
    assert_eq!(h.take_commands(), [load(&web_key, 1)]);

    // A late answer for revision 3 is ignored; revision 1's answer is shown.
    h.send(details(&web_key, 3, "stale: true\n"));
    h.click("helm-tab-values");
    assert!(h.bounds("helm-text-0").is_none());
    h.send(details(&web_key, 1, "replicas: 1\n"));
    assert!(h.bounds("helm-text-0").is_some());

    // Closing the pane clears the selection.
    h.click("helm-details-close");
    assert!(h.bounds("helm-details").is_none());
}

#[gpui_kit::test]
fn the_details_pane_follows_upgrades(cx: &mut TestAppContext) {
    let mut h = open_helm(cx);
    h.send(releases(CONTEXT, vec![release("shop", "web", 2)]));
    h.click("helm-shop/web");
    let web_key = key("shop", "web");
    assert_eq!(h.take_commands(), [load(&web_key, 2)]);

    h.send(releases(CONTEXT, vec![release("shop", "web", 3)]));
    assert_eq!(h.take_commands(), [load(&web_key, 3)]);

    // Uninstalled: the release and its pane go away.
    h.send(releases(CONTEXT, vec![]));
    assert!(h.bounds("helm-details").is_none());
    assert!(h.bounds("helm-shop/web").is_none());
}

#[gpui_kit::test]
fn other_contexts_and_problems(cx: &mut TestAppContext) {
    let mut h = open_helm(cx);
    h.send(releases("someone-else", vec![release("shop", "web", 1)]));
    assert!(h.bounds("helm-shop/web").is_none());

    h.send(Event::Helm(HelmEvent::Releases(HelmSnapshot {
        context: ContextName::new(CONTEXT),
        rows: Arc::from([release("shop", "web", 1)]),
        loaded: true,
        problem: Some("watching Helm Secrets failed: connection reset".into()),
    })));
    assert!(h.bounds("helm-shop/web").is_some(), "rows stay on screen");
    assert!(h.bounds("helm-problem").is_some());
}
