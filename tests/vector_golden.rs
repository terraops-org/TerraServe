use terraserve::vector::geojson::GeoJsonSource;
use terraserve::vector::render::render_vector;
use terraserve::vector::shape::Shaper;
use terraserve::vector::style::{FeatureTypeStyle, PolygonSym, Rule, Style, Symbolizer};

/// The canonical MVP render — a Europe window at 512×512. Shared by the determinism + golden tests.
fn render_airports_europe() -> Vec<u8> {
    let src = GeoJsonSource::load("fixtures/vector/airports.geojson").unwrap();
    let text = std::fs::read_to_string("fixtures/styles/airports.vec.json").unwrap();
    let style = Style::from_json_str(&text).unwrap();
    let sh =
        Shaper::from_font_bytes(&std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap()).unwrap();
    let bbox = [-1_500_000.0, 4_000_000.0, 3_000_000.0, 8_000_000.0];
    let rgba = render_vector(&src, &style, "EPSG:4326", "EPSG:3857", bbox, 512, 512, &sh).unwrap();
    terraserve::pngio::encode_rgba(&rgba, 512, 512).unwrap()
}

/// The same geographic window, but rendered in a **geographic grid CRS** (EPSG:4326) instead of
/// EPSG:3857. This is the one path where the shim declutter's metres-per-degree factor is
/// exercised: `render.rs`'s `res_m = res_grid × meters_per_unit(grid_crs)` returns 111_319.49 here
/// (1.0 for the projected europe golden), so `eff_zoom`/`max_priority` actually depend on it.
/// Pins SIMP-1 (`meters_per_unit` unified vs the old hard-coded 111_320.0) against silent
/// regression on the declutter path the EPSG:3857 goldens can't reach.
fn render_airports_wgs84() -> Vec<u8> {
    let src = GeoJsonSource::load("fixtures/vector/airports.geojson").unwrap();
    let text = std::fs::read_to_string("fixtures/styles/airports.vec.json").unwrap();
    let style = Style::from_json_str(&text).unwrap();
    let sh =
        Shaper::from_font_bytes(&std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap()).unwrap();
    // Same airports as the europe golden, expressed as a lon/lat (degrees) window.
    let bbox = [-13.47, 33.75, 26.95, 57.33];
    let rgba = render_vector(&src, &style, "EPSG:4326", "EPSG:4326", bbox, 512, 512, &sh).unwrap();
    terraserve::pngio::encode_rgba(&rgba, 512, 512).unwrap()
}

/// Regenerate the committed goldens. Run intentionally: `cargo test --test vector_golden
/// gen_golden -- --ignored`, eyeball the PNGs, then commit.
#[test]
#[ignore]
fn gen_golden() {
    std::fs::create_dir_all("fixtures/goldens").unwrap();
    std::fs::write(
        "fixtures/goldens/airports_europe_512.png",
        render_airports_europe(),
    )
    .unwrap();
    std::fs::write(
        "fixtures/goldens/airports_wgs84_512.png",
        render_airports_wgs84(),
    )
    .unwrap();
}

#[test]
fn render_is_deterministic() {
    // Same input twice, in-process → byte-identical (spec §8 determinism test).
    assert_eq!(render_airports_europe(), render_airports_europe());
}

#[test]
fn matches_golden() {
    let png = render_airports_europe();
    let golden = std::fs::read("fixtures/goldens/airports_europe_512.png")
        .expect("golden missing — generate it once with GEN_GOLDEN then commit");
    assert_eq!(
        png.len(),
        golden.len(),
        "golden size mismatch — regenerate intentionally on design change"
    );
    assert_eq!(png, golden, "output differs from committed golden");
}

/// Task 7: `render_vector` draws Polygon geometry (fills + outline stroke) under the marker/label
/// layer via a `GeomLayer` seeded into the `Canvas`. The countries fixture, plain single-rule
/// Polygon style, same Europe window as the airport goldens above. Raw RGBA8 (not PNG-encoded —
/// the committed byte-golden itself is Task 8's job); this just pins render-time determinism +
/// proves the geometry pass actually draws (a transparent canvas would make the byte-identical
/// assertion vacuous).
fn render_countries_polygons() -> Vec<u8> {
    let src = GeoJsonSource::load("fixtures/vector/countries.geojson").unwrap();
    let sh =
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
    // Same Europe/EPSG:3857 window as `render_airports_europe` — guaranteed to overlap several
    // country polygons.
    let bbox = [-1_500_000.0, 4_000_000.0, 3_000_000.0, 8_000_000.0];
    render_vector(&src, &style, "EPSG:4326", "EPSG:3857", bbox, 512, 512, &sh).unwrap()
}

/// Task 8: countries fill golden. Same fixture/window as `render_countries_polygons` above, but
/// loads the committed `countries.vec.json` (end-to-end JSON-front-end coverage, not a hand-built
/// `Style`) and PNG-encodes the result — the byte-exact artifact `matches_golden_countries` pins.
fn render_countries_rgba() -> Vec<u8> {
    let src = GeoJsonSource::load("fixtures/vector/countries.geojson").unwrap();
    let text = std::fs::read_to_string("fixtures/styles/countries.vec.json").unwrap();
    let style = Style::from_json_str(&text).unwrap();
    let sh =
        Shaper::from_font_bytes(&std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap()).unwrap();
    // Same Europe/EPSG:3857 window as the airport + countries-determinism goldens.
    let bbox = [-1_500_000.0, 4_000_000.0, 3_000_000.0, 8_000_000.0];
    render_vector(&src, &style, "EPSG:4326", "EPSG:3857", bbox, 512, 512, &sh).unwrap()
}
fn render_countries() -> Vec<u8> {
    terraserve::pngio::encode_rgba(&render_countries_rgba(), 512, 512).unwrap()
}

