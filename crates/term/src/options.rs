//! Options for the engine and for a local terminal.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Engine limits, with Ghostty's defaults: 10,000,000 bytes of scrollback
/// (`scrollback-limit` in `ghostty +show-config --default`), so a busy
/// terminal holds as much history as Ghostty's and no more.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EngineOptions {
    /// Maximum scrollback in bytes. Zero disables scrollback.
    pub(crate) scrollback_bytes: usize,
    /// Maximum scrollback in lines; `None` is unlimited.
    pub(crate) scrollback_lines: Option<usize>,
    /// The terminfo name reported to XTGETTCAP.
    pub(crate) terminfo_name: String,
    /// Maximum decoded bytes per Kitty clipboard write.
    pub(crate) clipboard_write_max_bytes: usize,
    pub(crate) park: crate::park::ParkOptions,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            scrollback_bytes: 10_000_000,
            scrollback_lines: None,
            terminfo_name: ghostty_vt::terminfo::TERM.to_owned(),
            clipboard_write_max_bytes: 64 << 20,
            park: crate::park::ParkOptions::default(),
        }
    }
}

impl EngineOptions {
    /// Set the scrollback byte limit.
    #[must_use]
    pub fn with_scrollback_bytes(mut self, bytes: usize) -> Self {
        self.scrollback_bytes = bytes;
        self
    }

    /// Set the scrollback line limit.
    #[must_use]
    pub fn with_scrollback_lines(mut self, lines: Option<usize>) -> Self {
        self.scrollback_lines = lines;
        self
    }

    /// Set the terminfo name.
    #[must_use]
    pub fn with_terminfo_name(mut self, name: impl Into<String>) -> Self {
        self.terminfo_name = name.into();
        self
    }

    /// Set the largest decoded Kitty clipboard write.
    #[must_use]
    pub fn with_clipboard_write_max_bytes(mut self, bytes: usize) -> Self {
        self.clipboard_write_max_bytes = bytes;
        self
    }

    /// Set when an idle terminal parks and where its snapshot goes. On by
    /// default, after 60 seconds idle, in
    /// memory.
    #[must_use]
    pub fn with_park(mut self, park: crate::park::ParkOptions) -> Self {
        self.park = park;
        self
    }

    /// When an idle terminal parks and where its snapshot goes.
    pub fn park(&self) -> &crate::park::ParkOptions {
        &self.park
    }

    /// The scrollback byte limit. Zero disables scrollback.
    pub fn scrollback_bytes(&self) -> usize {
        self.scrollback_bytes
    }

    /// The scrollback line limit; `None` is unlimited.
    pub fn scrollback_lines(&self) -> Option<usize> {
        self.scrollback_lines
    }

    /// The terminfo name reported to XTGETTCAP.
    pub fn terminfo_name(&self) -> &str {
        &self.terminfo_name
    }

    /// The largest decoded Kitty clipboard write.
    pub fn clipboard_write_max_bytes(&self) -> usize {
        self.clipboard_write_max_bytes
    }
}

/// Where a local terminal starts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum WorkingDirectory {
    /// The host process's working directory.
    #[default]
    Inherit,
    /// The user's home directory.
    Home,
    /// A path; a missing path falls back to home, then the root.
    Path(PathBuf),
}

/// Which shell integration script to inject.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShellIntegration {
    /// Pick by the program's file name.
    #[default]
    Detect,
    /// Inject nothing.
    None,
    /// Bash through `--posix` and `ENV`.
    Bash,
    /// Zsh through `ZDOTDIR`.
    Zsh,
    /// Fish through `XDG_DATA_DIRS`.
    Fish,
    /// Elvish through `XDG_DATA_DIRS`.
    Elvish,
    /// Nushell through `XDG_DATA_DIRS` and `--execute`.
    Nushell,
}

