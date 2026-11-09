//! The Ghostty terminal, render state and encoders owned by one thread.
//!
//! `Core` turns bytes into [`TerminalFrame`]s and typed input into the bytes
//! the program receives. It never blocks and never talks to the UI; the
//! owner thread in `engine.rs` drives it.
//!
//! Derived from Herdr (Apache-2.0).

use std::cell::Cell;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ghostty_vt as vt;
use ghostty_vt::render::{CellIterator, Dirty, RenderState, RowIterator};
use ghostty_vt::screen::{CellContentTag, CellWide, Screen};
use ghostty_vt::selection::{FormatOptions, Selection};
use ghostty_vt::style::StyleColor;
use ghostty_vt::terminal::{Event, Mode, Point, PointCoordinate, ScrollViewport};
use ghostty_vt::{key, mouse};

use crate::frame::{
    CellSelection, CellSpan, Color, Cursor, CursorShape, InputModes, Rgb, ScrollMetrics, Style,
    StyleRun, TerminalColors, TerminalFrame, TerminalRow, Underline, Viewport,
};
use crate::input::{KeyAction, Modifiers, MouseAction, MouseButton, ScrollRequest, TerminalInput};
use crate::options::EngineOptions;

/// How long a program may hold synchronized output before the hold is
/// broken. Ghostty uses the same limit.
const RENDER_HOLD_TIMEOUT: Duration = Duration::from_secs(1);

/// Side effects gathered while parsing, for the owner thread to forward.
#[derive(Debug, Default)]
pub(crate) struct Effects {
    /// Bytes the terminal wants written to the pty.
    pub replies: Vec<u8>,
    /// Bells received.
    pub bells: u64,
    /// Whether the title changed.
    pub title_changed: bool,
    /// Whether the working directory changed.
    pub cwd_changed: bool,
    /// Clipboard writes, oldest first.
    pub clipboard: Vec<String>,
    /// Desktop notifications, oldest first.
    pub notifications: Vec<(String, String)>,
}

pub(crate) struct Core {
    terminal: vt::Terminal,
    render: RenderState,
    /// Set by the render hold callback while synchronized output is held.
    held: Rc<Cell<bool>>,
    hold_started: Option<Instant>,
    rows: RowIterator,
    cells: CellIterator,
    key_encoder: key::Encoder,
    mouse_encoder: mouse::Encoder,
    held_buttons: Vec<MouseButton>,
    viewport: Viewport,
    last: Option<Arc<TerminalFrame>>,
    revision: u64,
    content_revision: u64,
    colors: TerminalColors,
    /// The default cursor the host set, which a parked snapshot does not
    /// carry.
    cursor: Option<(CursorShape, bool)>,
    title: String,
    cwd: Option<PathBuf>,
    scratch: String,
}

/// The largest unfinished escape sequence a parked snapshot keeps: 64 KiB,
/// enough for any OSC or DCS a shell writes between two reads.
const PARK_CONTINUATION_BYTES: usize = 64 << 10;

/// What a parked core keeps beside its snapshot.
#[derive(Debug)]
pub(crate) struct Parked {
    viewport: Viewport,
    colors: TerminalColors,
    cursor: Option<(CursorShape, bool)>,
    title: String,
    cwd: Option<PathBuf>,
    revision: u64,
    content_revision: u64,
}

fn fault(error: vt::Error) -> io::Error {
    io::Error::other(error.to_string())
}

fn rgb(color: Rgb) -> vt::style::RgbColor {
    vt::style::RgbColor {
        r: color.0,
        g: color.1,
        b: color.2,
    }
}

fn from_rgb(color: vt::style::RgbColor) -> Rgb {
    Rgb(color.r, color.g, color.b)
}

fn color(value: StyleColor) -> Color {
    match value {
        StyleColor::None => Color::Default,
        StyleColor::Palette(index) => Color::Indexed(index.0),
        StyleColor::Rgb(c) => Color::Rgb(from_rgb(c)),
    }
}

fn style(value: vt::style::Style) -> Style {
    Style {
        foreground: color(value.fg_color),
        background: color(value.bg_color),
        underline_color: color(value.underline_color),
        bold: value.bold,
        italic: value.italic,
        faint: value.faint,
        blink: value.blink,
        inverse: value.inverse,
        invisible: value.invisible,
        underline: match value.underline {
            vt::style::Underline::None => Underline::None,
            vt::style::Underline::Double => Underline::Double,
            vt::style::Underline::Curly => Underline::Curly,
            vt::style::Underline::Dotted => Underline::Dotted,
            vt::style::Underline::Dashed => Underline::Dashed,
            _ => Underline::Single,
        },
        strikethrough: value.strikethrough,
        overline: value.overline,
    }
}

