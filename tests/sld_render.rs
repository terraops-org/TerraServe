//! Task 8 integration test: `Style::load` dispatches `.sld` through the SLD lowering pass, and a
//! vector layer styled by a real `.sld` file renders over WMS GetMap.

use std::sync::Arc;
use terraserve::server::{Layer, VectorLayer};
use terraserve::vector::geojson::GeoJsonSource;
use terraserve::vector::shape::Shaper;
use terraserve::vector::source::{FeatureSource, VectorSource};
use terraserve::vector::style::{Style, Symbolizer};

const POINT_MIN_SLD: &str = "tests/fixtures/sld/point_min.sld";

fn sld_vector_layer() -> Layer {
    let src = Arc::new(GeoJsonSource::load("fixtures/vector/airports.geojson").unwrap());
    let style = Style::load(POINT_MIN_SLD).expect("point_min.sld should load");
    let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
    let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
    let ext = src.full_extent();
    Layer {
        name: "airports".into(),
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
            area_scale: 0.0, // MVT-only knob, unused by this test
            source: VectorSource::LoadAll(src),
            style,
            shaper,
            lod: None,
        }),
        pmtiles: None,
        overlay: None,
    }
}

const EUROPE_3857: &str =
    "CRS=EPSG:3857&BBOX=-1500000,4000000,3000000,8000000&WIDTH=512&HEIGHT=512";

#[test]
fn getmap_renders_sld_styled_vector_layer_as_png() {
    let layers = vec![sld_vector_layer()];
    let q = format!(
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=airports&STYLES=&{EUROPE_3857}&FORMAT=image/png"
    );
    let r = terraserve::wms::handle_layers(&layers, &q, None);
    assert!(r.bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47]), "PNG magic");
    assert!(
        r.bytes.len() > 1000,
        "non-trivial PNG ({} bytes)",
        r.bytes.len()
    );
}

#[test]
fn style_load_dispatches_sld_by_extension_and_content() {
    let style = Style::load(POINT_MIN_SLD).expect("point_min.sld should load via Style::load");
    assert!(
        !style.feature_type_styles[0].rules.is_empty(),
        "at least one rule lowered"
    );
    let rule = &style.feature_type_styles[0].rules[0];
    let has_point = rule
        .symbolizers
        .iter()
        .any(|s| matches!(s, Symbolizer::Point(_)));
    let has_text = rule
        .symbolizers
        .iter()
        .any(|s| matches!(s, Symbolizer::Text(_)));
    assert!(has_point, "point_min.sld rule has a Point symbolizer");
    assert!(has_text, "point_min.sld rule has a Text symbolizer");
}

#[test]
fn style_load_dispatches_json_unchanged() {
    let style = Style::load("fixtures/styles/airports.vec.json")
        .expect("airports.vec.json should load via Style::load");
    assert_eq!(
        style.feature_type_styles[0].rules.len(),
        1,
        "JSON front-end still one-rule shim"
    );
    assert_eq!(style.feature_type_styles[0].rules[0].symbolizers.len(), 2);
}
