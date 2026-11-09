//! The log pane: one log tab, virtualized, with follow, wrap, time and
//! search.
//!
//! The engine sends chunks of lines already sorted by time. The pane keeps
//! them in a [`LogBuffer`] and lays out only the visible lines with GPUI's
//! `list`, which also follows the tail: scrolling up pauses following, and
//! scrolling back to the bottom resumes it.

use std::ops::Range;
use std::sync::Arc;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::{
    AnyElement, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FollowMode,
    FontWeight, HighlightStyle, ListAlignment, ListOffset, ListState, MouseButton, Rgba,
    SharedString, StyledText, Subscription, Window, div, list, prelude::*, px, svg,
};
use model::{
    LineKind, LogBuffer, LogChange, LogLevel, LogLine, LogSource, LogState, LogTabId, LogTarget,
};

use crate::FocusLogSearch;
use crate::assets::Icon;
use crate::log_search::LogSearch;
use crate::theme;

/// Line height: 12 px mono at a 1.7 line height, as in the design.
const LINE_HEIGHT: f32 = 20.0;
/// Fits `00:00:00.000` in the 12 px mono font.
const TIME_WIDTH: f32 = 92.0;
/// Fits a five-character pod suffix.
const POD_TAG_WIDTH: f32 = 54.0;
/// Fits a container name, or a pod suffix and a container name.
const WIDE_TAG_WIDTH: f32 = 110.0;
/// Fits `debug` and `error`.
const LEVEL_WIDTH: f32 = 40.0;
const COLUMN_GAP: f32 = 12.0;
/// Lines shown above a search match when jumping to it.
const MATCH_CONTEXT: usize = 3;
/// Pixels laid out beyond the viewport, so fast scrolls do not flash.
const OVERDRAW: f32 = 400.0;

/// What the pane asks of its owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogPaneEvent {
    /// The user closed the pane.
    Close,
}

/// One log tab on screen.
pub struct LogPane {
    tab: LogTabId,
    target: LogTarget,
    buffer: LogBuffer,
    state: LogState,
    list: ListState,
    wrap: bool,
    show_time: bool,
    sources: Sources,
    search: LogSearch,
    search_input: Entity<InputState>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<LogPaneEvent> for LogPane {}

impl Focusable for LogPane {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl LogPane {
    /// A pane for a tab the engine is opening.
    pub fn new(
        tab: LogTabId,
        target: LogTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let list = ListState::new(0, ListAlignment::Top, px(OVERDRAW));
        list.set_follow_mode(FollowMode::Tail);
        let search_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search")
                .clean_on_escape()
        });
        let subscription = cx.subscribe_in(&search_input, window, Self::on_search_event);
        Self {
            tab,
            target,
            buffer: LogBuffer::default(),
            state: LogState::Starting,
            list,
            wrap: false,
            show_time: true,
            sources: Sources::default(),
            search: LogSearch::default(),
            search_input,
            focus: cx.focus_handle(),
            _subscriptions: vec![subscription],
        }
    }

    /// The tab this pane shows.
    pub fn tab(&self) -> LogTabId {
        self.tab
    }

    /// What the tab follows.
    pub fn target(&self) -> &LogTarget {
        &self.target
    }

    /// Adds a chunk from the engine.
    pub fn push(&mut self, lines: Vec<LogLine>, cx: &mut Context<Self>) {
        for line in &lines {
            self.sources.see(&line.source);
        }
        let old_len = self.buffer.len();
        let change = self.buffer.push(lines);
        for (range, count) in list_splices(old_len, self.buffer.len(), change) {
            self.list.splice(range, count);
        }
        self.search.update(change, &self.buffer);
        cx.notify();
    }

    /// Records the tab's new state from the engine.
    pub fn set_state(&mut self, state: LogState, cx: &mut Context<Self>) {
        if self.state != state {
            self.state = state;
            cx.notify();
        }
    }

