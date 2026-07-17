// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Bespoke COG reader.
//!
//! NO `gdal` / `tiff` / `geotiff` / COG-reader crates (score.sh enforces this). We
//! parse the TIFF/BigTIFF container ourselves: header, IFD chain, tile offsets /
//! bytecounts, the overview IFDs, and the tags needed to map pixel↔geo
//! (`ModelPixelScaleTag`, `ModelTiepointTag`). Decode tiles with an allowed codec
//! crate (DEFLATE required; YCbCr-JPEG stretch). Honor the mask/alpha band →
//! transparency. The source CRS is given (EPSG:3763) — no GeoKey CRS decoding needed.

use std::fs::File;
use std::os::unix::fs::FileExt;

/// GRADED SEAM: every COG byte read flows through this trait. The pilot ships
/// `LocalFileRangeSource`; the S3 impl (`s3::S3RangeSource`) reads over signed HTTP ranges.
/// Implementations are positioned (no shared cursor), so a source can be shared across
/// threads for parallel tile reads.
pub trait RangeSource {
    fn read_range(&self, offset: u64, len: usize) -> std::io::Result<Vec<u8>>;
}

/// Reads byte ranges from a local file. Uses positioned reads (`read_at`/pread) rather than
/// a seek cursor, so it is `Sync` — multiple threads can read distinct ranges concurrently.
pub struct LocalFileRangeSource {
    file: File,
}

impl LocalFileRangeSource {
    pub fn open(path: &str) -> std::io::Result<Self> {
        Ok(Self {
            file: File::open(path)?,
        })
    }
}

impl RangeSource for LocalFileRangeSource {
    fn read_range(&self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        self.file.read_exact_at(&mut buf, offset)?;
        Ok(buf)
    }
}

// ---------------------------------------------------------------------------
// TIFF / BigTIFF container parsing (bespoke).
// ---------------------------------------------------------------------------

// TIFF tag numbers we care about.
const T_IMAGE_WIDTH: u16 = 256;
const T_IMAGE_LENGTH: u16 = 257;
const T_BITS_PER_SAMPLE: u16 = 258;
const T_COMPRESSION: u16 = 259;
const T_PHOTOMETRIC: u16 = 262;
const T_SAMPLES_PER_PIXEL: u16 = 277;
const T_PREDICTOR: u16 = 317;
const T_TILE_WIDTH: u16 = 322;
const T_TILE_LENGTH: u16 = 323;
const T_NEW_SUBFILE_TYPE: u16 = 254;
const T_TILE_OFFSETS: u16 = 324;
const T_TILE_BYTECOUNTS: u16 = 325;
const T_EXTRA_SAMPLES: u16 = 338;
const T_SAMPLE_FORMAT: u16 = 339;
const T_JPEG_TABLES: u16 = 347;
const T_MODEL_PIXEL_SCALE: u16 = 33550;
const T_MODEL_TIEPOINT: u16 = 33922;

/// The geotransform that maps source pixel (col,row) centers / corners to geo (X,Y).
/// Pixel-is-area: geo of the upper-left *corner* of pixel (0,0) is (origin_x, origin_y);
/// x increases with col, y decreases with row.
#[derive(Clone, Copy, Debug)]
pub struct GeoTransform {
    pub origin_x: f64,
    pub origin_y: f64,
    pub px: f64, // pixel size in x (>0)
    pub py: f64, // pixel size in y (>0, applied downward)
}

impl GeoTransform {
    /// geo -> continuous pixel coordinate (u across cols, v down rows), pixel-is-area:
    /// u = (X - origin_x)/px ; a value in [c, c+1) lies in column c (center at c+0.5).
    #[inline]
    pub fn geo_to_pix(&self, x: f64, y: f64) -> (f64, f64) {
        ((x - self.origin_x) / self.px, (self.origin_y - y) / self.py)
    }
}

/// Below this array-byte size, `parse()` reads a level's tile-offset/bytecount arrays fully
/// into memory ("Resident"); at or above it, `parse()` switches to windowed ("Lazy") reads via
/// `index_chunk_entry`. Env `TERRASERVE_INDEX_LAZY_BYTES`, default 4 MiB. Read fresh from the
/// environment on every call (no caching) so tests can toggle it between `parse()` calls within
/// one process.
fn lazy_threshold_bytes() -> u64 {
    std::env::var("TERRASERVE_INDEX_LAZY_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(4 * 1024 * 1024)
}

/// Entries per lazily-read tile-index chunk (`index_chunk_entry`'s cache granularity). Env
/// `TERRASERVE_INDEX_CHUNK`, default 8192, floored at 1 and ceilinged at `1 << 20` (so a typo
/// can't set an absurd chunk size and read/allocate a huge slab per lookup). Read fresh from the
/// environment on every call (no caching) so tests can toggle it between calls within one process.
fn lazy_chunk_entries() -> usize {
    std::env::var("TERRASERVE_INDEX_CHUNK")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .map(|n| n.min(1 << 20))
        .unwrap_or(8192)
}

/// A level's tile-offset/bytecount index. `Resident` holds both arrays fully decoded in memory
/// (today's only path — every array `parse()` builds). `Lazy` instead remembers where the two
/// out-of-line arrays live in the file and reads them in cached chunks on demand via
/// `index_chunk_entry` — for a COG with millions of tiles, opening it no longer means paying
/// O(ntiles) bytes/time up front just to answer one tile's location.
#[derive(Clone, Debug)]
pub enum TileIndex {
    Resident {
        offsets: Vec<u64>,
        bytecounts: Vec<u64>,
    },
    Lazy {
        offsets_off: u64,
        bytecounts_off: u64,
        // Bytes per element: 4 (LONG, classic TIFF) or 8 (LONG8, BigTIFF) — tracked PER ARRAY,
        // not shared: real-world BigTIFF COGs (e.g. GDAL's writer) commonly store TileOffsets as
        // LONG8 but TileByteCounts as plain LONG, since byte counts rarely need 64 bits even when
        // offsets do. Assuming one shared element size corrupts every bytecount read on such a
        // file (caught live against `cascais.cog.deflate.tif`: TileOffsets typ=16/LONG8,
        // TileByteCounts typ=4/LONG in the same IFD).
        offsets_elem_size: u8,
        bytecounts_elem_size: u8,
        ntiles: u64,
        little_endian: bool,
    },
}

/// One image directory (full-res image or an overview level).
#[derive(Clone, Debug)]
pub struct Level {
    pub width: u32,
    pub height: u32,
    pub tile_w: u32,
    pub tile_h: u32,
    pub compression: u16,
    pub predictor: u16,
    pub photometric: u16,
    pub samples_per_pixel: u16,
    pub bits_per_sample: u16,
    pub sample_format: u16, // TIFF tag 339: 1 = unsigned int, 2 = signed int, 3 = float
    pub extra_samples: u16, // 0 = none, 2 = unassociated alpha (band count includes it)
    pub index: TileIndex,
    /// Id of the COG this level belongs to, assigned once per `parse()` call — the first
    /// component of an `IndexChunkKey` (`cache::IndexCache`), so chunk-cache entries from two
    /// different COGs sharing one process-wide cache never collide.
    pub source_id: u64,
    pub geo: GeoTransform,
}

impl Level {
    #[inline]
    pub fn tiles_across(&self) -> u32 {
        (self.width + self.tile_w - 1) / self.tile_w
    }
    #[inline]
    pub fn tiles_down(&self) -> u32 {
        (self.height + self.tile_h - 1) / self.tile_h
    }
    /// Downsample factor relative to a reference (native) width.
    pub fn factor(&self, native_width: u32) -> f64 {
        native_width as f64 / self.width as f64
    }
}

/// Parsed COG: the full-res level plus overview levels (index 0 = full res). Internal
/// transparency-mask IFDs (`NewSubfileType` bit 2, as in the JPEG COG) are recognized
/// and skipped so they don't pollute the image pyramid; the pilot's goldens take alpha
/// from an image alpha band (ExtraSamples), not the internal mask.
pub struct Cog {
    pub levels: Vec<Level>,
    pub jpeg_tables: Option<Vec<u8>>,
    /// Byte order of the container (from the TIFF header). Needed to read multi-byte
    /// (16/32-bit) samples in the band-math decode path.
    pub little_endian: bool,
}

impl Cog {
    #[inline]
    pub fn native_width(&self) -> u32 {
        self.levels[0].width
    }
}

/// A single raw IFD entry (tag, type, count, and the 8/4-byte value/offset field).
struct Entry {
    typ: u16,
    count: u64,
    value_field: [u8; 8], // holds inline value or the offset (only first 4 bytes used for classic)
}

fn type_size(typ: u16) -> u64 {
    match typ {
        1 | 2 | 6 | 7 => 1, // BYTE, ASCII, SBYTE, UNDEFINED
        3 | 8 => 2,         // SHORT, SSHORT
        4 | 9 | 11 => 4,    // LONG, SLONG, FLOAT
        5 | 10 | 12 => 8,   // RATIONAL, SRATIONAL, DOUBLE
        16 | 17 | 18 => 8,  // LONG8, SLONG8, IFD8
        _ => 1,
    }
}

struct Reader<'a, R: RangeSource> {
    src: &'a R,
    little: bool,
    big: bool, // BigTIFF
}

