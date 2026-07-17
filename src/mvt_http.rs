// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! `/mvt` HTTP front-end (Task 5) — bespoke MVT tiles over XYZ addressing (`{z}/{x}/{y}.pbf`, TOP-LEFT
//! row like WMTS/the core, no y-flip) plus a TileJSON 3.0.0 document. A thin adapter over
//! `vector::mvt::encode_tile`: resolve `{layer}` to a `VectorLayer`, resolve `{tms}` to a preset grid
//! (the MVT-fixed 4096-unit local coordinate grid — see `vector::mvt::tile::EXTENT`), range-check
//! `z/x/y`, then defer emptiness (no features / everything clipped away) to the encoder itself — an
//! IN-RANGE tile with no data is a valid 200 with an empty body (the MVT convention). Only an unknown
//! layer/grid or an out-of-range tile is a 4xx.

use std::collections::BTreeMap;

use crate::server::{Layer, ServeState, VectorLayer};
use crate::tms::{self, TileMatrixSet};
use crate::vector::feature::Value;
use crate::vector::mvt::{encode_tile_opt, features_for_tile, MvtOptimizations};

/// Resolve `{layer}` to its `VectorLayer`. `Err((404,_))` for an unknown layer, `Err((400,_))` when
/// the named layer exists but is raster-only (no `FeatureSource`) — MVT only applies to vector layers.
fn resolve_vector<'a>(
    state: &'a ServeState,
    layer: &str,
) -> Result<(&'a Layer, &'a VectorLayer), (u16, String)> {
    let l = state
        .layers
        .iter()
        .find(|l| l.name == layer)
        .ok_or((404u16, format!("no layer '{layer}'")))?;
    let v = l.vector.as_ref().ok_or((
        400u16,
        format!("layer '{layer}' is not a vector layer — MVT requires --vector"),
    ))?;
    Ok((l, v))
}

/// Resolve `{tms}` to a grid — the MVT tile-grid preset at the encoder's 4096-unit local extent (an
/// explicit `_{px}` size suffix in the id still overrides, per `tms::preset`'s R3 rule).
/// `Err((404,_))` on an unknown base id.
fn resolve_grid(tms_id: &str) -> Result<TileMatrixSet, (u16, String)> {
    tms::preset(tms_id, 4096).ok_or((404u16, format!("no TileMatrixSet '{tms_id}'")))
}

/// Render one MVT tile: `{layer}/{tms}/{z}/{x}/{y}`. Out-of-range `z/x/y` -> `Err((404,_))`;
/// in-range with no features (or everything clipped away by the encoder) -> `Ok` with an empty body.
pub fn render_mvt_tile(
    state: &ServeState,
    layer: &str,
    tms_id: &str,
    z: u32,
    x: u32,
    y: u32,
) -> Result<Vec<u8>, (u16, String)> {
    let (l, v) = resolve_vector(state, layer)?;
    let grid = resolve_grid(tms_id)?;
    let lvl = grid
        .level(z)
        .ok_or((404u16, format!("no zoom level {z}")))?;
    if x >= lvl.matrix_w || y >= lvl.matrix_h {
        return Err((404, format!("tile {z}/{x}/{y} out of range")));
    }
    // Write-through overlay (Spec 2): checks overlay index then its owned base; a miss falls
    // through to live encode + persist. When there's no overlay, the Spec-1 base check below runs
    // unchanged. Supersedes Spec-1 `l.pmtiles` when present (the loader sets at most one of the
    // two, but the overlay path is checked first regardless).
    if let Some(ov) = &l.overlay {
        match ov.get(z, x, y) {
            Ok(Some(bytes)) => return Ok(bytes),
            Ok(None) => {}
            Err(e) => eprintln!("overlay read {z}/{x}/{y}: {e}"),
        }
        let opts = MvtOptimizations::for_layer(state, v);
        let vs = v.source_for_zoom(z);
        let batch = features_for_tile(&vs, &grid, z, x, y, &l.src_crs);
        let live = encode_tile_opt(batch.as_slice(), &grid, z, x, y, &l.src_crs, &l.name, &opts);
        if !live.is_empty() && !ov.is_compacting() {
            let id = crate::vector::pmtiles::zxy_to_tileid(z, x, y);
            let _ = ov.put(id, &crate::vector::pmtiles::codec::gzip(&live)); // best-effort
        }
        return Ok(live);
    }
    // Archive-first (opt-in): a hit is served straight from the pre-built PMTiles; a miss (or no
    // archive) falls through to the live encode path below. The reader returns raw (decompressed)
    // MVT, the same shape as the live path.
    if let Some(reader) = &l.pmtiles {
        match reader.get(z, x, y) {
            Ok(Some(bytes)) => return Ok(bytes),
            Ok(None) => {}
            Err(e) => eprintln!("pmtiles read {z}/{x}/{y}: {e}"), // degrade to live encode
        }
    }
    // The optimization set for this layer — built ONCE from the layer's precomputed `area_scale`
    // (the encoder derives the per-zoom threshold from `z`), so the WMTS GetTile route produces
    // identical bytes with no duplicated derivation.
    let opts = MvtOptimizations::for_layer(state, v);
    // Per-zoom LOD: pick the zoom-appropriate pool (light at low zoom) if the layer has one.
    let vs = v.source_for_zoom(z);
    // Reads through the `VectorSource` seam (windowed-seam refactor): reproject the tile bbox into
    // the source CRS (`features_for_tile`) BEFORE reading, so a future windowed source's window is
    // correct — a harmless no-op for `LoadAll` (encode_tile_opt still runs its own candidate filter
    // over whatever slice it's handed).
    let batch = features_for_tile(&vs, &grid, z, x, y, &l.src_crs);
    Ok(cached_or_encode(state, &l.name, tms_id, z, x, y, || {
        encode_tile_opt(batch.as_slice(), &grid, z, x, y, &l.src_crs, &l.name, &opts)
    }))
}

