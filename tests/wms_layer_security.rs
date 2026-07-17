//! Two WMS correctness/security fixes verified through the full `wms::handle_layers` dispatch:
//!
//!  - SEC-1: layer Name/Title/CRS values are XML-escaped in GetCapabilities (a `&`/`<` in a
//!    configured layer name must not break well-formedness or let a name forge extra XML).
//!  - Unknown `LAYERS`/`QUERY_LAYERS`/`LAYER` values must produce a `LayerNotDefined` WMS
//!    exception, never a silent fallback that renders a *different* layer's data.

use std::sync::Arc;
use terraserve::server::{Layer, VectorLayer};
use terraserve::vector::geojson::GeoJsonSource;
use terraserve::vector::shape::Shaper;
use terraserve::vector::source::{FeatureSource, VectorSource};
use terraserve::vector::style::Style;

/// A minimal, cheap-to-build vector layer (no COG/raster fixtures needed) with a caller-chosen
/// name, reusing the same fixtures as `tests/vector_wms.rs`.
fn layer_named(name: &str) -> Layer {
    let src = Arc::new(GeoJsonSource::load("fixtures/vector/airports.geojson").unwrap());
    let text = std::fs::read_to_string("fixtures/styles/airports.vec.json").unwrap();
    let style = Style::from_json_str(&text).unwrap();
    let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
    let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
    let ext = src.full_extent();
    Layer {
        name: name.to_string(),
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

// ---- Fix A (SEC-1): escape layer Name/Title/CRS in GetCapabilities -------------------------

#[test]
fn capabilities_escapes_ampersand_and_angle_brackets_in_layer_name() {
    let layers = vec![layer_named("roads & rail <injected>")];
    let r = terraserve::wms::handle_layers(
        &layers,
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetCapabilities",
        None,
    );
    let xml = String::from_utf8(r.bytes).expect("utf8 xml");

    // The dangerous characters must show up escaped, not raw.
    assert!(
        xml.contains("roads &amp; rail &lt;injected&gt;"),
        "expected the escaped layer name in output:\n{xml}"
    );
    assert!(
        !xml.contains("roads & rail <injected>"),
        "raw, unescaped layer name must not appear:\n{xml}"
    );

    // The whole document must still be well-formed XML.
    roxmltree::Document::parse(&xml).expect("GetCapabilities must be well-formed XML");
}

#[test]
fn capabilities_layer_name_cannot_forge_extra_xml_elements() {
    // A classic XML-injection payload: if unescaped, this would close </Name></Layer> early
    // and open a forged sibling <Layer><Name>evil</Name>.
    let evil = "x</Name></Layer><Layer><Name>evil";
    let layers = vec![layer_named(evil)];
    let r = terraserve::wms::handle_layers(
        &layers,
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetCapabilities",
        None,
    );
    let xml = String::from_utf8(r.bytes).expect("utf8 xml");

    // Must parse as XML with exactly the one real layer <Name>, not a forged extra one.
    let doc = roxmltree::Document::parse(&xml).expect("GetCapabilities must be well-formed XML");
    let names: Vec<&str> = doc
        .descendants()
        .filter(|n| n.has_tag_name("Name") && n.parent().is_some_and(|p| p.has_tag_name("Layer")))
        .filter_map(|n| n.text())
        .collect();
    assert_eq!(
        names,
        vec![evil],
        "the injected payload must round-trip as ONE literal layer name, not forge XML structure"
    );
    assert!(
        !xml.contains("<Layer><Name>evil"),
        "the payload must not have forged a raw sibling <Layer> element:\n{xml}"
    );
}

#[test]
fn capabilities_native_crs_token_is_escaped() {
    // src_crs is also interpolated (as the extra native <CRS>/<SRS> entry); it must be escaped
    // too, even though real CRS tokens are never this adversarial in practice.
    let src = Arc::new(GeoJsonSource::load("fixtures/vector/airports.geojson").unwrap());
    let text = std::fs::read_to_string("fixtures/styles/airports.vec.json").unwrap();
    let style = Style::from_json_str(&text).unwrap();
    let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
    let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
    let ext = src.full_extent();
    let layers = vec![Layer {
        name: "airports".into(),
        cog_path: String::new(),
        cog: None,
        source: None,
        style: None,
        src_crs: "EPSG:1<x>&2".into(),
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
    }];
    let r = terraserve::wms::handle_layers(
        &layers,
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetCapabilities",
        None,
    );
    let xml = String::from_utf8(r.bytes).expect("utf8 xml");
    roxmltree::Document::parse(&xml).expect("GetCapabilities must be well-formed XML");
    assert!(
        xml.contains("EPSG:1&lt;X&gt;&amp;2") || xml.contains("EPSG:1&lt;x&gt;&amp;2"),
        "native CRS token must be escaped:\n{xml}"
    );
}

// ---- Fix B: unknown LAYERS must be a WMS exception, not a silent wrong-layer fallback -------

#[test]
fn getmap_with_unknown_layer_name_returns_layer_not_defined_exception() {
    let layers = vec![layer_named("airports")];
    let q = format!(
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=does-not-exist&STYLES=&{EUROPE_3857}&FORMAT=image/png"
    );
    let r = terraserve::wms::handle_layers(&layers, &q, None);

    assert!(
        !r.bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        "must NOT silently render the first/only configured layer as a PNG"
    );
    let body = String::from_utf8(r.bytes).expect("exception body is XML/UTF-8");
    assert!(
        body.contains("ServiceException"),
        "must be a WMS ServiceExceptionReport:\n{body}"
    );
    assert!(
        body.contains("LayerNotDefined"),
        "must carry/name the LayerNotDefined condition:\n{body}"
    );
    assert!(
        body.contains("does-not-exist"),
        "the exception should name the offending layer:\n{body}"
    );
}

#[test]
fn getfeatureinfo_with_unknown_query_layers_returns_layer_not_defined_exception() {
    let layers = vec![layer_named("airports")];
    let q = format!(
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS=airports&QUERY_LAYERS=nope&\
         STYLES=&{EUROPE_3857}&I=256&J=256&INFO_FORMAT=text/plain"
    );
    let r = terraserve::wms::handle_layers(&layers, &q, None);
    let body = String::from_utf8(r.bytes).expect("exception body is XML/UTF-8");
    assert!(
        body.contains("LayerNotDefined") && body.contains("nope"),
        "unknown QUERY_LAYERS must be a LayerNotDefined exception, not silently query 'airports':\n{body}"
    );
}

#[test]
fn getlegendgraphic_with_unknown_layer_returns_layer_not_defined_exception() {
    let layers = vec![layer_named("airports")];
    let q = "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetLegendGraphic&LAYER=ghost&FORMAT=image/png";
    let r = terraserve::wms::handle_layers(&layers, q, None);
    let body = String::from_utf8(r.bytes).expect("exception body is XML/UTF-8");
    assert!(
        body.contains("LayerNotDefined") && body.contains("ghost"),
        "unknown LAYER must be a LayerNotDefined exception:\n{body}"
    );
}

// ---- Backward compat: existing-layer GetMap + empty-LAYERS default still work ---------------

#[test]
fn getmap_with_existing_layer_name_still_renders() {
    let layers = vec![layer_named("airports")];
    let q = format!(
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=airports&STYLES=&{EUROPE_3857}&FORMAT=image/png"
    );
    let r = terraserve::wms::handle_layers(&layers, &q, None);
    assert!(
        r.bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        "a real, matching layer name must still render a PNG"
    );
}

#[test]
fn getmap_with_empty_layers_still_defaults_to_the_first_layer() {
    let layers = vec![layer_named("airports")];
    let q = format!(
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=&STYLES=&{EUROPE_3857}&FORMAT=image/png"
    );
    let r = terraserve::wms::handle_layers(&layers, &q, None);
    assert!(
        r.bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        "an empty LAYERS= must still default to the (only) configured layer, back-compat"
    );
}

#[test]
fn getmap_with_missing_layers_param_still_defaults_to_the_first_layer() {
    let layers = vec![layer_named("airports")];
    let q =
        format!("SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&STYLES=&{EUROPE_3857}&FORMAT=image/png");
    let r = terraserve::wms::handle_layers(&layers, &q, None);
    assert!(
        r.bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        "a missing LAYERS param must still default to the (only) configured layer, back-compat"
    );
}