impl<'a, R: RangeSource> Reader<'a, R> {
    fn u16(&self, b: &[u8]) -> u16 {
        if self.little {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        }
    }
    fn u32(&self, b: &[u8]) -> u32 {
        if self.little {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        }
    }
    fn u64(&self, b: &[u8]) -> u64 {
        if self.little {
            u64::from_le_bytes(b[0..8].try_into().unwrap())
        } else {
            u64::from_be_bytes(b[0..8].try_into().unwrap())
        }
    }
    fn f64(&self, b: &[u8]) -> f64 {
        f64::from_bits(self.u64(b))
    }

    /// Read the raw `count` values of a given entry as u64 (for integer tag types).
    fn read_uints(&self, e: &Entry) -> std::io::Result<Vec<u64>> {
        let ts = type_size(e.typ);
        let total = ts * e.count;
        let bytes = if total <= if self.big { 8 } else { 4 } {
            e.value_field[..total as usize].to_vec()
        } else {
            let off = if self.big {
                self.u64(&e.value_field)
            } else {
                self.u32(&e.value_field[..4]) as u64
            };
            self.src.read_range(off, total as usize)?
        };
        let mut out = Vec::with_capacity(e.count as usize);
        for i in 0..e.count as usize {
            let s = &bytes[i * ts as usize..];
            let v = match e.typ {
                1 | 2 | 6 | 7 => s[0] as u64,
                3 | 8 => self.u16(s) as u64,
                4 | 9 => self.u32(s) as u64,
                16 | 17 | 18 => self.u64(s),
                _ => self.u32(s) as u64,
            };
            out.push(v);
        }
        Ok(out)
    }

    /// Read DOUBLE (type 12) values of an entry.
    fn read_doubles(&self, e: &Entry) -> std::io::Result<Vec<f64>> {
        let ts = type_size(e.typ);
        let total = ts * e.count;
        let bytes = if total <= if self.big { 8 } else { 4 } {
            e.value_field[..total as usize].to_vec()
        } else {
            let off = if self.big {
                self.u64(&e.value_field)
            } else {
                self.u32(&e.value_field[..4]) as u64
            };
            self.src.read_range(off, total as usize)?
        };
        let mut out = Vec::with_capacity(e.count as usize);
        for i in 0..e.count as usize {
            out.push(self.f64(&bytes[i * 8..]));
        }
        Ok(out)
    }

    /// Read raw bytes (e.g. JPEGTables) of an entry.
    fn read_bytes(&self, e: &Entry) -> std::io::Result<Vec<u8>> {
        let total = type_size(e.typ) * e.count;
        if total <= if self.big { 8 } else { 4 } {
            Ok(e.value_field[..total as usize].to_vec())
        } else {
            let off = if self.big {
                self.u64(&e.value_field)
            } else {
                self.u32(&e.value_field[..4]) as u64
            };
            self.src.read_range(off, total as usize)
        }
    }

    /// File offset of an entry's out-of-line array (mirrors `read_uints`' offset math). Used by
    /// the lazy-open path to locate the TileOffsets/TileByteCounts arrays without reading them.
    fn entry_array_offset(&self, e: &Entry) -> u64 {
        if self.big {
            self.u64(&e.value_field)
        } else {
            self.u32(&e.value_field[..4]) as u64
        }
    }
}

/// Read (and cache) tile-index entry `tile_index` from the out-of-line array at `array_off`
/// (a TileOffsets or TileByteCounts array). Reads land in `cache` at chunk granularity
/// (`lazy_chunk_entries()`), so nearby tile lookups reuse an already-decoded chunk instead of
/// re-reading + re-decoding the same file range one tile at a time.
fn index_chunk_entry<R: RangeSource>(
    src: &R,
    cache: &crate::cache::IndexCache,
    source_id: u64,
    array_off: u64,
    elem_size: u8,
    ntiles: u64,
    little: bool,
    tile_index: u64,
) -> std::io::Result<u64> {
    let chunk = lazy_chunk_entries();
    // u64 (not u32): `tile_index / chunk` would truncate for an astronomically large ntiles.
    let chunk_no = tile_index / chunk as u64;
    let key = (source_id, array_off, chunk_no);
    let entries = if let Some(v) = cache.get(&key) {
        v
    } else {
        let start = chunk_no * chunk as u64;
        let len = ((ntiles - start).min(chunk as u64)) as usize;
        let bytes = src.read_range(
            array_off + start * elem_size as u64,
            len * elem_size as usize,
        )?;
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            let s = &bytes[i * elem_size as usize..];
            let val = match (elem_size, little) {
                (8, true) => u64::from_le_bytes(s[0..8].try_into().unwrap()),
                (8, false) => u64::from_be_bytes(s[0..8].try_into().unwrap()),
                (4, true) => u32::from_le_bytes(s[0..4].try_into().unwrap()) as u64,
                (4, false) => u32::from_be_bytes(s[0..4].try_into().unwrap()) as u64,
                // SHORT (2) is legal for TileByteCounts on classic TIFF — read exactly 2
                // bytes, or a 4-byte read would slice-OOB on the last entry.
                (2, true) => u16::from_le_bytes(s[0..2].try_into().unwrap()) as u64,
                (2, false) => u16::from_be_bytes(s[0..2].try_into().unwrap()) as u64,
                // `parse()`'s Lazy predicate only ever picks an elem_size of 2/4/8, but stay
                // defensive here too rather than fall into a catch-all that would slice-OOB on
                // (e.g.) size 1.
                (_, _) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unsupported tile-index element size {elem_size}"),
                    ))
                }
            };
            out.push(val);
        }
        let arc = std::sync::Arc::new(out);
        cache.insert(key, arc.clone());
        arc
    };
    Ok(entries[(tile_index - chunk_no * chunk as u64) as usize])
}

