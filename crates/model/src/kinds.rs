//! The kinds a cluster serves, found through API discovery.

use std::fmt;
use std::sync::Arc;

use crate::ContextName;

/// Where a resource lives in the API: group, version and plural name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GroupVersionResource {
    /// The API group. Empty for the core group (pods, services, ...).
    pub group: Arc<str>,
    /// The version, like `v1` or `v1beta1`.
    pub version: Arc<str>,
    /// The plural name used in URLs, like `deployments`.
    pub resource: Arc<str>,
}

impl GroupVersionResource {
    /// The `apiVersion` string: `apps/v1`, or just `v1` for the core group.
    pub fn api_version(&self) -> String {
        if self.group.is_empty() {
            self.version.to_string()
        } else {
            format!("{}/{}", self.group, self.version)
        }
    }
}

impl fmt::Display for GroupVersionResource {
    /// kubectl's fully qualified form: `deployments.v1.apps`, or `pods.v1`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.group.is_empty() {
            write!(f, "{}.{}", self.resource, self.version)
        } else {
            write!(f, "{}.{}.{}", self.resource, self.version, self.group)
        }
    }
}

/// Whether objects of a kind live in a namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Objects belong to a namespace, like pods.
    Namespaced,
    /// Objects are cluster-wide, like nodes.
    Cluster,
}

/// An API verb a kind supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verb {
    /// Read one object.
    Get,
    /// List a collection.
    List,
    /// Watch a collection for changes.
    Watch,
    /// Create an object.
    Create,
    /// Replace an object.
    Update,
    /// Patch an object, including server-side apply.
    Patch,
    /// Delete one object.
    Delete,
    /// Delete many objects at once.
    DeleteCollection,
}

impl Verb {
    /// Every verb, in the order kubectl prints them.
    pub const ALL: [Self; 8] = [
        Self::Create,
        Self::Delete,
        Self::DeleteCollection,
        Self::Get,
        Self::List,
        Self::Patch,
        Self::Update,
        Self::Watch,
    ];

    /// The verb as the API spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::List => "list",
            Self::Watch => "watch",
            Self::Create => "create",
            Self::Update => "update",
            Self::Patch => "patch",
            Self::Delete => "delete",
            Self::DeleteCollection => "deletecollection",
        }
    }

    /// Parses an API verb. Returns `None` for verbs the app does not use.
    pub fn parse(verb: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == verb)
    }

    fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

impl fmt::Display for Verb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of [`Verb`]s.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Verbs(u8);

impl Verbs {
    /// Adds a verb.
    pub fn insert(&mut self, verb: Verb) {
        self.0 |= verb.bit();
    }

    /// True when the set holds `verb`.
    pub fn contains(self, verb: Verb) -> bool {
        self.0 & verb.bit() != 0
    }

    /// True when the set is empty.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The verbs in the set, in [`Verb::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = Verb> {
        Verb::ALL.into_iter().filter(move |v| self.contains(*v))
    }
}

impl FromIterator<Verb> for Verbs {
    fn from_iter<I: IntoIterator<Item = Verb>>(iter: I) -> Self {
        let mut verbs = Self::default();
        for verb in iter {
            verbs.insert(verb);
        }
        verbs
    }
}

impl fmt::Debug for Verbs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// One kind the server serves, like `Deployment` in `apps/v1`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KindInfo {
    /// Where to list it.
    pub resource: GroupVersionResource,
    /// The kind name, like `Deployment`.
    pub kind: Arc<str>,
    /// The singular resource name, like `deployment`. May be empty on old servers.
    pub singular: Arc<str>,
    /// Namespaced or cluster-wide.
    pub scope: Scope,
    /// What the server lets clients do with it.
    pub verbs: Verbs,
    /// Short names, like `deploy`.
    pub short_names: Arc<[Arc<str>]>,
    /// Categories, like `all`.
    pub categories: Arc<[Arc<str>]>,
}

