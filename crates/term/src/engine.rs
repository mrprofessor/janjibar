//! The owner thread: one thread per terminal that owns the Ghostty state,
//! parses output, encodes input and publishes frames.
//!
//! The thread sleeps on its mailbox. It wakes for output from the byte
//! source, for input, resizes and visibility changes, and for its timers
//! (scrollback compression, the render hold watchdog). A frame is built
//! only when the UI has granted a credit, so a terminal that is not painted
//! stops producing frames while it keeps parsing.
//!
//! Derived from Herdr (Apache-2.0).

use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use gpui_kit::base::async_util;

use crate::core::{Core, Effects};
use crate::frame::{CellSelection, CursorShape, TerminalColors, TerminalFrame, Viewport};
use crate::input::TerminalInput;
use crate::options::EngineOptions;
use crate::source::{FrameHandle, FrameSink, FrameSource, StartOptions};

const INPUT_BYTES: usize = 1 << 20;
const INPUT_COMMANDS: usize = 256;
const WRITE_CHUNK: usize = 8192;
/// The most input written in one turn, so output keeps flowing during a
/// large paste to a program that reads as fast as it comes.
const WRITE_BURST: usize = 64 * 1024;
/// While a write waits for room, how often the engine tries again even
/// without [`ByteSink::writable`], for a source that never calls it.
const WRITE_RETRY: Duration = Duration::from_millis(50);
/// The output a source may queue ahead of the parser before
/// [`ByteSink::wait_for_room`] holds it back.
const READ_QUEUE_BYTES: usize = 1 << 20;
const COMPRESSION_IDLE: Duration = Duration::from_millis(250);
const COMPRESSION_STEP: Duration = Duration::from_millis(16);
/// How long after output the engine asks its source for the foreground
/// process. Output is what a new program or a return to the prompt
/// produces, so the check needs no timer while the terminal is quiet.
const FOREGROUND_CHECK: Duration = Duration::from_millis(50);

/// How a program ended.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExitStatus {
    pub(crate) code: Option<i32>,
    pub(crate) signal: Option<String>,
    pub(crate) runtime: Duration,
    pub(crate) abnormal: bool,
}

impl ExitStatus {
    /// An exit with `code`, or by `signal` when a signal ended the program.
    /// The engine fills in the run time.
    pub fn new(code: Option<i32>, signal: Option<impl Into<String>>) -> Self {
        Self {
            code,
            signal: signal.map(Into::into),
            runtime: Duration::ZERO,
            abnormal: false,
        }
    }

    /// The exit code, `None` when a signal ended the program.
    pub fn code(&self) -> Option<i32> {
        self.code
    }

    /// The signal name, when a signal ended the program.
    pub fn signal(&self) -> Option<&str> {
        self.signal.as_deref()
    }

    /// How long the program ran.
    pub fn runtime(&self) -> Duration {
        self.runtime
    }

    /// Whether the run was shorter than the abnormal-exit threshold.
    pub fn is_abnormal(&self) -> bool {
        self.abnormal
    }
}

/// The process in the foreground of a terminal: the one that reads its
/// keys, such as the shell at its prompt or the editor it started.
///
/// A local pty reads it from the process table: the terminal's foreground
/// process group (`tcgetpgrp`), then the name, arguments and working
/// directory of that group's leader, through `proc_pidinfo` and `sysctl`
/// on macOS and `/proc` on Linux. Another [`ByteSource`] reports its own,
/// or none.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ForegroundProcess {
    pub(crate) pid: u32,
    pub(crate) name: String,
    pub(crate) argv: Vec<String>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) shell: bool,
}

impl ForegroundProcess {
    /// Process `pid`, named `name`, such as `vim`.
    pub fn new(pid: u32, name: impl Into<String>) -> Self {
        Self {
            pid,
            name: name.into(),
            argv: Vec::new(),
            cwd: None,
            shell: false,
        }
    }

    /// Set the arguments, the program's own name first.
    #[must_use]
    pub fn with_argv(mut self, argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.argv = argv.into_iter().map(Into::into).collect();
        self
    }

    /// Set the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Set whether it is the program the terminal started, such as the
    /// shell at its prompt.
    #[must_use]
    pub fn with_shell(mut self, shell: bool) -> Self {
        self.shell = shell;
        self
    }

    /// The process id.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The executable's name, such as `vim`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The arguments, the program's own name first, when they could be
    /// read.
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// The working directory, when it could be read.
    pub fn cwd(&self) -> Option<&std::path::Path> {
        self.cwd.as_deref()
    }

