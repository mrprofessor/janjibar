//! Saved forwards: a small JSON file, grouped by cluster.
//!
//! ```json
//! {
//!   "version": 1,
//!   "clusters": {
//!     "kind-platform": [
//!       { "namespace": "gateway", "service": "gateway", "port": 80, "localPort": 8080 }
//!     ]
//!   }
//! }
//! ```

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use model::{ContextName, ForwardSpec, ForwardTarget};
use serde::{Deserialize, Serialize};

/// The file's format version. Bump it when the shape changes.
const VERSION: u32 = 1;

/// The file name inside the app's config folder.
const FILE_NAME: &str = "forwards.json";

/// Why the saved forwards could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum SavedError {
    /// The file could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        #[source]
        source: io::Error,
    },
    /// The file is not valid JSON in the expected shape.
    #[error("{path} is not a valid forwards file: {source}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        #[source]
        source: serde_json::Error,
    },
}

/// Where forwards are saved: `$XDG_CONFIG_HOME/<app>/forwards.json`, or
/// `~/.config/<app>/forwards.json`. `None` when neither variable is set.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })?;
    Some(base.join(model::brand::CONFIG_DIR).join(FILE_NAME))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    version: u32,
    #[serde(default)]
    clusters: BTreeMap<String, Vec<Entry>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    namespace: String,
    #[serde(flatten)]
    target: Target,
    port: u16,
    #[serde(default)]
    local_port: u16,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Target {
    Pod(String),
    Service(String),
}

/// Reads saved forwards. A missing file means none.
pub fn load(path: &Path) -> Result<Vec<ForwardSpec>, SavedError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(SavedError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    let file: File = serde_json::from_str(&text).map_err(|source| SavedError::Parse {
        path: path.to_owned(),
        source,
    })?;
    Ok(from_file(file))
}

/// Writes forwards, replacing the file in one step so a crash never leaves
/// half a file behind.
pub fn save(path: &Path, specs: &[ForwardSpec]) -> Result<(), SavedError> {
    let io_error = |source| SavedError::Io {
        path: path.to_owned(),
        source,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io_error)?;
    }
    let mut text =
        serde_json::to_string_pretty(&to_file(specs)).map_err(|source| SavedError::Parse {
            path: path.to_owned(),
            source,
        })?;
    text.push('\n');
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text).map_err(io_error)?;
    std::fs::rename(&temp, path).map_err(io_error)
}

fn from_file(file: File) -> Vec<ForwardSpec> {
    file.clusters
        .into_iter()
        .flat_map(|(context, entries)| {
            let context = ContextName::new(context);
            entries.into_iter().map(move |e| ForwardSpec {
                context: context.clone(),
                namespace: e.namespace.into(),
                target: match e.target {
                    Target::Pod(name) => ForwardTarget::Pod(name.into()),
                    Target::Service(name) => ForwardTarget::Service(name.into()),
                },
                port: e.port,
                local_port: e.local_port,
            })
        })
        .collect()
}

fn to_file(specs: &[ForwardSpec]) -> File {
    let mut clusters: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    for spec in specs {
        clusters
            .entry(spec.context.as_str().to_owned())
            .or_default()
            .push(Entry {
                namespace: spec.namespace.to_string(),
                target: match &spec.target {
                    ForwardTarget::Pod(name) => Target::Pod(name.to_string()),
                    ForwardTarget::Service(name) => Target::Service(name.to_string()),
                },
                port: spec.port,
                local_port: spec.local_port,
            });
    }
    File {
        version: VERSION,
        clusters,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(context: &str, target: ForwardTarget, port: u16, local_port: u16) -> ForwardSpec {
        ForwardSpec {
            context: context.into(),
            namespace: "shop".into(),
            target,
            port,
            local_port,
        }
    }

    #[test]
    fn forwards_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("forwards.json");
        let specs = vec![
            spec("kind", ForwardTarget::Service("gateway".into()), 80, 8080),
            spec("kind", ForwardTarget::Pod("web-0".into()), 9090, 0),
            spec("prod", ForwardTarget::Pod("db-0".into()), 5432, 15432),
        ];
        save(&path, &specs).unwrap();
        assert_eq!(load(&path).unwrap(), specs);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn the_file_groups_forwards_by_cluster() {
        let text = serde_json::to_value(to_file(&[
            spec("kind", ForwardTarget::Service("gateway".into()), 80, 8080),
            spec("kind", ForwardTarget::Pod("web-0".into()), 9090, 0),
        ]))
        .unwrap();
        assert_eq!(
            text,
            serde_json::json!({
                "version": 1,
                "clusters": {
                    "kind": [
                        { "namespace": "shop", "service": "gateway", "port": 80, "localPort": 8080 },
                        { "namespace": "shop", "pod": "web-0", "port": 9090, "localPort": 0 },
                    ],
                },
            })
        );
    }

    #[test]
    fn a_missing_file_means_no_forwards() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(&dir.path().join("none.json")).unwrap(), []);
    }

    #[test]
    fn a_broken_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forwards.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = load(&path).unwrap_err();
        assert!(matches!(err, SavedError::Parse { .. }), "{err}");
    }

    #[test]
    fn local_port_is_optional() {
        let file: File = serde_json::from_str(
            r#"{ "version": 1, "clusters": { "kind": [ { "namespace": "a", "pod": "p", "port": 1 } ] } }"#,
        )
        .unwrap();
        assert_eq!(from_file(file)[0].local_port, 0);
    }
}
