// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Rasterization: anti-aliased markers + glyph compositing with a two-pass halo (spec §7/C4).
//!
//! The point MVP needs no 2D engine — markers are hand-rolled AA circles, glyphs are `swash`
//! coverage bitmaps, halos are a dilation of that coverage. Label compositing is **two-pass**
//! (all halos, then all bodies) so a glyph's halo never paints over an adjacent glyph's body.

use super::shape::{GlyphBlit, ShapedLabel};
use super::style::{PointSym, TextSym};

pub struct Canvas {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

impl Canvas {
    pub fn transparent(w: u32, h: u32) -> Canvas {
        Canvas {
            rgba: vec![0u8; (w as usize) * (h as usize) * 4],
            w,
            h,
        }
    }

    /// Seed the canvas from a **straight-alpha** RGBA8 base image (`w*h*4` bytes — e.g.
    /// `vector::raster::GeomLayer::into_straight_rgba`), premultiplying it into the canvas's
    /// internal representation (`blend`/`into_rgba` above: everything this struct stores is
    /// premultiplied, straight alpha only at the public boundary). This is the geometry-under-
    /// markers seam (Task 7): `render_vector` draws polygon/line fills+strokes into a `GeomLayer`
    /// first, then seeds a `Canvas` with the result before running the existing marker/label
    /// passes on top, so a translucent geometry edge composites correctly instead of leaving a
    /// dark premultiplied-alpha fringe. Panics if `base.len() != w*h*4`.
    pub fn from_rgba(w: u32, h: u32, base: &[u8]) -> Canvas {
        assert_eq!(
            base.len(),
            (w as usize) * (h as usize) * 4,
            "Canvas::from_rgba: base buffer size must be w*h*4"
        );
        let mut rgba = vec![0u8; base.len()];
        for (dst, src) in rgba.chunks_mut(4).zip(base.chunks(4)) {
            let a = src[3] as f32 / 255.0;
            for c in 0..3 {
                dst[c] = (src[c] as f32 * a).round().clamp(0.0, 255.0) as u8;
            }
            dst[3] = src[3];
        }
        Canvas { rgba, w, h }
    }

