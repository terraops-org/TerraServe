// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Feature FlatBuffer decode: `.fgb` per-feature bytes → the internal `Geometry`/`Props` model
//! (`crate::vector::feature`). Hand-written accessors over `flatbuffers::Table`, same style as
//! `header.rs` — no `flatc`, no generated code, and (per this module's contract) never a panic
//! on malformed input.
//!
//! ## Feature FlatBuffer (field_id : voffset)
//!
//! `Feature`: **geometry `Geometry`**(0:4) **properties `[u8]`**(1:6) columns(2:8 — a
//! per-feature schema override; always empty for a single-schema file like `ogr2ogr`'s output,
//! so not read here).
//! `Geometry`: **ends `[u32]`**(0:4) **xy `[f64]`**(1:6) z(2:8) m(3:10) t(4:12) tm(5:14)
//! **type `u8`**(6:16) **parts `[Geometry]`**(7:18).
//!
//! `type` is 0 (`Unknown`) for a single-geometry-type layer — `tiny.fgb`'s Points/Polygon all
//! read 0 here — in which case the Header's own `geometry_type` (`header.rs`) is the fallback,
//! mirroring how other FlatGeoBuf readers resolve a per-feature type of 0.
//!
//! ## Geometry → `Geometry` mapping (2D only; any Z/M ordinates present are read then dropped)
//!
//! - `Point` (1): the first (only) `xy` pair.
//! - `LineString` (2): all of `xy`, no `ends`.
//! - `Polygon` (3): `xy` split into rings by `ends` (ring 0 = exterior, per this crate's
//!   `Geometry::Polygon` convention — matches `wkb.rs`).
//! - `MultiLineString` (5): `xy` split into parts by `ends` (one cut per line — flat encoding,
//!   no nested `parts`, same shape as `Polygon`).
//! - `MultiPolygon` (6): each element of `parts` is itself a `Polygon`-shaped `Geometry` (its
//!   own `xy` + `ends`), decoded recursively into one ring-set per part.
//! - `MultiPoint` (4) / `GeometryCollection` (7) / anything else unrecognized: **not modeled**
//!   → `None` — same contract as `wkb::decode_gpkg_geometry` (the caller skips the feature, not
//!   an error).
//!
//! The `MultiLineString`/`MultiPolygon` cases are transcribed from FlatGeoBuf's `geometry.fbs`
//! encoding rules but **not exercised by `tiny.fgb`** (2 Points + 1 Polygon only) — flagged for
//! Task 6 (the real-world `PRT.fgb` MultiPolygon run) to confirm empirically.
//!
//! ## Properties blob (`Feature.properties`, a raw `[u8]`)
//!
//! Repeated `(u16 column_index, value)` pairs running to the end of the blob. `column_index`
//! indexes `Header.columns()` for the name + `ColumnType` (a `u8`); the value's width/encoding
//! follows from that type: `Byte`/`UByte`/`Bool` = 1 byte, `Short`/`UShort` = 2, `Int`/`UInt` =
//! 4, `Long`/`ULong` = 8, `Float` = 4, `Double` = 8 (all little-endian), `String`/`Json`/
//! `DateTime` = `u32` length + UTF-8 bytes, `Binary` = `u32` length + raw bytes. Everything
//! decodes into `Props` as `Value::Num` (all the integer/float variants, cast to `f64` —
//! `Props`/`Value` has no dedicated integer type) or `Value::Str` (`String`, and `Json`/
//! `DateTime` too — JSON text and an ISO-8601 timestamp are both text, so surfacing them as
//! `Value::Str` matches the `Num`/`Str` model). `Binary`'s bytes are skipped (no `Value` can
//! hold raw bytes) but the cursor still advances past them, so later columns keep decoding. A
//! column index past the end of `Header.columns()`, or a `ColumnType` this decoder doesn't
//! recognize at all (`>= 15`), **stops** the properties loop for that feature — that value's
//! true byte width isn't known to this decoder, so any further bytes could be misread; the
//! properties decoded so far are kept (fail-soft, never a panic or a silently-wrong read).

use std::io;

use flatbuffers::{
    ForwardsUOffset, SkipSizePrefix, Table, Vector, Verifiable, Verifier, VerifierOptions,
};

use crate::vector::feature::{Feature, Geometry, Props, Value};

use super::header::Header;

