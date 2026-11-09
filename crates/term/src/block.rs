//! Device-aligned quads for block element glyphs.
//!
//! Block elements are painted as quads rather than shaped glyphs because font
//! rasterization leaves visible seams between adjacent cells at fractional
//! cell sizes. Snapping each edge to a physical pixel makes neighbouring
//! cells tile.
//!
//! Adapted from Muxy v2 (MIT).

use gpui_kit::{Bounds, Hsla, Pixels, point, px};

/// Quads covering a block element glyph, or `None` when the text is not one.
pub(crate) fn quads(
    text: &str,
    bounds: Bounds<Pixels>,
    mut color: Hsla,
    scale: f32,
) -> Option<impl Iterator<Item = (Bounds<Pixels>, Hsla)>> {
    let eighths: &[[u8; 4]] = match text {
        "\u{2580}" => &[[0, 0, 8, 4]],
        "\u{2581}" => &[[0, 7, 8, 8]],
        "\u{2582}" => &[[0, 6, 8, 8]],
        "\u{2583}" => &[[0, 5, 8, 8]],
        "\u{2584}" => &[[0, 4, 8, 8]],
        "\u{2585}" => &[[0, 3, 8, 8]],
        "\u{2586}" => &[[0, 2, 8, 8]],
        "\u{2587}" => &[[0, 1, 8, 8]],
        "\u{2588}" | "\u{2591}" | "\u{2592}" | "\u{2593}" => &[[0, 0, 8, 8]],
        "\u{2589}" => &[[0, 0, 7, 8]],
        "\u{258a}" => &[[0, 0, 6, 8]],
        "\u{258b}" => &[[0, 0, 5, 8]],
        "\u{258c}" => &[[0, 0, 4, 8]],
        "\u{258d}" => &[[0, 0, 3, 8]],
        "\u{258e}" => &[[0, 0, 2, 8]],
        "\u{258f}" => &[[0, 0, 1, 8]],
        "\u{2590}" => &[[4, 0, 8, 8]],
        "\u{2594}" => &[[0, 0, 8, 1]],
        "\u{2595}" => &[[7, 0, 8, 8]],
        "\u{2596}" => &[[0, 4, 4, 8]],
        "\u{2597}" => &[[4, 4, 8, 8]],
        "\u{2598}" => &[[0, 0, 4, 4]],
        "\u{2599}" => &[[0, 0, 4, 4], [0, 4, 8, 8]],
        "\u{259a}" => &[[0, 0, 4, 4], [4, 4, 8, 8]],
        "\u{259b}" => &[[0, 0, 8, 4], [0, 4, 4, 8]],
        "\u{259c}" => &[[0, 0, 8, 4], [4, 4, 8, 8]],
        "\u{259d}" => &[[4, 0, 8, 4]],
        "\u{259e}" => &[[4, 0, 8, 4], [0, 4, 4, 8]],
        "\u{259f}" => &[[4, 0, 8, 4], [0, 4, 8, 8]],
        _ => return None,
    };
    color.a *= match text {
        "\u{2591}" => 64.0 / 255.0,
        "\u{2592}" => 128.0 / 255.0,
        "\u{2593}" => 192.0 / 255.0,
        _ => 1.0,
    };
    Some(
        eighths
            .iter()
            .filter_map(move |&[left, top, right, bottom]| {
                let x = |fraction: u8| {
                    snap(
                        bounds.left() + bounds.size.width * f32::from(fraction) / 8.0,
                        scale,
                    )
                };
                let y = |fraction: u8| {
                    snap(
                        bounds.top() + bounds.size.height * f32::from(fraction) / 8.0,
                        scale,
                    )
                };
                let quad = Bounds::from_corners(point(x(left), y(top)), point(x(right), y(bottom)));
                (quad.size.width > px(0.0) && quad.size.height > px(0.0)).then_some((quad, color))
            }),
    )
}

fn snap(value: Pixels, scale: f32) -> Pixels {
    px((f32::from(value) * scale).round() / scale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{rgb, size};

    fn shapes(text: &str, bounds: Bounds<Pixels>, scale: f32) -> Vec<Bounds<Pixels>> {
        quads(text, bounds, rgb(0xff_00_00).into(), scale)
            .expect("block glyph")
            .map(|(bounds, _)| bounds)
            .collect()
    }

    #[test]
    fn ordinary_text_is_not_treated_as_a_block_glyph() {
        for text in ["", " ", "a", "hello", "\u{754c}", "e\u{301}", "\u{2500}"] {
            assert!(
                quads(text, Bounds::default(), rgb(0).into(), 2.0).is_none(),
                "{text}"
            );
        }
    }

    #[test]
    fn adjacent_full_blocks_tile_without_seams_at_fractional_cell_sizes() {
        for scale in [1.0, 1.5, 2.0, 3.0] {
            for (width, height) in [(7.5, 15.5), (8.0, 17.0), (10.5, 23.0)] {
                let cell = size(px(width), px(height));
                let origin = point(px(0.3), px(-2.7));
                let first = shapes("\u{2588}", Bounds::new(origin, cell), scale)[0];
                let right = shapes(
                    "\u{2588}",
                    Bounds::new(origin + point(cell.width, px(0.0)), cell),
                    scale,
                )[0];
                let below = shapes(
                    "\u{2588}",
                    Bounds::new(origin + point(px(0.0), cell.height), cell),
                    scale,
                )[0];
                assert!(f32::from(first.right() - right.left()).abs() * scale < 0.0001);
                assert!(f32::from(first.bottom() - below.top()).abs() * scale < 0.0001);
                for bounds in [first, right, below] {
                    for edge in [bounds.left(), bounds.top(), bounds.right(), bounds.bottom()] {
                        let physical = f32::from(edge) * scale;
                        assert!((physical - physical.round()).abs() < 0.0001);
                    }
                }
            }
        }
    }

    #[test]
    fn halves_quadrants_and_shades_cover_the_expected_areas() {
        let cell = Bounds::new(point(px(0.0), px(0.0)), size(px(8.0), px(16.0)));
        assert_eq!(shapes("\u{258c}", cell, 1.0)[0].right(), px(4.0));
        assert_eq!(shapes("\u{2590}", cell, 1.0)[0].left(), px(4.0));
        assert_eq!(shapes("\u{2580}", cell, 1.0)[0].bottom(), px(8.0));
        assert_eq!(shapes("\u{2584}", cell, 1.0)[0].top(), px(8.0));
        assert_eq!(shapes("\u{2598}", cell, 1.0).len(), 1);
        assert_eq!(shapes("\u{2599}", cell, 1.0).len(), 2);
        let full = quads("\u{2588}", cell, rgb(0xff_00_00).into(), 1.0)
            .and_then(|mut q| q.next())
            .expect("quad");
        let light = quads("\u{2591}", cell, rgb(0xff_00_00).into(), 1.0)
            .and_then(|mut q| q.next())
            .expect("quad");
        assert_eq!(full.0, light.0);
        assert!(light.1.a < full.1.a);
    }
}
