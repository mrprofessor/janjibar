//! Services and Ingresses.

use std::sync::Arc;

use k8s_openapi::api::core::v1::{LoadBalancerIngress, Service};
use k8s_openapi::api::networking::v1::Ingress;
use k8s_openapi::jiff::Timestamp;
use model::Health;
use model::resource::{Details, Field, Kind, Section};
use model::{Column, ColumnType, TableRow};

use crate::Spec;
use crate::common::{self, DASH, col, join, or_dash};

/// The Services spec.
pub struct Services;

fn lb_addresses(ingress: Option<&Vec<LoadBalancerIngress>>) -> Vec<String> {
    ingress
        .into_iter()
        .flatten()
        .filter_map(|i| i.ip.clone().or_else(|| i.hostname.clone()))
        .collect()
}

fn external(s: &Service) -> Arc<str> {
    let spec = s.spec.as_ref();
    let mut ips: Vec<String> = spec
        .and_then(|s| s.external_ips.clone())
        .unwrap_or_default();
    ips.extend(lb_addresses(
        s.status
            .as_ref()
            .and_then(|st| st.load_balancer.as_ref())
            .and_then(|lb| lb.ingress.as_ref()),
    ));
    if let Some(name) = spec.and_then(|s| s.external_name.as_deref()) {
        ips.push(name.to_owned());
    }
    if ips.is_empty() && spec.and_then(|s| s.type_.as_deref()) == Some("LoadBalancer") {
        return "<pending>".into();
    }
    join(ips)
}

fn ports(s: &Service) -> Arc<str> {
    join(
        s.spec
            .as_ref()
            .and_then(|s| s.ports.as_ref())
            .into_iter()
            .flatten()
            .map(|p| {
                let proto = p.protocol.as_deref().unwrap_or("TCP");
                match p.node_port {
                    Some(np) => format!("{}:{np}/{proto}", p.port),
                    None => format!("{}/{proto}", p.port),
                }
            }),
    )
}

impl Spec for Services {
    type Object = Service;
    const KIND: Kind = Kind::Service;

    fn columns() -> Vec<Column> {
        vec![
            col("Name", ColumnType::String, "name"),
            col("Type", ColumnType::String, ""),
            col("Cluster IP", ColumnType::String, ""),
            col("External IP", ColumnType::String, ""),
            col("Ports", ColumnType::String, "muted"),
            col("Age", ColumnType::Date, ""),
        ]
    }

    fn row(s: &Service, now: Timestamp) -> TableRow {
        let spec = s.spec.as_ref();
        common::row(
            &s.metadata,
            vec![
                common::name(&s.metadata),
                or_dash(spec.and_then(|s| s.type_.as_deref()).or(Some("ClusterIP"))),
                or_dash(spec.and_then(|s| s.cluster_ip.as_deref())),
                external(s),
                ports(s),
                common::age(&s.metadata, now),
            ],
            Health::Unknown,
        )
    }

    fn details(s: &Service, now: Timestamp) -> Details {
        let mut out = common::base_details(Kind::Service, &s.metadata, now);
        let spec = s.spec.as_ref();
        out.status = or_dash(spec.and_then(|s| s.type_.as_deref()).or(Some("ClusterIP")));
        let mut fields = vec![
            Field::new(
                "Cluster IPs",
                join(spec.and_then(|s| s.cluster_ips.clone()).unwrap_or_default()),
            ),
            Field::new("External", external(s)),
            Field::new(
                "Selector",
                common::pairs(spec.and_then(|s| s.selector.as_ref())),
            ),
            Field::new(
                "Session affinity",
                or_dash(spec.and_then(|s| s.session_affinity.as_deref())),
            ),
        ];
        fields.append(&mut out.fields);
        out.fields = fields;
        let port_fields: Vec<Field> = spec
            .and_then(|s| s.ports.as_ref())
            .into_iter()
            .flatten()
            .map(|p| {
                let target = p.target_port.as_ref().map_or_else(
                    || p.port.to_string(),
                    |t| match t {
                        k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(n) => {
                            n.to_string()
                        }
                        k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::String(s) => {
                            s.clone()
                        }
                    },
                );
                let proto = p.protocol.as_deref().unwrap_or("TCP");
                let value = match p.node_port {
                    Some(np) => format!("{} to {target}/{proto}, node port {np}", p.port),
                    None => format!("{} to {target}/{proto}", p.port),
                };
                Field::new(p.name.as_deref().unwrap_or("port"), value)
            })
            .collect();
        if !port_fields.is_empty() {
            out.sections.push(Section {
                title: "Ports".into(),
                fields: port_fields,
            });
        }
        out
    }
}

