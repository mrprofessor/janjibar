//! A local program in a pty, as a [`ByteSource`].
//!
//! The child environment follows Ghostty: `TERM=xterm-ghostty` with the
//! shipped terminfo database, `COLORTERM`, `TERM_PROGRAM`, the shell
//! integration scripts injected per shell, and the terminal-specific
//! variables of other emulators removed.
//!
//! Derived from Herdr (Apache-2.0), through gpui-cn.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::engine::{ByteHandle, ByteSink, ByteSource, ExitStatus, ForegroundProcess};
use crate::frame::Viewport;
use crate::options::{LocalTerminalOptions, ShellIntegration, WorkingDirectory};

/// A program to run in a local pty.
#[derive(Debug)]
pub struct LocalPty {
    options: LocalTerminalOptions,
}

impl LocalPty {
    /// A pty for `options`.
    #[must_use]
    pub fn new(options: LocalTerminalOptions) -> Self {
        Self { options }
    }
}

fn size(viewport: Viewport) -> PtySize {
    PtySize {
        rows: viewport.rows(),
        cols: viewport.columns(),
        pixel_width: u16::try_from(u32::from(viewport.columns()) * viewport.cell_width())
            .unwrap_or(u16::MAX),
        pixel_height: u16::try_from(u32::from(viewport.rows()) * viewport.cell_height())
            .unwrap_or(u16::MAX),
    }
}

fn detect_shell(program: &Path) -> ShellIntegration {
    match program.file_name().and_then(|n| n.to_str()) {
        Some("bash") => ShellIntegration::Bash,
        Some("zsh") => ShellIntegration::Zsh,
        Some("fish") => ShellIntegration::Fish,
        Some("elvish") => ShellIntegration::Elvish,
        Some("nu") => ShellIntegration::Nushell,
        _ => ShellIntegration::None,
    }
}

fn prepend_path_list(existing: Option<std::ffi::OsString>, dir: &Path, default: &str) -> String {
    let existing = existing
        .map(|v| v.to_string_lossy().into_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_owned());
    format!("{}:{existing}", dir.display())
}

