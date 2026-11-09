//! View tests for the log pane: opening it from the pod table, showing
//! lines from the engine, and closing it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

mod support {
    pub mod harness;
}

use std::sync::Arc;

use gpui_kit::TestAppContext;
use model::{
    Clock, Command, ConnectionState, Containers, ContextName, Event, LineKind, LogChunk, LogLine,
    LogOptions, LogRequest, LogSource, LogState, LogTabId, LogTarget,
};
use support::harness::{Harness, catalog, pod, snapshot};

/// A workspace connected to `kind-platform` with two pods on screen.
fn connected(cx: &mut TestAppContext) -> Harness<'_> {
    let mut h = Harness::new(cx);
    h.send(Event::Catalog(catalog(&["kind-platform", "prod-eu"])));
    h.click("cluster-kind-platform");
    h.send(Event::Connection {
        context: ContextName::new("kind-platform"),
        state: ConnectionState::Live,
    });
    h.send(snapshot(
        "kind-platform",
        vec![pod("faults", "crashloop-x2kqp"), pod("shop", "web-0")],
    ));
    h.take_commands();
    h
}

fn line(second: i64, text: &str) -> LogLine {
    LogLine {
        time: jiff::Timestamp::from_second(1_800_000_000 + second).unwrap(),
        clock: Clock::default(),
        source: Arc::new(LogSource {
            pod: "crashloop-x2kqp".into(),
            container: "app".into(),
        }),
        kind: LineKind::Text(None),
        text: text.into(),
    }
}

fn open_request(tab: u64) -> Command {
    Command::OpenLogs(LogRequest {
        tab: LogTabId::new(tab),
        context: ContextName::new("kind-platform"),
        target: LogTarget::Pod {
            namespace: "faults".into(),
            name: "crashloop-x2kqp".into(),
        },
        containers: Containers::All,
        options: LogOptions::default(),
    })
}

#[gpui_kit::test]
fn the_logs_button_opens_a_pane_for_the_selected_pod(cx: &mut TestAppContext) {
    let mut h = connected(cx);
    assert!(h.bounds("open-logs").is_none(), "no pod selected yet");

    h.click("pod-faults/crashloop-x2kqp");
    h.click("open-logs");
    assert_eq!(h.take_commands(), [open_request(1)]);
    let pane = h.bounds("log-pane").expect("the pane is drawn");
    let table = h.bounds("pod-shop/web-0").expect("the pods stay");
    assert!(pane.origin.y > table.origin.y, "logs sit under the pods");

    // Lines from the engine are drawn in order.
    h.send(Event::LogState {
        tab: LogTabId::new(1),
        state: LogState::Streaming {
            pods: 1,
            containers: 1,
        },
    });
    h.send(Event::Logs(LogChunk {
        tab: LogTabId::new(1),
        lines: vec![
            line(1, "starting up..."),
            line(6, "fatal: connection refused"),
        ],
    }));
    let first = h.bounds("log-line-0").expect("first line drawn");
    let second = h.bounds("log-line-1").expect("second line drawn");
    assert!(first.origin.y < second.origin.y);

    // Chunks for another tab are ignored.
    h.send(Event::Logs(LogChunk {
        tab: LogTabId::new(99),
        lines: vec![line(7, "not mine")],
    }));
    assert!(h.bounds("log-line-2").is_none());

    // Opening the same pod again does nothing.
    h.click("open-logs");
    assert_eq!(h.take_commands(), []);

    h.click("log-close");
    assert_eq!(h.take_commands(), [Command::CloseLogs(LogTabId::new(1))]);
    assert!(h.bounds("log-pane").is_none());
}

#[gpui_kit::test]
fn l_opens_logs_from_the_pod_table(cx: &mut TestAppContext) {
    let mut h = connected(cx);
    h.click("pod-faults/crashloop-x2kqp");
    h.cx.simulate_keystrokes("l");
    assert_eq!(h.take_commands(), [open_request(1)]);
    assert!(h.bounds("log-pane").is_some());
}

#[gpui_kit::test]
fn switching_context_closes_the_pane(cx: &mut TestAppContext) {
    let mut h = connected(cx);
    h.click("pod-faults/crashloop-x2kqp");
    h.click("open-logs");
    h.take_commands();

    h.click("cluster-prod-eu");
    assert_eq!(
        h.take_commands(),
        [
            Command::CloseLogs(LogTabId::new(1)),
            Command::SelectContext(ContextName::new("prod-eu")),
        ]
    );
    assert!(h.bounds("log-pane").is_none());
}

#[gpui_kit::test]
fn toggles_stay_in_the_view(cx: &mut TestAppContext) {
    let mut h = connected(cx);
    h.click("pod-faults/crashloop-x2kqp");
    h.click("open-logs");
    h.take_commands();
    h.send(Event::Logs(LogChunk {
        tab: LogTabId::new(1),
        lines: vec![line(1, "one")],
    }));
    for toggle in ["log-follow", "log-wrap", "log-time"] {
        h.click(toggle);
        assert!(h.bounds("log-line-0").is_some(), "{toggle} keeps the line");
    }
    assert_eq!(h.take_commands(), [], "toggles do not reach the engine");
}
