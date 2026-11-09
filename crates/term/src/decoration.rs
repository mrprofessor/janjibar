//! Underline, strikethrough and overline quads.
//!
//! Adapted from Muxy v2 (MIT).

use gpui_kit::{Bounds, Hsla, Pixels, point, px, size};

use crate::frame::{Style, Underline};

pub(crate) fn prepare(
    quads: &mut Vec<(Bounds<Pixels>, Hsla)>,
    style: &Style,
    bounds: Bounds<Pixels>,
    foreground: Hsla,
    mut color: Hsla,
    scale: f32,
) {
    let snap = |value: Pixels| px((f32::from(value) * scale).round() / scale);
    let thickness = px(1.0 / scale);
    let left = snap(bounds.left());
    let right = snap(bounds.right());
    let bottom = snap(bounds.bottom()) - thickness * 2.0;
    let mut line = |y, color| {
        quads.push((
            Bounds::new(point(left, y), size(right - left, thickness)),
            color,
        ));
    };
    if style.strikethrough {
        line(snap(bounds.top() + bounds.size.height / 2.0), foreground);
    }
    if style.overline {
        line(snap(bounds.top()), foreground);
    }
    color.a = foreground.a;
    match style.underline {
        Underline::None => {}
        Underline::Single => line(bottom, color),
        Underline::Double => {
            line(bottom, color);
            line(bottom - thickness * 2.0, color);
        }
        Underline::Curly | Underline::Dotted | Underline::Dashed => {
            let dashed = style.underline == Underline::Dashed;
            let curly = style.underline == Underline::Curly;
            let width = if dashed { 3.0 } else { 1.0 };
            let step = if curly { 1.0 } else { width + 1.0 };
            let mut x = left;
            while x < right {
                let phase = (f32::from(x) * scale).rem_euclid(4.0);
                let y = if curly {
                    bottom - thickness * (phase - 2.0).abs()
                } else {
                    bottom
                };
                quads.push((
                    Bounds::new(
                        point(x, y),
                        size((thickness * width).min(right - x), thickness),
                    ),
                    color,
                ));
                x += thickness * step;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quads(underline: Underline) -> Vec<(Bounds<Pixels>, Hsla)> {
        let mut quads = Vec::new();
        prepare(
            &mut quads,
            &Style {
                underline,
                ..Style::default()
            },
            Bounds::new(point(px(0.0), px(0.0)), size(px(16.0), px(16.0))),
            Hsla::white(),
            Hsla::white(),
            1.0,
        );
        quads
    }

    #[test]
    fn each_underline_style_has_its_own_shape() {
        assert_eq!(quads(Underline::None).len(), 0, "no underline, no quads");
        assert_eq!(quads(Underline::Single).len(), 1);
        assert_eq!(quads(Underline::Double).len(), 2);
        let curly = quads(Underline::Curly);
        assert!(curly.windows(2).any(|q| q[0].0.top() != q[1].0.top()));
        let dotted = quads(Underline::Dotted);
        let dashed = quads(Underline::Dashed);
        assert!(dotted.len() > dashed.len());
        assert!(dotted[0].0.size.width < dashed[0].0.size.width);
    }
}
