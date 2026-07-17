use terraserve::vector::geojson::GeoJsonSource;
use terraserve::vector::render::render_vector;
use terraserve::vector::shape::Shaper;
use terraserve::vector::style::{
    Cmp, FeatureTypeStyle, Filter, LabelPart, LineSym, PointSym, PolygonSym, Priority, Rule, Style,
    Symbolizer, TextSym,
};

fn shaper() -> Shaper {
    Shaper::from_font_bytes(&std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap()).unwrap()
}

fn airports_style() -> Style {
    let text = std::fs::read_to_string("fixtures/styles/airports.vec.json").unwrap();
    Style::from_json_str(&text).unwrap()
}

fn opaque(rgba: &[u8]) -> usize {
    rgba.chunks(4).filter(|p| p[3] > 0).count()
}

#[test]
fn renders_airports_with_labels() {
    let src = GeoJsonSource::load("fixtures/vector/airports.geojson").unwrap();
    let style = airports_style();
    let sh = shaper();
    // Europe-ish window in EPSG:3857
    let bbox = [-1_500_000.0, 4_000_000.0, 3_000_000.0, 8_000_000.0];
    let rgba = render_vector(&src, &style, "EPSG:4326", "EPSG:3857", bbox, 512, 512, &sh).unwrap();
    assert_eq!(rgba.len(), 512 * 512 * 4);
    let n = opaque(&rgba);
    assert!(n > 100, "markers + labels drawn, got {n} opaque px");
}

#[test]
fn empty_ocean_is_transparent() {
    let src = GeoJsonSource::load("fixtures/vector/airports.geojson").unwrap();
    let style = airports_style();
    let sh = shaper();
    // Deep South Pacific — no airports within the margin.
    let bbox = [-13_400_000.0, -7_450_000.0, -13_300_000.0, -7_350_000.0];
    let rgba = render_vector(&src, &style, "EPSG:4326", "EPSG:3857", bbox, 256, 256, &sh).unwrap();
    assert!(
        rgba.chunks(4).all(|p| p[3] == 0),
        "empty viewport fully transparent"
    );
}

/// A rule gated by BOTH a scale range and an attribute filter is honored at render time: at a scale
/// where the rule is active, only `scalerank < 3` features draw; at a scale where the rule is
/// inactive (the request's scale denominator is >= `max_scale`), the rule styles nothing → the
/// image is empty. Proves render.rs computes the OGC scale denominator and selects symbolizers by
/// scale + filter (Task 7).
#[test]
fn scale_and_filter_gate_rule() {
    let src = GeoJsonSource::load("fixtures/vector/airports.geojson").unwrap();
    let sh = shaper();
    // Europe window in EPSG:3857; ~4.5e6 m wide. scaleDenominator = (width_m / px) / 0.00028.
    let bbox = [-1_500_000.0, 4_000_000.0, 3_000_000.0, 8_000_000.0];

    // One rule: only features with scalerank < 3, and only where scaleDenominator < 3e7.
    let point = PointSym {
        radius: 3.0,
        fill: [30, 30, 30, 255],
        stroke: [255, 255, 255, 255],
        stroke_width: 1.0,
    };
    let text = TextSym {
        label: vec![terraserve::vector::style::LabelPart::Field("name".into())],
        priority: Some(terraserve::vector::style::Priority::Field(
            "scalerank".into(),
        )),
        priority_higher_wins: false,
        size: 16.0,
        color: [20, 20, 20, 255],
        halo_color: [255, 255, 255, 230],
        halo_radius: 2.0,
        offset: 4.0,
    };
    let style = Style {
        feature_type_styles: vec![FeatureTypeStyle {
            rules: vec![Rule {
                filter: Some(Filter::Cmp(Cmp::Lt, "scalerank".into(), "3".into())),
                else_filter: false,
                min_scale: None,
                max_scale: Some(3.0e7),
                symbolizers: vec![Symbolizer::Point(point), Symbolizer::Text(text)],
                title: None,
            }],
        }],
    };

    // 1024 px over 4.5e6 m → res ≈ 4394 m/px → scaleDenom ≈ 1.57e7 < 3e7 → rule ACTIVE.
    let active = render_vector(
        &src,
        &style,
        "EPSG:4326",
        "EPSG:3857",
        bbox,
        1024,
        1024,
        &sh,
    )
    .unwrap();
    let active_opaque = opaque(&active);

    // 256 px over the same bbox → res ≈ 17578 m/px → scaleDenom ≈ 6.28e7 >= 3e7 → rule INACTIVE.
    let inactive =
        render_vector(&src, &style, "EPSG:4326", "EPSG:3857", bbox, 256, 256, &sh).unwrap();
    let inactive_opaque = opaque(&inactive);

    assert!(
        active_opaque > 0,
        "rule active (scaleDenom < max_scale): scalerank<3 features drawn, got {active_opaque} px"
    );
    assert_eq!(
        inactive_opaque, 0,
        "rule inactive (scaleDenom >= max_scale): nothing drawn, got {inactive_opaque} px"
    );
    assert_ne!(
        active_opaque, inactive_opaque,
        "scale gating changes the render"
    );

    // The attribute filter also bites: an all-admitting filter at the active scale draws strictly
    // more (there are scalerank>=3 airports in view too), proving `scalerank<3` really filtered.
    let style_all = Style {
        feature_type_styles: vec![FeatureTypeStyle {
            rules: vec![Rule {
                filter: Some(Filter::Cmp(Cmp::Lt, "scalerank".into(), "100".into())),
                ..style.feature_type_styles[0].rules[0].clone()
            }],
        }],
    };
    let all = render_vector(
        &src,
        &style_all,
        "EPSG:4326",
        "EPSG:3857",
        bbox,
        1024,
        1024,
        &sh,
    )
    .unwrap();
    assert!(
        opaque(&all) > active_opaque,
        "unfiltered draws more than scalerank<3 ({} vs {})",
        opaque(&all),
        active_opaque
    );
}

