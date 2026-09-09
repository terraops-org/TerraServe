// Proves vello_cpu is wired and can fill a rect — and, by building, that the banned-crate gate
// still passes (score.sh is the real gate; this just anchors the dep).
//
// Replaced tests/tiny_skia_smoke.rs on 2026-09-09 when the vector rasterizer moved from tiny-skia
// to vello_cpu; see docs/renderer-benchmark-tinyskia-vs-vellocpu.md for the measurement.
#[test]
fn vello_cpu_fills_a_rect() {
    use vello_cpu::color::{AlphaColor, Srgb};
    use vello_cpu::kurbo::BezPath;
    use vello_cpu::peniko::Fill;
    use vello_cpu::{Pixmap, RenderContext, Resources};

    let mut ctx = RenderContext::new(4, 4);
    let red: AlphaColor<Srgb> = AlphaColor::from_rgba8(255, 0, 0, 255);
    ctx.set_paint(red);
    ctx.set_fill_rule(Fill::NonZero);

    let mut bp = BezPath::new();
    bp.move_to((0.0, 0.0));
    bp.line_to((4.0, 0.0));
    bp.line_to((4.0, 4.0));
    bp.line_to((0.0, 4.0));
    bp.close_path();
    ctx.fill_path(&bp);

    ctx.flush();
    let mut pm = Pixmap::new(4, 4);
    let mut res = Resources::new();
    ctx.render(&mut pm, &mut res);

    // Center pixel is opaque red (premultiplied == straight at full alpha).
    let px = pm.data()[2 * 4 + 2];
    assert_eq!((px.r, px.g, px.b, px.a), (255, 0, 0, 255));
}
