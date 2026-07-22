// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Tile codecs. DEFLATE (+ horizontal predictor) is the required path; YCbCr-JPEG is
//! the optional stretch used by two bonus cases. Output is always a 4-channel RGBA
//! tile buffer (alpha = the source alpha band, or 255 when the source has no alpha).

use crate::backend::{CompressedTile, DeviceBuffer};

use flate2::read::ZlibDecoder;
use std::io::Read;

/// Decode one tile to a `tile_w × tile_h × 4` RGBA buffer.
pub fn decode_tile_rgba(t: &CompressedTile) -> DeviceBuffer {
    let tw = t.tile_w as usize;
    let th = t.tile_h as usize;
    let spp = t.samples as usize;

    let one_bit = t.bits_per_sample == 1;
    // Bytes per decoded row (TIFF pads each row to a byte boundary).
    let row_bytes = if one_bit {
        (tw * spp + 7) / 8
    } else {
        tw * spp
    };

    let raw: Vec<u8> = match t.compression {
        // 8 = Adobe/zlib DEFLATE, 32946 = also deflate.
        8 | 32946 => inflate(&t.bytes, row_bytes * th),
        // 5 = LZW.
        5 => lzw_decode(&t.bytes, row_bytes * th),
        // 7 = "new" JPEG, 6 = old JPEG.
        7 | 6 => decode_jpeg(t, tw, th, spp),
        1 => t.bytes.clone(),
        // 50000 = ZSTD (lossless) — like DEFLATE, uses the TIFF predictor.
        50000 => zstd_decompress(&t.bytes, row_bytes * th),
        // 50001 = WEBP — final RGB(A) like JPEG, no predictor.
        50001 => decode_webp(t, tw, th, spp),
        _ => vec![0u8; row_bytes * th],
    };

    // DEFLATE/LZW/ZSTD/raw use the TIFF predictor; JPEG and WEBP return already-final samples.
    let mut samples = if matches!(t.compression, 8 | 32946 | 5 | 1 | 50000) {
        let mut s = raw;
        if t.predictor == 2 && !one_bit {
            unpredict_horizontal(&mut s, tw, th, spp);
        }
        s
    } else {
        raw
    };

    // Expand 1-bit bilevel samples (e.g. GDAL internal masks) to one byte per pixel:
    // set bit -> 255, clear -> 0. TIFF is MSB-first per byte, rows byte-aligned.
    if one_bit {
        let mut expanded = vec![0u8; tw * th * spp];
        for row in 0..th {
            for i in 0..tw * spp {
                let bit_index = row * row_bytes * 8 + i;
                let byte = samples.get(bit_index / 8).copied().unwrap_or(0);
                let set = (byte >> (7 - (i % 8))) & 1;
                expanded[row * tw * spp + i] = if set == 1 { 255 } else { 0 };
            }
        }
        samples = expanded;
    }

    to_rgba(&samples, tw, th, spp)
}

fn inflate(bytes: &[u8], expected: usize) -> Vec<u8> {
    let mut dec = ZlibDecoder::new(bytes);
    let mut out = Vec::with_capacity(expected);
    if dec.read_to_end(&mut out).is_err() {
        // Fall back to raw DEFLATE (no zlib header) if the zlib wrapper is absent.
        let mut d2 = flate2::read::DeflateDecoder::new(bytes);
        out.clear();
        let _ = d2.read_to_end(&mut out);
    }
    if out.len() < expected {
        out.resize(expected, 0);
    }
    out
}

/// Decompress a single ZSTD frame (TIFF compression 50000) to `expected` bytes.
/// A codec — the COG container/IFD/tiling/windowed-read stays bespoke. Fail-soft:
/// on a decode error or short output, zero-pad to `expected` rather than panic.
fn zstd_decompress(bytes: &[u8], expected: usize) -> Vec<u8> {
    let mut out = zstd::stream::decode_all(bytes).unwrap_or_default();
    if out.len() < expected {
        out.resize(expected, 0);
    }
    out
}

/// Reverse the TIFF horizontal differencing predictor (predictor=2), 8-bit samples,
/// `spp` interleaved samples per pixel, per row.
fn unpredict_horizontal(buf: &mut [u8], width: usize, height: usize, spp: usize) {
    let row_len = width * spp;
    for row in 0..height {
        let base = row * row_len;
        for col in 1..width {
            for s in 0..spp {
                let idx = base + col * spp + s;
                let prev = buf[idx - spp];
                buf[idx] = buf[idx].wrapping_add(prev);
            }
        }
    }
}

