//! Task 7: `serve --pmtiles` read-through + live fallback. Drives the real serve entry point
//! (`mvt_http::render_mvt_tile`) directly, no HTTP listener — mirrors `tests/mvt_http.rs`'s
//! `vector_layer()`/`state()` pattern (and `tests/pmtiles_e2e.rs`'s inline Layer construction).
//!
//! A tile the archive was built for (z0..=2) is served straight from the archive; a tile outside
//! the built range (z6) falls through to the live encoder — proven by comparing against a second,
//! identical Layer that carries NO archive at all.
//!
//! The archive-hit half builds the archive from a layer named **"ARCHIVED"** while the served layer
//! is named **"countries"** — `encode_tile_opt`'s `layer_name` is written verbatim into the MVT
//! protobuf (`Layer.name`, field 1), so an archived tile's bytes are provably different from what a
//! live encode of the same geometry under the "countries" name would produce. That is what makes the
//! `served == archive-bytes` / `served != live-bytes` pair a genuine regression guard for the
//! archive-first early-return in `render_mvt_tile`: if that branch were silently removed, `served`
//! would fall through to the live encoder and become byte-identical to `live_countries`, failing the
//! `assert_ne!` below. Without the name difference the two paths produce byte-identical output by
//! construction (same layer, same opts), so the assertion would be tautological.

use std::sync::Arc;

use terraserve::mvt_http::render_mvt_tile;
use terraserve::server::{Layer, ServeState, VectorLayer};
use terraserve::vector::geojson::GeoJsonSource;
use terraserve::vector::mvt::opts::MvtOptimizations;
use terraserve::vector::pmtiles::generate::build_pmtiles;
use terraserve::vector::pmtiles::read::PmtilesReader;
use terraserve::vector::shape::Shaper;
use terraserve::vector::source::{FeatureSource, VectorSource};
use terraserve::vector::style::Style;

const LAYER: &str = "countries";
/// The name the archive is generated under — deliberately different from `LAYER` so the archived
/// tile's embedded `Layer.name` distinguishes it from a live "countries" encode (see module doc).
const ARCHIVE_LAYER_NAME: &str = "ARCHIVED";

/// Build a Layer from the worldwide countries polygon fixture (EPSG:4326), mirroring
/// `tests/mvt_http.rs`'s `vector_layer()`. `pmtiles` starts unset — the caller wires it up.
/// `name` becomes both the served/looked-up layer name AND the MVT `Layer.name` embedded by the
/// encoder — callers pass `LAYER` for a servable layer, `ARCHIVE_LAYER_NAME` for archive generation.
fn countries_layer(name: &str) -> Layer {
    let src = Arc::new(GeoJsonSource::load("fixtures/vector/countries.geojson").unwrap());
    let style = Style::load("fixtures/styles/countries.vec.json").unwrap();
    let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
    let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
    let ext = src.full_extent();
    Layer {
        name: name.into(),
        cog_path: String::new(),
        cog: None,
        source: None,
        style: None,
        src_crs: "EPSG:4326".into(),
        band_math: None,
        bounds_wgs84: ext,
        tile_cache: None,
        index_cache: terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes()),
        grids: Vec::new(),
        vector: Some(VectorLayer {
            fields: terraserve::mvt_http::feature_field_schema(src.as_ref()),
            area_scale: terraserve::vector::mvt::layer_area_scale(ext, ext),
            source: VectorSource::LoadAll(src),
            style,
            shaper,
            lod: None,
        }),
        pmtiles: None,
        overlay: None,
    }
}