/// The Ingresses spec.
pub struct Ingresses;

fn hosts(i: &Ingress) -> Arc<str> {
    join(
        i.spec
            .as_ref()
            .and_then(|s| s.rules.as_ref())
            .into_iter()
            .flatten()
            .map(|r| r.host.clone().unwrap_or_else(|| "*".to_owned())),
    )
}

fn address(i: &Ingress) -> Arc<str> {
    join(
        i.status
            .as_ref()
            .and_then(|s| s.load_balancer.as_ref())
            .and_then(|lb| lb.ingress.as_ref())
            .into_iter()
            .flatten()
            .filter_map(|x| x.ip.clone().or_else(|| x.hostname.clone())),
    )
}

impl Spec for Ingresses {
    type Object = Ingress;
    const KIND: Kind = Kind::Ingress;

    fn columns() -> Vec<Column> {
        vec![
            col("Name", ColumnType::String, "name"),
            col("Class", ColumnType::String, ""),
            col("Hosts", ColumnType::String, ""),
            col("Address", ColumnType::String, "muted"),
            col("Age", ColumnType::Date, ""),
        ]
    }

    fn row(i: &Ingress, now: Timestamp) -> TableRow {
        common::row(
            &i.metadata,
            vec![
                common::name(&i.metadata),
                or_dash(
                    i.spec
                        .as_ref()
                        .and_then(|s| s.ingress_class_name.as_deref()),
                ),
                hosts(i),
                address(i),
                common::age(&i.metadata, now),
            ],
            Health::Unknown,
        )
    }

    fn details(i: &Ingress, now: Timestamp) -> Details {
        let mut out = common::base_details(Kind::Ingress, &i.metadata, now);
        let spec = i.spec.as_ref();
        out.status = "Ingress".into();
        let tls = join(
            spec.and_then(|s| s.tls.as_ref())
                .into_iter()
                .flatten()
                .flat_map(|t| t.hosts.clone().unwrap_or_default()),
        );
        let mut fields = vec![
            Field::new(
                "Class",
                or_dash(spec.and_then(|s| s.ingress_class_name.as_deref())),
            ),
            Field::new("Address", address(i)),
            Field::new("TLS hosts", tls),
        ];
        fields.append(&mut out.fields);
        out.fields = fields;
        let mut rules = Vec::new();
        for rule in spec.and_then(|s| s.rules.as_ref()).into_iter().flatten() {
            let host = rule.host.as_deref().unwrap_or("*");
            for path in rule.http.iter().flat_map(|h| h.paths.iter()) {
                let backend = path.backend.service.as_ref().map_or_else(
                    || DASH.to_owned(),
                    |svc| {
                        let port = svc.port.as_ref().map_or_else(String::new, |p| {
                            p.number.map_or_else(
                                || p.name.clone().unwrap_or_default(),
                                |n| n.to_string(),
                            )
                        });
                        format!("{}:{port}", svc.name)
                    },
                );
                rules.push(Field::new(
                    format!("{host}{}", path.path.as_deref().unwrap_or("/")),
                    backend,
                ));
            }
        }
        if !rules.is_empty() {
            out.sections.push(Section {
                title: "Rules".into(),
                fields: rules,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{ServicePort, ServiceSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    #[test]
    fn service_row() {
        let svc = Service {
            metadata: ObjectMeta {
                name: Some("api".into()),
                namespace: Some("shop".into()),
                ..ObjectMeta::default()
            },
            spec: Some(ServiceSpec {
                type_: Some("NodePort".into()),
                cluster_ip: Some("10.96.0.10".into()),
                ports: Some(vec![
                    ServicePort {
                        port: 80,
                        node_port: Some(30080),
                        ..ServicePort::default()
                    },
                    ServicePort {
                        port: 53,
                        protocol: Some("UDP".into()),
                        ..ServicePort::default()
                    },
                ]),
                ..ServiceSpec::default()
            }),
            status: None,
        };
        let row = Services::row(&svc, Timestamp::UNIX_EPOCH);
        let cells: Vec<&str> = row.cells.iter().map(|c| &**c).collect();
        assert_eq!(
            cells,
            [
                "api",
                "NodePort",
                "10.96.0.10",
                "-",
                "80:30080/TCP, 53/UDP",
                "-"
            ]
        );
    }

    #[test]
    fn pending_load_balancer() {
        let svc = Service {
            spec: Some(ServiceSpec {
                type_: Some("LoadBalancer".into()),
                ..ServiceSpec::default()
            }),
            ..Service::default()
        };
        assert_eq!(&*external(&svc), "<pending>");
    }
}