    /// Finish: convert the internally-accumulated **premultiplied** rgb to straight alpha (what
    /// PNG expects). Without this, a client compositing the transparent overlay gets a dark
    /// fringe around every AA marker edge and halo.
    pub fn into_rgba(mut self) -> Vec<u8> {
        for px in self.rgba.chunks_mut(4) {
            let a = px[3] as f32;
            if a > 0.0 {
                for c in 0..3 {
                    px[c] = ((px[c] as f32) * 255.0 / a).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
        self.rgba
    }

    /// Source-over composite of `color` at `(x,y)` with extra coverage `cov` in [0,1].
    fn blend(&mut self, x: i32, y: i32, color: [u8; 4], cov: f32) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return;
        }
        let a = (color[3] as f32 / 255.0) * cov;
        if a <= 0.0 {
            return;
        }
        let idx = ((y as u32 * self.w + x as u32) * 4) as usize;
        for c in 0..3 {
            let src = color[c] as f32;
            let dst = self.rgba[idx + c] as f32;
            self.rgba[idx + c] = (src * a + dst * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
        }
        let da = self.rgba[idx + 3] as f32 / 255.0;
        let oa = a + da * (1.0 - a);
        self.rgba[idx + 3] = (oa * 255.0).round().clamp(0.0, 255.0) as u8;
    }

    /// Anti-aliased filled circle + stroke ring, centred at `(cx,cy)`.
    pub fn draw_marker(&mut self, cx: f32, cy: f32, sym: &PointSym) {
        let r = sym.radius;
        let outer = r + sym.stroke_width;
        let x0 = (cx - outer - 1.0).floor() as i32;
        let x1 = (cx + outer + 1.0).ceil() as i32;
        let y0 = (cy - outer - 1.0).floor() as i32;
        let y1 = (cy + outer + 1.0).ceil() as i32;
        for y in y0..=y1 {
            for x in x0..=x1 {
                let dx = x as f32 + 0.5 - cx;
                let dy = y as f32 + 0.5 - cy;
                let d = (dx * dx + dy * dy).sqrt();
                if sym.stroke_width > 0.0 {
                    let stroke_cov = (outer - d + 0.5).clamp(0.0, 1.0);
                    if stroke_cov > 0.0 {
                        self.blend(x, y, sym.stroke, stroke_cov);
                    }
                }
                let fill_cov = (r - d + 0.5).clamp(0.0, 1.0);
                if fill_cov > 0.0 {
                    self.blend(x, y, sym.fill, fill_cov);
                }
            }
        }
    }

    /// Draw a placed label's HALO pass only. `origin` is the label **box top-left** (the placement
    /// box); the baseline sits `ascent` below it, so glyph ink (whose `dy` is baseline-relative)
    /// lands inside the reserved collision box.
    pub fn draw_label_halo(&mut self, origin: [f32; 2], label: &ShapedLabel, text: &TextSym) {
        let hr = text.halo_radius.max(0.0);
        if hr <= 0.0 {
            return;
        }
        let base = [origin[0], origin[1] + label.ascent];
        for g in &label.glyphs {
            self.blit_dilated(base, g, text.halo_color, hr);
        }
    }

    /// Draw a placed label's BODY (glyph) pass only. See `draw_label_halo` for the origin frame.
    pub fn draw_label_body(&mut self, origin: [f32; 2], label: &ShapedLabel, text: &TextSym) {
        let base = [origin[0], origin[1] + label.ascent];
        for g in &label.glyphs {
            self.blit(base, g, text.color);
        }
    }

    /// Draw one label (halo then body). For a *scene* with multiple labels prefer the global
    /// two-pass — `draw_label_halo` for all, then `draw_label_body` for all — so a later label's
    /// halo can't paint over an earlier label's body.
    pub fn draw_label(&mut self, origin: [f32; 2], label: &ShapedLabel, text: &TextSym) {
        self.draw_label_halo(origin, label, text);
        self.draw_label_body(origin, label, text);
    }

    fn blit(&mut self, origin: [f32; 2], g: &GlyphBlit, color: [u8; 4]) {
        let ox = (origin[0] + g.dx).round() as i32;
        let oy = (origin[1] + g.dy).round() as i32;
        for gy in 0..g.h {
            for gx in 0..g.w {
                let cov = g.coverage[(gy * g.w + gx) as usize];
                if cov > 0 {
                    self.blend(ox + gx as i32, oy + gy as i32, color, cov as f32 / 255.0);
                }
            }
        }
    }

    /// Composite a dilated (max-filtered) copy of the glyph coverage — the halo.
    fn blit_dilated(&mut self, origin: [f32; 2], g: &GlyphBlit, color: [u8; 4], radius: f32) {
        let rad = radius.ceil() as i32;
        let ox = (origin[0] + g.dx).round() as i32;
        let oy = (origin[1] + g.dy).round() as i32;
        let gw = g.w as i32;
        let gh = g.h as i32;
        let r2 = radius * radius;
        for py in -rad..(gh + rad) {
            for px in -rad..(gw + rad) {
                let mut m = 0u8;
                for dy in -rad..=rad {
                    for dx in -rad..=rad {
                        if (dx * dx + dy * dy) as f32 > r2 {
                            continue;
                        }
                        let sx = px + dx;
                        let sy = py + dy;
                        if sx >= 0 && sy >= 0 && sx < gw && sy < gh {
                            m = m.max(g.coverage[(sy as u32 * g.w + sx as u32) as usize]);
                        }
                    }
                }
                if m > 0 {
                    self.blend(ox + px, oy + py, color, m as f32 / 255.0);
                }
            }
        }
    }
}