/// Run `flatbuffers::Verifier` over a Feature FlatBuffer, treating it as a `size_prefixed`
/// buffer per FlatGeoBuf's `u32 feat_size · Feature FlatBuffer` container section, with the
/// `feat_size` prefix already stripped by the caller (`bytes` starts at the root uoffset,
/// matching `decode_feature`'s existing contract). See `header.rs`'s `verify_size_prefixed` for
/// why this reconstruction matters: `Verifier::is_aligned` checks alignment relative to the
/// TRUE flatbuffers position 0 the original writer padded against, which for a size-prefixed
/// buffer is 4 bytes before where `bytes` starts here -- verifying at position 0 directly
/// shifts every alignment check by 4 bytes and spuriously rejects real `ogr2ogr` files
/// (confirmed empirically the same way).
fn verify_size_prefixed<T: Verifiable>(buf: &[u8], opts: &VerifierOptions) -> bool {
    let mut framed = Vec::with_capacity(4 + buf.len());
    framed.extend_from_slice(&[0u8; 4]);
    framed.extend_from_slice(buf);
    let mut verifier = Verifier::new(opts, &framed);
    <SkipSizePrefix<ForwardsUOffset<T>>>::run_verifier(&mut verifier, 0).is_ok()
}

/// `Geometry.fbs`, restricted to the fields this reader ever touches: `ends`=4 (`[uint]`),
/// `xy`=6 (`[double]`), `type`=16 (`u8`), `parts`=18 (`[Geometry]`, recursive). `z`/`m`/`t`/`tm`
/// (voffsets 8/10/12/14) are never read by this decoder, so deliberately not visited -- the
/// verifier only needs to prove safe what we actually access. See the module doc's Task 7 note.
pub(super) struct GeometryV;
impl Verifiable for GeometryV {
    fn run_verifier(
        v: &mut Verifier,
        pos: usize,
    ) -> std::result::Result<(), flatbuffers::InvalidFlatbuffer> {
        v.visit_table(pos)?
            .visit_field::<ForwardsUOffset<Vector<'static, u32>>>("ends", 4, false)?
            .visit_field::<ForwardsUOffset<Vector<'static, f64>>>("xy", 6, false)?
            .visit_field::<u8>("type", 16, false)?
            .visit_field::<ForwardsUOffset<Vector<'static, ForwardsUOffset<GeometryV>>>>(
                "parts", 18, false,
            )?
            .finish();
        Ok(())
    }
}

/// A raw `[ubyte]` vector (FlatGeoBuf's `Feature.properties`) -- the `flatbuffers` crate ships a
/// `Verifiable` impl for `&str` (UTF-8 + null-terminator checked) but NOT for a raw `&[u8]`
/// byte vector, so this hand-rolls the equivalent bounds check minus the UTF-8/null-terminator
/// parts: read the `u32` element count at `pos` (itself bounds-checked by `get_uoffset`), then
/// prove the `count`-byte range right after it is in-buffer via the public
/// `Verifier::range_in_buffer` (`saturating_add`-based -- never wraps/panics on a huge crafted
/// count, just fails the range check and returns `Err`).
struct RawBytesV;
impl Verifiable for RawBytesV {
    fn run_verifier(
        v: &mut Verifier,
        pos: usize,
    ) -> std::result::Result<(), flatbuffers::InvalidFlatbuffer> {
        let len = v.get_uoffset(pos)? as usize;
        let start = pos.saturating_add(flatbuffers::SIZE_UOFFSET);
        v.range_in_buffer(start, len)?;
        Ok(())
    }
}

/// `Feature.fbs`, restricted to the fields this reader ever touches: `geometry`=4
/// (`ForwardsUOffset<GeometryV>`), `properties`=6 (`ForwardsUOffset<RawBytesV>`). `columns`=8
/// (a per-feature schema override) is never read by this decoder (see the module doc), so not
/// visited.
struct FeatureV;
impl Verifiable for FeatureV {
    fn run_verifier(
        v: &mut Verifier,
        pos: usize,
    ) -> std::result::Result<(), flatbuffers::InvalidFlatbuffer> {
        v.visit_table(pos)?
            .visit_field::<ForwardsUOffset<GeometryV>>("geometry", 4, false)?
            .visit_field::<ForwardsUOffset<RawBytesV>>("properties", 6, false)?
            .finish();
        Ok(())
    }
}

/// Run the full recursive `flatbuffers::Verifier` over a Feature FlatBuffer (`bytes` = the
/// per-record bytes with the `u32 feat_size` container prefix already stripped, same convention
/// as `decode_feature`). `true` iff every field this reader can possibly walk into -- geometry,
/// `xy`/`ends`/`type`, `parts` recursively, and `properties` -- resolves in-bounds, with
/// `VerifierOptions::default()`'s `max_depth` (64) bounding a crafted `parts` chain's recursion
/// depth well short of any stack-overflow risk (native recursion of 64 frames is trivial; no
/// real FlatGeoBuf geometry nests anywhere close). Shared by `decode_feature` (below) and
/// `mod.rs::feature_bbox` (the bruteforce-query path, whose own recursive `parts` walker,
/// `accumulate_geometry_bbox`, needs the exact same pre-validation to stay panic/UB-free and
/// depth-bounded).
pub(super) fn verify_feature_buf(bytes: &[u8]) -> bool {
    verify_size_prefixed::<FeatureV>(bytes, &VerifierOptions::default())
}

