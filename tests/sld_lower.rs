//! Task 6 focused test: `vector::sld_lower::lower` — SLD document AST → renderer Style IR.
//!
//! This is the interop firewall test: every SLD-ism (hex `CssParameter`/`SvgParameter` colors,
//! `<ogc:Expression>` literals, scale denominators, `WellKnownName` markers, `<ogc:Filter>`
//! trees) must resolve to plain IR values with no SLD types leaking through.

use terraserve::vector::sld_lower::lower;
use terraserve::vector::style::{Cmp, Filter, Symbolizer};

const FIXTURE_DIR: &str = "tests/fixtures/sld";

fn read_fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{FIXTURE_DIR}/{name}"))
        .unwrap_or_else(|e| panic!("{name}: should be readable: {e}"))
}

/// The minimal hand-authored fixture (`point_min.sld`, from Task 2): one rule, a `MaxScaleDenominator`,
/// a `PointSymbolizer` (circle mark, fill `#1e1e1e`, size 6) and a `TextSymbolizer` (label bound
/// to the `name` property, font-size 16, halo radius 2 + white fill, text fill `#141414`).
#[test]
fn lowers_point_min_fixture_to_expected_style_ir() {
    let sld = terraserve::sld::parse(&read_fixture("point_min.sld")).expect("fixture should parse");
    let style = lower(sld).expect("fixture should lower");

    assert_eq!(style.feature_type_styles[0].rules.len(), 1);
    let rule = &style.feature_type_styles[0].rules[0];
    assert!(rule.filter.is_none());
    assert!(rule.min_scale.is_none());
    assert_eq!(rule.max_scale, Some(20e6));
    assert_eq!(rule.symbolizers.len(), 2);

    let mut saw_point = false;
    let mut saw_text = false;
    for sym in &rule.symbolizers {
        match sym {
            Symbolizer::Point(p) => {
                saw_point = true;
                assert_eq!(p.fill, [30, 30, 30, 255]);
                assert_eq!(p.radius, 3.0, "radius should be Size(6)/2");
            }
            Symbolizer::Text(t) => {
                saw_text = true;
                assert_eq!(
                    t.label,
                    vec![terraserve::vector::style::LabelPart::Field(
                        "name".to_string()
                    )]
                );
                assert_eq!(t.size, 16.0);
                assert_eq!(t.halo_radius, 2.0);
                assert_eq!(t.color, [20, 20, 20, 255]);
            }
            other => panic!("point_min.sld should lower to Point/Text only, got {other:?}"),
        }
    }
    assert!(saw_point && saw_text);
}

/// A richer SE 1.1 conformance fixture (`se11_point_scale_filter.sld`, from Task 4): `se:`-prefixed
/// elements, min+max scale denominators, and a deeply nested `<ogc:Filter>` (an 8-way `And` mixing
/// comparisons, `IsNull`, `Like`, `Not`, a nested `Or`, and `Between`). Exercises the
/// `SldFilter -> Filter` structural mapping end-to-end, plus `se:SvgParameter`-based fill/stroke.
#[test]
fn lowers_se11_scale_filter_fixture_with_nested_filter_and_stroke() {
    let sld = terraserve::sld::parse(&read_fixture("se11_point_scale_filter.sld"))
        .expect("fixture should parse");
    let style = lower(sld).expect("fixture should lower");

    assert_eq!(style.feature_type_styles[0].rules.len(), 1);
    let rule = &style.feature_type_styles[0].rules[0];
    assert_eq!(rule.min_scale, Some(10_000.0));
    assert_eq!(rule.max_scale, Some(20_000.0));
    assert_eq!(rule.symbolizers.len(), 1);

    match &rule.symbolizers[0] {
        Symbolizer::Point(p) => {
            assert_eq!(p.fill, [255, 0, 0, 255], "se:SvgParameter fill #FF0000");
            assert_eq!(p.stroke, [0, 0, 0, 255], "se:SvgParameter stroke #000000");
            assert_eq!(p.stroke_width, 2.0);
            assert_eq!(p.radius, 3.0, "radius should be Size(6)/2");
        }
        other => panic!("expected Symbolizer::Point, got {other:?}"),
    }

    // Structural check on the lowered filter: top-level And of 8 clauses (Eq, Eq, IsNull, Like,
    // Like, Not(Gt), Or, Between), first clause a Cmp::Eq on NAME/"New York" — proves the
    // SldFilter tree walked all the way through, not just parsed.
    match rule.filter.as_ref().expect("rule should carry a filter") {
        Filter::And(items) => {
            assert_eq!(items.len(), 8);
            match &items[0] {
                Filter::Cmp(Cmp::Eq, prop, value) => {
                    assert_eq!(prop, "NAME");
                    assert_eq!(value, "New York");
                }
                other => panic!("expected Filter::Cmp(Eq, NAME, ...), got {other:?}"),
            }
            match &items[2] {
                Filter::IsNull(prop) => assert_eq!(prop, "TEST"),
                other => panic!("expected Filter::IsNull(TEST), got {other:?}"),
            }
            match &items[5] {
                Filter::Not(inner) => match inner.as_ref() {
                    Filter::Cmp(Cmp::Gt, prop, value) => {
                        assert_eq!(prop, "POPULATION");
                        assert_eq!(value, "100000");
                    }
                    other => panic!("expected Filter::Cmp(Gt, POPULATION, ...), got {other:?}"),
                },
                other => panic!("expected Filter::Not(...), got {other:?}"),
            }
            match &items[6] {
                Filter::Or(ors) => assert_eq!(ors.len(), 2),
                other => panic!("expected Filter::Or(...), got {other:?}"),
            }
            match &items[7] {
                Filter::Between(prop, lo, hi) => {
                    assert_eq!(prop, "TEST3");
                    assert_eq!(lo, "1");
                    assert_eq!(hi, "5");
                }
                other => panic!("expected Filter::Between(TEST3, 1, 5), got {other:?}"),
            }
        }
        other => panic!("expected top-level Filter::And, got {other:?}"),
    }
}

