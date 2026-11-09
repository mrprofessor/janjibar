//! Resolve frame colors to GPUI colors.
//!
//! Derived from Herdr (Apache-2.0), in part from Muxy v2 (MIT).

use gpui_kit::{Hsla, rgb};

use crate::frame::{Color, Rgb, Style, TerminalColors};

/// Foreground and background resolution for one frame's palette.
#[derive(Clone, Copy)]
pub(crate) struct Palette<'a> {
    colors: &'a TerminalColors,
}

pub(crate) fn hsla(color: Rgb) -> Hsla {
    rgb((u32::from(color.0) << 16) | (u32::from(color.1) << 8) | u32::from(color.2)).into()
}

impl<'a> Palette<'a> {
    pub(crate) fn new(colors: &'a TerminalColors) -> Self {
        Self { colors }
    }

    pub(crate) fn background(self) -> Hsla {
        hsla(self.colors.background)
    }

    pub(crate) fn foreground(self) -> Hsla {
        hsla(self.colors.foreground)
    }

    pub(crate) fn cursor(self) -> Hsla {
        self.colors.cursor.map_or_else(|| self.foreground(), hsla)
    }

    /// The selection background: the configured one, or the default
    /// foreground, as Ghostty inverts the window colors when no selection
    /// color is set.
    pub(crate) fn selection(self) -> Hsla {
        self.colors
            .selection_background
            .map_or_else(|| self.foreground(), hsla)
    }

    /// The selection foreground: the configured one, or the default
    /// background, the other half of Ghostty's inversion.
    pub(crate) fn selection_foreground(self) -> Hsla {
        self.colors
            .selection_foreground
            .map_or_else(|| self.background(), hsla)
    }

    pub(crate) fn indexed(self, index: u8) -> Hsla {
        hsla(self.colors.palette[usize::from(index)])
    }

    fn resolve(self, color: Color, fallback: Hsla) -> Hsla {
        match color {
            Color::Default => fallback,
            Color::Indexed(index) => self.indexed(index),
            Color::Rgb(value) => hsla(value),
        }
    }

    /// Effective foreground and background for a run: inverse before faint,
    /// so an inverted faint cell dims the swapped foreground.
    pub(crate) fn style(self, style: &Style) -> (Hsla, Hsla) {
        let mut foreground = self.resolve(style.foreground, self.foreground());
        let mut background = self.resolve(style.background, self.background());
        if style.inverse {
            std::mem::swap(&mut foreground, &mut background);
        }
        if style.invisible {
            foreground = background;
        }
        if style.faint {
            foreground.a *= 0.5;
        }
        (foreground, background)
    }

    /// The underline color, falling back to the effective foreground.
    pub(crate) fn underline(self, style: &Style) -> Hsla {
        let (foreground, _) = self.style(style);
        self.resolve(style.underline_color, foreground)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> TerminalColors {
        TerminalColors::new(Rgb(200, 200, 200), Rgb(10, 10, 10))
    }

    #[test]
    fn default_colors_use_the_frame_foreground_and_background() {
        let colors = colors();
        let palette = Palette::new(&colors);
        let (foreground, background) = palette.style(&Style::default());
        assert_eq!(foreground, hsla(Rgb(200, 200, 200)));
        assert_eq!(background, hsla(Rgb(10, 10, 10)));
    }

    #[test]
    fn inverse_swaps_before_faint_dims_the_visible_foreground() {
        let colors = colors();
        let palette = Palette::new(&colors);
        let style = Style {
            inverse: true,
            faint: true,
            ..Style::default()
        };
        let (foreground, background) = palette.style(&style);
        assert_eq!(background, hsla(Rgb(200, 200, 200)));
        assert!(foreground.a < 1.0);
    }

    #[test]
    fn invisible_cells_paint_the_background_color_as_text() {
        let colors = colors();
        let palette = Palette::new(&colors);
        let style = Style {
            invisible: true,
            foreground: Color::Indexed(1),
            ..Style::default()
        };
        let (foreground, background) = palette.style(&style);
        assert_eq!(foreground, background);
    }

    #[test]
    fn an_unset_selection_inverts_the_window_colors_as_ghostty_does() {
        let colors = colors();
        let palette = Palette::new(&colors);
        assert_eq!(palette.selection(), hsla(Rgb(200, 200, 200)));
        assert_eq!(palette.selection_foreground(), hsla(Rgb(10, 10, 10)));
        let set = colors.with_selection(Rgb(1, 2, 3), Rgb(4, 5, 6));
        let palette = Palette::new(&set);
        assert_eq!(palette.selection(), hsla(Rgb(1, 2, 3)));
        assert_eq!(palette.selection_foreground(), hsla(Rgb(4, 5, 6)));
    }

    #[test]
    fn underline_color_falls_back_to_the_effective_foreground() {
        let colors = colors();
        let palette = Palette::new(&colors);
        let plain = Style {
            foreground: Color::Rgb(Rgb(1, 2, 3)),
            ..Style::default()
        };
        assert_eq!(palette.underline(&plain), hsla(Rgb(1, 2, 3)));
        let explicit = Style {
            underline_color: Color::Rgb(Rgb(9, 9, 9)),
            ..plain
        };
        assert_eq!(palette.underline(&explicit), hsla(Rgb(9, 9, 9)));
    }
}
