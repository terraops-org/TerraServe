//! TMS 1.0.0 front-end unit checks: the y-flip, spec parsing, profile, and the TileMap XML
//! (BoundingBox + bottom-left Origin + one TileSet per zoom). Pure functions — no server needed.
//!
//! Plus the tiled-raster-vector-layer tests (`render_tms_tile` over a `vector:` layer), which do
//! run a real render — of `fixtures/gpkg/mini.gpkg`, the same tiny fixture `tests/gpkg_source.rs`
//! and the PostGIS live suite read, so no COG and no network is involved.

use terraserve::tms::TileMatrixSet;
use terraserve::tms_http;

#[test]
fn y_flip_maps_bottom_left_to_top_left() {
    let g = TileMatrixSet::web_mercator_quad(256);
    // At z=2, matrix_h=4. TMS y=0 (bottom) -> core row 3 (top-left, south).
    assert_eq!(tms_http::tms_y_to_core_row(&g, 2, 0), Some(3));
    assert_eq!(tms_http::tms_y_to_core_row(&g, 2, 3), Some(0));
    assert_eq!(tms_http::tms_y_to_core_row(&g, 2, 4), None); // out of range
}

#[test]
fn parse_layer_spec_splits_grid() {
    assert_eq!(
        tms_http::parse_layer_spec("basemap"),
        ("basemap".to_string(), None)
    );
    assert_eq!(
        tms_http::parse_layer_spec("basemap@WebMercatorQuad"),
        ("basemap".to_string(), Some("WebMercatorQuad".to_string()))
    );
}

#[test]
fn profile_is_well_known_for_canonical_grids() {
    assert_eq!(
        tms_http::tms_profile(&TileMatrixSet::web_mercator_quad(256)),
        "global-mercator"
    );
    assert_eq!(
        tms_http::tms_profile(&TileMatrixSet::world_crs84_quad(256)),
        "global-geodetic"
    );
    // A 512 variant is not the canonical well-known grid -> local.
    assert_eq!(
        tms_http::tms_profile(&TileMatrixSet::web_mercator_quad(512)),
        "local"
    );
    assert_eq!(
        tms_http::tms_profile(&TileMatrixSet::ups_wgs84_quad("EPSG:5041", 256)),
        "local"
    );
}

#[test]
fn tilemap_xml_has_bbox_bottom_left_origin_and_tilesets() {
    let g = TileMatrixSet::web_mercator_quad(256);
    let xml = tms_http::tilemap_xml_for("basemap", &g, None, "http://h/tms/1.0.0");
    assert!(xml.contains("<SRS>EPSG:3857</SRS>"));
    assert!(
        xml.contains("<BoundingBox"),
        "spec-required BoundingBox missing"
    );
    assert!(xml.contains("<Origin"));
    assert!(xml.contains("profile=\"global-mercator\""));
    // Bottom-left Origin == the grid's SW corner (WebMercator full extent).
    assert!(xml.contains("x=\"-20037508.3427892\""));
    assert!(xml.contains("y=\"-20037508.3427892\""));
    // One TileSet per zoom (25 levels).
    assert_eq!(xml.matches("<TileSet ").count(), 25);
    // A tile href a client appends /{x}/{y}.png to.
    assert!(xml.contains("href=\"http://h/tms/1.0.0/basemap@WebMercatorQuad/0\""));
}

/// `tms_root` now COMPOSES on an already-derived origin rather than deriving one itself.
/// Stripping `/wms` and the trailing slash moved to `ServeState::advertised_origin`, which is
/// the single derivation shared by WMS, WMTS, TMS and MVT; keeping a private copy here is what
/// left TMS unable to honour `X-Forwarded-Proto`. The derivation is tested in
/// `server::advertised_origin_tests`; this only pins the path composition.
#[test]
fn tms_root_appends_the_service_path_to_an_origin() {
    assert_eq!(
        tms_http::tms_root("http://localhost:8080"),
        "http://localhost:8080/tms/1.0.0"
    );
    // A proxied origin carrying a path prefix composes the same way.
    assert_eq!(
        tms_http::tms_root("https://terraserve.io/demo/swiss"),
        "https://terraserve.io/demo/swiss/tms/1.0.0"
    );
}

