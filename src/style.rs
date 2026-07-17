// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Style parsing (`style.json`) and application. Two modes: `rgb` passthrough and
//! `pseudocolor` ramp. We hand-parse the small JSON to avoid pulling a JSON crate.

use crate::backend::{DeviceBuffer, RampLut};

#[derive(Debug)]
pub enum Style {
    Rgb {
        bands: [usize; 3], // 1-based band indices -> R,G,B
    },
    Pseudocolor {
        band: usize, // 1-based
        nodata_transparent: bool,
        stops: Vec<Stop>,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct Stop {
    pub value: f64,
    pub rgba: [u8; 4],
}

impl Style {
    pub fn load(path: &str) -> Result<Style, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("read style: {e}"))?;
        parse_style(&text)
    }

    /// Colorize a plane of derived values (e.g. NDVI in [-1, 1]) **directly** through the
    /// pseudocolor stops, keyed on the real value domain — no 0..255 LUT indirection. This
    /// is the band-math styling path: the same `[value, r,g,b,a]` stops as `style.json`, just
    /// with float values. A pixel is transparent when its value is non-finite (e.g. NDVI's
    /// 0/0) or `valid[i]` is false (nodata) and `nodata_transparent` is set. Flat RGBA out.
    pub fn colorize_values(&self, values: &[f32], valid: &[bool]) -> Result<Vec<u8>, String> {
        match self {
            Style::Pseudocolor {
                stops,
                nodata_transparent,
                ..
            } => {
                let n = values.len();
                let mut out = vec![0u8; n * 4];
                for i in 0..n {
                    let v = values[i];
                    let masked = *nodata_transparent && !valid.get(i).copied().unwrap_or(true);
                    if masked || !v.is_finite() {
                        continue; // leave [0,0,0,0] — transparent
                    }
                    out[i * 4..i * 4 + 4].copy_from_slice(&interpolate(stops, v as f64));
                }
                Ok(out)
            }
            _ => Err("colorize_values requires a pseudocolor style".into()),
        }
    }

    /// Resolve a `pseudocolor` ramp into a 256-entry RGBA LUT (value 0..255 -> RGBA).
    pub fn ramp_lut(&self) -> Option<RampLut> {
        match self {
            Style::Pseudocolor {
                stops,
                nodata_transparent,
                ..
            } => {
                let mut lut = [[0u8; 4]; 256];
                for v in 0..256 {
                    lut[v] = interpolate(stops, v as f64);
                }
                Some(RampLut {
                    lut,
                    nodata_transparent: *nodata_transparent,
                })
            }
            _ => None,
        }
    }
}

/// Apply the `rgb` style: reorder warped channels per `bands` and take alpha from the
/// source mask (channel 3 of the warped buffer). Returns a flat RGBA buffer.
pub fn apply_rgb(warped: &DeviceBuffer, bands: [usize; 3]) -> Vec<u8> {
    let ch = warped.channels as usize;
    let n = (warped.width as usize) * (warped.height as usize);
    let mut out = vec![0u8; n * 4];
    for p in 0..n {
        let base = p * ch;
        let o = p * 4;
        for k in 0..3 {
            let b = bands[k].saturating_sub(1); // 1-based -> 0-based
            out[o + k] = if b < ch { warped.data[base + b] } else { 0 };
        }
        out[o + 3] = if ch >= 4 { warped.data[base + 3] } else { 255 };
    }
    out
}

/// Linear RGBA interpolation between the two surrounding stops (clamped at the ends).
/// Linear RGBA interpolation between the two surrounding pseudocolor stops (clamped at the ends).
/// Public so the legend renderer samples the same ramp the render path uses.
pub fn interpolate(stops: &[Stop], v: f64) -> [u8; 4] {
    if stops.is_empty() {
        return [0, 0, 0, 255];
    }
    if v <= stops[0].value {
        return stops[0].rgba;
    }
    let last = stops.len() - 1;
    if v >= stops[last].value {
        return stops[last].rgba;
    }
    for i in 0..last {
        let a = &stops[i];
        let b = &stops[i + 1];
        if v >= a.value && v <= b.value {
            let t = if (b.value - a.value).abs() < 1e-12 {
                0.0
            } else {
                (v - a.value) / (b.value - a.value)
            };
            let mut out = [0u8; 4];
            for k in 0..4 {
                let val = a.rgba[k] as f64 + t * (b.rgba[k] as f64 - a.rgba[k] as f64);
                out[k] = val.round().max(0.0).min(255.0) as u8;
            }
            return out;
        }
    }
    stops[last].rgba
}

// ---------------------------------------------------------------------------
// Minimal JSON parsing for the fixed style shapes.
// ---------------------------------------------------------------------------

fn parse_style(text: &str) -> Result<Style, String> {
    let mode = json_string_field(text, "mode").ok_or("style: missing 'mode'")?;
    match mode.as_str() {
        "rgb" => {
            let bands_v = json_number_array(text, "bands").unwrap_or_else(|| vec![1.0, 2.0, 3.0]);
            let mut bands = [1usize, 2, 3];
            for (i, b) in bands_v.iter().take(3).enumerate() {
                bands[i] = *b as usize;
            }
            Ok(Style::Rgb { bands })
        }
        "pseudocolor" => {
            let band = json_number_field(text, "band").unwrap_or(1.0) as usize;
            let nodata_transparent = json_bool_field(text, "nodata_transparent").unwrap_or(true);
            let stops = parse_stops(text)?;
            Ok(Style::Pseudocolor {
                band,
                nodata_transparent,
                stops,
            })
        }
        other => Err(format!("style: unknown mode '{other}'")),
    }
}

