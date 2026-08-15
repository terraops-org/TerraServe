//! A source read that FAILS must reach the client as an error, never as an empty tile behind a
//! 200 OK.
//!
//! This is the regression test for a bug that shipped in every windowed reader at once: PostGIS
//! logged a line and returned `Vec::new()`, GeoPackage swallowed six different SQLite errors the
//! same way, and FlatGeoBuf did it with no log at all. A dropped connection, a statement timeout
//! or a truncated S3 range read therefore arrived as a blank map with a success status -- and
//! `build-pmtiles` froze that blank into an archive, where a read-through hit shadows the live
//! path permanently. Two EU5 bakes lost 10 and 4 tiles that way, silently.
//!
//! The stub below is a `WindowedSource` that always fails. It stands in for all three real readers
//! because the fix is at the seam they share: `WindowedSource::query` returns `Result`, and every
//! front-end above it turns `Err` into a 500 / OWS exception.

use std::sync::Arc;

use terraserve::server::{Layer, PublishedGrid, ServeState, VectorLayer};
use terraserve::vector::feature::Feature;
use terraserve::vector::source::{VectorSource, WindowedSource};

const BOOM: &str = "simulated source failure: connection closed";

/// A windowed source whose every read fails. NOT an empty source -- the whole point is that the
/// two must not look alike downstream.
struct FailingSource;

impl WindowedSource for FailingSource {
    fn query(&self, _bbox: [f64; 4]) -> Result<Vec<Feature>, String> {
        Err(BOOM.to_string())
    }
    fn full_extent(&self) -> [f64; 4] {
        [-10.0, -10.0, 10.0, 10.0]
    }
    fn crs(&self) -> Option<&str> {
        Some("EPSG:4326")
    }
}

fn style() -> terraserve::vector::style::Style {
    let text = std::fs::read_to_string("fixtures/styles/airports.vec.json").unwrap();
    terraserve::vector::style::Style::from_json_str(&text).unwrap()
}

fn state_with_failing_layer() -> ServeState {
    let shaper = Arc::new(
        terraserve::vector::shape::Shaper::from_font_bytes(
            &std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap(),
        )
        .unwrap(),
    );
    let ext = [-10.0, -10.0, 10.0, 10.0];
    let layer = Layer {
        name: "boom".into(),
        cog_path: String::new(),
        cog: None,
        source: None,
        style: None,
        src_crs: "EPSG:4326".into(),
        band_math: None,
        bounds_wgs84: ext,
        tile_cache: None,
        index_cache: terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes()),
        // The TMS/WMTS raster route needs a published grid; the MVT route would fall back to
        // the preset, but this test exercises both.
        grids: vec![PublishedGrid {
            tms: terraserve::tms::preset("WebMercatorQuad", 256).unwrap(),
            data_bounds: None,
        }],
        vector: Some(VectorLayer {
            fields: std::collections::BTreeMap::new(),
            area_scale: 0.0,
            min_feature_px: 0.0,
            source: VectorSource::Windowed(Arc::new(FailingSource)),
            style: style(),
            shaper,
            lod: None,
        }),
        pmtiles: std::collections::BTreeMap::new(),
        raster_pmtiles: std::collections::BTreeMap::new(),
        overlay: std::collections::BTreeMap::new(),
    };
    ServeState::new(vec![layer], "http://localhost/wms".into(), 4)
}

/// The RASTER tile path (TMS): the failure must surface as 5xx, not as a valid transparent PNG.
#[test]
fn tms_raster_tile_reports_a_failed_read_instead_of_serving_a_blank_tile() {
    let st = state_with_failing_layer();
    let out = terraserve::tms_http::render_tms_tile(&st, "boom", 2, 2, 2);
    match out {
        Ok(png) => panic!(
            "a failed source read was served as a {}-byte tile with success -- the silent-blank bug",
            png.len()
        ),
        Err((code, msg)) => {
            assert_eq!(code, 500, "a broken read is a server error, not a 404/200");
            assert!(msg.contains(BOOM), "the cause must reach the operator: {msg}");
        }
    }
}

/// The VECTOR tile path (`/mvt`): same rule. An empty MVT body is a perfectly valid tile, which is
/// precisely why returning one here would be undetectable.
#[test]
fn mvt_tile_reports_a_failed_read_instead_of_encoding_an_empty_tile() {
    let st = state_with_failing_layer();
    let out = terraserve::mvt_http::render_mvt_tile(&st, "boom", "WebMercatorQuad", 2, 2, 2);
    match out {
        Ok(mvt) => panic!(
            "a failed source read was encoded as a {}-byte MVT with success",
            mvt.len()
        ),
        Err((code, msg)) => {
            assert_eq!(code, 500);
            assert!(
                msg.contains(BOOM),
                "the cause must reach the operator: {msg}"
            );
        }
    }
}

/// The contrast that makes the two tests above mean something: a source that reads FINE and simply
/// has nothing in the window still returns a tile with success. "Empty" and "broken" are different
/// answers, and only this test proves the fix did not turn every empty tile into an error.
struct EmptySource;

impl WindowedSource for EmptySource {
    fn query(&self, _bbox: [f64; 4]) -> Result<Vec<Feature>, String> {
        Ok(Vec::new())
    }
    fn full_extent(&self) -> [f64; 4] {
        [-10.0, -10.0, 10.0, 10.0]
    }
    fn crs(&self) -> Option<&str> {
        Some("EPSG:4326")
    }
}

#[test]
fn an_empty_window_is_still_a_successful_tile() {
    let mut st = state_with_failing_layer();
    if let Some(v) = st.layers[0].vector.as_mut() {
        v.source = VectorSource::Windowed(Arc::new(EmptySource));
    }
    let png = terraserve::tms_http::render_tms_tile(&st, "boom", 2, 2, 2)
        .expect("an empty window is not a failure");
    assert!(!png.is_empty(), "an empty window still renders a PNG");
}
