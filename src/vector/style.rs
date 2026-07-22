// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! The vector Style IR + its JSON front-end (spec §9). The IR is the stable core; SLD (rung 7)
//! would parse into the same types. Uses `serde_json` (already a dep for the GeoJSON reader) —
//! the style *semantics* are ours.

use serde_json::Value as J;

use crate::vector::feature::Props;

/// One part of a label template: literal text, or a feature attribute to interpolate.
#[derive(Clone, Debug, PartialEq)]
pub enum LabelPart {
    Literal(String),
    Field(String),
}

/// A label expression: parts concatenated at render time (see `eval_label`).
pub type Label = Vec<LabelPart>;

/// A label-conflict priority value source. `Literal` is a constant; `Field` is a numeric
/// attribute looked up per feature. Direction (which value wins a contested slot) is carried
/// separately by `TextSym.priority_higher_wins`.
#[derive(Clone, Debug, PartialEq)]
pub enum Priority {
    Literal(f64),
    Field(String),
}

/// Evaluate a label template against a feature's props: concatenate each part. `Field` uses
/// `get_display` (a numeric field renders, not blank); an absent field contributes "".
pub fn eval_label(label: &[LabelPart], props: &Props) -> String {
    label
        .iter()
        .map(|part| match part {
            LabelPart::Literal(s) => s.clone(),
            LabelPart::Field(f) => props.get_display(f).unwrap_or_default(),
        })
        .collect()
}

/// Evaluate a priority's raw value against props. `None` when a `Field` is absent/non-numeric.
/// The render folds direction + missing-handling into the placement sort key.
pub fn eval_priority(p: &Priority, props: &Props) -> Option<f64> {
    match p {
        Priority::Literal(n) => Some(*n),
        Priority::Field(f) => props.get_f64(f),
    }
}

#[derive(Clone, Debug)]
pub struct PointSym {
    pub radius: f32,
    pub fill: [u8; 4],
    pub stroke: [u8; 4],
    pub stroke_width: f32,
}

#[derive(Clone, Debug)]
pub struct TextSym {
    pub label: Label,
    pub priority: Option<Priority>,
    /// True for SLD-sourced priorities (OGC `<Priority>`: higher value = more important). False
    /// for the JSON shim's NE `scalerank` convention (lower = more important). The render negates
    /// the raw value when true, so both feed the engine's ascending (lower-first) placement sort.
    pub priority_higher_wins: bool,
    pub size: f32,
    pub color: [u8; 4],
    pub halo_color: [u8; 4],
    pub halo_radius: f32,
    pub offset: f32,
}

#[derive(Clone, Debug)]
pub struct PolygonSym {
    pub fill: [u8; 4],
    pub stroke: Option<[u8; 4]>,
    pub stroke_width: f32,
}

#[derive(Clone, Debug)]
pub struct LineSym {
    pub stroke: [u8; 4],
    pub stroke_width: f32,
}

// ---------------------------------------------------------------------------------------------
// Rule-based Style IR (spec §9): SLD's model is rule-based, so the renderer's Style IR is
// `Style { feature_type_styles: Vec<FeatureTypeStyle> }`, each FTS holding `Vec<Rule>`, each
// `Rule` gated by an optional `Filter` + scale range + an `else_filter` fallback flag, holding a
// list of `Symbolizer`s. `render.rs` composites each FTS as its own z-ordered pass. `sld_lower`
// preserves the SLD document's FeatureTypeStyle grouping; the JSON front-end
// (`Style::from_json_str`) produces a single FTS with one plain rule.
// ---------------------------------------------------------------------------------------------

/// A symbolizer attached to a `Rule`. `Line`/`Polygon` are carried by the Style IR (Task 4) but not
/// yet drawn by `render.rs` — that lands in a later task.
#[derive(Clone, Debug)]
pub enum Symbolizer {
    Point(PointSym),
    Text(TextSym),
    Line(LineSym),
    Polygon(PolygonSym),
}

