// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! PMTiles v3 directory + header codec (bespoke): columnar ULEB128 varints with the offset sentinel,
//! gzip via flate2, and the fixed 127-byte header.

use super::{Entry, Header, PmResult};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::io::{Read, Write};

pub(crate) fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(bytes).expect("gzip write");
    enc.finish().expect("gzip finish")
}

/// A ~1 KB crafted gzip blob (a directory/metadata/tile entry, read via the length-bounded
/// `read_at`) can decompress to many GB -- a "gzip bomb". `read_at` bounds the COMPRESSED read,
/// but gunzip itself decompresses unboundedly, so an over-large allocation would abort the
/// process via the allocator's OOM path (uncatchable). 256 MiB is far above any legitimate
/// directory or MVT tile and well below the point where the allocation itself becomes dangerous.
const MAX_GUNZIP_OUT: u64 = 256 * 1024 * 1024;

pub(crate) fn gunzip(bytes: &[u8]) -> PmResult<Vec<u8>> {
    gunzip_capped(bytes, MAX_GUNZIP_OUT)
}

/// `gunzip` with an explicit output cap (broken out for testability with a small cap, so the
/// oversized-rejection test doesn't need to allocate hundreds of MB to exercise the path).
pub(crate) fn gunzip_capped(bytes: &[u8], max_out: u64) -> PmResult<Vec<u8>> {
    let mut out = Vec::new();
    // take(max_out + 1) so an over-limit stream reads exactly one byte past the cap -> detectable
    // without ever buffering the full (potentially huge) decompressed output.
    GzDecoder::new(bytes)
        .take(max_out + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("gunzip: {e}"))?;
    if out.len() as u64 > max_out {
        return Err(format!("gunzip: decompressed size exceeds cap {max_out}"));
    }
    Ok(out)
}

fn write_uvarint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

fn read_uvarint(bytes: &[u8], pos: &mut usize) -> PmResult<u64> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let b = *bytes.get(*pos).ok_or("varint: unexpected end")?;
        *pos += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= 64 {
            return Err("varint: overflow".into());
        }
    }
}

/// Serialize a directory: sort by tile_id, write 5 columns of varints (tile_id delta-encoded, offset
/// sentinel-encoded), then gzip.
pub fn serialize_directory(entries: &[Entry]) -> Vec<u8> {
    let mut es = entries.to_vec();
    es.sort_by_key(|e| e.tile_id);
    let mut raw = Vec::new();
    write_uvarint(&mut raw, es.len() as u64);
    let mut last_id = 0u64;
    for e in &es {
        write_uvarint(&mut raw, e.tile_id - last_id);
        last_id = e.tile_id;
    }
    for e in &es {
        write_uvarint(&mut raw, e.run_length);
    }
    for e in &es {
        write_uvarint(&mut raw, e.length);
    }
    for (i, e) in es.iter().enumerate() {
        if i > 0 && e.offset == es[i - 1].offset + es[i - 1].length {
            write_uvarint(&mut raw, 0);
        } else {
            write_uvarint(&mut raw, e.offset + 1);
        }
    }
    gzip(&raw)
}

/// Inverse of `serialize_directory` (input is gzip'd).
pub fn deserialize_directory(bytes: &[u8]) -> PmResult<Vec<Entry>> {
    let raw = gunzip(bytes)?;
    let mut pos = 0usize;
    let n = read_uvarint(&raw, &mut pos)? as usize;
    if n.saturating_mul(4) > raw.len().saturating_sub(pos) {
        return Err(format!(
            "directory: entry count {n} exceeds available bytes"
        ));
    }
    let mut es = vec![
        Entry {
            tile_id: 0,
            offset: 0,
            length: 0,
            run_length: 0
        };
        n
    ];
    let mut last = 0u64;
    for e in es.iter_mut() {
        last += read_uvarint(&raw, &mut pos)?;
        e.tile_id = last;
    }
    for e in es.iter_mut() {
        e.run_length = read_uvarint(&raw, &mut pos)?;
    }
    for e in es.iter_mut() {
        e.length = read_uvarint(&raw, &mut pos)?;
    }
    for i in 0..n {
        let v = read_uvarint(&raw, &mut pos)?;
        es[i].offset = if v == 0 {
            if i == 0 {
                return Err("directory: offset sentinel 0 at first entry".into());
            }
            es[i - 1].offset + es[i - 1].length
        } else {
            v - 1
        };
    }
    Ok(es)
}

