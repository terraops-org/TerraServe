//! Task 9: a realistic, GeoServer-shaped `airports.sld` expresses the MVP's automatic
//! `scalerank` declutter as **authored** SLD rules — two scale-gated `<Rule>`s, each with its own
//! `<ogc:Filter>` (`scalerank <= N`) and `MaxScaleDenominator`, each with its own marker/label
//! styling — instead of `render.rs`'s built-in zoom-derived threshold. Proves the full
//! parse → lower → render pipeline on a real multi-rule `.sld` file, deterministically, against a
//! committed golden PNG.

use terraserve::vector::feature::Geometry;
use terraserve::vector::geojson::GeoJsonSource;
use terraserve::vector::render::render_vector;
use terraserve::vector::shape::Shaper;
use terraserve::vector::source::FeatureSource;
use terraserve::vector::style::{Cmp, FeatureTypeStyle, Filter, PointSym, Rule, Style, Symbolizer};

const AIRPORTS_SLD: &str = "fixtures/sld/airports.sld";

/// Same Europe/EPSG:3857 window `tests/sld_render.rs` uses, at 512x512: width = 4,500,000 m /
/// 512 px => res ~8789.06 m/px => OGC scale denominator ~3.14e7 (`res / 0.00028`). That is below
/// `airports.sld`'s "major" rule `MaxScaleDenominator` (5e7) — active — and at/above its "minor"
/// rule's (2e7) — inactive. So this golden renders exactly the declutter's intended zoomed-out
/// state: only `scalerank <= 4` majors draw, with their labels; the denser minor tier stays
/// hidden until the map is zoomed in further (see `sld_tiers_activate_progressively_by_scale`
/// below, which zooms in on the same bbox and shows the minor tier joining).
const EUROPE_BBOX: [f64; 4] = [-1_500_000.0, 4_000_000.0, 3_000_000.0, 8_000_000.0];

fn shaper() -> Shaper {
    Shaper::from_font_bytes(&std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap()).unwrap()
}

fn render_airports_sld_europe() -> Vec<u8> {
    let src = GeoJsonSource::load("fixtures/vector/airports.geojson").unwrap();
    let style = Style::load(AIRPORTS_SLD).expect("airports.sld should load via Style::load");
    let sh = shaper();
    let rgba = render_vector(
        &src,
        &style,
        "EPSG:4326",
        "EPSG:3857",
        EUROPE_BBOX,
        512,
        512,
        &sh,
    )
    .unwrap();
    terraserve::pngio::encode_rgba(&rgba, 512, 512).unwrap()
}

fn opaque(rgba: &[u8]) -> usize {
    rgba.chunks(4).filter(|p| p[3] > 0).count()
}

/// `airports.sld` parses + lowers into the shape the brief describes: two rules, tiered by
/// `MaxScaleDenominator` and an `scalerank <= N` filter, each with a Point + Text symbolizer.
#[test]
fn style_load_lowers_two_tier_declutter_rules() {
    let style = Style::load(AIRPORTS_SLD).expect("airports.sld should load");
    assert_eq!(
        style.feature_type_styles[0].rules.len(),
        2,
        "major + minor tiers"
    );

    let major = &style.feature_type_styles[0].rules[0];
    assert_eq!(
        major.max_scale,
        Some(50_000_000.0),
        "major: visible zoomed further out"
    );
    assert!(
        matches!(&major.filter, Some(Filter::Cmp(Cmp::Le, prop, lit)) if prop == "scalerank" && lit == "4"),
        "major filter is scalerank <= 4, got {:?}",
        major.filter
    );

    let minor = &style.feature_type_styles[0].rules[1];
    assert_eq!(
        minor.max_scale,
        Some(20_000_000.0),
        "minor: needs a smaller (more zoomed-in) scale denominator to activate"
    );
    // After the additive-semantics fix (CORR-1) the tiers are made mutually exclusive so a major
    // airport is not drawn twice: minor is now `scalerank >= 5 AND scalerank <= 8`.
    assert!(
        matches!(&minor.filter, Some(Filter::And(fs)) if fs.len() == 2
            && matches!(&fs[0], Filter::Cmp(Cmp::Ge, prop, lit) if prop == "scalerank" && lit == "5")
            && matches!(&fs[1], Filter::Cmp(Cmp::Le, prop, lit) if prop == "scalerank" && lit == "8")),
        "minor filter is scalerank >= 5 AND scalerank <= 8, got {:?}",
        minor.filter
    );

    for (label, rule) in [("major", major), ("minor", minor)] {
        let has_point = rule
            .symbolizers
            .iter()
            .any(|s| matches!(s, Symbolizer::Point(_)));
        let has_text = rule
            .symbolizers
            .iter()
            .any(|s| matches!(s, Symbolizer::Text(_)));
        assert!(has_point, "{label} rule has a Point symbolizer");
        assert!(has_text, "{label} rule has a Text symbolizer");
    }
}

