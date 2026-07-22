// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! PMTiles v3 streaming writer (bespoke). Tiles are added in ascending TileID order (already gzip'd);
//! data streams to a temp file while a dedup map + RLE-collapsed entry list stay in RAM; `finish`
//! writes header + directories, then appends the temp data section.

use super::codec::{build_roots_leaves, gzip, write_header};
use super::{Entry, Header, PmResult};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// FNV-1a 64-bit (bespoke, cheap) — for dedup keying only.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[derive(Clone)]
pub struct HeaderFields {
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub bounds_e7: [i32; 4],    // [min_lon, min_lat, max_lon, max_lat]
    pub center: (u8, i32, i32), // (zoom, lon_e7, lat_e7)
}

pub struct Counts {
    pub addressed: u64,
    pub entries: u64,
    pub contents: u64,
    pub bytes: u64,
}

pub struct PmtilesWriter {
    tmp_path: PathBuf,
    data: BufWriter<File>,
    data_len: u64,
    dedup: std::collections::HashMap<u64, (u64, u64)>, // hash -> (offset, length)
    entries: Vec<Entry>,
    addressed: u64,
    last_id: Option<u64>,
}

impl PmtilesWriter {
    pub fn new(tmp_dir: &Path) -> PmResult<PmtilesWriter> {
        let tmp_path = tmp_dir.join(format!("ts_pmtiles_data_{}.tmp", std::process::id()));
        let data = BufWriter::new(
            File::create(&tmp_path).map_err(|e| format!("create temp {tmp_path:?}: {e}"))?,
        );
        Ok(PmtilesWriter {
            tmp_path,
            data,
            data_len: 0,
            dedup: std::collections::HashMap::new(),
            entries: Vec::new(),
            addressed: 0,
            last_id: None,
        })
    }

    /// Add one already-gzip'd tile at `tile_id`. MUST be called in strictly ascending tile_id order.
    pub fn add(&mut self, tile_id: u64, gzipped_tile: Vec<u8>) -> PmResult<()> {
        if let Some(prev) = self.last_id {
            if tile_id <= prev {
                return Err(format!(
                    "pmtiles writer: tile_id {tile_id} not ascending (prev {prev})"
                ));
            }
        }
        self.last_id = Some(tile_id);
        self.addressed += 1;
        let h = fnv1a64(&gzipped_tile);
        let (offset, length) = match self.dedup.get(&h) {
            Some(&(off, len)) => (off, len),
            None => {
                let off = self.data_len;
                let len = gzipped_tile.len() as u64;
                self.data
                    .write_all(&gzipped_tile)
                    .map_err(|e| format!("write temp: {e}"))?;
                self.data_len += len;
                self.dedup.insert(h, (off, len));
                (off, len)
            }
        };
        // RLE: extend the previous entry when this tile is the immediate next id AND same blob.
        if let Some(last) = self.entries.last_mut() {
            if last.tile_id + last.run_length == tile_id && last.offset == offset {
                last.run_length += 1;
                return Ok(());
            }
        }
        self.entries.push(Entry {
            tile_id,
            offset,
            length,
            run_length: 1,
        });
        Ok(())
    }

    pub fn finish(
        mut self,
        hf: HeaderFields,
        metadata_json: &str,
        out_path: &Path,
    ) -> PmResult<Counts> {
        self.data.flush().map_err(|e| format!("flush temp: {e}"))?;
        let (root, leaves) = build_roots_leaves(&self.entries);
        let metadata = gzip(metadata_json.as_bytes());
        let root_off = 127u64;
        let meta_off = root_off + root.len() as u64;
        let leaf_off = meta_off + metadata.len() as u64;
        let data_off = leaf_off + leaves.len() as u64;
        let header = Header {
            root_dir_offset: root_off,
            root_dir_length: root.len() as u64,
            metadata_offset: meta_off,
            metadata_length: metadata.len() as u64,
            leaf_dirs_offset: leaf_off,
            leaf_dirs_length: leaves.len() as u64,
            tile_data_offset: data_off,
            tile_data_length: self.data_len,
            num_addressed_tiles: self.addressed,
            num_tile_entries: self.entries.len() as u64,
            num_tile_contents: self.dedup.len() as u64,
            clustered: 1,
            internal_compression: 2,
            tile_compression: 2,
            tile_type: 1,
            min_zoom: hf.min_zoom,
            max_zoom: hf.max_zoom,
            min_lon_e7: hf.bounds_e7[0],
            min_lat_e7: hf.bounds_e7[1],
            max_lon_e7: hf.bounds_e7[2],
            max_lat_e7: hf.bounds_e7[3],
            center_zoom: hf.center.0,
            center_lon_e7: hf.center.1,
            center_lat_e7: hf.center.2,
        };
        // Assemble into out_path.tmp then rename (atomic).
        let tmp_out = out_path.with_extension("pmtiles.tmp");
        let assemble = || -> PmResult<()> {
            let mut f = BufWriter::new(
                File::create(&tmp_out).map_err(|e| format!("create out {tmp_out:?}: {e}"))?,
            );
            f.write_all(&write_header(&header))
                .map_err(|e| format!("write header: {e}"))?;
            f.write_all(&root).map_err(|e| format!("write root: {e}"))?;
            f.write_all(&metadata)
                .map_err(|e| format!("write meta: {e}"))?;
            f.write_all(&leaves)
                .map_err(|e| format!("write leaves: {e}"))?;
            // stream the temp data section
            let mut data_in =
                File::open(&self.tmp_path).map_err(|e| format!("reopen temp: {e}"))?;
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = data_in
                    .read(&mut buf)
                    .map_err(|e| format!("read temp: {e}"))?;
                if n == 0 {
                    break;
                }
                f.write_all(&buf[..n])
                    .map_err(|e| format!("copy data: {e}"))?;
            }
            f.flush().map_err(|e| format!("flush out: {e}"))?;
            Ok(())
        };
        if let Err(e) = assemble() {
            std::fs::remove_file(&tmp_out).ok();
            return Err(e);
        }
        std::fs::rename(&tmp_out, out_path).map_err(|e| format!("rename out: {e}"))?;
        std::fs::remove_file(&self.tmp_path).ok();
        let total = data_off + self.data_len;
        Ok(Counts {
            addressed: self.addressed,
            entries: self.entries.len() as u64,
            contents: self.dedup.len() as u64,
            bytes: total,
        })
    }
}

