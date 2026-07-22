// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! The vector+label pipeline (spec §5.1): source → cull → forward-project → shape → place →
//! draw → RGBA8. Viewport-global placement (one WMS GetMap image → no tile seams). Returns a
//! flat RGBA8 buffer (`w*h*4`), exactly like the raster path — ready for `pngio::encode_rgba`.
//!
//! Styling is **rule-based** (`Style`, spec §9): per request an OGC scale denominator gates which
//! rules are active; per feature the symbolizers are the **union of ALL active non-else rules
//! whose filter matches** (OGC SLD 1.0 §11.4 — rules within a FeatureTypeStyle are additive, not
//! first-match), so one feature can emit several markers + labels (`Point` → marker, `Text` →
//! label). Only when NO non-else rule matches does the first active `<ElseFilter/>` rule (the
//! fallback) apply. A feature that no rule styles is skipped entirely.

use super::draw::Canvas;
use super::feature::{Feature, Geometry, Props};
use super::geom::{bbox_overlaps, source_filter_bbox, Projector};
use super::place::{self, LabelItem};
use super::raster::GeomLayer;
use super::shape::Shaper;
use super::source::{FeatureSource, VectorSource};
use super::style::{LineSym, PointSym, PolygonSym, Priority, Rule, Style, Symbolizer, TextSym};

/// OGC scale denominator (0.28 mm/px rule) for a GetMap request: grid-CRS resolution × metres per
/// unit / 0.00028. Reuses `tms::meters_per_unit` for the geographic-vs-projected factor.
pub(crate) fn request_scale_denominator(bbox: [f64; 4], width: u32, grid_crs: &str) -> f64 {
    let res_grid = (bbox[2] - bbox[0]).abs() / width.max(1) as f64;
    res_grid * crate::tms::meters_per_unit(grid_crs) / 0.00028
}

/// A rule is active at `scale` when `min_scale <= scale < max_scale` (each bound open where `None`).
fn rule_active(rule: &Rule, scale: f64) -> bool {
    rule.min_scale.is_none_or(|mn| scale >= mn) && rule.max_scale.is_none_or(|mx| scale < mx)
}

/// The rules that style a feature under OGC SLD 1.0 **additive** semantics (§11.4): **every active
/// non-else rule whose filter matches** (a `None` filter always matches), in document order — the
/// feature gets the UNION of all their symbolizers. Only if NO non-else rule matched is the
/// **first active else rule** (`<ElseFilter/>`, the fallback) returned instead — a single rule,
/// never combined with the matched set. An empty result → no rule styles the feature, so it is
/// skipped entirely (marker + label).
///
/// Returns a small `Vec` of borrows (typically 1 element; the JSON one-rule shim is exactly 1,
/// which keeps its golden byte-identical). Push order == document order, which the render loop
/// relies on for its "first matching rule" priority choice.
fn select_rules<'a>(rules: &'a [Rule], props: &Props, scale: f64) -> Vec<&'a Rule> {
    let mut matched: Vec<&Rule> = Vec::new();
    let mut fallback: Option<&Rule> = None;
    for r in rules {
        if !rule_active(r, scale) {
            continue;
        }
        if r.else_filter {
            if fallback.is_none() {
                fallback = Some(r);
            }
            continue;
        }
        if r.filter.as_ref().is_none_or(|f| f.eval(props)) {
            matched.push(r);
        }
    }
    if matched.is_empty() {
        fallback.into_iter().collect()
    } else {
        matched
    }
}

/// First `Point` symbolizer of a rule (the marker), if any.
fn point_of(rule: &Rule) -> Option<&PointSym> {
    rule.symbolizers.iter().find_map(|s| match s {
        Symbolizer::Point(p) => Some(p),
        _ => None,
    })
}

/// First `Text` symbolizer of a rule (the label), if any.
fn text_of(rule: &Rule) -> Option<&TextSym> {
    rule.symbolizers.iter().find_map(|s| match s {
        Symbolizer::Text(t) => Some(t),
        _ => None,
    })
}

