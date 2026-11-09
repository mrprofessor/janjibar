//! The kubeconfig catalog: every context across every kubeconfig file.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use kube::config::{Kubeconfig, KubeconfigError};
use model::{Catalog, ContextInfo, ContextName, SourceProblem};

/// The kubeconfig files to read, in priority order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KubeconfigSources {
    paths: Vec<PathBuf>,
}

impl KubeconfigSources {
    /// Uses exactly these files, in this order.
    pub fn new(paths: Vec<PathBuf>) -> Self {
        Self { paths }
    }

    /// The files named in `KUBECONFIG`, then `~/.kube/config` if it is not
    /// already among them.
    pub fn from_env() -> Self {
        Self::from_parts(
            std::env::var_os("KUBECONFIG").as_deref(),
            std::env::home_dir().as_deref(),
        )
    }

    /// Builds the source list from a `KUBECONFIG` value and a home directory.
    /// Empty entries in `KUBECONFIG` are ignored, as kubectl does.
    pub fn from_parts(kubeconfig: Option<&OsStr>, home: Option<&Path>) -> Self {
        let mut paths: Vec<PathBuf> = kubeconfig
            .map(|value| {
                std::env::split_paths(value)
                    .filter(|p| !p.as_os_str().is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if let Some(home) = home {
            let default = home.join(".kube").join("config");
            if !paths.contains(&default) {
                paths.push(default);
            }
        }
        Self { paths }
    }

    /// The files, in order.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

/// The result of reading every source.
#[derive(Clone, Debug, Default)]
pub struct Loaded {
    /// What the UI shows.
    pub catalog: Catalog,
    /// All readable files merged with kubectl's rules (first value wins).
    /// Clients are built from this, so a context may use a user or cluster
    /// defined in another file.
    pub merged: Kubeconfig,
}

/// Reads every source file and builds the catalog.
///
/// Missing files are skipped silently, like kubectl. Files that exist but
/// cannot be read or parsed are reported in [`Catalog::problems`] and the
/// other files still load. This does blocking file I/O.
pub fn load(sources: &KubeconfigSources) -> Loaded {
    let mut catalog = Catalog {
        sources: sources.paths().to_vec(),
        ..Catalog::default()
    };
    let mut merged = Kubeconfig::default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut files: Vec<(&Path, Kubeconfig)> = Vec::new();

    for path in sources.paths() {
        let config = match Kubeconfig::read_from(path) {
            Ok(config) => config,
            Err(err) if is_not_found(&err) => continue,
            Err(err) => {
                catalog.problems.push(SourceProblem {
                    path: path.clone(),
                    message: describe(&err),
                });
                continue;
            }
        };
        merged = match merged.merge(config.clone()) {
            Ok(next) => next,
            Err(err) => {
                // Merge only fails on kind or apiVersion mismatches, and it
                // consumed `merged`. Rebuild it from the files read so far.
                catalog.problems.push(SourceProblem {
                    path: path.clone(),
                    message: describe(&err),
                });
                rebuild(&files)
            }
        };
        files.push((path, config));
    }

    for (path, config) in &files {
        if catalog.current.is_none() {
            catalog.current = config
                .current_context
                .as_deref()
                .filter(|name| !name.is_empty())
                .map(ContextName::from);
        }
        for named in &config.contexts {
            let info = context_info(path, named, &merged);
            if seen.insert(named.name.clone()) {
                catalog.contexts.push(info);
            } else {
                catalog.shadowed.push(info);
            }
        }
    }

    Loaded { catalog, merged }
}

fn rebuild(files: &[(&Path, Kubeconfig)]) -> Kubeconfig {
    files
        .iter()
        .try_fold(Kubeconfig::default(), |acc, (_, config)| {
            acc.merge(config.clone())
        })
        .unwrap_or_default()
}

fn context_info(
    path: &Path,
    named: &kube::config::NamedContext,
    merged: &Kubeconfig,
) -> ContextInfo {
    let context = named.context.as_ref();
    let cluster = context.map(|c| c.cluster.clone()).unwrap_or_default();
    let server = merged
        .clusters
        .iter()
        .find(|c| c.name == cluster)
        .and_then(|c| c.cluster.as_ref())
        .and_then(|c| c.server.clone());
    ContextInfo {
        name: ContextName::new(named.name.as_str()),
        cluster,
        user: context.and_then(|c| c.user.clone()).unwrap_or_default(),
        namespace: context.and_then(|c| c.namespace.clone()),
        server,
        source: path.to_path_buf(),
    }
}

fn is_not_found(err: &KubeconfigError) -> bool {
    matches!(err, KubeconfigError::ReadConfig(io, _) if io.kind() == io::ErrorKind::NotFound)
}

fn describe(err: &KubeconfigError) -> String {
    match err {
        KubeconfigError::ReadConfig(io, _) => format!("could not read the file: {io}"),
        KubeconfigError::Parse(e) => format!("not valid kubeconfig YAML: {e}"),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn names(catalog: &Catalog) -> Vec<&str> {
        catalog.contexts.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn sources_from_kubeconfig_then_home() {
        let var = std::env::join_paths(["/a/one", "", "/b/two"]).unwrap();
        let sources = KubeconfigSources::from_parts(Some(&var), Some(Path::new("/home/me")));
        assert_eq!(
            sources.paths(),
            [
                PathBuf::from("/a/one"),
                PathBuf::from("/b/two"),
                PathBuf::from("/home/me/.kube/config"),
            ]
        );
    }

    #[test]
    fn home_config_is_not_listed_twice() {
        let var = std::env::join_paths(["/home/me/.kube/config", "/x"]).unwrap();
        let sources = KubeconfigSources::from_parts(Some(&var), Some(Path::new("/home/me")));
        assert_eq!(
            sources.paths(),
            [PathBuf::from("/home/me/.kube/config"), PathBuf::from("/x")]
        );
    }

    #[test]
    fn no_kubeconfig_var_means_home_only() {
        let sources = KubeconfigSources::from_parts(None, Some(Path::new("/h")));
        assert_eq!(sources.paths(), [PathBuf::from("/h/.kube/config")]);
    }

    #[test]
    fn merges_contexts_across_files_in_order() {
        let sources = KubeconfigSources::new(vec![fixture("work.yaml"), fixture("home.yaml")]);
        let loaded = load(&sources);
        let catalog = &loaded.catalog;

        assert_eq!(names(catalog), ["prod-eu", "staging", "kind-dev"]);
        assert_eq!(catalog.problems, []);

        let staging = catalog.get(&"staging".into()).unwrap();
        assert_eq!(staging.source, fixture("work.yaml"));
        assert_eq!(staging.namespace.as_deref(), Some("web"));
        assert_eq!(
            staging.server.as_deref(),
            Some("https://staging.example.com")
        );

        let kind = catalog.get(&"kind-dev".into()).unwrap();
        assert_eq!(kind.source, fixture("home.yaml"));
        assert_eq!(kind.server.as_deref(), Some("https://127.0.0.1:6443"));
    }

    #[test]
    fn first_file_wins_on_duplicates() {
        let sources = KubeconfigSources::new(vec![fixture("work.yaml"), fixture("home.yaml")]);
        let catalog = load(&sources).catalog;

        // home.yaml also defines `staging`; work.yaml came first.
        let staging = catalog.get(&"staging".into()).unwrap();
        assert_eq!(staging.source, fixture("work.yaml"));
        assert_eq!(catalog.shadowed.len(), 1);
        assert_eq!(catalog.shadowed[0].name.as_str(), "staging");
        assert_eq!(catalog.shadowed[0].source, fixture("home.yaml"));

        // current-context comes from the first file that sets one.
        assert_eq!(catalog.current, Some("prod-eu".into()));
    }

    #[test]
    fn order_decides_current_context() {
        let sources = KubeconfigSources::new(vec![fixture("home.yaml"), fixture("work.yaml")]);
        let catalog = load(&sources).catalog;
        assert_eq!(catalog.current, Some("kind-dev".into()));
        assert_eq!(names(&catalog), ["kind-dev", "staging", "prod-eu"]);
    }

    #[test]
    fn context_can_use_a_cluster_from_another_file() {
        // cross-file.yaml defines a context whose cluster lives in work.yaml.
        let sources =
            KubeconfigSources::new(vec![fixture("work.yaml"), fixture("cross-file.yaml")]);
        let catalog = load(&sources).catalog;
        let cross = catalog.get(&"prod-eu-admin".into()).unwrap();
        assert_eq!(cross.source, fixture("cross-file.yaml"));
        assert_eq!(cross.server.as_deref(), Some("https://prod-eu.example.com"));
    }

    #[test]
    fn missing_files_are_skipped_quietly() {
        let sources =
            KubeconfigSources::new(vec![fixture("does-not-exist.yaml"), fixture("home.yaml")]);
        let catalog = load(&sources).catalog;
        assert_eq!(names(&catalog), ["kind-dev", "staging"]);
        assert_eq!(catalog.problems, []);
        assert_eq!(catalog.sources.len(), 2);
    }

    #[test]
    fn broken_file_is_reported_and_others_still_load() {
        let sources = KubeconfigSources::new(vec![fixture("broken.yaml"), fixture("home.yaml")]);
        let catalog = load(&sources).catalog;
        assert_eq!(names(&catalog), ["kind-dev", "staging"]);
        assert_eq!(catalog.problems.len(), 1);
        assert_eq!(catalog.problems[0].path, fixture("broken.yaml"));
    }

    #[test]
    fn no_files_means_an_empty_catalog() {
        let sources = KubeconfigSources::new(vec![fixture("nope-1.yaml"), fixture("nope-2.yaml")]);
        let catalog = load(&sources).catalog;
        assert!(catalog.is_empty());
        assert!(catalog.current.is_none());
        assert_eq!(catalog.problems, []);
    }

    #[test]
    fn multi_document_file_is_merged() {
        let sources = KubeconfigSources::new(vec![fixture("multi-doc.yaml")]);
        let catalog = load(&sources).catalog;
        assert_eq!(names(&catalog), ["first", "second"]);
    }
}
