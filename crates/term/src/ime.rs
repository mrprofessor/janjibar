//! Platform text input for the terminal.
//!
//! A terminal has no editable document, so the input handler presents the
//! composition buffer as the entire text. Committed text is sent to the
//! program and the buffer is cleared.
//!
//! Derived from Herdr (Apache-2.0).

use std::ops::Range;

use gpui_kit::{Bounds, Context, EntityInputHandler, Pixels, Point, UTF16Selection, Window};

use crate::input::TerminalInput;
use crate::state::TerminalState;

impl EntityInputHandler for TerminalState {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let marked = self.marked_text.as_deref().unwrap_or_default();
        let utf16: Vec<u16> = marked.encode_utf16().collect();
        let start = range.start.min(utf16.len());
        let end = range.end.min(utf16.len()).max(start);
        if start != range.start || end != range.end {
            *adjusted = Some(start..end);
        }
        Some(String::from_utf16_lossy(&utf16[start..end]))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let length = self.marked_utf16_len();
        Some(UTF16Selection {
            range: length..length,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_text
            .as_ref()
            .map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.marked_text.take().is_some() {
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = None;
        if !text.is_empty() {
            self.selection = None;
            self.send(TerminalInput::Text(text.to_owned()), cx);
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = (!new_text.is_empty()).then(|| new_text.to_owned());
        cx.notify();
    }

    /// The candidate window follows the cursor cell.
    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let geometry = self.geometry?;
        let cursor = self.frame().cursor;
        Some(
            geometry
                .cell_bounds(cursor.column, cursor.row)
                .intersect(&element_bounds.union(&geometry.grid_bounds())),
        )
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl TerminalState {
    fn marked_utf16_len(&self) -> usize {
        self.marked_text
            .as_ref()
            .map_or(0, |text| text.encode_utf16().count())
    }

    /// Text being composed by an input method, drawn at the cursor.
    #[must_use]
    pub fn marked_text(&self) -> Option<&str> {
        self.marked_text.as_deref()
    }
}