/// Task 2: `GET /tileMatrixSets/{id}` (`tms_http::tile_matrix_set_doc`, the handler's pure-function
/// core) resolves against the UNION of every layer's `grids` — here a single layer built from a
/// Swiss-style `--config` (a custom grid loaded from an OGC TileMatrixSet 2.0 JSON fixture via
/// `config::resolve_grids_presets`, the same seam a real `grids: [swissLV95.json]` entry goes
/// through). 200, the `crs` field carries the EPSG code, and the non-standard `proj4` convenience
/// field is populated (libproj resolves EPSG:2056, a Swiss oblique Mercator).
#[test]
fn tile_matrix_set_doc_resolves_a_custom_grid_published_on_a_layer() {
    use terraserve::server::{Layer, PublishedGrid, ServeState};

    let lv95 = terraserve::config::resolve_grids_presets(
        &["fixtures/grids/swissLV95.json".to_string()],
        256,
        &std::collections::BTreeMap::new(),
    )
    .expect("swissLV95.json should resolve via the .json dispatch in config::resolve_one")
    .into_iter()
    .next()
    .unwrap();
    assert_eq!(lv95.id, "swissLV95"); // sanity: id comes from the JSON, not the path

    let layer = Layer {
        name: "swiss".into(),
        cog_path: String::new(),
        cog: None,
        source: None,
        style: None,
        src_crs: "EPSG:2056".into(),
        band_math: None,
        bounds_wgs84: [0.0, 0.0, 0.0, 0.0],
        tile_cache: None,
        index_cache: terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes()),
        grids: vec![PublishedGrid {
            tms: lv95,
            data_bounds: None,
        }],
        vector: None,
        pmtiles: std::collections::BTreeMap::new(),
        raster_pmtiles: std::collections::BTreeMap::new(),
        overlay: std::collections::BTreeMap::new(),
    };
    let state = ServeState::new(vec![layer], "http://h/wms".into(), 16);

    let json = tms_http::tile_matrix_set_doc(&state, "swissLV95").expect("200 for swissLV95");
    assert!(
        json.contains("2056"),
        "crs must carry the EPSG code: {json}"
    );
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON body");
    let proj4 = v.get("proj4").and_then(|p| p.as_str()).unwrap_or("");
    assert!(!proj4.is_empty(), "proj4 must be non-empty: {json}");
}

/// An id no layer publishes -> 404 (not a panic, not a 200 with an empty body).
#[test]
fn tile_matrix_set_doc_unknown_id_is_404() {
    use terraserve::server::ServeState;
    let state = ServeState::new(Vec::new(), "http://h/wms".into(), 16);
    let err = tms_http::tile_matrix_set_doc(&state, "nope-not-a-grid")
        .expect_err("unknown grid id must fail");
    assert_eq!(err.0, 404);
}

