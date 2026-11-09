//! The terminal entity: frames in, input out.
//!
//! Derived from Herdr (Apache-2.0).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::{
    App, ClipboardItem, Context, ElementId, EventEmitter, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, KeyUpEvent, Keystroke, KeystrokeEvent, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, ScrollEvent, SharedString, Subscription,
    Task, Window,
};

use crate::appearance::TerminalAppearance;
use crate::cursor::Blink;
use crate::engine::{ExitStatus, ForegroundProcess, TerminalSnapshot, TerminalStatus};
use crate::frame::{TerminalColors, TerminalFrame, Viewport};
use crate::geometry::Geometry;
use crate::host_theme::HostTheme;
use crate::input::{KeyAction, MouseAction, ScrollRequest, TerminalInput};
use crate::keys::{self, ScrollAccumulator};
use crate::links::{self, Link};
use crate::selection::{self, Cell, Selection};
use crate::source::{FrameHandle, FrameSink, FrameSource, StartOptions};

/// Events a host subscribes to.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TerminalEvent {
    /// The terminal took keyboard focus.
    Focused,
    /// The program changed the title; read it with `title`.
    TitleChanged,
    /// The program reported a new working directory; read it with `cwd`.
    CwdChanged,
    /// The program rang the bell.
    Bell,
    /// The program wrote to the clipboard through OSC 52.
    ClipboardWritten,
    /// The program asked for a desktop notification.
    Notification {
        /// The title, possibly empty.
        title: SharedString,
        /// The body.
        body: SharedString,
    },
    /// The program ended.
    Exited(ExitStatus),
    /// The terminal could not start or the engine failed.
    Failed(SharedString),
    /// The font size changed through the zoom actions or
    /// [`TerminalState::set_font_size`]; read it with `font_size`.
    FontSizeChanged,
    /// Another process took the foreground, such as an editor the shell
    /// started, or the shell back at its prompt; read it with
    /// `foreground`.
    ForegroundChanged,
}

/// Colors and appearance a terminal starts with.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct TerminalConfig {
    colors: TerminalColors,
    appearance: TerminalAppearance,
}

impl TerminalConfig {
    /// Set the colors.
    #[must_use]
    pub fn with_colors(mut self, colors: TerminalColors) -> Self {
        self.colors = colors;
        self
    }

    /// Set the appearance.
    #[must_use]
    pub fn with_appearance(mut self, appearance: TerminalAppearance) -> Self {
        self.appearance = appearance;
        self
    }

    /// The colors.
    pub fn colors(&self) -> &TerminalColors {
        &self.colors
    }

    /// The appearance.
    pub fn appearance(&self) -> &TerminalAppearance {
        &self.appearance
    }
}

/// A terminal: the latest frame, the program's status and everything the
/// element needs to draw and to send input.
#[expect(
    clippy::struct_excessive_bools,
    reason = "view flags that change independently: focus, hover, drag and the like"
)]
#[non_exhaustive]
pub struct TerminalState {
    handle: Option<Box<dyn FrameHandle>>,
    closed: bool,
    frame: Arc<TerminalFrame>,
    status: TerminalStatus,
    title: SharedString,
    cwd: Option<PathBuf>,
    foreground: Option<ForegroundProcess>,
    bell_count: u64,
    clipboard_sequence: u64,
    notification_sequence: u64,
    at_prompt: bool,
    colors: TerminalColors,
    appearance: TerminalAppearance,
    /// The size the font was zoomed to, over the appearance's.
    font_size: Option<Pixels>,
    focus: FocusHandle,
    focused: bool,
    visible: bool,
    pub(crate) geometry: Option<Geometry>,
    geometry_scale: Option<f32>,
    requested: Viewport,
    pub(crate) selection: Option<Selection>,
    pub(crate) marked_text: Option<String>,
    /// Rows prepared in the last frame, for the element to reuse.
    pub(crate) row_cache: crate::element::RowCache,
    pressed_keys: HashSet<String>,
    right_click_passthrough: bool,
    hovered_link: Option<(u16, Link)>,
    scroll: ScrollAccumulator,
    pub(crate) cursor: Blink,
    frames: Option<Task<()>>,
    _subscriptions: [Subscription; 3],
}

