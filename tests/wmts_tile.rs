//! WMTS get_tile: byte-identical to the TMS front-end at the flipped y (proves the two front-ends'
//! y conventions are consistent — WMTS top-left, TMS bottom-left), plus a data-row check and error
//! codes. Skips if the polar fixture is absent.

use std::sync::Arc;

use terraserve::cog::{self, LocalFileRangeSource};
use terraserve::expr;
use terraserve::render::BandMath;
use terraserve::s3::{AnySource, S3Config};
use terraserve::server::{Layer, PublishedGrid, ServeState};
use terraserve::style::Style;
use terraserve::tms::{self, TileMatrixSet};
use terraserve::{tms_http, wmts};

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
        bounds_wgs84: [-40.0, 68.0, -28.0, 70.0],
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
fn wmts_tile_equals_tms_at_flipped_y() {
    let Some(st) = state() else { return };
    let grid = &st.layers[0].grids[0].tms;
    let z = grid.levels.len() as u32 - 1; // finest
    let lvl = grid.level(z).unwrap();
    assert!(
        lvl.matrix_h >= 2,
        "need a multi-row level to exercise the flip"
    );
    let (row, col) = (0u32, lvl.matrix_w / 2); // north edge, mid column -> has data

    // WMTS TileRow=row (top-left) must equal the TMS tile at y = matrix_h-1-row (bottom-left).
    let wmts_png = wmts::get_tile(&st, "arctic", "default", "from_cog", z, row, col).unwrap();
    let tms_y = lvl.matrix_h - 1 - row;
    assert_ne!(tms_y, row, "flip must actually differ");
    let tms_png = tms_http::render_tms_tile(&st, "arctic@from_cog", z, col, tms_y).unwrap();
    assert_eq!(
        wmts_png, tms_png,
        "WMTS(row) must equal TMS(matrix_h-1-row) byte-for-byte"
    );

    // The north-edge data tile is a real (large) PNG; the padded SE corner is transparent (small).
    let corner = wmts::get_tile(
        &st,
        "arctic",
        "default",
        "from_cog",
        z,
        lvl.matrix_h - 1,
        lvl.matrix_w - 1,
    )
    .unwrap();
    assert!(
        wmts_png.len() > corner.len() * 4,
        "data tile ({}) should dwarf the empty corner ({})",
        wmts_png.len(),
        corner.len()
    );
}

#[test]
fn wmts_tile_error_codes() {
    let Some(st) = state() else { return };
    let grid = &st.layers[0].grids[0].tms;
    let z = grid.levels.len() as u32 - 1;
    let mw = grid.level(z).unwrap().matrix_w;

    let e = wmts::get_tile(&st, "nope", "default", "from_cog", z, 0, 0).unwrap_err();
    assert_eq!(e.code, "InvalidParameterValue");
    assert_eq!(e.locator.as_deref(), Some("LAYER"));

    let e = wmts::get_tile(&st, "arctic", "default", "NoGrid", z, 0, 0).unwrap_err();
    assert_eq!(e.locator.as_deref(), Some("TILEMATRIXSET"));

    let e = wmts::get_tile(&st, "arctic", "default", "from_cog", z, 0, mw).unwrap_err();
    assert_eq!(e.code, "TileOutOfRange");
}