/// Build a bounded byte-cache of `String → Arc<Vec<u8>>` sized in **MiB** (`--mvt-cache` /
/// `--wms-cache`). Weighed by byte length (a dissolved MVT tile or a WMS PNG can be multi-MB), so RSS
/// stays hard-bounded — mirrors the raster `--cache-lru` MiB semantics (Fable-5 review #1: an
/// entry-count bound let 512 × multi-MB ≈ 1.4 GB).
pub fn build_byte_cache(max_mib: u64) -> moka::sync::Cache<String, std::sync::Arc<Vec<u8>>> {
    moka::sync::Cache::builder()
        .max_capacity(max_mib.saturating_mul(1024 * 1024))
        .weigher(|_k, v: &std::sync::Arc<Vec<u8>>| v.len().min(u32::MAX as usize) as u32)
        .build()
}

/// Serve `encode()`'s bytes via the MVT cache when enabled — computed once per `layer/tms/z/x/y`
/// (the encode is a pure function of that key + the fixed-per-run opts), with `get_with`
/// single-flight so a cold (e.g. dissolved low-zoom) tile isn't recomputed N times under a burst.
/// Shared by the `/mvt` XYZ + WMTS GetTile routes.
pub(crate) fn cached_or_encode(
    state: &ServeState,
    layer: &str,
    tms_id: &str,
    z: u32,
    x: u32,
    y: u32,
    encode: impl FnOnce() -> Vec<u8>,
) -> Vec<u8> {
    match &state.mvt_cache {
        Some(cache) => {
            let key = format!("{layer}/{tms_id}/{z}/{x}/{y}");
            (*cache.get_with(key, || std::sync::Arc::new(encode()))).clone()
        }
        None => encode(),
    }
}

/// A TileJSON 3.0.0 document for `{layer}` on `{tms}`. `tiles` is an ABSOLUTE URL template derived
/// from the advertised `base_url` — the same `…/wms` -> origin split the TMS/WMTS front-ends use.
pub fn tilejson_doc(
    state: &ServeState,
    layer: &str,
    tms_id: &str,
    request_host: Option<&str>,
) -> Result<String, (u16, String)> {
    let (l, v) = resolve_vector(state, layer)?;
    let grid = resolve_grid(tms_id)?;
    let minzoom = grid.levels.iter().map(|lv| lv.z).min().unwrap_or(0);
    let maxzoom = grid.levels.iter().map(|lv| lv.z).max().unwrap_or(0);
    let origin = advertised_origin(state, request_host);
    let tile_url = format!("{origin}/mvt/{layer}/{tms_id}/{{z}}/{{x}}/{{y}}.pbf");

    // Attribute schema is precomputed once at layer load (see `feature_field_schema`); reading it
    // here keeps TileJSON O(1) instead of re-scanning all features on every request.
    let doc = serde_json::json!({
        "tilejson": "3.0.0",
        "tiles": [tile_url],
        "minzoom": minzoom,
        "maxzoom": maxzoom,
        "bounds": l.bounds_wgs84.to_vec(),
        "vector_layers": [
            { "id": layer, "fields": &v.fields }
        ],
    });
    Ok(doc.to_string())
}