/// Task 8: roads line golden. `roads.geojson` (1043 LineString features across Europe) styled by
/// the committed `roads.vec.json`, same window/size as the other geometry goldens.
fn render_roads_rgba() -> Vec<u8> {
    let src = GeoJsonSource::load("fixtures/vector/roads.geojson").unwrap();
    let text = std::fs::read_to_string("fixtures/styles/roads.vec.json").unwrap();
    let style = Style::from_json_str(&text).unwrap();
    let sh =
        Shaper::from_font_bytes(&std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap()).unwrap();
    let bbox = [-1_500_000.0, 4_000_000.0, 3_000_000.0, 8_000_000.0];
    render_vector(&src, &style, "EPSG:4326", "EPSG:3857", bbox, 512, 512, &sh).unwrap()
}
fn render_roads() -> Vec<u8> {
    terraserve::pngio::encode_rgba(&render_roads_rgba(), 512, 512).unwrap()
}

/// Regenerate the Task 8 geometry goldens. Run intentionally: `cargo test --test vector_golden
/// gen_golden_geom -- --ignored`, eyeball the PNGs, then commit.
#[test]
#[ignore]
fn gen_golden_geom() {
    std::fs::create_dir_all("fixtures/goldens").unwrap();
    std::fs::write(
        "fixtures/goldens/countries_fill_512.png",
        render_countries(),
    )
    .unwrap();
    std::fs::write("fixtures/goldens/roads_line_512.png", render_roads()).unwrap();
}

#[test]
fn render_polygons_is_deterministic() {
    let a = render_countries_polygons();
    let opaque = a.chunks(4).filter(|p| p[3] > 0).count();
    assert!(
        opaque > 0,
        "polygon style must actually draw geometry, got 0 opaque px"
    );
    let b = render_countries_polygons();
    assert_eq!(
        a, b,
        "polygon render must be byte-identical across identical runs"
    );
}

#[test]
fn wgs84_render_is_deterministic() {
    assert_eq!(render_airports_wgs84(), render_airports_wgs84());
}

#[test]
fn matches_golden_wgs84() {
    let png = render_airports_wgs84();
    let golden = std::fs::read("fixtures/goldens/airports_wgs84_512.png")
        .expect("wgs84 golden missing — generate it once with gen_golden --ignored then commit");
    assert_eq!(
        png.len(),
        golden.len(),
        "wgs84 golden size mismatch — regenerate intentionally on design change"
    );
    assert_eq!(
        png, golden,
        "EPSG:4326 shim-declutter output differs from committed golden (SIMP-1 path)"
    );
}

/// Task 8: countries fill render is byte-identical run-to-run (no float/hash-order nondeterminism
/// in the polygon rasterizer's per-feature draw loop).
#[test]
fn countries_render_is_deterministic() {
    let a = render_countries_rgba();
    // Non-vacuous guard: a blank canvas would make the byte-identical assertion — and any golden
    // regenerated from it — meaningless, so require the polygon fill to actually draw.
    let opaque = a.chunks(4).filter(|p| p[3] > 0).count();
    assert!(
        opaque > 0,
        "countries fill must draw geometry, got 0 opaque px"
    );
    assert_eq!(
        a,
        render_countries_rgba(),
        "countries render must be byte-identical across runs"
    );
}

#[test]
fn matches_golden_countries() {
    let png = render_countries();
    let golden = std::fs::read("fixtures/goldens/countries_fill_512.png").expect(
        "countries golden missing — generate it once with `gen_golden_geom -- --ignored` then commit",
    );
    assert_eq!(
        png.len(),
        golden.len(),
        "countries golden size mismatch — regenerate intentionally on design change"
    );
    assert_eq!(
        png, golden,
        "countries fill render differs from the committed golden"
    );
}

/// Task 8: roads line render is byte-identical run-to-run.
#[test]
fn roads_render_is_deterministic() {
    let a = render_roads_rgba();
    let opaque = a.chunks(4).filter(|p| p[3] > 0).count();
    assert!(
        opaque > 0,
        "roads stroke must draw geometry, got 0 opaque px"
    );
    assert_eq!(
        a,
        render_roads_rgba(),
        "roads render must be byte-identical across runs"
    );
}

#[test]
fn matches_golden_roads() {
    let png = render_roads();
    let golden = std::fs::read("fixtures/goldens/roads_line_512.png").expect(
        "roads golden missing — generate it once with `gen_golden_geom -- --ignored` then commit",
    );
    assert_eq!(
        png.len(),
        golden.len(),
        "roads golden size mismatch — regenerate intentionally on design change"
    );
    assert_eq!(
        png, golden,
        "roads line render differs from the committed golden"
    );
}
