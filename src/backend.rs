// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Render backend — a GRADED ARCHITECTURE REQUIREMENT (see CLAUDE.md, "GPU-max").
//!
//! Keep this trait **batch-first** and **buffer-oriented** so a future `wgpu` backend
//! is a *port, not a rewrite*. Ship `CpuBackend` (v1). Leave a `WgpuBackend` as a
//! documented stub — no GPU code in the pilot.
//!
//! Data-layout rules (checked by review):
//!   * pixel payloads are FLAT + POD (`bytemuck`-able), row-major, explicit strides —
//!     so a GPU upload is a zero-repack memcpy and CPU-SIMD also benefits later;
//!   * `warp` follows the texture-sampling model (per-output-pixel source coord +
//!     sampling mode); precompute the CRS transform on CPU into `WarpMap` and keep
//!     transcendental proj math OFF the per-pixel hot path;
//!   * keep `decode_tiles` / `warp` / `colorize` isolated so a later SIMD-intrinsics
//!     pass is a drop-in swap. No SIMD/assembly in the pilot — clean Rust.

use std::sync::Arc;

use rayon::prelude::*;

use crate::decode;

/// Resampling kernel.
#[derive(Copy, Clone, Debug)]
pub enum Resample {
    Nearest,
    Bilinear,
}

/// A compressed COG tile (its bytes already fetched via `RangeSource`), handed to the
/// decoder together with the metadata the decode needs.
pub struct CompressedTile {
    pub bytes: Vec<u8>,
    pub compression: u16,
    pub predictor: u16,
    pub tile_w: u32,
    pub tile_h: u32,
    pub samples: u16,
    pub bits_per_sample: u16,
    /// TIFF SampleFormat (tag 339): 1 = unsigned int, 2 = signed int, 3 = IEEE float.
    /// Used by the band-math decode path to interpret integer samples; the RGBA path
    /// (8-bit unsigned) ignores it.
    pub sample_format: u16,
    /// Byte order of multi-byte samples (from the TIFF header). Needed to read 16/32-bit
    /// samples in the band-math path; the 8-bit RGBA path is byte-order-agnostic.
    pub little_endian: bool,
    pub photometric: u16,
    pub jpeg_tables: Option<Arc<Vec<u8>>>,
    /// Position of this tile in the assembled window's tile grid (col, row).
    pub grid_col: u32,
    pub grid_row: u32,
    /// Whether this tile slot actually has data (false => fill transparent).
    pub present: bool,
}

/// Opaque, device-agnostic pixel buffer. The CPU backend holds bytes; a GPU backend
/// would hold a `wgpu::Buffer`/texture handle instead. Payload is flat + POD, row-major.
pub struct DeviceBuffer {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub channels: u8,
}

impl DeviceBuffer {
    pub fn new(width: u32, height: u32, channels: u8) -> Self {
        Self {
            data: vec![0u8; (width as usize) * (height as usize) * (channels as usize)],
            width,
            height,
            channels,
        }
    }
    #[inline]
    pub fn stride(&self) -> usize {
        self.width as usize * self.channels as usize
    }
}

/// A batch of device buffers (batch-first API — one dispatch maps to the whole batch).
pub struct DeviceBuffers(pub Vec<DeviceBuffer>);

/// Per-output-pixel source-coordinate mapping (the texture-sampling model). The CRS
/// transform is precomputed on CPU into this; the hot `warp` loop only samples. Coords
/// are in the assembled source-window's local pixel space (pixel-is-area: an integer+0.5
/// is a pixel center). `NaN` marks an output pixel with no valid source (=> transparent).
pub struct WarpMap {
    pub width: u32,
    pub height: u32,
    pub coords: Vec<[f64; 2]>, // len = width*height, [u, v] window-local, NaN = out of source
}

/// A color-ramp lookup table for `pseudocolor` styling (256 RGBA entries, value-indexed).
pub struct RampLut {
    pub lut: [[u8; 4]; 256],
    pub nodata_transparent: bool,
}

/// GRADED: batch-first render backend. Kernels take **batches**, not single tiles.
pub trait RenderBackend {
    fn decode_tiles(&self, tiles: &[CompressedTile]) -> DeviceBuffers;
    fn warp(&self, src: &DeviceBuffer, map: &WarpMap, mode: Resample) -> DeviceBuffer;
    fn colorize(&self, src: &DeviceBuffer, ramp: &RampLut) -> DeviceBuffer;
}

