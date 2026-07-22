// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Polygon/line rasterization: a standalone tiny-skia kernel that turns already-projected
//! pixel-space geometry into a straight-alpha RGBA8 layer.
//!
//! This module does **no** projection/CRS work — `render.rs` (Task 7) hands it pixel-space
//! `[x, y]` coordinates and composites the result under the marker/label layer built by
//! `draw::Canvas`. The one piece of real domain logic here is the fill rule: polygons are
//! filled **even-odd**, not nonzero-winding, so a hole ring renders as a hole regardless of
//! whether the source data's exterior/hole rings follow the OGC right-hand-rule convention —
//! GeoJSON/SLD producers are not reliably consistent about this, and even-odd sidesteps needing
//! to detect/normalize ring orientation.

use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Shader, Stroke, Transform,
};

use super::style::PolygonSym;

/// An anti-aliased, opaque-or-translucent solid-color `Paint`, built without the
/// `Default::default()`-then-reassign pattern (a struct literal keeps `Paint` immutable at the
/// call site — every paint here is used exactly once, for a single fill/stroke call).
fn solid_paint(color: [u8; 4]) -> Paint<'static> {
    let [r, g, b, a] = color;
    Paint {
        shader: Shader::SolidColor(Color::from_rgba8(r, g, b, a)),
        anti_alias: true,
        ..Paint::default()
    }
}

/// Accumulates projected geometry into a base RGBA8 (straight-alpha) layer via tiny-skia.
///
/// tiny-skia's `Pixmap` stores **premultiplied** RGBA8 internally (required for correct
/// source-over blending as shapes are painted on top of each other); `into_straight_rgba`
/// converts back to straight alpha once, at the end, for the caller (`draw::Canvas` expects
/// straight alpha, matching PNG's expectation — see `draw.rs`'s own `into_rgba`).
pub struct GeomLayer {
    pixmap: Pixmap,
}

impl GeomLayer {
    /// A transparent `w`×`h` canvas. Panics if `w` or `h` is 0 — tiny-skia rejects a zero-size
    /// pixmap and Task 7 never asks for a degenerate viewport.
    pub fn new(w: u32, h: u32) -> GeomLayer {
        GeomLayer {
            pixmap: Pixmap::new(w, h).expect("GeomLayer::new: width/height must be non-zero"),
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

        let fill_paint = solid_paint(sym.fill);
        self.pixmap.fill_path(
            &path,
            &fill_paint,
            FillRule::EvenOdd,
            Transform::identity(),
            None,
        );

        if let Some(stroke_color) = sym.stroke {
            let stroke_paint = solid_paint(stroke_color);
            let stroke = Stroke {
                width: sym.stroke_width,
                line_cap: LineCap::Round,
                line_join: LineJoin::Round,
                ..Default::default()
            };
            self.pixmap
                .stroke_path(&path, &stroke_paint, &stroke, Transform::identity(), None);
        }
    }

    /// `lines`: each a pixel-space `[x, y]` polyline, stroked with `stroke`/`width` (round cap +
    /// round join). All lines go into one path as separate open contours (no `close()`). Lines
    /// with fewer than 2 points are degenerate (nothing to stroke) and are skipped.
    pub fn stroke_lines(&mut self, lines: &[Vec<[f32; 2]>], stroke: [u8; 4], width: f32) {
        let mut pb = PathBuilder::new();
        let mut any = false;
        for line in lines {
            if line.len() < 2 {
                continue;
            }
            any = true;
            pb.move_to(line[0][0], line[0][1]);
            for p in &line[1..] {
                pb.line_to(p[0], p[1]);
            }
        }
        if !any {
            return;
        }
        let Some(path) = pb.finish() else {
            return;
        };

        let paint = solid_paint(stroke);
        let s = Stroke {
            width,
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Default::default()
        };
        self.pixmap
            .stroke_path(&path, &paint, &s, Transform::identity(), None);
    }

    /// Demultiplied straight-alpha RGBA8 (`w*h*4`), ready to seed `draw::Canvas`'s buffer.
    ///
    /// Skipping this step is exactly the bug that produces a dark premultiplied-alpha fringe
    /// where Task 7 composites this layer under the markers/labels: a translucent pixel's stored
    /// RGB is `color * alpha`, not `color`, so compositing it again (Canvas's own `blend`, or a
    /// downstream PNG viewer) would double-apply the alpha.
    pub fn into_straight_rgba(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pixmap.pixels().len() * 4);
        for px in self.pixmap.pixels() {
            let s = px.demultiply();
            out.push(s.red());
            out.push(s.green());
            out.push(s.blue());
            out.push(s.alpha());
        }
        out
    }
}

/// Build a single path from `rings`: for each ring with >= 3 points, `move_to` the first vertex,
/// `line_to` the rest, then `close()`. Rings with < 3 points are skipped. `None` if the
/// resulting path has no contours at all (every ring skipped, or `rings` empty).
fn build_rings_path(rings: &[Vec<[f32; 2]>]) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    let mut any = false;
    for ring in rings {
        if ring.len() < 3 {
            continue;
        }
        any = true;
        pb.move_to(ring[0][0], ring[0][1]);
        for p in &ring[1..] {
            pb.line_to(p[0], p[1]);
        }
        pb.close();
    }
    if !any {
        return None;
    }
    pb.finish()
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
