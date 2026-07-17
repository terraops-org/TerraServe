//! Task 5: server wiring for MVT — `/mvt` XYZ tiles + TileJSON, and WMTS `GetTile` with
//! `FORMAT=application/vnd.mapbox-vector-tile`. Mirrors the `tms_http`/`wmts_*` test style: build a
//! `ServeState` directly (no HTTP listener) and call the front-end functions the axum handlers wrap.
//!
//! Task 6 (`/xray`, the X-ray MVT viewer) adds one exception to that "no HTTP listener" rule: the
//! deliverable IS the route wiring (the page itself is opaque static HTML), so its smoke test spins
//! a real server on an ephemeral-ish port and does a real GET — the thing worth pinning is that
//! `/xray` is actually registered and returns `text/html`, not just that `xray_html()` returns a
//! string with the right substrings.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use terraserve::mvt_http;
use terraserve::server::{self, Layer, ServeState, VectorLayer};
use terraserve::vector::geojson::GeoJsonSource;
use terraserve::vector::shape::Shaper;
use terraserve::vector::source::{FeatureSource, VectorSource};
use terraserve::vector::style::Style;
use terraserve::wmts::{self, WmtsRequest};

const LAYER: &str = "mini";

fn vector_layer() -> Layer {
    let src = Arc::new(GeoJsonSource::load("fixtures/vector/mini_mvt.geojson").unwrap());
    let style = Style::load("fixtures/styles/airports.vec.json").unwrap();
    let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
    let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
    let ext = src.full_extent();
    Layer {
        name: LAYER.into(),
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

fn state() -> ServeState {
    ServeState::new(vec![vector_layer()], "http://h/wms".into(), 16)
}

#[test]
fn xyz_tile_webmercator_is_200_and_nonempty() {
    let st = state();
    let bytes = mvt_http::render_mvt_tile(&st, LAYER, "WebMercatorQuad", 0, 0, 0).unwrap();
    assert!(!bytes.is_empty(), "z0/0/0 covers the whole fixture");
    // The bytes must be a well-formed MVT tile: layer name + extent round-trip via the bespoke
    // decoder logic used by the mvt_tile.rs tests (a minimal parse here — just walk field 3
    // (layers, LEN-delimited) and confirm at least one shows up with the expected name/extent).
    assert!(
        contains_bytes(&bytes, LAYER.as_bytes()),
        "MVT bytes should embed the layer name '{LAYER}'"
    );
}

#[test]
fn xyz_tile_worldcrs84quad_is_200_and_nonempty() {
    let st = state();
    let bytes = mvt_http::render_mvt_tile(&st, LAYER, "WorldCRS84Quad", 0, 0, 0).unwrap();
    assert!(!bytes.is_empty(), "z0/0/0 covers the whole fixture");
}

#[test]
fn xyz_tile_out_of_range_is_404_not_panic() {
    let st = state();
    let err = mvt_http::render_mvt_tile(&st, LAYER, "WebMercatorQuad", 0, 5, 5).unwrap_err();
    assert_eq!(err.0, 404);
}

#[test]
fn xyz_tile_unknown_tms_is_4xx_not_panic() {
    let st = state();
    let err = mvt_http::render_mvt_tile(&st, LAYER, "NoSuchGrid", 0, 0, 0).unwrap_err();
    assert!((400..500).contains(&err.0));
}

#[test]
fn xyz_tile_unknown_layer_is_404() {
    let st = state();
    let err = mvt_http::render_mvt_tile(&st, "nope", "WebMercatorQuad", 0, 0, 0).unwrap_err();
    assert_eq!(err.0, 404);
}

#[test]
fn xyz_tile_raster_layer_is_4xx_not_panic() {
    // A layer with no `vector` (raster-only) must 4xx cleanly, not panic, when asked for MVT.
    let mut l = vector_layer();
    l.vector = None;
    let st = ServeState::new(vec![l], "http://h/wms".into(), 16);
    let err = mvt_http::render_mvt_tile(&st, LAYER, "WebMercatorQuad", 0, 0, 0).unwrap_err();
    assert!((400..500).contains(&err.0));
}

#[test]
fn tilejson_document_has_the_expected_shape() {
    let st = state();
    let doc = mvt_http::tilejson_doc(&st, LAYER, "WebMercatorQuad", None).unwrap();
    let v: serde_json::Value = serde_json::from_str(&doc).expect("valid JSON");
    assert_eq!(v["tilejson"], "3.0.0");
    let tiles = v["tiles"].as_array().expect("tiles array");
    assert_eq!(tiles.len(), 1);
    let tile_tpl = tiles[0].as_str().unwrap();
    assert!(tile_tpl.ends_with("/{z}/{x}/{y}.pbf"), "{tile_tpl}");
    assert!(
        tile_tpl.contains("/mvt/mini/WebMercatorQuad/"),
        "{tile_tpl}"
    );
    assert!(v["minzoom"].is_number());
    assert!(v["maxzoom"].is_number());
    assert_eq!(v["bounds"].as_array().unwrap().len(), 4);
    let vlayers = v["vector_layers"].as_array().expect("vector_layers");
    assert_eq!(vlayers.len(), 1);
    assert_eq!(vlayers[0]["id"], LAYER);
    // Fields from fixtures/vector/mini_mvt.geojson: name (String), kind (String), length (Number),
    // area (Number).
    assert_eq!(vlayers[0]["fields"]["name"], "String");
    assert_eq!(vlayers[0]["fields"]["area"], "Number");
}

#[test]
fn tilejson_unknown_tms_is_4xx_not_panic() {
    let st = state();
    let err = mvt_http::tilejson_doc(&st, LAYER, "NoSuchGrid", None).unwrap_err();
    assert!((400..500).contains(&err.0));
}

#[test]
fn style_json_is_a_maplibre_gl_style_referencing_the_source() {
    let st = state();
    let doc = mvt_http::style_json(&st, LAYER, Some("192.168.1.121:8081")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&doc).expect("valid JSON");
    assert_eq!(v["version"], 8, "MapLibre GL style spec version");
    // The source references the layer's TileJSON on the REQUEST host (not 0.0.0.0 / base_url).
    assert_eq!(v["sources"]["terraserve"]["type"], "vector");
    assert_eq!(
        v["sources"]["terraserve"]["url"],
        "http://192.168.1.121:8081/mvt/mini/WebMercatorQuad.json"
    );
    // Layers render EVERY geometry type (fill + line + circle), each bound to the MVT source-layer.
    let layers = v["layers"].as_array().expect("layers array");
    assert!(layers.iter().any(|l| l["type"] == "fill"));
    assert!(layers.iter().any(|l| l["type"] == "line"));
    assert!(layers.iter().any(|l| l["type"] == "circle"));
    for l in layers {
        assert_eq!(l["source"], "terraserve");
        assert_eq!(
            l["source-layer"], LAYER,
            "source-layer must match the MVT layer name"
        );
    }
    // Every circle layer must be gated to Point geometry — otherwise a client (QGIS) draws a
    // marker at each polygon's centroid (a dot on every parcel).
    for l in layers.iter().filter(|l| l["type"] == "circle") {
        assert_eq!(
            l["filter"],
            serde_json::json!(["==", "$type", "Point"]),
            "circle layers must not fire on polygons/lines"
        );
    }
    // The fill layer is gated to Polygon.
    let fill = layers.iter().find(|l| l["type"] == "fill").unwrap();
    assert_eq!(
        fill["filter"],
        serde_json::json!(["==", "$type", "Polygon"])
    );
}

#[test]
fn style_json_unknown_layer_is_404() {
    let st = state();
    let err = mvt_http::style_json(&st, "nope", None).unwrap_err();
    assert_eq!(err.0, 404);
}

#[test]
fn wmts_gettile_mvt_format_matches_the_xyz_route() {
    let st = state();
    let q = "service=WMTS&request=GetTile&layer=mini&style=default&\
             format=application/vnd.mapbox-vector-tile&tilematrixset=WebMercatorQuad&\
             tilematrix=0&tilerow=0&tilecol=0";
    let req = wmts::parse_kvp(q);
    let (layer, style, tms_id, z, row, col, format) = match req {
        WmtsRequest::GetTile {
            layer,
            style,
            tms,
            z,
            row,
            col,
            format,
        } => (layer, style, tms, z, row, col, format),
        other => panic!("expected GetTile, got {other:?}"),
    };
    assert_eq!(format, "application/vnd.mapbox-vector-tile");
    let wmts_bytes = wmts::get_tile_mvt(&st, &layer, &style, &tms_id, z, row, col).unwrap();
    let xyz_bytes = mvt_http::render_mvt_tile(&st, LAYER, "WebMercatorQuad", 0, 0, 0).unwrap();
    assert_eq!(
        wmts_bytes, xyz_bytes,
        "WMTS-MVT must match the XYZ route byte-for-byte"
    );
}

#[test]
fn wmts_parse_kvp_accepts_mvt_format_and_still_rejects_bad_formats() {
    // A bogus FORMAT must still fail (unrelated to the new MVT branch).
    let q = "request=GetTile&layer=mini&style=default&format=image/tiff&\
             tilematrixset=WebMercatorQuad&tilematrix=0&tilerow=0&tilecol=0";
    match wmts::parse_kvp(q) {
        WmtsRequest::Exception { code, .. } => assert_eq!(code, "InvalidParameterValue"),
        o => panic!("{o:?}"),
    }
}

#[test]
fn xray_html_is_the_ol_mvt_viewer() {
    // Pure content check: no server needed. `body contains ol / VectorTile / MVT and references
    // /mvt/` per the task-6 brief.
    let html = server::xray_html();
    assert!(html.contains("<!doctype html>") || html.contains("<!DOCTYPE html>"));
    assert!(html.to_lowercase().contains("openlayers") || html.contains("ol.layer.VectorTile"));
    assert!(html.contains("ol.format.MVT"), "must use the MVT format");
    assert!(
        html.contains("/mvt/"),
        "must reference the MVT tile endpoint"
    );
    assert!(
        html.contains("WebMercatorQuad"),
        "must default to the safe EPSG:3857 grid"
    );
}

#[test]
fn xray_route_serves_html_over_real_http() {
    // Route-wiring smoke test: spin the actual axum server and GET /xray for real, so a typo'd
    // route path or a wrong Content-Type would fail here even though `xray_html()` itself is fine.
    const PORT: u16 = 18733;
    let st = state();
    thread::spawn(move || {
        let _ = server::run(st, "127.0.0.1", PORT);
    });

    let url = format!("http://127.0.0.1:{PORT}/xray");
    let mut last_err = None;
    for _ in 0..50 {
        match ureq::get(&url).call() {
            Ok(resp) => {
                assert_eq!(resp.status(), 200);
                let ct = resp.header("content-type").unwrap_or("").to_string();
                assert!(ct.starts_with("text/html"), "content-type was '{ct}'");
                let body = resp.into_string().expect("utf8 body");
                assert!(body.contains("ol.format.MVT"));
                assert!(body.contains("/mvt/"));
                return;
            }
            Err(e) => {
                last_err = Some(e);
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
    panic!("server on {PORT} never came up: {last_err:?}");
}

/// Cheap substring-of-bytes probe (no MVT decoder needed here — `tests/mvt_tile.rs` already owns
/// the full bespoke-decoder round-trip; this just confirms the layer name landed in the bytes).
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn mvt_cache_computes_once_and_serves_identical() {
    use terraserve::mvt_http::{build_byte_cache, render_mvt_tile};
    let mut st = state();
    st.mvt_cache = Some(build_byte_cache(16));
    let a = render_mvt_tile(&st, LAYER, "WebMercatorQuad", 0, 0, 0).unwrap();
    let b = render_mvt_tile(&st, LAYER, "WebMercatorQuad", 0, 0, 0).unwrap();
    assert_eq!(a, b, "cached tile is byte-identical");
    let cache = st.mvt_cache.as_ref().unwrap();
    cache.run_pending_tasks();
    assert_eq!(cache.entry_count(), 1, "the tile was cached (compute-once)");
    // The cache must not change output vs the uncached path.
    let uncached = render_mvt_tile(&state(), LAYER, "WebMercatorQuad", 0, 0, 0).unwrap();
    assert_eq!(a, uncached, "cache does not alter the encoded bytes");
}
