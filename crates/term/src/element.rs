//! The terminal element: one canvas that paints the grid.
//!
//! Everything the paint phase needs is built during prepaint so painting
//! never reads the entity or the engine.
//!
//! Derived from Herdr (Apache-2.0). The canvas structure, cell-aligned glyph
//! positioning and merged background quads are adapted from Muxy v2 (MIT).

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::base::{StyledExt as _, TestSupportExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, BorderStyle, Bounds, ContentMask, DispatchPhase, ElementId, ElementInputHandler, Entity,
    EntityInputHandler as _, Font, FontStyle, FontWeight, HitboxBehavior, Hsla, InteractiveElement,
    IntoElement, LineLayout, ModifiersChangedEvent, MouseButton, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Point, RenderOnce, ScrollEvent, ShapedLine, Size,
    StatefulInteractiveElement as _, StyleRefinement, Styled, TextAlign, TextRun, Window, canvas,
    div, fill, outline, point, px, size,
};

use crate::actions::{
    Clear, Copy, DecreaseFontSize, IncreaseFontSize, Paste, ResetFontSize, ScrollLineDown,
    ScrollLineUp, ScrollPageDown, ScrollPageUp, ScrollToBottom, ScrollToTop, SelectAll,
};
use crate::block;
use crate::colors::Palette;
use crate::decoration;
use crate::frame::{CursorShape, StyleRun, TerminalColors, TerminalFrame, TerminalRow};
use crate::geometry::Geometry;
use crate::host_theme::HostTheme;
use crate::input::ScrollRequest;
use crate::selection::Selection;
use crate::state::TerminalState;
use ghostty_vt::render::RowId;

/// Zero-width non-joiner separating runs so shaping cannot form a ligature
/// across a style boundary.
const SEPARATOR: char = '\u{200c}';

/// The key context actions are bound in.
pub const KEY_CONTEXT: &str = "Terminal";

/// The terminal grid, drawn for a [`TerminalState`]. It fills its parent
/// unless styled otherwise.
#[derive(IntoElement)]
pub struct Terminal {
    id: ElementId,
    state: Entity<TerminalState>,
    style: StyleRefinement,
}

impl Terminal {
    /// An element `id` that draws `state`.
    pub fn new(id: impl Into<ElementId>, state: &Entity<TerminalState>) -> Self {
        Self {
            id: id.into(),
            state: state.clone(),
            style: StyleRefinement::default(),
        }
    }
}

impl Styled for Terminal {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

fn update<E: 'static>(
    state: &Entity<TerminalState>,
    handler: impl Fn(&mut TerminalState, &E, &mut Window, &mut gpui_kit::Context<TerminalState>)
    + 'static,
) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    let state = state.clone();
    move |event, window, cx| state.update(cx, |s, cx| handler(s, event, window, cx))
}

