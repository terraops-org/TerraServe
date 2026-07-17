//! Band-math nodata/validity guard (Step-0 P0 — the flagship NDVI failure).
//!
//! The dangerous failure the P0 audit flagged: nodata (−32768) *participating* in
//! `(B08−B04)/(B08+B04)` yields an IN-RANGE value (e.g. −0.0 → "no vegetation") that gets a
//! ramp colour — plausible garbage painted across scene edges / sparse tiles. The engine
//! avoids this by masking on the RAW band values before/independent of the arithmetic
//! (`render.rs`: `valid[wp] = false` when any referenced band == nodata), and colorizing
//! invalid pixels transparent regardless of the computed value. This test guards that
//! observable contract: valid data is colour+opaque; anything invalid (nodata or beyond the
//! image extent) is transparent, never coloured — and the nodata/NaN path never panics.
//!
//! Uses the local band-math COG; skips if absent (so a lean checkout / CI still passes).

use terraserve::backend::Resample;
use terraserve::cog::{self, LocalFileRangeSource};
use terraserve::expr;
use terraserve::render::{self, BandMath, RenderRequest};
use terraserve::style::Style;

const COG: &str = "../cogs/s2_stack.cog.tif";
const STYLE: &str = "fixtures/styles/ndvi.json";
const NDVI: &str = "(B08 - B04) / (B08 + B04)";
const BANDS: &[&str] = &["B02", "B03", "B04", "B08"]; // physical order
const NODATA: f64 = -32768.0;
const CRS: &str = "EPSG:32629";

fn have_fixtures() -> bool {
    std::path::Path::new(COG).exists() && std::path::Path::new(STYLE).exists()
}

/// (opaque, transparent) pixel counts in an RGBA buffer.
fn alpha_counts(rgba: &[u8]) -> (usize, usize) {
    let (mut opaque, mut transparent) = (0, 0);
    for px in rgba.chunks_exact(4) {
        match px[3] {
            255 => opaque += 1,
            0 => transparent += 1,
            _ => {}
        }
    }
    (opaque, transparent)
}

#[test]
fn ndvi_masks_nodata_and_out_of_extent_transparent() {
    if !have_fixtures() {
        eprintln!("skipping ndvi_masks_nodata_and_out_of_extent_transparent: fixtures absent");
        return;
    }
    let src = LocalFileRangeSource::open(COG).unwrap();
    let cog = cog::parse(&src).unwrap();
    let g = &cog.levels[0].geo;
    let (w, h) = (cog.levels[0].width as f64, cog.levels[0].height as f64);
    let (minx, maxy) = (g.origin_x, g.origin_y);
    let (maxx, miny) = (g.origin_x + w * g.px, g.origin_y - h * g.py);
    let (midx, midy) = ((minx + maxx) / 2.0, (miny + maxy) / 2.0);

    let program = expr::Program::compile(NDVI, BANDS).unwrap();
    let bm = BandMath {
        program,
        nodata: NODATA,
    };
    let style = Style::load(STYLE).unwrap();
    let index_cache = terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes());
    let render = |bbox: [f64; 4]| {
        render::render_with_cog(
            &RenderRequest {
                cog_path: COG,
                bbox,
                crs: CRS,
                src_crs: CRS,
                width: 256,
                height: 256,
                resample: Resample::Nearest,
                style: &style,
                band_math: Some(&bm),
                index_cache: index_cache.clone(),
            },
            &cog,
            &src,
            None,
        )
        .expect("render must not error on the nodata/band-math path")
    };

    // Center of the scene: real data must render as opaque, ramp-coloured pixels (no panic).
    let (c_opaque, _c_transparent) = alpha_counts(&render([
        midx - 2560.0,
        midy - 2560.0,
        midx + 2560.0,
        midy + 2560.0,
    ]));
    assert!(
        c_opaque > 0,
        "center NDVI render produced no opaque data pixels"
    );

    // Entirely beyond the image extent (x > maxx): every pixel is invalid, so every pixel
    // must be TRANSPARENT — never assigned an in-range ramp colour.
    let (b_opaque, b_transparent) = alpha_counts(&render([
        maxx + 1000.0,
        midy - 2560.0,
        maxx + 6120.0,
        midy + 2560.0,
    ]));
    assert_eq!(
        b_opaque, 0,
        "out-of-extent NDVI render coloured invalid pixels ({b_opaque} opaque) — nodata/mask regression"
    );
    assert!(
        b_transparent > 0,
        "out-of-extent render produced no pixels at all"
    );
}