    /// Whether it is the program the terminal started, such as the shell at
    /// its prompt, rather than a program that program runs. An application
    /// asks before it closes a terminal where this is false.
    pub fn is_shell(&self) -> bool {
        self.shell
    }
}

/// The state of a terminal's program.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum TerminalStatus {
    /// The program has not started yet.
    #[default]
    Starting,
    /// The program is running.
    Live,
    /// The program ended.
    Exited(ExitStatus),
    /// The terminal could not start or the engine failed.
    Failed(String),
}

/// The latest published terminal state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TerminalSnapshot {
    /// The frame.
    pub frame: Arc<TerminalFrame>,
    /// The program's state.
    pub status: TerminalStatus,
    /// The title the program set.
    pub title: String,
    /// The working directory the program reported.
    pub cwd: Option<PathBuf>,
    /// Bells received so far.
    pub bell_count: u64,
    /// Increments per clipboard write.
    pub clipboard_sequence: u64,
    /// The last clipboard write.
    pub clipboard: Option<Arc<str>>,
    /// Increments per desktop notification.
    pub notification_sequence: u64,
    /// The last desktop notification as title and body.
    pub notification: Option<(Arc<str>, Arc<str>)>,
    /// Whether the cursor was at a shell prompt when the frame was built.
    pub at_prompt: bool,
    /// The process in the foreground, when the source reports one.
    pub foreground: Option<ForegroundProcess>,
}

impl TerminalSnapshot {
    /// A snapshot holding only a frame, for fixtures and tests.
    #[must_use]
    pub fn for_frame(frame: Arc<TerminalFrame>) -> Self {
        Self {
            frame,
            status: TerminalStatus::Live,
            ..Self::default()
        }
    }

    /// Set the state of the program.
    #[must_use]
    pub fn with_status(mut self, status: TerminalStatus) -> Self {
        self.status = status;
        self
    }

    /// Set the title the program set.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Set the working directory the program reported.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Set the number of bells received so far. A change rings the bell.
    #[must_use]
    pub fn with_bell_count(mut self, count: u64) -> Self {
        self.bell_count = count;
        self
    }

    /// Set the last clipboard write and its sequence number. A new sequence
    /// number writes `text` to the clipboard.
    #[must_use]
    pub fn with_clipboard(mut self, sequence: u64, text: impl Into<Arc<str>>) -> Self {
        self.clipboard_sequence = sequence;
        self.clipboard = Some(text.into());
        self
    }

    /// Set the last desktop notification and its sequence number.
    #[must_use]
    pub fn with_notification(
        mut self,
        sequence: u64,
        title: impl Into<Arc<str>>,
        body: impl Into<Arc<str>>,
    ) -> Self {
        self.notification_sequence = sequence;
        self.notification = Some((title.into(), body.into()));
        self
    }

    /// Set whether the cursor was at a shell prompt.
    #[must_use]
    pub fn with_at_prompt(mut self, at_prompt: bool) -> Self {
        self.at_prompt = at_prompt;
        self
    }

    /// Set the process in the foreground.
    #[must_use]
    pub fn with_foreground(mut self, foreground: ForegroundProcess) -> Self {
        self.foreground = Some(foreground);
        self
    }
}

/// A source of terminal output that also accepts the program's input.
///
/// The engine calls [`ByteSource::start`] on its owner thread and pumps
/// everything the source writes to the [`ByteSink`] through the terminal.
pub trait ByteSource: Send + 'static {
    /// Start producing output into `sink` at `viewport` and return the
    /// handle for input.
    fn start(
        self: Box<Self>,
        sink: ByteSink,
        viewport: Viewport,
    ) -> io::Result<Box<dyn ByteHandle>>;
}

/// The engine's side of a byte source.
pub trait ByteHandle: Send {
    /// Write input for the program without blocking: as much of `bytes` as
    /// it takes now, and return how much that was. When that is less than
    /// all of it, the source calls [`ByteSink::writable`] once more fits,
    /// and the engine writes the rest then, ahead of newer input.
    fn write(&self, bytes: &[u8]) -> io::Result<usize>;
    /// The grid changed.
    fn resize(&self, viewport: Viewport) -> io::Result<()>;
    /// Whether the program ended, and how.
    fn try_wait(&self) -> io::Result<Option<ExitStatus>>;
    /// Stop the program and release the source.
    fn close(&self);
    /// The process in the foreground, if the source knows it. The engine
    /// asks on its own thread, at most every 50 ms and only after output,
    /// so it may read the process table but must not block.
    fn foreground(&self) -> Option<ForegroundProcess> {
        None
    }
}

