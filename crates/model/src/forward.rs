//! Port-forwards: what the user asked for and how each one is doing.

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::ContextName;

/// Identifies one forward for as long as the engine runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ForwardId(u64);

impl ForwardId {
    /// Wraps a raw id. The engine hands these out; the UI only echoes them.
    pub fn new(id: u64) -> Self {
        Self(id)
    }

    /// The raw id.
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ForwardId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What a forward connects to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ForwardTarget {
    /// One pod, by name.
    Pod(Arc<str>),
    /// A service, by name. Each connection goes to a ready pod behind it,
    /// picked when the connection opens, so the forward survives restarts.
    Service(Arc<str>),
}

impl ForwardTarget {
    /// The target's name.
    pub fn name(&self) -> &str {
        match self {
            Self::Pod(name) | Self::Service(name) => name,
        }
    }
}

impl fmt::Display for ForwardTarget {
    /// kubectl's spelling: `pod/web-0` or `svc/gateway`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pod(name) => write!(f, "pod/{name}"),
            Self::Service(name) => write!(f, "svc/{name}"),
        }
    }
}

/// A forward as the user asked for it. This is what gets saved.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ForwardSpec {
    /// The cluster.
    pub context: ContextName,
    /// The target's namespace.
    pub namespace: Arc<str>,
    /// The pod or service.
    pub target: ForwardTarget,
    /// A container port for pods, or a service port for services.
    pub port: u16,
    /// The local port on 127.0.0.1. 0 lets the system pick one; the engine
    /// then keeps the port it got, so it stays the same after a restart.
    pub local_port: u16,
}

impl ForwardSpec {
    /// Parses a forward written the way kubectl takes it:
    /// `[namespace/][pod/|svc/]name [local:]port`.
    ///
    /// `gateway/svc/gateway 8080:80` forwards local port 8080 to port 80 of
    /// service `gateway` in namespace `gateway`. A bare name is a pod, and
    /// a missing namespace is `default_namespace`. Without a local port, the
    /// system picks one.
    pub fn parse(
        input: &str,
        context: &ContextName,
        default_namespace: &str,
    ) -> Result<Self, ForwardParseError> {
        let mut words = input.split_whitespace();
        let (Some(target), Some(ports), None) = (words.next(), words.next(), words.next()) else {
            return Err(ForwardParseError::Shape);
        };
        let parts: Vec<&str> = target.split('/').collect();
        let (namespace, kind, name) = match parts.as_slice() {
            [name] => (default_namespace, "pod", *name),
            [kind, name] if is_kind(kind) => (default_namespace, *kind, *name),
            [namespace, name] => (*namespace, "pod", *name),
            [namespace, kind, name] => (*namespace, *kind, *name),
            _ => return Err(ForwardParseError::Shape),
        };
        if namespace.is_empty() || name.is_empty() {
            return Err(ForwardParseError::Shape);
        }
        let target = match kind {
            "pod" | "pods" | "po" => ForwardTarget::Pod(name.into()),
            "svc" | "service" | "services" => ForwardTarget::Service(name.into()),
            other => return Err(ForwardParseError::Kind(other.to_owned())),
        };
        let (local_port, port) = match ports.split_once(':') {
            Some((local, remote)) => (parse_port(local, true)?, parse_port(remote, false)?),
            None => (0, parse_port(ports, false)?),
        };
        Ok(Self {
            context: context.clone(),
            namespace: namespace.into(),
            target,
            port,
            local_port,
        })
    }
}

fn is_kind(word: &str) -> bool {
    matches!(word, "pod" | "pods" | "po" | "svc" | "service" | "services")
}

fn parse_port(text: &str, allow_zero: bool) -> Result<u16, ForwardParseError> {
    match text.parse::<u16>() {
        Ok(0) if !allow_zero => Err(ForwardParseError::Port(text.to_owned())),
        Ok(port) => Ok(port),
        Err(_) => Err(ForwardParseError::Port(text.to_owned())),
    }
}

