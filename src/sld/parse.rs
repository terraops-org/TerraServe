// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! XML → `sld::model` parser, built on `roxmltree`.
//!
//! **Namespace-tolerant by design:** every element/attribute match below is on the *local* name
//! (`node.tag_name().name()`), never a fully-qualified/namespaced name. Real-world SLD documents
//! mix the `sld:`/`se:` prefixes (or none at all — a bare default namespace, or none whatsoever),
//! and this parser accepts all of them uniformly. Unknown/unrecognized elements are silently
//! ignored rather than treated as parse errors — that is deliberate interop tolerance, not an
//! oversight.
//!
//! Scope for this task: `PointSymbolizer` + `TextSymbolizer` fully; `LineSymbolizer` /
//! `PolygonSymbolizer` get a light best-effort parse (fill/stroke only) since the model already
//! has slots for them and the walk is nearly free. A `<Rule>`'s `<ogc:Filter>` (if present) is
//! parsed via `super::filter::parse_filter` (Task 3) into `Rule::filter`.

use roxmltree::Node;

use super::model::*;
use super::xml::{child, children_named, local_name, text};
use super::SldError;

/// Parse an SLD 1.0.0 XML document into the document AST.
pub fn parse(xml: &str) -> Result<StyledLayerDescriptor, SldError> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| SldError(e.to_string()))?;
    let root = doc.root_element();
    if local_name(root) != "StyledLayerDescriptor" {
        return Err(SldError(format!(
            "expected a <StyledLayerDescriptor> root element, found <{}>",
            root.tag_name().name()
        )));
    }

    let named_layers: Vec<NamedLayer> = children_named(root, "NamedLayer")
        .into_iter()
        .map(parse_named_layer)
        .collect();
    // Pragmatic stopgap, NOT an XSD requirement: SLD 1.0 actually allows zero layers
    // (the NamedLayer/UserLayer choice is minOccurs="0"). We reject an empty result because
    // (a) `UserLayer` isn't modeled in the AST yet, so a UserLayer-only doc would silently
    // yield nothing, and (b) a layer-less document carries no style to apply. Revisit when
    // `UserLayer` lands (then accept it, and allow truly metadata-only documents).
    if named_layers.is_empty() {
        return Err(SldError(
            "<StyledLayerDescriptor> has no <NamedLayer> (UserLayer not yet supported)".to_string(),
        ));
    }

    Ok(StyledLayerDescriptor { named_layers })
}

// ---------------------------------------------------------------------------------------------
// Parser-specific helpers. The generic namespace-tolerant roxmltree helpers (`local_name`,
// `children_named`, `child`, `text`) now live in `super::xml` (SIMP-7), shared with `filter`.
// ---------------------------------------------------------------------------------------------

fn parse_f64_child(node: Node, name: &str) -> Option<f64> {
    child(node, name)
        .and_then(text)
        .and_then(|s| s.parse().ok())
}

/// An `ogc:Expression`: a `<ogc:PropertyName>` child, an explicit `<ogc:Literal>` child, or (the
/// common shorthand real SLDs use for scalar params like `<Size>`/`<Rotation>`) the element's own
/// direct text. Real-world fixture `geoserver_point_external_graphic.sld` (GeoServer's `burg.sld`)
/// writes `<Size><ogc:Literal>20</ogc:Literal></Size>` rather than the bare-text `<Size>20</Size>`
/// shorthand — both are spec-legal, so both are accepted here. Mirrors the same
/// prefer-explicit-child-then-fall-back-to-own-text idiom `filter::boundary_text` already uses for
/// `<LowerBoundary>`/`<UpperBoundary>`, kept consistent across the module.
fn expression(node: Node) -> Expression {
    if let Some(prop) = child(node, "PropertyName") {
        Expression::PropertyName(text(prop).unwrap_or_default())
    } else if let Some(lit) = child(node, "Literal") {
        Expression::Literal(text(lit).unwrap_or_default())
    } else {
        Expression::Literal(text(node).unwrap_or_default())
    }
}

