//! Integration: the WMS surface serving a vector (label) layer.

use std::sync::Arc;
use terraserve::server::{Layer, VectorLayer};
use terraserve::vector::geojson::GeoJsonSource;
use terraserve::vector::shape::Shaper;
use terraserve::vector::source::{FeatureSource, VectorSource};
use terraserve::vector::style::Style;

fn vector_layer() -> Layer {
    let src = Arc::new(GeoJsonSource::load("fixtures/vector/airports.geojson").unwrap());
    let text = std::fs::read_to_string("fixtures/styles/airports.vec.json").unwrap();
    let style = Style::from_json_str(&text).unwrap();
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
fn getmap_renders_vector_layer_as_png() {
    let layers = vec![vector_layer()];
    let q = format!(
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=airports&STYLES=&{EUROPE_3857}&FORMAT=image/png"
    );
    let r = terraserve::wms::handle_layers(&layers, &q, None);
    assert!(r.bytes.starts_with(&[0x89, b'P', b'N', b'G']), "PNG output");
    assert!(
        r.bytes.len() > 1000,
        "non-trivial PNG ({} bytes)",
        r.bytes.len()
    );
}

#[test]
fn getfeatureinfo_vector_never_panics() {
    let layers = vec![vector_layer()];
    let q = format!(
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS=airports&QUERY_LAYERS=airports&\
         STYLES=&{EUROPE_3857}&I=256&J=256&INFO_FORMAT=text/plain"
    );
    let r = terraserve::wms::handle_layers(&layers, &q, None);
    assert!(!r.bytes.is_empty(), "GFI returns a body, no panic");
    assert_eq!(r.content_type.as_deref(), Some("text/plain"));
}

#[test]
fn getcapabilities_advertises_vector_layer() {
    let layers = vec![vector_layer()];
    let q = "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetCapabilities";
    let r = terraserve::wms::handle_layers(&layers, q, Some("http://h/wms"));
    let xml = String::from_utf8_lossy(&r.bytes);
    assert!(
        xml.contains("airports"),
        "capabilities advertise the vector layer"
    );
}

#[test]
fn wmts_capabilities_omits_grid_less_vector_layer() {
    // A vector layer has no tile grids; advertising it as a WMTS <Layer> would be schema-invalid
    // (0 TileMatrixSetLink) and every GetTile would 400. It must be omitted (regression guard).
    let state = terraserve::server::ServeState::new(vec![vector_layer()], "http://h".into(), 4);
    let xml = terraserve::wmts::capabilities_xml(&state, "http://h/wmts", "http://h/wmts/1.0.0");
    assert!(
        !xml.contains("<Layer>"),
        "vector layer must not appear as a WMTS <Layer>"
    );
}
