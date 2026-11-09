//! The sidebar: clusters, then the selected cluster's kinds.

use gpui_kit::{AnyElement, Context, FontWeight, Rgba, SharedString, div, prelude::*, px};
use model::resource::Kind;
use model::{Catalog, ConnectionState, ContextInfo};

use crate::assets::{Icon, kind_icon};
use crate::forwards;
use crate::theme;
use crate::widgets::{dot, icon, plural, section_label};
use crate::workspace::{Workspace, shared};

/// Typed kinds in sidebar groups, like Freelens. Discovered kinds (custom
/// resources and the rest) get their own groups with issue #19.
const GROUPS: [(&str, &[Kind]); 5] = [
    (
        "Workloads",
        &[
            Kind::Pod,
            Kind::Deployment,
            Kind::StatefulSet,
            Kind::DaemonSet,
            Kind::ReplicaSet,
            Kind::Job,
            Kind::CronJob,
        ],
    ),
    ("Network", &[Kind::Service, Kind::Ingress]),
    ("Config", &[Kind::ConfigMap, Kind::Secret]),
    (
        "Storage",
        &[
            Kind::PersistentVolumeClaim,
            Kind::PersistentVolume,
            Kind::StorageClass,
        ],
    ),
    ("Cluster", &[Kind::Node, Kind::Namespace, Kind::Event]),
];

/// Room for the macOS window buttons at the top left of the sidebar.
const TRAFFIC_LIGHTS_WIDTH: f32 = 80.0;

impl Workspace {
    pub(crate) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(theme::SIDEBAR_WIDTH))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::CHROME)
            .border_r_1()
            .border_color(theme::BORDER)
            .child(self.render_brand())
            .child(
                div()
                    .id("sidebar-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .pb(px(8.0))
                    .child(section_label("Clusters"))
                    .child(self.render_clusters(cx))
                    .child(self.render_kinds(cx))
                    .child(self.render_forwards_row(cx)),
            )
            .child(self.render_sidebar_footer())
    }

    fn render_brand(&self) -> impl IntoElement {
        // The traffic lights sit at the left of this row; the name follows them.
        div()
            .h(px(46.0))
            .flex_none()
            .flex()
            .items_center()
            .pl(px(TRAFFIC_LIGHTS_WIDTH))
            .pr(px(14.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .font_weight(FontWeight::MEDIUM)
                    .child(
                        div()
                            .size(px(18.0))
                            .rounded(px(5.0))
                            .bg(theme::TEXT)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon(Icon::Box, 11.0, theme::CHROME)),
                    )
                    .child(self.app_name.clone()),
            )
    }

    fn render_clusters(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(catalog) = &self.catalog else {
            return note("Reading kubeconfig").into_any_element();
        };
        if catalog.is_empty() {
            return note("No contexts found").into_any_element();
        }
        let mut rows = Vec::with_capacity(catalog.contexts.len());
        for info in &catalog.contexts {
            rows.push(
                self.render_cluster_row(info, catalog, cx)
                    .into_any_element(),
            );
        }
        div()
            .flex()
            .flex_col()
            .px(px(8.0))
            .children(rows)
            .into_any_element()
    }

    fn render_cluster_row(
        &self,
        info: &ContextInfo,
        catalog: &Catalog,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let name = info.name.clone();
        let selected = self.selected.as_ref() == Some(&info.name);
        let is_current = catalog.current.as_ref() == Some(&info.name);
        let right: AnyElement = match self.states.get(&info.name) {
            Some(state) => dot(state_color(state), 7.0).into_any_element(),
            None if is_current => div()
                .text_size(px(theme::TEXT_LABEL))
                .text_color(theme::DIM)
                .child("current")
                .into_any_element(),
            None => div().into_any_element(),
        };
        div()
            .id(SharedString::from(format!("cluster-{}", info.name)))
            .debug_selector(|| format!("cluster-{}", info.name))
            .h(px(theme::ROW_HEIGHT))
            .flex()
            .items_center()
            .gap(px(9.0))
            .px(px(8.0))
            .rounded(px(theme::ROW_RADIUS))
            .text_color(if selected {
                theme::TEXT
            } else {
                theme::TERM_TEXT
            })
            .when(selected, |el| el.bg(theme::SELECTED))
            .when(!selected, |el| el.hover(|s| s.bg(theme::HOVER)))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.select(name.clone(), cx)))
            .child(letter_tile(info.name.as_str()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(shared(&info.name)),
            )
            .child(div().flex_none().child(right))
    }

    fn render_kinds(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut groups: Vec<AnyElement> = Vec::new();
        for (title, kinds) in GROUPS {
            groups.push(section_label(title).into_any_element());
            let mut rows = Vec::new();
            for kind in kinds {
                rows.push(self.render_kind_row(*kind, cx).into_any_element());
            }
            groups.push(
                div()
                    .px(px(8.0))
                    .flex()
                    .flex_col()
                    .children(rows)
                    .into_any_element(),
            );
        }
        let title: SharedString = self
            .selected
            .as_ref()
            .map_or_else(|| "Resources".into(), shared);
        div()
            .mt(px(6.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .mx(px(16.0))
                    .mt(px(4.0))
                    .pt(px(8.0))
                    .border_t_1()
                    .border_color(theme::BORDER)
                    .text_size(px(theme::TEXT_LABEL))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::MUTED)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(title),
            )
            .children(groups)
    }

    fn render_kind_row(&self, kind: Kind, cx: &mut Context<Self>) -> impl IntoElement {
        let active = kind == self.kind;
        let context = self.selected.clone();
        let count = context
            .as_ref()
            .and_then(|c| self.counts.get(&(c.clone(), kind)))
            .copied();
        let failed = context.as_ref().is_some_and(|c| {
            matches!(
                self.statuses.get(&(c.clone(), kind)),
                Some(ConnectionState::Failed(_))
            )
        });
        let right: Option<AnyElement> = if failed {
            Some(dot(theme::RED, 6.0).into_any_element())
        } else {
            count.map(|n| {
                div()
                    .font_family(theme::MONO_FONT)
                    .text_size(px(theme::TEXT_LABEL))
                    .text_color(theme::DIM)
                    .child(n.to_string())
                    .into_any_element()
            })
        };
        div()
            .id(SharedString::from(format!("kind-{kind:?}")))
            .debug_selector(|| format!("kind-{}", kind.singular()))
            .h(px(28.0))
            .flex()
            .items_center()
            .gap(px(9.0))
            .px(px(8.0))
            .rounded(px(theme::ROW_RADIUS))
            .cursor_pointer()
            .text_color(if active { theme::TEXT } else { theme::MUTED })
            .when(active, |el| el.bg(theme::SELECTED))
            .when(!active, |el| el.hover(|s| s.bg(theme::HOVER)))
            .on_click(cx.listener(move |this, _, _, cx| this.select_kind(kind, cx)))
            .child(icon(
                kind_icon(kind),
                14.0,
                if active { theme::TEXT } else { theme::DIM },
            ))
            .child(div().flex_1().child(kind.plural()))
            .children(right)
    }

    fn render_forwards_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = self.forwards_pane().read(cx);
        div().mt(px(10.0)).px(px(8.0)).child(forwards::sidebar_row(
            pane.running(),
            pane.total(),
            self.show_forwards,
            cx.listener(|this, _, _, cx| {
                this.show_forwards = !this.show_forwards;
                cx.notify();
            }),
        ))
    }

    fn render_sidebar_footer(&self) -> impl IntoElement {
        let (text, color) = match &self.catalog {
            None => (SharedString::from(""), theme::DIM),
            Some(c) if !c.problems.is_empty() => (
                format!(
                    "{} kubeconfig file{} could not be read",
                    c.problems.len(),
                    plural(c.problems.len())
                )
                .into(),
                theme::AMBER,
            ),
            Some(c) => {
                let found = c.sources.len();
                (
                    format!(
                        "{} contexts from {found} file{}",
                        c.contexts.len(),
                        plural(found)
                    )
                    .into(),
                    theme::DIM,
                )
            }
        };
        div()
            .h(px(40.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(16.0))
            .border_t_1()
            .border_color(theme::BORDER)
            .text_size(px(12.0))
            .text_color(color)
            .child(icon(Icon::FolderOpen, 13.0, theme::DIM))
            .child(
                div()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(text),
            )
    }
}

