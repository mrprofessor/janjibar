//! Finding the Prometheus service in a cluster, the way each provider does.
//!
//! Ported from Freelens's providers (MIT). Stacklight follows Lens and looks
//! for service `prometheus-server` in namespace `stacklight`; Freelens
//! passes those two names as label selectors, which never match.

use std::fmt;

use futures::future::join_all;
use k8s_openapi::api::core::v1::Service;
use kube::Api;
use kube::api::ListParams;
use model::{PrometheusProvider, ServiceRef};

/// How a provider finds its service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lookup {
    /// This service in this namespace.
    Named {
        namespace: &'static str,
        name: &'static str,
    },
    /// The first service in any namespace with these labels.
    Labels(&'static str),
}

fn lookup(provider: PrometheusProvider) -> Lookup {
    match provider {
        PrometheusProvider::Lens => Lookup::Named {
            namespace: "lens-metrics",
            name: "prometheus",
        },
        PrometheusProvider::Helm => Lookup::Labels(
            "app.kubernetes.io/name=prometheus,app.kubernetes.io/component=server,app.kubernetes.io/managed-by=Helm",
        ),
        PrometheusProvider::Helm14 => {
            Lookup::Labels("app=prometheus,component=server,heritage=Helm")
        }
        PrometheusProvider::Operator => Lookup::Labels("operated-prometheus=true"),
        PrometheusProvider::Stacklight => Lookup::Named {
            namespace: "stacklight",
            name: "prometheus-server",
        },
        PrometheusProvider::OpenShift => Lookup::Named {
            namespace: "openshift-monitoring",
            name: "prometheus-k8s",
        },
    }
}

/// Why one provider found no service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Miss {
    /// The provider that looked.
    pub provider: PrometheusProvider,
    /// What went wrong.
    pub reason: MissReason,
}

/// What went wrong when a provider looked for its service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MissReason {
    /// Nothing matched. Holds what was looked for.
    NotFound(String),
    /// The service exists but declares no ports.
    NoPorts(String),
    /// The user may not read services there. Holds the server's message.
    Forbidden(String),
    /// The API server could not answer. Holds the error.
    Failed(String),
}

impl fmt::Display for Miss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let provider = self.provider.label();
        match &self.reason {
            MissReason::NotFound(what) => write!(f, "{provider}: no {what}"),
            MissReason::NoPorts(what) => write!(f, "{provider}: {what} has no ports"),
            MissReason::Forbidden(message) => write!(f, "{provider}: forbidden: {message}"),
            MissReason::Failed(message) => write!(f, "{provider}: {message}"),
        }
    }
}

/// No provider found a Prometheus service.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no Prometheus service found: {}", join(.misses))]
pub struct NotFound {
    /// Why each provider that looked found nothing, in the order tried.
    pub misses: Vec<Miss>,
}

fn join(misses: &[Miss]) -> String {
    let parts: Vec<String> = misses.iter().map(Miss::to_string).collect();
    parts.join("; ")
}

/// Looks for each provider's service at once and returns the first found,
/// in the order of `providers`.
pub async fn detect(
    client: &kube::Client,
    providers: &[PrometheusProvider],
) -> Result<(PrometheusProvider, ServiceRef), NotFound> {
    let found = join_all(providers.iter().map(|&p| find_service(client, p))).await;
    let mut misses = Vec::new();
    for (provider, result) in providers.iter().zip(found) {
        match result {
            Ok(service) => {
                tracing::debug!(%service, provider = provider.id(), "found Prometheus");
                return Ok((*provider, service));
            }
            Err(miss) => misses.push(miss),
        }
    }
    Err(NotFound { misses })
}

