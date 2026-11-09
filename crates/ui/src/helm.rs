//! The Helm releases view: a table of releases and a details pane with
//! values, notes, manifest and history.
//!
//! The engine watches releases only while this view is open. Details are
//! decoded on demand, one revision at a time.

mod details;
mod table;

use std::sync::Arc;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::Size;
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::{
    AnyElement, Context, Div, Entity, FontWeight, Rgba, SharedString, Stateful, Subscription,
    UniformListScrollHandle, Window, div, prelude::*, px, svg,
};
use model::{
    Command, ContextName, HelmCommand, HelmEvent, HelmReleaseDetails, HelmReleaseKey, HelmRow,
    HelmSnapshot,
};

use crate::assets::Icon;
use crate::theme;
use crate::workspace::CommandSink;
use table::HelmTable;

/// The icon for Helm everywhere in the app.
pub(crate) const HELM_ICON: Icon = Icon::ShipWheel;

/// Narrowest the release list gets next to the details pane, enough for
/// the name column. With the details pane's minimum, both fit beside the
/// sidebar in an 800 pt window.
const LIST_MIN_WIDTH: f32 = 200.0;

/// The text tabs and the history tab of the details pane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Tab {
    #[default]
    Values,
    Notes,
    Manifest,
    History,
}

/// Lines of one text tab, split once when the details arrive.
struct TextLines {
    lines: Arc<[SharedString]>,
    /// Index of the longest line, which sets the scroll width.
    widest: usize,
}

/// What the details pane holds for the selected release.
enum Details {
    /// Nothing requested.
    None,
    /// Waiting for the engine.
    Loading(HelmReleaseKey, u32),
    /// Decoded.
    Loaded {
        release: HelmReleaseKey,
        revision: u32,
        details: Box<HelmReleaseDetails>,
        values: TextLines,
        notes: TextLines,
        manifest: TextLines,
    },
    /// The revision could not be read.
    Failed(HelmReleaseKey, u32, SharedString),
}

impl Details {
    fn is_for(&self, release: &HelmReleaseKey, revision: u32) -> bool {
        match self {
            Self::None => false,
            Self::Loading(k, r)
            | Self::Failed(k, r, _)
            | Self::Loaded {
                release: k,
                revision: r,
                ..
            } => k == release && *r == revision,
        }
    }
}

/// The Helm releases view of the selected context.
pub struct HelmView {
    send: CommandSink,
    /// The selected context. Snapshots for others are ignored.
    context: Option<ContextName>,
    /// Whether the engine is watching releases for us.
    open: bool,
    /// The latest snapshot for `context`.
    snapshot: Option<HelmSnapshot>,
    table: Entity<TableState<HelmTable>>,
    /// The release in the details pane.
    selected: Option<HelmReleaseKey>,
    /// A revision picked in the history tab. `None` follows the latest.
    pinned: Option<u32>,
    tab: Tab,
    details: Details,
    text_scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl HelmView {
    /// Builds the view. It shows nothing until a context is set.
    pub fn new(send: CommandSink, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let table =
            cx.new(|cx| TableState::new(HelmTable::default(), window, cx).col_movable(false));
        let selection = cx.subscribe(&table, |this, table, event: &TableEvent, cx| match event {
            TableEvent::SelectRow(ix) => {
                let key = table
                    .read(cx)
                    .delegate()
                    .rows()
                    .get(*ix)
                    .map(|r| r.key.clone());
                if let Some(key) = key {
                    this.select(key, cx);
                }
            }
            TableEvent::ClearSelection => this.deselect(cx),
            _ => {}
        });
        Self {
            send,
            context: None,
            open: false,
            snapshot: None,
            table,
            selected: None,
            pinned: None,
            tab: Tab::default(),
            details: Details::None,
            text_scroll: UniformListScrollHandle::new(),
            _subscriptions: vec![selection],
        }
    }

    /// Follows a new selected context: the old rows and details go.
    pub fn set_context(&mut self, context: Option<ContextName>, cx: &mut Context<Self>) {
        if self.context == context {
            return;
        }
        self.context = context;
        self.clear(cx);
    }

    /// Asks the engine to watch releases. The watch follows context changes.
    pub fn open(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            self.open = true;
            (self.send)(Command::Helm(HelmCommand::Open));
            cx.notify();
        }
    }