/// Decode one Feature FlatBuffer. `bytes` is the Feature record's bytes with the leading `u32
/// feat_size` container prefix already stripped by the caller (`FgbSource::read_feature_record`
/// in `mod.rs`) — the same convention `mod.rs::feature_bbox`'s `body` parameter uses, since a
/// FlatBuffer root offset is always relative to the start of its own buffer. `fid` is left `0`;
/// `FgbSource::decode_all`/`decode_at` assign the feature's byte offset as its stable `fid`
/// after this returns.
///
/// `Ok(None)`: the feature has no geometry field, or its geometry is a type this crate doesn't
/// model (`MultiPoint`/`GeometryCollection`/unrecognized) — the caller skips the feature, the
/// same contract as `wkb::decode_gpkg_geometry`. `Err`: the feature bytes don't even parse as a
/// well-formed FlatBuffer root table (truncated/corrupt) — never a panic.
pub(super) fn decode_feature(bytes: &[u8], header: &Header) -> io::Result<Option<Feature>> {
    let Some(root_loc) = super::validate_root_table(bytes) else {
        return Err(super::invalid("fgb: feature root table invalid"));
    };
    // Task 7: full recursive structural verification -- geometry, xy/ends/type, parts
    // (recursively, depth-bounded), and properties -- BEFORE any accessor below reads a nested
    // field. `validate_root_table` above only proves the root table's own vtable is safe to
    // read slot offsets from; every accessor past that point is still an unchecked
    // `flatbuffers::Table::get` (see the module doc's "Task 7 hardening" section), so this gate
    // is what actually makes them safe.
    if !verify_feature_buf(bytes) {
        return Err(super::invalid(
            "fgb: feature failed FlatBuffer structural verification",
        ));
    }
    // Safety: `validate_root_table` just proved the root offset + vtable resolve in-bounds —
    // the same precondition `header.rs`'s `Header::table()` relies on for the Header table —
    // and `verify_feature_buf` above just proved every field this reader touches (transitively,
    // through `parts`) resolves in-bounds too.
    let feature = unsafe { Table::new(bytes, root_loc) };

    let Some(geometry) = (unsafe { feature.get::<ForwardsUOffset<Table<'_>>>(4, None) }) else {
        return Ok(None); // no geometry field at all
    };
    let Some(geom) = decode_geometry(&geometry, header.geometry_type()) else {
        return Ok(None); // MultiPoint / GeometryCollection / unrecognized -- unmodeled
    };

    let props_blob = unsafe { feature.get::<ForwardsUOffset<&[u8]>>(6, None) }.unwrap_or(&[]);
    let props = decode_props(props_blob, &header.columns());

    Ok(Some(Feature::new(geom, props, 0)))
}

/// `Geometry.type`(6:16), falling back to the Header's own `geometry_type` when 0/absent (a
/// single-geometry-type layer, like `tiny.fgb`, omits the per-feature type).
fn decode_geometry(geom: &Table<'_>, header_geom_type: u8) -> Option<Geometry> {
    let ty = unsafe { geom.get::<u8>(16, Some(0)) }.unwrap_or(0);
    let ty = if ty == 0 { header_geom_type } else { ty };
    match ty {
        1 => read_xy(geom)?.into_iter().next().map(Geometry::Point),
        2 => Some(Geometry::LineString(read_xy(geom)?)),
        3 => {
            let xy = read_xy(geom)?;
            let ends = read_ends(geom);
            Some(Geometry::Polygon(split_by_ends(&xy, &ends)))
        }
        4 => None, // MultiPoint -- valid FlatGeoBuf, unmodeled (mirrors wkb.rs)
        5 => {
            let xy = read_xy(geom)?;
            let ends = read_ends(geom);
            Some(Geometry::MultiLineString(split_by_ends(&xy, &ends)))
        }
        6 => {
            let parts = read_parts(geom)?;
            let mut polys = Vec::with_capacity(parts.len());
            for part in &parts {
                let xy = read_xy(part)?;
                let ends = read_ends(part);
                polys.push(split_by_ends(&xy, &ends));
            }
            Some(Geometry::MultiPolygon(polys))
        }
        7 => None, // GeometryCollection -- valid FlatGeoBuf, unmodeled (mirrors wkb.rs)
        _ => None, // unrecognized GeometryType byte
    }
}

/// `Geometry.xy`(1:6) as `[x, y]` pairs. `None` if the field is absent. A trailing odd ordinate
/// (malformed) is simply dropped, never read out of bounds.
fn read_xy(geom: &Table<'_>) -> Option<Vec<[f64; 2]>> {
    let xy = unsafe { geom.get::<ForwardsUOffset<Vector<'_, f64>>>(6, None) }?;
    let n = xy.len() - (xy.len() % 2);
    let mut pts = Vec::with_capacity(n / 2);
    let mut i = 0;
    while i + 1 < n {
        pts.push([xy.get(i), xy.get(i + 1)]);
        i += 2;
    }
    Some(pts)
}

/// `Geometry.ends`(0:4). Empty (not `None`) when the field is absent — `split_by_ends` treats
/// "no ends" as "one ring/part covering the whole `xy`".
fn read_ends(geom: &Table<'_>) -> Vec<u32> {
    match unsafe { geom.get::<ForwardsUOffset<Vector<'_, u32>>>(4, None) } {
        Some(v) => (0..v.len()).map(|i| v.get(i)).collect(),
        None => Vec::new(),
    }
}

/// `Geometry.parts`(7:18) — only populated for `MultiPolygon`/`GeometryCollection`; each
/// element is itself a full `Geometry` table (its own `xy` + `ends`).
fn read_parts<'a>(geom: &Table<'a>) -> Option<Vec<Table<'a>>> {
    let parts =
        unsafe { geom.get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<Table<'_>>>>>(18, None) }?;
    Some((0..parts.len()).map(|i| parts.get(i)).collect())
}

/// Split a flat `xy` coordinate list into rings/parts at each `ends` cut (FlatGeoBuf packs a
/// `Polygon`'s rings, or a `MultiLineString`'s lines, this way — ring/line 0 first). No `ends`
/// at all means the whole `xy` is a single ring/part. A malformed `ends` entry (out of range,
/// or non-monotonic — `end < start`) stops the split there rather than panicking: `[T]::get` on
/// an invalid range returns `None`, it never indexes out of bounds.
fn split_by_ends(xy: &[[f64; 2]], ends: &[u32]) -> Vec<Vec<[f64; 2]>> {
    if ends.is_empty() {
        return vec![xy.to_vec()];
    }
    let mut out = Vec::with_capacity(ends.len());
    let mut start = 0usize;
    for &e in ends {
        let end = e as usize;
        match xy.get(start..end) {
            Some(slice) => out.push(slice.to_vec()),
            None => break,
        }
        start = end;
    }
    out
}

/// Decode `Feature.properties`(1:6) — a raw `[u8]` blob of repeated `(u16 column_index,
/// value)` pairs — into `Props`, keyed by `columns[column_index].0` (name). See the module doc
/// for the width table and the "stop, don't guess" policy on an unrecognized `ColumnType`.
fn decode_props(blob: &[u8], columns: &[(String, u8)]) -> Props {
    let mut props = Props::new();
    let mut pos = 0usize;
    while pos + 2 <= blob.len() {
        let col_idx = u16::from_le_bytes([blob[pos], blob[pos + 1]]) as usize;
        pos += 2;
        let Some((name, ty)) = columns.get(col_idx) else {
            break; // unknown column index -- its value's width is unknowable, stop safely
        };
        match *ty {
            0 => {
                // Byte (i8)
                let Some(&b) = blob.get(pos) else { break };
                pos += 1;
                props.insert(name.clone(), Value::Num(b as i8 as f64));
            }
            1 | 2 => {
                // UByte / Bool
                let Some(&b) = blob.get(pos) else { break };
                pos += 1;
                props.insert(name.clone(), Value::Num(b as f64));
            }
            3 => {
                // Short (i16)
                let Some(b) = blob.get(pos..pos + 2) else {
                    break;
                };
                pos += 2;
                props.insert(
                    name.clone(),
                    Value::Num(i16::from_le_bytes(b.try_into().unwrap()) as f64),
                );
            }
            4 => {
                // UShort (u16)
                let Some(b) = blob.get(pos..pos + 2) else {
                    break;
                };
                pos += 2;
                props.insert(
                    name.clone(),
                    Value::Num(u16::from_le_bytes(b.try_into().unwrap()) as f64),
                );
            }
            5 => {
                // Int (i32)
                let Some(b) = blob.get(pos..pos + 4) else {
                    break;
                };
                pos += 4;
                props.insert(
                    name.clone(),
                    Value::Num(i32::from_le_bytes(b.try_into().unwrap()) as f64),
                );
            }
            6 => {
                // UInt (u32)
                let Some(b) = blob.get(pos..pos + 4) else {
                    break;
                };
                pos += 4;
                props.insert(
                    name.clone(),
                    Value::Num(u32::from_le_bytes(b.try_into().unwrap()) as f64),
                );
            }
            7 => {
                // Long (i64)
                let Some(b) = blob.get(pos..pos + 8) else {
                    break;
                };
                pos += 8;
                props.insert(
                    name.clone(),
                    Value::Num(i64::from_le_bytes(b.try_into().unwrap()) as f64),
                );
            }
            8 => {
                // ULong (u64)
                let Some(b) = blob.get(pos..pos + 8) else {
                    break;
                };
                pos += 8;
                props.insert(
                    name.clone(),
                    Value::Num(u64::from_le_bytes(b.try_into().unwrap()) as f64),
                );
            }
            9 => {
                // Float (f32)
                let Some(b) = blob.get(pos..pos + 4) else {
                    break;
                };
                pos += 4;
                props.insert(
                    name.clone(),
                    Value::Num(f32::from_le_bytes(b.try_into().unwrap()) as f64),
                );
            }
            10 => {
                // Double (f64)
                let Some(b) = blob.get(pos..pos + 8) else {
                    break;
                };
                pos += 8;
                props.insert(
                    name.clone(),
                    Value::Num(f64::from_le_bytes(b.try_into().unwrap())),
                );
            }
            11 | 12 | 13 => {
                // String / Json / DateTime: u32 len + UTF-8 bytes. Json is JSON text and
                // DateTime is an ISO-8601 text string -- `Props`/`Value` has only `Num`/`Str`,
                // so both surface as `Value::Str`, same as String.
                let Some(len_b) = blob.get(pos..pos + 4) else {
                    break;
                };
                let len = u32::from_le_bytes(len_b.try_into().unwrap()) as usize;
                pos += 4;
                let Some(end) = pos.checked_add(len) else {
                    break;
                };
                let Some(s) = blob.get(pos..end) else {
                    break;
                };
                pos = end;
                props.insert(
                    name.clone(),
                    Value::Str(String::from_utf8_lossy(s).into_owned()),
                );
            }
            14 => {
                // Binary: u32 len + raw bytes. `Props`/`Value` cannot represent raw bytes, so
                // nothing is inserted -- but the cursor still advances past it (same bounds
                // checks as String/Json/DateTime) so later columns keep decoding.
                let Some(len_b) = blob.get(pos..pos + 4) else {
                    break;
                };
                let len = u32::from_le_bytes(len_b.try_into().unwrap()) as usize;
                pos += 4;
                let Some(end) = pos.checked_add(len) else {
                    break;
                };
                if blob.get(pos..end).is_none() {
                    break;
                }
                pos = end;
            }
            _ => break, // ColumnType >= 15: genuinely unknown width -- stop safely
        }
    }
    props
}

#[cfg(test)]
mod tests {
    use super::{decode_feature, decode_props};
    use crate::vector::feature::Geometry;

    /// FIX A: `ColumnType` 12 (Json)/13 (DateTime) are length-prefixed exactly like 11
    /// (String) -- `u32 len + UTF-8 bytes` -- so a column of either type must decode to
    /// `Value::Str` (same as String), not stop the whole properties loop. Synthetic blob:
    /// column 0 = String "Rua X", column 1 = DateTime "2024-01-02", column 2 = Int 42. Before
    /// the fix, hitting the DateTime column (`_ => break`) drops it AND the trailing Int.
    #[test]
    fn decode_props_survives_json_datetime_binary_column_types() {
        let columns = vec![
            ("name".to_string(), 11u8),    // String
            ("updated".to_string(), 13u8), // DateTime
            ("pop".to_string(), 5u8),      // Int
        ];
        let mut blob = Vec::new();
        blob.extend_from_slice(&0u16.to_le_bytes()); // col 0: name
        blob.extend_from_slice(&5u32.to_le_bytes());
        blob.extend_from_slice(b"Rua X");
        blob.extend_from_slice(&1u16.to_le_bytes()); // col 1: updated
        blob.extend_from_slice(&10u32.to_le_bytes());
        blob.extend_from_slice(b"2024-01-02");
        blob.extend_from_slice(&2u16.to_le_bytes()); // col 2: pop
        blob.extend_from_slice(&42i32.to_le_bytes());

        let props = decode_props(&blob, &columns);
        assert_eq!(props.get_str("name"), Some("Rua X"));
        assert_eq!(
            props.get_str("updated"),
            Some("2024-01-02"),
            "DateTime column must decode to Value::Str, not stop the loop"
        );
        assert_eq!(props.get_f64("pop"), Some(42.0));
    }

    /// FIX A continued: a Binary(14) column between two Int(5) columns must not stop the
    /// loop either -- its length-prefixed bytes are skipped (no `Value` can hold raw bytes)
    /// but the cursor still advances past it, so the trailing Int survives.
    #[test]
    fn decode_props_skips_binary_column_but_keeps_cursor_aligned() {
        let columns = vec![
            ("a".to_string(), 5u8),    // Int
            ("bin".to_string(), 14u8), // Binary
            ("b".to_string(), 5u8),    // Int
        ];
        let mut blob = Vec::new();
        blob.extend_from_slice(&0u16.to_le_bytes()); // col 0: a
        blob.extend_from_slice(&7i32.to_le_bytes());
        blob.extend_from_slice(&1u16.to_le_bytes()); // col 1: bin
        blob.extend_from_slice(&3u32.to_le_bytes());
        blob.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        blob.extend_from_slice(&2u16.to_le_bytes()); // col 2: b
        blob.extend_from_slice(&9i32.to_le_bytes());

        let props = decode_props(&blob, &columns);
        assert_eq!(props.get_f64("a"), Some(7.0));
        assert!(
            props.get("bin").is_none(),
            "Binary has no Value representation -- skipped, not inserted"
        );
        assert_eq!(
            props.get_f64("b"),
            Some(9.0),
            "Int after a Binary column must still decode -- cursor stayed aligned"
        );
    }

    #[test]
    fn decode_tiny_features() {
        let src = crate::cog::LocalFileRangeSource::open("fixtures/fgb/tiny.fgb").unwrap();
        let fgb = super::super::FgbSource::open(src).unwrap();
        let feats = fgb.decode_all().unwrap(); // Vec<Feature> in on-disk order
        assert_eq!(feats.len(), 3);

        // `decode_all` is a sequential scan of the features SECTION, i.e. on-disk (Hilbert-
        // sorted) order — NOT GeoJSON source-insertion order. `rtree.rs`'s already-committed,
        // hand-verified tests establish the exact physical layout of this fixture:
        // relative offset 0 = point b(5,6), 96 = point a(1,2), 192 = polygon c
        // (`rtree_query_matches_bruteforce_on_tiny`: "point b's feature is at relative offset
        // 0", "point a's feature starts at relative offset 96, polygon c's at 192"). So
        // feats[0]=b, feats[1]=a, feats[2]=c here, matching that order.
        match &feats[0].geom {
            Geometry::Point(p) => {
                assert!((p[0] - 5.0).abs() < 1e-9 && (p[1] - 6.0).abs() < 1e-9);
            }
            g => panic!("{g:?}"),
        }
        assert_eq!(feats[0].props.get_str("name"), Some("b"));
        assert_eq!(feats[0].props.get_f64("pop"), Some(20.0));
        assert_eq!(feats[0].fid, 0);

        // point a at (1,2), prop name="a", pop=10
        match &feats[1].geom {
            Geometry::Point(p) => {
                assert!((p[0] - 1.0).abs() < 1e-9 && (p[1] - 2.0).abs() < 1e-9);
            }
            g => panic!("{g:?}"),
        }
        assert_eq!(feats[1].props.get_str("name"), Some("a"));
        assert_eq!(feats[1].props.get_f64("pop"), Some(10.0));
        assert_eq!(feats[1].fid, 96);

        // polygon c: exterior ring of 5 points
        match &feats[2].geom {
            Geometry::Polygon(rings) => {
                assert_eq!(rings[0].len(), 5);
            }
            g => panic!("{g:?}"),
        }
        assert_eq!(feats[2].props.get_str("name"), Some("c"));
        assert_eq!(feats[2].props.get_f64("pop"), Some(30.0));
        assert_eq!(feats[2].fid, 192);
    }

    #[test]
    fn decode_at_matches_decode_all_by_offset() {
        // Task 5's windowed path decodes features one `rtree_query` byte offset at a time
        // (`decode_at`), rather than a sequential scan (`decode_all`) — the two entry points
        // must decode the SAME feature for the SAME offset.
        let src = crate::cog::LocalFileRangeSource::open("fixtures/fgb/tiny.fgb").unwrap();
        let fgb = super::super::FgbSource::open(src).unwrap();
        let all = fgb.decode_all().unwrap();
        for feat in &all {
            let one = fgb
                .decode_at(feat.fid)
                .unwrap()
                .expect("feature at its own fid offset");
            assert_eq!(one.fid, feat.fid);
            assert_eq!(format!("{:?}", one.geom), format!("{:?}", feat.geom));
            assert_eq!(one.props.get_str("name"), feat.props.get_str("name"));
        }
    }

    /// Task 6/7 fixture: a synthetic polygon-WITH-A-HOLE `.fgb`, minted once via `ogr2ogr -f
    /// FlatGeobuf` from `fixtures/fgb/hole.geojson` (a 10x10 square exterior ring with a 4x4
    /// square hole cut out at its center) -- `tiny.fgb`'s only Polygon (feature "c") is a
    /// single-ring square, so `split_by_ends`'s multi-cut loop (more than one `ends` entry) was
    /// never actually exercised by any committed fixture before this. This is that coverage.
    #[test]
    fn decode_polygon_with_hole_exercises_multicut() {
        let src = crate::cog::LocalFileRangeSource::open("fixtures/fgb/hole.fgb").unwrap();
        let fgb = super::super::FgbSource::open(src).unwrap();
        let feats = fgb.decode_all().unwrap();
        assert_eq!(feats.len(), 1);
        assert_eq!(feats[0].props.get_str("name"), Some("donut"));
        assert_eq!(feats[0].props.get_f64("pop"), Some(42.0));
        match &feats[0].geom {
            Geometry::Polygon(rings) => {
                assert_eq!(
                    rings.len(),
                    2,
                    "expected exterior + 1 hole, got {} rings",
                    rings.len()
                );
                // Exterior: closed 10x10 square (5 points, first == last).
                assert_eq!(rings[0].len(), 5, "exterior ring point count");
                assert_eq!(rings[0][0], rings[0][4], "exterior ring must be closed");
                let ext_xs: Vec<f64> = rings[0].iter().map(|p| p[0]).collect();
                assert!(ext_xs.iter().all(|&x| (0.0..=10.0).contains(&x)));
                // Hole: closed 4x4 square (5 points, first == last), strictly inside the exterior.
                assert_eq!(rings[1].len(), 5, "hole ring point count");
                assert_eq!(rings[1][0], rings[1][4], "hole ring must be closed");
                for p in &rings[1] {
                    assert!(
                        p[0] > 0.0 && p[0] < 10.0 && p[1] > 0.0 && p[1] < 10.0,
                        "hole point {p:?} must lie strictly inside the exterior ring"
                    );
                }
            }
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    /// `decode_feature` must never panic on truncated or byte-flipped Feature FlatBuffer bytes
    /// -- only ever a clean `Err` (or, for a structurally-valid-but-unmodeled geometry, `Ok
    /// (None)`). Exercised against the real "c" (Polygon, `ends`+`xy` both populated) feature
    /// body from `tiny.fgb`, which has the richest field set of the fixture's 3 features.
    #[test]
    fn decode_feature_never_panics_on_truncated_or_corrupt_bytes() {
        let (header, body) = tiny_header_and_polygon_feature_body();
        assert!(
            decode_feature(&body, &header).is_ok(),
            "sanity: the real, unmodified feature body must decode cleanly"
        );

        for cut in 0..body.len() {
            // Must not panic. Both Ok and Err are acceptable outcomes of a truncation.
            let _ = decode_feature(&body[..cut], &header);
        }
        for i in 0..body.len() {
            let mut flipped = body.clone();
            flipped[i] ^= 0xFF;
            // Must not panic. A bit flip can land on inert padding (still Ok) or on a
            // length/offset field (Err) -- either is fine, a panic or OOB read is not.
            let _ = decode_feature(&flipped, &header);
        }
    }

    /// Same contract, applied to `Header::parse`'s truncated/corrupt paths from the *feature*
    /// decode's perspective too: a `decode_feature` call is always paired with an already-parsed
    /// `Header`, but the `Header::parse` malformed-input contract itself is asserted directly in
    /// `header.rs`'s own test module (`parse_never_panics_on_truncated_or_corrupt_bytes`).
    /// This test instead proves the two malformed-input gates compose: a feature body decoded
    /// against a header whose `columns()` is empty (e.g. constructed from a minimal/degenerate
    /// header) must still never panic, only ever decode with an empty `Props`.
    #[test]
    fn decode_feature_never_panics_against_a_minimal_header() {
        let (_header, body) = tiny_header_and_polygon_feature_body();
        // A header FlatBuffer that carries no columns/crs/envelope at all -- still a
        // structurally valid, `Header::parse`-accepted buffer (every field this reader reads is
        // optional), just a degenerate one.
        let minimal = minimal_header_bytes();
        let header = super::Header::parse(minimal).expect("minimal header must still parse");
        assert_eq!(header.columns().len(), 0);
        // Must not panic; the geometry still decodes (it doesn't depend on columns), properties
        // decode against an empty column list (every column index is now "unknown" -> the
        // properties loop stops immediately, `Props` ends up empty).
        let decoded = decode_feature(&body, &header).expect("still decodes structurally");
        if let Some(f) = decoded {
            assert_eq!(
                f.props.iter().count(),
                0,
                "no columns known -> no properties decoded"
            );
        }
    }

    /// A crafted `Geometry.parts` chain nested deeper than `VerifierOptions::default().max_depth`
    /// (64) must be rejected by `verify_feature_buf` (and therefore `decode_feature`) with a
    /// clean `Err`, never a stack overflow -- the concrete proof behind the module doc's "Task 7
    /// hardening" claim. A shallow chain (well within the depth bound, and the shape any real
    /// FlatGeoBuf MultiPolygon/GeometryCollection actually uses) must still verify fine.
    #[test]
    fn decode_feature_rejects_parts_nested_past_the_depth_bound() {
        let (header, _) = tiny_header_and_polygon_feature_body();

        let shallow = build_nested_parts_feature(10);
        assert!(
            super::verify_feature_buf(&shallow),
            "10 levels of Geometry.parts nesting is well within the depth bound"
        );
        assert!(decode_feature(&shallow, &header).is_ok());

        let too_deep = build_nested_parts_feature(200);
        assert!(
            !super::verify_feature_buf(&too_deep),
            "200 levels of Geometry.parts nesting must be rejected by the depth bound"
        );
        assert!(
            decode_feature(&too_deep, &header).is_err(),
            "decode_feature must surface the depth-bound rejection as a clean Err, not panic"
        );
    }

    /// Read `tiny.fgb`'s Header plus the raw bytes of feature "c" (the Polygon, at relative
    /// offset 192 -- see `decode_tiny_features`'s doc comment for how that layout was
    /// established), the same size-prefix-then-body shape `mod.rs::read_feature_record` reads.
    fn tiny_header_and_polygon_feature_body() -> (super::Header, Vec<u8>) {
        let src = crate::cog::LocalFileRangeSource::open("fixtures/fgb/tiny.fgb").unwrap();
        let prefix = crate::cog::RangeSource::read_range(&src, 0, 12).unwrap();
        let header_size = u32::from_le_bytes(prefix[8..12].try_into().unwrap()) as u64;
        let header_bytes =
            crate::cog::RangeSource::read_range(&src, 12, header_size as usize).unwrap();
        let header = super::Header::parse(header_bytes).unwrap();

        // features_start=808, feature "c" at relative offset 192 (both hand-verified and
        // asserted elsewhere: `mod.rs`'s `open_tiny_reports_crs_and_layout` and this file's
        // `decode_tiny_features`).
        const FEATURES_START: u64 = 808;
        const POLYGON_REL_OFFSET: u64 = 192;
        let size_bytes =
            crate::cog::RangeSource::read_range(&src, FEATURES_START + POLYGON_REL_OFFSET, 4)
                .unwrap();
        let size = u32::from_le_bytes(size_bytes[0..4].try_into().unwrap()) as usize;
        let body = crate::cog::RangeSource::read_range(
            &src,
            FEATURES_START + POLYGON_REL_OFFSET + 4,
            size,
        )
        .unwrap();
        (header, body)
    }

    /// A minimal, structurally-valid Header FlatBuffer with every field absent (this reader
    /// treats every Header field as optional -- see `header.rs`'s `HeaderV`) -- just a root
    /// table with an empty vtable, built by hand (not via `ogr2ogr`) to exercise
    /// `Header::parse`/`decode_feature` against a degenerate-but-valid header. Built
    /// `size_prefixed` (`finish_size_prefixed`, not `finish_minimal`) and then the leading `u32`
    /// size prefix is stripped, exactly mirroring the real container convention `mod.rs::open`
    /// reads (`u32 header_size · Header FlatBuffer`) -- `Header::parse`'s `verify_size_prefixed`
    /// helper assumes that shape (see its doc comment on why alignment depends on it).
    fn minimal_header_bytes() -> Vec<u8> {
        let mut fbb = flatbuffers::FlatBufferBuilder::new();
        let start = fbb.start_table();
        let end = fbb.end_table(start);
        fbb.finish_size_prefixed(end, None);
        fbb.finished_data()[4..].to_vec()
    }

    /// Build a synthetic Feature FlatBuffer whose geometry is `depth` levels of `Geometry.parts`
    /// nesting (each level a bare `Geometry` table with a 1-element `parts` vector pointing at
    /// the next level down; the innermost level has no fields at all). Built bottom-up (as
    /// FlatBuffers requires) via `flatbuffers::FlatBufferBuilder` directly -- the same low-level
    /// API this crate's hand-written accessors read, just used to write instead. Only the
    /// nesting SHAPE matters here (proving the depth bound), not realistic geometry content.
    /// `size_prefixed` + stripped, same reasoning as `minimal_header_bytes` above (this one
    /// feeds `decode_feature`, whose `feat_size` prefix is stripped by `mod.rs` the same way).
    fn build_nested_parts_feature(depth: usize) -> Vec<u8> {
        let mut fbb = flatbuffers::FlatBufferBuilder::new();
        let mut inner: Option<flatbuffers::WIPOffset<flatbuffers::TableFinishedWIPOffset>> = None;
        for _ in 0..depth {
            let parts = inner.map(|w| fbb.create_vector(&[w]));
            let start = fbb.start_table();
            if let Some(p) = parts {
                fbb.push_slot_always(18u16, p); // Geometry.parts
            }
            inner = Some(fbb.end_table(start));
        }
        let feat_start = fbb.start_table();
        if let Some(g) = inner {
            fbb.push_slot_always(4u16, g); // Feature.geometry
        }
        let feat_end = fbb.end_table(feat_start);
        fbb.finish_size_prefixed(feat_end, None);
        fbb.finished_data()[4..].to_vec()
    }
}
