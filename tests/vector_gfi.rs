//! GetFeatureInfo over a vector (label) layer — real assertions (not just "never panics").
//!
//! Covers the Fable senior consult (missed-item a): `Props::get_str` returned `None` for a
//! `Value::Num`, so a numeric label field (a) rendered a blank label (`render.rs`) and (b) made
//! GetFeatureInfo report "(no feature at this location)" for a real hit — a false negative,
//! because the old code drove the "no feature" branch off the label lookup instead of the
//! nearest-feature `Option`. These tests pin both the string-label happy path and the
//! numeric-label regression, plus the true no-hit (empty ocean) case so the three states
//! (string hit / numeric hit / no hit) stay distinguishable.

use std::sync::Arc;
use terraserve::server::{Layer, VectorLayer};
use terraserve::vector::geojson::GeoJsonSource;
use terraserve::vector::geom::Projector;
use terraserve::vector::render::render_vector;
use terraserve::vector::shape::Shaper;
use terraserve::vector::source::{FeatureSource, VectorSource};
use terraserve::vector::style::Style;

/// The same Europe window as `tests/vector_wms.rs` / `tests/vector_golden.rs`.
const BBOX: [f64; 4] = [-1_500_000.0, 4_000_000.0, 3_000_000.0, 8_000_000.0];
const EUROPE_3857: &str =
    "CRS=EPSG:3857&BBOX=-1500000,4000000,3000000,8000000&WIDTH=512&HEIGHT=512";

/// A known feature well inside the Europe window, with an unambiguous string name and a
/// low-ish `scalerank` so it isn't decluttered by the auto-declutter shim at this zoom.
const BRISTOL_LON: f64 = -2.710864691343084;
const BRISTOL_LAT: f64 = 51.386293418914839;
const BRISTOL_NAME: &str = "Bristol Int'l";
const BRISTOL_SCALERANK: &str = "8"; // `scalerank: 8` in fixtures/vector/airports.geojson

fn vector_layer(style: Style) -> Layer {
    let src = Arc::new(GeoJsonSource::load("fixtures/vector/airports.geojson").unwrap());
    let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
    let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
    let ext = src.full_extent();
    Layer {
        name: "airports".into(),
        cog_path: String::new(),
        cog: None,
        source: None,
        style: None,
        src_crs: "EPSG:4326".into(),
        band_math: None,
        bounds_wgs84: ext,
        tile_cache: None,
        index_cache: terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes()),
        grids: Vec::new(),
        vector: Some(VectorLayer {
            fields: terraserve::mvt_http::feature_field_schema(src.as_ref()),
            area_scale: 0.0, // MVT-only knob, unused by this test
            source: VectorSource::LoadAll(src),
            style,
            shaper,
            lod: None,
        }),
        pmtiles: None,
        overlay: None,
    }
}

fn string_label_style() -> Style {
    let text = std::fs::read_to_string("fixtures/styles/airports.vec.json").unwrap();
    Style::from_json_str(&text).unwrap()
}

/// A style whose label field is NUMERIC (`scalerank`, an `i64`-in-`f64` attribute) — pins
/// missed-item a. `text.label` must be a field that exists as `Value::Num` on every feature.
fn numeric_label_style() -> Style {
    let json = r#"{
        "mode": "vector",
        "point": { "radius": 3.0, "fill": [30, 30, 30, 255], "stroke": [255, 255, 255, 255], "stroke_width": 1.0 },
        "text": {
            "label": "scalerank", "priority": "scalerank",
            "size": 16.0, "color": [20, 20, 20, 255],
            "halo": { "color": [255, 255, 255, 230], "radius": 2.0 },
            "offset": 4.0
        }
    }"#;
    Style::from_json_str(json).unwrap()
}

/// Project a source-CRS (lon,lat) to the integer WMS `I,J` pixel of the shared Europe window —
/// the exact same transform `get_feature_info_vector` uses internally, so the query pixel lands
/// well inside the 12px hit tolerance.
fn pixel_of(lon: f64, lat: f64) -> (i64, i64) {
    let proj = Projector::new("EPSG:4326", "EPSG:3857", BBOX, 512, 512).unwrap();
    let (px, py) = proj.to_pixel(lon, lat).unwrap();
    (px.round() as i64, py.round() as i64)
}

