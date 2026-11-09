//! The terminal engine with a real pty. Skips when no POSIX shell is
//! available.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns may panic; clippy.toml only covers #[test]"
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use term::{
    Engine, EngineOptions, FrameSink, FrameSource, LocalPty, LocalTerminalOptions,
    ShellIntegration, StartOptions, TerminalSnapshot, TerminalStatus, Viewport,
};

fn shell() -> Option<&'static str> {
    ["/bin/sh", "/usr/bin/sh"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
}

/// Wait until a snapshot satisfies `done`, or fail with the last frame.
fn wait_for(
    sink: &FrameSink,
    wake: &gpui_kit::base::async_util::Receiver<()>,
    handle: &dyn term::FrameHandle,
    done: impl Fn(&TerminalSnapshot) -> bool,
) -> Arc<TerminalSnapshot> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last: Option<Arc<TerminalSnapshot>> = None;
    while Instant::now() < deadline {
        let _ = wake.recv_blocking_timeout(Duration::from_millis(50));
        if let Some(snapshot) = sink.take() {
            let hit = done(&snapshot);
            last = Some(snapshot);
            handle.request_frame();
            if hit {
                return last.unwrap();
            }
        }
    }
    panic!(
        "timed out; last frame:\n{}",
        last.map_or_else(String::new, |s| s.frame.text())
    );
}

trait RecvTimeout {
    fn recv_blocking_timeout(&self, timeout: Duration) -> bool;
}