/// The user's home, from `HOME`.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Build the command, following Ghostty's environment rules.
fn command(options: &LocalTerminalOptions, resources: &Path, terminfo: &Path) -> CommandBuilder {
    let program = options
        .program
        .clone()
        .unwrap_or_else(crate::shell::default_shell);
    let integration = match options.shell_integration {
        ShellIntegration::Detect => detect_shell(&program),
        other => other,
    };
    let mut args: Vec<String> = options.args.clone();
    let mut cmd = CommandBuilder::new(&program);

    match &options.cwd {
        WorkingDirectory::Inherit => {}
        WorkingDirectory::Home => {
            if let Some(home) = home_dir() {
                cmd.cwd(home);
            }
        }
        WorkingDirectory::Path(path) => {
            let fallback = home_dir();
            if path.is_dir() {
                cmd.cwd(path);
            } else if let Some(home) = fallback.filter(|h| h.is_dir()) {
                cmd.cwd(home);
            } else {
                cmd.cwd("/");
            }
        }
    }

    for name in crate::shell::FOREIGN_TERMINAL_ENV {
        cmd.env_remove(name);
    }
    for name in &options.env_remove {
        cmd.env_remove(name);
    }
    cmd.env("TERM", ghostty_vt::terminfo::TERM);
    cmd.env("TERMINFO", terminfo);
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "ghostty");
    cmd.env(
        "TERM_PROGRAM_VERSION",
        ghostty_vt::build_info::pinned_version(),
    );
    cmd.env("GHOSTTY_RESOURCES_DIR", resources);
    if std::env::var_os("LANG").is_none_or(|v| v.is_empty()) {
        cmd.env("LANG", "en_US.UTF-8");
    }
    if options.program.is_none() {
        cmd.env("SHELL", &program);
    }
    if !options.shell_integration_features.is_empty() && integration != ShellIntegration::None {
        cmd.env(
            "GHOSTTY_SHELL_FEATURES",
            options.shell_integration_features.join(","),
        );
    }

    let integration_dir = resources.join("shell-integration");
    match integration {
        ShellIntegration::None | ShellIntegration::Detect => {}
        ShellIntegration::Bash => {
            // Ghostty starts bash in POSIX mode with ENV pointing at its
            // script, which then loads the normal startup files itself.
            if let Some(env) = std::env::var_os("ENV") {
                cmd.env("GHOSTTY_BASH_ENV", env);
            }
            cmd.env("ENV", integration_dir.join("bash").join("ghostty.bash"));
            let mut inject = String::from("1");
            if options.login {
                inject.push_str(" --login");
            }
            cmd.env("GHOSTTY_BASH_INJECT", inject);
            if std::env::var_os("HISTFILE").is_none()
                && let Some(home) = std::env::var_os("HOME")
            {
                cmd.env("HISTFILE", PathBuf::from(home).join(".bash_history"));
                cmd.env("GHOSTTY_BASH_UNEXPORT_HISTFILE", "1");
            }
            args.insert(0, "--posix".to_owned());
        }
        ShellIntegration::Zsh => {
            if let Some(old) = std::env::var_os("ZDOTDIR") {
                cmd.env("GHOSTTY_ZSH_ZDOTDIR", old);
            }
            cmd.env("ZDOTDIR", integration_dir.join("zsh"));
        }
        ShellIntegration::Fish | ShellIntegration::Elvish | ShellIntegration::Nushell => {
            cmd.env("GHOSTTY_SHELL_INTEGRATION_XDG_DIR", &integration_dir);
            cmd.env(
                "XDG_DATA_DIRS",
                prepend_path_list(
                    std::env::var_os("XDG_DATA_DIRS"),
                    &integration_dir,
                    "/usr/local/share:/usr/share",
                ),
            );
            if integration == ShellIntegration::Nushell {
                args.push("--execute".to_owned());
                args.push("use ghostty *".to_owned());
            }
        }
    }
    for (name, value) in &options.env {
        cmd.env(name, value);
    }
    // The user's shell starts as a login shell the way Ghostty starts it:
    // through login(1) on macOS, with `-l` elsewhere. Bash and Nushell
    // log in through their integration instead of `-l`.
    let (program, args) = if options.login && options.program.is_none() {
        if cfg!(target_os = "macos") {
            crate::shell::login_command(&program, &args).into_parts()
        } else if matches!(
            integration,
            ShellIntegration::Bash | ShellIntegration::Nushell
        ) {
            (program, args)
        } else {
            crate::shell::login_command(&program, &args).into_parts()
        }
    } else {
        (program, args)
    };
    let mut argv = vec![program.into_os_string()];
    argv.extend(args.into_iter().map(Into::into));
    *cmd.get_argv_mut() = argv;
    cmd
}

struct Handle {
    io: crate::pty_io::PollIo,
    master: Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    /// The file name of the program the terminal started, such as `zsh`.
    program: Option<String>,
    /// The last foreground process group and its process, kept while the
    /// group has the foreground.
    foreground: Mutex<Option<(libc::pid_t, ForegroundProcess)>>,
}

impl ByteHandle for Handle {
    fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        self.io.write(bytes)
    }

    fn resize(&self, viewport: Viewport) -> io::Result<()> {
        self.master
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .resize(size(viewport))
            .map_err(io::Error::other)
    }

    fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        let mut child = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(child.try_wait()?.map(|status| ExitStatus {
            code: status.exit_code().try_into().ok(),
            signal: status.signal().map(str::to_owned),
            runtime: std::time::Duration::ZERO,
            abnormal: false,
        }))
    }

    fn close(&self) {
        self.io.close();
        let mut child = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The shell leads its own session; its jobs end with it, even
        // when the shell itself has already exited, as in herdr.
        if let Some(leader) = child
            .process_id()
            .and_then(|pid| libc::pid_t::try_from(pid).ok())
        {
            crate::process::end_session(leader, || matches!(child.try_wait(), Ok(Some(_))));
            return;
        }
        if matches!(child.try_wait(), Ok(None))
            && let Err(error) = child.kill()
        {
            tracing::debug!("could not kill the terminal's program: {error}");
        }
    }

    fn foreground(&self) -> Option<ForegroundProcess> {
        use crate::process;
        let pgid = self
            .master
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .process_group_leader()?;
        let mut last = self
            .foreground
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((group, known)) = last.as_mut()
            && *group == pgid
        {
            let pid = libc::pid_t::try_from(known.pid).ok()?;
            // The same group and the same program, unless it called exec.
            if process::name(pid).as_deref() == Some(known.name.as_str()) {
                // Only a shell at its prompt changes directory.
                if known.shell {
                    known.cwd = process::cwd(pid).or(known.cwd.take());
                }
                return Some(known.clone());
            }
        }
        let described = process::describe_group(pgid)?;
        let shell = self
            .program
            .as_deref()
            .is_some_and(|program| is_program(&described, program));
        let known = ForegroundProcess {
            pid: u32::try_from(described.pid).ok()?,
            name: described.name,
            argv: described.argv,
            cwd: described.cwd,
            shell,
        };
        *last = Some((pgid, known.clone()));
        Some(known)
    }
}

