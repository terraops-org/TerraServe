// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! GetLegendGraphic rendering: a pseudocolor ramp bar + numeric value labels, drawn with a tiny
//! built-in 5×7 bitmap font. We have no font engine (the label engine is a future piece), and a
//! legend only needs digits/dot/minus — so a hand-rolled numeric font is enough and dependency-free.
//! RGB-passthrough layers have no meaningful legend and return an error (surfaced as a WMS exception).

use crate::style::{self, Style};

const FONT_W: usize = 5;
const FONT_H: usize = 7;

/// 5×7 glyph for a legend character (digits, `.`, `-`). Each row's low 5 bits are the pixels,
/// MSB = leftmost. `None` for unsupported characters (they advance but draw nothing).
fn glyph(c: char) -> Option<[u8; 7]> {
    Some(match c {
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [31, 2, 4, 2, 1, 17, 14],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 30, 1, 1, 17, 14],
        '6' => [6, 8, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 2, 12],
        '.' => [0, 0, 0, 0, 0, 12, 12],
        '-' => [0, 0, 0, 31, 0, 0, 0],
        _ => return None,
    })
}

fn draw_text(buf: &mut [u8], cw: usize, ch: usize, x: usize, y: usize, s: &str, color: [u8; 4]) {
    let mut cx = x;
    for c in s.chars() {
        if let Some(g) = glyph(c) {
            for (ry, row) in g.iter().enumerate() {
                for bit in 0..FONT_W {
                    if row & (1 << (FONT_W - 1 - bit)) != 0 {
                        let (px, py) = (cx + bit, y + ry);
                        if px < cw && py < ch {
                            let o = (py * cw + px) * 4;
                            buf[o..o + 4].copy_from_slice(&color);
                        }
                    }
                }
            }
        }
        cx += FONT_W + 1;
    }
}

/// Compact numeric label: integers without a decimal, else 2 dp.
fn fmt_val(v: f64) -> String {
    if v.is_finite() && v == v.trunc() && v.abs() < 1e9 {
        format!("{}", v as i64)
    } else {
        format!("{v:.2}")
    }
}

/// Render a legend PNG for a style. Pseudocolor → a vertical ramp (max at top) + value labels;
/// RGB passthrough → `Err` (there's no meaningful legend for arbitrary RGB).
pub fn render_legend(style: &Style, width: u32, height: u32) -> Result<Vec<u8>, String> {
    let stops = match style {
        Style::Pseudocolor { stops, .. } => stops,
        Style::Rgb { .. } => {
            return Err("layer is not queryable for a legend (RGB passthrough)".to_string())
        }
    };
    if stops.is_empty() {
        return Err("style has no color stops".to_string());
    }
    let (w, h) = (width.max(1) as usize, height.max(1) as usize);
    let mut buf = vec![255u8; w * h * 4]; // white background

    let pad = 6usize;
    let barw = 22usize.min(w / 3).max(1);
    let bar_x0 = pad;
    let bar_y0 = pad;
    let bar_h = h.saturating_sub(2 * pad).max(1);
    let vmin = stops.first().unwrap().value;
    let vmax = stops.last().unwrap().value;

    // Color bar: top row = vmax, bottom = vmin.
    let denom = (bar_h.max(2) - 1) as f64;
    for row in 0..bar_h {
        let t = row as f64 / denom; // 0 at top
        let v = vmax + t * (vmin - vmax);
        let color = style::interpolate(stops, v);
        for col in 0..barw {
            let o = ((bar_y0 + row) * w + bar_x0 + col) * 4;
            buf[o..o + 4].copy_from_slice(&color);
        }
    }

    // Value labels + tick marks at each stop.
    let label_x = bar_x0 + barw + 5;
    for s in stops {
        let frac = if vmax > vmin {
            (vmax - s.value) / (vmax - vmin)
        } else {
            0.0
        };
        let py = bar_y0 + (frac * (bar_h.saturating_sub(1)) as f64).round() as usize;
        // Tick: a 3px black mark just right of the bar.
        for tx in 0..3usize {
            let (px, pyy) = (bar_x0 + barw + tx, py.min(h - 1));
            let o = (pyy * w + px.min(w - 1)) * 4;
            buf[o..o + 4].copy_from_slice(&[0, 0, 0, 255]);
        }
        let ty = py.saturating_sub(FONT_H / 2).min(h.saturating_sub(FONT_H));
        draw_text(
            &mut buf,
            w,
            h,
            label_x,
            ty,
            &fmt_val(s.value),
            [0, 0, 0, 255],
        );
    }

    crate::pngio::encode_rgba(&buf, width, height)
}

