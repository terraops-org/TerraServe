//! WMTS GetFeatureInfo: KVP parse + the value read over a tile's in-tile pixel (reuses the WMS
//! sample_point + formatters). Skips if the polar fixture is absent.

use std::sync::Arc;

use terraserve::cog::{self, LocalFileRangeSource};
use terraserve::expr;
use terraserve::render::BandMath;
use terraserve::s3::{AnySource, S3Config};
use terraserve::server::{Layer, PublishedGrid, ServeState};
use terraserve::style::Style;
use terraserve::tms::{self, TileMatrixSet};
use terraserve::wmts::{self, WmtsRequest};

#[test]
fn parses_getfeatureinfo_kvp() {
    let q = "service=WMTS&request=GetFeatureInfo&layer=arctic&style=default&format=image/png&\
             tilematrixset=g&tilematrix=3&tilerow=2&tilecol=1&i=100&j=50&infoformat=application/json";
    match wmts::parse_kvp(q) {
        WmtsRequest::GetFeatureInfo {
            layer,
            z,
            row,
            col,
            i,
            j,
            info_format,
            ..
        } => {
            assert_eq!(layer, "arctic");
            assert_eq!((z, row, col), (3, 2, 1));
            assert_eq!((i, j), (100, 50));
            assert_eq!(info_format, "application/json");
        }
        other => panic!("{other:?}"),
    }
    // Missing I -> MissingParameterValue.
    match wmts::parse_kvp(
        "request=GetFeatureInfo&layer=a&tilematrixset=g&tilematrix=0&tilerow=0&tilecol=0&j=0",
    ) {
        WmtsRequest::Exception { code, .. } => assert_eq!(code, "MissingParameterValue"),
        o => panic!("{o:?}"),
    }
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
    let grid = TileMatrixSet::from_cog(&cog, "EPSG:3413", 256);
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
fn wmts_featureinfo_reports_value_on_a_data_tile() {
    let Some(st) = state() else { return };
    let grid = &st.layers[0].grids[0].tms;
    // A z0 tile that contains data (from_cog anchors data at the top-left), center in-tile pixel.
    let z = 0u32;
    let (i, j) = (grid.tile_w / 2, grid.tile_h / 2);
    let (body, ct) = wmts::get_feature_info(
        &st,
        "arctic",
        "default",
        "from_cog",
        z,
        0,
        0,
        i,
        j,
        "application/json",
    )
    .unwrap();
    assert_eq!(ct, "application/json");
    let s = String::from_utf8(body).unwrap();
    assert!(s.contains("\"value\":"), "expected a value: {s}");

    // I/J outside the tile -> error.
    assert!(wmts::get_feature_info(
        &st,
        "arctic",
        "default",
        "from_cog",
        z,
        0,
        0,
        grid.tile_w,
        0,
        "text/plain"
    )
    .is_err());
    // Unknown TileMatrixSet -> error.
    assert!(wmts::get_feature_info(
        &st,
        "arctic",
        "default",
        "nope",
        z,
        0,
        0,
        i,
        j,
        "text/plain"
    )
    .is_err());
}

#[test]
fn capabilities_advertise_wmts_getfeatureinfo() {
    let Some(st) = state() else { return };
    let xml = wmts::capabilities_xml(&st, "http://h/wmts", "http://h/wmts/1.0.0");
    assert!(
        xml.contains("name=\"GetFeatureInfo\""),
        "caps must advertise GetFeatureInfo"
    );
    assert!(
        xml.contains("<InfoFormat>application/json</InfoFormat>"),
        "caps must list InfoFormat"
    );
}
