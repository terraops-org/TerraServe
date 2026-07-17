//! Tier-1 georegistration guard — the cheap tier of the Step-0 P0 "identity render" oracle
//! (see `docs/step0-scenario-matrix-detailed.md`, Addendum).
//!
//! An IDENTITY render — same CRS, BBOX == a source tile's exact geo extent, WIDTH/HEIGHT ==
//! that tile's native pixel dims, nearest resampling, passthrough style — must reproduce that
//! tile's pixels. The tile is decoded DIRECTLY (independent of the warp path) as ground truth,
//! so this catches the GROSS georegistration regressions a golden-with-generous-tolerance would
//! miss: a flipped or transposed axis, a full-pixel offset, the wrong overview level, or a tile
//! mis-assembly.
//!
//! It deliberately does NOT catch a sub-pixel/half-pixel *convention* error: at exactly 1:1,
//! `floor(i + 0.5) == floor(i) == i`, so the correct pixel-center convention and a corner
//! convention land on the same source pixel. That subtle case needs the non-integer-scale
//! (0.5×/2×) phase-correlation oracle — Tier 2, bundled with the test-rig pass.
//!
//! Needs the real fixture COG; skips if it is absent (so a lean checkout still passes).

use std::sync::Arc;

use terraserve::backend::{CompressedTile, Resample};
use terraserve::cog::{self, LocalFileRangeSource, RangeSource};
use terraserve::decode;
use terraserve::render::{self, RenderRequest};
use terraserve::style::Style;

const COG: &str = "../cogs/cascais.cog.deflate.tif";
const STYLE: &str = "fixtures/styles/rgb.json";

fn have_fixtures() -> bool {
    std::path::Path::new(COG).exists() && std::path::Path::new(STYLE).exists()
}

#[test]
fn identity_render_reproduces_source_tile() {
    if !have_fixtures() {
        eprintln!("skipping identity_render_reproduces_source_tile: fixtures absent");
        return;
    }
    let src = LocalFileRangeSource::open(COG).unwrap();
    let cog = cog::parse(&src).unwrap();
    let level = &cog.levels[0];
    let (tw, th) = (level.tile_w, level.tile_h);

    // Ground truth: decode source tile (0,0) directly — no warp, no reprojection.
    let index_cache = terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes());
    let (off, len) = level
        .tile_location(&src, &index_cache, 0)
        .unwrap()
        .expect("tile present");
    let bytes = src.read_range(off, len).unwrap();
    let ct = CompressedTile {
        bytes,
        compression: level.compression,
        predictor: level.predictor,
        tile_w: tw,
        tile_h: th,
        samples: level.samples_per_pixel,
        bits_per_sample: level.bits_per_sample,
        sample_format: level.sample_format,
        little_endian: cog.little_endian,
        photometric: level.photometric,
        jpeg_tables: cog.jpeg_tables.as_ref().map(|t| Arc::new(t.clone())),
        grid_col: 0,
        grid_row: 0,
        present: true,
    };
    let truth = decode::decode_tile_rgba(&ct); // RGBA, tw*th*4

    // Identity render of tile (0,0)'s exact geo extent at its native pixel dims.
    let g = &level.geo;
    let bbox = [
        g.origin_x,                    // minx (left edge of pixel col 0)
        g.origin_y - th as f64 * g.py, // miny
        g.origin_x + tw as f64 * g.px, // maxx
        g.origin_y,                    // maxy (top edge of pixel row 0)
    ];
    let style = Style::load(STYLE).unwrap();
    let req = RenderRequest {
        cog_path: COG,
        bbox,
        crs: "EPSG:3763",
        src_crs: "EPSG:3763",
        width: tw,
        height: th,
        resample: Resample::Nearest,
        style: &style,
        band_math: None,
        index_cache: index_cache.clone(),
    };
    let got = render::render_with_cog(&req, &cog, &src, None).unwrap();

    assert_eq!(
        got.len(),
        truth.data.len(),
        "identity render size differs from source tile"
    );

    // The interior (excluding a 1px border, where an identity-CRS PROJ round-trip may jitter a
    // boundary sample by a ULP) must be byte-exact. A flip/transpose/full-pixel offset corrupts
    // the interior en masse, so this stays a real gross-regression guard.
    let (w, h) = (tw as usize, th as usize);
    let (mut total, mut interior) = (0usize, 0usize);
    for j in 0..h {
        for i in 0..w {
            let p = (j * w + i) * 4;
            if got[p..p + 4] != truth.data[p..p + 4] {
                total += 1;
                if i > 0 && j > 0 && i < w - 1 && j < h - 1 {
                    interior += 1;
                }
            }
        }
    }
    eprintln!("identity {w}x{h}: total_mismatch={total} interior_mismatch={interior}");
    assert_eq!(
        interior, 0,
        "identity render does not reproduce the source tile interior — gross georegistration regression"
    );
}
