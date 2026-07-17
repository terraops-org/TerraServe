use std::fs;
use terraserve::vector::feature::{Props, Value};
use terraserve::vector::style::{Cmp, Filter, Style, Symbolizer};

#[test]
fn style_from_json_str_yields_one_rule_two_symbolizers() {
    let text = fs::read_to_string("fixtures/styles/airports.vec.json").unwrap();
    let style = Style::from_json_str(&text).unwrap();
    assert_eq!(style.feature_type_styles[0].rules.len(), 1);
    let rule = &style.feature_type_styles[0].rules[0];
    assert!(rule.filter.is_none());
    assert!(
        !rule.else_filter,
        "the JSON shim's one rule is not an else-rule"
    );
    assert!(rule.min_scale.is_none());
    assert!(rule.max_scale.is_none());
    assert_eq!(rule.symbolizers.len(), 2);

    let mut saw_point = false;
    let mut saw_text = false;
    for sym in &rule.symbolizers {
        match sym {
            Symbolizer::Point(p) => {
                saw_point = true;
                assert_eq!(p.radius, 3.0);
                assert_eq!(p.fill, [30, 30, 30, 255]);
                assert_eq!(p.stroke, [255, 255, 255, 255]);
            }
            Symbolizer::Text(t) => {
                saw_text = true;
                assert_eq!(
                    t.label,
                    vec![terraserve::vector::style::LabelPart::Field(
                        "name".to_string()
                    )]
                );
                assert_eq!(
                    t.priority,
                    Some(terraserve::vector::style::Priority::Field(
                        "scalerank".to_string()
                    ))
                );
                assert_eq!(t.size, 16.0);
                assert_eq!(t.offset, 4.0);
                assert_eq!(t.halo_radius, 2.0);
            }
            other => panic!("point+text-only shim must not carry Line/Polygon, got {other:?}"),
        }
    }
    assert!(saw_point && saw_text);
}