/// CPU implementation (v1). Clean Rust, hot paths isolated. No GPU/SIMD/assembly.
pub struct CpuBackend;

impl RenderBackend for CpuBackend {
    /// Decode a batch of compressed tiles into 4-channel RGBA device buffers. Tiles that
    /// are not present (past the image edge) come back fully transparent.
    fn decode_tiles(&self, tiles: &[CompressedTile]) -> DeviceBuffers {
        // Per-tile decode is independent and CPU-bound — run the batch in parallel across
        // rayon's pool. `par_iter().collect()` preserves input order, which the caller's
        // tile-to-slot mapping relies on. (Runs inside spawn_blocking, off the async reactor.)
        let out: Vec<DeviceBuffer> = tiles
            .par_iter()
            .map(|t| {
                if !t.present {
                    DeviceBuffer::new(t.tile_w, t.tile_h, 4)
                } else {
                    decode::decode_tile_rgba(t)
                }
            })
            .collect();
        DeviceBuffers(out)
    }

    /// Texture-sampling warp: for every output pixel, sample the source window at the
    /// precomputed source coordinate. Independent per-channel resampling. NaN source
    /// coords (outside the data) yield a fully transparent output pixel.
    fn warp(&self, src: &DeviceBuffer, map: &WarpMap, mode: Resample) -> DeviceBuffer {
        let ch = src.channels as usize;
        let sw = src.width as i64;
        let sh = src.height as i64;
        let sstride = src.stride();
        let mut out = DeviceBuffer::new(map.width, map.height, 4);
        let ostride = out.stride();

        // Estimate the global downsample scale (source pixels per output pixel) per axis,
        // so bilinear can widen its kernel when downsampling (GDAL-style anti-aliasing).
        let (scale_x, scale_y) = estimate_scale(map);
        let rx = scale_x.max(1.0);
        let ry = scale_y.max(1.0);

        let sample_nearest = |u: f64, v: f64, o: &mut [u8]| {
            let c = u.floor() as i64;
            let r = v.floor() as i64;
            if c < 0 || r < 0 || c >= sw || r >= sh {
                o.iter_mut().for_each(|x| *x = 0);
                return;
            }
            let base = r as usize * sstride + c as usize * ch;
            for k in 0..4 {
                o[k] = if k < ch { src.data[base + k] } else { 255 };
            }
        };

        // Separable tent (triangle) filter. Support radius = the downsample scale, so a
        // 1:1 map degenerates to the classic 2x2 bilinear and a downsample averages over
        // the source footprint with linear weights — matching GDAL's anti-aliased kernel.
        let sample_bilinear = |u: f64, v: f64, o: &mut [u8]| {
            let ci0 = (u - rx).floor() as i64;
            let ci1 = (u + rx).ceil() as i64;
            let ri0 = (v - ry).floor() as i64;
            let ri1 = (v + ry).ceil() as i64;
            let mut acc = [0.0f64; 4];
            let mut wsum = 0.0f64;
            let mut r = ri0;
            while r <= ri1 {
                let wy = 1.0 - ((r as f64 + 0.5 - v).abs() / ry);
                if wy > 0.0 {
                    let rc = r.max(0).min(sh - 1) as usize;
                    let mut c = ci0;
                    while c <= ci1 {
                        let wx = 1.0 - ((c as f64 + 0.5 - u).abs() / rx);
                        if wx > 0.0 {
                            let cc = c.max(0).min(sw - 1) as usize;
                            let w = wx * wy;
                            let base = rc * sstride + cc * ch;
                            for k in 0..ch {
                                acc[k] += w * src.data[base + k] as f64;
                            }
                            wsum += w;
                        }
                        c += 1;
                    }
                }
                r += 1;
            }
            if wsum > 0.0 {
                for k in 0..4 {
                    o[k] = if k < ch {
                        (acc[k] / wsum).round().max(0.0).min(255.0) as u8
                    } else {
                        255
                    };
                }
            } else {
                o.iter_mut().for_each(|x| *x = 0);
            }
        };

        for j in 0..map.height as usize {
            for i in 0..map.width as usize {
                let o = &mut out.data[j * ostride + i * 4..j * ostride + i * 4 + 4];
                let [u, v] = map.coords[j * map.width as usize + i];
                if u.is_nan() {
                    o.iter_mut().for_each(|x| *x = 0);
                    continue;
                }
                match mode {
                    Resample::Nearest => sample_nearest(u, v, o),
                    Resample::Bilinear => sample_bilinear(u, v, o),
                }
            }
        }
        out
    }