/// Where a [`ByteSource`] writes its output.
#[derive(Clone, Debug)]
pub struct ByteSink {
    shared: Arc<Shared>,
}

impl ByteSink {
    /// Deliver output. Returns false once the terminal is closing. It
    /// never blocks; a source that reads on its own thread calls
    /// [`wait_for_room`](Self::wait_for_room) before each read so the
    /// output waiting for the parser stays bounded.
    pub fn write(&self, bytes: &[u8]) -> bool {
        let mut mail = self.shared.lock();
        if self.shared.closing.load(Ordering::Acquire) {
            return false;
        }
        mail.reads.push_back(bytes.to_vec());
        mail.read_bytes += bytes.len();
        mail.generation = mail.generation.wrapping_add(1);
        self.shared.wake.notify_one();
        true
    }

    /// Blocks while a megabyte of output waits for the parser, which then
    /// holds the program back through the pty instead of filling memory.
    /// Returns false once the terminal is closing. The engine never waits
    /// on the source, so this always ends.
    pub fn wait_for_room(&self) -> bool {
        let mut mail = self.shared.lock();
        while mail.read_bytes >= READ_QUEUE_BYTES && !self.shared.closing.load(Ordering::Acquire) {
            mail = self
                .shared
                .room
                .wait(mail)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        !self.shared.closing.load(Ordering::Acquire)
    }

    /// The program can take input again after a short
    /// [`ByteHandle::write`]; wakes the engine to write the rest.
    pub fn writable(&self) {
        let mut mail = self.shared.lock();
        mail.writable = true;
        self.shared.bump(&mut mail);
    }

    /// The source reached end of file.
    pub fn finished(&self) {
        let mut mail = self.shared.lock();
        mail.reader_exited = true;
        mail.generation = mail.generation.wrapping_add(1);
        self.shared.wake.notify_one();
    }
}

/// Why an input was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputRejected {
    /// The bounded input queue is full; try again after a frame.
    QueueFull,
    /// The terminal is closed.
    Closed,
}

impl std::fmt::Display for InputRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueFull => f.write_str("terminal input queue is full"),
            Self::Closed => f.write_str("terminal is closed"),
        }
    }
}

impl std::error::Error for InputRejected {}

#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent wake reason for the owner thread"
)]
#[derive(Default)]
struct Mailbox {
    commands: VecDeque<TerminalInput>,
    bytes: usize,
    reads: VecDeque<Vec<u8>>,
    /// The bytes in `reads`.
    read_bytes: usize,
    /// The source said it can take input again.
    writable: bool,
    viewport: Option<Viewport>,
    visible: bool,
    credit: bool,
    /// The UI painted since the owner last looked, which counts as use.
    painted: bool,
    reader_exited: bool,
    reveal: bool,
    interrupt: Option<TerminalInput>,
    copy: Option<(CellSelection, async_util::Sender<io::Result<String>>)>,
    cursor: Option<(CursorShape, bool)>,
    colors: Option<TerminalColors>,
    generation: u64,
}

#[derive(Debug)]
struct Shared {
    mailbox: Mutex<Mailbox>,
    wake: Condvar,
    /// Signals a source in [`ByteSink::wait_for_room`].
    room: Condvar,
    closing: AtomicBool,
}

impl std::fmt::Debug for Mailbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mailbox")
            .field("commands", &self.commands.len())
            .field("reads", &self.reads.len())
            .finish_non_exhaustive()
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Mailbox> {
        self.mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn bump(&self, mail: &mut Mailbox) {
        mail.generation = mail.generation.wrapping_add(1);
        self.wake.notify_one();
    }

    /// Marks the terminal closing and frees a source waiting for room.
    fn set_closing(&self) {
        self.closing.store(true, Ordering::Release);
        self.room.notify_all();
    }
}

/// The UI's handle to a running engine.
///
/// Every method is cheap and never blocks on the owner thread.
#[derive(Clone, Debug)]
pub(crate) struct EngineHandle {
    shared: Arc<Shared>,
}