/// Parse a `<Label>`'s mixed content into ordered parts (see the design's whitespace rule):
/// `<ogc:PropertyName>` -> `PropertyName`; `<ogc:Literal>` -> `Literal` verbatim (whitespace
/// kept); any other element -> warn + drop; a bare text node -> `Literal` only if it has a
/// non-whitespace char, kept verbatim.
fn parse_label(node: Node) -> Vec<LabelPart> {
    let mut parts = Vec::new();
    for child in node.children() {
        if child.is_element() {
            match local_name(child) {
                "PropertyName" => parts.push(LabelPart::PropertyName(text(child).unwrap_or_default())),
                "Literal" => {
                    // Concatenate ALL text-node children so an interleaved comment/CDATA can't
                    // truncate the literal (`<ogc:Literal>a<!--x-->b</ogc:Literal>` -> "ab");
                    // whitespace kept verbatim. roxmltree `Node::text()` alone returns only the
                    // first text child.
                    let s: String = child
                        .children()
                        .filter(|n| n.is_text())
                        .filter_map(|n| n.text())
                        .collect();
                    parts.push(LabelPart::Literal(s));
                }
                other => eprintln!("sld::parse: <Label> child <{other}> ignored (only PropertyName/Literal supported)"),
            }
        } else if child.is_text() {
            if let Some(raw) = child.text() {
                if raw.chars().any(|c| !c.is_whitespace()) {
                    parts.push(LabelPart::Literal(raw.to_string()));
                }
            }
        }
    }
    parts
}

/// Find a `CssParameter`/`SvgParameter` child (both spellings accepted) with `name="<name>"`.
fn find_css_param<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<Node<'a, 'input>> {
    node.children().find(|c| {
        c.is_element()
            && matches!(local_name(*c), "CssParameter" | "SvgParameter")
            && c.attribute("name") == Some(name)
    })
}

fn css_param_text(node: Node, name: &str) -> Option<String> {
    find_css_param(node, name).and_then(text)
}

fn css_param_expr(node: Node, name: &str) -> Option<Expression> {
    find_css_param(node, name).map(expression)
}

// ---------------------------------------------------------------------------------------------
// Document tree.
// ---------------------------------------------------------------------------------------------

fn parse_named_layer(node: Node) -> NamedLayer {
    NamedLayer {
        name: child(node, "Name").and_then(text),
        styles: children_named(node, "UserStyle")
            .into_iter()
            .map(parse_user_style)
            .collect(),
    }
}

fn parse_user_style(node: Node) -> UserStyle {
    UserStyle {
        name: child(node, "Name").and_then(text),
        feature_type_styles: children_named(node, "FeatureTypeStyle")
            .into_iter()
            .map(parse_feature_type_style)
            .collect(),
    }
}

fn parse_feature_type_style(node: Node) -> FeatureTypeStyle {
    FeatureTypeStyle {
        rules: children_named(node, "Rule")
            .into_iter()
            .map(parse_rule)
            .collect(),
    }
}

fn parse_rule(node: Node) -> Rule {
    Rule {
        name: child(node, "Name").and_then(text),
        title: child(node, "Title").and_then(text),
        // `parse_filter` returns `None` for a missing `<Filter>`, an unrecognized operator, or a
        // recognized-but-malformed one — and a `None` filter means "no gate": the rule matches
        // ALL features (see `Rule.filter`'s doc / `render.rs`'s consumer). So an unparseable
        // filter silently fails OPEN, not closed: a rule the author meant to restrict can end up
        // matching everything instead of nothing. That's a deliberate lenient-interop stance
        // (matches `sld::parse`'s general tolerance of malformed/unknown SLD), but it's a sharp
        // edge — a future fail-closed mode (or at least a warning) would be worth adding.
        filter: child(node, "Filter").and_then(super::filter::parse_filter),
        else_filter: child(node, "ElseFilter").is_some(),
        min_scale: parse_f64_child(node, "MinScaleDenominator"),
        max_scale: parse_f64_child(node, "MaxScaleDenominator"),
        symbolizers: parse_symbolizers(node),
    }
}