/// Options for a local terminal with its own pty.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LocalTerminalOptions {
    /// Engine limits.
    pub(crate) engine: EngineOptions,
    /// The program; `None` runs the user's shell.
    pub(crate) program: Option<PathBuf>,
    /// Arguments for the program.
    pub(crate) args: Vec<String>,
    /// Working directory.
    pub(crate) cwd: WorkingDirectory,
    /// Extra environment variables.
    pub(crate) env: BTreeMap<String, String>,
    /// Environment variables to remove.
    pub(crate) env_remove: Vec<String>,
    /// Bytes written to the program right after it starts.
    pub(crate) input: Vec<u8>,
    /// Shell integration.
    pub(crate) shell_integration: ShellIntegration,
    /// Shell integration features, as `GHOSTTY_SHELL_FEATURES`.
    pub(crate) shell_integration_features: Vec<String>,
    /// Start the shell as a login shell.
    pub(crate) login: bool,
    /// An exit faster than this is reported as abnormal.
    pub(crate) abnormal_exit_runtime: Duration,
}

impl Default for LocalTerminalOptions {
    fn default() -> Self {
        Self {
            engine: EngineOptions::default(),
            program: None,
            args: Vec::new(),
            cwd: WorkingDirectory::Inherit,
            env: BTreeMap::new(),
            env_remove: Vec::new(),
            input: Vec::new(),
            shell_integration: ShellIntegration::Detect,
            shell_integration_features: vec![
                "cursor".to_owned(),
                "sudo".to_owned(),
                "title".to_owned(),
            ],
            login: true,
            abnormal_exit_runtime: Duration::from_millis(250),
        }
    }
}

impl LocalTerminalOptions {
    /// Set the program and its arguments.
    #[must_use]
    pub fn with_program(
        mut self,
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.program = Some(program.into());
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Set the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: WorkingDirectory) -> Self {
        self.cwd = cwd;
        self
    }

    /// Add an environment variable.
    #[must_use]
    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    /// Set the shell integration.
    #[must_use]
    pub fn with_shell_integration(mut self, integration: ShellIntegration) -> Self {
        self.shell_integration = integration;
        self
    }

    /// Set the engine options.
    #[must_use]
    pub fn with_engine(mut self, engine: EngineOptions) -> Self {
        self.engine = engine;
        self
    }

    /// Set whether the shell starts as a login shell.
    #[must_use]
    pub fn with_login(mut self, login: bool) -> Self {
        self.login = login;
        self
    }

    /// Set the bytes written right after the program starts.
    #[must_use]
    pub fn with_input(mut self, input: impl Into<Vec<u8>>) -> Self {
        self.input = input.into();
        self
    }

    /// Remove an environment variable from the program's environment.
    #[must_use]
    pub fn without_env(mut self, name: impl Into<String>) -> Self {
        self.env_remove.push(name.into());
        self
    }

    /// Set the shell integration features, as `GHOSTTY_SHELL_FEATURES`.
    #[must_use]
    pub fn with_shell_integration_features(
        mut self,
        features: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.shell_integration_features = features.into_iter().map(Into::into).collect();
        self
    }

    /// Set the run time under which an exit is reported as abnormal.
    #[must_use]
    pub fn with_abnormal_exit_runtime(mut self, runtime: Duration) -> Self {
        self.abnormal_exit_runtime = runtime;
        self
    }

    /// The engine options.
    pub fn engine(&self) -> &EngineOptions {
        &self.engine
    }

    /// The program; `None` runs the user's shell.
    pub fn program(&self) -> Option<&std::path::Path> {
        self.program.as_deref()
    }

    /// The program's arguments.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// The working directory.
    pub fn cwd(&self) -> &WorkingDirectory {
        &self.cwd
    }

    /// Extra environment variables.
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// Environment variables removed from the program's environment.
    pub fn env_remove(&self) -> &[String] {
        &self.env_remove
    }

    /// The bytes written right after the program starts.
    pub fn input(&self) -> &[u8] {
        &self.input
    }

    /// The shell integration.
    pub fn shell_integration(&self) -> ShellIntegration {
        self.shell_integration
    }

    /// The shell integration features.
    pub fn shell_integration_features(&self) -> &[String] {
        &self.shell_integration_features
    }

    /// Whether the shell starts as a login shell.
    pub fn login(&self) -> bool {
        self.login
    }

    /// The run time under which an exit is reported as abnormal.
    pub fn abnormal_exit_runtime(&self) -> Duration {
        self.abnormal_exit_runtime
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_scrollback_limit_is_ghosttys_default() {
        assert_eq!(
            super::EngineOptions::default().scrollback_bytes(),
            10_000_000
        );
    }
}
