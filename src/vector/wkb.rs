// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Bespoke GeoPackageBinary + WKB geometry decoder.
//!
//! No `wkb` / `geo-types-from-wkb` crates — this is the clean-room heart of the native
//! GeoPackage reader, the same role `cog.rs`'s hand-rolled IFD parser plays for TIFF. A
//! GeoPackage `geom` BLOB column is a small header (the "GeoPackageBinary" wrapper, see
//! OGC GeoPackage §2.1.3) followed by standard ISO WKB. We parse both by hand over a
//! bounds-checked cursor — **never panic, never index out of bounds**: any truncation or
//! malformed byte becomes `Err`, not a crash.
//!
//! ## GeoPackageBinary header (8 bytes + envelope)
//!
//! ```text
//! byte 0-1   magic         b"GP"
//! byte 2     version       (ignored)
//! byte 3     flags         bit0       = header int byte order (0=big,1=little) — unused here,
//!                                       since we only ever *skip* srs_id/envelope, never decode
//!                                       their values
//!                          bits1-3    = envelope indicator (0/1/2/3/4) → envelope byte length
//!                                       0, 32, 48, 48, 64 (2/3 both mean 48: XYZ or XYM bounds)
//!                          bit4       = empty geometry flag
//! byte 4-7   srs_id        int32 (skipped — SRS is resolved elsewhere, from `gpkg_geometry_columns`)
//! byte 8..   envelope      `envelope_len` bytes, skipped (min/max bounds, not needed to decode)
//! ..end      wkb           standard ISO WKB, byte order **per geometry** (see below)
//! ```
//!
//! ## WKB body
//!
//! Every geometry — including each element of a `Multi*` collection — starts with its own
//! byte-order byte (0=big-endian, 1=little-endian) followed by a `u32` geometry type. This
//! is a classic bug spot: a `MultiLineString`/`MultiPolygon` can mix byte orders across its
//! elements (each element is a **complete, self-describing** sub-WKB), so the byte order must
//! be re-read per sub-geometry, never inherited from the parent. `read_geom` below is called
//! recursively for exactly this reason.
//!
//! The geometry type is `base + 1000*zm` where `base` is the OGC type (1 Point, 2 LineString,
//! 3 Polygon, 4 MultiPoint, 5 MultiLineString, 6 MultiPolygon, 7 GeometryCollection) and `zm`
//! is 0 (XY), 1 (XYZ), 2 (XYM), or 3 (XYZM). We read `dims = 2 + has_z + has_m` ordinates per
//! point and keep only `[x, y]` — Z/M are parsed (to stay aligned in the byte stream) and
//! dropped, matching `feature::Geometry`'s 2D-only shape.
//!
//! `MultiPoint` and `GeometryCollection` are valid WKB we simply don't model yet (no
//! `Geometry::MultiPoint`/`GeometryCollection` variant) — those decode to `Ok(None)`, same as
//! an empty geometry, rather than `Err`.

use crate::vector::feature::Geometry;

/// Decode a GeoPackage `geom` BLOB column into the internal `Geometry` model.
///
/// `Ok(None)` covers two distinct-but-equally-"nothing to draw" cases: the GeoPackageBinary
/// empty-geometry flag is set, or the WKB is a well-formed type we don't model
/// (`MultiPoint`/`GeometryCollection`). `Err` is reserved for genuinely malformed input:
/// truncation, a bad magic, an unrecognized byte-order byte, an invalid envelope indicator,
/// or an unknown geometry type.
pub fn decode_gpkg_geometry(blob: &[u8]) -> Result<Option<Geometry>, String> {
    if blob.len() < 8 || &blob[0..2] != b"GP" {
        return Err("wkb: invalid GeoPackageBinary header (bad magic or truncated)".to_string());
    }
    let flags = blob[3];
    let empty = (flags >> 4) & 1 == 1;
    let env_indicator = (flags >> 1) & 0x07;
    let env_len: usize = match env_indicator {
        0 => 0,
        1 => 32,
        2 | 3 => 48,
        4 => 64,
        other => return Err(format!("wkb: invalid envelope indicator {other}")),
    };
    if empty {
        return Ok(None);
    }
    let wkb_start = 8 + env_len;
    let wkb = blob
        .get(wkb_start..)
        .ok_or_else(|| "wkb: truncated (envelope overruns blob)".to_string())?;
    if wkb.is_empty() {
        return Err("wkb: truncated (no WKB body after header)".to_string());
    }
    let mut r = Rdr { b: wkb, pos: 0 };
    read_geom(&mut r)
}

