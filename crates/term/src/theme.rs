//! Ghostty theme files.
//!
//! A Ghostty theme is a config file with only color keys, such as the files
//! under `<Ghostty resources>/themes` or `~/.config/ghostty/themes`. The
//! default [`TerminalColors`] are Ghostty's defaults, so a host that loads
//! no theme looks like a stock Ghostty.

use ghostty_vt::style::{Palette, RgbColor};

use crate::frame::{Rgb, TerminalColors};

/// A line a theme file could not be read from.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ThemeError {
    line: usize,
    text: String,
}

impl ThemeError {
    /// The 1-based line number.
    pub fn line(&self) -> usize {
        self.line
    }

    /// The offending line.
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl std::fmt::Display for ThemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: cannot parse {:?}", self.line, self.text)
    }
}

impl std::error::Error for ThemeError {}

fn rgb(color: RgbColor) -> Rgb {
    Rgb(color.r, color.g, color.b)
}

impl TerminalColors {
    /// Parse a Ghostty theme, starting from the defaults.
    ///
    /// Recognized keys: `palette`, `background`, `foreground`,
    /// `cursor-color`, `selection-background`, `selection-foreground`.
    /// Other keys are ignored so a full Ghostty config can be given too.
    /// Special values such as `cell-foreground` leave the default in place.
    pub fn from_ghostty_theme(text: &str) -> Result<Self, ThemeError> {
        let mut colors = Self::default();
        let mut palette = Palette(colors.palette.map(|c| RgbColor {
            r: c.0,
            g: c.1,
            b: c.2,
        }));
        for (index, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            let error = || ThemeError {
                line: index + 1,
                text: raw.to_owned(),
            };
            let color = || RgbColor::parse(value).ok();
            match key {
                "palette" => {
                    let (slot, color) = Palette::parse_palette_entry(value).map_err(|_| error())?;
                    palette.set(slot, color);
                }
                "background" => colors.background = rgb(color().ok_or_else(error)?),
                "foreground" => colors.foreground = rgb(color().ok_or_else(error)?),
                "cursor-color" => colors.cursor = color().map(rgb),
                "selection-background" => colors.selection_background = color().map(rgb),
                "selection-foreground" => colors.selection_foreground = color().map(rgb),
                _ => {}
            }
        }
        colors.palette = palette.0.map(rgb);
        Ok(colors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ghostty_theme_sets_palette_and_special_colors() {
        let theme = "\
# Dracula
palette = 0=#21222c
palette = 1=#ff5555
background = #282a36
foreground = f8f8f2
cursor-color = #f8f8f2
cursor-text = #282a36
selection-background = cell-foreground
window-padding-x = 10
";
        let colors = TerminalColors::from_ghostty_theme(theme).unwrap();
        assert_eq!(colors.palette[0], Rgb(0x21, 0x22, 0x2c));
        assert_eq!(colors.palette[1], Rgb(0xff, 0x55, 0x55));
        assert_eq!(colors.palette[2], TerminalColors::default().palette[2]);
        assert_eq!(colors.background, Rgb(0x28, 0x2a, 0x36));
        assert_eq!(colors.foreground, Rgb(0xf8, 0xf8, 0xf2));
        assert_eq!(colors.cursor, Some(Rgb(0xf8, 0xf8, 0xf2)));
        assert_eq!(colors.selection_background, None);
    }

    #[test]
    fn a_bad_line_reports_its_number() {
        let error =
            TerminalColors::from_ghostty_theme("background = #fff\npalette = x\n").unwrap_err();
        assert_eq!(error.line(), 2);
    }
}
