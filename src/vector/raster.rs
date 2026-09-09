// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Polygon/line rasterization: a standalone vello_cpu kernel that turns already-projected
//! pixel-space geometry into a straight-alpha RGBA8 layer.
//!
//! This module does **no** projection/CRS work — `render.rs` (Task 7) hands it pixel-space
//! `[x, y]` coordinates and composites the result under the marker/label layer built by
//! `draw::Canvas`. The one piece of real domain logic here is the fill rule: polygons are
//! filled **even-odd**, not nonzero-winding, so a hole ring renders as a hole regardless of
//! whether the source data's exterior/hole rings follow the OGC right-hand-rule convention —
//! GeoJSON/SLD producers are not reliably consistent about this, and even-odd sidesteps needing
//! to detect/normalize ring orientation.
//!
//! **Renderer swapped tiny-skia -> vello_cpu on 2026-09-09.** Measured on four real eu5
//! production tiles, vello_cpu won 24 of 24 cases by 1.15x-4.39x, with the largest margin on the
//! layer that dominates the render budget (roads, 197.7 ms -> 72.1 ms). Full method and numbers:
//! `docs/renderer-benchmark-tinyskia-vs-vellocpu.md`. The public API of this module is unchanged —
//! no renderer type appears in any signature — so every caller was insulated from the swap.

use vello_cpu::color::{AlphaColor, Srgb};
use vello_cpu::kurbo::{BezPath, Cap, Join, Stroke};
use vello_cpu::peniko::Fill;
use vello_cpu::{Pixmap, RenderContext, Resources};

use super::style::PolygonSym;

/// An opaque-or-translucent solid colour. Anti-aliasing is vello_cpu's default (an aliasing
/// threshold of `None`), matching the `anti_alias: true` this module used under tiny-skia.
fn solid_color(color: [u8; 4]) -> AlphaColor<Srgb> {
    let [r, g, b, a] = color;
    AlphaColor::from_rgba8(r, g, b, a)
}

/// A round-capped, round-joined stroke of `width` px, as every stroke in this module has always
/// been (both the optional polygon outline and the line-layer stroke).
fn round_stroke(width: f32) -> Stroke {
    Stroke::new(width as f64)
        .with_caps(Cap::Round)
        .with_join(Join::Round)
}

/// Accumulates projected geometry into a base RGBA8 (straight-alpha) layer via vello_cpu.
///
/// vello_cpu composites into a **premultiplied** RGBA8 pixmap (required for correct source-over
/// blending as shapes are painted on top of each other); `into_straight_rgba` converts back to
/// straight alpha once, at the end, for the caller (`draw::Canvas` expects straight alpha,
/// matching PNG's expectation — see `draw.rs`'s own `into_rgba`).
pub struct GeomLayer {
    ctx: RenderContext,
    w: u16,
    h: u16,
}

impl GeomLayer {
    /// A transparent `w`×`h` canvas. Panics if `w` or `h` is 0, or exceeds 65535 — vello_cpu's
    /// surface dimensions are `u16`, and Task 7 never asks for a degenerate or absurd viewport.
    pub fn new(w: u32, h: u32) -> GeomLayer {
        assert!(
            w > 0 && h > 0,
            "GeomLayer::new: width/height must be non-zero"
        );
        let w: u16 = w
            .try_into()
            .expect("GeomLayer::new: width must fit in u16 (<= 65535)");
        let h: u16 = h
            .try_into()
            .expect("GeomLayer::new: height must fit in u16 (<= 65535)");
        GeomLayer {
            ctx: RenderContext::new(w, h),
            w,
            h,
        }
    }

    /// `rings`: exterior + holes (ring 0 = exterior, the rest holes), each a pixel-space `[x,
    /// y]` polygon boundary. All rings go into a single path and are filled **even-odd** in one
    /// call — this is what makes a hole ring subtract from the exterior regardless of winding
    /// direction. Rings with fewer than 3 points are degenerate (cannot enclose an area) and are
    /// skipped. If `sym.stroke` is set, every ring's boundary is additionally stroked at
    /// `sym.stroke_width`.
    pub fn fill_polygon(&mut self, rings: &[Vec<[f32; 2]>], sym: &PolygonSym) {
        let Some(path) = build_rings_path(rings) else {
            return;
        };

        self.ctx.set_paint(solid_color(sym.fill));
        self.ctx.set_fill_rule(Fill::EvenOdd);
        self.ctx.fill_path(&path);

        if let Some(stroke_color) = sym.stroke {
            self.ctx.set_paint(solid_color(stroke_color));
            self.ctx.set_stroke(round_stroke(sym.stroke_width));
            self.ctx.stroke_path(&path);
        }
    }

