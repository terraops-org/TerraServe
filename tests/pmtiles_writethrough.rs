//! Write-through task 4: `Layer.overlay` wired into `render_mvt_tile`. Drives the real serve
//! entry point directly (no HTTP listener), mirroring `tests/pmtiles_serve.rs` / `tests/mvt_http.rs`'s
//! inline-layer pattern.
//!
//! A tile the (empty) overlay has never seen is live-encoded, then persisted to the overlay log; a
//! second identical request must return the exact same bytes, now served from the overlay itself.

use std::sync::Arc;
use terraserve::server::{Layer, ServeState, VectorLayer};
use terraserve::vector::geojson::GeoJsonSource;
use terraserve::vector::pmtiles::overlay::TileOverlay;
use terraserve::vector::shape::Shaper;
use terraserve::vector::source::{FeatureSource, VectorSource};
use terraserve::vector::style::Style;

/// Build a Layer from the worldwide countries polygon fixture (EPSG:4326), mirroring
/// `tests/pmtiles_serve.rs`'s `countries_layer()` / `tests/mvt_http.rs`'s `vector_layer()`, then
/// attach an EMPTY write-through overlay (base `None`) at `log` so a miss is live-encoded and
/// persisted rather than served from any archive.
fn layer_with_empty_overlay(log: &std::path::Path) -> Layer {
    let src = Arc::new(GeoJsonSource::load("fixtures/vector/countries.geojson").unwrap());
    let style = Style::load("fixtures/styles/countries.vec.json").unwrap();
    let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
    let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
    let ext = src.full_extent();
    let mut l = Layer {
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
        pmtiles: None,
        overlay: None,
    };
    l.pmtiles = None;
    l.overlay = Some(Arc::new(TileOverlay::open(log, None).unwrap()));
    l
}

#[test]
fn miss_is_live_encoded_then_served_from_overlay() {
    let log = std::env::temp_dir().join(format!("ts_wt_{}.log", std::process::id()));
    std::fs::remove_file(&log).ok();
    let layer = layer_with_empty_overlay(&log);
    let ov = layer.overlay.clone().unwrap();
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