/// One rule: an optional filter + scale-denominator gate, and the symbolizers it applies when
/// the gate passes.
#[derive(Clone, Debug)]
pub struct Rule {
    pub filter: Option<Filter>,
    /// `<ElseFilter/>`: this rule is a **fallback** — it applies only to features that no
    /// non-else rule (active at the same scale) matched. Never selected while a normal rule
    /// matches. Mirrors `sld::Rule.else_filter`.
    pub else_filter: bool,
    pub min_scale: Option<f64>,
    pub max_scale: Option<f64>,
    pub symbolizers: Vec<Symbolizer>,
    /// Human-readable label for a legend swatch (the SLD `<Title>`, falling back to `<Name>`);
    /// `None` for the JSON one-rule shim. Used by vector GetLegendGraphic.
    pub title: Option<String>,
}

/// One `<FeatureTypeStyle>` — a z-ordered group of rules. Rendered as its own pass: geometry
/// (fills/strokes) composites in document order so a later FTS draws over an earlier one; markers
/// and labels across ALL FeatureTypeStyles are collected and placed by one global pass (see
/// `render.rs`), a documented deviation from strict SLD that keeps label decluttering global.
#[derive(Clone, Debug)]
pub struct FeatureTypeStyle {
    pub rules: Vec<Rule>,
}

/// A rule-based style: an ordered list of `FeatureTypeStyle`s (spec §9). The JSON front-end
/// (`from_json_str`) produces a single FTS with one plain rule; `sld_lower` preserves the SLD
/// document's FeatureTypeStyle grouping so per-FTS z-ordering is honored.
#[derive(Clone, Debug)]
pub struct Style {
    pub feature_type_styles: Vec<FeatureTypeStyle>,
}

/// Comparison operator for `Filter::Cmp`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

/// A feature filter, evaluated against a feature's `Props` (spec §9 / OGC Filter Encoding
/// semantics, kept lenient like SLD comparisons: numeric if both sides parse as `f64`, string
/// otherwise).
#[derive(Clone, Debug)]
pub enum Filter {
    /// `Cmp(op, property, literal)`.
    Cmp(Cmp, String, String),
    /// `Between(property, lower, upper)` — inclusive.
    Between(String, String, String),
    /// `Like(property, pattern)` — SLD wildcard: `%` = any run of characters, `_` = any single
    /// character (the classic SQL/SLD pair; case-sensitive), `\` escapes the next pattern char as
    /// a literal (so `\%`/`\_`/`\\` match a literal `%`/`_`/`\`). The pattern here is always
    /// already in this canonical convention — `sld::filter::parse_like` normalizes a source SLD
    /// document's own `wildCard`/`singleChar`/`escapeChar` delimiters into it before this type is
    /// ever constructed (see `sld_lower::lower_filter`).
    Like(String, String),
    /// True when the property is absent from the feature's `Props`.
    IsNull(String),
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Not(Box<Filter>),
}

impl Filter {
    pub fn eval(&self, props: &crate::vector::feature::Props) -> bool {
        match self {
            Filter::Cmp(op, prop, value) => cmp_eval(*op, props, prop, value),
            Filter::Between(prop, lo, hi) => {
                match (props.get_f64(prop), lo.parse::<f64>(), hi.parse::<f64>()) {
                    (Some(v), Ok(lo), Ok(hi)) => v >= lo && v <= hi,
                    _ => match props.get_str(prop) {
                        Some(s) => s >= lo.as_str() && s <= hi.as_str(),
                        None => false,
                    },
                }
            }
            Filter::Like(prop, pattern) => match props.get_str(prop) {
                Some(s) => like_match(s, pattern),
                None => false,
            },
            Filter::IsNull(prop) => props.get_f64(prop).is_none() && props.get_str(prop).is_none(),
            Filter::And(fs) => fs.iter().all(|f| f.eval(props)),
            Filter::Or(fs) => fs.iter().any(|f| f.eval(props)),
            Filter::Not(f) => !f.eval(props),
        }
    }
}

