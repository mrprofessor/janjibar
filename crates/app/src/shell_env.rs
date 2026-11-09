//! Copy PATH from the user's login shell into this process.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use engine::shell_env::capture_login_env;

/// How long the login shell may take. Heavy shell configs can be slow.
const SHELL_TIMEOUT: Duration = Duration::from_secs(5);

/// Replaces this process's PATH with the login shell's.
///
/// Apps opened from Finder inherit launchd's short PATH, so exec auth
/// plugins (`aws`, `kubelogin`, `gke-gcloud-auth-plugin`) are not found.
/// On any failure the inherited PATH stays and a warning is logged.
///
/// Must run before any other thread exists: before the tokio runtime,
/// before GPUI starts, before anything that might read the environment.
pub fn sync_path_from_login_shell() {
    let shell = std::env::var_os("SHELL").map_or_else(|| PathBuf::from("/bin/zsh"), PathBuf::from);
    let started = Instant::now();
    match capture_login_env(&shell, SHELL_TIMEOUT) {
        Ok(env) => match env.get("PATH") {
            Some(path) if !path.is_empty() => {
                set_path(path);
                tracing::info!(
                    shell = %shell.display(),
                    entries = std::env::split_paths(path).count(),
                    ms = started.elapsed().as_millis(),
                    "PATH copied from the login shell"
                );
            }
            _ => tracing::warn!(shell = %shell.display(), "login shell printed no PATH"),
        },
        Err(err) => tracing::warn!("keeping the inherited PATH: {err}"),
    }
}

#[expect(
    unsafe_code,
    reason = "set_var is unsafe in edition 2024; see the SAFETY note"
)]
fn set_path(path: &str) {
    // SAFETY: `set_var` is unsound only if another thread reads or writes the
    // environment at the same time. This runs first thing in `main`, before
    // the tokio runtime, GPUI or any other thread is created, and the only
    // thread we spawned (the shell output reader) has already been joined.
    unsafe { std::env::set_var("PATH", path) };
}
