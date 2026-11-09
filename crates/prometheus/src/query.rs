//! Running range queries against a cluster's Prometheus.
//!
//! In-cluster Prometheus is reached through the API server's service proxy
//! with the cluster's own [`kube::Client`], so exec plugins, OIDC and client
//! certificates keep working and no port-forward or local proxy is needed.
//! A URL outside the cluster gets its own `kube::Client`, which brings the
//! same TLS stack (the system trust store) and connection pooling.

use std::error::Error as _;
use std::fmt;
use std::time::{Duration, SystemTime};

use futures::future::join_all;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use http::uri::PathAndQuery;
use http::{HeaderName, HeaderValue, Request};
use kube::client::Body;
use model::{
    MetricQuery, MetricSeries, PrometheusProvider, PrometheusSettings, PrometheusSource,
    PrometheusUrl, QueryMethod, ServiceRef, TimeRange,
};

use crate::detect::{self, NotFound};
use crate::response;

/// How long to wait for a TCP and TLS connection to a Prometheus URL.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Where queries go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// A service, through the API server's service proxy.
    Service(ServiceRef),
    /// A URL outside the cluster.
    Url(String),
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Service(service) => write!(f, "service {service}"),
            Self::Url(url) => f.write_str(url),
        }
    }
}

/// How failed queries are retried.
///
/// Only failures the client does not retry itself count: no response at all,
/// `500` and `502`. kube's client already retries `429`, `503` and `504`
/// with backoff. Errors in the query itself (`400`, `422`) never retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    /// Tries in total, the first included. `1` means no retry.
    pub attempts: u32,
    /// The wait after the first failure. The wait after try `n` is `n`
    /// times this.
    pub delay: Duration,
}

impl Default for Retry {
    /// Freelens's policy: five tries, waiting 1, 2, 3 and 4 seconds.
    fn default() -> Self {
        Self {
            attempts: 5,
            delay: Duration::from_secs(1),
        }
    }
}

/// Why Prometheus could not be set up.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    /// No provider found a service.
    #[error(transparent)]
    NotFound(#[from] NotFound),
    /// The configured URL cannot be used.
    #[error("invalid Prometheus URL `{url}`: {reason}")]
    InvalidUrl {
        /// The URL as configured.
        url: String,
        /// What is wrong with it.
        reason: String,
    },
    /// The path prefix is not a valid URL path.
    #[error("invalid path prefix `{0}`")]
    InvalidPrefix(String),
    /// A configured header name or value is not valid HTTP.
    #[error("invalid header `{0}`")]
    InvalidHeader(String),
    /// Building the client for a URL failed.
    #[error("cannot set up a client for {url}: {reason}")]
    Client {
        /// The URL as configured.
        url: String,
        /// The error.
        reason: String,
    },
}

/// Why a query failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    /// Prometheus answered with an error: a bad query, a timeout, overload.
    #[error("Prometheus {kind} error ({status}): {message}")]
    Prometheus {
        /// The HTTP status.
        status: u16,
        /// Prometheus's `errorType`, such as `bad_data` or `timeout`.
        kind: String,
        /// Prometheus's message.
        message: String,
    },
    /// The API server or a proxy answered with an HTTP error, such as `403`
    /// for a user who may not proxy to services, or `503` when the service
    /// has no ready pods.
    #[error("HTTP {status}: {message}")]
    Http {
        /// The HTTP status.
        status: u16,
        /// The server's message.
        message: String,
    },
    /// No response arrived: network, TLS or authentication failed.
    #[error("request failed: {0}")]
    Request(String),
    /// The response was not what the Prometheus HTTP API sends.
    #[error("unexpected response: {0}")]
    Decode(String),
}

impl QueryError {
    /// True for failures that a later try may not hit. See [`Retry`].
    fn is_retryable(&self) -> bool {
        match self {
            Self::Request(_) => true,
            Self::Prometheus { status, .. } | Self::Http { status, .. } => {
                matches!(status, 500 | 502)
            }
            Self::Decode(_) => false,
        }
    }
}

/// A cluster's Prometheus, found and ready to query. Cheap to clone.
#[derive(Clone)]
pub struct Prometheus {
    client: kube::Client,
    /// Everything before `/api/v1/query_range`: the proxy path and prefix.
    base_path: String,
    endpoint: Endpoint,
    provider: PrometheusProvider,
    auto_detected: bool,
    method: QueryMethod,
    mountpoints: String,
    retry: Retry,
}

impl fmt::Debug for Prometheus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Prometheus")
            .field("endpoint", &self.endpoint)
            .field("base_path", &self.base_path)
            .field("provider", &self.provider)
            .field("auto_detected", &self.auto_detected)
            .field("method", &self.method)
            .finish_non_exhaustive()
    }
}

