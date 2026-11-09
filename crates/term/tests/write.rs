//! Input to a source that takes it a little at a time: the engine resumes
//! a short write where it stopped, in order, when the source says it has
//! room, and keeps parsing output meanwhile.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use term::{
    ByteHandle, ByteSink, ByteSource, Engine, EngineOptions, ExitStatus, FrameSink, FrameSource,
    StartOptions, TerminalInput, Viewport,
};

/// What the slow program has read, and the sink to answer on.
#[derive(Default)]
struct Program {
    read: Vec<u8>,
    /// Write calls refused because the program was not reading.
    refused: usize,
    sink: Option<ByteSink>,
}

/// A program that reads at most `SLICE` bytes per write, and none until
/// its reader thread drains them.
struct Slow {
    program: Arc<Mutex<Program>>,
}

struct SlowHandle {
    program: Arc<Mutex<Program>>,
    room: Arc<Mutex<usize>>,
}

const SLICE: usize = 1000;

impl ByteSource for Slow {
    fn start(self: Box<Self>, sink: ByteSink, _: Viewport) -> std::io::Result<Box<dyn ByteHandle>> {
        self.program.lock().unwrap().sink = Some(sink.clone());
        let room = Arc::new(Mutex::new(SLICE));
        // The program's reader: frees a slice of room every 2 ms, says so,
        // and writes some output meanwhile.
        std::thread::spawn({
            let room = Arc::clone(&room);
            move || {
                for tick in 0.. {
                    std::thread::sleep(Duration::from_millis(2));
                    *room.lock().unwrap() = SLICE;
                    sink.writable();
                    if !sink.write(format!("tick {tick}\r\n").as_bytes()) {
                        break;
                    }
                }
            }
        });
        Ok(Box::new(SlowHandle {
            program: self.program,
            room,
        }))
    }
}

impl ByteHandle for SlowHandle {
    fn write(&self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut room = self.room.lock().unwrap();
        let take = bytes.len().min(*room);
        *room -= take;
        let mut program = self.program.lock().unwrap();
        program.read.extend_from_slice(&bytes[..take]);
        if take == 0 {
            program.refused += 1;
        }
        Ok(take)
    }

    fn resize(&self, _: Viewport) -> std::io::Result<()> {
        Ok(())
    }

    fn try_wait(&self) -> std::io::Result<Option<ExitStatus>> {
        Ok(None)
    }

    fn close(&self) {}
}

#[test]
fn a_short_write_resumes_in_order_when_the_source_has_room() {
    let program = Arc::new(Mutex::new(Program::default()));
    let (sink, _wake) = FrameSink::new();
    let handle = Box::new(Engine::new(
        Slow {
            program: Arc::clone(&program),
        },
        EngineOptions::default(),
    ))
    .start(sink.clone(), StartOptions::default())
    .unwrap();

    let paste: String = (b'a'..=b'z').cycle().take(20_000).map(char::from).collect();
    handle.input(TerminalInput::Paste(paste.clone())).unwrap();
    handle.input(TerminalInput::Text("after".into())).unwrap();
    let expected = format!("{paste}after");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let read = program.lock().unwrap().read.clone();
        if read.len() >= expected.len() {
            assert_eq!(String::from_utf8(read).unwrap(), expected, "in order");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "only {} bytes arrived",
            read.len()
        );
        // Output keeps being parsed while the paste waits.
        if let Some(snapshot) = sink.take() {
            handle.request_frame();
            let _ = snapshot;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let refused = program.lock().unwrap().refused;
    // About one refused write per slice: the engine waits for the source to
    // say it has room instead of retrying in a loop.
    assert!(
        refused <= 2 * 20_000 / SLICE + 10,
        "{refused} refused writes"
    );
    let shown = handle_frame_text(&sink, handle.as_ref());
    assert!(
        shown.contains("tick"),
        "output was parsed meanwhile:\n{shown}"
    );
    handle.close();
}

fn handle_frame_text(sink: &FrameSink, handle: &dyn term::FrameHandle) -> String {
    handle.set_visible(false);
    handle.set_visible(true);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(snapshot) = sink.take() {
            return snapshot.frame.text();
        }
        assert!(Instant::now() < deadline, "no frame");
        handle.request_frame();
        std::thread::sleep(Duration::from_millis(5));
    }
}
