//! WMTS GetCapabilities: the TileMatrixSet XML (reference numbers + axis order) and a full-document
//! structural check (namespaces, OWS Layer child order, row-first limits, ResourceURL template).

use std::sync::Arc;

use terraserve::cog::{self, LocalFileRangeSource};
use terraserve::expr;
use terraserve::render::BandMath;
use terraserve::s3::{AnySource, S3Config};
use terraserve::server::{Layer, PublishedGrid, ServeState};
use terraserve::style::Style;
use terraserve::tms::{self, TileMatrixSet};
use terraserve::wmts;

#[test]
fn tile_matrix_set_xml_matches_reference_projected() {
    let g = TileMatrixSet::web_mercator_quad(256);
    let xml = wmts::tile_matrix_set_xml(&g);
    assert!(xml.contains("<ows:Identifier>WebMercatorQuad</ows:Identifier>"));
    assert!(xml.contains("<ows:SupportedCRS>urn:ogc:def:crs:EPSG::3857</ows:SupportedCRS>"));
    assert!(
        xml.contains("<ScaleDenominator>559082264"),
        "scale denominator wrong"
    );
    // Projected CRS: TopLeftCorner is X Y.
    assert!(xml.contains("<TopLeftCorner>-20037508.3427892 20037508.3427892</TopLeftCorner>"));
    assert!(xml.contains("<MatrixWidth>1</MatrixWidth>"));
    assert!(
        xml.contains("GoogleMapsCompatible"),
        "wkss missing for canonical preset"
    );
    assert!(
        !xml.to_lowercase().contains("cellsize"),
        "no cellSize in WMTS 1.0.0"
    );
}

#[test]
fn geographic_top_left_corner_is_lat_lon() {
    let g = TileMatrixSet::world_crs84_quad(256);
    let xml = wmts::tile_matrix_set_xml(&g);
    // Geographic EPSG:4326: TopLeftCorner written Lat Lon (Y X) -> "90 -180".
    assert!(
        xml.contains("<TopLeftCorner>90 -180</TopLeftCorner>"),
        "geographic axis order wrong"
    );
    assert!(xml.contains("urn:ogc:def:crs:EPSG::4326"));
}

const PATH: &str = "../cogs/polar/arcticdem_18_47_32m_gunnbjorn_dem.tif";

fn state() -> Option<ServeState> {
    if !std::path::Path::new(PATH).exists() {
        eprintln!("skipping: polar fixture absent");
        return None;
    }
    let source = Arc::new(AnySource::open(PATH, &S3Config::default()).unwrap());
    let cog = Arc::new(cog::parse(&LocalFileRangeSource::open(PATH).unwrap()).unwrap());
    let bm = BandMath {
        program: expr::Program::compile("elev", &["elev"]).unwrap(),
        nodata: -9999.0,
    };
    let mut grid = TileMatrixSet::from_cog(&cog, "EPSG:3413", 256);
    grid.id = "arctic_native".into(); // mirror build_layer's per-layer uniquify
    let data_bounds = tms::bounds_in_grid_crs(&cog, "EPSG:3413", "EPSG:3413");
    let layer = Layer {
        name: "arctic".into(),
        cog_path: PATH.into(),
        cog: Some(cog),
        source: Some(source),
        style: Some(Style::load("fixtures/styles/dem.json").unwrap()),
        src_crs: "EPSG:3413".into(),
        band_math: Some(bm),
        bounds_wgs84: [-30.4, 68.0, -27.3, 69.2],
        tile_cache: None,
        index_cache: terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes()),
        vector: None,
        pmtiles: None,
        overlay: None,
        grids: vec![PublishedGrid {
            tms: grid,
            data_bounds,
        }],
    };
    Some(ServeState::new(vec![layer], "http://h/wms".into(), 16))
}

#[test]
fn capabilities_document_is_structurally_conformant() {
    let Some(st) = state() else { return };
    let xml = wmts::capabilities_xml(&st, "http://h/wmts", "http://h/wmts/1.0.0");

    // Root element + required namespaces.
    assert!(xml.contains("<Capabilities xmlns=\"http://www.opengis.net/wmts/1.0\""));
    assert!(xml.contains("xmlns:ows=\"http://www.opengis.net/ows/1.1\""));
    assert!(xml.contains("xmlns:xlink=\"http://www.w3.org/1999/xlink\""));
    assert!(xml.contains("<ows:ServiceType>OGC WMTS</ows:ServiceType>"));

    // WR4: in <Layer>, ows:WGS84BoundingBox MUST precede ows:Identifier.
    let bbox = xml.find("<ows:WGS84BoundingBox>").unwrap();
    let ident = xml.find("<ows:Identifier>arctic</ows:Identifier>").unwrap();
    assert!(bbox < ident, "WGS84BoundingBox must come before Identifier");

    // WR6: one ResourceURL per layer with {TileMatrixSet} etc. as client variables, matching the route.
    assert!(xml.contains("template=\"http://h/wmts/1.0.0/arctic/{style}/{TileMatrixSet}/{TileMatrix}/{TileRow}/{TileCol}.png\""));

    // WR5: TileMatrixSetLimits nested in the link, row-first (MinTileRow before MinTileCol).
    let link = xml.find("<TileMatrixSetLink>").unwrap();
    let limits = xml.find("<TileMatrixSetLimits>").unwrap();
    assert!(link < limits, "limits nest inside the link");
    let minrow = xml.find("<MinTileRow>").unwrap();
    let mincol = xml.find("<MinTileCol>").unwrap();
    assert!(minrow < mincol, "TileMatrixLimits must be row-first");

    // Both bindings advertised: KVP constraint + RESTful ResourceURL + self-link.
    assert!(xml.contains("<ows:Value>KVP</ows:Value>"));
    assert!(xml
        .contains("<ServiceMetadataURL xlink:href=\"http://h/wmts/1.0.0/WMTSCapabilities.xml\"/>"));

    // The embedded TileMatrixSet id matches what the layer links to (no orphan reference).
    assert!(xml.contains("<TileMatrixSet>arctic_native</TileMatrixSet>")); // link
    assert!(xml.contains("<ows:Identifier>arctic_native</ows:Identifier>")); // definition
}
