//! The Helm releases list: a virtualized gpui-kit table fed by snapshots.

use std::sync::Arc;

use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::{
    AnyElement, App, Context, Div, Entity, FontWeight, Rgba, SharedString, Stateful, Styled,
    Window, div, prelude::*, px,
};
use model::{HelmChartInfo, HelmReleaseKey, HelmRow, HelmStatus};

use crate::theme;

/// The table's columns, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Col {
    Name,
    Namespace,
    Chart,
    AppVersion,
    Revision,
    Status,
    Updated,
}

const COLUMNS: [Col; 7] = [
    Col::Name,
    Col::Namespace,
    Col::Chart,
    Col::AppVersion,
    Col::Revision,
    Col::Status,
    Col::Updated,
];

impl Col {
    fn key(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Namespace => "namespace",
            Self::Chart => "chart",
            Self::AppVersion => "app-version",
            Self::Revision => "revision",
            Self::Status => "status",
            Self::Updated => "updated",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Namespace => "Namespace",
            Self::Chart => "Chart",
            Self::AppVersion => "App version",
            Self::Revision => "Revision",
            Self::Status => "Status",
            Self::Updated => "Updated",
        }
    }

    fn width(self) -> f32 {
        match self {
            Self::Name | Self::Chart => 220.0,
            Self::Namespace => 140.0,
            Self::AppVersion => 110.0,
            Self::Revision => 76.0,
            Self::Status => 150.0,
            Self::Updated => 80.0,
        }
    }

    fn right_aligned(self) -> bool {
        matches!(self, Self::Revision | Self::Updated)
    }

    fn text(self, row: &HelmRow) -> SharedString {
        match self {
            Self::Name => SharedString::from(Arc::clone(&row.key.name)),
            Self::Namespace => SharedString::from(Arc::clone(&row.key.namespace)),
            Self::Chart => match &row.chart {
                HelmChartInfo::Loading => "…".into(),
                HelmChartInfo::Known(chart) => chart.to_string().into(),
                HelmChartInfo::Unreadable(_) => "unreadable".into(),
            },
            Self::AppVersion => match &row.chart {
                HelmChartInfo::Known(chart) if !chart.app_version.is_empty() => {
                    SharedString::from(Arc::clone(&chart.app_version))
                }
                HelmChartInfo::Known(_) => "-".into(),
                HelmChartInfo::Loading | HelmChartInfo::Unreadable(_) => "".into(),
            },
            Self::Revision => row.revision.to_string().into(),
            Self::Status => row.status.as_str().into(),
            Self::Updated => SharedString::from(Arc::clone(&row.age)),
        }
    }
}

/// Colour of a release status.
pub fn status_color(status: HelmStatus) -> Rgba {
    match status {
        HelmStatus::Deployed => theme::GREEN,
        HelmStatus::Failed => theme::RED,
        HelmStatus::PendingInstall
        | HelmStatus::PendingUpgrade
        | HelmStatus::PendingRollback
        | HelmStatus::Uninstalling => theme::AMBER,
        HelmStatus::Superseded | HelmStatus::Uninstalled | HelmStatus::Unknown => theme::DIM,
    }
}

/// Table delegate holding the current snapshot's rows.
#[derive(Debug, Default)]
pub struct HelmTable {
    rows: Arc<[HelmRow]>,
}

impl HelmTable {
    /// The rows on screen.
    pub fn rows(&self) -> &[HelmRow] {
        &self.rows
    }

    /// Index of a release in the (sorted) rows.
    pub fn index_of(&self, key: &HelmReleaseKey) -> Option<usize> {
        self.rows.binary_search_by(|r| r.key.cmp(key)).ok()
    }
}

