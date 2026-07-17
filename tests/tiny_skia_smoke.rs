// Proves tiny-skia is wired and can fill a rect — and, by building, that the banned-crate gate
// still passes (score.sh is the real gate; this just anchors the dep).
#[test]
fn tiny_skia_fills_a_rect() {
    let mut pm = tiny_skia::Pixmap::new(4, 4).unwrap();
    let mut paint = tiny_skia::Paint::default();
    paint.set_color_rgba8(255, 0, 0, 255);
    let mut pb = tiny_skia::PathBuilder::new();
    pb.move_to(0.0, 0.0);
    pb.line_to(4.0, 0.0);
    pb.line_to(4.0, 4.0);
    pb.line_to(0.0, 4.0);
    pb.close();
    let path = pb.finish().unwrap();
    pm.fill_path(
        &path,
        &paint,
        tiny_skia::FillRule::Winding,
        tiny_skia::Transform::identity(),
        None,
    );
    // Center pixel is opaque red (premultiplied == straight at full alpha).
    let px = pm.pixel(2, 2).unwrap();
    assert_eq!(
        (px.red(), px.green(), px.blue(), px.alpha()),
        (255, 0, 0, 255)
    );
}