#[test]
fn read_through_hits_archive_and_falls_back_live() {
    let grid = terraserve::tms::preset("WebMercatorQuad", 4096).unwrap();

    // Generation layer: same geometry/style/font as the served layer, but named "ARCHIVED" — the
    // archive's tiles carry THIS name in their embedded MVT `Layer.name`, not "countries".
    let gen_layer = countries_layer(ARCHIVE_LAYER_NAME);
    let gen_v = gen_layer.vector.as_ref().unwrap();
    let gen_opts =
        MvtOptimizations::for_layer(&ServeState::new(vec![], "http://h/wms".into(), 16), gen_v);

    // Build a small archive covering ONLY z0..=2.
    let out = std::env::temp_dir().join(format!("ts_serve_{}.pmtiles", std::process::id()));
    build_pmtiles(
        &gen_layer,
        &gen_opts,
        &grid,
        0,
        2,
        gen_layer.bounds_wgs84,
        &out,
        &std::env::temp_dir(),
    )
    .unwrap();

    // Served layer: named "countries" (what clients actually request), with the "ARCHIVED" archive
    // attached — this is the mismatch that makes archive-served bytes distinguishable from a live
    // "countries" encode.
    let mut served_layer = countries_layer(LAYER);
    served_layer.pmtiles = Some(Arc::new(PmtilesReader::open(&out).unwrap()));
    let st = ServeState::new(vec![served_layer], "http://h/wms".into(), 16);

    // A second, identical "countries" layer with NO archive at all — what a live encode of
    // "countries" actually produces. Reused for both halves of the test below.
    let st_no_archive = ServeState::new(vec![countries_layer(LAYER)], "http://h/wms".into(), 16);

    // --- Archive hit: find a z2 tile the archive actually has data for, and confirm BOTH that
    // serving it via render_mvt_tile returns exactly the archive's bytes, AND that those bytes are
    // NOT what a live "countries" encode of the same tile would produce (see module doc for why the
    // name mismatch makes this a genuine regression guard rather than a tautology).
    let reader = st.layers[0].pmtiles.as_ref().unwrap().clone();
    let bbox3857 = terraserve::reproj::crs_bounds(
        "EPSG:4326",
        "EPSG:3857",
        gen_layer.bounds_wgs84[0],
        gen_layer.bounds_wgs84[1],
        gen_layer.bounds_wgs84[2],
        gen_layer.bounds_wgs84[3],
    )
    .expect("reproject bounds to 3857");
    let (c0, c1, r0, r1) = grid
        .tile_limits(bbox3857, 2)
        .expect("z2 tile range for the fixture bounds");
    let mut found_archive_hit = false;
    'outer: for x in c0..=c1 {
        for y in r0..=r1 {
            if let Some(archived) = reader.get(2, x, y).unwrap() {
                let served = render_mvt_tile(&st, LAYER, "WebMercatorQuad", 2, x, y).unwrap();
                let live_countries =
                    render_mvt_tile(&st_no_archive, LAYER, "WebMercatorQuad", 2, x, y).unwrap();
                assert_eq!(
                    served, archived,
                    "archived z2 tile {x}/{y} served from archive"
                );
                assert_ne!(
                    served, live_countries,
                    "served bytes must be the archive's (embedded name '{ARCHIVE_LAYER_NAME}'), \
                     not a live '{LAYER}' encode — if they match, the archive-first \
                     early-return in render_mvt_tile did not run"
                );
                found_archive_hit = true;
                break 'outer;
            }
        }
    }
    assert!(
        found_archive_hit,
        "expected at least one non-empty z2 tile in the archive"
    );

    // --- Live fallback: a z6 tile (outside the built 0..=2 range) must fall through to the live
    // encoder. Prove it by comparing against the no-archive "countries" layer.
    let (zc0, _zc1, zr0, _zr1) = grid
        .tile_limits(bbox3857, 6)
        .expect("z6 tile range for the fixture bounds");
    let (z, x, y) = (6, zc0, zr0);
    assert_eq!(
        reader.get(z, x, y).unwrap(),
        None,
        "z6 must be outside the archive's built 0..=2 range"
    );

    let served_z6 = render_mvt_tile(&st, LAYER, "WebMercatorQuad", z, x, y).unwrap();
    let live_z6 = render_mvt_tile(&st_no_archive, LAYER, "WebMercatorQuad", z, x, y).unwrap();
    assert_eq!(
        served_z6, live_z6,
        "unarchived z6 tile falls back to live encode, matching a no-archive layer"
    );

    std::fs::remove_file(&out).ok();
}
