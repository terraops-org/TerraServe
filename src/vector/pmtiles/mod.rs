// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Bespoke PMTiles v3 support — TileID (Hilbert) + on-disk format types.
//! See docs/superpowers/specs/2026-07-15-pmtiles-pyramid-serve-design.md.

pub mod codec;
pub mod generate;
pub mod overlay;
pub mod read;
pub mod write;

pub type PmResult<T> = Result<T, String>;

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