/// Sanity check on the scale math the golden's bbox/size depends on, spelled out so a future
/// change to `EUROPE_BBOX`/size is caught here rather than via an opaque golden-byte diff.
#[test]
fn europe_512_scale_gates_major_only() {
    let width = 512.0_f64;
    let res_grid = (EUROPE_BBOX[2] - EUROPE_BBOX[0]).abs() / width; // EPSG:3857: 1 grid unit = 1 m
    let scale = res_grid / 0.00028;
    assert!(
        scale < 50_000_000.0,
        "below major's MaxScaleDenominator (scale={scale})"
    );
    assert!(
        scale >= 20_000_000.0,
        "at/above minor's MaxScaleDenominator (scale={scale})"
    );
}

/// Regenerate the committed golden. Run intentionally:
/// `cargo test --test sld_golden gen_golden -- --ignored`, eyeball the PNG, then commit.
#[test]
#[ignore]
fn gen_golden() {
    std::fs::create_dir_all("fixtures/goldens").unwrap();
    std::fs::write(
        "fixtures/goldens/airports_sld_europe_512.png",
        render_airports_sld_europe(),
    )
    .unwrap();
}

#[test]
fn render_is_deterministic() {
    assert_eq!(render_airports_sld_europe(), render_airports_sld_europe());
}

#[test]
fn matches_golden() {
    let png = render_airports_sld_europe();
    let golden = std::fs::read("fixtures/goldens/airports_sld_europe_512.png")
        .expect("golden missing — generate it once with gen_golden then commit");
    assert_eq!(
        png.len(),
        golden.len(),
        "golden size mismatch — regenerate intentionally on design change"
    );
    assert_eq!(png, golden, "output differs from committed golden");
}

/// The declutter is genuinely scale-driven, not a fixed drawing: zooming in on the *same* bbox
/// (higher pixel width => smaller resolution => smaller OGC scale denominator) crosses the
/// minor tier's `MaxScaleDenominator` (2e7) and admits `scalerank` 5..=8 airports that the
/// europe-512 golden above correctly hides. Mirrors `tests/vector_render.rs::scale_and_filter_gate_rule`,
/// but driving the real `.sld` file end-to-end instead of a hand-built `Style`.
#[test]
fn sld_tiers_activate_progressively_by_scale() {
    let src = GeoJsonSource::load("fixtures/vector/airports.geojson").unwrap();
    let style = Style::load(AIRPORTS_SLD).unwrap();
    let sh = shaper();

    // 512 px: scale ~3.14e7 -> only "major" (scalerank <= 4) active.
    let major_only = render_vector(
        &src,
        &style,
        "EPSG:4326",
        "EPSG:3857",
        EUROPE_BBOX,
        512,
        512,
        &sh,
    )
    .unwrap();

    // 1536 px over the SAME bbox: res shrinks 3x -> scale ~1.05e7, below both tiers'
    // MaxScaleDenominator -> "major" AND "minor" (scalerank <= 8) both active.
    let both_tiers = render_vector(
        &src,
        &style,
        "EPSG:4326",
        "EPSG:3857",
        EUROPE_BBOX,
        1536,
        1536,
        &sh,
    )
    .unwrap();

    // Compare raw opaque-pixel counts, not density: markers/labels are a FIXED pixel footprint
    // regardless of canvas size, so a bigger canvas over the same bbox does not by itself make
    // the render "denser" — but admitting the minor tier's extra airports (5..=8, on top of
    // major's 4) draws strictly more marker+label pixels in absolute terms.
    let count_major = opaque(&major_only);
    let count_both = opaque(&both_tiers);
    assert!(
        count_both > count_major,
        "zoomed-in render (both tiers active) draws more opaque px than the major-only render \
         (major-only={count_major}, both={count_both})"
    );
}

