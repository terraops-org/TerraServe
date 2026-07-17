use terraserve::vector::geom::Projector;

#[test]
fn projects_identity_exactly() {
    // EPSG:4326 grid over a 2°×2° window; identity transform → exact pixel arithmetic.
    let bbox = [-10.0, 38.0, -8.0, 40.0]; // minx,miny,maxx,maxy (deg)
    let p = Projector::new("EPSG:4326", "EPSG:4326", bbox, 256, 256).unwrap();
    let (px, py) = p.to_pixel(-9.0, 39.0).unwrap(); // exact centre
    assert!(
        (px - 128.0).abs() < 0.01 && (py - 128.0).abs() < 0.01,
        "centre ({px},{py})"
    );
    let (tx, ty) = p.to_pixel(-10.0, 40.0).unwrap(); // top-left corner
    assert!(tx.abs() < 0.01 && ty.abs() < 0.01, "top-left ({tx},{ty})");
    let (ox, oy) = p.to_pixel(10.0, 0.0).unwrap(); // far outside
    assert!(ox > 256.0 && oy > 256.0, "far point off-canvas ({ox},{oy})");
}

#[test]
fn projects_4326_into_3857_window() {
    // Feature coords are 4326; render into a web-mercator window around Lisbon.
    let bbox = [-1_020_000.0, 4_670_000.0, -1_010_000.0, 4_690_000.0];
    let p = Projector::new("EPSG:4326", "EPSG:3857", bbox, 256, 256).unwrap();
    let (px, py) = p.to_pixel(-9.14, 38.72).unwrap();
    assert!(
        (0.0..=256.0).contains(&px) && (0.0..=256.0).contains(&py),
        "on-canvas ({px},{py})"
    );
    let (ox, _) = p.to_pixel(0.0, 0.0).unwrap();
    assert!(ox < 0.0 || ox > 256.0, "far point off-canvas ({ox})");
}
