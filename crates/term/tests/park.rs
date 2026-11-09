//! Parking: an idle terminal saves a snapshot to its store and frees its
//! state, then restores it, unchanged, the moment something needs it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use term::input::ScrollRequest;
use term::park::{DirStore, MemoryStore, ParkOptions, ParkStore};
use term::{
    Engine, EngineOptions, FrameHandle, FrameSink, FrameSource, StartOptions, StreamEvent,
    StreamPeer, StreamSource, TerminalInput, TerminalSnapshot, Viewport,
};

/// A custom store that counts what the engine asks of it, over memory.
#[derive(Default)]
struct Counting {
    inner: MemoryStore,
    saves: AtomicUsize,
    takes: AtomicUsize,
    removes: AtomicUsize,
}

impl ParkStore for Counting {
    fn save(&self, key: u64, snapshot: &[u8]) -> std::io::Result<()> {
        self.saves.fetch_add(1, Ordering::SeqCst);
        self.inner.save(key, snapshot)
    }
    fn take(&self, key: u64) -> std::io::Result<Vec<u8>> {
        self.takes.fetch_add(1, Ordering::SeqCst);
        self.inner.take(key)
    }
    fn remove(&self, key: u64) {
        self.removes.fetch_add(1, Ordering::SeqCst);
        self.inner.remove(key);
    }
}

impl Counting {
    fn saves(&self) -> usize {
        self.saves.load(Ordering::SeqCst)
    }
    fn takes(&self) -> usize {
        self.takes.load(Ordering::SeqCst)
    }
}

const IDLE: Duration = Duration::from_millis(100);

/// The idle period of the tests that keep a terminal busy, long against
/// [`TICK`] so a slow machine never misses a tick by a whole period.
const LONG_IDLE: Duration = Duration::from_millis(500);

/// How often those tests write or paint.
const TICK: Duration = Duration::from_millis(20);

struct Running {
    peer: StreamPeer,
    sink: FrameSink,
    handle: Box<dyn FrameHandle>,
}

fn start(park: ParkOptions) -> Running {
    let (source, peer) = StreamSource::new();
    let (sink, _wake) = FrameSink::new();
    let handle = Box::new(Engine::new(
        source,
        EngineOptions::default().with_park(park),
    ))
    .start(
        sink.clone(),
        StartOptions::default().with_viewport(Viewport::new(80, 10, 8, 16).unwrap()),
    )
    .unwrap();
    let running = Running { peer, sink, handle };
    running.frame(|_| true);
    running
}

fn counting() -> (Arc<Counting>, ParkOptions) {
    counting_after(IDLE)
}

fn counting_after(idle: Duration) -> (Arc<Counting>, ParkOptions) {
    let store = Arc::new(Counting::default());
    let options = ParkOptions::default()
        .with_idle(idle)
        .with_shared_store(Arc::clone(&store) as Arc<dyn ParkStore>);
    (store, options)
}

impl Running {
    /// Grants a frame and waits for one that satisfies `done`.
    fn frame(&self, done: impl Fn(&TerminalSnapshot) -> bool) -> Arc<TerminalSnapshot> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(snapshot) = self.sink.take() {
                if done(&snapshot) {
                    return snapshot;
                }
                self.handle.request_frame();
            }
            assert!(Instant::now() < deadline, "timed out waiting for a frame");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Writes 200 numbered lines, so the 10-row screen leaves the rest in
    /// scrollback, and waits until they show.
    fn fill(&self) {
        let mut text = String::new();
        for line in 0..200 {
            writeln!(text, "line {line}\r").unwrap();
        }
        text.push_str("$ ");
        self.peer.output(text.as_bytes());
        self.handle.request_frame();
        self.frame(|s| s.frame.text().contains("line 199"));
    }