    /// Pseudocolor: map channel-0 value through the ramp LUT. Alpha comes from the source
    /// mask (channel 3) — transparent where the source is invalid.
    fn colorize(&self, src: &DeviceBuffer, ramp: &RampLut) -> DeviceBuffer {
        let ch = src.channels as usize;
        let mut out = DeviceBuffer::new(src.width, src.height, 4);
        let n = (src.width as usize) * (src.height as usize);
        for p in 0..n {
            let base = p * ch;
            let value = src.data[base];
            let mask_a = if ch >= 4 { src.data[base + 3] } else { 255 };
            let rgba = ramp.lut[value as usize];
            let o = p * 4;
            // Output alpha is binary: masked (invalid) source -> fully transparent,
            // otherwise the ramp's alpha. (Goldens use a hard mask, not a blended edge.)
            if ramp.nodata_transparent && mask_a < 128 {
                out.data[o..o + 4].copy_from_slice(&[0, 0, 0, 0]);
            } else {
                out.data[o] = rgba[0];
                out.data[o + 1] = rgba[1];
                out.data[o + 2] = rgba[2];
                out.data[o + 3] = rgba[3];
            }
        }
        out
    }
}

/// Estimate the mean source-pixels-per-output-pixel spacing along each axis from the
/// warp map (used to size the bilinear kernel). Falls back to 1.0 where undetermined.
fn estimate_scale(map: &WarpMap) -> (f64, f64) {
    let w = map.width as usize;
    let h = map.height as usize;
    let mut sx_sum = 0.0;
    let mut sx_n = 0u64;
    let mut sy_sum = 0.0;
    let mut sy_n = 0u64;
    let step_x = (w / 64).max(1);
    let step_y = (h / 64).max(1);
    for j in (0..h).step_by(step_y) {
        for i in (0..w.saturating_sub(1)).step_by(step_x) {
            let a = map.coords[j * w + i];
            let b = map.coords[j * w + i + 1];
            if a[0].is_finite() && b[0].is_finite() {
                sx_sum += (b[0] - a[0]).abs();
                sx_n += 1;
            }
        }
    }
    for j in (0..h.saturating_sub(1)).step_by(step_y) {
        for i in (0..w).step_by(step_x) {
            let a = map.coords[j * w + i];
            let b = map.coords[(j + 1) * w + i];
            if a[1].is_finite() && b[1].is_finite() {
                sy_sum += (b[1] - a[1]).abs();
                sy_n += 1;
            }
        }
    }
    let sx = if sx_n > 0 { sx_sum / sx_n as f64 } else { 1.0 };
    let sy = if sy_n > 0 { sy_sum / sy_n as f64 } else { 1.0 };
    (sx, sy)
}

/// Documented FUTURE backend — no GPU code in the pilot (see CLAUDE.md: CPU only).
pub struct WgpuBackend;

#[cfg(test)]
mod tests {
    use super::*;

    // Not-present tiles need no compressed bytes: each decodes to a transparent buffer.
    // Checks the parallel `decode_tiles` preserves order and length — the property the
    // render gather-loop relies on to map decoded tiles back to their grid slots.
    #[test]
    fn decode_tiles_preserves_order_and_len() {
        let mk = |w, h| CompressedTile {
            bytes: Vec::new(),
            compression: 8,
            predictor: 1,
            tile_w: w,
            tile_h: h,
            samples: 4,
            bits_per_sample: 8,
            sample_format: 1,
            little_endian: true,
            photometric: 2,
            jpeg_tables: None,
            grid_col: 0,
            grid_row: 0,
            present: false,
        };
        let tiles = vec![mk(4, 4), mk(8, 2), mk(1, 1)];
        let out = CpuBackend.decode_tiles(&tiles).0;
        assert_eq!(out.len(), 3);
        assert_eq!((out[0].width, out[0].height), (4, 4));
        assert_eq!((out[1].width, out[1].height), (8, 2));
        assert_eq!((out[2].width, out[2].height), (1, 1));
        assert!(out.iter().all(|b| b.data.iter().all(|&x| x == 0)));
    }
}
