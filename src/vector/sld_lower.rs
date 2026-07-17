// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! `sld_lower` — lowers a parsed SLD document (`crate::sld`) into the renderer's Style IR
//! (`crate::vector::style`). This is the interop **firewall**: every SLD-ism — `CssParameter`/
//! `SvgParameter` hex-color strings, `<ogc:Expression>` literals vs. `PropertyName`s, scale
//! denominators, `WellKnownName` markers, `<ogc:Filter>` trees, vendor options — gets resolved to
//! plain IR values *here* and never reaches `render.rs`/`draw.rs`.
//!
//! Scope: `Point`/`Text`/`Line`/`Polygon` symbolizers all lower to Style IR values (Task 5 wired
//! Line/Polygon, which previously lowered to `None` and were dropped). Only the **first**
//! `NamedLayer`'s **first** `UserStyle` is read — that is the shape every fixture in this repo,
//! and every mainstream SLD producer (GeoServer/QGIS single-layer export), uses; multi-layer SLD
//! documents are out of scope until a caller needs them. All `FeatureTypeStyle`s within that
//! `UserStyle` are preserved as `Style.feature_type_styles`, in document order — the renderer
//! composites each FTS as its own z-ordered pass (per-FTS ElseFilter scoping).
//!
//! Defaults mirror the JSON front-end's defaults (`style.rs`, `parse_point_text`/`parse_polygon`/
//! `parse_line`) so a hand-written `.vec.json` style and a lowered SLD produce the same fallback
//! look when a field is absent: point radius 3.0 / fill `#1e1e1e` / stroke white / stroke-width
//! 1.0; text size 13.0 / color `#141414` / halo white-ish (230 alpha) / halo radius 2.0 / offset
//! 4.0; polygon fill mid-grey `#808080` / stroke black `#000000` (only used when a `<Stroke>`
//! element is present but its color is missing) / stroke-width 1.0, with `stroke: None` when the
//! `<PolygonSymbolizer>` has no `<Stroke>` element at all (an unstroked fill — matches
//! `parse_polygon`'s `stroke: rgba(j, "stroke")` with no `.unwrap_or(...)`); line stroke black
//! `#000000` / stroke-width 1.0.

use crate::sld;
use crate::sld::filter::{CompOp, SldFilter};
use crate::vector::style::{
    Cmp, FeatureTypeStyle, Filter, LabelPart, LineSym, PointSym, PolygonSym, Priority, Rule, Style,
    Symbolizer, TextSym,
};

const DEFAULT_POINT_RADIUS: f32 = 3.0;
const DEFAULT_POINT_FILL: [u8; 4] = [30, 30, 30, 255];
const DEFAULT_POINT_STROKE: [u8; 4] = [255, 255, 255, 255];
const DEFAULT_STROKE_WIDTH: f32 = 1.0;

/// Mid-grey fill for a `<PolygonSymbolizer>` with no `<Fill>` element at all — mirrors the JSON
/// front-end's own polygon default (`vector::style::parse_polygon`), so a hand-written `.vec.json`
/// style and a lowered SLD produce the same fallback look when `<Fill>` is absent.
const DEFAULT_POLYGON_FILL: [u8; 4] = [128, 128, 128, 255];
/// Black stroke for a `<PolygonSymbolizer>`/`<LineSymbolizer>` `<Stroke>` element that omits a
/// `stroke` color — same rationale/source as `DEFAULT_POLYGON_FILL` (`parse_polygon`/`parse_line`).
/// Note this only applies when a `<Stroke>` element is *present* but its color is missing/
/// unparseable; a `<PolygonSymbolizer>` with no `<Stroke>` element at all lowers to `stroke: None`
/// (no outline) rather than falling back to this color — see `lower_symbolizer`.
const DEFAULT_STROKE_COLOR: [u8; 4] = [0, 0, 0, 255];

const DEFAULT_TEXT_SIZE: f32 = 13.0;
const DEFAULT_TEXT_COLOR: [u8; 4] = [20, 20, 20, 255];
const DEFAULT_HALO_COLOR: [u8; 4] = [255, 255, 255, 230];
const DEFAULT_HALO_RADIUS: f32 = 2.0;
const DEFAULT_TEXT_OFFSET: f32 = 4.0;

/// Sanity bound for lowered size-like values (`<Size>`'s contribution to point radius,
/// `<Halo><Radius>`, font size, stroke width): mirrors the JSON front-end's
/// `halo_radius`/`offset` `.max(0.0)` clamp (`vector::style::parse_point_text`) — non-negative,
/// because a negative size/radius is nonsensical and (per that front-end's own comment on
/// `offset`) can silently break rendering — and additionally caps an absurd upper bound so a
/// malformed or hostile SLD document (`<Size>999999999</Size>`) can't drive the shaper/draw
/// kernels into an unbounded allocation. 10,000px is comfortably above any real map symbol or
/// label size.
const MAX_SANE_SIZE: f32 = 10_000.0;

