// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! PMTiles v3 reader (bespoke). Opens an archive, caches header + root directory + metadata, and
//! looks up a tile's decompressed bytes by (z,x,y) using positioned reads (Sync, lock-free).

use super::codec::{deserialize_directory, gunzip, read_header};
use super::{zxy_to_tileid, Entry, Header, PmResult};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;

pub struct PmtilesReader {
    file: Mutex<File>,
    file_len: u64,
    header: Header,
    root: Vec<Entry>,
    metadata: String,
}

/// Read `len` bytes at `offset`. Rejects (returns `Err`, does NOT allocate) any read whose range
/// falls outside `[0, file_len)` -- `offset`/`len` come from header/directory fields in an
/// untrusted `.pmtiles` file, and a crafted huge `len` must not reach `vec![0u8; len as usize]`
/// (an over-large allocation aborts the process via the allocator's OOM path, which is
/// uncatchable -- this bounds check is what prevents that).
fn read_at(file: &Mutex<File>, file_len: u64, offset: u64, len: u64) -> PmResult<Vec<u8>> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| format!("pmtiles: read {offset}+{len} overflows"))?;
    if end > file_len {
        return Err(format!(
            "pmtiles: read {offset}+{len} exceeds file length {file_len}"
        ));
    }
    let mut f = file.lock().map_err(|_| "pmtiles file lock poisoned")?;
    f.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek: {e}"))?;
    let mut buf = vec![0u8; len as usize];
    f.read_exact(&mut buf).map_err(|e| format!("read: {e}"))?;
    Ok(buf)
}

impl PmtilesReader {
    pub fn open(path: &Path) -> PmResult<PmtilesReader> {
        let f = File::open(path).map_err(|e| format!("open {path:?}: {e}"))?;
        let file_len = f
            .metadata()
            .map_err(|e| format!("stat {path:?}: {e}"))?
            .len();
        let file = Mutex::new(f);
        let head_bytes = read_at(&file, file_len, 0, 127)?;
        let header = read_header(&head_bytes)?;
        let root = deserialize_directory(&read_at(
            &file,
            file_len,
            header.root_dir_offset,
            header.root_dir_length,
        )?)?;
        let metadata = if header.metadata_length > 0 {
            String::from_utf8(gunzip(&read_at(
                &file,
                file_len,
                header.metadata_offset,
                header.metadata_length,
            )?)?)
            .map_err(|e| format!("metadata utf8: {e}"))?
        } else {
            String::new()
        };
        Ok(PmtilesReader {
            file,
            file_len,
            header,
            root,
            metadata,
        })
    }

    pub fn metadata(&self) -> &str {
        &self.metadata
    }

    /// Decompressed MVT bytes for (z,x,y), or None on a miss.
    pub fn get(&self, z: u32, x: u32, y: u32) -> PmResult<Option<Vec<u8>>> {
        if z > 26 {
            return Ok(None);
        }
        let target = zxy_to_tileid(z, x, y);
        let mut dir = self.root.clone();
        for _ in 0..4 {
            // largest tile_id <= target
            let idx = match dir.binary_search_by(|e| e.tile_id.cmp(&target)) {
                Ok(i) => i,
                Err(0) => return Ok(None),
                Err(i) => i - 1,
            };
            let e = dir[idx];
            if e.run_length == 0 {
                // leaf pointer -> descend
                let leaf_off = self
                    .header
                    .leaf_dirs_offset
                    .checked_add(e.offset)
                    .ok_or_else(|| "pmtiles: leaf dir offset overflow".to_string())?;
                let bytes = read_at(&self.file, self.file_len, leaf_off, e.length)?;
                dir = deserialize_directory(&bytes)?;
                continue;
            }
            let run_end = e
                .tile_id
                .checked_add(e.run_length)
                .ok_or_else(|| "pmtiles: run_length overflow".to_string())?;
            if target < run_end {
                let tile_off = self
                    .header
                    .tile_data_offset
                    .checked_add(e.offset)
                    .ok_or_else(|| "pmtiles: tile data offset overflow".to_string())?;
                let blob = read_at(&self.file, self.file_len, tile_off, e.length)?;
                return Ok(Some(gunzip(&blob)?));
            }
            return Ok(None); // in a gap within the run's id range
        }
        Err("pmtiles: leaf descent exceeded 4 levels".into())
    }

