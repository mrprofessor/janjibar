//! The processes behind a local terminal.
//!
//! A shell in a pty leads its own session, and every job it starts stays in
//! that session, even one in another process group. Closing the terminal
//! ends them all, as herdr does: SIGHUP, which a shell passes on to its
//! jobs, then SIGTERM, then SIGKILL, each to every process left in the
//! session, with up to 250 ms between steps.
//!
//! The foreground process is the terminal's foreground process group
//! (`tcgetpgrp` on the pty), described by its leader, or by a member when
//! the leader has exited, as the first command of a pipeline does.
//!
//! Signals and session lookups go through rustix. Reading the process
//! table on macOS (`proc_listallpids`, `proc_pidinfo`, `proc_name` and the
//! `KERN_PROCARGS2` sysctl) has no safe wrapper, so those calls use libc
//! directly, each with its own safety note.
//!
//! Derived from Herdr (Apache-2.0), through gpui-cn.

#![cfg_attr(
    target_os = "macos",
    expect(
        unsafe_code,
        reason = "the macOS process table is read through libc calls with no safe wrapper"
    )
)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal};

/// A process's name, arguments and working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Described {
    pub(crate) pid: libc::pid_t,
    pub(crate) name: String,
    pub(crate) argv: Vec<String>,
    pub(crate) cwd: Option<PathBuf>,
}

/// Describes the process group `pgid`: its leader, or a live member when
/// the leader is gone.
pub(crate) fn describe_group(pgid: libc::pid_t) -> Option<Described> {
    if pgid <= 0 {
        return None;
    }
    let pid = if alive(pgid) {
        pgid
    } else {
        all_pids()
            .into_iter()
            .find(|pid| group_of(*pid) == Some(pgid) && alive(*pid))?
    };
    Some(Described {
        pid,
        name: name(pid)?,
        argv: argv(pid).unwrap_or_default(),
        cwd: cwd(pid),
    })
}

/// The process group of `pid`.
fn group_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    let group = rustix::process::getpgid(Some(Pid::from_raw(pid)?)).ok()?;
    Some(group.as_raw_nonzero().get())
}

/// The session of `pid`.
fn session_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    let session = rustix::process::getsid(Some(Pid::from_raw(pid)?)).ok()?;
    Some(session.as_raw_nonzero().get())
}

#[cfg(target_os = "macos")]
pub(crate) fn name(pid: libc::pid_t) -> Option<String> {
    let mut buf = [0u8; 4 * libc::MAXPATHLEN as usize];
    let capacity = u32::try_from(buf.len()).ok()?;
    // SAFETY: the buffer is valid for writes of `capacity` bytes, and
    // proc_name writes at most that many.
    let len = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), capacity) };
    let len = usize::try_from(len).ok().filter(|len| *len > 0)?;
    Some(String::from_utf8_lossy(buf.get(..len)?).into_owned())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn name(pid: libc::pid_t) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(comm.trim_end_matches('\n').to_owned())
}

/// The arguments, from `KERN_PROCARGS2`: the argument count, the
/// executable path, padding, then the arguments, each ending in a NUL.
#[cfg(target_os = "macos")]
fn argv(pid: libc::pid_t) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    let mut max: libc::c_int = 0;
    let mut size = std::mem::size_of_val(&max);
    // SAFETY: the name is the two-entry MIB above, and the out pointer is
    // a c_int, whose size `size` holds.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&raw mut max).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    let mut buf = vec![0u8; usize::try_from(max).ok()?];
    let mut size = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: the name is the three-entry MIB above, and the buffer is
    // valid for writes of `size` bytes; the kernel writes back how many it
    // used.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    let buf = buf.get(..size)?;
    let count = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
    let rest = buf.get(4..)?;
    // Skip the executable path and the padding after it.
    let path_end = rest.iter().position(|b| *b == 0)?;
    let start = rest[path_end..].iter().position(|b| *b != 0)? + path_end;
    Some(
        rest[start..]
            .split(|b| *b == 0)
            .take(usize::try_from(count).ok()?)
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect(),
    )
}

#[cfg(not(target_os = "macos"))]
fn argv(pid: libc::pid_t) -> Option<Vec<String>> {
    let line = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        line.split(|b| *b == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect(),
    )
}