impl TerminalState {
    /// A terminal fed by `source`, started at once.
    pub fn new(
        source: impl FrameSource,
        config: TerminalConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let this = cx.weak_entity();
        let mut state = Self {
            handle: None,
            closed: false,
            frame: Arc::new(TerminalFrame::default()),
            status: TerminalStatus::Starting,
            title: SharedString::default(),
            cwd: None,
            foreground: None,
            bell_count: 0,
            clipboard_sequence: 0,
            notification_sequence: 0,
            at_prompt: false,
            colors: config.colors,
            appearance: config.appearance,
            font_size: None,
            focus: focus.clone(),
            focused: false,
            visible: true,
            geometry: None,
            geometry_scale: None,
            requested: Viewport::default(),
            selection: None,
            marked_text: None,
            row_cache: crate::element::RowCache::default(),
            pressed_keys: HashSet::new(),
            right_click_passthrough: true,
            hovered_link: None,
            scroll: ScrollAccumulator::default(),
            cursor: Blink::new(),
            frames: None,
            _subscriptions: [
                cx.on_focus_in(&focus, window, |state: &mut Self, _, cx| {
                    state.focus_changed(true, cx);
                }),
                cx.on_focus_out(&focus, window, |state: &mut Self, _, _, cx| {
                    state.focus_changed(false, cx);
                }),
                cx.intercept_keystrokes({
                    let state = this;
                    let focus = focus.clone();
                    move |event, window, cx| {
                        if !focus.is_focused(window) {
                            return;
                        }
                        if let Some(state) = state.upgrade() {
                            state.update(cx, |state, cx| state.claim_key(event, window, cx));
                        }
                    }
                }),
            ],
        };
        state.start(Box::new(source), cx);
        state
    }

    /// A terminal running a local program in a pty.
    pub fn local(
        options: crate::options::LocalTerminalOptions,
        config: TerminalConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let engine = crate::engine::Engine::new(
            crate::pty::LocalPty::new(options.clone()),
            options.engine.clone(),
        )
        .with_abnormal_exit_runtime(options.abnormal_exit_runtime)
        .with_input(options.input);
        Self::new(engine, config, window, cx)
    }