impl EngineHandle {
    /// Queue input. Ctrl+C jumps the queue and discards pending input.
    pub fn input(&self, input: TerminalInput) -> Result<(), InputRejected> {
        let mut mail = self.shared.lock();
        if self.shared.closing.load(Ordering::Acquire) {
            return Err(InputRejected::Closed);
        }
        if input.is_interrupt() {
            let discarded: usize = mail.commands.iter().map(TerminalInput::budget).sum();
            mail.commands.clear();
            mail.bytes = mail.bytes.saturating_sub(discarded);
            mail.interrupt = Some(input);
            self.shared.bump(&mut mail);
            return Ok(());
        }
        let budget = input.budget();
        if mail.commands.len() >= INPUT_COMMANDS || budget > INPUT_BYTES.saturating_sub(mail.bytes)
        {
            return Err(InputRejected::QueueFull);
        }
        mail.bytes += budget;
        mail.commands.push_back(input);
        self.shared.bump(&mut mail);
        Ok(())
    }

    /// Queued input bytes not yet written to the program.
    #[must_use]
    pub fn input_backlog(&self) -> usize {
        self.shared.lock().bytes
    }

    /// Change the grid.
    pub fn resize(&self, viewport: Viewport) {
        let mut mail = self.shared.lock();
        mail.viewport = Some(viewport);
        self.shared.bump(&mut mail);
    }

    /// Tell the engine whether the terminal is painted. A hidden terminal
    /// keeps parsing but builds no frames; revealing it builds one at once.
    pub fn set_visible(&self, visible: bool) {
        let mut mail = self.shared.lock();
        if mail.visible != visible {
            mail.visible = visible;
            mail.credit = visible;
            mail.reveal = visible;
            self.shared.bump(&mut mail);
        }
    }

    /// Grant one frame of credit. The UI calls this after it painted the
    /// last frame.
    pub fn request_frame(&self) {
        let mut mail = self.shared.lock();
        if !mail.credit || !mail.painted {
            mail.credit = true;
            mail.painted = true;
            self.shared.bump(&mut mail);
        }
    }

    /// Set the default cursor shape and blink.
    pub fn set_cursor(&self, shape: CursorShape, blinking: bool) {
        let mut mail = self.shared.lock();
        mail.cursor = Some((shape, blinking));
        self.shared.bump(&mut mail);
    }

    /// Change the default colors. The next frame carries them.
    pub fn set_colors(&self, colors: TerminalColors) {
        let mut mail = self.shared.lock();
        mail.colors = Some(colors);
        self.shared.bump(&mut mail);
    }

    /// Ask the owner thread for the selected text. The answer arrives on the
    /// returned receiver.
    pub fn copy(&self, selection: CellSelection) -> async_util::Receiver<io::Result<String>> {
        let (tx, rx) = async_util::unbounded();
        let mut mail = self.shared.lock();
        mail.copy = Some((selection, tx));
        self.shared.bump(&mut mail);
        rx
    }

    /// Stop the program and the owner thread. Idempotent.
    pub fn close(&self) {
        let mut mail = self.shared.lock();
        self.shared.set_closing();
        self.shared.bump(&mut mail);
    }
}

/// Everything needed to start an engine.
#[derive(Debug)]
pub(crate) struct EngineConfig {
    /// Engine limits.
    pub options: EngineOptions,
    /// The initial grid.
    pub viewport: Viewport,
    /// The initial colors.
    pub colors: TerminalColors,
    /// Whether the terminal starts visible.
    pub visible: bool,
    /// The default cursor.
    pub cursor: Option<(CursorShape, bool)>,
    /// Bytes written to the program right after it starts.
    pub input: Vec<u8>,
    /// A run shorter than this is an abnormal exit.
    pub abnormal_exit_runtime: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            options: EngineOptions::default(),
            viewport: Viewport::default(),
            colors: TerminalColors::default(),
            visible: true,
            cursor: None,
            input: Vec::new(),
            abnormal_exit_runtime: Duration::from_millis(250),
        }
    }
}

/// Start an owner thread for `source`, publishing to `sink`.
///
/// The first snapshot is published as soon as the source started.
pub(crate) fn spawn(
    source: Box<dyn ByteSource>,
    config: EngineConfig,
    sink: FrameSink,
) -> io::Result<EngineHandle> {
    let shared = Arc::new(Shared {
        // The first frame is published as the source starts, so there is no
        // credit until the UI paints it, the same as for every later frame.
        mailbox: Mutex::new(Mailbox {
            visible: config.visible,
            credit: false,
            ..Mailbox::default()
        }),
        wake: Condvar::new(),
        room: Condvar::new(),
        closing: AtomicBool::new(false),
    });
    let handle = EngineHandle {
        shared: Arc::clone(&shared),
    };
    let byte_sink = ByteSink {
        shared: Arc::clone(&shared),
    };
    std::thread::Builder::new()
        .name("term-engine".to_owned())
        .spawn(move || {
            let mut owner =
                match Owner::start(source, byte_sink, config, Arc::clone(&shared), &sink) {
                    Ok(owner) => owner,
                    Err(error) => {
                        shared.set_closing();
                        sink.publish(Arc::new(TerminalSnapshot {
                            status: TerminalStatus::Failed(error.to_string()),
                            ..TerminalSnapshot::default()
                        }));
                        return;
                    }
                };
            owner.run();
        })?;
    Ok(handle)
}

