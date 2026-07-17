//! GetLegendGraphic rendering: a pseudocolor style yields a ramp PNG; RGB passthrough has no legend.

use terraserve::legend::render_legend;
use terraserve::style::Style;

#[test]
fn pseudocolor_legend_is_a_ramp_png() {
    let style = Style::load("fixtures/styles/dem.json").unwrap();
    let png = render_legend(&style, 120, 256).unwrap();
    assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "not a PNG");
    assert!(
        png.len() > 500,
        "suspiciously small legend PNG ({} bytes)",
        png.len()
    );
}

#[test]
fn rgb_layer_has_no_legend() {
    let style = Style::load("fixtures/styles/rgb.json").unwrap();
    assert!(
        render_legend(&style, 120, 256).is_err(),
        "RGB passthrough should have no legend"
    );
}