/// Walk the `<Rule>`'s direct element children in document order, dispatching on local name.
/// Unrecognized elements (`Filter`, `ElseFilter`, `Name`, vendor extensions, typos, ...) are
/// skipped here — they're either handled above or simply not symbolizers.
fn parse_symbolizers(rule_node: Node) -> Vec<Symbolizer> {
    rule_node
        .children()
        .filter(|c| c.is_element())
        .filter_map(|c| match local_name(c) {
            "PointSymbolizer" => Some(Symbolizer::Point(parse_point_symbolizer(c))),
            "LineSymbolizer" => Some(Symbolizer::Line(parse_line_symbolizer(c))),
            "PolygonSymbolizer" => Some(Symbolizer::Polygon(parse_polygon_symbolizer(c))),
            "TextSymbolizer" => Some(Symbolizer::Text(parse_text_symbolizer(c))),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Point.
// ---------------------------------------------------------------------------------------------

fn parse_point_symbolizer(node: Node) -> PointSymbolizer {
    PointSymbolizer {
        graphic: child(node, "Graphic")
            .map(parse_graphic)
            .unwrap_or(Graphic {
                marks: Vec::new(),
                size: None,
                opacity: None,
            }),
    }
}

fn parse_graphic(node: Node) -> Graphic {
    Graphic {
        marks: children_named(node, "Mark")
            .into_iter()
            .map(parse_mark)
            .collect(),
        size: child(node, "Size").map(expression),
        opacity: child(node, "Opacity").map(expression),
    }
}

fn parse_mark(node: Node) -> Mark {
    Mark {
        // SLD default well-known mark is "square"; we still record whatever is present verbatim.
        well_known_name: child(node, "WellKnownName")
            .and_then(text)
            .unwrap_or_else(|| "square".to_string()),
        fill: child(node, "Fill").map(parse_fill),
        stroke: child(node, "Stroke").map(parse_stroke),
    }
}

fn parse_fill(node: Node) -> Fill {
    Fill {
        color: css_param_text(node, "fill"),
        opacity: css_param_expr(node, "fill-opacity"),
    }
}

fn parse_stroke(node: Node) -> Stroke {
    Stroke {
        color: css_param_text(node, "stroke"),
        width: css_param_expr(node, "stroke-width"),
        opacity: css_param_expr(node, "stroke-opacity"),
    }
}

// ---------------------------------------------------------------------------------------------
// Line / Polygon (best-effort — fill/stroke only; not this task's focus).
// ---------------------------------------------------------------------------------------------

fn parse_line_symbolizer(node: Node) -> LineSymbolizer {
    LineSymbolizer {
        stroke: child(node, "Stroke").map(parse_stroke),
    }
}

fn parse_polygon_symbolizer(node: Node) -> PolygonSymbolizer {
    PolygonSymbolizer {
        fill: child(node, "Fill").map(parse_fill),
        stroke: child(node, "Stroke").map(parse_stroke),
    }
}

// ---------------------------------------------------------------------------------------------
// Text.
// ---------------------------------------------------------------------------------------------

fn parse_text_symbolizer(node: Node) -> TextSymbolizer {
    TextSymbolizer {
        label: child(node, "Label").map(parse_label).unwrap_or_default(),
        font: child(node, "Font").map(parse_font).unwrap_or(Font {
            family: None,
            size: None,
            weight: None,
            style: None,
        }),
        placement: child(node, "LabelPlacement")
            .map(parse_label_placement)
            .unwrap_or_else(default_point_placement),
        halo: child(node, "Halo").map(parse_halo),
        fill: child(node, "Fill").map(parse_fill),
        priority: child(node, "Priority").map(expression),
        vendor: children_named(node, "VendorOption")
            .into_iter()
            .filter_map(|v| {
                v.attribute("name")
                    .map(|n| (n.to_string(), text(v).unwrap_or_default()))
            })
            .collect(),
    }
}

fn parse_font(node: Node) -> Font {
    Font {
        family: css_param_text(node, "font-family"),
        size: css_param_expr(node, "font-size"),
        weight: css_param_text(node, "font-weight"),
        style: css_param_text(node, "font-style"),
    }
}

fn default_point_placement() -> LabelPlacement {
    LabelPlacement::Point(PointPlacement {
        anchor: None,
        displacement: None,
        rotation: None,
    })
}

fn parse_label_placement(node: Node) -> LabelPlacement {
    if let Some(pp) = child(node, "PointPlacement") {
        LabelPlacement::Point(parse_point_placement(pp))
    } else if let Some(lp) = child(node, "LinePlacement") {
        LabelPlacement::Line(parse_line_placement(lp))
    } else {
        default_point_placement()
    }
}

fn parse_point_placement(node: Node) -> PointPlacement {
    PointPlacement {
        anchor: child(node, "AnchorPoint").and_then(parse_xy_pair("AnchorPointX", "AnchorPointY")),
        displacement: child(node, "Displacement")
            .and_then(parse_xy_pair("DisplacementX", "DisplacementY")),
        rotation: child(node, "Rotation").map(expression),
    }
}

fn parse_line_placement(node: Node) -> LinePlacement {
    LinePlacement {
        offset: child(node, "PerpendicularOffset").map(expression),
    }
}

/// Returns a closure that, given a container node (e.g. `<AnchorPoint>`), reads its `x_name`/
/// `y_name` numeric children into `[f64; 2]` — `None` unless both are present and parse as f64.
fn parse_xy_pair(x_name: &'static str, y_name: &'static str) -> impl Fn(Node) -> Option<[f64; 2]> {
    move |node: Node| {
        let x: f64 = child(node, x_name).and_then(text)?.parse().ok()?;
        let y: f64 = child(node, y_name).and_then(text)?.parse().ok()?;
        Some([x, y])
    }
}

fn parse_halo(node: Node) -> Halo {
    Halo {
        radius: child(node, "Radius").map(expression),
        fill: child(node, "Fill").map(parse_fill),
    }
}