/// A [`FrameSource`] that runs a [`ByteSource`] through the engine.
#[derive(Debug)]
pub struct Engine<S> {
    source: S,
    options: EngineOptions,
    abnormal_exit_runtime: Duration,
    input: Vec<u8>,
}

impl<S: ByteSource> Engine<S> {
    /// An engine for `source` with `options`.
    #[must_use]
    pub fn new(source: S, options: EngineOptions) -> Self {
        Self {
            source,
            options,
            abnormal_exit_runtime: Duration::from_millis(250),
            input: Vec::new(),
        }
    }

    /// Set the run time under which an exit is reported as abnormal.
    #[must_use]
    pub fn with_abnormal_exit_runtime(mut self, runtime: Duration) -> Self {
        self.abnormal_exit_runtime = runtime;
        self
    }

    /// Set bytes written to the program right after it starts.
    #[must_use]
    pub fn with_input(mut self, input: impl Into<Vec<u8>>) -> Self {
        self.input = input.into();
        self
    }
}

impl<S: ByteSource> FrameSource for Engine<S> {
    fn start(
        self: Box<Self>,
        sink: FrameSink,
        options: StartOptions,
    ) -> io::Result<Box<dyn FrameHandle>> {
        let handle = spawn(
            Box::new(self.source),
            EngineConfig {
                options: self.options,
                viewport: options.viewport,
                colors: options.colors,
                visible: options.visible,
                cursor: options.cursor,
                input: self.input,
                abnormal_exit_runtime: self.abnormal_exit_runtime,
            },
            sink,
        )?;
        Ok(Box::new(handle))
    }
}

impl FrameHandle for EngineHandle {
    fn input(&self, input: TerminalInput) -> Result<(), InputRejected> {
        Self::input(self, input)
    }

    fn input_backlog(&self) -> usize {
        Self::input_backlog(self)
    }

    fn resize(&self, viewport: Viewport) {
        Self::resize(self, viewport);
    }

    fn set_visible(&self, visible: bool) {
        Self::set_visible(self, visible);
    }

    fn request_frame(&self) {
        Self::request_frame(self);
    }

    fn set_colors(&self, colors: TerminalColors) {
        Self::set_colors(self, colors);
    }

    fn set_cursor(&self, shape: CursorShape, blinking: bool) {
        Self::set_cursor(self, shape, blinking);
    }

    fn copy(&self, selection: CellSelection) -> Option<async_util::Receiver<io::Result<String>>> {
        Some(Self::copy(self, selection))
    }

    fn close(&self) {
        Self::close(self);
    }
}

struct PendingWrite {
    bytes: Vec<u8>,
    offset: usize,
    budget: usize,
    paste: bool,
}

/// The owner's core: live, or parked as a snapshot in the park store.
struct CoreSlot {
    live: Option<Core>,
    parked: Option<crate::core::Parked>,
}

impl std::ops::Deref for CoreSlot {
    type Target = Core;

    #[expect(
        clippy::expect_used,
        reason = "the owner thread unparks the core before every use"
    )]
    fn deref(&self) -> &Core {
        self.live
            .as_ref()
            .expect("the core is restored before it is used")
    }
}

impl std::ops::DerefMut for CoreSlot {
    #[expect(
        clippy::expect_used,
        reason = "the owner thread unparks the core before every use"
    )]
    fn deref_mut(&mut self) -> &mut Core {
        self.live
            .as_mut()
            .expect("the core is restored before it is used")
    }
}

/// What the owner needs to park and restore its core.
struct Parking {
    options: EngineOptions,
    key: u64,
    /// The last output, input, resize, paint or other request.
    last_use: Instant,
}

struct Owner {
    shared: Arc<Shared>,
    core: CoreSlot,
    parking: Parking,
    handle: Box<dyn ByteHandle>,
    sink: FrameSink,
    snapshot: TerminalSnapshot,
    /// Bytes that go ahead of input: replies to the program's queries and
    /// interrupts.
    urgent: Vec<u8>,
    pending: Option<PendingWrite>,
    /// The source took less than it was given and has not said it has
    /// room again.
    write_blocked: bool,
    exit: Option<ExitStatus>,
    started: Instant,
    abnormal_exit_runtime: Duration,
    compression_deadline: Option<Instant>,
    /// When to ask the source for the foreground process again.
    foreground_due: Option<Instant>,
    dirty: bool,
    force: bool,
    effects: Effects,
}