/// Clamp a lowered size-like scalar to `[0, MAX_SANE_SIZE]` (see `MAX_SANE_SIZE` doc).
fn clamp_size(v: f32) -> f32 {
    v.clamp(0.0, MAX_SANE_SIZE)
}

/// Lower a parsed SLD document into the renderer's rule-based Style IR.
///
/// `Err` when there are zero rules to lower (empty document, or missing `NamedLayer`/`UserStyle`)
/// — an empty style is never useful to a caller, so this is treated as a hard error rather than
/// silently producing an empty `Style`.
pub fn lower(doc: sld::StyledLayerDescriptor) -> Result<Style, String> {
    if doc.named_layers.len() > 1 {
        eprintln!(
            "sld_lower: {} <NamedLayer>s present; only the first is rendered",
            doc.named_layers.len()
        );
    }
    let feature_type_styles: Vec<FeatureTypeStyle> = doc
        .named_layers
        .first()
        .and_then(|layer| {
            if layer.styles.len() > 1 {
                eprintln!(
                    "sld_lower: {} <UserStyle>s in the first NamedLayer; only the first is rendered",
                    layer.styles.len()
                );
            }
            layer.styles.first()
        })
        .map(|style| {
            // Preserve the document's FeatureTypeStyle grouping (per-FTS z-order in render.rs),
            // rather than flattening every FTS into one rule list.
            style
                .feature_type_styles
                .iter()
                .map(|fts| FeatureTypeStyle {
                    rules: fts.rules.iter().map(lower_rule).collect(),
                })
                .collect()
        })
        .unwrap_or_default();

    if feature_type_styles.iter().all(|fts| fts.rules.is_empty()) {
        return Err(
            "sld_lower: no rules to lower (empty document, or no NamedLayer/UserStyle)".to_string(),
        );
    }
    Ok(Style {
        feature_type_styles,
    })
}

fn lower_rule(rule: &sld::Rule) -> Rule {
    let symbolizers = rule
        .symbolizers
        .iter()
        .filter_map(lower_symbolizer)
        .collect();
    Rule {
        filter: rule.filter.as_ref().map(lower_filter),
        else_filter: rule.else_filter,
        min_scale: rule.min_scale,
        max_scale: rule.max_scale,
        symbolizers,
        // Legend label: prefer <Title>, fall back to <Name> (COS uses Title = "code - class name").
        title: rule.title.clone().or_else(|| rule.name.clone()),
    }
}

fn lower_symbolizer(sym: &sld::Symbolizer) -> Option<Symbolizer> {
    match sym {
        sld::Symbolizer::Point(p) => Some(Symbolizer::Point(lower_point(p))),
        sld::Symbolizer::Text(t) => Some(Symbolizer::Text(lower_text(t))),
        sld::Symbolizer::Line(l) => Some(Symbolizer::Line(lower_line(l))),
        sld::Symbolizer::Polygon(p) => Some(Symbolizer::Polygon(lower_polygon(p))),
    }
}

fn lower_point(p: &sld::PointSymbolizer) -> PointSym {
    let mark = p.graphic.marks.first();
    if let Some(m) = mark {
        if !m.well_known_name.eq_ignore_ascii_case("circle") {
            eprintln!(
                "sld_lower: WellKnownName {:?} not supported, rendering as circle",
                m.well_known_name
            );
        }
    }
    let fill = resolve_fill(mark.and_then(|m| m.fill.as_ref()), DEFAULT_POINT_FILL);
    let (stroke, stroke_width) = resolve_stroke(
        mark.and_then(|m| m.stroke.as_ref()),
        DEFAULT_POINT_STROKE,
        DEFAULT_STROKE_WIDTH,
    );
    warn_if_data_driven(p.graphic.size.as_ref(), "Size");
    let radius = p
        .graphic
        .size
        .as_ref()
        .and_then(expr_to_f32)
        .map(|s| clamp_size(s / 2.0))
        .unwrap_or(DEFAULT_POINT_RADIUS);
    PointSym {
        radius,
        fill,
        stroke,
        stroke_width,
    }
}