// -------------------------------------------------------------------------------------------
// Task 3 (CORR-6): SLD `<Priority>` is higher-wins — end-to-end render of a contested slot.
// -------------------------------------------------------------------------------------------

/// Build a 5-feature source: `A`/`B` sit at the EXACT same anchor (same marker radius, same
/// label text/size, so their label boxes are geometrically identical) and contest one placement
/// slot, styled by two rules (`id == "A"` / `id == "B"`) whose only visible difference is the text
/// color (red vs blue). `prio_a`/`prio_b` feed the `prio` property read via `Priority::Field` +
/// `priority_higher_wins: true` — the SLD-sourced direction fold in `render.rs` — so this exercises
/// `sld_lower::lower_priority_expr`'s consumer end-to-end: real GeoJSON parse → real filter
/// selection → real `eval_priority` + negation → the real placement kernel → real pixel output.
///
/// `place::candidates` offsets its 8 slots from the marker edge by `e = r + offset`: for any
/// `e >= 0` the east-side slots (E/NE/SE) and west-side slots (NW/SW/W) sit in provably disjoint
/// x-ranges, so with only A and B in the scene the loser can ALWAYS dodge to the opposite side —
/// no amount of size/offset tuning forces a real 8-way collision from just two same-anchor items
/// (verified empirically while building this test: both colors were present regardless of
/// priority order). Three extra zero-width "blocker" features (`BLK1..3`) — whose markers, like ALL
/// markers, are seeded into the collision grid as obstacles before any label is placed (this does
/// not depend on their priority) — sit immediately west of the shared anchor, sized to cover the
/// NW/SW/W/N/S candidate region while staying clear of the E/NE/SE region — so whichever of A/B is
/// processed first (by priority) still freely claims E, but the other is now genuinely blocked on
/// every one of its 8 candidates.
/// Coordinates are chosen in `EPSG:3857 -> EPSG:3857` (identity projection) so pixel positions are
/// exact integers, not approximated through Mercator.
fn render_contested(prio_a: f64, prio_b: f64) -> Vec<u8> {
    let geojson = format!(
        r#"{{"type":"FeatureCollection","features":[
        {{"type":"Feature","properties":{{"id":"A","prio":{prio_a}}},"geometry":{{"type":"Point","coordinates":[512.0,512.0]}}}},
        {{"type":"Feature","properties":{{"id":"B","prio":{prio_b}}},"geometry":{{"type":"Point","coordinates":[512.0,512.0]}}}},
        {{"type":"Feature","properties":{{"id":"BLK1","prio":1000000}},"geometry":{{"type":"Point","coordinates":[503.0,528.0]}}}},
        {{"type":"Feature","properties":{{"id":"BLK2","prio":1000000}},"geometry":{{"type":"Point","coordinates":[503.0,512.0]}}}},
        {{"type":"Feature","properties":{{"id":"BLK3","prio":1000000}},"geometry":{{"type":"Point","coordinates":[503.0,496.0]}}}}
        ]}}"#
    );
    let src = GeoJsonSource::from_str(&geojson).unwrap();
    let sh = shaper();

    let point = PointSym {
        radius: 4.0,
        fill: [30, 30, 30, 255],
        stroke: [255, 255, 255, 255],
        stroke_width: 0.0,
    };
    let mk_text = |color: [u8; 4]| TextSym {
        label: vec![LabelPart::Literal("X".to_string())],
        priority: Some(Priority::Field("prio".to_string())),
        priority_higher_wins: true, // SLD semantics: higher value wins the contested slot.
        size: 16.0,
        color,
        halo_color: [255, 255, 255, 230],
        halo_radius: 2.0,
        offset: 4.0,
    };
    let rule_for = |id: &str, color: [u8; 4]| Rule {
        filter: Some(Filter::Cmp(Cmp::Eq, "id".into(), id.into())),
        else_filter: false,
        min_scale: None,
        max_scale: None,
        symbolizers: vec![
            Symbolizer::Point(point.clone()),
            Symbolizer::Text(mk_text(color)),
        ],
        title: None,
    };
    let blocker_point = PointSym {
        radius: 15.0,
        fill: [30, 30, 30, 255],
        stroke: [255, 255, 255, 255],
        stroke_width: 0.0,
    };
    let blocker_rule = |id: &str| Rule {
        filter: Some(Filter::Cmp(Cmp::Eq, "id".into(), id.into())),
        else_filter: false,
        min_scale: None,
        max_scale: None,
        symbolizers: vec![
            Symbolizer::Point(blocker_point.clone()),
            Symbolizer::Text(TextSym {
                label: vec![], // empty -> width 0 -> never itself competes for a slot
                priority: Some(Priority::Field("prio".to_string())),
                priority_higher_wins: true,
                size: 16.0,
                color: [0, 0, 0, 0],
                halo_color: [0, 0, 0, 0],
                halo_radius: 0.0,
                offset: 4.0,
            }),
        ],
        title: None,
    };
    let style = Style {
        feature_type_styles: vec![FeatureTypeStyle {
            rules: vec![
                rule_for("A", [255, 0, 0, 255]), // pure red
                rule_for("B", [0, 0, 255, 255]), // pure blue
                blocker_rule("BLK1"),
                blocker_rule("BLK2"),
                blocker_rule("BLK3"),
            ],
        }],
    };

    let bbox = [0.0, 0.0, 1024.0, 1024.0];
    render_vector(
        &src,
        &style,
        "EPSG:3857",
        "EPSG:3857",
        bbox,
        1024,
        1024,
        &sh,
    )
    .unwrap()
}

