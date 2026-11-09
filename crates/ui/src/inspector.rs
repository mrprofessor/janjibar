//! The inspector pane: follows the selected row.
//!
//! Order follows the design brief: owner chain, name, status, actions,
//! details, containers, kind-specific sections, labels.

use std::sync::Arc;

use gpui_kit::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};
use model::resource::{Action, ContainerInfo, Details, Field, InspectTarget, OwnerLink, Section};

use crate::assets::{Icon, kind_icon};
use crate::theme;
use crate::widgets::{chip, health_color, icon};
use crate::workspace::Workspace;

/// Inspector width in pixels.
pub(crate) const INSPECTOR_WIDTH: f32 = 380.0;

/// Width of the label column in field lists.
const LABEL_WIDTH: f32 = 112.0;

fn s(text: &Arc<str>) -> SharedString {
    SharedString::from(Arc::clone(text))
}

impl Workspace {
    pub(crate) fn render_inspector(
        &self,
        target: &InspectTarget,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = match &target.key.namespace {
            Some(ns) => format!("{} in {ns}", target.kind.singular()),
            None => target.kind.singular().to_owned(),
        };
        let header = div()
            .h(px(theme::PANE_HEADER_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .pl(px(11.0))
            .pr(px(6.0))
            .border_b_1()
            .border_color(theme::BORDER)
            .text_size(px(12.0))
            .text_color(theme::MUTED)
            .child(icon(kind_icon(target.kind), 13.0, theme::MUTED))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(title),
            )
            .child(
                div()
                    .id("inspector-close")
                    .debug_selector(|| "inspector-close".to_owned())
                    .size(px(22.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|el| el.bg(theme::HOVER))
                    .on_click(cx.listener(|this, _, _, cx| this.close_inspector(cx)))
                    .child(icon(Icon::Close, 13.0, theme::DIM)),
            );

        let body: AnyElement = match (&self.details, self.details_gone) {
            (_, true) => centered(format!(
                "This {} no longer exists.",
                target.kind.singular().to_lowercase()
            )),
            (None, false) => centered("Loading".to_owned()),
            (Some(details), false) => self.render_details(target, details, cx),
        };

        div()
            .debug_selector(|| "inspector".to_owned())
            .w(px(INSPECTOR_WIDTH))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::PANE)
            .border_1()
            .border_color(theme::BORDER)
            .rounded(px(theme::PANE_RADIUS))
            .overflow_hidden()
            .child(header)
            .child(body)
    }

    fn render_details(
        &self,
        target: &InspectTarget,
        d: &Details,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut body: Vec<AnyElement> = Vec::new();

        // Owner chain, outermost first, then this object's kind.
        let chain: Vec<OwnerLink> = if self.owners.is_empty() {
            d.owners.iter().take(1).cloned().collect()
        } else {
            self.owners.clone()
        };
        if !chain.is_empty() {
            let mut parts: Vec<AnyElement> = Vec::new();
            for link in &chain {
                parts.push(
                    div()
                        .flex()
                        .gap(px(4.0))
                        .child(div().text_color(theme::DIM).child(s(&link.kind)))
                        .child(div().text_color(theme::TERM_TEXT).child(s(&link.name)))
                        .into_any_element(),
                );
                parts.push(
                    div()
                        .text_color(theme::DIM)
                        .child("\u{203a}")
                        .into_any_element(),
                );
            }
            parts.push(
                div()
                    .text_color(theme::DIM)
                    .child(d.kind.singular())
                    .into_any_element(),
            );
            body.push(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(6.0))
                    .text_size(px(11.5))
                    .children(parts)
                    .into_any_element(),
            );
        }

        // Name and status.
        body.push(
            div()
                .debug_selector(|| "inspector-name".to_owned())
                .font_family(theme::MONO_FONT)
                .text_size(px(14.0))
                .text_color(theme::TEXT)
                .child(s(&d.key.name))
                .into_any_element(),
        );
        let mut chips: Vec<AnyElement> =
            vec![chip(s(&d.status), status_chip_color(d)).into_any_element()];
        for badge in &d.badges {
            chips.push(chip(s(badge), theme::MUTED).into_any_element());
        }
        body.push(
            div()
                .flex()
                .flex_wrap()
                .gap(px(6.0))
                .children(chips)
                .into_any_element(),
        );

        // Actions for this kind. They report clicks; wiring comes later.
        let actions = d.kind.actions();
        if !actions.is_empty() {
            let mut buttons = Vec::new();
            for action in actions {
                buttons.push(action_button(*action, target, cx).into_any_element());
            }
            body.push(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(6.0))
                    .children(buttons)
                    .into_any_element(),
            );
        }

