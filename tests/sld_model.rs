//! Task 1 focused test: the spec-faithful SLD document AST (`terraserve::sld`) is constructible
//! and its fields are reachable. No parsing yet (Task 2) — this only exercises the model types.

use terraserve::sld::{
    Expression, FeatureTypeStyle, Font, LabelPart, LabelPlacement, NamedLayer, PointPlacement,
    Rule, StyledLayerDescriptor, Symbolizer, TextSymbolizer, UserStyle,
};

#[test]
fn builds_minimal_sld_with_one_rule_and_text_symbolizer() {
    let sld = StyledLayerDescriptor {
        named_layers: vec![NamedLayer {
            name: Some("airports".to_string()),
            styles: vec![UserStyle {
                name: Some("default".to_string()),
                feature_type_styles: vec![FeatureTypeStyle {
                    rules: vec![Rule {
                        name: Some("major".to_string()),
                        title: None,
                        filter: None,
                        else_filter: false,
                        min_scale: None,
                        max_scale: Some(20_000_000.0),
                        symbolizers: vec![Symbolizer::Text(TextSymbolizer {
                            label: vec![LabelPart::PropertyName("name".into())],
                            font: Font {
                                family: Some("DejaVu Sans".to_string()),
                                size: Some(Expression::Literal("16".to_string())),
                                weight: None,
                                style: None,
                            },
                            placement: LabelPlacement::Point(PointPlacement {
                                anchor: None,
                                displacement: None,
                                rotation: None,
                            }),
                            halo: None,
                            fill: None,
                            priority: None,
                            vendor: Vec::new(),
                        })],
                    }],
                }],
            }],
        }],
    };

    // Walk the whole tree back down and assert field access at each level.
    assert_eq!(sld.named_layers.len(), 1);
    let layer = &sld.named_layers[0];
    assert_eq!(layer.name.as_deref(), Some("airports"));
    assert_eq!(layer.styles.len(), 1);

    let style = &layer.styles[0];
    assert_eq!(style.name.as_deref(), Some("default"));
    assert_eq!(style.feature_type_styles.len(), 1);

    let fts = &style.feature_type_styles[0];
    assert_eq!(fts.rules.len(), 1);

    let rule = &fts.rules[0];
    assert_eq!(rule.name.as_deref(), Some("major"));
    assert!(rule.filter.is_none());
    assert!(!rule.else_filter);
    assert_eq!(rule.min_scale, None);
    assert_eq!(rule.max_scale, Some(20_000_000.0));
    assert_eq!(rule.symbolizers.len(), 1);

    match &rule.symbolizers[0] {
        Symbolizer::Text(text) => {
            assert_eq!(
                text.label,
                vec![LabelPart::PropertyName("name".to_string())]
            );
            assert_eq!(text.font.family.as_deref(), Some("DejaVu Sans"));
            match &text.placement {
                LabelPlacement::Point(p) => {
                    assert!(p.anchor.is_none());
                    assert!(p.displacement.is_none());
                    assert!(p.rotation.is_none());
                }
                LabelPlacement::Line(_) => panic!("expected point placement"),
            }
            assert!(text.halo.is_none());
            assert!(text.fill.is_none());
            assert!(text.priority.is_none());
            assert!(text.vendor.is_empty());
        }
        other => panic!("expected Symbolizer::Text, got {other:?}"),
    }
}

#[test]
fn top_level_parse_entry_point_exists_and_is_stubbed() {
    // Task 2 implements the real parser; for Task 1 it must exist and fail loudly rather than
    // silently returning something bogus.
    let result = terraserve::sld::parse("<StyledLayerDescriptor/>");
    assert!(result.is_err());
}