/// Expand interleaved samples to RGBA. 4 samples => RGBA passthrough; 3 => RGB + opaque;
/// 1 => grayscale replicated + opaque.
fn to_rgba(samples: &[u8], width: usize, height: usize, spp: usize) -> DeviceBuffer {
    let mut out = DeviceBuffer::new(width as u32, height as u32, 4);
    let n = width * height;
    for p in 0..n {
        let si = p * spp;
        let di = p * 4;
        match spp {
            4 => out.data[di..di + 4].copy_from_slice(&samples[si..si + 4]),
            3 => {
                out.data[di..di + 3].copy_from_slice(&samples[si..si + 3]);
                out.data[di + 3] = 255;
            }
            1 => {
                out.data[di] = samples[si];
                out.data[di + 1] = samples[si];
                out.data[di + 2] = samples[si];
                out.data[di + 3] = 255;
            }
            _ => {
                for k in 0..3 {
                    out.data[di + k] = if k < spp { samples[si + k] } else { 0 };
                }
                out.data[di + 3] = 255;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Band-math decode path: de-interleaved, numeric (f32) band planes.
// ---------------------------------------------------------------------------

/// A decoded tile as separate band planes of `f32` samples — the de-interleaved, numeric
/// form band-math consumes (vs `decode_tile_rgba`'s RGBA form for direct display).
/// `bands[b][row * width + col]` is band `b`'s value at that pixel.
pub struct BandTile {
    pub width: u32,
    pub height: u32,
    pub bands: Vec<Vec<f32>>,
}

/// Decode one tile into per-band `f32` planes for band math. Handles 8- and 16-bit integer
/// samples (signed/unsigned via `sample_format`), DEFLATE (+ horizontal predictor), and
/// PIXEL-interleaved multi-band tiles. Band-math COGs are integer DEFLATE rasters; JPEG is a
/// display codec (8-bit YCbCr), not a band-math source, so it is not handled here.
/// Bytes per sample for `bits_per_sample` (1 for 8-bit, 2 for 16-bit).
#[inline]
pub fn bytes_per_sample(bits_per_sample: u16) -> usize {
    (bits_per_sample as usize + 7) / 8
}

/// Decode a tile to its raw NATIVE interleaved samples: inflate (+ horizontal predictor),
/// no dtype conversion. This is the **expensive** part (decompress + de-predict), and it is
/// what the band-math tile cache stores — **dense** (native width), unlike the f32 planes.
/// The cheap de-interleave-to-f32 is done per request by the caller (see `read_sample`).
pub fn decode_tile_samples(t: &CompressedTile) -> Vec<u8> {
    let tw = t.tile_w as usize;
    let th = t.tile_h as usize;
    let spp = t.samples as usize;
    let bps = bytes_per_sample(t.bits_per_sample);
    let row_bytes = tw * spp * bps;

    let mut raw: Vec<u8> = match t.compression {
        8 | 32946 => inflate(&t.bytes, row_bytes * th),
        5 => lzw_decode(&t.bytes, row_bytes * th),
        1 => t.bytes.clone(),
        50000 => zstd_decompress(&t.bytes, row_bytes * th),
        // JPEG/unknown: not a band-math source — yield zeros rather than misinterpret.
        _ => vec![0u8; row_bytes * th],
    };
    if raw.len() < row_bytes * th {
        raw.resize(row_bytes * th, 0);
    }
    match t.predictor {
        2 if bps == 2 => unpredict_horizontal_16(&mut raw, tw, th, spp, t.little_endian),
        2 => unpredict_horizontal(&mut raw, tw, th, spp),
        3 => unpredict_float(&mut raw, tw, th, spp, bps, t.little_endian),
        _ => {}
    }
    raw
}

/// Decode TIFF LZW (compression 5). A codec — the TIFF variant is MSB-first with the early
/// size switch. `weezl` is the codec (like `flate2` for DEFLATE); the COG container stays bespoke.
fn lzw_decode(bytes: &[u8], expected: usize) -> Vec<u8> {
    let mut dec = weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8);
    match dec.decode(bytes) {
        Ok(mut out) => {
            if out.len() < expected {
                out.resize(expected, 0);
            }
            out
        }
        Err(_) => vec![0u8; expected],
    }
}

/// Reverse the TIFF floating-point predictor (predictor=3): undo the byte-wise horizontal
/// differencing, then de-shuffle the (MSB-first) byte planes back into native-endian samples.
fn unpredict_float(
    buf: &mut [u8],
    width: usize,
    height: usize,
    spp: usize,
    bps: usize,
    little: bool,
) {
    let n = width * spp; // samples per row
    let row_bytes = n * bps;
    if row_bytes == 0 {
        return;
    }
    let mut tmp = vec![0u8; row_bytes];
    for r in 0..height {
        let base = r * row_bytes;
        if base + row_bytes > buf.len() {
            break;
        }
        let row = &mut buf[base..base + row_bytes];
        // (A) undo horizontal byte differencing (stride 1 across the whole row).
        for i in 1..row_bytes {
            row[i] = row[i].wrapping_add(row[i - 1]);
        }
        // (B) de-shuffle: plane `b` holds the b-th most-significant byte of every sample.
        for i in 0..n {
            for b in 0..bps {
                let plane_byte = row[b * n + i];
                let dst = i * bps + if little { bps - 1 - b } else { b };
                tmp[dst] = plane_byte;
            }
        }
        row.copy_from_slice(&tmp);
    }
}

pub fn decode_tile_bands(t: &CompressedTile) -> BandTile {
    let raw = decode_tile_samples(t);
    let tw = t.tile_w as usize;
    let th = t.tile_h as usize;
    let spp = t.samples as usize;
    let bps = bytes_per_sample(t.bits_per_sample);

    // De-interleave PIXEL-interleaved samples into f32 band planes.
    let n = tw * th;
    let mut bands = vec![vec![0f32; n]; spp];
    for p in 0..n {
        for (b, plane) in bands.iter_mut().enumerate() {
            let off = (p * spp + b) * bps;
            // f32 planes for now (lossless for the ≤16-bit + f32 sources this path handles;
            // f64-domain planes for u32/i32/f64 are a follow-up increment).
            plane[p] = read_sample(&raw, off, bps, t.sample_format, t.little_endian) as f32;
        }
    }
    BandTile {
        width: t.tile_w,
        height: t.tile_h,
        bands,
    }
}

/// TIFF sample data type — `SampleFormat` (tag 339: 1=uint, 2=int, 3=float) × `BitsPerSample`.
/// One classifier so the numeric pipeline can pick a lossless plane width per source dtype.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SampleType {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    F32,
    F64,
}

/// The numeric width a dtype needs to be represented **losslessly** (see `numeric_width`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NumWidth {
    F32,
    F64,
}

impl SampleType {
    /// Classify from the TIFF `SampleFormat` and `BitsPerSample`. Unknown/unspecified format
    /// falls back to unsigned int of the given width.
    pub fn from_tiff(sample_format: u16, bits_per_sample: u16) -> SampleType {
        match (sample_format, bits_per_sample) {
            (2, 8) => SampleType::I8,
            (2, 16) => SampleType::I16,
            (2, 32) => SampleType::I32,
            (3, 32) => SampleType::F32,
            (3, 64) => SampleType::F64,
            (_, 8) => SampleType::U8,
            (_, 16) => SampleType::U16,
            (_, 32) => SampleType::U32,
            _ => SampleType::U8,
        }
    }

    pub fn bytes(&self) -> usize {
        match self {
            SampleType::U8 | SampleType::I8 => 1,
            SampleType::U16 | SampleType::I16 => 2,
            SampleType::U32 | SampleType::I32 | SampleType::F32 => 4,
            SampleType::F64 => 8,
        }
    }

    /// The minimal **lossless** numeric width for this dtype: `f32` for ≤16-bit ints and `f32`
    /// (already exact); `f64` for 32-bit ints and `f64`, which `f32`'s 24-bit mantissa cannot
    /// hold exactly (> 2²⁴). This is the dtype-adaptive decision — NOT a universal funnel.
    pub fn numeric_width(&self) -> NumWidth {
        match self {
            SampleType::U32 | SampleType::I32 | SampleType::F64 => NumWidth::F64,
            _ => NumWidth::F32,
        }
    }
}

/// Read one sample at `off` as `f64` — the single dtype dispatch point. `f64` is lossless for
/// every supported dtype (all int widths ≤ 32 bits fit in 53-bit mantissa; f32/f64 exact), so
/// the *accessor* never loses precision; callers choose the plane width they store into.
#[inline]
pub fn read_sample(buf: &[u8], off: usize, bytes: usize, fmt: u16, little: bool) -> f64 {
    #[inline]
    fn get<const N: usize>(buf: &[u8], off: usize) -> [u8; N] {
        let mut a = [0u8; N];
        for (i, slot) in a.iter_mut().enumerate() {
            *slot = buf.get(off + i).copied().unwrap_or(0);
        }
        a
    }
    match bytes {
        1 => {
            let b = buf.get(off).copied().unwrap_or(0);
            if fmt == 2 {
                (b as i8) as f64
            } else {
                b as f64
            }
        }
        2 => {
            let a = get::<2>(buf, off);
            let raw = if little {
                u16::from_le_bytes(a)
            } else {
                u16::from_be_bytes(a)
            };
            if fmt == 2 {
                (raw as i16) as f64
            } else {
                raw as f64
            }
        }
        4 => {
            let a = get::<4>(buf, off);
            match fmt {
                3 => {
                    (if little {
                        f32::from_le_bytes(a)
                    } else {
                        f32::from_be_bytes(a)
                    }) as f64
                }
                2 => {
                    (if little {
                        i32::from_le_bytes(a)
                    } else {
                        i32::from_be_bytes(a)
                    }) as f64
                }
                _ => {
                    (if little {
                        u32::from_le_bytes(a)
                    } else {
                        u32::from_be_bytes(a)
                    }) as f64
                }
            }
        }
        8 => {
            let a = get::<8>(buf, off);
            match fmt {
                3 => {
                    if little {
                        f64::from_le_bytes(a)
                    } else {
                        f64::from_be_bytes(a)
                    }
                }
                2 => {
                    (if little {
                        i64::from_le_bytes(a)
                    } else {
                        i64::from_be_bytes(a)
                    }) as f64
                }
                _ => {
                    (if little {
                        u64::from_le_bytes(a)
                    } else {
                        u64::from_be_bytes(a)
                    }) as f64
                }
            }
        }
        _ => 0.0,
    }
}

/// Reverse the TIFF horizontal predictor for 16-bit integer samples: `spp` interleaved
/// samples per pixel, per row, stored in the file's byte order (`little`). The wrapping add
/// is on the raw u16 bit pattern — identical result for signed/unsigned two's-complement.
fn unpredict_horizontal_16(buf: &mut [u8], width: usize, height: usize, spp: usize, little: bool) {
    let row_len = width * spp * 2;
    for row in 0..height {
        let base = row * row_len;
        for col in 1..width {
            for s in 0..spp {
                let idx = base + (col * spp + s) * 2;
                let prev = load16(buf, idx - spp * 2, little);
                let cur = load16(buf, idx, little);
                store16(buf, idx, cur.wrapping_add(prev), little);
            }
        }
    }
}

#[inline]
fn load16(buf: &[u8], i: usize, little: bool) -> u16 {
    let (a, b) = (buf[i], buf[i + 1]);
    if little {
        u16::from_le_bytes([a, b])
    } else {
        u16::from_be_bytes([a, b])
    }
}

#[inline]
fn store16(buf: &mut [u8], i: usize, v: u16, little: bool) {
    let bytes = if little {
        v.to_le_bytes()
    } else {
        v.to_be_bytes()
    };
    buf[i] = bytes[0];
    buf[i + 1] = bytes[1];
}

/// Decode a self-contained WEBP tile (TIFF compression 50001) to interleaved RGB(A)
/// samples matching the tile's `spp`. Mirrors `decode_jpeg`: returns already-final
/// samples (no TIFF predictor). WEBP tiles in TIFF are complete RIFF WebP files (no
/// shared-tables assembly). Fail-soft: on any error, return zeros of the expected size.
fn decode_webp(t: &CompressedTile, tw: usize, th: usize, spp: usize) -> Vec<u8> {
    use std::io::Cursor;
    let fallback = || vec![0u8; tw * th * spp];
    let mut dec = match image_webp::WebPDecoder::new(Cursor::new(&t.bytes)) {
        Ok(d) => d,
        Err(_) => return fallback(),
    };
    let ch = if dec.has_alpha() { 4usize } else { 3usize };
    let mut buf = vec![0u8; tw * th * ch];
    if dec.read_image(&mut buf).is_err() {
        return fallback();
    }
    // Reconcile the WebP's native channel count to the tile's declared `spp`. GDAL/libtiff
    // WEBP COGs frequently declare spp=4 (RGBA) yet encode only RGB in the WebP bitstream
    // (the alpha/mask lives outside the tile), so a WebP that decodes to 3 channels must be
    // padded to spp with an opaque (255) alpha, not rejected. `to_rgba` reads `spp`/pixel.
    if ch == spp {
        return buf;
    }
    let n = tw * th;
    let mut out = vec![0u8; n * spp];
    for p in 0..n {
        for c in 0..spp {
            out[p * spp + c] = if c < ch { buf[p * ch + c] } else { 255 };
        }
    }
    out
}

/// Decode a (possibly abbreviated) tiled JPEG. If the COG carries shared `JPEGTables`,
/// splice them ahead of the tile's scan data to form a complete JPEG stream.
fn decode_jpeg(t: &CompressedTile, tw: usize, th: usize, _spp: usize) -> Vec<u8> {
    let stream = assemble_jpeg(&t.bytes, t.jpeg_tables.as_deref().map(|v| v.as_slice()));
    let mut dec = zune_jpeg::JpegDecoder::new(&stream);
    match dec.decode() {
        Ok(pixels) => {
            // zune-jpeg returns interleaved RGB (3 channels) for YCbCr JPEGs.
            let expected3 = tw * th * 3;
            if pixels.len() >= expected3 {
                pixels
            } else {
                let mut v = pixels;
                v.resize(expected3, 0);
                v
            }
        }
        Err(_) => vec![0u8; tw * th * 3],
    }
}

/// Build a full JPEG stream from abbreviated tile data + shared tables. Tables stream is
/// `SOI ... tables ... EOI`; tile is `SOI ... SOF/SOS ... EOI`. We concatenate the tables
/// (without trailing EOI) followed by the tile body (without leading SOI).
fn assemble_jpeg(tile: &[u8], tables: Option<&[u8]>) -> Vec<u8> {
    match tables {
        Some(tbl) if tbl.len() >= 2 && tile.len() >= 2 => {
            let tbl_body = if tbl.ends_with(&[0xFF, 0xD9]) {
                &tbl[..tbl.len() - 2]
            } else {
                tbl
            };
            let tile_body = if tile.starts_with(&[0xFF, 0xD8]) {
                &tile[2..]
            } else {
                tile
            };
            let mut out = Vec::with_capacity(tbl_body.len() + tile_body.len());
            out.extend_from_slice(tbl_body);
            out.extend_from_slice(tile_body);
            out
        }
        _ => tile.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int16_tile(bytes: Vec<u8>, spp: u16, tw: u32, th: u32, predictor: u16) -> CompressedTile {
        CompressedTile {
            bytes,
            compression: 1, // raw — we control the sample bytes directly
            predictor,
            tile_w: tw,
            tile_h: th,
            samples: spp,
            bits_per_sample: 16,
            sample_format: 2, // signed int (Sentinel-2 Int16)
            little_endian: true,
            photometric: 1,
            jpeg_tables: None,
            grid_col: 0,
            grid_row: 0,
            present: true,
        }
    }

    #[test]
    fn deinterleaves_signed_int16_bands() {
        // 2x1 tile, 2 bands, little-endian signed int16, no predictor.
        // pixel0: band0=1000, band1=-2000 ; pixel1: band0=32000, band1=-1
        let mut bytes = Vec::new();
        for v in [1000i16, -2000, 32000, -1] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let bt = decode_tile_bands(&int16_tile(bytes, 2, 2, 1, 1));
        assert_eq!(bt.bands.len(), 2);
        assert_eq!(bt.bands[0], vec![1000.0, 32000.0]); // band 0 across the two pixels
        assert_eq!(bt.bands[1], vec![-2000.0, -1.0]); // band 1 — negatives preserved
    }

    #[test]
    fn reverses_16bit_horizontal_predictor() {
        // predictor=2 stores per-band horizontal differences. Target values:
        //   band0 = [100, 150], band1 = [200, 190]
        // Interleaved differences: pixel0 [100, 200], pixel1 [150-100=50, 190-200=-10].
        let mut bytes = Vec::new();
        for v in [100i16, 200, 50, -10] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let bt = decode_tile_bands(&int16_tile(bytes, 2, 2, 1, 2));
        assert_eq!(bt.bands[0], vec![100.0, 150.0]);
        assert_eq!(bt.bands[1], vec![200.0, 190.0]);
    }

    #[test]
    fn ndvi_arithmetic_from_decoded_bands() {
        // Sanity: the decoded planes support the NDVI ratio directly (Red=band0, NIR=band1).
        // Red=2000, NIR=6000 -> (6000-2000)/(6000+2000) = 0.5
        let mut bytes = Vec::new();
        for v in [2000i16, 6000i16] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let bt = decode_tile_bands(&int16_tile(bytes, 2, 1, 1, 1));
        let (red, nir) = (bt.bands[0][0], bt.bands[1][0]);
        assert!(((nir - red) / (nir + red) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn sample_type_classification_and_width() {
        assert_eq!(SampleType::from_tiff(1, 8), SampleType::U8);
        assert_eq!(SampleType::from_tiff(2, 8), SampleType::I8);
        assert_eq!(SampleType::from_tiff(1, 16), SampleType::U16);
        assert_eq!(SampleType::from_tiff(2, 16), SampleType::I16);
        assert_eq!(SampleType::from_tiff(1, 32), SampleType::U32);
        assert_eq!(SampleType::from_tiff(2, 32), SampleType::I32);
        assert_eq!(SampleType::from_tiff(3, 32), SampleType::F32);
        assert_eq!(SampleType::from_tiff(3, 64), SampleType::F64);
        // Dtype-adaptive: f32 for ≤16-bit + f32; f64 only for 32-bit int + f64.
        for t in [
            SampleType::U8,
            SampleType::I8,
            SampleType::U16,
            SampleType::I16,
            SampleType::F32,
        ] {
            assert_eq!(
                t.numeric_width(),
                NumWidth::F32,
                "{t:?} should be f32-domain"
            );
        }
        for t in [SampleType::U32, SampleType::I32, SampleType::F64] {
            assert_eq!(
                t.numeric_width(),
                NumWidth::F64,
                "{t:?} should be f64-domain"
            );
        }
    }

    #[test]
    fn read_sample_roundtrips_every_dtype() {
        assert_eq!(read_sample(&[200u8], 0, 1, 1, true), 200.0); // u8
        assert_eq!(read_sample(&[(-50i8) as u8], 0, 1, 2, true), -50.0); // i8
        assert_eq!(read_sample(&40000u16.to_le_bytes(), 0, 2, 1, true), 40000.0); // u16
        assert_eq!(
            read_sample(&(-9999i16).to_le_bytes(), 0, 2, 2, true),
            -9999.0
        ); // i16 (nodata)
        assert_eq!(
            read_sample(&3_000_000_000u32.to_le_bytes(), 0, 4, 1, true),
            3_000_000_000.0
        ); // u32
        assert_eq!(read_sample(&3744.5f32.to_le_bytes(), 0, 4, 3, true), 3744.5); // f32 (DEM elev)
        assert_eq!(
            read_sample(&(-1234.5678f64).to_le_bytes(), 0, 8, 3, true),
            -1234.5678
        ); // f64
           // Big-endian path too.
        assert_eq!(
            read_sample(&(-9999i16).to_be_bytes(), 0, 2, 2, false),
            -9999.0
        );
    }

    #[test]
    fn read_sample_int32_precision_guard() {
        // The point of f64: an Int32 value > 2^24 must survive — f32 would corrupt it.
        let big: i32 = 20_000_003; // > 16_777_216
        assert_ne!(
            big as f32 as i32, big,
            "sanity: f32 genuinely loses this value"
        );
        let got = read_sample(&big.to_le_bytes(), 0, 4, 2, true);
        assert_eq!(
            got, big as f64,
            "read_sample must return the exact Int32 value as f64"
        );
        assert_eq!(got as i32, big);
    }
}

#[cfg(test)]
mod codec_tests {
    use super::*;

    #[test]
    fn zstd_roundtrip_reconstructs_bytes() {
        // A buffer with structure a byte-entropy coder actually compresses.
        let original: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let encoded = zstd::stream::encode_all(&original[..], 3).expect("encode");
        assert!(encoded.len() < original.len(), "expected zstd to shrink it");
        let decoded = zstd_decompress(&encoded, original.len());
        assert_eq!(decoded, original);
    }

    #[test]
    fn zstd_short_output_is_zero_padded_to_expected() {
        let encoded = zstd::stream::encode_all(&[1u8, 2, 3][..], 3).expect("encode");
        let decoded = zstd_decompress(&encoded, 8);
        assert_eq!(decoded, vec![1, 2, 3, 0, 0, 0, 0, 0]);
    }

    // A 4x4 lossless WebP (VP8L) generated with PIL: pixel (x,y) =
    // (x*40+10, y*40+20, (x+y)*20+5), row-major. Decoder-only, CI-safe (no fixture file).
    const WEBP_4X4_LOSSLESS: &[u8] = &[
        0x52, 0x49, 0x46, 0x46, 0x4c, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38,
        0x4c, 0x3f, 0x00, 0x00, 0x00, 0x2f, 0x03, 0xc0, 0x00, 0x00, 0x7f, 0x40, 0x90, 0x6d, 0x33,
        0x88, 0x43, 0xcc, 0x9f, 0x66, 0x40, 0x87, 0x10, 0x0b, 0xa6, 0x78, 0x77, 0x24, 0x89, 0x05,
        0x93, 0x2b, 0x83, 0xf9, 0x13, 0x06, 0x31, 0xff, 0x6d, 0x58, 0x15, 0x42, 0x30, 0x59, 0x96,
        0x31, 0x0f, 0x32, 0x10, 0x04, 0x40, 0xc4, 0x39, 0x70, 0xe0, 0xc0, 0xa7, 0x03, 0x07, 0x0e,
        0xa4, 0x44, 0xf4, 0x3f, 0x7c, 0x8d, 0x45, 0x00, 0x00,
    ];
    const WEBP_4X4_RGB: &[u8] = &[
        10, 20, 5, 50, 20, 25, 90, 20, 45, 130, 20, 65, 10, 60, 25, 50, 60, 45, 90, 60, 65, 130,
        60, 85, 10, 100, 45, 50, 100, 65, 90, 100, 85, 130, 100, 105, 10, 140, 65, 50, 140, 85, 90,
        140, 105, 130, 140, 125,
    ];

    #[test]
    fn webp_lossless_decodes_to_expected_rgb() {
        let tile = CompressedTile {
            bytes: WEBP_4X4_LOSSLESS.to_vec(),
            compression: 50001,
            predictor: 1,
            tile_w: 4,
            tile_h: 4,
            samples: 3,
            bits_per_sample: 8,
            sample_format: 1,
            little_endian: true,
            photometric: 2,
            jpeg_tables: None,
            grid_col: 0,
            grid_row: 0,
            present: true,
        };
        let out = decode_webp(&tile, 4, 4, 3);
        assert_eq!(out, WEBP_4X4_RGB);
    }

    #[test]
    fn webp_rgb_tile_padded_to_opaque_rgba_when_spp_is_4() {
        // GDAL/libtiff WEBP COGs commonly declare spp=4 (RGBA) but encode only RGB in the
        // tile; decode_webp must pad an opaque (255) alpha, not fail to transparent zeros.
        let tile = CompressedTile {
            bytes: WEBP_4X4_LOSSLESS.to_vec(),
            compression: 50001,
            predictor: 1,
            tile_w: 4,
            tile_h: 4,
            samples: 4,
            bits_per_sample: 8,
            sample_format: 1,
            little_endian: true,
            photometric: 2,
            jpeg_tables: None,
            grid_col: 0,
            grid_row: 0,
            present: true,
        };
        let out = decode_webp(&tile, 4, 4, 4);
        let mut want = Vec::new();
        for px in WEBP_4X4_RGB.chunks(3) {
            want.extend_from_slice(px);
            want.push(255);
        }
        assert_eq!(out, want);
    }
}