#[test]
fn style_from_json_str_missing_text_is_error() {
    assert!(Style::from_json_str(r#"{"point":{"radius":2.0}}"#).is_err());
}

#[test]
fn style_from_json_str_polygon_only_yields_polygon_symbolizer() {
    let text =
        r#"{"polygon":{"fill":[200,180,140,255],"stroke":[80,60,40,255],"stroke_width":1.5}}"#;
    let style = Style::from_json_str(text).unwrap();
    assert_eq!(style.feature_type_styles[0].rules.len(), 1);
    let rule = &style.feature_type_styles[0].rules[0];
    assert_eq!(rule.symbolizers.len(), 1);
    match &rule.symbolizers[0] {
        Symbolizer::Polygon(p) => {
            assert_eq!(p.fill, [200, 180, 140, 255]);
            assert_eq!(p.stroke, Some([80, 60, 40, 255]));
            assert_eq!(p.stroke_width, 1.5);
        }
        other => panic!("expected Symbolizer::Polygon, got {other:?}"),
    }
}

#[test]
fn style_from_json_str_polygon_stroke_absent_is_none() {
    let text = r#"{"polygon":{"fill":[200,180,140,255],"stroke_width":1.5}}"#;
    let style = Style::from_json_str(text).unwrap();
    match &style.feature_type_styles[0].rules[0].symbolizers[0] {
        Symbolizer::Polygon(p) => assert_eq!(p.stroke, None),
        other => panic!("expected Symbolizer::Polygon, got {other:?}"),
    }
}

#[test]
fn style_from_json_str_line_only_yields_line_symbolizer() {
    let text = r#"{"line":{"stroke":[120,120,120,255],"stroke_width":2.0}}"#;
    let style = Style::from_json_str(text).unwrap();
    assert_eq!(style.feature_type_styles[0].rules.len(), 1);
    let rule = &style.feature_type_styles[0].rules[0];
    assert_eq!(rule.symbolizers.len(), 1);
    match &rule.symbolizers[0] {
        Symbolizer::Line(l) => {
            assert_eq!(l.stroke, [120, 120, 120, 255]);
            assert_eq!(l.stroke_width, 2.0);
        }
        other => panic!("expected Symbolizer::Line, got {other:?}"),
    }
}

#[test]
fn style_from_json_str_combined_point_text_polygon_yields_all_three() {
    let text = fs::read_to_string("fixtures/styles/airports.vec.json").unwrap();
    let mut j: serde_json::Value = serde_json::from_str(&text).unwrap();
    j.as_object_mut().unwrap().insert(
        "polygon".to_string(),
        serde_json::json!({"fill": [200, 180, 140, 255], "stroke": [80, 60, 40, 255], "stroke_width": 1.5}),
    );
    let style = Style::from_json_str(&j.to_string()).unwrap();
    assert_eq!(style.feature_type_styles[0].rules.len(), 1);
    let rule = &style.feature_type_styles[0].rules[0];
    assert_eq!(rule.symbolizers.len(), 3);

    let mut saw_point = false;
    let mut saw_text = false;
    let mut saw_polygon = false;
    for sym in &rule.symbolizers {
        match sym {
            Symbolizer::Point(_) => saw_point = true,
            Symbolizer::Text(_) => saw_text = true,
            Symbolizer::Polygon(p) => {
                saw_polygon = true;
                assert_eq!(p.fill, [200, 180, 140, 255]);
                assert_eq!(p.stroke, Some([80, 60, 40, 255]));
                assert_eq!(p.stroke_width, 1.5);
            }
            Symbolizer::Line(_) => panic!("no line symbolizer expected"),
        }
    }
    assert!(saw_point && saw_text && saw_polygon);
}

fn props_with_scalerank(n: f64) -> Props {
    let mut p = Props::new();
    p.insert("scalerank".to_string(), Value::Num(n));
    p
}

#[test]
fn filter_cmp_numeric_lt_true_and_false() {
    let f = Filter::Cmp(Cmp::Lt, "scalerank".to_string(), "4".to_string());
    assert!(f.eval(&props_with_scalerank(2.0)));
    assert!(!f.eval(&props_with_scalerank(6.0)));
}

#[test]
fn filter_cmp_string_eq_works() {
    let mut p = Props::new();
    p.insert("name".to_string(), Value::Str("Lisboa".to_string()));
    let f = Filter::Cmp(Cmp::Eq, "name".to_string(), "Lisboa".to_string());
    assert!(f.eval(&p));
    let f2 = Filter::Cmp(Cmp::Eq, "name".to_string(), "Porto".to_string());
    assert!(!f2.eval(&p));
}

#[test]
fn filter_is_null_true_when_absent() {
    let p = Props::new();
    assert!(Filter::IsNull("missing".to_string()).eval(&p));
    let p2 = props_with_scalerank(1.0);
    assert!(!Filter::IsNull("scalerank".to_string()).eval(&p2));
}

#[test]
fn filter_between_and_like_and_and_or_not() {
    let p = props_with_scalerank(3.0);
    let between = Filter::Between("scalerank".to_string(), "1".to_string(), "5".to_string());
    assert!(between.eval(&p));
    let between_out = Filter::Between("scalerank".to_string(), "4".to_string(), "5".to_string());
    assert!(!between_out.eval(&p));

    let mut np = Props::new();
    np.insert("name".to_string(), Value::Str("Lisboa Intl".to_string()));
    assert!(Filter::Like("name".to_string(), "Lisboa%".to_string()).eval(&np));
    assert!(!Filter::Like("name".to_string(), "Porto%".to_string()).eval(&np));

    let and = Filter::And(vec![
        Filter::Cmp(Cmp::Ge, "scalerank".to_string(), "1".to_string()),
        Filter::Cmp(Cmp::Le, "scalerank".to_string(), "3".to_string()),
    ]);
    assert!(and.eval(&p));

    let or = Filter::Or(vec![
        Filter::Cmp(Cmp::Eq, "scalerank".to_string(), "99".to_string()),
        Filter::Cmp(Cmp::Eq, "scalerank".to_string(), "3".to_string()),
    ]);
    assert!(or.eval(&p));

    let not = Filter::Not(Box::new(Filter::Cmp(
        Cmp::Eq,
        "scalerank".to_string(),
        "3".to_string(),
    )));
    assert!(!not.eval(&p));
}

#[test]
fn filter_cmp_numeric_ne_true_and_false() {
    let f = Filter::Cmp(Cmp::Ne, "scalerank".to_string(), "4".to_string());
    assert!(f.eval(&props_with_scalerank(2.0)));
    assert!(!f.eval(&props_with_scalerank(4.0)));
}

#[test]
fn filter_cmp_string_ne_true_and_false() {
    let mut p = Props::new();
    p.insert("name".to_string(), Value::Str("Lisboa".to_string()));
    let f = Filter::Cmp(Cmp::Ne, "name".to_string(), "Porto".to_string());
    assert!(f.eval(&p));
    let f2 = Filter::Cmp(Cmp::Ne, "name".to_string(), "Lisboa".to_string());
    assert!(!f2.eval(&p));
}

#[test]
fn filter_cmp_numeric_gt_true_and_false() {
    let f = Filter::Cmp(Cmp::Gt, "scalerank".to_string(), "4".to_string());
    assert!(f.eval(&props_with_scalerank(6.0)));
    assert!(!f.eval(&props_with_scalerank(2.0)));
    // Equal does not satisfy Gt.
    assert!(!f.eval(&props_with_scalerank(4.0)));
}

#[test]
fn filter_cmp_le_false_when_greater() {
    let f = Filter::Cmp(Cmp::Le, "scalerank".to_string(), "4".to_string());
    assert!(!f.eval(&props_with_scalerank(6.0)));
    // True-direction sanity check alongside the false case above.
    assert!(f.eval(&props_with_scalerank(2.0)));
}

#[test]
fn filter_cmp_ge_false_when_less() {
    let f = Filter::Cmp(Cmp::Ge, "scalerank".to_string(), "4".to_string());
    assert!(!f.eval(&props_with_scalerank(2.0)));
    // True-direction sanity check alongside the false case above.
    assert!(f.eval(&props_with_scalerank(6.0)));
}

#[test]
fn filter_like_underscore_matches_exactly_one_char() {
    let mut p = Props::new();
    p.insert("name".to_string(), Value::Str("ABC".to_string()));
    assert!(Filter::Like("name".to_string(), "A_C".to_string()).eval(&p));

    let mut too_short = Props::new();
    too_short.insert("name".to_string(), Value::Str("AC".to_string()));
    assert!(!Filter::Like("name".to_string(), "A_C".to_string()).eval(&too_short));

    let mut too_long = Props::new();
    too_long.insert("name".to_string(), Value::Str("ABBC".to_string()));
    assert!(!Filter::Like("name".to_string(), "A_C".to_string()).eval(&too_long));
}

#[test]
fn filter_like_percent_matches_substring_anywhere_not_just_prefix() {
    let mut p = Props::new();
    p.insert("name".to_string(), Value::Str("New York".to_string()));
    assert!(Filter::Like("name".to_string(), "%York%".to_string()).eval(&p));

    // `%York%` also matches when "York" is embedded further inside the string (not anchored to
    // either edge — this is the normalized form of the real-world SE-1.1/QGIS bug case).
    let mut p2 = Props::new();
    p2.insert("name".to_string(), Value::Str("New Yorkshire".to_string()));
    assert!(Filter::Like("name".to_string(), "%York%".to_string()).eval(&p2));
}

#[test]
fn filter_like_backslash_escapes_a_literal_percent() {
    let mut matches = Props::new();
    matches.insert("x".to_string(), Value::Str("100%".to_string()));
    assert!(Filter::Like("x".to_string(), "100\\%".to_string()).eval(&matches));

    let mut no_match = Props::new();
    no_match.insert("x".to_string(), Value::Str("100 done".to_string()));
    assert!(!Filter::Like("x".to_string(), "100\\%".to_string()).eval(&no_match));
}

// -------------------------------------------------------------------------------------------
// SEC-2: `PropertyIsLike` uses an iterative two-pointer matcher (was a recursive `like_rec` that
// branched exponentially on `%`). These pin both correctness parity and — crucially — that a
// pathological operator-authored pattern returns promptly instead of hanging the request.
// -------------------------------------------------------------------------------------------

#[test]
fn filter_like_matcher_parity_edges() {
    let mk = |s: &str| {
        let mut p = Props::new();
        p.insert("x".to_string(), Value::Str(s.to_string()));
        p
    };
    let like = |pat: &str| Filter::Like("x".to_string(), pat.to_string());

    // `%` matches an empty run, a whole string, and anchors at either edge or the interior.
    assert!(like("%").eval(&mk("")));
    assert!(like("%").eval(&mk("anything")));
    assert!(like("a%").eval(&mk("a")));
    assert!(like("%z").eval(&mk("xyz")));
    assert!(like("%mid%").eval(&mk("a mid b")));
    // Consecutive `%%` are equivalent to a single `%` (the normalize pass also collapses them).
    assert!(like("a%%c").eval(&mk("abbc")));
    // `_` is exactly one char; a literal must match verbatim.
    assert!(like("a_c").eval(&mk("abc")));
    assert!(!like("a_c").eval(&mk("ac")));
    assert!(like("abc").eval(&mk("abc")));
    assert!(!like("abc").eval(&mk("abd")));
    // Escapes: `\%` / `\_` / `\\` match the literal metacharacter, not the wildcard.
    assert!(like("100\\%").eval(&mk("100%")));
    assert!(!like("100\\%").eval(&mk("100x")));
    assert!(like("a\\_b").eval(&mk("a_b")));
    assert!(!like("a\\_b").eval(&mk("axb")));
    assert!(like("a\\\\b").eval(&mk("a\\b")));
    // A trailing `\` (nothing after) is a literal backslash.
    assert!(like("a\\").eval(&mk("a\\")));
}

#[test]
fn filter_like_pathological_pattern_returns_promptly_not_exponentially() {
    // `%a%a…%b` vs an all-`a` string with no `b` is the classic backtracking bomb: the old
    // recursive matcher was ~exponential in the number of `%a` groups (n=12 already timed out).
    // The iterative matcher is O(text × pattern), so this completes instantly; if someone reverts
    // to the exponential form this test hangs the suite (a CI timeout is the regression signal).
    let pattern: String = std::iter::repeat("%a").take(30).collect::<String>() + "%b";
    let text: String = std::iter::repeat('a').take(60).collect();
    let mut p = Props::new();
    p.insert("x".to_string(), Value::Str(text));
    assert!(!Filter::Like("x".to_string(), pattern).eval(&p));
}

#[test]
fn filter_between_inclusive_boundaries() {
    let between = Filter::Between("scalerank".to_string(), "1".to_string(), "5".to_string());
    assert!(between.eval(&props_with_scalerank(1.0)));
    assert!(between.eval(&props_with_scalerank(5.0)));
}

#[test]
fn filter_and_or_vacuous_semantics() {
    let p = props_with_scalerank(3.0);
    assert!(Filter::And(vec![]).eval(&p));
    assert!(!Filter::Or(vec![]).eval(&p));
}