/// Companion check for the assertion above: `airports.geojson` actually contains scalerank 5..=8
/// features inside `EUROPE_BBOX` (WGS84), so "minor tier switching on" is a real, non-vacuous
/// effect and not an artifact of an empty filter.
#[test]
fn minor_tier_scalerank_range_present_in_europe_bbox() {
    let src = GeoJsonSource::load("fixtures/vector/airports.geojson").unwrap();
    // EUROPE_BBOX in EPSG:3857 reprojected to WGS84 (computed once, hardcoded here for a cheap,
    // dependency-free check): lon in [-13.47, 26.95], lat in [33.79, 58.16].
    let (lon_min, lon_max) = (-13.48, 26.95);
    let (lat_min, lat_max) = (33.78, 58.16);
    let minor_only_count = src
        .features()
        .iter()
        .filter(|f| {
            let in_scale = f
                .props
                .get_f64("scalerank")
                .is_some_and(|sr| (5.0..=8.0).contains(&sr));
            let in_bbox = matches!(f.geom, Geometry::Point([x, y])
                if (lon_min..=lon_max).contains(&x) && (lat_min..=lat_max).contains(&y));
            in_scale && in_bbox
        })
        .count();
    assert!(
        minor_only_count > 0,
        "expected scalerank 5..=8 airports inside the Europe bbox, found {minor_only_count}"
    );
}

// ---------------------------------------------------------------------------------------------
// CORR-1: additive rule selection (OGC SLD 1.0 §11.4) + ElseFilter-is-fallback-only semantics.
//
// The committed europe-512 golden above CANNOT exercise these: at that scale (~3.14e7) only the
// major tier is active, so a feature matches at most one rule. These tests build a `Style` by
// hand + an in-memory point source with explicit scaleranks, use *point-only* rules with distinct
// solid fills (stroke-width 0), and assert on the presence/absence of each rule's fill color.
// A fully-covered marker pixel is exactly its fill with alpha 255 (premultiplied→straight-alpha
// round-trips a=255 losslessly), so exact color matching is a reliable "did this rule draw?" probe.
// ---------------------------------------------------------------------------------------------

/// An in-memory GeoJSON point source from `(lon, lat, scalerank)` tuples (WGS84).
fn point_source(feats: &[(f64, f64, i64)]) -> GeoJsonSource {
    let mut s = String::from(r#"{"type":"FeatureCollection","features":["#);
    for (i, (lon, lat, sr)) in feats.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            r#"{{"type":"Feature","geometry":{{"type":"Point","coordinates":[{lon},{lat}]}},"properties":{{"scalerank":{sr},"name":"p{i}"}}}}"#
        ));
    }
    s.push_str("]}");
    GeoJsonSource::from_str(&s).unwrap()
}

/// A rule with a single solid `Point` symbolizer (no stroke, no text) — the color probe.
fn point_only_rule(filter: Option<Filter>, fill: [u8; 4], radius: f32) -> Rule {
    Rule {
        filter,
        else_filter: false,
        min_scale: None,
        max_scale: None,
        symbolizers: vec![Symbolizer::Point(PointSym {
            radius,
            fill,
            stroke: [0, 0, 0, 0],
            stroke_width: 0.0,
        })],
        title: None,
    }
}

/// Is there any fully-opaque pixel of exactly this RGB? (A rule's marker drew iff its fill shows.)
fn has_color(rgba: &[u8], c: [u8; 3]) -> bool {
    rgba.chunks(4)
        .any(|p| p[3] == 255 && p[0] == c[0] && p[1] == c[1] && p[2] == c[2])
}

const RED: [u8; 3] = [220, 20, 20];
const BLUE: [u8; 3] = [20, 20, 220];

/// CORR-1 core: rules within a FeatureTypeStyle are **additive**. A feature matching two non-else
/// rules gets the UNION of their symbolizers — here a large RED marker (rule A, `scalerank <= 4`)
/// AND a small BLUE marker (rule B, `scalerank <= 8`) drawn on top. Under the old first-match
/// selection only rule A (RED) would draw; BLUE's presence is the proof that BOTH rules fired.
#[test]
fn additive_rules_union_both_symbolizers() {
    let src = point_source(&[(5.0, 50.0, 2)]); // scalerank 2 → matches BOTH <=4 and <=8
    let sh = shaper();
    let style = Style {
        feature_type_styles: vec![FeatureTypeStyle {
            rules: vec![
                point_only_rule(
                    Some(Filter::Cmp(Cmp::Le, "scalerank".into(), "4".into())),
                    [220, 20, 20, 255],
                    8.0,
                ),
                point_only_rule(
                    Some(Filter::Cmp(Cmp::Le, "scalerank".into(), "8".into())),
                    [20, 20, 220, 255],
                    3.0,
                ),
            ],
        }],
    };
    let rgba = render_vector(
        &src,
        &style,
        "EPSG:4326",
        "EPSG:3857",
        EUROPE_BBOX,
        512,
        512,
        &sh,
    )
    .unwrap();
    assert!(has_color(&rgba, RED), "rule A (red) marker drawn");
    assert!(
        has_color(&rgba, BLUE),
        "rule B (blue) marker ALSO drawn — additive union, not first-match"
    );
}