    /// Asks the engine to stop watching, and forgets what it sent.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            (self.send)(Command::Helm(HelmCommand::Close));
            self.clear(cx);
        }
    }

    /// How many releases the list holds, once it has loaded.
    pub fn release_count(&self) -> Option<usize> {
        self.snapshot
            .as_ref()
            .filter(|s| s.loaded)
            .map(|s| s.rows.len())
    }

    /// Takes in an engine event.
    pub fn apply(&mut self, event: HelmEvent, cx: &mut Context<Self>) {
        match event {
            HelmEvent::Releases(snapshot) => {
                if !self.open || self.context.as_ref() != Some(&snapshot.context) {
                    return;
                }
                table::set_rows(&self.table, Arc::clone(&snapshot.rows), cx);
                self.snapshot = Some(snapshot);
                self.follow_selection(cx);
            }
            HelmEvent::Details {
                context,
                release,
                revision,
                result,
            } => {
                let current = matches!(&self.details, Details::Loading(k, r) if *k == release && *r == revision);
                if self.context.as_ref() != Some(&context) || !current {
                    return;
                }
                self.details = match result {
                    Ok(details) => Details::Loaded {
                        release,
                        revision,
                        values: lines(&details.values),
                        notes: lines(&details.notes),
                        manifest: lines(&details.manifest),
                        details: Box::new(details),
                    },
                    Err(message) => Details::Failed(release, revision, message.into()),
                };
            }
        }
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.snapshot = None;
        self.selected = None;
        self.pinned = None;
        self.details = Details::None;
        table::set_rows(&self.table, Arc::from([]), cx);
        self.table.update(cx, TableState::clear_selection);
        cx.notify();
    }

    fn select(&mut self, key: HelmReleaseKey, cx: &mut Context<Self>) {
        if self.selected.as_ref() == Some(&key) {
            return;
        }
        self.selected = Some(key);
        self.pinned = None;
        self.follow_selection(cx);
        cx.notify();
    }

    fn deselect(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        self.pinned = None;
        self.details = Details::None;
        cx.notify();
    }

    /// Picks a revision from the history. The latest one unpins.
    fn pin(&mut self, revision: u32, cx: &mut Context<Self>) {
        let latest = self.selected_row().map(|r| r.revision);
        self.pinned = (latest != Some(revision)).then_some(revision);
        self.follow_selection(cx);
        cx.notify();
    }

    fn show_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            self.text_scroll
                .scroll_to_item(0, gpui_kit::ScrollStrategy::Top);
            cx.notify();
        }
    }

    /// The row of the selected release, if it still exists.
    fn selected_row(&self) -> Option<&HelmRow> {
        let key = self.selected.as_ref()?;
        let rows = &self.snapshot.as_ref()?.rows;
        rows.binary_search_by(|r| r.key.cmp(key))
            .ok()
            .and_then(|ix| rows.get(ix))
    }

    /// The revision the details pane shows: the pinned one while it is
    /// still stored, else the latest.
    fn shown_revision(&self) -> Option<u32> {
        let row = self.selected_row()?;
        Some(
            self.pinned
                .filter(|p| row.history.iter().any(|h| h.revision == *p))
                .unwrap_or(row.revision),
        )
    }

    /// Asks for the details the pane should show, if it does not have them.
    fn follow_selection(&mut self, cx: &mut Context<Self>) {
        let (Some(context), Some(key)) = (self.context.clone(), self.selected.clone()) else {
            return;
        };
        let Some(revision) = self.shown_revision() else {
            // The selected release has not arrived yet, or is gone.
            if self.snapshot.as_ref().is_some_and(|s| s.loaded) {
                self.deselect(cx);
            }
            return;
        };
        if self.pinned.is_some() && self.pinned != Some(revision) {
            self.pinned = None;
        }
        if self.details.is_for(&key, revision) {
            return;
        }
        self.details = Details::Loading(key.clone(), revision);
        self.text_scroll
            .scroll_to_item(0, gpui_kit::ScrollStrategy::Top);
        (self.send)(Command::Helm(HelmCommand::LoadDetails {
            context,
            release: key,
            revision,
        }));
    }
}

/// Splits text into lines for the text tabs. Tabs become spaces so columns
/// line up in the monospace font.
fn lines(text: &str) -> TextLines {
    let lines: Arc<[SharedString]> = text
        .lines()
        .map(|line| SharedString::from(line.replace('\t', "    ")))
        .collect();
    let widest = lines
        .iter()
        .enumerate()
        .max_by_key(|(_, line)| line.chars().count())
        .map_or(0, |(ix, _)| ix);
    TextLines { lines, widest }
}

impl Render for HelmView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let details = self
            .selected_row()
            .is_some()
            .then(|| self.render_details(cx));
        div()
            .size_full()
            .flex()
            .gap(px(theme::PANE_GAP))
            .child(self.render_list())
            .children(details)
    }
}