/// The metadata JSON embedded in a generated `.pmtiles` archive (Task 6) — a minimal TileJSON 3.0
/// object carrying the layer's `vector_layers` attribute schema, mirroring the `vector_layers` shape
/// `tilejson_doc` serves so a PMTiles client sees the same layer id + typed fields. Unlike
/// `tilejson_doc` there is no live `tiles` URL (the archive IS the tiles), so only the layer-level
/// metadata travels. A raster layer yields an empty `fields` map.
pub fn pmtiles_metadata_json(layer: &Layer) -> String {
    let fields = layer
        .vector
        .as_ref()
        .map(|v| v.fields.clone())
        .unwrap_or_default();
    let doc = serde_json::json!({
        "tilejson": "3.0.0",
        "name": layer.name,
        "vector_layers": [
            { "id": layer.name, "fields": fields }
        ],
    });
    doc.to_string()
}

/// The absolute origin (`scheme://host[:port]`) to embed in advertised URLs. Prefers the request's
/// `Host` header (the address the client actually reached us on) so URLs are reachable even when the
/// server binds `0.0.0.0` (whose literal address is not routable from another machine). Falls back
/// to the configured `base_url` (e.g. an explicit `--public-url`) when there's no Host header.
fn advertised_origin(state: &ServeState, request_host: Option<&str>) -> String {
    match request_host {
        Some(h) if !h.is_empty() => format!("http://{h}"),
        _ => state
            .base_url
            .strip_suffix("/wms")
            .unwrap_or(&state.base_url)
            .trim_end_matches('/')
            .to_string(),
    }
}

/// A **MapLibre/Mapbox GL Style JSON** (`version: 8`) for `{layer}` — the "one URL" a client
/// (QGIS's *Style URL* field, MapLibre GL, the X-ray viewer) points at to get both the source and
/// its styling. The `sources` entry references the layer's WebMercatorQuad TileJSON (MapLibre is
/// web-mercator only, so the grid is fixed); the `layers` are a generic **X-ray** treatment —
/// glowing cyan outline + faint fill + point discs — that renders ANY geometry type (polygons,
/// lines, points), independent of the layer's server-side `--vec-style`. `source-layer` is the MVT
/// layer name (== the served layer's name). Returns `Err((404/400,_))` for an unknown/raster layer.
/// Derive a MapLibre-GL `fill` layer from the vector layer's Style IR (its `--vec-style` SLD/JSON): a
/// per-class `["match", ["get", FIELD], value, colour, …, default]` fill-color built from the rules
/// that select `FIELD = value` and carry a Polygon fill — i.e. the SAME class palette the WMS renders.
/// `None` unless there is a single-field class→colour mapping (COS-style SLDs qualify; range/function
/// filters don't). Lets the X-ray viewer's "Use WMS style" colour vector tiles from the one SLD.
fn sld_class_fill_layer(
    layer_id: &str,
    style: &crate::vector::style::Style,
) -> Option<serde_json::Value> {
    use crate::vector::style::{Cmp, Filter, Symbolizer};
    let hex = |c: [u8; 4]| {
        format!(
            "rgba({},{},{},{:.3})",
            c[0],
            c[1],
            c[2],
            c[3] as f64 / 255.0
        )
    };
    let mut field: Option<String> = None;
    let mut stops: Vec<(String, String)> = Vec::new();
    let mut default_col: Option<String> = None;
    for fts in &style.feature_type_styles {
        for rule in &fts.rules {
            let Some(fill) = rule.symbolizers.iter().find_map(|s| match s {
                Symbolizer::Polygon(p) => Some(p.fill),
                _ => None,
            }) else {
                continue;
            };
            let col = hex(fill);
            match &rule.filter {
                Some(Filter::Cmp(Cmp::Eq, prop, val)) => {
                    match &field {
                        None => field = Some(prop.clone()),
                        Some(f) if f != prop => continue, // single-field mapping only
                        _ => {}
                    }
                    stops.push((val.clone(), col));
                }
                None if rule.else_filter => default_col = Some(col),
                _ => {} // non-equality / non-else rule: not part of the class map
            }
        }
    }
    let field = field?;
    if stops.is_empty() {
        return None;
    }
    let mut m: Vec<serde_json::Value> = vec![
        serde_json::json!("match"),
        serde_json::json!(["get", field]),
    ];
    for (v, c) in stops {
        m.push(serde_json::json!(v));
        m.push(serde_json::json!(c));
    }
    m.push(serde_json::json!(
        default_col.unwrap_or_else(|| "#cccccc".into())
    ));
    Some(serde_json::json!({
        "id": format!("{layer_id}-wms"),
        "type": "fill",
        "paint": { "fill-color": serde_json::Value::Array(m) }
    }))
}