impl RenderOnce for Terminal {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = self.state;
        let (focus, hovered, message, background) = {
            let s = state.read(cx);
            (
                s.focus_handle().clone(),
                s.hovered_link().is_some(),
                s.status_message(),
                Palette::new(&s.frame().colors).background(),
            )
        };
        let focused = focus.is_focused(window);
        let status_id = ElementId::NamedChild(Arc::new(self.id.clone()), "status".into());
        let theme = HostTheme::of(cx);
        let (pill, pill_text, radius, text_size) =
            (theme.pill, theme.pill_text, theme.radius, theme.text_size);
        div()
            .id(self.id)
            .test_support()
            .key_context(KEY_CONTEXT)
            .track_focus(&focus)
            .relative()
            .size_full()
            .overflow_hidden()
            .role(gpui_kit::Role::Terminal)
            .aria_label("Terminal")
            .aria_value("")
            .on_a11y_action(gpui_kit::AccessibleAction::SetValue, {
                let state = state.clone();
                move |data, window, cx| {
                    let Some(gpui_kit::accesskit::ActionData::Value(text)) = data else {
                        return;
                    };
                    state.update(cx, |s, cx| s.replace_text_in_range(None, text, window, cx));
                }
            })
            .bg(background)
            .refine_style(&self.style)
            .on_action(update(&state, |s, _: &Copy, _, cx| {
                if s.has_selection() {
                    s.copy(cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(update(&state, |s, _: &Paste, _, cx| s.paste(cx)))
            .on_action(update(&state, |s, _: &SelectAll, _, cx| s.select_all(cx)))
            .on_action(update(&state, |s, _: &Clear, _, cx| s.clear(cx)))
            .on_action(update(&state, |s, _: &IncreaseFontSize, _, cx| {
                s.increase_font_size(px(1.), cx);
            }))
            .on_action(update(&state, |s, _: &DecreaseFontSize, _, cx| {
                s.decrease_font_size(px(1.), cx);
            }))
            .on_action(update(&state, |s, _: &ResetFontSize, _, cx| {
                s.reset_font_size(cx);
            }))
            .on_action(update(&state, |s, _: &ScrollToTop, _, cx| {
                s.scroll(ScrollRequest::Top, cx);
            }))
            .on_action(update(&state, |s, _: &ScrollToBottom, _, cx| {
                s.scroll(ScrollRequest::Bottom, cx);
            }))
            .on_action(update(&state, |s, _: &ScrollPageUp, _, cx| {
                s.scroll_pages(-1, cx);
            }))
            .on_action(update(&state, |s, _: &ScrollPageDown, _, cx| {
                s.scroll_pages(1, cx);
            }))
            .on_action(update(&state, |s, _: &ScrollLineUp, _, cx| {
                s.scroll(ScrollRequest::Lines(-1), cx);
            }))
            .on_action(update(&state, |s, _: &ScrollLineDown, _, cx| {
                s.scroll(ScrollRequest::Lines(1), cx);
            }))
            .when(hovered, Styled::cursor_pointer)
            .on_modifiers_changed(update(&state, |s, event: &ModifiersChangedEvent, w, cx| {
                s.hover_link(w.mouse_position(), event.modifiers.platform, cx);
            }))
            .on_key_down(update(&state, TerminalState::on_key_down))
            .on_key_up(update(&state, TerminalState::on_key_up))
            .on_mouse_down(
                MouseButton::Left,
                update(&state, TerminalState::on_mouse_down),
            )
            .on_mouse_down(
                MouseButton::Middle,
                update(&state, TerminalState::on_mouse_down),
            )
            .on_mouse_down(
                MouseButton::Right,
                update(&state, TerminalState::on_mouse_down),
            )
            .on_mouse_move(update(&state, TerminalState::on_mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                update(&state, TerminalState::on_mouse_up),
            )
            .on_mouse_up(
                MouseButton::Middle,
                update(&state, TerminalState::on_mouse_up),
            )
            .on_mouse_up(
                MouseButton::Right,
                update(&state, TerminalState::on_mouse_up),
            )
            .child(grid(state, focused))
            .when_some(message, |element, message| {
                element.child(
                    div()
                        .id(status_id)
                        .test_support()
                        .absolute()
                        .bottom_2()
                        .left_2()
                        .px_2()
                        .py_1()
                        .rounded(radius)
                        .bg(pill)
                        .text_color(pill_text)
                        .text_size(text_size)
                        .child(message),
                )
            })
    }
}

/// Everything the paint phase needs.
#[derive(Default)]
struct Painting {
    backgrounds: Vec<(Bounds<Pixels>, Hsla)>,
    selections: Vec<Bounds<Pixels>>,
    blocks: Vec<(Bounds<Pixels>, Hsla)>,
    decorations: Vec<(Bounds<Pixels>, Hsla)>,
    lines: Vec<(Point<Pixels>, ShapedLine)>,
    cursor: Option<(Bounds<Pixels>, Hsla, CursorShape)>,
    cell_height: Pixels,
    selection: Hsla,
    cursor_thickness: Pixels,
    cursor_opacity: f32,
}

/// Each row's prepared glyphs and quads from the last frame, relative to
/// the row's top left, so a row that did not change is not built again:
/// not on a cursor blink, a hover, or a scroll that moves it.
///
/// A row is keyed by its stable id, its revision and its selected columns.
/// Everything a row's look depends on beyond those (the font, its size, the
/// cell, the scale and the colors) is the cache's `look`; a change to it
/// clears the cache. The cache keeps only the rows the last frame drew, so
/// it never holds more than one screen.
#[derive(Default)]
pub(crate) struct RowCache {
    look: Option<RowLook>,
    rows: HashMap<RowKey, Arc<RowPaint>>,
}

#[derive(Clone, PartialEq)]
struct RowLook {
    font: Font,
    font_size: Pixels,
    cell: Size<Pixels>,
    columns: u16,
    scale: f32,
    colors: TerminalColors,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct RowKey {
    id: RowId,
    revision: u64,
    selected: std::ops::Range<u16>,
}

/// One row's share of a [`Painting`], at row zero of a grid at the origin.
#[derive(Default)]
struct RowPaint {
    backgrounds: Vec<(Bounds<Pixels>, Hsla)>,
    blocks: Vec<(Bounds<Pixels>, Hsla)>,
    decorations: Vec<(Bounds<Pixels>, Hsla)>,
    line: Option<ShapedLine>,
}

impl RowPaint {
    /// Moves this row's parts by `offset` into the frame's painting.
    fn emit(&self, offset: Point<Pixels>, painting: &mut Painting) {
        let moved = |(bounds, color): &(Bounds<Pixels>, Hsla)| {
            (Bounds::new(bounds.origin + offset, bounds.size), *color)
        };
        painting
            .backgrounds
            .extend(self.backgrounds.iter().map(moved));
        painting.blocks.extend(self.blocks.iter().map(moved));
        painting
            .decorations
            .extend(self.decorations.iter().map(moved));
        if let Some(line) = &self.line {
            painting.lines.push((offset, line.clone()));
        }
    }
}

fn grid(state: Entity<TerminalState>, focused: bool) -> impl IntoElement {
    let paint_state = state.clone();
    canvas(
        move |bounds, window, cx| {
            let theme = HostTheme::of(cx);
            let look = Look {
                cursor_thickness: theme.cursor_thickness,
                cursor_opacity: theme.cursor_opacity,
            };
            let (frame, appearance, marked) = {
                let s = state.read(cx);
                (
                    Arc::clone(s.frame()),
                    s.drawn_appearance(&theme),
                    s.marked_text().map(str::to_owned),
                )
            };
            let font = appearance.font();
            let font_size = appearance.size();
            let cell = appearance.cell_size(window);
            let geometry =
                Geometry::new(bounds, cell, appearance.inset(), appearance.padding_balance);
            let scale = window.scale_factor();
            {
                let s = state.read(cx);
                if s.geometry != Some(geometry) || s.geometry_scale() != Some(scale) {
                    let deferred = state.downgrade();
                    window.defer(cx, move |_, cx| {
                        let _ = deferred.update(cx, |s, cx| s.set_geometry(geometry, scale, cx));
                    });
                }
            }
            let active = window.is_window_active();
            let reduce_motion = cx.reduce_motion();
            let on_screen = !window
                .content_mask()
                .bounds
                .intersect(&geometry.bounds)
                .is_empty();
            state.update(cx, |s, cx| {
                let blinking = focused
                    && active
                    && on_screen
                    && !reduce_motion
                    && s.is_visible()
                    && s.is_live()
                    && marked.is_none()
                    && cursor_in_grid(s.frame(), geometry)
                    && s.appearance().cursor_blinks(s.frame().cursor.blinking);
                s.cursor.sync(blinking, cx, |s, cx| {
                    s.cursor.tick();
                    cx.notify();
                });
            });
            let mut rows = state.update(cx, |s, _| std::mem::take(&mut s.row_cache));
            let s = state.read(cx);
            let mut painting = prepare(
                &frame,
                geometry,
                look,
                s.selection,
                cursor_style(
                    focused,
                    s.cursor.visible,
                    appearance.cursor_shape.unwrap_or(frame.cursor.shape),
                ),
                marked.as_deref(),
                &font,
                font_size,
                &mut rows,
                window,
            );
            if let Some((row, columns)) = s.hovered_link_span()
                && row < geometry.rows
            {
                let columns = columns.start..columns.end.min(geometry.columns);
                if columns.start < columns.end {
                    let bounds = geometry.span_bounds(columns, row);
                    painting.decorations.push((
                        underline(bounds, look.cursor_thickness),
                        Palette::new(&frame.colors).foreground(),
                    ));
                }
            }
            state.update(cx, |s, _| s.row_cache = rows);
            let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
            (painting, geometry, hitbox)
        },
        move |_bounds, (painting, geometry, hitbox), window, cx| {
            // A scroll area around the terminal takes the wheel in the
            // capture phase, from the outside in, so the terminal takes
            // its steps there too, and its stop keeps them from the page.
            let scroll_state = paint_state.downgrade();
            window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture && hitbox.should_handle_scroll(window) {
                    let _ = scroll_state.update(cx, |s, cx| s.on_scroll(event, window, cx));
                }
            });
            let drag_state = paint_state.downgrade();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture
                    && event.pressed_button == Some(MouseButton::Left)
                {
                    let _ = drag_state.update(cx, |s, cx| s.drag_selection(event.position, cx));
                }
            });
            let release_state = paint_state.downgrade();
            window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture && event.button == MouseButton::Left {
                    let _ =
                        release_state.update(cx, |s, cx| s.finish_selection(event.position, cx));
                }
            });
            let focus = paint_state.read(cx).focus_handle().clone();
            window.handle_input(
                &focus,
                ElementInputHandler::new(geometry.grid_bounds(), paint_state.clone()),
                cx,
            );
            let mut painted = false;
            window.with_content_mask(
                Some(ContentMask {
                    bounds: geometry.bounds,
                }),
                |window| {
                    painted = !window.content_mask().bounds.is_empty();
                    paint(painting, window, cx);
                },
            );
            let painted_state = paint_state.downgrade();
            window.defer(cx, move |_, cx| {
                let _ = painted_state.update(cx, |s, _| s.frame_painted(painted));
            });
        },
    )
    .size_full()
}

