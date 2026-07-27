//! Write-through task 4: `Layer.overlay` wired into `render_mvt_tile`. Drives the real serve
//! entry point directly (no HTTP listener), mirroring `tests/pmtiles_serve.rs` / `tests/mvt_http.rs`'s
//! inline-layer pattern.
//!
//! A tile the (empty) overlay has never seen is live-encoded, then persisted to the overlay log; a
//! second identical request must return the exact same bytes, now served from the overlay itself.

use std::sync::Arc;
use terraserve::server::{Layer, ServeState, VectorLayer};
use terraserve::vector::geojson::GeoJsonSource;
use terraserve::vector::mvt::opts::MvtOptimizations;
use terraserve::vector::pmtiles::generate::build_pmtiles;
use terraserve::vector::pmtiles::overlay::TileOverlay;
use terraserve::vector::pmtiles::read::PmtilesReader;
use terraserve::vector::shape::Shaper;
use terraserve::vector::source::{FeatureSource, VectorSource};
use terraserve::vector::style::Style;

/// Build a Layer from the worldwide countries polygon fixture (EPSG:4326), mirroring
/// `tests/pmtiles_serve.rs`'s `countries_layer()` / `tests/mvt_http.rs`'s `vector_layer()`, with NO
/// overlay attached yet — callers wire up `.overlay` (one entry per grid, task 4).
fn countries_layer_no_overlay() -> Layer {
    let src = Arc::new(GeoJsonSource::load("fixtures/vector/countries.geojson").unwrap());
    let style = Style::load("fixtures/styles/countries.vec.json").unwrap();
    let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
    let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
    let ext = src.full_extent();
    Layer {
        name: "countries".into(),
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
        pmtiles: std::collections::BTreeMap::new(),
        overlay: std::collections::BTreeMap::new(),
    }
}

/// `countries_layer_no_overlay()` with a single EMPTY write-through overlay (base `None`) at `log`,
/// keyed under `grid_id` (task 4: per-grid overlay map), so a miss for that grid is live-encoded and
/// persisted rather than served from any archive.
fn layer_with_empty_overlay(log: &std::path::Path, grid_id: &str) -> Layer {
    let mut l = countries_layer_no_overlay();
    l.overlay.insert(
        grid_id.to_string(),
        Arc::new(TileOverlay::open(log, None).unwrap()),
    );
    l
}

#[test]
fn miss_is_live_encoded_then_served_from_overlay() {
    let log = std::env::temp_dir().join(format!("ts_wt_{}.log", std::process::id()));
    std::fs::remove_file(&log).ok();
    let layer = layer_with_empty_overlay(&log, "WebMercatorQuad");
    let ov = layer.overlay.get("WebMercatorQuad").unwrap().clone();
    let st = ServeState::new(vec![layer], "http://h/wms".into(), 16);
    // First request: overlay empty + base None -> live encode; must also populate the overlay.
    let first = terraserve::mvt_http::render_mvt_tile(&st, "countries", "WebMercatorQuad", 2, 1, 1)
        .unwrap();
    if !first.is_empty() {
        let id = terraserve::vector::pmtiles::zxy_to_tileid(2, 1, 1);
        assert!(
            ov.get_by_id(id).unwrap().is_some(),
            "miss must be persisted to the overlay"
        );
        // Second identical request returns the SAME bytes (now served from the overlay).
        let second =
            terraserve::mvt_http::render_mvt_tile(&st, "countries", "WebMercatorQuad", 2, 1, 1)
                .unwrap();
        assert_eq!(
            first, second,
            "overlay-served bytes must match the live encode"
        );
    }
    std::fs::remove_file(&log).ok();
}