/// Lenient comparison: numeric if BOTH the feature's property value and the filter's literal
/// parse as `f64`; otherwise string comparison via `get_str`. A property that is absent from
/// the feature (neither numeric nor string) never satisfies a `Cmp`.
fn cmp_eval(op: Cmp, props: &crate::vector::feature::Props, prop: &str, value: &str) -> bool {
    if let (Some(a), Ok(b)) = (props.get_f64(prop), value.parse::<f64>()) {
        return apply_cmp(op, a.partial_cmp(&b));
    }
    if let Some(a) = props.get_str(prop) {
        return apply_cmp(op, Some(a.cmp(value)));
    }
    false
}

fn apply_cmp(op: Cmp, ord: Option<std::cmp::Ordering>) -> bool {
    use std::cmp::Ordering::*;
    match (op, ord) {
        (Cmp::Eq, Some(Equal)) => true,
        (Cmp::Ne, Some(o)) => o != Equal,
        (Cmp::Ne, None) => true,
        (Cmp::Lt, Some(Less)) => true,
        (Cmp::Gt, Some(Greater)) => true,
        (Cmp::Le, Some(Less)) | (Cmp::Le, Some(Equal)) => true,
        (Cmp::Ge, Some(Greater)) | (Cmp::Ge, Some(Equal)) => true,
        _ => false,
    }
}

