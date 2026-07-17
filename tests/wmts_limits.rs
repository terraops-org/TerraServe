//! WMTS TileMatrixSetLimits grid math (`tile_limits`): row/col range covering a bounds, clamped,
//! with the WR9 max-edge (a tile the data only touches on its edge is excluded) + disjoint→None.

use terraserve::tms::TileMatrixSet;

#[test]
fn tile_limits_clamps_bounds_to_matrix() {
    let g = TileMatrixSet::web_mercator_quad(256);
    let span = 256.0 * g.level(2).unwrap().resolution; // one tile span at z=2 (4x4 matrix)

    // Interior of the NW tile -> (0,0,0,0).
    let nw = [
        g.origin_x + 0.1,
        g.origin_y - span + 0.1,
        g.origin_x + span - 0.1,
        g.origin_y - 0.1,
    ];
    assert_eq!(g.tile_limits(nw, 2), Some((0, 0, 0, 0)));

    // The whole grid extent -> the full 0..3 range.
    let world = g.full_extent().unwrap();
    assert_eq!(g.tile_limits(world, 2), Some((0, 3, 0, 3)));

    assert_eq!(g.tile_limits(world, 99), None); // no such level
}

#[test]
fn tile_limits_excludes_edge_touch_and_disjoint() {
    let g = TileMatrixSet::web_mercator_quad(256);
    let span = 256.0 * g.level(2).unwrap().resolution;

    // maxx sits EXACTLY on the col-2 tile boundary -> col 2 is only touched, not covered -> maxcol=1.
    let edge = [
        g.origin_x,
        g.origin_y - span,
        g.origin_x + 2.0 * span,
        g.origin_y,
    ];
    let (mincol, maxcol, _minrow, _maxrow) = g.tile_limits(edge, 2).unwrap();
    assert_eq!((mincol, maxcol), (0, 1), "edge-touch tile must be excluded");

    // Bounds entirely west of the matrix -> disjoint -> None.
    let west = [
        g.origin_x - 10.0 * span,
        g.origin_y - span,
        g.origin_x - span,
        g.origin_y,
    ];
    assert_eq!(g.tile_limits(west, 2), None);
}