/// `<ElseFilter/>` lowers to `Rule.else_filter = true`; a normal (filtered) rule stays `false`.
/// The fallback rule is what `render.rs` uses only for features no normal rule matched (Task 6
/// compliance P0). Parsed from a two-rule SLD: a `PropertyIsLessThan` rule + an `ElseFilter` rule.
#[test]
fn lowers_else_filter_rule_with_else_filter_true() {
    const DOC: &str = r#"<StyledLayerDescriptor version="1.0.0"
        xmlns="http://www.opengis.net/sld" xmlns:ogc="http://www.opengis.net/ogc">
      <NamedLayer><Name>airports</Name><UserStyle><FeatureTypeStyle>
        <Rule><Name>major</Name>
          <ogc:Filter><ogc:PropertyIsLessThan>
            <ogc:PropertyName>scalerank</ogc:PropertyName><ogc:Literal>3</ogc:Literal>
          </ogc:PropertyIsLessThan></ogc:Filter>
          <PointSymbolizer><Graphic><Mark><WellKnownName>circle</WellKnownName></Mark></Graphic></PointSymbolizer>
        </Rule>
        <Rule><Name>rest</Name>
          <ElseFilter/>
          <PointSymbolizer><Graphic><Mark><WellKnownName>circle</WellKnownName></Mark></Graphic></PointSymbolizer>
        </Rule>
      </FeatureTypeStyle></UserStyle></NamedLayer>
    </StyledLayerDescriptor>"#;
    let sld = terraserve::sld::parse(DOC).expect("document should parse");
    let style = lower(sld).expect("document should lower");
    assert_eq!(style.feature_type_styles[0].rules.len(), 2);
    assert!(
        !style.feature_type_styles[0].rules[0].else_filter,
        "the filtered rule is not an else-rule"
    );
    assert!(style.feature_type_styles[0].rules[0].filter.is_some());
    assert!(
        style.feature_type_styles[0].rules[1].else_filter,
        "the <ElseFilter/> rule lowers with else_filter = true"
    );
    assert!(
        style.feature_type_styles[0].rules[1].filter.is_none(),
        "an else-rule carries no comparison filter"
    );
}

/// An SLD document with a `NamedLayer` but no `UserStyle`s (and therefore no rules to lower)
/// errors rather than silently producing an empty `Style`.
#[test]
fn lower_errors_on_zero_rules() {
    let sld = terraserve::sld::StyledLayerDescriptor {
        named_layers: vec![],
    };
    assert!(lower(sld).is_err());
}

