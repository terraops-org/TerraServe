// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Text shaping (harfrust) + glyph rasterization (swash) for the label engine.
//!
//! This is the plumbing layer of the label engine: turn a UTF-8 string into a set of
//! positioned 8-bit alpha-coverage glyph bitmaps that the placement/compositing stages
//! can blit onto the RGBA output buffer. It is *not* the differentiator — shaping and
//! rasterization are crate-backed. The bespoke work (placement, conflict resolution,
//! the Style IR) lives above this.
//!
//! Pipeline per `shape()` call:
//!   1. `harfrust` shapes the text at the font's UnitsPerEm (harfrust has no size concept —
//!      "Shaping is always using UnitsPerEm; you should scale the result manually"), giving
//!      glyph ids + advances + offsets + clusters.
//!   2. Advances/offsets are scaled to pixels via `size_px / units_per_em`.
//!   3. `swash` rasterizes each glyph id (same font bytes → glyph ids agree) at `size_px`
//!      to an 8-bit alpha mask.
//!
//! **Determinism**: each glyph is rasterized at a *quantized* (rounded to integer px) pen
//! origin, so the fractional subpixel offset handed to swash is always `(0, 0)`. Glyph
//! rasterization is otherwise a pure function of (outline, size, offset) — no time, no RNG —
//! so repeated calls with identical input produce byte-identical coverage bitmaps. The label
//! engine relies on this for reproducible tile output and for its glyph cache key.

use harfrust::{FontRef as HrFontRef, ShapeOptions, ShaperData, UnicodeBuffer};
use swash::scale::{Render, ScaleContext, Source};
use swash::zeno::{Format, Vector};
use swash::FontRef as SwFontRef;

/// A rasterized, positioned glyph.
///
/// `coverage` is a row-major, `w * h` 8-bit alpha mask (255 = fully inside the glyph).
/// `dx`/`dy` are the top-left corner of the bitmap in pixels, relative to the label origin
/// (label origin = left edge of the run, on the text baseline; +y points down). Most glyphs
/// sit above the baseline so `dy` is typically negative.
pub struct GlyphBlit {
    pub coverage: Vec<u8>,
    pub w: u32,
    pub h: u32,
    pub dx: f32,
    pub dy: f32,
}

/// The result of shaping one run of text at one size.
///
/// `width` is the total pen advance in px; `height` is the font's ascent+descent scaled to
/// `size_px`. `glyphs` holds one `GlyphBlit` per shaped glyph that has a non-empty bitmap
/// (glyphs with empty bitmaps, e.g. spaces, still advance the pen but are not emitted).
pub struct ShapedLabel {
    pub width: f32,
    pub height: f32,
    /// Distance from the label box top down to the text baseline (px). Glyph `dy` is
    /// baseline-relative, so a glyph's box-top-relative y is `ascent + dy`.
    pub ascent: f32,
    pub glyphs: Vec<GlyphBlit>,
}

/// Owns the font bytes and produces `ShapedLabel`s.
///
/// Holds the raw font bytes (not the parsed faces) because harfrust's `Shaper`/read-fonts'
/// `FontRef` and swash's `FontRef` all borrow the byte buffer, which would make a struct that
/// stores both bytes and parser self-referential. Re-parsing per `shape()` call keeps the
/// types simple and is cheap relative to shaping + rasterizing.
pub struct Shaper {
    font: Vec<u8>,
}

impl Shaper {
    /// Parse and validate a font from its raw bytes.
    ///
    /// Both the harfrust and swash parsers must accept the bytes here, so that the glyph ids
    /// harfrust emits are the same ids swash rasterizes (they share one byte buffer).
    pub fn from_font_bytes(bytes: &[u8]) -> Result<Shaper, String> {
        HrFontRef::new(bytes).map_err(|e| format!("harfrust: failed to parse font: {e:?}"))?;
        SwFontRef::from_index(bytes, 0).ok_or_else(|| "swash: failed to parse font".to_string())?;
        Ok(Shaper {
            font: bytes.to_vec(),
        })
    }

    /// Shape `text` at `size_px` and rasterize each glyph.
    pub fn shape(&self, text: &str, size_px: f32) -> ShapedLabel {
        // --- Shape with harfrust (positions come back in font design units) ---
        let hr_font = HrFontRef::new(&self.font).expect("validated in from_font_bytes");
        let data = ShaperData::new(&hr_font);
        let shaper = data.shaper(&hr_font).build();

        let upem = shaper.units_per_em() as f32;
        // font units -> pixels
        let scale = if upem > 0.0 { size_px / upem } else { 0.0 };

        let mut buffer = UnicodeBuffer::new();
        buffer.push_str(text);
        // Infer direction/script/language from the text (LTR Latin here).
        buffer.guess_segment_properties();
        let glyph_buffer = shaper.shape(buffer, ShapeOptions::new());

        let infos = glyph_buffer.glyph_infos();
        let positions = glyph_buffer.glyph_positions();

        // --- swash rasterizer at the target pixel size ---
        let sw_font = SwFontRef::from_index(&self.font, 0).expect("validated in from_font_bytes");

        // Line box height: ascent + descent scaled to size_px. swash stores descent as a
        // positive magnitude (it negates the raw hhea/OS2 descender), so this is the full
        // baseline-to-baseline box minus leading.
        let metrics = sw_font.metrics(&[]).scale(size_px);
        let height = metrics.ascent + metrics.descent;

        let mut ctx = ScaleContext::new();
        let mut scaler = ctx.builder(sw_font).size(size_px).hint(false).build();

        // Alpha (8-bit) mask, rendered at an integer origin (offset 0,0) for determinism.
        let sources = [Source::Outline];
        let mut render = Render::new(&sources);
        render.format(Format::Alpha).offset(Vector::new(0.0, 0.0));

        let mut glyphs = Vec::with_capacity(infos.len());
        let mut pen_x: f32 = 0.0;

        for (info, pos) in infos.iter().zip(positions.iter()) {
            let gid = info.glyph_id as u16;

            // Shaping offsets (kerning-via-mark etc.); ~0 for plain Latin but honored anyway.
            let x_off = pos.x_offset as f32 * scale;
            let y_off = pos.y_offset as f32 * scale;

            // Quantize the pen origin to an integer pixel. This is what makes the raster
            // deterministic: the fractional part (handed to swash as a subpixel offset) is
            // always 0, so the coverage bytes never depend on sub-pixel pen position.
            let origin_x = (pen_x + x_off).round();

            if let Some(image) = render.render(&mut scaler, gid) {
                let p = image.placement;
                // Skip glyphs with no ink (spaces): still advance the pen below.
                if p.width > 0 && p.height > 0 && !image.data.is_empty() {
                    // `p.left` is the left side bearing; `p.top` is the distance from the
                    // baseline UP to the top of the bitmap. Convert to a top-left offset in a
                    // y-down coordinate system anchored at the label origin (baseline).
                    let dx = origin_x + p.left as f32;
                    let dy = -(p.top as f32) + y_off;
                    glyphs.push(GlyphBlit {
                        coverage: image.data,
                        w: p.width,
                        h: p.height,
                        dx,
                        dy,
                    });
                }
            }

            // Advance the pen regardless of whether the glyph had ink.
            pen_x += pos.x_advance as f32 * scale;
        }

        ShapedLabel {
            width: pen_x,
            height,
            ascent: metrics.ascent,
            glyphs,
        }
    }
}