    /// `lines`: each a pixel-space `[x, y]` polyline, stroked with `stroke`/`width` (round cap +
    /// round join). All lines go into one path as separate open contours (no `close_path()`).
    /// Lines with fewer than 2 points are degenerate (nothing to stroke) and are skipped.
    pub fn stroke_lines(&mut self, lines: &[Vec<[f32; 2]>], stroke: [u8; 4], width: f32) {
        let mut bp = BezPath::new();
        let mut any = false;
        for line in lines {
            if line.len() < 2 {
                continue;
            }
            any = true;
            bp.move_to((line[0][0] as f64, line[0][1] as f64));
            for p in &line[1..] {
                bp.line_to((p[0] as f64, p[1] as f64));
            }
        }
        if !any {
            return;
        }

        self.ctx.set_paint(solid_color(stroke));
        self.ctx.set_stroke(round_stroke(width));
        self.ctx.stroke_path(&bp);
    }

    /// Demultiplied straight-alpha RGBA8 (`w*h*4`), ready to seed `draw::Canvas`'s buffer.
    ///
    /// Skipping this step is exactly the bug that produces a dark premultiplied-alpha fringe
    /// where Task 7 composites this layer under the markers/labels: a translucent pixel's stored
    /// RGB is `color * alpha`, not `color`, so compositing it again (Canvas's own `blend`, or a
    /// downstream PNG viewer) would double-apply the alpha.
    pub fn into_straight_rgba(mut self) -> Vec<u8> {
        self.ctx.flush();
        let mut pixmap = Pixmap::new(self.w, self.h);
        let mut resources = Resources::new();
        self.ctx.render(&mut pixmap, &mut resources);

        let mut out = Vec::with_capacity(self.w as usize * self.h as usize * 4);
        for px in pixmap.take_unpremultiplied() {
            out.push(px.r);
            out.push(px.g);
            out.push(px.b);
            out.push(px.a);
        }
        out
    }
}

