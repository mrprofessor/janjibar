//! The terminal pane in a headless window, fed by an in-memory stream:
//! the same `TerminalState` and `Terminal` element the app draws, with a
//! `StreamSource` in place of a pod's exec stream.
//!
//! The engine parses on its own thread and wakes the view from there, so
//! the tests allow parking, GPUI's switch for tests that mix real threads
//! with its deterministic executor, and the helpers wait in real time while
//! the executor draws and drains tasks.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use gpui_kit::{
    AppContext as _, Context, Entity, IntoElement, Modifiers, MouseButton, ParentElement as _,
    Pixels, Point, Render, Styled as _, TestAppContext, VisualTestContext, Window, div, point, px,
    size,
};
use term::{
    Engine, EngineOptions, StreamEvent, StreamPeer, StreamSource, Terminal, TerminalConfig,
    TerminalState, Viewport,
};

/// A view that draws one terminal, as a pane does.
struct Host {
    terminal: Entity<TerminalState>,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(Terminal::new("pane", &self.terminal))
    }
}

/// Opens a focused pane on a fresh stream.
fn open(cx: &mut TestAppContext) -> (Entity<TerminalState>, StreamPeer, &mut VisualTestContext) {
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    let (source, peer) = StreamSource::new();
    let mut source = Some(source);
    let (host, cx) = cx.add_window_view(|window, cx| {
        let engine = Engine::new(source.take().unwrap(), EngineOptions::default());
        let terminal =
            cx.new(|cx| TerminalState::new(engine, TerminalConfig::default(), window, cx));
        cx.observe(&terminal, |_, _, cx| cx.notify()).detach();
        let focus = terminal.read(cx).focus_handle().clone();
        window.focus(&focus, cx);
        Host { terminal }
    });
    let terminal = cx.update(|_, cx| host.read(cx).terminal.clone());
    (terminal, peer, cx)
}