pub fn style_json(
    state: &ServeState,
    layer: &str,
    request_host: Option<&str>,
) -> Result<String, (u16, String)> {
    // Validate: the layer must exist and be a vector layer (MVT/style only applies to vectors).
    let (_, v) = resolve_vector(state, layer)?;
    let origin = advertised_origin(state, request_host);
    let source_url = format!("{origin}/mvt/{layer}/WebMercatorQuad.json");

    // An operator-supplied `--mvt-style` (a JSON object `{ "layers": [...], "metadata": {...} }`,
    // or a bare `[...]` layer array) if present; otherwise the generic X-ray default. This is how a
    // thematic style (e.g. the DGT COS2018 land-cover legend) is served without the engine knowing
    // the classification — the `metadata` (e.g. a legend) rides along to the client.
    let (raw_layers, metadata) = match &state.mvt_style {
        Some(serde_json::Value::Array(arr)) => (arr.clone(), serde_json::Value::Null),
        Some(serde_json::Value::Object(obj)) => (
            obj.get("layers")
                .and_then(|l| l.as_array())
                .cloned()
                .unwrap_or_default(),
            obj.get("metadata")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        ),
        // No `--mvt-style`: derive a class-colour fill from the layer's `--vec-style` SLD/JSON (the
        // same palette the WMS renders) so the X-ray viewer's "Use WMS style" can colour the vector
        // tiles from the one SLD; fall back to the generic X-ray line default when there's no
        // single-field class→colour mapping.
        _ => match sld_class_fill_layer(layer, &v.style) {
            Some(fill) => (vec![fill], serde_json::Value::Null),
            None => (xray_default_layers(layer), serde_json::Value::Null),
        },
    };
    // Inject the source binding onto every layer (the operator provides only paint/filter/type/id).
    let layers: Vec<serde_json::Value> = raw_layers
        .into_iter()
        .map(|mut o| {
            if let Some(m) = o.as_object_mut() {
                m.entry("source")
                    .or_insert_with(|| serde_json::json!("terraserve"));
                m.entry("source-layer")
                    .or_insert_with(|| serde_json::json!(layer));
            }
            o
        })
        .collect();

    let mut doc = serde_json::json!({
        "version": 8,
        "name": format!("TerraServe — {layer}"),
        "sources": {
            "terraserve": { "type": "vector", "url": source_url }
        },
        "layers": layers,
    });
    if !metadata.is_null() {
        doc.as_object_mut()
            .unwrap()
            .insert("metadata".to_string(), metadata);
    }
    Ok(doc.to_string())
}

/// The generic **X-ray** layer set (used when no `--mvt-style` is supplied): glowing cyan outline +
/// faint fill + point discs, rendering ANY geometry type. Each layer is gated by geometry type
/// (`$type`) so it only draws its natural geometry — crucially, the circle layers must NOT fire on
/// polygons/lines, or a client (QGIS) renders a marker at each polygon's centroid (an unwanted dot).
fn xray_default_layers(layer: &str) -> Vec<serde_json::Value> {
    let cyan = "#00e5ff";
    let glow = "rgba(0, 229, 255, 0.25)";
    serde_json::json!([
        { "id": "fill", "type": "fill", "source": "terraserve", "source-layer": layer,
          "filter": ["==", "$type", "Polygon"],
          "paint": { "fill-color": cyan, "fill-opacity": 0.05 } },
        { "id": "line-glow", "type": "line", "source": "terraserve", "source-layer": layer,
          "filter": ["!=", "$type", "Point"],
          "layout": { "line-cap": "round", "line-join": "round" },
          "paint": { "line-color": glow, "line-width": 3.0 } },
        { "id": "line", "type": "line", "source": "terraserve", "source-layer": layer,
          "filter": ["!=", "$type", "Point"],
          "layout": { "line-cap": "round", "line-join": "round" },
          "paint": { "line-color": cyan, "line-width": 1.0 } },
        { "id": "point-glow", "type": "circle", "source": "terraserve", "source-layer": layer,
          "filter": ["==", "$type", "Point"],
          "paint": { "circle-color": glow, "circle-radius": 6.0 } },
        { "id": "point", "type": "circle", "source": "terraserve", "source-layer": layer,
          "filter": ["==", "$type", "Point"],
          "paint": { "circle-color": cyan, "circle-radius": 2.5 } }
    ])
    .as_array()
    .unwrap()
    .clone()
}

