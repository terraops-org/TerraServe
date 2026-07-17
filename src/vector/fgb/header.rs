// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Hand-written FlatBuffers accessors for the FlatGeoBuf `Header` table (no `flatc`, no
//! generated code). The `flatbuffers` crate's low-level `Table`/`Follow` API does no bounds
//! validation of its own — its safe `root()` entry point requires a `Verifiable` impl per
//! table, which is exactly the codegen machinery we're avoiding by hand-writing this reader —
//! so `Header::parse` does its own validation up front: the root uoffset and the table's
//! vtable must resolve to in-bounds, well-formed locations before any field is read. A
//! malformed header is a clean `io::Error`, never a panic.
//!
//! Field slot numbers (`4 + 2*field_id`, the "voffset" parameter `flatbuffers::Table::get`
//! takes) are transcribed from FlatGeoBuf's frozen `header.fbs` schema and were verified
//! empirically by hex-dumping + hand-walking `fixtures/fgb/tiny.fgb` (a real `ogr2ogr`-minted
//! file, confirmed against `ogrinfo`'s report of the same file) before being encoded here:
//! `geometry_type`=8, `envelope`=6, `columns`=18, `features_count`=20, `index_node_size`=22,
//! `crs`=24; `Column.name`=4 / `Column.type`=6; `Crs.code`=6.
//!
//! ## Task 7 hardening: the manual root/vtable check above is NOT sufficient
//!
//! The two checks above (root uoffset in-bounds, vtable in-bounds) only prove the ROOT table's
//! own vtable is safe to read *slot offsets* from. Every field accessor below still calls
//! `flatbuffers::Table::get`, which is `unsafe` and does **no bounds checking of its own**: it
//! resolves a field's data location (`table_loc + slot_offset`) and hands it straight to
//! `Follow::follow`, which reads scalars via `core::ptr::copy_nonoverlapping` guarded only by
//! `debug_assert!` — **compiled out in `--release`**. A crafted vtable slot pointing near/past
//! the buffer end, or a nested `ForwardsUOffset`/`Vector` whose declared length runs past the
//! buffer, is a heap out-of-bounds read (real UB) in the release binary this project ships, or
//! at best a slice-index panic in debug. Neither is acceptable for untrusted/remote `.fgb`
//! input (S3, `mvt_http`, GetFeatureInfo — all reachable from a network request).
//!
//! The fix: `Header::parse` runs the buffer through `flatbuffers::Verifier` — the crate's own
//! recursive structural validator (normally driven by `flatc`-generated `Verifiable` impls) —
//! over hand-written `Verifiable` marker types (`HeaderV`/`ColumnV`/`CrsV` below) that mirror
//! this file's field-slot map. The verifier walks every field this reader touches (envelope,
//! columns + each column's name/type, crs + its code), checking every offset/length against
//! the buffer bounds (`Verifier::range_in_buffer`, `saturating_add`-based, never panics/wraps)
//! before returning `Ok`. Once verification passes, the existing hand-written accessors below
//! are provably safe to call unchanged — every location they can possibly resolve to has
//! already been proven in-bounds by the verifier.

use std::io;

use flatbuffers::{
    ForwardsUOffset, SkipSizePrefix, Table, Vector, Verifiable, Verifier, VerifierOptions,
};

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("fgb header: {msg}"))
}

