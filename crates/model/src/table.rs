//! Server-side tables: rows for any kind, with columns chosen by the server.
//!
//! The API server can answer a list or watch in the `meta.k8s.io/v1 Table`
//! format, the same one `kubectl get` prints. The engine parses it into these
//! types, so a custom resource gets its printer columns without code per kind.

use std::sync::Arc;
use std::time::SystemTime;

use crate::{ConnectionState, ContextName, GroupVersionResource};

/// The type of a column's values, as the server declares it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColumnType {
    /// Text.
    String,
    /// Whole numbers, like restarts or replicas.
    Integer,
    /// Any number.
    Number,
    /// `true` or `false`.
    Boolean,
    /// A time, shown as an age like `3d`.
    Date,
    /// A type this app does not know.
    Unknown,
}

impl ColumnType {
    /// Parses the `type` of a column definition.
    pub fn parse(ty: &str) -> Self {
        match ty {
            "string" => Self::String,
            "integer" => Self::Integer,
            "number" => Self::Number,
            "boolean" => Self::Boolean,
            "date" => Self::Date,
            _ => Self::Unknown,
        }
    }
}

/// One column, as the server defines it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    /// The header, like `Ready` or `Age`.
    pub name: Arc<str>,
    /// The type of the values.
    pub ty: ColumnType,
    /// A display hint, like `name`. Often empty.
    pub format: Arc<str>,
    /// 0 for the default view. Higher numbers are extra detail, like the
    /// columns `kubectl get -o wide` adds.
    pub priority: i32,
    /// What the column means, for a tooltip.
    pub description: Arc<str>,
}

/// One object in a table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableRow {
    /// The object's name.
    pub name: Arc<str>,
    /// The object's namespace. `None` for cluster-scoped kinds.
    pub namespace: Option<Arc<str>>,
    /// The object's uid.
    pub uid: Arc<str>,
    /// When the object was created, if the server said.
    pub created: Option<SystemTime>,
    /// Display values for [`TableSnapshot::columns`], in the same order.
    pub cells: Arc<[Arc<str>]>,
    /// Display values for [`TableSnapshot::detail_columns`], in the same order.
    pub details: Arc<[Arc<str>]>,
}

/// All rows of one kind in one context at one moment.
///
/// Rows are sorted by namespace, then name. The server does not send a
/// namespace column; when listing across namespaces, show
/// [`TableRow::namespace`] as one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableSnapshot {
    /// The context the rows belong to.
    pub context: ContextName,
    /// The kind being listed.
    pub resource: GroupVersionResource,
    /// The namespace listed, or `None` for all namespaces and cluster-scoped kinds.
    pub namespace: Option<Arc<str>>,
    /// Where the watch stands. Rows stay while it retries.
    pub state: ConnectionState,
    /// Priority-0 columns: the list view.
    pub columns: Arc<[Column]>,
    /// The other columns, for the details view.
    pub detail_columns: Arc<[Column]>,
    /// The rows.
    pub rows: Arc<[TableRow]>,
}