impl KindInfo {
    /// True when `name` names this kind the way kubectl accepts it.
    ///
    /// Accepts the plural, singular, kind or a short name, in any case, and
    /// optionally qualified by group (`deployments.apps`) or by version and
    /// group (`deployments.v1.apps`).
    pub fn matches(&self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        let (base, qualifier) = match name.split_once('.') {
            Some((base, rest)) => (base, Some(rest)),
            None => (name.as_str(), None),
        };
        let base_matches = *self.resource.resource == *base
            || *self.singular == *base
            || self.kind.eq_ignore_ascii_case(base)
            || self.short_names.iter().any(|s| **s == *base);
        if !base_matches {
            return false;
        }
        let group = &*self.resource.group;
        let version = &*self.resource.version;
        let Some(q) = qualifier else {
            return true;
        };
        if q == group {
            return true;
        }
        match q.split_once('.') {
            Some((v, g)) => v == version && g == group,
            // `pods.v1`: a bare version only qualifies the core group.
            None => group.is_empty() && q == version,
        }
    }
}

/// The kinds of one API group, for the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KindGroup {
    /// The group name. Empty for the core group.
    pub name: Arc<str>,
    /// The version the server prefers for this group.
    pub preferred_version: Arc<str>,
    /// Kinds sorted by name. Each kind appears once, at the most preferred
    /// version that serves it.
    pub kinds: Vec<KindInfo>,
}

impl KindGroup {
    /// True for the core group (pods, services, config maps, ...).
    pub fn is_core(&self) -> bool {
        self.name.is_empty()
    }
}

/// A part of the API that discovery could not read. The rest still loads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryProblem {
    /// The group and version, like `metrics.k8s.io/v1beta1`.
    pub group_version: String,
    /// What went wrong, in plain words.
    pub message: String,
}

/// Every kind a context serves that can be listed, grouped by API group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KindCatalog {
    /// The context the kinds belong to.
    pub context: ContextName,
    /// Groups in the server's priority order: core first, custom resources
    /// usually last.
    pub groups: Vec<KindGroup>,
    /// Groups or versions that failed or are stale.
    pub problems: Vec<DiscoveryProblem>,
}

impl KindCatalog {
    /// Every kind, group by group.
    pub fn kinds(&self) -> impl Iterator<Item = &KindInfo> {
        self.groups.iter().flat_map(|g| g.kinds.iter())
    }

    /// Finds a kind by any name kubectl accepts (see [`KindInfo::matches`]).
    ///
    /// When several groups serve the same name, the group the server lists
    /// first wins, so `events` is the core kind, as with kubectl.
    pub fn find(&self, name: &str) -> Option<&KindInfo> {
        self.kinds().find(|k| k.matches(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deployment() -> KindInfo {
        KindInfo {
            resource: GroupVersionResource {
                group: "apps".into(),
                version: "v1".into(),
                resource: "deployments".into(),
            },
            kind: "Deployment".into(),
            singular: "deployment".into(),
            scope: Scope::Namespaced,
            verbs: [Verb::List, Verb::Watch].into_iter().collect(),
            short_names: vec!["deploy".into()].into(),
            categories: vec!["all".into()].into(),
        }
    }

    #[test]
    fn kinds_match_every_name_kubectl_accepts() {
        let kind = deployment();
        for name in [
            "deployments",
            "deployment",
            "Deployment",
            "deploy",
            "DEPLOY",
            "deployments.apps",
            "deploy.apps",
            "deployments.v1.apps",
        ] {
            assert!(kind.matches(name), "{name} should match");
        }
        for name in [
            "deployments.batch",
            "deployments.v2.apps",
            "deployments.v1",
            "dep",
            "pods",
        ] {
            assert!(!kind.matches(name), "{name} should not match");
        }
    }

    #[test]
    fn verbs_are_a_set() {
        let verbs: Verbs = [Verb::Watch, Verb::List, Verb::List].into_iter().collect();
        assert!(verbs.contains(Verb::List));
        assert!(!verbs.contains(Verb::Delete));
        assert_eq!(verbs.iter().collect::<Vec<_>>(), [Verb::List, Verb::Watch]);
        assert_eq!(format!("{verbs:?}"), "{List, Watch}");
        assert_eq!(
            Verb::parse("deletecollection"),
            Some(Verb::DeleteCollection)
        );
        assert_eq!(Verb::parse("impersonate"), None);
    }

    #[test]
    fn core_resources_have_a_bare_api_version() {
        let pods = GroupVersionResource {
            group: "".into(),
            version: "v1".into(),
            resource: "pods".into(),
        };
        assert_eq!(pods.api_version(), "v1");
        assert_eq!(pods.to_string(), "pods.v1");
        assert_eq!(deployment().resource.api_version(), "apps/v1");
        assert_eq!(deployment().resource.to_string(), "deployments.v1.apps");
    }
}