/// A vector layer that publishes NO grids at all (`layer.grids` empty) — mirrors
/// `fixtures/fgb/multi.yaml` / `fixtures/cite/*.yaml` / the live cos2023+vida deploy configs, all of
/// which leave `grids:` unset and rely entirely on the preset fallback (`tms::preset`) that
/// `mvt_http::resolve_grid`/`wmts::get_tile_mvt` already apply for the actual tile routes.
fn vector_layer_no_grids() -> terraserve::server::Layer {
    use std::sync::Arc;
    use terraserve::server::{Layer, VectorLayer};
    use terraserve::vector::geojson::GeoJsonSource;
    use terraserve::vector::shape::Shaper;
    use terraserve::vector::source::{FeatureSource, VectorSource};
    use terraserve::vector::style::Style;

    let src = Arc::new(GeoJsonSource::load("fixtures/vector/mini_mvt.geojson").unwrap());
    let style = Style::load("fixtures/styles/airports.vec.json").unwrap();
    let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
    let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
    let ext = src.full_extent();
    Layer {
        name: "mini".into(),
        cog_path: String::new(),
        cog: None,
        source: None,
        style: None,
        src_crs: "EPSG:4326".into(),
        band_math: None,
        bounds_wgs84: ext,
        tile_cache: None,
        index_cache: terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes()),
        grids: Vec::new(), // <-- the crux: no published grids, only served via the preset fallback.
        vector: Some(VectorLayer {
            fields: terraserve::mvt_http::feature_field_schema(src.as_ref()),
            area_scale: terraserve::vector::mvt::layer_area_scale(ext, ext),
            min_feature_px: 0.0, // size gate off (the default)
            source: VectorSource::LoadAll(src),
            style,
            shaper,
            lod: None,
        }),
        pmtiles: std::collections::BTreeMap::new(),
        raster_pmtiles: std::collections::BTreeMap::new(),
        overlay: std::collections::BTreeMap::new(),
    }
}

/// Task 2 Important-finding fix: a vector layer with an EMPTY `grids` list still serves
/// `WebMercatorQuad` MVT tiles (via the preset fallback in `mvt_http::resolve_grid` /
/// `wmts::get_tile_mvt`), so `/tileMatrixSets/WebMercatorQuad` must ALSO resolve — 404ing here while
/// the tile route serves fine breaks any generic client (the viewer, Task 3) that fetches
/// `/tileMatrixSets/{grid}` before requesting tiles.
#[test]
fn tile_matrix_set_doc_falls_back_to_a_preset_when_no_layer_publishes_it() {
    use terraserve::server::ServeState;
    let state = ServeState::new(vec![vector_layer_no_grids()], "http://h/wms".into(), 16);

    let json = tms_http::tile_matrix_set_doc(&state, "WebMercatorQuad")
        .expect("preset fallback must answer for a preset id even with empty layer.grids");
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON body");
    let crs = v.get("crs").and_then(|c| c.as_str()).unwrap_or("");
    assert!(crs.contains("3857"), "crs must carry EPSG:3857: {json}");
    let tms = v
        .get("tileMatrices")
        .and_then(|t| t.as_array())
        .expect("tileMatrices array");
    assert!(!tms.is_empty(), "preset must have tile matrices: {json}");

    // Whole-branch review #1 regression guard: the preset fallback MUST advertise the STANDARD
    // 256-px quad — NOT a 4096-px one. `build_quad` sets `res0 = base_span / tile_px`, so a 4096
    // tileWidth makes z0 cellSize 16x finer (9784 vs the canonical 156543 m/px) and the id
    // `WebMercatorQuad_4096`. An OpenLayers/OGC client derives its zoom<->resolution ladder from a
    // 256 tile, so a 4096-advertised grid shifts tile selection 4 levels coarser (log2(4096/256))
    // and can clamp the startup view to the whole-world z0 tile. Served MVT tiles are byte-identical
    // either way (tile bbox is `tile_w`-invariant; the encoder uses a fixed 4096-unit EXTENT), so
    // 256 is both standards-correct and viewer-correct.
    assert_eq!(
        v.get("id").and_then(|i| i.as_str()),
        Some("WebMercatorQuad"),
        "fallback must advertise the bare standard id, not a _4096 suffix: {json}"
    );
    let m0 = &tms[0];
    assert_eq!(
        m0.get("tileWidth").and_then(|w| w.as_u64()),
        Some(256),
        "fallback must advertise the standard 256-px tile, not 4096: {json}"
    );
    let cell0 = m0.get("cellSize").and_then(|c| c.as_f64()).unwrap_or(0.0);
    assert!(
        (cell0 - 156543.033928).abs() < 1.0,
        "z0 cellSize must be the canonical WebMercatorQuad 156543 m/px, got {cell0}: {json}"
    );

    // A truly unknown id must still 404 — the fallback only covers the 4 built-in presets.
    let err = tms_http::tile_matrix_set_doc(&state, "nope-not-a-grid")
        .expect_err("unknown grid id must still fail even with the fallback in place");
    assert_eq!(err.0, 404);
}