fn lower_text(t: &sld::TextSymbolizer) -> TextSym {
    // 1:1 map from the SLD model's mixed-content `<Label>` to the Style IR: a literal text part
    // stays literal text; an `<ogc:PropertyName>` becomes a `Field` lookup. This is what makes a
    // literal `<Label>Airport</Label>` finally render "Airport" instead of a blank field lookup.
    let label: Vec<LabelPart> = t
        .label
        .iter()
        .map(|p| match p {
            sld::LabelPart::Literal(s) => LabelPart::Literal(s.clone()),
            sld::LabelPart::PropertyName(s) => LabelPart::Field(s.clone()),
        })
        .collect();
    warn_if_data_driven(t.font.size.as_ref(), "font Size");
    let size = t
        .font
        .size
        .as_ref()
        .and_then(expr_to_f32)
        .map(clamp_size)
        .unwrap_or(DEFAULT_TEXT_SIZE);
    let color = resolve_fill(t.fill.as_ref(), DEFAULT_TEXT_COLOR);
    let (halo_color, halo_radius) = match &t.halo {
        Some(h) => {
            warn_if_data_driven(h.radius.as_ref(), "Halo Radius");
            (
                resolve_fill(h.fill.as_ref(), DEFAULT_HALO_COLOR),
                h.radius
                    .as_ref()
                    .and_then(expr_to_f32)
                    .map(clamp_size)
                    .unwrap_or(DEFAULT_HALO_RADIUS),
            )
        }
        None => (DEFAULT_HALO_COLOR, DEFAULT_HALO_RADIUS),
    };
    // `<Priority>` wins; else a `<VendorOption name="priority">` field name. `<Priority>` itself
    // distinguishes literal-vs-field (Task 3, `lower_priority_expr`); a vendor option is always a
    // bare field name. SLD priorities are higher-wins (OGC), unlike the JSON shim's scalerank.
    let priority = t.priority.as_ref().map(lower_priority_expr).or_else(|| {
        t.vendor
            .iter()
            .find(|(k, _)| k == "priority")
            .map(|(_, v)| Priority::Field(v.clone()))
    });
    if let sld::LabelPlacement::Line(lp) = &t.placement {
        if lp.offset.is_some() {
            eprintln!(
                "sld_lower: <PerpendicularOffset> parsed but not applied (line-following labels unsupported)"
            );
        }
    }
    // Apply `<PointPlacement><Displacement>` [dx,dy] as the label offset *distance* (magnitude). A
    // zero or absent displacement keeps the default (QGIS/GeoServer emit `<Displacement>0 0</>` as
    // boilerplate). We keep the 8-candidate auto-placement (a documented deviation from SLD's fixed
    // single-position placement); the authored "push the label off the marker" intent is honored via
    // the offset distance. `<PerpendicularOffset>` (line placement) is warned above, not applied.
    let offset = match &t.placement {
        sld::LabelPlacement::Point(pp) => pp
            .displacement
            .map(|[dx, dy]| clamp_size((dx * dx + dy * dy).sqrt() as f32))
            .filter(|&m| m > 0.0)
            .unwrap_or(DEFAULT_TEXT_OFFSET),
        sld::LabelPlacement::Line(_) => DEFAULT_TEXT_OFFSET,
    };
    TextSym {
        label,
        priority,
        priority_higher_wins: true,
        size,
        color,
        halo_color,
        halo_radius,
        offset,
    }
}

/// `<PolygonSymbolizer>` → `PolygonSym`. `fill` resolves via `resolve_fill` against
/// `DEFAULT_POLYGON_FILL`. `stroke` is `None` when the SLD element has no `<Stroke>` at all (an
/// unstroked fill, matching the JSON front-end's `parse_polygon`); when a `<Stroke>` is present,
/// `resolve_stroke` resolves its color (falling back to `DEFAULT_STROKE_COLOR` if the `<Stroke>`
/// is present but its `color` isn't) + width into `Some((color, width))`.
fn lower_polygon(p: &sld::PolygonSymbolizer) -> PolygonSym {
    let fill = resolve_fill(p.fill.as_ref(), DEFAULT_POLYGON_FILL);
    let (stroke, stroke_width) = match &p.stroke {
        Some(s) => {
            let (color, width) =
                resolve_stroke(Some(s), DEFAULT_STROKE_COLOR, DEFAULT_STROKE_WIDTH);
            (Some(color), width)
        }
        None => (None, DEFAULT_STROKE_WIDTH),
    };
    PolygonSym {
        fill,
        stroke,
        stroke_width,
    }
}