/// Build a single path from `rings`: for each ring with >= 3 points, `move_to` the first vertex,
/// `line_to` the rest, then `close_path()`. Rings with < 3 points are skipped. `None` if the
/// resulting path has no contours at all (every ring skipped, or `rings` empty).
fn build_rings_path(rings: &[Vec<[f32; 2]>]) -> Option<BezPath> {
    let mut bp = BezPath::new();
    let mut any = false;
    for ring in rings {
        if ring.len() < 3 {
            continue;
        }
        any = true;
        bp.move_to((ring[0][0] as f64, ring[0][1] as f64));
        for p in &ring[1..] {
            bp.line_to((p[0] as f64, p[1] as f64));
        }
        bp.close_path();
    }
    if !any {
        return None;
    }
    Some(bp)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Straight-RGBA8 pixel at `(x, y)` in a `w`×`h` buffer produced by `into_straight_rgba`.
    fn px(rgba: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    #[test]
    fn fill_polygon_paints_interior_and_leaves_exterior_transparent() {
        let mut layer = GeomLayer::new(20, 20);
        let sym = PolygonSym {
            fill: [255, 0, 0, 255],
            stroke: None,
            stroke_width: 0.0,
        };
        // A single exterior ring — a 10x10 square from (5,5) to (15,15).
        let rings = vec![vec![[5.0, 5.0], [15.0, 5.0], [15.0, 15.0], [5.0, 15.0]]];
        layer.fill_polygon(&rings, &sym);
        let rgba = layer.into_straight_rgba();

        // Deep interior: opaque fill color.
        assert_eq!(px(&rgba, 20, 10, 10), [255, 0, 0, 255]);
        // Outside the square entirely: transparent.
        let outside = px(&rgba, 20, 1, 1);
        assert_eq!(
            outside[3], 0,
            "pixel outside the polygon must be transparent"
        );
    }

    #[test]
    fn fill_polygon_even_odd_leaves_hole_transparent() {
        let mut layer = GeomLayer::new(20, 20);
        let sym = PolygonSym {
            fill: [0, 255, 0, 255],
            stroke: None,
            stroke_width: 0.0,
        };
        let exterior = vec![[5.0, 5.0], [15.0, 5.0], [15.0, 15.0], [5.0, 15.0]];
        let hole = vec![[8.0, 8.0], [12.0, 8.0], [12.0, 12.0], [8.0, 12.0]];
        layer.fill_polygon(&[exterior, hole], &sym);
        let rgba = layer.into_straight_rgba();

        // Between the exterior boundary and the hole: filled.
        assert_eq!(px(&rgba, 20, 6, 6), [0, 255, 0, 255]);
        // Inside the hole: even-odd parity makes this "outside" — transparent.
        let hole_px = px(&rgba, 20, 10, 10);
        assert_eq!(
            hole_px[3], 0,
            "hole pixel must be transparent (even-odd fill)"
        );
    }

    #[test]
    fn stroke_lines_paints_on_line_and_leaves_far_pixels_transparent() {
        let mut layer = GeomLayer::new(20, 20);
        let lines = vec![vec![[5.0, 10.0], [15.0, 10.0]]];
        layer.stroke_lines(&lines, [0, 0, 255, 255], 3.0);
        let rgba = layer.into_straight_rgba();

        // On the line's centerline, well clear of the round-cap ends: full stroke color.
        assert_eq!(px(&rgba, 20, 10, 10), [0, 0, 255, 255]);
        // Far from the line (near the top edge of the canvas): transparent.
        let far = px(&rgba, 20, 10, 1);
        assert_eq!(
            far[3], 0,
            "pixel far from the stroked line must be transparent"
        );
    }

    #[test]
    fn into_straight_rgba_demultiplies_translucent_fill() {
        let mut layer = GeomLayer::new(20, 20);
        let sym = PolygonSym {
            fill: [200, 100, 50, 128], // ~50% alpha
            stroke: None,
            stroke_width: 0.0,
        };
        let rings = vec![vec![[5.0, 5.0], [15.0, 5.0], [15.0, 15.0], [5.0, 15.0]]];
        layer.fill_polygon(&rings, &sym);
        let rgba = layer.into_straight_rgba();

        let p = px(&rgba, 20, 10, 10);
        // The stored color must be the STRAIGHT (un-premultiplied) input color, not the
        // premultiplied value tiny-skia keeps internally (which would be ~[100, 50, 25, 128] —
        // color * alpha). A small tolerance absorbs u8 premultiply/demultiply rounding.
        assert!(
            (p[0] as i32 - 200).abs() <= 2,
            "red should demultiply back to ~200, got {}",
            p[0]
        );
        assert!(
            (p[1] as i32 - 100).abs() <= 2,
            "green should demultiply back to ~100, got {}",
            p[1]
        );
        assert!(
            (p[2] as i32 - 50).abs() <= 2,
            "blue should demultiply back to ~50, got {}",
            p[2]
        );
        assert!(
            (p[3] as i32 - 128).abs() <= 2,
            "alpha should stay ~128, got {}",
            p[3]
        );
    }

    #[test]
    fn degenerate_rings_and_lines_are_skipped_without_panicking() {
        let mut layer = GeomLayer::new(10, 10);
        let sym = PolygonSym {
            fill: [1, 2, 3, 255],
            stroke: None,
            stroke_width: 0.0,
        };
        // A 2-point "ring" and an empty ring set are both degenerate.
        layer.fill_polygon(&[vec![[1.0, 1.0], [2.0, 2.0]]], &sym);
        layer.fill_polygon(&[], &sym);
        // A 1-point "line" and an empty line set are both degenerate.
        layer.stroke_lines(&[vec![[1.0, 1.0]]], [0, 0, 0, 255], 1.0);
        layer.stroke_lines(&[], [0, 0, 0, 255], 1.0);

        let rgba = layer.into_straight_rgba();
        assert!(
            rgba.iter().all(|&b| b == 0),
            "canvas must stay fully transparent"
        );
    }
}