/// SLD `PropertyIsLike` wildcard match: `%` = any run of characters (incl. empty), `_` = any
/// single character, `\` escapes the next pattern char as a literal (see `Filter::Like` doc).
/// Case-sensitive.
///
/// Iterative two-pointer matcher with `%`-backtracking (SEC-2): worst case O(text × pattern), no
/// recursion and no exponential blow-up. The previous recursive `like_rec` branched into two calls
/// per `%` with no memoization, so an operator-authored pattern like `%a%a…%b` against a
/// non-matching string ran in exponential time — a self-inflicted per-request GetMap DoS.
fn like_match(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (n, m) = (t.len(), p.len());
    let (mut ti, mut pi) = (0usize, 0usize);
    // Most recent `%` to rewind to: the pattern index just past it, and the text index we
    // re-advance from on each backtrack.
    let mut star_pi: Option<usize> = None;
    let mut star_ti = 0usize;
    while ti < n {
        if pi < m && p[pi] == '%' {
            // Greedy `%`: try consuming zero chars first; the mismatch path below rewinds here and
            // lets it swallow one more char per backtrack. Only the latest `%` need be remembered,
            // which is what makes this O(text × pattern) rather than the old exponential recursion.
            star_pi = Some(pi + 1);
            star_ti = ti;
            pi += 1;
            continue;
        }
        // Does the current pattern token match `t[ti]`? `_` matches any single char; a `\x` escape
        // and a plain char match that literal; a trailing `\` (nothing after) is a literal `\`.
        let escaped = pi < m && p[pi] == '\\' && pi + 1 < m;
        let tok_matches = if escaped {
            t[ti] == p[pi + 1]
        } else {
            match p.get(pi) {
                Some('_') => true,
                Some(&c) => c == t[ti],
                None => false, // pattern exhausted but text remains → only a `%` rewind can save it
            }
        };
        if tok_matches {
            ti += 1;
            pi += if escaped { 2 } else { 1 };
        } else if let Some(sp) = star_pi {
            pi = sp;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    // Text fully consumed: whatever pattern remains must be only `%` for a complete match.
    while pi < m && p[pi] == '%' {
        pi += 1;
    }
    pi == m
}

impl Style {
    /// Load a vector style from disk, dispatching on file type: SLD documents route through
    /// `crate::sld::parse` + `crate::vector::sld_lower::lower`; everything else falls back to
    /// the JSON vec-style front-end (`from_json_str`). SLD detection is `.sld`-extension OR
    /// content sniffing (leading `<?xml` / `<StyledLayerDescriptor`, tolerant of a leading BOM
    /// or whitespace) — the belt-and-suspenders match handles a `.sld` file that some producers
    /// emit without an XML prolog, and an XML document saved under a non-`.sld` extension.
    pub fn load(path: &str) -> Result<Style, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
        if path.ends_with(".sld") || looks_like_sld(&text) {
            let doc = crate::sld::parse(&text).map_err(|e| format!("{path}: {}", e.0))?;
            crate::vector::sld_lower::lower(doc).map_err(|e| format!("{path}: {e}"))
        } else {
            Style::from_json_str(&text).map_err(|e| format!("{path}: {e}"))
        }
    }

    /// Parse the simple vec-style JSON into a **one-rule** `Style` — the shim that feeds today's
    /// hand-written JSON style through the rule-based IR (a real multi-rule front-end is SLD, via
    /// `sld_lower`). The single rule is a *plain* rule (no filter, no scale gate, not an else):
    /// `render.rs` recognizes this shape and applies its auto-declutter.
    ///
    /// The JSON may carry a `point`+`text` pair (both required together, exactly as before — this
    /// is the byte-identical point path the airport goldens depend on) and/or a `polygon` and/or a
    /// `line` object; whichever are present are collected into the rule's `symbolizers`, in the
    /// order Point, Text, Polygon, Line. At least one symbolizer must be present.
    pub fn from_json_str(text: &str) -> Result<Style, String> {
        let symbolizers = parse_symbolizers_json(text)?;
        Ok(Style {
            feature_type_styles: vec![FeatureTypeStyle {
                rules: vec![Rule {
                    filter: None,
                    else_filter: false,
                    min_scale: None,
                    max_scale: None,
                    symbolizers,
                    title: None,
                }],
            }],
        })
    }

    /// All rules across all FeatureTypeStyles in document order — the flattened view for global
    /// concerns (`primary_label`, the plain-shim declutter detection) that don't need FTS grouping.
    pub fn all_rules(&self) -> impl Iterator<Item = &Rule> {
        self.feature_type_styles.iter().flat_map(|f| &f.rules)
    }

    /// The primary label template: the `label` of the first `Text` symbolizer across all rules
    /// (document order). Used by GetFeatureInfo to render the display string. `None` when the
    /// style has no text symbolizer.
    pub fn primary_label(&self) -> Option<&Label> {
        self.all_rules()
            .flat_map(|r| &r.symbolizers)
            .find_map(|s| match s {
                Symbolizer::Text(t) => Some(&t.label),
                _ => None,
            })
    }
}

/// Parse the vec-style JSON into the rule's `symbolizers` list. `point`+`text` are a pair — if
/// either key is present the other is required too (exactly today's `parse_point_text` contract,
/// preserved so the point path stays byte-identical), pushed in the order Point, Text. `polygon`
/// and `line` are each independently optional, pushed (when present) after Point/Text in the
/// order Polygon, Line. At least one symbolizer must end up present, or this errors.
fn parse_symbolizers_json(text: &str) -> Result<Vec<Symbolizer>, String> {
    let j: J = serde_json::from_str(text).map_err(|e| format!("json: {e}"))?;
    let mut symbolizers = Vec::new();

    if j.get("point").is_some() || j.get("text").is_some() {
        let (point, text) = parse_point_text(&j)?;
        symbolizers.push(Symbolizer::Point(point));
        symbolizers.push(Symbolizer::Text(text));
    }
    if let Some(poly) = j.get("polygon") {
        symbolizers.push(Symbolizer::Polygon(parse_polygon(poly)));
    }
    if let Some(line) = j.get("line") {
        symbolizers.push(Symbolizer::Line(parse_line(line)));
    }

    if symbolizers.is_empty() {
        return Err(
            "style JSON must contain at least one of `point`+`text`, `polygon`, `line`".to_string(),
        );
    }
    Ok(symbolizers)
}

/// Parse the `{point, text}` vec-style JSON into a `(PointSym, TextSym)` pair. Shared by the
/// JSON front-end (`parse_symbolizers_json`); the same field defaults `sld_lower` mirrors.
fn parse_point_text(j: &J) -> Result<(PointSym, TextSym), String> {
    let pt = j.get("point").ok_or("missing `point`")?;
    let tx = j.get("text").ok_or("missing `text`")?;
    let point = PointSym {
        radius: num(pt, "radius").unwrap_or(3.0) as f32,
        fill: rgba(pt, "fill").unwrap_or([30, 30, 30, 255]),
        stroke: rgba(pt, "stroke").unwrap_or([255, 255, 255, 255]),
        stroke_width: num(pt, "stroke_width").unwrap_or(1.0) as f32,
    };
    let halo = tx.get("halo");
    let text = TextSym {
        label: vec![LabelPart::Field(
            tx.get("label")
                .and_then(|v| v.as_str())
                .ok_or("text.label required")?
                .to_string(),
        )],
        priority: tx
            .get("priority")
            .and_then(|v| v.as_str())
            .map(|s| Priority::Field(s.to_string())),
        priority_higher_wins: false,
        size: num(tx, "size").unwrap_or(13.0) as f32,
        color: rgba(tx, "color").unwrap_or([20, 20, 20, 255]),
        halo_color: halo
            .and_then(|h| rgba(h, "color"))
            .unwrap_or([255, 255, 255, 230]),
        halo_radius: halo.and_then(|h| num(h, "radius")).unwrap_or(2.0).max(0.0) as f32,
        // A negative offset would put every candidate over the own marker → all labels drop.
        offset: num(tx, "offset").unwrap_or(4.0).max(0.0) as f32,
    };
    Ok((point, text))
}

/// Parse a `polygon` object (`{fill, stroke?, stroke_width}`) into a `PolygonSym`. `stroke` is
/// optional (absent → `None`, an unstroked fill); `fill`/`stroke_width` default like the other
/// symbolizers' JSON shim (mid-grey fill, 1.0 stroke width) when absent.
fn parse_polygon(j: &J) -> PolygonSym {
    PolygonSym {
        fill: rgba(j, "fill").unwrap_or([128, 128, 128, 255]),
        stroke: rgba(j, "stroke"),
        stroke_width: num(j, "stroke_width").unwrap_or(1.0) as f32,
    }
}

/// Parse a `line` object (`{stroke, stroke_width}`) into a `LineSym`.
fn parse_line(j: &J) -> LineSym {
    LineSym {
        stroke: rgba(j, "stroke").unwrap_or([0, 0, 0, 255]),
        stroke_width: num(j, "stroke_width").unwrap_or(1.0) as f32,
    }
}

/// Content-sniff for an SLD document: trims a leading UTF-8 BOM and whitespace, then checks for
/// an XML prolog (`<?xml`) or a bare `<StyledLayerDescriptor` root element (case-sensitive per
/// the spec's element name).
fn looks_like_sld(text: &str) -> bool {
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    trimmed.starts_with("<?xml") || trimmed.starts_with("<StyledLayerDescriptor")
}

fn num(j: &J, k: &str) -> Option<f64> {
    j.get(k).and_then(|v| v.as_f64())
}

fn rgba(j: &J, k: &str) -> Option<[u8; 4]> {
    let a = j.get(k)?.as_array()?;
    if a.len() < 3 {
        return None;
    }
    let g = |i: usize| -> u8 { a.get(i).and_then(|v| v.as_u64()).unwrap_or(0).min(255) as u8 };
    Some([
        g(0),
        g(1),
        g(2),
        a.get(3)
            .and_then(|v| v.as_u64())
            .map(|x| x.min(255) as u8)
            .unwrap_or(255),
    ])
}
