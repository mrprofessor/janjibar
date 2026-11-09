//! A virtualized gpui-kit table for any kind. Columns arrive with the rows.
//!
//! Typed specs and server-side tables both send a [`TableSnapshot`]: the
//! table draws them the same way. Neither sends a namespace column; when a
//! namespaced kind is listed across namespaces, the table adds one after the
//! first column, from [`TableRow::namespace`].

use std::sync::Arc;

use gpui_kit::component::table::{Column as KitColumn, TableDelegate, TableState};
use gpui_kit::{
    AnyElement, App, Context, Div, Entity, FontWeight, SharedString, Stateful, Styled, Window, div,
    prelude::*, px,
};
use model::resource::ObjectKey;
use model::{Column, ColumnType, Health, TableRow};

use crate::theme;
use crate::widgets::health_color;

/// How a column's cells are drawn, from its type and format hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Look {
    Name,
    Text,
    Muted,
    Status,
    Number,
}

impl Look {
    fn of(col: &Column) -> Self {
        match (&*col.format, col.ty) {
            ("name", _) => Self::Name,
            ("status", _) => Self::Status,
            ("muted", _) => Self::Muted,
            (_, ColumnType::Integer | ColumnType::Number | ColumnType::Date) => Self::Number,
            _ => Self::Text,
        }
    }

    fn right_aligned(self) -> bool {
        self == Self::Number
    }

    fn width(self, col: &Column) -> f32 {
        match self {
            Self::Name => 320.0,
            Self::Status => 160.0,
            Self::Number if col.ty == ColumnType::Date => 72.0,
            Self::Number => (col.name.len() as f32 * 8.0 + 24.0).max(64.0),
            Self::Muted => 180.0,
            Self::Text => 150.0,
        }
    }
}

/// Table delegate holding the current snapshot.
#[derive(Debug, Default)]
pub struct ResourceTable {
    columns: Arc<[Column]>,
    rows: Arc<[TableRow]>,
    /// Draw a namespace column after the first one.
    show_namespace: bool,
}

/// What one displayed column shows.
enum Slot<'a> {
    Namespace,
    Cell(usize, &'a Column),
}

impl ResourceTable {
    /// The rows on screen.
    pub fn rows(&self) -> &[TableRow] {
        &self.rows
    }

    /// The key of the row at `ix`.
    pub fn key_at(&self, ix: usize) -> Option<ObjectKey> {
        self.rows.get(ix).map(key)
    }

    fn index_of(&self, wanted: &ObjectKey) -> Option<usize> {
        self.rows.iter().position(|r| key(r) == *wanted)
    }

    fn slot(&self, col_ix: usize) -> Option<Slot<'_>> {
        match (self.show_namespace, col_ix) {
            (true, 1) => Some(Slot::Namespace),
            (true, ix) if ix > 1 => self.columns.get(ix - 1).map(|c| Slot::Cell(ix - 1, c)),
            (_, ix) => self.columns.get(ix).map(|c| Slot::Cell(ix, c)),
        }
    }
}

fn key(row: &TableRow) -> ObjectKey {
    ObjectKey {
        namespace: row.namespace.clone(),
        name: Arc::clone(&row.name),
    }
}

/// Swaps in a new snapshot, keeping the selected object selected.
pub fn set_rows(
    table: &Entity<TableState<ResourceTable>>,
    columns: Arc<[Column]>,
    rows: Arc<[TableRow]>,
    show_namespace: bool,
    cx: &mut App,
) {
    table.update(cx, |state, cx| {
        let old_ix = state.selected_row();
        let selected = old_ix.and_then(|ix| state.delegate().key_at(ix));
        let delegate = state.delegate();
        let layout_changed =
            delegate.columns != columns || delegate.show_namespace != show_namespace;
        let delegate = state.delegate_mut();
        delegate.columns = columns;
        delegate.rows = rows;
        delegate.show_namespace = show_namespace;
        if layout_changed {
            state.refresh(cx);
        }
        if let Some(key) = selected {
            match state.delegate().index_of(&key) {
                Some(ix) if Some(ix) != old_ix => state.set_selected_row(ix, cx),
                Some(_) => {}
                None => state.clear_selection(cx),
            }
        }
        cx.notify();
    });
}

/// Empties the table, for a new kind or context.
pub fn clear(table: &Entity<TableState<ResourceTable>>, cx: &mut App) {
    table.update(cx, |state, cx| {
        state.clear_selection(cx);
        let delegate = state.delegate_mut();
        delegate.rows = Arc::default();
        cx.notify();
    });
}

