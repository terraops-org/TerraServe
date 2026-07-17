//! Step-1 seam tests: a pre-parsed COG can be injected into the render pipeline and
//! produces pixels byte-identical to the path-based `render()`. This is what lets the
//! async server parse the COG once at startup instead of re-parsing per request.
//!
//! These tests need the real fixture COG (`../cogs/cascais.cog.deflate.tif`); if it is
//! not present they skip (so a checkout without the big binaries still passes).

use terraserve::backend::Resample;
use terraserve::cache;
use terraserve::cog::{self, LocalFileRangeSource};
use terraserve::render::{self, RenderRequest};
use terraserve::style::Style;

const COG: &str = "../cogs/cascais.cog.deflate.tif";
const STYLE: &str = "fixtures/styles/rgb.json";
// A native-resolution window from the manifest `sc_native_center` case.
const BBOX: [f64; 4] = [-112701.25, -106296.25, -112573.25, -106168.25];

fn have_fixtures() -> bool {
    std::path::Path::new(COG).exists() && std::path::Path::new(STYLE).exists()
}

fn req<'a>(style: &'a Style) -> RenderRequest<'a> {
    RenderRequest {
        cog_path: COG,
        bbox: BBOX,
        crs: "EPSG:3763",
        src_crs: "EPSG:3763",
        width: 512,
        height: 512,
        resample: Resample::Nearest,
        style,
        band_math: None,
        index_cache: cache::new_index_cache(cache::index_cache_bytes()),
    }
}

#[test]
fn render_with_cog_matches_render() {
    if !have_fixtures() {
        eprintln!("skipping render_with_cog_matches_render: fixtures absent");
        return;
    }
    let style = Style::load(STYLE).unwrap();

    // Reference: the path-based entry point (opens + parses internally).
    let want = render::render(&req(&style)).expect("render() failed");

    // New seam: parse the COG once, then render from the cached structure.
    let src = LocalFileRangeSource::open(COG).unwrap();
    let cog = cog::parse(&src).unwrap();
    let got =
        render::render_with_cog(&req(&style), &cog, &src, None).expect("render_with_cog() failed");

    assert_eq!(want.len(), got.len(), "output sizes differ");
    assert!(want == got, "render_with_cog pixels differ from render");
}

#[test]
fn one_parse_serves_many_renders() {
    if !have_fixtures() {
        eprintln!("skipping one_parse_serves_many_renders: fixtures absent");
        return;
    }
    let style = Style::load(STYLE).unwrap();

    // Parse ONCE (the server does this at startup) ...
    let src = LocalFileRangeSource::open(COG).unwrap();
    let cog = cog::parse(&src).unwrap();

    // ... then reuse across renders — deterministic and identical to the reference.
    let a = render::render_with_cog(&req(&style), &cog, &src, None).unwrap();
    let b = render::render_with_cog(&req(&style), &cog, &src, None).unwrap();
    assert!(
        a == b,
        "repeated render from one parsed COG is not deterministic"
    );

    let reference = render::render(&req(&style)).unwrap();
    assert!(
        a == reference,
        "cached-COG render differs from path-based render"
    );
}

#[test]
fn warm_cache_matches_cold() {
    if !have_fixtures() {
        eprintln!("skipping warm_cache_matches_cold: fixtures absent");
        return;
    }
    let style = Style::load(STYLE).unwrap();
    let src = LocalFileRangeSource::open(COG).unwrap();
    let cog = cog::parse(&src).unwrap();
    let cache = cache::new_tile_cache(cache::DEFAULT_CAP_BYTES);

    // First render fills the cache (all misses); the second is served from it (hits).
    let cold = render::render_with_cog(&req(&style), &cog, &src, Some(&cache)).unwrap();
    let warm = render::render_with_cog(&req(&style), &cog, &src, Some(&cache)).unwrap();
    cache.run_pending_tasks();
    assert!(cache.entry_count() > 0, "cache did not populate");
    assert!(warm == cold, "warm-cache render differs from cold");

    // Both must equal the uncached reference — caching changes latency, never pixels.
    let reference = render::render_with_cog(&req(&style), &cog, &src, None).unwrap();
    assert!(cold == reference, "cached render differs from uncached");
}
