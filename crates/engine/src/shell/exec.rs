//! `exec` into a pod with a TTY, pumped between the API server's websocket
//! and a terminal.

use futures::SinkExt as _;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
use kube::api::{AttachParams, AttachedProcess, TerminalSize};
use kube::{Api, Client};
use model::{ShellError, ShellEvent, ShellExit, ShellInput};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::{READ_BUFFER, ShellIo, shell_error};

/// Starts the best shell the container has, with a `TERM` its terminfo
/// knows. Containers rarely ship Ghostty's terminfo, and many set no
/// `TERM` at all, which leaves shells and editors in dumb mode. `exec`
/// replaces `sh`, so the shell's exit code is the session's.
const SHELL_SCRIPT: &str = "export TERM=xterm-256color COLORTERM=truecolor; \
     if command -v bash >/dev/null 2>&1; then exec bash; \
     elif command -v ash >/dev/null 2>&1; then exec ash; \
     else exec sh; fi";

/// The script that picks a shell, for `sh -c`.
pub(crate) fn shell_script() -> &'static str {
    SHELL_SCRIPT
}

/// The command that starts a shell in a container.
pub(crate) fn shell_command() -> Vec<String> {
    vec!["sh".to_owned(), "-c".to_owned(), SHELL_SCRIPT.to_owned()]
}

/// One `exec` with a TTY.
pub(crate) struct Exec<'a> {
    pub(crate) namespace: &'a str,
    pub(crate) pod: &'a str,
    pub(crate) container: Option<&'a str>,
    pub(crate) command: Vec<String>,
    /// Input that arrived before the shell could start, sent first.
    pub(crate) pending: Vec<ShellInput>,
}

impl Exec<'_> {
    /// Runs the command and pumps bytes until it exits or the terminal is
    /// gone. Dropping the process on the way out closes the websocket.
    pub(crate) async fn run(self, client: Client, mut io: ShellIo) {
        let pods: Api<Pod> = Api::namespaced(client, self.namespace);
        let mut params = AttachParams::interactive_tty().max_stdout_buf_size(READ_BUFFER);
        if let Some(container) = self.container {
            params = params.container(container);
        }
        let process = match pods.exec(self.pod, self.command, &params).await {
            Ok(process) => process,
            Err(err) => {
                tracing::debug!(pod = self.pod, "exec failed: {err}");
                io.fail(shell_error(&err)).await;
                return;
            }
        };
        match pump(process, self.pending, &mut io).await {
            Ok(Some(exit)) if is_missing_shell(&exit) => {
                io.fail(ShellError::NoShell(exit.message.unwrap_or_default()))
                    .await;
            }
            Ok(Some(exit)) => {
                let _ = io.send(ShellEvent::Exited(exit)).await;
            }
            // The terminal is gone; nobody is left to tell.
            Ok(None) => {}
            Err(error) => io.fail(error).await,
        }
    }
}

/// Moves bytes both ways until the process ends (`Some` with its exit) or
/// the terminal goes away (`None`).
async fn pump(
    mut process: AttachedProcess,
    pending: Vec<ShellInput>,
    io: &mut ShellIo,
) -> Result<Option<ShellExit>, ShellError> {
    let (Some(mut stdin), Some(mut stdout), Some(mut resize), Some(status)) = (
        process.stdin(),
        process.stdout(),
        process.terminal_size(),
        process.take_status(),
    ) else {
        return Err(ShellError::Other(
            "the exec stream is missing stdin, stdout or the terminal size".to_owned(),
        ));
    };
    for input in pending {
        send_input(input, &mut stdin, &mut resize).await;
    }
    let mut buf = vec![0u8; READ_BUFFER];
    loop {
        tokio::select! {
            // Both branches are cancel safe: `read` loses nothing when
            // another branch wins, and neither does `recv`.
            read = stdout.read(&mut buf) => match read {
                Ok(0) => break,
                Ok(n) => {
                    if !io.send(ShellEvent::Output(buf[..n].to_vec())).await {
                        return Ok(None);
                    }
                }
                Err(err) => return Err(ShellError::Other(format!("the exec stream broke: {err}"))),
            },
            input = io.input.recv() => match input {
                Some(input) => send_input(input, &mut stdin, &mut resize).await,
                None => return Ok(None),
            },
        }
    }
    // The output ended: the server sends the status just before closing.
    Ok(Some(exit_from_status(status.await.as_ref())))
}

