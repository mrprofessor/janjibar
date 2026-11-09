//! Command-line arguments.
//!
//! The app is a GUI first; flags exist for development and scripted checks:
//! open straight onto a context, a kind and an object.

use model::resource::{Kind, ObjectKey};

/// Parsed flags.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// Connect to this context at startup.
    pub context: Option<String>,
    /// Show this kind first.
    pub kind: Option<Kind>,
    /// Open this object in the inspector (`namespace/name` or `name`).
    pub inspect: Option<ObjectKey>,
    /// Open logs for this pod at startup, as (namespace, name).
    pub logs: Option<(String, String)>,
}

const USAGE: &str =
    "usage: [--context NAME] [--kind KIND] [--inspect NAMESPACE/NAME] [--logs NAMESPACE/POD]";

/// Parses flags. Each takes a value, as `--flag value` or `--flag=value`.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut out = Args::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f.to_owned(), Some(v.to_owned())),
            None => (arg, None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| it.next())
                .ok_or_else(|| format!("{flag} needs a value. {USAGE}"))
        };
        match flag.as_str() {
            "--context" => out.context = Some(value()?),
            "--kind" => {
                let name = value()?;
                out.kind = Some(parse_kind(&name).ok_or_else(|| {
                    format!("unknown kind `{name}`; try pods, deployments, nodes")
                })?);
            }
            "--logs" => {
                let pod = value()?;
                let (ns, name) = pod
                    .split_once('/')
                    .ok_or_else(|| format!("--logs needs NAMESPACE/POD. {USAGE}"))?;
                out.logs = Some((ns.to_owned(), name.to_owned()));
            }
            "--inspect" => {
                let target = value()?;
                out.inspect = Some(match target.split_once('/') {
                    Some((ns, name)) => ObjectKey::namespaced(ns, name),
                    None => ObjectKey::cluster(target.as_str()),
                });
            }
            other => return Err(format!("unknown argument `{other}`. {USAGE}")),
        }
    }
    Ok(out)
}

/// A kind by its singular or plural name, any case.
fn parse_kind(name: &str) -> Option<Kind> {
    Kind::ALL
        .into_iter()
        .find(|k| k.singular().eq_ignore_ascii_case(name) || k.plural().eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args, String> {
        parse(list.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn parses_every_flag() {
        let parsed = args(&[
            "--context",
            "kind-platform",
            "--kind=deployments",
            "--inspect",
            "faults/oomkilled",
        ])
        .unwrap();
        assert_eq!(parsed.context.as_deref(), Some("kind-platform"));
        assert_eq!(parsed.kind, Some(Kind::Deployment));
        assert_eq!(
            parsed.inspect,
            Some(ObjectKey::namespaced("faults", "oomkilled"))
        );
    }

    #[test]
    fn kinds_by_singular_or_plural() {
        assert_eq!(parse_kind("Pod"), Some(Kind::Pod));
        assert_eq!(parse_kind("statefulsets"), Some(Kind::StatefulSet));
        assert_eq!(parse_kind("PVCs"), Some(Kind::PersistentVolumeClaim));
        assert_eq!(parse_kind("widgets"), None);
    }

    #[test]
    fn cluster_scoped_inspect_target() {
        let parsed = args(&["--inspect", "worker-1"]).unwrap();
        assert_eq!(parsed.inspect, Some(ObjectKey::cluster("worker-1")));
    }

    #[test]
    fn logs_need_a_namespace() {
        let parsed = args(&["--context", "kind", "--logs", "faults/crashloop-0"]).unwrap();
        assert_eq!(
            parsed.logs,
            Some(("faults".to_owned(), "crashloop-0".to_owned()))
        );
        assert!(
            args(&["--logs", "crashloop-0"])
                .unwrap_err()
                .contains("NAMESPACE/POD")
        );
    }

    #[test]
    fn errors_name_the_problem() {
        assert!(args(&["--context"]).unwrap_err().contains("needs a value"));
        assert!(
            args(&["--kind", "nope"])
                .unwrap_err()
                .contains("unknown kind")
        );
        assert!(
            args(&["--verbose"])
                .unwrap_err()
                .contains("unknown argument")
        );
    }
}
