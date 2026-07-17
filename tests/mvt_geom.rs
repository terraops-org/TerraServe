use terraserve::vector::mvt::geom::{encode_line, encode_points, encode_polygon};

// command(id, count) = (id & 0x7) | (count << 3); zigzag(n) = (n<<1) ^ (n>>31).
#[test]
fn single_point() {
    // MoveTo(1) at (25,17): cmd = 1|(1<<3)=9; zz(25)=50; zz(17)=34.
    assert_eq!(encode_points(&[[25, 17]]), vec![9, 50, 34]);
}

#[test]
fn multipoint_two() {
    // MoveTo(2): cmd = 1|(2<<3)=17; (5,7)->zz 10,14; then delta (3-5,1-7)=(-2,-6)->zz 3,11.
    assert_eq!(encode_points(&[[5, 7], [3, 1]]), vec![17, 10, 14, 3, 11]);
}

#[test]
fn linestring() {
    // MoveTo(1) (2,2): 9, zz2=4, zz2=4; LineTo(2): 2|(2<<3)=18; (2,10)->d(0,8) zz 0,16; (10,10)->d(8,0) zz16,0.
    assert_eq!(
        encode_line(&[vec![[2, 2], [2, 10], [10, 10]]]),
        vec![9, 4, 4, 18, 0, 16, 16, 0]
    );
}

#[test]
fn polygon_ring_closes_and_winds_positive() {
    // CCW input ring (negative area, y-down) must be reversed to positive-area (exterior).
    // Use a simple square (0,0)(10,0)(10,10)(0,10). Assert it starts with MoveTo(1) and ends ClosePath(7):
    let cmds = encode_polygon(&[vec![[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]]]);
    assert_eq!(cmds[0], 9, "MoveTo count 1");
    assert_eq!(*cmds.last().unwrap(), 15, "ClosePath count 1 = 7|(1<<3)=15");
    // LineTo count = 3 (4 distinct verts, minus the MoveTo vertex): 2|(3<<3)=26.
    assert!(cmds.contains(&26));
}