/// Sends one input to the process. A failed send means the stream is
/// closing; the read side sees the end and reports the exit.
async fn send_input(
    input: ShellInput,
    stdin: &mut (impl tokio::io::AsyncWrite + Unpin),
    resize: &mut futures::channel::mpsc::Sender<TerminalSize>,
) {
    match input {
        ShellInput::Bytes(bytes) => {
            if let Err(err) = stdin.write_all(&bytes).await {
                tracing::debug!("exec stdin closed: {err}");
            }
        }
        ShellInput::Resize { columns, rows } => {
            let size = TerminalSize {
                width: columns,
                height: rows,
            };
            if let Err(err) = resize.send(size).await {
                tracing::debug!("exec resize not sent: {err}");
            }
        }
    }
}

/// The exit described by the status the server sends at the end of an
/// `exec`. `None` means the stream ended without one.
fn exit_from_status(status: Option<&Status>) -> ShellExit {
    let Some(status) = status else {
        return ShellExit {
            code: None,
            message: Some("the connection closed without an exit status".to_owned()),
        };
    };
    if status.status.as_deref() == Some("Success") {
        return ShellExit {
            code: Some(0),
            message: None,
        };
    }
    let code = (status.reason.as_deref() == Some("NonZeroExitCode"))
        .then(|| {
            status
                .details
                .as_ref()?
                .causes
                .as_ref()?
                .iter()
                .find(|cause| cause.reason.as_deref() == Some("ExitCode"))?
                .message
                .as_deref()?
                .parse()
                .ok()
        })
        .flatten();
    ShellExit {
        code,
        message: if code.is_some() {
            None
        } else {
            status.message.clone()
        },
    }
}

/// Whether an exit means the container has no `sh` to run.
pub(crate) fn is_missing_shell(exit: &ShellExit) -> bool {
    exit.code.is_none()
        && exit
            .message
            .as_deref()
            .is_some_and(|message| message.contains("executable file not found"))
}

#[cfg(test)]
mod tests {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{StatusCause, StatusDetails};

    use super::*;

    fn failure(reason: &str, message: &str, causes: Vec<StatusCause>) -> Status {
        Status {
            status: Some("Failure".to_owned()),
            reason: Some(reason.to_owned()),
            message: Some(message.to_owned()),
            details: Some(StatusDetails {
                causes: Some(causes),
                ..StatusDetails::default()
            }),
            ..Status::default()
        }
    }

    #[test]
    fn success_is_exit_code_zero() {
        let status = Status {
            status: Some("Success".to_owned()),
            ..Status::default()
        };
        assert_eq!(
            exit_from_status(Some(&status)),
            ShellExit {
                code: Some(0),
                message: None
            }
        );
    }

    #[test]
    fn a_non_zero_exit_carries_its_code() {
        let status = failure(
            "NonZeroExitCode",
            "command terminated with non-zero exit code: error executing command [sh], exit code 130",
            vec![StatusCause {
                reason: Some("ExitCode".to_owned()),
                message: Some("130".to_owned()),
                ..StatusCause::default()
            }],
        );
        assert_eq!(
            exit_from_status(Some(&status)),
            ShellExit {
                code: Some(130),
                message: None
            }
        );
    }

    #[test]
    fn a_missing_shell_is_recognised() {
        let status = failure(
            "InternalError",
            "OCI runtime exec failed: exec failed: unable to start container process: \
             exec: \"sh\": executable file not found in $PATH: unknown",
            Vec::new(),
        );
        let exit = exit_from_status(Some(&status));
        assert_eq!(exit.code, None);
        assert!(is_missing_shell(&exit));
    }

    #[test]
    fn no_status_is_an_unknown_exit() {
        let exit = exit_from_status(None);
        assert_eq!(exit.code, None);
        assert!(!is_missing_shell(&exit));
    }

    #[test]
    fn the_shell_script_prefers_bash_and_sets_a_known_term() {
        let command = shell_command();
        assert_eq!(command[..2], ["sh", "-c"]);
        let script = &command[2];
        assert!(script.contains("TERM=xterm-256color"));
        assert!(script.find("bash") < script.find("exec sh"));
    }
}