fn body(layers: &[Layer], i: i64, j: i64, info_format: &str) -> (String, Option<String>) {
    let q = format!(
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS=airports&QUERY_LAYERS=airports&\
         STYLES=&{EUROPE_3857}&I={i}&J={j}&INFO_FORMAT={info_format}"
    );
    let r = terraserve::wms::handle_layers(layers, &q, None);
    (
        String::from_utf8_lossy(&r.bytes).into_owned(),
        r.content_type,
    )
}

#[test]
fn getfeatureinfo_text_plain_hits_known_airport_by_string_label() {
    let layers = vec![vector_layer(string_label_style())];
    let (px, py) = pixel_of(BRISTOL_LON, BRISTOL_LAT);
    let (text, ct) = body(&layers, px, py, "text/plain");
    assert_eq!(ct.as_deref(), Some("text/plain"));
    assert!(
        text.contains(BRISTOL_NAME),
        "expected the airport's name in the GFI body, got: {text:?}"
    );
    assert!(
        !text.contains("no feature"),
        "a real hit must not report no-feature, got: {text:?}"
    );
}

#[test]
fn getfeatureinfo_json_hits_known_airport_with_name_and_point() {
    let layers = vec![vector_layer(string_label_style())];
    let (px, py) = pixel_of(BRISTOL_LON, BRISTOL_LAT);
    let (text, ct) = body(&layers, px, py, "application/json");
    assert_eq!(ct.as_deref(), Some("application/json"));
    let json: serde_json::Value = serde_json::from_str(&text).expect("valid JSON body");
    assert_eq!(json["type"], "FeatureCollection");
    let feats = json["features"].as_array().expect("features array");
    assert_eq!(feats.len(), 1, "exactly one feature hit, got: {text:?}");
    assert_eq!(feats[0]["properties"]["name"], BRISTOL_NAME);
    assert_eq!(feats[0]["geometry"]["type"], "Point");
    let coords = feats[0]["geometry"]["coordinates"]
        .as_array()
        .expect("coordinates array");
    assert!((coords[0].as_f64().unwrap() - BRISTOL_LON).abs() < 1e-6);
    assert!((coords[1].as_f64().unwrap() - BRISTOL_LAT).abs() < 1e-6);
}

#[test]
fn getfeatureinfo_empty_ocean_reports_no_feature() {
    let layers = vec![vector_layer(string_label_style())];
    // Pixel (10, 250): >100px from every airport in the Europe window fixture (checked against
    // the full fixture set) — well outside the 12px hit tolerance.
    let (text, ct) = body(&layers, 10, 250, "text/plain");
    assert_eq!(ct.as_deref(), Some("text/plain"));
    assert!(
        text.contains("no feature at this location"),
        "empty water must report no-feature, got: {text:?}"
    );
}

/// The regression test for missed-item a: a NUMERIC label field must still report on a real hit,
/// not fall into the "no feature" branch. Before the fix, `get_str` returned `None` for
/// `Value::Num(_)`, and `get_feature_info_vector` drove the no-feature message off that `None` —
/// so a real hit on a numeric-labeled layer was misreported as no-feature.
#[test]
fn getfeatureinfo_numeric_label_field_reports_value_not_no_feature() {
    let layers = vec![vector_layer(numeric_label_style())];
    let (px, py) = pixel_of(BRISTOL_LON, BRISTOL_LAT);

    let (text, _) = body(&layers, px, py, "text/plain");
    assert!(
        !text.contains("no feature"),
        "a real hit on a numeric-labeled layer must not report no-feature, got: {text:?}"
    );
    assert!(
        text.contains(BRISTOL_SCALERANK),
        "expected the numeric scalerank value ({BRISTOL_SCALERANK}) in the GFI body, got: {text:?}"
    );
    // The old bug would have printed an empty/absent label, never a trailing `.0` — but assert
    // the negative explicitly so a future regression to `.to_string()`-on-f64 is caught too.
    assert!(
        !text.contains("8.0"),
        "an integer-valued numeric label must print without a trailing .0, got: {text:?}"
    );

    let (json_text, _) = body(&layers, px, py, "application/json");
    let json: serde_json::Value = serde_json::from_str(&json_text).expect("valid JSON body");
    // JSON GFI now returns the feature's RAW properties (not a synthesized `name` = label): the
    // numeric `scalerank` attribute rides through as a JSON number, and the string `name` attribute
    // is present too — a client's Identify panel gets every real attribute.
    assert_eq!(
        json["features"][0]["properties"]["scalerank"].as_f64(),
        BRISTOL_SCALERANK.parse::<f64>().ok(),
        "JSON GFI must carry the raw numeric attribute as a number, got: {json_text:?}"
    );
    assert_eq!(
        json["features"][0]["properties"]["name"], BRISTOL_NAME,
        "JSON GFI must carry all raw properties, got: {json_text:?}"
    );
}