fn parse_stops(text: &str) -> Result<Vec<Stop>, String> {
    // Find "stops" : [ [..],[..] ]
    let key = "\"stops\"";
    let ki = text.find(key).ok_or("style: missing 'stops'")?;
    let after = &text[ki + key.len()..];
    let lb = after.find('[').ok_or("style: malformed 'stops'")?;
    // Scan to matching outer ']'.
    let bytes = after.as_bytes();
    let mut depth = 0i32;
    let mut end = None;
    for (i, &c) in bytes.iter().enumerate().skip(lb) {
        match c {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end.ok_or("style: unterminated 'stops'")?;
    let inner = &after[lb + 1..end];
    let mut stops = Vec::new();
    // Each stop is a bracketed list of numbers.
    let ib = inner.as_bytes();
    let mut i = 0;
    while i < ib.len() {
        if ib[i] == b'[' {
            // find closing ]
            let mut j = i + 1;
            while j < ib.len() && ib[j] != b']' {
                j += 1;
            }
            let nums = parse_number_list(&inner[i + 1..j]);
            if nums.len() >= 5 {
                stops.push(Stop {
                    value: nums[0],
                    rgba: [
                        clamp_u8(nums[1]),
                        clamp_u8(nums[2]),
                        clamp_u8(nums[3]),
                        clamp_u8(nums[4]),
                    ],
                });
            } else if nums.len() == 4 {
                stops.push(Stop {
                    value: nums[0],
                    rgba: [clamp_u8(nums[1]), clamp_u8(nums[2]), clamp_u8(nums[3]), 255],
                });
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    if stops.is_empty() {
        return Err("style: no stops parsed".into());
    }
    stops.sort_by(|a, b| a.value.partial_cmp(&b.value).unwrap());
    Ok(stops)
}

fn clamp_u8(v: f64) -> u8 {
    v.round().max(0.0).min(255.0) as u8
}

fn parse_number_list(s: &str) -> Vec<f64> {
    s.split(',')
        .filter_map(|p| p.trim().parse::<f64>().ok())
        .collect()
}

fn json_string_field(text: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let ki = text.find(&pat)?;
    let after = &text[ki + pat.len()..];
    let colon = after.find(':')?;
    let rest = after[colon + 1..].trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    let inner = &rest[1..];
    let end = inner.find('"')?;
    Some(inner[..end].to_string())
}

fn json_number_field(text: &str, key: &str) -> Option<f64> {
    let pat = format!("\"{key}\"");
    let ki = text.find(&pat)?;
    let after = &text[ki + pat.len()..];
    let colon = after.find(':')?;
    let rest = after[colon + 1..].trim_start();
    let end = rest
        .find(|c: char| {
            !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E')
        })
        .unwrap_or(rest.len());
    rest[..end].parse::<f64>().ok()
}

fn json_bool_field(text: &str, key: &str) -> Option<bool> {
    let pat = format!("\"{key}\"");
    let ki = text.find(&pat)?;
    let after = &text[ki + pat.len()..];
    let colon = after.find(':')?;
    let rest = after[colon + 1..].trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn json_number_array(text: &str, key: &str) -> Option<Vec<f64>> {
    let pat = format!("\"{key}\"");
    let ki = text.find(&pat)?;
    let after = &text[ki + pat.len()..];
    let lb = after.find('[')?;
    let rb = after[lb..].find(']')? + lb;
    Some(parse_number_list(&after[lb + 1..rb]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ndvi_style() -> Style {
        // Classic red->yellow->green NDVI ramp keyed on the [-1, 1] value domain.
        Style::Pseudocolor {
            band: 1,
            nodata_transparent: true,
            stops: vec![
                Stop {
                    value: -1.0,
                    rgba: [165, 0, 38, 255],
                },
                Stop {
                    value: 0.0,
                    rgba: [255, 255, 191, 255],
                },
                Stop {
                    value: 1.0,
                    rgba: [0, 104, 55, 255],
                },
            ],
        }
    }

    #[test]
    fn colorize_values_hits_exact_stops() {
        let s = ndvi_style();
        let out = s
            .colorize_values(&[-1.0, 0.0, 1.0], &[true, true, true])
            .unwrap();
        assert_eq!(&out[0..4], &[165, 0, 38, 255]);
        assert_eq!(&out[4..8], &[255, 255, 191, 255]);
        assert_eq!(&out[8..12], &[0, 104, 55, 255]);
    }

    #[test]
    fn colorize_values_interpolates_midpoint() {
        // 0.5 is halfway between the 0.0 and 1.0 stops.
        let s = ndvi_style();
        let out = s.colorize_values(&[0.5], &[true]).unwrap();
        assert_eq!(out[0], 128); // (255+0)/2 rounded
        assert_eq!(out[1], 180); // (255+104)/2 = 179.5 -> 180
        assert_eq!(out[2], 123); // (191+55)/2 = 123
        assert_eq!(out[3], 255);
    }

    #[test]
    fn colorize_values_transparency() {
        let s = ndvi_style();
        // NaN (e.g. NDVI 0/0) is always transparent; masked-invalid is transparent.
        let out = s.colorize_values(&[f32::NAN, 0.5], &[true, false]).unwrap();
        assert_eq!(&out[0..4], &[0, 0, 0, 0]); // NaN
        assert_eq!(&out[4..8], &[0, 0, 0, 0]); // valid=false
    }

    #[test]
    fn colorize_values_clamps_out_of_domain() {
        // Values beyond the end stops clamp to the end colors (interpolate's behavior).
        let s = ndvi_style();
        let out = s.colorize_values(&[-5.0, 5.0], &[true, true]).unwrap();
        assert_eq!(&out[0..4], &[165, 0, 38, 255]); // clamped low
        assert_eq!(&out[4..8], &[0, 104, 55, 255]); // clamped high
    }
}