/// Fill a rectangle in a straight-alpha RGBA8 buffer (clipped to the buffer).
fn fill_rect(buf: &mut [u8], w: u32, h: u32, x: u32, y: u32, rw: u32, rh: u32, color: [u8; 4]) {
    for yy in y..(y + rh).min(h) {
        for xx in x..(x + rw).min(w) {
            let i = ((yy * w + xx) * 4) as usize;
            buf[i..i + 4].copy_from_slice(&color);
        }
    }
}

/// GetLegendGraphic for a **vector** rule-based style: one row per drawable rule — a colour swatch
/// (its first Polygon fill / Line stroke / Point fill) + the rule's `title` (SLD `<Title>`, else
/// `<Name>`), text shaped by the layer's font. Auto-sized; a large categorized style is capped so the
/// PNG stays sane. `Err` only if the style has no drawable rule at all.
pub fn render_vector_legend(
    style: &crate::vector::style::Style,
    shaper: &crate::vector::shape::Shaper,
) -> Result<Vec<u8>, String> {
    use crate::vector::style::Symbolizer;
    let mut entries: Vec<([u8; 4], String)> = Vec::new();
    for fts in &style.feature_type_styles {
        for rule in &fts.rules {
            let color = rule.symbolizers.iter().find_map(|s| match s {
                Symbolizer::Polygon(p) => Some(p.fill),
                Symbolizer::Line(l) => Some(l.stroke),
                Symbolizer::Point(p) => Some(p.fill),
                Symbolizer::Text(_) => None,
            });
            let Some(color) = color else { continue };
            entries.push((color, rule.title.clone().unwrap_or_default()));
        }
    }
    if entries.is_empty() {
        return Err("no legend entries (the style has no drawable rules)".into());
    }
    // A categorized style with hundreds of classes would be an unusably tall PNG — cap the rows.
    const MAX_ROWS: usize = 60;
    if entries.len() > MAX_ROWS {
        let more = entries.len() - MAX_ROWS;
        entries.truncate(MAX_ROWS);
        entries.push(([255, 255, 255, 0], format!("... (+{more} more classes)")));
    }

    let size_px = 13.0f32;
    let shaped: Vec<_> = entries
        .iter()
        .map(|(_, l)| shaper.shape(l, size_px))
        .collect();
    let (sw, gap, pad, row_h) = (18u32, 8u32, 8u32, 20u32);
    let text_w = shaped
        .iter()
        .map(|s| s.width.ceil() as u32)
        .max()
        .unwrap_or(60)
        .clamp(30, 500);
    let w = pad + sw + gap + text_w + pad;
    let h = pad + entries.len() as u32 * row_h + pad;

    // White background; fill each swatch.
    let mut buf = vec![255u8; (w * h * 4) as usize];
    for (i, (color, _)) in entries.iter().enumerate() {
        let y0 = pad + i as u32 * row_h + (row_h - 14) / 2;
        fill_rect(&mut buf, w, h, pad, y0, sw, 14, *color);
    }
    // Text via the shaper onto the seeded canvas.
    let mut canvas = crate::vector::draw::Canvas::from_rgba(w, h, &buf);
    let text_sym = crate::vector::style::TextSym {
        label: Vec::new(),
        priority: None,
        priority_higher_wins: false,
        size: size_px,
        color: [30, 30, 30, 255],
        halo_color: [0, 0, 0, 0],
        halo_radius: 0.0,
        offset: 0.0,
    };
    for (i, sl) in shaped.iter().enumerate() {
        let x = (pad + sw + gap) as f32;
        let y = (pad + i as u32 * row_h) as f32 + (row_h as f32 - sl.height) / 2.0;
        canvas.draw_label_body([x, y], sl, &text_sym);
    }
    crate::pngio::encode_rgba(&canvas.into_rgba(), w, h)
}
