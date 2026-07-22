// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! FlatGeoBuf (`.fgb`) container reader — bespoke, hand-written on the `flatbuffers` runtime
//! (a codec, not a format reader: see `Cargo.toml`). Reads through the existing
//! `cog::RangeSource` byte-range trait (local + S3 for free) — FGB does not invent a reader.
//!
//! ## Container layout
//! ```text
//! magic (8 bytes: 66 67 62 03 66 67 62 01)        "fgb" 0x03 "fgb" 0x01
//! u32 header_size · Header FlatBuffer               size-prefixed, at file offset 12
//! [ packed Hilbert R-tree ]                          present iff index_node_size>0 && features_count>0
//! features: (u32 feat_size · Feature FlatBuffer)*    each size-prefixed, Hilbert-ordered
//! ```
//!
//! This module (Task 2 of the FlatGeoBuf batch) opens the container, validates the magic,
//! decodes the Header (`header.rs`), and computes the byte layout (`index_size` +
//! `features_start`) — it reads **no** index nodes and **no** features at open. The packed
//! R-tree windowed traversal and per-feature geometry/property decode are later tasks in the
//! same batch; this type only exposes header-derived metadata for now.

mod feat;
pub mod header;
mod rtree;

use std::io;

use crate::cog::RangeSource;
use crate::vector::feature::Feature;
use flatbuffers::{ForwardsUOffset, Table, Vector};
use header::Header;

/// FlatGeoBuf magic: `"fgb"` 0x03 (spec version 3) `"fgb"` 0x01 (padding/alignment marker).
const MAGIC: [u8; 8] = [0x66, 0x67, 0x62, 0x03, 0x66, 0x67, 0x62, 0x01];

