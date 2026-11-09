//! Lists and watches one kind as Tables, keeping a [`Store`] current.
//!
//! kube-rs's `watcher` only speaks typed objects, so this is a small
//! hand-written version of the same loop: list in pages, watch from the
//! list's resource version, resume after the server's timeout, re-list on
//! `410 Gone`, and back off on errors.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{AsyncBufReadExt as _, StreamExt as _};
use kube::Client;
use kube::api::{ListParams, WatchParams};
use model::ConnectError;
use tokio::sync::{mpsc, watch};

use super::wire::{self, Change, TABLE_ACCEPT};
use super::{Store, TableQuery, lock};
use crate::errors::{connect_error_from_request, connect_error_from_status};

/// Objects per list page, the same as client-go and kube-rs.
const PAGE_SIZE: u32 = 500;

/// How long the server keeps a watch open before we resume it. Under the
/// client's 295 s read timeout, so a quiet watch never looks like a failure.
const WATCH_TIMEOUT_SECS: u32 = 290;

/// Why a list or watch stopped.
#[derive(Debug, thiserror::Error)]
pub(super) enum StreamError {
    /// The request failed or the server refused it.
    #[error(transparent)]
    Request(#[from] kube::Error),
    /// The connection broke mid-stream.
    #[error("the watch stream broke: {0}")]
    Read(std::io::Error),
    /// The server sent something that is not a Table, a list or an event.
    #[error("unexpected response from the server: {0}")]
    Parse(#[from] serde_json::Error),
    /// The server ended a watch with an error status.
    #[error("{code}: {message}")]
    Status {
        /// The HTTP status code.
        code: u16,
        /// The server's message.
        message: String,
    },
}

impl StreamError {
    /// True when the resource version is too old and only a re-list helps.
    fn is_expired(&self) -> bool {
        match self {
            Self::Status { code, .. } => *code == 410,
            Self::Request(kube::Error::Api(status)) => status.code == 410,
            _ => false,
        }
    }

    /// The error as the UI explains it.
    pub(super) fn to_connect_error(&self) -> ConnectError {
        match self {
            Self::Request(err) => connect_error_from_request(err),
            Self::Status { code, message } => connect_error_from_status(*code, message),
            Self::Read(_) | Self::Parse(_) => ConnectError::Other(self.to_string()),
        }
    }
}

/// Retry delays after errors: 800 ms doubling up to 30 s, like kube-rs's
/// `default_backoff`. Reset after every successful list.
struct Backoff {
    next: Duration,
}

impl Backoff {
    const FIRST: Duration = Duration::from_millis(800);
    const MAX: Duration = Duration::from_secs(30);

    fn new() -> Self {
        Self { next: Self::FIRST }
    }

    fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(Self::MAX);
        delay
    }

    fn reset(&mut self) {
        self.next = Self::FIRST;
    }
}

/// Keeps `store` current until the task is dropped.
///
/// Every change bumps `changed`. Errors go to `errors`; the loop keeps
/// retrying until nobody listens for them.
pub(super) async fn run(
    client: Client,
    query: TableQuery,
    store: Arc<Mutex<Store>>,
    changed: watch::Sender<u64>,
    errors: mpsc::UnboundedSender<StreamError>,
) {
    let path = query.path();
    let mut backoff = Backoff::new();
    loop {
        let attempt = async {
            let version = list(&client, &path, &store, &changed).await?;
            backoff.reset();
            watch(&client, &path, version, &store, &changed).await
        };
        let Err(err) = attempt.await;
        if err.is_expired() {
            tracing::debug!(%path, "table watch expired, listing again");
            continue;
        }
        if errors.send(err).is_err() {
            return;
        }
        tokio::time::sleep(backoff.next_delay()).await;
    }
}

/// Lists every page and replaces the store. Returns the list's resource version.
async fn list(
    client: &Client,
    path: &str,
    store: &Mutex<Store>,
    changed: &watch::Sender<u64>,
) -> Result<String, StreamError> {
    let mut columns = Vec::new();
    let mut rows = Vec::new();
    let mut continue_token: Option<String> = None;
    let version = loop {
        let mut params = ListParams::default().limit(PAGE_SIZE);
        if let Some(token) = &continue_token {
            params = params.continue_token(token);
        }
        let request = kube::core::Request::new(path)
            .list(&params)
            .map_err(kube::Error::BuildRequest)?;
        let page = wire::parse_page(&client.request_text(as_table(request)?).await?)?;
        if columns.is_empty() {
            columns = page.columns;
        }
        rows.extend(page.rows);
        match page.continue_token.filter(|t| !t.is_empty()) {
            Some(token) => continue_token = Some(token),
            None => break page.resource_version.unwrap_or_default(),
        }
    };
    tracing::debug!(%path, rows = rows.len(), %version, "table listed");
    lock(store).replace(columns, rows);
    changed.send_modify(bump);
    Ok(version)
}

/// Watches from `version`, resuming whenever the server closes the stream.
/// Returns only on an error.
async fn watch(
    client: &Client,
    path: &str,
    mut version: String,
    store: &Mutex<Store>,
    changed: &watch::Sender<u64>,
) -> Result<Infallible, StreamError> {
    let params = WatchParams::default().timeout(WATCH_TIMEOUT_SECS);
    loop {
        let request = kube::core::Request::new(path)
            .watch(&params, &version)
            .map_err(kube::Error::BuildRequest)?;
        let body = client.request_stream(as_table(request)?).await?;
        let mut lines = std::pin::pin!(body.lines());
        while let Some(line) = lines.next().await {
            let line = line.map_err(StreamError::Read)?;
            if line.trim().is_empty() {
                continue;
            }
            match wire::parse_change(&line)? {
                Change::Applied(page) => {
                    version = page.resource_version.clone().unwrap_or(version);
                    lock(store).apply(page.columns, page.rows);
                }
                Change::Deleted(page) => {
                    version = page.resource_version.clone().unwrap_or(version);
                    lock(store).delete(&page.rows);
                }
                Change::Bookmark(rv) => {
                    version = rv.unwrap_or(version);
                    continue;
                }
                Change::Error { code, message } => {
                    return Err(StreamError::Status { code, message });
                }
            }
            changed.send_modify(bump);
        }
        tracing::trace!(%path, %version, "table watch closed by the server, resuming");
    }
}

/// Turns a list or watch request into a Table request.
fn as_table(mut request: http::Request<Vec<u8>>) -> Result<http::Request<Vec<u8>>, StreamError> {
    // kube's builders always end the path with `?` and their parameters.
    let uri = request.uri().to_string();
    let separator = if uri.ends_with('?') { "" } else { "&" };
    *request.uri_mut() = format!("{uri}{separator}includeObject=Metadata")
        .parse()
        .map_err(|e| kube::Error::HttpError(http::Error::from(e)))?;
    request.headers_mut().insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static(TABLE_ACCEPT),
    );
    Ok(request)
}