impl Owner {
    fn start(
        source: Box<dyn ByteSource>,
        byte_sink: ByteSink,
        config: EngineConfig,
        shared: Arc<Shared>,
        sink: &FrameSink,
    ) -> io::Result<Self> {
        let mut core = Core::new(config.viewport, &config.options, config.colors)?;
        if let Some((shape, blinking)) = config.cursor {
            core.set_cursor(shape, blinking)?;
        }
        let frame = core
            .frame(true)?
            .ok_or_else(|| io::Error::other("initial terminal frame missing"))?;
        let handle = source.start(byte_sink, config.viewport)?;
        let snapshot = TerminalSnapshot {
            frame,
            status: TerminalStatus::Live,
            ..TerminalSnapshot::default()
        };
        sink.publish(Arc::new(snapshot.clone()));
        Ok(Self {
            shared,
            core: CoreSlot {
                live: Some(core),
                parked: None,
            },
            parking: Parking {
                options: config.options.clone(),
                key: crate::park::next_key(),
                last_use: Instant::now(),
            },
            handle,
            sink: sink.clone(),
            snapshot,
            urgent: config.input.clone(),
            pending: None,
            write_blocked: false,
            exit: None,
            started: Instant::now(),
            abnormal_exit_runtime: config.abnormal_exit_runtime,
            compression_deadline: None,
            foreground_due: Some(Instant::now() + FOREGROUND_CHECK),
            dirty: true,
            force: false,
            effects: Effects::default(),
        })
    }

    fn run(&mut self) {
        while !self.shared.closing.load(Ordering::Acquire) {
            if let Err(error) = self.turn() {
                self.snapshot.status = TerminalStatus::Failed(error.to_string());
                self.publish();
                break;
            }
        }
        self.cleanup();
    }