/// True if any pixel is (close to) pure red — i.e. A's label glyphs were drawn.
fn has_red(rgba: &[u8]) -> bool {
    rgba.chunks(4)
        .any(|p| p[0] > 180 && p[1] < 80 && p[2] < 80 && p[3] > 128)
}
/// True if any pixel is (close to) pure blue — i.e. B's label glyphs were drawn.
fn has_blue(rgba: &[u8]) -> bool {
    rgba.chunks(4)
        .any(|p| p[2] > 180 && p[0] < 80 && p[1] < 80 && p[3] > 128)
}

/// SLD higher-wins: feature A (prio 1000) beats feature B (prio 1) for the one contested label
/// slot — A's red glyphs are drawn, B's blue glyphs are not.
#[test]
fn contested_priority_higher_value_wins() {
    let rgba = render_contested(1000.0, 1.0);
    assert!(
        has_red(&rgba),
        "higher-priority feature A's label should be placed"
    );
    assert!(
        !has_blue(&rgba),
        "lower-priority feature B's label should collide and be dropped"
    );
}

/// Direction check: swapping which feature carries the higher priority value flips the winner —
/// proves the fold is driven by the priority *value* (higher wins), not by feature order/fid.
#[test]
fn contested_priority_direction_flips_with_value() {
    let rgba = render_contested(1.0, 1000.0);
    assert!(
        has_blue(&rgba),
        "higher-priority feature B's label should be placed"
    );
    assert!(
        !has_red(&rgba),
        "lower-priority feature A's label should collide and be dropped"
    );
}

// -------------------------------------------------------------------------------------------
// Task 5 (T7): a rule draws ALL symbolizers of a kind, not just the first.
// -------------------------------------------------------------------------------------------