/// The fixed 127-byte header, little-endian.
pub fn write_header(h: &Header) -> [u8; 127] {
    let mut b = [0u8; 127];
    b[0..7].copy_from_slice(b"PMTiles");
    b[7] = 3;
    let mut u = |off: usize, v: u64| b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    u(8, h.root_dir_offset);
    u(16, h.root_dir_length);
    u(24, h.metadata_offset);
    u(32, h.metadata_length);
    u(40, h.leaf_dirs_offset);
    u(48, h.leaf_dirs_length);
    u(56, h.tile_data_offset);
    u(64, h.tile_data_length);
    u(72, h.num_addressed_tiles);
    u(80, h.num_tile_entries);
    u(88, h.num_tile_contents);
    b[96] = h.clustered;
    b[97] = h.internal_compression;
    b[98] = h.tile_compression;
    b[99] = h.tile_type;
    b[100] = h.min_zoom;
    b[101] = h.max_zoom;
    let mut i = |off: usize, v: i32| b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    i(102, h.min_lon_e7);
    i(106, h.min_lat_e7);
    i(110, h.max_lon_e7);
    i(114, h.max_lat_e7);
    i(119, h.center_lon_e7);
    i(123, h.center_lat_e7);
    b[118] = h.center_zoom;
    b
}

/// Decode a 127-byte header. Errors on bad magic/version/length.
pub fn read_header(bytes: &[u8]) -> PmResult<Header> {
    if bytes.len() < 127 {
        return Err("header: too short".into());
    }
    if &bytes[0..7] != b"PMTiles" {
        return Err("header: bad magic".into());
    }
    if bytes[7] != 3 {
        return Err(format!("header: unsupported version {}", bytes[7]));
    }
    let u = |off: usize| u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
    let i = |off: usize| i32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
    Ok(Header {
        root_dir_offset: u(8),
        root_dir_length: u(16),
        metadata_offset: u(24),
        metadata_length: u(32),
        leaf_dirs_offset: u(40),
        leaf_dirs_length: u(48),
        tile_data_offset: u(56),
        tile_data_length: u(64),
        num_addressed_tiles: u(72),
        num_tile_entries: u(80),
        num_tile_contents: u(88),
        clustered: bytes[96],
        internal_compression: bytes[97],
        tile_compression: bytes[98],
        tile_type: bytes[99],
        min_zoom: bytes[100],
        max_zoom: bytes[101],
        min_lon_e7: i(102),
        min_lat_e7: i(106),
        max_lon_e7: i(110),
        max_lat_e7: i(114),
        center_zoom: bytes[118],
        center_lon_e7: i(119),
        center_lat_e7: i(123),
    })
}

const MAX_ROOT_BYTES: usize = 16384 - 127; // 16257

