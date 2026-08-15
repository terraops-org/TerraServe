// SPDX-License-Identifier: MPL-2.0
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

/// Resolve `{tms}` to a grid: the layer's OWN published grids first (Task 2's `layer.grids`, e.g. a
/// custom `--grid`/`--config` grid), matched on exact id or on the STORED id's `_{px}` size suffix
/// stripped against the raw request id — mirrors `wmts::get_tile`'s raster grid lookup (wmts.rs:212)
/// EXACTLY (asymmetric: only the stored side is stripped, so an explicit `_{px}` suffix on the
/// request never matches a bare-id stored grid — it falls through to `tms::preset`'s own suffix
/// handling below, per R3). Falls back to the MVT tile-grid preset at the encoder's 4096-unit local
/// extent (an explicit `_{px}` size suffix in the id still overrides, per `tms::preset`'s R3 rule)
/// for the 4 built-ins. `Err((404,_))` when neither resolves.
fn resolve_grid(layer: &Layer, tms_id: &str) -> Result<TileMatrixSet, (u16, String)> {
    if let Some(g) = layer
        .grids
        .iter()
        .find(|g| g.tms.id == tms_id || tms::strip_size_suffix(&g.tms.id) == tms_id)
    {
        return Ok(g.tms.clone());
    }
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
    let grid = resolve_grid(l, tms_id)?;
    let lvl = grid
        .level(z)
        .ok_or((404u16, format!("no zoom level {z}")))?;
    if x >= lvl.matrix_w || y >= lvl.matrix_h {
        return Err((404, format!("tile {z}/{x}/{y} out of range")));
    }
    // Write-through overlay (Spec 2, per-grid as of task 4): selects the overlay for the REQUESTED
    // grid (`tms_id`) — mirrors `l.pmtiles.get(tms_id)` below (design commitment 1: never mix grids
    // in one archive/overlay). Checks that grid's overlay index then its owned base; a miss falls
    // through to live encode + persist into THAT grid's overlay, so a swissLV95 z16/3/2 miss can
    // never land in a WebMercatorQuad overlay even though the two tile ids collide under
    // `zxy_to_tileid`. When no overlay is registered for this grid, the Spec-1 base check below runs
    // unchanged. Supersedes Spec-1 `l.pmtiles.get(tms_id)` when present (the loader populates at most
    // one of the two for a given grid, but the overlay path is checked first regardless).
    if let Some(ov) = l.overlay.get(tms_id) {
        match ov.get(z, x, y) {
            Ok(Some(bytes)) => return Ok(bytes),
            Ok(None) => {}
            Err(e) => eprintln!("overlay read {z}/{x}/{y}: {e}"),
        }
        let opts = MvtOptimizations::for_layer(state, v);
        let vs = v.source_for_zoom(z);
        let batch =
            features_for_tile(&vs, &grid, z, x, y, &l.src_crs, &opts).map_err(read_failed)?;
        let live = encode_tile_opt(batch.as_slice(), &grid, z, x, y, &l.src_crs, &l.name, &opts);
        if !live.is_empty() && !ov.is_compacting() {
            let id = crate::vector::pmtiles::zxy_to_tileid(z, x, y);
            let _ = ov.put(id, &crate::vector::pmtiles::codec::gzip(&live)); // best-effort
        }
        return Ok(live);
    }
    // Archive-first (opt-in): a hit is served straight from the pre-built PMTiles archive for the
    // REQUESTED grid (`tms_id`); a miss (or no archive registered for this grid) falls through to the
    // live encode path below. The reader returns raw (decompressed) MVT, the same shape as the live
    // path. Selecting by `tms_id` rather than "the" archive is what makes per-grid PMTiles work: a
    // layer with e.g. a WebMercatorQuad archive AND a swissLV95 archive serves each grid from its own
    // file (design commitment 1: never mixed in one archive).
    if let Some(reader) = l.pmtiles.get(tms_id) {
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
    let batch = features_for_tile(&vs, &grid, z, x, y, &l.src_crs, &opts).map_err(read_failed)?;
    Ok(cached_or_encode(state, &l.name, tms_id, z, x, y, || {
        encode_tile_opt(batch.as_slice(), &grid, z, x, y, &l.src_crs, &l.name, &opts)
    }))
}

/// A failed source READ is a 500, never an empty tile. Encoding whatever came back from a broken
/// query would emit a valid, empty MVT with a 200 — the silent-blank failure this whole error
/// channel exists to remove. Rendering nothing is a legitimate answer only when the window really
/// is empty, which is `Ok(vec![])`, not `Err`.
fn read_failed(e: String) -> (u16, String) {
    (500, e)
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
    forwarded_proto: Option<&str>,
) -> Result<String, (u16, String)> {
    let (l, v) = resolve_vector(state, layer)?;
    let grid = resolve_grid(l, tms_id)?;
    let minzoom = grid.levels.iter().map(|lv| lv.z).min().unwrap_or(0);
    let maxzoom = grid.levels.iter().map(|lv| lv.z).max().unwrap_or(0);
    let origin = advertised_origin(state, request_host, forwarded_proto);
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
pub fn pmtiles_metadata_json(layer: &Layer, grid_id: Option<&str>) -> String {
    let fields = layer
        .vector
        .as_ref()
        .map(|v| v.fields.clone())
        .unwrap_or_default();
    let mut doc = serde_json::json!({
        "tilejson": "3.0.0",
        "name": layer.name,
        "vector_layers": [
            { "id": layer.name, "fields": fields }
        ],
    });
    // Self-describe the grid this archive's z/x/y belong to (design commitment 2): serve maps
    // `grid_id -> reader` and reads an archive only for matching-grid requests. Absent = WebMercatorQuad.
    if let Some(gid) = grid_id {
        doc["grid_id"] = serde_json::Value::String(gid.to_string());
    }
    doc.to_string()
}

/// The absolute origin (`scheme://host[:port]`) to embed in advertised URLs. Prefers the request's
/// `Host` header (the address the client actually reached us on) so URLs are reachable even when the
/// server binds `0.0.0.0` (whose literal address is not routable from another machine). Falls back
/// to the configured `base_url` (e.g. an explicit `--public-url`) when there's no Host header.
/// Thin delegate to the single shared derivation, `ServeState::advertised_origin`. Kept as a
/// named function because this module's call sites and its regression tests read through it.
fn advertised_origin(
    state: &ServeState,
    request_host: Option<&str>,
    forwarded_proto: Option<&str>,
) -> String {
    state.advertised_origin(request_host, forwarded_proto)
}

/// A **MapLibre/Mapbox GL Style JSON** (`version: 8`) for `{layer}` — the "one URL" a client
/// (QGIS's *Style URL* field, MapLibre GL, the X-ray viewer) points at to get both the source and
/// its styling. The `sources` entry references the layer's `{grid_id}` TileJSON — parametrized (the
/// HTTP handler defaults `grid_id` to `WebMercatorQuad` when the caller doesn't ask for another grid
/// via `server::mvt_style_handler`'s `?tms=` query param) — so a style requested for e.g.
/// `WorldCRS84Quad` embeds a source that actually matches the tiles it will fetch, instead of always
/// pointing at WebMercatorQuad's. NOTE: MapLibre GL itself only ever renders Web Mercator — pointing
/// it at a non-Mercator grid's tiles is a CLIENT limitation (MapLibre can't reproject on the fly),
/// not something this server can or should paper over.
///
/// The `layers` are a generic **X-ray** treatment — glowing cyan outline + faint fill + point discs
/// — that renders ANY geometry type (polygons, lines, points), independent of the layer's
/// server-side `--vec-style`. `source-layer` is the MVT layer name (== the served layer's name).
/// Returns `Err((404/400,_))` for an unknown/raster layer.
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
    grid_id: &str,
    request_host: Option<&str>,
    forwarded_proto: Option<&str>,
) -> Result<String, (u16, String)> {
    // Validate: the layer must exist and be a vector layer (MVT/style only applies to vectors).
    let (_, v) = resolve_vector(state, layer)?;
    let origin = advertised_origin(state, request_host, forwarded_proto);
    let source_url = format!("{origin}/mvt/{layer}/{grid_id}.json");

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
        // Read the slice straight off the load-all source: it is already parsed, so there is no
        // read to fail and therefore no `Result` worth unwrapping here.
        crate::vector::source::VectorSource::LoadAll(s) => {
            feature_field_schema_slice(crate::vector::source::FeatureSource::features(s))
        }
        crate::vector::source::VectorSource::Windowed(w) => w.field_schema(),
    }
}