/// A bounds-checked byte cursor over a WKB slice. Every read validates the range against
/// `b.len()` first and returns `Err` on overrun — no `panic!`, no unchecked indexing.
struct Rdr<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Rdr<'a> {
    fn u8(&mut self) -> Result<u8, String> {
        let v = *self
            .b
            .get(self.pos)
            .ok_or_else(|| "wkb: truncated (byte order)".to_string())?;
        self.pos += 1;
        Ok(v)
    }

    fn u32(&mut self, little: bool) -> Result<u32, String> {
        let bytes = self.take(4)?;
        let arr: [u8; 4] = bytes.try_into().expect("take(4) yields exactly 4 bytes");
        Ok(if little {
            u32::from_le_bytes(arr)
        } else {
            u32::from_be_bytes(arr)
        })
    }

    fn f64(&mut self, little: bool) -> Result<f64, String> {
        let bytes = self.take(8)?;
        let arr: [u8; 8] = bytes.try_into().expect("take(8) yields exactly 8 bytes");
        Ok(if little {
            f64::from_le_bytes(arr)
        } else {
            f64::from_be_bytes(arr)
        })
    }

    /// The next `n` bytes, advancing the cursor. `Err` (not a panic) if fewer than `n` remain.
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| "wkb: truncated (length overflow)".to_string())?;
        let bytes = self
            .b
            .get(self.pos..end)
            .ok_or_else(|| "wkb: truncated (unexpected end of WKB)".to_string())?;
        self.pos = end;
        Ok(bytes)
    }
}

/// Read one `[x, y]` point, consuming `dims` ordinates total (Z/M, if present, are read to
/// stay aligned in the byte stream and then dropped).
fn read_point(r: &mut Rdr, little: bool, dims: usize) -> Result<[f64; 2], String> {
    let x = r.f64(little)?;
    let y = r.f64(little)?;
    for _ in 2..dims {
        r.f64(little)?; // Z and/or M — parsed for alignment, not kept
    }
    Ok([x, y])
}

