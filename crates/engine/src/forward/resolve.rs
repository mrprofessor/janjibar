//! Finds the pod and container port a connection should go to.
//!
//! Pods forward as asked. Services resolve on every connection, the way
//! `kubectl port-forward svc/...` does once: take a ready pod matching the
//! service selector, and turn the service port into that pod's container
//! port, looking up named target ports in its container specs.

use k8s_openapi::api::core::v1::{Pod, Service};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::ListParams;
use kube::{Api, Client};
use model::ForwardTarget;

/// Where one connection goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Resolved {
    /// The pod's name.
    pub pod: String,
    /// The container port inside the pod.
    pub port: u16,
}

/// Why a connection has nowhere to go.
#[derive(Debug, thiserror::Error)]
pub(super) enum ResolveError {
    /// The pod exists but is not running yet, or any more.
    #[error("pod {pod} is {phase}, not running")]
    PodNotRunning {
        /// The pod.
        pod: String,
        /// Its phase.
        phase: String,
    },
    /// The service selects no pods by label.
    #[error("service {0} has no selector, so it has no pods to forward to")]
    NoSelector(String),
    /// The service has no such port.
    #[error("service {service} has no port {port}")]
    NoServicePort {
        /// The service.
        service: String,
        /// The port asked for.
        port: u16,
    },
    /// No pod behind the service is ready.
    #[error("no ready pod behind service {0}")]
    NoReadyPod(String),
    /// The service targets a named port the pod does not declare.
    #[error("pod {pod} has no container port named {name}")]
    NoNamedPort {
        /// The pod.
        pod: String,
        /// The port name.
        name: String,
    },
    /// A port number outside 1 to 65535.
    #[error("port {0} is out of range")]
    BadPort(i32),
    /// The API call failed.
    #[error(transparent)]
    Api(#[from] kube::Error),
}

/// Resolves a target for one new connection.
pub(super) async fn resolve(
    client: &Client,
    namespace: &str,
    target: &ForwardTarget,
    port: u16,
) -> Result<Resolved, ResolveError> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    match target {
        ForwardTarget::Pod(name) => {
            let pod = pods.get(name).await?;
            let phase = phase(&pod);
            if phase != "Running" {
                return Err(ResolveError::PodNotRunning {
                    pod: name.to_string(),
                    phase: phase.to_owned(),
                });
            }
            Ok(Resolved {
                pod: name.to_string(),
                port,
            })
        }
        ForwardTarget::Service(name) => {
            let services: Api<Service> = Api::namespaced(client.clone(), namespace);
            let service = services.get(name).await?;
            let selector =
                selector(&service).ok_or_else(|| ResolveError::NoSelector(name.to_string()))?;
            let candidates = pods.list(&ListParams::default().labels(&selector)).await?;
            let pod = pick_ready(&candidates.items)
                .ok_or_else(|| ResolveError::NoReadyPod(name.to_string()))?;
            let port = container_port(&service, port, pod)?;
            Ok(Resolved {
                pod: pod.metadata.name.clone().unwrap_or_default(),
                port,
            })
        }
    }
}

fn phase(pod: &Pod) -> &str {
    pod.status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .unwrap_or("Unknown")
}

/// The service selector as a label selector, like `app=web,tier=front`.
/// `None` when the service selects nothing.
fn selector(service: &Service) -> Option<String> {
    let selector = service.spec.as_ref()?.selector.as_ref()?;
    if selector.is_empty() {
        return None;
    }
    // A BTreeMap, so the order is stable.
    Some(
        selector
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(","),
    )
}

/// The first ready, running pod that is not being deleted, by name.
fn pick_ready(pods: &[Pod]) -> Option<&Pod> {
    pods.iter()
        .filter(|pod| pod.metadata.deletion_timestamp.is_none() && is_ready(pod))
        .min_by(|a, b| a.metadata.name.cmp(&b.metadata.name))
}

fn is_ready(pod: &Pod) -> bool {
    let Some(status) = &pod.status else {
        return false;
    };
    status.phase.as_deref() == Some("Running")
        && status
            .conditions
            .iter()
            .flatten()
            .any(|c| c.type_ == "Ready" && c.status == "True")
}