// ---- Tiled raster tiles for a VECTOR layer ---------------------------------------------------
//
// A vector layer used to be rejected on every tile path ("vector layer is not tiled — use WMS
// GetMap"), so the only way to raster it was one uncached, untiled GetMap over the whole viewport.
// These pin the tile path: it renders, and it renders GEOMETRY ONLY (docs/postgis-layers.md,
// "Labels are WMS-only").

use std::sync::Arc;
use terraserve::config::GridConfig;
use terraserve::server::{Layer, PublishedGrid, ServeState, VectorLayer};
use terraserve::vector::gpkg::GpkgSource;
use terraserve::vector::shape::Shaper;
use terraserve::vector::source::{FeatureSource, VectorSource};
use terraserve::vector::style::{
    FeatureTypeStyle, LabelPart, PolygonSym, Rule, Style, Symbolizer, TextSym,
};

const MINI_GPKG: &str = "fixtures/gpkg/mini.gpkg";
/// The custom grid `mini_vector_layer` publishes: one zoom, one tile, covering the whole fixture.
const MINI_GRID: &str = "minigrid";

/// A grid whose z0/0/0 tile covers `ext` entirely (origin = its top-left corner, one tile wide).
/// Built from the fixture's OWN extent rather than a preset so the single test tile is guaranteed
/// to contain the data — a WebMercatorQuad z0 tile would put these 3 features in a few pixels.
fn mini_grid(ext: [f64; 4]) -> terraserve::tms::TileMatrixSet {
    GridConfig {
        crs: "EPSG:4326".to_string(),
        origin: [ext[0], ext[3]],
        extent: ext,
        tile_px: 256,
        resolutions: vec![(ext[2] - ext[0]) / 256.0],
    }
    .to_tms(MINI_GRID)
}

fn plain_rule(sym: Symbolizer) -> Style {
    Style {
        feature_type_styles: vec![FeatureTypeStyle {
            rules: vec![Rule {
                filter: None,
                else_filter: false,
                min_scale: None,
                max_scale: None,
                symbolizers: vec![sym],
                title: None,
            }],
        }],
    }
}

/// Geometry only — what a tile is expected to draw.
fn polygon_style() -> Style {
    plain_rule(Symbolizer::Polygon(PolygonSym {
        fill: [180, 200, 180, 255],
        stroke: Some([60, 60, 60, 255]),
        stroke_width: 1.0,
    }))
}

/// Labels only, nothing else — so "did the tile suppress labels?" is answerable from the pixels:
/// WMS draws text here, the tile must be empty.
fn text_only_style() -> Style {
    plain_rule(Symbolizer::Text(TextSym {
        label: vec![LabelPart::Field("name".to_string())],
        priority: None,
        priority_higher_wins: false,
        size: 24.0,
        color: [255, 255, 255, 255],
        halo_color: [0, 0, 0, 255],
        halo_radius: 2.0,
        offset: 0.0,
    }))
}