    /// The screen as a fresh, forced frame shows it.
    fn screen(&self) -> String {
        self.handle.set_visible(false);
        self.handle.set_visible(true);
        self.frame(|_| true).frame.text()
    }
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_event(peer: &StreamPeer, event: &StreamEvent) {
    wait_until("the stream event", || {
        std::iter::from_fn(|| peer.events().try_recv().ok()).any(|seen| &seen == event)
    });
}

#[test]
fn an_idle_terminal_parks_and_output_restores_the_same_screen_and_scrollback() {
    let (store, options) = counting();
    let terminal = start(options);
    terminal.fill();
    let before = terminal.screen();
    wait_until("it parks", || store.saves() == 1);
    assert!(store.inner.len_bytes() > 0, "the snapshot is in the store");

    terminal.peer.output(b"after");
    terminal.handle.request_frame();
    let after = terminal.frame(|s| s.frame.text().contains("after"));
    assert_eq!(store.takes(), 1, "output restores it");
    assert_eq!(
        store.inner.len_bytes(),
        0,
        "the store gives the snapshot up"
    );
    assert_eq!(
        after.frame.text().replace("$ after", "$"),
        before,
        "the same screen"
    );

    // The scrollback came back too.
    terminal
        .handle
        .input(TerminalInput::Scroll(ScrollRequest::Top))
        .unwrap();
    terminal.handle.request_frame();
    let top = terminal.frame(|s| s.frame.text().starts_with("line 0\n"));
    assert!(top.frame.text().contains("line 9"));
    terminal.handle.close();
}

#[test]
fn a_paint_input_or_resize_restores_a_parked_terminal() {
    let (store, options) = counting();
    let terminal = start(options);
    terminal.fill();
    let before = terminal.screen();

    wait_until("it parks", || store.saves() == 1);
    terminal.handle.request_frame();
    wait_until("a paint restores it", || store.takes() == 1);
    assert_eq!(terminal.screen(), before);

    wait_until("it parks again", || store.saves() == 2);
    terminal
        .handle
        .input(TerminalInput::Text("a".into()))
        .unwrap();
    wait_event(&terminal.peer, &StreamEvent::Input(b"a".to_vec()));
    assert_eq!(store.takes(), 2, "input restores it");

    wait_until("it parks a third time", || store.saves() == 3);
    let bigger = Viewport::new(100, 20, 8, 16).unwrap();
    terminal.handle.resize(bigger);
    terminal.handle.request_frame();
    let resized = terminal.frame(|s| s.frame.viewport == bigger);
    assert_eq!(store.takes(), 3, "a resize restores it");
    assert!(resized.frame.text().contains("line 199"));
    terminal.handle.close();
}

#[test]
fn a_busy_or_painted_terminal_never_parks() {
    let (store, options) = counting_after(LONG_IDLE);
    let terminal = start(options);
    // Output faster than the idle period.
    let until = Instant::now() + LONG_IDLE * 2;
    while Instant::now() < until {
        terminal.peer.output(b"tick\r\n");
        let _ = terminal.sink.take();
        terminal.handle.request_frame();
        std::thread::sleep(TICK);
    }
    assert_eq!(store.saves(), 0, "busy");
    // Painted faster than the idle period, as a blinking cursor is.
    let until = Instant::now() + LONG_IDLE * 2;
    while Instant::now() < until {
        let _ = terminal.sink.take();
        terminal.handle.request_frame();
        std::thread::sleep(TICK);
    }
    assert_eq!(store.saves(), 0, "painted");
    terminal.handle.close();
}

#[test]
fn parking_can_be_turned_off() {
    let (store, options) = counting();
    let terminal = start(options.with_enabled(false));
    terminal.fill();
    std::thread::sleep(IDLE * 4);
    assert_eq!(store.saves(), 0);
    terminal.handle.close();
}

#[test]
fn a_dir_store_holds_the_snapshot_in_a_file_until_the_terminal_restores() {
    let dir = std::env::temp_dir().join(format!("term-park-test-{}", std::process::id()));
    let store = DirStore::new(&dir).unwrap();
    let files = {
        let dir = dir.clone();
        move || std::fs::read_dir(&dir).unwrap().count()
    };
    let terminal = start(ParkOptions::default().with_idle(IDLE).with_store(store));
    terminal.fill();
    let before = terminal.screen();
    wait_until("the snapshot file", || files() == 1);
    terminal.peer.output(b"");
    terminal.handle.request_frame();
    wait_until("the restore removes it", || files() == 0);
    assert_eq!(terminal.screen(), before);
    wait_until("parked again", || files() == 1);
    terminal.handle.close();
    wait_until("a close removes it", || files() == 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn closing_a_parked_terminal_forgets_its_snapshot() {
    let (store, options) = counting();
    let terminal = start(options);
    terminal.fill();
    wait_until("it parks", || store.saves() == 1);
    terminal.handle.close();
    wait_until("the store forgets it", || {
        store.removes.load(Ordering::SeqCst) == 1
    });
    assert_eq!(store.inner.len_bytes(), 0);
    let _ = Mutex::new(());
}

#[test]
fn terminals_that_share_a_store_park_and_restore_on_their_own() {
    let (store, options) = counting_after(LONG_IDLE);
    let idle = start(options.clone());
    let busy = start(options);
    idle.fill();
    busy.fill();
    let idle_screen = idle.screen();
    // Keep one painted while the other goes idle, and a period more.
    let paint = |until: Instant| {
        while Instant::now() < until {
            let _ = busy.sink.take();
            busy.handle.request_frame();
            std::thread::sleep(TICK);
        }
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while store.saves() == 0 {
        assert!(Instant::now() < deadline, "timed out: the idle one parks");
        paint(Instant::now() + TICK);
    }
    paint(Instant::now() + LONG_IDLE * 2);
    assert_eq!(store.saves(), 1, "only the idle one parked");

    busy.peer.output(b"more");
    busy.handle.request_frame();
    busy.frame(|s| s.frame.text().contains("more"));
    assert_eq!(
        store.takes(),
        0,
        "the busy one's output left the other parked"
    );

    idle.handle.request_frame();
    wait_until("the idle one restores", || store.takes() == 1);
    assert_eq!(idle.screen(), idle_screen, "its own screen");
    idle.handle.close();
    busy.handle.close();
}