/// Build the root directory (and leaf directories if needed) so the root fits within the first 16 KiB.
/// Returns (root_gzip, leaves_gzip). Entries must be tile entries (run_length >= 1).
pub fn build_roots_leaves(entries: &[Entry]) -> (Vec<u8>, Vec<u8>) {
    // Fast path: everything fits in the root.
    let root = serialize_directory(entries);
    if root.len() <= MAX_ROOT_BYTES {
        return (root, Vec::new());
    }
    // Spill: partition entries into leaves of `leaf_size`, grow ×1.2 until the root of leaf-pointers fits.
    let mut leaf_size = 4096usize;
    loop {
        let mut leaves = Vec::new();
        let mut root_entries = Vec::new();
        let mut sorted = entries.to_vec();
        sorted.sort_by_key(|e| e.tile_id);
        for chunk in sorted.chunks(leaf_size) {
            let leaf = serialize_directory(chunk);
            root_entries.push(Entry {
                tile_id: chunk[0].tile_id,
                offset: leaves.len() as u64,
                length: leaf.len() as u64,
                run_length: 0, // leaf pointer
            });
            leaves.extend_from_slice(&leaf);
        }
        let root = serialize_directory(&root_entries);
        if root.len() <= MAX_ROOT_BYTES {
            return (root, leaves);
        }
        leaf_size = (leaf_size as f64 * 1.2).ceil() as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::pmtiles::{Entry, Header};

    fn e(id: u64, off: u64, len: u64, rl: u64) -> Entry {
        Entry {
            tile_id: id,
            offset: off,
            length: len,
            run_length: rl,
        }
    }

    #[test]
    fn directory_round_trips_contiguous_and_gapped() {
        // entry[1] offset is contiguous (== e0.off + e0.len) -> sentinel 0;
        // entry[2] offset is NOT contiguous -> stored as offset+1.
        let dir = vec![e(1, 0, 10, 1), e(2, 10, 5, 3), e(9, 100, 7, 1)];
        let round = deserialize_directory(&serialize_directory(&dir)).unwrap();
        assert_eq!(round, dir);
    }

    #[test]
    fn directory_round_trips_leaf_pointer() {
        // run_length == 0 => leaf pointer; must survive the codec.
        let dir = vec![e(0, 0, 40, 0), e(5000, 40, 33, 0)];
        assert_eq!(
            deserialize_directory(&serialize_directory(&dir)).unwrap(),
            dir
        );
    }

    #[test]
    fn header_round_trips() {
        let h = Header {
            root_dir_offset: 127,
            root_dir_length: 200,
            metadata_offset: 327,
            metadata_length: 50,
            leaf_dirs_offset: 0,
            leaf_dirs_length: 0,
            tile_data_offset: 377,
            tile_data_length: 4096,
            num_addressed_tiles: 1000,
            num_tile_entries: 40,
            num_tile_contents: 12,
            clustered: 1,
            internal_compression: 2,
            tile_compression: 2,
            tile_type: 1,
            min_zoom: 0,
            max_zoom: 14,
            min_lon_e7: -95000000,
            min_lat_e7: 360000000_i32.wrapping_neg(),
            max_lon_e7: -80000000,
            max_lat_e7: 420000000,
            center_zoom: 7,
            center_lon_e7: -87000000,
            center_lat_e7: 390000000,
        };
        let bytes = write_header(&h);
        assert_eq!(bytes.len(), 127);
        assert_eq!(&bytes[0..7], b"PMTiles");
        assert_eq!(bytes[7], 3);
        assert_eq!(read_header(&bytes).unwrap(), h);
    }

    #[test]
    fn deserialize_directory_rejects_huge_entry_count() {
        // Only the entry-count varint (claiming 1,000,000 entries) is present; no column bytes
        // follow. Must be a hard Err, not an OOM-scale allocation or panic.
        let mut raw = Vec::new();
        write_uvarint(&mut raw, 1_000_000);
        let gzipped = gzip(&raw);
        assert!(deserialize_directory(&gzipped).is_err());
    }

    #[test]
    fn deserialize_directory_rejects_offset_sentinel_zero_at_first_entry() {
        // Hand-build the raw columnar layout: count=1, tile_id=[0], run_length=[1], length=[10],
        // offset=[0]. A sentinel 0 at i==0 has no preceding entry to inherit from, so a valid
        // writer never emits it -- must be rejected, not underflow/panic/wrap.
        let mut raw = Vec::new();
        write_uvarint(&mut raw, 1); // count
        write_uvarint(&mut raw, 0); // tile_id delta
        write_uvarint(&mut raw, 1); // run_length
        write_uvarint(&mut raw, 10); // length
        write_uvarint(&mut raw, 0); // offset sentinel (invalid at first entry)
        let gzipped = gzip(&raw);
        assert!(deserialize_directory(&gzipped).is_err());
    }

    #[test]
    fn gunzip_rejects_oversized_output() {
        // A small gzip'd blob whose decompressed size (5000 bytes) exceeds a SMALL cap must be
        // rejected -- this is the gzip-bomb guard, exercised here without allocating anything
        // near the real 256 MiB production cap.
        let bomb = gzip(&vec![0u8; 5000]);
        assert!(gunzip_capped(&bomb, 100).is_err());
        assert!(gunzip_capped(&bomb, 10_000).is_ok());
    }

    #[test]
    fn small_directory_needs_no_leaves() {
        let dir: Vec<Entry> = (0..50).map(|k| e(k, k * 10, 10, 1)).collect();
        let (root, leaves) = build_roots_leaves(&dir);
        assert!(leaves.is_empty(), "small dir fits in root");
        assert!(127 + root.len() <= 16384);
        // root deserializes back to the same tile entries (no leaf pointers)
        assert_eq!(deserialize_directory(&root).unwrap(), {
            let mut d = dir.clone();
            d.sort_by_key(|x| x.tile_id);
            d
        });
    }

    #[test]
    fn large_directory_spills_to_leaves_and_root_fits() {
        // Enough distinct entries that the compressed root would exceed 16 KiB without leaf-splitting.
        let dir: Vec<Entry> = (0..200_000u64)
            .map(|k| e(k * 7 + (k % 3), k * 40, 39, 1))
            .collect();
        let (root, leaves) = build_roots_leaves(&dir);
        assert!(!leaves.is_empty(), "large dir must spill to leaves");
        assert!(
            127 + root.len() <= 16384,
            "root must fit 16 KiB, got {}",
            root.len()
        );
        // every root entry is a leaf pointer (run_length == 0)
        for entry in deserialize_directory(&root).unwrap() {
            assert_eq!(entry.run_length, 0, "root entries are leaf pointers");
        }
    }
}