/// Build a minimal one-rule, one-`PointSymbolizer` SLD document with the given `fill`/`stroke`
/// `CssParameter` values spliced in verbatim (each `Some` becomes a `<CssParameter name="...">`
/// element; `None` omits it). Shared by the CORR-3 / TG3 / TG8 / Fix-C tests below, which only
/// differ in which of these knobs they set.
fn point_sld(
    size: Option<&str>,
    fill: Option<&str>,
    fill_opacity: Option<&str>,
    stroke: Option<&str>,
    stroke_opacity: Option<&str>,
) -> String {
    let css = |name: &str, v: &Option<&str>| -> String {
        v.map(|v| format!(r#"<CssParameter name="{name}">{v}</CssParameter>"#))
            .unwrap_or_default()
    };
    format!(
        r#"<StyledLayerDescriptor version="1.0.0"
            xmlns="http://www.opengis.net/sld" xmlns:ogc="http://www.opengis.net/ogc">
          <NamedLayer><Name>t</Name><UserStyle><FeatureTypeStyle>
            <Rule><PointSymbolizer><Graphic><Mark><WellKnownName>circle</WellKnownName>
              <Fill>{fill}{fill_op}</Fill>
              <Stroke>{stroke}{stroke_op}</Stroke>
            </Mark>{size}</Graphic></PointSymbolizer></Rule>
          </FeatureTypeStyle></UserStyle></NamedLayer>
        </StyledLayerDescriptor>"#,
        fill = css("fill", &fill),
        fill_op = css("fill-opacity", &fill_opacity),
        stroke = css("stroke", &stroke),
        stroke_op = css("stroke-opacity", &stroke_opacity),
        size = size
            .map(|s| format!("<Size>{s}</Size>"))
            .unwrap_or_default(),
    )
}

fn lower_point_fill_stroke(doc: &str) -> ([u8; 4], [u8; 4]) {
    let sld = terraserve::sld::parse(doc).expect("document should parse");
    let style = lower(sld).expect("document should lower");
    match &style.feature_type_styles[0].rules[0].symbolizers[0] {
        Symbolizer::Point(p) => (p.fill, p.stroke),
        other => panic!("expected Symbolizer::Point, got {other:?}"),
    }
}

/// TG3: `fill-opacity`/`stroke-opacity` resolve to alpha (0.5 → 128, 0.7 → 179 — both round-half-
/// away-from-zero of `opacity * 255`).
#[test]
fn opacity_resolves_to_alpha() {
    let doc = point_sld(
        None,
        Some("#ff8800"),
        Some("0.5"),
        Some("#000000"),
        Some("0.7"),
    );
    let (fill, stroke) = lower_point_fill_stroke(&doc);
    assert_eq!(fill[3], 128, "0.5 * 255 rounds to 128");
    assert_eq!(stroke[3], 179, "0.7 * 255 rounds to 179");
}

/// TG3 clamp cases: an out-of-[0,1] opacity clamps rather than over/underflowing — `1.5` → full
/// alpha (255), `-0.2` → zero alpha (0).
#[test]
fn opacity_out_of_range_clamps_to_0_1() {
    let over = point_sld(None, Some("#ff8800"), Some("1.5"), None, None);
    let (fill, _) = lower_point_fill_stroke(&over);
    assert_eq!(fill[3], 255, "opacity 1.5 clamps to 1.0 -> alpha 255");

    let under = point_sld(None, Some("#ff8800"), Some("-0.2"), None, None);
    let (fill, _) = lower_point_fill_stroke(&under);
    assert_eq!(fill[3], 0, "opacity -0.2 clamps to 0.0 -> alpha 0");
}

/// CORR-3: an 8-digit `#rrggbbaa` fill carries its own alpha through to the IR instead of being
/// rejected by `parse_hex_rgb` and silently falling back to the default gray (the pre-fix bug —
/// total color loss for any producer that emits an alpha-bearing hex).
#[test]
fn corr3_eight_digit_hex_alpha_lowers_correctly() {
    let doc = point_sld(None, Some("#ff8800cc"), None, None, None);
    let (fill, _) = lower_point_fill_stroke(&doc);
    assert_eq!(
        fill,
        [255, 136, 0, 204],
        "#ff8800cc -> rgba, alpha 0xcc=204"
    );
}

/// CORR-3 control: a plain 6-digit hex still lowers to full alpha (255) — the fix must not change
/// behavior for the common no-alpha case.
#[test]
fn corr3_six_digit_hex_has_full_alpha() {
    let doc = point_sld(None, Some("#ff8800"), None, None, None);
    let (fill, _) = lower_point_fill_stroke(&doc);
    assert_eq!(fill, [255, 136, 0, 255]);
}

/// CORR-3 + TG3 combined: an 8-digit hex-alpha AND a separate `fill-opacity` are both present ->
/// multiplied (the precedence this fix picked: hex-alpha x opacity, both being independent [0,1]
/// fractions). `0xcc/255 = 0.8`; `0.8 * 0.5 = 0.4`; `round(0.4 * 255) = 102`.
#[test]
fn corr3_hex_alpha_and_opacity_multiply() {
    let doc = point_sld(None, Some("#ff8800cc"), Some("0.5"), None, None);
    let (fill, _) = lower_point_fill_stroke(&doc);
    assert_eq!(
        fill,
        [255, 136, 0, 102],
        "hex-alpha (0.8) * opacity (0.5) -> 0.4 -> 102"
    );
}

/// TG8: 3-digit hex shorthand expands each nibble (`#abc` -> `#aabbcc`), full alpha.
#[test]
fn tg8_three_digit_hex_expands_nibbles() {
    let doc = point_sld(None, Some("#abc"), None, None, None);
    let (fill, _) = lower_point_fill_stroke(&doc);
    assert_eq!(fill, [170, 187, 204, 255]);
}

/// Fix C: a negative `<Size>` (here `-40`, giving a raw radius of `-20`) clamps to `0.0` rather
/// than flowing a negative radius into the draw kernel.
#[test]
fn fix_c_negative_size_clamps_radius_to_zero() {
    let doc = point_sld(Some("-40"), None, None, None, None);
    let sld = terraserve::sld::parse(&doc).expect("document should parse");
    let style = lower(sld).expect("document should lower");
    match &style.feature_type_styles[0].rules[0].symbolizers[0] {
        Symbolizer::Point(p) => assert_eq!(p.radius, 0.0, "Size(-40)/2 = -20, clamped to 0.0"),
        other => panic!("expected Symbolizer::Point, got {other:?}"),
    }
}

// -------------------------------------------------------------------------------------------
// Task 5: Polygon/Line symbolizer lowering (previously dropped to `None` by `lower_symbolizer`).
// -------------------------------------------------------------------------------------------

/// A one-rule SLD document with a single `<PolygonSymbolizer>`: `<Fill>` `#c8b48c`, `<Stroke>`
/// `#503c28` width `1`.
const POLYGON_SLD: &str = r#"<StyledLayerDescriptor version="1.0.0"
    xmlns="http://www.opengis.net/sld" xmlns:ogc="http://www.opengis.net/ogc">
  <NamedLayer><Name>t</Name><UserStyle><FeatureTypeStyle>
    <Rule><PolygonSymbolizer>
      <Fill><CssParameter name="fill">#c8b48c</CssParameter></Fill>
      <Stroke>
        <CssParameter name="stroke">#503c28</CssParameter>
        <CssParameter name="stroke-width">1</CssParameter>
      </Stroke>
    </PolygonSymbolizer></Rule>
  </FeatureTypeStyle></UserStyle></NamedLayer>
</StyledLayerDescriptor>"#;

/// Same as `POLYGON_SLD` but with no `<Stroke>` at all — an unstroked fill.
const POLYGON_SLD_NO_STROKE: &str = r#"<StyledLayerDescriptor version="1.0.0"
    xmlns="http://www.opengis.net/sld" xmlns:ogc="http://www.opengis.net/ogc">
  <NamedLayer><Name>t</Name><UserStyle><FeatureTypeStyle>
    <Rule><PolygonSymbolizer>
      <Fill><CssParameter name="fill">#c8b48c</CssParameter></Fill>
    </PolygonSymbolizer></Rule>
  </FeatureTypeStyle></UserStyle></NamedLayer>
</StyledLayerDescriptor>"#;

/// A one-rule SLD document with a single `<LineSymbolizer>`: `<Stroke>` `#787878` width `2`.
const LINE_SLD: &str = r#"<StyledLayerDescriptor version="1.0.0"
    xmlns="http://www.opengis.net/sld" xmlns:ogc="http://www.opengis.net/ogc">
  <NamedLayer><Name>t</Name><UserStyle><FeatureTypeStyle>
    <Rule><LineSymbolizer>
      <Stroke>
        <CssParameter name="stroke">#787878</CssParameter>
        <CssParameter name="stroke-width">2</CssParameter>
      </Stroke>
    </LineSymbolizer></Rule>
  </FeatureTypeStyle></UserStyle></NamedLayer>
</StyledLayerDescriptor>"#;

/// `<PolygonSymbolizer>` with both `<Fill>` and `<Stroke>` lowers to `Symbolizer::Polygon` with
/// the resolved fill/stroke RGBA + stroke width — not dropped to `None` (the pre-Task-5 behavior).
#[test]
fn lowers_polygon_symbolizer_with_fill_and_stroke() {
    let sld = terraserve::sld::parse(POLYGON_SLD).expect("document should parse");
    let style = lower(sld).expect("document should lower");
    assert_eq!(style.feature_type_styles[0].rules.len(), 1);
    assert_eq!(
        style.feature_type_styles[0].rules[0].symbolizers.len(),
        1,
        "must not be dropped"
    );
    match &style.feature_type_styles[0].rules[0].symbolizers[0] {
        Symbolizer::Polygon(poly) => {
            assert_eq!(poly.fill, [200, 180, 140, 255], "#c8b48c");
            assert_eq!(poly.stroke, Some([80, 60, 40, 255]), "#503c28");
            assert_eq!(poly.stroke_width, 1.0);
        }
        other => panic!("expected Symbolizer::Polygon, got {other:?}"),
    }
}

/// `<PolygonSymbolizer>` with a `<Fill>` but no `<Stroke>` element at all lowers `stroke` to
/// `None` (an unstroked fill) rather than making up a stroke color.
#[test]
fn lowers_polygon_symbolizer_with_no_stroke_to_none() {
    let sld = terraserve::sld::parse(POLYGON_SLD_NO_STROKE).expect("document should parse");
    let style = lower(sld).expect("document should lower");
    match &style.feature_type_styles[0].rules[0].symbolizers[0] {
        Symbolizer::Polygon(poly) => {
            assert_eq!(poly.fill, [200, 180, 140, 255], "#c8b48c");
            assert_eq!(poly.stroke, None, "no <Stroke> element -> no outline");
        }
        other => panic!("expected Symbolizer::Polygon, got {other:?}"),
    }
}

/// `<LineSymbolizer>` with a `<Stroke>` lowers to `Symbolizer::Line` with the resolved stroke
/// RGBA + width — not dropped to `None`.
#[test]
fn lowers_line_symbolizer_with_stroke() {
    let sld = terraserve::sld::parse(LINE_SLD).expect("document should parse");
    let style = lower(sld).expect("document should lower");
    assert_eq!(style.feature_type_styles[0].rules.len(), 1);
    assert_eq!(
        style.feature_type_styles[0].rules[0].symbolizers.len(),
        1,
        "must not be dropped"
    );
    match &style.feature_type_styles[0].rules[0].symbolizers[0] {
        Symbolizer::Line(line) => {
            assert_eq!(line.stroke, [120, 120, 120, 255], "#787878");
            assert_eq!(line.stroke_width, 2.0);
        }
        other => panic!("expected Symbolizer::Line, got {other:?}"),
    }
}

// -------------------------------------------------------------------------------------------
// Task 2: mixed-content <Label> lowering.
// -------------------------------------------------------------------------------------------

/// A pure-literal `<Label>Airport</Label>` lowers to a single `LabelPart::Literal` part — the
/// bug Task 2 fixes (the Task 1 interim mapped every label to `Field`, so a literal label
/// rendered blank).
#[test]
fn literal_label_lowers_to_literal_part() {
    // A literal <Label> (no <ogc:PropertyName>) lowers to a Literal part → renders "Airport";
    // before Task 2 it was treated as a field name and rendered blank.
    let doc = terraserve::sld::parse(
        r#"<StyledLayerDescriptor><NamedLayer><UserStyle><FeatureTypeStyle><Rule>
        <TextSymbolizer><Label>Airport</Label></TextSymbolizer>
        </Rule></FeatureTypeStyle></UserStyle></NamedLayer></StyledLayerDescriptor>"#,
    )
    .unwrap();
    let lowered = terraserve::vector::sld_lower::lower(doc).unwrap();
    match &lowered.feature_type_styles[0].rules[0].symbolizers[0] {
        terraserve::vector::style::Symbolizer::Text(t) => assert_eq!(
            t.label,
            vec![terraserve::vector::style::LabelPart::Literal(
                "Airport".to_string()
            )]
        ),
        _ => panic!("expected Text"),
    }
}

// -------------------------------------------------------------------------------------------
// Task 3: literal-vs-field <Priority> lowering.
// -------------------------------------------------------------------------------------------

fn lower_priority(inner: &str) -> Option<terraserve::vector::style::Priority> {
    let doc = terraserve::sld::parse(&format!(
        "<StyledLayerDescriptor xmlns:ogc=\"http://www.opengis.net/ogc\"><NamedLayer><UserStyle><FeatureTypeStyle><Rule>\
        <TextSymbolizer><Label><ogc:PropertyName>name</ogc:PropertyName></Label>{inner}</TextSymbolizer>\
        </Rule></FeatureTypeStyle></UserStyle></NamedLayer></StyledLayerDescriptor>"
    )).unwrap();
    let s = terraserve::vector::sld_lower::lower(doc).unwrap();
    match &s.feature_type_styles[0].rules[0].symbolizers[0] {
        terraserve::vector::style::Symbolizer::Text(t) => {
            assert!(t.priority_higher_wins, "SLD priority is higher-wins");
            t.priority.clone()
        }
        _ => panic!("expected Text"),
    }
}

#[test]
fn priority_literal_is_numeric() {
    assert_eq!(
        lower_priority("<Priority>1000</Priority>"),
        Some(terraserve::vector::style::Priority::Literal(1000.0))
    );
}
#[test]
fn priority_property_is_field() {
    assert_eq!(
        lower_priority("<Priority><ogc:PropertyName>pop_max</ogc:PropertyName></Priority>"),
        Some(terraserve::vector::style::Priority::Field(
            "pop_max".to_string()
        ))
    );
}
#[test]
fn priority_bare_text_nonnumeric_stays_field() {
    // <Priority>scalerank</Priority> (no PropertyName wrapper) must keep working as a field.
    assert_eq!(
        lower_priority("<Priority>scalerank</Priority>"),
        Some(terraserve::vector::style::Priority::Field(
            "scalerank".to_string()
        ))
    );
}

#[test]
fn data_driven_size_lowers_to_default_not_panic() {
    // A data-driven <Size><ogc:PropertyName>mag</ogc:PropertyName></Size> is unsupported by the
    // MVP Style IR — lowering must succeed (warn + fall back to the default radius), not panic.
    let doc = terraserve::sld::parse(
        "<StyledLayerDescriptor xmlns:ogc=\"http://www.opengis.net/ogc\"><NamedLayer><UserStyle><FeatureTypeStyle><Rule>\
        <PointSymbolizer><Graphic><Mark><WellKnownName>circle</WellKnownName></Mark>\
        <Size><ogc:PropertyName>mag</ogc:PropertyName></Size></Graphic></PointSymbolizer>\
        </Rule></FeatureTypeStyle></UserStyle></NamedLayer></StyledLayerDescriptor>",
    )
    .unwrap();
    let s = lower(doc).unwrap();
    match &s.feature_type_styles[0].rules[0].symbolizers[0] {
        Symbolizer::Point(p) => assert_eq!(p.radius, 3.0),
        other => panic!("expected Point, got {other:?}"),
    }
}

// -------------------------------------------------------------------------------------------
// Task 6: <PointPlacement><Displacement> → the label offset (magnitude); 0/absent → default 4.0.
// -------------------------------------------------------------------------------------------

fn lower_offset(placement: &str) -> f32 {
    let doc = terraserve::sld::parse(&format!(
        "<StyledLayerDescriptor xmlns:ogc=\"http://www.opengis.net/ogc\"><NamedLayer><UserStyle><FeatureTypeStyle><Rule>\
        <TextSymbolizer><Label>x</Label>{placement}</TextSymbolizer>\
        </Rule></FeatureTypeStyle></UserStyle></NamedLayer></StyledLayerDescriptor>"
    ))
    .unwrap();
    let s = lower(doc).unwrap();
    match &s.feature_type_styles[0].rules[0].symbolizers[0] {
        Symbolizer::Text(t) => t.offset,
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn displacement_sets_offset_to_magnitude() {
    let o = lower_offset(
        "<LabelPlacement><PointPlacement><Displacement>\
         <DisplacementX>3</DisplacementX><DisplacementY>4</DisplacementY>\
         </Displacement></PointPlacement></LabelPlacement>",
    );
    assert!((o - 5.0).abs() < 1e-3, "sqrt(3^2+4^2)=5, got {o}");
}

#[test]
fn zero_displacement_keeps_default_offset() {
    let o = lower_offset(
        "<LabelPlacement><PointPlacement><Displacement>\
         <DisplacementX>0</DisplacementX><DisplacementY>0</DisplacementY>\
         </Displacement></PointPlacement></LabelPlacement>",
    );
    assert_eq!(
        o, 4.0,
        "zero displacement (boilerplate) keeps the default gap"
    );
}

#[test]
fn absent_displacement_keeps_default_offset() {
    assert_eq!(lower_offset(""), 4.0);
}
