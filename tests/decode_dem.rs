//! Polar DEM decode guard: real ArcticDEM/REMA tiles are **LZW + float-predictor + Float32**.
//! Decoding one tile must yield plausible *elevations* (not garbage), proving the LZW codec, the
//! TIFF floating-point predictor (predictor=3), and the Float32 sample arm all work end-to-end
//! against real data. A wrong float-predictor produces huge/NaN garbage — this catches it.
//!
//! Uses the local polar fixtures (cogs/polar/); skips if absent (lean checkout / CI).

use std::sync::Arc;

use terraserve::backend::{CompressedTile, Resample};
use terraserve::cog::{self, LocalFileRangeSource, RangeSource};
use terraserve::decode;
use terraserve::expr;
use terraserve::render::{self, BandMath, RenderRequest};
use terraserve::style::Style;

// (path, EPSG) — ArcticDEM is EPSG:3413, REMA is EPSG:3031; both LZW + predictor=3 + Float32.
const DEMS: &[&str] = &[
    "../cogs/polar/arcticdem_18_47_32m_gunnbjorn_dem.tif",
    "../cogs/polar/rema_42_07_32m_antarctic_peninsula_dem.tif",
];

fn decode_center_tile(path: &str) -> Option<Vec<f32>> {
    if !std::path::Path::new(path).exists() {
        eprintln!("skipping {path}: fixture absent");
        return None;
    }
    let src = LocalFileRangeSource::open(path).unwrap();
    let cog = cog::parse(&src).unwrap();
    let level = &cog.levels[0];

    // Sanity: these fixtures really are LZW (5) + float predictor (3) + Float32 (fmt 3, 32-bit).
    assert_eq!(level.compression, 5, "{path}: expected LZW");
    assert_eq!(level.predictor, 3, "{path}: expected float predictor");
    assert_eq!(level.sample_format, 3, "{path}: expected IEEE float");
    assert_eq!(level.bits_per_sample, 32, "{path}: expected 32-bit");

    // A center-ish tile (real mountain data, not an all-nodata corner).
    let across = level.width.div_ceil(level.tile_w);
    let down = level.height.div_ceil(level.tile_h);
    let (gx, gy) = (across / 2, down / 2);
    let idx = (gy * across + gx) as usize;
    let index_cache = terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes());
    let (off, len) = level
        .tile_location(&src, &index_cache, idx as u64)
        .unwrap()
        .expect("tile present");
    let bytes = src.read_range(off, len).unwrap();
    let ct = CompressedTile {
        bytes,
        compression: level.compression,
        predictor: level.predictor,
        tile_w: level.tile_w,
        tile_h: level.tile_h,
        samples: level.samples_per_pixel,
        bits_per_sample: level.bits_per_sample,
        sample_format: level.sample_format,
        little_endian: cog.little_endian,
        photometric: level.photometric,
        jpeg_tables: cog.jpeg_tables.as_ref().map(|t| Arc::new(t.clone())),
        grid_col: gx,
        grid_row: gy,
        present: true,
    };
    Some(
        decode::decode_tile_bands(&ct)
            .bands
            .into_iter()
            .next()
            .unwrap(),
    )
}

#[test]
fn polar_dem_decodes_to_plausible_elevations() {
    for path in DEMS {
        let Some(vals) = decode_center_tile(path) else {
            continue;
        };

        // Classify: nodata (−9999), plausible Earth elevation (−500..9000 m), or GARBAGE.
        let (mut nodata, mut elev, mut garbage) = (0usize, 0usize, 0usize);
        for &v in &vals {
            if !v.is_finite() || v.abs() > 100_000.0 {
                garbage += 1;
            } else if (v - (-9999.0)).abs() < 0.5 {
                nodata += 1;
            } else if (-500.0..=9000.0).contains(&v) {
                elev += 1;
            } else {
                garbage += 1; // finite but implausible for a DEM → a decode error
            }
        }
        eprintln!(
            "{path}: {} px — elev={elev} nodata={nodata} garbage={garbage}",
            vals.len()
        );
        assert_eq!(
            garbage, 0,
            "{path}: {garbage} garbage values — LZW/float-predictor decode is wrong"
        );
        assert!(
            elev > 0,
            "{path}: no plausible elevation pixels — decode produced only nodata/zeros"
        );
    }
}

// (path, native CRS) — the full render path, DEM → colorized elevation tile.
const DEM_LAYERS: &[(&str, &str)] = &[
    (
        "../cogs/polar/arcticdem_18_47_32m_gunnbjorn_dem.tif",
        "EPSG:3413",
    ),
    (
        "../cogs/polar/rema_42_07_32m_antarctic_peninsula_dem.tif",
        "EPSG:3031",
    ),
];

#[test]
fn polar_dem_renders_colorized_elevation() {
    let style = match Style::load("fixtures/styles/dem.json") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skipping: dem.json missing ({e})");
            return;
        }
    };
    // Single-band DEM rendered as band-math with an identity expression + the elevation ramp.
    let program = expr::Program::compile("elev", &["elev"]).unwrap();
    let bm = BandMath {
        program,
        nodata: -9999.0,
    };

    for (path, crs) in DEM_LAYERS {
        if !std::path::Path::new(path).exists() {
            eprintln!("skipping {path}: fixture absent");
            continue;
        }
        let src = LocalFileRangeSource::open(path).unwrap();
        let cog = cog::parse(&src).unwrap();
        let g = &cog.levels[0].geo;
        let (w, h) = (cog.levels[0].width as f64, cog.levels[0].height as f64);
        let bbox = [
            g.origin_x,
            g.origin_y - h * g.py,
            g.origin_x + w * g.px,
            g.origin_y,
        ];
        let req = RenderRequest {
            cog_path: path,
            bbox,
            crs,
            src_crs: crs,
            width: 256,
            height: 256,
            resample: Resample::Nearest,
            style: &style,
            band_math: Some(&bm),
            index_cache: terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes()),
        };
        let rgba = render::render_with_cog(&req, &cog, &src, None).unwrap();

        // A real hypsometric render: many distinct colours, and opaque where there's data.
        let mut colors = std::collections::HashSet::new();
        let mut opaque = 0usize;
        for px in rgba.chunks_exact(4) {
            colors.insert([px[0], px[1], px[2], px[3]]);
            if px[3] == 255 {
                opaque += 1;
            }
        }
        eprintln!(
            "{path}: {} distinct colours, {opaque} opaque px",
            colors.len()
        );
        assert!(
            colors.len() > 50,
            "{path}: only {} colours — DEM did not colorize",
            colors.len()
        );
        assert!(opaque > 0, "{path}: no opaque data pixels rendered");
    }
}