/// The TileJSON attribute schema for a feature source: distinct property keys typed String|Number
/// (first non-null value seen wins the type; a key seen only as Null is skipped, same as the
/// encoder's own dedup in `vector::mvt::tile::encode_tile`). `BTreeMap` keeps the field order
/// deterministic. Computed ONCE at layer load and cached on `VectorLayer::fields` — this is an
/// O(all features × props) scan, ~1.6 s at BUPi's 3.4M-feature scale, so it must not run per request.
///
/// Kept taking a bare `&dyn FeatureSource` — many test fixtures build a `VectorLayer` directly from
/// a concrete load-all source and call this with it. `feature_field_schema_vs` below is the
/// `VectorSource`-seam-aware twin used by the real layer-build path (`lib.rs::build_vector_layer`);
/// both funnel through `feature_field_schema_slice`, so they're byte-identical for a load-all source.
pub fn feature_field_schema(
    source: &dyn crate::vector::source::FeatureSource,
) -> BTreeMap<String, String> {
    feature_field_schema_slice(source.features())
}

/// Reading through the `VectorSource` seam (windowed-seam refactor, the FlatGeoBuf plan's Task 1),
/// dispatching on the load-all/windowed split:
/// - `LoadAll` — same whole-slice scan as `feature_field_schema` above (`full_extent()` on a
///   `LoadAll` source just borrows the already-resident slice, no extra cost), byte-identical
///   output.
/// - `Windowed` — delegates to `WindowedSource::field_schema`, which answers from cheap source
///   metadata (e.g. `FgbSource` reads the FlatGeoBuf Header's already-parsed `columns()`) and
///   never decodes a feature. Scanning every feature of a multi-million-feature windowed layer
///   just to list field names/types was exactly the 5.8 GB windowed-layer-setup bug this avoids —
///   see `WindowedSource::field_schema`'s doc comment.
pub fn feature_field_schema_vs(
    source: &crate::vector::source::VectorSource,
) -> BTreeMap<String, String> {
    match source {
        crate::vector::source::VectorSource::LoadAll(_) => {
            let batch = source.features_in(source.full_extent());
            feature_field_schema_slice(batch.as_slice())
        }
        crate::vector::source::VectorSource::Windowed(w) => w.field_schema(),
    }
}

fn feature_field_schema_slice(
    feats: &[crate::vector::feature::Feature],
) -> BTreeMap<String, String> {
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    for f in feats {
        for (k, val) in f.props.iter() {
            let ty = match val {
                Value::Str(_) => "String",
                Value::Num(_) => "Number",
                Value::Null => continue,
            };
            fields.entry(k.clone()).or_insert_with(|| ty.to_string());
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::sld_class_fill_layer;
    use crate::vector::style::{
        Cmp, FeatureTypeStyle, Filter, PolygonSym, Rule, Style, Symbolizer,
    };

    fn poly_rule(val: Option<&str>, fill: [u8; 4]) -> Rule {
        Rule {
            filter: val.map(|v| Filter::Cmp(Cmp::Eq, "COS".into(), v.into())),
            else_filter: val.is_none(),
            min_scale: None,
            max_scale: None,
            symbolizers: vec![Symbolizer::Polygon(PolygonSym {
                fill,
                stroke: None,
                stroke_width: 0.0,
            })],
            title: None,
        }
    }

    #[test]
    fn sld_class_fill_derives_match_from_polygon_rules() {
        let style = Style {
            feature_type_styles: vec![FeatureTypeStyle {
                rules: vec![
                    poly_rule(Some("1"), [255, 0, 0, 255]),
                    poly_rule(Some("2"), [0, 0, 255, 255]),
                    poly_rule(None, [128, 128, 128, 255]), // <ElseFilter/> -> default colour
                ],
            }],
        };
        let layer = sld_class_fill_layer("cos", &style).expect("class fill derived");
        let fc = &layer["paint"]["fill-color"];
        // ["match", ["get","COS"], "1", rgba(255,0,0,1), "2", rgba(0,0,255,1), rgba(128,128,128,1)]
        assert_eq!(fc[0], "match");
        assert_eq!(fc[1], serde_json::json!(["get", "COS"]));
        assert_eq!(fc[2], "1");
        assert_eq!(fc[3], "rgba(255,0,0,1.000)");
        assert_eq!(fc[4], "2");
        assert_eq!(fc[5], "rgba(0,0,255,1.000)");
        assert_eq!(fc[6], "rgba(128,128,128,1.000)"); // default is the last element
    }

    #[test]
    fn sld_class_fill_none_without_equality_polygon_rules() {
        let style = Style {
            feature_type_styles: vec![FeatureTypeStyle { rules: vec![] }],
        };
        assert!(sld_class_fill_layer("cos", &style).is_none());
    }
}
