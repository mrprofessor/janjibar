//! Cell geometry and hit testing for the grid.
//!
//! Every pixel-to-cell conversion goes through [`Geometry`] so the element,
//! the pointer handlers, the IME candidate rectangle and the resize request
//! cannot disagree about the grid.
//!
//! Derived from Herdr (Apache-2.0). Flooring the content box to whole cells
//! is adapted from Muxy v2 (MIT).

use gpui_kit::{Bounds, Pixels, Point, Size, point, px, size};

use crate::frame::MAX_GRID;
use crate::selection::Cell;

/// Where the grid sits inside the element and how large one cell is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    /// The element bounds.
    pub bounds: Bounds<Pixels>,
    /// The top left of cell (0, 0).
    pub origin: Point<Pixels>,
    /// One cell.
    pub cell: Size<Pixels>,
    /// Whole columns that fit.
    pub columns: u16,
    /// Whole rows that fit.
    pub rows: u16,
}

impl Geometry {
    /// Geometry for `bounds` with `padding` on every side.
    ///
    /// The space left over after whole cells sits at the right and bottom,
    /// as in Ghostty. With `balance` it is split between both sides.
    #[must_use]
    pub fn new(bounds: Bounds<Pixels>, cell: Size<Pixels>, padding: Pixels, balance: bool) -> Self {
        let width = (bounds.size.width - padding * 2.0).max(px(0.0));
        let height = (bounds.size.height - padding * 2.0).max(px(0.0));
        let columns = whole_cells(width, cell.width);
        let rows = whole_cells(height, cell.height);
        let mut origin = bounds.origin + point(padding, padding);
        if balance {
            let spare_x = (width - cell.width * f32::from(columns)).max(px(0.0));
            let spare_y = (height - cell.height * f32::from(rows)).max(px(0.0));
            origin += point((spare_x / 2.0).floor(), (spare_y / 2.0).floor());
        }
        Self {
            bounds,
            origin,
            cell,
            columns,
            rows,
        }
    }

    /// The bounds of one cell.
    #[must_use]
    pub fn cell_bounds(self, column: u16, row: u16) -> Bounds<Pixels> {
        Bounds::new(
            self.origin
                + point(
                    self.cell.width * f32::from(column),
                    self.cell.height * f32::from(row),
                ),
            self.cell,
        )
    }

    /// The bounds of a run of columns on one row.
    #[must_use]
    pub fn span_bounds(self, columns: std::ops::Range<u16>, row: u16) -> Bounds<Pixels> {
        let width = self.cell.width * f32::from(columns.end.saturating_sub(columns.start));
        Bounds::new(
            self.origin
                + point(
                    self.cell.width * f32::from(columns.start),
                    self.cell.height * f32::from(row),
                ),
            size(width, self.cell.height),
        )
    }

    /// The top left of a row.
    #[must_use]
    pub fn row_origin(self, row: u16) -> Point<Pixels> {
        self.origin + point(px(0.0), self.cell.height * f32::from(row))
    }

    /// The bounds of the whole grid.
    #[must_use]
    pub fn grid_bounds(self) -> Bounds<Pixels> {
        Bounds::new(
            self.origin,
            size(
                self.cell.width * f32::from(self.columns),
                self.cell.height * f32::from(self.rows),
            ),
        )
    }

    /// The nearest cell boundary, for selection ends.
    #[must_use]
    pub fn selection_boundary_at(self, position: Point<Pixels>) -> Cell {
        Cell::new(
            axis_cell(
                position.x - self.origin.x + self.cell.width * 0.5,
                self.cell.width,
                self.columns,
            ),
            self.cell_at(position).row,
        )
    }

    /// The cell under a window position, clamped into the grid.
    #[must_use]
    pub fn cell_at(self, position: Point<Pixels>) -> Cell {
        Cell::new(
            axis_cell(
                position.x - self.origin.x,
                self.cell.width,
                self.columns.saturating_sub(1),
            ),
            axis_cell(
                position.y - self.origin.y,
                self.cell.height,
                self.rows.saturating_sub(1),
            ),
        )
    }
}