/// A single Feature FlatBuffer larger than this is treated as a malformed/hostile length prefix
/// and rejected before allocation (real features are KB-scale). 256 MiB is far above any
/// legitimate one.
const MAX_FEATURE_BYTES: u64 = 256 * 1024 * 1024;
/// The Header FlatBuffer larger than this is rejected before allocation (real headers are
/// KB-scale).
const MAX_HEADER_BYTES: u64 = 64 * 1024 * 1024;

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Max features `WindowedSource::query` will decode for one request, bounding per-request memory
/// on a wide/low-zoom query against a dense `.fgb`. Env `TERRASERVE_FGB_MAX_QUERY_FEATURES`,
/// default 500_000. Read fresh each call so tests/ops can tune it. Fail-open: a query matching
/// more than the cap decodes the first `cap` (R-tree/spatial order) and warns, rather than
/// allocating unbounded.
fn max_query_features() -> usize {
    std::env::var("TERRASERVE_FGB_MAX_QUERY_FEATURES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(500_000)
}

/// An opened `.fgb` file: the parsed `Header` plus the computed container layout. Reads
/// through `R: RangeSource` — a local file or S3, whichever `R` is (the same seam COG uses).
pub struct FgbSource<R: RangeSource> {
    src: R,
    header: Header,
    index_start: u64,
    index_size: u64,
    features_start: u64,
    crs: Option<String>,
}

impl<R: RangeSource> FgbSource<R> {
    /// Open a `.fgb` over `src`: read + validate the magic, read + decode the Header, and
    /// compute the container layout (`index_start`, `index_size`, `features_start`). Reads no
    /// index nodes and no features — those are windowed reads issued lazily by
    /// `rtree_query`/`bruteforce_query`.
    pub fn open(src: R) -> io::Result<Self> {
        let prefix = src.read_range(0, 12)?;
        if prefix.len() < 12 {
            return Err(invalid(
                "fgb: file shorter than the 12-byte container prefix",
            ));
        }
        if prefix[0..8] != MAGIC {
            return Err(invalid(format!(
                "fgb: bad magic {:02x?} (expected {:02x?})",
                &prefix[0..8],
                MAGIC
            )));
        }
        let header_size = u32::from_le_bytes(prefix[8..12].try_into().unwrap()) as u64;
        if header_size == 0 {
            return Err(invalid("fgb: header_size is 0"));
        }
        // FIX C: `header_size` is a fully attacker-controlled length prefix -- reject an
        // implausible value BEFORE `read_range` pre-allocates it (`cog.rs`/`s3.rs` both
        // allocate the full requested length up front).
        if header_size > MAX_HEADER_BYTES {
            return Err(invalid("fgb: header_size implausibly large"));
        }
        let header_bytes = src.read_range(12, header_size as usize)?;
        if header_bytes.len() != header_size as usize {
            return Err(invalid("fgb: short read on the Header FlatBuffer"));
        }
        let header = Header::parse(header_bytes)?;

        let index_start = 12 + header_size;
        // Task 7: `header.features_count()` is a raw, unbounded `u64` an attacker fully
        // controls -- `index_size()` itself now saturates rather than wraps/panics (see
        // `rtree.rs`), but a saturated (implausible) result must not silently become a bogus
        // `features_start` here: `checked_add` turns that into a clean `Err` instead.
        let idx_size =
            rtree::tree_layout(header.features_count(), header.index_node_size()).index_size();
        let Some(features_start) = index_start.checked_add(idx_size) else {
            return Err(invalid(
                "fgb: index section size overflows the container layout",
            ));
        };

        let crs = header.crs_code().map(|code| format!("EPSG:{code}"));

        Ok(FgbSource {
            src,
            header,
            index_start,
            index_size: idx_size,
            features_start,
            crs,
        })
    }

    pub fn features_count(&self) -> u64 {
        self.header.features_count()
    }

    /// Column names in schema order — the same order each feature's packed property bytes
    /// reference by index.
    pub fn column_names(&self) -> Vec<String> {
        self.header
            .columns()
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// `[west, south, east, north]` from the Header envelope, or the degenerate
    /// `[0.0, 0.0, 0.0, 0.0]` sentinel if the header carries none (a valid FlatGeoBuf file may
    /// omit `envelope`).
    pub fn full_extent(&self) -> [f64; 4] {
        self.header.envelope().unwrap_or([0.0, 0.0, 0.0, 0.0])
    }

    pub fn index_node_size(&self) -> u16 {
        self.header.index_node_size()
    }

    pub fn crs(&self) -> Option<&str> {
        self.crs.as_deref()
    }

    /// Byte size of the packed R-tree section (0 if the file has no index).
    pub fn index_size(&self) -> u64 {
        self.index_size
    }

    /// File offset where the (Hilbert-ordered, size-prefixed) features section begins.
    pub fn features_start(&self) -> u64 {
        self.features_start
    }

    /// Decode at most `cap` features overlapping `bbox` (see `WindowedSource::query`, which
    /// delegates here with `cap = max_query_features()`) — the cap bounds per-request memory:
    /// `rtree_query` returning a big `Vec<u64>` of offsets is cheap (8 bytes each), but
    /// `decode_at`-ing every hit into a full `Feature` is the expensive part, so the cap bounds
    /// the decode loop, not the candidate list. Fail-open: warns once and returns the first
    /// `cap` (R-tree/spatial order) if more than `cap` match, rather than allocating unbounded.
    pub fn query_capped(&self, bbox: [f64; 4], cap: usize) -> Vec<Feature> {
        let hits = match self.rtree_query(bbox) {
            Ok(h) => h,
            Err(_) => return Vec::new(),
        };
        if hits.len() > cap {
            eprintln!(
                "fgb: query matched {} features; capping decode at {} (raise TERRASERVE_FGB_MAX_QUERY_FEATURES)",
                hits.len(),
                cap
            );
        }
        let mut out = Vec::with_capacity(hits.len().min(cap));
        for rel_offset in hits.into_iter().take(cap) {
            if let Ok(Some(feature)) = self.decode_at(rel_offset) {
                if bbox_intersects(feature.bbox, bbox) {
                    out.push(feature);
                }
            }
        }
        out
    }

    /// Windowed bbox query (`[minx, miny, maxx, maxy]`): top-down packed-R-tree traversal via
    /// `rtree::query`, pruning subtrees whose stored bbox doesn't overlap. Falls back to
    /// `bruteforce_query` when the file has no index (`index_node_size < 2` or
    /// `features_count == 0` — `rtree::tree_layout` returns an empty `Layout` in exactly those
    /// cases). Returns each matching feature's byte offset *relative to `features_start`* —
    /// add `features_start()` to seek an absolute file offset.
    pub fn rtree_query(&self, bbox: [f64; 4]) -> io::Result<Vec<u64>> {
        if self.header.index_node_size() < 2 || self.header.features_count() == 0 {
            return self.bruteforce_query(bbox);
        }
        rtree::query(
            &self.src,
            self.index_start,
            self.header.features_count(),
            self.header.index_node_size(),
            bbox,
        )
    }

    /// Decode every feature in the features section, sequentially, in on-disk (Hilbert-sorted —
    /// see `rtree.rs`'s module doc; NOT source-insertion order) order. Reads exactly
    /// `features_count` size-prefixed Feature FlatBuffers via `read_feature_record` (no
    /// reliance on the file's total length), decoding each through `feat::decode_feature`. A
    /// feature whose geometry isn't modeled (`Ok(None)` — `MultiPoint`/`GeometryCollection`/
    /// absent) is skipped, not included. Each kept `Feature`'s `fid` is set to its byte offset
    /// relative to `features_start` — the same unit `rtree_query`/`bruteforce_query` return, so
    /// a feature has one stable id regardless of whether it was reached by this sequential scan
    /// or by `decode_at` via a windowed query.
    ///
    /// FIX D: grows incrementally (`Vec::new()`, same as `bruteforce_query`) rather than
    /// pre-sizing by `features_count` — that count is a raw, unbounded, attacker-controlled
    /// `u64` `open`'s own overflow guard doesn't fully cover (a no-index file, `index_node_size
    /// < 2`, makes `index_size()` zero regardless of `features_count`, so `open` can succeed
    /// even for `features_count = u64::MAX`); `Vec::with_capacity(u64::MAX as usize)` was a hard
    /// "capacity overflow" panic reachable through this `pub` function. FIX C's per-feature
    /// `size` guard (via `read_feature_record`) then turns a malformed count into a clean
    /// past-EOF `Err` on the first bad record instead.
    pub fn decode_all(&self) -> io::Result<Vec<Feature>> {
        let mut out = Vec::new();
        let mut rel_offset = 0u64;
        for _ in 0..self.header.features_count() {
            let (body, record_len) = self.read_feature_record(rel_offset)?;
            if let Some(mut feature) = feat::decode_feature(&body, &self.header)? {
                feature.fid = rel_offset;
                out.push(feature);
            }
            rel_offset += record_len;
        }
        Ok(out)
    }

    /// Decode a single feature at byte offset `rel_offset` (relative to `features_start` — the
    /// unit `rtree_query`/`bruteforce_query` return). The windowed-read entry point (Task 5):
    /// one pair of `read_range` calls (size prefix + body), no sequential scan.
    pub fn decode_at(&self, rel_offset: u64) -> io::Result<Option<Feature>> {
        let (body, _record_len) = self.read_feature_record(rel_offset)?;
        let mut feature = feat::decode_feature(&body, &self.header)?;
        if let Some(f) = feature.as_mut() {
            f.fid = rel_offset;
        }
        Ok(feature)
    }

    /// Read one size-prefixed Feature FlatBuffer at `rel_offset` (relative to `features_start`):
    /// the `u32 feat_size` prefix (consumed, not returned) plus the `feat_size`-byte body.
    /// Returns `(body, 4 + feat_size)` — the second value is how far a sequential-scan cursor
    /// (`decode_all`) should advance to reach the next record. Same read shape as
    /// `bruteforce_query`'s loop (kept separate: that one also decodes+tests a bbox inline,
    /// this one just fetches raw bytes for `feat::decode_feature`).
    fn read_feature_record(&self, rel_offset: u64) -> io::Result<(Vec<u8>, u64)> {
        let size_bytes = self.src.read_range(self.features_start + rel_offset, 4)?;
        if size_bytes.len() != 4 {
            return Err(invalid("fgb: short read on a feature size prefix"));
        }
        let size = u32::from_le_bytes(size_bytes[0..4].try_into().unwrap()) as u64;
        // FIX C: `size` is a fully attacker-controlled length prefix -- reject an implausible
        // value BEFORE `read_range` pre-allocates it (see `open`'s matching `header_size` guard).
        if size > MAX_FEATURE_BYTES {
            return Err(invalid("fgb: feature size prefix implausibly large"));
        }
        let body = self
            .src
            .read_range(self.features_start + rel_offset + 4, size as usize)?;
        if body.len() != size as usize {
            return Err(invalid("fgb: short read on a Feature FlatBuffer"));
        }
        Ok((body, 4 + size))
    }

    /// Sequential scan of every feature (decoding just enough of each Feature FlatBuffer to
    /// get its geometry's bbox — see `feature_bbox`), keeping those that overlap `bbox`. The
    /// oracle `rtree_query` is tested against, and the fallback `rtree_query` itself uses when
    /// the file carries no spatial index. Reads exactly `features_count` size-prefixed Feature
    /// records — no reliance on knowing the file's total length.
    pub fn bruteforce_query(&self, bbox: [f64; 4]) -> io::Result<Vec<u64>> {
        let mut hits = Vec::new();
        let mut rel_offset = 0u64;
        for _ in 0..self.header.features_count() {
            let size_bytes = self.src.read_range(self.features_start + rel_offset, 4)?;
            if size_bytes.len() != 4 {
                return Err(invalid("fgb: short read on a feature size prefix"));
            }
            let size = u32::from_le_bytes(size_bytes[0..4].try_into().unwrap()) as u64;
            // FIX C: same untrusted-length guard as `read_feature_record` -- reject an
            // implausible `size` before `read_range` pre-allocates it.
            if size > MAX_FEATURE_BYTES {
                return Err(invalid("fgb: feature size prefix implausibly large"));
            }
            let body = self
                .src
                .read_range(self.features_start + rel_offset + 4, size as usize)?;
            if body.len() != size as usize {
                return Err(invalid("fgb: short read on a Feature FlatBuffer"));
            }
            if let Some(fb) = feature_bbox(&body) {
                if bbox_intersects(fb, bbox) {
                    hits.push(rel_offset);
                }
            }
            rel_offset += 4 + size;
        }
        Ok(hits)
    }
}

/// Standard min/max bbox overlap test — same test `rtree::query` uses internally, duplicated
/// here (rather than made `pub(crate)` there) because this one operates on a fully-decoded
/// feature bbox, not a raw R-tree node; kept next to `feature_bbox`, its only caller.
fn bbox_intersects(a: [f64; 4], b: [f64; 4]) -> bool {
    a[2] >= b[0] && a[0] <= b[2] && a[3] >= b[1] && a[1] <= b[3]
}

/// Task 5: the windowed-seam entry point — every serving path (WMS GetMap/GFI, MVT, WMTS) reads
/// a `.fgb` layer through this impl via `VectorSource::Windowed`/`features_in(bbox)`.
impl<R: RangeSource + Send + Sync> crate::vector::source::WindowedSource for FgbSource<R> {
    /// Features overlapping `bbox` (source CRS): candidate offsets from `rtree_query` (which
    /// itself falls back to `bruteforce_query` when the file carries no usable index —
    /// `index_node_size < 2` or zero features, `rtree::tree_layout`'s empty-`Layout` cases),
    /// each decoded via `decode_at`, then re-filtered against the feature's own decoded
    /// `bbox` — the R-tree only proves the *node* bbox overlaps, not the feature's actual
    /// geometry (a node packs several features under one conservative bbox). A `decode_at`
    /// that returns `Ok(None)` (unmodeled geometry — MultiPoint/GeometryCollection) is skipped,
    /// not an error. On an I/O error from `rtree_query` itself, fails open (empty result) —
    /// `WindowedSource::query` has no error channel; a request-time read failure should not
    /// panic the server. Delegates to `query_capped` with `max_query_features()` — a wide/
    /// low-zoom query against a dense `.fgb` decodes at most that many features, bounding
    /// per-request memory (see `query_capped`'s doc comment).
    fn query(&self, bbox: [f64; 4]) -> Vec<Feature> {
        self.query_capped(bbox, max_query_features())
    }

    /// `[west, south, east, north]` from the Header envelope — delegates to the inherent
    /// `FgbSource::full_extent` (method resolution prefers the inherent impl, so this is not
    /// self-recursive).
    fn full_extent(&self) -> [f64; 4] {
        self.full_extent()
    }

    /// `"EPSG:{code}"` from the Header CRS — delegates to the inherent `FgbSource::crs`.
    fn crs(&self) -> Option<&str> {
        self.crs()
    }

    /// Attribute schema from the Header's `columns()` (name + `ColumnType`, already parsed at
    /// `open()`) — no feature read, no `query`/`decode_all`. Mirrors the two-bucket type model
    /// `mvt_http::feature_field_schema_slice` derives from actually-decoded features
    /// (`Value::Num` vs `Value::Str`): the numeric `ColumnType`s (`feat::decode_props`'s 0..=10 —
    /// Byte/UByte/Bool/Short/UShort/Int/UInt/Long/ULong/Float/Double) map to `"Number"`, and 11
    /// (`String`) maps to `"String"`. `Binary`(14) is omitted — `decode_props` never inserts a
    /// `Value` for it, so it has no place in a schema derived from decoded values — and neither
    /// is any `ColumnType` this decoder doesn't recognize at all (`>= 15`). `Json`(12)/
    /// `DateTime`(13) DO now decode to `Value::Str` (`decode_props`'s FIX A), so this header-only
    /// shortcut technically under-reports those two relative to a full scan; left as a known gap
    /// (a cheap header read vs. an actual per-feature decode) rather than expanded here.
    fn field_schema(&self) -> std::collections::BTreeMap<String, String> {
        let mut out = std::collections::BTreeMap::new();
        for (name, ty) in self.header.columns() {
            let mapped = match ty {
                0..=10 => "Number",
                11 => "String",
                _ => continue,
            };
            out.insert(name, mapped.to_string());
        }
        out
    }
}

/// Validate a FlatBuffer's root table location exactly as `header::Header::parse` does (same 4
/// checks: buffer big enough for a root uoffset; the uoffset resolves in-bounds; the vtable —
/// found via the backwards soffset at the root location — resolves in-bounds; the vtable's own
/// declared size fits the buffer). Returns the validated root location, or `None` for anything
/// malformed — never panics.
fn validate_root_table(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    let root_off = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
    if root_off == 0 || root_off + 4 > buf.len() {
        return None;
    }
    let soffset = i32::from_le_bytes(buf[root_off..root_off + 4].try_into().unwrap());
    let vtable_loc = root_off as i64 - soffset as i64;
    if vtable_loc < 0 || (vtable_loc as usize) + 4 > buf.len() {
        return None;
    }
    let vtable_loc = vtable_loc as usize;
    let vtable_num_bytes =
        u16::from_le_bytes(buf[vtable_loc..vtable_loc + 2].try_into().unwrap()) as usize;
    if vtable_num_bytes < 4 || vtable_loc + vtable_num_bytes > buf.len() {
        return None;
    }
    Some(root_off)
}

/// Decode a Feature FlatBuffer's geometry bbox — the bruteforce oracle's only per-feature work.
/// Scans every `xy` coordinate vector reachable from the geometry, at every nesting level: a
/// simple geometry (Point/LineString/Polygon, even multi-ring) carries its coordinates
/// directly in the top-level `Geometry.xy`; a Multi*/GeometryCollection nests each
/// sub-geometry under `Geometry.parts`, recursively, per FlatGeoBuf's `geometry.fbs`. Returns
/// `None` for a feature with no geometry or no coordinates at all (such a feature is excluded
/// from every bbox query — it cannot overlap anything).
///
/// Field slot numbers: `Feature.geometry`=0 (voffset 4) and `Geometry.xy`=1 (voffset 6) were
/// verified empirically (hex-dumping `fixtures/fgb/tiny.fgb`, the same method `header.rs`'s
/// module doc describes) — the fixture's 3 features (2 Points + 1 Polygon) exercise exactly
/// this path, and their decoded bboxes match a hand-walk of the raw bytes exactly (see
/// `rtree.rs`'s `rtree_query_matches_bruteforce_on_tiny` test). `Geometry.parts`=7 (voffset
/// 18) is transcribed from the schema's field order (`ends, xy, z, m, t, tm, type, parts`) but
/// **not** exercised by any fixture here — `tiny.fgb` has no Multi*/GeometryCollection
/// features. Flagged for Task 6 (the PRT.fgb / real-world MultiPolygon run) to confirm.
fn feature_bbox(body: &[u8]) -> Option<[f64; 4]> {
    let root_loc = validate_root_table(body)?;
    // Task 7: `accumulate_geometry_bbox` below recurses into `Geometry.parts` unboundedly and,
    // like every other accessor here, reads through unchecked `flatbuffers::Table::get` calls
    // -- so it needs the exact same full structural verification (bounds AND the verifier's
    // `max_depth` recursion cap) `feat::decode_feature` gates on, or a crafted deeply-nested
    // `parts` chain / out-of-bounds field offset could stack-overflow or OOB-read here instead.
    if !feat::verify_feature_buf(body) {
        return None;
    }
    let feature = unsafe { Table::new(body, root_loc) };
    let geometry = unsafe { feature.get::<ForwardsUOffset<Table<'_>>>(4, None) }?;
    let mut acc: Option<[f64; 4]> = None;
    accumulate_geometry_bbox(&geometry, &mut acc);
    acc
}

fn accumulate_geometry_bbox(geometry: &Table<'_>, acc: &mut Option<[f64; 4]>) {
    if let Some(xy) = unsafe { geometry.get::<ForwardsUOffset<Vector<'_, f64>>>(6, None) } {
        let n = xy.len();
        let mut i = 0;
        while i + 1 < n {
            let (x, y) = (xy.get(i), xy.get(i + 1));
            *acc = Some(match *acc {
                Some([minx, miny, maxx, maxy]) => {
                    [minx.min(x), miny.min(y), maxx.max(x), maxy.max(y)]
                }
                None => [x, y, x, y],
            });
            i += 2;
        }
    }
    if let Some(parts) =
        unsafe { geometry.get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<Table<'_>>>>>(18, None) }
    {
        for i in 0..parts.len() {
            let part = parts.get(i);
            accumulate_geometry_bbox(&part, acc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::feature::Geometry;

    /// Task 6: a `RangeSource` wrapper that counts total bytes read through it, `Send + Sync`
    /// (unlike `rtree.rs`'s test-only `Rc<Cell<u64>>`-based `CountingRangeSource`, which is
    /// deliberately `!Sync` and only ever wraps the bare `rtree::query` function) — needed here
    /// because `WindowedSource for FgbSource<R>` is only implemented for `R: RangeSource + Send
    /// + Sync`, and the reads-less proof below goes through that trait method (the real serving
    /// path), not the inherent `rtree_query`.
    struct CountingRangeSource<R> {
        inner: R,
        bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }

    impl<R: RangeSource> RangeSource for CountingRangeSource<R> {
        fn read_range(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
            let out = self.inner.read_range(offset, len)?;
            self.bytes
                .fetch_add(len as u64, std::sync::atomic::Ordering::Relaxed);
            Ok(out)
        }
    }

    #[test]
    fn open_tiny_reads_header() {
        let src = crate::cog::LocalFileRangeSource::open("fixtures/fgb/tiny.fgb").unwrap();
        let fgb = FgbSource::open(src).expect("open tiny.fgb");
        assert_eq!(fgb.features_count(), 3);
        assert_eq!(fgb.column_names(), vec!["name", "pop"]);
        let e = fgb.full_extent(); // [w,s,e,n]
        assert!(
            (e[0] - 0.0).abs() < 1e-9 && (e[2] - 5.0).abs() < 1e-9 && (e[3] - 6.0).abs() < 1e-9
        );
        assert!(fgb.index_node_size() >= 1); // has an index (ogr2ogr default 16)
    }

    #[test]
    fn open_tiny_reports_crs_and_layout() {
        let src = crate::cog::LocalFileRangeSource::open("fixtures/fgb/tiny.fgb").unwrap();
        let fgb = FgbSource::open(src).expect("open tiny.fgb");
        assert_eq!(fgb.crs(), Some("EPSG:4326"));
        // Cross-checked by hand against the real file: header_size=636, index_size=160 (3
        // items @ node_size 16 -> 3 leaves + 1 root = 4 nodes * 40 bytes), features_start=808,
        // and that offset lands exactly on 3 size-prefixed Feature FlatBuffers ending at EOF.
        assert_eq!(fgb.index_size(), 160);
        assert_eq!(fgb.features_start(), 808);
    }

    #[test]
    fn open_rejects_bad_magic() {
        struct BadMagic;
        impl RangeSource for BadMagic {
            fn read_range(&self, _offset: u64, len: usize) -> io::Result<Vec<u8>> {
                Ok(vec![0u8; len])
            }
        }
        assert!(FgbSource::open(BadMagic).is_err());
    }

    /// FIX C: `header_size` (the container's leading `u32` prefix) is attacker-controlled and,
    /// before any validation, fed straight into `read_range` -- a crafted value near
    /// `0xFFFF_FFF0` (~4.29 GB) must be rejected with a clean `Err` BEFORE that read is
    /// attempted. The mock panics if ever asked to read anything past a small sanity bound, so
    /// a missing/misplaced guard fails this test loudly instead of trying to actually allocate
    /// ~4 GB.
    #[test]
    fn open_rejects_implausible_header_size_before_allocating() {
        struct PanicsOnHugeRead;
        impl RangeSource for PanicsOnHugeRead {
            fn read_range(&self, _offset: u64, len: usize) -> io::Result<Vec<u8>> {
                if len == 12 {
                    // the initial container-prefix read: valid magic + an absurd header_size
                    let mut buf = MAGIC.to_vec();
                    buf.extend_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
                    return Ok(buf);
                }
                assert!(
                    len < 10_000_000,
                    "read_range called with implausible len={len} -- the header_size guard \
                     must reject before this read, not after"
                );
                Ok(vec![0u8; len])
            }
        }
        let result = FgbSource::open(PanicsOnHugeRead);
        assert!(
            result.is_err(),
            "an implausible header_size must be rejected with a clean Err"
        );
    }

    /// FIX C: a Feature's `u32 size` prefix is just as attacker-controlled as `header_size` --
    /// `read_feature_record` (used by `decode_at`/`decode_all`) and `bruteforce_query` each
    /// decode it and feed it straight into `read_range` before any validation. A crafted size
    /// near `0xFFFF_FFF0` must be rejected with a clean `Err` at BOTH call sites, before the
    /// giant read is attempted (same panicking-mock proof as the `header_size` test above).
    #[test]
    fn feature_size_guard_rejects_before_allocating_at_decode_at_and_bruteforce_query() {
        let mut fbb = flatbuffers::FlatBufferBuilder::new();
        let start = fbb.start_table();
        fbb.push_slot_always(20u16, 1u64); // Header.features_count -- plausible
        fbb.push_slot_always(22u16, 0u16); // Header.index_node_size -- no index section
        let end = fbb.end_table(start);
        fbb.finish_size_prefixed(end, None);

        let mut file_bytes = MAGIC.to_vec();
        file_bytes.extend_from_slice(fbb.finished_data());
        // The features section starts right here (index_size is 0 -- no index). The one
        // feature's size prefix is an implausible ~4.29 GB; no body bytes are needed since the
        // guard must reject before ever reading them.
        file_bytes.extend_from_slice(&0xFFFF_FFF0u32.to_le_bytes());

        struct PanicsOnHugeRead(Vec<u8>);
        impl RangeSource for PanicsOnHugeRead {
            fn read_range(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
                assert!(
                    len < 10_000_000,
                    "read_range called with implausible len={len} -- the feature-size guard \
                     must reject before this read, not after"
                );
                let start = offset as usize;
                if start > self.0.len() {
                    return Ok(Vec::new());
                }
                let end = (start + len).min(self.0.len());
                Ok(self.0[start..end].to_vec())
            }
        }

        let fgb = FgbSource::open(PanicsOnHugeRead(file_bytes)).expect("header/layout is valid");

        assert!(
            fgb.decode_at(0).is_err(),
            "decode_at must reject an implausible feature size prefix"
        );
        assert!(
            fgb.bruteforce_query([0.0, 0.0, 1.0, 1.0]).is_err(),
            "bruteforce_query must reject an implausible feature size prefix"
        );
    }

    /// Task 7: `features_count` is a raw, unbounded `u64` an attacker fully controls -- with it
    /// set to `u64::MAX`, `rtree::tree_layout(...).index_size()` saturates to `u64::MAX` (see
    /// `rtree.rs`'s own overflow tests), and `FgbSource::open`'s `index_start.checked_add
    /// (idx_size)` must then turn that into a clean `Err` rather than silently wrapping to a
    /// bogus small `features_start` -- and, above all, `open` must not panic getting there. The
    /// header FlatBuffer here is built by hand (`flatbuffers::FlatBufferBuilder`, `size_prefixed`
    /// to match the container's own `u32 header_size` convention -- see `header.rs`'s
    /// `verify_size_prefixed` doc comment) rather than mutating a real file's bytes, so the only
    /// thing under test is the `features_count` value itself.
    #[test]
    fn open_rejects_absurd_features_count_without_panicking() {
        let mut fbb = flatbuffers::FlatBufferBuilder::new();
        let start = fbb.start_table();
        fbb.push_slot_always(20u16, u64::MAX); // Header.features_count
        fbb.push_slot_always(22u16, 16u16); // Header.index_node_size -- a real index, not "none"
        let end = fbb.end_table(start);
        fbb.finish_size_prefixed(end, None);

        let mut file_bytes = MAGIC.to_vec();
        file_bytes.extend_from_slice(fbb.finished_data());

        struct InMemory(Vec<u8>);
        impl RangeSource for InMemory {
            fn read_range(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
                let start = offset as usize;
                if start > self.0.len() {
                    return Ok(Vec::new());
                }
                let end = (start + len).min(self.0.len());
                Ok(self.0[start..end].to_vec())
            }
        }

        let result = FgbSource::open(InMemory(file_bytes));
        assert!(
            result.is_err(),
            "an absurd features_count (u64::MAX) must be rejected with a clean Err, not panic"
        );
    }

    /// FIX D: `open`'s overflow guard above only catches an absurd `features_count` when it
    /// actually inflates `index_size` -- for a **no-index** file (`index_node_size < 2`),
    /// `rtree::tree_layout` returns an empty `Layout` (`index_size() == 0`) regardless of
    /// `features_count`, so `open` succeeds even for `u64::MAX`. `decode_all` is `pub` (callable
    /// by future tooling) and used to pre-size its output with
    /// `Vec::with_capacity(features_count as usize)` -- a hard "capacity overflow" panic on a
    /// malformed no-index file. Must now grow incrementally instead and hit a clean past-EOF
    /// short-read `Err` on the very first iteration (the mock has no feature bytes at all past
    /// `features_start`), never a panic.
    #[test]
    fn decode_all_rejects_absurd_features_count_on_a_no_index_file_without_panicking() {
        let mut fbb = flatbuffers::FlatBufferBuilder::new();
        let start = fbb.start_table();
        fbb.push_slot_always(20u16, u64::MAX); // Header.features_count
        fbb.push_slot_always(22u16, 0u16); // Header.index_node_size -- no index (the no-index branch)
        let end = fbb.end_table(start);
        fbb.finish_size_prefixed(end, None);

        let mut file_bytes = MAGIC.to_vec();
        file_bytes.extend_from_slice(fbb.finished_data());

        struct InMemory(Vec<u8>);
        impl RangeSource for InMemory {
            fn read_range(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
                let start = offset as usize;
                if start > self.0.len() {
                    return Ok(Vec::new());
                }
                let end = (start + len).min(self.0.len());
                Ok(self.0[start..end].to_vec())
            }
        }

        let fgb = FgbSource::open(InMemory(file_bytes))
            .expect("no-index layout: open succeeds even with an absurd features_count");

        let result = fgb.decode_all();
        assert!(
            result.is_err(),
            "decode_all must hit a clean past-EOF Err, not panic, on an absurd features_count"
        );
    }

    #[test]
    fn index_size_formula() {
        // tiny.fgb: 3 features, ogr2ogr default node_size 16. `index_size` itself now lives in
        // `rtree::tree_layout(...).index_size()` (Task 3 factored it out of this module).
        assert_eq!(rtree::tree_layout(3, 16).index_size(), 160);
        assert_eq!(rtree::tree_layout(0, 16).index_size(), 0);
        assert_eq!(rtree::tree_layout(5, 0).index_size(), 0);
    }

    /// FIX B: `query_capped` bounds the number of features `decode_at` materializes for one
    /// query, so a wide/low-zoom request against a dense `.fgb` can't spike per-request memory
    /// unbounded. `tiny.fgb` has 3 features; a bbox covering all of them must return at most
    /// `cap` when `cap` is smaller than the match count, and the full 3 when `cap` is generous
    /// (proving the default/uncapped-in-practice path is unaffected).
    #[test]
    fn query_capped_truncates_decode_to_the_cap() {
        let src = crate::cog::LocalFileRangeSource::open("fixtures/fgb/tiny.fgb").unwrap();
        let fgb = FgbSource::open(src).unwrap();
        let bbox = [-1.0, -1.0, 6.0, 7.0]; // covers all 3 tiny.fgb features

        let capped = fgb.query_capped(bbox, 2);
        assert!(
            capped.len() <= 2,
            "cap=2 must bound the decode to at most 2 features, got {}",
            capped.len()
        );

        let uncapped = fgb.query_capped(bbox, 1000);
        assert_eq!(
            uncapped.len(),
            3,
            "a generous cap must not truncate below the true match count"
        );
    }

    #[test]
    fn windowedsource_query_returns_only_window() {
        let src = crate::cog::LocalFileRangeSource::open("fixtures/fgb/tiny.fgb").unwrap();
        let fgb = FgbSource::open(src).unwrap();
        // a bbox around point b (5,6) returns exactly 1 feature with name="b"
        let got = crate::vector::source::WindowedSource::query(&fgb, [4.5, 5.5, 5.5, 6.5]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].props.get_str("name"), Some("b"));
    }

    #[test]
    fn windowedsource_full_extent_and_crs_match_header() {
        let src = crate::cog::LocalFileRangeSource::open("fixtures/fgb/tiny.fgb").unwrap();
        let fgb = FgbSource::open(src).unwrap();
        let ext = crate::vector::source::WindowedSource::full_extent(&fgb);
        assert!((ext[0] - 0.0).abs() < 1e-9 && (ext[2] - 5.0).abs() < 1e-9);
        assert_eq!(
            crate::vector::source::WindowedSource::crs(&fgb),
            Some("EPSG:4326")
        );
    }

    #[test]
    fn windowedsource_field_schema_from_header_columns_no_feature_decode() {
        // tiny.fgb's schema is (name: String, pop: Int) per open_tiny_reads_header above —
        // field_schema must report that WITHOUT decoding any of the 3 features (it reads only
        // the already-parsed Header.columns(), the fix for the 5.8 GB windowed-layer-setup bug).
        let src = crate::cog::LocalFileRangeSource::open("fixtures/fgb/tiny.fgb").unwrap();
        let fgb = FgbSource::open(src).unwrap();
        let schema = crate::vector::source::WindowedSource::field_schema(&fgb);
        assert_eq!(schema.len(), 2);
        assert_eq!(schema.get("name").map(String::as_str), Some("String"));
        assert_eq!(schema.get("pop").map(String::as_str), Some("Number"));

        // Cross-check against the actually-decoded schema (mvt_http's full-scan path) to prove
        // the header-derived shortcut agrees with reality on this fixture.
        let src2 = crate::cog::LocalFileRangeSource::open("fixtures/fgb/tiny.fgb").unwrap();
        let fgb2 = FgbSource::open(src2).unwrap();
        let vs = crate::vector::source::VectorSource::Windowed(std::sync::Arc::new(fgb2));
        let scanned = crate::mvt_http::feature_field_schema_vs(&vs);
        assert_eq!(schema, scanned);
    }

    /// Task 6 — the real-data oracle. `PRT.fgb` (6,097,126 Polygon features, EPSG:4326, VIDA
    /// worldwide-buildings extract for Portugal — see `~/.claude` memory
    /// `reference-vida-buildings-fgb-testset`) is NOT committed (1.5 GB); this test self-skips
    /// when it is absent, exactly like `tests/render_seam.rs`'s `have_fixtures()` guard, so a
    /// checkout without the big file still passes `cargo test`.
    ///
    /// (a) **Multi-level R-tree correctness**: a small bbox over Lisbon
    /// (lon -9.20..-9.10, lat 38.68..38.78) forces the traversal through several tree levels —
    /// `tree_layout(6_097_126, 16)` has ceil(log_16(6.1M)) ≈ 6 levels, so this is the proof
    /// `tiny.fgb`'s 4-node (1 root + 3 leaves) tree structurally cannot give. The oracle count
    /// comes from `ogrinfo -spat -9.20 38.68 -9.10 38.78 -so /home/mende012/geodata/vida/PRT.fgb
    /// PRT`, run once by hand: **`Feature Count: 25480`** (full-file count with no `-spat` is
    /// 6097126, confirming the header `features_count` this reader parses). GDAL's spatial
    /// filter does an exact geometry-`Intersects` test against the query rectangle (not just a
    /// bbox overlap) after its own R-tree candidate pass, whereas `WindowedSource::query` here
    /// only ever tests bbox-overlap (`bbox_intersects`, both at the R-tree-node level and against
    /// each decoded feature's own bbox — see the doc comment on the `WindowedSource` impl below);
    /// a feature whose bbox clips the corner of the query rect without its actual polygon ever
    /// crossing into it is a bbox-only false positive GDAL's exact test correctly excludes. So
    /// `query.len()` is expected to be >= the GDAL count, close but not necessarily exactly
    /// equal — asserted with a tolerance (measured empirically at open time below; see the
    /// in-test comment for the observed number).
    ///
    /// (b) **Reads-less at scale**: the SAME query, over a `CountingRangeSource`-wrapped file,
    /// must read a tiny fraction of the 1.5 GB file — proof the whole feature section (and the
    /// whole R-tree) is never touched for a small window.
    #[test]
    fn prt_fgb_windowed_matches_ogr_and_reads_less() {
        const PATH: &str = "/home/mende012/geodata/vida/PRT.fgb";
        if !std::path::Path::new(PATH).exists() {
            eprintln!("skipping prt_fgb_windowed_matches_ogr_and_reads_less: {PATH} absent");
            return;
        }

        // `ogrinfo -spat -9.20 38.68 -9.10 38.78 -so PRT.fgb PRT` -> "Feature Count: 25480"
        // (documented above; the same bbox order [minx, miny, maxx, maxy] this reader takes).
        const GDAL_SPAT_COUNT: usize = 25480;
        let bbox = [-9.20, 38.68, -9.10, 38.78];

        // (a) Correctness: wrap in the byte counter up front so (b) reuses the same query.
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counting = CountingRangeSource {
            inner: crate::cog::LocalFileRangeSource::open(PATH).unwrap(),
            bytes: counter.clone(),
        };
        let fgb = FgbSource::open(counting).unwrap();
        assert_eq!(fgb.features_count(), 6_097_126);

        let feats = crate::vector::source::WindowedSource::query(&fgb, bbox);
        assert!(!feats.is_empty());

        // Every returned feature's own decoded bbox must actually overlap the query bbox (the
        // `WindowedSource::query` re-filter, `mod.rs`'s doc comment on the impl) — never a raw
        // R-tree-node-bbox false positive leaking through.
        for f in &feats {
            assert!(
                bbox_intersects(f.bbox, bbox),
                "feature bbox {:?} does not overlap query bbox {:?}",
                f.bbox,
                bbox
            );
        }

        // Bbox-candidate count vs GDAL's exact-geometry-intersects count: expected to be close,
        // and never fewer (a bbox-overlap test can only ever be as-or-more permissive than an
        // exact intersects test over the SAME candidate set) -- a tolerance of 5% covers the
        // near-boundary bbox-only false positives an exact test excludes, without hiding a real
        // multi-level-traversal bug (which would show as a gross mismatch, not a few percent).
        let got = feats.len();
        let tolerance = (GDAL_SPAT_COUNT as f64 * 0.05).ceil() as usize;
        assert!(
            got >= GDAL_SPAT_COUNT && got <= GDAL_SPAT_COUNT + tolerance,
            "query returned {got} features, GDAL -spat reports {GDAL_SPAT_COUNT} \
             (tolerance ±{tolerance}) -- multi-level R-tree traversal may be wrong"
        );

        // (b) Reads-less: the whole file is 1.5+ GB; this windowed query (R-tree traversal +
        // decoding ~25k matched leaf features) must read a tiny fraction of it.
        let total_file_bytes = std::fs::metadata(PATH).unwrap().len();
        let bytes_read = counter.load(std::sync::atomic::Ordering::Relaxed);
        const MAX_BYTES: u64 = 50 * 1024 * 1024; // 50 MB
        assert!(
            bytes_read < MAX_BYTES,
            "read {bytes_read} bytes for a small-bbox query (file is {total_file_bytes} bytes) \
             -- expected well under {MAX_BYTES}"
        );
        assert!(bytes_read < total_file_bytes / 10);
        eprintln!(
            "prt_fgb_windowed_matches_ogr_and_reads_less: {got} features (GDAL {GDAL_SPAT_COUNT}), \
             {bytes_read} bytes read of {total_file_bytes} ({:.4}%)",
            100.0 * bytes_read as f64 / total_file_bytes as f64
        );
    }

    /// Task 6 — MultiPolygon decode against a real file. `COS2018v3-S2.fgb` (780,240
    /// MultiPolygon features, EPSG:3763, DGT COS2018 land-cover — see `~/.claude` memory
    /// `reference-cos2018-dgt-landcover`) is NOT committed (1.3 GB); self-skips when absent.
    ///
    /// Every one of this file's 780,240 features was scanned by hand (`ogr2ogr -f GeoJSONSeq`
    /// piped through a Python `json.loads` + `len(coordinates)` scan, run once): every single
    /// feature is a **1-part** MultiPolygon (a land-cover polygon plus its holes, never a
    /// disjoint multi-piece area) — so this file cannot exercise `decode_geometry`'s `parts.len()
    /// > 1` case either. What it DOES exercise, that `tiny.fgb` (a plain `Polygon`, type 3, no
    /// `parts` field at all) cannot: the real on-disk `Geometry.parts`(7:18) FlatBuffer field —
    /// `decode_geometry`'s `ty == 6` arm, `read_parts` resolving a nested `Geometry` sub-table,
    /// then `read_xy`/`read_ends` recursing INTO it — on a feature with a real multi-ring
    /// structure: **feature `ID=699772`** (`COS18_n4_C="6.1.1.1"`, `AREA_ha≈1625.77`) has **37
    /// rings** in its one part (`ogr2ogr -f GeoJSON` + `jq`, run once): ring 0 (exterior) has
    /// 3258 points, and rings 1..37 are 36 holes with lengths
    /// `[17,29,35,18,76,23,30,20,26,23,31,44,76,37,68,44,38,46,93,38,27,35,22,16,20,42,46,42,20,
    /// 22,40,80,47,22,58,20]`. Its bbox (computed from the same GeoJSON dump) is
    /// `[88839.18, -179394.11, 96883.47, -173740.15]` (EPSG:3763, matching the layer's own CRS).
    #[test]
    fn cos2018_multipolygon_decodes_real_ring_structure() {
        const PATH: &str = "/data/COS2018v3-S2.fgb";
        if !std::path::Path::new(PATH).exists() {
            eprintln!("skipping cos2018_multipolygon_decodes_real_ring_structure: {PATH} absent");
            return;
        }

        let src = crate::cog::LocalFileRangeSource::open(PATH).unwrap();
        let fgb = FgbSource::open(src).unwrap();
        assert_eq!(fgb.features_count(), 780_240);
        assert_eq!(fgb.crs(), Some("EPSG:3763"));

        let bbox = [88839.18, -179394.11, 96883.47, -173740.15];
        let feats = crate::vector::source::WindowedSource::query(&fgb, bbox);
        assert!(
            !feats.is_empty(),
            "no features in feature 699772's own bbox"
        );

        let f = feats
            .iter()
            .find(|f| f.props.get_f64("ID") == Some(699772.0))
            .expect("feature ID=699772 present in this window");
        assert_eq!(f.props.get_str("COS18_n4_C"), Some("6.1.1.1"));
        let area = f.props.get_f64("AREA_ha").expect("AREA_ha");
        assert!((area - 1625.77154409962).abs() < 1e-3, "AREA_ha={area}");

        match &f.geom {
            Geometry::MultiPolygon(parts) => {
                assert_eq!(parts.len(), 1, "feature 699772 is a 1-part MultiPolygon");
                let rings = &parts[0];
                assert_eq!(rings.len(), 37, "expected 37 rings (1 exterior + 36 holes)");
                assert_eq!(rings[0].len(), 3258, "exterior ring point count");
                let hole_lens: Vec<usize> = rings[1..].iter().map(|r| r.len()).collect();
                assert_eq!(
                    hole_lens,
                    vec![
                        17, 29, 35, 18, 76, 23, 30, 20, 26, 23, 31, 44, 76, 37, 68, 44, 38, 46, 93,
                        38, 27, 35, 22, 16, 20, 42, 46, 42, 20, 22, 40, 80, 47, 22, 58, 20
                    ]
                );
            }
            other => panic!("expected MultiPolygon, got {other:?}"),
        }
    }
}
