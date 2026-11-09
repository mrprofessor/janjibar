//! A byte source for a shell the engine runs: `exec` into a pod, or a
//! node shell.
//!
//! The engine and the terminal share a [`ShellSession`]: keys and grid
//! sizes go out as [`ShellInput`], output and the exit come back as
//! [`ShellEvent`]s. One thread per terminal moves the events into the
//! terminal and waits while the parser is behind, so a pod that floods its
//! output is held back by the websocket instead of by memory.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use model::{ShellEvent, ShellInput, ShellSession};
use tokio::sync::mpsc;

use crate::engine::{ByteHandle, ByteSink, ByteSource, ExitStatus};
use crate::frame::Viewport;

/// A [`ByteSource`] fed by a [`ShellSession`].
#[derive(Debug)]
pub struct SessionSource {
    session: ShellSession,
}

impl SessionSource {
    /// A source for `session`. The shell ends when the terminal closes.
    #[must_use]
    pub fn new(session: ShellSession) -> Self {
        Self { session }
    }
}

#[derive(Debug, Default)]
struct Shared {
    /// `None` once the terminal closed, which tells the engine to stop.
    input: Mutex<Option<mpsc::UnboundedSender<ShellInput>>>,
    exit: Mutex<Option<ExitStatus>>,
}

impl Shared {
    /// Sends input to the shell. Input after the shell ended is dropped:
    /// the exit is already on its way to the terminal.
    fn send(&self, input: ShellInput) {
        if let Some(sender) = self
            .input
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            // An error means the shell ended; see above.
            let _ = sender.send(input);
        }
    }
}

impl ByteSource for SessionSource {
    fn start(
        self: Box<Self>,
        sink: ByteSink,
        viewport: Viewport,
    ) -> io::Result<Box<dyn ByteHandle>> {
        let ShellSession { input, events } = self.session;
        let shared = Arc::new(Shared {
            input: Mutex::new(Some(input)),
            exit: Mutex::new(None),
        });
        shared.send(resize(viewport));
        let pump = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("term-shell".to_owned())
            .spawn(move || run(events, &sink, &pump))?;
        Ok(Box::new(Handle { shared }))
    }
}

fn resize(viewport: Viewport) -> ShellInput {
    ShellInput::Resize {
        columns: viewport.columns(),
        rows: viewport.rows(),
    }
}

/// Moves events into the terminal until the shell ends or the terminal
/// closes.
fn run(mut events: mpsc::Receiver<ShellEvent>, sink: &ByteSink, shared: &Shared) {
    let exit = loop {
        // A plain thread, never inside a runtime, so blocking is fine.
        let Some(event) = events.blocking_recv() else {
            notice(sink, Tone::Error, "The connection to the shell closed.");
            break ExitStatus::new(None, None::<String>);
        };
        match event {
            ShellEvent::Output(bytes) => {
                if !sink.wait_for_room() || !sink.write(&bytes) {
                    // The terminal is closing; nobody reads the exit.
                    return;
                }
            }
            ShellEvent::Progress(text) => notice(sink, Tone::Progress, &text),
            ShellEvent::Exited(exit) => {
                if let Some(message) = &exit.message {
                    notice(sink, Tone::Error, message);
                }
                break ExitStatus::new(exit.code, None::<String>);
            }
            ShellEvent::Failed(error) => {
                notice(sink, Tone::Error, &error.to_string());
                break ExitStatus::new(None, None::<String>);
            }
        }
    };
    *shared.exit.lock().unwrap_or_else(PoisonError::into_inner) = Some(exit);
    sink.finished();
}

/// How a line that is not program output looks.
#[derive(Clone, Copy)]
enum Tone {
    /// Dim: getting ready.
    Progress,
    /// Red: something went wrong.
    Error,
}

/// Writes a line of our own into the terminal, on a fresh line, styled so
/// it does not pass for program output.
fn notice(sink: &ByteSink, tone: Tone, text: &str) {
    let style = match tone {
        Tone::Progress => "\x1b[2m",
        Tone::Error => "\x1b[31m",
    };
    // Carriage return and erase line first, in case a prompt is half drawn.
    let line = format!("\r\x1b[K{style}{}\x1b[0m\r\n", text.replace('\n', "\r\n"));
    sink.write(line.as_bytes());
}

struct Handle {
    shared: Arc<Shared>,
}

impl ByteHandle for Handle {
    fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        self.shared.send(ShellInput::Bytes(bytes.to_vec()));
        Ok(bytes.len())
    }

    fn resize(&self, viewport: Viewport) -> io::Result<()> {
        self.shared.send(resize(viewport));
        Ok(())
    }

    fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        Ok(self
            .shared
            .exit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone())
    }

    fn close(&self) {
        // Dropping the sender ends the shell; for a node shell the engine
        // then deletes its pod.
        self.shared
            .input
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}
