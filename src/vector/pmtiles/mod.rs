// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Bespoke PMTiles v3 support — TileID (Hilbert) + on-disk format types.
//! See docs/superpowers/specs/2026-07-15-pmtiles-pyramid-serve-design.md.

pub mod codec;
pub mod generate;
pub mod overlay;
pub mod raster;
pub mod read;
pub mod write;

pub type PmResult<T> = Result<T, String>;

/// How many times a bake re-attempts one tile whose SOURCE READ failed before giving up.
///
/// Not a style choice — a measurement. Two consecutive EU5 bakes lost 10 then 4 tiles to
/// `connection closed`, always with NOTHING in the Postgres log, i.e. closed client-side and
/// transient. Aborting the whole bake on the first of those would kill a 12-minute run at minute
/// nine for a fault that clears on the next attempt.
const TILE_READ_ATTEMPTS: u32 = 3;
/// Backoff between attempts. Short: this covers a dropped connection or a pool hiccup, not an
/// outage, and a bake with thousands of tiles should not spend minutes sleeping.
const TILE_RETRY_BACKOFF_MS: u64 = 250;

/// Run one tile's build, retrying a failed SOURCE READ, and on final failure return an error that
/// names the tile.
///
/// ⚠ This exists because the alternative shipped and was worse. A tile whose query failed used to
/// be baked as an EMPTY tile: the reader `postgis.rs`/`fgb.rs`/`gpkg.rs` returned `Vec::new()` on
/// error, so the bake saw "no features here" and froze a hole into the archive — where it then
/// shadows the live path FOREVER, because a read-through archive hit never falls back. The failure
/// was silent, permanent, and the archive cannot tell you which tiles it hit.
///
/// So: retry, then ABORT. A bake that stops with "z9/271/164 failed after 3 attempts" costs the
/// operator a re-run; a bake that quietly writes blanks costs them a wrong map they cannot see.
pub(crate) fn build_tile_with_retry<T>(
    z: u32,
    x: u32,
    y: u32,
    mut build: impl FnMut() -> PmResult<T>,
) -> PmResult<T> {
    let mut last = String::new();
    for attempt in 1..=TILE_READ_ATTEMPTS {
        match build() {
            Ok(v) => {
                if attempt > 1 {
                    eprintln!("build-pmtiles: z{z}/{x}/{y} succeeded on attempt {attempt}");
                }
                return Ok(v);
            }
            Err(e) => {
                eprintln!(
                    "build-pmtiles: z{z}/{x}/{y} attempt {attempt}/{TILE_READ_ATTEMPTS} failed: {e}"
                );
                last = e;
                if attempt < TILE_READ_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(TILE_RETRY_BACKOFF_MS));
                }
            }
        }
    }
    Err(format!(
        "build-pmtiles: tile z{z}/{x}/{y} failed after {TILE_READ_ATTEMPTS} attempts: {last}. \
         Aborting rather than writing an empty tile — a blank baked here would shadow the live \
         path permanently. Fix the source (for PostGIS, often TERRASERVE_PG_STATEMENT_TIMEOUT_MS \
         at its 30 s default on a low-zoom query) and re-run."
    ))
}

/// `base(z) = (4^z - 1)/3` via accumulation (overflow-safe).
fn zoom_base(z: u32) -> u64 {
    (0..z).fold(0u64, |a, tz| a + (1u64 << (2 * tz)))
}

/// (z,x,y) -> PMTiles Hilbert TileID. Matches the reference js `zxyToTileId`.
pub fn zxy_to_tileid(z: u32, x: u32, y: u32) -> u64 {
    debug_assert!(z <= 26, "zoom > 26 exceeds PMTiles interop cap");
    let n: u64 = 1 << z;
    let (mut xx, mut yy) = (x as u64, y as u64);
    let mut d: u64 = 0;
    let mut s = n >> 1;
    while s > 0 {
        let rx = ((xx & s) > 0) as u64;
        let ry = ((yy & s) > 0) as u64;
        d += s * s * ((3 * rx) ^ ry);
        if ry == 0 {
            if rx == 1 {
                xx = n - 1 - xx;
                yy = n - 1 - yy;
            }
            std::mem::swap(&mut xx, &mut yy);
        }
        s >>= 1;
    }
    zoom_base(z) + d
}

/// TileID -> (z,x,y). Inverse of `zxy_to_tileid`.
pub fn tileid_to_zxy(id: u64) -> (u32, u32, u32) {
    let (mut acc, mut z) = (0u64, 0u32);
    loop {
        let num = 1u64 << (2 * z);
        if acc + num > id {
            break;
        }
        acc += num;
        z += 1;
    }
    let mut t = id - acc;
    let (mut x, mut y) = (0u64, 0u64);
    let mut s: u64 = 1;
    while s < (1u64 << z) {
        let rx = 1 & (t / 2);
        let ry = 1 & (t ^ rx);
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - x;
                y = s - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        x += s * rx;
        y += s * ry;
        t /= 4;
        s <<= 1;
    }
    (z, x as u32, y as u32)
}

/// A directory entry. `run_length == 0` marks a leaf-directory pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u64,
    pub run_length: u64,
}

/// The fixed 127-byte PMTiles v3 header (decoded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub root_dir_offset: u64,
    pub root_dir_length: u64,
    pub metadata_offset: u64,
    pub metadata_length: u64,
    pub leaf_dirs_offset: u64,
    pub leaf_dirs_length: u64,
    pub tile_data_offset: u64,
    pub tile_data_length: u64,
    pub num_addressed_tiles: u64,
    pub num_tile_entries: u64,
    pub num_tile_contents: u64,
    pub clustered: u8,
    pub internal_compression: u8,
    pub tile_compression: u8,
    pub tile_type: u8,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub min_lon_e7: i32,
    pub min_lat_e7: i32,
    pub max_lon_e7: i32,
    pub max_lat_e7: i32,
    pub center_zoom: u8,
    pub center_lon_e7: i32,
    pub center_lat_e7: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hilbert_conformance_vectors() {
        // From the PMTiles reference (js/src/index.ts) — the byte-exact oracle.
        assert_eq!(zxy_to_tileid(1, 0, 0), 1);
        assert_eq!(zxy_to_tileid(1, 0, 1), 2);
        assert_eq!(zxy_to_tileid(1, 1, 1), 3);
        assert_eq!(zxy_to_tileid(1, 1, 0), 4);
        assert_eq!(zxy_to_tileid(0, 0, 0), 0);
        assert_eq!(zxy_to_tileid(8, 40, 87), 36052);
        assert_eq!(zxy_to_tileid(12, 3423, 1763), 19078479);
    }

    #[test]
    fn tileid_zxy_round_trips() {
        for z in 0..=14u32 {
            let n = 1u32 << z;
            let step = (n / 7).max(1);
            let mut x = 0;
            while x < n {
                let mut y = 0;
                while y < n {
                    let id = zxy_to_tileid(z, x, y);
                    assert_eq!(tileid_to_zxy(id), (z, x, y), "z{z} {x},{y} id {id}");
                    y += step;
                }
                x += step;
            }
        }
    }
}