/// Turns a service port into the pod's container port.
fn container_port(service: &Service, port: u16, pod: &Pod) -> Result<u16, ResolveError> {
    let service_name = service.metadata.name.clone().unwrap_or_default();
    let entry = service
        .spec
        .as_ref()
        .and_then(|s| s.ports.as_ref())
        .and_then(|ports| ports.iter().find(|p| p.port == i32::from(port)))
        .ok_or(ResolveError::NoServicePort {
            service: service_name,
            port,
        })?;
    match &entry.target_port {
        // No target port means the same number as the service port.
        None => Ok(port),
        Some(IntOrString::Int(n)) => u16::try_from(*n).map_err(|_| ResolveError::BadPort(*n)),
        Some(IntOrString::String(name)) => {
            let declared = pod
                .spec
                .iter()
                .flat_map(|s| s.containers.iter())
                .flat_map(|c| c.ports.iter().flatten())
                .find(|p| p.name.as_deref() == Some(name.as_str()))
                .ok_or_else(|| ResolveError::NoNamedPort {
                    pod: pod.metadata.name.clone().unwrap_or_default(),
                    name: name.clone(),
                })?;
            u16::try_from(declared.container_port)
                .map_err(|_| ResolveError::BadPort(declared.container_port))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        Container, ContainerPort, PodCondition, PodSpec, PodStatus, ServicePort, ServiceSpec,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};

    fn pod(name: &str, phase: &str, ready: bool) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                ..ObjectMeta::default()
            },
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: "app".to_owned(),
                    ports: Some(vec![ContainerPort {
                        name: Some("http".to_owned()),
                        container_port: 8080,
                        ..ContainerPort::default()
                    }]),
                    ..Container::default()
                }],
                ..PodSpec::default()
            }),
            status: Some(PodStatus {
                phase: Some(phase.to_owned()),
                conditions: Some(vec![PodCondition {
                    type_: "Ready".to_owned(),
                    status: if ready { "True" } else { "False" }.to_owned(),
                    ..PodCondition::default()
                }]),
                ..PodStatus::default()
            }),
        }
    }

    fn service(target_port: Option<IntOrString>) -> Service {
        Service {
            metadata: ObjectMeta {
                name: Some("gateway".to_owned()),
                ..ObjectMeta::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(
                    [
                        ("app".to_owned(), "gateway".to_owned()),
                        ("tier".to_owned(), "edge".to_owned()),
                    ]
                    .into(),
                ),
                ports: Some(vec![ServicePort {
                    name: Some("http".to_owned()),
                    port: 80,
                    target_port,
                    ..ServicePort::default()
                }]),
                ..ServiceSpec::default()
            }),
            ..Service::default()
        }
    }

    #[test]
    fn only_ready_running_pods_are_picked() {
        let mut deleting = pod("a-deleting", "Running", true);
        deleting.metadata.deletion_timestamp = Some(Time("2026-11-09T00:00:00Z".parse().unwrap()));
        let pods = [
            deleting,
            pod("b-not-ready", "Running", false),
            pod("c-pending", "Pending", true),
            pod("e-ready", "Running", true),
            pod("d-ready", "Running", true),
        ];
        assert_eq!(
            pick_ready(&pods).and_then(|p| p.metadata.name.as_deref()),
            Some("d-ready")
        );
        assert!(pick_ready(&pods[..3]).is_none());
    }

    #[test]
    fn target_ports_resolve_by_number_name_or_default() {
        let web = pod("web", "Running", true);
        assert_eq!(container_port(&service(None), 80, &web).unwrap(), 80);
        assert_eq!(
            container_port(&service(Some(IntOrString::Int(9000))), 80, &web).unwrap(),
            9000
        );
        assert_eq!(
            container_port(&service(Some(IntOrString::String("http".into()))), 80, &web).unwrap(),
            8080
        );
    }

    #[test]
    fn missing_ports_explain_themselves() {
        let web = pod("web", "Running", true);
        let err = container_port(&service(None), 443, &web).unwrap_err();
        assert_eq!(err.to_string(), "service gateway has no port 443");
        let err = container_port(&service(Some(IntOrString::String("grpc".into()))), 80, &web)
            .unwrap_err();
        assert_eq!(err.to_string(), "pod web has no container port named grpc");
        let err = container_port(&service(Some(IntOrString::Int(70_000))), 80, &web).unwrap_err();
        assert_eq!(err.to_string(), "port 70000 is out of range");
    }

    #[test]
    fn selectors_become_label_selectors() {
        assert_eq!(
            selector(&service(None)).as_deref(),
            Some("app=gateway,tier=edge")
        );
        let mut headless = service(None);
        if let Some(spec) = headless.spec.as_mut() {
            spec.selector = None;
        }
        assert_eq!(selector(&headless), None);
    }
}
