//! Task 2 focused test: `sld::parse` — roxmltree XML → `sld::model` AST.
//!
//! Fixture `fixtures/sld/point_min.sld`: one `NamedLayer` ("airports") with one `UserStyle` /
//! `FeatureTypeStyle` / `Rule` ("major", `MaxScaleDenominator` 20_000_000) carrying a
//! `PointSymbolizer` (circle mark, fill #1e1e1e, size 6) and a `TextSymbolizer` (label
//! `ogc:PropertyName` "name", font-size 16, halo radius 2 / fill #ffffff, fill #141414).

use terraserve::sld::{Expression, LabelPart, LabelPlacement, Symbolizer};

fn load_fixture() -> terraserve::sld::StyledLayerDescriptor {
    let xml = std::fs::read_to_string("tests/fixtures/sld/point_min.sld")
        .expect("fixture tests/fixtures/sld/point_min.sld should exist");
    terraserve::sld::parse(&xml).expect("fixture should parse")
}

#[test]
fn parses_named_layer_and_rule_scale() {
    let sld = load_fixture();

    assert_eq!(sld.named_layers.len(), 1);
    let layer = &sld.named_layers[0];
    assert_eq!(layer.name.as_deref(), Some("airports"));
    assert_eq!(layer.styles.len(), 1);

    let style = &layer.styles[0];
    assert_eq!(style.feature_type_styles.len(), 1);
    let fts = &style.feature_type_styles[0];
    assert_eq!(fts.rules.len(), 1);

    let rule = &fts.rules[0];
    assert_eq!(rule.name.as_deref(), Some("major"));
    assert_eq!(rule.min_scale, None);
    assert_eq!(rule.max_scale, Some(20_000_000.0));
    assert!(
        rule.filter.is_none(),
        "fixture has no <Filter> element on this Rule"
    );
    assert!(!rule.else_filter);
    assert_eq!(rule.symbolizers.len(), 2);
}

#[test]
fn parses_point_symbolizer_mark_fill_and_size() {
    let sld = load_fixture();
    let rule = &sld.named_layers[0].styles[0].feature_type_styles[0].rules[0];

    match &rule.symbolizers[0] {
        Symbolizer::Point(point) => {
            assert_eq!(point.graphic.marks.len(), 1);
            let mark = &point.graphic.marks[0];
            assert_eq!(mark.well_known_name, "circle");
            let fill = mark.fill.as_ref().expect("mark should have a fill");
            assert_eq!(fill.color.as_deref(), Some("#1e1e1e"));

            assert_eq!(
                point.graphic.size,
                Some(Expression::Literal("6".to_string()))
            );
        }
        other => panic!("expected Symbolizer::Point, got {other:?}"),
    }
}

#[test]
fn parses_text_symbolizer_label_font_halo_and_fill() {
    let sld = load_fixture();
    let rule = &sld.named_layers[0].styles[0].feature_type_styles[0].rules[0];

    match &rule.symbolizers[1] {
        Symbolizer::Text(text) => {
            assert_eq!(
                text.label,
                vec![LabelPart::PropertyName("name".to_string())]
            );

            assert_eq!(text.font.size, Some(Expression::Literal("16".to_string())));

            let halo = text.halo.as_ref().expect("should have a halo");
            assert_eq!(halo.radius, Some(Expression::Literal("2".to_string())));
            let halo_fill = halo.fill.as_ref().expect("halo should have a fill");
            assert_eq!(halo_fill.color.as_deref(), Some("#ffffff"));

            let fill = text.fill.as_ref().expect("text should have a fill");
            assert_eq!(fill.color.as_deref(), Some("#141414"));

            // No <LabelPlacement> in the fixture — parser supplies a default Point placement.
            match &text.placement {
                LabelPlacement::Point(_) => {}
                LabelPlacement::Line(_) => panic!("expected default point placement"),
            }
        }
        other => panic!("expected Symbolizer::Text, got {other:?}"),
    }
}

