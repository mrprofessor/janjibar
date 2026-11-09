//! View tests for the port-forward manager and its sidebar count.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

mod support {
    #[allow(dead_code, reason = "the pod helpers serve the other view tests")]
    pub mod harness;
}

use std::sync::Arc;

use gpui_kit::TestAppContext;
use model::{Command, Event, ForwardId, ForwardRow, ForwardSpec, ForwardState, ForwardTarget};
use support::harness::{Harness, catalog};

fn forward(id: u64, state: ForwardState) -> ForwardRow {
    ForwardRow {
        id: ForwardId::new(id),
        spec: ForwardSpec {
            context: "kind-platform".into(),
            namespace: "gateway".into(),
            target: ForwardTarget::Service("gateway".into()),
            port: 80,
            local_port: 8080 + u16::try_from(id).unwrap(),
        },
        local_addr: None,
        state,
        active: 0,
        total: 0,
        last_pod: None,
        last_error: None,
    }
}

fn forwards(rows: Vec<ForwardRow>) -> Event {
    Event::Forwards(Arc::from(rows))
}

#[gpui_kit::test]
fn the_sidebar_counts_running_forwards(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    h.send(Event::Catalog(catalog(&["kind-platform"])));
    assert!(h.bounds("sidebar-forwards").is_some());
    assert!(
        h.bounds("sidebar-forwards-count").is_none(),
        "no count without forwards"
    );

    h.send(forwards(vec![
        forward(1, ForwardState::Listening),
        forward(2, ForwardState::Waiting),
    ]));
    assert!(h.bounds("sidebar-forwards-count").is_some());
}

#[gpui_kit::test]
fn the_pane_opens_from_the_sidebar_and_lists_forwards(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    h.send(Event::Catalog(catalog(&["kind-platform"])));
    h.send(forwards(vec![
        forward(1, ForwardState::Listening),
        forward(
            2,
            ForwardState::Failed("port 8082 is already in use".into()),
        ),
    ]));
    assert!(h.bounds("forwards-pane").is_none(), "closed by default");

    h.click("sidebar-forwards");
    let pane = h.bounds("forwards-pane").expect("pane drawn");
    let first = h.bounds("forward-1").expect("first forward drawn");
    let second = h.bounds("forward-2").expect("second forward drawn");
    assert!(pane.contains(&first.center()));
    assert!(second.origin.y > first.origin.y, "rows keep engine order");

    h.click("sidebar-forwards");
    assert!(
        h.bounds("forwards-pane").is_none(),
        "clicking again closes it"
    );
}

#[gpui_kit::test]
fn stopping_a_forward_sends_its_id(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    h.send(Event::Catalog(catalog(&["kind-platform"])));
    h.send(forwards(vec![forward(7, ForwardState::Listening)]));
    h.click("sidebar-forwards");

    h.click("forward-stop-7");
    assert_eq!(h.take_commands(), [Command::StopForward(ForwardId::new(7))]);
}

#[gpui_kit::test]
fn typing_a_forward_starts_it_in_the_selected_cluster(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    h.send(Event::Catalog(catalog(&["kind-platform"])));
    h.click("cluster-kind-platform");
    h.click("sidebar-forwards");
    h.take_commands();

    h.click("forward-input");
    h.cx.simulate_input("gateway/svc/gateway 8080:80");
    h.cx.simulate_keystrokes("enter");
    h.cx.run_until_parked();

    assert_eq!(
        h.take_commands(),
        [Command::StartForward(ForwardSpec {
            context: "kind-platform".into(),
            namespace: "gateway".into(),
            target: ForwardTarget::Service("gateway".into()),
            port: 80,
            local_port: 8080,
        })]
    );
}

#[gpui_kit::test]
fn bad_input_starts_nothing(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    h.send(Event::Catalog(catalog(&["kind-platform"])));
    h.click("cluster-kind-platform");
    h.click("sidebar-forwards");
    h.take_commands();

    h.click("forward-input");
    // Deployments cannot be forwarded to; only pods and services.
    h.cx.simulate_input("shop/deploy/web 80");
    h.cx.simulate_keystrokes("enter");
    h.cx.run_until_parked();
    assert_eq!(h.take_commands(), []);
}