impl Level {
    /// Resolve tile `tile_index`'s `(file offset, byte length)`, or `None` when it's absent
    /// (zero offset/bytecount — a sparse COG's "no data here" tile) or past the end of the
    /// index (out-of-range). The one accessor both `Resident` and `Lazy` levels answer through.
    pub fn tile_location<R: RangeSource>(
        &self,
        src: &R,
        cache: &crate::cache::IndexCache,
        tile_index: u64,
    ) -> std::io::Result<Option<(u64, usize)>> {
        let (off, bc) = match &self.index {
            TileIndex::Resident {
                offsets,
                bytecounts,
            } => {
                let i = tile_index as usize;
                if i >= offsets.len() {
                    return Ok(None);
                }
                (offsets[i], *bytecounts.get(i).unwrap_or(&0))
            }
            TileIndex::Lazy {
                offsets_off,
                bytecounts_off,
                offsets_elem_size,
                bytecounts_elem_size,
                ntiles,
                little_endian,
            } => {
                if tile_index >= *ntiles {
                    return Ok(None);
                }
                let off = index_chunk_entry(
                    src,
                    cache,
                    self.source_id,
                    *offsets_off,
                    *offsets_elem_size,
                    *ntiles,
                    *little_endian,
                    tile_index,
                )?;
                let bc = index_chunk_entry(
                    src,
                    cache,
                    self.source_id,
                    *bytecounts_off,
                    *bytecounts_elem_size,
                    *ntiles,
                    *little_endian,
                    tile_index,
                )?;
                (off, bc)
            }
        };
        Ok(if off == 0 || bc == 0 {
            None
        } else {
            Some((off, bc as usize))
        })
    }
}

/// Parse the whole COG: header + IFD chain (full res followed by overviews).
pub fn parse<R: RangeSource>(src: &R) -> std::io::Result<Cog> {
    let head = src.read_range(0, 16)?;
    let little = &head[0..2] == b"II";
    if !little && &head[0..2] != b"MM" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not a TIFF (bad byte order mark)",
        ));
    }
    let mut rdr = Reader {
        src,
        little,
        big: false,
    };
    let magic = rdr.u16(&head[2..4]);
    let first_ifd: u64;
    if magic == 43 {
        // BigTIFF: bytesize (2) must be 8, reserved (2)=0, then 8-byte first IFD offset.
        rdr.big = true;
        first_ifd = rdr.u64(&head[8..16]);
    } else if magic == 42 {
        rdr.big = false;
        first_ifd = rdr.u32(&head[4..8]) as u64;
    } else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not a TIFF (bad magic)",
        ));
    }

    // Every `Level` this parse produces shares one id, distinguishing this COG's index-chunk
    // cache entries from any other COG's sharing the same process-wide `cache::IndexCache`.
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_SOURCE_ID: AtomicU64 = AtomicU64::new(1);
    let source_id = NEXT_SOURCE_ID.fetch_add(1, Ordering::Relaxed);

    let mut levels: Vec<Level> = Vec::new();
    let mut jpeg_tables: Option<Vec<u8>> = None;
    // Geo comes from the full-res IFD (first one).
    let mut native_geo: Option<GeoTransform> = None;
    let mut native_dims: Option<(u32, u32)> = None;

    let mut ifd_off = first_ifd;
    while ifd_off != 0 {
        let (entries, next) = read_ifd(&rdr, ifd_off)?;

        let get = |tag: u16| entries.iter().find(|(t, _)| *t == tag).map(|(_, e)| e);
        let get_u = |tag: u16, default: u64| -> u64 {
            get(tag)
                .and_then(|e| rdr.read_uints(e).ok())
                .and_then(|v| v.first().copied())
                .unwrap_or(default)
        };

        let subfile_type = get_u(T_NEW_SUBFILE_TYPE, 0);
        let is_mask = subfile_type & 0x4 != 0; // TIFF: bit 2 = transparency mask
        let width = get_u(T_IMAGE_WIDTH, 0) as u32;
        let height = get_u(T_IMAGE_LENGTH, 0) as u32;
        let tile_w = get_u(T_TILE_WIDTH, 0) as u32;
        let tile_h = get_u(T_TILE_LENGTH, 0) as u32;
        let compression = get_u(T_COMPRESSION, 1) as u16;
        let predictor = get_u(T_PREDICTOR, 1) as u16;
        let photometric = get_u(T_PHOTOMETRIC, 1) as u16;
        let samples_per_pixel = get_u(T_SAMPLES_PER_PIXEL, 1) as u16;
        let bits_per_sample = get_u(T_BITS_PER_SAMPLE, 8) as u16;
        let sample_format = get_u(T_SAMPLE_FORMAT, 1) as u16;
        let extra_samples = get(T_EXTRA_SAMPLES)
            .and_then(|e| rdr.read_uints(e).ok())
            .and_then(|v| v.first().copied())
            .unwrap_or(0) as u16;

        // Cheap: just the entry's `count`, not the array bytes — how many tiles this IFD has,
        // known without reading the (possibly huge) TileOffsets/TileByteCounts arrays.
        let ntiles = get(T_TILE_OFFSETS).map(|e| e.count).unwrap_or(0);

        // Geo tags (only on full-res IFD in practice).
        if let Some(e) = get(T_MODEL_PIXEL_SCALE) {
            if let Ok(scale) = rdr.read_doubles(e) {
                if let Some(tie) = get(T_MODEL_TIEPOINT).and_then(|e| rdr.read_doubles(e).ok()) {
                    // tie = [i, j, k, X, Y, Z]; pixel (i,j) maps to geo (X,Y).
                    let (pi, pj) = (tie[0], tie[1]);
                    let (gx, gy) = (tie[3], tie[4]);
                    let px = scale[0];
                    let py = scale[1];
                    let origin_x = gx - pi * px;
                    let origin_y = gy + pj * py;
                    native_geo = Some(GeoTransform {
                        origin_x,
                        origin_y,
                        px,
                        py,
                    });
                }
            }
        }

        // JPEGTables (shared quantization/huffman tables for tiled JPEG).
        if jpeg_tables.is_none() {
            if let Some(e) = get(T_JPEG_TABLES) {
                jpeg_tables = rdr.read_bytes(e).ok();
            }
        }

        // Keep only image directories that carry tiles; skip internal transparency-mask
        // IFDs (their alpha is not applied — image alpha comes from an ExtraSample band), and
        // skip a level whose TileWidth/TileLength is missing or zero (a malformed COG can carry
        // TileOffsets with no valid tile dimensions — `get_u` defaults to 0 — which would parse
        // fine here then panic later in `tiles_across`/`tiles_down`/render.rs's div-by-tile_w).
        if width > 0 && height > 0 && ntiles > 0 && tile_w > 0 && tile_h > 0 && !is_mask {
            // Dual-mode: below the threshold, read both arrays fully now (Resident, today's
            // behavior for our fixture-sized COGs); at/above it, remember where the two
            // out-of-line arrays live and read them in cached chunks on demand (Lazy) — a COG
            // with millions of tiles no longer costs O(ntiles) bytes/time just to open.
            let off_e = get(T_TILE_OFFSETS).unwrap();
            let bc_e = get(T_TILE_BYTECOUNTS);
            // TRACK PER ARRAY: TileOffsets and TileByteCounts can (and in real GDAL-written
            // BigTIFF COGs, commonly do) have different element sizes — LONG8 offsets alongside
            // plain LONG bytecounts, since a byte count rarely needs 64 bits even when a file
            // offset does. Using one shared size (derived from the offsets entry only) for both
            // arrays corrupts every bytecount read on such a file.
            let offsets_elem_size = type_size(off_e.typ) as u8;
            let index_bytes = off_e.count.saturating_mul(offsets_elem_size as u64);
            let lazy_bytes = lazy_threshold_bytes();
            // A tile-index array small enough to be stored INLINE in the IFD entry itself
            // (rather than out-of-line at a real file offset) can never be windowed —
            // `entry_array_offset` would misread the inline bytes as a bogus file offset.
            // Require BOTH arrays to be genuinely out-of-line before picking Lazy, regardless of
            // how low the configured threshold is (a forced-low threshold, as some tests use, must
            // never make Lazy misfire on a tiny/single-tile overview level).
            let inline_cap = if rdr.big { 8u64 } else { 4u64 };
            let offsets_out_of_line = index_bytes > inline_cap;
            let bytecounts_out_of_line = bc_e
                .map(|e| e.count.saturating_mul(type_size(e.typ)) > inline_cap)
                .unwrap_or(false);
            // Only pick Lazy for index shapes `index_chunk_entry` can actually decode: element
            // sizes it has explicit arms for (2/4/8 — SHORT/LONG/LONG8), and the two arrays the
            // same length (Lazy always reads both arrays at the same tile_index; a mismatched
            // TileByteCounts count would silently misalign or run past its own array). Anything
            // else falls through to Resident, which decodes any type/count correctly.
            let offsets_elem_size_safe = matches!(offsets_elem_size, 2 | 4 | 8);
            let bytecounts_elem_size_safe = bc_e
                .map(|e| matches!(type_size(e.typ) as u8, 2 | 4 | 8))
                .unwrap_or(false);
            let counts_match = bc_e.map(|e| e.count == off_e.count).unwrap_or(false);
            let index = if index_bytes >= lazy_bytes
                && bc_e.is_some()
                && offsets_out_of_line
                && bytecounts_out_of_line
                && offsets_elem_size_safe
                && bytecounts_elem_size_safe
                && counts_match
            {
                let bc_e = bc_e.unwrap();
                TileIndex::Lazy {
                    offsets_off: rdr.entry_array_offset(off_e),
                    bytecounts_off: rdr.entry_array_offset(bc_e),
                    offsets_elem_size,
                    bytecounts_elem_size: type_size(bc_e.typ) as u8,
                    ntiles: off_e.count,
                    little_endian: little,
                }
            } else {
                let offsets = rdr.read_uints(off_e)?;
                let bytecounts = bc_e
                    .map(|e| rdr.read_uints(e))
                    .transpose()?
                    .unwrap_or_default();
                TileIndex::Resident {
                    offsets,
                    bytecounts,
                }
            };
            if native_dims.is_none() {
                native_dims = Some((width, height));
                if native_geo.is_none() {
                    // Fallback identity geotransform (shouldn't happen for our data).
                    native_geo = Some(GeoTransform {
                        origin_x: 0.0,
                        origin_y: 0.0,
                        px: 1.0,
                        py: 1.0,
                    });
                }
            }
            let ng = native_geo.unwrap_or(GeoTransform {
                origin_x: 0.0,
                origin_y: 0.0,
                px: 1.0,
                py: 1.0,
            });
            let (nw, nh) = native_dims.unwrap_or((width, height));
            // Overview levels cover the same geographic extent as the full image; derive
            // their geotransform by scaling the native pixel size by the size ratio.
            let extent_w = nw as f64 * ng.px;
            let extent_h = nh as f64 * ng.py;
            let geo = GeoTransform {
                origin_x: ng.origin_x,
                origin_y: ng.origin_y,
                px: extent_w / width as f64,
                py: extent_h / height as f64,
            };
            levels.push(Level {
                width,
                height,
                tile_w,
                tile_h,
                compression,
                predictor,
                photometric,
                samples_per_pixel,
                bits_per_sample,
                sample_format,
                extra_samples,
                index,
                source_id,
                geo,
            });
        }

        ifd_off = next;
    }

    if levels.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "no image IFDs found",
        ));
    }
    // Ensure levels are ordered finest -> coarsest (full res first).
    levels.sort_by(|a, b| b.width.cmp(&a.width));
    Ok(Cog {
        levels,
        jpeg_tables,
        little_endian: little,
    })
}

