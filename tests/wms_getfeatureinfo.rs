//! WMS GetFeatureInfo through the full `wms::handle_layers` dispatch: content-type + body per
//! INFO_FORMAT, and the capabilities advertise the operation. Skips if the polar fixture is absent.

use std::sync::Arc;

use terraserve::cog::{self, LocalFileRangeSource};
use terraserve::expr;
use terraserve::render::BandMath;
use terraserve::s3::{AnySource, S3Config};
use terraserve::server::Layer;
use terraserve::style::Style;
use terraserve::wms;

const PATH: &str = "../cogs/polar/arcticdem_18_47_32m_gunnbjorn_dem.tif";

fn layer() -> Option<Layer> {
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
    Some(Layer {
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
        grids: vec![],
    })
}

fn native_query(l: &Layer, info_format: &str) -> String {
    let lv = &l.cog.as_ref().unwrap().levels[0];
    let g = lv.geo;
    let (minx, maxy) = (g.origin_x, g.origin_y);
    let (maxx, miny) = (
        g.origin_x + lv.width as f64 * g.px,
        g.origin_y - lv.height as f64 * g.py,
    );
    let (w, h) = (lv.width, lv.height);
    let (i, j) = (w / 2, h / 2);
    format!(
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS=arctic&QUERY_LAYERS=arctic&\
         CRS=EPSG:3413&BBOX={minx},{miny},{maxx},{maxy}&WIDTH={w}&HEIGHT={h}&I={i}&J={j}&INFO_FORMAT={info_format}"
    )
}

#[test]
fn getfeatureinfo_json_reports_exact_value() {
    let Some(l) = layer() else { return };
    let layers = vec![l];
    let q = native_query(&layers[0], "application/json");
    let r = wms::handle_layers(&layers, &q, None);
    assert_eq!(r.content_type.as_deref(), Some("application/json"));
    let body = String::from_utf8(r.bytes).unwrap();
    assert!(body.contains("\"type\":\"FeatureCollection\""), "{body}");
    assert!(body.contains("\"band_1\":"), "{body}");
    assert!(body.contains("\"value\":"), "{body}");
}

#[test]
fn getfeatureinfo_plain_and_outside() {
    let Some(l) = layer() else { return };
    let layers = vec![l];

    let plain = wms::handle_layers(&layers, &native_query(&layers[0], "text/plain"), None);
    assert_eq!(plain.content_type.as_deref(), Some("text/plain"));
    assert!(String::from_utf8(plain.bytes).unwrap().contains("band_1:"));

    // A point west of the data -> outside coverage.
    let lv = &layers[0].cog.as_ref().unwrap().levels[0];
    let g = lv.geo;
    let far = format!(
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS=arctic&QUERY_LAYERS=arctic&\
         CRS=EPSG:3413&BBOX={},{},{},{}&WIDTH=16&HEIGHT=16&I=8&J=8&INFO_FORMAT=text/plain",
        g.origin_x - 1_000_000.0,
        g.origin_y - 100_000.0,
        g.origin_x - 900_000.0,
        g.origin_y,
    );
    let out = wms::handle_layers(&layers, &far, None);
    assert!(String::from_utf8(out.bytes)
        .unwrap()
        .contains("outside coverage"));
}

#[test]
fn capabilities_advertise_getfeatureinfo_and_queryable() {
    let Some(l) = layer() else { return };
    let layers = vec![l];
    let caps = wms::handle_layers(
        &layers,
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetCapabilities",
        None,
    );
    let xml = String::from_utf8(caps.bytes).unwrap();
    assert!(
        xml.contains("<GetFeatureInfo>"),
        "caps must advertise GetFeatureInfo"
    );
    assert!(
        xml.contains("application/json"),
        "caps must list the JSON info format"
    );
    assert!(xml.contains("queryable=\"1\""), "layers must be queryable");
}