    fn on_search_event(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                let query = input.read(cx).value();
                self.search.set_query(&query, &self.buffer);
                cx.notify();
            }
            InputEvent::PressEnter { shift, .. } => {
                if *shift {
                    self.prev_match(cx);
                } else {
                    self.next_match(cx);
                }
            }
            InputEvent::Focus | InputEvent::Blur => {}
        }
    }

    fn next_match(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self.search.next(&self.buffer) {
            self.reveal(ix);
        }
        cx.notify();
    }

    fn prev_match(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self.search.prev(&self.buffer) {
            self.reveal(ix);
        }
        cx.notify();
    }

    /// Scrolls a line into view near the top. This pauses following.
    fn reveal(&self, ix: usize) {
        self.list.scroll_to(ListOffset {
            item_ix: ix.saturating_sub(MATCH_CONTEXT),
            offset_in_item: px(0.0),
        });
    }

    fn toggle_follow(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        // `Normal` stops following for good; `Tail` jumps to the end and
        // keeps up from there.
        let mode = if self.list.is_following_tail() {
            FollowMode::Normal
        } else {
            FollowMode::Tail
        };
        self.list.set_follow_mode(mode);
        cx.notify();
    }

    fn toggle_wrap(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.wrap = !self.wrap;
        self.list.remeasure();
        cx.notify();
    }

    fn toggle_time(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.show_time = !self.show_time;
        self.list.remeasure();
        cx.notify();
    }

    fn focus_search(&mut self, _: &FocusLogSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.search_input
            .update(cx, |input, cx| input.focus(window, cx));
    }
}

impl Render for LogPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("log-pane")
            .key_context("LogPane")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::focus_search))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.focus.focus(window, cx)),
            )
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::PANE)
            .border_1()
            .border_color(theme::BORDER)
            .rounded(px(theme::PANE_RADIUS))
            .overflow_hidden()
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(self.render_body(cx)))
    }
}

impl LogPane {
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (detail, color) = state_detail(&self.state, &self.target);
        div()
            .h(px(theme::PANE_HEADER_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .pl(px(11.0))
            .pr(px(8.0))
            .border_b_1()
            .border_color(theme::BORDER)
            .text_size(px(12.0))
            .text_color(theme::MUTED)
            .whitespace_nowrap()
            .child(icon(Icon::Logs, 13.0, theme::MUTED))
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::TEXT)
                    .child("Logs"),
            )
            .child(
                div()
                    .min_w(px(60.0))
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(SharedString::from(self.target.title())),
            )
            .child(div().flex_none().size(px(6.0)).rounded_full().bg(color))
            .child(div().flex_none().text_color(theme::DIM).child(detail))
            .child(div().flex_1().min_w(px(4.0)))
            .child(toggle(
                "log-follow",
                "Follow",
                self.list.is_following_tail(),
                cx.listener(Self::toggle_follow),
            ))
            .child(toggle(
                "log-wrap",
                "Wrap",
                self.wrap,
                cx.listener(Self::toggle_wrap),
            ))
            .child(toggle(
                "log-time",
                "Time",
                self.show_time,
                cx.listener(Self::toggle_time),
            ))
            .child(self.render_search(cx))
            .child(
                icon_button("log-close", Icon::Close)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(LogPaneEvent::Close))),
            )
    }

    fn render_search(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count: SharedString = match self.search.position() {
            Some((n, total)) => format!("{n} of {total}").into(),
            None if self.search.is_active() => "0 of 0".into(),
            None => "".into(),
        };
        div()
            .h(px(22.0))
            .w(px(240.0))
            .min_w(px(110.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .pl(px(8.0))
            .pr(px(2.0))
            .rounded(px(6.0))
            .bg(theme::HOVER)
            .border_1()
            .border_color(theme::BORDER)
            .child(icon(Icon::Search, 12.0, theme::DIM))
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.search_input)
                        .appearance(false)
                        .h(px(20.0))
                        .text_size(px(12.0))
                        .text_color(theme::TERM_TEXT)
                        .aria_label("Search logs"),
                ),
            )
            .when(self.search.is_active(), |el| {
                el.child(
                    div()
                        .flex_none()
                        .font_family(theme::MONO_FONT)
                        .text_size(px(11.0))
                        .text_color(theme::DIM)
                        .child(count),
                )
                .child(
                    icon_button("log-prev", Icon::ChevronUp)
                        .on_click(cx.listener(|this, _, _, cx| this.prev_match(cx))),
                )
                .child(
                    icon_button("log-next", Icon::ChevronDown)
                        .on_click(cx.listener(|this, _, _, cx| this.next_match(cx))),
                )
            })
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.buffer.is_empty() {
            let (text, color) = empty_text(&self.state, &self.target);
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p(px(16.0))
                .text_center()
                .text_color(color)
                .child(text)
                .into_any_element();
        }
        list(
            self.list.clone(),
            cx.processor(|this, ix: usize, _, _| this.render_line(ix)),
        )
        .size_full()
        .py(px(6.0))
        .into_any_element()
    }

    fn render_line(&self, ix: usize) -> AnyElement {
        let Some(line) = self.buffer.get(ix) else {
            return div().into_any_element();
        };
        let current = self.search.current_line(&self.buffer) == Some(ix);
        let tag = self.sources.tag(&line.source);
        let (level_text, level_color) = level_cell(line.kind);
        let text_color = match line.kind {
            LineKind::Text(_) => theme::TERM_TEXT,
            LineKind::Restarted { .. } | LineKind::Notice => theme::DIM,
        };
        let text = SharedString::from(Arc::clone(&line.text));
        let highlights = self.search.ranges(&text);
        let styled = StyledText::new(text).with_highlights(highlights.into_iter().map(highlight));

        let mut row = div()
            .w_full()
            .flex()
            .gap(px(COLUMN_GAP))
            .px(px(14.0))
            .font_family(theme::MONO_FONT)
            .text_size(px(theme::TEXT_MONO))
            .line_height(px(LINE_HEIGHT))
            .when(current, |el| el.bg(theme::SELECTED));
        row = if self.wrap {
            row.items_start()
        } else {
            row.h(px(LINE_HEIGHT)).items_center().whitespace_nowrap()
        };
        if self.show_time {
            row = row.child(
                div()
                    .w(px(TIME_WIDTH))
                    .flex_none()
                    .text_color(theme::DIM)
                    .child(SharedString::from(line.clock.to_string())),
            );
        }
        if let Some((tag, color, width)) = tag {
            row = row.child(
                div()
                    .w(px(width))
                    .flex_none()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(color)
                    .child(tag),
            );
        }
        row.child(
            div()
                .w(px(LEVEL_WIDTH))
                .flex_none()
                .text_color(level_color)
                .child(level_text),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(text_color)
                .when(!self.wrap, |el| el.overflow_hidden().text_ellipsis())
                .child(styled),
        )
        .into_any_element()
    }
}

