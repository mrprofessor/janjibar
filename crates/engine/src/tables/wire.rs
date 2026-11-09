//! The `meta.k8s.io/v1 Table` wire format, for lists and watch events.
//!
//! The server sends a Table when asked with [`TABLE_ACCEPT`]. Servers that
//! cannot (some aggregated API servers) fall back to plain JSON; those
//! objects become rows with just a name and an age.

use std::sync::Arc;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use model::{Column, ColumnType};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use super::StoredRow;

/// Asks for a Table, and accepts plain JSON from servers that cannot make one.
pub(super) const TABLE_ACCEPT: &str =
    "application/json;as=Table;v=v1;g=meta.k8s.io,application/json";

/// What kubectl prints for a missing value, such as an unset printer column.
const NONE_CELL: &str = "<none>";

/// The description the API server gives creation-time columns.
const CREATION_DESCRIPTION: &str = "CreationTimestamp";

/// One page of a list, or the object of one watch event.
#[derive(Debug, Default)]
pub(super) struct Page {
    /// Column definitions. Empty when the server left them out, which it does
    /// for every watch event after the first.
    pub columns: Vec<Column>,
    /// The rows, one per object.
    pub rows: Vec<StoredRow>,
    /// The resource version the page or event reflects.
    pub resource_version: Option<String>,
    /// The token for the next page, if there is one.
    pub continue_token: Option<String>,
}

/// One watch event.
#[derive(Debug)]
pub(super) enum Change {
    /// Objects were added or modified.
    Applied(Page),
    /// Objects were deleted.
    Deleted(Page),
    /// Nothing changed; the watch is now at this resource version.
    Bookmark(Option<String>),
    /// The server ended the watch with a `Status`, like `410 Gone`.
    Error {
        /// The HTTP status code.
        code: u16,
        /// The server's message.
        message: String,
    },
}

/// Parses a list response: a Table, or a plain list as a fallback.
pub(super) fn parse_page(text: &str) -> serde_json::Result<Page> {
    serde_json::from_str::<WireObject>(text).map(WireObject::into_page)
}

/// Parses one line of a watch stream.
pub(super) fn parse_change(line: &str) -> serde_json::Result<Change> {
    let event: WireEvent = serde_json::from_str(line)?;
    Ok(match event.ty.as_str() {
        "ADDED" | "MODIFIED" => Change::Applied(event.object.into_page()),
        "DELETED" => Change::Deleted(event.object.into_page()),
        // A bookmark carries a placeholder row with no name; ignore it.
        "BOOKMARK" => Change::Bookmark(event.object.metadata.resource_version),
        _ => Change::Error {
            code: event.object.code.unwrap_or(500),
            message: if event.object.message.is_empty() {
                format!("watch event {}", event.ty)
            } else {
                event.object.message
            },
        },
    })
}

/// Columns for objects that did not come as a Table.
pub(super) fn fallback_columns() -> Vec<Column> {
    vec![
        Column {
            name: "Name".into(),
            ty: ColumnType::String,
            format: "name".into(),
            priority: 0,
            description: "".into(),
        },
        Column {
            name: "Age".into(),
            ty: ColumnType::Date,
            format: "".into(),
            priority: 0,
            description: CREATION_DESCRIPTION.into(),
        },
    ]
}

/// True for the column that shows the object's age, which the engine keeps
/// current from the creation time instead of the server's frozen text.
pub(super) fn is_creation_age(column: &Column) -> bool {
    column.name.eq_ignore_ascii_case("age")
        && (column.ty == ColumnType::Date || column.description.starts_with(CREATION_DESCRIPTION))
}

#[derive(Deserialize)]
struct WireEvent {
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    object: WireObject,
}

/// Any object a list or watch can return: a Table, a plain list, a plain
/// object, or a `Status`. Fields that do not apply stay empty.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct WireObject {
    #[serde(deserialize_with = "null_as_default")]
    kind: String,
    metadata: WireMeta,
    /// The server sends `null` here in watch events after the first.
    #[serde(deserialize_with = "null_as_default")]
    column_definitions: Vec<WireColumn>,
    #[serde(deserialize_with = "null_as_default")]
    rows: Vec<WireRow>,
    #[serde(deserialize_with = "null_as_default")]
    items: Vec<WireItem>,
    code: Option<u16>,
    #[serde(deserialize_with = "null_as_default")]
    message: String,
}

/// Fields of `ObjectMeta` and `ListMeta` that the table needs.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct WireMeta {
    name: Option<String>,
    namespace: Option<String>,
    uid: Option<String>,
    creation_timestamp: Option<Time>,
    resource_version: Option<String>,
    #[serde(rename = "continue")]
    continue_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireColumn {
    name: String,
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    ty: String,
    #[serde(default, deserialize_with = "null_as_default")]
    format: String,
    #[serde(default, deserialize_with = "null_as_default")]
    description: String,
    #[serde(default, deserialize_with = "null_as_default")]
    priority: i32,
}