/// Why a forward could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForwardParseError {
    /// Not of the form `[namespace/][pod/|svc/]name [local:]port`.
    Shape,
    /// A kind other than pod or service.
    Kind(String),
    /// A port that is not a number from 1 to 65535.
    Port(String),
}

impl fmt::Display for ForwardParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shape => f.write_str("write it as [namespace/][pod/|svc/]name [local:]port"),
            Self::Kind(kind) => write!(f, "`{kind}` is not pod or svc"),
            Self::Port(port) => write!(f, "`{port}` is not a port from 1 to 65535"),
        }
    }
}

impl std::error::Error for ForwardParseError {}

/// Where a forward stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForwardState {
    /// Saved, and starts when its cluster connects.
    Waiting,
    /// Connecting to the cluster or binding the local port.
    Starting,
    /// Accepting connections on its local port.
    Listening,
    /// Could not start, for example because the local port is taken.
    /// Starts again the next time its cluster connects.
    Failed(String),
}

/// One forward in the manager.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardRow {
    /// The id to stop it with.
    pub id: ForwardId,
    /// What it forwards.
    pub spec: ForwardSpec,
    /// Where it listens, once it does.
    pub local_addr: Option<SocketAddr>,
    /// Where it stands.
    pub state: ForwardState,
    /// Connections open right now.
    pub active: u32,
    /// Connections served since it started.
    pub total: u64,
    /// The pod the latest connection went to.
    pub last_pod: Option<Arc<str>>,
    /// Why the latest failed connection failed. Cleared by the next success.
    pub last_error: Option<Arc<str>>,
}

impl ForwardRow {
    /// True when it accepts connections.
    pub fn is_running(&self) -> bool {
        self.state == ForwardState::Listening
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> Result<ForwardSpec, ForwardParseError> {
        ForwardSpec::parse(input, &"kind".into(), "default")
    }

    #[test]
    fn forwards_parse_like_kubectl_arguments() {
        let spec = parse("gateway/svc/gateway 8080:80").unwrap();
        assert_eq!(&*spec.namespace, "gateway");
        assert_eq!(spec.target, ForwardTarget::Service("gateway".into()));
        assert_eq!((spec.local_port, spec.port), (8080, 80));
        assert_eq!(spec.context, "kind".into());

        let spec = parse("svc/web 80").unwrap();
        assert_eq!(&*spec.namespace, "default");
        assert_eq!(spec.target, ForwardTarget::Service("web".into()));
        assert_eq!((spec.local_port, spec.port), (0, 80));

        let spec = parse("  web-0   9090 ").unwrap();
        assert_eq!(spec.target, ForwardTarget::Pod("web-0".into()));
        let spec = parse("shop/web-0 9090").unwrap();
        assert_eq!(&*spec.namespace, "shop");
        assert_eq!(spec.target, ForwardTarget::Pod("web-0".into()));
        let spec = parse("shop/po/web-0 0:9090").unwrap();
        assert_eq!(spec.local_port, 0);
    }

    #[test]
    fn bad_forwards_explain_themselves() {
        assert_eq!(parse("svc/web"), Err(ForwardParseError::Shape));
        assert_eq!(parse("a/b/c/d 80"), Err(ForwardParseError::Shape));
        assert_eq!(parse("svc/ 80"), Err(ForwardParseError::Shape));
        assert_eq!(
            parse("shop/deploy/web 80"),
            Err(ForwardParseError::Kind("deploy".into()))
        );
        assert_eq!(parse("web 0"), Err(ForwardParseError::Port("0".into())));
        assert_eq!(
            parse("web 8080:http"),
            Err(ForwardParseError::Port("http".into()))
        );
        assert_eq!(
            ForwardParseError::Shape.to_string(),
            "write it as [namespace/][pod/|svc/]name [local:]port"
        );
    }

    #[test]
    fn targets_print_like_kubectl() {
        assert_eq!(ForwardTarget::Pod("web-0".into()).to_string(), "pod/web-0");
        assert_eq!(
            ForwardTarget::Service("gateway".into()).to_string(),
            "svc/gateway"
        );
        assert_eq!(ForwardTarget::Service("gateway".into()).name(), "gateway");
    }
}