/// Run `flatbuffers::Verifier` over `buf`, treating it as a `size_prefixed` FlatBuffer (per
/// FlatGeoBuf's `u32 header_size · Header FlatBuffer` container section) with its leading `u32`
/// size prefix **already stripped by the caller** (`buf` starts at the root uoffset, matching
/// `Header::parse`'s existing contract).
///
/// This is NOT just bookkeeping: `flatbuffers::Verifier::is_aligned` checks field alignment
/// (e.g. `f64` needs 8-byte alignment) *relative to position 0 of the true flatbuffers
/// reference frame the original writer computed padding against* — which, for a
/// `size_prefixed` buffer, is the position of the size prefix's own first byte, 4 bytes BEFORE
/// where `buf` starts here. Verifying `buf` directly at position 0 shifts every alignment check
/// by 4 bytes relative to what the writer actually aligned to, so real, well-formed `ogr2ogr`
/// files spuriously fail verification (confirmed empirically against `fixtures/fgb/tiny.fgb`).
/// Prepending 4 placeholder bytes reconstructs the correct reference frame; `SkipSizePrefix`
/// then does what its name says before verifying the root table.
fn verify_size_prefixed<T: Verifiable>(buf: &[u8], opts: &VerifierOptions) -> bool {
    let mut framed = Vec::with_capacity(4 + buf.len());
    framed.extend_from_slice(&[0u8; 4]);
    framed.extend_from_slice(buf);
    let mut verifier = Verifier::new(opts, &framed);
    <SkipSizePrefix<ForwardsUOffset<T>>>::run_verifier(&mut verifier, 0).is_ok()
}

/// `Column.fbs` (`name`=4, `type`=6) — both optional here (this reader defaults a missing
/// `name` to `""` and a missing `type` to `0`), matching `Header::columns`'s existing
/// `unwrap_or` semantics: the verifier must not reject a well-formed file that omits either.
struct ColumnV;
impl Verifiable for ColumnV {
    fn run_verifier(
        v: &mut Verifier,
        pos: usize,
    ) -> std::result::Result<(), flatbuffers::InvalidFlatbuffer> {
        v.visit_table(pos)?
            .visit_field::<ForwardsUOffset<&str>>("name", 4, false)?
            .visit_field::<u8>("type", 6, false)?
            .finish();
        Ok(())
    }
}

/// `Crs.fbs` (`code`=6) — optional (`Header::crs_code` returns `None` when absent).
struct CrsV;
impl Verifiable for CrsV {
    fn run_verifier(
        v: &mut Verifier,
        pos: usize,
    ) -> std::result::Result<(), flatbuffers::InvalidFlatbuffer> {
        v.visit_table(pos)?
            .visit_field::<i32>("code", 6, false)?
            .finish();
        Ok(())
    }
}

/// `Header.fbs`, restricted to the fields this reader ever touches (`geometry_type`=8,
/// `envelope`=6, `columns`=18, `features_count`=20, `index_node_size`=22, `crs`=24 — see the
/// module doc's field-slot map). All optional, matching every accessor's own `Option`/
/// `unwrap_or` default. Fields this reader never reads (`name`, `title`, `metadata`, …) are
/// deliberately NOT visited — the verifier only needs to prove safe what we actually access.
struct HeaderV;
impl Verifiable for HeaderV {
    fn run_verifier(
        v: &mut Verifier,
        pos: usize,
    ) -> std::result::Result<(), flatbuffers::InvalidFlatbuffer> {
        v.visit_table(pos)?
            .visit_field::<u8>("geometry_type", 8, false)?
            .visit_field::<ForwardsUOffset<Vector<'static, f64>>>("envelope", 6, false)?
            .visit_field::<ForwardsUOffset<Vector<'static, ForwardsUOffset<ColumnV>>>>(
                "columns", 18, false,
            )?
            .visit_field::<u64>("features_count", 20, false)?
            .visit_field::<u16>("index_node_size", 22, false)?
            .visit_field::<ForwardsUOffset<CrsV>>("crs", 24, false)?
            .finish();
        Ok(())
    }
}

/// A parsed, validated FlatGeoBuf `Header` FlatBuffer. Holds the raw header bytes (as read by
/// `read_range(12, header_size)`, `header_size` = `u32` at file offset 8) plus the validated
/// root table location; field accessors re-derive a `flatbuffers::Table` cheaply (just a
/// `(&[u8], usize)` pair) on each call.
#[derive(Debug)]
pub struct Header {
    buf: Vec<u8>,
    root_loc: usize,
}

