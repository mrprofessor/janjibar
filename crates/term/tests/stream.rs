//! A terminal driven by the application's own byte stream, as a remote
//! session or an application's own pty would drive it. The engine runs on
//! its own thread, so the test reads its frames from the sink, as the
//! pty test does.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use term::frame::{Color, TerminalRow};
use term::input::KeyInput;
use term::{
    Engine, EngineOptions, ExitStatus, FrameHandle, FrameSink, FrameSource, StartOptions,
    StreamEvent, StreamPeer, StreamSource, TerminalInput, TerminalSnapshot, TerminalStatus,
    Viewport,
};

/// Waits until a snapshot satisfies `done`, granting a frame after each.
fn wait_for(
    sink: &FrameSink,
    handle: &dyn FrameHandle,
    done: impl Fn(&TerminalSnapshot) -> bool,
) -> Arc<TerminalSnapshot> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = None;
    while Instant::now() < deadline {
        if let Some(snapshot) = sink.take() {
            handle.request_frame();
            if done(&snapshot) {
                return snapshot;
            }
            last = Some(snapshot);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "timed out; last frame:\n{}",
        last.map_or_else(String::new, |s| s.frame.text())
    );
}

/// Waits until the peer has seen `event`.
fn wait_event(peer: &StreamPeer, event: &StreamEvent) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        while let Ok(seen) = peer.events().try_recv() {
            if &seen == event {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {event:?}");
}

#[test]
fn a_custom_stream_renders_output_gets_input_and_sizes_and_reports_exit() {
    let (source, peer) = StreamSource::new();
    // Output written before the terminal starts is kept.
    peer.output(b"remote$ ");
    let (sink, _wake) = FrameSink::new();
    let start = Viewport::new(60, 10, 8, 16).unwrap();
    let handle = Box::new(Engine::new(source, EngineOptions::default()))
        .start(sink.clone(), StartOptions::default().with_viewport(start))
        .unwrap();
    wait_event(&peer, &StreamEvent::Resize(start));

    peer.output(b"ls\r\nnotes.txt\r\n");
    let shown = wait_for(&sink, handle.as_ref(), |s| {
        s.frame.text().contains("notes.txt")
    });
    assert!(shown.frame.text().starts_with("remote$ ls\nnotes.txt"));
    assert_eq!(shown.status, TerminalStatus::Live);

    // Keys arrive encoded for the program.
    handle
        .input(TerminalInput::Key(KeyInput::new("a").with_text("a")))
        .unwrap();
    wait_event(&peer, &StreamEvent::Input(b"a".to_vec()));

    let bigger = Viewport::new(100, 30, 8, 16).unwrap();
    handle.resize(bigger);
    wait_event(&peer, &StreamEvent::Resize(bigger));
    let resized = wait_for(&sink, handle.as_ref(), |s| s.frame.viewport == bigger);
    assert_eq!(resized.frame.rows.len(), 30);

    peer.exit(ExitStatus::new(Some(3), None::<String>));
    let exited = wait_for(&sink, handle.as_ref(), |s| {
        matches!(s.status, TerminalStatus::Exited(_))
    });
    let TerminalStatus::Exited(status) = &exited.status else {
        unreachable!()
    };
    assert_eq!(status.code(), Some(3));

    handle.close();
    wait_event(&peer, &StreamEvent::Closed);
}

/// The style of the run in `row` that holds `text`, ignoring the blanks
/// that pad the last run to the row's end.
fn run_for<'a>(row: &'a TerminalRow, text: &str) -> &'a term::frame::Style {
    &row.runs
        .iter()
        .find(|run| row.text[run.text.clone()].trim_end() == text)
        .unwrap_or_else(|| panic!("no run for {text:?} in {:?}", row.runs))
        .style
}