#[test]
fn label_literal_only() {
    let sld = terraserve::sld::parse(
        r#"<StyledLayerDescriptor><NamedLayer><UserStyle><FeatureTypeStyle><Rule>
        <TextSymbolizer><Label>Airport</Label></TextSymbolizer>
        </Rule></FeatureTypeStyle></UserStyle></NamedLayer></StyledLayerDescriptor>"#,
    )
    .unwrap();
    let rule = &sld.named_layers[0].styles[0].feature_type_styles[0].rules[0];
    if let terraserve::sld::model::Symbolizer::Text(t) = &rule.symbolizers[0] {
        assert_eq!(
            t.label,
            vec![terraserve::sld::model::LabelPart::Literal("Airport".into())]
        );
    } else {
        panic!("expected Text symbolizer");
    }
}

#[test]
fn label_mixed_content_keeps_intentional_text_drops_formatting() {
    use terraserve::sld::model::LabelPart::{Literal, PropertyName};
    // Intentional " (" and ")" kept; the pretty-print newline/indent before <ogc:PropertyName>
    // is whitespace-only -> dropped.
    let sld = terraserve::sld::parse(
        "<StyledLayerDescriptor><NamedLayer><UserStyle><FeatureTypeStyle><Rule>\
        <TextSymbolizer><Label><ogc:PropertyName xmlns:ogc=\"http://www.opengis.net/ogc\">name</ogc:PropertyName> (<ogc:PropertyName xmlns:ogc=\"http://www.opengis.net/ogc\">pop</ogc:PropertyName>)</Label></TextSymbolizer>\
        </Rule></FeatureTypeStyle></UserStyle></NamedLayer></StyledLayerDescriptor>",
    )
    .unwrap();
    let rule = &sld.named_layers[0].styles[0].feature_type_styles[0].rules[0];
    if let terraserve::sld::model::Symbolizer::Text(t) = &rule.symbolizers[0] {
        assert_eq!(
            t.label,
            vec![
                PropertyName("name".into()),
                Literal(" (".into()),
                PropertyName("pop".into()),
                Literal(")".into())
            ]
        );
    } else {
        panic!("expected Text symbolizer");
    }
}

#[test]
fn label_ogc_literal_whitespace_kept() {
    use terraserve::sld::model::LabelPart::{Literal, PropertyName};
    let sld = terraserve::sld::parse(
        "<StyledLayerDescriptor xmlns:ogc=\"http://www.opengis.net/ogc\"><NamedLayer><UserStyle><FeatureTypeStyle><Rule>\
        <TextSymbolizer><Label><ogc:PropertyName>a</ogc:PropertyName><ogc:Literal> </ogc:Literal><ogc:PropertyName>b</ogc:PropertyName></Label></TextSymbolizer>\
        </Rule></FeatureTypeStyle></UserStyle></NamedLayer></StyledLayerDescriptor>",
    )
    .unwrap();
    let rule = &sld.named_layers[0].styles[0].feature_type_styles[0].rules[0];
    if let terraserve::sld::model::Symbolizer::Text(t) = &rule.symbolizers[0] {
        assert_eq!(
            t.label,
            vec![
                PropertyName("a".into()),
                Literal(" ".into()),
                PropertyName("b".into())
            ]
        );
    } else {
        panic!("expected Text symbolizer");
    }
}

#[test]
fn rejects_a_styled_layer_descriptor_with_no_named_layers() {
    // Pragmatic stopgap (NOT an XSD rule — SLD 1.0 allows zero layers): we reject a layer-less
    // doc because UserLayer isn't modeled yet and there's nothing to style. Revisit with UserLayer.
    let result = terraserve::sld::parse("<StyledLayerDescriptor/>");
    assert!(result.is_err());
}

#[test]
fn rejects_malformed_xml() {
    let result = terraserve::sld::parse("<StyledLayerDescriptor><Unclosed>");
    assert!(result.is_err());
}
