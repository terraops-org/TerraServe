// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Re-encode an MVT archive's tiles into another compression WITHOUT re-baking it.
//!
//! A bake re-reads the source and re-runs every gate, band and cap; that is hours for a PostGIS
//! layer and can change the tiles. Changing only the compression needs none of it: each stored
//! blob is decompressed with the header's encoding and compressed with the new one, so the
//! decoded MVT is byte-identical by construction, and `verify` proves it tile by tile.
//!
//! The header fields and metadata are copied; directories stay gzip (the writer's default, and
//! what every PMTiles reader supports). Identical blobs still dedup and adjacent runs still
//! collapse, because the writer does both on whatever it is handed.

use rayon::prelude::*;
use std::path::Path;

use super::encoding::{Effort, TileEncoding};
use super::read::PmtilesReader;
use super::write::{Counts, HeaderFields, PmtilesWriter, TILE_TYPE_MVT};
use super::PmResult;

/// Entries transcoded per parallel batch: bounds RAM to one batch of blobs.
const BATCH: usize = 1024;

pub fn recompress_pmtiles(
    input: &Path,
    out: &Path,
    tmp_dir: &Path,
    to: TileEncoding,
    level: Option<i32>,
) -> PmResult<Counts> {
    if to == TileEncoding::Identity {
        return Err(
            "pmtiles-recompress: an MVT archive must be compressed (gzip, br or zstd)".into(),
        );
    }
    let r = PmtilesReader::open(input)?;
    let h = r.header();
    if h.tile_type != TILE_TYPE_MVT {
        return Err(format!(
            "pmtiles-recompress: {input:?} holds tile_type {} ({}), only MVT archives are re-encoded \
             (a PNG archive is already compressed)",
            h.tile_type,
            super::write::tile_type_name(h.tile_type)
        ));
    }
    let from = TileEncoding::from_pmtiles(h.tile_compression)?;
    let entries = r.tile_entries()?;
    let mut w = PmtilesWriter::new(tmp_dir)?.tile_format(TILE_TYPE_MVT, to.to_pmtiles());
    for batch in entries.chunks(BATCH) {
        let blobs: Vec<PmResult<Vec<u8>>> = batch
            .par_iter()
            .map(|e| {
                let stored = r.entry_blob(e)?;
                if stored.is_empty() {
                    return Ok(stored);
                }
                let raw = from.decompress(&stored)?;
                Ok(to.compress(&raw, Effort::Archive, level))
            })
            .collect();
        for (e, blob) in batch.iter().zip(blobs) {
            let blob = blob?;
            for k in 0..e.run_length {
                w.add(e.tile_id + k, blob.clone())?;
            }
        }
    }
    let hf = HeaderFields {
        min_zoom: h.min_zoom,
        max_zoom: h.max_zoom,
        bounds_e7: [h.min_lon_e7, h.min_lat_e7, h.max_lon_e7, h.max_lat_e7],
        center: (h.center_zoom, h.center_lon_e7, h.center_lat_e7),
    };
    w.finish(hf, r.metadata(), out)
}

