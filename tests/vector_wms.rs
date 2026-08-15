//! Integration: the WMS surface serving a vector (label) layer.

use std::sync::Arc;
use terraserve::server::{Layer, PublishedGrid, VectorLayer};
use terraserve::tms::TileMatrixSet;
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
            area_scale: 0.0,     // size-gate calibration, unused by this test
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

/// Like `vector_layer()`, but with a non-empty `grids` — the shape a vector layer has when its
/// config publishes grids. Since vector layers raster on the tile paths, this is the shape that
/// MUST be advertised: the grid it publishes is a grid it can serve.
fn vector_layer_with_grid() -> Layer {
    let mut l = vector_layer();
    l.grids = vec![PublishedGrid {
        tms: TileMatrixSet::web_mercator_quad(256),
        data_bounds: None,
    }];
    l
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
    // `grids:` unset (the vector default: `from_cog` is dropped for a layer with no COG). There is
    // no grid to raster on, and a <Layer> with 0 TileMatrixSetLink is schema-invalid, so it must be
    // omitted — the other half of "advertised == servable" from the test above.
    let state = terraserve::server::ServeState::new(vec![vector_layer()], "http://h".into(), 4);
    let xml = terraserve::wmts::capabilities_xml(&state, "http://h/wmts", "http://h/wmts/1.0.0");
    assert!(
        !xml.contains("<Layer>"),
        "vector layer must not appear as a WMTS <Layer>"
    );
}

#[test]
fn wmts_capabilities_advertises_a_vector_layer_with_grids() {
    // INVERTED with tiled raster vector layers: `get_tile` rasters a vector layer, so the `.png`
    // <ResourceURL> is real. While it 400'd, listing it advertised an always-400 endpoint; now
    // omitting it would hide a working endpoint from QGIS, which builds its layer list from here.
    let state =
        terraserve::server::ServeState::new(vec![vector_layer_with_grid()], "http://h".into(), 4);
    let xml = terraserve::wmts::capabilities_xml(&state, "http://h/wmts", "http://h/wmts/1.0.0");
    assert!(
        xml.contains("<Layer>") && xml.contains("<ows:Identifier>airports</ows:Identifier>"),
        "vector layer with grids must be advertised as a WMTS <Layer>: {xml}"
    );
    assert!(
        xml.contains("<TileMatrixSet>WebMercatorQuad</TileMatrixSet>"),
        "…linked to the grid it publishes: {xml}"
    );
    // …but NOT offering GetFeatureInfo: that stayed raster-only, so advertising the InfoFormats
    // would offer an operation that 400s on this layer (<InfoFormat> is 0..* in WMTS 1.0.0).
    assert!(
        !xml.contains("<InfoFormat>"),
        "a vector layer must not advertise GetFeatureInfo formats: {xml}"
    );
}

#[test]
fn tms_root_advertises_a_vector_layer_with_grids() {
    // INVERTED with tiled raster vector layers: `render_tms_tile` now rasters a vector layer
    // (`VectorLayer::render_tile`), so a `<TileMap>` href for a grid it publishes resolves to a
    // real PNG. While vector tiles 400'd, listing one here was advertising an always-400 endpoint;
    // omitting it now would be the opposite bug — a working endpoint no client can discover.
    let state =
        terraserve::server::ServeState::new(vec![vector_layer_with_grid()], "http://h".into(), 4);
    let root = terraserve::tms_http::tms_root(&state.base_url);
    let xml = terraserve::tms_http::tilemapservice_xml(&state, &root);
    assert!(
        xml.contains("<TileMap ") && xml.contains("airports@WebMercatorQuad"),
        "vector layer with grids must be advertised as a TMS <TileMap>: {xml}"
    );
}

#[test]
fn tms_root_omits_a_vector_layer_that_publishes_no_grids() {
    // The other half: `grids:` unset (the default `from_cog` is dropped for a vector layer) means
    // there is no grid to raster on, so there is nothing honest to advertise. Advertised == servable
    // in BOTH directions.
    let state = terraserve::server::ServeState::new(vec![vector_layer()], "http://h".into(), 4);
    let root = terraserve::tms_http::tms_root(&state.base_url);
    let xml = terraserve::tms_http::tilemapservice_xml(&state, &root);
    assert!(
        !xml.contains("<TileMap "),
        "a grid-less vector layer has no servable tile endpoint to advertise"
    );
}
