//! The viewport selection model.
//!
//! Derived from Herdr (Apache-2.0).

use crate::frame::{CellSelection, TerminalFrame};

/// A cell position in the viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cell {
    /// The row.
    pub row: u16,
    /// The column.
    pub column: u16,
}

impl Cell {
    /// A cell at `column`, `row`.
    #[must_use]
    pub fn new(column: u16, row: u16) -> Self {
        Self { row, column }
    }
}

/// A selection anchored to the content it was made on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    anchor: Cell,
    head: Cell,
    revision: u64,
    rectangle: bool,
    dragging: bool,
}

impl Selection {
    /// A selection starting at `anchor` on frame content `revision`.
    #[must_use]
    pub fn new(anchor: Cell, revision: u64, rectangle: bool) -> Self {
        Self {
            anchor,
            head: anchor,
            revision,
            rectangle,
            dragging: true,
        }
    }

    /// Move the head.
    pub fn extend(&mut self, head: Cell) {
        self.head = head;
    }

    /// Whether the pointer is still down.
    #[must_use]
    pub fn is_dragging(self) -> bool {
        self.dragging
    }

    /// The pointer went up.
    pub fn finish(&mut self) {
        self.dragging = false;
    }

    /// Keep the selection across a new frame when the selected rows did not
    /// change, so streaming output on another row does not clear it.
    #[must_use]
    pub fn refresh(mut self, previous: &TerminalFrame, next: &TerminalFrame) -> Option<Self> {
        if previous.viewport != next.viewport
            || previous.scroll != next.scroll
            || previous.modes.alternate_screen != next.modes.alternate_screen
        {
            return None;
        }
        if previous.content_revision != next.content_revision {
            let (start, end) = self.ordered();
            for index in start.row..=end.row {
                let old = previous.rows.get(usize::from(index))?;
                let new = next.rows.get(usize::from(index))?;
                if old.text != new.text
                    || old.cells != new.cells
                    || old.soft_wrapped != new.soft_wrapped
                {
                    return None;
                }
            }
        }
        self.revision = next.content_revision;
        Some(self)
    }

    /// True when the drag never left its starting cell.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.anchor == self.head
    }

    fn ordered(self) -> (Cell, Cell) {
        if self.head < self.anchor {
            (self.head, self.anchor)
        } else {
            (self.anchor, self.head)
        }
    }

    /// The selected columns on one row; empty when the row is outside.
    #[must_use]
    pub fn columns(self, row: u16, columns: u16) -> std::ops::Range<u16> {
        let (start, end) = self.ordered();
        if row < start.row || row > end.row {
            return 0..0;
        }
        if self.rectangle {
            let left = start.column.min(end.column);
            let right = start.column.max(end.column);
            return left..right.min(columns);
        }
        let left = if row == start.row { start.column } else { 0 };
        let right = if row == end.row { end.column } else { columns };
        left..right.min(columns)
    }

    /// The selection as inclusive cells for the engine.
    #[must_use]
    pub fn to_cells(self) -> CellSelection {
        let (start, end) = self.ordered();
        CellSelection::new(
            (start.column, start.row),
            (end.column.saturating_sub(1), end.row),
            self.rectangle,
        )
    }

    /// The selected text from the visible rows of a frame.
    ///
    /// A cell is selected when its columns overlap the selection, and a cell
    /// carries its own byte range in the row text, so a selection that cuts
    /// across a wide character or a combining cluster copies the whole
    /// grapheme. A row that soft-wraps joins the next without a newline,
    /// except in a rectangle, where every row is its own line.
    #[must_use]
    pub fn text(self, frame: &TerminalFrame) -> String {
        let mut text = String::new();
        let mut needs_newline = false;
        for (index, row) in frame.rows.iter().enumerate() {
            let Ok(row_index) = u16::try_from(index) else {
                break;
            };
            let columns = self.columns(row_index, frame.viewport.columns());
            if columns.is_empty() {
                continue;
            }
            if needs_newline {
                text.push('\n');
            }
            for cell in row.cells.iter() {
                if cell.spacer
                    || cell.column >= columns.end
                    || columns.start >= cell.column.saturating_add(u16::from(cell.width))
                {
                    continue;
                }
                if let Some(grapheme) = row.text.get(cell.text.clone()) {
                    text.push_str(grapheme);
                }
            }
            needs_newline = self.rectangle || !row.soft_wrapped;
        }
        text
    }
}