    fn start(&mut self, source: Box<dyn FrameSource>, cx: &mut Context<Self>) {
        let (sink, wake) = FrameSink::new();
        let options = StartOptions {
            viewport: self.requested,
            colors: self.colors.clone(),
            visible: self.visible,
            cursor: self
                .appearance
                .cursor_shape
                .map(|shape| (shape, self.appearance.cursor_blink.unwrap_or(true))),
        };
        match source.start(sink.clone(), options) {
            Ok(handle) => {
                self.handle = Some(handle);
                self.status = TerminalStatus::Starting;
            }
            Err(error) => {
                self.status = TerminalStatus::Failed(error.to_string());
                cx.emit(TerminalEvent::Failed(error.to_string().into()));
                return;
            }
        }
        self.frames = Some(cx.spawn(async move |state, cx| {
            while wake.recv().await.is_ok() {
                let Some(snapshot) = sink.take() else {
                    continue;
                };
                if state
                    .update(cx, |state, cx| state.apply(&snapshot, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    /// Install a snapshot. Public so a host driving its own frames can use
    /// the same path as the engine.
    pub fn apply(&mut self, snapshot: &TerminalSnapshot, cx: &mut Context<Self>) {
        let reflowed = snapshot.frame.content_revision != self.frame.content_revision;
        self.selection = self
            .selection
            .and_then(|selection| selection.refresh(&self.frame, &snapshot.frame));
        self.frame = Arc::clone(&snapshot.frame);
        self.at_prompt = snapshot.at_prompt;
        if snapshot.title != self.title.as_ref() {
            self.title = snapshot.title.clone().into();
            cx.emit(TerminalEvent::TitleChanged);
        }
        if snapshot.cwd != self.cwd {
            self.cwd.clone_from(&snapshot.cwd);
            cx.emit(TerminalEvent::CwdChanged);
        }
        if snapshot.foreground != self.foreground {
            let cwd = self.cwd().map(Path::to_path_buf);
            self.foreground.clone_from(&snapshot.foreground);
            cx.emit(TerminalEvent::ForegroundChanged);
            if self.cwd().map(Path::to_path_buf) != cwd {
                cx.emit(TerminalEvent::CwdChanged);
            }
        }
        if snapshot.bell_count != self.bell_count {
            self.bell_count = snapshot.bell_count;
            cx.emit(TerminalEvent::Bell);
        }
        if snapshot.clipboard_sequence != self.clipboard_sequence {
            self.clipboard_sequence = snapshot.clipboard_sequence;
            if let Some(text) = &snapshot.clipboard {
                cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                cx.emit(TerminalEvent::ClipboardWritten);
            }
        }
        if snapshot.notification_sequence != self.notification_sequence {
            self.notification_sequence = snapshot.notification_sequence;
            if let Some((title, body)) = &snapshot.notification {
                cx.emit(TerminalEvent::Notification {
                    title: title.to_string().into(),
                    body: body.to_string().into(),
                });
            }
        }
        if snapshot.status != self.status {
            self.status = snapshot.status.clone();
            match &self.status {
                TerminalStatus::Exited(status) => cx.emit(TerminalEvent::Exited(status.clone())),
                TerminalStatus::Failed(error) => {
                    cx.emit(TerminalEvent::Failed(error.clone().into()));
                }
                TerminalStatus::Starting | TerminalStatus::Live => {}
            }
            if !matches!(self.status, TerminalStatus::Live) {
                self.cursor.stop();
            }
        }
        if reflowed {
            self.hovered_link = None;
        }
        cx.notify();
    }

    /// The frame being drawn.
    #[must_use]
    pub fn frame(&self) -> &Arc<TerminalFrame> {
        &self.frame
    }

    /// The program's status.
    #[must_use]
    pub fn status(&self) -> &TerminalStatus {
        &self.status
    }

    /// The title the program set.
    #[must_use]
    pub fn title(&self) -> &SharedString {
        &self.title
    }

    /// The working directory: the one the shell reported through its
    /// integration (OSC 7), or else the foreground process's.
    #[must_use]
    pub fn cwd(&self) -> Option<&Path> {
        self.cwd
            .as_deref()
            .or_else(|| self.foreground.as_ref()?.cwd())
    }

    /// The process in the foreground, such as the shell at its prompt or an
    /// editor it started, when the source reports one. A local pty does.
    /// [`TerminalEvent::ForegroundChanged`] reports a change.
    #[must_use]
    pub fn foreground(&self) -> Option<&ForegroundProcess> {
        self.foreground.as_ref()
    }

    /// Whether closing the terminal would end a program other than the one
    /// it started, such as an editor or a build in the foreground, so an
    /// application should ask first, as Ghostty does. Without a foreground
    /// process, as from a stream, it is whether the cursor is away from a
    /// shell prompt.
    #[must_use]
    pub fn needs_confirm_close(&self) -> bool {
        if !matches!(self.status, TerminalStatus::Live) {
            return false;
        }
        match &self.foreground {
            Some(process) => !process.is_shell(),
            None => !self.at_prompt,
        }
    }

    /// Queued input bytes not yet delivered; the busy signal.
    #[must_use]
    pub fn input_backlog(&self) -> usize {
        self.handle.as_ref().map_or(0, |h| h.input_backlog())
    }

    /// The appearance.
    #[must_use]
    pub fn appearance(&self) -> &TerminalAppearance {
        &self.appearance
    }

    /// The font size the grid draws with: the zoomed size, else the
    /// appearance's, else the theme's code font size.
    pub fn font_size(&self, cx: &App) -> Pixels {
        self.font_size
            .unwrap_or_else(|| self.appearance.resolved(&HostTheme::of(cx)).size())
    }

    /// Draw the grid at `size`, clamped to 1-255 points as Ghostty clamps
    /// it. The grid is measured again at the next paint, which resizes
    /// the program's grid to fit. Emits [`TerminalEvent::FontSizeChanged`].
    pub fn set_font_size(&mut self, size: impl Into<Pixels>, cx: &mut Context<Self>) {
        let size = size.into().clamp(gpui_kit::px(1.), gpui_kit::px(255.));
        if self.font_size == Some(size) {
            return;
        }
        self.font_size = Some(size);
        cx.emit(TerminalEvent::FontSizeChanged);
        cx.notify();
    }

    /// Zoom the font in by `points`, as Ghostty's `increase_font_size`.
    pub fn increase_font_size(&mut self, points: impl Into<Pixels>, cx: &mut Context<Self>) {
        let size = self.font_size(cx) + points.into();
        self.set_font_size(size, cx);
    }

    /// Zoom the font out by `points`, as Ghostty's `decrease_font_size`.
    pub fn decrease_font_size(&mut self, points: impl Into<Pixels>, cx: &mut Context<Self>) {
        let size = self.font_size(cx) - points.into();
        self.set_font_size(size, cx);
    }

    /// Go back to the appearance's font size, or the theme's.
    pub fn reset_font_size(&mut self, cx: &mut Context<Self>) {
        if self.font_size.take().is_some() {
            cx.emit(TerminalEvent::FontSizeChanged);
            cx.notify();
        }
    }

    /// The appearance with the theme's values filled in and the zoom on top,
    /// as the grid draws with it.
    pub(crate) fn drawn_appearance(&self, theme: &HostTheme) -> TerminalAppearance {
        let mut appearance = self.appearance.resolved(theme);
        if let Some(size) = self.font_size {
            appearance.font_size = Some(size);
        }
        appearance
    }

    /// Change the appearance. The grid is measured again at the next paint.
    pub fn set_appearance(&mut self, appearance: TerminalAppearance, cx: &mut Context<Self>) {
        if self.appearance == appearance {
            return;
        }
        let cursor = appearance
            .cursor_shape
            .map(|shape| (shape, appearance.cursor_blink.unwrap_or(true)));
        if cursor
            != self
                .appearance
                .cursor_shape
                .map(|shape| (shape, self.appearance.cursor_blink.unwrap_or(true)))
            && let (Some(handle), Some((shape, blink))) = (&self.handle, cursor)
        {
            handle.set_cursor(shape, blink);
        }
        self.appearance = appearance;
        cx.notify();
    }

    /// The colors.
    #[must_use]
    pub fn colors(&self) -> &TerminalColors {
        &self.colors
    }

    /// Change the colors. The next frame carries them.
    pub fn set_colors(&mut self, colors: TerminalColors, cx: &mut Context<Self>) {
        if self.colors == colors {
            return;
        }
        if let Some(handle) = &self.handle {
            handle.set_colors(colors.clone());
        }
        self.colors = colors;
        cx.notify();
    }

    /// Whether a program is running.
    #[must_use]
    pub fn is_live(&self) -> bool {
        matches!(self.status, TerminalStatus::Live) && !self.closed
    }

    /// Whether the terminal was closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Whether the terminal is visible.
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Hidden terminals keep parsing output but stop producing frames and
    /// stop the cursor cadence. Revealing one asks for a current frame.
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if let Some(handle) = &self.handle {
            handle.set_visible(visible);
        }
        if !visible {
            self.cursor.stop();
        }
        cx.notify();
    }

    /// Stop the program and the source. Idempotent.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        self.closed = true;
        if let Some(handle) = &self.handle {
            handle.close();
        }
        self.selection = None;
        self.cursor.stop();
        cx.notify();
    }

    /// Whether a right click goes to a program that reports the mouse. It
    /// does by default. When it does not, or a modifier is held, or no
    /// program reports the mouse, the click is left to the host, such as for
    /// a context menu.
    pub fn set_right_click_passthrough(&mut self, passthrough: bool) {
        self.right_click_passthrough = passthrough;
    }

    fn leaves_right_click(&self, button: MouseButton, modifiers: Modifiers, _: &Window) -> bool {
        button == MouseButton::Right
            && (!self.right_click_passthrough
                || modifiers != Modifiers::default()
                || !self.reports_mouse(modifiers))
    }

    /// Whether a non-empty selection exists.
    #[must_use]
    pub fn has_selection(&self) -> bool {
        self.selection.is_some_and(|s| !s.is_empty())
    }

    /// The selected text as shown, without scrollback.
    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection.filter(|s| !s.is_empty())?;
        let text = selection.text(&self.frame);
        (!text.is_empty()).then_some(text)
    }

    /// The link under the pointer while the platform modifier is held.
    #[must_use]
    pub fn hovered_link(&self) -> Option<&str> {
        self.hovered_link
            .as_ref()
            .map(|(_, link)| link.url.as_str())
    }

    pub(crate) fn hovered_link_span(&self) -> Option<(u16, std::ops::Range<u16>)> {
        self.hovered_link
            .as_ref()
            .map(|(row, link)| (*row, link.columns.clone()))
    }

    /// Copy the selection to the clipboard. A live engine is asked for the
    /// text because it can reach scrollback the frame does not carry.
    pub fn copy(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.filter(|s| !s.is_empty()) else {
            return;
        };
        let trim = self.appearance.trim_trailing_spaces;
        if let Some(reply) = self
            .handle
            .as_ref()
            .and_then(|handle| handle.copy(selection.to_cells()))
        {
            cx.spawn(async move |state, cx| {
                if let Ok(Ok(text)) = reply.recv().await {
                    let _ = state.update(cx, |_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(finish(&text, trim)));
                    });
                }
            })
            .detach();
            return;
        }
        let text = selection.text(&self.frame);
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(finish(&text, trim)));
        }
    }

