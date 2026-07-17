//! Task 4 focused test: a real-world SLD **conformance corpus**.
//!
//! `tests/fixtures/sld/*.sld` holds representative GeoServer/QGIS-shaped SLD documents, harvested
//! verbatim from real sources (see the provenance comment above each `.sld` file's entry below —
//! and `.superpowers/sdd/task-4-report.md` for the full detail). The bar for this test is the one
//! set by the task brief: every fixture must `sld::parse` to `Ok`, with at least one rule and at
//! least one symbolizer, and never panic — proving the parser survives real-world SLD shapes, not
//! just the hand-rolled minimal fixture from Task 2 (`point_min.sld`).
//!
//! Fixture provenance (harvested 2026-07-12):
//!   * `geoserver_default_point.sld` — GeoServer `data/app-schema-tutorial/styles/default_point.sld`
//!     (upstream `geoserver/geoserver`, branch `main`): the canonical "default point style" shape —
//!     `<Mark><WellKnownName>square</WellKnownName>` + `<CssParameter name="fill">`, XML comments
//!     interspersed (GeoServer's own doc-comment style).
//!   * `geoserver_point_external_graphic.sld` — GeoServer `data/release/styles/burg.sld`: a point
//!     style with **no `<Mark>` at all** — `<ExternalGraphic>` (SVG icon) instead — plus a `<Size>`
//!     wrapped in `<ogc:Literal>` rather than given as bare text. Stresses the "Graphic may have
//!     zero marks" and "Expression may be an explicit `<ogc:Literal>` child" shapes.
//!   * `geoserver_point_attribute_scale_rules.sld` — GeoServer SLD Cookbook
//!     (`docs.geoserver.org/.../cookbook/artifacts/point_attribute.sld`, the real "Attribute-based
//!     point" cookbook recipe): three `<Rule>`s in one `FeatureTypeStyle`, `<ogc:PropertyIsLessThan>`
//!     / `<ogc:PropertyIsGreaterThanOrEqualTo>` / `<ogc:And>` filters, ISO-8859-1 XML declaration.
//!   * `qgis_point_export.sld` — `GraceTHD-community/GraceTHD-MCD` `qgis/sld/gracethd-noeud.sld`: a
//!     genuine QGIS-exported point-layer SLD (French fibre-network GIS project) — `se:` prefixes
//!     throughout, `se:SvgParameter` (not `CssParameter`), `se:Name` repeated at both `NamedLayer`
//!     and `UserStyle` level.
//!   * `se11_point_simple.sld` — `geostyler/geostyler-sld-parser` test corpus
//!     `data/slds/1.1/point_simplepoint.sld`: SLD 1.1.0 with the full `xmlns:se="http://www.opengis.net/se"`
//!     namespace, fill-opacity/stroke-opacity `se:SvgParameter`s, and a self-closing empty `<se:Name />`.
//!   * `se11_point_scale_filter.sld` — same corpus, `data/slds/1.1/point_simplepoint_filter.sld`:
//!     the canonical "realistic decluttering style" — `se:MinScaleDenominator`/`MaxScaleDenominator`
//!     **plus** a deeply nested `<ogc:Filter>` (`And` of `PropertyIsEqualTo` ×2, `PropertyIsNull`,
//!     `PropertyIsLike` ×2, `Not(PropertyIsGreaterThan)`, `Or`, `PropertyIsBetween`), with the
//!     `<ogc:Filter xmlns="http://www.opengis.net/ogc">` element locally re-asserting a default
//!     namespace and bare (unprefixed) `<LowerBoundary>`/`<UpperBoundary>`. This is the richest
//!     fixture, so it's the one snapshot-asserted below.
//!   * `point_min.sld` — the pre-existing Task 2 fixture (hand-authored, not harvested); included
//!     in the directory walk for completeness since it's already a valid SLD document.
//!
//! All six harvested files were fetched byte-verbatim (`curl` + `xmllint --noout` well-formedness
//! check) — no hand-editing of their SLD content.

use std::path::Path;

use terraserve::sld::Symbolizer;

const FIXTURE_DIR: &str = "tests/fixtures/sld";

/// Every `.sld` file directly under `tests/fixtures/sld/`, sorted for deterministic test order.
fn fixture_paths() -> Vec<std::path::PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(FIXTURE_DIR)
        .unwrap_or_else(|e| panic!("{FIXTURE_DIR} should be readable: {e}"))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("sld"))
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "expected at least one .sld fixture under {FIXTURE_DIR}"
    );
    paths
}

#[test]
fn every_real_world_fixture_parses_with_at_least_one_rule_and_symbolizer() {
    for path in fixture_paths() {
        let xml = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: should be readable: {e}", path.display()));

        let sld = terraserve::sld::parse(&xml)
            .unwrap_or_else(|e| panic!("{}: expected Ok, got Err({e:?})", path.display()));

        let rule_count: usize = sld
            .named_layers
            .iter()
            .flat_map(|l| &l.styles)
            .flat_map(|s| &s.feature_type_styles)
            .map(|fts| fts.rules.len())
            .sum();
        assert!(
            rule_count >= 1,
            "{}: expected >=1 Rule, got {rule_count}",
            path.display()
        );

        let symbolizer_count: usize = sld
            .named_layers
            .iter()
            .flat_map(|l| &l.styles)
            .flat_map(|s| &s.feature_type_styles)
            .flat_map(|fts| &fts.rules)
            .map(|r| r.symbolizers.len())
            .sum();
        assert!(
            symbolizer_count >= 1,
            "{}: expected >=1 Symbolizer, got {symbolizer_count}",
            path.display()
        );
    }
}