impl Header {
    /// Validate + wrap the Header FlatBuffer bytes. Checks, in order: the buffer is large
    /// enough to hold a root uoffset; the root uoffset resolves in-bounds; the table's vtable
    /// (found via the backwards soffset stored at the root location) resolves in-bounds; the
    /// vtable's own declared size fits inside the buffer; and finally (Task 7) the WHOLE header
    /// tree — envelope, columns + each column's fields, crs + its code — passes
    /// `flatbuffers::Verifier`, which recursively checks every offset/length this reader can
    /// possibly follow against the buffer bounds (see the module doc's "Task 7 hardening"
    /// section). Only after all of that does any field accessor below run — so those accessors'
    /// `unsafe` `Table::get` calls cannot walk past `buf`, panic, or read out of bounds, no
    /// matter how the bytes are crafted.
    pub fn parse(buf: Vec<u8>) -> io::Result<Self> {
        if buf.len() < 4 {
            return Err(invalid("buffer too small for a root offset"));
        }
        let root_off = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        // A flatbuffers uoffset is relative to the position it is stored at — here, 0.
        if root_off == 0 || root_off + 4 > buf.len() {
            return Err(invalid("root offset out of bounds"));
        }
        let root_loc = root_off;
        // The vtable pointer is a backwards soffset (i32) stored at the table's own location.
        let soffset = i32::from_le_bytes(buf[root_loc..root_loc + 4].try_into().unwrap());
        let vtable_loc = root_loc as i64 - soffset as i64;
        if vtable_loc < 0 || (vtable_loc as usize) + 4 > buf.len() {
            return Err(invalid("vtable offset out of bounds"));
        }
        let vtable_loc = vtable_loc as usize;
        let vtable_num_bytes =
            u16::from_le_bytes(buf[vtable_loc..vtable_loc + 2].try_into().unwrap()) as usize;
        // A vtable is at minimum its own 2 size fields (num_bytes, object_inline_num_bytes).
        if vtable_num_bytes < 4 || vtable_loc + vtable_num_bytes > buf.len() {
            return Err(invalid("vtable size out of bounds"));
        }
        // Full recursive structural verification (Task 7): every field this reader touches,
        // and everything reachable through it, must resolve in-bounds. This re-derives +
        // re-checks the root uoffset itself too (redundant with the two checks above, which
        // stay as a cheap early bail-out for the common truncated/corrupt cases).
        if !verify_size_prefixed::<HeaderV>(&buf, &VerifierOptions::default()) {
            return Err(invalid("failed FlatBuffer structural verification"));
        }
        Ok(Header { buf, root_loc })
    }