    /// Every addressed TileID in the archive (run_lengths expanded), ascending. Descends leaf dirs.
    pub fn all_tile_ids(&self) -> PmResult<Vec<u64>> {
        let mut ids = Vec::new();
        self.walk_dir(&self.root, &mut |e| {
            for k in 0..e.run_length {
                if let Some(id) = e.tile_id.checked_add(k) {
                    ids.push(id);
                }
            }
        })?;
        ids.sort_unstable();
        Ok(ids)
    }

    /// The stored (gzip'd) blob for a TileID, or None. Descends leaves; does NOT gunzip.
    pub fn raw_tile_by_id(&self, tile_id: u64) -> PmResult<Option<Vec<u8>>> {
        let mut dir = self.root.clone();
        for _ in 0..4 {
            let idx = match dir.binary_search_by(|e| e.tile_id.cmp(&tile_id)) {
                Ok(i) => i,
                Err(0) => return Ok(None),
                Err(i) => i - 1,
            };
            let e = dir[idx];
            if e.run_length == 0 {
                let leaf_off = self
                    .header
                    .leaf_dirs_offset
                    .checked_add(e.offset)
                    .ok_or_else(|| "pmtiles: leaf dir offset overflow".to_string())?;
                let bytes = read_at(&self.file, self.file_len, leaf_off, e.length)?;
                dir = deserialize_directory(&bytes)?;
                continue;
            }
            let end = e
                .tile_id
                .checked_add(e.run_length)
                .ok_or_else(|| "pmtiles: run_length overflow".to_string())?;
            if tile_id < end {
                return Ok(Some(read_at(
                    &self.file,
                    self.file_len,
                    self.header
                        .tile_data_offset
                        .checked_add(e.offset)
                        .ok_or_else(|| "pmtiles: offset overflow".to_string())?,
                    e.length,
                )?));
            }
            return Ok(None);
        }
        Err("pmtiles: leaf descent exceeded 4 levels".into())
    }

