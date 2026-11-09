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
use model::resource::{Details, InspectTarget, Kind, ObjectKey};
use model::{Command, ConnectionState, ContextName, Event, Health, SubscriptionId};
use support::harness::{Harness, catalog, pod, snapshot};
use ui::theme;

fn subscribe(id: u64, context: &str, kind: Kind) -> Command {
    Command::Subscribe {
        id: SubscriptionId(id),
        context: ContextName::new(context),
        resource: kind.resource(),
        namespace: None,
    }
}

/// Selects `context` and drops the commands that sends.
fn connect(h: &mut Harness<'_>, context: &'static str, selector: &'static str) {
    h.send(Event::Catalog(catalog(&[context])));
    h.click(selector);
    h.send(Event::Connection {
        context: ContextName::new(context),
        state: ConnectionState::Live,
    });
    h.take_commands();
}

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
        [
            Command::SelectContext(ContextName::new("prod-eu")),
            subscribe(1, "prod-eu", Kind::Pod),
        ]
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
    let web0 = h.bounds("row-default/web-0").expect("web-0 drawn");
    let web1 = h.bounds("row-default/web-1").expect("web-1 drawn");
    let dns = h
        .bounds("row-kube-system/coredns-7db6d8ff4d-x2bqv")
        .expect("coredns drawn");
    assert!(web0.origin.y < web1.origin.y && web1.origin.y < dns.origin.y);

    // A snapshot replaces the rows: a deleted pod disappears.
    h.send(snapshot("kind-platform", vec![pod("default", "web-0")]));
    assert!(h.bounds("row-default/web-0").is_some());
    assert!(h.bounds("row-default/web-1").is_none());

    // Rows for a context that is not selected are ignored.
    h.send(snapshot("other", vec![pod("default", "intruder")]));
    assert!(h.bounds("row-default/intruder").is_none());
    assert!(h.bounds("row-default/web-0").is_some());
}

#[gpui_kit::test]
fn clicking_a_kind_moves_the_subscription(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    connect(&mut h, "kind-platform", "cluster-kind-platform");

    h.click("kind-Deployment");
    assert_eq!(
        h.take_commands(),
        [
            Command::Unsubscribe(SubscriptionId(1)),
            subscribe(2, "kind-platform", Kind::Deployment),
        ]
    );

    // Pod rows that arrive late, from the warm watch, do not show.
    h.send(snapshot("kind-platform", vec![pod("default", "web-0")]));
    assert!(h.bounds("row-default/web-0").is_none());
}

fn details(name: &str) -> Details {
    Details {
        kind: Kind::Pod,
        key: ObjectKey::namespaced("default", name),
        status: "Running".into(),
        health: Health::Good,
        badges: vec!["1/1 ready".into()],
        fields: Vec::new(),
        sections: Vec::new(),
        containers: Vec::new(),
        labels: Vec::new(),
        owners: Vec::new(),
    }
}

#[gpui_kit::test]
fn selecting_a_row_opens_the_inspector(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    connect(&mut h, "kind-platform", "cluster-kind-platform");
    h.send(snapshot(
        "kind-platform",
        vec![pod("default", "web-0"), pod("default", "web-1")],
    ));
    assert!(h.bounds("inspector").is_none());

    h.click("row-default/web-1");
    let target = InspectTarget {
        context: ContextName::new("kind-platform"),
        kind: Kind::Pod,
        namespace: None,
        key: ObjectKey::namespaced("default", "web-1"),
    };
    assert_eq!(h.take_commands(), [Command::Inspect(Some(target.clone()))]);
    let inspector = h.bounds("inspector").expect("inspector opens at once");
    assert!(h.bounds("inspector-name").is_none(), "still loading");

    // Details for another object are ignored; ours fill the pane.
    let mut stray = target.clone();
    stray.key = ObjectKey::namespaced("default", "web-0");
    h.send(Event::Details {
        target: stray,
        details: Some(Box::new(details("web-0"))),
    });
    assert!(h.bounds("inspector-name").is_none());
    h.send(Event::Details {
        target: target.clone(),
        details: Some(Box::new(details("web-1"))),
    });
    let name = h.bounds("inspector-name").expect("name drawn");
    assert!(
        name.origin.x >= inspector.origin.x,
        "name sits in the inspector"
    );
    assert!(h.bounds("action-Logs").is_some(), "pods offer logs");

    h.click("inspector-close");
    assert_eq!(h.take_commands(), [Command::Inspect(None)]);
    assert!(h.bounds("inspector").is_none());
}

#[gpui_kit::test]
fn the_inspector_logs_button_opens_the_log_pane(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    connect(&mut h, "kind-platform", "cluster-kind-platform");
    h.send(snapshot("kind-platform", vec![pod("default", "web-1")]));
    h.click("row-default/web-1");
    let target = InspectTarget {
        context: ContextName::new("kind-platform"),
        kind: Kind::Pod,
        namespace: None,
        key: ObjectKey::namespaced("default", "web-1"),
    };
    h.send(Event::Details {
        target,
        details: Some(Box::new(details("web-1"))),
    });
    h.take_commands();
    assert!(h.bounds("log-pane").is_none());

    h.click("action-Logs");
    let commands = h.take_commands();
    assert!(
        matches!(commands.as_slice(), [Command::OpenLogs(request)]
        if request.target == model::LogTarget::Pod {
            namespace: "default".into(),
            name: "web-1".into(),
        }),
        "got {commands:?}"
    );
    assert!(h.bounds("log-pane").is_some());
}