/// Trim trailing whitespace from every line.
pub(crate) fn trim_lines(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_click_without_movement_selects_nothing() {
        let selection = Selection::new(Cell::new(3, 1), 7, false);
        assert!(selection.is_empty());
        assert!(selection.columns(1, 80).is_empty());
    }

    #[test]
    fn a_multi_row_drag_selects_to_the_row_edges_in_either_direction() {
        let mut forward = Selection::new(Cell::new(5, 1), 7, false);
        forward.extend(Cell::new(2, 3));
        assert_eq!(forward.columns(0, 80), 0..0);
        assert_eq!(forward.columns(1, 80), 5..80);
        assert_eq!(forward.columns(2, 80), 0..80);
        assert_eq!(forward.columns(3, 80), 0..2);
        let mut backward = Selection::new(Cell::new(2, 3), 7, false);
        backward.extend(Cell::new(5, 1));
        for row in 0..5 {
            assert_eq!(forward.columns(row, 80), backward.columns(row, 80));
        }
    }

    #[test]
    fn a_rectangular_drag_uses_the_same_columns_on_every_row() {
        let mut selection = Selection::new(Cell::new(5, 1), 7, true);
        selection.extend(Cell::new(2, 3));
        for row in 1..=3 {
            assert_eq!(selection.columns(row, 80), 2..5);
        }
        assert_eq!(selection.columns(4, 80), 0..0);
    }

    #[test]
    fn the_engine_selection_is_ordered_and_inclusive() {
        let mut selection = Selection::new(Cell::new(5, 3), 42, false);
        selection.extend(Cell::new(2, 1));
        let cells = selection.to_cells();
        assert_eq!(cells.start, (2, 1));
        assert_eq!(cells.end, (4, 3));
    }

    fn frame(columns: u16, rows: u16, bytes: &str) -> std::sync::Arc<TerminalFrame> {
        use crate::core::{Core, Effects};
        use crate::{EngineOptions, TerminalColors, Viewport};
        let mut core = Core::new(
            Viewport::new(columns, rows, 8, 16).unwrap(),
            &EngineOptions::default(),
            TerminalColors::default(),
        )
        .unwrap();
        core.write(bytes.as_bytes(), &mut Effects::default());
        core.frame(true).unwrap().unwrap()
    }

    fn pick(frame: &TerminalFrame, start: (u16, u16), end: (u16, u16), rectangle: bool) -> String {
        let mut selection = Selection::new(
            Cell::new(start.0, start.1),
            frame.content_revision,
            rectangle,
        );
        selection.extend(Cell::new(end.0, end.1));
        selection.text(frame)
    }

    #[test]
    fn copying_uses_half_open_boundaries_including_the_last_column() {
        let frame = frame(6, 2, "alphaX\r\nbetaYZ");
        for (start, end, expected) in [
            ((0, 0), (5, 0), "alpha"),
            ((0, 0), (1, 0), "a"),
            ((5, 0), (0, 0), "alpha"),
            ((0, 0), (6, 0), "alphaX"),
            ((0, 0), (0, 1), "alphaX"),
        ] {
            assert_eq!(
                pick(&frame, start, end, false),
                expected,
                "{start:?} to {end:?}"
            );
        }
        assert_eq!(pick(&frame, (0, 0), (6, 1), false), "alphaX\nbetaYZ");
    }

    #[test]
    fn copying_joins_soft_wrapped_rows_and_keeps_hard_line_breaks() {
        let frame = frame(6, 3, "alpha bravo\r\nlast");
        assert!(frame.rows[0].soft_wrapped);
        assert!(!frame.rows[1].soft_wrapped);
        let copied = pick(&frame, (0, 0), (4, 2), false);
        assert_eq!(copied, "alpha bravo \nlast");
        assert_eq!(trim_lines(&copied), "alpha bravo\nlast");
    }

    #[test]
    fn copying_a_rectangle_keeps_wrapped_rows_separate() {
        let frame = frame(6, 2, "abcdefghijkl");
        assert!(frame.rows[0].soft_wrapped);
        assert_eq!(pick(&frame, (1, 0), (4, 1), true), "bcd\nhij");
        assert_eq!(pick(&frame, (4, 1), (1, 0), true), "bcd\nhij");
    }

    #[test]
    fn copying_either_half_of_a_wide_cell_keeps_the_whole_grapheme() {
        let frame = frame(10, 1, "\u{6f22}e\u{301}X");
        for (start, end, expected) in [
            (0, 1, "\u{6f22}"),
            (1, 2, "\u{6f22}"),
            (0, 2, "\u{6f22}"),
            (2, 3, "e\u{301}"),
            (0, 4, "\u{6f22}e\u{301}X"),
        ] {
            assert_eq!(
                pick(&frame, (start, 0), (end, 0), false),
                expected,
                "columns {start}..{end}"
            );
        }
    }
}