/// Prove `out` holds the same tiles as `input`: same addressed tile ids, same metadata, same
/// zoom/bounds header fields, and every addressed tile's DECODED bytes equal. Returns the number
/// of addressed tiles compared.
pub fn verify_same_tiles(input: &Path, out: &Path) -> PmResult<u64> {
    let a = PmtilesReader::open(input)?;
    let b = PmtilesReader::open(out)?;
    let (ha, hb) = (a.header(), b.header());
    if (
        ha.min_zoom,
        ha.max_zoom,
        ha.min_lon_e7,
        ha.min_lat_e7,
        ha.max_lon_e7,
        ha.max_lat_e7,
    ) != (
        hb.min_zoom,
        hb.max_zoom,
        hb.min_lon_e7,
        hb.min_lat_e7,
        hb.max_lon_e7,
        hb.max_lat_e7,
    ) {
        return Err("verify: zoom range or bounds differ".into());
    }
    if a.metadata() != b.metadata() {
        return Err("verify: metadata differs".into());
    }
    if a.all_tile_ids()? != b.all_tile_ids()? {
        return Err("verify: the addressed tile ids differ".into());
    }
    let (ea, eb) = (
        TileEncoding::from_pmtiles(ha.tile_compression)?,
        TileEncoding::from_pmtiles(hb.tile_compression)?,
    );
    let entries = a.tile_entries()?;
    let n: u64 = entries.iter().map(|e| e.run_length).sum();
    entries.par_chunks(BATCH).try_for_each(|batch| {
        for e in batch {
            let sa = a.entry_blob(e)?;
            let da = if sa.is_empty() {
                sa
            } else {
                ea.decompress(&sa)?
            };
            // Every id of a run, not just its first: the output may split or join runs differently.
            for k in 0..e.run_length {
                let id = e.tile_id + k;
                let sb = b
                    .raw_tile_by_id(id)?
                    .ok_or_else(|| format!("verify: tile id {id} missing"))?;
                let db = if sb.is_empty() {
                    sb
                } else {
                    eb.decompress(&sb)?
                };
                if da != db {
                    return Err(format!("verify: tile id {id} decodes differently"));
                }
            }
        }
        Ok(())
    })?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::pmtiles::codec::gzip;
    use crate::vector::pmtiles::zxy_to_tileid;

    #[test]
    fn a_gzip_archive_recompresses_to_zstd_with_identical_tiles() {
        let dir = std::env::temp_dir().join(format!("ts_recompress_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.pmtiles");
        let dst = dir.join("dst.pmtiles");
        let tile = |i: u32| format!("tile {i} ").repeat(40 + i as usize).into_bytes();
        {
            let mut w = PmtilesWriter::new(&dir).unwrap();
            // z1: four tiles, two identical (dedup) and an empty one.
            w.add(zxy_to_tileid(1, 0, 0), gzip(&tile(1))).unwrap();
            w.add(zxy_to_tileid(1, 0, 1), gzip(&tile(1))).unwrap();
            w.add(zxy_to_tileid(1, 1, 1), Vec::new()).unwrap();
            w.add(zxy_to_tileid(1, 1, 0), gzip(&tile(3))).unwrap();
            w.finish(
                HeaderFields {
                    min_zoom: 1,
                    max_zoom: 1,
                    bounds_e7: [-10, -20, 30, 40],
                    center: (1, 5, 6),
                },
                r#"{"vector_layers":[{"id":"x"}]}"#,
                &src,
            )
            .unwrap_or_else(|_| panic!("ids must be ascending in this test"));
        }
        recompress_pmtiles(&src, &dst, &dir, TileEncoding::Zstd, None).unwrap();
        let r = PmtilesReader::open(&dst).unwrap();
        assert_eq!(r.tile_compression(), TileEncoding::Zstd.to_pmtiles());
        assert_eq!(r.header().internal_compression, 2, "directories stay gzip");
        assert_eq!(r.metadata(), r#"{"vector_layers":[{"id":"x"}]}"#);
        assert_eq!(r.header().center_lat_e7, 6);
        assert_eq!(verify_same_tiles(&src, &dst).unwrap(), 4);
        assert_eq!(r.get(1, 0, 0).unwrap().unwrap(), tile(1));
        // A corrupted copy must FAIL verification, or the check proves nothing.
        let bad = dir.join("bad.pmtiles");
        {
            let mut w = PmtilesWriter::new(&dir)
                .unwrap()
                .tile_format(TILE_TYPE_MVT, TileEncoding::Zstd.to_pmtiles());
            w.add(
                zxy_to_tileid(1, 0, 0),
                TileEncoding::Zstd.compress(&tile(1), Effort::Live, None),
            )
            .unwrap();
            w.add(
                zxy_to_tileid(1, 0, 1),
                TileEncoding::Zstd.compress(&tile(2), Effort::Live, None),
            )
            .unwrap();
            w.add(zxy_to_tileid(1, 1, 1), Vec::new()).unwrap();
            w.add(
                zxy_to_tileid(1, 1, 0),
                TileEncoding::Zstd.compress(&tile(3), Effort::Live, None),
            )
            .unwrap();
            w.finish(
                HeaderFields {
                    min_zoom: 1,
                    max_zoom: 1,
                    bounds_e7: [-10, -20, 30, 40],
                    center: (1, 5, 6),
                },
                r#"{"vector_layers":[{"id":"x"}]}"#,
                &bad,
            )
            .unwrap();
        }
        assert!(verify_same_tiles(&src, &bad).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
