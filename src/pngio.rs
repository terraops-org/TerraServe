// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! PNG encoding (RGBA8). Uses the allowed `png` crate.

/// Encode a flat RGBA8 buffer (row-major, width*height*4 bytes) to PNG bytes.
pub fn encode_rgba(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().map_err(|e| format!("png header: {e}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| format!("png data: {e}"))?;
    }
    Ok(out)
}

/// Flatten an RGBA8 buffer onto an opaque background color: each pixel is alpha-blended over `bg`
/// and made fully opaque. Used for WMS `TRANSPARENT=FALSE` (the default) with `BGCOLOR`.
pub fn composite_over_bg(rgba: &mut [u8], bg: [u8; 3]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u32;
        for c in 0..3 {
            px[c] = ((px[c] as u32 * a + bg[c] as u32 * (255 - a)) / 255) as u8;
        }
        px[3] = 255;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn composite_fills_transparent_with_bg_and_opaques() {
        // one fully-transparent pixel + one opaque red pixel
        let mut rgba = vec![0, 0, 0, 0, 255, 0, 0, 255];
        composite_over_bg(&mut rgba, [0, 0, 255]); // blue bg
        assert_eq!(rgba, vec![0, 0, 255, 255, 255, 0, 0, 255]);
    }
    #[test]
    fn composite_blends_half_alpha() {
        let mut rgba = vec![255, 255, 255, 128]; // 50% white over black bg
        composite_over_bg(&mut rgba, [0, 0, 0]);
        assert_eq!(rgba[3], 255);
        assert!((rgba[0] as i32 - 128).abs() <= 1);
    }
}