impl TableDelegate for ResourceTable {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len() + usize::from(self.show_namespace && !self.columns.is_empty())
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> KitColumn {
        let (title, width, right) = match self.slot(col_ix) {
            Some(Slot::Namespace) => (SharedString::from("Namespace"), 150.0, false),
            Some(Slot::Cell(_, col)) => {
                let look = Look::of(col);
                (
                    SharedString::from(Arc::clone(&col.name)),
                    look.width(col),
                    look.right_aligned(),
                )
            }
            None => return KitColumn::new("missing", ""),
        };
        let column = KitColumn::new(title.clone(), title)
            .width(px(width))
            .min_width(px(48.0))
            .resizable(true);
        if right { column.text_right() } else { column }
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let (title, right) = match self.slot(col_ix) {
            Some(Slot::Namespace) => (SharedString::from("Namespace"), false),
            Some(Slot::Cell(_, c)) => (
                SharedString::from(Arc::clone(&c.name)),
                Look::of(c).right_aligned(),
            ),
            None => (SharedString::default(), false),
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .when(right, Styled::justify_end)
            .text_size(px(theme::TEXT_LABEL))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme::DIM)
            .child(title)
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let tr = div().id(("row", row_ix)).rounded(px(theme::ROW_RADIUS));
        match self.rows.get(row_ix) {
            // View tests find rows by key. A no-op outside test builds.
            Some(row) => tr.debug_selector(|| format!("row-{}", key(row))),
            None => tr,
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(row) = self.rows.get(row_ix) else {
            return div().into_any_element();
        };
        match self.slot(col_ix) {
            Some(Slot::Namespace) => cell(
                Look::Muted,
                row.namespace
                    .clone()
                    .map(SharedString::from)
                    .unwrap_or_default(),
                row.health,
            ),
            Some(Slot::Cell(ix, col)) => {
                let text = row
                    .cells
                    .get(ix)
                    .map(|c| SharedString::from(Arc::clone(c)))
                    .unwrap_or_default();
                cell(Look::of(col), text, row.health)
            }
            None => div().into_any_element(),
        }
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _: &App) -> Option<SharedString> {
        let row = self.rows.get(row_ix)?;
        match self.slot(col_ix)? {
            Slot::Namespace => row.namespace.clone().map(SharedString::from),
            Slot::Cell(ix, _) => Some(SharedString::from(Arc::clone(row.cells.get(ix)?))),
        }
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, cx: &App) -> String {
        self.cell_label(row_ix, col_ix, cx)
            .map(|s| s.to_string())
            .unwrap_or_default()
    }
}

fn cell(look: Look, text: SharedString, health: Health) -> AnyElement {
    let base = div()
        .w_full()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis();
    let mono = |el: Div| {
        el.font_family(theme::MONO_FONT)
            .text_size(px(theme::TEXT_MONO))
    };
    match look {
        Look::Name => mono(base)
            .text_color(theme::TEXT)
            .child(text)
            .into_any_element(),
        Look::Text => base
            .text_color(theme::TERM_TEXT)
            .child(text)
            .into_any_element(),
        Look::Muted => base.text_color(theme::MUTED).child(text).into_any_element(),
        Look::Number => mono(base)
            .flex()
            .justify_end()
            .text_color(theme::TERM_TEXT)
            .child(text)
            .into_any_element(),
        Look::Status => {
            let color = health_color(health);
            div()
                .flex()
                .items_center()
                .gap(px(7.0))
                .overflow_hidden()
                .child(div().flex_none().size(px(6.0)).rounded_full().bg(color))
                .child(
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(match health {
                            Health::Good => theme::TERM_TEXT,
                            Health::Done | Health::Unknown => theme::MUTED,
                            Health::Pending | Health::Bad => color,
                        })
                        .child(text),
                )
                .into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ns: &str, name: &str) -> TableRow {
        TableRow {
            name: name.into(),
            namespace: Some(ns.into()),
            uid: "".into(),
            created: None,
            cells: Arc::from([Arc::from(name), Arc::from("Running")]),
            details: Arc::from([]),
            health: Health::Good,
        }
    }

    fn column(name: &str, ty: ColumnType, format: &str) -> Column {
        Column {
            name: name.into(),
            ty,
            format: format.into(),
            priority: 0,
            description: "".into(),
        }
    }

    #[test]
    fn finds_rows_by_key() {
        let table = ResourceTable {
            columns: Arc::default(),
            rows: vec![row("a", "x"), row("a", "y"), row("b", "a")].into(),
            show_namespace: false,
        };
        assert_eq!(table.index_of(&ObjectKey::namespaced("a", "y")), Some(1));
        assert_eq!(table.index_of(&ObjectKey::namespaced("c", "a")), None);
        assert_eq!(
            table.key_at(2).map(|k| k.to_string()).as_deref(),
            Some("b/a")
        );
    }

    #[test]
    fn namespace_column_goes_after_the_first() {
        let table = ResourceTable {
            columns: vec![
                column("Name", ColumnType::String, "name"),
                column("Status", ColumnType::String, "status"),
            ]
            .into(),
            rows: vec![row("shop", "web-0")].into(),
            show_namespace: true,
        };
        assert!(matches!(table.slot(0), Some(Slot::Cell(0, _))));
        assert!(matches!(table.slot(1), Some(Slot::Namespace)));
        assert!(matches!(table.slot(2), Some(Slot::Cell(1, _))));
        assert!(table.slot(3).is_none());
    }

    #[test]
    fn looks_follow_type_and_format() {
        assert_eq!(
            Look::of(&column("Name", ColumnType::String, "name")),
            Look::Name
        );
        assert_eq!(
            Look::of(&column("Status", ColumnType::String, "status")),
            Look::Status
        );
        assert_eq!(
            Look::of(&column("Restarts", ColumnType::Integer, "")),
            Look::Number
        );
        assert_eq!(Look::of(&column("Age", ColumnType::Date, "")), Look::Number);
        assert_eq!(
            Look::of(&column("Node", ColumnType::String, "muted")),
            Look::Muted
        );
        assert_eq!(
            Look::of(&column("Image", ColumnType::String, "")),
            Look::Text
        );
    }
}