impl Prometheus {
    /// Finds Prometheus for a cluster as `settings` say.
    ///
    /// `cluster` is the context's client. Detection lists or reads services
    /// with it; in-cluster queries go through its service proxy.
    pub async fn connect(
        cluster: &kube::Client,
        settings: &PrometheusSettings,
    ) -> Result<Self, SetupError> {
        let prefix = normalize_prefix(&settings.path_prefix);
        let (client, endpoint, provider, auto_detected) = match &settings.source {
            PrometheusSource::Auto => {
                let (provider, service) = detect::detect(cluster, &PrometheusProvider::ALL).await?;
                (cluster.clone(), Endpoint::Service(service), provider, true)
            }
            PrometheusSource::Provider(provider) => {
                let service = detect::find_service(cluster, *provider)
                    .await
                    .map_err(|miss| NotFound { misses: vec![miss] })?;
                (
                    cluster.clone(),
                    Endpoint::Service(service),
                    *provider,
                    false,
                )
            }
            PrometheusSource::Service { service, queries } => (
                cluster.clone(),
                Endpoint::Service(service.clone()),
                *queries,
                false,
            ),
            PrometheusSource::Url { url, queries } => (
                url_client(url).await?,
                Endpoint::Url(url.url.clone()),
                *queries,
                false,
            ),
        };
        let base_path = match &endpoint {
            Endpoint::Service(service) => format!("{}{prefix}", proxy_path(service)),
            Endpoint::Url(_) => prefix.clone(),
        };
        if PathAndQuery::try_from(format!("{base_path}/api/v1/query_range")).is_err() {
            return Err(SetupError::InvalidPrefix(settings.path_prefix.clone()));
        }
        tracing::info!(%endpoint, provider = provider.id(), auto_detected, "using Prometheus");
        Ok(Self {
            client,
            base_path,
            endpoint,
            provider,
            auto_detected,
            method: settings.method,
            mountpoints: settings.mountpoints.clone(),
            retry: Retry::default(),
        })
    }

    /// Replaces the retry policy.
    #[must_use]
    pub fn with_retry(mut self, retry: Retry) -> Self {
        self.retry = retry;
        self
    }

    /// Whose queries this Prometheus gets.
    pub fn provider(&self) -> PrometheusProvider {
        self.provider
    }

    /// Where queries go.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// True when the provider was found by trying them all.
    pub fn auto_detected(&self) -> bool {
        self.auto_detected
    }

    /// The PromQL this Prometheus runs for `query`.
    pub fn promql(&self, query: &MetricQuery) -> String {
        crate::promql(self.provider, query, &self.mountpoints)
    }

    /// Runs `query` over `range`.
    pub async fn query(
        &self,
        query: &MetricQuery,
        range: &TimeRange,
    ) -> Result<Vec<MetricSeries>, QueryError> {
        self.query_range(&self.promql(query), range).await
    }

    /// Runs several queries at once. Results come back in the same order.
    /// Queries that turn into the same PromQL are sent once.
    pub async fn query_many(
        &self,
        queries: &[MetricQuery],
        range: &TimeRange,
    ) -> Vec<Result<Vec<MetricSeries>, QueryError>> {
        let mut unique: Vec<String> = Vec::new();
        let mut slots = Vec::with_capacity(queries.len());
        for query in queries {
            let text = self.promql(query);
            let slot = if let Some(i) = unique.iter().position(|u| *u == text) {
                i
            } else {
                unique.push(text);
                unique.len() - 1
            };
            slots.push(slot);
        }
        let results = join_all(unique.iter().map(|text| self.query_range(text, range))).await;
        // Every slot indexes `unique`, and `results` has one entry per item.
        slots.into_iter().map(|i| results[i].clone()).collect()
    }

    /// Runs raw PromQL over `range`, retrying as the [`Retry`] policy says.
    pub async fn query_range(
        &self,
        promql: &str,
        range: &TimeRange,
    ) -> Result<Vec<MetricSeries>, QueryError> {
        let mut attempt = 1;
        loop {
            match self.query_once(promql, range).await {
                Err(err) if err.is_retryable() && attempt < self.retry.attempts => {
                    tracing::debug!(attempt, "Prometheus query failed, retrying: {err}");
                    tokio::time::sleep(self.retry.delay * attempt).await;
                    attempt += 1;
                }
                result => return result,
            }
        }
    }

    async fn query_once(
        &self,
        promql: &str,
        range: &TimeRange,
    ) -> Result<Vec<MetricSeries>, QueryError> {
        let request = self.request(promql, range)?;
        let response = self
            .client
            .send(request.map(Body::from))
            .await
            .map_err(|e| QueryError::Request(root_cause(&e)))?;
        let status = response.status();
        let body = response
            .into_body()
            .collect_bytes()
            .await
            .map_err(|e| QueryError::Request(root_cause(&e)))?;
        if status.is_success() {
            response::parse_matrix(&body)
        } else {
            Err(response::parse_error(status.as_u16(), &body))
        }
    }

