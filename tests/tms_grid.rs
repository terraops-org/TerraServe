//! TileMatrixSet grid math — verified against docs/tilematrixset-reference.md (authoritative OGC
//! registry numbers). No COG needed for the preset tests; `from_cog` uses the polar fixture.

use terraserve::tms::{meters_per_unit, TileMatrixSet};

fn approx(a: f64, b: f64) {
    assert!((a - b).abs() <= b.abs() * 1e-6 + 1e-9, "{a} != {b}");
}

#[test]
fn web_mercator_quad_256_matches_reference() {
    let g = TileMatrixSet::web_mercator_quad(256);
    assert_eq!(g.id, "WebMercatorQuad");
    assert_eq!(g.crs, "EPSG:3857");
    assert_eq!(g.levels.len(), 25);
    approx(g.levels[0].resolution, 156543.033928041);
    approx(g.scale_denominator(0), 559082264.028717);
    assert_eq!(g.levels[0].matrix_w, 1);
    assert_eq!(g.levels[0].matrix_h, 1);
    assert_eq!(g.levels[3].matrix_w, 8); // 2^3
                                         // L0 single tile spans the full extent.
    let [minx, miny, maxx, maxy] = g.tile_bounds(0, 0, 0).unwrap();
    approx(minx, -20037508.3427892);
    approx(maxy, 20037508.3427892);
    approx(maxx, 20037508.3427892);
    approx(miny, -20037508.3427892);
    assert!(g.tile_bounds(0, 1, 0).is_none()); // out of matrix
}

#[test]
fn world_crs84_quad_256_is_2x1_at_z0() {
    let g = TileMatrixSet::world_crs84_quad(256);
    assert_eq!(g.id, "WorldCRS84Quad");
    assert_eq!(g.crs, "EPSG:4326");
    assert_eq!(g.levels.len(), 24);
    approx(g.levels[0].resolution, 0.703125);
    approx(g.scale_denominator(0), 279541132.014358);
    assert_eq!(g.levels[0].matrix_w, 2); // 2^(0+1)
    assert_eq!(g.levels[0].matrix_h, 1); // 2^0
    let [minx, _miny, maxx, maxy] = g.tile_bounds(0, 0, 0).unwrap();
    approx(minx, -180.0);
    approx(maxy, 90.0);
    approx(maxx, 0.0); // one of two z0 tiles covers [-180,0]
}

#[test]
fn ups_arctic_equals_antarctic_numerically() {
    let a = TileMatrixSet::ups_wgs84_quad("EPSG:5041", 256);
    let b = TileMatrixSet::ups_wgs84_quad("EPSG:5042", 256);
    assert_eq!(a.id, "UPSArcticWGS84Quad");
    assert_eq!(b.id, "UPSAntarcticWGS84Quad");
    approx(a.levels[0].resolution, 128443.4324);
    approx(a.scale_denominator(0), 458726544.4);
    // Identical geometry, differ only by CRS/id.
    assert_eq!(a.origin_x, b.origin_x);
    assert_eq!(a.origin_y, b.origin_y);
    assert_eq!(a.levels[0].resolution, b.levels[0].resolution);
    assert_ne!(a.crs, b.crs);
}

#[test]
fn tile_size_512_is_geometrically_consistent() {
    let g256 = TileMatrixSet::web_mercator_quad(256);
    let g512 = TileMatrixSet::web_mercator_quad(512);
    // 512 z0 == 256 z1 resolution.
    approx(g512.levels[0].resolution, g256.levels[1].resolution);
    // World extent invariant: matrix_w * tile_px * resolution.
    let w256 = g256.levels[0].matrix_w as f64 * 256.0 * g256.levels[0].resolution;
    let w512 = g512.levels[0].matrix_w as f64 * 512.0 * g512.levels[0].resolution;
    approx(w256, w512);
    // Non-256 build carries the suffixed id (protects CITE well-known-id).
    assert_eq!(g512.id, "WebMercatorQuad_512");
}

#[test]
fn meters_per_unit_by_crs() {
    approx(meters_per_unit("EPSG:4326"), 111319.4907932736);
    assert_eq!(meters_per_unit("EPSG:3857"), 1.0);
    assert_eq!(meters_per_unit("EPSG:5041"), 1.0);
}

#[test]
fn preset_lookup_maps_well_known_ids() {
    assert_eq!(
        terraserve::tms::preset("WebMercatorQuad", 256).unwrap().crs,
        "EPSG:3857"
    );
    assert_eq!(
        terraserve::tms::preset("WorldCRS84Quad", 512).unwrap().id,
        "WorldCRS84Quad_512"
    );
    assert_eq!(
        terraserve::tms::preset("UPSArcticWGS84Quad", 256)
            .unwrap()
            .crs,
        "EPSG:5041"
    );
    // R3: an id with an explicit size suffix resolves (and pins that size).
    assert_eq!(
        terraserve::tms::preset("WebMercatorQuad_256", 512)
            .unwrap()
            .id,
        "WebMercatorQuad"
    );
    assert!(terraserve::tms::preset("NotAGrid", 256).is_none());
}

#[test]
fn from_cog_is_tms_indexable_and_native() {
    use terraserve::cog::{self, LocalFileRangeSource};
    let path = "../cogs/polar/arcticdem_18_47_32m_gunnbjorn_dem.tif";
    if !std::path::Path::new(path).exists() {
        eprintln!("skipping: polar fixture absent");
        return;
    }
    let src = LocalFileRangeSource::open(path).unwrap();
    let cog = cog::parse(&src).unwrap();
    let g = TileMatrixSet::from_cog(&cog, "EPSG:3413", 512);
    assert_eq!(g.crs, "EPSG:3413");
    assert_eq!(g.tile_w, 512);
    // Origin == COG top-left corner.
    approx(g.origin_x, cog.levels[0].geo.origin_x);
    approx(g.origin_y, cog.levels[0].geo.origin_y);
    // z0 is the coarsest (largest resolution); finest z == native pixel size.
    assert!(g.levels[0].resolution >= g.levels[g.levels.len() - 1].resolution);
    approx(
        g.levels[g.levels.len() - 1].resolution,
        cog.levels[0].geo.px,
    );
    // LEVEL-INVARIANT EXTENT (the TMS-indexability property): matrix·tile·res is constant across z,
    // so the single bottom-left <Origin> is correct at every zoom.
    let ext_x = |l: &terraserve::tms::TmLevel| l.matrix_w as f64 * g.tile_w as f64 * l.resolution;
    let ext_y = |l: &terraserve::tms::TmLevel| l.matrix_h as f64 * g.tile_h as f64 * l.resolution;
    for l in &g.levels {
        approx(ext_x(l), ext_x(&g.levels[0]));
        approx(ext_y(l), ext_y(&g.levels[0]));
    }
    // The grid extent covers the data, and the top-left tile starts exactly at the origin.
    assert!(ext_x(&g.levels[0]) >= cog.levels[0].width as f64 * cog.levels[0].geo.px - 1.0);
    let [minx, _miny, _maxx, maxy] = g.tile_bounds(0, 0, 0).unwrap();
    approx(minx, g.origin_x);
    approx(maxy, g.origin_y);
}
