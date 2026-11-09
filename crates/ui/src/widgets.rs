//! Small building blocks shared by the views.

use gpui_kit::{
    FontWeight, IntoElement, ParentElement as _, Rgba, SharedString, Styled as _, div, px, svg,
};
use model::Health;

use crate::assets::Icon;
use crate::theme;

/// An icon at `size` pixels in `color`.
pub(crate) fn icon(icon: Icon, size: f32, color: Rgba) -> impl IntoElement {
    svg()
        .path(icon.path())
        .flex_none()
        .size(px(size))
        .text_color(color)
}

/// A round status dot.
pub(crate) fn dot(color: Rgba, size: f32) -> impl IntoElement {
    div().flex_none().size(px(size)).rounded_full().bg(color)
}

/// A sentence-case section label: 11.5 px, medium, dim.
pub(crate) fn section_label(text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .h(px(30.0))
        .flex_none()
        .flex()
        .items_center()
        .px(px(16.0))
        .text_size(px(theme::TEXT_LABEL))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme::DIM)
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .child(text.into())
}

/// A small tinted chip, like Herdr's "Needs you" chips.
pub(crate) fn chip(text: impl Into<SharedString>, color: Rgba) -> impl IntoElement {
    let mut bg = color;
    bg.a = 0.15;
    div()
        .h(px(18.0))
        .px(px(6.0))
        .flex()
        .items_center()
        .rounded(px(5.0))
        .bg(bg)
        .text_size(px(10.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .whitespace_nowrap()
        .child(text.into())
}

/// Colour for a health class.
pub(crate) fn health_color(health: Health) -> Rgba {
    match health {
        Health::Good => theme::GREEN,
        Health::Pending => theme::AMBER,
        Health::Bad => theme::RED,
        Health::Done | Health::Unknown => theme::DIM,
    }
}

/// `s` when `n` is not one.
pub(crate) fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plural_suffix() {
        assert_eq!(plural(1), "");
        assert_eq!(plural(0), "s");
        assert_eq!(plural(2), "s");
    }
}
