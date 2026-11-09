//! Design tokens: Herdr's graphite theme, sizes and fonts.
//!
//! Views use these constants, never raw hex. [`apply_to_kit`] pushes the same
//! colours into gpui-kit's theme so kit components (table, scrollbars) match.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Rgba, px};

const fn hex(v: u32) -> Rgba {
    Rgba {
        r: ((v >> 16) & 0xff) as f32 / 255.0,
        g: ((v >> 8) & 0xff) as f32 / 255.0,
        b: (v & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

const fn hexa(v: u32) -> Rgba {
    Rgba {
        r: ((v >> 24) & 0xff) as f32 / 255.0,
        g: ((v >> 16) & 0xff) as f32 / 255.0,
        b: ((v >> 8) & 0xff) as f32 / 255.0,
        a: (v & 0xff) as f32 / 255.0,
    }
}

/// Sidebar, tab bar and status bar.
pub const CHROME: Rgba = hex(0x0010_1114);
/// The work area behind panes.
pub const CANVAS: Rgba = hex(0x0013_1518);
/// Pane cards.
pub const PANE: Rgba = hex(0x0017_191d);
/// Menus and the palette.
pub const POPOVER: Rgba = hex(0x0025_272b);
/// The active tab.
pub const RAISED: Rgba = hex(0x002c_2e31);
/// Hover overlay.
pub const HOVER: Rgba = hexa(0xffff_ff0e);
/// Selected row overlay.
pub const SELECTED: Rgba = hexa(0xffff_ff17);
/// 1 px borders.
pub const BORDER: Rgba = hexa(0xffff_ff18);
/// Input borders.
pub const INPUT: Rgba = hexa(0xffff_ff24);
/// Primary text.
pub const TEXT: Rgba = hex(0x00ee_eeef);
/// Table cells and terminal text.
pub const TERM_TEXT: Rgba = hex(0x00d2_d4d9);
/// Secondary text.
pub const MUTED: Rgba = hex(0x0098_999b);
/// Labels, headers and hints.
pub const DIM: Rgba = hex(0x0069_6a6d);
/// Focused pane border, links.
pub const ACCENT: Rgba = hex(0x0096_b2d7);
/// Healthy.
pub const GREEN: Rgba = hex(0x00a6_bd91);
/// Pending, warnings.
pub const AMBER: Rgba = hex(0x00e5_c07b);
/// Failing, production.
pub const RED: Rgba = hex(0x00f0_705f);
/// Information.
pub const BLUE: Rgba = hex(0x0076_a8ff);
/// Fully transparent.
pub const CLEAR: Rgba = hexa(0x0000_0000);

/// Letter-tile colours for clusters, picked by name.
pub const TILE_COLORS: [Rgba; 6] = [
    hex(0x00d9_663a),
    hex(0x002a_9d8f),
    hex(0x006d_7fd1),
    hex(0x00b5_739d),
    hex(0x00b8_963e),
    hex(0x005f_9e6e),
];

/// Interface font.
pub const UI_FONT: &str = "Geist";
/// Font for names, numbers, logs and terminals.
pub const MONO_FONT: &str = "JetBrains Mono";

/// Base text size.
pub const TEXT_BASE: f32 = 13.0;
/// Section labels.
pub const TEXT_LABEL: f32 = 11.5;
/// Monospace cells.
pub const TEXT_MONO: f32 = 12.0;

/// Sidebar width.
pub const SIDEBAR_WIDTH: f32 = 252.0;
/// Tab bar height.
pub const TAB_BAR_HEIGHT: f32 = 42.0;
/// Status bar height.
pub const STATUS_BAR_HEIGHT: f32 = 28.0;
/// List and table row height.
pub const ROW_HEIGHT: f32 = 30.0;
/// Row highlight radius.
pub const ROW_RADIUS: f32 = 8.0;
/// Pane card radius.
pub const PANE_RADIUS: f32 = 10.0;
/// Pane header height.
pub const PANE_HEADER_HEIGHT: f32 = 30.0;
/// Gap between panes and around the work area.
pub const PANE_GAP: f32 = 8.0;

/// Pushes the graphite tokens into gpui-kit's theme. Call once at startup,
/// after `gpui_kit::init`.
pub fn apply_to_kit(cx: &mut App) {
    // Switch mode first: a mode change reloads the kit's default colours,
    // which would undo anything set in the same edit.
    Theme::change(ThemeMode::Dark, None, cx);
    Theme::update(cx, |theme| {
        theme.font_family = UI_FONT.into();
        theme.mono_font_family = MONO_FONT.into();
        theme.font_size = px(TEXT_BASE);
        theme.mono_font_size = px(TEXT_MONO);
        theme.radius = px(6.0);
        theme.radius_lg = px(PANE_RADIUS);
        theme.shadow = false;
        theme.focus_ring = false;

        let c = &mut theme.colors;
        c.background = CANVAS.into();
        c.foreground = TEXT.into();
        c.border = BORDER.into();
        c.input = INPUT.into();
        c.ring = ACCENT.into();
        c.accent = HOVER.into();
        c.accent_foreground = TEXT.into();
        c.muted = RAISED.into();
        c.muted_foreground = MUTED.into();
        c.popover = POPOVER.into();
        c.popover_foreground = TEXT.into();
        c.selection = hexa(0x96b2_d740).into();
        c.list = PANE.into();
        c.list_hover = HOVER.into();
        c.list_active = SELECTED.into();
        c.list_active_border = CLEAR.into();
        c.table = PANE.into();
        c.table_head = PANE.into();
        c.table_head_foreground = DIM.into();
        c.table_hover = HOVER.into();
        c.table_active = SELECTED.into();
        c.table_active_border = CLEAR.into();
        c.table_even = PANE.into();
        c.table_row_border = CLEAR.into();
        c.scrollbar = CLEAR.into();
        c.scrollbar_thumb = INPUT.into();
        c.scrollbar_thumb_hover = hexa(0xffff_ff38).into();
        c.sidebar = CHROME.into();
        c.sidebar_border = BORDER.into();
        c.sidebar_foreground = TERM_TEXT.into();
        c.status_bar = CHROME.into();
        c.status_bar_border = BORDER.into();
        c.title_bar = CHROME.into();
        c.title_bar_border = BORDER.into();
        c.tab_bar = CHROME.into();
        c.tab = CLEAR.into();
        c.tab_active = RAISED.into();
        c.tab_foreground = MUTED.into();
        c.tab_active_foreground = TEXT.into();
        c.success = GREEN.into();
        c.warning = AMBER.into();
        c.danger = RED.into();
        c.info = BLUE.into();
        c.green = GREEN.into();
        c.yellow = AMBER.into();
        c.red = RED.into();
        c.blue = BLUE.into();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parses_channels() {
        let c = hex(0x0017_191d);
        assert!((c.r - 0x17 as f32 / 255.0).abs() < f32::EPSILON);
        assert!((c.g - 0x19 as f32 / 255.0).abs() < f32::EPSILON);
        assert!((c.b - 0x1d as f32 / 255.0).abs() < f32::EPSILON);
        assert!((c.a - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn hexa_parses_alpha() {
        let c = hexa(0xffff_ff18);
        assert!((c.r - 1.0).abs() < f32::EPSILON);
        assert!((c.a - 0x18 as f32 / 255.0).abs() < f32::EPSILON);
    }
}
