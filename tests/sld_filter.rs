//! Task 3 focused test: `sld::filter::parse_filter` — the `<ogc:Filter>` AST + parser
//! (comparison + logical operators), and its wiring into `sld::parse::parse_rule`
//! (`<Rule>`'s `<Filter>` populates `rule.filter`; `<ElseFilter/>` sets `else_filter`).

use terraserve::sld::filter::{parse_filter, CompOp, SldFilter};

/// Parse a filter-operator fragment wrapped in a self-contained `<Filter>` root element (its own
/// `ogc:` namespace declaration — doesn't need the surrounding `<StyledLayerDescriptor>`).
fn parse_fragment(inner: &str) -> Option<SldFilter> {
    let xml = format!(r#"<Filter xmlns:ogc="http://www.opengis.net/ogc">{inner}</Filter>"#);
    let doc = roxmltree::Document::parse(&xml).expect("fragment should be well-formed XML");
    parse_filter(doc.root_element())
}

#[test]
fn parses_and_of_two_comparisons() {
    // Brief's worked example: `<Filter><ogc:And><ogc:PropertyIsLessThan>...` → `And([...])`.
    let filter = parse_fragment(
        "<ogc:And>\
           <ogc:PropertyIsLessThan>\
             <ogc:PropertyName>scalerank</ogc:PropertyName><ogc:Literal>4</ogc:Literal>\
           </ogc:PropertyIsLessThan>\
           <ogc:PropertyIsEqualTo>\
             <ogc:PropertyName>kind</ogc:PropertyName><ogc:Literal>city</ogc:Literal>\
           </ogc:PropertyIsEqualTo>\
         </ogc:And>",
    )
    .expect("should parse");

    assert_eq!(
        filter,
        SldFilter::And(vec![
            SldFilter::Comparison {
                op: CompOp::Lt,
                prop: "scalerank".to_string(),
                value: "4".to_string(),
            },
            SldFilter::Comparison {
                op: CompOp::Eq,
                prop: "kind".to_string(),
                value: "city".to_string(),
            },
        ])
    );
}

#[test]
fn parses_or_of_two_comparisons() {
    let filter = parse_fragment(
        "<ogc:Or>\
           <ogc:PropertyIsGreaterThan>\
             <ogc:PropertyName>pop</ogc:PropertyName><ogc:Literal>1000000</ogc:Literal>\
           </ogc:PropertyIsGreaterThan>\
           <ogc:PropertyIsNotEqualTo>\
             <ogc:PropertyName>kind</ogc:PropertyName><ogc:Literal>village</ogc:Literal>\
           </ogc:PropertyIsNotEqualTo>\
         </ogc:Or>",
    )
    .expect("should parse");

    assert_eq!(
        filter,
        SldFilter::Or(vec![
            SldFilter::Comparison {
                op: CompOp::Gt,
                prop: "pop".to_string(),
                value: "1000000".to_string(),
            },
            SldFilter::Comparison {
                op: CompOp::Ne,
                prop: "kind".to_string(),
                value: "village".to_string(),
            },
        ])
    );
}

#[test]
fn parses_not_of_a_comparison() {
    let filter = parse_fragment(
        "<ogc:Not>\
           <ogc:PropertyIsEqualTo>\
             <ogc:PropertyName>kind</ogc:PropertyName><ogc:Literal>capital</ogc:Literal>\
           </ogc:PropertyIsEqualTo>\
         </ogc:Not>",
    )
    .expect("should parse");

    assert_eq!(
        filter,
        SldFilter::Not(Box::new(SldFilter::Comparison {
            op: CompOp::Eq,
            prop: "kind".to_string(),
            value: "capital".to_string(),
        }))
    );
}

#[test]
fn parses_all_six_comparison_operators() {
    let cases = [
        ("PropertyIsEqualTo", CompOp::Eq),
        ("PropertyIsNotEqualTo", CompOp::Ne),
        ("PropertyIsLessThan", CompOp::Lt),
        ("PropertyIsGreaterThan", CompOp::Gt),
        ("PropertyIsLessThanOrEqualTo", CompOp::Le),
        ("PropertyIsGreaterThanOrEqualTo", CompOp::Ge),
    ];
    for (tag, op) in cases {
        let inner = format!(
            "<ogc:{tag}><ogc:PropertyName>x</ogc:PropertyName><ogc:Literal>1</ogc:Literal></ogc:{tag}>"
        );
        let filter = parse_fragment(&inner).unwrap_or_else(|| panic!("{tag} should parse"));
        assert_eq!(
            filter,
            SldFilter::Comparison {
                op,
                prop: "x".to_string(),
                value: "1".to_string(),
            },
            "mismatched result for {tag}"
        );
    }
}

#[test]
fn parses_between() {
    let filter = parse_fragment(
        "<ogc:PropertyIsBetween>\
           <ogc:PropertyName>scalerank</ogc:PropertyName>\
           <ogc:LowerBoundary><ogc:Literal>1</ogc:Literal></ogc:LowerBoundary>\
           <ogc:UpperBoundary><ogc:Literal>4</ogc:Literal></ogc:UpperBoundary>\
         </ogc:PropertyIsBetween>",
    )
    .expect("should parse");

    assert_eq!(
        filter,
        SldFilter::Between {
            prop: "scalerank".to_string(),
            lo: "1".to_string(),
            hi: "4".to_string(),
        }
    );
}

// -------------------------------------------------------------------------------------------
// `PropertyIsLike` wildcard normalization: the element's `wildCard`/`singleChar`/`escapeChar`
// attributes describe the SOURCE document's own delimiter convention, which `parse_like` rewrites
// into the engine's canonical `%`/`_`/`\` convention before it's ever stored — see
// `sld::filter::normalize_like_pattern`'s doc for the exact rules. Regression-covers the bug
// where a real SE-1.1/QGIS filter like `<PropertyIsLike wildCard="*" ...><Literal>*York*</Literal>`
// used to lower straight through to `Like(_, "*York*")`, which then matched NOTHING against the
// `%`/`_`-only IR matcher (`Filter::Like`'s `like_match`) — silently vanishing every feature.
// -------------------------------------------------------------------------------------------

#[test]
fn parses_like_normalizes_source_wildcard_to_canonical_percent() {
    // The brief's worked example, byte-for-byte: `*` is this document's wildcard, `.` its
    // single-char, `!` its escape — none of which are the canonical `%`/`_`/`\`.
    let filter = parse_fragment(
        "<ogc:PropertyIsLike wildCard=\"*\" singleChar=\".\" escapeChar=\"!\">\
           <ogc:PropertyName>name</ogc:PropertyName><ogc:Literal>*York*</ogc:Literal>\
         </ogc:PropertyIsLike>",
    )
    .expect("should parse");

    assert_eq!(
        filter,
        SldFilter::Like {
            prop: "name".to_string(),
            pattern: "%York%".to_string(),
        }
    );
}

#[test]
fn parses_like_escaped_source_wildcard_char_becomes_a_literal_not_percent() {
    // `escapeChar="!"` protects the `*` in `gr!*de` — it must survive as a literal `*` in the
    // canonical pattern, NOT be converted to `%` (that would turn an exact-match request into a
    // wildcard one).
    let filter = parse_fragment(
        "<ogc:PropertyIsLike wildCard=\"*\" singleChar=\".\" escapeChar=\"!\">\
           <ogc:PropertyName>name</ogc:PropertyName><ogc:Literal>gr!*de</ogc:Literal>\
         </ogc:PropertyIsLike>",
    )
    .expect("should parse");

    assert_eq!(
        filter,
        SldFilter::Like {
            prop: "name".to_string(),
            pattern: "gr*de".to_string(),
        }
    );
}

#[test]
fn parses_like_escaped_source_char_that_collides_with_canonical_metachar_is_escaped() {
    // `escapeChar="!"` protects a literal `%` in the source pattern (source wildCard is `*`, not
    // `%`) — since `%` is a canonical metacharacter, the normalized pattern must escape it
    // (`\%`), or the IR matcher would misread it as "any run of characters".
    let filter = parse_fragment(
        "<ogc:PropertyIsLike wildCard=\"*\" singleChar=\".\" escapeChar=\"!\">\
           <ogc:PropertyName>name</ogc:PropertyName><ogc:Literal>100!%</ogc:Literal>\
         </ogc:PropertyIsLike>",
    )
    .expect("should parse");

    assert_eq!(
        filter,
        SldFilter::Like {
            prop: "name".to_string(),
            pattern: "100\\%".to_string(),
        }
    );
}

#[test]
fn parses_like_defaults_to_percent_underscore_backslash_when_attributes_absent() {
    // OGC Filter Encoding says `wildCard`/`singleChar`/`escapeChar` are required, but a real
    // document may omit them; default to the classic SQL/SLD triple, i.e. the source pattern is
    // treated as already-canonical — normalization is a no-op.
    let filter = parse_fragment(
        "<ogc:PropertyIsLike>\
           <ogc:PropertyName>name</ogc:PropertyName><ogc:Literal>New%York</ogc:Literal>\
         </ogc:PropertyIsLike>",
    )
    .expect("should parse");

    assert_eq!(
        filter,
        SldFilter::Like {
            prop: "name".to_string(),
            pattern: "New%York".to_string(),
        }
    );
}

#[test]
fn parses_like_collapses_consecutive_wildcards() {
    // SEC-2: consecutive source wildcards collapse to a single canonical `%` (semantically
    // identical, and a shorter pattern for the matcher). Source wildcard here is `*`.
    let filter = parse_fragment(
        "<ogc:PropertyIsLike wildCard=\"*\" singleChar=\".\" escapeChar=\"!\">\
           <ogc:PropertyName>name</ogc:PropertyName><ogc:Literal>a**b</ogc:Literal>\
         </ogc:PropertyIsLike>",
    )
    .expect("should parse");

    assert_eq!(
        filter,
        SldFilter::Like {
            prop: "name".to_string(),
            pattern: "a%b".to_string(),
        }
    );
}

#[test]
fn parses_is_null() {
    let filter = parse_fragment(
        "<ogc:PropertyIsNull><ogc:PropertyName>name</ogc:PropertyName></ogc:PropertyIsNull>",
    )
    .expect("should parse");

    assert_eq!(
        filter,
        SldFilter::IsNull {
            prop: "name".to_string(),
        }
    );
}

#[test]
fn namespace_tolerant_unprefixed_elements_still_parse() {
    // Local-name matching only — a document with no `ogc:` prefix (or a bare default namespace)
    // must parse identically.
    let xml = "<Filter><PropertyIsEqualTo><PropertyName>kind</PropertyName><Literal>city</Literal></PropertyIsEqualTo></Filter>";
    let doc = roxmltree::Document::parse(xml).expect("well-formed");
    let filter = parse_filter(doc.root_element()).expect("should parse without ogc: prefix");

    assert_eq!(
        filter,
        SldFilter::Comparison {
            op: CompOp::Eq,
            prop: "kind".to_string(),
            value: "city".to_string(),
        }
    );
}

#[test]
fn unknown_operator_returns_none_without_panicking() {
    let filter = parse_fragment("<ogc:SomeFutureOperator/>");
    assert_eq!(filter, None);
}

#[test]
fn comparison_missing_an_operand_returns_none() {
    // No <Literal> — can't build a valid Comparison.
    let filter = parse_fragment(
        "<ogc:PropertyIsEqualTo><ogc:PropertyName>kind</ogc:PropertyName></ogc:PropertyIsEqualTo>",
    );
    assert_eq!(filter, None);
}

// -------------------------------------------------------------------------------------------
// And/Or/Not failure-propagation semantics (CORR-4): `parse_filter`'s `And`/`Or`/`Not` arms
// propagate ANY present child's parse failure to the whole clause — whether that's an
// unrecognized operator or a recognized-but-malformed one (e.g. `PropertyIsEqualTo` missing its
// `<Literal>`) — returning `None` for the enclosing clause rather than silently dropping the
// failed child and keeping the rest.
//
// This REVERSES an earlier Task-3 review-fix that pinned drop-and-keep-the-rest as "intended":
// dropping a conjunct makes an `And` match MORE features, dropping a disjunct makes an `Or`
// match FEWER, and either way it happens with no signal. A single `None` propagating all the way
// up means the whole `<Filter>` fails to parse → `Rule.filter` stays `None` → the rule matches
// all features — the documented, consistent whole-filter fail-open stance, instead of a clause
// being silently reshaped mid-tree.
//
// A genuinely EMPTY `And`/`Or` (zero element children — nothing present to fail) is a distinct,
// unaffected case: it still parses to `Some(And(vec![]))`/`Some(Or(vec![]))`, the vacuous-match
// values (`And([])` evals true, `Or([])` evals false).
// -------------------------------------------------------------------------------------------

#[test]
fn and_propagates_failure_of_a_malformed_recognized_child_returning_none() {
    // Second child is a recognized operator (PropertyIsEqualTo) but missing its <Literal>. Per
    // CORR-4 that failure now propagates to the whole `<And>`, NOT just drops the bad child.
    let filter = parse_fragment(
        "<ogc:And>\
           <ogc:PropertyIsLessThan>\
             <ogc:PropertyName>scalerank</ogc:PropertyName><ogc:Literal>4</ogc:Literal>\
           </ogc:PropertyIsLessThan>\
           <ogc:PropertyIsEqualTo>\
             <ogc:PropertyName>kind</ogc:PropertyName>\
           </ogc:PropertyIsEqualTo>\
         </ogc:And>",
    );

    assert_eq!(
        filter, None,
        "a malformed child must fail the whole And, not silently drop out of it"
    );
}

#[test]
fn and_propagates_failure_of_an_unknown_operator_child_returning_none() {
    // Second child is an operator `parse_filter` doesn't recognize at all — one valid conjunct,
    // one unparseable. Per CORR-4 this must yield `None` for the whole `<And>`, NOT
    // `And([valid])`: silently dropping a conjunct would make the And match MORE features than
    // the document intended.
    let filter = parse_fragment(
        "<ogc:And>\
           <ogc:PropertyIsEqualTo>\
             <ogc:PropertyName>kind</ogc:PropertyName><ogc:Literal>city</ogc:Literal>\
           </ogc:PropertyIsEqualTo>\
           <ogc:PropertyIsRandomUnsupportedOp/>\
         </ogc:And>",
    );

    assert_eq!(
        filter, None,
        "an unrecognized-operator child must fail the whole And, not silently drop out of it"
    );
}

#[test]
fn and_with_every_child_unparseable_returns_none() {
    // One malformed recognized child + one unrecognized child — every child fails, and (as
    // before CORR-4) the whole `<And>` resolves to `None`. CORR-4 doesn't change this case; it
    // extends the same `None` result to the *any-child-fails* case above, not just *all-fail*.
    let filter = parse_fragment(
        "<ogc:And>\
           <ogc:PropertyIsEqualTo><ogc:PropertyName>kind</ogc:PropertyName></ogc:PropertyIsEqualTo>\
           <ogc:SomeFutureOperator/>\
         </ogc:And>",
    );
    assert_eq!(filter, None);
}

#[test]
fn and_with_zero_element_children_is_the_vacuous_empty_and_not_none() {
    // A genuinely EMPTY `<And>` (no element children at all — nothing present to fail) is
    // distinct from "a child was present and failed": CORR-4's propagation only fires when a
    // child exists and fails to parse. `Some(And(vec![]))` is the vacuous-true value.
    let filter = parse_fragment("<ogc:And></ogc:And>").expect("empty And should still parse");
    assert_eq!(filter, SldFilter::And(vec![]));
}

#[test]
fn or_propagates_failure_of_a_malformed_recognized_child_returning_none() {
    // Per CORR-4: dropping a disjunct would make the Or match FEWER features than the document
    // intended, silently — so the malformed sibling must fail the whole `<Or>` instead.
    let filter = parse_fragment(
        "<ogc:Or>\
           <ogc:PropertyIsGreaterThan>\
             <ogc:PropertyName>pop</ogc:PropertyName><ogc:Literal>1000000</ogc:Literal>\
           </ogc:PropertyIsGreaterThan>\
           <ogc:PropertyIsNotEqualTo>\
             <ogc:PropertyName>kind</ogc:PropertyName>\
           </ogc:PropertyIsNotEqualTo>\
         </ogc:Or>",
    );

    assert_eq!(
        filter, None,
        "a malformed child must fail the whole Or, not silently drop out of it"
    );
}

#[test]
fn or_with_every_child_unparseable_returns_none() {
    let filter = parse_fragment(
        "<ogc:Or>\
           <ogc:PropertyIsEqualTo><ogc:PropertyName>kind</ogc:PropertyName></ogc:PropertyIsEqualTo>\
           <ogc:SomeFutureOperator/>\
         </ogc:Or>",
    );
    assert_eq!(filter, None);
}

#[test]
fn or_with_zero_element_children_is_the_vacuous_empty_or_not_none() {
    // Mirrors the empty-And case: zero element children (nothing present to fail) parses to the
    // vacuous-false `Some(Or(vec![]))`, distinct from CORR-4's failure-propagation case.
    let filter = parse_fragment("<ogc:Or></ogc:Or>").expect("empty Or should still parse");
    assert_eq!(filter, SldFilter::Or(vec![]));
}

#[test]
fn not_propagates_failure_of_its_single_child_returning_none() {
    // `<Not>` already wraps a single child via `.map()`, which naturally propagates a `None`
    // inner parse — this test pins that down explicitly alongside the And/Or CORR-4 fix so all
    // three logical operators are covered by the same fail-propagation contract.
    let filter = parse_fragment("<ogc:Not><ogc:PropertyIsRandomUnsupportedOp/></ogc:Not>");
    assert_eq!(filter, None);
}

// -------------------------------------------------------------------------------------------
// `Between` bare-text boundary fallback (lock-in): `boundary_text` prefers a wrapped
// `<Literal>` child but falls back to the boundary element's own direct text when there's no
// `<Literal>` — a non-conformant but harmless `<LowerBoundary>1</LowerBoundary>` shape.
// -------------------------------------------------------------------------------------------

#[test]
fn between_accepts_a_bare_text_lower_boundary_without_a_literal_wrapper() {
    let filter = parse_fragment(
        "<ogc:PropertyIsBetween>\
           <ogc:PropertyName>scalerank</ogc:PropertyName>\
           <ogc:LowerBoundary>1</ogc:LowerBoundary>\
           <ogc:UpperBoundary><ogc:Literal>4</ogc:Literal></ogc:UpperBoundary>\
         </ogc:PropertyIsBetween>",
    )
    .expect("bare LowerBoundary text should be used as a fallback with no <Literal> wrapper");

    assert_eq!(
        filter,
        SldFilter::Between {
            prop: "scalerank".to_string(),
            lo: "1".to_string(),
            hi: "4".to_string(),
        }
    );
}

// -------------------------------------------------------------------------------------------
// Wiring: `sld::parse::parse_rule` populates `Rule.filter` / `Rule.else_filter`.
// -------------------------------------------------------------------------------------------

#[test]
fn rule_filter_element_is_parsed_and_attached() {
    let xml = r#"<StyledLayerDescriptor xmlns:ogc="http://www.opengis.net/ogc">
      <NamedLayer><Name>places</Name><UserStyle><FeatureTypeStyle>
        <Rule>
          <Name>small</Name>
          <ogc:Filter>
            <ogc:PropertyIsLessThan>
              <ogc:PropertyName>scalerank</ogc:PropertyName><ogc:Literal>4</ogc:Literal>
            </ogc:PropertyIsLessThan>
          </ogc:Filter>
        </Rule>
      </FeatureTypeStyle></UserStyle></NamedLayer>
    </StyledLayerDescriptor>"#;

    let sld = terraserve::sld::parse(xml).expect("should parse");
    let rule = &sld.named_layers[0].styles[0].feature_type_styles[0].rules[0];

    assert_eq!(
        rule.filter,
        Some(SldFilter::Comparison {
            op: CompOp::Lt,
            prop: "scalerank".to_string(),
            value: "4".to_string(),
        })
    );
    assert!(!rule.else_filter);
}

#[test]
fn rule_else_filter_element_sets_flag_and_leaves_filter_none() {
    let xml = r#"<StyledLayerDescriptor>
      <NamedLayer><Name>places</Name><UserStyle><FeatureTypeStyle>
        <Rule>
          <Name>fallback</Name>
          <ElseFilter/>
        </Rule>
      </FeatureTypeStyle></UserStyle></NamedLayer>
    </StyledLayerDescriptor>"#;

    let sld = terraserve::sld::parse(xml).expect("should parse");
    let rule = &sld.named_layers[0].styles[0].feature_type_styles[0].rules[0];

    assert!(rule.else_filter);
    assert!(rule.filter.is_none());
}

// -------------------------------------------------------------------------------------------
// SEC-3: `parse_filter` caps `And`/`Or`/`Not`/`Filter` nesting depth (roxmltree parses the DOM
// iteratively, so a pathological deeply-nested document survives XML parsing and would then
// overflow the native stack in the recursive descent). Beyond the cap the subtree fails to `None`
// — combined with whole-filter fail-open, a malicious document degrades to "no filter", not a
// crash. A shallow, legal nest still parses.
// -------------------------------------------------------------------------------------------

#[test]
fn deeply_nested_filter_returns_none_instead_of_recursing_unbounded() {
    // A document far past the 64 cap. roxmltree's OWN DOM parse recurses on element nesting, so we
    // run on a generous stack to isolate what we're actually asserting: once such a document is
    // parsed, `parse_filter` caps its descent and returns None (the whole-filter fail-open → the
    // rule matches all features), so the repeatable per-request eval/lower path never recurses on a
    // deep tree. Without the cap `parse_filter` itself would recurse to `depth`.
    let handle = std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            let depth = 400;
            let inner = "<ogc:PropertyIsEqualTo><ogc:PropertyName>k</ogc:PropertyName>\
                         <ogc:Literal>v</ogc:Literal></ogc:PropertyIsEqualTo>";
            let nested = format!(
                "{}{}{}",
                "<ogc:Not>".repeat(depth),
                inner,
                "</ogc:Not>".repeat(depth)
            );
            parse_fragment(&nested)
        })
        .expect("spawn");
    assert_eq!(
        handle
            .join()
            .expect("must not overflow on a generous stack"),
        None,
        "a filter nested past the cap must fail to None, not recurse to full depth"
    );
}