/// A single rule with TWO PointSymbolizers must draw BOTH markers for one feature (before T7 the
/// `find_map` first-only read drew only the big red one). A big red marker with a smaller blue
/// marker composited on top → both pure colors are present in the output; pre-T7 there would be no
/// blue. Identity projection so the feature lands dead-center.
#[test]
fn two_point_symbolizers_draw_two_markers() {
    let geojson = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","properties":{"id":"A"},"geometry":{"type":"Point","coordinates":[512.0,512.0]}}
    ]}"#;
    let src = GeoJsonSource::from_str(geojson).unwrap();
    let sh = shaper();
    let big_red = PointSym {
        radius: 12.0,
        fill: [255, 0, 0, 255],
        stroke: [0, 0, 0, 0],
        stroke_width: 0.0,
    };
    let small_blue = PointSym {
        radius: 4.0,
        fill: [0, 0, 255, 255],
        stroke: [0, 0, 0, 0],
        stroke_width: 0.0,
    };
    let style = Style {
        feature_type_styles: vec![FeatureTypeStyle {
            rules: vec![Rule {
                filter: None,
                else_filter: false,
                min_scale: None,
                max_scale: None,
                symbolizers: vec![Symbolizer::Point(big_red), Symbolizer::Point(small_blue)],
                title: None,
            }],
        }],
    };
    let bbox = [0.0, 0.0, 1024.0, 1024.0];
    let rgba = render_vector(
        &src,
        &style,
        "EPSG:3857",
        "EPSG:3857",
        bbox,
        1024,
        1024,
        &sh,
    )
    .unwrap();
    assert!(has_red(&rgba), "first PointSymbolizer (big red) drawn");
    assert!(
        has_blue(&rgba),
        "second PointSymbolizer (small blue) ALSO drawn — T7 draws all of a kind, not just the first"
    );
}

// -------------------------------------------------------------------------------------------
// Task 7 (CORR-2): per-FeatureTypeStyle semantics — ElseFilter scoped per FTS, and geometry
// composited per FTS in document order. Each test DISTINGUISHES per-FTS from the old flat model
// (both were verified to fail if the per-FTS machinery is reverted).
// -------------------------------------------------------------------------------------------

/// Per-FTS `<ElseFilter>` scoping. FTS1 has a rule matching feature A (big red marker); FTS2 has
/// ONLY an `<ElseFilter>` rule (small blue marker). Nothing in FTS2 matched A, so FTS2's else fires
/// → A draws BOTH markers (red annulus + blue center). A flattened/global model would see FTS1's
/// rule match and suppress the else entirely (red only) — `has_blue` is the discriminator.
#[test]
fn per_fts_elsefilter_fires_within_its_own_fts() {
    let geojson = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","properties":{"id":"A"},"geometry":{"type":"Point","coordinates":[512.0,512.0]}}
    ]}"#;
    let sh = shaper();
    let marker = |radius: f32, fill: [u8; 4]| {
        Symbolizer::Point(PointSym {
            radius,
            fill,
            stroke: [0, 0, 0, 0],
            stroke_width: 0.0,
        })
    };
    let fts_match = FeatureTypeStyle {
        rules: vec![Rule {
            filter: Some(Filter::Cmp(Cmp::Eq, "id".into(), "A".into())),
            else_filter: false,
            min_scale: None,
            max_scale: None,
            symbolizers: vec![marker(12.0, [255, 0, 0, 255])],
            title: None,
        }],
    };
    let fts_else = FeatureTypeStyle {
        rules: vec![Rule {
            filter: None,
            else_filter: true,
            min_scale: None,
            max_scale: None,
            symbolizers: vec![marker(5.0, [0, 0, 255, 255])],
            title: None,
        }],
    };
    let style = Style {
        feature_type_styles: vec![fts_match, fts_else],
    };
    let src = GeoJsonSource::from_str(geojson).unwrap();
    let rgba = render_vector(
        &src,
        &style,
        "EPSG:3857",
        "EPSG:3857",
        [0.0, 0.0, 1024.0, 1024.0],
        1024,
        1024,
        &sh,
    )
    .unwrap();
    assert!(has_red(&rgba), "FTS1's matching rule draws the red marker");
    assert!(
        has_blue(&rgba),
        "FTS2's <ElseFilter> fires within its own FTS (nothing in FTS2 matched A) — a flat model would suppress it"
    );
}

