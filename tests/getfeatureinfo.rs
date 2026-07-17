//! GetFeatureInfo value-read core (`render::sample_point_with_cog`): the reported value must be the
//! EXACT native source pixel — this test guards the two failure modes (a half-pixel shift → wrong
//! value; reading a downsampled overview → an averaged value). Skips if the polar fixture is absent.

use std::sync::Arc;

use terraserve::backend::CompressedTile;
use terraserve::cog::{self, LocalFileRangeSource, RangeSource};
use terraserve::decode;
use terraserve::render::{sample_point_with_cog, InfoRequest};

const PATH: &str = "../cogs/polar/arcticdem_18_47_32m_gunnbjorn_dem.tif";

fn load() -> Option<(cog::Cog, LocalFileRangeSource)> {
    if !std::path::Path::new(PATH).exists() {
        eprintln!("skipping: polar fixture absent");
        return None;
    }
    let src = LocalFileRangeSource::open(PATH).unwrap();
    let cog = cog::parse(&src).unwrap();
    Some((cog, src))
}

/// Full native extent bbox in the source CRS (identity transform: crs == src_crs).
fn native_bbox(cog: &cog::Cog) -> ([f64; 4], u32, u32) {
    let l = &cog.levels[0];
    let g = l.geo;
    let bbox = [
        g.origin_x,
        g.origin_y - l.height as f64 * g.py,
        g.origin_x + l.width as f64 * g.px,
        g.origin_y,
    ];
    (bbox, l.width, l.height)
}

fn req<'a>(bbox: [f64; 4], width: u32, height: u32, i: u32, j: u32) -> InfoRequest<'a> {
    InfoRequest {
        bbox,
        crs: "EPSG:3413",
        src_crs: "EPSG:3413",
        width,
        height,
        i,
        j,
        band_math: None,
    }
}

#[test]
fn pixel_mapping_is_shift_exact() {
    let Some((cog, src)) = load() else { return };
    let index_cache = terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes());
    let (bbox, w, h) = native_bbox(&cog);
    // At a 1:1 map (width==native width), pixel (i,j) must map to source pixel exactly (i,j).
    for &(i, j) in &[(0u32, 0u32), (1000, 1500), (w / 2, h / 2), (w - 1, h - 1)] {
        let info = sample_point_with_cog(&req(bbox, w, h, i, j), &cog, &src, &index_cache).unwrap();
        assert!(info.in_image, "({i},{j}) should be in image");
        assert_eq!(info.source_col, i as i64, "col shift at ({i},{j})");
        assert_eq!(info.source_row, j as i64, "row shift at ({i},{j})");
    }
}

#[test]
fn value_equals_independent_tile_decode() {
    let Some((cog, src)) = load() else { return };
    let index_cache = terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes());
    let (bbox, w, h) = native_bbox(&cog);
    let (i, j) = (w / 2, h / 2);
    let info = sample_point_with_cog(&req(bbox, w, h, i, j), &cog, &src, &index_cache).unwrap();

    // Independently decode the tile containing (i,j) and read the same pixel from its band plane.
    let l = &cog.levels[0];
    let across = l.width.div_ceil(l.tile_w);
    let (gx, gy) = (i / l.tile_w, j / l.tile_h);
    let idx = (gy * across + gx) as usize;
    let (off, len) = l
        .tile_location(&src, &index_cache, idx as u64)
        .unwrap()
        .expect("tile present");
    let bytes = src.read_range(off, len).unwrap();
    let ct = CompressedTile {
        bytes,
        compression: l.compression,
        predictor: l.predictor,
        tile_w: l.tile_w,
        tile_h: l.tile_h,
        samples: l.samples_per_pixel,
        bits_per_sample: l.bits_per_sample,
        sample_format: l.sample_format,
        little_endian: cog.little_endian,
        photometric: l.photometric,
        jpeg_tables: cog.jpeg_tables.as_ref().map(|t| Arc::new(t.clone())),
        grid_col: gx,
        grid_row: gy,
        present: true,
    };
    let plane = &decode::decode_tile_bands(&ct).bands[0];
    let local = (j % l.tile_h) as usize * l.tile_w as usize + (i % l.tile_w) as usize;
    assert!(
        (info.bands[0] - plane[local] as f64).abs() < 1e-6,
        "sample_point value {} != independent decode {}",
        info.bands[0],
        plane[local]
    );
}

#[test]
fn reads_native_resolution_not_an_overview() {
    let Some((cog, src)) = load() else { return };
    let index_cache = terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes());
    let (bbox, w, h) = native_bbox(&cog);
    // The SAME geo point (map center) at full res and at a 4x-downsampled view must resolve to the
    // same NATIVE source pixel (± rounding) — proving GFI reads level 0, not a coarse overview.
    let full =
        sample_point_with_cog(&req(bbox, w, h, w / 2, h / 2), &cog, &src, &index_cache).unwrap();
    let quarter = sample_point_with_cog(
        &req(bbox, w / 4, h / 4, w / 8, h / 8),
        &cog,
        &src,
        &index_cache,
    )
    .unwrap();
    assert!(full.in_image && quarter.in_image);
    assert!(
        (full.source_col - quarter.source_col).abs() <= 2,
        "not native-res: {} vs {}",
        full.source_col,
        quarter.source_col
    );
    assert!((full.source_row - quarter.source_row).abs() <= 2);
    // The downsampled view's source_col is a NATIVE index (~w/2), not a small-grid index.
    assert!(quarter.source_col as u32 > w / 4);
}

#[test]
fn point_outside_extent_is_not_in_image() {
    let Some((cog, src)) = load() else { return };
    let index_cache = terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes());
    let (bbox, _, _) = native_bbox(&cog);
    // A tiny map far to the west of the data.
    let far = [bbox[0] - 1_000_000.0, bbox[1], bbox[0] - 900_000.0, bbox[3]];
    let info = sample_point_with_cog(&req(far, 16, 16, 8, 8), &cog, &src, &index_cache).unwrap();
    assert!(!info.in_image);
}