#[test]
fn shallow_legal_nesting_still_parses_under_the_depth_cap() {
    // A handful of nested Nots is ordinary and must still parse (the cap only rejects the absurd).
    let filter = parse_fragment(
        "<ogc:Not><ogc:Not>\
           <ogc:PropertyIsEqualTo>\
             <ogc:PropertyName>kind</ogc:PropertyName><ogc:Literal>city</ogc:Literal>\
           </ogc:PropertyIsEqualTo>\
         </ogc:Not></ogc:Not>",
    )
    .expect("shallow nesting must parse");
    assert_eq!(
        filter,
        SldFilter::Not(Box::new(SldFilter::Not(Box::new(SldFilter::Comparison {
            op: CompOp::Eq,
            prop: "kind".to_string(),
            value: "city".to_string(),
        }))))
    );
}

#[test]
fn rule_with_no_filter_and_no_else_filter_leaves_both_absent() {
    let xml = r#"<StyledLayerDescriptor>
      <NamedLayer><Name>places</Name><UserStyle><FeatureTypeStyle>
        <Rule>
          <Name>plain</Name>
        </Rule>
      </FeatureTypeStyle></UserStyle></NamedLayer>
    </StyledLayerDescriptor>"#;

    let sld = terraserve::sld::parse(xml).expect("should parse");
    let rule = &sld.named_layers[0].styles[0].feature_type_styles[0].rules[0];

    assert!(rule.filter.is_none());
    assert!(!rule.else_filter);
}
