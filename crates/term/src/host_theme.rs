//! What the terminal takes from the application's theme.
//!
//! The grid's colors come from [`crate::TerminalColors`]. The rest of the
//! look, the font, the padding and the status pill, follows gpui-kit's
//! `Theme` global, which the application sets from its own design tokens.

use gpui_kit::component::Theme;
use gpui_kit::{App, Hsla, Pixels, SharedString, px};

/// Space between the pane's edge and the grid.
const PADDING: Pixels = px(6.);
/// Width of a bar cursor and height of an underline cursor.
const CURSOR_THICKNESS: Pixels = px(2.);
/// Opacity of the cursor drawn over the cell it covers.
const CURSOR_OPACITY: f32 = 0.55;

/// The theme values the terminal reads, copied out of the kit's theme.
#[derive(Clone, Debug)]
pub(crate) struct HostTheme {
    pub(crate) mono_font_family: SharedString,
    pub(crate) mono_font_size: Pixels,
    pub(crate) padding: Pixels,
    pub(crate) cursor_thickness: Pixels,
    pub(crate) cursor_opacity: f32,
    pub(crate) pill: Hsla,
    pub(crate) pill_text: Hsla,
    pub(crate) radius: Pixels,
    pub(crate) text_size: Pixels,
}

impl HostTheme {
    /// Reads the kit's theme.
    pub(crate) fn of(cx: &App) -> Self {
        let theme = Theme::global(cx);
        Self {
            mono_font_family: theme.mono_font_family.clone(),
            mono_font_size: theme.mono_font_size,
            padding: PADDING,
            cursor_thickness: CURSOR_THICKNESS,
            cursor_opacity: CURSOR_OPACITY,
            pill: theme.colors.popover,
            pill_text: theme.colors.popover_foreground,
            radius: theme.radius,
            text_size: theme.font_size,
        }
    }
}