fn whole_cells(available: Pixels, cell: Pixels) -> u16 {
    if cell <= px(0.0) {
        return 1;
    }
    let count = (available / cell).floor();
    if !count.is_finite() || count < 1.0 {
        return 1;
    }
    if count >= f32::from(MAX_GRID) {
        return MAX_GRID;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "`count` is a whole, positive number below MAX_GRID"
    )]
    {
        count as u16
    }
}

fn axis_cell(offset: Pixels, cell: Pixels, last: u16) -> u16 {
    if cell <= px(0.0) {
        return 0;
    }
    let index = (offset / cell).floor();
    if !index.is_finite() || index < 0.0 {
        return 0;
    }
    if index >= f32::from(last) {
        return last;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "`index` is a whole, positive number below `last`"
    )]
    {
        index as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(padding: Pixels) -> Geometry {
        Geometry::new(
            Bounds::new(point(px(10.0), px(20.0)), size(px(108.0), px(84.0))),
            size(px(8.0), px(16.0)),
            padding,
            false,
        )
    }

    #[test]
    fn padding_moves_the_grid_and_reduces_the_usable_cells() {
        let unpadded = geometry(px(0.0));
        assert_eq!(unpadded.origin, point(px(10.0), px(20.0)));
        assert_eq!((unpadded.columns, unpadded.rows), (13, 5));
        let padded = geometry(px(6.0));
        assert_eq!(padded.origin, point(px(16.0), px(26.0)));
        assert_eq!((padded.columns, padded.rows), (12, 4));
    }

    #[test]
    fn a_tiny_element_still_reports_a_usable_grid() {
        let tiny = Geometry::new(Bounds::default(), size(px(8.0), px(16.0)), px(0.0), false);
        assert_eq!((tiny.columns, tiny.rows), (1, 1));
    }

    #[test]
    fn balancing_splits_the_spare_space_between_both_sides() {
        let balanced = Geometry::new(
            Bounds::new(point(px(0.0), px(0.0)), size(px(108.0), px(84.0))),
            size(px(8.0), px(16.0)),
            px(0.0),
            true,
        );
        assert_eq!((balanced.columns, balanced.rows), (13, 5));
        assert_eq!(balanced.origin, point(px(2.0), px(2.0)));
    }

    #[test]
    fn positions_map_to_cells_and_clamp_at_the_edges() {
        let geometry = geometry(px(0.0));
        assert_eq!(geometry.cell_at(point(px(10.0), px(20.0))), Cell::new(0, 0));
        assert_eq!(geometry.cell_at(point(px(18.0), px(36.0))), Cell::new(1, 1));
        assert_eq!(geometry.cell_at(point(px(0.0), px(0.0))), Cell::new(0, 0));
        assert_eq!(
            geometry.cell_at(point(px(9999.0), px(9999.0))),
            Cell::new(12, 4)
        );
        assert_eq!(
            geometry.selection_boundary_at(point(px(13.9), px(20.0))),
            Cell::new(0, 0)
        );
        assert_eq!(
            geometry.selection_boundary_at(point(px(14.1), px(20.0))),
            Cell::new(1, 0)
        );
        assert_eq!(
            geometry.selection_boundary_at(point(px(900.0), px(20.0))),
            Cell::new(13, 0)
        );
    }

    #[test]
    fn cell_and_span_bounds_line_up_with_the_grid() {
        let geometry = geometry(px(0.0));
        assert_eq!(
            geometry.cell_bounds(2, 1),
            Bounds::new(point(px(26.0), px(36.0)), size(px(8.0), px(16.0)))
        );
        assert_eq!(
            geometry.span_bounds(2..5, 1),
            Bounds::new(point(px(26.0), px(36.0)), size(px(24.0), px(16.0)))
        );
        assert_eq!(geometry.grid_bounds().size, size(px(104.0), px(80.0)));
    }
}