/// Swaps in new rows, keeping the selected release selected by key.
pub fn set_rows(table: &Entity<TableState<HelmTable>>, rows: Arc<[HelmRow]>, cx: &mut App) {
    table.update(cx, |state, cx| {
        let selected = state
            .selected_row()
            .and_then(|ix| state.delegate().rows.get(ix))
            .map(|r| r.key.clone());
        let old_ix = state.selected_row();
        state.delegate_mut().rows = rows;
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

impl TableDelegate for HelmTable {
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
        let tr = div().id(("helm", row_ix)).rounded(px(theme::ROW_RADIUS));
        match self.rows.get(row_ix) {
            // View tests find rows by name. A no-op outside test builds.
            Some(row) => {
                tr.debug_selector(|| format!("helm-{}/{}", row.key.namespace, row.key.name))
            }
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

fn render_cell(col: Col, row: &HelmRow) -> AnyElement {
    let text = col.text(row);
    let base = div()
        .w_full()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis();
    let mono = |el: Div| {
        el.font_family(theme::MONO_FONT)
            .text_size(px(theme::TEXT_MONO))
    };
    match col {
        Col::Name => mono(base)
            .text_color(theme::TEXT)
            .child(text)
            .into_any_element(),
        Col::Namespace => base.text_color(theme::MUTED).child(text).into_any_element(),
        Col::Chart => {
            let color = match row.chart {
                HelmChartInfo::Known(_) => theme::TERM_TEXT,
                HelmChartInfo::Loading => theme::DIM,
                HelmChartInfo::Unreadable(_) => theme::AMBER,
            };
            mono(base).text_color(color).child(text).into_any_element()
        }
        Col::AppVersion => mono(base)
            .text_color(theme::MUTED)
            .child(text)
            .into_any_element(),
        Col::Status => {
            let color = status_color(row.status);
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
                        .text_color(match row.status {
                            HelmStatus::Deployed => theme::TERM_TEXT,
                            HelmStatus::Superseded
                            | HelmStatus::Uninstalled
                            | HelmStatus::Unknown => theme::MUTED,
                            _ => color,
                        })
                        .child(text),
                )
                .into_any_element()
        }
        Col::Revision | Col::Updated => mono(base)
            .flex()
            .justify_end()
            .text_color(theme::TERM_TEXT)
            .child(text)
            .into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use model::{HelmChart, HelmHistoryEntry, HelmStorage};

    use super::*;

    fn row(ns: &str, name: &str, chart: HelmChartInfo) -> HelmRow {
        HelmRow {
            key: HelmReleaseKey {
                namespace: ns.into(),
                name: name.into(),
                storage: HelmStorage::Secret,
            },
            revision: 4,
            status: HelmStatus::Deployed,
            age: "3d".into(),
            chart,
            history: Arc::from([HelmHistoryEntry {
                revision: 4,
                status: HelmStatus::Deployed,
                age: "3d".into(),
            }]),
        }
    }

    fn texts(row: &HelmRow) -> Vec<String> {
        COLUMNS.iter().map(|c| c.text(row).to_string()).collect()
    }

    #[test]
    fn cell_text_matches_columns() {
        let chart = HelmChart {
            name: "web".into(),
            version: "1.4.0".into(),
            app_version: "2.31.0".into(),
        };
        assert_eq!(
            texts(&row("shop", "web", HelmChartInfo::Known(chart))),
            ["web", "shop", "web-1.4.0", "2.31.0", "4", "deployed", "3d"]
        );
    }

    #[test]
    fn chart_cells_show_loading_and_unreadable() {
        assert_eq!(
            texts(&row("a", "x", HelmChartInfo::Loading))[2..4],
            ["…", ""]
        );
        let bad = HelmChartInfo::Unreadable("broken".into());
        assert_eq!(texts(&row("a", "x", bad))[2..4], ["unreadable", ""]);
        let no_app = HelmChartInfo::Known(HelmChart {
            name: "x".into(),
            version: "1".into(),
            app_version: "".into(),
        });
        assert_eq!(texts(&row("a", "x", no_app))[3], "-");
    }

    #[test]
    fn finds_rows_by_key() {
        let table = HelmTable {
            rows: vec![
                row("a", "x", HelmChartInfo::Loading),
                row("a", "y", HelmChartInfo::Loading),
                row("b", "a", HelmChartInfo::Loading),
            ]
            .into(),
        };
        assert_eq!(table.index_of(&table.rows[1].key.clone()), Some(1));
        let missing = HelmReleaseKey {
            namespace: "c".into(),
            name: "a".into(),
            storage: HelmStorage::Secret,
        };
        assert_eq!(table.index_of(&missing), None);
    }
}
