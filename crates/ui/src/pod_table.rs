//! The pod list: a virtualized gpui-kit table fed by engine snapshots.

use std::sync::Arc;

use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::{
    AnyElement, App, Context, Div, Entity, FontWeight, SharedString, Stateful, Styled, Window, div,
    prelude::*, px,
};
use model::{Health, PodRow};

use crate::theme;

/// The table's columns, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Col {
    Name,
    Namespace,
    Ready,
    Status,
    Restarts,
    Age,
    Node,
}

const COLUMNS: [Col; 7] = [
    Col::Name,
    Col::Namespace,
    Col::Ready,
    Col::Status,
    Col::Restarts,
    Col::Age,
    Col::Node,
];

impl Col {
    fn key(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Namespace => "namespace",
            Self::Ready => "ready",
            Self::Status => "status",
            Self::Restarts => "restarts",
            Self::Age => "age",
            Self::Node => "node",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Namespace => "Namespace",
            Self::Ready => "Ready",
            Self::Status => "Status",
            Self::Restarts => "Restarts",
            Self::Age => "Age",
            Self::Node => "Node",
        }
    }

    fn width(self) -> f32 {
        match self {
            Self::Name => 340.0,
            Self::Namespace => 150.0,
            Self::Ready => 64.0,
            Self::Status => 170.0,
            Self::Restarts => 76.0,
            Self::Age => 70.0,
            Self::Node => 200.0,
        }
    }

    fn right_aligned(self) -> bool {
        matches!(self, Self::Restarts | Self::Age)
    }

    fn text(self, row: &PodRow) -> SharedString {
        match self {
            Self::Name => SharedString::from(Arc::clone(&row.name)),
            Self::Namespace => SharedString::from(Arc::clone(&row.namespace)),
            Self::Ready => row.ready.to_string().into(),
            Self::Status => SharedString::from(Arc::clone(&row.status)),
            Self::Restarts => row.restarts.to_string().into(),
            Self::Age => SharedString::from(Arc::clone(&row.age)),
            Self::Node => SharedString::from(Arc::clone(&row.node)),
        }
    }
}

/// Colour of a status dot and text.
pub fn health_color(health: Health) -> gpui_kit::Rgba {
    match health {
        Health::Good => theme::GREEN,
        Health::Pending => theme::AMBER,
        Health::Bad => theme::RED,
        Health::Done | Health::Unknown => theme::DIM,
    }
}

/// Table delegate holding the current snapshot's rows.
#[derive(Debug, Default)]
pub struct PodTable {
    rows: Arc<[PodRow]>,
}

impl PodTable {
    /// The rows on screen.
    pub fn rows(&self) -> &[PodRow] {
        &self.rows
    }

    /// Index of a pod in the (sorted) rows.
    fn index_of(&self, namespace: &str, name: &str) -> Option<usize> {
        self.rows
            .binary_search_by(|r| (&*r.namespace, &*r.name).cmp(&(namespace, name)))
            .ok()
    }
}

/// The selected pod's namespace and name.
pub fn selected_pod(
    table: &Entity<TableState<PodTable>>,
    cx: &App,
) -> Option<(Arc<str>, Arc<str>)> {
    let state = table.read(cx);
    let row = state.delegate().rows.get(state.selected_row()?)?;
    Some((Arc::clone(&row.namespace), Arc::clone(&row.name)))
}

/// Swaps in a new snapshot, keeping the selected pod selected by name.
pub fn set_rows(table: &Entity<TableState<PodTable>>, rows: Arc<[PodRow]>, cx: &mut App) {
    table.update(cx, |state, cx| {
        let selected = state
            .selected_row()
            .and_then(|ix| state.delegate().rows.get(ix))
            .map(|r| (Arc::clone(&r.namespace), Arc::clone(&r.name)));
        let old_ix = state.selected_row();
        state.delegate_mut().rows = rows;
        if let Some((ns, name)) = selected {
            match state.delegate().index_of(&ns, &name) {
                Some(ix) if Some(ix) != old_ix => state.set_selected_row(ix, cx),
                Some(_) => {}
                None => state.clear_selection(cx),
            }
        }
        cx.notify();
    });
}

