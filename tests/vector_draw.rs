use terraserve::vector::draw::Canvas;
use terraserve::vector::shape::Shaper;
use terraserve::vector::style::{PointSym, TextSym};

#[test]
fn marker_is_antialiased() {
    let mut c = Canvas::transparent(20, 20);
    let sym = PointSym {
        radius: 5.0,
        fill: [0, 0, 0, 255],
        stroke: [255, 255, 255, 255],
        stroke_width: 1.0,
    };
    c.draw_marker(10.0, 10.0, &sym);
    let out = c.into_rgba();
    let alpha = |x: u32, y: u32| out[((y * 20 + x) * 4 + 3) as usize];
    assert_eq!(alpha(10, 10), 255, "centre opaque");
    assert_eq!(alpha(0, 0), 0, "far corner transparent");
    let partial = (0..20u32 * 20)
        .filter(|i| {
            let a = out[(*i as usize) * 4 + 3];
            a > 0 && a < 255
        })
        .count();
    assert!(
        partial > 0,
        "anti-aliasing produces partial-alpha edge pixels"
    );
}

/// Task 7's Canvas seed seam: a fully-opaque seed pixel round-trips through `from_rgba` →
/// `into_rgba` exactly (premultiply-then-demultiply at alpha=255 is lossless), and an untouched
/// pixel stays fully transparent — the same guarantee `Canvas::transparent` gives today.
#[test]
fn from_rgba_seeds_opaque_pixel_exactly() {
    let (w, h) = (4u32, 4u32);
    let mut base = vec![0u8; (w * h * 4) as usize];
    let i = ((1 * w + 1) * 4) as usize;
    base[i..i + 4].copy_from_slice(&[200, 50, 25, 255]);
    let c = Canvas::from_rgba(w, h, &base);
    let out = c.into_rgba();
    assert_eq!(
        &out[i..i + 4],
        &[200, 50, 25, 255],
        "opaque seed pixel round-trips exactly"
    );
    assert_eq!(
        &out[0..4],
        &[0, 0, 0, 0],
        "untouched pixel stays transparent"
    );
}

/// The actual draw-order seam: a translucent seeded base (as `GeomLayer::into_straight_rgba`
/// would hand over) composites correctly under an opaque marker drawn on top afterwards — no
/// premultiplied-alpha fringe, and the seed shows through untouched elsewhere.
#[test]
fn from_rgba_seed_composites_correctly_under_a_marker() {
    let (w, h) = (20u32, 20u32);
    let mut base = vec![0u8; (w * h * 4) as usize];
    for px in base.chunks_mut(4) {
        px.copy_from_slice(&[0, 200, 0, 128]); // ~50% translucent green everywhere
    }
    let mut c = Canvas::from_rgba(w, h, &base);
    let sym = PointSym {
        radius: 3.0,
        fill: [255, 0, 0, 255],
        stroke: [255, 0, 0, 255],
        stroke_width: 0.0,
    };
    c.draw_marker(10.0, 10.0, &sym);
    let out = c.into_rgba();

    let centre = ((10 * w + 10) * 4) as usize;
    assert_eq!(
        &out[centre..centre + 4],
        &[255, 0, 0, 255],
        "opaque marker fully covers the seed at its centre"
    );

    // Far corner: untouched by the marker — the seed's straight-alpha color/alpha must survive
    // the premultiply (seed) -> demultiply (into_rgba) round trip within u8 rounding tolerance.
    let corner = 0usize;
    assert!(
        (out[corner + 1] as i32 - 200).abs() <= 2,
        "seed green channel round-trips, got {}",
        out[corner + 1]
    );
    assert!(
        (out[corner + 3] as i32 - 128).abs() <= 2,
        "seed alpha round-trips, got {}",
        out[corner + 3]
    );
}

#[test]
fn label_draws_halo_and_body_straight_alpha() {
    let sh =
        Shaper::from_font_bytes(&std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap()).unwrap();
    let label = sh.shape("A", 20.0);
    let mut c = Canvas::transparent(60, 60);
    let text = TextSym {
        label: vec![],
        priority: None,
        priority_higher_wins: false,
        size: 20.0,
        color: [0, 0, 0, 255],
        halo_color: [255, 255, 255, 255],
        halo_radius: 2.0,
        offset: 0.0,
    };
    c.draw_label([10.0, 10.0], &label, &text); // origin = box top-left
    let out = c.into_rgba();
    let has_body = out.chunks(4).any(|p| p[3] > 0 && p[0] < 80);
    // Straight-alpha: a white halo pixel is truly white, not premultiplied-darkened (Blocker 2).
    let has_white_halo = out
        .chunks(4)
        .any(|p| p[3] > 100 && p[0] > 240 && p[1] > 240 && p[2] > 240);
    assert!(has_body, "text body (dark) drawn");
    assert!(
        has_white_halo,
        "halo is straight-alpha white, not premultiplied-darkened"
    );
}
