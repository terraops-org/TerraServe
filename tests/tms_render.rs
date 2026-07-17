//! TileFactory / render_tile integration against the real polar DEM (skips if the fixture is
//! absent).
//!
//! - `tile_known_point_golden_ups`: a KNOWN-POINT GOLDEN. Project the fixture's data center
//!   (EPSG:3413) into the UPS grid CRS (EPSG:5041), compute its `(z,col,row)` from the grid math,
//!   and assert THAT tile is mostly opaque while a grid-corner tile is fully transparent. A y-flip,
//!   axis-swap, or origin-sign bug would land the index on empty ocean → fails (a non-emptiness
//!   test would not catch it).
//! - `render_tile_native_grid_png`: the decoupled `TileFactory::render_tile` path returns a real
//!   PNG on the native `from_cog` grid, and an out-of-matrix tile errors cleanly (no panic).

use terraserve::backend::Resample;
use terraserve::cog::{self, LocalFileRangeSource};
use terraserve::expr;
use terraserve::render::{render_with_cog, BandMath, RenderRequest};
use terraserve::reproj::Transformer;
use terraserve::s3::{AnySource, S3Config};
use terraserve::style::Style;
use terraserve::tms::{TileFactory, TileMatrixSet, TileRequest};

const PATH: &str = "../cogs/polar/arcticdem_18_47_32m_gunnbjorn_dem.tif";

fn opaque_fraction(rgba: &[u8]) -> f64 {
    let n = rgba.len() / 4;
    if n == 0 {
        return 0.0;
    }
    rgba.chunks_exact(4).filter(|p| p[3] == 255).count() as f64 / n as f64
}

fn load() -> Option<(cog::Cog, AnySource, BandMath, Style)> {
    if !std::path::Path::new(PATH).exists() {
        eprintln!("skipping: polar fixture absent");
        return None;
    }
    let source = AnySource::open(PATH, &S3Config::default()).unwrap();
    let cog = cog::parse(&LocalFileRangeSource::open(PATH).unwrap()).unwrap();
    let bm = BandMath {
        program: expr::Program::compile("elev", &["elev"]).unwrap(),
        nodata: -9999.0,
    };
    let style = Style::load("fixtures/styles/dem.json").unwrap();
    Some((cog, source, bm, style))
}

#[test]
fn tile_known_point_golden_ups() {
    let Some((cog, source, bm, style)) = load() else {
        return;
    };

    let grid = TileMatrixSet::ups_wgs84_quad("EPSG:5041", 512);
    let z = 9u32;
    let lvl = grid.level(z).unwrap();
    let span = grid.tile_w as f64 * lvl.resolution;

    // Fixture data center (EPSG:3413 extent midpoint) → grid CRS (EPSG:5041) → tile index.
    let (cx, cy) = (650000.0, -2250000.0);
    let t = Transformer::new("EPSG:3413", "EPSG:5041").unwrap();
    let (px, py) = t.to_source(cx, cy).unwrap();
    let col = ((px - grid.origin_x) / span).floor() as u32;
    let row = ((grid.origin_y - py) / span).floor() as u32;
    assert!(
        col < lvl.matrix_w && row < lvl.matrix_h,
        "computed tile ({col},{row}) out of matrix {}x{}",
        lvl.matrix_w,
        lvl.matrix_h
    );

    let index_cache = terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes());
    let render = |bbox: [f64; 4]| {
        let rr = RenderRequest {
            cog_path: PATH,
            bbox,
            crs: "EPSG:5041",
            src_crs: "EPSG:3413",
            width: grid.tile_w,
            height: grid.tile_h,
            resample: Resample::Bilinear,
            style: &style,
            band_math: Some(&bm),
            index_cache: index_cache.clone(),
        };
        render_with_cog(&rr, &cog, &source, None).unwrap()
    };

    // The tile holding the data center is mostly opaque; the grid corner (far from Greenland) empty.
    let hit = render(grid.tile_bounds(z, col, row).unwrap());
    let hf = opaque_fraction(&hit);
    assert!(
        hf > 0.5,
        "data tile only {hf:.2} opaque — grid index / reprojection is wrong"
    );
    let far = render(grid.tile_bounds(z, 0, 0).unwrap());
    assert_eq!(
        opaque_fraction(&far),
        0.0,
        "grid-corner tile should be fully transparent"
    );
}

#[test]
fn render_tile_native_grid_png() {
    let Some((cog, source, bm, style)) = load() else {
        return;
    };
    let grid = TileMatrixSet::from_cog(&cog, "EPSG:3413", 256);
    let z = grid.levels.len() as u32 - 1; // finest
    let lvl = grid.level(z).unwrap();
    let (col, row) = (lvl.matrix_w / 2, lvl.matrix_h / 2);

    let index_cache = terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes());
    let mk = |z, col, row| TileRequest {
        cog: &cog,
        source: &source,
        cog_path: PATH,
        src_crs: "EPSG:3413",
        style: &style,
        band_math: Some(&bm),
        cache: None,
        index_cache: &index_cache,
        data_bounds: None,
        grid: &grid,
        z,
        col,
        row,
    };

    let png = TileFactory::render_tile(&mk(z, col, row)).unwrap();
    assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "not a PNG");
    assert!(
        png.len() > 500,
        "suspiciously small PNG ({} bytes)",
        png.len()
    );

    // Out-of-matrix tile errors cleanly (no panic).
    assert!(TileFactory::render_tile(&mk(z, lvl.matrix_w, row)).is_err());
}