/// Which pods and containers a tab has shown, to decide the tag column.
#[derive(Debug, Default)]
struct Sources {
    first: Option<Arc<LogSource>>,
    many_pods: bool,
    many_containers: bool,
}

impl Sources {
    fn see(&mut self, source: &Arc<LogSource>) {
        match &self.first {
            None => self.first = Some(Arc::clone(source)),
            Some(first) => {
                self.many_pods |= first.pod != source.pod;
                self.many_containers |= first.container != source.container;
            }
        }
    }

    /// The tag text, colour and column width for a line, or `None` when
    /// every line so far came from one container.
    fn tag(&self, source: &LogSource) -> Option<(SharedString, Rgba, f32)> {
        match (self.many_pods, self.many_containers) {
            (false, false) => None,
            (true, false) => Some((
                source.short_pod().to_owned().into(),
                tag_color(&source.pod),
                POD_TAG_WIDTH,
            )),
            (false, true) => Some((
                SharedString::from(Arc::clone(&source.container)),
                tag_color(&source.container),
                WIDE_TAG_WIDTH,
            )),
            (true, true) => Some((
                format!("{}/{}", source.short_pod(), source.container).into(),
                tag_color(&source.pod),
                WIDE_TAG_WIDTH,
            )),
        }
    }
}

/// How to splice the list after a buffer push: drop the evicted lines,
/// then replace everything from the first changed line on.
fn list_splices(old_len: usize, new_len: usize, change: LogChange) -> Vec<(Range<usize>, usize)> {
    if change.first_changed == 0 {
        // Something landed at the very front; redo the whole list.
        return vec![(0..old_len, new_len)];
    }
    // Lines before `first_changed` are old lines that only moved down, so
    // every evicted line was an old one.
    let kept = old_len - change.evicted;
    let mut splices = Vec::with_capacity(2);
    if change.evicted > 0 {
        splices.push((0..change.evicted, 0));
    }
    splices.push((change.first_changed..kept, new_len - change.first_changed));
    splices
}