/// `<LineSymbolizer>` → `LineSym`. `resolve_stroke` resolves color + width against
/// `DEFAULT_STROKE_COLOR`/`DEFAULT_STROKE_WIDTH` (used verbatim when `<Stroke>` is absent — a
/// `<LineSymbolizer>` with no stroke at all is unusual but not invalid SLD).
fn lower_line(l: &sld::LineSymbolizer) -> LineSym {
    let (stroke, stroke_width) = resolve_stroke(
        l.stroke.as_ref(),
        DEFAULT_STROKE_COLOR,
        DEFAULT_STROKE_WIDTH,
    );
    LineSym {
        stroke,
        stroke_width,
    }
}

/// `SldFilter` → IR `Filter`: a direct structural mapping (comparison ops line up 1:1), no
/// semantic changes. `SldFilter::Like.pattern` is already normalized into the engine's canonical
/// `%`/`_`/`\` convention by `sld::filter::parse_like` (it reads the source document's own
/// `wildCard`/`singleChar`/`escapeChar` attributes and rewrites the pattern at parse time), so
/// this mapping just carries it through unchanged.
fn lower_filter(f: &SldFilter) -> Filter {
    match f {
        SldFilter::Comparison { op, prop, value } => {
            Filter::Cmp(lower_comp_op(*op), prop.clone(), value.clone())
        }
        SldFilter::Between { prop, lo, hi } => {
            Filter::Between(prop.clone(), lo.clone(), hi.clone())
        }
        SldFilter::Like { prop, pattern } => Filter::Like(prop.clone(), pattern.clone()),
        SldFilter::IsNull { prop } => Filter::IsNull(prop.clone()),
        SldFilter::And(items) => Filter::And(items.iter().map(lower_filter).collect()),
        SldFilter::Or(items) => Filter::Or(items.iter().map(lower_filter).collect()),
        SldFilter::Not(inner) => Filter::Not(Box::new(lower_filter(inner))),
    }
}

fn lower_comp_op(op: CompOp) -> Cmp {
    match op {
        CompOp::Eq => Cmp::Eq,
        CompOp::Ne => Cmp::Ne,
        CompOp::Lt => Cmp::Lt,
        CompOp::Gt => Cmp::Gt,
        CompOp::Le => Cmp::Le,
        CompOp::Ge => Cmp::Ge,
    }
}

// -------------------------------------------------------------------------------------------
// Expression / color resolution helpers — the actual "firewall" work.
// -------------------------------------------------------------------------------------------

/// Lower an SLD `<Priority>` expression to the IR `Priority`. A numeric `Literal` -> `Literal(n)`;
/// a non-numeric `Literal` (a bare-text field name like `scalerank`) -> `Field` + warning
/// (preserves the accidental-but-useful field-lookup behavior); a `PropertyName` -> `Field`.
fn lower_priority_expr(expr: &sld::Expression) -> Priority {
    match expr {
        sld::Expression::PropertyName(s) => Priority::Field(s.clone()),
        sld::Expression::Literal(s) => match s.trim().parse::<f64>() {
            Ok(n) => Priority::Literal(n),
            Err(_) => {
                eprintln!(
                    "sld_lower: non-numeric literal <Priority> {s:?} treated as a field name"
                );
                Priority::Field(s.clone())
            }
        },
    }
}

/// Warn when a scalar SLD slot carries a data-driven `<ogc:PropertyName>` the MVP Style IR can't
/// represent — the caller then silently falls back to its default. Keeps `expr_to_f32` pure; this
/// is the load-time visibility for that whole class of dropped constructs.
fn warn_if_data_driven(expr: Option<&sld::Expression>, slot: &str) {
    if let Some(sld::Expression::PropertyName(p)) = expr {
        eprintln!("sld_lower: data-driven <{slot}> ({p:?}) unsupported; using default");
    }
}

/// Scalar (non-label) expressions — font-size, halo radius, mark size, stroke-width, opacity —
/// only resolve from a `Literal` that parses as a number. A `PropertyName` here would mean a
/// data-driven symbol property (e.g. size scaled by an attribute), which the MVP renderer's
/// Style IR has no representation for yet; the caller falls back to its own default in that case.
fn expr_to_f32(expr: &sld::Expression) -> Option<f32> {
    match expr {
        sld::Expression::Literal(s) => s.trim().parse::<f32>().ok(),
        sld::Expression::PropertyName(_) => None,
    }
}

