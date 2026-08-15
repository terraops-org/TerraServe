// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Offline PMTiles v3 pyramid generator: drive `encode_tile_opt` over a WebMercatorQuad grid in
//! Hilbert-TileID order per zoom, gzip each tile, and feed the dedup + RLE-collapsing streaming
//! writer (Task 5). Empty (fully-clipped / featureless) tiles are omitted entirely.

use super::write::{Counts, HeaderFields, PmtilesWriter};
use super::{codec::gzip, zxy_to_tileid, PmResult};
use crate::reproj;
use crate::server::Layer;
use crate::tms::TileMatrixSet;
use crate::vector::mvt::opts::MvtOptimizations;
use crate::vector::mvt::tile::{encode_tile_opt, features_for_tile};
use rayon::prelude::*;
use std::path::Path;

/// Tiles per parallel render batch — bounds peak RAM (one gzip'd MVT per in-flight tile) while
/// keeping the writer's strictly-ascending `add` order intact (each batch is drained in id order).
const BATCH: usize = 1024;

/// Build a `.pmtiles` pyramid for `layer` over `grid` (WebMercatorQuad) across `[min_zoom, max_zoom]`,
/// clipped to `bbox_wgs84`. Tiles are rendered with `opts` — the SAME `MvtOptimizations` the live
/// `/mvt` + WMTS routes use — so archived bytes match a live render. Returns dedup/RLE counts.
pub fn build_pmtiles(
    layer: &Layer,
    opts: &MvtOptimizations,
    grid: &TileMatrixSet,
    min_zoom: u8,
    max_zoom: u8,
    bbox_wgs84: [f64; 4],
    out_path: &Path,
    tmp_dir: &Path,
) -> PmResult<Counts> {
    let v = layer
        .vector
        .as_ref()
        .ok_or("build_pmtiles: not a vector layer")?;
    // Reproject the WGS84 bbox into the GRID's CRS so `tile_limits` (which works in grid units) is
    // correct for ANY grid, not just EPSG:3857. WebMercator → the same 4326->3857 as before
    // (byte-identical); swissLV95 → 4326->2056; WorldCRS84Quad → 4326->4326 (identity).
    let bbox_grid = reproj::crs_bounds(
        "EPSG:4326",
        &grid.crs,
        bbox_wgs84[0],
        bbox_wgs84[1],
        bbox_wgs84[2],
        bbox_wgs84[3],
    )
    .ok_or_else(|| format!("build_pmtiles: cannot reproject bounds to {}", grid.crs))?;
    let mut w = PmtilesWriter::new(tmp_dir)?;
    for z in (min_zoom as u32)..=(max_zoom as u32) {
        let Some((c0, c1, r0, r1)) = grid.tile_limits(bbox_grid, z) else {
            continue;
        };
        // The PMTiles Hilbert TileID (`zxy_to_tileid`) addresses a SQUARE 2^z x 2^z quad, so a tile
        // col/row at or beyond 2^z aliases another tile's id (surfaces as a "tile_id not ascending"
        // panic in the writer). Square grids (WebMercatorQuad) and custom grids whose
        // matrixWidth/Height stay below 2^z (e.g. swissLV95) are fine; a 2:1 grid like
        // WorldCRS84Quad (matrixWidth = 2^(z+1)) is not — reject it up front with a clear message
        // rather than panicking mid-bake. (A non-square TileID scheme is a possible follow-up.)
        if (c1 as u64) >= (1u64 << z) || (r1 as u64) >= (1u64 << z) {
            return Err(format!(
                "build-pmtiles: grid '{}' at z{z} reaches tile col/row ({c1},{r1}), at or beyond \
                 2^{z} = {} — its tile matrix is not a square 2^z quad (e.g. WorldCRS84Quad is 2:1), \
                 which the PMTiles Hilbert TileID cannot address uniquely. Use WebMercatorQuad or a \
                 custom grid whose matrixWidth/Height stay below 2^z (like swissLV95).",
                grid.id,
                1u64 << z
            )
            .into());
        }
        // Per-zoom LOD: pick the zoom-appropriate feature pool (as the live routes do).
        let vs = v.source_for_zoom(z);
        // Enumerate the covered tiles and sort by Hilbert TileID — the writer requires strictly
        // ascending ids, and this is also the PMTiles clustered on-disk order.
        let mut ids: Vec<(u64, u32, u32)> = Vec::new();
        for x in c0..=c1 {
            for y in r0..=r1 {
                ids.push((zxy_to_tileid(z, x, y), x, y));
            }
        }
        ids.sort_by_key(|t| t.0);
        for tile_batch in ids.chunks(BATCH) {
            let rendered: Vec<(u64, crate::vector::pmtiles::PmResult<Vec<u8>>)> = tile_batch
                .par_iter()
                .map(|&(id, x, y)| {
                    // Retry a failed SOURCE READ, then abort. An `Err` from `features_for_tile` is
                    // a broken query, NOT an empty region — and an empty MVT tile is `continue`d
                    // below, so without this the failure would be indistinguishable from empty
                    // ground and silently omitted from the archive.
                    let mvt = crate::vector::pmtiles::build_tile_with_retry(z, x, y, || {
                        // Reads through the `VectorSource` seam: reproject the tile bbox into the
                        // source CRS before reading — a no-op for `LoadAll`, correct for windowed.
                        let feats = features_for_tile(&vs, grid, z, x, y, &layer.src_crs, &opts)?;
                        Ok(encode_tile_opt(
                            feats.as_slice(),
                            grid,
                            z,
                            x,
                            y,
                            &layer.src_crs,
                            &layer.name,
                            opts,
                        ))
                    });
                    (id, mvt)
                })
                .collect();
            for (id, mvt) in rendered {
                let mvt = mvt?;
                if mvt.is_empty() {
                    continue; // omit empty tiles entirely (no address, no blob)
                }
                w.add(id, gzip(&mvt))?;
            }
        }
    }
    let e7 = |d: f64| (d * 1e7) as i32;
    let hf = HeaderFields {
        min_zoom,
        max_zoom,
        bounds_e7: [
            e7(bbox_wgs84[0]),
            e7(bbox_wgs84[1]),
            e7(bbox_wgs84[2]),
            e7(bbox_wgs84[3]),
        ],
        center: (
            min_zoom,
            e7((bbox_wgs84[0] + bbox_wgs84[2]) / 2.0),
            e7((bbox_wgs84[1] + bbox_wgs84[3]) / 2.0),
        ),
    };
    // Reuse the layer's TileJSON metadata (vector_layers) — the same shape the /mvt route serves —
    // and stamp the grid this pyramid was baked on so serve reads it only for matching-grid requests.
    let metadata = crate::mvt_http::pmtiles_metadata_json(layer, Some(&grid.id));
    w.finish(hf, &metadata, out_path)
}