fn mods(value: Modifiers) -> key::Mods {
    let mut mods = key::Mods::empty();
    mods.set(key::Mods::SHIFT, value.shift);
    mods.set(key::Mods::CTRL, value.control);
    mods.set(key::Mods::ALT, value.alt);
    mods.set(key::Mods::SUPER, value.super_key);
    mods
}

impl Core {
    pub(crate) fn new(
        viewport: Viewport,
        options: &EngineOptions,
        colors: TerminalColors,
    ) -> io::Result<Self> {
        let mut terminal = vt::Terminal::new(vt::TerminalOptions {
            cols: viewport.columns(),
            rows: viewport.rows(),
            max_scrollback: options.scrollback_bytes,
        })
        .map_err(fault)?;
        // The C API starts with prompt redraw off (c/terminal.zig), so a
        // resize keeps the old prompt and the shell's redraw stacks under it.
        // The Ghostty app keeps Terminal.init's default of on. OSC 133
        // `redraw=1` is the only way to set it; the `C` that follows puts
        // the cursor back in output, so a shell without marks loses nothing.
        terminal.vt_write(b"\x1b]133;A;redraw=1\x07\x1b]133;C\x07");
        Self::around(terminal, viewport, options, colors)
    }

    /// A core around `terminal`, configured from `options` and `colors`, as
    /// a new terminal or one restored from a parked snapshot.
    fn around(
        mut terminal: vt::Terminal,
        viewport: Viewport,
        options: &EngineOptions,
        colors: TerminalColors,
    ) -> io::Result<Self> {
        terminal
            .resize(
                viewport.columns(),
                viewport.rows(),
                viewport.cell_width(),
                viewport.cell_height(),
            )
            .map_err(fault)?;
        terminal
            .set_default_fg_color(Some(rgb(colors.foreground)))
            .map_err(fault)?
            .set_default_bg_color(Some(rgb(colors.background)))
            .map_err(fault)?
            .set_default_cursor_color(colors.cursor.map(rgb))
            .map_err(fault)?
            .set_default_color_palette(Some(vt::style::Palette(colors.palette.map(rgb))))
            .map_err(fault)?
            .set_scrollback_max_bytes(Some(options.scrollback_bytes))
            .map_err(fault)?
            .set_scrollback_max_lines(options.scrollback_lines)
            .map_err(fault)?
            .set_clipboard_write_max_bytes(Some(options.clipboard_write_max_bytes))
            .map_err(fault)?
            .set_terminfo_name(Some(&options.terminfo_name))
            .map_err(fault)?;
        terminal.set_xtversion(Some(format!(
            "ghostty {}",
            vt::build_info::pinned_version()
        )));
        // A terminal parked in the middle of an escape sequence keeps the
        // unfinished bytes in its snapshot, so its parser resumes as if it
        // never stopped.
        terminal
            .set_continuation_max_bytes(PARK_CONTINUATION_BYTES)
            .map_err(fault)?;

        // The last published frame is the one the program wants left on
        // screen, so a hold only has to stop frames until it ends.
        let held = Rc::new(Cell::new(false));
        {
            let held = Rc::clone(&held);
            terminal.set_render_hold(Some(Box::new(move |_, hold| held.set(hold))));
        }
        let render = RenderState::new().map_err(fault)?;

        Ok(Self {
            terminal,
            render,
            held,
            hold_started: None,
            rows: RowIterator::new().map_err(fault)?,
            cells: CellIterator::new().map_err(fault)?,
            key_encoder: key::Encoder::new().map_err(fault)?,
            mouse_encoder: mouse::Encoder::new().map_err(fault)?,
            held_buttons: Vec::new(),
            viewport,
            last: None,
            revision: 0,
            content_revision: 0,
            colors,
            cursor: None,
            title: String::new(),
            cwd: None,
            scratch: String::new(),
        })
    }

    /// Encodes the whole terminal, scrollback included, and keeps what the
    /// snapshot does not carry, so the core can be dropped and rebuilt
    /// later with [`Core::restore`].
    pub(crate) fn park(&self) -> io::Result<(Vec<u8>, Parked)> {
        let bytes = vt::snapshot::encode(&self.terminal)
            .map_err(fault)?
            .to_vec();
        Ok((
            bytes,
            Parked {
                viewport: self.viewport,
                colors: self.colors.clone(),
                cursor: self.cursor,
                title: self.title.clone(),
                cwd: self.cwd.clone(),
                revision: self.revision,
                content_revision: self.content_revision,
            },
        ))
    }