impl Drop for PmtilesWriter {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.tmp_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::pmtiles::codec::gzip;
    use crate::vector::pmtiles::read::PmtilesReader;
    use crate::vector::pmtiles::zxy_to_tileid;

    #[test]
    fn writer_dedups_rle_and_reads_back() {
        // Dedicated subdir (NOT bare temp_dir): the writer's scratch is `ts_pmtiles_data_<pid>.tmp`,
        // keyed on pid alone, so a writer built on the shared temp dir collides with another test's
        // writer running concurrently in this binary (e.g. overlay's compaction_merges test). Isolate.
        let tmp = std::env::temp_dir().join(format!(
            "ts_pmt_dedup_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let out = tmp.join("dedup_out.pmtiles");
        let mut w = PmtilesWriter::new(&tmp).unwrap();
        // Three consecutive tiles at z1: two identical (RLE + dedup), one distinct.
        let same = gzip(b"AAAA");
        let diff = gzip(b"BBBB");
        // z1 ids: (0,0)=1,(0,1)=2,(1,1)=3 — add in ascending id order.
        w.add(zxy_to_tileid(1, 0, 0), same.clone()).unwrap();
        w.add(zxy_to_tileid(1, 0, 1), same.clone()).unwrap();
        w.add(zxy_to_tileid(1, 1, 1), diff.clone()).unwrap();
        let hf = HeaderFields {
            min_zoom: 1,
            max_zoom: 1,
            bounds_e7: [0, 0, 0, 0],
            center: (1, 0, 0),
        };
        let counts = w.finish(hf, r#"{"vector_layers":[]}"#, &out).unwrap();
        assert_eq!(counts.addressed, 3);
        assert_eq!(counts.contents, 2, "two identical tiles dedup to one blob");
        assert_eq!(
            counts.entries, 2,
            "the two consecutive identical tiles RLE-collapse to one entry"
        );
        let r = PmtilesReader::open(&out).unwrap();
        assert_eq!(r.get(1, 0, 0).unwrap(), Some(b"AAAA".to_vec()));
        assert_eq!(r.get(1, 0, 1).unwrap(), Some(b"AAAA".to_vec())); // covered by the run
        assert_eq!(r.get(1, 1, 1).unwrap(), Some(b"BBBB".to_vec()));
        std::fs::remove_file(&out).ok();
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Strictly-ascending tile_ids for the spill test: deltas drawn from a wide pseudo-random
    /// (xorshift64) range, cumulatively summed. A pure function of `n` (always returns the first
    /// `n` ids of the same deterministic sequence) so write time and verify time agree.
    ///
    /// Irregular deltas matter: `build_roots_leaves` spills only once the gzip'd columnar
    /// directory exceeds 16 KiB, and gzip is very good at crushing regular columns. A dense
    /// `id = k` sequence has a constant delta of 1 for all 200_000 entries; combined with the
    /// writer packing tile blobs back-to-back (which collapses the `offset` column to the
    /// codec's contiguous-offset sentinel regardless of content) and near-identical small blobs
    /// (which collapse `length` too), an earlier version of this test compressed the WHOLE
    /// 200_000-entry root to under 1 KB and never spilled. Wide pseudo-random deltas give the
    /// `tile_id` column genuine entropy so the root can't be crushed that far, while blobs stay
    /// tiny (fast to generate/gzip/write).
    fn spill_test_ids(n: u64) -> Vec<u64> {
        let mut s = 0x2545_F491_4F6C_DD1Du64;
        let mut id = 0u64;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                id += 1 + (s % 65_535); // delta in [1, 65535]
                id
            })
            .collect()
    }

    #[test]
    fn writer_spills_to_leaves_and_reads_back() {
        // Drive the REAL PmtilesWriter through enough ascending-tile_id `add()` calls to force
        // `build_roots_leaves` to spill the directory into leaves (the codec's own large-dir test
        // proves ~200_000 distinct entries can spill; mirror that count here end-to-end through
        // the writer, not a hand-built directory). See `spill_test_ids` for why the ids must be
        // irregular, not dense.
        const N: u64 = 200_000;
        // A dedicated subdirectory, NOT bare `std::env::temp_dir()`: `PmtilesWriter`'s streamed
        // data temp file is named only `ts_pmtiles_data_<pid>.tmp` (keyed on the process id, not
        // per-instance), so two writers built directly on the shared temp dir in the same test
        // binary (this test running concurrently with e.g. `writer_dedups_rle_and_reads_back`)
        // would collide on that filename.
        let tmp = std::env::temp_dir().join(format!("ts_pmt_spill_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let out = tmp.join("spill_out.pmtiles");
        let ids = spill_test_ids(N);
        let mut w = PmtilesWriter::new(&tmp).unwrap();
        for (k, &tile_id) in ids.iter().enumerate() {
            // Each tile a small DISTINCT blob (keyed off `k`) so entries don't dedup/RLE-collapse.
            let blob = gzip(&(k as u64).to_le_bytes());
            w.add(tile_id, blob).unwrap();
        }
        let hf = HeaderFields {
            min_zoom: 0,
            max_zoom: 22,
            bounds_e7: [0, 0, 0, 0],
            center: (0, 0, 0),
        };
        let counts = w.finish(hf, r#"{"vector_layers":[]}"#, &out).unwrap();
        assert_eq!(counts.addressed, N);
        assert_eq!(counts.entries, N, "distinct blobs -> no RLE collapse");

        // Confirm the directory actually spilled: read the 127-byte header straight off disk and
        // check leaf_dirs_length > 0 -- the definitive signal (PmtilesReader keeps its parsed
        // header private, so this reads the bytes directly via the same codec the writer used).
        let mut head_buf = [0u8; 127];
        {
            let mut f = File::open(&out).unwrap();
            f.read_exact(&mut head_buf).unwrap();
        }
        let header = crate::vector::pmtiles::codec::read_header(&head_buf).unwrap();
        assert!(
            header.leaf_dirs_length > 0,
            "expected {N} distinct entries to spill the directory into leaves"
        );

        let r = PmtilesReader::open(&out).unwrap();
        // A spread of ids across the full range -- necessarily exercises leaf descent for any id
        // that isn't root-resident, since the directory spilled.
        for &k in &[
            0usize,
            1,
            (N / 4) as usize,
            (N / 2) as usize,
            (3 * N / 4) as usize,
            (N - 1) as usize,
        ] {
            let (z, x, y) = crate::vector::pmtiles::tileid_to_zxy(ids[k]);
            let got = r.get(z, x, y).unwrap();
            assert_eq!(
                got,
                Some((k as u64).to_le_bytes().to_vec()),
                "tile_id {} (k={k}) round-trip mismatch",
                ids[k]
            );
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    fn no_tmp_files_in(dir: &Path) -> bool {
        std::fs::read_dir(dir).unwrap().all(|e| {
            let name = e.unwrap().file_name();
            !name.to_string_lossy().ends_with(".tmp")
        })
    }

    #[test]
    fn temp_files_cleaned_up() {
        let dir = std::env::temp_dir().join(format!("ts_pmt_cleanup_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Case A: writer dropped mid-build, without calling finish() — the streamed
        // data temp file must not survive the drop.
        {
            let mut w = PmtilesWriter::new(&dir).unwrap();
            w.add(zxy_to_tileid(1, 0, 0), gzip(b"x")).unwrap();
        }
        assert!(
            no_tmp_files_in(&dir),
            "temp file left behind after drop without finish()"
        );

        // Case B: successful finish() — output exists, no *.tmp remnants remain.
        let out = dir.join("cleanup_out.pmtiles");
        {
            let mut w = PmtilesWriter::new(&dir).unwrap();
            w.add(zxy_to_tileid(1, 0, 0), gzip(b"a")).unwrap();
            w.add(zxy_to_tileid(1, 1, 1), gzip(b"b")).unwrap();
            let hf = HeaderFields {
                min_zoom: 1,
                max_zoom: 1,
                bounds_e7: [0, 0, 0, 0],
                center: (1, 0, 0),
            };
            w.finish(hf, r#"{"vector_layers":[]}"#, &out).unwrap();
        }
        assert!(out.exists(), "finish() did not produce the output file");
        assert!(
            no_tmp_files_in(&dir),
            "temp file left behind after successful finish()"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