    /// Select the whole viewport.
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        let rows = self.frame.rows.len().min(usize::from(u16::MAX));
        let columns = self.frame.viewport.columns();
        if rows == 0 || columns == 0 {
            return;
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "`rows` comes from the grid, whose size is a u16"
        )]
        let last_row = (rows - 1) as u16;
        let mut selection = Selection::new(Cell::new(0, 0), self.frame.content_revision, false);
        selection.extend(Cell::new(columns, last_row));
        selection.finish();
        self.selection = Some(selection);
        cx.notify();
    }

    /// Drop the selection. Returns whether there was one.
    pub fn clear_selection(&mut self, cx: &mut Context<Self>) -> bool {
        if self.selection.take().is_some() {
            cx.notify();
            return true;
        }
        false
    }

    /// Send form feed, which clears the screen at most shells.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.selection = None;
        self.send(TerminalInput::Text("\u{c}".to_owned()), cx);
    }

    /// Paste the clipboard text.
    pub fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.paste_text(text, cx);
        }
    }

    /// Paste text, bracketed when the program asked for it.
    pub fn paste_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.selection = None;
        self.send(TerminalInput::Paste(text.into()), cx);
    }

    /// Send raw text to the program.
    pub fn send_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.send(TerminalInput::Text(text.into()), cx);
    }

    /// Scroll the viewport.
    pub fn scroll(&mut self, request: ScrollRequest, cx: &mut Context<Self>) {
        if let Some(handle) = &self.handle {
            let _ = handle.input(TerminalInput::Scroll(request));
        }
        cx.notify();
    }

    /// Scroll by whole pages; negative is toward older output.
    pub fn scroll_pages(&mut self, pages: isize, cx: &mut Context<Self>) {
        let rows = isize::try_from(self.frame.viewport.rows()).unwrap_or(24);
        self.scroll(ScrollRequest::Lines(pages.saturating_mul(rows)), cx);
    }

    /// Whether the viewport is scrolled into history.
    #[must_use]
    pub fn is_scrolled(&self) -> bool {
        !self.frame.scroll.at_bottom()
    }

    /// The focus handle.
    #[must_use]
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub(crate) fn send(&mut self, input: TerminalInput, cx: &mut Context<Self>) {
        let Some(handle) = &self.handle else {
            return;
        };
        let scroll = !matches!(input, TerminalInput::Scroll(_) | TerminalInput::Focus(_))
            && self.is_scrolled();
        let _ = handle.input(input);
        if scroll {
            let _ = handle.input(TerminalInput::Scroll(ScrollRequest::Bottom));
        }
        cx.notify();
    }

    /// Grant credit for the next frame after a real paint. Never notifies,
    /// or it would drive a repaint loop.
    pub(crate) fn frame_painted(&self, painted: bool) {
        if painted
            && self.visible
            && !self.closed
            && matches!(self.status, TerminalStatus::Live)
            && let Some(handle) = &self.handle
        {
            handle.request_frame();
        }
    }

    pub(crate) fn geometry_scale_value(&self) -> f32 {
        self.geometry_scale.unwrap_or(1.0)
    }

    /// Record geometry measured during paint and resize the source only
    /// when the grid or the physical cell size changed.
    pub(crate) fn set_geometry(&mut self, geometry: Geometry, scale: f32, cx: &mut Context<Self>) {
        if self.geometry == Some(geometry) && self.geometry_scale == Some(scale) {
            return;
        }
        self.geometry = Some(geometry);
        self.geometry_scale = Some(scale);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a cell is a few dozen positive pixels, rounded"
        )]
        let viewport = Viewport::new(
            geometry.columns,
            geometry.rows,
            (f32::from(geometry.cell.width) * scale).round().max(1.0) as u32,
            (f32::from(geometry.cell.height) * scale).round().max(1.0) as u32,
        );
        let Some(viewport) = viewport else {
            return;
        };
        if self.requested == viewport {
            return;
        }
        self.requested = viewport;
        if let Some(handle) = &self.handle {
            handle.resize(viewport);
        }
        cx.notify();
    }

    fn focus_changed(&mut self, focused: bool, cx: &mut Context<Self>) {
        if self.focused == focused {
            return;
        }
        self.focused = focused;
        self.send(TerminalInput::Focus(focused), cx);
        if focused {
            cx.emit(TerminalEvent::Focused);
        } else {
            self.pressed_keys.clear();
            self.cursor.stop();
        }
        cx.notify();
    }

    /// Keys the terminal consumes stop propagation, which is what prevents
    /// printable text arriving twice: once as a key and again as an IME
    /// commit.
    pub(crate) fn on_key_down(
        &mut self,
        event: &KeyDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.marked_text.is_some() {
            return;
        }
        let keystroke = &event.keystroke;
        if keystroke.key == "escape"
            && keystroke.modifiers == Modifiers::default()
            && self.clear_selection(cx)
        {
            cx.stop_propagation();
            return;
        }
        if let Some(key) = keys::key(keystroke, keys::key_action(event.is_held)) {
            self.selection = None;
            self.pressed_keys.insert(keystroke.key.clone());
            self.send(TerminalInput::Key(key), cx);
            cx.stop_propagation();
        }
    }

    pub(crate) fn send_keystroke(
        &mut self,
        keystroke: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(keystroke) = Keystroke::parse_with_os(keystroke, cx.platform_info().os) else {
            return;
        };
        let event = KeyDownEvent {
            keystroke,
            is_held: false,
            prefer_character_input: false,
        };
        self.on_key_down(&event, window, cx);
    }

    /// Takes a key from a binding outside the terminal that would keep it
    /// from the program, before the binding runs. The terminal is focused.
    fn claim_key(&mut self, event: &KeystrokeEvent, window: &mut Window, cx: &mut Context<Self>) {
        let winner = cx
            .key_bindings()
            .borrow()
            .bindings_for_input(std::slice::from_ref(&event.keystroke), &event.context_stack)
            .0
            .into_iter()
            .next();
        let Some(binding) = winner else {
            return;
        };
        match keys::claim(binding.action().name(), &event.keystroke) {
            keys::Claim::None => {}
            keys::Claim::Send => {
                let key = KeyDownEvent {
                    keystroke: event.keystroke.clone(),
                    is_held: false,
                    prefer_character_input: false,
                };
                self.on_key_down(&key, window, cx);
                cx.stop_propagation();
            }
            keys::Claim::Copy => {
                if self.has_selection() {
                    self.copy(cx);
                    cx.stop_propagation();
                }
            }
        }
    }

    pub(crate) fn on_key_up(&mut self, event: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.pressed_keys.remove(&event.keystroke.key) || self.marked_text.is_some() {
            return;
        }
        if let Some(key) = keys::key(&event.keystroke, KeyAction::Release) {
            self.send(TerminalInput::Key(key), cx);
        }
    }

    pub(crate) fn on_scroll(
        &mut self,
        event: &ScrollEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The terminal scrolls its own content, so the wheel step is its
        // own even at the end of the scrollback: the page around it must
        // not scroll as well.
        cx.stop_propagation();
        let Some(geometry) = self.geometry else {
            return;
        };
        let delta = event.delta.pixel_delta(geometry.cell.height).y / geometry.cell.height;
        if self.reports_mouse(event.modifiers) {
            self.scroll.reset();
            let action = if delta > 0.0 {
                MouseAction::ScrollUp
            } else {
                MouseAction::ScrollDown
            };
            if delta != 0.0 {
                self.report_mouse(action, None, event.modifiers, event.position, window, cx);
            }
            return;
        }
        if self.frame.modes.alternate_screen && self.frame.modes.alternate_scroll {
            // Mode 1007: the wheel becomes arrow keys on the alternate screen.
            if let Some(ScrollRequest::Lines(lines)) = self.scroll.push(delta) {
                let key = if lines < 0 { "up" } else { "down" };
                for _ in 0..lines.unsigned_abs().min(64) {
                    self.send_keystroke(key, window, cx);
                }
            }
            return;
        }
        if let Some(request) = self.scroll.push(delta) {
            self.scroll(request, cx);
        }
    }

    /// Update the hovered link. A link is live only while the platform
    /// modifier is held, so dragging a selection across a URL opens nothing.
    pub(crate) fn hover_link(
        &mut self,
        position: Point<Pixels>,
        active: bool,
        cx: &mut Context<Self>,
    ) {
        let found = active
            .then(|| {
                let geometry = self.geometry?;
                if !geometry.bounds.contains(&position) {
                    return None;
                }
                let cell = geometry.cell_at(position);
                let row = self.frame.rows.get(usize::from(cell.row))?;
                links::at(row, cell.column).map(|link| (cell.row, link))
            })
            .flatten();
        if found != self.hovered_link {
            self.hovered_link = found;
            cx.notify();
        }
    }

    fn reports_mouse(&self, modifiers: Modifiers) -> bool {
        keys::reports_mouse(self.frame.modes.mouse_reporting, modifiers)
    }

    fn report_mouse(
        &mut self,
        action: MouseAction,
        button: Option<MouseButton>,
        modifiers: Modifiers,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(geometry) = self.geometry else {
            return;
        };
        let scale = window.scale_factor();
        let local = position - geometry.origin;
        let mouse = keys::mouse(
            action,
            button.and_then(keys::mouse_button),
            modifiers,
            f32::from(local.x) * scale,
            f32::from(local.y) * scale,
        );
        self.send(TerminalInput::Mouse(mouse), cx);
    }

    pub(crate) fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        if self.leaves_right_click(event.button, event.modifiers, window) {
            return;
        }
        if event.button == MouseButton::Left && event.modifiers.platform {
            self.hover_link(event.position, true, cx);
            if let Some((_, link)) = self.hovered_link.clone() {
                self.selection = None;
                cx.open_url(&link.url);
                cx.stop_propagation();
                return;
            }
        }
        if self.reports_mouse(event.modifiers) {
            if self.selection.take().is_some() {
                cx.notify();
            }
            self.report_mouse(
                MouseAction::Press,
                Some(event.button),
                event.modifiers,
                event.position,
                window,
                cx,
            );
            cx.stop_propagation();
            return;
        }
        if event.button == MouseButton::Left
            && let Some(geometry) = self.geometry
        {
            self.selection = Some(Selection::new(
                geometry.selection_boundary_at(event.position),
                self.frame.content_revision,
                event.modifiers.alt,
            ));
            cx.notify();
        }
    }

    pub(crate) fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.reports_mouse(event.modifiers) {
            self.report_mouse(
                MouseAction::Move,
                event.pressed_button,
                event.modifiers,
                event.position,
                window,
                cx,
            );
        }
    }

    pub(crate) fn drag_selection(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let (Some(geometry), Some(selection)) = (self.geometry, self.selection.as_mut()) else {
            return;
        };
        if !selection.is_dragging() {
            return;
        }
        selection.extend(geometry.selection_boundary_at(position));
        cx.notify();
    }

    pub(crate) fn finish_selection(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if !self.selection.is_some_and(Selection::is_dragging) {
            return;
        }
        self.drag_selection(position, cx);
        if let Some(selection) = self.selection.as_mut() {
            selection.finish();
        }
        if self.appearance.copy_on_select && self.has_selection() {
            self.copy(cx);
        }
    }

    pub(crate) fn on_mouse_up(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.leaves_right_click(event.button, event.modifiers, window) {
            return;
        }
        if self.reports_mouse(event.modifiers) {
            self.report_mouse(
                MouseAction::Release,
                Some(event.button),
                event.modifiers,
                event.position,
                window,
                cx,
            );
        }
    }

    /// The message shown over the grid when the program is not running.
    pub(crate) fn status_message(&self) -> Option<SharedString> {
        match &self.status {
            TerminalStatus::Failed(error) => Some(error.clone().into()),
            TerminalStatus::Exited(status) => Some(match (status.code, &status.signal) {
                (Some(0), None) => "Process exited".into(),
                (Some(code), None) => format!("Process exited with code {code}").into(),
                (_, Some(signal)) => format!("Process ended by {signal}").into(),
                (None, None) => "Process ended".into(),
            }),
            TerminalStatus::Starting | TerminalStatus::Live => None,
        }
    }
}

fn finish(text: &str, trim: bool) -> String {
    if trim {
        selection::trim_lines(text)
    } else {
        text.to_owned()
    }
}

impl EventEmitter<TerminalEvent> for TerminalState {}

impl Focusable for TerminalState {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalState {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::element::Terminal::new(
            ElementId::NamedChild(ElementId::View(cx.entity_id()).into(), "terminal".into()),
            &cx.entity(),
        )
    }
}