/// A stable colour per pod (or container) name.
fn tag_color(name: &str) -> Rgba {
    // FNV-1a: tiny, stable across runs and platforms.
    let hash = name.bytes().fold(0x811c_9dc5_u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    });
    let ix = usize::try_from(hash).unwrap_or(0) % theme::TAG_COLORS.len();
    theme::TAG_COLORS[ix]
}

fn level_cell(kind: LineKind) -> (&'static str, Rgba) {
    match kind {
        LineKind::Text(Some(level)) => (level.as_str(), level_color(level)),
        LineKind::Text(None) | LineKind::Restarted { .. } | LineKind::Notice => ("", theme::DIM),
    }
}

fn level_color(level: LogLevel) -> Rgba {
    match level {
        LogLevel::Error => theme::RED,
        LogLevel::Warn => theme::AMBER,
        LogLevel::Info | LogLevel::Debug | LogLevel::Trace => theme::DIM,
    }
}

fn highlight(range: Range<usize>) -> (Range<usize>, HighlightStyle) {
    let style = HighlightStyle {
        background_color: Some(theme::MARK.into()),
        color: Some(theme::TEXT.into()),
        ..HighlightStyle::default()
    };
    (range, style)
}

/// A short state for the header, and the colour of its dot.
fn state_detail(state: &LogState, target: &LogTarget) -> (SharedString, Rgba) {
    match state {
        LogState::Starting => ("connecting".into(), theme::AMBER),
        LogState::Streaming { pods, containers } => {
            let text = match target {
                LogTarget::Pod { .. } if *containers > 1 => format!("{containers} containers"),
                LogTarget::Pod { .. } => "live".to_owned(),
                LogTarget::Selector { .. } => format!("{pods} pod{}", plural(*pods)),
            };
            (text.into(), theme::GREEN)
        }
        LogState::NoPods => match target {
            LogTarget::Pod { .. } => ("pod not found".into(), theme::DIM),
            LogTarget::Selector { .. } => ("no pods".into(), theme::DIM),
        },
        LogState::Done => ("finished".into(), theme::DIM),
        LogState::Failed(_) => ("failed".into(), theme::RED),
    }
}

/// What the body says before any line arrives.
fn empty_text(state: &LogState, target: &LogTarget) -> (SharedString, Rgba) {
    match state {
        LogState::Starting => ("Connecting".into(), theme::DIM),
        LogState::Streaming { .. } => ("Waiting for log lines".into(), theme::DIM),
        LogState::NoPods => match target {
            LogTarget::Pod { name, .. } => (
                format!("Pod {name} was not found. It may have been deleted.").into(),
                theme::DIM,
            ),
            LogTarget::Selector { selector, .. } => (
                format!("No pods match {selector}. New ones will show up here.").into(),
                theme::DIM,
            ),
        },
        LogState::Done => ("No log lines".into(), theme::DIM),
        LogState::Failed(message) => (
            format!("Could not stream logs: {message}").into(),
            theme::RED,
        ),
    }
}