    fn turn(&mut self) -> io::Result<()> {
        let (
            reads,
            viewport,
            command,
            visible,
            credit,
            eof,
            reveal,
            interrupt,
            copy,
            cursor,
            colors,
            painted,
            generation,
        ) = {
            let mut mail = self.shared.lock();
            let interrupt = mail.interrupt.take();
            let command = if interrupt.is_none() && self.pending.is_none() {
                mail.commands.pop_front()
            } else {
                None
            };
            mail.read_bytes = 0;
            self.shared.room.notify_all();
            if std::mem::take(&mut mail.writable) {
                self.write_blocked = false;
            }
            (
                std::mem::take(&mut mail.reads),
                mail.viewport.take(),
                command,
                mail.visible,
                mail.credit,
                mail.reader_exited,
                std::mem::take(&mut mail.reveal),
                interrupt,
                mail.copy.take(),
                mail.cursor.take(),
                mail.colors.take(),
                std::mem::take(&mut mail.painted),
                mail.generation,
            )
        };
        let had_work = !reads.is_empty()
            || viewport.is_some()
            || command.is_some()
            || reveal
            || interrupt.is_some()
            || copy.is_some()
            || cursor.is_some()
            || colors.is_some();
        {
            // Anything that needs the terminal restores a parked one first;
            // with nothing to do, a parked terminal only waits.
            if had_work || painted || (eof && self.exit.is_none()) {
                self.parking.last_use = Instant::now();
                self.unpark()?;
            } else if self.core.live.is_none() {
                self.wait(generation, None);
                return Ok(());
            }
        }
        if reveal {
            self.dirty = true;
            self.force = true;
        }
        if let Some((shape, blinking)) = cursor {
            self.core.set_cursor(shape, blinking)?;
            self.dirty = true;
        }
        if let Some(colors) = colors {
            self.core.set_colors(colors)?;
            self.dirty = true;
            self.force = true;
        }
        if !reads.is_empty() {
            for chunk in reads {
                self.core.write(&chunk, &mut self.effects);
            }
            self.compression_deadline = Some(Instant::now() + COMPRESSION_IDLE);
            self.foreground_due
                .get_or_insert_with(|| Instant::now() + FOREGROUND_CHECK);
            self.dirty = true;
            self.apply_effects();
        }
        if let Some(interrupt) = interrupt {
            let partial_paste = self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.paste && pending.offset > 0);
            if let Some(pending) = self.pending.take() {
                self.release(pending.budget);
            }
            let mut bytes = if partial_paste {
                b"\x1b[201~".to_vec()
            } else {
                Vec::new()
            };
            bytes.extend(self.core.input(interrupt)?);
            self.urgent.extend(bytes);
            self.dirty = true;
        }
        if let Some(viewport) = viewport {
            self.core.resize(viewport, &mut self.effects)?;
            self.handle.resize(viewport)?;
            self.apply_effects();
            self.dirty = true;
            self.force = true;
        }
        if let Some(input) = command {
            let budget = input.budget();
            if self.exit.is_some() && !matches!(input, TerminalInput::Scroll(_)) {
                self.release(budget);
            } else {
                let paste = matches!(input, TerminalInput::Paste(_));
                match self.core.input(input) {
                    Ok(bytes) if !bytes.is_empty() => {
                        self.pending = Some(PendingWrite {
                            bytes,
                            offset: 0,
                            budget,
                            paste,
                        });
                    }
                    Ok(_) => self.release(budget),
                    Err(error) => {
                        self.release(budget);
                        return Err(error);
                    }
                }
                self.dirty = true;
            }
        }
        if let Some((selection, reply)) = copy {
            let _ = reply.try_send(self.core.copy(selection));
        }
        self.flush()?;
        if eof
            && self.exit.is_none()
            && let Some(mut status) = self.handle.try_wait()?
        {
            status.runtime = self.started.elapsed();
            status.abnormal = status.runtime <= self.abnormal_exit_runtime;
            self.exit = Some(status.clone());
            self.snapshot.status = TerminalStatus::Exited(status);
            self.dirty = true;
            self.force = true;
        }
        if self.foreground_due.is_some_and(|due| Instant::now() >= due) {
            self.foreground_due = None;
            let foreground = if self.exit.is_some() {
                None
            } else {
                self.handle.foreground()
            };
            if foreground != self.snapshot.foreground {
                self.snapshot.foreground = foreground;
                // Published with the frame already shown, so a hidden or
                // unpainted terminal reports it too.
                self.publish();
            }
        }
        let exited = self.exit.is_some();
        let held = self.core.held();
        if self.dirty && ((visible && credit && !held) || (exited && self.force)) {
            if let Some(frame) = self.core.frame(self.force)? {
                self.snapshot.frame = frame;
                self.snapshot.at_prompt = self.core.at_prompt();
                self.shared.lock().credit = false;
                self.publish();
            }
            self.dirty = false;
            self.force = false;
        }
        if self
            .compression_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            let complete = self.core.compress();
            self.compression_deadline = (!complete).then(|| Instant::now() + COMPRESSION_STEP);
        }
        let unsent = self.pending.is_some() || !self.urgent.is_empty();
        if !had_work && (!unsent || self.write_blocked) {
            let delay = if unsent {
                Some(WRITE_RETRY)
            } else if self.dirty && visible && credit {
                self.core
                    .hold_remaining()
                    .or(Some(Duration::from_millis(1)))
            } else if let Some(deadline) = self.compression_deadline {
                Some(deadline.saturating_duration_since(Instant::now()))
            } else if eof && self.exit.is_none() {
                Some(Duration::from_millis(20))
            } else {
                None
            };
            let delay = match self.foreground_due {
                Some(due) => {
                    let until = due.saturating_duration_since(Instant::now());
                    Some(delay.map_or(until, |delay| delay.min(until)))
                }
                None => delay,
            };
            let delay = match self.park_after() {
                Some(_) if self.park_if_idle() => None,
                Some(after) => Some(delay.map_or(after, |delay| delay.min(after))),
                None => delay,
            };
            self.wait(generation, delay);
        }
        Ok(())
    }

    /// Sleeps until the mailbox changes, or `delay` passes.
    fn wait(&self, generation: u64, delay: Option<Duration>) {
        let mail = self.shared.lock();
        if mail.generation == generation && !self.shared.closing.load(Ordering::Acquire) {
            match delay {
                None => drop(
                    self.shared
                        .wake
                        .wait(mail)
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                ),
                Some(delay) => drop(
                    self.shared
                        .wake
                        .wait_timeout(mail, delay.max(Duration::from_millis(1)))
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                ),
            }
        }
    }

    /// How long until a live terminal may park, when parking is on and
    /// nothing keeps it awake.
    fn park_after(&mut self) -> Option<Duration> {
        let park = &self.parking.options.park;
        if !park.enabled
            || self.core.live.is_none()
            || self.pending.is_some()
            || self.exit.is_some()
            || self.core.held()
        {
            return None;
        }
        Some(park.idle.saturating_sub(self.parking.last_use.elapsed()))
    }

    /// Parks the terminal when it has been idle long enough. Returns
    /// whether it parked.
    fn park_if_idle(&mut self) -> bool {
        if self.park_after() != Some(Duration::ZERO) {
            return false;
        }
        let store = Arc::clone(&self.parking.options.park.store);
        let parked = self
            .core
            .park()
            .and_then(|(bytes, parked)| store.save(self.parking.key, &bytes).map(|()| parked));
        if let Ok(parked) = parked {
            self.core.live = None;
            self.core.parked = Some(parked);
            self.compression_deadline = None;
            true
        } else {
            // Try again after another idle period rather than at once.
            self.parking.last_use = Instant::now();
            false
        }
    }

    /// Restores a parked terminal from its store.
    fn unpark(&mut self) -> io::Result<()> {
        let Some(parked) = self.core.parked.take() else {
            return Ok(());
        };
        let bytes = self.parking.options.park.store.take(self.parking.key)?;
        self.core.live = Some(Core::restore(&bytes, parked, &self.parking.options)?);
        Ok(())
    }

    fn release(&self, budget: usize) {
        let mut mail = self.shared.lock();
        mail.bytes = mail.bytes.saturating_sub(budget);
    }

    /// Writes what the program takes now: the urgent bytes, then up to
    /// [`WRITE_BURST`] of the pending input, resuming where the last short
    /// write stopped.
    fn flush(&mut self) -> io::Result<()> {
        if self.write_blocked {
            return Ok(());
        }
        if !self.urgent.is_empty() {
            let written = self.handle.write(&self.urgent)?;
            self.urgent.drain(..written.min(self.urgent.len()));
            if !self.urgent.is_empty() {
                self.write_blocked = true;
                return Ok(());
            }
        }
        let mut burst = WRITE_BURST;
        while burst > 0 {
            let Some(pending) = &mut self.pending else {
                break;
            };
            let end = (pending.offset + WRITE_CHUNK.min(burst)).min(pending.bytes.len());
            let offered = end - pending.offset;
            let written = self
                .handle
                .write(&pending.bytes[pending.offset..end])?
                .min(offered);
            pending.offset += written;
            burst = burst.saturating_sub(written);
            if pending.offset >= pending.bytes.len() {
                let budget = pending.budget;
                self.pending = None;
                self.release(budget);
                break;
            }
            if written < offered {
                self.write_blocked = true;
                break;
            }
        }
        Ok(())
    }

    fn apply_effects(&mut self) {
        let effects = std::mem::take(&mut self.effects);
        self.urgent.extend_from_slice(&effects.replies);
        let mut changed = false;
        if effects.title_changed {
            self.snapshot.title = self.core.title().to_owned();
            changed = true;
        }
        if effects.cwd_changed {
            self.snapshot.cwd = self.core.cwd().cloned();
            changed = true;
        }
        if effects.bells > 0 {
            self.snapshot.bell_count += effects.bells;
            changed = true;
        }
        if let Some(text) = effects.clipboard.last() {
            self.snapshot.clipboard_sequence += effects.clipboard.len() as u64;
            self.snapshot.clipboard = Some(Arc::from(text.as_str()));
            changed = true;
        }
        if let Some((title, body)) = effects.notifications.last() {
            self.snapshot.notification_sequence += effects.notifications.len() as u64;
            self.snapshot.notification =
                Some((Arc::from(title.as_str()), Arc::from(body.as_str())));
            changed = true;
        }
        if changed {
            self.dirty = true;
            self.force = true;
        }
    }

    fn publish(&self) {
        self.sink.publish(Arc::new(self.snapshot.clone()));
    }

    fn cleanup(&mut self) {
        self.shared.set_closing();
        {
            let mut mail = self.shared.lock();
            mail.commands.clear();
            mail.bytes = 0;
            mail.reads.clear();
            mail.read_bytes = 0;
        }
        self.pending = None;
        if self.core.parked.take().is_some() {
            self.parking.options.park.store.remove(self.parking.key);
        }
        self.handle.close();
        if self.exit.is_none() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if let Ok(Some(mut status)) = self.handle.try_wait() {
                    status.runtime = self.started.elapsed();
                    status.abnormal = status.runtime <= self.abnormal_exit_runtime;
                    self.snapshot.status = TerminalStatus::Exited(status.clone());
                    self.exit = Some(status);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        self.publish();
    }
}