    /// Visit every TILE entry (run_length>0), descending leaves. (Helper for all_tile_ids.)
    fn walk_dir(&self, dir: &[Entry], visit: &mut dyn FnMut(&Entry)) -> PmResult<()> {
        self.walk_dir_depth(dir, visit, 0)
    }
    fn walk_dir_depth(
        &self,
        dir: &[Entry],
        visit: &mut dyn FnMut(&Entry),
        depth: u32,
    ) -> PmResult<()> {
        if depth >= 4 {
            return Err("pmtiles: leaf descent exceeded 4 levels".into());
        }
        for e in dir {
            if e.run_length == 0 {
                let leaf_off = self
                    .header
                    .leaf_dirs_offset
                    .checked_add(e.offset)
                    .ok_or_else(|| "pmtiles: leaf dir offset overflow".to_string())?;
                let bytes = read_at(&self.file, self.file_len, leaf_off, e.length)?;
                let leaf = deserialize_directory(&bytes)?;
                self.walk_dir_depth(&leaf, visit, depth + 1)?;
            } else {
                visit(e);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::pmtiles::codec::{
        build_roots_leaves, deserialize_directory, gzip, serialize_directory, write_header,
    };
    use crate::vector::pmtiles::{tileid_to_zxy, zxy_to_tileid, Entry, Header};
    use std::io::Write;

    #[test]
    fn reads_back_a_hand_built_archive() {
        // Two tiles: (2,0,0) and (2,1,1). Store their raw bytes gzip'd, one root dir, no leaves.
        let t_a = b"tile-A-mvt-bytes".to_vec();
        let t_b = b"tile-B".to_vec();
        let (ga, gb) = (gzip(&t_a), gzip(&t_b));
        let id_a = zxy_to_tileid(2, 0, 0);
        let id_b = zxy_to_tileid(2, 1, 1);
        // tile data: ga then gb
        let mut tile_data = Vec::new();
        let off_a = 0u64;
        tile_data.extend_from_slice(&ga);
        let off_b = tile_data.len() as u64;
        tile_data.extend_from_slice(&gb);
        let entries = vec![
            Entry {
                tile_id: id_a,
                offset: off_a,
                length: ga.len() as u64,
                run_length: 1,
            },
            Entry {
                tile_id: id_b,
                offset: off_b,
                length: gb.len() as u64,
                run_length: 1,
            },
        ];
        let root = serialize_directory(&entries);
        let metadata = gzip(br#"{"vector_layers":[]}"#);
        let root_off = 127u64;
        let meta_off = root_off + root.len() as u64;
        let data_off = meta_off + metadata.len() as u64;
        let h = Header {
            root_dir_offset: root_off,
            root_dir_length: root.len() as u64,
            metadata_offset: meta_off,
            metadata_length: metadata.len() as u64,
            leaf_dirs_offset: 0,
            leaf_dirs_length: 0,
            tile_data_offset: data_off,
            tile_data_length: tile_data.len() as u64,
            num_addressed_tiles: 2,
            num_tile_entries: 2,
            num_tile_contents: 2,
            clustered: 1,
            internal_compression: 2,
            tile_compression: 2,
            tile_type: 1,
            min_zoom: 2,
            max_zoom: 2,
            min_lon_e7: 0,
            min_lat_e7: 0,
            max_lon_e7: 0,
            max_lat_e7: 0,
            center_zoom: 2,
            center_lon_e7: 0,
            center_lat_e7: 0,
        };
        let dir = std::env::temp_dir().join(format!("ts_pmt_{}.pmtiles", std::process::id()));
        {
            let mut f = std::fs::File::create(&dir).unwrap();
            f.write_all(&write_header(&h)).unwrap();
            f.write_all(&root).unwrap();
            f.write_all(&metadata).unwrap();
            f.write_all(&tile_data).unwrap();
        }
        let r = PmtilesReader::open(&dir).unwrap();
        assert_eq!(r.get(2, 0, 0).unwrap().as_deref(), Some(&t_a[..]));
        assert_eq!(r.get(2, 1, 1).unwrap().as_deref(), Some(&t_b[..]));
        assert_eq!(r.get(2, 1, 0).unwrap(), None); // miss
        assert!(r.metadata().contains("vector_layers"));
        std::fs::remove_file(&dir).ok();
    }

    #[test]
    fn reads_a_leaf_resident_tile() {
        // ~200k entries with distinct ascending tile_ids and distinct offsets (mirrors the codec's
        // proven-to-spill pattern -- a constant offset/length across all entries compresses too well
        // and never spills) forces build_roots_leaves to split into leaf directories. We then pick a
        // tile_id that is NOT one of the root's leaf-pointer entries (i.e. not the first id of its
        // leaf chunk), so PmtilesReader::get must descend into a leaf directory to resolve it. That
        // one entry is rewired to point at the single real tile blob written to the file below; the
        // other 199,999 entries' offsets are never read (their bytes need not exist in the file).
        let blob_raw = b"leaf-resident-tile-bytes".to_vec();
        let blob = gzip(&blob_raw);
        let mut entries: Vec<Entry> = (0..200_000u64)
            .map(|k| Entry {
                tile_id: k * 7 + (k % 3),
                offset: k * 40,
                length: 39,
                run_length: 1,
            })
            .collect();

        // Determine root-level (leaf-pointer) tile_ids from the baseline layout, then pick a k from
        // the middle whose tile_id is not one of them.
        let (root0, _leaves0) = build_roots_leaves(&entries);
        let root_ids0: std::collections::HashSet<u64> = deserialize_directory(&root0)
            .unwrap()
            .iter()
            .map(|e| e.tile_id)
            .collect();
        let mut k = 100_000u64;
        while root_ids0.contains(&(k * 7 + (k % 3))) {
            k += 1;
        }
        let target_id = k * 7 + (k % 3);
        entries[k as usize].offset = 0;
        entries[k as usize].length = blob.len() as u64;

        let (root, leaves) = build_roots_leaves(&entries);
        assert!(
            !leaves.is_empty(),
            "expected the directory to spill to leaves"
        );
        let root_ids: std::collections::HashSet<u64> = deserialize_directory(&root)
            .unwrap()
            .iter()
            .map(|e| e.tile_id)
            .collect();
        assert!(
            !root_ids.contains(&target_id),
            "target tile_id must require leaf descent, not be root-resident"
        );

        let metadata = gzip(br#"{"vector_layers":[]}"#);
        let root_off = 127u64;
        let meta_off = root_off + root.len() as u64;
        let leaf_off = meta_off + metadata.len() as u64;
        let data_off = leaf_off + leaves.len() as u64;
        let h = Header {
            root_dir_offset: root_off,
            root_dir_length: root.len() as u64,
            metadata_offset: meta_off,
            metadata_length: metadata.len() as u64,
            leaf_dirs_offset: leaf_off,
            leaf_dirs_length: leaves.len() as u64,
            tile_data_offset: data_off,
            tile_data_length: blob.len() as u64,
            num_addressed_tiles: entries.len() as u64,
            num_tile_entries: entries.len() as u64,
            num_tile_contents: 1,
            clustered: 1,
            internal_compression: 2,
            tile_compression: 2,
            tile_type: 1,
            min_zoom: 0,
            max_zoom: 26,
            min_lon_e7: 0,
            min_lat_e7: 0,
            max_lon_e7: 0,
            max_lat_e7: 0,
            center_zoom: 0,
            center_lon_e7: 0,
            center_lat_e7: 0,
        };
        let path = std::env::temp_dir().join(format!("ts_pmt_leaf_{}.pmtiles", std::process::id()));
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(&write_header(&h)).unwrap();
            f.write_all(&root).unwrap();
            f.write_all(&metadata).unwrap();
            f.write_all(&leaves).unwrap();
            f.write_all(&blob).unwrap();
        }

        let r = PmtilesReader::open(&path).unwrap();
        let (z, x, y) = tileid_to_zxy(target_id);
        assert_eq!(r.get(z, x, y).unwrap().as_deref(), Some(&blob_raw[..]));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn corrupt_length_returns_err_not_abort() {
        // A header whose root_dir_length claims ~1 TiB, written to a file that is ONLY the 127-byte
        // header (no root directory bytes follow). Before the file-length bound was added, this
        // length flowed straight into `vec![0u8; len as usize]` -- an allocation that huge aborts
        // the process via the allocator's OOM path (uncatchable, not a `panic!`/`Result`). `open`
        // must instead return `Err`.
        let h = Header {
            root_dir_offset: 127,
            root_dir_length: 1u64 << 40,
            metadata_offset: 127,
            metadata_length: 0,
            leaf_dirs_offset: 0,
            leaf_dirs_length: 0,
            tile_data_offset: 127,
            tile_data_length: 0,
            num_addressed_tiles: 0,
            num_tile_entries: 0,
            num_tile_contents: 0,
            clustered: 1,
            internal_compression: 2,
            tile_compression: 2,
            tile_type: 1,
            min_zoom: 0,
            max_zoom: 0,
            min_lon_e7: 0,
            min_lat_e7: 0,
            max_lon_e7: 0,
            max_lat_e7: 0,
            center_zoom: 0,
            center_lon_e7: 0,
            center_lat_e7: 0,
        };
        let path =
            std::env::temp_dir().join(format!("ts_pmt_corrupt_{}.pmtiles", std::process::id()));
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(&write_header(&h)).unwrap(); // ONLY the header -- no root dir bytes.
        }
        let result = PmtilesReader::open(&path);
        assert!(
            result.is_err(),
            "open must return Err on an out-of-bounds directory length, not abort"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn run_length_interior_hit_and_gap_miss() {
        // A single directory entry {tile_id: T, run_length: 3} at z=3 covers ids [T, T+3). Any id
        // inside that range must resolve to the entry's blob (interior hit); the id right after the
        // run (T+3) falls in the gap and must miss -- covers the `target < tile_id + run_length`
        // branch on both sides.
        let blob_raw = b"run-length-tile-bytes".to_vec();
        let blob = gzip(&blob_raw);
        let t = zxy_to_tileid(3, 2, 2);
        let entries = vec![Entry {
            tile_id: t,
            offset: 0,
            length: blob.len() as u64,
            run_length: 3,
        }];
        let root = serialize_directory(&entries);
        let metadata = gzip(br#"{"vector_layers":[]}"#);
        let root_off = 127u64;
        let meta_off = root_off + root.len() as u64;
        let data_off = meta_off + metadata.len() as u64;
        let h = Header {
            root_dir_offset: root_off,
            root_dir_length: root.len() as u64,
            metadata_offset: meta_off,
            metadata_length: metadata.len() as u64,
            leaf_dirs_offset: 0,
            leaf_dirs_length: 0,
            tile_data_offset: data_off,
            tile_data_length: blob.len() as u64,
            num_addressed_tiles: 3,
            num_tile_entries: 1,
            num_tile_contents: 1,
            clustered: 1,
            internal_compression: 2,
            tile_compression: 2,
            tile_type: 1,
            min_zoom: 3,
            max_zoom: 3,
            min_lon_e7: 0,
            min_lat_e7: 0,
            max_lon_e7: 0,
            max_lat_e7: 0,
            center_zoom: 3,
            center_lon_e7: 0,
            center_lat_e7: 0,
        };
        let path = std::env::temp_dir().join(format!("ts_pmt_run_{}.pmtiles", std::process::id()));
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(&write_header(&h)).unwrap();
            f.write_all(&root).unwrap();
            f.write_all(&metadata).unwrap();
            f.write_all(&blob).unwrap();
        }
        let r = PmtilesReader::open(&path).unwrap();

        // Interior hit: T+1 is inside [T, T+3).
        let (z, x, y) = tileid_to_zxy(t + 1);
        assert_eq!(r.get(z, x, y).unwrap().as_deref(), Some(&blob_raw[..]));

        // Gap miss: T+3 is just past the run.
        let (z2, x2, y2) = tileid_to_zxy(t + 3);
        assert_eq!(r.get(z2, x2, y2).unwrap(), None);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn all_ids_and_raw_by_id_round_trip() {
        use crate::vector::pmtiles::codec::{gzip, serialize_directory, write_header};
        use crate::vector::pmtiles::{zxy_to_tileid, Entry, Header};
        use std::io::Write;
        let ta = b"AAAA".to_vec();
        let tb = b"BBBB".to_vec();
        let (ga, gb) = (gzip(&ta), gzip(&tb));
        let id_a = zxy_to_tileid(1, 0, 0);
        let id_b = zxy_to_tileid(1, 1, 1);
        let mut data = Vec::new();
        let off_a = 0u64;
        data.extend_from_slice(&ga);
        let off_b = data.len() as u64;
        data.extend_from_slice(&gb);
        let entries = vec![
            Entry {
                tile_id: id_a,
                offset: off_a,
                length: ga.len() as u64,
                run_length: 1,
            },
            Entry {
                tile_id: id_b,
                offset: off_b,
                length: gb.len() as u64,
                run_length: 1,
            },
        ];
        let root = serialize_directory(&entries);
        let meta = gzip(b"{}");
        let h = Header {
            root_dir_offset: 127,
            root_dir_length: root.len() as u64,
            metadata_offset: 127 + root.len() as u64,
            metadata_length: meta.len() as u64,
            leaf_dirs_offset: 0,
            leaf_dirs_length: 0,
            tile_data_offset: 127 + root.len() as u64 + meta.len() as u64,
            tile_data_length: data.len() as u64,
            num_addressed_tiles: 2,
            num_tile_entries: 2,
            num_tile_contents: 2,
            clustered: 1,
            internal_compression: 2,
            tile_compression: 2,
            tile_type: 1,
            min_zoom: 1,
            max_zoom: 1,
            min_lon_e7: 0,
            min_lat_e7: 0,
            max_lon_e7: 0,
            max_lat_e7: 0,
            center_zoom: 1,
            center_lon_e7: 0,
            center_lat_e7: 0,
        };
        let p = std::env::temp_dir().join(format!("ts_ids_{}.pmtiles", std::process::id()));
        {
            let mut f = std::fs::File::create(&p).unwrap();
            f.write_all(&write_header(&h)).unwrap();
            f.write_all(&root).unwrap();
            f.write_all(&meta).unwrap();
            f.write_all(&data).unwrap();
        }
        let r = PmtilesReader::open(&p).unwrap();
        let mut ids = r.all_tile_ids().unwrap();
        ids.sort();
        assert_eq!(ids, {
            let mut v = vec![id_a, id_b];
            v.sort();
            v
        });
        assert_eq!(r.raw_tile_by_id(id_a).unwrap().as_deref(), Some(&ga[..])); // STILL gzip'd
        assert_eq!(r.raw_tile_by_id(zxy_to_tileid(1, 0, 1)).unwrap(), None);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn all_tile_ids_expands_run_length() {
        // A single directory entry {tile_id: T, run_length: 3} addresses three consecutive TileIDs
        // (T, T+1, T+2) via one entry -- the compactor relies on all_tile_ids() expanding that run
        // rather than returning just the entry's own tile_id.
        let blob_raw = b"run-length-tile-bytes".to_vec();
        let blob = gzip(&blob_raw);
        let t = zxy_to_tileid(3, 2, 2);
        let entries = vec![Entry {
            tile_id: t,
            offset: 0,
            length: blob.len() as u64,
            run_length: 3,
        }];
        let root = serialize_directory(&entries);
        let metadata = gzip(br#"{"vector_layers":[]}"#);
        let root_off = 127u64;
        let meta_off = root_off + root.len() as u64;
        let data_off = meta_off + metadata.len() as u64;
        let h = Header {
            root_dir_offset: root_off,
            root_dir_length: root.len() as u64,
            metadata_offset: meta_off,
            metadata_length: metadata.len() as u64,
            leaf_dirs_offset: 0,
            leaf_dirs_length: 0,
            tile_data_offset: data_off,
            tile_data_length: blob.len() as u64,
            num_addressed_tiles: 3,
            num_tile_entries: 1,
            num_tile_contents: 1,
            clustered: 1,
            internal_compression: 2,
            tile_compression: 2,
            tile_type: 1,
            min_zoom: 3,
            max_zoom: 3,
            min_lon_e7: 0,
            min_lat_e7: 0,
            max_lon_e7: 0,
            max_lat_e7: 0,
            center_zoom: 3,
            center_lon_e7: 0,
            center_lat_e7: 0,
        };
        let path =
            std::env::temp_dir().join(format!("ts_pmt_run_ids_{}.pmtiles", std::process::id()));
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(&write_header(&h)).unwrap();
            f.write_all(&root).unwrap();
            f.write_all(&metadata).unwrap();
            f.write_all(&blob).unwrap();
        }
        let r = PmtilesReader::open(&path).unwrap();
        let mut ids = r.all_tile_ids().unwrap();
        ids.sort();
        assert_eq!(ids, vec![t, t + 1, t + 2]);
        std::fs::remove_file(&path).ok();
    }
}