    /// Rebuilds a parked core from its snapshot. The first frame after it
    /// is built in full.
    pub(crate) fn restore(
        bytes: &[u8],
        parked: Parked,
        options: &EngineOptions,
    ) -> io::Result<Self> {
        let mut decoder = vt::snapshot::Decoder::from_slice(bytes);
        decoder
            .set_max_continuation_bytes(PARK_CONTINUATION_BYTES)
            .map_err(fault)?
            .set_retain_continuation(true)
            .map_err(fault)?;
        let terminal = decoder.decode().map_err(fault)?;
        let mut core = Self::around(terminal, parked.viewport, options, parked.colors)?;
        if let Some((shape, blinking)) = parked.cursor {
            core.set_cursor(shape, blinking)?;
        }
        core.title = parked.title;
        core.cwd = parked.cwd;
        core.revision = parked.revision;
        // The rows are rebuilt, so a selection anchored to them is too.
        core.content_revision = parked.content_revision + 1;
        Ok(core)
    }

    /// Replace the default colors. The next frame carries them.
    pub(crate) fn set_colors(&mut self, colors: TerminalColors) -> io::Result<()> {
        self.terminal
            .set_default_fg_color(Some(rgb(colors.foreground)))
            .map_err(fault)?
            .set_default_bg_color(Some(rgb(colors.background)))
            .map_err(fault)?
            .set_default_cursor_color(colors.cursor.map(rgb))
            .map_err(fault)?
            .set_default_color_palette(Some(vt::style::Palette(colors.palette.map(rgb))))
            .map_err(fault)?;
        self.colors = colors;
        self.last = None;
        Ok(())
    }

    pub(crate) fn set_cursor(&mut self, shape: CursorShape, blinking: bool) -> io::Result<()> {
        let style = match shape {
            CursorShape::Block | CursorShape::Hollow => vt::terminal::CursorStyle::Block,
            CursorShape::Bar => vt::terminal::CursorStyle::Bar,
            CursorShape::Underline => vt::terminal::CursorStyle::Underline,
        };
        self.terminal
            .set_default_cursor_style(Some(style))
            .map_err(fault)?
            .set_default_cursor_blink(Some(blinking))
            .map_err(fault)?;
        self.cursor = Some((shape, blinking));
        Ok(())
    }

    /// Parse output and collect the side effects.
    pub(crate) fn write(&mut self, bytes: &[u8], effects: &mut Effects) {
        self.terminal.vt_write(bytes);
        self.drain(effects);
    }