/// All `Polygon` symbolizers of a rule, in document order (T7 — a rule may carry several, each
/// composited in order).
fn polygons_of(rule: &Rule) -> impl Iterator<Item = &PolygonSym> {
    rule.symbolizers.iter().filter_map(|s| match s {
        Symbolizer::Polygon(p) => Some(p),
        _ => None,
    })
}

/// All `Line` symbolizers of a rule, in document order (T7 — e.g. road casing: a wide dark stroke
/// under a narrow light one, both drawn).
fn lines_of(rule: &Rule) -> impl Iterator<Item = &LineSym> {
    rule.symbolizers.iter().filter_map(|s| match s {
        Symbolizer::Line(l) => Some(l),
        _ => None,
    })
}

/// Project every vertex of `rings` (`Projector::to_pixel`, dropping any vertex whose transform
/// fails or lands non-finite — a degenerate vertex, not a hard error), then cull the WHOLE
/// geometry if its projected bounding box doesn't intersect the canvas+margin viewport (mirrors
/// the point path's viewport-global cull). Shared by both `Polygon`/`MultiPolygon` (each ring
/// list — exterior + holes) and `LineString`/`MultiLineString` (each a "ring" with no fill
/// semantics, just an open line) since both end up as a `Vec<Vec<[f32;2]>>` — exactly
/// `GeomLayer::fill_polygon`/`stroke_lines`'s input shape. `None` when culled (fully outside) or
/// when every vertex failed to project.
///
/// **KNOWN LIMITATION (MVP):** an individual unprojectable vertex is DROPPED, not clipped. For a
/// point that's clean omission; for a polygon/line it shortens the ring in place, so a geometry
/// straddling the projection domain (EPSG:3857 |lat| > ~85.06°, or an antimeridian-crossing ring)
/// renders a straight "short-cut" across the dropped span. Not reachable by the in-domain fixtures
/// (goldens unaffected), but reachable panning world data in web-mercator. Proper fix is domain
/// clipping (Sutherland–Hodgman) rather than vertex-drop — a filed follow-up.
fn project_and_cull(
    proj: &Projector,
    rings: &[Vec<[f64; 2]>],
    w: f32,
    h: f32,
    margin: f32,
) -> Option<Vec<Vec<[f32; 2]>>> {
    let (mut minx, mut miny, mut maxx, mut maxy) = (
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    );
    let out: Vec<Vec<[f32; 2]>> = rings
        .iter()
        .map(|ring| {
            ring.iter()
                .filter_map(|&[lon, lat]| {
                    let (px, py) = proj.to_pixel(lon, lat)?;
                    if !px.is_finite() || !py.is_finite() {
                        return None;
                    }
                    minx = minx.min(px);
                    maxx = maxx.max(px);
                    miny = miny.min(py);
                    maxy = maxy.max(py);
                    Some([px, py])
                })
                .collect()
        })
        .collect();
    if maxx < -margin || minx > w + margin || maxy < -margin || miny > h + margin {
        return None; // whole geometry fully outside the viewport (incl. every vertex unprojectable)
    }
    Some(out)
}