#[test]
fn sequences_split_across_chunks_draw_as_if_they_came_whole() {
    let (peer, sink, handle) = started(Viewport::new(40, 5, 8, 16).unwrap());

    // A websocket can cut anywhere: inside a CSI, between ESC and its
    // final bytes, inside a UTF-8 character and inside an OSC.
    peer.output(b"plain \x1b[3");
    let held = wait_for(&sink, handle.as_ref(), |s| s.frame.text().contains("plain"));
    assert_eq!(
        held.frame.text().lines().next(),
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

    let shown = wait_for(&sink, handle.as_ref(), |s| {
        s.frame.text().contains('$') && s.title == "pod shell"
    });
    let text = shown.frame.text();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("plain red café"));
    assert_eq!(lines.next(), Some("$"));
    let row = &shown.frame.rows[0];
    assert_eq!(run_for(row, "red").foreground, Color::Indexed(1));
    assert_eq!(run_for(row, " café").foreground, Color::Default);
    assert_eq!(shown.frame.cursor.row, 1);
    assert_eq!(shown.frame.cursor.column, 2);
    handle.close();
}

/// Starts an engine on a stream, takes its first frame and grants credit
/// for the next, as a painted terminal does.
fn started(viewport: Viewport) -> (StreamPeer, FrameSink, Box<dyn FrameHandle>) {
    let (source, peer) = StreamSource::new();
    let (sink, _wake) = FrameSink::new();
    let handle = Box::new(Engine::new(source, EngineOptions::default()))
        .start(
            sink.clone(),
            StartOptions::default().with_viewport(viewport),
        )
        .unwrap();
    wait_for(&sink, handle.as_ref(), |_| true);
    (peer, sink, handle)
}

/// Fails if the engine publishes a frame within `window`.
fn assert_no_frame(sink: &FrameSink, window: Duration, why: &str) {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        assert!(sink.take().is_none(), "{why}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Program output: `lines` numbered lines, the last one marked.
fn flood(lines: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for line in 0..lines {
        bytes.extend_from_slice(format!("line {line} the quick brown fox\r\n").as_bytes());
    }
    bytes.extend_from_slice(b"LAST");
    bytes
}

#[test]
fn an_idle_terminal_builds_no_frames() {
    let (_peer, sink, handle) = started(Viewport::new(80, 24, 8, 16).unwrap());
    assert_no_frame(&sink, Duration::from_millis(400), "nothing changed");
    handle.close();
}

#[test]
fn a_hidden_terminal_drains_output_without_frames_and_shows_it_when_revealed() {
    let (peer, sink, handle) = started(Viewport::new(80, 24, 8, 16).unwrap());
    handle.set_visible(false);
    peer.output(&flood(50_000));
    assert_no_frame(
        &sink,
        Duration::from_millis(400),
        "a hidden terminal draws nothing",
    );
    handle.set_visible(true);
    let shown = wait_for(&sink, handle.as_ref(), |s| s.frame.text().contains("LAST"));
    assert!(
        shown.frame.text().contains("line 49999"),
        "the output was parsed"
    );
    handle.close();
}

#[test]
fn a_terminal_that_is_not_painted_builds_no_more_frames() {
    // The first frame is taken but never painted, so no credit comes back:
    // what a terminal on a page that is not shown, or in a window that is
    // minimized, sees.
    let (source, peer) = StreamSource::new();
    let (sink, _wake) = FrameSink::new();
    let handle = Box::new(Engine::new(source, EngineOptions::default()))
        .start(
            sink.clone(),
            StartOptions::default().with_viewport(Viewport::new(80, 24, 8, 16).unwrap()),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while sink.take().is_none() {
        assert!(Instant::now() < deadline, "no first frame");
        std::thread::sleep(Duration::from_millis(5));
    }
    peer.output(&flood(20_000));
    assert_no_frame(&sink, Duration::from_millis(400), "no credit, no frame");
    // A paint grants one frame, and it is current.
    handle.request_frame();
    let shown = wait_for(&sink, handle.as_ref(), |s| s.frame.text().contains("LAST"));
    assert!(shown.frame.text().contains("line 19999"));
    handle.close();
}
