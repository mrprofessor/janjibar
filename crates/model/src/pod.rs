//! Pod rows, already projected for display.

use std::fmt;
use std::sync::Arc;

use crate::ContextName;

/// Ready containers out of total, like kubectl's `READY` column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Ready {
    /// Containers reporting ready.
    pub ready: u32,
    /// Containers in the pod (including restartable init containers).
    pub total: u32,
}

impl fmt::Display for Ready {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.ready, self.total)
    }
}

/// How a pod's status should be coloured. Derived from the status text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Health {
    /// Running and ready.
    Good,
    /// Starting, pending, or terminating.
    Pending,
    /// Failing: crash loops, image pull errors, OOM kills, failed phase.
    Bad,
    /// Finished successfully.
    Done,
    /// Status not known.
    Unknown,
}

/// One row of the pod table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodRow {
    /// Namespace.
    pub namespace: Arc<str>,
    /// Pod name.
    pub name: Arc<str>,
    /// Ready containers out of total.
    pub ready: Ready,
    /// kubectl-style status: a phase, or a reason like `CrashLoopBackOff`.
    pub status: Arc<str>,
    /// Colour class for `status`.
    pub health: Health,
    /// Container restarts, summed.
    pub restarts: u32,
    /// kubectl-style age, like `3d4h`.
    pub age: Arc<str>,
    /// Node name, empty when not scheduled.
    pub node: Arc<str>,
}

/// All pod rows of one context at one moment, sorted by namespace and name.
#[derive(Clone, Debug)]
pub struct PodSnapshot {
    /// The context the rows belong to.
    pub context: ContextName,
    /// The rows.
    pub rows: Arc<[PodRow]>,
}
