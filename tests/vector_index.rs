use terraserve::vector::index::{Aabb, Grid};

fn bb(x: f32, y: f32, w: f32, h: f32) -> Aabb {
    Aabb {
        min: [x, y],
        max: [x + w, y + h],
    }
}

#[test]
fn overlap_is_correct_and_order_independent() {
    let boxes = vec![
        bb(0., 0., 10., 10.),
        bb(100., 100., 10., 10.),
        bb(50., 50., 5., 5.),
    ];
    let probe = bb(5., 5., 2., 2.);
    let mut a = Grid::new(16.0);
    for b in &boxes {
        a.insert(*b);
    }
    let mut c = Grid::new(16.0);
    for b in boxes.iter().rev() {
        c.insert(*b);
    }
    assert!(a.overlaps(probe));
    // boolean answer is independent of insertion order
    assert_eq!(a.overlaps(probe), c.overlaps(probe));
    assert!(!a.overlaps(bb(200., 200., 1., 1.)));
    // touching-but-not-overlapping is not an overlap
    assert!(!a.overlaps(bb(10., 0., 1., 1.)));
}
