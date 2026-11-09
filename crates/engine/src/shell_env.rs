//! Read the user's login-shell environment.
//!
//! Apps started from Finder or the Dock get launchd's bare PATH, so exec auth
//! plugins like `aws` or `kubelogin` are not found. We run the login shell
//! once at startup, print its environment between two markers, and parse it.
//!
//! This module only reads and parses. Changing the process environment is
//! the app's job, done before any thread starts.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Printed before the environment block.
pub const START_MARKER: &str = "__APP_ENV_START__";
/// Printed after the environment block.
pub const END_MARKER: &str = "__APP_ENV_END__";

/// Why the login environment could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ShellEnvError {
    /// The shell could not be started.
    #[error("could not start {shell}: {source}")]
    Spawn {
        /// The shell binary.
        shell: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The shell took too long and was killed.
    #[error("{shell} did not finish within {timeout:?}")]
    Timeout {
        /// The shell binary.
        shell: String,
        /// How long we waited.
        timeout: Duration,
    },
    /// Waiting for the shell failed.
    #[error("waiting for the shell failed: {0}")]
    Wait(#[source] std::io::Error),
    /// The output had no environment block between the markers.
    #[error("the shell printed no environment block")]
    NoBlock,
}

/// Runs `shell` as an interactive login shell and returns its environment.
///
/// Interactive mode matters: many people set PATH in `.zshrc`, which only
/// interactive shells read. Stdin is `/dev/null` so nothing can wait on it.
/// The shell is killed after `timeout`.
///
/// This blocks the calling thread. Call it before the async runtime starts.
pub fn capture_login_env(
    shell: &Path,
    timeout: Duration,
) -> Result<HashMap<String, String>, ShellEnvError> {
    let shell_name = shell.display().to_string();
    let script = format!("printf '%s' '{START_MARKER}'; env -0; printf '%s' '{END_MARKER}'");
    let mut child = Command::new(shell)
        .args(["-l", "-i", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| ShellEnvError::Spawn {
            shell: shell_name.clone(),
            source,
        })?;

    // Read stdout on its own thread so a chatty shell cannot fill the pipe
    // and deadlock while we wait for it to exit.
    let reader = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            // A read error leaves a partial buffer; the marker check catches it.
            let _ = out.read_to_end(&mut buf);
            buf
        })
    });

    let started = Instant::now();
    loop {
        match child.try_wait().map_err(ShellEnvError::Wait)? {
            Some(_status) => break,
            None if started.elapsed() >= timeout => {
                // Best effort: the process may have exited in between.
                let _ = child.kill();
                let _ = child.wait();
                return Err(ShellEnvError::Timeout {
                    shell: shell_name,
                    timeout,
                });
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }

    let output = reader
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    parse_env_block(&output).ok_or(ShellEnvError::NoBlock)
}

/// Extracts `KEY=value` pairs from the NUL-separated block between the
/// markers. Returns `None` when the markers are missing.
///
/// Text before the start marker (banners, motd) and after the end marker is
/// ignored. Values may contain `=` and newlines. Entries without `=` and
/// entries that are not valid UTF-8 are skipped.
pub fn parse_env_block(output: &[u8]) -> Option<HashMap<String, String>> {
    let start = find(output, START_MARKER.as_bytes())? + START_MARKER.len();
    let rest = &output[start..];
    let end = rfind(rest, END_MARKER.as_bytes())?;
    let block = &rest[..end];

    let vars = block
        .split(|&b| b == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .filter_map(|entry| entry.split_once('='))
        .filter(|(key, _)| !key.is_empty())
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    Some(vars)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(entries: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(START_MARKER.as_bytes());
        for entry in entries {
            out.extend_from_slice(entry.as_bytes());
            out.push(0);
        }
        out.extend_from_slice(END_MARKER.as_bytes());
        out
    }

    #[test]
    fn parses_simple_block() {
        let env = parse_env_block(&block(&["PATH=/usr/bin:/bin", "HOME=/Users/me"])).unwrap();
        assert_eq!(env["PATH"], "/usr/bin:/bin");
        assert_eq!(env["HOME"], "/Users/me");
        assert_eq!(env.len(), 2);
    }

    #[test]
    fn ignores_banners_around_the_block() {
        let mut out = b"Welcome to zsh!\nLast login: today\n".to_vec();
        out.extend(block(&["PATH=/opt/homebrew/bin:/usr/bin"]));
        out.extend_from_slice(b"\nbye\n");
        let env = parse_env_block(&out).unwrap();
        assert_eq!(env["PATH"], "/opt/homebrew/bin:/usr/bin");
    }

    #[test]
    fn keeps_equals_and_newlines_in_values() {
        let env = parse_env_block(&block(&["OPTS=a=b=c", "MULTI=line one\nline two"])).unwrap();
        assert_eq!(env["OPTS"], "a=b=c");
        assert_eq!(env["MULTI"], "line one\nline two");
    }

    #[test]
    fn skips_malformed_entries() {
        let mut out = START_MARKER.as_bytes().to_vec();
        out.extend_from_slice(b"NOEQUALS\0=novalue\0BAD=\xff\xfe\0GOOD=yes\0");
        out.extend_from_slice(END_MARKER.as_bytes());
        let env = parse_env_block(&out).unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env["GOOD"], "yes");
    }

    #[test]
    fn empty_value_is_kept() {
        let env = parse_env_block(&block(&["EMPTY="])).unwrap();
        assert_eq!(env["EMPTY"], "");
    }

    #[test]
    fn missing_markers_is_none() {
        assert!(parse_env_block(b"PATH=/usr/bin\0").is_none());
        assert!(parse_env_block(START_MARKER.as_bytes()).is_none());
        let mut only_end = b"PATH=/bin\0".to_vec();
        only_end.extend_from_slice(END_MARKER.as_bytes());
        assert!(parse_env_block(&only_end).is_none());
    }

    #[test]
    fn marker_text_inside_a_value_does_not_cut_the_block() {
        let value = format!("ECHO=say {END_MARKER} please");
        let env = parse_env_block(&block(&[&value, "PATH=/bin"])).unwrap();
        assert_eq!(env["PATH"], "/bin");
        assert_eq!(env["ECHO"], format!("say {END_MARKER} please"));
    }

    #[test]
    fn captures_from_a_real_shell() {
        // /bin/sh exists on every macOS and Linux machine.
        let env = capture_login_env(Path::new("/bin/sh"), Duration::from_secs(10)).unwrap();
        assert!(env.contains_key("PATH"));
    }

    #[test]
    fn missing_shell_is_a_spawn_error() {
        let err = capture_login_env(Path::new("/no/such/shell"), Duration::from_secs(1));
        assert!(matches!(err, Err(ShellEnvError::Spawn { .. })));
    }
}
