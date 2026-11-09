//! The kubeconfig catalog: every context the user has, and where it came from.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

/// The name of a kubeconfig context. Unique within a [`Catalog`].
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContextName(Arc<str>);

impl ContextName {
    /// Wraps a context name.
    pub fn new(name: impl Into<Arc<str>>) -> Self {
        Self(name.into())
    }

    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The name as a shared string, for cheap hand-off to the UI.
    pub fn as_arc(&self) -> &Arc<str> {
        &self.0
    }
}

impl fmt::Debug for ContextName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl fmt::Display for ContextName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ContextName {
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

/// One context from a kubeconfig file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextInfo {
    /// The context's name.
    pub name: ContextName,
    /// The cluster entry it points at.
    pub cluster: String,
    /// The user entry it points at.
    pub user: String,
    /// The default namespace, if the context sets one.
    pub namespace: Option<String>,
    /// The API server URL, if the cluster entry was found.
    pub server: Option<String>,
    /// The file this context was read from.
    pub source: PathBuf,
}

/// A kubeconfig file that could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceProblem {
    /// The file.
    pub path: PathBuf,
    /// What went wrong, in plain words.
    pub message: String,
}

/// Every context across the user's kubeconfig files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalog {
    /// Contexts in file order. When two files define the same name, the
    /// first one wins, as with kubectl.
    pub contexts: Vec<ContextInfo>,
    /// The `current-context` from the first file that sets one.
    pub current: Option<ContextName>,
    /// The files that were considered, in order, whether or not they exist.
    pub sources: Vec<PathBuf>,
    /// Files that exist but could not be read or parsed.
    pub problems: Vec<SourceProblem>,
    /// Contexts hidden because an earlier file used the same name.
    pub shadowed: Vec<ContextInfo>,
}

impl Catalog {
    /// Finds a context by name.
    pub fn get(&self, name: &ContextName) -> Option<&ContextInfo> {
        self.contexts.iter().find(|c| &c.name == name)
    }

    /// True when no file produced any context.
    pub fn is_empty(&self) -> bool {
        self.contexts.is_empty()
    }
}
