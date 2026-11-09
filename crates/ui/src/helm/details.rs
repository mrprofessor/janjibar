//! The details pane: a summary of one revision, then its values, notes,
//! manifest or the release's history.

use std::sync::Arc;

use gpui_kit::component::table::TableState;
use gpui_kit::{
    AnyElement, Context, FontWeight, ListHorizontalSizingBehavior, SharedString, div, prelude::*,
    px, relative, uniform_list,
};
use model::{HelmChartInfo, HelmHistoryEntry, HelmRow};

use super::table::status_color;
use super::{Details, HELM_ICON, HelmView, Tab, TextLines, icon, pane, pane_header};
use crate::assets::Icon;
use crate::theme;

/// Share of the work area the details pane takes, within the bounds below.
const DETAILS_SHARE: f32 = 0.45;

/// Narrowest the details pane gets; below this the summary wraps badly.
const DETAILS_MIN_WIDTH: f32 = 300.0;

/// Widest the details pane gets, so the list keeps the room on big screens.
const DETAILS_MAX_WIDTH: f32 = 640.0;

/// Height of one line in the text tabs.
const LINE_HEIGHT: f32 = 19.0;

impl HelmView {
    pub(super) fn render_details(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.selected_row() else {
            return div().into_any_element();
        };
        let revision = self.shown_revision().unwrap_or(row.revision);
        pane()
            .w(relative(DETAILS_SHARE))
            .min_w(px(DETAILS_MIN_WIDTH))
            .max_w(px(DETAILS_MAX_WIDTH))
            .flex_none()
            .debug_selector(|| "helm-details".to_owned())
            .child(
                pane_header()
                    .child(icon(HELM_ICON, 13.0, theme::MUTED))
                    .child(
                        div()
                            .font_family(theme::MONO_FONT)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::TEXT)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(SharedString::from(Arc::clone(&row.key.name))),
                    )
                    .child(SharedString::from(Arc::clone(&row.key.namespace)))
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("helm-details-close")
                            .debug_selector(|| "helm-details-close".to_owned())
                            .size(px(20.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::HOVER))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.table.update(cx, TableState::clear_selection);
                            }))
                            .child(icon(Icon::Close, 13.0, theme::MUTED)),
                    ),
            )
            .child(self.render_summary(row, revision))
            .child(self.render_tabs(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_tab_body(row, revision, cx)),
            )
            .into_any_element()
    }

    fn render_summary(&self, row: &HelmRow, revision: u32) -> impl IntoElement {
        let entry = row.history.iter().find(|h| h.revision == revision);
        let loaded = match &self.details {
            Details::Loaded {
                details,
                release,
                revision: r,
                ..
            } if *release == row.key && *r == revision => Some(details),
            _ => None,
        };
        // The list knows the latest revision's chart; older ones come from
        // their decoded details.
        let (chart, app_version): (SharedString, SharedString) = match (loaded, &row.chart) {
            (Some(details), _) => (
                details.chart.to_string().into(),
                or_dash(&details.chart.app_version),
            ),
            (None, HelmChartInfo::Known(chart)) if revision == row.revision => {
                (chart.to_string().into(), or_dash(&chart.app_version))
            }
            _ => ("…".into(), "…".into()),
        };
        let revision_text = if revision == row.revision {
            format!("{revision}, latest of {}", row.history.len())
        } else {
            format!("{revision}, latest is {}", row.revision)
        };
        let status = entry.map_or(row.status, |e| e.status);
        let age = entry.map_or_else(|| Arc::clone(&row.age), |e| Arc::clone(&e.age));
        let description: SharedString = loaded
            .map(|d| SharedString::from(Arc::clone(&d.description)))
            .unwrap_or_default();

        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(5.0))
            .px(px(14.0))
            .py(px(10.0))
            .border_b_1()
            .border_color(theme::BORDER)
            .child(field(
                "Status",
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(dot(status))
                    .child(status.as_str()),
            ))
            .child(field("Revision", mono(revision_text)))
            .child(field("Chart", mono(chart)))
            .child(field("App version", mono(app_version)))
            .child(field("Updated", mono(format!("{age} ago"))))
            .when(!description.is_empty(), |el| {
                el.child(field(
                    "Description",
                    div().text_color(theme::MUTED).child(description),
                ))
            })
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut tabs = Vec::with_capacity(4);
        for (tab, label, id) in [
            (Tab::Values, "Values", "helm-tab-values"),
            (Tab::Notes, "Notes", "helm-tab-notes"),
            (Tab::Manifest, "Manifest", "helm-tab-manifest"),
            (Tab::History, "History", "helm-tab-history"),
        ] {
            let active = self.tab == tab;
            tabs.push(
                div()
                    .id(id)
                    .debug_selector(move || id.to_owned())
                    .h(px(24.0))
                    .px(px(9.0))
                    .flex()
                    .items_center()
                    .rounded(px(6.0))
                    .text_size(px(12.0))
                    .text_color(if active { theme::TEXT } else { theme::MUTED })
                    .when(active, |el| el.bg(theme::SELECTED))
                    .when(!active, |el| {
                        el.cursor_pointer().hover(|s| s.bg(theme::HOVER))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.show_tab(tab, cx)))
                    .child(label)
                    .into_any_element(),
            );
        }
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(3.0))
            .px(px(8.0))
            .py(px(5.0))
            .border_b_1()
            .border_color(theme::BORDER)
            .children(tabs)
    }

    fn render_tab_body(&self, row: &HelmRow, revision: u32, cx: &mut Context<Self>) -> AnyElement {
        if self.tab == Tab::History {
            return Self::render_history(row, revision, cx);
        }
        let (lines, none) = match &self.details {
            Details::Loaded {
                values,
                notes,
                manifest,
                ..
            } => match self.tab {
                Tab::Values => (values, "No user-supplied values"),
                Tab::Notes => (notes, "This chart has no notes"),
                Tab::Manifest => (manifest, "The manifest is empty"),
                Tab::History => return div().into_any_element(),
            },
            Details::Failed(_, _, message) => {
                return message_body(message.clone(), theme::RED);
            }
            Details::Loading(..) | Details::None => {
                return message_body(format!("Loading revision {revision}").into(), theme::DIM);
            }
        };
        if lines.lines.is_empty() {
            return message_body(none.into(), theme::DIM);
        }
        text_list(lines, &self.text_scroll)
    }

    fn render_history(row: &HelmRow, shown: u32, cx: &mut Context<Self>) -> AnyElement {
        let mut items = Vec::with_capacity(row.history.len());
        for entry in row.history.iter() {
            items.push(Self::render_history_entry(
                entry,
                entry.revision == shown,
                cx,
            ));
        }
        div()
            .id("helm-history")
            .size_full()
            .overflow_y_scroll()
            .p(px(6.0))
            .flex()
            .flex_col()
            .children(items)
            .into_any_element()
    }

    fn render_history_entry(
        entry: &HelmHistoryEntry,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let revision = entry.revision;
        div()
            .id(("helm-history", revision as usize))
            .debug_selector(move || format!("helm-history-{revision}"))
            .h(px(theme::ROW_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(8.0))
            .rounded(px(theme::ROW_RADIUS))
            .when(selected, |el| el.bg(theme::SELECTED))
            .when(!selected, |el| {
                el.cursor_pointer().hover(|s| s.bg(theme::HOVER))
            })
            .on_click(cx.listener(move |this, _, _, cx| this.pin(revision, cx)))
            .child(
                div()
                    .w(px(56.0))
                    .font_family(theme::MONO_FONT)
                    .text_size(px(theme::TEXT_MONO))
                    .text_color(theme::TEXT)
                    .child(format!("#{revision}")),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .text_color(theme::TERM_TEXT)
                    .child(dot(entry.status))
                    .child(entry.status.as_str()),
            )
            .child(
                div()
                    .font_family(theme::MONO_FONT)
                    .text_size(px(theme::TEXT_MONO))
                    .text_color(theme::MUTED)
                    .child(SharedString::from(Arc::clone(&entry.age))),
            )
            .into_any_element()
    }
}