    fn drain(&mut self, effects: &mut Effects) {
        for event in self.terminal.events() {
            match event {
                Event::PtyWrite(bytes) => effects.replies.extend_from_slice(&bytes),
                Event::Bell => effects.bells += 1,
                Event::TitleChanged(title) => {
                    if self.title != title {
                        self.title = title;
                        effects.title_changed = true;
                    }
                }
                Event::PwdChanged(value) => {
                    let cwd = reported_cwd(&value);
                    if cwd != self.cwd {
                        self.cwd = cwd;
                        effects.cwd_changed = true;
                    }
                }
                Event::ClipboardWrite(write) => {
                    if let Some(text) = write
                        .contents
                        .iter()
                        .find(|c| c.mime.starts_with("text/"))
                        .and_then(|c| String::from_utf8(c.data.clone()).ok())
                    {
                        effects.clipboard.push(text);
                    }
                }
                Event::DesktopNotification { title, body } => {
                    effects.notifications.push((title, body));
                }
                Event::RenderHold(true) => self.hold_started = Some(Instant::now()),
                Event::RenderHold(false) => self.hold_started = None,
                Event::ProgressReport { .. } => {}
            }
        }
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn cwd(&self) -> Option<&PathBuf> {
        self.cwd.as_ref()
    }

    pub(crate) fn resize(&mut self, viewport: Viewport, effects: &mut Effects) -> io::Result<()> {
        if self.viewport != viewport {
            self.terminal
                .resize(
                    viewport.columns(),
                    viewport.rows(),
                    viewport.cell_width(),
                    viewport.cell_height(),
                )
                .map_err(fault)?;
            self.viewport = viewport;
            self.last = None;
            self.drain(effects);
        }
        Ok(())
    }

    /// Whether a render hold is active. Breaks the hold after the timeout.
    pub(crate) fn held(&mut self) -> bool {
        if !self.held.get() {
            return false;
        }
        if self
            .hold_started
            .is_some_and(|started| started.elapsed() >= RENDER_HOLD_TIMEOUT)
        {
            let _ = self.terminal.set_mode(Mode::SYNC_OUTPUT, false);
            self.held.set(false);
            self.hold_started = None;
            return false;
        }
        true
    }

    /// Time until the render hold watchdog fires, when a hold is active.
    pub(crate) fn hold_remaining(&self) -> Option<Duration> {
        self.hold_started
            .filter(|_| self.held.get())
            .map(|started| RENDER_HOLD_TIMEOUT.saturating_sub(started.elapsed()))
    }

    fn mode(&self, mode: Mode) -> bool {
        self.terminal.mode(mode).unwrap_or(false)
    }

    /// Encode input into the bytes the program receives.
    pub(crate) fn input(&mut self, input: TerminalInput) -> io::Result<Vec<u8>> {
        let bytes = match input {
            TerminalInput::Text(text) => text.into_bytes(),
            TerminalInput::Paste(text) => {
                let bracketed = self.mode(Mode::BRACKETED_PASTE);
                let mut data = text.into_bytes();
                let mut out = vec![0u8; data.len() + 16];
                let written = match vt::paste::encode(&mut data, bracketed, &mut out) {
                    Ok(n) => n,
                    Err(vt::Error::OutOfSpace { required }) => {
                        out.resize(required, 0);
                        vt::paste::encode(&mut data, bracketed, &mut out).map_err(fault)?
                    }
                    Err(e) => return Err(fault(e)),
                };
                out.truncate(written);
                out
            }
            TerminalInput::Key(key)
                if matches!(key.key.as_str(), "enter" | "return")
                    && key.modifiers.shift
                    && !key.modifiers.control
                    && !key.modifiers.alt
                    && !key.modifiers.super_key =>
            {
                if key.action == KeyAction::Release {
                    Vec::new()
                } else {
                    vec![b'\n']
                }
            }
            TerminalInput::Key(input) => {
                let mut event = key::Event::new().map_err(fault)?;
                event.set_action(match input.action {
                    KeyAction::Press => key::Action::Press,
                    KeyAction::Repeat => key::Action::Repeat,
                    KeyAction::Release => key::Action::Release,
                });
                event.set_key(key_code(&input.key));
                event.set_mods(mods(input.modifiers));
                let single = input.key.chars().count() == 1;
                match &input.text {
                    Some(text) => event.set_utf8(Some(text.as_str())),
                    None if single => event.set_utf8(Some(input.key.as_str())),
                    None => event.set_utf8(None::<&str>),
                };
                if let Some(ch) = input.key.chars().next().filter(|_| single) {
                    event.set_unshifted_codepoint(ch);
                }
                self.key_encoder.set_options_from_terminal(&self.terminal);
                let mut out = Vec::new();
                self.key_encoder
                    .encode_to_vec(&event, &mut out)
                    .map_err(fault)?;
                out
            }
            TerminalInput::Focus(focused) => {
                if self.mode(Mode::FOCUS_EVENT) {
                    let event = if focused {
                        vt::focus::Event::Gained
                    } else {
                        vt::focus::Event::Lost
                    };
                    let mut buf = [0u8; 8];
                    let n = event.encode(&mut buf).map_err(fault)?;
                    buf[..n].to_vec()
                } else {
                    Vec::new()
                }
            }
            TerminalInput::Scroll(request) => {
                self.terminal.scroll_viewport(match request {
                    ScrollRequest::Lines(n) => ScrollViewport::Delta(n),
                    ScrollRequest::Top => ScrollViewport::Top,
                    ScrollRequest::Bottom => ScrollViewport::Bottom,
                });
                return Ok(Vec::new());
            }
            TerminalInput::Mouse(input) => {
                if !input.x.is_finite() || !input.y.is_finite() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "invalid mouse position",
                    ));
                }
                if !self.terminal.is_mouse_tracking().unwrap_or(false) {
                    self.held_buttons.clear();
                    return Ok(Vec::new());
                }
                if let Some(button) = input.button {
                    match input.action {
                        MouseAction::Press if !self.held_buttons.contains(&button) => {
                            self.held_buttons.push(button);
                        }
                        MouseAction::Release => self.held_buttons.retain(|held| *held != button),
                        _ => {}
                    }
                }
                let viewport = self.viewport;
                self.mouse_encoder
                    .set_options_from_terminal(&self.terminal)
                    .set_any_button_pressed(!self.held_buttons.is_empty())
                    .set_size(mouse::EncoderSize {
                        screen_width: u32::from(viewport.columns()) * viewport.cell_width(),
                        screen_height: u32::from(viewport.rows()) * viewport.cell_height(),
                        cell_width: viewport.cell_width(),
                        cell_height: viewport.cell_height(),
                        padding_top: 0,
                        padding_bottom: 0,
                        padding_right: 0,
                        padding_left: 0,
                    });
                let mut event = mouse::Event::new().map_err(fault)?;
                event.set_action(match input.action {
                    MouseAction::Press | MouseAction::ScrollUp | MouseAction::ScrollDown => {
                        mouse::Action::Press
                    }
                    MouseAction::Release => mouse::Action::Release,
                    MouseAction::Move => mouse::Action::Motion,
                });
                event.set_button(match input.action {
                    MouseAction::ScrollUp => Some(mouse::Button::Four),
                    MouseAction::ScrollDown => Some(mouse::Button::Five),
                    _ => input.button.map(|b| match b {
                        MouseButton::Left => mouse::Button::Left,
                        MouseButton::Middle => mouse::Button::Middle,
                        MouseButton::Right => mouse::Button::Right,
                    }),
                });
                event.set_mods(mods(input.modifiers));
                event.set_position(mouse::Position {
                    x: input.x.max(0.0),
                    y: input.y.max(0.0),
                });
                let mut out = Vec::new();
                self.mouse_encoder
                    .encode_to_vec(&event, &mut out)
                    .map_err(fault)?;
                out
            }
        };
        if !bytes.is_empty() {
            self.terminal.scroll_viewport(ScrollViewport::Bottom);
        }
        Ok(bytes)
    }

    /// The selected text, with trailing whitespace trimmed per line.
    pub(crate) fn copy(&self, selection: CellSelection) -> io::Result<String> {
        let point = |(x, y): (u16, u16)| {
            self.terminal
                .grid_ref(Point::Viewport(PointCoordinate { x, y: u32::from(y) }))
                .map_err(fault)
        };
        let selection = Selection::new(
            point(selection.start)?,
            point(selection.end)?,
            selection.rectangle,
        );
        let bytes = self
            .terminal
            .format_selection_alloc(
                FormatOptions::new()
                    .with_selection(&selection)
                    .with_emit_format(vt::fmt::Format::Plain)
                    .with_unwrap(true)
                    .with_trim(true),
            )
            .map_err(fault)?;
        Ok(bytes.map_or_else(String::new, |b| String::from_utf8_lossy(&b).into_owned()))
    }

    /// Whether the cursor sits at a shell prompt (OSC 133).
    pub(crate) fn at_prompt(&self) -> bool {
        self.terminal.is_cursor_at_prompt().unwrap_or(false)
    }

    /// Run one bounded compression step; true when nothing more is pending.
    pub(crate) fn compress(&mut self) -> bool {
        !matches!(
            self.terminal
                .compress(vt::terminal::CompressionMode::Incremental),
            Ok(vt::terminal::CompressionResult::Pending)
        )
    }

    /// Build a frame when something changed, or always when `force` is set.
    ///
    /// During a render hold the frame captured when the hold began is used.
    pub(crate) fn frame(&mut self, force: bool) -> io::Result<Option<Arc<TerminalFrame>>> {
        if self.held() {
            return Ok(None);
        }
        let bracketed_paste = self.mode(Mode::BRACKETED_PASTE);
        let focus_reporting = self.mode(Mode::FOCUS_EVENT);
        let alternate_scroll = self.mode(Mode::ALT_SCROLL);
        let snapshot = self.render.update(&self.terminal).map_err(fault)?;
        let dirty = snapshot.dirty().map_err(fault)?;
        let raw_cursor = snapshot.cursor().map_err(fault)?;
        let cursor = Cursor {
            column: raw_cursor.viewport.map_or(0, |p| p.x),
            row: raw_cursor.viewport.map_or(0, |p| p.y),
            visible: raw_cursor.visible && raw_cursor.viewport.is_some(),
            blinking: raw_cursor.blinking,
            shape: match raw_cursor.visual_style {
                vt::render::CursorVisualStyle::Bar => CursorShape::Bar,
                vt::render::CursorVisualStyle::Underline => CursorShape::Underline,
                vt::render::CursorVisualStyle::BlockHollow => CursorShape::Hollow,
                _ => CursorShape::Block,
            },
        };
        let raw_colors = snapshot.colors().map_err(fault)?;
        let mut colors = self.colors.clone();
        colors.foreground = from_rgb(raw_colors.foreground);
        colors.background = from_rgb(raw_colors.background);
        colors.palette = raw_colors.palette.map(from_rgb);
        colors.cursor = raw_colors.cursor.map(from_rgb).or(colors.cursor);
        let raw_scroll = self.terminal.scrollbar().map_err(fault)?;
        let scroll = ScrollMetrics {
            total_rows: usize::try_from(raw_scroll.total).unwrap_or(usize::MAX),
            offset: usize::try_from(raw_scroll.offset).unwrap_or(usize::MAX),
            viewport_rows: usize::try_from(raw_scroll.len).unwrap_or(usize::MAX),
        };
        let modes = InputModes {
            mouse_reporting: self.terminal.is_mouse_tracking().unwrap_or(false),
            alternate_screen: self.terminal.active_screen().ok() == Some(Screen::Alternate),
            bracketed_paste,
            focus_reporting,
            alternate_scroll,
        };
        let unchanged_state = self.last.as_ref().is_some_and(|f| {
            f.cursor == cursor && f.colors == colors && f.scroll == scroll && f.modes == modes
        });
        if !force && dirty == Dirty::Clean && unchanged_state {
            return Ok(None);
        }

        let all = self.last.is_none() || dirty == Dirty::Full;
        let mut rows = self.last.as_ref().map_or_else(
            || {
                let empty = Arc::new(TerminalRow::default());
                vec![empty; usize::from(self.viewport.rows())]
            },
            |f| f.rows.to_vec(),
        );
        self.revision += 1;
        let mut row_iter = self.rows.update(&snapshot).map_err(fault)?;
        let mut index = 0usize;
        while let Some(row) = row_iter.next() {
            if index >= rows.len() {
                break;
            }
            let dirty = all || row.dirty().map_err(fault)?;
            let id = row.id().map_err(fault)?;
            if dirty || rows[index].id != id {
                let soft_wrapped = row.raw_row().map_err(fault)?.is_wrapped().map_err(fault)?;
                let mut text = String::new();
                let mut spans = Vec::new();
                let mut runs: Vec<StyleRun> = Vec::new();
                let mut column = 0u16;
                let mut cells = self.cells.update(row).map_err(fault)?;
                while let Some(cell) = cells.next() {
                    let raw = cell.raw_cell().map_err(fault)?;
                    let wide = raw.wide().map_err(fault)?;
                    let spacer = matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead);
                    let width = if spacer {
                        0
                    } else if wide == CellWide::Wide {
                        2
                    } else {
                        1
                    };
                    let start = text.len();
                    if !spacer {
                        self.scratch.clear();
                        cell.graphemes_utf8(&mut self.scratch).map_err(fault)?;
                        if self.scratch.is_empty() {
                            text.push(' ');
                        } else {
                            text.push_str(&self.scratch);
                        }
                    }
                    let end = text.len();
                    let hyperlink = if raw.has_hyperlink().map_err(fault)? {
                        hyperlink(&self.terminal, column, index)
                    } else {
                        None
                    };
                    spans.push(CellSpan {
                        text: start..end,
                        column,
                        width,
                        spacer,
                        hyperlink,
                    });
                    if !spacer {
                        let mut cell_style = style(cell.style().map_err(fault)?);
                        if cell_style.background == Color::Default {
                            cell_style.background = match raw.content_tag().map_err(fault)? {
                                CellContentTag::BgColorPalette => {
                                    Color::Indexed(raw.bg_color_palette().map_err(fault)?.0)
                                }
                                CellContentTag::BgColorRgb => {
                                    Color::Rgb(from_rgb(raw.bg_color_rgb().map_err(fault)?))
                                }
                                _ => Color::Default,
                            };
                        }
                        let end_col = column.saturating_add(u16::from(width));
                        if let Some(run) = runs
                            .last_mut()
                            .filter(|r| r.style == cell_style && r.columns.end == column)
                        {
                            run.text.end = end;
                            run.columns.end = end_col;
                        } else {
                            runs.push(StyleRun {
                                text: start..end,
                                columns: column..end_col,
                                style: cell_style,
                            });
                        }
                    }
                    column += 1;
                }
                let row = TerminalRow {
                    id,
                    revision: self.revision,
                    text: text.into(),
                    cells: spans.into(),
                    runs: runs.into(),
                    soft_wrapped,
                };
                let old = &rows[index];
                if old.id != row.id
                    || old.text != row.text
                    || old.cells != row.cells
                    || old.runs != row.runs
                    || old.soft_wrapped != row.soft_wrapped
                {
                    rows[index] = Arc::new(row);
                }
            }
            index += 1;
        }
        snapshot.clean().map_err(fault)?;

        let unchanged_rows = self.last.as_ref().is_some_and(|last| {
            last.rows.len() == rows.len()
                && last.rows.iter().zip(&rows).all(|(a, b)| Arc::ptr_eq(a, b))
        });
        if !force
            && unchanged_rows
            && unchanged_state
            && self
                .last
                .as_ref()
                .is_some_and(|last| last.viewport == self.viewport)
        {
            return Ok(None);
        }
        if self.last.as_ref().is_none_or(|last| {
            last.viewport != self.viewport || last.scroll != scroll || !unchanged_rows
        }) {
            self.content_revision += 1;
        }
        let frame = Arc::new(TerminalFrame {
            revision: self.revision,
            content_revision: self.content_revision,
            viewport: self.viewport,
            rows: rows.into(),
            cursor,
            colors,
            scroll,
            modes,
        });
        self.last = Some(Arc::clone(&frame));
        Ok(Some(frame))
    }
}