/// Whether `process` runs `program`, by executable name, or by its first
/// argument, which a login shell starts with a dash, as `-zsh`.
fn is_program(process: &crate::process::Described, program: &str) -> bool {
    // Linux keeps 15 bytes of the name.
    let short = &program[..program.len().min(15)];
    process.name == program
        || process.name == short
        || process.argv.first().is_some_and(|arg0| {
            let arg0 = arg0.strip_prefix('-').unwrap_or(arg0);
            Path::new(arg0).file_name().and_then(|n| n.to_str()) == Some(program)
        })
}

impl ByteSource for LocalPty {
    fn start(
        self: Box<Self>,
        sink: ByteSink,
        viewport: Viewport,
    ) -> io::Result<Box<dyn ByteHandle>> {
        let resources = ghostty_vt::shell_integration::resources_dir()?;
        let terminfo = ghostty_vt::terminfo::dir()?;
        let cmd = command(&self.options, &resources, &terminfo);
        let program = self
            .options
            .program
            .clone()
            .unwrap_or_else(crate::shell::default_shell)
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned);
        let pty = native_pty_system()
            .openpty(size(viewport))
            .map_err(io::Error::other)?;
        let child = pty.slave.spawn_command(cmd).map_err(io::Error::other)?;
        drop(pty.slave);
        let io = crate::pty_io::PollIo::start(
            pty.master
                .as_raw_fd()
                .ok_or_else(|| io::Error::other("the pty has no descriptor"))?,
            sink,
        )?;
        Ok(Box::new(Handle {
            io,
            master: Mutex::new(pty.master),
            child: Mutex::new(child),
            program,
            foreground: Mutex::new(None),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_detection_uses_the_file_name() {
        assert_eq!(detect_shell(Path::new("/bin/zsh")), ShellIntegration::Zsh);
        assert_eq!(
            detect_shell(Path::new("/opt/homebrew/bin/fish")),
            ShellIntegration::Fish
        );
        assert_eq!(
            detect_shell(Path::new("/usr/bin/nu")),
            ShellIntegration::Nushell
        );
        assert_eq!(detect_shell(Path::new("/bin/sh")), ShellIntegration::None);
    }

    #[test]
    fn xdg_data_dirs_are_prepended_with_the_default_when_unset() {
        let dir = Path::new("/res/shell-integration");
        assert_eq!(
            prepend_path_list(None, dir, "/usr/local/share:/usr/share"),
            "/res/shell-integration:/usr/local/share:/usr/share"
        );
        assert_eq!(
            prepend_path_list(
                Some("/opt/share".into()),
                dir,
                "/usr/local/share:/usr/share"
            ),
            "/res/shell-integration:/opt/share"
        );
    }

    #[test]
    fn zsh_gets_zdotdir_and_a_login_flag() {
        let options =
            LocalTerminalOptions::default().with_program("/bin/zsh", Vec::<String>::new());
        let cmd = command(&options, Path::new("/res"), Path::new("/ti"));
        let env: std::collections::HashMap<_, _> = cmd
            .iter_extra_env_as_str()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let zdotdir = Path::new("/res").join("shell-integration").join("zsh");
        assert_eq!(env.get("ZDOTDIR").map(String::as_str), zdotdir.to_str());
        assert_eq!(env.get("TERM").map(String::as_str), Some("xterm-ghostty"));
        assert_eq!(env.get("TERMINFO").map(String::as_str), Some("/ti"));
        assert!(
            !cmd.get_argv().iter().any(|a| a == "-l"),
            "an explicit program is not made a login shell"
        );
    }
}
