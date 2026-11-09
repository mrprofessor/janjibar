//! The I/O of a local pty, without blocking the engine.
//!
//! The engine never waits on the program. Input is written as far as the
//! pty takes it and the rest is kept, in order, until the pty has room
//! again; output is read only while the engine's read queue has room, so a
//! program that floods the terminal is held back by the pty, not by memory.
//! Neither direction waits on the other, so a large paste into a program
//! that is busy writing cannot deadlock.
//!
//! One thread owns both directions: it polls the pty for output and, while
//! a write is short, for room to write, and wakes the engine when the pty
//! can take input again. Input itself is written by the engine with
//! non-blocking writes.
//!
//! Derived from Herdr (Apache-2.0), through gpui-cn.

use std::io;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::event::{PollFd, PollFlags};
use rustix::fs::OFlags;
use rustix::io::{Errno, FdFlags};

use crate::engine::ByteSink;

/// How much output one read takes.
const READ_BUFFER: usize = 64 * 1024;

/// The pty master in non-blocking mode, with the thread that reads it.
pub(crate) struct PollIo {
    /// Our own descriptor for the master, for writes.
    writer: OwnedFd,
    shared: Arc<Shared>,
}

struct Shared {
    /// A write was short; the thread watches for room to write.
    want_writable: AtomicBool,
    closed: AtomicBool,
    /// The write end of the pipe that wakes the thread.
    wake: OwnedFd,
}

impl Shared {
    fn poke(&self) {
        // A full pipe already holds a wake, so a failed write loses nothing.
        let _ = rustix::io::write(&self.wake, &[1]);
    }
}

impl PollIo {
    /// Puts the pty master `master` in non-blocking mode and starts the
    /// thread that reads it into `sink`.
    ///
    /// The descriptor is duplicated at once, so it only has to stay open
    /// for the length of this call.
    pub(crate) fn start(master: RawFd, sink: ByteSink) -> io::Result<Self> {
        #[expect(
            unsafe_code,
            reason = "portable-pty hands out the master only as a raw descriptor"
        )]
        // SAFETY: `master` is the open descriptor of the pty master, which
        // the caller owns and keeps open for this call. It is borrowed only
        // to duplicate it below.
        let master = unsafe { BorrowedFd::borrow_raw(master) };
        // Our own descriptors, so neither side ever uses a closed or reused
        // one. They share the non-blocking mode with the master.
        let writer = rustix::io::fcntl_dupfd_cloexec(master, 0)?;
        let reader = rustix::io::fcntl_dupfd_cloexec(master, 0)?;
        set_nonblocking(writer.as_fd())?;
        let (wake_rx, wake_tx) = pipe()?;
        let shared = Arc::new(Shared {
            want_writable: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            wake: wake_tx,
        });
        let thread = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("term-pty".to_owned())
            .spawn(move || run(&reader, &wake_rx, &sink, &thread))?;
        Ok(Self { writer, shared })
    }

    /// Writes what the pty takes now. A short write has the thread watch
    /// for room and call [`ByteSink::writable`].
    pub(crate) fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        loop {
            match rustix::io::write(&self.writer, bytes) {
                Ok(written) => {
                    if written < bytes.len() {
                        self.want_writable();
                    }
                    return Ok(written);
                }
                Err(Errno::INTR) => {}
                Err(Errno::AGAIN) => {
                    self.want_writable();
                    return Ok(0);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn want_writable(&self) {
        self.shared.want_writable.store(true, Ordering::SeqCst);
        self.shared.poke();
    }

    /// Stops the thread.
    pub(crate) fn close(&self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        self.shared.poke();
    }
}

impl Drop for PollIo {
    fn drop(&mut self) {
        self.close();
    }
}

fn run(reader: &OwnedFd, wake: &OwnedFd, sink: &ByteSink, shared: &Shared) {
    let mut buf = vec![0u8; READ_BUFFER];
    while !shared.closed.load(Ordering::SeqCst) {
        let want = shared.want_writable.load(Ordering::SeqCst);
        let mut events = PollFlags::IN;
        if want {
            events |= PollFlags::OUT;
        }
        let mut fds = [
            PollFd::new(reader, events),
            PollFd::new(wake, PollFlags::IN),
        ];
        match rustix::event::poll(&mut fds, None) {
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(_) => break,
        }
        let (pty, woken) = (fds[0].revents(), fds[1].revents());
        if !woken.is_empty() {
            drain(wake);
        }
        if shared.closed.load(Ordering::SeqCst) {
            break;
        }
        if pty.contains(PollFlags::OUT) && shared.want_writable.swap(false, Ordering::SeqCst) {
            sink.writable();
        }
        if pty.contains(PollFlags::NVAL) {
            break;
        }
        if pty.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
            if !sink.wait_for_room() {
                break;
            }
            match rustix::io::read(reader, &mut buf[..]) {
                Ok(0) => break,
                Ok(read) => {
                    if !sink.write(&buf[..read]) {
                        break;
                    }
                }
                Err(Errno::AGAIN | Errno::INTR) => {
                    // A hang-up with nothing left to read is the end.
                    if pty.contains(PollFlags::HUP) {
                        break;
                    }
                }
                // EIO: the program and every holder of the pty are gone.
                Err(_) => break,
            }
        }
    }
    sink.finished();
}

/// Empties the wake pipe. It is non-blocking, so this stops when it is
/// empty.
fn drain(fd: &OwnedFd) {
    let mut buf = [0u8; 64];
    while matches!(rustix::io::read(fd, &mut buf[..]), Ok(read) if read > 0) {}
}

fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd)?;
    rustix::fs::fcntl_setfl(fd, flags | OFlags::NONBLOCK)?;
    Ok(())
}

/// A non-blocking, close-on-exec pipe, as (read end, write end).
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let (rx, tx) = rustix::pipe::pipe()?;
    for fd in [&rx, &tx] {
        set_nonblocking(fd.as_fd())?;
        rustix::io::fcntl_setfd(fd, FdFlags::CLOEXEC)?;
    }
    Ok((rx, tx))
}