/// A representative anchor point (source CRS) for a feature's marker/label. A Point returns itself;
/// a Polygon/MultiPolygon the area-weighted centroid of its (largest polygon's) exterior ring — a
/// good anchor for the blob-like land-cover / cadastre polygons (a pole-of-inaccessibility point
/// would be more robust for strongly concave shapes — a follow-up); a Line its middle vertex. `None`
/// if the geometry has no usable vertices. This is what lets a `TextSymbolizer`/`PointSymbolizer`
/// label a polygon or line, not just a point.
fn representative_point(geom: &Geometry) -> Option<[f64; 2]> {
    fn vertex_mean(pts: &[[f64; 2]]) -> Option<[f64; 2]> {
        if pts.is_empty() {
            return None;
        }
        let (mut sx, mut sy) = (0.0f64, 0.0f64);
        for p in pts {
            sx += p[0];
            sy += p[1];
        }
        let n = pts.len() as f64;
        Some([sx / n, sy / n])
    }
    fn ring_centroid(ring: &[[f64; 2]]) -> Option<[f64; 2]> {
        if ring.len() < 3 {
            return vertex_mean(ring);
        }
        let (mut a2, mut cx, mut cy) = (0.0f64, 0.0f64, 0.0f64);
        let n = ring.len();
        for i in 0..n {
            let p = ring[i];
            let q = ring[(i + 1) % n]; // %n closes the ring (a closed ring's last edge is zero-area)
            let cross = p[0] * q[1] - q[0] * p[1];
            a2 += cross;
            cx += (p[0] + q[0]) * cross;
            cy += (p[1] + q[1]) * cross;
        }
        if a2.abs() < 1e-12 {
            return vertex_mean(ring); // degenerate (zero-area) — fall back to the mean
        }
        Some([cx / (3.0 * a2), cy / (3.0 * a2)])
    }
    // For a MultiPolygon, anchor on the LARGEST part (by vertex count — cheap proxy for area) so the
    // label sits on the dominant piece, not a sliver.
    fn largest_exterior(polys: &[Vec<Vec<[f64; 2]>>]) -> Option<&Vec<[f64; 2]>> {
        polys
            .iter()
            .filter_map(|p| p.first())
            .max_by_key(|r| r.len())
    }
    match geom {
        Geometry::Point(p) => Some(*p),
        Geometry::Polygon(rings) => rings.first().and_then(|r| ring_centroid(r)),
        Geometry::MultiPolygon(polys) => largest_exterior(polys).and_then(|r| ring_centroid(r)),
        Geometry::LineString(l) => l.get(l.len() / 2).copied(),
        Geometry::MultiLineString(parts) => parts
            .iter()
            .max_by_key(|l| l.len())
            .and_then(|l| l.get(l.len() / 2).copied()),
    }
}

/// Renders from a `FeatureSource` borrow — kept for existing callers (many test fixtures build a
/// `VectorLayer` directly from a concrete load-all source). `render_vector_from` below is the
/// `VectorSource`-seam-aware entry point used by the WMS GetMap vector request path; both funnel
/// through `render_vector_impl`, so the two are byte-identical for a load-all source.
#[allow(clippy::too_many_arguments)]
pub fn render_vector(
    src: &dyn FeatureSource,
    style: &Style,
    src_crs: &str,
    grid_crs: &str,
    bbox: [f64; 4],
    width: u32,
    height: u32,
    shaper: &Shaper,
) -> Result<Vec<u8>, String> {
    render_vector_impl(
        src.features(),
        style,
        src_crs,
        grid_crs,
        bbox,
        width,
        height,
        shaper,
    )
}

