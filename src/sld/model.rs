// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! The spec-faithful SLD 1.0.0 document AST.
//!
//! This is a direct, rendering-agnostic transcription of the SLD/SE XML schema shape (not the
//! renderer's own Style IR — see `vector::style::Style`, populated later by `vector::sld_lower`).
//! Kept deliberately dumb: no defaulting, no unit conversion, no color parsing — those are the
//! lowering pass's job (Task 6), so this module stays a faithful mirror of the spec.

/// Root of a parsed SLD document (`<StyledLayerDescriptor>`).
#[derive(Clone, Debug, PartialEq)]
pub struct StyledLayerDescriptor {
    pub named_layers: Vec<NamedLayer>,
}

/// `<NamedLayer>`.
#[derive(Clone, Debug, PartialEq)]
pub struct NamedLayer {
    pub name: Option<String>,
    pub styles: Vec<UserStyle>,
}

/// `<UserStyle>`.
#[derive(Clone, Debug, PartialEq)]
pub struct UserStyle {
    pub name: Option<String>,
    pub feature_type_styles: Vec<FeatureTypeStyle>,
}

/// `<FeatureTypeStyle>`.
#[derive(Clone, Debug, PartialEq)]
pub struct FeatureTypeStyle {
    pub rules: Vec<Rule>,
}

/// `<Rule>`: a filter/scale-gated group of symbolizers.
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub name: Option<String>,
    /// `<Title>` — the human-readable label for this rule (used by GetLegendGraphic).
    pub title: Option<String>,
    /// `<ogc:Filter>`. The `SldFilter` AST + parser (`filter::parse_filter`) live in `filter.rs`.
    pub filter: Option<super::filter::SldFilter>,
    /// `<ElseFilter/>` — true when this rule is the fallback for features no other rule matched.
    pub else_filter: bool,
    pub min_scale: Option<f64>,
    pub max_scale: Option<f64>,
    pub symbolizers: Vec<Symbolizer>,
}

/// A `*Symbolizer` element. Polygon/Line included for spec-faithfulness even though the MVP
/// renderer (rung 1) only lowers Point + Text.
#[derive(Clone, Debug, PartialEq)]
pub enum Symbolizer {
    Point(PointSymbolizer),
    Line(LineSymbolizer),
    Polygon(PolygonSymbolizer),
    Text(TextSymbolizer),
}

/// `<PointSymbolizer>`.
#[derive(Clone, Debug, PartialEq)]
pub struct PointSymbolizer {
    pub graphic: Graphic,
}

/// `<Graphic>`.
#[derive(Clone, Debug, PartialEq)]
pub struct Graphic {
    pub marks: Vec<Mark>,
    pub size: Option<Expression>,
    pub opacity: Option<Expression>,
}

/// `<Mark>`.
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    pub well_known_name: String,
    pub fill: Option<Fill>,
    pub stroke: Option<Stroke>,
}

/// `<TextSymbolizer>`.
#[derive(Clone, Debug, PartialEq)]
pub struct TextSymbolizer {
    pub label: Vec<LabelPart>,
    pub font: Font,
    pub placement: LabelPlacement,
    pub halo: Option<Halo>,
    pub fill: Option<Fill>,
    /// VendorOption `<Priority>` (label-conflict priority), if present.
    pub priority: Option<Expression>,
    /// Other `<VendorOption name="...">value</VendorOption>` pairs, verbatim.
    pub vendor: Vec<(String, String)>,
}

/// `<Font>`.
#[derive(Clone, Debug, PartialEq)]
pub struct Font {
    pub family: Option<String>,
    pub size: Option<Expression>,
    pub weight: Option<String>,
    pub style: Option<String>,
}

/// `<LabelPlacement>`.
#[derive(Clone, Debug, PartialEq)]
pub enum LabelPlacement {
    Point(PointPlacement),
    Line(LinePlacement),
}

/// `<PointPlacement>`.
#[derive(Clone, Debug, PartialEq)]
pub struct PointPlacement {
    pub anchor: Option<[f64; 2]>,
    pub displacement: Option<[f64; 2]>,
    pub rotation: Option<Expression>,
}

/// `<LinePlacement>`.
#[derive(Clone, Debug, PartialEq)]
pub struct LinePlacement {
    pub offset: Option<Expression>,
}

/// `<Halo>`.
#[derive(Clone, Debug, PartialEq)]
pub struct Halo {
    pub radius: Option<Expression>,
    pub fill: Option<Fill>,
}

/// `<Fill>`. `color` is the raw `"#rrggbb"` (or `"#rrggbbaa"`) string — hex parsing is a lowering
/// concern (Task 6), not this AST's.
#[derive(Clone, Debug, PartialEq)]
pub struct Fill {
    pub color: Option<String>,
    pub opacity: Option<Expression>,
}

/// `<Stroke>`.
#[derive(Clone, Debug, PartialEq)]
pub struct Stroke {
    pub color: Option<String>,
    pub width: Option<Expression>,
    pub opacity: Option<Expression>,
}

/// `<LineSymbolizer>`.
#[derive(Clone, Debug, PartialEq)]
pub struct LineSymbolizer {
    pub stroke: Option<Stroke>,
}

/// `<PolygonSymbolizer>`.
#[derive(Clone, Debug, PartialEq)]
pub struct PolygonSymbolizer {
    pub fill: Option<Fill>,
    pub stroke: Option<Stroke>,
}

/// A CSS/SVG parameter or `<ogc:Expression>` value: either a literal or `<ogc:PropertyName>`.
/// Arithmetic/function expressions are out of scope for now (spec allows them; add later).
#[derive(Clone, Debug, PartialEq)]
pub enum Expression {
    Literal(String),
    PropertyName(String),
}

/// A `<Label>`'s content: an ordered concatenation of literal text and `<ogc:PropertyName>`
/// references (OGC SE permits `<Label>` to be a concatenated `ogc:Expression`).
#[derive(Clone, Debug, PartialEq)]
pub enum LabelPart {
    Literal(String),
    PropertyName(String),
}