fn toggle(
    id: &'static str,
    label: &'static str,
    on: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .h(px(22.0))
        .px(px(7.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .cursor_pointer()
        .text_color(if on { theme::TEXT } else { theme::DIM })
        .when(on, |el| el.bg(theme::SELECTED))
        .when(!on, |el| el.hover(|s| s.bg(theme::HOVER)))
        .on_click(on_click)
        .child(label)
}

fn icon_button(id: &'static str, glyph: Icon) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .size(px(20.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.0))
        .cursor_pointer()
        .hover(|s| s.bg(theme::HOVER))
        .child(icon(glyph, 12.0, theme::MUTED))
}

fn icon(glyph: Icon, size: f32, color: Rgba) -> impl IntoElement {
    svg()
        .path(glyph.path())
        .flex_none()
        .size(px(size))
        .text_color(color)
}

fn plural(n: u32) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(pod: &str, container: &str) -> Arc<LogSource> {
        Arc::new(LogSource {
            pod: pod.into(),
            container: container.into(),
        })
    }

    #[test]
    fn one_container_has_no_tag_column() {
        let mut sources = Sources::default();
        let a = source("web-6b9-x2kqp", "app");
        sources.see(&a);
        sources.see(&a);
        assert!(sources.tag(&a).is_none());
    }

    #[test]
    fn many_pods_are_tagged_by_suffix_with_a_stable_colour() {
        let mut sources = Sources::default();
        let a = source("web-6b9-x2kqp", "app");
        let b = source("web-6b9-m8vtn", "app");
        sources.see(&a);
        sources.see(&b);
        let (tag, color, width) = sources.tag(&a).unwrap();
        assert_eq!(&*tag, "x2kqp");
        assert_eq!(color, tag_color("web-6b9-x2kqp"));
        assert_eq!(color, sources.tag(&a).unwrap().1, "stable");
        assert!((width - POD_TAG_WIDTH).abs() < f32::EPSILON);
    }

    #[test]
    fn many_containers_are_tagged_by_name() {
        let mut sources = Sources::default();
        sources.see(&source("web-0", "app"));
        sources.see(&source("web-0", "envoy"));
        assert_eq!(&*sources.tag(&source("web-0", "envoy")).unwrap().0, "envoy");
        sources.see(&source("web-1", "app"));
        assert_eq!(&*sources.tag(&source("web-1", "app")).unwrap().0, "1/app");
    }

    #[test]
    fn splices_for_an_append() {
        let change = LogChange {
            evicted: 0,
            first_changed: 10,
            inserted: 3,
        };
        assert_eq!(list_splices(10, 13, change), [(10..10, 3)]);
    }

    #[test]
    fn splices_for_an_append_to_a_full_buffer() {
        // Capacity 10, 10 held, 3 appended: 3 evicted from the front.
        let change = LogChange {
            evicted: 3,
            first_changed: 7,
            inserted: 3,
        };
        assert_eq!(list_splices(10, 10, change), [(0..3, 0), (7..7, 3)]);
    }

    #[test]
    fn splices_for_a_late_line() {
        // One line lands at index 4 of 10.
        let change = LogChange {
            evicted: 0,
            first_changed: 4,
            inserted: 1,
        };
        assert_eq!(list_splices(10, 11, change), [(4..10, 7)]);
    }

    #[test]
    fn splices_for_the_first_chunk_and_front_inserts() {
        let change = LogChange {
            evicted: 0,
            first_changed: 0,
            inserted: 5,
        };
        assert_eq!(list_splices(0, 5, change), [(0..0, 5)]);
        assert_eq!(
            list_splices(
                8,
                9,
                LogChange {
                    inserted: 1,
                    ..change
                }
            ),
            [(0..8, 9)]
        );
    }

    #[test]
    fn levels_colour_only_warnings_and_errors() {
        assert_eq!(
            level_cell(LineKind::Text(Some(LogLevel::Error))),
            ("error", theme::RED)
        );
        assert_eq!(
            level_cell(LineKind::Text(Some(LogLevel::Warn))),
            ("warn", theme::AMBER)
        );
        assert_eq!(
            level_cell(LineKind::Text(Some(LogLevel::Info))),
            ("info", theme::DIM)
        );
        assert_eq!(level_cell(LineKind::Text(None)).0, "");
    }

    #[test]
    fn header_states() {
        let pod = LogTarget::Pod {
            namespace: "shop".into(),
            name: "web-0".into(),
        };
        let workload = LogTarget::Selector {
            namespace: "shop".into(),
            selector: "app=web".into(),
            title: "deploy/web".into(),
        };
        let streaming = LogState::Streaming {
            pods: 3,
            containers: 3,
        };
        assert_eq!(&*state_detail(&streaming, &workload).0, "3 pods");
        assert_eq!(&*state_detail(&streaming, &pod).0, "3 containers");
        let one = LogState::Streaming {
            pods: 1,
            containers: 1,
        };
        assert_eq!(&*state_detail(&one, &pod).0, "live");
        assert_eq!(&*state_detail(&one, &workload).0, "1 pod");
        assert_eq!(
            state_detail(&LogState::NoPods, &pod).0.as_ref(),
            "pod not found"
        );
        assert_eq!(
            state_detail(&LogState::Failed("x".into()), &pod).1,
            theme::RED
        );
        assert!(
            empty_text(&LogState::NoPods, &workload)
                .0
                .contains("app=web")
        );
    }
}
