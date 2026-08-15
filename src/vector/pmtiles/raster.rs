// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Offline PMTiles v3 pyramid of **PNG** tiles for a vector layer — the raster twin of
//! `generate.rs`, which bakes MVT.
//!
//! Why it exists: WMTS/TMS can raster a vector layer, but every tile was rendered live, every time.
//! Poland OSM buildings (17.8 M features) cost seconds per low-zoom tile with no cache anywhere on
//! that path, which makes a panning QGIS session unusable at country scale. This bakes those zooms
//! once so serving them is a disk read.
//!
//! **One renderer, still.** Each tile is produced by `server::VectorLayer::render_tile`, the same
//! call `wmts::get_tile` and `tms_http::render_tms_tile` make on a miss — so the per-zoom LOD
//! selection and the feature-cap warning apply here exactly as they do live, and a baked tile
//! cannot drift from the one it replaces. A "bake" variant of the renderer would defeat the whole
//! point of precomputing.
//!
//! Unlike the MVT generator, a blank tile is NOT omitted: a PNG is always produced, and the
//! writer's content dedup + RLE collapse fold every identical transparent tile into one blob and
//! one directory entry. Keeping them addressed means a request over empty ground is a disk read
//! rather than a live (for PostGIS, a real round-trip) render of nothing.

use super::write::{Counts, HeaderFields, PmtilesWriter, COMPRESSION_NONE, TILE_TYPE_PNG};
use super::{zxy_to_tileid, PmResult};
use crate::reproj;
use crate::server::Layer;
use crate::tms::TileMatrixSet;
use rayon::prelude::*;
use std::path::Path;

/// Tiles per parallel render batch — bounds peak RAM (one rendered PNG per in-flight tile) while
/// keeping the writer's strictly-ascending `add` order intact. Smaller than the MVT generator's
/// 1024 because a low-zoom raster tile of a dense layer holds millions of features in flight.
const BATCH: usize = 64;

/// The metadata JSON stamped into a raster archive. Deliberately NOT
/// `mvt_http::pmtiles_metadata_json`: a PNG archive has no `vector_layers` (there are no attributes
/// in a picture), and it must carry one thing an MVT archive does not — the tile PIXEL size it was
/// baked at. A 256-px archive served on a 512-px grid would hand the client an image of the wrong
/// size for the ground it covers, so serve checks this at startup (`layer::build_vector_layer`) and
/// refuses the pair rather than drawing a quarter-size map.
pub fn raster_metadata_json(layer_name: &str, grid_id: &str, tile_px: u32) -> String {
    serde_json::json!({
        "name": layer_name,
        "format": "png",
        "grid_id": grid_id,
        "tile_px": tile_px,
    })
    .to_string()
}

/// Build a PNG `.pmtiles` pyramid for `layer` over `grid` across `[min_zoom, max_zoom]`, clipped to
/// `bbox_wgs84`. The archive declares `tile_type` 2 (PNG) and `tile_compression` 1 (none) — PNG is
/// already compressed, and an archive that lies about either breaks every other PMTiles reader.
pub fn build_raster_pmtiles(
    layer: &Layer,
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
        .ok_or("build_raster_pmtiles: not a vector layer")?;
    // The WGS84 bbox in the GRID's CRS, so `tile_limits` (which works in grid units) is correct for
    // any grid, not just EPSG:3857 — same reprojection the MVT generator does.
    let bbox_grid = reproj::crs_bounds(
        "EPSG:4326",
        &grid.crs,
        bbox_wgs84[0],
        bbox_wgs84[1],
        bbox_wgs84[2],
        bbox_wgs84[3],
    )
    .ok_or_else(|| {
        format!(
            "build_raster_pmtiles: cannot reproject bounds to {}",
            grid.crs
        )
    })?;
    let mut w = PmtilesWriter::new(tmp_dir)?.tile_format(TILE_TYPE_PNG, COMPRESSION_NONE);
    for z in (min_zoom as u32)..=(max_zoom as u32) {
        let Some((c0, c1, r0, r1)) = grid.tile_limits(bbox_grid, z) else {
            continue;
        };
        // The PMTiles Hilbert TileID addresses a SQUARE 2^z x 2^z quad; a 2:1 grid like
        // WorldCRS84Quad aliases two tiles onto one id. Reject up front with the same message the
        // MVT generator gives rather than panicking mid-bake in the writer's ascending-id check.
        if (c1 as u64) >= (1u64 << z) || (r1 as u64) >= (1u64 << z) {
            return Err(format!(
                "build-pmtiles: grid '{}' at z{z} reaches tile col/row ({c1},{r1}), at or beyond \
                 2^{z} = {} — its tile matrix is not a square 2^z quad (e.g. WorldCRS84Quad is 2:1), \
                 which the PMTiles Hilbert TileID cannot address uniquely. Use WebMercatorQuad or a \
                 custom grid whose matrixWidth/Height stay below 2^z (like swissLV95).",
                grid.id,
                1u64 << z
            ));
        }
        let mut ids: Vec<(u64, u32, u32)> = Vec::new();
        for x in c0..=c1 {
            for y in r0..=r1 {
                ids.push((zxy_to_tileid(z, x, y), x, y));
            }
        }
        ids.sort_by_key(|t| t.0);
        let started = std::time::Instant::now();
        let total = ids.len();
        for tile_batch in ids.chunks(BATCH) {
            let rendered: Vec<(u64, PmResult<Vec<u8>>)> = tile_batch
                .par_iter()
                .map(|&(id, x, y)| {
                    // THE shared renderer — same call the live WMTS/TMS miss path makes, so LOD
                    // selection and the feature-cap warning behave identically here. Wrapped in
                    // the retry so a transient source read does not abort a long bake, and a
                    // persistent one aborts it instead of freezing a blank tile into the archive.
                    let png = crate::vector::pmtiles::build_tile_with_retry(z, x, y, || {
                        v.render_tile(&layer.src_crs, grid, z, x, y)
                    });
                    (id, png)
                })
                .collect();
            for (id, png) in rendered {
                w.add(id, png?)?;
            }
        }
        // A country-scale bake runs for minutes; say which zoom is in flight rather than going
        // silent (the MVT bake that "reported success while doing nothing" is why this path is
        // noisy on purpose).
        eprintln!(
            "build-pmtiles: z{z} · {total} tiles · {:.1}s",
            started.elapsed().as_secs_f64()
        );
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
    let metadata = raster_metadata_json(&layer.name, &grid.id, grid.tile_w);
    w.finish(hf, &metadata, out_path)
}
