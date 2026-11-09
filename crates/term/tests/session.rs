//! A terminal fed by a shell session, as the engine serves `exec` into a
//! pod or a node shell. The test plays the engine's side of the channels.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use model::{ShellError, ShellEvent, ShellExit, ShellInput, ShellSession};
use term::input::KeyInput;
use term::{
    Engine, EngineOptions, FrameHandle, FrameSink, FrameSource, SessionSource, StartOptions,
    TerminalInput, TerminalSnapshot, TerminalStatus, Viewport,
};
use tokio::sync::mpsc;

/// The engine's ends of a session.
struct Remote {
    input: mpsc::UnboundedReceiver<ShellInput>,
    events: mpsc::Sender<ShellEvent>,
}

impl Remote {
    fn send(&self, event: ShellEvent) {
        self.events.blocking_send(event).unwrap();
    }

    /// The next input, waiting up to ten seconds.
    fn next_input(&mut self) -> Option<ShellInput> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.input.try_recv() {
                Ok(input) => return Some(input),
                Err(mpsc::error::TryRecvError::Disconnected) => return None,
                Err(mpsc::error::TryRecvError::Empty) => {
                    assert!(Instant::now() < deadline, "no input");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
}

fn started(viewport: Viewport) -> (Remote, FrameSink, Box<dyn FrameHandle>) {
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (events_tx, events_rx) = mpsc::channel(4);
    let session = ShellSession {
        input: input_tx,
        events: events_rx,
    };
    let (sink, _wake) = FrameSink::new();
    let handle = Box::new(Engine::new(
        SessionSource::new(session),
        EngineOptions::default(),
    ))
    .start(
        sink.clone(),
        StartOptions::default().with_viewport(viewport),
    )
    .unwrap();
    let remote = Remote {
        input: input_rx,
        events: events_tx,
    };
    (remote, sink, handle)
}

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

#[test]
fn a_session_shows_progress_and_output_takes_keys_and_resizes_and_exits() {
    let viewport = Viewport::new(60, 10, 8, 16).unwrap();
    let (mut remote, sink, handle) = started(viewport);
    assert_eq!(
        remote.next_input(),
        Some(ShellInput::Resize {
            columns: 60,
            rows: 10
        }),
        "the shell learns the grid first"
    );

    remote.send(ShellEvent::Progress("Starting a node shell".to_owned()));
    remote.send(ShellEvent::Output(b"root@node:/# ".to_vec()));
    let shown = wait_for(&sink, handle.as_ref(), |s| s.frame.text().contains("root@"));
    let text = shown.frame.text();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("Starting a node shell"));
    assert_eq!(lines.next(), Some("root@node:/#"));
    assert_eq!(shown.status, TerminalStatus::Live);

    handle
        .input(TerminalInput::Key(KeyInput::new("l").with_text("l")))
        .unwrap();
    assert_eq!(remote.next_input(), Some(ShellInput::Bytes(b"l".to_vec())));

    handle.resize(Viewport::new(40, 8, 8, 16).unwrap());
    assert_eq!(
        remote.next_input(),
        Some(ShellInput::Resize {
            columns: 40,
            rows: 8
        })
    );

    remote.send(ShellEvent::Exited(ShellExit {
        code: Some(3),
        message: None,
    }));
    let ended = wait_for(&sink, handle.as_ref(), |s| {
        matches!(s.status, TerminalStatus::Exited(_))
    });
    let TerminalStatus::Exited(status) = &ended.status else {
        unreachable!()
    };
    assert_eq!(status.code(), Some(3));
}

#[test]
fn a_failure_is_written_into_the_terminal_and_ends_it() {
    let (mut remote, sink, handle) = started(Viewport::new(80, 6, 8, 16).unwrap());
    remote.next_input();
    remote.send(ShellEvent::Failed(ShellError::Forbidden(
        "pods \"web\" is forbidden".to_owned(),
    )));
    let ended = wait_for(&sink, handle.as_ref(), |s| {
        matches!(s.status, TerminalStatus::Exited(_))
    });
    assert!(
        ended
            .frame
            .text()
            .starts_with("not allowed: pods \"web\" is forbidden"),
        "{}",
        ended.frame.text()
    );
}

#[test]
fn closing_the_terminal_ends_the_shell() {
    let (mut remote, _sink, handle) = started(Viewport::new(80, 6, 8, 16).unwrap());
    remote.next_input();
    handle.close();
    assert_eq!(remote.next_input(), None, "the engine sees its input close");
}

#[test]
fn a_flood_bigger_than_every_queue_arrives_whole() {
    let (mut remote, sink, handle) = started(Viewport::new(80, 6, 8, 16).unwrap());
    remote.next_input();
    // More than six megabytes, past the session's four-event queue and the
    // engine's one-megabyte read queue, sent from another thread as the
    // engine's websocket reader would. The marker after it still arrives.
    let events = remote.events.clone();
    let writer = std::thread::spawn(move || {
        for _ in 0..200 {
            events
                .blocking_send(ShellEvent::Output(vec![b'x'; 32 * 1024]))
                .unwrap();
        }
        events
            .blocking_send(ShellEvent::Output(b"\r\nDONE".to_vec()))
            .unwrap();
    });
    wait_for(&sink, handle.as_ref(), |s| s.frame.text().contains("DONE"));
    writer.join().unwrap();
}