/// Looks for `provider`'s service.
pub async fn find_service(
    client: &kube::Client,
    provider: PrometheusProvider,
) -> Result<ServiceRef, Miss> {
    let miss = |reason| Miss { provider, reason };
    match lookup(provider) {
        Lookup::Named { namespace, name } => {
            let api: Api<Service> = Api::namespaced(client.clone(), namespace);
            let what = format!("service {namespace}/{name}");
            match api.get_opt(name).await {
                Ok(Some(service)) => {
                    service_ref(&service).ok_or_else(|| miss(MissReason::NoPorts(what)))
                }
                Ok(None) => Err(miss(MissReason::NotFound(what))),
                Err(err) => Err(miss(reason_from(&err))),
            }
        }
        Lookup::Labels(selector) => {
            let api: Api<Service> = Api::all(client.clone());
            // Freelens takes the first match; one item is all we need.
            let params = ListParams::default().labels(selector).limit(1);
            let what = format!("service labelled {selector}");
            match api.list(&params).await {
                Ok(list) => match list.items.first() {
                    Some(service) => {
                        service_ref(service).ok_or_else(|| miss(MissReason::NoPorts(what)))
                    }
                    None => Err(miss(MissReason::NotFound(what))),
                },
                Err(err) => Err(miss(reason_from(&err))),
            }
        }
    }
}

/// The service's namespace, name and first port, as Freelens picks it.
fn service_ref(service: &Service) -> Option<ServiceRef> {
    let port = service.spec.as_ref()?.ports.as_ref()?.first()?;
    // A port named or tagged `https` speaks TLS; the proxy must use it too.
    let https =
        port.name.as_deref() == Some("https") || port.app_protocol.as_deref() == Some("https");
    Some(ServiceRef {
        namespace: service.metadata.namespace.clone()?,
        name: service.metadata.name.clone()?,
        port: u16::try_from(port.port).ok()?,
        https,
    })
}

fn reason_from(err: &kube::Error) -> MissReason {
    match err {
        kube::Error::Api(status) if status.code == 403 => {
            MissReason::Forbidden(status.message.clone())
        }
        kube::Error::Api(status) => {
            MissReason::Failed(format!("{}: {}", status.code, status.message))
        }
        other => MissReason::Failed(crate::query::root_cause(other)),
    }
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::{ServicePort, ServiceSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;

    fn service(ports: Vec<ServicePort>) -> Service {
        Service {
            metadata: ObjectMeta {
                name: Some("prometheus".to_owned()),
                namespace: Some("monitoring".to_owned()),
                ..ObjectMeta::default()
            },
            spec: Some(ServiceSpec {
                ports: Some(ports),
                ..ServiceSpec::default()
            }),
            ..Service::default()
        }
    }

    fn port(name: &str, number: i32) -> ServicePort {
        ServicePort {
            name: Some(name.to_owned()),
            port: number,
            ..ServicePort::default()
        }
    }

    #[test]
    fn the_first_port_wins() {
        let found = service_ref(&service(vec![port("web", 9090), port("grpc", 10901)]));
        assert_eq!(
            found,
            Some(ServiceRef {
                namespace: "monitoring".to_owned(),
                name: "prometheus".to_owned(),
                port: 9090,
                https: false,
            })
        );
    }

    #[test]
    fn an_https_port_is_proxied_over_https() {
        let found = service_ref(&service(vec![port("https", 9091)]));
        assert!(found.is_some_and(|s| s.https));
    }

    #[test]
    fn a_service_without_ports_is_not_used() {
        assert_eq!(service_ref(&service(Vec::new())), None);
    }

    #[test]
    fn misses_read_well() {
        let error = NotFound {
            misses: vec![
                Miss {
                    provider: PrometheusProvider::Lens,
                    reason: MissReason::NotFound("service lens-metrics/prometheus".to_owned()),
                },
                Miss {
                    provider: PrometheusProvider::Operator,
                    reason: MissReason::Forbidden("cannot list services".to_owned()),
                },
            ],
        };
        assert_eq!(
            error.to_string(),
            "no Prometheus service found: Lens: no service lens-metrics/prometheus; \
             Prometheus Operator: forbidden: cannot list services"
        );
    }
}