/// Read a single IFD: returns (entries, next_ifd_offset).
fn read_ifd<R: RangeSource>(
    rdr: &Reader<R>,
    off: u64,
) -> std::io::Result<(Vec<(u16, Entry)>, u64)> {
    if rdr.big {
        let hdr = rdr.src.read_range(off, 8)?;
        let n = rdr.u64(&hdr);
        let body_len = (n as usize) * 20 + 8;
        let body = rdr.src.read_range(off + 8, body_len)?;
        let mut entries = Vec::with_capacity(n as usize);
        for i in 0..n as usize {
            let e = &body[i * 20..i * 20 + 20];
            let tag = rdr.u16(&e[0..2]);
            let typ = rdr.u16(&e[2..4]);
            let count = rdr.u64(&e[4..12]);
            let mut vf = [0u8; 8];
            vf.copy_from_slice(&e[12..20]);
            entries.push((
                tag,
                Entry {
                    typ,
                    count,
                    value_field: vf,
                },
            ));
        }
        let next = rdr.u64(&body[n as usize * 20..]);
        Ok((entries, next))
    } else {
        let hdr = rdr.src.read_range(off, 2)?;
        let n = rdr.u16(&hdr);
        let body_len = (n as usize) * 12 + 4;
        let body = rdr.src.read_range(off + 2, body_len)?;
        let mut entries = Vec::with_capacity(n as usize);
        for i in 0..n as usize {
            let e = &body[i * 12..i * 12 + 12];
            let tag = rdr.u16(&e[0..2]);
            let typ = rdr.u16(&e[2..4]);
            let count = rdr.u32(&e[4..8]) as u64;
            let mut vf = [0u8; 8];
            vf[..4].copy_from_slice(&e[8..12]);
            entries.push((
                tag,
                Entry {
                    typ,
                    count,
                    value_field: vf,
                },
            ));
        }
        let next = rdr.u32(&body[n as usize * 12..][..4]) as u64;
        Ok((entries, next))
    }
}