/// Dot colour for a connection.
pub(crate) fn state_color(state: &ConnectionState) -> Rgba {
    match state {
        ConnectionState::Connecting | ConnectionState::Retrying(_) => theme::AMBER,
        ConnectionState::Live => theme::GREEN,
        ConnectionState::Failed(_) => theme::RED,
    }
}

fn note(text: &'static str) -> impl IntoElement {
    div()
        .h(px(theme::ROW_HEIGHT))
        .flex()
        .items_center()
        .px(px(16.0))
        .text_color(theme::DIM)
        .child(text)
}

fn letter_tile(name: &str) -> impl IntoElement {
    let letter = name
        .chars()
        .find(char::is_ascii_alphanumeric)
        .map_or('?', |c| c.to_ascii_uppercase());
    div()
        .size(px(18.0))
        .flex_none()
        .rounded(px(5.0))
        .bg(tile_color(name))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(10.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(gpui_kit::white())
        .child(SharedString::from(letter.to_string()))
}

/// A stable colour per cluster name.
fn tile_color(name: &str) -> Rgba {
    // FNV-1a: tiny, stable across runs and platforms.
    let hash = name.bytes().fold(0x811c_9dc5_u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    });
    let ix = usize::try_from(hash).unwrap_or(0) % theme::TILE_COLORS.len();
    theme::TILE_COLORS[ix]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_is_in_one_group() {
        let mut listed: Vec<Kind> = GROUPS.iter().flat_map(|(_, k)| k.iter().copied()).collect();
        listed.sort();
        let mut all = Kind::ALL.to_vec();
        all.sort();
        assert_eq!(listed, all);
    }

    #[test]
    fn tile_colour_is_stable() {
        assert_eq!(tile_color("kind-platform"), tile_color("kind-platform"));
    }
}
