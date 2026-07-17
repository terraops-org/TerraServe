use terraserve::vector::mvt::clip::{clip_line, clip_polygon};

const RECT: [f64; 4] = [0.0, 0.0, 10.0, 10.0];

#[test]
fn polygon_straddling_edge_is_clipped_to_rect() {
    // A square from (-5,-5) to (5,5) → clipped to the (0,0)-(10,10) rect → the (0,0)-(5,5) quadrant.
    let ring = vec![
        [-5.0, -5.0],
        [5.0, -5.0],
        [5.0, 5.0],
        [-5.0, 5.0],
        [-5.0, -5.0],
    ];
    let out = clip_polygon(&[ring], RECT);
    assert_eq!(out.len(), 1);
    let r = &out[0];
    // every vertex inside the rect (with a tiny epsilon)
    for p in r {
        assert!(p[0] >= -1e-9 && p[0] <= 10.0 + 1e-9 && p[1] >= -1e-9 && p[1] <= 10.0 + 1e-9);
    }
    // the clipped area is the unit-ish quadrant [0,5]x[0,5]
    assert!(r.iter().any(|p| (p[0] - 5.0).abs() < 1e-6));
    assert!(r.iter().any(|p| p[0].abs() < 1e-6));
}

#[test]
fn polygon_fully_outside_is_dropped() {
    let ring = vec![[20.0, 20.0], [30.0, 20.0], [30.0, 30.0], [20.0, 20.0]];
    let out = clip_polygon(&[ring], RECT);
    assert!(out.is_empty() || out[0].is_empty());
}

#[test]
fn polygon_fully_inside_is_unchanged_shape() {
    let ring = vec![[2.0, 2.0], [8.0, 2.0], [8.0, 8.0], [2.0, 8.0], [2.0, 2.0]];
    let out = clip_polygon(&[ring.clone()], RECT);
    assert_eq!(out.len(), 1);
    assert!(out[0].len() >= 4);
}

#[test]
fn line_crossing_rect_is_clipped_to_the_inside_segment() {
    // horizontal line from x=-5 to x=15 at y=5 → clipped to x in [0,10].
    let line = vec![[-5.0, 5.0], [15.0, 5.0]];
    let pieces = clip_line(&line, RECT);
    assert_eq!(pieces.len(), 1);
    let seg = &pieces[0];
    assert!((seg[0][0] - 0.0).abs() < 1e-6 && (seg[seg.len() - 1][0] - 10.0).abs() < 1e-6);
}

#[test]
fn line_fully_outside_is_dropped() {
    let line = vec![[-5.0, 20.0], [15.0, 20.0]];
    assert!(clip_line(&line, RECT).is_empty());
}

#[test]
fn line_fully_inside_stays_one_piece_despite_float_drift() {
    // A fully-inside polyline whose interior segment recomputes its end with float drift
    // (3.0 + 1.0*(0.1-3.0) != 0.1) must NOT be split. The old continuity check compared the drifted
    // end against the next segment's exact start, saw a false gap, and shattered the line.
    let line = vec![[3.0, 5.0], [0.1, 5.0], [7.0, 5.0]];
    let pieces = clip_line(&line, RECT);
    assert_eq!(
        pieces.len(),
        1,
        "a continuous in-rect polyline must stay one piece, got {pieces:?}"
    );
    assert_eq!(pieces[0].len(), 3, "all three vertices preserved");
}