/// A virtualized list of text lines: only the visible ones are laid out.
fn text_list(lines: &TextLines, scroll: &gpui_kit::UniformListScrollHandle) -> AnyElement {
    let shown = Arc::clone(&lines.lines);
    uniform_list("helm-text", shown.len(), move |range, _, _| {
        range
            .filter_map(|ix| {
                let line = shown.get(ix)?.clone();
                Some(
                    div()
                        .debug_selector(move || format!("helm-text-{ix}"))
                        .h(px(LINE_HEIGHT))
                        .px(px(14.0))
                        .whitespace_nowrap()
                        .child(line),
                )
            })
            .collect()
    })
    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
    .with_width_from_item(Some(lines.widest))
    .track_scroll(scroll)
    .size_full()
    .py(px(6.0))
    .font_family(theme::MONO_FONT)
    .text_size(px(theme::TEXT_MONO))
    .text_color(theme::TERM_TEXT)
    .into_any_element()
}

fn message_body(text: SharedString, color: gpui_kit::Rgba) -> AnyElement {
    div()
        .size_full()
        .p(px(14.0))
        .text_color(color)
        .child(text)
        .into_any_element()
}

fn field(label: &'static str, value: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .items_start()
        .gap(px(10.0))
        .child(
            div()
                .w(px(92.0))
                .flex_none()
                .text_size(px(theme::TEXT_LABEL))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::DIM)
                .child(label),
        )
        .child(div().flex_1().min_w_0().child(value))
}

fn mono(text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .font_family(theme::MONO_FONT)
        .text_size(px(theme::TEXT_MONO))
        .text_color(theme::TERM_TEXT)
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .child(text.into())
}

fn dot(status: model::HelmStatus) -> impl IntoElement {
    div()
        .flex_none()
        .size(px(6.0))
        .rounded_full()
        .bg(status_color(status))
}

fn or_dash(text: &Arc<str>) -> SharedString {
    if text.is_empty() {
        "-".into()
    } else {
        SharedString::from(Arc::clone(text))
    }
}