// ---------------------------------------------------------------------------
// Test-only harness: synthetic in-memory COG + range-source wrappers. Task 1 of the
// lazy/windowed-open work; later tasks build on `MemRangeSource`, `CountingRangeSource`,
// and `TileIndexMeta` to construct `Lazy` levels without re-parsing the IFD.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod lazy_index_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Guards every test in this module that reads or mutates the process-wide
    /// `TERRASERVE_INDEX_LAZY_BYTES` / `TERRASERVE_INDEX_CHUNK` env vars `parse()` and
    /// `index_chunk_entry` consult. `cargo test` runs tests in parallel within one process, so
    /// without this lock a test toggling the threshold could flip another concurrently-running
    /// `parse()` call's Resident/Lazy decision out from under it. Every test that calls
    /// `parse()` in this module — whether or not it itself sets an env var — takes this lock
    /// first. Poison-tolerant: an earlier panicking test must not spuriously fail every test
    /// after it.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// An in-memory `RangeSource` over a byte buffer — the fixture backing store for
    /// `build_test_cog`. Errors (rather than panics) on a range past the end of `data`,
    /// mirroring how a real source (file/S3) behaves on a short read.
    struct MemRangeSource {
        data: Vec<u8>,
    }

    impl RangeSource for MemRangeSource {
        fn read_range(&self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
            let start = offset as usize;
            let end = start.checked_add(len).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "range overflow")
            })?;
            if end > self.data.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "read past end of in-memory COG",
                ));
            }
            Ok(self.data[start..end].to_vec())
        }
    }

    /// Wraps a `&R` and tallies total bytes requested via `read_range`, so later
    /// lazy/windowed-open tasks can assert how much of a COG a given operation touched.
    struct CountingRangeSource<'a, R> {
        inner: &'a R,
        bytes: AtomicU64,
    }

    impl<'a, R> CountingRangeSource<'a, R> {
        fn new(inner: &'a R) -> Self {
            Self {
                inner,
                bytes: AtomicU64::new(0),
            }
        }
        fn bytes(&self) -> u64 {
            self.bytes.load(Ordering::Relaxed)
        }
    }

    impl<'a, R: RangeSource> RangeSource for CountingRangeSource<'a, R> {
        fn read_range(&self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
            let out = self.inner.read_range(offset, len)?;
            self.bytes.fetch_add(len as u64, Ordering::Relaxed);
            Ok(out)
        }
    }

    /// File offsets of the two BigTIFF LONG8 tile-index arrays `build_test_cog` writes,
    /// so later tasks can construct a `Lazy` level directly without re-parsing the IFD.
    /// When `inline` is true, the arrays fit entirely in their entries' 8-byte value fields
    /// (no out-of-line data-section bytes) — `offsets_off`/`bytecounts_off` are meaningless
    /// (`0`) in that case, mirroring how a real inline TIFF entry has no file offset to give.
    #[derive(Debug, Clone, Copy)]
    struct TileIndexMeta {
        offsets_off: u64,
        bytecounts_off: u64,
        elem_size: u8, // bytes per element (8 = LONG8)
        ntiles: u64,
        inline: bool,
    }

    fn push_entry(buf: &mut Vec<u8>, tag: u16, typ: u16, count: u64, value: [u8; 8]) {
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&typ.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        buf.extend_from_slice(&value);
    }

    fn inline_u32(v: u32) -> [u8; 8] {
        let mut f = [0u8; 8];
        f[..4].copy_from_slice(&v.to_le_bytes());
        f
    }

    fn inline_u16(v: u16) -> [u8; 8] {
        let mut f = [0u8; 8];
        f[..2].copy_from_slice(&v.to_le_bytes());
        f
    }

    fn offset_field(off: u64) -> [u8; 8] {
        off.to_le_bytes()
    }

    /// Build a minimal little-endian BigTIFF byte buffer with one tiled image IFD,
    /// carrying exactly the tags `parse()` reads: ImageWidth/Length, TileWidth/Length,
    /// Compression=1, BitsPerSample=8, SamplesPerPixel=1, PlanarConfig=1, TileOffsets +
    /// TileByteCounts (LONG8 arrays), ModelPixelScale, ModelTiepoint.
    ///
    /// Per TIFF/BigTIFF, an entry's array lives INLINE in its own 8-byte value field when
    /// `count * elem_size <= 8` (the BigTIFF inline cap) — only an out-of-line array (too big
    /// to fit) gets a real file-offset pointer. For `ntiles > 1` (elem_size is always 8/LONG8
    /// here, so that's `8 * ntiles > 8`) this builder writes the usual out-of-line arrays: each
    /// tile gets a distinct nonzero sentinel offset (`1_000_000 + i*7`) and bytecount
    /// (`100 + i`), except one tile (the middle index) whose offset is deliberately zeroed to
    /// exercise the "missing tile" path. For the single-tile case (`ntiles == 1`, `8 <= 8`) the
    /// one offset/bytecount value is written directly into the entry's value field instead —
    /// spec-correct, and exactly the shape `parse()`'s Lazy predicate must recognize as
    /// never-lazy-able (see `inline_tile_index_stays_resident_even_when_lazy_forced`).
    /// Returns the bytes plus the file offsets of the two tile arrays (when out-of-line), so
    /// other tests can build a `Lazy` index without re-parsing the IFD.
    fn build_test_cog(tiles_across: u32, tiles_down: u32, tile: u32) -> (Vec<u8>, TileIndexMeta) {
        let ntiles = tiles_across as u64 * tiles_down as u64;
        let width = tiles_across * tile;
        let height = tiles_down * tile;
        const ELEM_SIZE: u64 = 8; // LONG8
        const INLINE_CAP: u64 = 8; // BigTIFF entry value-field capacity
        let inline = ntiles * ELEM_SIZE <= INLINE_CAP;

        let mut buf = Vec::new();

        // --- Header (16 bytes): "II", BigTIFF magic 43, bytesize 8, reserved 0, first IFD offset.
        buf.extend_from_slice(b"II");
        buf.extend_from_slice(&43u16.to_le_bytes());
        buf.extend_from_slice(&8u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        let first_ifd_off: u64 = 16;
        buf.extend_from_slice(&first_ifd_off.to_le_bytes());
        assert_eq!(buf.len(), 16);

        // --- IFD: u64 entry count, then 20-byte entries, then u64 next-IFD (0 = last). ---
        const NUM_ENTRIES: u64 = 12;
        buf.extend_from_slice(&NUM_ENTRIES.to_le_bytes());

        let entries_start = buf.len(); // 24
        let entries_bytes = NUM_ENTRIES as usize * 20;
        let next_ifd_field_off = entries_start + entries_bytes; // 264
        let data_start = (next_ifd_field_off + 8) as u64; // 272

        // Data-section layout, computed up front so entry value fields can point at it. An
        // inline array occupies zero data-section bytes (its value lives entirely in the
        // entry itself), so the section after it simply starts where it would have gone.
        let offsets_off = data_start;
        let bytecounts_off = if inline {
            offsets_off
        } else {
            offsets_off + ntiles * ELEM_SIZE
        };
        let pixelscale_off = if inline {
            bytecounts_off
        } else {
            bytecounts_off + ntiles * ELEM_SIZE
        };
        let tiepoint_off = pixelscale_off + 24; // 3 doubles
        let data_end = tiepoint_off + 48; // 6 doubles

        // Tag types: SHORT=3, LONG=4, DOUBLE=12, LONG8=16. Entries in ascending tag order
        // (not required by the parser, but matches real TIFF convention).
        push_entry(&mut buf, 256, 4, 1, inline_u32(width)); // ImageWidth
        push_entry(&mut buf, 257, 4, 1, inline_u32(height)); // ImageLength
        push_entry(&mut buf, 258, 3, 1, inline_u16(8)); // BitsPerSample
        push_entry(&mut buf, 259, 3, 1, inline_u16(1)); // Compression
        push_entry(&mut buf, 277, 3, 1, inline_u16(1)); // SamplesPerPixel
        push_entry(&mut buf, 284, 3, 1, inline_u16(1)); // PlanarConfig
        push_entry(&mut buf, 322, 4, 1, inline_u32(tile)); // TileWidth
        push_entry(&mut buf, 323, 4, 1, inline_u32(tile)); // TileLength
        if inline {
            // Single tile: the array's one value fits in the 8-byte value field directly —
            // no out-of-line pointer. Nonzero sentinels (never the "missing tile" case).
            push_entry(&mut buf, 324, 16, ntiles, offset_field(1_000_000)); // TileOffsets
            push_entry(&mut buf, 325, 16, ntiles, offset_field(100)); // TileByteCounts
        } else {
            push_entry(&mut buf, 324, 16, ntiles, offset_field(offsets_off)); // TileOffsets
            push_entry(&mut buf, 325, 16, ntiles, offset_field(bytecounts_off));
            // TileByteCounts
        }
        push_entry(&mut buf, 33550, 12, 3, offset_field(pixelscale_off)); // ModelPixelScale
        push_entry(&mut buf, 33922, 12, 6, offset_field(tiepoint_off)); // ModelTiepoint

        assert_eq!(buf.len(), next_ifd_field_off);
        buf.extend_from_slice(&0u64.to_le_bytes()); // next IFD = 0 (last)
        assert_eq!(buf.len() as u64, data_start);

        // --- Data section: TileOffsets, TileByteCounts (only when out-of-line), then
        // ModelPixelScale, ModelTiepoint.
        if !inline {
            let zero_idx = (ntiles / 2) as usize;
            for i in 0..ntiles as usize {
                let off: u64 = if i == zero_idx {
                    0
                } else {
                    1_000_000 + i as u64 * 7
                };
                buf.extend_from_slice(&off.to_le_bytes());
            }
            assert_eq!(buf.len() as u64, bytecounts_off);
            for i in 0..ntiles as usize {
                let bc: u64 = 100 + i as u64;
                buf.extend_from_slice(&bc.to_le_bytes());
            }
        }
        assert_eq!(buf.len() as u64, pixelscale_off);
        for v in [1.0f64, 1.0, 0.0] {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(buf.len() as u64, tiepoint_off);
        for v in [0.0f64, 0.0, 0.0, 0.0, 0.0, 0.0] {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(buf.len() as u64, data_end);

        let meta = TileIndexMeta {
            offsets_off: if inline { 0 } else { offsets_off },
            bytecounts_off: if inline { 0 } else { bytecounts_off },
            elem_size: ELEM_SIZE as u8,
            ntiles,
            inline,
        };
        (buf, meta)
    }

    #[test]
    fn synthetic_cog_parses_with_expected_tiles() {
        // Asserts `parse()` builds Resident under the DEFAULT threshold — take the shared lock
        // so a concurrently-running env-mutating test can't flip that decision underfoot.
        let _g = env_lock();
        let (bytes, meta) = build_test_cog(40, 40, 256); // 1600 tiles
        assert_eq!(meta.ntiles, 1600);
        assert_eq!(meta.elem_size, 8);

        let src = MemRangeSource { data: bytes };
        let cog = parse(&src).expect("synthetic COG must parse");
        let lvl = &cog.levels[0];
        assert_eq!(lvl.width, 40 * 256);
        assert_eq!(lvl.height, 40 * 256);
        assert_eq!(lvl.tiles_across() * lvl.tiles_down(), 1600);

        // Source-fidelity: the eagerly-parsed (Resident) index must carry the exact sentinel
        // values build_test_cog wrote, including the one deliberately zeroed tile (the "missing
        // tile" path). Task 2: parse() always builds Resident — assert that directly.
        let TileIndex::Resident {
            offsets,
            bytecounts,
        } = &lvl.index
        else {
            panic!("parse() must build a Resident index (Task 2 never builds Lazy)");
        };
        assert_eq!(offsets.len(), 1600);
        assert_eq!(bytecounts.len(), 1600);
        let zero_idx = 1600usize / 2;
        assert_eq!(
            offsets[zero_idx], 0,
            "expected exactly one deliberately zeroed tile offset"
        );
        for i in 0..1600usize {
            if i != zero_idx {
                assert_eq!(offsets[i], 1_000_000 + i as u64 * 7);
            }
            assert_eq!(bytecounts[i], 100 + i as u64);
        }

        // Cross-check the eager parse against a raw read at the offsets TileIndexMeta
        // recorded, independent of read_uints — the array build_test_cog wrote lines up
        // byte-for-byte with what parse() consumed.
        let raw = src
            .read_range(meta.offsets_off, meta.elem_size as usize)
            .expect("raw TileOffsets read");
        let raw_first = u64::from_le_bytes(raw[..8].try_into().unwrap());
        assert_eq!(raw_first, offsets[0]);
        let raw_bc = src
            .read_range(meta.bytecounts_off, meta.elem_size as usize)
            .expect("raw TileByteCounts read");
        let raw_bc_first = u64::from_le_bytes(raw_bc[..8].try_into().unwrap());
        assert_eq!(raw_bc_first, bytecounts[0]);

        // MemRangeSource must error (not panic) past EOF — CountingRangeSource (wrapping
        // any RangeSource) relies on this propagating cleanly in later lazy-open tasks.
        assert!(src.read_range(src.data.len() as u64, 1).is_err());
        assert!(src.read_range(u64::MAX - 8, 16).is_err());
    }

    #[test]
    fn counting_range_source_tallies_and_delegates() {
        let (bytes, _meta) = build_test_cog(2, 2, 16); // 4 tiny tiles
        let src = MemRangeSource { data: bytes };
        let counting = CountingRangeSource::new(&src);

        let a = counting.read_range(0, 16).unwrap();
        let b = counting.read_range(16, 8).unwrap();
        assert_eq!(a.len(), 16);
        assert_eq!(b.len(), 8);
        assert_eq!(counting.bytes(), 24);

        // Delegates faithfully: same bytes as reading directly from the inner source.
        assert_eq!(
            counting.read_range(0, 16).unwrap(),
            src.read_range(0, 16).unwrap()
        );
        assert_eq!(counting.bytes(), 40);
    }

    /// Task 2: `Level::tile_location` must answer identically whether the level's index is
    /// `Resident` (today's only path — `parse()` always builds it) or a hand-built `Lazy` index
    /// over the SAME on-disk arrays (the accessor `parse()` will switch to once a future task
    /// adds the lazy-open threshold decision). Builds a `Lazy` `Level` directly from
    /// `build_test_cog`'s returned `TileIndexMeta`, per the "if exposing the array offsets is
    /// awkward" fallback — no re-parsing of the IFD needed.
    #[test]
    fn tile_location_lazy_matches_expected_and_reads_less() {
        // Asserts `parse()` builds Resident under the DEFAULT threshold/chunk size — take the
        // shared lock so a concurrently-running env-mutating test can't perturb either default.
        let _g = env_lock();
        // --- Part 1: correctness — Lazy must match Resident tile-for-tile, including the
        // deliberately-zeroed "missing tile" and out-of-range indices.
        let (bytes, meta) = build_test_cog(40, 40, 256); // 1600 tiles
        let mem = MemRangeSource { data: bytes };
        let cog = parse(&mem).expect("synthetic COG must parse");
        let resident = cog.levels[0].clone();
        let TileIndex::Resident { .. } = &resident.index else {
            panic!("parse() must build a Resident index");
        };

        let lazy = Level {
            index: TileIndex::Lazy {
                offsets_off: meta.offsets_off,
                bytecounts_off: meta.bytecounts_off,
                offsets_elem_size: meta.elem_size,
                bytecounts_elem_size: meta.elem_size,
                ntiles: meta.ntiles,
                little_endian: cog.little_endian,
            },
            ..resident.clone()
        };

        let cache = crate::cache::new_index_cache(64 << 20);
        let zero_idx = meta.ntiles / 2; // the tile build_test_cog deliberately zeroed

        // A scattered sample of tile indices (not just the front of the array) must agree.
        let sample = [
            0u64,
            1,
            2,
            17,
            zero_idx.saturating_sub(1),
            zero_idx,
            zero_idx + 1,
            meta.ntiles - 1,
        ];
        for i in sample {
            let want = resident
                .tile_location(&mem, &cache, i)
                .expect("resident tile_location");
            let got = lazy
                .tile_location(&mem, &cache, i)
                .expect("lazy tile_location");
            assert_eq!(got, want, "tile {i}: lazy tile_location != resident");
        }
        // The deliberately-zeroed tile is None via both paths (present in `sample` above, but
        // assert the exact sentinel explicitly too).
        assert_eq!(
            resident.tile_location(&mem, &cache, zero_idx).unwrap(),
            None
        );
        assert_eq!(lazy.tile_location(&mem, &cache, zero_idx).unwrap(), None);
        // Out-of-range (== ntiles, and well past it) is None via both paths.
        for oor in [meta.ntiles, meta.ntiles + 1, meta.ntiles + 10_000] {
            assert_eq!(resident.tile_location(&mem, &cache, oor).unwrap(), None);
            assert_eq!(lazy.tile_location(&mem, &cache, oor).unwrap(), None);
        }

        // --- Part 2: a small tile window touches far fewer bytes than the full index arrays.
        // Use a bigger synthetic COG (40,000 tiles) so the DEFAULT chunk size (8192 entries,
        // `TERRASERVE_INDEX_CHUNK` unset) spans only a fraction of the index — the assertion
        // must hold under the default config, not a test-only env override.
        let (big_bytes, big_meta) = build_test_cog(200, 200, 16); // 40,000 tiles
        let big_mem = MemRangeSource { data: big_bytes };
        let big_lazy = Level {
            index: TileIndex::Lazy {
                offsets_off: big_meta.offsets_off,
                bytecounts_off: big_meta.bytecounts_off,
                offsets_elem_size: big_meta.elem_size,
                bytecounts_elem_size: big_meta.elem_size,
                ntiles: big_meta.ntiles,
                little_endian: true, // build_test_cog always writes little-endian
            },
            ..resident // reuse geo/dims fields; only `index`/`source_id` matter to tile_location
        };
        let cache2 = crate::cache::new_index_cache(64 << 20);
        let counting = CountingRangeSource::new(&big_mem);
        for i in [0u64, 1, 2] {
            big_lazy
                .tile_location(&counting, &cache2, i)
                .expect("lazy tile_location over CountingRangeSource");
        }
        let full_array_bytes = big_meta.ntiles * big_meta.elem_size as u64 * 2; // offsets + bytecounts
        assert!(
            counting.bytes() < full_array_bytes / 2,
            "expected a small tile window to read far fewer bytes than the full index: {} bytes \
             read vs {full_array_bytes} for the full array",
            counting.bytes(),
        );
    }

    /// Task 3: `parse()` must pick the SAME tile locations whether it built `Resident` (the
    /// array-byte size is below the threshold) or `Lazy` (at/above it) — the threshold only
    /// changes how the index is stored, never what it answers. Forces each mode via
    /// `TERRASERVE_INDEX_LAZY_BYTES` on two `parse()` calls of the SAME bytes, then checks every
    /// tile index (plus one past the end) agrees.
    #[test]
    fn resident_and_lazy_indexes_are_identical_for_every_tile() {
        let _g = env_lock();
        let (bytes, _meta) = build_test_cog(40, 40, 256); // 1600 tiles
        let mem = MemRangeSource { data: bytes };
        let cache = crate::cache::new_index_cache(64 << 20);

        std::env::set_var("TERRASERVE_INDEX_LAZY_BYTES", "0"); // force Lazy
        let lazy = parse(&mem);
        std::env::set_var("TERRASERVE_INDEX_LAZY_BYTES", "999999999999"); // force Resident
        let resident = parse(&mem);
        std::env::remove_var("TERRASERVE_INDEX_LAZY_BYTES");
        let lazy = lazy.expect("lazy parse");
        let resident = resident.expect("resident parse");

        assert!(matches!(lazy.levels[0].index, TileIndex::Lazy { .. }));
        assert!(matches!(
            resident.levels[0].index,
            TileIndex::Resident { .. }
        ));
        let n = lazy.levels[0].tiles_across() as u64 * lazy.levels[0].tiles_down() as u64;
        for i in 0..=n {
            // include one past the end
            let a = lazy.levels[0].tile_location(&mem, &cache, i).unwrap();
            let b = resident.levels[0].tile_location(&mem, &cache, i).unwrap();
            assert_eq!(a, b, "tile {i}");
        }
    }

    /// Task 3: a `Lazy`-opened COG must not pay to read the full tile-index arrays just to
    /// answer a handful of `tile_location` lookups — the whole point of the lazy-open path.
    /// Forces small chunks (`TERRASERVE_INDEX_CHUNK=64`) on a 1600-tile synthetic COG and checks
    /// that touching 8 tiles reads well under a quarter of what reading both full arrays would
    /// cost.
    #[test]
    fn lazy_open_reads_only_touched_index_chunks() {
        let _g = env_lock();
        std::env::set_var("TERRASERVE_INDEX_LAZY_BYTES", "0");
        std::env::set_var("TERRASERVE_INDEX_CHUNK", "64"); // small chunks
        let (bytes, _meta) = build_test_cog(40, 40, 256); // 1600 tiles
        let mem = MemRangeSource { data: bytes };
        let parsed = parse(&mem); // Lazy; reads NO tile arrays
        let cog = parsed.expect("lazy parse");
        let counting = CountingRangeSource::new(&mem);
        let cache = crate::cache::new_index_cache(64 << 20);
        let lvl = &cog.levels[0];
        for i in 0..8u64 {
            let _ = lvl.tile_location(&counting, &cache, i).unwrap();
        } // one chunk-ish
        let read = counting.bytes();
        let full = 1600u64 * 8 * 2; // both arrays fully
        std::env::remove_var("TERRASERVE_INDEX_LAZY_BYTES");
        std::env::remove_var("TERRASERVE_INDEX_CHUNK");
        assert!(read < full / 4, "read {read} should be << full {full}");
    }

    /// Hardening-review regression: a level whose tile-index arrays fit INLINE in the IFD
    /// entry's own value field (a single-tile level — `1 * elem_size(8) <= 8`, the BigTIFF
    /// inline cap) must stay `Resident` even with the lazy threshold forced to its minimum.
    /// An inline array has no out-of-line file offset to window over; `entry_array_offset`
    /// would misread the inline value bytes as a bogus file offset if `parse()` ever picked
    /// `Lazy` for it — the `offsets_out_of_line`/`bytecounts_out_of_line` guards in `parse()`
    /// exist precisely to prevent that. Also checks `tile_location` still returns the correct
    /// `(offset, len)` for the one tile.
    #[test]
    fn inline_tile_index_stays_resident_even_when_lazy_forced() {
        let _g = env_lock();
        let (bytes, meta) = build_test_cog(1, 1, 16); // single tile -> inline TileOffsets/TileByteCounts
        assert_eq!(meta.ntiles, 1);
        assert!(
            meta.inline,
            "fixture must actually exercise the inline-array path"
        );

        std::env::set_var("TERRASERVE_INDEX_LAZY_BYTES", "0"); // force Lazy wherever legal
        let src = MemRangeSource { data: bytes };
        let parsed = parse(&src);
        std::env::remove_var("TERRASERVE_INDEX_LAZY_BYTES");
        let cog = parsed.expect("inline-index synthetic COG must parse");

        let lvl = &cog.levels[0];
        assert!(
            matches!(lvl.index, TileIndex::Resident { .. }),
            "an inline tile-index array must stay Resident even with the lazy threshold forced to 0, got {:?}",
            lvl.index
        );

        let cache = crate::cache::new_index_cache(64 << 20);
        let got = lvl
            .tile_location(&src, &cache, 0)
            .expect("tile_location on inline-index level");
        assert_eq!(got, Some((1_000_000, 100)));
    }

    /// Coverage the Task-2 review flagged: `build_test_cog` only ever emits BigTIFF LONG8
    /// (`elem_size=8`) little-endian arrays, so `index_chunk_entry`'s `elem_size == 4` (classic
    /// TIFF LONG / u32 — what a real classic-TIFF COG's TileOffsets/TileByteCounts use) and
    /// `little_endian == false` (big-endian "MM" byte order) decode branches were never directly
    /// exercised. `parse()` making `Lazy` live for real COGs means both are now reachable in
    /// production, not just the synthetic BigTIFF/LE fixture. Calls `index_chunk_entry` directly
    /// against a hand-built u32 array, once little-endian and once big-endian.
    #[test]
    fn index_chunk_entry_decodes_classic_tiff_u32_and_big_endian() {
        let values: [u32; 4] = [1000, 2000, 3000, 4000];
        let array_off: u64 = 16; // arbitrary nonzero offset, with leading padding bytes
        let cache = crate::cache::new_index_cache(64 << 20);

        // elem_size = 4 (classic-TIFF LONG), little-endian.
        let mut le_buf = vec![0u8; array_off as usize];
        for v in values {
            le_buf.extend_from_slice(&v.to_le_bytes());
        }
        let le_src = MemRangeSource { data: le_buf };
        for (i, &want) in values.iter().enumerate() {
            let got = index_chunk_entry(
                &le_src,
                &cache,
                /* source_id */ 1,
                array_off,
                /* elem_size */ 4,
                values.len() as u64,
                /* little */ true,
                i as u64,
            )
            .expect("index_chunk_entry (u32 LE)");
            assert_eq!(got, want as u64, "entry {i} (classic-TIFF LONG, LE)");
        }

        // elem_size = 4, big-endian ("MM" byte order).
        let mut be_buf = vec![0u8; array_off as usize];
        for v in values {
            be_buf.extend_from_slice(&v.to_be_bytes());
        }
        let be_src = MemRangeSource { data: be_buf };
        for (i, &want) in values.iter().enumerate() {
            let got = index_chunk_entry(
                &be_src,
                &cache,
                /* source_id */
                2, // distinct from the LE case above (shares one `cache`)
                array_off,
                /* elem_size */ 4,
                values.len() as u64,
                /* little */ false,
                i as u64,
            )
            .expect("index_chunk_entry (u32 BE)");
            assert_eq!(got, want as u64, "entry {i} (classic-TIFF LONG, BE)");
        }

        // elem_size = 2 (classic-TIFF SHORT TileByteCounts) — the last entry must not
        // slice-OOB (regression for the fixed 4-byte-read-on-a-2-byte-array bug).
        let shorts: [u16; 4] = [10, 20, 30, 40];
        for (little, sid) in [(true, 3u64), (false, 4u64)] {
            let mut buf = vec![0u8; array_off as usize];
            for v in shorts {
                buf.extend_from_slice(&if little {
                    v.to_le_bytes()
                } else {
                    v.to_be_bytes()
                });
            }
            let src = MemRangeSource { data: buf };
            for (i, &want) in shorts.iter().enumerate() {
                let got = index_chunk_entry(
                    &src,
                    &cache,
                    sid,
                    array_off,
                    2,
                    shorts.len() as u64,
                    little,
                    i as u64,
                )
                .expect("index_chunk_entry (u16 SHORT)");
                assert_eq!(got, want as u64, "entry {i} (SHORT, little={little})");
            }
        }
    }

    /// Local self-skipping check (mirrors `tests/render_seam.rs`'s fixture guard): renders the
    /// same real-world window of the ~1 GB `cascais.cog.deflate.tif` fixture once forced Lazy
    /// (`TERRASERVE_INDEX_LAZY_BYTES=0`) and once forced Resident (a huge threshold), and asserts
    /// the two RGBA buffers are byte-identical. Skips (rather than fails) when the big fixture
    /// COG isn't checked out, so a lean checkout still passes.
    #[test]
    fn cascais_lazy_and_resident_render_identically() {
        const COG_PATH: &str = "../cogs/cascais.cog.deflate.tif";
        const STYLE_PATH: &str = "fixtures/styles/rgb.json";
        if !std::path::Path::new(COG_PATH).exists() || !std::path::Path::new(STYLE_PATH).exists() {
            eprintln!("skipping cascais_lazy_and_resident_render_identically: fixtures absent");
            return;
        }
        let _g = env_lock();
        let style = crate::style::Style::load(STYLE_PATH).expect("load rgb.json style");
        // A native-resolution window (same as `tests/render_seam.rs`'s `sc_native_center` case).
        const BBOX: [f64; 4] = [-112701.25, -106296.25, -112573.25, -106168.25];
        let src = LocalFileRangeSource::open(COG_PATH).expect("open cascais fixture");

        std::env::set_var("TERRASERVE_INDEX_LAZY_BYTES", "0"); // force Lazy
        let cog_lazy = parse(&src);
        std::env::set_var("TERRASERVE_INDEX_LAZY_BYTES", "999999999999"); // force Resident
        let cog_resident = parse(&src);
        std::env::remove_var("TERRASERVE_INDEX_LAZY_BYTES");
        let cog_lazy = cog_lazy.expect("lazy parse of cascais fixture");
        let cog_resident = cog_resident.expect("resident parse of cascais fixture");
        assert!(matches!(cog_lazy.levels[0].index, TileIndex::Lazy { .. }));
        assert!(matches!(
            cog_resident.levels[0].index,
            TileIndex::Resident { .. }
        ));

        fn mk_req<'a>(style: &'a crate::style::Style) -> crate::render::RenderRequest<'a> {
            crate::render::RenderRequest {
                cog_path: COG_PATH,
                bbox: BBOX,
                crs: "EPSG:3763",
                src_crs: "EPSG:3763",
                width: 512,
                height: 512,
                resample: crate::backend::Resample::Nearest,
                style,
                band_math: None,
                index_cache: crate::cache::new_index_cache(64 << 20),
            }
        }

        let got_lazy = crate::render::render_with_cog(&mk_req(&style), &cog_lazy, &src, None)
            .expect("render_with_cog (lazy)");
        let got_resident =
            crate::render::render_with_cog(&mk_req(&style), &cog_resident, &src, None)
                .expect("render_with_cog (resident)");

        assert_eq!(got_lazy.len(), got_resident.len(), "output sizes differ");
        assert!(
            got_lazy == got_resident,
            "lazy vs resident render pixels differ"
        );
    }
}
