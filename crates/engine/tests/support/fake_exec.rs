//! A fake API server that answers pod `exec` the way the kubelet does,
//! over a websocket with Kubernetes' channel protocol.
//!
//! Each binary message starts with a channel byte: 0 stdin, 1 stdout,
//! 3 status (a JSON `Status`, then the server closes), 4 resize (JSON
//! `{"Width":..,"Height":..}`), 255 close. The "shell" behind it:
//!
//! - prints `$ ` on connect;
//! - echoes stdin back on stdout, as a TTY would;
//! - prints `[size WxH]` for each resize;
//! - on a line `exit N`, sends a status with exit code N and closes.
//!
//! Pod `secret` answers 403 before the upgrade, pod `missing` 404, and pod
//! `distroless` a status saying `sh` is not found. Every exec URL is kept
//! so tests can check the command and the flags. There is no TLS and no
//! auth.

#![allow(dead_code, reason = "shared by tests; each uses a subset")]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode, header};

const STDIN: u8 = 0;
const STDOUT: u8 = 1;
const STATUS: u8 = 3;
const RESIZE: u8 = 4;

/// A running fake exec server.
#[derive(Clone)]
pub struct FakeExec {
    addr: SocketAddr,
    urls: Arc<Mutex<Vec<String>>>,
}

impl FakeExec {
    /// Listens on a free local port.
    pub async fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let server = Self {
            addr: listener.local_addr()?,
            urls: Arc::default(),
        };
        let urls = Arc::clone(&server.urls);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, Arc::clone(&urls)));
            }
        });
        Ok(server)
    }

    /// The base URL for a kubeconfig or `kube::Config`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The path and query of every exec request so far.
    pub fn urls(&self) -> Vec<String> {
        self.urls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

fn status_body(code: u16, reason: &str, message: &str) -> String {
    json!({
        "kind": "Status",
        "apiVersion": "v1",
        "status": "Failure",
        "message": message,
        "reason": reason,
        "code": code,
    })
    .to_string()
}

async fn serve(stream: TcpStream, urls: Arc<Mutex<Vec<String>>>) {
    let mut pod = String::new();
    #[expect(
        clippy::result_large_err,
        reason = "tungstenite's handshake callback returns its ErrorResponse"
    )]
    let accept = |request: &Request, mut response: Response| {
        let url = request.uri().to_string();
        urls.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(url.clone());
        // /api/v1/namespaces/{ns}/pods/{pod}/exec?...
        url.split('?')
            .next()
            .and_then(|path| path.split('/').nth(6))
            .unwrap_or_default()
            .clone_into(&mut pod);
        let refuse = |status: StatusCode, reason: &str, message: String| -> ErrorResponse {
            let mut refused =
                ErrorResponse::new(Some(status_body(status.as_u16(), reason, &message)));
            *refused.status_mut() = status;
            refused.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            refused
        };
        match pod.as_str() {
            "secret" => {
                return Err(refuse(
                    StatusCode::FORBIDDEN,
                    "Forbidden",
                    "pods \"secret\" is forbidden: cannot create resource \"pods/exec\"".to_owned(),
                ));
            }
            "missing" => {
                return Err(refuse(
                    StatusCode::NOT_FOUND,
                    "NotFound",
                    "pods \"missing\" not found".to_owned(),
                ));
            }
            _ => {}
        }
        response.headers_mut().insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("v5.channel.k8s.io"),
        );
        Ok(response)
    };
    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, accept).await else {
        return;
    };
    if pod == "distroless" {
        let status = json!({
            "status": "Failure",
            "reason": "InternalError",
            "message": "OCI runtime exec failed: exec failed: unable to start container process: \
                        exec: \"sh\": executable file not found in $PATH: unknown",
        });
        let _ = ws
            .send(channel(STATUS, status.to_string().as_bytes()))
            .await;
        let _ = ws.close(None).await;
        return;
    }
    if ws.send(channel(STDOUT, b"$ ")).await.is_err() {
        return;
    }
    let mut line = Vec::new();
    while let Some(Ok(message)) = ws.next().await {
        let Message::Binary(data) = message else {
            continue;
        };
        let Some((&kind, payload)) = data.split_first() else {
            continue;
        };
        match kind {
            STDIN => {
                if ws.send(channel(STDOUT, payload)).await.is_err() {
                    return;
                }
                line.extend_from_slice(payload);
                if let Some(code) = exit_code(&mut line) {
                    let _ = ws.send(channel(STATUS, exit_status(code).as_bytes())).await;
                    let _ = ws.close(None).await;
                    return;
                }
            }
            RESIZE => {
                let size: Value = serde_json::from_slice(payload).unwrap_or_default();
                let text = format!("[size {}x{}]\r\n", size["Width"], size["Height"]);
                if ws.send(channel(STDOUT, text.as_bytes())).await.is_err() {
                    return;
                }
            }
            _ => {}
        }
    }
}

/// The exit code of a finished `exit N` line, clearing finished lines.
fn exit_code(line: &mut Vec<u8>) -> Option<i32> {
    let end = line.iter().position(|b| *b == b'\r' || *b == b'\n')?;
    let text = String::from_utf8_lossy(&line[..end]).into_owned();
    line.drain(..=end);
    text.trim().strip_prefix("exit ")?.trim().parse().ok()
}

fn exit_status(code: i32) -> String {
    if code == 0 {
        return json!({ "status": "Success" }).to_string();
    }
    json!({
        "status": "Failure",
        "reason": "NonZeroExitCode",
        "message": format!("command terminated with non-zero exit code: exit code {code}"),
        "details": { "causes": [ { "reason": "ExitCode", "message": code.to_string() } ] },
    })
    .to_string()
}

fn channel(kind: u8, payload: &[u8]) -> Message {
    let mut data = Vec::with_capacity(payload.len() + 1);
    data.push(kind);
    data.extend_from_slice(payload);
    Message::binary(data)
}