/// Task 4: `Layer.overlay` is a per-GRID map, mirroring the read-through `Layer.pmtiles` map — the
/// scenario is two `--pmtiles` archives on DIFFERENT grids under `--pmtiles-cache` (`run_serve`'s
/// wiring in lib.rs opens exactly this: one overlay per archive, keyed by the grid_id it
/// self-describes). This is reproduced inline here — rather than driving `run_serve` itself, which
/// blocks on a real HTTP server — the same way every other test in this file family avoids a live
/// listener.
///
/// The two grids are WebMercatorQuad and the real `fixtures/grids/swissLV95.json` fixture — the
/// EXACT collision this task's brief calls out (swissLV95 z16/3/2 vs WebMercatorQuad z16/3/2 map to
/// the same `zxy_to_tileid` but are different tiles). At z2 specifically, WebMercatorQuad's
/// `matrix_w == matrix_h == 4` makes `zxy_to_tileid(2, 1, 1)` land on the SAME id a same-shaped grid
/// would produce, so grid A's archive/overlay covers ONLY z0..=1 (z2 is a genuine miss that must
/// live-encode + persist), while grid B's archive covers a single Swiss-region z0 tile — enough to
/// prove B's overlay is real and self-describes its own grid_id, without needing swissLV95 z2 data.
#[test]
fn per_grid_overlays_isolate_puts_by_grid() {
    let pid = std::process::id();
    let out_a = std::env::temp_dir().join(format!("ts_wt_grids_a_{pid}.pmtiles"));
    let out_b = std::env::temp_dir().join(format!("ts_wt_grids_b_{pid}.pmtiles"));
    let wal_a = std::env::temp_dir().join(format!("ts_wt_grids_a_{pid}.pmtiles.wal"));
    let wal_b = std::env::temp_dir().join(format!("ts_wt_grids_b_{pid}.pmtiles.wal"));
    for p in [&out_a, &out_b, &wal_a, &wal_b] {
        std::fs::remove_file(p).ok();
    }

    // `tms::preset(_, 4096)` (the MVT-baking resolution) suffixes non-256px ids with `_4096` — reset
    // to the bare id an operator's `--grid` actually names, mirroring `run_build_pmtiles` /
    // `tests/pmtiles_e2e.rs::build_pmtiles_uses_the_given_layer_name`.
    let mut grid_a = terraserve::tms::preset("WebMercatorQuad", 4096).unwrap();
    grid_a.id = "WebMercatorQuad".to_string();
    let swiss_json = std::fs::read_to_string("fixtures/grids/swissLV95.json")
        .expect("fixtures/grids/swissLV95.json");
    let grid_b = terraserve::tms::from_ogc_json(&swiss_json).expect("swissLV95.json parses");
    assert_eq!(grid_b.id, "swissLV95");

    let gen_layer = countries_layer_no_overlay();
    let gen_state = ServeState::new(vec![], "http://h/wms".into(), 16);
    let opts = MvtOptimizations::for_layer(&gen_state, gen_layer.vector.as_ref().unwrap());

    // Grid A: z0..=1 only, over the fixture's own (worldwide) bounds — matches
    // `miss_is_live_encoded_then_served_from_overlay` above, so z2/1/1 is a genuine miss.
    build_pmtiles(
        &gen_layer,
        &opts,
        &grid_a,
        0,
        1,
        gen_layer.bounds_wgs84,
        &out_a,
        &std::env::temp_dir(),
    )
    .unwrap();
    // Grid B: z0 only (swissLV95's level "0" is a single 1x1 tile), over a small Swiss-region bbox
    // — a real archive self-describing grid_id "swissLV95", independent of whether it holds any of
    // the (worldwide, country-outline) fixture's geometry.
    build_pmtiles(
        &gen_layer,
        &opts,
        &grid_b,
        0,
        0,
        [5.9, 45.8, 10.6, 47.9],
        &out_b,
        &std::env::temp_dir(),
    )
    .unwrap();

    let reader_a = PmtilesReader::open(&out_a).unwrap();
    let reader_b = PmtilesReader::open(&out_b).unwrap();
    let (gid_a, gid_b) = (reader_a.grid_id(), reader_b.grid_id());
    assert_eq!(gid_a, "WebMercatorQuad");
    assert_eq!(gid_b, "swissLV95");
    assert_ne!(
        gid_a, gid_b,
        "the two archives must self-describe DIFFERENT grids"
    );

    // Exactly `run_serve`'s --pmtiles-cache loop (lib.rs): open each archive as a base, wrap it in a
    // write-through overlay at `<archive>.wal`, stamp the overlay's own metadata, key by grid_id.
    let ov_a = Arc::new(TileOverlay::open(&wal_a, Some(Arc::new(reader_a))).unwrap());
    ov_a.set_metadata(terraserve::mvt_http::pmtiles_metadata_json(
        &gen_layer,
        Some(&gid_a),
    ));
    let ov_b = Arc::new(TileOverlay::open(&wal_b, Some(Arc::new(reader_b))).unwrap());
    ov_b.set_metadata(terraserve::mvt_http::pmtiles_metadata_json(
        &gen_layer,
        Some(&gid_b),
    ));

    let mut layer = countries_layer_no_overlay();
    layer.grids = vec![
        terraserve::server::PublishedGrid {
            tms: grid_a.clone(),
            data_bounds: None,
        },
        terraserve::server::PublishedGrid {
            tms: grid_b.clone(),
            data_bounds: None,
        },
    ];
    layer.overlay.insert(gid_a.clone(), ov_a.clone());
    layer.overlay.insert(gid_b.clone(), ov_b.clone());
    assert_eq!(
        layer.overlay.len(),
        2,
        "two --pmtiles archives on different grids -> two overlays"
    );

    let st = ServeState::new(vec![layer], "http://h/wms".into(), 16);

    // z2 is outside grid A's built 0..=1 range -> a genuine miss. `zxy_to_tileid(2, 1, 1)` is a
    // valid id on BOTH grids' TileID spaces (design commitment 1's collision risk), so this is also
    // the exact id a stray write into grid B's overlay would show up under.
    let id2 = terraserve::vector::pmtiles::zxy_to_tileid(2, 1, 1);
    assert!(
        ov_a.get_by_id(id2).unwrap().is_none(),
        "grid A archive has nothing at z2 yet"
    );
    assert!(
        ov_b.snapshot_ids().is_empty(),
        "grid B's overlay must start empty"
    );

    let first_a = terraserve::mvt_http::render_mvt_tile(&st, "countries", &gid_a, 2, 1, 1).unwrap();
    if !first_a.is_empty() {
        assert!(
            ov_a.get_by_id(id2).unwrap().is_some(),
            "a live miss for grid A must persist into A's own overlay"
        );
        assert!(
            ov_b.snapshot_ids().is_empty(),
            "grid B's overlay must stay untouched by a grid-A request, even though \
             zxy_to_tileid(2,1,1) is the SAME id on both grids"
        );

        // A later request for the SAME grid A tile hits A's overlay and returns identical bytes.
        let second_a =
            terraserve::mvt_http::render_mvt_tile(&st, "countries", &gid_a, 2, 1, 1).unwrap();
        assert_eq!(
            first_a, second_a,
            "a later grid-A request must be served from A's overlay"
        );
    }

    for p in [&out_a, &out_b, &wal_a, &wal_b] {
        std::fs::remove_file(p).ok();
    }
}