/// The theme values the paint phase needs.
#[derive(Clone, Copy)]
struct Look {
    cursor_thickness: Pixels,
    cursor_opacity: f32,
}

/// A hairline under a span of cells, `offset` above the cell bottom.
fn underline(span: Bounds<Pixels>, offset: Pixels) -> Bounds<Pixels> {
    Bounds::new(
        span.origin + point(px(0.0), span.size.height - offset),
        size(span.size.width, px(1.0)),
    )
}

impl TerminalState {
    fn geometry_scale(&self) -> Option<f32> {
        self.geometry.map(|_| self.geometry_scale_value())
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "prepaint hands each piece over separately so paint never reads the entity"
)]
fn prepare(
    frame: &TerminalFrame,
    geometry: Geometry,
    look: Look,
    selection: Option<Selection>,
    cursor: Option<CursorShape>,
    marked: Option<&str>,
    font: &Font,
    font_size: Pixels,
    cache: &mut RowCache,
    window: &mut Window,
) -> Painting {
    let palette = Palette::new(&frame.colors);
    let mut painting = Painting {
        cell_height: geometry.cell.height,
        selection: palette.selection(),
        cursor_thickness: look.cursor_thickness,
        cursor_opacity: look.cursor_opacity,
        ..Painting::default()
    };
    let scale = window.scale_factor();

    let row_look = RowLook {
        font: font.clone(),
        font_size,
        cell: geometry.cell,
        columns: geometry.columns,
        scale,
        colors: frame.colors.clone(),
    };
    if cache.look.as_ref() != Some(&row_look) {
        cache.look = Some(row_look);
        cache.rows.clear();
    }
    // Rows are prepared at row zero of a grid at the origin and moved into
    // place, so a cached row fits wherever it is now.
    let at_origin = Geometry {
        origin: point(px(0.0), px(0.0)),
        ..geometry
    };
    let mut drawn = HashMap::with_capacity(usize::from(geometry.rows));
    let visible = usize::from(geometry.rows).min(frame.rows.len());
    for (index, row) in frame.rows.iter().take(visible).enumerate() {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "`visible` is at most the grid's row count, a u16"
        )]
        let row_index = index as u16;
        let selected = selection
            .map(|selection| selection.columns(row_index, geometry.columns))
            .unwrap_or(0..0);
        if !selected.is_empty() {
            painting
                .selections
                .push(geometry.span_bounds(selected.clone(), row_index));
        }
        let key = RowKey {
            id: row.id,
            revision: row.revision,
            selected: selected.clone(),
        };
        let row_paint = cache.rows.remove(&key).unwrap_or_else(|| {
            let mut row_paint = RowPaint::default();
            prepare_row(
                row,
                at_origin,
                palette,
                selected,
                font,
                font_size,
                scale,
                window,
                &mut row_paint,
            );
            Arc::new(row_paint)
        });
        row_paint.emit(geometry.row_origin(row_index), &mut painting);
        drawn.insert(key, row_paint);
    }
    cache.rows = drawn;

    if let Some(marked) = marked.filter(|m| !m.is_empty())
        && cursor_in_grid(frame, geometry)
    {
        // Composition text is drawn at the cursor with an underline.
        let start = frame.cursor.column;
        let columns = u16::try_from(marked.chars().count()).unwrap_or(u16::MAX);
        let end = start.saturating_add(columns).min(geometry.columns);
        let bounds = geometry.span_bounds(start..end, frame.cursor.row);
        painting.backgrounds.push((bounds, palette.background()));
        let line = window.text_system().shape_line(
            marked.to_owned().into(),
            font_size,
            &[TextRun {
                len: marked.len(),
                font: font.clone(),
                color: palette.foreground(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        painting.lines.push((bounds.origin, line));
        painting.decorations.push((
            underline(bounds, look.cursor_thickness),
            palette.foreground(),
        ));
        return painting;
    }

    if let Some(shape) = cursor
        && cursor_in_grid(frame, geometry)
    {
        let bounds = geometry.cell_bounds(frame.cursor.column, frame.cursor.row);
        painting.cursor = Some((bounds, palette.cursor(), shape));
    }
    painting
}

/// The cursor to draw, in Ghostty's order (`src/renderer/cursor.zig`): a
/// terminal without focus always shows a hollow block, so a split shows
/// where each pane's cursor sits; one with focus shows `shape` while the
/// blink is on.
fn cursor_style(focused: bool, blink_on: bool, shape: CursorShape) -> Option<CursorShape> {
    if !focused {
        Some(CursorShape::Hollow)
    } else if blink_on {
        Some(shape)
    } else {
        None
    }
}

fn cursor_in_grid(frame: &TerminalFrame, geometry: Geometry) -> bool {
    frame.cursor.visible
        && frame.scroll.at_bottom()
        && frame.cursor.column < geometry.columns
        && frame.cursor.row < geometry.rows
}

#[expect(
    clippy::too_many_arguments,
    reason = "prepaint hands each piece over separately so paint never reads the entity"
)]
fn prepare_row(
    row: &TerminalRow,
    geometry: Geometry,
    palette: Palette<'_>,
    selected: std::ops::Range<u16>,
    font: &Font,
    font_size: Pixels,
    scale: f32,
    window: &mut Window,
    painting: &mut RowPaint,
) {
    let row_index = 0;
    let mut text = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    let mut columns: Vec<u16> = Vec::new();
    let mut last_column = 0_u16;

    for run in row.runs.iter() {
        if run.columns.start >= geometry.columns {
            break;
        }
        let end = run.columns.end.min(geometry.columns);
        if end <= run.columns.start {
            continue;
        }
        let (foreground, background) = palette.style(&run.style);
        let bounds = geometry.span_bounds(run.columns.start..end, row_index);
        if background != palette.background() {
            merge_quad(&mut painting.backgrounds, bounds, background);
        }
        last_column = end;
        if run.style.invisible {
            continue;
        }
        let first = row
            .cells
            .partition_point(|cell| cell.column < run.columns.start);
        let mut pending: Option<(StyleRun, Hsla)> = None;
        for cell in row.cells[first..]
            .iter()
            .take_while(|cell| cell.column < end)
            .filter(|cell| !cell.spacer)
        {
            let cell_end = cell.column.saturating_add(u16::from(cell.width)).min(end);
            let cell_bounds = geometry.span_bounds(cell.column..cell_end, row_index);
            let color = if selected.start < cell_end && cell.column < selected.end {
                palette.selection_foreground()
            } else {
                foreground
            };
            let cell_text = row.text.get(cell.text.clone()).unwrap_or_default();
            let quads = block::quads(cell_text, cell_bounds, color, scale);
            if (quads.is_some() || pending.as_ref().is_some_and(|(_, p)| *p != color))
                && let Some((span, color)) = pending.take()
            {
                append_run(row, &span, color, font, &mut text, &mut runs, &mut columns);
            }
            if let Some(quads) = quads {
                for (bounds, color) in quads {
                    merge_quad(&mut painting.blocks, bounds, color);
                }
            } else if let Some((span, _)) = pending.as_mut() {
                span.text.end = cell.text.end;
                span.columns.end = cell_end;
            } else {
                let mut span = run.clone();
                span.text = cell.text.clone();
                span.columns = cell.column..cell_end;
                pending = Some((span, color));
            }
        }
        if let Some((span, color)) = pending {
            append_run(row, &span, color, font, &mut text, &mut runs, &mut columns);
        }
        let selected_start = selected.start.clamp(run.columns.start, end);
        let selected_end = selected.end.clamp(selected_start, end);
        for (span, in_selection) in [
            (run.columns.start..selected_start, false),
            (selected_start..selected_end, true),
            (selected_end..end, false),
        ] {
            if span.is_empty() {
                continue;
            }
            let color = if in_selection {
                palette.selection_foreground()
            } else {
                foreground
            };
            let underline = if in_selection {
                palette.selection_foreground()
            } else {
                palette.underline(&run.style)
            };
            decoration::prepare(
                &mut painting.decorations,
                &run.style,
                geometry.span_bounds(span, row_index),
                color,
                underline,
                scale,
            );
        }
    }

    if !text.is_empty() {
        let mut line = window
            .text_system()
            .shape_line(text.into(), font_size, &runs, None);
        align_to_cells(&mut line, &columns, geometry.cell.width, last_column);
        painting.line = Some(line);
    }
}

/// Append one style run's text, recording the grid column each byte belongs
/// to so glyphs can be snapped back onto cell boundaries after shaping.
fn append_run(
    row: &TerminalRow,
    run: &StyleRun,
    color: Hsla,
    font: &Font,
    text: &mut String,
    runs: &mut Vec<TextRun>,
    columns: &mut Vec<u16>,
) {
    let run_text = row.text.get(run.text.clone()).unwrap_or_default();
    if run_text.is_empty() || run_text.bytes().all(|byte| byte == b' ') {
        return;
    }
    let start = text.len();
    text.push_str(run_text);
    text.push(SEPARATOR);

    let mut byte = run.text.start;
    let first = row
        .cells
        .partition_point(|cell| cell.column < run.columns.start);
    for cell in row.cells[first..]
        .iter()
        .take_while(|cell| cell.column < run.columns.end)
    {
        if cell.spacer || cell.text.end <= run.text.start || cell.text.start >= run.text.end {
            continue;
        }
        let cell_start = cell.text.start.max(run.text.start);
        let cell_end = cell.text.end.min(run.text.end);
        columns.extend(std::iter::repeat_n(cell.column, cell_end - cell_start));
        byte = cell_end;
    }
    columns.extend(std::iter::repeat_n(
        run.columns.start,
        run.text.end.saturating_sub(byte),
    ));
    columns.resize(text.len() - SEPARATOR.len_utf8(), run.columns.start);
    columns.extend(std::iter::repeat_n(
        run.columns.end.saturating_sub(1),
        SEPARATOR.len_utf8(),
    ));

    let mut font = font.clone();
    if run.style.bold {
        font.weight = FontWeight::BOLD;
    }
    if run.style.italic {
        font.style = FontStyle::Italic;
    }
    runs.push(TextRun {
        len: text.len() - start,
        font,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    });
}

/// Snap each glyph back to its grid column. Proportional fallback fonts,
/// CJK glyphs and combining marks otherwise drift away from the grid.
fn align_to_cells(line: &mut ShapedLine, columns: &[u16], cell_width: Pixels, width: u16) {
    let mut runs = line.runs.clone();
    let mut anchors: HashMap<u16, Pixels> = HashMap::new();
    for glyph in runs.iter().flat_map(|run| &run.glyphs) {
        if let Some(column) = columns.get(glyph.index) {
            anchors.entry(*column).or_insert(glyph.position.x);
        }
    }
    for run in &mut runs {
        for glyph in &mut run.glyphs {
            if let Some(column) = columns.get(glyph.index)
                && let Some(anchor) = anchors.get(column)
            {
                glyph.position.x = cell_width * f32::from(*column) + glyph.position.x - *anchor;
            }
        }
    }
    **line = Arc::new(LineLayout {
        font_size: line.font_size,
        width: cell_width * f32::from(width),
        ascent: line.ascent,
        descent: line.descent,
        runs,
        len: line.len(),
    });
}

/// Merge a quad into the previous one when they are horizontally adjacent
/// and share a color.
fn merge_quad(quads: &mut Vec<(Bounds<Pixels>, Hsla)>, bounds: Bounds<Pixels>, color: Hsla) {
    if let Some((previous, previous_color)) = quads.last_mut()
        && *previous_color == color
        && previous.origin.y == bounds.origin.y
        && previous.right() == bounds.left()
        && previous.size.height == bounds.size.height
    {
        previous.size.width += bounds.size.width;
        return;
    }
    quads.push((bounds, color));
}

fn paint(painting: Painting, window: &mut Window, cx: &mut App) {
    for (bounds, color) in painting.backgrounds {
        window.paint_quad(fill(bounds, color));
    }
    for bounds in painting.selections {
        window.paint_quad(fill(bounds, painting.selection));
    }
    for (bounds, color) in painting.decorations {
        window.paint_quad(fill(bounds, color));
    }
    for (bounds, color) in painting.blocks {
        window.paint_quad(fill(bounds, color));
    }
    for (origin, line) in painting.lines {
        let _ = line.paint(
            origin,
            painting.cell_height,
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
    if let Some((bounds, color, shape)) = painting.cursor {
        let thickness = painting.cursor_thickness;
        match shape {
            CursorShape::Block => {
                window.paint_quad(fill(bounds, color.opacity(painting.cursor_opacity)));
            }
            CursorShape::Hollow => {
                window.paint_quad(outline(bounds, color, BorderStyle::Solid));
            }
            CursorShape::Bar => window.paint_quad(fill(
                Bounds::new(bounds.origin, size(thickness, bounds.size.height)),
                color,
            )),
            CursorShape::Underline => window.paint_quad(fill(
                Bounds::new(
                    bounds.origin + point(px(0.0), bounds.size.height - thickness),
                    size(bounds.size.width, thickness),
                ),
                color,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{cursor_style, merge_quad};
    use crate::frame::CursorShape;

    #[test]
    fn a_terminal_without_focus_shows_a_hollow_block_through_the_blink() {
        assert_eq!(
            cursor_style(false, true, CursorShape::Bar),
            Some(CursorShape::Hollow)
        );
        assert_eq!(
            cursor_style(false, false, CursorShape::Bar),
            Some(CursorShape::Hollow)
        );
        assert_eq!(
            cursor_style(true, true, CursorShape::Bar),
            Some(CursorShape::Bar)
        );
        assert_eq!(cursor_style(true, false, CursorShape::Bar), None);
    }
    use gpui_kit::{Bounds, Hsla, point, px, size};

    #[test]
    fn adjacent_quads_of_one_color_merge_and_others_stay_separate() {
        let mut quads = Vec::new();
        let white = Hsla::white();
        merge_quad(
            &mut quads,
            Bounds::new(point(px(0.0), px(0.0)), size(px(8.0), px(16.0))),
            white,
        );
        merge_quad(
            &mut quads,
            Bounds::new(point(px(8.0), px(0.0)), size(px(8.0), px(16.0))),
            white,
        );
        assert_eq!(quads.len(), 1);
        assert_eq!(quads[0].0.size.width, px(16.0));
        merge_quad(
            &mut quads,
            Bounds::new(point(px(16.0), px(0.0)), size(px(8.0), px(16.0))),
            Hsla::black(),
        );
        merge_quad(
            &mut quads,
            Bounds::new(point(px(0.0), px(16.0)), size(px(8.0), px(16.0))),
            white,
        );
        assert_eq!(quads.len(), 3);
    }
}
