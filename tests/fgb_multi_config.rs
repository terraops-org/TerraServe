//! Task 6 (FlatGeoBuf batch): multi-layer `--config` smoke test over TWO `.fgb` sources —
//! `fixtures/fgb/multi.yaml` lists `fixtures/fgb/tiny.fgb` (2 Points + 1 Polygon,
//! extent [0,0]-[5,6]) and `fixtures/fgb/lines.fgb` (2 LineStrings, extent
//! [10,6.5]-[12,11]), both committed fixtures -- no real-data dependency, so this test never
//! self-skips (unlike the PRT.fgb/COS2018v3-S2.fgb oracle tests in `src/vector/fgb/mod.rs`).
//!
//! Mirrors the existing multi-vector-layer config shape (`fixtures/cite/wms13-layers.yaml`,
//! N named `vector:`+`vec_style:` layers) but exercises the windowed `.fgb` reader path
//! end-to-end through the public `config::Config` + `vector::fgb::FgbSource` +
//! `vector::source::VectorSource` seam: both layers resolve from one config, and each layer's
//! `features_in(bbox)` returns only its OWN features -- proving the two `FgbSource`s opened by
//! a multi-layer config don't cross-contaminate.

use terraserve::cog::LocalFileRangeSource;
use terraserve::config::Config;
use terraserve::vector::fgb::FgbSource;
use terraserve::vector::source::VectorSource;

const CONFIG: &str = "fixtures/fgb/multi.yaml";

#[test]
fn multi_layer_config_resolves_two_fgb_layers_each_own_features() {
    let cfg = Config::load(CONFIG).expect("load fixtures/fgb/multi.yaml");
    assert_eq!(cfg.layers.len(), 2);
    assert_eq!(cfg.layers[0].name, "tiny");
    assert_eq!(cfg.layers[1].name, "lines");

    // Both layers are `vector:` (not `cog:`), pointing at a `.fgb`, with a `vec_style` --
    // `validate()` (already run by `Config::load`) accepts that combination.
    for l in &cfg.layers {
        assert!(l.cog.is_none(), "layer '{}' unexpectedly has a cog", l.name);
        let path = l.vector.as_deref().expect("vector path");
        assert!(path.ends_with(".fgb"), "layer '{}': {path}", l.name);
    }

    // Open each layer's own FgbSource, exactly as `build_vector_layer`'s `.fgb` arm does
    // (src/lib.rs), and wrap it as `VectorSource::Windowed` -- the seam every serving path
    // (WMS/MVT/WMTS) reads through.
    let tiny_path = cfg.layers[0].vector.as_deref().unwrap();
    let lines_path = cfg.layers[1].vector.as_deref().unwrap();
    let tiny_src = LocalFileRangeSource::open(tiny_path).unwrap();
    let lines_src = LocalFileRangeSource::open(lines_path).unwrap();
    let tiny = VectorSource::Windowed(std::sync::Arc::new(
        FgbSource::open(tiny_src).expect("open tiny.fgb"),
    ));
    let lines = VectorSource::Windowed(std::sync::Arc::new(
        FgbSource::open(lines_src).expect("open lines.fgb"),
    ));

    // A bbox covering tiny.fgb's whole extent ([0,0]-[5,6]) must return all 3 tiny features
    // and NONE of lines.fgb's (disjoint extent [10,6.5]-[12,11] -- see fixtures/fgb/lines.geojson).
    let tiny_bbox = [-1.0, -1.0, 6.0, 7.0];
    let tiny_batch = tiny.features_in(tiny_bbox);
    assert_eq!(tiny_batch.len(), 3, "tiny layer's own window");
    let lines_over_tiny_bbox = lines.features_in(tiny_bbox);
    assert!(
        lines_over_tiny_bbox.is_empty(),
        "lines layer must not answer tiny's bbox with its own (disjoint-extent) features"
    );

    // Symmetric check: lines.fgb's whole extent returns both LineStrings, and tiny.fgb answers
    // empty over that same window.
    let lines_bbox = [9.5, 6.0, 12.5, 11.5];
    let lines_batch = lines.features_in(lines_bbox);
    assert_eq!(lines_batch.len(), 2, "lines layer's own window");
    for f in lines_batch.as_slice() {
        assert_eq!(
            f.props.get_f64("lanes").is_some(),
            true,
            "lines feature should carry a 'lanes' property, got {:?}",
            f.props
        );
    }
    let tiny_over_lines_bbox = tiny.features_in(lines_bbox);
    assert!(
        tiny_over_lines_bbox.is_empty(),
        "tiny layer must not answer lines' bbox with its own (disjoint-extent) features"
    );

    // Sanity on the actual geometry/property content, not just counts: the "north" line by name.
    let north = lines_batch
        .as_slice()
        .iter()
        .find(|f| f.props.get_str("road") == Some("north"))
        .expect("'north' line feature present");
    match &north.geom {
        terraserve::vector::feature::Geometry::LineString(pts) => assert_eq!(pts.len(), 3),
        g => panic!("expected LineString, got {g:?}"),
    }
}