impl RecvTimeout for gpui_kit::base::async_util::Receiver<()> {
    fn recv_blocking_timeout(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.try_recv().is_ok() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn a_resize_reaches_the_program_and_the_grid_together() {
    let Some(shell) = shell() else {
        eprintln!("skipping: no POSIX shell");
        return;
    };
    let options = LocalTerminalOptions::default()
        .with_program(shell, ["-c", "while true; do stty size; sleep 0.2; done"])
        .with_shell_integration(ShellIntegration::None);
    let (sink, wake) = FrameSink::new();
    let handle = Box::new(Engine::new(
        LocalPty::new(options),
        EngineOptions::default(),
    ))
    .start(
        sink.clone(),
        StartOptions::default().with_viewport(Viewport::new(60, 10, 8, 16).unwrap()),
    )
    .unwrap();
    let first = wait_for(&sink, &wake, handle.as_ref(), |s| {
        s.frame.text().contains("10 60")
    });
    assert_eq!(first.status, TerminalStatus::Live);

    handle.resize(Viewport::new(100, 30, 8, 16).unwrap());
    let resized = wait_for(&sink, &wake, handle.as_ref(), |s| {
        s.frame.text().contains("30 100")
    });
    assert_eq!(resized.frame.viewport.rows(), 30);
    assert_eq!(resized.frame.rows.len(), 30);

    handle.close();
    let exited = wait_for(&sink, &wake, handle.as_ref(), |s| {
        matches!(s.status, TerminalStatus::Exited(_))
    });
    assert!(matches!(exited.status, TerminalStatus::Exited(_)));
}

#[test]
fn closing_the_terminal_ends_the_jobs_its_shell_started() {
    let Some(shell) = shell() else {
        eprintln!("skipping: no POSIX shell");
        return;
    };
    // A job in its own process group, as a shell with job control starts it.
    let options = LocalTerminalOptions::default()
        .with_program(
            shell,
            ["-c", "set -m; sleep 300 & echo JOB $!; exec sleep 300"],
        )
        .with_shell_integration(ShellIntegration::None);
    let (sink, wake) = FrameSink::new();
    let handle = Box::new(Engine::new(
        LocalPty::new(options),
        EngineOptions::default(),
    ))
    .start(sink.clone(), StartOptions::default())
    .unwrap();
    let shown = wait_for(&sink, &wake, handle.as_ref(), |s| {
        s.frame.text().contains("JOB ")
    });
    let job: i32 = shown
        .frame
        .text()
        .split("JOB ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|pid| pid.parse().ok())
        .expect("the job's pid");
    let alive = |pid: i32| {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    };
    assert!(alive(job));
    handle.close();
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(job) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(job), "the job ended with the terminal");
}

#[test]
fn two_local_terminals_have_their_own_programs() {
    let Some(shell) = shell() else {
        eprintln!("skipping: no POSIX shell");
        return;
    };
    let start = || {
        let options = LocalTerminalOptions::default()
            .with_program(shell, ["-c", "echo PID $$; exec cat"])
            .with_shell_integration(ShellIntegration::None);
        let (sink, wake) = FrameSink::new();
        let handle = Box::new(Engine::new(
            LocalPty::new(options),
            EngineOptions::default(),
        ))
        .start(sink.clone(), StartOptions::default())
        .unwrap();
        (sink, wake, handle)
    };
    let (sink_a, wake_a, a) = start();
    let (sink_b, wake_b, b) = start();
    let pid = |s: &TerminalSnapshot| {
        s.frame
            .text()
            .split("PID ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next().map(str::to_owned))
    };
    let pid_a = pid(&wait_for(&sink_a, &wake_a, a.as_ref(), |s| {
        pid(s).is_some()
    }));
    let pid_b = pid(&wait_for(&sink_b, &wake_b, b.as_ref(), |s| {
        pid(s).is_some()
    }));
    assert_ne!(pid_a, pid_b, "a pty and a process each");

    a.input(term::TerminalInput::Text("only-a\r".into()))
        .unwrap();
    wait_for(&sink_a, &wake_a, a.as_ref(), |s| {
        s.frame.text().contains("only-a")
    });
    a.close();
    wait_for(&sink_a, &wake_a, a.as_ref(), |s| {
        matches!(s.status, TerminalStatus::Exited(_))
    });

    b.input(term::TerminalInput::Text("still-b\r".into()))
        .unwrap();
    let shown = wait_for(&sink_b, &wake_b, b.as_ref(), |s| {
        s.frame.text().contains("still-b")
    });
    assert_eq!(
        shown.status,
        TerminalStatus::Live,
        "closing one left the other"
    );
    assert!(!shown.frame.text().contains("only-a"));
    b.close();
}

#[test]
fn the_foreground_process_follows_the_program_the_shell_runs() {
    let Some(shell) = shell() else {
        eprintln!("skipping: no POSIX shell");
        return;
    };
    let dir = std::env::temp_dir().canonicalize().unwrap();
    let options = LocalTerminalOptions::default()
        .with_program(shell, ["-c", "echo READY; read x; exec /bin/cat"])
        .with_cwd(term::WorkingDirectory::Path(dir.clone()))
        .with_shell_integration(ShellIntegration::None);
    let (sink, wake) = FrameSink::new();
    let handle = Box::new(Engine::new(
        LocalPty::new(options),
        EngineOptions::default(),
    ))
    .start(sink.clone(), StartOptions::default())
    .unwrap();
    let named = |name: &'static str| {
        move |s: &TerminalSnapshot| s.foreground.as_ref().is_some_and(|p| p.name() == name)
    };
    // macOS runs /bin/sh as bash, so the shell is known by its argv.
    let at_shell = wait_for(&sink, &wake, handle.as_ref(), |s| s.foreground.is_some());
    let process = at_shell.foreground.as_ref().unwrap();
    assert!(process.is_shell(), "{process:?}");
    assert_eq!(process.argv().first().map(String::as_str), Some(shell));
    assert_eq!(
        process.cwd().map(|cwd| cwd.canonicalize().unwrap()),
        Some(dir),
        "the working directory, with no shell integration to report it"
    );

    // The shell execs cat; each line cat echoes is output, which is what
    // makes the engine look again.
    handle
        .input(term::TerminalInput::Text("go\r".into()))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let in_cat = loop {
        handle
            .input(term::TerminalInput::Text("ping\r".into()))
            .unwrap();
        let snapshot = wait_for(&sink, &wake, handle.as_ref(), |_| true);
        if named("cat")(&snapshot) {
            break snapshot;
        }
        assert!(Instant::now() < deadline, "cat never took the foreground");
        std::thread::sleep(Duration::from_millis(60));
    };
    let process = in_cat.foreground.as_ref().unwrap();
    assert!(!process.is_shell(), "{process:?}");
    assert_eq!(process.argv(), ["/bin/cat"]);
    handle.close();
}

#[test]
fn a_large_paste_into_a_program_busy_writing_does_not_deadlock() {
    let Some(shell) = shell() else {
        eprintln!("skipping: no POSIX shell");
        return;
    };
    // The program floods 8 MB of output before it reads any input, more
    // than the engine queues and the pty buffers, so the paste and the
    // output each wait on the other unless neither blocks the engine.
    const PASTE: usize = 900_000;
    let script = format!(
        "stty raw -echo; echo RAW; sleep 0.2; head -c 8000000 /dev/zero | tr '\\0' x; echo; \
         echo FLOODED; head -c {PASTE} | wc -c; echo DONE"
    );
    let options = LocalTerminalOptions::default()
        .with_program(shell, ["-c", script.as_str()])
        .with_shell_integration(ShellIntegration::None);
    let (sink, wake) = FrameSink::new();
    let handle = Box::new(Engine::new(
        LocalPty::new(options),
        EngineOptions::default(),
    ))
    .start(sink.clone(), StartOptions::default())
    .unwrap();
    // In canonical mode the line discipline drops a line this long.
    wait_for(&sink, &wake, handle.as_ref(), |s| {
        s.frame.text().contains("RAW")
    });
    handle
        .input(term::TerminalInput::Paste("y".repeat(PASTE)))
        .unwrap();
    let done = wait_for(&sink, &wake, handle.as_ref(), |s| {
        s.frame.text().contains("DONE")
    });
    let text = done.frame.text();
    assert!(text.contains("FLOODED"), "{text}");
    assert!(
        text.contains(&PASTE.to_string()),
        "every pasted byte arrived:\n{text}"
    );
    handle.close();
}