    /// Rebuild the low-level `Table` view over the validated root location.
    fn table(&self) -> Table<'_> {
        // Safety: `parse` validated the root offset resolves to a valid, in-bounds vtable
        // whose declared size fits the buffer — the precondition `Table::new` documents.
        unsafe { Table::new(&self.buf, self.root_loc) }
    }

    /// `header.fbs` field 2 (voffset 8) — `GeometryType`, a byte enum (0 = Unknown). Absent in
    /// a mixed-geometry file (e.g. this crate's own `tiny.fgb` fixture: 2 Points + 1 Polygon);
    /// default 0.
    pub fn geometry_type(&self) -> u8 {
        unsafe { self.table().get::<u8>(8, Some(0)) }.unwrap_or(0)
    }

    /// `header.fbs` field 8 (voffset 20) — total feature count.
    pub fn features_count(&self) -> u64 {
        unsafe { self.table().get::<u64>(20, Some(0)) }.unwrap_or(0)
    }

    /// `header.fbs` field 9 (voffset 22) — R-tree branching factor; 0 means "no index".
    /// `header.fbs` declares a default of 16 (also `ogr2ogr`'s default), used when absent.
    pub fn index_node_size(&self) -> u16 {
        unsafe { self.table().get::<u16>(22, Some(16)) }.unwrap_or(16)
    }

    /// `header.fbs` field 1 (voffset 6) — `[minx, miny, maxx, maxy, ...]`. 2D-only scope: only
    /// the first 4 values are read. `None` if the field is absent or the vector is short.
    pub fn envelope(&self) -> Option<[f64; 4]> {
        let vec = unsafe {
            self.table()
                .get::<ForwardsUOffset<Vector<'_, f64>>>(6, None)
        }?;
        if vec.len() < 4 {
            return None;
        }
        Some([vec.get(0), vec.get(1), vec.get(2), vec.get(3)])
    }

    /// `header.fbs` field 7 (voffset 18) — `[(name, ColumnType-as-u8)]`, in schema order (the
    /// same order each feature's packed property bytes reference by index). Empty if absent.
    pub fn columns(&self) -> Vec<(String, u8)> {
        let vec = unsafe {
            self.table()
                .get::<ForwardsUOffset<Vector<'_, ForwardsUOffset<Table<'_>>>>>(18, None)
        };
        let Some(vec) = vec else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(vec.len());
        for i in 0..vec.len() {
            let col = vec.get(i);
            // Column.fbs field 0 (voffset 4) — name; field 1 (voffset 6) — ColumnType.
            let name = unsafe { col.get::<ForwardsUOffset<&str>>(4, None) }.unwrap_or("");
            let ty = unsafe { col.get::<u8>(6, Some(0)) }.unwrap_or(0);
            out.push((name.to_string(), ty));
        }
        out
    }

    /// `header.fbs` field 10 (voffset 24) → `Crs.code` (`crs.fbs` field 1, voffset 6), e.g.
    /// `4326`. `None` if the header carries no CRS.
    pub fn crs_code(&self) -> Option<i32> {
        let crs = unsafe { self.table().get::<ForwardsUOffset<Table<'_>>>(24, None) }?;
        unsafe { crs.get::<i32>(6, None) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_truncated_buffer() {
        assert!(Header::parse(vec![]).is_err());
        assert!(Header::parse(vec![0, 0, 0]).is_err());
    }

    #[test]
    fn parse_rejects_root_offset_out_of_bounds() {
        // The root offset points past the (tiny) buffer.
        let buf = 0xFFFF_FFFFu32.to_le_bytes().to_vec();
        assert!(Header::parse(buf).is_err());
    }

    #[test]
    fn parse_rejects_root_offset_zero() {
        // A root offset of 0 would make the table point at itself — reject it explicitly
        // rather than let the vtable math wrap.
        let buf = 0u32.to_le_bytes().to_vec();
        assert!(Header::parse(buf).is_err());
    }

    /// Task 2 review flagged this arm (`vtable_loc < 0 || vtable_loc + 4 > buf.len()`, the
    /// "vtable offset out of bounds" error) as untested: a root uoffset that resolves in-bounds
    /// on its own, but whose backwards soffset points the vtable location far outside the
    /// buffer. `root_off=4`; the `i32` soffset stored at `buf[4..8]` is `i32::MAX`, so
    /// `vtable_loc = 4 - i32::MAX` is deeply negative.
    #[test]
    fn parse_rejects_vtable_location_out_of_bounds() {
        let mut buf = vec![0u8; 8];
        buf[0..4].copy_from_slice(&4u32.to_le_bytes());
        buf[4..8].copy_from_slice(&i32::MAX.to_le_bytes());
        let err = Header::parse(buf).expect_err("vtable location out of bounds must be rejected");
        assert!(
            err.to_string().contains("vtable offset out of bounds"),
            "{err}"
        );
    }

    /// Task 2 review flagged this arm too (`vtable_num_bytes < 4 || vtable_loc + vtable_num_bytes
    /// > buf.len()`, the "vtable size out of bounds" error): a root uoffset AND a resolved
    /// vtable location that are both in-bounds, but the vtable's own declared `num_bytes` runs
    /// past the buffer. `root_off=8` (soffset stored at `buf[8..12]`); soffset=4 resolves
    /// `vtable_loc=4`, itself in-bounds (`4+4<=16`); the `u16` at `buf[4..6]` (the vtable's
    /// declared size) is set to 0xFFFF, which fails the second check.
    #[test]
    fn parse_rejects_vtable_size_out_of_bounds() {
        let mut buf = vec![0u8; 16];
        buf[0..4].copy_from_slice(&8u32.to_le_bytes());
        buf[4..6].copy_from_slice(&0xFFFFu16.to_le_bytes());
        buf[8..12].copy_from_slice(&4i32.to_le_bytes());
        let err = Header::parse(buf).expect_err("vtable size out of bounds must be rejected");
        assert!(
            err.to_string().contains("vtable size out of bounds"),
            "{err}"
        );
    }

    /// Truncated / byte-flipped Header buffers must never panic or read out of bounds — only
    /// ever a clean `Err`. This is the malformed-input contract Task 7 exists to enforce: before
    /// this task, a crafted vtable slot offset (this is exactly what a bit-flip on the vtable
    /// bytes produces) reached `Table::get`'s unchecked `Follow::follow` with no bounds check.
    #[test]
    fn parse_never_panics_on_truncated_or_corrupt_bytes() {
        let src =
            crate::cog::LocalFileRangeSource::open("fixtures/fgb/tiny.fgb").expect("open tiny.fgb");
        let prefix = crate::cog::RangeSource::read_range(&src, 0, 12).unwrap();
        let header_size = u32::from_le_bytes(prefix[8..12].try_into().unwrap());
        let full_header =
            crate::cog::RangeSource::read_range(&src, 12, header_size as usize).unwrap();
        assert!(
            Header::parse(full_header.clone()).is_ok(),
            "sanity: full header parses"
        );

        // Every truncation length, including the pathological single-byte and zero-byte cases.
        // The property under test is simply that this loop COMPLETES without a panic/abort --
        // not that every truncation is rejected: a cut close to the full length can land past
        // every field this reader actually touches (trailing padding/unused vtable slots),
        // which the verifier correctly has no reason to reject.
        for cut in 0..full_header.len() {
            let truncated = full_header[..cut].to_vec();
            let _ = Header::parse(truncated); // must not panic; Ok or Err both fine
        }

        // Byte flips at every offset: still never panics; most flip a length/offset field into
        // something invalid (Err), a few flip inert padding and still parse fine (Ok) -- both
        // outcomes are acceptable, a panic is not.
        for i in 0..full_header.len() {
            let mut flipped = full_header.clone();
            flipped[i] ^= 0xFF;
            let _ = Header::parse(flipped); // must not panic; result (Ok or Err) both fine
        }
    }

    /// A well-formed header (this crate's own `tiny.fgb` fixture) must still parse correctly
    /// after Task 7's added `Verifier` pass -- a too-strict verifier that rejects valid
    /// `ogr2ogr`-minted files would be a regression, not a fix.
    #[test]
    fn parse_still_accepts_real_ogr2ogr_header() {
        let src =
            crate::cog::LocalFileRangeSource::open("fixtures/fgb/tiny.fgb").expect("open tiny.fgb");
        let prefix = crate::cog::RangeSource::read_range(&src, 0, 12).unwrap();
        let header_size = u32::from_le_bytes(prefix[8..12].try_into().unwrap());
        let full_header =
            crate::cog::RangeSource::read_range(&src, 12, header_size as usize).unwrap();
        let header = Header::parse(full_header).expect("real header must still parse");
        assert_eq!(header.features_count(), 3);
        assert_eq!(
            header.columns(),
            vec![("name".to_string(), 11), ("pop".to_string(), 5)]
        );
        assert_eq!(header.crs_code(), Some(4326));
        let env = header.envelope().expect("envelope present");
        assert!((env[0] - 0.0).abs() < 1e-9 && (env[2] - 5.0).abs() < 1e-9);
    }
}