fn hyperlink(terminal: &vt::Terminal, column: u16, row: usize) -> Option<Arc<str>> {
    let grid_ref = terminal
        .grid_ref(Point::Viewport(PointCoordinate {
            x: column,
            y: u32::try_from(row).ok()?,
        }))
        .ok()?;
    let mut buf = vec![0u8; 256];
    let len = match grid_ref.hyperlink_uri(&mut buf) {
        Ok(len) => len,
        Err(vt::Error::OutOfSpace { required }) => {
            buf.resize(required, 0);
            grid_ref.hyperlink_uri(&mut buf).ok()?
        }
        Err(_) => return None,
    };
    if len == 0 {
        return None;
    }
    Some(Arc::from(String::from_utf8_lossy(&buf[..len]).as_ref()))
}

/// Decode an OSC 7 value into an absolute path. On Windows a file URI
/// names a drive after its first slash, as in `file://host/C:/Users`, and
/// the slash before the drive is dropped.
pub(crate) fn reported_cwd(value: &str) -> Option<PathBuf> {
    let path = if let Some(uri) = value.strip_prefix("file://") {
        let slash = uri.find('/')?;
        let path = &uri[slash..];
        if cfg!(windows) && path.as_bytes().get(2) == Some(&b':') {
            &path[1..]
        } else {
            path
        }
    } else {
        value
    };
    let mut bytes = Vec::new();
    let input = path.as_bytes();
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%'
            && index + 2 < input.len()
            && let Ok(value) =
                u8::from_str_radix(std::str::from_utf8(&input[index + 1..index + 3]).ok()?, 16)
        {
            bytes.push(value);
            index += 3;
            continue;
        }
        bytes.push(input[index]);
        index += 1;
    }
    let path = PathBuf::from(String::from_utf8(bytes).ok()?);
    path.is_absolute().then_some(path)
}

