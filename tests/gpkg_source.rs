//! Integration tests for the native GeoPackage reader (`GpkgSource`) against the tiny
//! `fixtures/gpkg/mini.gpkg` fixture: 2 polygons (one with a hole) + 1 LineString, each with
//! a `name` TEXT + `rank` INTEGER attribute, EPSG:4326. Built offline via:
//!   `ogr2ogr -f GPKG fixtures/gpkg/mini.gpkg mini.geojson -nln feats -a_srs EPSG:4326`

use terraserve::vector::feature::Geometry;
use terraserve::vector::gpkg::GpkgSource;
use terraserve::vector::render::render_vector;
use terraserve::vector::shape::Shaper;
use terraserve::vector::source::FeatureSource;
use terraserve::vector::style::{FeatureTypeStyle, PolygonSym, Rule, Style, Symbolizer};

const MINI: &str = "fixtures/gpkg/mini.gpkg";

#[test]
fn loads_mini_gpkg_fixture() {
    let s = GpkgSource::load(MINI, None).unwrap();

    // Pinned feature count (observed via `ogrinfo -al`: 3 features, fid 1..3).
    assert_eq!(s.features().len(), 3);

    // A Polygon with more than one ring (the hole) is present.
    let with_hole = s
        .features()
        .iter()
        .find(|f| f.props.get_str("name") == Some("square_with_hole"))
        .expect("square_with_hole feature present");
    match &with_hole.geom {
        Geometry::Polygon(rings) => {
            assert!(rings.len() > 1, "expected >1 ring (a hole), got {rings:?}");
        }
        other => panic!("expected Polygon, got {other:?}"),
    }
    assert_eq!(with_hole.props.get_f64("rank"), Some(2.0));
    assert_eq!(with_hole.fid, 2, "fid must be the table PK");

    // The plain polygon (single ring) is present too.
    let plain = s
        .features()
        .iter()
        .find(|f| f.props.get_str("name") == Some("square_a"))
        .expect("square_a feature present");
    match &plain.geom {
        Geometry::Polygon(rings) => assert_eq!(rings.len(), 1),
        other => panic!("expected Polygon, got {other:?}"),
    }
    assert_eq!(plain.props.get_f64("rank"), Some(1.0));
    assert_eq!(plain.fid, 1, "fid must be the table PK");

    // A LineString is present.
    let line = s
        .features()
        .iter()
        .find(|f| f.props.get_str("name") == Some("connector_line"))
        .expect("connector_line feature present");
    match &line.geom {
        Geometry::LineString(pts) => assert_eq!(pts.len(), 3),
        other => panic!("expected LineString, got {other:?}"),
    }
    assert_eq!(line.props.get_f64("rank"), Some(3.0));
    assert_eq!(line.fid, 3, "fid must be the table PK");

    // CRS resolves via gpkg_spatial_ref_sys → EPSG:4326.
    assert_eq!(s.crs(), Some("EPSG:4326"));

    // full_extent() is finite and correctly ordered [w,s,e,n].
    let ext = s.full_extent();
    assert!(
        ext.iter().all(|v| v.is_finite()),
        "extent not finite: {ext:?}"
    );
    assert!(ext[0] < ext[2], "west < east: {ext:?}");
    assert!(ext[1] < ext[3], "south < north: {ext:?}");
    // Known bounds of the fixture geometry: x in [0,30], y in [0,10].
    assert_eq!(ext, [0.0, 0.0, 30.0, 10.0]);
}

#[test]
fn loads_with_explicit_layer_name() {
    let s = GpkgSource::load(MINI, Some("feats")).unwrap();
    assert_eq!(s.features().len(), 3);
    assert_eq!(s.crs(), Some("EPSG:4326"));
}

#[test]
fn unknown_layer_name_errs() {
    let result = GpkgSource::load(MINI, Some("does_not_exist"));
    assert!(result.is_err());
}

#[test]
fn missing_file_errs() {
    let result = GpkgSource::load("fixtures/gpkg/does_not_exist.gpkg", None);
    assert!(result.is_err());
}