impl TableDelegate for PodTable {
    fn columns_count(&self, _: &App) -> usize {
        COLUMNS.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let col = COLUMNS.get(col_ix).copied().unwrap_or(Col::Name);
        let column = Column::new(col.key(), col.title())
            .width(px(col.width()))
            .min_width(px(48.0))
            .resizable(true);
        if col.right_aligned() {
            column.text_right()
        } else {
            column
        }
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let col = COLUMNS.get(col_ix).copied().unwrap_or(Col::Name);
        div()
            .size_full()
            .flex()
            .items_center()
            .when(col.right_aligned(), Styled::justify_end)
            .text_size(px(theme::TEXT_LABEL))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme::DIM)
            .child(col.title())
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let tr = div().id(("pod", row_ix)).rounded(px(theme::ROW_RADIUS));
        match self.rows.get(row_ix) {
            // View tests find rows by name. A no-op outside test builds.
            Some(pod) => tr.debug_selector(|| format!("pod-{}/{}", pod.namespace, pod.name)),
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
        let (Some(row), Some(col)) = (self.rows.get(row_ix), COLUMNS.get(col_ix).copied()) else {
            return div().into_any_element();
        };
        render_cell(col, row)
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _: &App) -> Option<SharedString> {
        let row = self.rows.get(row_ix)?;
        Some(COLUMNS.get(col_ix)?.text(row))
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        self.rows
            .get(row_ix)
            .zip(COLUMNS.get(col_ix))
            .map(|(row, col)| col.text(row).to_string())
            .unwrap_or_default()
    }
}

fn render_cell(col: Col, row: &PodRow) -> AnyElement {
    let text = col.text(row);
    let base = div()
        .w_full()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis();
    match col {
        Col::Name => base
            .font_family(theme::MONO_FONT)
            .text_size(px(theme::TEXT_MONO))
            .text_color(theme::TEXT)
            .child(text)
            .into_any_element(),
        Col::Namespace | Col::Node => base.text_color(theme::MUTED).child(text).into_any_element(),
        Col::Ready => base
            .font_family(theme::MONO_FONT)
            .text_size(px(theme::TEXT_MONO))
            .text_color(
                if row.ready.ready < row.ready.total && row.health != Health::Done {
                    theme::AMBER
                } else {
                    theme::TERM_TEXT
                },
            )
            .child(text)
            .into_any_element(),
        Col::Status => {
            let color = health_color(row.health);
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
                        .text_color(match row.health {
                            Health::Good => theme::TERM_TEXT,
                            Health::Done | Health::Unknown => theme::MUTED,
                            Health::Pending | Health::Bad => color,
                        })
                        .child(text),
                )
                .into_any_element()
        }
        Col::Restarts => base
            .flex()
            .justify_end()
            .font_family(theme::MONO_FONT)
            .text_size(px(theme::TEXT_MONO))
            .text_color(if row.restarts > 0 && row.health == Health::Bad {
                theme::RED
            } else {
                theme::TERM_TEXT
            })
            .child(text)
            .into_any_element(),
        Col::Age => base
            .flex()
            .justify_end()
            .font_family(theme::MONO_FONT)
            .text_size(px(theme::TEXT_MONO))
            .text_color(theme::TERM_TEXT)
            .child(text)
            .into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::Ready;

    fn row(ns: &str, name: &str) -> PodRow {
        PodRow {
            namespace: ns.into(),
            name: name.into(),
            ready: Ready { ready: 1, total: 1 },
            status: "Running".into(),
            health: Health::Good,
            restarts: 0,
            age: "1m".into(),
            node: "n".into(),
        }
    }

    #[test]
    fn finds_rows_by_key() {
        let table = PodTable {
            rows: vec![row("a", "x"), row("a", "y"), row("b", "a")].into(),
        };
        assert_eq!(table.index_of("a", "y"), Some(1));
        assert_eq!(table.index_of("b", "a"), Some(2));
        assert_eq!(table.index_of("c", "a"), None);
    }

    #[test]
    fn cell_text_matches_columns() {
        let table = PodTable {
            rows: vec![row("shop", "web-0")].into(),
        };
        let texts: Vec<String> = (0..COLUMNS.len())
            .map(|c| COLUMNS[c].text(&table.rows[0]).to_string())
            .collect();
        assert_eq!(texts, ["web-0", "shop", "1/1", "Running", "0", "1m", "n"]);
    }
}