/// Read one complete, self-describing WKB geometry: its own byte-order byte, its own type,
/// then its body. Called recursively for `Multi*` elements so each sub-geometry's byte order
/// is re-read rather than inherited from the parent — a mixed-endian `MultiLineString` /
/// `MultiPolygon` is valid WKB and must round-trip correctly.
fn read_geom(r: &mut Rdr) -> Result<Option<Geometry>, String> {
    let bo = r.u8()?;
    let little = match bo {
        0 => false,
        1 => true,
        other => return Err(format!("wkb: unknown byte order byte {other}")),
    };
    let ty = r.u32(little)?;
    let base = ty % 1000;
    let zm = ty / 1000;
    let dims = 2 + usize::from(zm == 1 || zm == 3) + usize::from(zm >= 2);

    match base {
        1 => {
            let pt = read_point(r, little, dims)?;
            Ok(Some(Geometry::Point(pt)))
        }
        2 => {
            let n = r.u32(little)?;
            let mut pts = Vec::new();
            for _ in 0..n {
                pts.push(read_point(r, little, dims)?);
            }
            Ok(Some(Geometry::LineString(pts)))
        }
        3 => {
            let nrings = r.u32(little)?;
            let mut rings = Vec::new();
            for _ in 0..nrings {
                let npts = r.u32(little)?;
                let mut ring = Vec::new();
                for _ in 0..npts {
                    ring.push(read_point(r, little, dims)?);
                }
                rings.push(ring);
            }
            Ok(Some(Geometry::Polygon(rings)))
        }
        4 => Ok(None), // MultiPoint — valid WKB, unmodeled
        5 => {
            let n = r.u32(little)?;
            let mut lines = Vec::new();
            for _ in 0..n {
                match read_geom(r)? {
                    Some(Geometry::LineString(pts)) => lines.push(pts),
                    Some(_) => {
                        return Err("wkb: MultiLineString element is not a LineString".to_string())
                    }
                    None => {
                        return Err("wkb: MultiLineString element is empty or unmodeled".to_string())
                    }
                }
            }
            Ok(Some(Geometry::MultiLineString(lines)))
        }
        6 => {
            let n = r.u32(little)?;
            let mut polys = Vec::new();
            for _ in 0..n {
                match read_geom(r)? {
                    Some(Geometry::Polygon(rings)) => polys.push(rings),
                    Some(_) => return Err("wkb: MultiPolygon element is not a Polygon".to_string()),
                    None => {
                        return Err("wkb: MultiPolygon element is empty or unmodeled".to_string())
                    }
                }
            }
            Ok(Some(Geometry::MultiPolygon(polys)))
        }
        7 => Ok(None), // GeometryCollection — valid WKB, unmodeled
        other => Err(format!("wkb: unknown geometry type {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- hand-built blob helpers -------------------------------------------------------

    fn push_u32(buf: &mut Vec<u8>, little: bool, v: u32) {
        buf.extend_from_slice(&if little {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        });
    }

    fn push_f64(buf: &mut Vec<u8>, little: bool, v: f64) {
        buf.extend_from_slice(&if little {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        });
    }

    /// The GeoPackageBinary header: magic + version + flags (envelope=0, given empty bit) +
    /// a dummy srs_id, followed by `wkb_body`.
    fn gpkg_blob(empty: bool, wkb_body: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"GP");
        b.push(0); // version
        let flags: u8 = (1/* header int LE, arbitrary */) | ((empty as u8) << 4);
        b.push(flags);
        b.extend_from_slice(&[0, 0, 0, 0]); // srs_id, value irrelevant (skipped)
        b.extend_from_slice(wkb_body);
        b
    }

    fn wkb_geom_header(buf: &mut Vec<u8>, little: bool, ty: u32) {
        buf.push(if little { 1 } else { 0 });
        push_u32(buf, little, ty);
    }

    fn wkb_point_body(little: bool, ty: u32, coords: &[f64]) -> Vec<u8> {
        let mut b = Vec::new();
        wkb_geom_header(&mut b, little, ty);
        for c in coords {
            push_f64(&mut b, little, *c);
        }
        b
    }

    fn wkb_linestring_body(little: bool, pts: &[[f64; 2]]) -> Vec<u8> {
        let mut b = Vec::new();
        wkb_geom_header(&mut b, little, 2);
        push_u32(&mut b, little, pts.len() as u32);
        for p in pts {
            push_f64(&mut b, little, p[0]);
            push_f64(&mut b, little, p[1]);
        }
        b
    }

    fn wkb_polygon_body(little: bool, rings: &[Vec<[f64; 2]>]) -> Vec<u8> {
        let mut b = Vec::new();
        wkb_geom_header(&mut b, little, 3);
        push_u32(&mut b, little, rings.len() as u32);
        for ring in rings {
            push_u32(&mut b, little, ring.len() as u32);
            for p in ring {
                push_f64(&mut b, little, p[0]);
                push_f64(&mut b, little, p[1]);
            }
        }
        b
    }

    /// `lines`: `(byte_order_is_little, points)` per element — lets a caller build a
    /// mixed-endian MultiLineString.
    fn wkb_multilinestring_body(outer_little: bool, lines: &[(bool, Vec<[f64; 2]>)]) -> Vec<u8> {
        let mut b = Vec::new();
        wkb_geom_header(&mut b, outer_little, 5);
        push_u32(&mut b, outer_little, lines.len() as u32);
        for (little, pts) in lines {
            b.extend_from_slice(&wkb_linestring_body(*little, pts));
        }
        b
    }

    /// `polys`: `(byte_order_is_little, rings)` per element — lets a caller build a
    /// mixed-endian MultiPolygon.
    fn wkb_multipolygon_body(outer_little: bool, polys: &[(bool, Vec<Vec<[f64; 2]>>)]) -> Vec<u8> {
        let mut b = Vec::new();
        wkb_geom_header(&mut b, outer_little, 6);
        push_u32(&mut b, outer_little, polys.len() as u32);
        for (little, rings) in polys {
            b.extend_from_slice(&wkb_polygon_body(*little, rings));
        }
        b
    }

    // -- Point ---------------------------------------------------------------------------

    #[test]
    fn point_little_endian() {
        let body = wkb_point_body(true, 1, &[30.0, 10.0]);
        let blob = gpkg_blob(false, &body);
        match decode_gpkg_geometry(&blob) {
            Ok(Some(Geometry::Point(p))) => assert_eq!(p, [30.0, 10.0]),
            other => panic!("expected Point([30.0, 10.0]), got {other:?}"),
        }
    }

    #[test]
    fn point_big_endian() {
        let body = wkb_point_body(false, 1, &[30.0, 10.0]);
        let blob = gpkg_blob(false, &body);
        match decode_gpkg_geometry(&blob) {
            Ok(Some(Geometry::Point(p))) => assert_eq!(p, [30.0, 10.0]),
            other => panic!("expected Point([30.0, 10.0]), got {other:?}"),
        }
    }

    // -- LineString ------------------------------------------------------------------------

    #[test]
    fn linestring_little_endian() {
        let pts = vec![[0.0, 0.0], [10.0, 10.0], [20.0, 5.0]];
        let body = wkb_linestring_body(true, &pts);
        let blob = gpkg_blob(false, &body);
        match decode_gpkg_geometry(&blob) {
            Ok(Some(Geometry::LineString(p))) => assert_eq!(p, pts),
            other => panic!("expected LineString, got {other:?}"),
        }
    }

    #[test]
    fn linestring_big_endian() {
        let pts = vec![[0.0, 0.0], [10.0, 10.0], [20.0, 5.0]];
        let body = wkb_linestring_body(false, &pts);
        let blob = gpkg_blob(false, &body);
        match decode_gpkg_geometry(&blob) {
            Ok(Some(Geometry::LineString(p))) => assert_eq!(p, pts),
            other => panic!("expected LineString, got {other:?}"),
        }
    }

    // -- Polygon with a hole -----------------------------------------------------------

    fn square_rings() -> Vec<Vec<[f64; 2]>> {
        vec![
            // exterior
            vec![
                [0.0, 0.0],
                [10.0, 0.0],
                [10.0, 10.0],
                [0.0, 10.0],
                [0.0, 0.0],
            ],
            // hole
            vec![[2.0, 2.0], [4.0, 2.0], [4.0, 4.0], [2.0, 4.0], [2.0, 2.0]],
        ]
    }

    #[test]
    fn polygon_with_hole_little_endian() {
        let rings = square_rings();
        let body = wkb_polygon_body(true, &rings);
        let blob = gpkg_blob(false, &body);
        match decode_gpkg_geometry(&blob) {
            Ok(Some(Geometry::Polygon(r))) => {
                assert_eq!(r.len(), 2);
                assert_eq!(r, rings);
            }
            other => panic!("expected Polygon with 2 rings, got {other:?}"),
        }
    }

    #[test]
    fn polygon_with_hole_big_endian() {
        let rings = square_rings();
        let body = wkb_polygon_body(false, &rings);
        let blob = gpkg_blob(false, &body);
        match decode_gpkg_geometry(&blob) {
            Ok(Some(Geometry::Polygon(r))) => {
                assert_eq!(r.len(), 2);
                assert_eq!(r, rings);
            }
            other => panic!("expected Polygon with 2 rings, got {other:?}"),
        }
    }

    // -- Multi*, mixed endian across sub-geometries -------------------------------------

    #[test]
    fn multilinestring_mixed_endian_sub_geometries() {
        let line_a = vec![[0.0, 0.0], [1.0, 1.0]];
        let line_b = vec![[5.0, 5.0], [6.0, 7.0], [8.0, 9.0]];
        // outer byte order LE, first element LE, second element BE.
        let body =
            wkb_multilinestring_body(true, &[(true, line_a.clone()), (false, line_b.clone())]);
        let blob = gpkg_blob(false, &body);
        match decode_gpkg_geometry(&blob) {
            Ok(Some(Geometry::MultiLineString(lines))) => {
                assert_eq!(lines, vec![line_a, line_b]);
            }
            other => panic!("expected MultiLineString, got {other:?}"),
        }
    }

    #[test]
    fn multipolygon_mixed_endian_sub_geometries() {
        let poly_a = square_rings();
        let poly_b = vec![vec![
            [100.0, 100.0],
            [110.0, 100.0],
            [110.0, 110.0],
            [100.0, 100.0],
        ]];
        // outer byte order BE, first element BE, second element LE — the reverse mix from
        // the MultiLineString test above, so both outer orders get exercised.
        let body = wkb_multipolygon_body(false, &[(false, poly_a.clone()), (true, poly_b.clone())]);
        let blob = gpkg_blob(false, &body);
        match decode_gpkg_geometry(&blob) {
            Ok(Some(Geometry::MultiPolygon(polys))) => {
                assert_eq!(polys, vec![poly_a, poly_b]);
            }
            other => panic!("expected MultiPolygon, got {other:?}"),
        }
    }

    // -- Z ordinate is parsed and dropped ------------------------------------------------

    #[test]
    fn z_flagged_point_drops_z() {
        let body = wkb_point_body(true, 1001, &[1.0, 2.0, 3.0]); // XYZ Point
        let blob = gpkg_blob(false, &body);
        match decode_gpkg_geometry(&blob) {
            Ok(Some(Geometry::Point(p))) => assert_eq!(p, [1.0, 2.0]),
            other => panic!("expected Point([1.0, 2.0]) with Z dropped, got {other:?}"),
        }
    }

    // -- empty geometry flag --------------------------------------------------------------

    #[test]
    fn empty_flag_yields_none() {
        let blob = gpkg_blob(true, &[]);
        assert!(matches!(decode_gpkg_geometry(&blob), Ok(None)));
    }

    // -- valid-but-unmodeled types → None, not Err ----------------------------------------

    #[test]
    fn multipoint_is_unmodeled_none() {
        let mut body = Vec::new();
        wkb_geom_header(&mut body, true, 4); // MultiPoint
        push_u32(&mut body, true, 0); // 0 elements — irrelevant, type alone decides
        let blob = gpkg_blob(false, &body);
        assert!(matches!(decode_gpkg_geometry(&blob), Ok(None)));
    }

    #[test]
    fn geometrycollection_is_unmodeled_none() {
        let mut body = Vec::new();
        wkb_geom_header(&mut body, true, 7); // GeometryCollection
        push_u32(&mut body, true, 0);
        let blob = gpkg_blob(false, &body);
        assert!(matches!(decode_gpkg_geometry(&blob), Ok(None)));
    }

    // -- envelope indicator: skip length must be right, not just its presence/absence ------

    /// A GeoPackageBinary header with a non-zero envelope indicator: `env_indicator` bytes of
    /// header, then `env_len` zero bytes standing in for the (unused, always skipped) envelope,
    /// then `wkb_body`. Exercises `wkb_start = 8 + env_len` for each indicator value.
    fn gpkg_blob_with_envelope(env_indicator: u8, env_len: usize, wkb_body: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"GP");
        b.push(0); // version
        let flags: u8 = 1 | (env_indicator << 1); // header int LE (arbitrary), not empty
        b.push(flags);
        b.extend_from_slice(&[0, 0, 0, 0]); // srs_id, value irrelevant (skipped)
        b.extend(std::iter::repeat(0u8).take(env_len)); // dummy envelope, skipped
        b.extend_from_slice(wkb_body);
        b
    }

    #[test]
    fn envelope_indicator_1_skips_32_bytes() {
        let body = wkb_point_body(true, 1, &[30.0, 10.0]);
        let blob = gpkg_blob_with_envelope(1, 32, &body);
        match decode_gpkg_geometry(&blob) {
            Ok(Some(Geometry::Point(p))) => assert_eq!(p, [30.0, 10.0]),
            other => panic!("expected Point([30.0, 10.0]), got {other:?}"),
        }
    }

    #[test]
    fn envelope_indicator_4_skips_64_bytes() {
        let body = wkb_point_body(false, 1, &[1.0, 2.0]);
        let blob = gpkg_blob_with_envelope(4, 64, &body);
        match decode_gpkg_geometry(&blob) {
            Ok(Some(Geometry::Point(p))) => assert_eq!(p, [1.0, 2.0]),
            other => panic!("expected Point([1.0, 2.0]), got {other:?}"),
        }
    }

    #[test]
    fn invalid_envelope_indicator_is_err() {
        let body = wkb_point_body(true, 1, &[30.0, 10.0]);
        // indicator 5, 6, 7 are all reserved/invalid.
        let blob = gpkg_blob_with_envelope(5, 0, &body);
        assert!(decode_gpkg_geometry(&blob).is_err());
    }

    // -- malformed input → Err, never a panic ---------------------------------------------

    #[test]
    fn truncated_blob_header_only_is_err() {
        let blob = gpkg_blob(false, &[]); // valid header, empty flag unset, but no WKB body
        assert!(decode_gpkg_geometry(&blob).is_err());
    }

    #[test]
    fn bad_magic_is_err() {
        let mut blob = gpkg_blob(false, &wkb_point_body(true, 1, &[1.0, 2.0]));
        blob[0] = b'X';
        blob[1] = b'Y';
        assert!(decode_gpkg_geometry(&blob).is_err());
    }

    #[test]
    fn truncated_wkb_body_is_err() {
        // LineString header claims 5 points but the blob only carries 1 — must not panic.
        let mut body = Vec::new();
        wkb_geom_header(&mut body, true, 2);
        push_u32(&mut body, true, 5);
        push_f64(&mut body, true, 1.0);
        push_f64(&mut body, true, 2.0);
        let blob = gpkg_blob(false, &body);
        assert!(decode_gpkg_geometry(&blob).is_err());
    }

    #[test]
    fn unknown_byte_order_is_err() {
        let mut body = Vec::new();
        body.push(2u8); // invalid: neither 0 nor 1
        push_u32(&mut body, true, 1);
        push_f64(&mut body, true, 1.0);
        push_f64(&mut body, true, 2.0);
        let blob = gpkg_blob(false, &body);
        assert!(decode_gpkg_geometry(&blob).is_err());
    }

    #[test]
    fn unknown_geometry_type_is_err() {
        let body = wkb_point_body(true, 999, &[1.0, 2.0]); // 999 is not a valid base type
        let blob = gpkg_blob(false, &body);
        assert!(decode_gpkg_geometry(&blob).is_err());
    }
}