/// A Debug-string identity for a whole `Feature` (fid + geometry + props). `Geometry`/`Props`
/// don't derive `PartialEq`, but they do derive `Debug`, and f64's `Debug` is shortest-round-trip
/// — so string equality is a faithful, byte-level proxy here.
fn feature_sig(f: &terraserve::vector::feature::Feature) -> String {
    format!("{f:?}")
}

fn signatures(s: &GpkgSource) -> Vec<String> {
    s.features().iter().map(feature_sig).collect()
}

/// The partitioned parallel load (multiple ranges, one connection per range) must yield the
/// EXACT same feature vector — same order, same geometry, same props, same fid — as a single-range
/// load over the whole rowid span. `load_partitioned(threshold, n_ranges)` is the test seam:
/// `(i64::MAX, 1)` forces the single-range fallback; `(1, 3)` forces a genuine 3-way partition of
/// the 3-row fixture regardless of the host CPU count.
#[test]
fn parallel_load_matches_single_thread() {
    let single = GpkgSource::load_partitioned(MINI, None, i64::MAX, 1).unwrap();
    let parallel = GpkgSource::load_partitioned(MINI, None, 1, 3).unwrap();

    assert_eq!(
        single.features().len(),
        parallel.features().len(),
        "feature counts differ"
    );
    assert_eq!(
        signatures(&single),
        signatures(&parallel),
        "parallel load must match single-thread load feature-for-feature, in order"
    );
    assert_eq!(
        single.full_extent(),
        parallel.full_extent(),
        "reduced extent must match"
    );
    assert_eq!(single.crs(), parallel.crs(), "crs must match");
}

/// Two partitioned loads must be byte-identical: the render draw order depends on a stable,
/// reproducible feature order. Uses the forced-partition seam so both loads exercise the true
/// multi-range path (the plain `load()` on this tiny fixture would always take the fallback).
#[test]
fn parallel_load_is_deterministic() {
    let a = GpkgSource::load_partitioned(MINI, None, 1, 3).unwrap();
    let b = GpkgSource::load_partitioned(MINI, None, 1, 3).unwrap();

    assert_eq!(
        signatures(&a),
        signatures(&b),
        "two partitioned loads must produce byte-identical feature vectors"
    );
    assert_eq!(a.full_extent(), b.full_extent());
    assert_eq!(a.crs(), b.crs());
}

/// Task 5: render smoke test — a `GpkgSource`, wrapped exactly as `build_vector_layer` wraps it
/// (`Arc<dyn FeatureSource>`), must be render-interchangeable with `GeoJsonSource`: fed to
/// `render_vector` with a plain polygon style over the fixture's own extent, it must draw
/// non-transparent pixels (the two polygon features, `square_a` + `square_with_hole`). Proves the
/// gpkg source slots into the existing vector pipeline unchanged, not just that it loads.
#[test]
fn renders_through_the_vector_pipeline() {
    let s = GpkgSource::load(MINI, None).unwrap();
    let src: std::sync::Arc<dyn FeatureSource> = std::sync::Arc::new(s);
    let shaper =
        Shaper::from_font_bytes(&std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap()).unwrap();
    let style = Style {
        feature_type_styles: vec![FeatureTypeStyle {
            rules: vec![Rule {
                filter: None,
                else_filter: false,
                min_scale: None,
                max_scale: None,
                symbolizers: vec![Symbolizer::Polygon(PolygonSym {
                    fill: [180, 200, 180, 255],
                    stroke: Some([60, 60, 60, 255]),
                    stroke_width: 1.0,
                })],
                title: None,
            }],
        }],
    };

    // The fixture's own extent (EPSG:4326, both source and grid CRS — no reprojection needed).
    let bbox = src.full_extent();
    let rgba = render_vector(
        src.as_ref(),
        &style,
        "EPSG:4326",
        "EPSG:4326",
        bbox,
        256,
        256,
        &shaper,
    )
    .unwrap();
    assert_eq!(rgba.len(), 256 * 256 * 4);
    let opaque = rgba.chunks(4).filter(|p| p[3] > 0).count();
    assert!(
        opaque > 0,
        "GpkgSource polygon geometry must render through the same pipeline as GeoJsonSource, got 0 opaque px"
    );
}