/// The `<ogc:Literal>`-wrapped `<Size>` in `geoserver_point_external_graphic.sld` (`burg.sld`)
/// should resolve to the literal's value ("20"), not an empty string — this is the real-world gap
/// `expression()` was extended to cover (see the parser-extension note in the task report).
#[test]
fn external_graphic_fixture_resolves_ogc_literal_wrapped_size() {
    let xml = std::fs::read_to_string(
        Path::new(FIXTURE_DIR).join("geoserver_point_external_graphic.sld"),
    )
    .expect("fixture should exist");
    let sld = terraserve::sld::parse(&xml).expect("fixture should parse");
    let rule = &sld.named_layers[0].styles[0].feature_type_styles[0].rules[0];
    match &rule.symbolizers[0] {
        Symbolizer::Point(point) => {
            assert_eq!(
                point.graphic.size,
                Some(terraserve::sld::Expression::Literal("20".to_string())),
                "ogc:Literal child of <Size> should be resolved, not left empty"
            );
            // No <Mark> in this fixture at all (ExternalGraphic instead) — confirms the parser
            // tolerates a Graphic with zero marks rather than panicking or erroring.
            assert!(point.graphic.marks.is_empty());
        }
        other => panic!("expected Symbolizer::Point, got {other:?}"),
    }
}

/// Multi-rule `FeatureTypeStyle` (GeoServer's real "Attribute-based point" cookbook recipe): three
/// rules, each gated by a different `<ogc:Filter>` shape (simple comparison, `And`-combination,
/// simple comparison again) — confirms all three parse and all three keep their filter.
#[test]
fn attribute_based_cookbook_fixture_parses_three_filtered_rules() {
    let xml = std::fs::read_to_string(
        Path::new(FIXTURE_DIR).join("geoserver_point_attribute_scale_rules.sld"),
    )
    .expect("fixture should exist");
    let sld = terraserve::sld::parse(&xml).expect("fixture should parse");
    let rules = &sld.named_layers[0].styles[0].feature_type_styles[0].rules;
    assert_eq!(rules.len(), 3);
    for rule in rules {
        assert!(
            rule.filter.is_some(),
            "rule {:?} should carry a parsed ogc:Filter",
            rule.name
        );
        assert_eq!(rule.symbolizers.len(), 1);
    }
}

/// QGIS-exported fixture: `se:` prefixes end-to-end, `se:SvgParameter` (not `CssParameter`) for
/// fill/stroke — confirms the namespace-tolerant, dual-attribute-name lookups both work together
/// on a genuinely QGIS-shaped document.
#[test]
fn qgis_export_fixture_parses_se_prefixed_mark_and_svg_parameters() {
    let xml = std::fs::read_to_string(Path::new(FIXTURE_DIR).join("qgis_point_export.sld"))
        .expect("fixture should exist");
    let sld = terraserve::sld::parse(&xml).expect("fixture should parse");
    let rule = &sld.named_layers[0].styles[0].feature_type_styles[0].rules[0];
    match &rule.symbolizers[0] {
        Symbolizer::Point(point) => {
            let mark = &point.graphic.marks[0];
            assert_eq!(mark.well_known_name, "circle");
            let fill = mark.fill.as_ref().expect("mark should have a fill");
            assert_eq!(fill.color.as_deref(), Some("#b2afc4"));
            let stroke = mark.stroke.as_ref().expect("mark should have a stroke");
            assert_eq!(stroke.color.as_deref(), Some("#000000"));
        }
        other => panic!("expected Symbolizer::Point, got {other:?}"),
    }
}

/// SE 1.1 fixture with a deeply nested filter + scale denominators — the canonical structural
/// snapshot. If this debug rendering ever changes unexpectedly, something in the parse tree walk
/// shifted; review the diff before updating this string.
#[test]
fn se11_scale_filter_fixture_matches_structural_snapshot() {
    let xml = std::fs::read_to_string(Path::new(FIXTURE_DIR).join("se11_point_scale_filter.sld"))
        .expect("fixture should exist");
    let sld = terraserve::sld::parse(&xml).expect("fixture should parse");

    let rule = &sld.named_layers[0].styles[0].feature_type_styles[0].rules[0];
    assert_eq!(rule.min_scale, Some(10_000.0));
    assert_eq!(rule.max_scale, Some(20_000.0));

    let debug = format!("{sld:#?}");
    let expected = include_str!("fixtures/sld/se11_point_scale_filter.debug.snapshot");
    assert_eq!(
        debug.trim_end(),
        expected.trim_end(),
        "parsed structural snapshot for se11_point_scale_filter.sld changed — review the diff \
         (print `format!(\"{sld:#?}\")` for the fixture and compare) before overwriting \
         tests/fixtures/sld/se11_point_scale_filter.debug.snapshot"
    );
}
