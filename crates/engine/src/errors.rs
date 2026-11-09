//! Turning kube-rs errors into [`ConnectError`]s the UI can explain.

use std::error::Error as _;
use std::io;

use kube::client::AuthError;
use kube::runtime::watcher;
use model::ConnectError;

/// Maps an error from building a client.
///
/// `exec_command` is the auth plugin the context's user runs, if any. It is
/// used to name the missing binary.
pub fn connect_error_from_client(err: &kube::Error, exec_command: Option<&str>) -> ConnectError {
    if let kube::Error::Auth(AuthError::AuthExecStart(io)) = err
        && io.kind() == io::ErrorKind::NotFound
    {
        return ConnectError::AuthPluginMissing {
            command: exec_command.unwrap_or("(unknown)").to_owned(),
            path: std::env::var("PATH").unwrap_or_default(),
        };
    }
    match err {
        kube::Error::Auth(auth) => ConnectError::Auth(auth_message(auth)),
        kube::Error::InferKubeconfig(e) => ConnectError::Config(e.to_string()),
        kube::Error::RustlsTls(e) => ConnectError::Config(format!("TLS setup failed: {e}")),
        other => from_request(other),
    }
}

/// Maps an error from the pod watcher, before the first list succeeded.
pub fn connect_error_from_watch(err: &watcher::Error) -> ConnectError {
    match err {
        watcher::Error::InitialListFailed(e)
        | watcher::Error::WatchStartFailed(e)
        | watcher::Error::WatchFailed(e) => from_request(e),
        watcher::Error::WatchError(status) => from_status(status.code, &status.message),
        watcher::Error::NoResourceVersion => ConnectError::Other(err.to_string()),
    }
}

fn from_request(err: &kube::Error) -> ConnectError {
    match err {
        kube::Error::Api(status) => from_status(status.code, &status.message),
        kube::Error::HyperError(_) | kube::Error::Service(_) => {
            // Auth refresh failures also surface as service errors; look for
            // one in the source chain before calling it a network problem.
            if let Some(auth) = find_source::<AuthError>(err) {
                return ConnectError::Auth(auth_message(auth));
            }
            ConnectError::Unreachable(root_cause(err))
        }
        kube::Error::Auth(auth) => ConnectError::Auth(auth_message(auth)),
        other => ConnectError::Other(other.to_string()),
    }
}

fn from_status(code: u16, message: &str) -> ConnectError {
    match code {
        401 => ConnectError::Auth(message.to_owned()),
        403 => ConnectError::Forbidden(message.to_owned()),
        _ => ConnectError::Other(format!("{code}: {message}")),
    }
}

fn auth_message(err: &AuthError) -> String {
    match err {
        AuthError::AuthExecRun { cmd, out } => {
            let status = out.status;
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stderr = stderr.trim();
            if stderr.is_empty() {
                format!("`{cmd}` exited with {status}")
            } else {
                format!("`{cmd}` exited with {status}: {stderr}")
            }
        }
        other => other.to_string(),
    }
}

fn find_source<'a, T: std::error::Error + 'static>(
    err: &'a (dyn std::error::Error + 'static),
) -> Option<&'a T> {
    let mut current = Some(err);
    while let Some(e) = current {
        if let Some(found) = e.downcast_ref::<T>() {
            return Some(found);
        }
        current = e.source();
    }
    None
}

/// The innermost error message, which is usually the one that says what
/// happened ("connection refused", "deadline has elapsed").
fn root_cause(err: &kube::Error) -> String {
    let mut message = err.to_string();
    let mut current = err.source();
    while let Some(e) = current {
        message = e.to_string();
        current = e.source();
    }
    message
}
