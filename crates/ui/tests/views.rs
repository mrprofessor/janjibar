//! View tests: the workspace in a headless GPUI window with a fake engine.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

mod support {
    pub mod harness;
}

use gpui_kit::{TestAppContext, px};
use model::{Command, ConnectionState, ContextName, Event};
use support::harness::{Harness, catalog, pod, snapshot};
use ui::theme;

#[gpui_kit::test]
fn sidebar_lists_every_context(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    assert!(h.bounds("cluster-kind-platform").is_none());

    h.send(Event::Catalog(catalog(&["kind-platform", "prod-eu"])));

    let first = h
        .bounds("cluster-kind-platform")
        .expect("first context drawn");
    let second = h.bounds("cluster-prod-eu").expect("second context drawn");
    assert!(second.origin.y > first.origin.y, "contexts keep file order");
    assert!(
        first.right() <= px(theme::SIDEBAR_WIDTH),
        "rows sit in the sidebar"
    );
}

#[gpui_kit::test]
fn clicking_a_context_connects_to_it(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    h.send(Event::Catalog(catalog(&["kind-platform", "prod-eu"])));

    h.click("cluster-prod-eu");
    assert_eq!(
        h.take_commands(),
        [Command::SelectContext(ContextName::new("prod-eu"))]
    );

    // Clicking the selected context again does not reconnect.
    h.click("cluster-prod-eu");
    assert_eq!(h.take_commands(), []);
}

#[gpui_kit::test]
fn pod_table_shows_rows_from_a_snapshot(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    h.send(Event::Catalog(catalog(&["kind-platform"])));
    h.click("cluster-kind-platform");
    h.send(Event::Connection {
        context: ContextName::new("kind-platform"),
        state: ConnectionState::Live,
    });

    h.send(snapshot(
        "kind-platform",
        vec![
            pod("default", "web-0"),
            pod("default", "web-1"),
            pod("kube-system", "coredns-7db6d8ff4d-x2bqv"),
        ],
    ));
    let web0 = h.bounds("pod-default/web-0").expect("web-0 drawn");
    let web1 = h.bounds("pod-default/web-1").expect("web-1 drawn");
    let dns = h
        .bounds("pod-kube-system/coredns-7db6d8ff4d-x2bqv")
        .expect("coredns drawn");
    assert!(web0.origin.y < web1.origin.y && web1.origin.y < dns.origin.y);

    // A snapshot replaces the rows: a deleted pod disappears.
    h.send(snapshot("kind-platform", vec![pod("default", "web-0")]));
    assert!(h.bounds("pod-default/web-0").is_some());
    assert!(h.bounds("pod-default/web-1").is_none());

    // Rows for a context that is not selected are ignored.
    h.send(snapshot("other", vec![pod("default", "intruder")]));
    assert!(h.bounds("pod-default/intruder").is_none());
    assert!(h.bounds("pod-default/web-0").is_some());
}