        if !d.fields.is_empty() {
            body.push(block("Details", fields(&d.fields)));
        }
        if !d.containers.is_empty() {
            let rows: Vec<AnyElement> = d.containers.iter().map(container).collect();
            body.push(block(
                "Containers",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .children(rows)
                    .into_any_element(),
            ));
        }
        for section in &d.sections {
            body.push(render_section(section));
        }
        if !d.labels.is_empty() {
            let tags: Vec<AnyElement> = d
                .labels
                .iter()
                .map(|(k, v)| {
                    div()
                        .px(px(7.0))
                        .py(px(2.0))
                        .rounded(px(6.0))
                        .bg(theme::HOVER)
                        .font_family(theme::MONO_FONT)
                        .text_size(px(11.0))
                        .text_color(theme::MUTED)
                        .child(format!("{k}={v}"))
                        .into_any_element()
                })
                .collect();
            body.push(block(
                "Labels",
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(5.0))
                    .children(tags)
                    .into_any_element(),
            ));
        }

        div()
            .id("inspector-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(px(14.0))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .children(body)
            .into_any_element()
    }
}

fn status_chip_color(d: &Details) -> gpui_kit::Rgba {
    match d.health {
        model::Health::Unknown | model::Health::Done => theme::MUTED,
        other => health_color(other),
    }
}

fn action_button(
    action: Action,
    target: &InspectTarget,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let target = target.clone();
    let color = if action.destructive() {
        theme::RED
    } else {
        theme::TERM_TEXT
    };
    div()
        .id(SharedString::from(format!("action-{action:?}")))
        .debug_selector(move || format!("action-{}", action.label()))
        .h(px(26.0))
        .px(px(9.0))
        .flex()
        .items_center()
        .gap(px(7.0))
        .rounded(px(7.0))
        .border_1()
        .border_color(theme::BORDER)
        .bg(theme::HOVER)
        .cursor_pointer()
        .hover(|el| el.bg(theme::SELECTED))
        .text_size(px(12.0))
        .text_color(color)
        .on_click(cx.listener(move |this, _, window, cx| {
            tracing::debug!(?action, key = %target.key, "inspector action");
            this.run_action(action, target.clone(), window, cx);
        }))
        .child(action.label())
        .children(action.key().map(|k| {
            div()
                .font_family(theme::MONO_FONT)
                .text_size(px(10.5))
                .text_color(theme::DIM)
                .child(k)
        }))
}

fn block(title: &'static str, content: AnyElement) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .text_size(px(theme::TEXT_LABEL))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::DIM)
                .child(title),
        )
        .child(content)
        .into_any_element()
}

fn render_section(section: &Section) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .text_size(px(theme::TEXT_LABEL))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::DIM)
                .child(s(&section.title)),
        )
        .child(fields(&section.fields))
        .into_any_element()
}

fn fields(list: &[Field]) -> AnyElement {
    let rows: Vec<AnyElement> = list
        .iter()
        .map(|f| {
            div()
                .flex()
                .gap(px(8.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .w(px(LABEL_WIDTH))
                        .flex_none()
                        .text_color(theme::DIM)
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(s(&f.label)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme::TERM_TEXT)
                        .child(s(&f.value)),
                )
                .into_any_element()
        })
        .collect();
    div()
        .flex()
        .flex_col()
        .gap(px(5.0))
        .children(rows)
        .into_any_element()
}

fn container(c: &ContainerInfo) -> AnyElement {
    let color = match c.health {
        model::Health::Good => theme::TERM_TEXT,
        model::Health::Done | model::Health::Unknown => theme::MUTED,
        other => health_color(other),
    };
    let mut lines: Vec<AnyElement> = vec![
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .font_family(theme::MONO_FONT)
                    .text_size(px(12.0))
                    .text_color(theme::TEXT)
                    .child(s(&c.name)),
            )
            .when(c.init, |el| el.child(chip("init", theme::MUTED)))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(color)
                    .child(s(&c.state)),
            )
            .into_any_element(),
        div()
            .font_family(theme::MONO_FONT)
            .text_size(px(11.0))
            .text_color(theme::MUTED)
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .child(s(&c.image))
            .into_any_element(),
    ];
    let mut extra = Vec::new();
    if c.restarts > 0 {
        extra.push(format!("{} restarts", c.restarts));
    }
    if let Some(r) = &c.resources {
        extra.push(r.to_string());
    }
    if !extra.is_empty() {
        lines.push(
            div()
                .text_size(px(11.5))
                .text_color(theme::DIM)
                .child(extra.join(", "))
                .into_any_element(),
        );
    }
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .children(lines)
        .into_any_element()
}

fn centered(text: String) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .p(px(20.0))
        .text_color(theme::DIM)
        .child(text)
        .into_any_element()
}