/// Same pipeline, reading through the `VectorSource` seam (windowed-seam refactor, the FlatGeoBuf
/// plan's Task 1): `LoadAll` borrows the whole slice via `features_in` — byte-identical to
/// `render_vector` above; a future `Windowed` source (e.g. FlatGeoBuf) would instead fetch only the
/// request `bbox` window here, with zero further change to this pipeline. This is
/// `wms::get_map_vector`'s entry point (the production WMS GetMap vector path).
#[allow(clippy::too_many_arguments)]
pub fn render_vector_from(
    src: &VectorSource,
    style: &Style,
    src_crs: &str,
    grid_crs: &str,
    bbox: [f64; 4],
    width: u32,
    height: u32,
    shaper: &Shaper,
) -> Result<Vec<u8>, String> {
    // CRS fix: `bbox` is in `grid_crs` (the request/grid CRS), but `features_in` expects the layer's
    // source CRS — reproject via the same `source_filter_bbox` helper the per-feature pre-filter
    // below uses (`None` → fall back to the whole extent, fail-open, matching the pre-filter's own
    // fallback). A harmless no-op for `LoadAll` (the `Borrowed` arm ignores its bbox argument
    // entirely); this is what makes the query bbox correct once a `Windowed` source (FlatGeoBuf)
    // lands here.
    let query_bbox =
        source_filter_bbox(grid_crs, src_crs, bbox).unwrap_or_else(|| src.full_extent());
    let batch = src.features_in(query_bbox);
    render_vector_impl(
        batch.as_slice(),
        style,
        src_crs,
        grid_crs,
        bbox,
        width,
        height,
        shaper,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_vector_impl(
    feats: &[Feature],
    style: &Style,
    src_crs: &str,
    grid_crs: &str,
    bbox: [f64; 4],
    width: u32,
    height: u32,
    shaper: &Shaper,
) -> Result<Vec<u8>, String> {
    let proj = Projector::new(src_crs, grid_crs, bbox, width, height)?;
    let w = width as f32;
    let h = height as f32;
    // Cheap per-feature bbox pre-filter (same as the MVT path): the request footprint reprojected
    // into the source CRS, so a feature whose source-CRS bbox misses it is skipped BEFORE the
    // expensive per-vertex projection. `None` (transform unavailable) → no filter (project all).
    // This is the O(all-features) → O(in-view) win for a large layer (COS 780k / BUPi 3.4M).
    let filt = source_filter_bbox(grid_crs, src_crs, bbox);

    // OGC scale denominator for this request — gates which rules are active.
    let scale = request_scale_denominator(bbox, width, grid_crs);

    // The JSON one-rule shim: a single *plain* rule (no filter, no scale gate, not an else). ONLY
    // this shape gets the automatic scalerank declutter below; real rules/scales/filters govern
    // themselves (no auto-declutter).
    // Flattened rule view for the two global concerns (plain-shim detection + the max-size fold)
    // that don't care about FeatureTypeStyle grouping.
    let all_rules: Vec<&Rule> = style.all_rules().collect();
    let plain_shim = match all_rules.as_slice() {
        [r] if r.filter.is_none()
            && r.min_scale.is_none()
            && r.max_scale.is_none()
            && !r.else_filter =>
        {
            Some(*r)
        }
        _ => None,
    };

    // Keep just-off-canvas features whose marker/label may intrude; scale the cull margin with the
    // largest label size so a wide label can still reach the canvas from off-screen (viewport-global
    // margin). The per-label offset is now carried on each `LabelItem` (set from the SLD
    // `<Displacement>` at lower time), so the placement kernel no longer takes a global offset.
    let max_text_size = style
        .all_rules()
        .flat_map(|r| &r.symbolizers)
        .filter_map(|s| match s {
            Symbolizer::Text(t) => Some(t.size),
            _ => None,
        })
        .fold(0.0f32, |ms, s| ms.max(s));
    let margin = (max_text_size * 40.0).max(512.0);

    // Geometry pass (draw order: BOTTOM layer, under markers+labels): for every non-Point
    // feature, every matching rule's Polygon/Line symbolizer is drawn into a `GeomLayer` —
    // `Polygon`/`MultiPolygon` → `fill_polygon` (fill + its own optional outline stroke, in one
    // call — "per-feature order: fills first"); `LineString`/`MultiLineString` → `stroke_lines`.
    // A geometry/symbolizer type mismatch (e.g. a Line symbolizer on a Polygon feature) is simply
    // ignored — `polygons_of`/`lines_of` only look at symbolizer *kind*, but the `match` below only
    // ever calls into a rule's Polygon symbolizer for Polygon/MultiPolygon geometry (Line
    // symbolizer likewise gated to LineString/MultiLineString), so a mismatched pair never draws.
    // Lazily allocated: if no rule ever contributes a Polygon/Line symbolizer that actually draws
    // something, `geom_layer` stays `None` and the `Canvas` below starts fully transparent EXACTLY
    // as before this task — the point goldens' byte-identity depends on this staying vacuous for a
    // point-only style.
    let mut geom_layer: Option<GeomLayer> = None;
    // Per-FeatureTypeStyle z-order: each FTS composites its geometry in document order (later FTS on
    // top); select_rules is scoped to the FTS so ElseFilter is per-FTS.
    for fts in &style.feature_type_styles {
        for f in feats {
            if let Some(fb) = filt {
                if !bbox_overlaps(f.bbox, fb) {
                    continue; // out of the request footprint — skip before projecting
                }
            }
            if matches!(f.geom, Geometry::Point(_)) {
                continue; // points are handled entirely by the marker/label loop below
            }
            // Additive rule selection within this FTS (OGC SLD 1.0 §11.4); no priority/scale declutter
            // here — that shim is a point/label-only concept (Text symbolizer priority).
            let rules = select_rules(&fts.rules, &f.props, scale);
            for &rule in &rules {
                match &f.geom {
                    // T7: every Polygon symbolizer of the rule fills, in document order (project
                    // once, then composite each). `syms` empty → geometry pass stays vacuous, so a
                    // point-only style keeps `geom_layer` None (golden byte-identity).
                    Geometry::Polygon(rings) => {
                        let syms: Vec<&PolygonSym> = polygons_of(rule).collect();
                        if !syms.is_empty() {
                            if let Some(proj_rings) = project_and_cull(&proj, rings, w, h, margin) {
                                let layer =
                                    geom_layer.get_or_insert_with(|| GeomLayer::new(width, height));
                                for sym in syms {
                                    layer.fill_polygon(&proj_rings, sym);
                                }
                            }
                        }
                    }
                    Geometry::MultiPolygon(polys) => {
                        let syms: Vec<&PolygonSym> = polygons_of(rule).collect();
                        if !syms.is_empty() {
                            for rings in polys {
                                if let Some(proj_rings) =
                                    project_and_cull(&proj, rings, w, h, margin)
                                {
                                    let layer = geom_layer
                                        .get_or_insert_with(|| GeomLayer::new(width, height));
                                    for &sym in &syms {
                                        layer.fill_polygon(&proj_rings, sym);
                                    }
                                }
                            }
                        }
                    }
                    // T7: every Line symbolizer strokes, in document order (road casing = a wide
                    // dark stroke then a narrow light one on top).
                    Geometry::LineString(pts) => {
                        let syms: Vec<&LineSym> = lines_of(rule).collect();
                        if !syms.is_empty() {
                            if let Some(proj_lines) =
                                project_and_cull(&proj, std::slice::from_ref(pts), w, h, margin)
                            {
                                let layer =
                                    geom_layer.get_or_insert_with(|| GeomLayer::new(width, height));
                                for sym in syms {
                                    layer.stroke_lines(&proj_lines, sym.stroke, sym.stroke_width);
                                }
                            }
                        }
                    }
                    Geometry::MultiLineString(lines) => {
                        let syms: Vec<&LineSym> = lines_of(rule).collect();
                        if !syms.is_empty() {
                            if let Some(proj_lines) = project_and_cull(&proj, lines, w, h, margin) {
                                let layer =
                                    geom_layer.get_or_insert_with(|| GeomLayer::new(width, height));
                                for sym in syms {
                                    layer.stroke_lines(&proj_lines, sym.stroke, sym.stroke_width);
                                }
                            }
                        }
                    }
                    Geometry::Point(_) => unreachable!("filtered out above"),
                }
            }
        }
    }
    let mut canvas = match geom_layer {
        Some(layer) => Canvas::from_rgba(width, height, &layer.into_straight_rgba()),
        None => Canvas::transparent(width, height),
    };

    // Scale-based declutter (SHIM ONLY): at low zoom a dense marker field makes every label collide
    // → all labels drop. Derive an effective web-mercator zoom from the map resolution and hide
    // features whose priority value exceeds a zoom-derived threshold (NE `scalerank`: lower = more
    // important), so zoomed-out views show only the important features + their labels. Real rules
    // use their own scale denominators instead of this automatic default.
    // `res_grid` is grid-CRS units per px; `res_m` converts it to metres via `meters_per_unit` —
    // now one source of truth with `request_scale_denominator` (111_319.49 m/° for geographic CRSs
    // — EPSG:4326 / CRS:84 / OGC:CRS84 / urn forms — and 1.0 for projected). NB SIMP-1 is a small
    // *behavior* change on this shim-declutter path, not a pure refactor: vs the old hard-coded
    // 111_320.0 the constant shifts ~5e-6 (nudges `max_priority` by 1 only where `eff_zoom` lands
    // within ~7e-6 of a half-integer), and OGC:CRS84/urn are now correctly geographic (the old
    // `.contains("4326")` test read them as projected). Projected grids — incl. every EPSG:3857
    // golden — are unaffected (×1.0 either way); the EPSG:4326 shim path is pinned by
    // `airports_wgs84_512.png` in tests/vector_golden.rs.
    let res_grid = (bbox[2] - bbox[0]).abs() / width.max(1) as f64;
    let res_m = res_grid * crate::tms::meters_per_unit(grid_crs);
    let eff_zoom = (156_543.03 / res_m.max(1e-6)).log2();
    let shim_priority_armed = plain_shim
        .and_then(text_of)
        .is_some_and(|t| matches!(t.priority, Some(Priority::Field(_))) && !t.priority_higher_wins);
    let max_priority = if shim_priority_armed {
        (eff_zoom + 1.0).round() // more features admitted as you zoom in
    } else {
        f64::INFINITY
    };

    let mut markers: Vec<([f32; 2], PointSym)> = Vec::new();
    let mut items: Vec<LabelItem> = Vec::new();
    // Per-label text symbolizer, aligned index-for-index with `items` (a `Placement.item` indexes
    // both), so each label draws with its own rule's Text style.
    let mut label_styles: Vec<TextSym> = Vec::new();
    // Per-FeatureTypeStyle: collect markers + labels across ALL FTS (per-FTS selection), then place
    // in ONE global pass below so labels never overlap across FTS (documented deviation from SLD).
    for fts in &style.feature_type_styles {
        for f in feats {
            if let Some(fb) = filt {
                if !bbox_overlaps(f.bbox, fb) {
                    continue; // out of the request footprint — skip before projecting
                }
            }
            // Anchor: a Point is itself; a Polygon/Line gets a representative interior/mid point so
            // its label/marker renders (was point-only before). Features with no vertices are skipped.
            let [lon, lat] = match representative_point(&f.geom) {
                Some(p) => p,
                None => continue,
            };
            let (px, py) = match proj.to_pixel(lon, lat) {
                Some(v) => v,
                None => continue,
            };
            if !px.is_finite() || !py.is_finite() {
                continue;
            }
            if px < -margin || px > w + margin || py < -margin || py > h + margin {
                continue;
            }
            // Additive rule selection within this FTS (OGC SLD 1.0 §11.4): ALL active non-else rules
            // whose filter matches; else the single else fallback. Empty → skip marker + label.
            let rules = select_rules(&fts.rules, &f.props, scale);
            if rules.is_empty() {
                continue;
            }

            // Priority drives BOTH the shim declutter gate and the placement order. Under additive
            // selection a feature can match several rules; we take the priority field from the FIRST
            // matching rule's Text symbolizer (document order) and apply that one value to EVERY item
            // this feature emits, so the feature declutters + places as a unit (marker and label never
            // split across a threshold). This is byte-identical to the old single-rule read for the
            // one-rule shim. Missing/absent priority → last (INFINITY). Direction-aware: SLD priorities
            // are higher-wins (negated onto the engine's ascending sort key); the JSON shim's scalerank
            // is already lower-wins (used as-is).
            let priority = match text_of(rules[0])
                .and_then(|t| t.priority.as_ref().map(|p| (p, t.priority_higher_wins)))
            {
                Some((p, higher)) => super::style::eval_priority(p, &f.props)
                    .map(|v| if higher { -v } else { v })
                    .unwrap_or(f64::INFINITY),
                None => f64::INFINITY,
            };
            if plain_shim.is_some() && priority > max_priority {
                continue; // decluttered at this zoom (skip marker + label)
            }

            // Union of symbolizers (T7): each matching rule contributes EVERY Point symbolizer as a
            // marker AND EVERY Text symbolizer as a label item — not just the first of each kind — so a
            // rule with two PointSymbolizers draws two markers, two TextSymbolizers make two labels.
            // `items`/`label_styles` are pushed together, preserving their 1:1 index alignment. The
            // feature's primary marker (first Point of the first matching rule) sizes every label's
            // obstacle box, exactly as before T7 (byte-identical for the one-Point-one-Text shim).
            let marker_r = point_of(rules[0]).map_or(0.0, |p| p.radius + p.stroke_width);
            for &rule in &rules {
                for sym in &rule.symbolizers {
                    match sym {
                        Symbolizer::Point(p) => markers.push(([px, py], p.clone())),
                        Symbolizer::Text(t) => {
                            // `eval_label` (get_display under the hood): a numeric label field (e.g.
                            // `pop_max`, `scalerank`) must still render text, not go blank.
                            let text = super::style::eval_label(&t.label, &f.props);
                            let label = shaper.shape(&text, t.size);
                            items.push(LabelItem {
                                fid: f.fid,
                                priority,
                                anchor: [px, py],
                                marker_r,
                                label,
                                offset: t.offset,
                            });
                            label_styles.push(t.clone());
                        }
                        Symbolizer::Polygon(_) | Symbolizer::Line(_) => {}
                    }
                }
            }
        }
    }

    // Markers are always drawn, under the labels.
    for (pos, sym) in &markers {
        canvas.draw_marker(pos[0], pos[1], sym);
    }

    // Deterministic priority-greedy placement.
    let placements = place::place_labels(&items);
    // Global two-pass: ALL halos first, then ALL bodies — so a later label's halo cannot paint
    // over an earlier label's body. Draw by the placement's item index (never a fid map).
    for p in &placements {
        canvas.draw_label_halo(p.origin, &items[p.item].label, &label_styles[p.item]);
    }
    for p in &placements {
        canvas.draw_label_body(p.origin, &items[p.item].label, &label_styles[p.item]);
    }

    Ok(canvas.into_rgba())
}

#[cfg(test)]
mod tests {
    use super::representative_point;
    use crate::vector::feature::Geometry;

    #[test]
    fn representative_point_of_a_square_is_its_center() {
        let sq = Geometry::Polygon(vec![vec![
            [0.0, 0.0],
            [4.0, 0.0],
            [4.0, 4.0],
            [0.0, 4.0],
            [0.0, 0.0],
        ]]);
        let p = representative_point(&sq).unwrap();
        assert!(
            (p[0] - 2.0).abs() < 1e-9 && (p[1] - 2.0).abs() < 1e-9,
            "square centroid should be (2,2), got {p:?}"
        );
    }

    #[test]
    fn representative_point_of_a_point_is_itself() {
        assert_eq!(
            representative_point(&Geometry::Point([1.5, -3.0])),
            Some([1.5, -3.0])
        );
    }

    #[test]
    fn representative_point_of_a_line_is_a_mid_vertex() {
        let l = Geometry::LineString(vec![[0.0, 0.0], [5.0, 0.0], [10.0, 0.0]]);
        assert_eq!(representative_point(&l), Some([5.0, 0.0]));
    }
}