// The list pane.
impl HelmView {
    fn render_list(&self) -> impl IntoElement {
        let count = self.release_count();
        let problem = self
            .snapshot
            .as_ref()
            .filter(|s| s.loaded)
            .and_then(|s| s.problem.clone());
        pane()
            .flex_1()
            .min_w(px(LIST_MIN_WIDTH))
            .child(
                pane_header()
                    .child(icon(HELM_ICON, 13.0, theme::MUTED))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::TEXT)
                            .child("Helm releases"),
                    )
                    .when(self.context.is_some(), |el| el.child("all namespaces"))
                    .child(div().flex_1())
                    .children(count.map(|n| {
                        div()
                            .font_family(theme::MONO_FONT)
                            .text_color(theme::DIM)
                            .child(format!("{n} release{}", if n == 1 { "" } else { "s" }))
                    })),
            )
            .children(problem.map(|text| {
                div()
                    .debug_selector(|| "helm-problem".to_owned())
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(12.0))
                    .py(px(6.0))
                    .border_b_1()
                    .border_color(theme::BORDER)
                    .text_size(px(12.0))
                    .text_color(theme::AMBER)
                    .child(icon(Icon::TriangleAlert, 13.0, theme::AMBER))
                    .child(SharedString::from(text))
            }))
            .child(div().flex_1().min_h_0().child(self.render_list_body()))
    }

    fn render_list_body(&self) -> AnyElement {
        let empty = match (&self.context, &self.snapshot) {
            (None, _) => Some(empty(
                Icon::Plug,
                theme::DIM,
                "Select a cluster",
                "Pick a context in the sidebar to see its Helm releases.",
            )),
            (Some(_), None) => Some(empty(
                Icon::LoaderCircle,
                theme::DIM,
                "Loading Helm releases",
                "",
            )),
            (Some(_), Some(s)) if !s.loaded => Some(empty(
                Icon::KeyRound,
                theme::RED,
                "Could not list Helm releases",
                s.problem.as_deref().unwrap_or_default(),
            )),
            (Some(_), Some(s)) if s.rows.is_empty() => Some(empty(
                HELM_ICON,
                theme::DIM,
                "No Helm releases",
                "No Secrets or ConfigMaps you can see are labelled owner=helm.",
            )),
            _ => None,
        };
        if let Some(empty) = empty {
            return empty;
        }
        div()
            .size_full()
            .px(px(6.0))
            .pt(px(2.0))
            .child(
                DataTable::new(&self.table)
                    .with_size(Size::Small)
                    .header_row_height(px(28.0))
                    .bordered(false)
                    .stripe(false)
                    .accessibility_label("Helm releases"),
            )
            .into_any_element()
    }
}

/// A sidebar row for the Helm releases view, styled like the kind rows.
/// The caller adds the click handler.
pub(crate) fn sidebar_entry(active: bool, enabled: bool, count: Option<usize>) -> Stateful<Div> {
    div()
        .id("sidebar-helm")
        .debug_selector(|| "sidebar-helm".to_owned())
        .h(px(28.0))
        .flex()
        .items_center()
        .gap(px(9.0))
        .px(px(8.0))
        .rounded(px(theme::ROW_RADIUS))
        .text_color(if active {
            theme::TEXT
        } else if enabled {
            theme::MUTED
        } else {
            theme::DIM
        })
        .when(active, |el| el.bg(theme::SELECTED))
        .when(enabled && !active, |el| {
            el.cursor_pointer().hover(|s| s.bg(theme::HOVER))
        })
        .child(icon(
            HELM_ICON,
            14.0,
            if active { theme::TEXT } else { theme::DIM },
        ))
        .child(div().flex_1().child("Helm releases"))
        .children(count.map(|n| {
            div()
                .font_family(theme::MONO_FONT)
                .text_size(px(theme::TEXT_LABEL))
                .text_color(theme::DIM)
                .child(n.to_string())
        }))
}

/// A pane card, like the pod pane.
fn pane() -> Div {
    div()
        .h_full()
        .flex()
        .flex_col()
        .bg(theme::PANE)
        .border_1()
        .border_color(theme::BORDER)
        .rounded(px(theme::PANE_RADIUS))
        .overflow_hidden()
}

/// A pane's 30 px header row.
fn pane_header() -> Div {
    div()
        .h(px(theme::PANE_HEADER_HEIGHT))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(8.0))
        .pl(px(11.0))
        .pr(px(10.0))
        .border_b_1()
        .border_color(theme::BORDER)
        .text_size(px(12.0))
        .text_color(theme::MUTED)
}

fn icon(icon: Icon, size: f32, color: Rgba) -> impl IntoElement {
    svg()
        .path(icon.path())
        .flex_none()
        .size(px(size))
        .text_color(color)
}

fn empty(
    path: Icon,
    color: Rgba,
    title: &'static str,
    body: impl Into<SharedString>,
) -> AnyElement {
    let body = body.into();
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p(px(24.0))
        .child(
            div()
                .max_w(px(460.0))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.0))
                .child(icon(path, 22.0, color))
                .child(
                    div()
                        .mt(px(4.0))
                        .text_size(px(14.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme::TEXT)
                        .child(title),
                )
                .when(!body.is_empty(), |el| {
                    el.child(div().text_center().text_color(theme::MUTED).child(body))
                }),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_split_and_expand_tabs() {
        let split = lines("a: 1\n\tb: 2\n");
        let texts: Vec<&str> = split.lines.iter().map(SharedString::as_ref).collect();
        assert_eq!(texts, ["a: 1", "    b: 2"]);
        assert_eq!(split.widest, 1);
        assert!(lines("").lines.is_empty());
    }
}