/// The working directory of `pid`.
#[cfg(target_os = "macos")]
pub(crate) fn cwd(pid: libc::pid_t) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    // SAFETY: proc_vnodepathinfo is plain C data, valid when all zero.
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_vnodepathinfo>()).ok()?;
    // SAFETY: the buffer is a proc_vnodepathinfo of the size given.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    // The kernel fills the path as a C string; an array with no NUL is
    // not one, so it reads as no directory.
    let raw: Vec<u8> = info
        .pvi_cdir
        .vip_path
        .iter()
        .flatten()
        .map(|c| c.cast_unsigned())
        .collect();
    let bytes = std::ffi::CStr::from_bytes_until_nul(&raw).ok()?.to_bytes();
    (!bytes.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn cwd(pid: libc::pid_t) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// How long each signal gets before the next, stronger one.
const STEP: Duration = Duration::from_millis(250);

/// Ends every process in the session `leader` leads. `reap` collects the
/// leader's exit when it has one, since a zombie still answers a signal
/// probe; it returns whether the leader has exited.
#[cfg(unix)]
pub(crate) fn end_session(leader: libc::pid_t, mut reap: impl FnMut() -> bool) {
    for signal in [Signal::HUP, Signal::TERM, Signal::KILL] {
        let pids = session(leader);
        if pids.is_empty() {
            return;
        }
        for pid in pids.iter().filter_map(|pid| Pid::from_raw(*pid)) {
            // A process that is already gone cannot be signalled, which is
            // the goal anyway.
            let _ = rustix::process::kill_process(pid, signal);
        }
        let deadline = Instant::now() + STEP;
        loop {
            reap();
            if session(leader).is_empty() || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    reap();
}

/// The live processes in session `sid`, the leader included while it
/// lives.
#[cfg(unix)]
pub(crate) fn session(sid: libc::pid_t) -> Vec<libc::pid_t> {
    all_pids()
        .into_iter()
        .filter(|pid| session_of(*pid) == Some(sid) && alive(*pid))
        .collect()
}

/// Whether `pid` is running: it exists and is not a zombie.
#[cfg(unix)]
fn alive(pid: libc::pid_t) -> bool {
    Pid::from_raw(pid).is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
        && !zombie(pid)
}

#[cfg(target_os = "macos")]
fn zombie(pid: libc::pid_t) -> bool {
    // SAFETY: proc_bsdinfo is plain C data, valid when all zero.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let Ok(size) = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()) else {
        return false;
    };
    // SAFETY: the buffer is a proc_bsdinfo of the size given.
    let read =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    // SZOMB is 5 in the BSD process states.
    read == size && info.pbi_status == 5
}

#[cfg(all(unix, not(target_os = "macos")))]
fn zombie(pid: libc::pid_t) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let state = stat.rsplit_once(')')?.1.trim_start().chars().next()?;
            Some(state == 'Z')
        })
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn all_pids() -> Vec<libc::pid_t> {
    // SAFETY: a null buffer of zero bytes asks for the count only.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Some(count) = usize::try_from(count).ok().filter(|count| *count > 0) else {
        return Vec::new();
    };
    // Room for processes started since the count.
    let mut pids: Vec<libc::pid_t> = vec![0; count + 64];
    let Ok(bytes) = libc::c_int::try_from(std::mem::size_of_val(pids.as_slice())) else {
        return Vec::new();
    };
    // SAFETY: the buffer is valid for writes of `bytes` bytes of pids.
    let found = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(usize::try_from(found).unwrap_or(0));
    pids.retain(|pid| *pid > 0);
    pids
}

#[cfg(all(unix, not(target_os = "macos")))]
fn all_pids() -> Vec<libc::pid_t> {
    std::fs::read_dir("/proc")
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::BufRead as _;
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};

    #[test]
    fn a_process_group_is_described_by_its_name_arguments_and_directory() {
        let dir = std::env::temp_dir().canonicalize().unwrap();
        let mut child = Command::new("/bin/sleep")
            .arg("300")
            .current_dir(&dir)
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as libc::pid_t;
        // Linux fills /proc/<pid>/cmdline a moment after exec returns.
        let deadline = Instant::now() + Duration::from_secs(5);
        let described = loop {
            let described = describe_group(pid).expect("described");
            if !described.argv.is_empty() || Instant::now() >= deadline {
                break described;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(described.pid, pid);
        assert_eq!(described.name, "sleep");
        assert_eq!(described.argv, ["/bin/sleep", "300"]);
        assert_eq!(
            described.cwd.map(|cwd| cwd.canonicalize().unwrap()),
            Some(dir)
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(describe_group(pid), None, "gone");
    }

    #[test]
    fn a_group_whose_leader_exited_is_described_by_a_member() {
        // `sh -c 'sleep & exec ...'` style: the leader exits, a member stays.
        let mut leader = Command::new("/bin/sh")
            .args(["-c", "/bin/sleep 300 & echo $!"])
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = leader.id() as libc::pid_t;
        let mut line = String::new();
        std::io::BufReader::new(leader.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let member: libc::pid_t = line.trim().parse().unwrap();
        leader.wait().unwrap();
        // A forked child keeps its parent's name until it calls exec.
        let deadline = Instant::now() + Duration::from_secs(5);
        let described = loop {
            let described = describe_group(pgid).expect("the member");
            if described.name == "sleep" || Instant::now() >= deadline {
                break described;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(described.pid, member);
        assert_eq!(described.name, "sleep");
        rustix::process::kill_process(Pid::from_raw(member).unwrap(), Signal::KILL).unwrap();
    }

    #[test]
    fn ending_a_session_ends_its_jobs_in_other_process_groups() {
        // A session leader, as a shell in a pty is, whose job runs in its
        // own process group, as a shell with job control starts it. Bash,
        // because dash turns `set -m` off without a terminal.
        let mut command = Command::new("/bin/bash");
        command
            .args(["-c", "set -m; sleep 300 & echo $!; exec sleep 300"])
            .stdout(Stdio::piped());
        // SAFETY: the hook only calls setsid, which is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                rustix::process::setsid()?;
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let leader = child.id() as libc::pid_t;
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let job: libc::pid_t = line.trim().parse().unwrap();
        let pids = session(leader);
        assert!(pids.contains(&leader) && pids.contains(&job), "{pids:?}");
        assert_ne!(
            group_of(job),
            Some(leader),
            "the job has its own process group"
        );

        end_session(leader, || matches!(child.try_wait(), Ok(Some(_))));
        assert!(session(leader).is_empty(), "the whole session ended");
        assert!(!alive(job));
    }
}