fn bump(n: &mut u64) {
    *n = n.wrapping_add(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_thirty_seconds() {
        let mut backoff = Backoff::new();
        let delays: Vec<u64> = (0..8)
            .map(|_| backoff.next_delay().as_millis().try_into().unwrap())
            .collect();
        assert_eq!(
            delays,
            [800, 1600, 3200, 6400, 12_800, 25_600, 30_000, 30_000]
        );
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(800));
    }

    #[test]
    fn table_requests_ask_for_metadata_and_the_table_type() {
        let request = kube::core::Request::new("/api/v1/pods")
            .list(&ListParams::default().limit(PAGE_SIZE))
            .unwrap();
        let request = as_table(request).unwrap();
        let uri = request.uri().to_string();
        assert!(uri.starts_with("/api/v1/pods?"), "{uri}");
        assert!(uri.contains("limit=500"), "{uri}");
        assert!(uri.ends_with("&includeObject=Metadata"), "{uri}");
        assert_eq!(
            request.headers()[http::header::ACCEPT],
            "application/json;as=Table;v=v1;g=meta.k8s.io,application/json"
        );

        let watch = kube::core::Request::new("/apis/stable.example.com/v1/crontabs")
            .watch(&WatchParams::default().timeout(WATCH_TIMEOUT_SECS), "42")
            .unwrap();
        let uri = as_table(watch).unwrap().uri().to_string();
        assert!(uri.contains("watch=true"), "{uri}");
        assert!(uri.contains("resourceVersion=42"), "{uri}");
        assert!(uri.contains("allowWatchBookmarks=true"), "{uri}");
        assert!(uri.ends_with("&includeObject=Metadata"), "{uri}");
    }

    #[test]
    fn only_gone_means_list_again() {
        let gone = StreamError::Status {
            code: 410,
            message: "too old resource version".to_owned(),
        };
        assert!(gone.is_expired());
        let forbidden = StreamError::Status {
            code: 403,
            message: "no".to_owned(),
        };
        assert!(!forbidden.is_expired());
        assert!(matches!(
            forbidden.to_connect_error(),
            ConnectError::Forbidden(_)
        ));
    }
}