/// `#rgb`, `#rgba`, `#rrggbb`, or `#rrggbbaa` → `[r,g,b,a]` (3-/6-digit bodies carry no alpha,
/// so `a` is `255`). Whitespace-tolerant; anything else (missing `#`, wrong length, non-hex
/// digits) is `None` rather than a panic — the same lenient-interop stance the rest of
/// `sld::parse` takes. `src/sld/model.rs`'s `Fill.color` doc explicitly advertises `#rrggbbaa`
/// support, so an 8-digit body is not just tolerated but a first-class case (CORR-3: it used to
/// return `None` here, which silently discarded the whole color — RGB *and* alpha — in favor of
/// the caller's default).
fn parse_hex_rgba(hex: &str) -> Option<[u8; 4]> {
    let h = hex.trim().strip_prefix('#')?;
    let nibbles = |s: &str| -> Option<Vec<u8>> {
        s.chars().map(|c| c.to_digit(16).map(|d| d as u8)).collect()
    };
    match h.len() {
        3 | 4 => {
            let n = nibbles(h)?;
            let up = |d: u8| d * 16 + d;
            let a = n.get(3).copied().map(up).unwrap_or(255);
            Some([up(n[0]), up(n[1]), up(n[2]), a])
        }
        6 | 8 => {
            let r = u8::from_str_radix(&h[0..2], 16).ok()?;
            let g = u8::from_str_radix(&h[2..4], 16).ok()?;
            let b = u8::from_str_radix(&h[4..6], 16).ok()?;
            let a = if h.len() == 8 {
                u8::from_str_radix(&h[6..8], 16).ok()?
            } else {
                255
            };
            Some([r, g, b, a])
        }
        _ => None,
    }
}

/// Shared color+opacity resolution for `<Fill>`/`<Stroke>` (SIMP-4: `resolve_fill` and
/// `resolve_stroke` used to duplicate this). `color` is a raw hex string (`Fill.color` /
/// `Stroke.color`); `opacity` is the raw `fill-opacity`/`stroke-opacity` expression, if any.
///
/// Alpha precedence (CORR-3): an 8-/4-digit hex carries its own alpha; a separate `opacity`
/// attribute is a *second*, independent [0,1] fraction. Both can be present at once (e.g.
/// `fill="#ff8800cc" fill-opacity="0.5"`), so they're combined multiplicatively — hex-alpha ×
/// opacity — rather than one silently overriding the other. A 3-/6-digit hex (no embedded alpha)
/// behaves exactly as before: alpha comes from `opacity` alone (default 255 when absent).
///
/// `color == None` or unparseable falls back to `default`'s RGB with hex-alpha treated as opaque
/// (255), matching the pre-fix behavior of `resolve_fill`/`resolve_stroke` when only the color
/// (not the whole `<Fill>`/`<Stroke>` element) is missing.
fn resolve_rgba(
    color: Option<&str>,
    opacity: Option<&sld::Expression>,
    default: [u8; 4],
) -> [u8; 4] {
    let (rgb, hex_alpha) = match color.and_then(parse_hex_rgba) {
        Some([r, g, b, a]) => ([r, g, b], a),
        None => ([default[0], default[1], default[2]], 255),
    };
    warn_if_data_driven(opacity, "opacity");
    let opacity_frac = opacity
        .and_then(expr_to_f32)
        .map(|op| op.clamp(0.0, 1.0))
        .unwrap_or(1.0);
    let hex_alpha_frac = hex_alpha as f32 / 255.0;
    let a = (hex_alpha_frac * opacity_frac * 255.0).round() as u8;
    [rgb[0], rgb[1], rgb[2], a]
}

/// Resolve a `<Fill>` (present on `Mark`, `TextSymbolizer`, `Halo`) to `[u8;4]` via
/// `resolve_rgba`. `fill == None` entirely (no `<Fill>` element at all) uses `default` unchanged,
/// alpha included.
fn resolve_fill(fill: Option<&sld::Fill>, default: [u8; 4]) -> [u8; 4] {
    let Some(fill) = fill else {
        return default;
    };
    resolve_rgba(fill.color.as_deref(), fill.opacity.as_ref(), default)
}

/// Same as `resolve_fill` but for `<Stroke>`, which additionally carries `width` →
/// `(color, stroke_width)`. `stroke == None` uses `(default_color, default_width)` unchanged.
/// `width` is clamped via `clamp_size` (Fix C — a negative or absurd stroke-width is as nonsensical
/// as a negative/absurd radius).
fn resolve_stroke(
    stroke: Option<&sld::Stroke>,
    default_color: [u8; 4],
    default_width: f32,
) -> ([u8; 4], f32) {
    let Some(stroke) = stroke else {
        return (default_color, default_width);
    };
    let rgba = resolve_rgba(
        stroke.color.as_deref(),
        stroke.opacity.as_ref(),
        default_color,
    );
    warn_if_data_driven(stroke.width.as_ref(), "Stroke width");
    let width = stroke
        .width
        .as_ref()
        .and_then(expr_to_f32)
        .map(clamp_size)
        .unwrap_or(default_width);
    (rgba, width)
}