/// A feature hit whose label field is present but the *value itself* is absent/null must still
/// report the feature (not "no feature") — the discriminator is the nearest-feature hit, never
/// the label string.
#[test]
fn getfeatureinfo_hit_with_missing_label_field_still_reports_hit() {
    let json = r#"{
        "mode": "vector",
        "point": { "radius": 3.0, "fill": [30, 30, 30, 255], "stroke": [255, 255, 255, 255], "stroke_width": 1.0 },
        "text": {
            "label": "no_such_field", "priority": "scalerank",
            "size": 16.0, "color": [20, 20, 20, 255],
            "halo": { "color": [255, 255, 255, 230], "radius": 2.0 },
            "offset": 4.0
        }
    }"#;
    let style = Style::from_json_str(json).unwrap();
    let layers = vec![vector_layer(style)];
    let (px, py) = pixel_of(BRISTOL_LON, BRISTOL_LAT);
    let (text, _) = body(&layers, px, py, "text/plain");
    assert!(
        !text.contains("no feature at this location"),
        "a real hit with an absent label field must not be misreported as no-feature, got: {text:?}"
    );
}

/// Sibling of the above: a GFI hit on a feature whose label field is absent must report the
/// literal string `(unnamed)`, never an empty label and never "no feature". This pins the
/// `wms.rs` empty/None -> "(unnamed)" mapping that Task 1 (label/priority IR spine) evaluates
/// via `Style::primary_label` + `eval_label` instead of the old `primary_label_field` +
/// `get_display` pair.
#[test]
fn getfeatureinfo_hit_with_absent_label_field_reports_unnamed() {
    let json = r#"{
        "mode": "vector",
        "point": { "radius": 3.0, "fill": [30, 30, 30, 255], "stroke": [255, 255, 255, 255], "stroke_width": 1.0 },
        "text": {
            "label": "no_such_field", "priority": "scalerank",
            "size": 16.0, "color": [20, 20, 20, 255],
            "halo": { "color": [255, 255, 255, 230], "radius": 2.0 },
            "offset": 4.0
        }
    }"#;
    let style = Style::from_json_str(json).unwrap();
    let layers = vec![vector_layer(style)];
    let (px, py) = pixel_of(BRISTOL_LON, BRISTOL_LAT);
    let (text, _) = body(&layers, px, py, "text/plain");
    assert!(
        text.contains("(unnamed)"),
        "hit on field-less feature must report (unnamed), got: {text:?}"
    );
}

/// Render-side proof that `render.rs` uses `get_display` (not `get_str`): a style whose label
/// field is numeric (`scalerank`) must draw MORE opaque pixels than the identical style pointed
/// at a field that is absent on every feature (blank text, zero glyphs) — same markers, same
/// bbox, same declutter, the only difference is whether the label field resolves to text.
#[test]
fn numeric_label_field_draws_more_than_blank_label_field() {
    let src = GeoJsonSource::load("fixtures/vector/airports.geojson").unwrap();
    let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
    let sh = Shaper::from_font_bytes(&font).unwrap();
    let bbox = BBOX;

    let numeric = numeric_label_style();
    let blank = Style::from_json_str(
        r#"{
            "mode": "vector",
            "point": { "radius": 3.0, "fill": [30, 30, 30, 255], "stroke": [255, 255, 255, 255], "stroke_width": 1.0 },
            "text": {
                "label": "no_such_field", "priority": "scalerank",
                "size": 16.0, "color": [20, 20, 20, 255],
                "halo": { "color": [255, 255, 255, 230], "radius": 2.0 },
                "offset": 4.0
            }
        }"#,
    )
    .unwrap();

    let rgba_numeric = render_vector(
        &src,
        &numeric,
        "EPSG:4326",
        "EPSG:3857",
        bbox,
        512,
        512,
        &sh,
    )
    .unwrap();
    let rgba_blank =
        render_vector(&src, &blank, "EPSG:4326", "EPSG:3857", bbox, 512, 512, &sh).unwrap();

    let opaque = |rgba: &[u8]| rgba.chunks(4).filter(|p| p[3] > 0).count();
    let (n_numeric, n_blank) = (opaque(&rgba_numeric), opaque(&rgba_blank));
    assert!(
        n_numeric > n_blank,
        "numeric label field must draw glyphs beyond the (identical) markers: \
         numeric={n_numeric} px, blank-label={n_blank} px"
    );
}