/// Every feature property a MapLibre/Mapbox `--mvt-style` reads, i.e. the `FIELD` of every
/// `["get", "FIELD"]` expression anywhere in the document.
///
/// `--mvt-style` is pass-through JSON: it is served to the client and never parsed into a
/// [`crate::vector::style::Style`], so `Style::referenced_fields` cannot see any of this. That is
/// harmless for a file source, which carries every field regardless, and quietly fatal for a
/// `postgis://` layer, whose `SELECT` list is derived from `referenced_fields` — the class column
/// is then never fetched and the client styles the whole map with its fallback paint, 200 OK
/// throughout. This is deliberately a shallow syntactic scan, not a style-spec parser: it powers a
/// startup WARNING, so over-reporting a field that is really there costs a line of text, while
/// under-reporting costs a blank map.
pub fn mvt_style_fields(v: &serde_json::Value) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    collect_get_fields(v, &mut out);
    out
}

fn collect_get_fields(v: &serde_json::Value, out: &mut std::collections::BTreeSet<String>) {
    match v {
        serde_json::Value::Array(items) => {
            // `["get", "name"]` — and only that shape. The 3-argument form `["get", k, obj]` reads
            // a property of some other object, not of the feature, so it is deliberately skipped.
            if items.len() == 2 {
                if let (Some("get"), Some(f)) = (items[0].as_str(), items[1].as_str()) {
                    out.insert(f.to_string());
                }
            }
            for it in items {
                collect_get_fields(it, out);
            }
        }
        serde_json::Value::Object(map) => {
            for val in map.values() {
                collect_get_fields(val, out);
            }
        }
        _ => {}
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
    use super::mvt_style_fields;
    use super::sld_class_fill_layer;

    #[test]
    fn mvt_style_fields_finds_every_get_expression_however_deeply_nested() {
        // Shaped like a real MapLibre style: `get` appears inside a paint expression, inside a
        // filter, and nested several arrays deep in a `match`. Missing any of these on a postgis://
        // layer means that column is never SELECTed and the client paints its fallback colour.
        let v: serde_json::Value = serde_json::from_str(
            r##"{
                 "layers": [
                   { "id": "a",
                     "filter": ["all", ["==", ["get", "kind"], "road"]],
                     "paint": { "fill-color":
                       ["match", ["get", "COS23_n4_C"], "1.1.1.1", "#aaa", "#bbb"] } },
                   { "id": "b", "layout": { "text-field": ["get", "name"] } }
                 ] }"##,
        )
        .unwrap();
        let got = mvt_style_fields(&v);
        assert!(got.contains("kind"), "{got:?}");
        assert!(got.contains("COS23_n4_C"), "{got:?}");
        assert!(got.contains("name"), "{got:?}");
        assert_eq!(got.len(), 3, "nothing else should be reported: {got:?}");
    }

    #[test]
    fn mvt_style_fields_ignores_the_three_argument_get_and_non_string_keys() {
        // `["get", k, obj]` reads a property of ANOTHER object, not of the feature, so requesting
        // that column would be wrong. A non-string key is not a column name either.
        let v: serde_json::Value =
            serde_json::from_str(r#"[["get","k",{"k":1}], ["get", 3], ["get"]]"#).unwrap();
        assert!(
            mvt_style_fields(&v).is_empty(),
            "{:?}",
            mvt_style_fields(&v)
        );
    }

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

    /// Task 3: `/mvt/{layer}/{tms}/...` must resolve a CUSTOM grid the layer publishes (Task 2's
    /// `layer.grids`), not just the 4 built-in presets `tms::preset` knows about. Before this task
    /// `resolve_grid` only ever called `tms::preset`, so a layer served on a custom grid 404'd on
    /// its own tile route — mirrors `tests/mvt_http.rs`'s `vector_layer()` harness, plus a Task-2-style
    /// custom `GridConfig` (this time in the layer's own EPSG:4326 so the tile is guaranteed
    /// non-empty without a cross-CRS reprojection).
    #[test]
    fn mvt_custom_grid_route_resolves_layer_grid() {
        use crate::config::GridConfig;
        use crate::server::{Layer, PublishedGrid, ServeState, VectorLayer};
        use crate::vector::geojson::GeoJsonSource;
        use crate::vector::shape::Shaper;
        use crate::vector::source::{FeatureSource, VectorSource};
        use std::sync::Arc;

        let src = Arc::new(GeoJsonSource::load("fixtures/vector/mini_mvt.geojson").unwrap());
        let style = Style::load("fixtures/styles/airports.vec.json").unwrap();
        let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
        let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
        let ext = src.full_extent();

        // A custom grid covering the fixture's own extent ([-30,-30,30,30], see
        // fixtures/vector/mini_mvt.geojson) at z0, so a single z0/0/0 request returns the whole
        // layer — same "z0/0/0 covers the whole fixture" idiom as the WebMercatorQuad/WorldCRS84Quad
        // tests in tests/mvt_http.rs.
        let grid_cfg = GridConfig {
            crs: "EPSG:4326".to_string(),
            origin: [-30.0, 30.0],
            extent: [-30.0, -30.0, 30.0, 30.0],
            tile_px: 256,
            resolutions: vec![60.0 / 256.0, 30.0 / 256.0],
        };
        let grid = PublishedGrid {
            tms: grid_cfg.to_tms("testgrid"),
            data_bounds: None,
        };

        let layer = Layer {
            name: "mini".into(),
            cog_path: String::new(),
            cog: None,
            source: None,
            style: None,
            src_crs: "EPSG:4326".into(),
            band_math: None,
            bounds_wgs84: ext,
            tile_cache: None,
            index_cache: crate::cache::new_index_cache(crate::cache::index_cache_bytes()),
            grids: vec![grid],
            vector: Some(VectorLayer {
                fields: super::feature_field_schema(src.as_ref()),
                area_scale: crate::vector::mvt::layer_area_scale(ext, ext),
                min_feature_px: 0.0, // size gate off (the default)
                source: VectorSource::LoadAll(src),
                style,
                shaper,
                lod: None,
            }),
            pmtiles: std::collections::BTreeMap::new(),
            raster_pmtiles: std::collections::BTreeMap::new(),
            overlay: std::collections::BTreeMap::new(),
        };

        let st = ServeState::new(vec![layer], "http://h/wms".into(), 16);
        let bytes = super::render_mvt_tile(&st, "mini", "testgrid", 0, 0, 0)
            .expect("custom grid 'testgrid' should resolve, not 404");
        assert!(!bytes.is_empty(), "z0/0/0 covers the whole fixture");
    }

    /// Regression for the Task-3 review finding (R3 precedence): a layer's custom grid stored under
    /// a BARE id that happens to equal a preset base (`"WebMercatorQuad"`, `tile_px` 256) must NOT
    /// absorb a request that carries an EXPLICIT `_{px}` suffix (`"WebMercatorQuad_512"`) — the
    /// suffixed request must fall through to `tms::preset`'s own suffix-override parsing (which pins
    /// `tile_px` to 512), exactly like `wmts::get_tile`'s asymmetric predicate (only the STORED id is
    /// stripped — via `tms::strip_size_suffix` — and compared against the RAW request id; the request
    /// id itself is never stripped). Before the fix, `resolve_grid` pre-stripped the request into a
    /// `base` local and compared `strip_size_suffix(stored) == base`, so the bare-id 256px custom grid
    /// silently absorbed the 512px request instead of falling through to the preset.
    #[test]
    fn mvt_explicit_suffix_falls_through_to_preset_over_bare_id_custom_grid() {
        use crate::config::GridConfig;
        use crate::server::{Layer, PublishedGrid, VectorLayer};
        use crate::vector::geojson::GeoJsonSource;
        use crate::vector::shape::Shaper;
        use crate::vector::source::{FeatureSource, VectorSource};
        use std::sync::Arc;

        let src = Arc::new(GeoJsonSource::load("fixtures/vector/mini_mvt.geojson").unwrap());
        let style = Style::load("fixtures/styles/airports.vec.json").unwrap();
        let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
        let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
        let ext = src.full_extent();

        // A custom grid stored under the BARE preset base id "WebMercatorQuad" at 256px — the base
        // name R3 intends a client to match by requesting WITHOUT a suffix. The request used below
        // instead carries an EXPLICIT "_512" suffix, which must NOT match this grid.
        let grid_cfg = GridConfig {
            crs: "EPSG:3857".to_string(),
            origin: [-20037508.3427892, 20037508.3427892],
            extent: [
                -20037508.3427892,
                -20037508.3427892,
                20037508.3427892,
                20037508.3427892,
            ],
            tile_px: 256,
            resolutions: vec![156543.03392804097],
        };
        let grid = PublishedGrid {
            tms: grid_cfg.to_tms("WebMercatorQuad"),
            data_bounds: None,
        };

        let layer = Layer {
            name: "mini".into(),
            cog_path: String::new(),
            cog: None,
            source: None,
            style: None,
            src_crs: "EPSG:4326".into(),
            band_math: None,
            bounds_wgs84: ext,
            tile_cache: None,
            index_cache: crate::cache::new_index_cache(crate::cache::index_cache_bytes()),
            grids: vec![grid],
            vector: Some(VectorLayer {
                fields: super::feature_field_schema(src.as_ref()),
                area_scale: crate::vector::mvt::layer_area_scale(ext, ext),
                min_feature_px: 0.0, // size gate off (the default)
                source: VectorSource::LoadAll(src),
                style,
                shaper,
                lod: None,
            }),
            pmtiles: std::collections::BTreeMap::new(),
            raster_pmtiles: std::collections::BTreeMap::new(),
            overlay: std::collections::BTreeMap::new(),
        };

        // Sanity check: the bare id (no suffix) hits the layer's own custom 256px grid — R3's
        // intended "omit the suffix to match a suffixed/bare stored grid" case.
        let bare = super::resolve_grid(&layer, "WebMercatorQuad").expect("bare id resolves");
        assert_eq!(
            bare.tile_w, 256,
            "bare-id request hits the layer's custom 256px grid"
        );

        // An EXPLICIT "_512" suffix must NOT match the bare-id 256px custom grid — it falls through
        // to tms::preset's own suffix-override parsing, which pins tile_px to 512.
        let suffixed =
            super::resolve_grid(&layer, "WebMercatorQuad_512").expect("falls through to preset");
        assert_eq!(
            suffixed.tile_w, 512,
            "explicit suffix must resolve via tms::preset (512px), not the bare-id 256px custom grid"
        );
        assert_eq!(suffixed.id, "WebMercatorQuad_512");
    }

    /// A minimal, bespoke, READ-ONLY MVT decoder — test-only (mirrors `tests/mvt_tile.rs`'s
    /// `testdec`, trimmed to exactly what Task 5 needs: walk `Tile{layers}` -> `Layer{features}` ->
    /// `Feature{tags}`, resolved against the layer's interned key/value pools, looking for one
    /// string property). No runtime MVT-decode crate/dependency is added anywhere in `src/` — this
    /// module only exists under `#[cfg(test)]`.
    mod dec {
        struct Reader<'a> {
            buf: &'a [u8],
            pos: usize,
        }
        impl<'a> Reader<'a> {
            fn new(buf: &'a [u8]) -> Self {
                Reader { buf, pos: 0 }
            }
            fn eof(&self) -> bool {
                self.pos >= self.buf.len()
            }
            fn varint(&mut self) -> u64 {
                let mut result = 0u64;
                let mut shift = 0;
                loop {
                    let b = self.buf[self.pos];
                    self.pos += 1;
                    result |= ((b & 0x7f) as u64) << shift;
                    if b & 0x80 == 0 {
                        break;
                    }
                    shift += 7;
                }
                result
            }
            fn tag(&mut self) -> (u32, u32) {
                let t = self.varint();
                ((t >> 3) as u32, (t & 0x7) as u32)
            }
            fn skip(&mut self, wire: u32) {
                match wire {
                    0 => {
                        self.varint();
                    }
                    1 => self.pos += 8,
                    5 => self.pos += 4,
                    2 => {
                        let len = self.varint() as usize;
                        self.pos += len;
                    }
                    _ => panic!("bad wire type {wire}"),
                }
            }
            fn bytes_field(&mut self) -> &'a [u8] {
                let len = self.varint() as usize;
                let s = &self.buf[self.pos..self.pos + len];
                self.pos += len;
                s
            }
            fn packed_u32(&mut self) -> Vec<u32> {
                let bytes = self.bytes_field();
                let mut r = Reader::new(bytes);
                let mut out = Vec::new();
                while !r.eof() {
                    out.push(r.varint() as u32);
                }
                out
            }
        }

        /// True if any feature in any layer of `buf` (a raw encoded MVT tile) has a string
        /// property `key == val`.
        pub fn feature_with_str_prop(buf: &[u8], key: &str, val: &str) -> bool {
            let mut r = Reader::new(buf);
            while !r.eof() {
                let (field, wire) = r.tag();
                if field == 3 && wire == 2 {
                    let layer_buf = r.bytes_field();
                    if layer_has_str_prop(layer_buf, key, val) {
                        return true;
                    }
                } else {
                    r.skip(wire);
                }
            }
            false
        }

        fn layer_has_str_prop(buf: &[u8], key: &str, val: &str) -> bool {
            let mut r = Reader::new(buf);
            let mut keys: Vec<String> = Vec::new();
            let mut values: Vec<Option<String>> = Vec::new();
            let mut feature_bufs: Vec<Vec<u8>> = Vec::new();
            while !r.eof() {
                let (field, wire) = r.tag();
                match field {
                    2 => feature_bufs.push(r.bytes_field().to_vec()),
                    3 => keys.push(String::from_utf8(r.bytes_field().to_vec()).unwrap()),
                    4 => values.push(decode_value_str(r.bytes_field())),
                    _ => r.skip(wire),
                }
            }
            feature_bufs
                .iter()
                .any(|fb| feature_has_str_prop(fb, &keys, &values, key, val))
        }

        /// `Value` field 1 is the string variant (field 2/3/... are numeric/bool variants, not
        /// needed here — mirrors `testdec::decode_value`'s `1 => DValue::Str(...)` arm).
        fn decode_value_str(buf: &[u8]) -> Option<String> {
            let mut r = Reader::new(buf);
            let mut out = None;
            while !r.eof() {
                let (field, wire) = r.tag();
                if field == 1 && wire == 2 {
                    out = Some(String::from_utf8(r.bytes_field().to_vec()).unwrap());
                } else {
                    r.skip(wire);
                }
            }
            out
        }

        fn feature_has_str_prop(
            buf: &[u8],
            keys: &[String],
            values: &[Option<String>],
            key: &str,
            val: &str,
        ) -> bool {
            let mut r = Reader::new(buf);
            let mut tags: Vec<u32> = Vec::new();
            while !r.eof() {
                let (field, wire) = r.tag();
                match field {
                    2 => tags = r.packed_u32(),
                    _ => r.skip(wire),
                }
            }
            let mut i = 0;
            while i + 1 < tags.len() {
                let k = &keys[tags[i] as usize];
                let v = values.get(tags[i + 1] as usize).and_then(|v| v.as_deref());
                if k == key && v == Some(val) {
                    return true;
                }
                i += 2;
            }
            false
        }
    }

    /// Task 5 (end-to-end proof): a small synthetic feature INSIDE Switzerland (lon 8.2, lat 46.8,
    /// well within the official CH extent) is served as MVT on BOTH `WorldCRS84Quad` (the built-in
    /// non-Mercator preset — the CRS84 baseline that reproduces soilgrids.org's non-Mercator MVT on
    /// our OWN grid) AND `swissLV95.json` (an OGC TileMatrixSet 2.0 document, EPSG:2056, loaded
    /// through the `.json`-suffix dispatch this task adds to `config::resolve_one` — exercised here
    /// via the public `resolve_grids_presets`, the exact seam a `--config` `grids: [swissLV95.json]`
    /// entry goes through). Each grid's z0 tile covers its WHOLE extent by construction
    /// (WorldCRS84Quad z0 is 2 tiles — col 1 is the full eastern hemisphere; swissLV95's z0 is a
    /// single 1x1-matrix tile, `cellSize` 4000 not even filling the CH extent once — see
    /// `fixtures/grids/swissLV95.json`), so no per-feature tile-index arithmetic is needed to pick a
    /// covering z/x/y; the feature reprojects 4326 -> 2056 inside the encoder either way.
    #[test]
    fn crs84_and_lv95_serve_the_same_feature_as_mvt() {
        use crate::server::{Layer, PublishedGrid, ServeState, VectorLayer};
        use crate::vector::geojson::GeoJsonSource;
        use crate::vector::shape::Shaper;
        use crate::vector::source::{FeatureSource, VectorSource};
        use std::sync::Arc;

        // A small polygon around lon 8.2 / lat 46.8 (central Switzerland, well inside the CH bbox).
        let geojson = r#"{
          "type": "FeatureCollection",
          "features": [
            {
              "type": "Feature",
              "properties": { "name": "swiss_test" },
              "geometry": {
                "type": "Polygon",
                "coordinates": [[
                  [8.15, 46.75], [8.25, 46.75], [8.25, 46.85], [8.15, 46.85], [8.15, 46.75]
                ]]
              }
            }
          ]
        }"#;
        let src = Arc::new(GeoJsonSource::from_str(geojson).unwrap());
        let style = Style::load("fixtures/styles/airports.vec.json").unwrap();
        let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
        let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
        let ext = src.full_extent();

        // Grid 1: the built-in WorldCRS84Quad preset.
        let crs84 = PublishedGrid {
            tms: crate::tms::TileMatrixSet::world_crs84_quad(256),
            data_bounds: None,
        };

        // Grid 2: swissLV95, loaded from the OGC TMS 2.0 JSON fixture via `resolve_grids_presets`
        // (which calls `config::resolve_one` per id) — the `.json` dispatch under test.
        let lv95_tms = crate::config::resolve_grids_presets(
            &["fixtures/grids/swissLV95.json".to_string()],
            256,
            &std::collections::BTreeMap::new(),
        )
        .expect("swissLV95.json should resolve via the .json dispatch in config::resolve_one")
        .into_iter()
        .next()
        .unwrap();
        assert_eq!(
            lv95_tms.id, "swissLV95",
            "id comes from the JSON, not the path"
        );
        assert_eq!(
            lv95_tms.crs, "EPSG:2056",
            "normalize_crs must strip the OGC URI form"
        );
        let lv95 = PublishedGrid {
            tms: lv95_tms,
            data_bounds: None,
        };

        let layer = Layer {
            name: "mini".into(),
            cog_path: String::new(),
            cog: None,
            source: None,
            style: None,
            src_crs: "EPSG:4326".into(),
            band_math: None,
            bounds_wgs84: ext,
            tile_cache: None,
            index_cache: crate::cache::new_index_cache(crate::cache::index_cache_bytes()),
            grids: vec![crs84, lv95],
            vector: Some(VectorLayer {
                fields: super::feature_field_schema(src.as_ref()),
                area_scale: crate::vector::mvt::layer_area_scale(ext, ext),
                min_feature_px: 0.0, // size gate off (the default)
                source: VectorSource::LoadAll(src),
                style,
                shaper,
                lod: None,
            }),
            pmtiles: std::collections::BTreeMap::new(),
            raster_pmtiles: std::collections::BTreeMap::new(),
            overlay: std::collections::BTreeMap::new(),
        };

        let st = ServeState::new(vec![layer], "http://h/wms".into(), 16);

        // WorldCRS84Quad z0: matrix_w=2, matrix_h=1; col 1 = [0,180] x [-90,90] (the whole eastern
        // hemisphere) — covers lon 8.2 / lat 46.8 with no per-feature arithmetic.
        let crs84_bytes = super::render_mvt_tile(&st, "mini", "WorldCRS84Quad", 0, 1, 0)
            .expect("WorldCRS84Quad z0/1/0 should render");
        assert!(
            dec::feature_with_str_prop(&crs84_bytes, "name", "swiss_test"),
            "feature must appear in the WorldCRS84Quad (CRS84 baseline) tile"
        );

        // swissLV95 z0: matrixWidth=matrixHeight=1 — the single z0/0/0 tile covers the whole
        // official CH extent [2420000,1030000,2900000,1350000], guaranteed to contain the
        // reprojected feature.
        let lv95_bytes = super::render_mvt_tile(&st, "mini", "swissLV95", 0, 0, 0)
            .expect("swissLV95 z0/0/0 should render");
        assert!(
            dec::feature_with_str_prop(&lv95_bytes, "name", "swiss_test"),
            "feature must appear in the swissLV95 (EPSG:2056, reprojected 4326->2056) tile"
        );
    }

    /// Task 3: `Layer.pmtiles` is a `BTreeMap<grid_id, Arc<PmtilesReader>>`, and `render_mvt_tile`
    /// must select the entry matching the REQUESTED grid (`tms_id`), not just "the" archive (Spec 1's
    /// old `Option<Arc<PmtilesReader>>` shape). Builds two tiny, REAL `.pmtiles` archives (the same
    /// minimal `PmtilesWriter` pattern `write.rs`'s own tests use) holding distinct, deliberately
    /// non-MVT payload bytes at z0/0/0, files them under two different grid ids on one layer, and
    /// proves a request on grid A returns exactly archive A's bytes while grid B returns exactly
    /// archive B's — i.e. the map is genuinely keyed by grid, not just "first entry wins" or "last
    /// entry wins". Uses two real preset ids (`WebMercatorQuad`/`WorldCRS84Quad`) as `tms_id` so
    /// `resolve_grid`'s preset fallback resolves both without any custom `layer.grids` setup.
    #[test]
    fn pmtiles_read_through_selects_the_archive_for_the_requested_grid() {
        use crate::server::{Layer, ServeState, VectorLayer};
        use crate::vector::geojson::GeoJsonSource;
        use crate::vector::pmtiles::codec::gzip;
        use crate::vector::pmtiles::read::PmtilesReader;
        use crate::vector::pmtiles::write::{HeaderFields, PmtilesWriter};
        use crate::vector::pmtiles::zxy_to_tileid;
        use crate::vector::shape::Shaper;
        use crate::vector::source::{FeatureSource, VectorSource};
        use std::sync::Arc;

        // A minimal, valid one-tile `.pmtiles` at z0/0/0 carrying `payload` verbatim (gzip'd, as the
        // writer/reader always store/decompress) and a `grid_id`-tagged metadata JSON — mirrors
        // `pmtiles_metadata_json`'s shape closely enough for `PmtilesReader::open` (which only needs
        // a well-formed header + directory; it doesn't validate `metadata` as MVT).
        fn build_tiny_archive(
            tmp: &std::path::Path,
            name: &str,
            grid_id: &str,
            payload: &[u8],
        ) -> PmtilesReader {
            let dir = tmp.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let out = dir.join("out.pmtiles");
            let mut w = PmtilesWriter::new(&dir).unwrap();
            w.add(zxy_to_tileid(0, 0, 0), gzip(payload)).unwrap();
            let hf = HeaderFields {
                min_zoom: 0,
                max_zoom: 0,
                bounds_e7: [0, 0, 0, 0],
                center: (0, 0, 0),
            };
            let metadata = format!(r#"{{"vector_layers":[],"grid_id":"{grid_id}"}}"#);
            w.finish(hf, &metadata, &out).unwrap();
            PmtilesReader::open(&out).unwrap()
        }

        let tmp = std::env::temp_dir().join(format!(
            "ts_mvt_http_pmtiles_select_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let reader_a = build_tiny_archive(&tmp, "a", "WebMercatorQuad", b"MERCATOR_TILE");
        let reader_b = build_tiny_archive(&tmp, "b", "WorldCRS84Quad", b"CRS84_TILE");
        // Distinct grid_id() readings prove the two archives are genuinely independent objects, not
        // just two handles onto the same bytes.
        assert_eq!(reader_a.grid_id(), "WebMercatorQuad");
        assert_eq!(reader_b.grid_id(), "WorldCRS84Quad");

        // A minimal, real vector layer — archive-hit is an early-return in `render_mvt_tile` before
        // `v` is ever touched, so any small fixture will do; reuses the fixture already loaded above.
        let src = Arc::new(GeoJsonSource::load("fixtures/vector/mini_mvt.geojson").unwrap());
        let style = Style::load("fixtures/styles/airports.vec.json").unwrap();
        let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
        let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
        let ext = src.full_extent();

        let mut pmtiles = std::collections::BTreeMap::new();
        pmtiles.insert("WebMercatorQuad".to_string(), Arc::new(reader_a));
        pmtiles.insert("WorldCRS84Quad".to_string(), Arc::new(reader_b));
        // Map keying, directly: two distinct grid ids resolve to two distinct `Arc<PmtilesReader>`s.
        assert!(!Arc::ptr_eq(
            pmtiles.get("WebMercatorQuad").unwrap(),
            pmtiles.get("WorldCRS84Quad").unwrap()
        ));

        let layer = Layer {
            name: "mini".into(),
            cog_path: String::new(),
            cog: None,
            source: None,
            style: None,
            src_crs: "EPSG:4326".into(),
            band_math: None,
            bounds_wgs84: ext,
            tile_cache: None,
            index_cache: crate::cache::new_index_cache(crate::cache::index_cache_bytes()),
            grids: Vec::new(),
            vector: Some(VectorLayer {
                fields: super::feature_field_schema(src.as_ref()),
                area_scale: crate::vector::mvt::layer_area_scale(ext, ext),
                min_feature_px: 0.0, // size gate off (the default)
                source: VectorSource::LoadAll(src),
                style,
                shaper,
                lod: None,
            }),
            pmtiles,
            raster_pmtiles: std::collections::BTreeMap::new(),
            overlay: std::collections::BTreeMap::new(),
        };
        let st = ServeState::new(vec![layer], "http://h/wms".into(), 16);

        // End-to-end through `render_mvt_tile`: each grid's request must be served from ITS OWN
        // archive, not the other's (and not a live encode — a live tile would be valid MVT bytes,
        // never the literal `MERCATOR_TILE`/`CRS84_TILE` markers).
        let got_a = super::render_mvt_tile(&st, "mini", "WebMercatorQuad", 0, 0, 0).unwrap();
        let got_b = super::render_mvt_tile(&st, "mini", "WorldCRS84Quad", 0, 0, 0).unwrap();
        assert_eq!(got_a, b"MERCATOR_TILE");
        assert_eq!(got_b, b"CRS84_TILE");
        assert_ne!(got_a, got_b);

        std::fs::remove_dir_all(&tmp).ok();
    }

    // ---- advertised origin (TileJSON / style.json `sources.*.url`) ----------------------------
    //
    // Regression tests for a LIVE production bug, reproduced 2026-08-02 against terraserve.io:
    //
    //   GET https://terraserve.io/demo/vida/mvt/vida/style.json
    //     -> "url": "http://terraserve.io/mvt/vida/WebMercatorQuad.json"   (404, and mixed-content
    //                                                                       blocked before that)
    //   correct:  https://terraserve.io/demo/vida/mvt/vida/WebMercatorQuad.json   (200)
    //
    // `advertised_origin` preferred the Host header unconditionally, which (a) hardcoded the
    // `http://` scheme even behind TLS-terminating Traefik and (b) rebuilt the origin from the
    // host alone, discarding the `/demo/vida` path prefix. Because HTTP/1.1 always sends `Host`,
    // the configured `--public-url` branch was effectively dead code.

    fn state_with(public_url: Option<&str>, base_url: &str) -> crate::server::ServeState {
        let mut st = crate::server::ServeState::new(vec![], base_url.into(), 1);
        st.public_url = public_url.map(|s| s.to_string());
        st
    }

    /// The bug itself: an explicitly configured `--public-url` is authoritative. It is the only
    /// source that carries BOTH the public scheme and the path prefix, neither of which any
    /// request header reliably provides, so it must win over the Host header.
    #[test]
    fn advertised_origin_prefers_explicit_public_url_over_host_header() {
        let st = state_with(
            Some("https://terraserve.io/demo/vida/wms"),
            "https://terraserve.io/demo/vida/wms",
        );
        let got = super::advertised_origin(&st, Some("terraserve.io"), None);
        assert_eq!(
            got, "https://terraserve.io/demo/vida",
            "configured --public-url must win over the Host header (scheme AND path prefix)"
        );
    }

    /// Traefik terminates TLS, so the origin scheme must come from `X-Forwarded-Proto`, never be
    /// assumed. Applies when there is no `--public-url` to be authoritative.
    #[test]
    fn advertised_origin_honours_forwarded_proto_when_no_public_url() {
        let st = state_with(None, "http://127.0.0.1:8080/wms");
        let got = super::advertised_origin(&st, Some("example.org"), Some("https"));
        assert_eq!(got, "https://example.org");
    }

    /// A proxy may send a comma-separated `X-Forwarded-Proto` chain; the FIRST entry is the
    /// original client-facing scheme.
    #[test]
    fn advertised_origin_takes_first_forwarded_proto_of_a_chain() {
        let st = state_with(None, "http://127.0.0.1:8080/wms");
        let got = super::advertised_origin(&st, Some("example.org"), Some("https, http"));
        assert_eq!(got, "https://example.org");
    }

    /// Unchanged behaviour for a plain local run: no `--public-url`, no proxy headers -> derive
    /// from the Host header over http. This is what keeps `serve` working with no configuration.
    #[test]
    fn advertised_origin_falls_back_to_http_host_without_public_url_or_proto() {
        let st = state_with(None, "http://127.0.0.1:8080/wms");
        let got = super::advertised_origin(&st, Some("localhost:8080"), None);
        assert_eq!(got, "http://localhost:8080");
    }

    /// With neither a `--public-url` nor a Host header, fall back to the bind-address base_url,
    /// with the `/wms` suffix trimmed (the origin is the mount point, not the WMS endpoint).
    #[test]
    fn advertised_origin_falls_back_to_base_url_without_host() {
        let st = state_with(None, "http://127.0.0.1:8080/wms");
        let got = super::advertised_origin(&st, None, None);
        assert_eq!(got, "http://127.0.0.1:8080");
    }

    /// A `--public-url` given WITHOUT the conventional `/wms` suffix must not be mangled, and a
    /// trailing slash must not produce a doubled separator in the composed tile URL.
    #[test]
    fn advertised_origin_normalises_public_url_without_wms_suffix_or_trailing_slash() {
        let a = state_with(Some("https://maps.example.org/ts/"), "unused");
        assert_eq!(
            super::advertised_origin(&a, Some("maps.example.org"), None),
            "https://maps.example.org/ts"
        );
        let b = state_with(Some("https://maps.example.org/ts"), "unused");
        assert_eq!(
            super::advertised_origin(&b, Some("maps.example.org"), None),
            "https://maps.example.org/ts"
        );
    }
}