/// Reads `null` as the default value. Go servers send `null` for empty
/// slices and strings that `omitempty` does not drop.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
struct WireRow {
    #[serde(default, deserialize_with = "null_as_default")]
    cells: Vec<Value>,
    /// The object's `PartialObjectMetadata`, sent with `includeObject=Metadata`.
    #[serde(default)]
    object: Option<WireItem>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WireItem {
    metadata: WireMeta,
}

impl WireObject {
    fn into_page(self) -> Page {
        let resource_version = self.metadata.resource_version.clone();
        let continue_token = self.metadata.continue_token.clone();
        let (columns, rows) = if self.kind == "Table" {
            let columns = self.column_definitions.into_iter().map(column).collect();
            let rows = self
                .rows
                .into_iter()
                .filter_map(|row| {
                    let meta = row.object?.metadata;
                    stored_row(meta, row.cells.iter().map(display).collect())
                })
                .collect();
            (columns, rows)
        } else if self.kind.ends_with("List") {
            let rows = self
                .items
                .into_iter()
                .filter_map(|item| fallback_row(item.metadata))
                .collect();
            (fallback_columns(), rows)
        } else {
            let rows = fallback_row(self.metadata).into_iter().collect();
            (fallback_columns(), rows)
        };
        Page {
            columns,
            rows,
            resource_version,
            continue_token,
        }
    }
}

fn column(c: WireColumn) -> Column {
    Column {
        name: c.name.into(),
        ty: ColumnType::parse(&c.ty),
        format: c.format.into(),
        priority: c.priority,
        description: c.description.into(),
    }
}

/// A row for an object that did not come as a Table: its name, and an age
/// cell the projection fills in.
fn fallback_row(meta: WireMeta) -> Option<StoredRow> {
    let name: Arc<str> = meta.name.as_deref()?.into();
    stored_row(meta, vec![name, "".into()])
}

/// Builds a row. Rows without a name cannot be keyed and are dropped.
fn stored_row(meta: WireMeta, cells: Vec<Arc<str>>) -> Option<StoredRow> {
    let name = meta.name.filter(|n| !n.is_empty())?;
    Some(StoredRow {
        namespace: meta.namespace.filter(|n| !n.is_empty()).map(Into::into),
        name: name.into(),
        uid: meta.uid.unwrap_or_default().into(),
        created: meta.creation_timestamp.map(|t| t.0),
        cells,
    })
}

/// Formats a cell the way kubectl prints it.
fn display(cell: &Value) -> Arc<str> {
    match cell {
        Value::String(s) => s.as_str().into(),
        Value::Null => NONE_CELL.into(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Number(n) => n.to_string().into(),
        other => other.to_string().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(page: &Page) -> Vec<&str> {
        page.columns.iter().map(|c| &*c.name).collect()
    }

    fn cells(row: &StoredRow) -> Vec<&str> {
        row.cells.iter().map(AsRef::as_ref).collect()
    }

    #[test]
    fn parses_a_recorded_pod_table() {
        let page = parse_page(include_str!("../../tests/fixtures/tables/pods.json")).unwrap();
        assert_eq!(
            names(&page),
            [
                "Name",
                "Ready",
                "Status",
                "Restarts",
                "Age",
                "IP",
                "Node",
                "Nominated Node",
                "Readiness Gates"
            ]
        );
        let priorities: Vec<i32> = page.columns.iter().map(|c| c.priority).collect();
        assert_eq!(priorities, [0, 0, 0, 0, 0, 1, 1, 1, 1]);
        assert_eq!(&*page.columns[0].format, "name");
        assert!(
            page.columns[0]
                .description
                .starts_with("Name must be unique")
        );
        assert_eq!(page.resource_version.as_deref(), Some("1415"));
        assert_eq!(page.continue_token, None);

        assert_eq!(page.rows.len(), 4);
        let crashloop = &page.rows[0];
        assert_eq!(&*crashloop.name, "crashloop-54594bf7cc-tb6vb");
        assert_eq!(crashloop.namespace.as_deref(), Some("faults"));
        assert_eq!(&*crashloop.uid, "acecc5e1-68d3-43dc-9b1f-549831f338f5");
        assert_eq!(
            crashloop.created,
            Some("2026-11-09T00:57:30Z".parse().unwrap())
        );
        assert_eq!(
            cells(crashloop),
            [
                "crashloop-54594bf7cc-tb6vb",
                "0/1",
                "CrashLoopBackOff",
                "3 (47s ago)",
                "2m17s",
                "10.244.1.6",
                "platform-worker",
                "<none>",
                "<none>"
            ]
        );
        let pending = &page.rows[3];
        assert_eq!(cells(pending)[2], "Pending");
        assert_eq!(cells(pending)[5], "<none>");
    }

    #[test]
    fn parses_a_custom_resource_table_with_printer_columns() {
        let page = parse_page(include_str!("../../tests/fixtures/tables/crontabs.json")).unwrap();
        assert_eq!(names(&page), ["Name", "Spec", "Replicas", "Image", "Age"]);
        let types: Vec<ColumnType> = page.columns.iter().map(|c| c.ty).collect();
        assert_eq!(
            types,
            [
                ColumnType::String,
                ColumnType::String,
                ColumnType::Integer,
                ColumnType::String,
                ColumnType::Date
            ]
        );
        assert_eq!(page.columns[3].priority, 1);
        assert_eq!(
            &*page.columns[1].description,
            "The cron spec defining the interval a CronJob is run"
        );
        assert_eq!(
            cells(&page.rows[0]),
            ["nightly", "0 2 * * *", "1", "busybox:1.36", "5d"]
        );
        // Unset printer columns come as null and print like kubectl.
        assert_eq!(
            cells(&page.rows[1]),
            ["every-minute", "* * * * */1", "<none>", "<none>", "2m"]
        );
    }

    #[test]
    fn the_age_column_is_found_for_built_in_and_custom_kinds() {
        let pods = parse_page(include_str!("../../tests/fixtures/tables/pods.json")).unwrap();
        let ages: Vec<&str> = pods
            .columns
            .iter()
            .filter(|c| is_creation_age(c))
            .map(|c| &*c.name)
            .collect();
        assert_eq!(ages, ["Age"]);
        let crontabs =
            parse_page(include_str!("../../tests/fixtures/tables/crontabs.json")).unwrap();
        assert!(is_creation_age(&crontabs.columns[4]));
        assert!(!is_creation_age(&crontabs.columns[1]));
    }

    #[test]
    fn parses_a_recorded_watch_stream() {
        let changes: Vec<Change> = include_str!("../../tests/fixtures/tables/pods-watch.jsonl")
            .lines()
            .map(|line| parse_change(line).unwrap())
            .collect();
        assert_eq!(changes.len(), 8);

        // Only the first event carries column definitions.
        let Change::Applied(first) = &changes[0] else {
            panic!("expected MODIFIED, got {:?}", changes[0]);
        };
        assert_eq!(first.columns.len(), 9);
        assert_eq!(first.resource_version.as_deref(), Some("1421"));
        assert_eq!(cells(&first.rows[0])[2], "ErrImagePull");

        let Change::Applied(later) = &changes[2] else {
            panic!("expected MODIFIED, got {:?}", changes[2]);
        };
        assert_eq!(later.columns.len(), 0);
        assert_eq!(&*later.rows[0].name, "crashloop-54594bf7cc-tb6vb");
        assert_eq!(cells(&later.rows[0])[1..3], ["1/1", "Running"]);

        // Bookmarks come as a Table with a placeholder row, which is ignored.
        assert!(matches!(
            &changes[6],
            Change::Bookmark(Some(rv)) if rv == "1561"
        ));
    }

    #[test]
    fn a_gone_watch_reports_its_status() {
        let line = r#"{"type":"ERROR","object":{"kind":"Status","apiVersion":"v1","metadata":{},"status":"Failure","message":"too old resource version: 1 (1415)","reason":"Expired","code":410}}"#;
        match parse_change(line).unwrap() {
            Change::Error { code, message } => {
                assert_eq!(code, 410);
                assert_eq!(message, "too old resource version: 1 (1415)");
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn deletions_carry_the_deleted_row() {
        let line = r#"{"type":"DELETED","object":{"kind":"Table","apiVersion":"meta.k8s.io/v1","metadata":{"resourceVersion":"9"},"columnDefinitions":[],"rows":[{"cells":["web-0","1/1","Running","0","3d"],"object":{"kind":"PartialObjectMetadata","apiVersion":"meta.k8s.io/v1","metadata":{"name":"web-0","namespace":"shop","uid":"u1","resourceVersion":"9"}}}]}}"#;
        let Change::Deleted(page) = parse_change(line).unwrap() else {
            panic!("expected a deletion");
        };
        assert_eq!(&*page.rows[0].name, "web-0");
        assert_eq!(page.rows[0].namespace.as_deref(), Some("shop"));
    }

    #[test]
    fn plain_lists_fall_back_to_name_and_age() {
        let text = r#"{"kind":"PodMetricsList","apiVersion":"metrics.k8s.io/v1beta1","metadata":{"resourceVersion":"77"},"items":[{"metadata":{"name":"web-0","namespace":"shop","creationTimestamp":"2026-11-09T11:00:00Z"},"containers":[]}]}"#;
        let page = parse_page(text).unwrap();
        assert_eq!(names(&page), ["Name", "Age"]);
        assert!(is_creation_age(&page.columns[1]));
        assert_eq!(page.resource_version.as_deref(), Some("77"));
        assert_eq!(&*page.rows[0].name, "web-0");
        assert_eq!(cells(&page.rows[0])[0], "web-0");
    }

    #[test]
    fn cells_print_like_kubectl() {
        let cells: Vec<Arc<str>> = serde_json::json!(["a", 3, 2.5, true, null, ["x", "y"]])
            .as_array()
            .unwrap()
            .iter()
            .map(display)
            .collect();
        let cells: Vec<&str> = cells.iter().map(AsRef::as_ref).collect();
        assert_eq!(cells, ["a", "3", "2.5", "true", "<none>", r#"["x","y"]"#]);
    }
}
