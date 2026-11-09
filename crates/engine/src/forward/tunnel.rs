//! A forward's local listener, and one kube-rs port-forward per connection.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use futures::FutureExt;
use k8s_openapi::api::core::v1::Pod;
use kube::{Api, Client};
use model::ForwardSpec;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

use super::Reporter;
use super::resolve::{ResolveError, resolve};

/// How long to wait before accepting again after `accept` fails, for
/// example when the process is out of file descriptors.
const ACCEPT_RETRY: Duration = Duration::from_millis(250);

/// Why one connection failed. The forward keeps listening.
#[derive(Debug, thiserror::Error)]
pub(super) enum ConnectionError {
    /// There was no pod or port to send it to.
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    /// The API server refused the port-forward.
    #[error("could not open a port-forward to pod {pod}: {cause}")]
    Open {
        /// The pod.
        pod: String,
        /// What the server said.
        cause: kube::Error,
    },
    /// kube-rs gave no stream for the port. Should not happen.
    #[error("the port-forward to pod {0} gave no stream")]
    NoStream(String),
    /// The pod side reported an error, such as nothing listening on the port.
    #[error("{0}")]
    Pod(String),
    /// The connection broke while bytes were flowing.
    #[error("the connection to pod {pod} broke: {cause}")]
    Io {
        /// The pod.
        pod: String,
        /// What broke.
        cause: io::Error,
    },
}

/// Binds the forward's local port and serves connections until the task is
/// dropped. Dropping it closes the port and every open connection.
pub(super) async fn run(reporter: Reporter, spec: ForwardSpec, client: Client) {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, spec.local_port));
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(err) => {
            reporter.failed(bind_message(addr, &err));
            return;
        }
    };
    let local = match listener.local_addr() {
        Ok(local) => local,
        Err(err) => {
            reporter.failed(format!("could not read the local address: {err}"));
            return;
        }
    };
    tracing::info!(%local, target = %spec.target, port = spec.port, "port-forward listening");
    reporter.listening(local);

    // Owning the connections here means they all close with the forward.
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _peer)) => {
                    // Interactive protocols (shells, databases) want small
                    // writes sent at once. Failing to set it only costs latency.
                    let _ = stream.set_nodelay(true);
                    reporter.opened();
                    let (reporter, spec, client) = (reporter.clone(), spec.clone(), client.clone());
                    connections.spawn(async move {
                        let outcome = connect(&client, &spec, stream).await;
                        if let Err(err) = &outcome {
                            tracing::warn!(target = %spec.target, port = spec.port, "port-forward connection failed: {err}");
                        }
                        reporter.closed(outcome.map_err(|e| e.to_string()));
                    });
                }
                Err(err) => {
                    tracing::warn!(%local, "port-forward accept failed: {err}");
                    tokio::time::sleep(ACCEPT_RETRY).await;
                }
            },
            // Reap finished connections so their results do not pile up.
            Some(_) = connections.join_next() => {}
        }
    }
}

/// Carries one local connection to the pod. Returns the pod it went to.
async fn connect(
    client: &Client,
    spec: &ForwardSpec,
    mut stream: TcpStream,
) -> Result<String, ConnectionError> {
    let target = resolve(client, &spec.namespace, &spec.target, spec.port).await?;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &spec.namespace);
    let mut forwarder = pods
        .portforward(&target.pod, &[target.port])
        .await
        .map_err(|cause| ConnectionError::Open {
            pod: target.pod.clone(),
            cause,
        })?;
    let mut upstream = forwarder
        .take_stream(target.port)
        .ok_or_else(|| ConnectionError::NoStream(target.pod.clone()))?;
    let pod_error = forwarder.take_error(target.port);

    let copied = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
    // kube-rs delivers the pod's error message before it closes the stream,
    // so once copying is done any message is already waiting.
    let pod_error = pod_error.and_then(FutureExt::now_or_never).flatten();
    // Both sides have closed or failed; nothing useful is left in flight.
    forwarder.abort();

    if let Some(message) = pod_error {
        return Err(ConnectionError::Pod(message));
    }
    copied.map_err(|cause| ConnectionError::Io {
        pod: target.pod.clone(),
        cause,
    })?;
    Ok(target.pod)
}

fn bind_message(addr: SocketAddr, err: &io::Error) -> String {
    match err.kind() {
        io::ErrorKind::AddrInUse => {
            format!("port {} is already in use on {}", addr.port(), addr.ip())
        }
        io::ErrorKind::PermissionDenied => {
            format!(
                "not allowed to listen on port {}; pick one above 1023",
                addr.port()
            )
        }
        _ => format!("could not listen on {addr}: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_errors_say_what_to_do() {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 8080));
        assert_eq!(
            bind_message(addr, &io::Error::from(io::ErrorKind::AddrInUse)),
            "port 8080 is already in use on 127.0.0.1"
        );
        let low = SocketAddr::from((Ipv4Addr::LOCALHOST, 80));
        assert_eq!(
            bind_message(low, &io::Error::from(io::ErrorKind::PermissionDenied)),
            "not allowed to listen on port 80; pick one above 1023"
        );
    }
}
