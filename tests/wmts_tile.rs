//! WMTS get_tile: byte-identical to the TMS front-end at the flipped y (proves the two front-ends'
//! y conventions are consistent — WMTS top-left, TMS bottom-left), plus a data-row check and error
//! codes. The COG tests skip if the polar fixture is absent.
//!
//! The VECTOR tests at the bottom need no COG (CI has no `../cogs`): they raster
//! `fixtures/gpkg/mini.gpkg` on the tile path, including a real-HTTP check of both WMTS bindings.

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
        pmtiles: std::collections::BTreeMap::new(),
        raster_pmtiles: std::collections::BTreeMap::new(),
        overlay: std::collections::BTreeMap::new(),
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

// ---- A VECTOR layer on the WMTS tile path ----------------------------------------------------
//
// A vector layer used to be rejected here with (400, "vector layer is not tiled — use WMS GetMap").
// It now rasters through `VectorLayer::render_tile`, the same one `tms_http` calls, which is in turn
// `wms::get_map_vector` with a bbox computed from the grid.

/// `fixtures/gpkg/mini.gpkg` (2 polygons + 1 LineString, EPSG:4326) published on the canonical
/// `WorldCRS84Quad` preset — a real grid, and geographic like the fixture, so no reprojection
/// stands between the source and the tile. The data (lon 0..30, lat 0..10) sits inside the z1
/// tile `row=0, col=2` (lon 0..90, lat 0..90).
fn vector_state() -> ServeState {
    use terraserve::server::VectorLayer;
    use terraserve::vector::gpkg::GpkgSource;
    use terraserve::vector::source::{FeatureSource, VectorSource};
    use terraserve::vector::style::{
        FeatureTypeStyle, PolygonSym, Rule, Style as VecStyle, Symbolizer,
    };

    let src = Arc::new(GpkgSource::load("fixtures/gpkg/mini.gpkg", None).expect("mini.gpkg"));
    let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
    let shaper = Arc::new(terraserve::vector::shape::Shaper::from_font_bytes(&font).unwrap());
    let style = VecStyle {
        feature_type_styles: vec![FeatureTypeStyle {
            rules: vec![Rule {
                filter: None,
                else_filter: false,
                min_scale: None,
                max_scale: None,
                symbolizers: vec![Symbolizer::Polygon(PolygonSym {
                    fill: [180, 200, 180, 255],
                    stroke: Some([60, 60, 60, 255]),
                    stroke_width: 1.0,
                })],
                title: None,
            }],
        }],
    };
    let ext = src.full_extent();
    let layer = Layer {
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
            tms: TileMatrixSet::world_crs84_quad(256),
            // `None`, as `build_vector_layer` leaves it: `data_bounds` drives the COG path's
            // empty-tile short-circuit, which the vector path has no equivalent of yet.
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
    };
    ServeState::new(vec![layer], "http://h/wms".into(), 16)
}

/// The tile containing the fixture: z1, `TileRow=0`, `TileCol=2` (see `vector_state`).
const V: (u32, u32, u32) = (1, 0, 2);

/// PNG magic + a decode, because a 200 carrying an error string, or a blank tile, would pass any
/// weaker assertion. Returns the count of non-transparent pixels.
fn png_opaque_px(bytes: &[u8]) -> usize {
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
    buf[..info.buffer_size()]
        .chunks_exact(4)
        .filter(|p| p[3] > 0)
        .count()
}

#[test]
fn wmts_gettile_renders_a_vector_layer_as_png() {
    let st = vector_state();
    let (z, row, col) = V;
    let png = wmts::get_tile(&st, "mini", "default", "WorldCRS84Quad", z, row, col)
        .expect("a vector layer must serve a WMTS raster tile");
    assert!(
        png_opaque_px(&png) > 100,
        "the fixture polygons must paint this tile, got {} opaque px",
        png_opaque_px(&png)
    );
}

/// The same consistency check the COG test above makes, on the vector path: WMTS `TileRow` is
/// top-left, TMS `y` is bottom-left, and both front-ends must reach the SAME renderer. A byte
/// difference here would mean two render paths had appeared.
#[test]
fn wmts_vector_tile_equals_tms_at_flipped_y() {
    let st = vector_state();
    let (z, row, col) = V;
    let grid = &st.layers[0].grids[0].tms;
    let mh = grid.level(z).unwrap().matrix_h;
    let tms_y = mh - 1 - row;
    assert_ne!(tms_y, row, "flip must actually differ");

    let a = wmts::get_tile(&st, "mini", "default", "WorldCRS84Quad", z, row, col).unwrap();
    let b = tms_http::render_tms_tile(&st, "mini@WorldCRS84Quad", z, col, tms_y).unwrap();
    assert!(
        png_opaque_px(&a) > 100,
        "the tile under test must have data"
    );
    assert_eq!(
        a, b,
        "WMTS(row) must equal TMS(matrix_h-1-row) byte-for-byte"
    );
}

/// Both WMTS bindings, over real HTTP: KVP `FORMAT=image/png` must reach the raster path (not the
/// MVT encoder, which the KVP handler selects on FORMAT alone), and the RESTful `.png` route must
/// serve the identical bytes. A route or dispatch mistake is invisible to a function-level test.
#[test]
fn wmts_vector_tile_over_http_kvp_and_restful() {
    use std::io::Read;
    use std::thread;
    use std::time::Duration;

    const PORT: u16 = 18741;
    thread::spawn(move || {
        let _ = terraserve::server::run(vector_state(), "127.0.0.1", PORT);
    });
    let (z, row, col) = V;
    let kvp = format!(
        "http://127.0.0.1:{PORT}/wmts?SERVICE=WMTS&VERSION=1.0.0&REQUEST=GetTile&LAYER=mini&\
         STYLE=default&FORMAT=image/png&TILEMATRIXSET=WorldCRS84Quad&TILEMATRIX={z}&TILEROW={row}&\
         TILECOL={col}"
    );
    let rest = format!(
        "http://127.0.0.1:{PORT}/wmts/1.0.0/mini/default/WorldCRS84Quad/{z}/{row}/{col}.png"
    );

    let fetch = |url: &str| -> Vec<u8> {
        let mut last = String::new();
        for _ in 0..50 {
            match ureq::get(url).call() {
                Ok(resp) => {
                    assert_eq!(resp.status(), 200, "{url}");
                    let ct = resp.header("content-type").unwrap_or("").to_string();
                    assert_eq!(ct, "image/png", "{url}");
                    let mut body = Vec::new();
                    resp.into_reader().read_to_end(&mut body).unwrap();
                    return body;
                }
                Err(e) => {
                    last = e.to_string();
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }
        panic!("no tile from {url}: {last}");
    };

    let a = fetch(&kvp);
    assert!(png_opaque_px(&a) > 100, "KVP tile must carry the fixture");
    let b = fetch(&rest);
    assert_eq!(a, b, "the two bindings must serve the same tile");
}