/// Map a GPUI key name to a Ghostty key code.
pub(crate) fn key_code(name: &str) -> key::Key {
    use key::Key;
    if name.len() == 1 {
        let byte = name.as_bytes()[0];
        if byte.is_ascii_alphabetic() {
            let index = i32::from(byte.to_ascii_lowercase() - b'a');
            return Key::try_from(Key::A as i32 + index).unwrap_or(Key::Unidentified);
        }
        if byte.is_ascii_digit() {
            let index = i32::from(byte - b'0');
            return Key::try_from(Key::Digit0 as i32 + index).unwrap_or(Key::Unidentified);
        }
    }
    if let Some(number) = name
        .strip_prefix('f')
        .and_then(|n| n.parse::<i32>().ok())
        .filter(|n| (1..=25).contains(n))
    {
        return Key::try_from(Key::F1 as i32 + number - 1).unwrap_or(Key::Unidentified);
    }
    match name {
        "enter" | "return" => Key::Enter,
        "escape" => Key::Escape,
        "backspace" => Key::Backspace,
        "tab" => Key::Tab,
        "space" | " " => Key::Space,
        "up" => Key::ArrowUp,
        "down" => Key::ArrowDown,
        "left" => Key::ArrowLeft,
        "right" => Key::ArrowRight,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "delete" => Key::Delete,
        "insert" => Key::Insert,
        "-" => Key::Minus,
        "=" => Key::Equal,
        "[" => Key::BracketLeft,
        "]" => Key::BracketRight,
        "\\" => Key::Backslash,
        ";" => Key::Semicolon,
        "'" => Key::Quote,
        "," => Key::Comma,
        "." => Key::Period,
        "/" => Key::Slash,
        "`" => Key::Backquote,
        _ => Key::Unidentified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core() -> Core {
        Core::new(
            Viewport::new(20, 4, 8, 16).unwrap(),
            &EngineOptions::default(),
            TerminalColors::default(),
        )
        .unwrap()
    }

    #[test]
    fn frames_carry_text_styles_and_wide_cells() {
        let mut core = core();
        let mut effects = Effects::default();
        core.write("\x1b[1;31mred\x1b[0m \u{6f22}\r\n".as_bytes(), &mut effects);
        let frame = core.frame(false).unwrap().unwrap();
        assert_eq!(frame.rows[0].text.trim_end(), "red \u{6f22}");
        let run = &frame.rows[0].runs[0];
        assert!(run.style.bold);
        assert_eq!(run.style.foreground, Color::Indexed(1));
        assert_eq!(run.columns, 0..3);
        let wide = frame.rows[0]
            .cells
            .iter()
            .find(|c| c.width == 2)
            .expect("wide cell");
        assert_eq!(&frame.rows[0].text[wide.text.clone()], "\u{6f22}");
        assert!(frame.rows[0].cells[wide.column as usize + 1].spacer);
        assert!(
            core.frame(false).unwrap().is_none(),
            "clean frames are not republished"
        );
    }

    #[test]
    fn effects_collect_replies_title_and_bell() {
        let mut core = core();
        let mut effects = Effects::default();
        core.write(b"\x07\x1b]2;hello\x1b\\\x1b[c", &mut effects);
        assert_eq!(effects.bells, 1);
        assert!(effects.title_changed);
        assert_eq!(core.title(), "hello");
        assert_eq!(effects.replies, b"\x1b[?62;22c");
    }

    #[test]
    fn keys_are_encoded_with_the_terminal_modes() {
        let mut core = core();
        let up = core
            .input(TerminalInput::Key(KeyInputBuilder::key("up")))
            .unwrap();
        assert_eq!(up, b"\x1b[A");
        let mut effects = Effects::default();
        core.write(b"\x1b[?1h", &mut effects);
        let up = core
            .input(TerminalInput::Key(KeyInputBuilder::key("up")))
            .unwrap();
        assert_eq!(up, b"\x1bOA");
        let text = core
            .input(TerminalInput::Key(
                crate::input::KeyInput::new("a").with_text("a"),
            ))
            .unwrap();
        assert_eq!(text, b"a");
        let ctrl_c = core
            .input(TerminalInput::Key(
                crate::input::KeyInput::new("c").with_modifiers(Modifiers {
                    control: true,
                    ..Default::default()
                }),
            ))
            .unwrap();
        assert_eq!(ctrl_c, b"\x03");
    }

    #[test]
    fn paste_is_bracketed_only_when_the_program_asks() {
        let mut core = core();
        let plain = core.input(TerminalInput::Paste("a\nb".to_owned())).unwrap();
        assert_eq!(plain, b"a\rb");
        let mut effects = Effects::default();
        core.write(b"\x1b[?2004h", &mut effects);
        let bracketed = core.input(TerminalInput::Paste("a\nb".to_owned())).unwrap();
        assert_eq!(bracketed, b"\x1b[200~a\nb\x1b[201~");
    }

    fn screen(core: &mut Core) -> Vec<String> {
        let frame = core.frame(true).unwrap().unwrap();
        frame
            .rows
            .iter()
            .map(|row| row.text.trim_end().to_owned())
            .collect()
    }

    #[test]
    fn a_resize_clears_the_prompt_so_the_shell_redraws_one_copy() {
        let mut core = core();
        let mut effects = Effects::default();
        // Fish 4 marks its prompt this way and never sends `redraw=1`.
        core.write(
            b"\x1b]133;A;click_events=1\x1b\\user@host ~> \x1b]133;B\x1b\\",
            &mut effects,
        );
        assert!(core.at_prompt());
        core.resize(Viewport::new(16, 4, 8, 16).unwrap(), &mut effects)
            .unwrap();
        assert_eq!(screen(&mut core), vec!["", "", "", ""]);
    }

    #[test]
    fn a_resize_keeps_output_that_is_not_a_prompt() {
        let mut core = core();
        let mut effects = Effects::default();
        assert!(!core.at_prompt());
        core.write(b"build ok", &mut effects);
        core.resize(Viewport::new(4, 4, 8, 16).unwrap(), &mut effects)
            .unwrap();
        assert_eq!(screen(&mut core), vec!["buil", "d ok", "", ""]);
    }

    #[test]
    fn copy_reads_the_selected_cells() {
        let mut core = core();
        let mut effects = Effects::default();
        core.write(b"hello world\r\nsecond", &mut effects);
        core.frame(false).unwrap();
        let text = core
            .copy(CellSelection {
                start: (6, 0),
                end: (2, 1),
                rectangle: false,
            })
            .unwrap();
        assert_eq!(text, "world\nsec");
    }

    #[test]
    fn a_render_hold_stops_frames_until_released() {
        let mut core = core();
        let mut effects = Effects::default();
        core.write(b"one", &mut effects);
        assert!(core.frame(false).unwrap().is_some());
        core.write(b"\x1b[?2026htwo", &mut effects);
        assert!(core.held());
        assert!(core.frame(false).unwrap().is_none());
        core.write(b"\x1b[?2026l", &mut effects);
        assert!(!core.held());
        let frame = core.frame(false).unwrap().unwrap();
        assert_eq!(frame.rows[0].text.trim_end(), "onetwo");
    }

    #[cfg(not(windows))]
    #[test]
    fn reported_cwd_decodes_file_uris() {
        assert_eq!(
            reported_cwd("file://host/tmp/a%20b"),
            Some(PathBuf::from("/tmp/a b"))
        );
        assert_eq!(reported_cwd("/var/tmp"), Some(PathBuf::from("/var/tmp")));
        assert_eq!(reported_cwd("relative"), None);
    }

    #[cfg(windows)]
    #[test]
    fn reported_cwd_decodes_file_uris_with_a_drive() {
        assert_eq!(
            reported_cwd("file://host/C:/Users/a%20b"),
            Some(PathBuf::from("C:/Users/a b"))
        );
        assert_eq!(
            reported_cwd("C:\\Windows"),
            Some(PathBuf::from("C:\\Windows"))
        );
        assert_eq!(reported_cwd("/tmp"), None);
        assert_eq!(reported_cwd("relative"), None);
    }

    struct KeyInputBuilder;
    impl KeyInputBuilder {
        fn key(name: &str) -> crate::input::KeyInput {
            crate::input::KeyInput::new(name)
        }
    }
}