    fn request(&self, promql: &str, range: &TimeRange) -> Result<Request<Vec<u8>>, QueryError> {
        let params = form_urlencoded::Serializer::new(String::new())
            .append_pair("query", promql)
            .append_pair("start", &unix_seconds(range.start).to_string())
            .append_pair("end", &unix_seconds(range.end).to_string())
            .append_pair("step", &range.step.as_secs_f64().to_string())
            .finish();
        let path = format!("{}/api/v1/query_range", self.base_path);
        let request = match self.method {
            QueryMethod::Post => Request::post(path)
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(ACCEPT, "application/json")
                .body(params.into_bytes()),
            QueryMethod::Get => Request::get(format!("{path}?{params}"))
                .header(ACCEPT, "application/json")
                .body(Vec::new()),
        };
        request.map_err(|e| QueryError::Request(format!("cannot build the request: {e}")))
    }
}

/// The service proxy path: `/api/v1/namespaces/{ns}/services/{name}:{port}/proxy`,
/// with `https:` before the name for a TLS port.
fn proxy_path(service: &ServiceRef) -> String {
    let scheme = if service.https { "https:" } else { "" };
    format!(
        "/api/v1/namespaces/{}/services/{scheme}{}:{}/proxy",
        service.namespace, service.name, service.port
    )
}

/// `prometheus/` and `/prometheus` both become `/prometheus`; blank stays blank.
fn normalize_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim().trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("/{trimmed}")
    }
}

/// Whole seconds since the epoch, as Freelens sends them.
fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// A client for a URL outside the cluster.
async fn url_client(url: &PrometheusUrl) -> Result<kube::Client, SetupError> {
    let invalid = |reason: &str| SetupError::InvalidUrl {
        url: url.url.clone(),
        reason: reason.to_owned(),
    };
    let uri: http::Uri = url.url.trim().parse().map_err(|_| invalid("not a URL"))?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return Err(invalid("must start with http:// or https://"));
    }
    if uri.query().is_some() {
        return Err(invalid("must not have a query string"));
    }
    let mut config = kube::Config::new(uri);
    config.connect_timeout = Some(CONNECT_TIMEOUT);
    if let Some(token) = &url.bearer_token {
        let mut value = HeaderValue::from_str(&format!("Bearer {}", token.trim()))
            .map_err(|_| SetupError::InvalidHeader(AUTHORIZATION.to_string()))?;
        value.set_sensitive(true);
        config.headers.push((AUTHORIZATION, value));
    }
    for (name, value) in &url.headers {
        let header = HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|_| SetupError::InvalidHeader(name.clone()))?;
        let mut value = HeaderValue::from_str(value.trim())
            .map_err(|_| SetupError::InvalidHeader(name.clone()))?;
        // Header values may be credentials; keep them out of debug output.
        value.set_sensitive(true);
        config.headers.push((header, value));
    }
    let base = url.url.clone();
    // Building a client sets up TLS, which reads the system trust store.
    tokio::task::spawn_blocking(move || kube::Client::try_from(config))
        .await
        .map_err(|e| SetupError::Client {
            url: base.clone(),
            reason: format!("setup was interrupted: {e}"),
        })?
        .map_err(|e| SetupError::Client {
            url: base,
            reason: root_cause(&e),
        })
}

/// The innermost error message, which usually says what happened
/// ("connection refused", "invalid peer certificate").
pub(crate) fn root_cause(err: &kube::Error) -> String {
    let mut message = err.to_string();
    let mut current = err.source();
    while let Some(e) = current {
        message = e.to_string();
        current = e.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_are_normalized() {
        assert_eq!(normalize_prefix(""), "");
        assert_eq!(normalize_prefix(" / "), "");
        assert_eq!(normalize_prefix("prometheus"), "/prometheus");
        assert_eq!(normalize_prefix("/prometheus/"), "/prometheus");
        assert_eq!(normalize_prefix("/a/b"), "/a/b");
    }

    #[test]
    fn proxy_paths_name_the_port_and_scheme() {
        let mut service = ServiceRef {
            namespace: "monitoring".to_owned(),
            name: "prometheus-k8s".to_owned(),
            port: 9091,
            https: false,
        };
        assert_eq!(
            proxy_path(&service),
            "/api/v1/namespaces/monitoring/services/prometheus-k8s:9091/proxy"
        );
        service.https = true;
        assert_eq!(
            proxy_path(&service),
            "/api/v1/namespaces/monitoring/services/https:prometheus-k8s:9091/proxy"
        );
    }

    #[test]
    fn only_transient_failures_retry() {
        let http = |status| QueryError::Http {
            status,
            message: String::new(),
        };
        assert!(QueryError::Request("connection reset".to_owned()).is_retryable());
        assert!(http(500).is_retryable());
        assert!(http(502).is_retryable());
        // kube's client already retried these.
        assert!(!http(503).is_retryable());
        assert!(!http(403).is_retryable());
        assert!(
            !QueryError::Prometheus {
                status: 422,
                kind: "execution".to_owned(),
                message: String::new(),
            }
            .is_retryable()
        );
    }
}