/// Non-square-grid guard (piece A): `build_pmtiles` on a 2:1 grid like WorldCRS84Quad
/// (matrixWidth = 2^(z+1)) must return a CLEAR Err, not panic in the writer with "tile_id not
/// ascending" — the PMTiles Hilbert TileID (`zxy_to_tileid`) addresses a square 2^z quad only.
/// (Piece C's implementer hit this while writing the per-grid test above.)
#[test]
fn build_pmtiles_rejects_non_square_grid() {
    let out = std::env::temp_dir().join(format!("ts_nonsquare_{}.pmtiles", std::process::id()));
    std::fs::remove_file(&out).ok();
    let mut grid = terraserve::tms::preset("WorldCRS84Quad", 4096).unwrap();
    grid.id = "WorldCRS84Quad".to_string();
    let layer = countries_layer_no_overlay();
    let state = ServeState::new(vec![], "http://h/wms".into(), 16);
    let opts = MvtOptimizations::for_layer(&state, layer.vector.as_ref().unwrap());
    // A bbox spanning both z0 columns of the 2:1 grid -> tile col 1 at z0 (>= 2^0), which the guard
    // must reject BEFORE the writer panics on the aliased id.
    let msg = match build_pmtiles(
        &layer,
        &opts,
        &grid,
        0,
        2,
        [-180.0, -85.0, 180.0, 85.0],
        &out,
        &std::env::temp_dir(),
    ) {
        Ok(_) => panic!("a 2:1 grid must be rejected, not baked"),
        Err(e) => format!("{e}"),
    };
    assert!(
        msg.contains("square") && msg.contains("WorldCRS84Quad"),
        "guard error should name the square-quad constraint + the grid: {msg}"
    );
    std::fs::remove_file(&out).ok();
}