/// Per-FTS geometry z-order. Two OVERLAPPING polygons; FTS0 fills both green (opaque), FTS1 strokes
/// both with a blue boundary (transparent fill). Polygon A's right edge (x=5) lies INSIDE polygon B
/// (x in [4,8]). Under per-FTS compositing ALL fills (FTS0) are laid down before ANY boundary
/// (FTS1), so A's blue right edge sits on top of B's green fill → blue is present along that inner
/// edge. The old per-feature (interleaved) order draws A's boundary, THEN B's fill over it → green
/// there, no blue. The blue count in the A-right-edge strip is the discriminator.
#[test]
fn per_fts_geometry_boundaries_composite_over_all_fills() {
    let geojson = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","properties":{},"geometry":{"type":"Polygon","coordinates":[[[1,1],[5,1],[5,9],[1,9],[1,1]]]}},
        {"type":"Feature","properties":{},"geometry":{"type":"Polygon","coordinates":[[[4,1],[8,1],[8,9],[4,9],[4,1]]]}}
    ]}"#;
    let sh = shaper();
    let fill_fts = FeatureTypeStyle {
        rules: vec![Rule {
            filter: None,
            else_filter: false,
            min_scale: None,
            max_scale: None,
            symbolizers: vec![Symbolizer::Polygon(PolygonSym {
                fill: [0, 180, 0, 255],
                stroke: None,
                stroke_width: 0.0,
            })],
            title: None,
        }],
    };
    let boundary_fts = FeatureTypeStyle {
        rules: vec![Rule {
            filter: None,
            else_filter: false,
            min_scale: None,
            max_scale: None,
            symbolizers: vec![Symbolizer::Polygon(PolygonSym {
                fill: [0, 0, 0, 0],
                stroke: Some([0, 0, 255, 255]),
                stroke_width: 4.0,
            })],
            title: None,
        }],
    };
    let style = Style {
        feature_type_styles: vec![fill_fts, boundary_fts],
    };
    let src = GeoJsonSource::from_str(geojson).unwrap();
    // bbox [0,0,10,10] over 500px -> x=5 maps to px 250; A's right edge runs the full height there.
    let rgba = render_vector(
        &src,
        &style,
        "EPSG:3857",
        "EPSG:3857",
        [0.0, 0.0, 10.0, 10.0],
        500,
        500,
        &sh,
    )
    .unwrap();
    let blue = blue_pixels_in_rect(&rgba, 500, 245, 256, 150, 350);
    assert!(
        blue > 30,
        "A's inner boundary (2nd FTS) composites ON TOP of B's fill (per-FTS z-order); got {blue} blue px"
    );
}

// -------------------------------------------------------------------------------------------
// Task 5 (T7) extended to geometry: a rule draws ALL Line/Polygon symbolizers, not just the first.
// -------------------------------------------------------------------------------------------

/// Road casing: one rule with TWO LineSymbolizers — a wide dark stroke then a narrow light one on
/// top. Both must draw (before the T7 geometry extension only the first did), producing a dark
/// casing with a light center: both colors present.
#[test]
fn road_casing_draws_both_line_symbolizers() {
    let geojson = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","properties":{},"geometry":{"type":"LineString","coordinates":[[1,5],[9,5]]}}
    ]}"#;
    let sh = shaper();
    let style = Style {
        feature_type_styles: vec![FeatureTypeStyle {
            rules: vec![Rule {
                filter: None,
                else_filter: false,
                min_scale: None,
                max_scale: None,
                symbolizers: vec![
                    Symbolizer::Line(LineSym {
                        stroke: [20, 20, 20, 255],
                        stroke_width: 9.0,
                    }),
                    Symbolizer::Line(LineSym {
                        stroke: [240, 240, 0, 255],
                        stroke_width: 3.0,
                    }),
                ],
                title: None,
            }],
        }],
    };
    let src = GeoJsonSource::from_str(geojson).unwrap();
    let rgba = render_vector(
        &src,
        &style,
        "EPSG:3857",
        "EPSG:3857",
        [0.0, 0.0, 10.0, 10.0],
        500,
        500,
        &sh,
    )
    .unwrap();
    let dark = rgba
        .chunks(4)
        .any(|p| p[0] < 60 && p[1] < 60 && p[2] < 60 && p[3] > 128);
    let yellow = rgba
        .chunks(4)
        .any(|p| p[0] > 180 && p[1] > 180 && p[2] < 80 && p[3] > 128);
    assert!(dark, "wide dark casing (1st LineSymbolizer) drawn");
    assert!(
        yellow,
        "narrow light center (2nd LineSymbolizer) ALSO drawn — T7 draws all Line symbolizers, not just the first"
    );
}

/// Blue-pixel count within a pixel rect [x0,x1) x [y0,y1) of a row-major RGBA8 buffer `w` px wide.
fn blue_pixels_in_rect(rgba: &[u8], w: usize, x0: usize, x1: usize, y0: usize, y1: usize) -> usize {
    let mut n = 0;
    for y in y0..y1 {
        for x in x0..x1 {
            let p = &rgba[(y * w + x) * 4..][..4];
            if p[2] > 180 && p[0] < 80 && p[1] < 80 && p[3] > 128 {
                n += 1;
            }
        }
    }
    n
}