/// The `airports.sld` fix (Fix C): making the minor tier `scalerank >= 5 AND <= 8` carves it
/// disjoint from the major tier (`<= 4`), so a scalerank-2 major draws ONLY the major (red)
/// marker, never a second minor (blue) one — the doubling the europe-512 golden can't reveal
/// (there the minor tier is scale-inactive). Same two overlapping-looking rules as the additive
/// test above, but with the exclusion conjunct → exactly one marker.
#[test]
fn mutually_exclusive_tiers_draw_single_marker() {
    let src = point_source(&[(5.0, 50.0, 2)]);
    let sh = shaper();
    let style = Style {
        feature_type_styles: vec![FeatureTypeStyle {
            rules: vec![
                point_only_rule(
                    Some(Filter::Cmp(Cmp::Le, "scalerank".into(), "4".into())),
                    [220, 20, 20, 255],
                    8.0,
                ),
                point_only_rule(
                    Some(Filter::And(vec![
                        Filter::Cmp(Cmp::Ge, "scalerank".into(), "5".into()),
                        Filter::Cmp(Cmp::Le, "scalerank".into(), "8".into()),
                    ])),
                    [20, 20, 220, 255],
                    3.0,
                ),
            ],
        }],
    };
    let rgba = render_vector(
        &src,
        &style,
        "EPSG:4326",
        "EPSG:3857",
        EUROPE_BBOX,
        512,
        512,
        &sh,
    )
    .unwrap();
    assert!(has_color(&rgba, RED), "major (red) marker drawn");
    assert!(
        !has_color(&rgba, BLUE),
        "minor (blue) NOT drawn — mutually-exclusive tiers, no doubling"
    );
}

/// TG1 — `<ElseFilter/>` is **fallback-only**: it styles a feature ONLY when no non-else rule
/// (active at the same scale) matched it. Rule A = `scalerank < 3` → RED; rule B = else → BLUE.
#[test]
fn elsefilter_is_fallback_only() {
    let sh = shaper();
    let mut else_rule = point_only_rule(None, [20, 20, 220, 255], 6.0);
    else_rule.else_filter = true;
    let style = Style {
        feature_type_styles: vec![FeatureTypeStyle {
            rules: vec![
                point_only_rule(
                    Some(Filter::Cmp(Cmp::Lt, "scalerank".into(), "3".into())),
                    [220, 20, 20, 255],
                    6.0,
                ),
                else_rule,
            ],
        }],
    };

    // Mixed data: scalerank 2 matches A (red); scalerank 7 matches no non-else rule → else B (blue).
    let mixed = point_source(&[(0.0, 50.0, 2), (15.0, 45.0, 7)]);
    let rgba = render_vector(
        &mixed,
        &style,
        "EPSG:4326",
        "EPSG:3857",
        EUROPE_BBOX,
        512,
        512,
        &sh,
    )
    .unwrap();
    assert!(has_color(&rgba, RED), "matched feature gets rule A (red)");
    assert!(
        has_color(&rgba, BLUE),
        "unmatched feature falls through to the else rule (blue)"
    );

    // Every feature matches A → the else rule must NOT fire at all.
    let all_a = point_source(&[(0.0, 50.0, 2), (15.0, 45.0, 1)]);
    let rgba2 = render_vector(
        &all_a,
        &style,
        "EPSG:4326",
        "EPSG:3857",
        EUROPE_BBOX,
        512,
        512,
        &sh,
    )
    .unwrap();
    assert!(has_color(&rgba2, RED), "all features match A (red)");
    assert!(
        !has_color(&rgba2, BLUE),
        "else rule must not fire when a non-else rule matched every feature"
    );
}
