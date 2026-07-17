use terraserve::vector::shape::Shaper;

fn font() -> Vec<u8> {
    std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap()
}

#[test]
fn shapes_latin_and_is_deterministic() {
    let sh = Shaper::from_font_bytes(&font()).unwrap();
    let a = sh.shape("Lisboa", 13.0);
    assert!(
        a.width > 10.0 && a.height > 5.0,
        "box {}x{}",
        a.width,
        a.height
    );
    assert_eq!(a.glyphs.len(), 6, "6 latin glyphs (got {})", a.glyphs.len());
    assert!(
        a.glyphs.iter().any(|g| g.coverage.iter().any(|&c| c > 0)),
        "non-empty coverage"
    );
    let b = sh.shape("Lisboa", 13.0);
    assert_eq!(a.width, b.width);
    assert_eq!(
        a.glyphs[0].coverage, b.glyphs[0].coverage,
        "deterministic raster"
    );
}