/// Draws and drains tasks until the pane's screen satisfies `done`, and
/// returns the screen text.
fn wait_for_screen(
    cx: &mut VisualTestContext,
    terminal: &Entity<TerminalState>,
    done: impl Fn(&str) -> bool,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        let text = cx.update(|_, cx| terminal.read(cx).frame().text());
        if done(&text) {
            return text;
        }
        assert!(Instant::now() < deadline, "timed out; screen:\n{text}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Draws and drains tasks until the program side sees an event that
/// `wanted` picks, and returns it.
fn wait_for_event<T>(
    cx: &mut VisualTestContext,
    peer: &StreamPeer,
    wanted: impl Fn(&StreamEvent) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        while let Ok(event) = peer.events().try_recv() {
            if let Some(found) = wanted(&event) {
                return found;
            }
        }
        assert!(Instant::now() < deadline, "timed out waiting for an event");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The input bytes the program receives until `count` have arrived.
fn wait_for_input(cx: &mut VisualTestContext, peer: &StreamPeer, count: usize) -> Vec<u8> {
    let mut input = Vec::new();
    while input.len() < count {
        input.extend(wait_for_event(cx, peer, |event| match event {
            StreamEvent::Input(bytes) => Some(bytes.clone()),
            _ => None,
        }));
    }
    input
}

#[gpui_kit::test]
fn the_pane_draws_sequences_split_across_chunks(cx: &mut TestAppContext) {
    let (terminal, peer, cx) = open(cx);
    wait_for_event(cx, &peer, |event| match event {
        StreamEvent::Resize(viewport) => Some(*viewport),
        _ => None,
    });

    // A websocket can cut anywhere: inside a CSI, between ESC and its
    // final bytes, inside a UTF-8 character and inside an OSC.
    peer.output(b"plain \x1b[3");
    let held = wait_for_screen(cx, &terminal, |text| text.contains("plain"));
    assert_eq!(
        held.lines().next(),
        Some("plain"),
        "half a sequence is held back, never drawn"
    );
    for chunk in [
        &b"1mred\x1b"[..],
        b"[0m caf\xc3",
        b"\xa9 \x1b]2;pod sh",
        b"ell\x07\r\n$ ",
    ] {
        peer.output(chunk);
    }

    let screen = wait_for_screen(cx, &terminal, |text| text.contains('$'));
    let mut lines = screen.lines();
    assert_eq!(lines.next(), Some("plain red café"));
    assert_eq!(lines.next(), Some("$"));
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| terminal.read(cx).title().clone()),
        "pod shell"
    );
}

#[gpui_kit::test]
fn keys_typed_in_the_pane_reach_the_program(cx: &mut TestAppContext) {
    let (terminal, peer, cx) = open(cx);
    peer.output(b"$ ");
    wait_for_screen(cx, &terminal, |text| text.starts_with('$'));

    cx.simulate_keystrokes("l s enter");
    assert_eq!(wait_for_input(cx, &peer, 3), b"ls\r");
}

#[gpui_kit::test]
fn resizing_the_window_resizes_the_program_grid(cx: &mut TestAppContext) {
    let (terminal, peer, cx) = open(cx);
    // The engine starts at the default grid; the first paint measures the
    // real one and sends it to the program.
    let measured = wait_for_event(cx, &peer, |event| match event {
        StreamEvent::Resize(viewport) if *viewport != Viewport::default() => Some(*viewport),
        _ => None,
    });
    peer.output(b"$ ");
    wait_for_screen(cx, &terminal, |text| text.starts_with('$'));

    cx.simulate_resize(size(px(320.), px(200.)));
    let resized = wait_for_event(cx, &peer, |event| match event {
        StreamEvent::Resize(viewport) if *viewport != measured => Some(*viewport),
        _ => None,
    });
    assert!(
        resized.columns() < measured.columns() && resized.rows() < measured.rows(),
        "a smaller window has a smaller grid: {measured:?} -> {resized:?}"
    );
    // The engine redraws at the new size and keeps the screen.
    let deadline = Instant::now() + Duration::from_secs(10);
    while cx.update(|_, cx| terminal.read(cx).frame().viewport) != resized {
        assert!(Instant::now() < deadline, "no frame at the new size");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    let screen = cx.update(|_, cx| terminal.read(cx).frame().text());
    assert_eq!(screen.lines().next(), Some("$"));
}

/// The pane's padding around the grid, from `host_theme.rs`.
const PADDING: f32 = 6.0;

/// A point in a cell, `across` of the way from its left edge to its right,
/// in window coordinates. A drag snaps to the nearest cell edge, as in
/// Ghostty, so a selection starts near a cell's left edge and ends near
/// its right one.
fn in_cell(
    cx: &mut VisualTestContext,
    terminal: &Entity<TerminalState>,
    column: u16,
    row: u16,
    across: f32,
) -> Point<Pixels> {
    let (viewport, scale) =
        cx.update(|window, cx| (terminal.read(cx).frame().viewport, window.scale_factor()));
    let width = viewport.cell_width() as f32 / scale;
    let height = viewport.cell_height() as f32 / scale;
    point(
        px(PADDING + width * (f32::from(column) + across)),
        px(PADDING + height * (f32::from(row) + 0.5)),
    )
}

#[gpui_kit::test]
fn shift_page_up_shows_scrollback_and_shift_end_returns(cx: &mut TestAppContext) {
    let (terminal, peer, cx) = open(cx);
    cx.update(|_, cx| cx.bind_keys(term::default_key_bindings()));
    let mut output = String::new();
    for line in 1..=300 {
        write!(output, "line {line}\r\n").unwrap();
    }
    output.push_str("$ ");
    peer.output(output.as_bytes());
    wait_for_screen(cx, &terminal, |text| text.contains("line 300"));

    cx.simulate_keystrokes("shift-pageup");
    let older = wait_for_screen(cx, &terminal, |text| !text.contains("line 300"));
    assert!(older.starts_with("line "), "{older}");
    assert!(cx.update(|_, cx| terminal.read(cx).is_scrolled()));

    cx.simulate_keystrokes("shift-end");
    wait_for_screen(cx, &terminal, |text| text.contains("line 300"));
    assert!(!cx.update(|_, cx| terminal.read(cx).is_scrolled()));
}

#[gpui_kit::test]
fn a_drag_selects_text_and_cmd_c_copies_it(cx: &mut TestAppContext) {
    let (terminal, peer, cx) = open(cx);
    cx.update(|_, cx| cx.bind_keys(term::default_key_bindings()));
    peer.output(b"hello world\r\n$ ");
    wait_for_screen(cx, &terminal, |text| text.starts_with("hello world"));

    let (start, end) = (
        in_cell(cx, &terminal, 0, 0, 0.1),
        in_cell(cx, &terminal, 4, 0, 0.9),
    );
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| terminal.read(cx).selected_text()),
        Some("hello".to_owned())
    );

    cx.simulate_keystrokes("cmd-c");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        if copied.as_deref() == Some("hello") {
            break;
        }
        assert!(Instant::now() < deadline, "clipboard holds {copied:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}