/// `fixtures/gpkg/mini.gpkg` (2 polygons + 1 LineString, EPSG:4326) as a vector layer publishing
/// `mini_grid`. No COG, no `layer.style` — exactly the shape that used to 400 on every tile route.
fn mini_vector_layer(style: Style) -> Layer {
    let src = Arc::new(GpkgSource::load(MINI_GPKG, None).expect("load fixtures/gpkg/mini.gpkg"));
    let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
    let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
    let ext = src.full_extent();
    Layer {
        name: "mini".into(),
        cog_path: String::new(),
        cog: None,
        source: None,
        style: None,
        src_crs: "EPSG:4326".into(),
        band_math: None,
        bounds_wgs84: ext,
        tile_cache: None,
        index_cache: terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes()),
        grids: vec![PublishedGrid {
            tms: mini_grid(ext),
            data_bounds: None,
        }],
        vector: Some(VectorLayer {
            fields: terraserve::mvt_http::feature_field_schema(src.as_ref()),
            area_scale: 0.0,     // size-gate calibration, unused here
            min_feature_px: 0.0, // size gate off (the default)
            source: VectorSource::LoadAll(src),
            style,
            shaper,
            lod: None,
        }),
        pmtiles: std::collections::BTreeMap::new(),
        raster_pmtiles: std::collections::BTreeMap::new(),
        overlay: std::collections::BTreeMap::new(),
    }
}

/// Decode a PNG to straight RGBA8 — the assertions below are about PIXELS. A 200 carrying an error
/// string, or a uniformly transparent image, is the failure mode this project keeps hitting, and
/// neither is distinguishable from success without decoding.
fn decode_rgba(bytes: &[u8]) -> Vec<u8> {
    assert!(
        bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        "not a PNG: {:?}",
        String::from_utf8_lossy(&bytes[..bytes.len().min(120)])
    );
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .expect("PNG header");
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("PNG frame");
    assert_eq!(info.color_type, png::ColorType::Rgba, "RGBA8 tile");
    buf.truncate(info.buffer_size());
    buf
}

fn opaque_px(rgba: &[u8]) -> usize {
    rgba.chunks_exact(4).filter(|p| p[3] > 0).count()
}

/// The tile path renders a vector layer. Before this it returned `(400, "vector layer is not
/// tiled — use WMS GetMap")`, so there was no raster tile pyramid for a vector layer at all.
#[test]
fn tms_tile_renders_a_vector_layer_as_png() {
    let st = ServeState::new(
        vec![mini_vector_layer(polygon_style())],
        "http://h/wms".into(),
        16,
    );
    let bytes = tms_http::render_tms_tile(&st, "mini@minigrid", 0, 0, 0)
        .expect("a vector layer must serve a TMS raster tile");
    let rgba = decode_rgba(&bytes);
    assert!(
        opaque_px(&rgba) > 100,
        "z0/0/0 covers the whole fixture, so the polygons must paint it: {} opaque px",
        opaque_px(&rgba)
    );
}

/// The label decision, asserted from pixels: with a TEXT-ONLY style the tile is fully transparent,
/// while the SAME features/style/bbox through the WMS-side renderer draw text. The WMS half is what
/// keeps this from going vacuous — without it a broken renderer that draws nothing would pass.
#[test]
fn tms_tile_suppresses_labels_the_wms_path_draws() {
    let layer = mini_vector_layer(text_only_style());
    let grid = layer.grids[0].tms.clone();
    let bbox = grid.tile_bounds(0, 0, 0).unwrap();

    // The WMS-side renderer (`render_vector`, labels always on) over the same features, style,
    // bbox and pixel size the tile below uses.
    let src = GpkgSource::load(MINI_GPKG, None).unwrap();
    let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
    let sh = Shaper::from_font_bytes(&font).unwrap();
    let wms = terraserve::vector::render::render_vector(
        &src,
        &text_only_style(),
        "EPSG:4326",
        &grid.crs,
        bbox,
        grid.tile_w,
        grid.tile_h,
        &sh,
    )
    .unwrap();
    assert!(
        opaque_px(&wms) > 0,
        "the WMS path must draw the labels, else this test proves nothing"
    );

    let st = ServeState::new(vec![layer], "http://h/wms".into(), 16);
    let bytes = tms_http::render_tms_tile(&st, "mini@minigrid", 0, 0, 0).expect("tile renders");
    let tile = decode_rgba(&bytes);
    assert_eq!(
        opaque_px(&tile),
        0,
        "labels must be suppressed on the tile path (per-tile placement clips/duplicates at seams)"
    );
}
