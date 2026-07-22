// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! The tile assembler: `FeatureSource` + `(tms, z, x, y)` -> one MVT `Tile` message's bytes.
//! Combines `geom::Projector` (source CRS -> tile-local 4096 grid), `clip` (buffered-rect clip),
//! and `mvt::{geom, wire}` (command-stream + protobuf encode) per `vector_tile.proto`:
//! `Tile{layers=3}`, `Layer{version=15,name=1,features=2,keys=3,values=4,extent=5}`,
//! `Feature{id=1,tags=2,type=3,geometry=4}`, `Value{string_value=1,double_value=3}`.

use std::collections::HashMap;

use super::opts::MvtOptimizations;
use super::wire::PbfWriter;
use super::{clip, geom as mvtgeom};
use crate::tms::TileMatrixSet;
use crate::vector::feature::{Feature, Geometry, Value};
use crate::vector::geom::{bbox_overlaps, source_filter_bbox, Projector};
use crate::vector::source::FeatureSource;

/// MVT tile extent (the local coordinate grid each feature's geometry is expressed in).
pub(crate) const EXTENT: u32 = 4096;
/// Clip buffer around the tile — 1/16th of the extent, the conventional MVT overscan so
/// geometry crossing a tile edge still renders correctly when adjacent tiles are stitched.
pub(crate) const BUF: f64 = (EXTENT / 16) as f64;
/// Default per-tile feature budget (see `encode_tile_opt`). Overridable at runtime with
/// `serve --mvt-max-features N` (0 = unlimited). When a low-zoom tile holds more in-tile features
/// than the budget, they are UNIFORMLY sampled down to it (spread evenly across the tile — NOT the
/// first-N by insertion order, which clusters spatially and leaves tile-aligned rectangular holes).
/// High-zoom tiles (fewer in-tile features than the budget) keep every feature at full detail.
pub const DEFAULT_MAX_FEATURES_PER_TILE: usize = 20_000;

/// Web-mercator (EPSG:3857) world extent in metres — full projected width and height.
pub(crate) const WORLD_MERC_M: f64 = 40_075_016.686;
/// The display tile size (px) the `--mvt-min-feature-px` threshold is measured against — the classic
/// 256-px slippy-map tile, so one display pixel spans `WORLD_MERC_M / (2^z · 256)` metres at zoom z.
pub(crate) const DISPLAY_TILE_PX: f64 = 256.0;

/// Web-mercator ground metres per display pixel at WebMercatorQuad zoom `z`. The single definition of
/// the zoom→resolution relationship shared by the MVT size gate (`min_area_src_for_zoom`) and the
/// per-zoom LOD tolerance (`topology::lod::px_len_src`) so they can never drift apart.
pub(crate) fn merc_m_per_px(z: u32) -> f64 {
    WORLD_MERC_M / (2f64.powi(z as i32) * DISPLAY_TILE_PX)
}

/// Web-mercator forward Y (metres) for a WGS84 latitude in degrees. Closed form (no libproj); only
/// used to size a layer's extent in mercator for `layer_area_scale`.
fn merc_y(lat_deg: f64) -> f64 {
    let r = WORLD_MERC_M / (2.0 * std::f64::consts::PI); // 6378137.0
    let lat = lat_deg.clamp(-85.051_128_78, 85.051_128_78).to_radians();
    r * (std::f64::consts::FRAC_PI_4 + lat / 2.0).tan().ln()
}

/// Metres²(web-mercator) per source-CRS unit² for a layer, from its WGS84 bounds `[w,s,e,n]` and its
/// source-CRS extent `[minx,miny,maxx,maxy]`. A single per-LAYER constant (the whole layer, never a
/// tile) so the per-zoom min-area threshold is identical for every tile of a layer — that is what
/// makes the selection tile-independent and therefore seam-free (a per-tile scale would flicker
/// knife-edge features across a boundary). It is a layer-mean: because mercator area distortion is
/// `1/cos²(lat)`, a very wide-latitude layer's on-screen `px²` calibration can be off by that factor
/// far from the mean latitude — still seam-free, just a coarser size estimate. Returns `0.0` if
/// either extent is degenerate or non-finite, which makes `min_area_src_for_zoom` disable the gate
/// (fail-OPEN, keep everything) — never `1.0`, which would fail CLOSED on a geographic CRS.
pub fn layer_area_scale(bounds_wgs84: [f64; 4], src_extent: [f64; 4]) -> f64 {
    let r = WORLD_MERC_M / (2.0 * std::f64::consts::PI);
    let [w, s, e, n] = bounds_wgs84;
    let merc_w = (e - w).to_radians() * r; // mercator X span (m)
    let merc_h = merc_y(n) - merc_y(s); // mercator Y span (m)
    let merc_area = (merc_w * merc_h).abs();
    let [sx0, sy0, sx1, sy1] = src_extent;
    let src_area = ((sx1 - sx0) * (sy1 - sy0)).abs();
    if merc_area.is_finite() && src_area.is_finite() && merc_area > 0.0 && src_area > 0.0 {
        merc_area / src_area
    } else {
        0.0
    }
}

/// The minimum feature area (in SOURCE-CRS units²) for a feature to appear at zoom `z`: a feature
/// must cover at least `min_feature_px` display-pixels². `0.0` (gate OFF) when `min_feature_px <= 0`
/// or `area_scale` is not positive. `area_scale` comes from [`layer_area_scale`]. A pure,
/// deterministic function of `(z, area_scale, min_feature_px)` — no tile coordinates — so every tile
/// at a given zoom applies the identical threshold (seam-free selection).
///
/// Calibrated for **WebMercatorQuad** (the pixel size is `WORLD_MERC_M / (2^z · 256)`). Seam-freeness
/// holds on any grid (still a per-zoom constant), but on non-mercator grids (WorldCRS84Quad, the UPS
/// polar grids) the `px²` figure is only approximate — treat `min_feature_px` as a tuning knob there.
pub fn min_area_src_for_zoom(z: u32, area_scale: f64, min_feature_px: f64) -> f64 {
    if min_feature_px <= 0.0 || !(area_scale > 0.0) {
        return 0.0;
    }
    let px_m = merc_m_per_px(z); // metres per display px at z
    let px_area_merc = px_m * px_m; // mercator m² per display px²
    min_feature_px * px_area_merc / area_scale // → source-CRS units²
}

/// Encode one MVT tile with the default feature budget. Thin wrapper over `encode_tile_opt`
/// (the entry the tests and any default caller use). Kept taking a bare `&dyn FeatureSource` —
/// several test fixtures build one directly and this is the one place that still needs the whole
/// slice (there is no bbox to narrow by, since this wrapper doesn't take a `VectorSource`).
pub fn encode_tile(
    src: &dyn FeatureSource,
    tms: &TileMatrixSet,
    z: u32,
    x: u32,
    y: u32,
    src_crs: &str,
    layer_name: &str,
) -> Vec<u8> {
    encode_tile_opt(
        src.features(),
        tms,
        z,
        x,
        y,
        src_crs,
        layer_name,
        &MvtOptimizations::defaults(),
    )
}

/// Resolve the features to encode for tile `(z,x,y)` of a `VectorSource` — the windowed-seam
/// migration's caller-side half of the CRS fix: reproject the tile's bbox (tile CRS) into the
/// layer's source CRS via `source_filter_bbox` (the same helper `encode_tile_opt`'s own per-feature
/// pre-filter uses below), THEN read through `features_in`. `LoadAll` ignores its bbox argument (the
/// `Borrowed` arm), so this is byte-identical to the pre-seam `src.features()` call for every source
/// today; a `Windowed` source (FlatGeoBuf) gets a real R-tree-narrowed window once it lands. A `None`
/// tile bounds (out of the grid) or an unavailable CRS transform (fail-open, matching the pre-filter's
/// own fallback) degrades to an empty batch / the source's full extent respectively — `encode_tile_opt`
/// re-derives `tms.tile_bounds` itself and returns empty on the same out-of-grid case, so an empty
/// batch here is never wrong, just occasionally a redundant zero-feature call.
pub fn features_for_tile<'a>(
    src: &'a crate::vector::source::VectorSource,
    tms: &TileMatrixSet,
    z: u32,
    x: u32,
    y: u32,
    src_crs: &str,
) -> crate::vector::source::FeatureBatch<'a> {
    let Some(bbox) = tms.tile_bounds(z, x, y) else {
        return crate::vector::source::FeatureBatch::Borrowed(&[]);
    };
    let query_bbox =
        source_filter_bbox(tms.crs.as_str(), src_crs, bbox).unwrap_or_else(|| src.full_extent());
    src.features_in(query_bbox)
}

/// One encoded MVT feature (geometry + interned attribute tag indices).
struct Encoded {
    fid: u64,
    tags: Vec<u32>,
    geom_type: u32,
    commands: Vec<u32>,
}

/// A tile's interning pools (keys/values, deduplicated) plus the emitted-feature accumulator, shared
/// by the mosaic / dissolve / plain encode paths so the survivor pass lives in exactly one place.
struct TileEncoder {
    key_list: Vec<String>,
    key_idx: HashMap<String, u32>,
    val_list: Vec<Value>,
    val_idx: HashMap<String, u32>,
    out_features: Vec<Encoded>,
}

impl TileEncoder {
    fn new() -> Self {
        TileEncoder {
            key_list: Vec::new(),
            key_idx: HashMap::new(),
            val_list: Vec::new(),
            val_idx: HashMap::new(),
            out_features: Vec::new(),
        }
    }

    /// Sample the survivor candidates down to `max_features` (uniformly, never first-N) and
    /// project/clip/encode each, interning its props from the EMITTED features only.
    ///
    /// `class_filter = Some(field)`: the points/lines pass that follows a polygon-replace pass
    /// (mosaic / dissolve) — a polygon that HAS the class field was already replaced, so skip it;
    /// keep polygons LACKING the field (Fable-5 #2: no silent data loss) and all points/lines.
    /// `class_filter = None`: the plain path — every candidate is a survivor.
    #[allow(clippy::too_many_arguments)]
    fn encode_survivors(
        &mut self,
        features: &[Feature],
        candidates: &[usize],
        class_filter: Option<&str>,
        max_features: usize,
        proj: &Projector,
        rect: [f64; 4],
        dedup: bool,
    ) {
        let survivors: Vec<usize> = match class_filter {
            Some(field) => candidates
                .iter()
                .copied()
                .filter(|&i| match &features[i].geom {
                    Geometry::Polygon(_) | Geometry::MultiPolygon(_) => {
                        !matches!(features[i].props.get(field), Some(v) if !matches!(v, Value::Null))
                    }
                    _ => true,
                })
                .collect(),
            None => candidates.to_vec(),
        };
        let keep = if max_features == 0 {
            (0..survivors.len()).collect::<Vec<_>>()
        } else {
            sampled_positions(survivors.len(), max_features)
        };
        for &k in &keep {
            let f = &features[survivors[k]];
            let Some((geom_type, commands)) = encode_feature_geometry(proj, &f.geom, rect, dedup)
            else {
                continue;
            };
            let mut tags = Vec::new();
            for (pk, pv) in f.props.iter() {
                if matches!(pv, Value::Null) {
                    continue;
                }
                tags.push(intern_key(&mut self.key_list, &mut self.key_idx, pk));
                tags.push(intern_val(&mut self.val_list, &mut self.val_idx, pv));
            }
            self.out_features.push(Encoded {
                fid: f.fid,
                tags,
                geom_type,
                commands,
            });
        }
    }
}

/// Encode one MVT tile under an [`MvtOptimizations`] set (the budget cap, seam-free size gate,
/// grid-snap dedup — the cell mosaic lands in Task B6). Returns an empty `Vec` if the tile is out
/// of the grid, the projector can't be built (bad/unsupported CRS pair), the source has no
/// features, or every feature is clipped away (nothing left to emit).
///
/// Hot path (see the 2026-07-13 efficiency audit): a cheap source-CRS **bbox pre-filter** rejects
/// features not overlapping the tile BEFORE the expensive per-vertex libproj projection, and the
/// attribute key/value pools are built from the **surviving (emitted) features only** — never all
/// N. Together these turn O(all features) per tile (~23 s / ~150 MB for 3.4M BUPi parcels) into
/// O(features-in-tile).
///
/// Takes a plain slice (not a `FeatureSource`/`VectorSource`) — the windowed-seam migration (Task
/// 1b) pushed the `VectorSource::features_in(bbox)` read out to the caller (`features_for_tile`
/// above, or the `/mvt`/WMTS/PMTiles-generator call sites directly), since a windowed read needs the
/// tile bbox reprojected into the source CRS BEFORE the read happens — this function only narrows
/// an already-fetched slice down to the tile's candidates.
pub fn encode_tile_opt(
    features: &[Feature],
    tms: &TileMatrixSet,
    z: u32,
    x: u32,
    y: u32,
    src_crs: &str,
    layer_name: &str,
    opts: &MvtOptimizations,
) -> Vec<u8> {
    // Derive the per-ZOOM min-feature-size threshold HERE (not in the caller) so the MVT and WMTS
    // routes produce identical bytes from a single derivation site. `max_features`/`dedup` come
    // straight off the opts. (Cell-mosaic wiring lands in Task B6.)
    let max_features = opts.max_features;
    let min_area_src = min_area_src_for_zoom(z, opts.area_scale, opts.min_feature_px);
    let Some(bbox) = tms.tile_bounds(z, x, y) else {
        return Vec::new();
    };
    let tile_crs = tms.crs.as_str();
    let Ok(proj) = Projector::new(src_crs, tile_crs, bbox, EXTENT, EXTENT) else {
        return Vec::new();
    };

    // Stage B (cell mosaic) active for THIS tile? When active, POLYGONS are replaced by the
    // dominant-class mosaic (which votes on the RAW candidate set — the size gate below is skipped so
    // dropped small polygons don't leave holes); points/lines still take the normal path.
    let mosaic_active = is_mosaic_active(opts, z);
    // Same-class dissolve is the other polygon-replace pass (mutually exclusive with the mosaic via
    // `for_layer`); it also votes on the raw candidate set (size gate skipped below).
    let dissolve_active = is_dissolve_active(opts, z);

    if features.is_empty() {
        return Vec::new();
    }

    let rect: [f64; 4] = [-BUF, -BUF, EXTENT as f64 + BUF, EXTENT as f64 + BUF];

    // The tile's footprint in the SOURCE CRS (for the cheap pre-filter): reproject the tile bbox
    // (tile CRS) back to the source CRS — densified along the edges by `crs_bounds` — then expand
    // by ~1/16 span (the same overscan as the tile-grid BUF) so features whose geometry only
    // enters the tile through the buffer still pass. `None` (transform unavailable) → no pre-filter
    // (fall back to projecting everything, the pre-audit behavior — never wrong, just slow).
    let filt = source_filter_bbox(tile_crs, src_crs, bbox);

    // Phase 1: cheap bbox pre-filter -> candidate indices (no libproj). Features whose source-CRS
    // bbox misses the tile footprint are rejected before the expensive projection — the
    // O(all-features) -> O(in-tile) win.
    let mut candidates: Vec<usize> = Vec::new();
    for (i, f) in features.iter().enumerate() {
        // Per-zoom min-feature-size selection (seam-free): drop POLYGON features whose source-CRS
        // area is below the zoom's threshold. The threshold is a per-layer/per-zoom CONSTANT (see
        // `min_area_src_for_zoom`), independent of x/y, so the SAME feature is kept or dropped in
        // EVERY tile at this zoom — no cross-tile density seam. `0.0` = gate off. Zero-area
        // geometries (points/lines) are EXEMPT — the size gate is a polygon-coverage concept; gating
        // them would blank a roads/airports layer at every zoom.
        // SKIPPED when the mosaic is active — it votes on the raw set (Fable-5 finding 2); dropping
        // small polygons here would leave the very holes the mosaic exists to fill.
        if !(mosaic_active || dissolve_active)
            && min_area_src > 0.0
            && f.area > 0.0
            && f.area < min_area_src
        {
            continue;
        }
        // No filter (transform unavailable) keeps every feature; otherwise keep those whose
        // source-CRS bbox overlaps the tile footprint.
        if filt.map_or(true, |fb| bbox_overlaps(f.bbox, fb)) {
            candidates.push(i);
        }
    }
    if candidates.is_empty() {
        return Vec::new();
    }

    // Interning pools + emitted-feature accumulator, shared by all three paths.
    let mut enc = TileEncoder::new();

    if mosaic_active {
        // Stage B — REPLACE the tile's polygons with the dominant-class mosaic (it bypasses the size
        // gate AND the budget: it is bounded by construction). Points/lines take the normal path.
        let field = opts.cell_field.as_deref().unwrap_or_default(); // mosaic_active ⇒ Some
        let poly_refs: Vec<&Feature> = candidates
            .iter()
            .map(|&i| &features[i])
            .filter(|f| matches!(f.geom, Geometry::Polygon(_) | Geometry::MultiPolygon(_)))
            .collect();
        for (ri, rc) in super::cell::mosaic_rects(&proj, &poly_refs, field, opts.cell_units)
            .iter()
            .enumerate()
        {
            let ring = vec![
                [rc.x0, rc.y0],
                [rc.x1, rc.y0],
                [rc.x1, rc.y1],
                [rc.x0, rc.y1],
            ];
            let ki = intern_key(&mut enc.key_list, &mut enc.key_idx, field);
            let vi = intern_val(&mut enc.val_list, &mut enc.val_idx, &rc.class);
            enc.out_features.push(Encoded {
                fid: CELL_FID_BASE + ri as u64,
                tags: vec![ki, vi],
                geom_type: mvtgeom::GEOM_POLYGON,
                commands: mvtgeom::encode_polygon(&[ring]),
            });
        }
        // Points/lines survivors: polygons with the class field were replaced above; encode the rest.
        enc.encode_survivors(
            features,
            &candidates,
            Some(field),
            max_features,
            &proj,
            rect,
            opts.dedup,
        );
    } else if dissolve_active {
        // Same-class DISSOLVE — REPLACE polygons with merged true-boundary geometry (votes on the
        // RAW candidates, bypassing the size gate + budget). Points/lines take the normal path
        // (mirrors the mosaic branch's points/lines handling).
        let field = opts.dissolve_field.as_deref().unwrap_or_default(); // dissolve_active ⇒ Some
        let poly_refs: Vec<&Feature> = candidates
            .iter()
            .map(|&i| &features[i])
            .filter(|f| matches!(f.geom, Geometry::Polygon(_) | Geometry::MultiPolygon(_)))
            .collect();
        let (dissolved, diag) =
            super::dissolve::dissolve_features(&poly_refs, field, src_crs, tile_crs, bbox);
        if diag.dropped_rings > 0 {
            eprintln!(
                "mvt dissolve: tile {z}/{x}/{y} dropped {} ring(s)",
                diag.dropped_rings
            );
        }
        for (fi, (value, poly_groups)) in dissolved.iter().enumerate() {
            // Clip each dissolved polygon (i32 → f64 → Sutherland-Hodgman → back to i32) and assemble
            // the class's MultiPolygon.
            let mut clipped_groups: Vec<Vec<Vec<[i32; 2]>>> = Vec::new();
            for group in poly_groups {
                let f64_rings: Vec<Vec<[f64; 2]>> = group
                    .iter()
                    .map(|r| r.iter().map(|v| [v[0] as f64, v[1] as f64]).collect())
                    .collect();
                let clipped = clip::clip_polygon(&f64_rings, rect);
                if clipped.is_empty() {
                    continue;
                }
                // Dissolve owns its rounding — always dedup (Fable-5 review #3: makes the
                // `--no-optimizations` invariance genuine + avoids zero-delta segments).
                clipped_groups.push(clipped.iter().map(|r| to_i32_ring(r, true)).collect());
            }
            if clipped_groups.is_empty() {
                continue;
            }
            let ki = intern_key(&mut enc.key_list, &mut enc.key_idx, field);
            let vi = intern_val(&mut enc.val_list, &mut enc.val_idx, value);
            enc.out_features.push(Encoded {
                fid: CELL_FID_BASE + fi as u64,
                tags: vec![ki, vi],
                geom_type: mvtgeom::GEOM_POLYGON,
                commands: mvtgeom::encode_multipolygon(&clipped_groups),
            });
        }
        // Points/lines survivors: polygons with the class field were replaced above; encode the rest.
        enc.encode_survivors(
            features,
            &candidates,
            Some(field),
            max_features,
            &proj,
            rect,
            opts.dedup,
        );
    } else {
        // Normal path: uniformly sample the candidates down to the budget (never first-N, which
        // clusters spatially and leaves tile-aligned holes) and encode them — no class replacement,
        // so every candidate is a survivor.
        enc.encode_survivors(
            features,
            &candidates,
            None,
            max_features,
            &proj,
            rect,
            opts.dedup,
        );
    }
    if enc.out_features.is_empty() {
        return Vec::new();
    }

    // Assemble Layer { version=15, name=1, features=2*, keys=3*, values=4*, extent=5 }.
    let mut layer_w = PbfWriter::new();
    layer_w.field_varint(15, 2);
    layer_w.field_bytes(1, layer_name.as_bytes());
    for ef in &enc.out_features {
        let mut feat_w = PbfWriter::new();
        feat_w.field_varint(1, ef.fid);
        feat_w.field_packed_u32(2, &ef.tags);
        feat_w.field_varint(3, ef.geom_type as u64);
        feat_w.field_packed_u32(4, &ef.commands);
        layer_w.field_bytes(2, &feat_w.into_bytes());
    }
    for k in &enc.key_list {
        layer_w.field_bytes(3, k.as_bytes());
    }
    for v in &enc.val_list {
        let mut val_w = PbfWriter::new();
        match v {
            Value::Str(s) => val_w.field_bytes(1, s.as_bytes()),
            Value::Num(n) => val_w.field_double(3, *n),
            Value::Null => {}
        }
        layer_w.field_bytes(4, &val_w.into_bytes());
    }
    layer_w.field_varint(5, EXTENT as u64);

    // Tile { layers=3* }.
    let mut tile_w = PbfWriter::new();
    tile_w.field_bytes(3, &layer_w.into_bytes());
    tile_w.into_bytes()
}

/// A canonical string key for value dedup (`Value` has no `Hash`/`Eq` because of the `f64`
/// variant; `Num`'s bit pattern avoids float-equality pitfalls while keeping distinct NaN/±0.0
/// encodings distinct, which is fine — we only need dedup, not numeric equivalence).
pub(crate) fn value_dedup_key(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("s:{s}"),
        Value::Num(n) => format!("d:{}", n.to_bits()),
        Value::Null => "n".to_string(),
    }
}

/// Whether the cell mosaic replaces polygons for a tile at zoom `z`: needs a cell size, a resolved
/// class field, and the zoom within the band. Activation is a per-ZOOM constant (never per-tile) —
/// that is what keeps it seam-free (a per-tile "mosaic if dense" rule would abut a blocky tile with
/// a sharp one). `cell_max_zoom == 0` = every zoom.
fn is_mosaic_active(opts: &MvtOptimizations, z: u32) -> bool {
    opts.cell_units > 0
        && opts.cell_field.is_some()
        && (opts.cell_max_zoom == 0 || z <= opts.cell_max_zoom)
}

/// Whether the same-class dissolve replaces polygons for a tile at zoom `z`: a resolved dissolve field
/// within the zoom band (`dissolve_max_zoom == 0` = every zoom). Per-ZOOM constant → seam-safe.
fn is_dissolve_active(opts: &MvtOptimizations, z: u32) -> bool {
    opts.dissolve_field.is_some() && (opts.dissolve_max_zoom == 0 || z <= opts.dissolve_max_zoom)
}

/// Feature-id base for synthetic mosaic rects. The top bit is set so rects never collide with a
/// real point/line survivor's `fid` on a mixed-geometry tile — real ids come from source attributes
/// (GPKG rowids, GeoJSON ids) that don't set bit 63. MVT ids SHOULD be unique per layer.
const CELL_FID_BASE: u64 = 1 << 63;

/// Intern an attribute KEY into the layer's key pool, returning its index (dedup by name). Used by
/// the mosaic branch; the normal survivor loop inlines the identical logic.
fn intern_key(list: &mut Vec<String>, idx: &mut HashMap<String, u32>, key: &str) -> u32 {
    if let Some(&i) = idx.get(key) {
        return i;
    }
    let i = list.len() as u32;
    list.push(key.to_string());
    idx.insert(key.to_string(), i);
    i
}

/// Intern an attribute VALUE into the layer's value pool (dedup by `value_dedup_key`), returning its
/// index.
fn intern_val(list: &mut Vec<Value>, idx: &mut HashMap<String, u32>, val: &Value) -> u32 {
    let vk = value_dedup_key(val);
    if let Some(&i) = idx.get(&vk) {
        return i;
    }
    let i = list.len() as u32;
    list.push(val.clone());
    idx.insert(vk, i);
    i
}

/// Project + clip + encode one feature's geometry. `None` if the geometry fails to project (a
/// vertex outside the CRS validity domain) or clips away to nothing.
fn encode_feature_geometry(
    proj: &Projector,
    geom: &Geometry,
    rect: [f64; 4],
    dedup: bool,
) -> Option<(u32, Vec<u32>)> {
    match geom {
        Geometry::Point(p) => {
            let [px, py] = to_pixel(proj, *p)?;
            if px < rect[0] || px > rect[2] || py < rect[1] || py > rect[3] {
                return None;
            }
            Some((
                mvtgeom::GEOM_POINT,
                mvtgeom::encode_points(&[to_i32([px, py])]),
            ))
        }
        Geometry::LineString(line) => {
            let proj_line = project_ring(proj, line)?;
            let parts: Vec<Vec<[i32; 2]>> = clip::clip_line(&proj_line, rect)
                .iter()
                .map(|piece| to_i32_ring(piece, dedup))
                .collect();
            if parts.is_empty() {
                return None;
            }
            Some((mvtgeom::GEOM_LINE, mvtgeom::encode_line(&parts)))
        }
        Geometry::Polygon(rings) => {
            let proj_rings = project_rings(proj, rings)?;
            let clipped = clip::clip_polygon(&proj_rings, rect);
            if clipped.is_empty() {
                return None;
            }
            let rings_i32: Vec<Vec<[i32; 2]>> =
                clipped.iter().map(|r| to_i32_ring(r, dedup)).collect();
            Some((mvtgeom::GEOM_POLYGON, mvtgeom::encode_polygon(&rings_i32)))
        }
        Geometry::MultiLineString(parts) => {
            let mut all_parts: Vec<Vec<[i32; 2]>> = Vec::new();
            for part in parts {
                // Skip only the failing part, not the whole feature: one unprojectable vertex
                // (a domain-edge / world-data vertex) must not delete every other part.
                let Some(proj_part) = project_ring(proj, part) else {
                    continue;
                };
                for piece in clip::clip_line(&proj_part, rect) {
                    all_parts.push(to_i32_ring(&piece, dedup));
                }
            }
            if all_parts.is_empty() {
                return None;
            }
            Some((mvtgeom::GEOM_LINE, mvtgeom::encode_line(&all_parts)))
        }
        Geometry::MultiPolygon(polys) => {
            // Each polygon is clipped independently so its exterior/hole grouping survives into
            // `encode_multipolygon`, which restarts the exterior-positive winding per polygon —
            // otherwise a MultiPolygon's 2nd+ piece would be wound like a hole (inverted). All the
            // pieces still emit into one command stream (a feature has one geometry field).
            let mut poly_groups: Vec<Vec<Vec<[i32; 2]>>> = Vec::new();
            for poly in polys {
                // Skip only the failing polygon, not the whole MultiPolygon feature.
                let Some(proj_rings) = project_rings(proj, poly) else {
                    continue;
                };
                let clipped = clip::clip_polygon(&proj_rings, rect);
                if clipped.is_empty() {
                    continue;
                }
                poly_groups.push(clipped.iter().map(|r| to_i32_ring(r, dedup)).collect());
            }
            if poly_groups.is_empty() {
                return None;
            }
            Some((
                mvtgeom::GEOM_POLYGON,
                mvtgeom::encode_multipolygon(&poly_groups),
            ))
        }
    }
}

fn to_pixel(proj: &Projector, p: [f64; 2]) -> Option<[f64; 2]> {
    proj.to_pixel(p[0], p[1]).map(|(x, y)| [x as f64, y as f64])
}

fn project_ring(proj: &Projector, ring: &[[f64; 2]]) -> Option<Vec<[f64; 2]>> {
    ring.iter().map(|p| to_pixel(proj, *p)).collect()
}

fn project_rings(proj: &Projector, rings: &[Vec<[f64; 2]>]) -> Option<Vec<Vec<[f64; 2]>>> {
    rings.iter().map(|r| project_ring(proj, r)).collect()
}

fn to_i32(p: [f64; 2]) -> [i32; 2] {
    [p[0].round() as i32, p[1].round() as i32]
}

/// Round each vertex to the MVT integer grid, and (when `dedup`) drop consecutive duplicates —
/// vertices that land in the same grid cell as their predecessor add a zero-delta command and no
/// visible detail, so at overview zooms a big polygon's thousands of source vertices collapse to a
/// handful (the Phase-1b vertex reduction that keeps seam-free overview tiles browser-light).
/// Sliver-SAFE: rounding is a global function of position, so a vertex shared by two polygons rounds
/// identically in both — their shared border stays coincident (unlike per-feature Douglas-Peucker).
/// Non-consecutive repeats (e.g. a closed ring's first==last) survive; `emit_ring`/`encode_line`
/// drop any ring/part left degenerate (< 3 / < 2 vertices). `dedup = false` (`--no-optimizations`)
/// keeps every rounded vertex — the raw ring, an A/B diagnostic.
pub(crate) fn to_i32_ring(pts: &[[f64; 2]], dedup: bool) -> Vec<[i32; 2]> {
    let mut out: Vec<[i32; 2]> = Vec::with_capacity(pts.len());
    for p in pts {
        let q = to_i32(*p);
        // `dedup` off (`--no-optimizations`) emits the RAW rounded ring — an A/B diagnostic that may
        // leave zero-delta segments a strict MVT decoder rejects. On (default) drops consecutive
        // grid duplicates: the Phase-1b vertex reduction.
        if !dedup || out.last() != Some(&q) {
            out.push(q);
        }
    }
    out
}

/// Positions `[0, n)` to keep so `n` candidates are uniformly sampled down to at most `budget`,
/// evenly spaced across the whole range (a fixed float stride). Keeps ALL positions when
/// `n <= budget`; empty when `budget == 0`. The uniform spread — not a first-N prefix — is what
/// prevents the overview's tile-aligned rectangular holes (see the Phase-2 comment in `encode_tile`).
fn sampled_positions(n: usize, budget: usize) -> Vec<usize> {
    if budget == 0 {
        return Vec::new();
    }
    if n <= budget {
        return (0..n).collect();
    }
    let step = n as f64 / budget as f64; // > 1 here (n > budget)
    let mut out = Vec::with_capacity(budget);
    let mut next = 0.0f64;
    for k in 0..n {
        if (k as f64) >= next {
            out.push(k);
            next += step;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{layer_area_scale, min_area_src_for_zoom, sampled_positions};
    use crate::vector::source::FeatureSource;

    #[test]
    fn min_area_off_when_feature_px_not_positive() {
        // px <= 0 => gate disabled (0.0), whatever the zoom/scale.
        assert_eq!(min_area_src_for_zoom(6, 1.0, 0.0), 0.0);
        assert_eq!(min_area_src_for_zoom(6, 1.0, -3.0), 0.0);
    }

    #[test]
    fn min_area_off_when_scale_not_positive() {
        // A non-computable area scale (degenerate layer) must not gate anything.
        assert_eq!(min_area_src_for_zoom(6, 0.0, 1.0), 0.0);
        assert_eq!(min_area_src_for_zoom(6, -1.0, 1.0), 0.0);
    }

    #[test]
    fn min_area_quarters_each_zoom_in() {
        // A display pixel's ground area quarters per zoom level (linear size halves), so the
        // min-feature-area threshold must quarter too. Mutation-proof: a per-tile or constant
        // threshold would not show this exact 4× ratio.
        let a = min_area_src_for_zoom(6, 1.0, 1.0);
        let b = min_area_src_for_zoom(7, 1.0, 1.0);
        assert!(a > 0.0 && b > 0.0);
        assert!((a / b - 4.0).abs() < 1e-6, "ratio {} not 4", a / b);
    }

    #[test]
    fn min_area_scales_linearly_with_feature_px() {
        let one = min_area_src_for_zoom(6, 1.0, 1.0);
        let four = min_area_src_for_zoom(6, 1.0, 4.0);
        assert!((four / one - 4.0).abs() < 1e-6, "ratio {}", four / one);
    }

    #[test]
    fn min_area_divides_by_area_scale() {
        // Doubling merc-per-source scale halves the source-unit threshold (same on-screen size).
        let s1 = min_area_src_for_zoom(6, 1.0, 1.0);
        let s2 = min_area_src_for_zoom(6, 2.0, 1.0);
        assert!((s1 / s2 - 2.0).abs() < 1e-6, "ratio {}", s1 / s2);
    }

    #[test]
    fn area_scale_equatorial_degree_box() {
        // Source in EPSG:4326: a 1°×1° box at the equator. mercator area ≈ (111319 m)² ≈ 1.239e10,
        // source area = 1 deg² => scale ≈ 1.24e10 merc-m² per deg².
        let scale = layer_area_scale([0.0, 0.0, 1.0, 1.0], [0.0, 0.0, 1.0, 1.0]);
        assert!(
            (1.2e10..1.3e10).contains(&scale),
            "scale {scale} out of expected equatorial range"
        );
    }

    #[test]
    fn area_scale_degenerate_is_zero_so_gate_disables() {
        // A non-computable scale must return 0.0 → `min_area_src_for_zoom` short-circuits to 0.0 →
        // gate OFF (fail-OPEN, keep everything). Returning 1.0 would fail CLOSED on a geographic CRS
        // (threshold in deg² ⇒ drops every feature on Earth). See review finding #2.
        assert_eq!(
            layer_area_scale([0.0, 0.0, 0.0, 0.0], [0.0, 0.0, 10.0, 10.0]),
            0.0
        );
        assert_eq!(
            layer_area_scale([0.0, 0.0, 1.0, 1.0], [5.0, 5.0, 5.0, 5.0]),
            0.0
        );
        assert_eq!(
            layer_area_scale([f64::NAN, 0.0, 1.0, 1.0], [0.0, 0.0, 1.0, 1.0]),
            0.0
        );
        // …and a 0.0 scale disables the gate:
        assert_eq!(min_area_src_for_zoom(6, 0.0, 1.0), 0.0);
    }

    // A minimal in-memory FeatureSource for encoder tests.
    struct VecSource {
        feats: Vec<crate::vector::feature::Feature>,
        extent: [f64; 4],
    }
    impl crate::vector::source::FeatureSource for VecSource {
        fn features(&self) -> &[crate::vector::feature::Feature] {
            &self.feats
        }
        fn full_extent(&self) -> [f64; 4] {
            self.extent
        }
    }

    /// A source-CRS-EPSG:3857 rectangle `[x0,x1]×[y0,y1]` as one polygon feature.
    fn rect_feature(
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        fid: u64,
    ) -> crate::vector::feature::Feature {
        use crate::vector::feature::{Feature, Geometry, Props};
        let ring = vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]];
        Feature::new(Geometry::Polygon(vec![ring]), Props::new(), fid)
    }

    #[test]
    fn min_area_selection_is_seam_free_across_adjacent_tiles() {
        use super::{encode_tile_opt, MvtOptimizations};
        // A big rectangle (area 4e10 merc-m²) straddling the shared edge (x≈0) between the two
        // horizontally-adjacent z6 tiles x=31 and x=32, in row y=24 (both footprints overlap it).
        let big = rect_feature(-100_000.0, 4.6e6, 100_000.0, 4.8e6, 1);
        assert!((big.area - 4e10).abs() < 1.0, "area {}", big.area);
        let src = VecSource {
            feats: vec![big],
            extent: [-100_000.0, 4.6e6, 100_000.0, 4.8e6],
        };
        let grid = crate::tms::preset("WebMercatorQuad", 4096).unwrap();
        // With `area_scale` set to the z6 display-px² area, `min_area_src_for_zoom` returns
        // `min_feature_px` verbatim — so `min_area` below IS the source-area threshold, keeping this
        // test identical to the pre-refactor `encode_tile_budgeted(..., 0, min_area)` form.
        let px_area = super::min_area_src_for_zoom(6, 1.0, 1.0);
        let enc = |x: u32, min_area: f64| {
            let opts = MvtOptimizations {
                max_features: 0,
                min_feature_px: min_area,
                area_scale: px_area,
                ..MvtOptimizations::defaults()
            };
            encode_tile_opt(src.features(), &grid, 6, x, 24, "EPSG:3857", "t", &opts)
        };

        // Threshold BELOW the feature's area → kept, so BOTH adjacent tiles emit it (non-empty).
        assert!(
            !enc(31, 1e10).is_empty(),
            "left tile dropped a kept feature"
        );
        assert!(
            !enc(32, 1e10).is_empty(),
            "right tile dropped a kept feature"
        );

        // Threshold ABOVE the feature's area → dropped, so BOTH tiles are empty (the only feature
        // is gone). If the gate were per-tile / mis-computed, one side could keep it → a seam.
        assert!(
            enc(31, 1e11).is_empty(),
            "left tile kept a dropped feature (seam)"
        );
        assert!(
            enc(32, 1e11).is_empty(),
            "right tile kept a dropped feature (seam)"
        );

        // Gate off (0.0) must keep it regardless — the default, byte-identical behaviour.
        assert!(!enc(31, 0.0).is_empty() && !enc(32, 0.0).is_empty());
    }

    #[test]
    fn zero_area_features_are_never_size_gated() {
        // Points/lines have zero area; the min-feature-SIZE gate is a polygon-coverage concept and
        // must NOT drop them, or `--mvt-min-feature-px` silently blanks a roads/airports layer at
        // EVERY zoom (review finding #1). A huge threshold must still emit a point inside the tile.
        use super::{encode_tile_opt, MvtOptimizations};
        use crate::vector::feature::{Feature, Geometry, Props};
        let pt = Feature::new(Geometry::Point([50_000.0, 4.7e6]), Props::new(), 1);
        assert_eq!(pt.area, 0.0);
        let src = VecSource {
            feats: vec![pt],
            extent: [0.0, 4.6e6, 100_000.0, 4.8e6],
        };
        let grid = crate::tms::preset("WebMercatorQuad", 4096).unwrap();
        // Tile (6,32,24) contains x=50000 (x-range ≈[0,626172]) and y=4.7e6 (row 24). A huge
        // min-feature threshold (min_feature_px 1e15 × area_scale) must still keep the zero-area point.
        let opts = MvtOptimizations {
            max_features: 0,
            min_feature_px: 1e15,
            area_scale: 1.0,
            ..MvtOptimizations::defaults()
        };
        let tile = encode_tile_opt(src.features(), &grid, 6, 32, 24, "EPSG:3857", "t", &opts);
        assert!(
            !tile.is_empty(),
            "a zero-area point was dropped by the size gate"
        );
    }

    #[test]
    fn encode_tile_opt_defaults_equals_encode_tile() {
        use super::{encode_tile, encode_tile_opt, MvtOptimizations};
        // The refactor's contract: the `defaults()` opts path is byte-for-byte the plain
        // `encode_tile` wrapper (whose exact bytes the tests/mvt_tile.rs goldens pin). Guards the
        // wrapper from silently drifting away from `defaults()`.
        let src = VecSource {
            feats: vec![rect_feature(-100_000.0, 4.6e6, 100_000.0, 4.8e6, 1)],
            extent: [-100_000.0, 4.6e6, 100_000.0, 4.8e6],
        };
        let grid = crate::tms::preset("WebMercatorQuad", 4096).unwrap();
        let a = encode_tile(&src, &grid, 6, 32, 24, "EPSG:3857", "t");
        let b = encode_tile_opt(
            src.features(),
            &grid,
            6,
            32,
            24,
            "EPSG:3857",
            "t",
            &MvtOptimizations::defaults(),
        );
        assert!(!a.is_empty(), "fixture should produce a non-empty tile");
        assert_eq!(
            a, b,
            "defaults() path must be byte-identical to encode_tile"
        );
    }

    #[test]
    fn no_optimizations_dedup_off_changes_tile_bytes() {
        use super::{encode_tile_opt, MvtOptimizations};
        use crate::vector::feature::{Feature, Geometry, Props};
        // A polygon whose first two vertices are 1 m apart → they round to the SAME z6 grid cell, so
        // the default dedup drops one but `--no-optimizations` (dedup=false) keeps both (a zero-delta
        // segment `emit_ring` does NOT strip). Pins that `dedup` flows end-to-end through
        // encode_tile_opt — not just `to_i32_ring`.
        let ring = vec![
            [0.0, 4.70e6],
            [1.0, 4.70e6 + 1.0], // ~1 m from the previous vertex → same MVT cell at z6
            [3.0e5, 4.70e6],
            [3.0e5, 4.80e6],
            [0.0, 4.80e6],
            [0.0, 4.70e6],
        ];
        let feat = Feature::new(Geometry::Polygon(vec![ring]), Props::new(), 1);
        let src = VecSource {
            feats: vec![feat],
            extent: [0.0, 4.70e6, 3.0e5, 4.80e6],
        };
        let grid = crate::tms::preset("WebMercatorQuad", 4096).unwrap();
        let enc = |dedup: bool| {
            let opts = MvtOptimizations {
                dedup,
                ..MvtOptimizations::defaults()
            };
            encode_tile_opt(src.features(), &grid, 6, 32, 24, "EPSG:3857", "t", &opts)
        };
        let on = enc(true);
        let off = enc(false);
        assert!(!on.is_empty() && !off.is_empty(), "both must encode");
        assert_ne!(
            on, off,
            "dedup off must change the bytes (keeps the duplicate vertex)"
        );
        assert!(
            off.len() > on.len(),
            "dedup off retains the duplicate → more bytes ({} !> {})",
            off.len(),
            on.len()
        );
    }

    #[test]
    fn is_mosaic_active_gates_on_units_field_and_zoom() {
        use super::{is_mosaic_active, MvtOptimizations};
        let base = MvtOptimizations {
            cell_units: 128,
            cell_field: Some("cls".into()),
            cell_max_zoom: 0,
            ..MvtOptimizations::defaults()
        };
        // All conditions met, cutoff 0 = every zoom → active at any z.
        assert!(is_mosaic_active(&base, 0));
        assert!(is_mosaic_active(&base, 20));
        // No cell size (cell_px not set / rounded to 0) → off.
        assert!(!is_mosaic_active(
            &MvtOptimizations {
                cell_units: 0,
                ..base.clone()
            },
            0
        ));
        // No resolved field (layer lacks it) → off.
        assert!(!is_mosaic_active(
            &MvtOptimizations {
                cell_field: None,
                ..base.clone()
            },
            0
        ));
        // Zoom cutoff: active at z ≤ cutoff, inactive above (the seam-safe per-zoom band).
        let banded = MvtOptimizations {
            cell_max_zoom: 6,
            ..base.clone()
        };
        assert!(is_mosaic_active(&banded, 6));
        assert!(!is_mosaic_active(&banded, 7));
    }

    #[test]
    fn to_i32_ring_drops_consecutive_grid_duplicates() {
        use super::to_i32_ring;
        // Vertices that round to the SAME MVT grid cell collapse to one; distinct cells survive.
        // rounds: [0,0],[0,0],[0,0],[6,8],[6,8] → dedup → [0,0],[6,8]. This is the Phase-1b vertex
        // reduction that makes the seam-free selection browser-light, and it is SLIVER-SAFE because
        // rounding is a GLOBAL function of position — two polygons sharing a vertex round it
        // identically, so their shared border stays coincident (unlike per-feature simplification).
        let pts = [[0.1, 0.2], [0.3, 0.4], [0.0, 0.0], [5.6, 7.8], [5.7, 7.9]];
        assert_eq!(to_i32_ring(&pts, true), vec![[0, 0], [6, 8]]);
    }

    #[test]
    fn to_i32_ring_no_dedup_keeps_consecutive_duplicates() {
        use super::to_i32_ring;
        // With dedup OFF (`--no-optimizations`) the raw rounded ring is emitted — consecutive
        // vertices rounding to the same grid cell are RETAINED (A/B diagnostic; may hold zero-delta
        // segments). Same input as the dedup-ON test, opposite result.
        let pts = [[0.1, 0.2], [0.3, 0.4], [0.0, 0.0], [5.6, 7.8], [5.7, 7.9]];
        assert_eq!(
            to_i32_ring(&pts, false),
            vec![[0, 0], [0, 0], [0, 0], [6, 8], [6, 8]]
        );
    }

    #[test]
    fn to_i32_ring_keeps_non_consecutive_repeats() {
        use super::to_i32_ring;
        // Only IMMEDIATE consecutive duplicates collapse — a revisited cell later in the ring stays
        // (e.g. a closed ring's first==last must survive so ClosePath still finds the closure).
        let pts = [[0.0, 0.0], [10.0, 10.0], [0.0, 0.0]];
        assert_eq!(to_i32_ring(&pts, true), vec![[0, 0], [10, 10], [0, 0]]);
    }

    #[test]
    fn keeps_all_when_within_budget() {
        assert_eq!(sampled_positions(5, 20), (0..5).collect::<Vec<_>>());
        assert_eq!(sampled_positions(20, 20), (0..20).collect::<Vec<_>>());
        assert_eq!(sampled_positions(0, 20), Vec::<usize>::new());
    }

    #[test]
    fn spreads_uniformly_across_the_full_range_when_oversized() {
        // 100 -> 10: evenly spaced across the WHOLE range. A first-N cut would return 0..10 and
        // never exceed 9 — this asserts the last kept position is in the far end (mutation-proving).
        let keep = sampled_positions(100, 10);
        assert!(keep.len() >= 9 && keep.len() <= 10, "kept {}", keep.len());
        assert_eq!(keep[0], 0, "first pick is the range start");
        assert!(
            *keep.last().unwrap() >= 80,
            "last pick {} not near the end — sampling is clustered, not uniform",
            keep.last().unwrap()
        );
        for w in keep.windows(2) {
            let gap = w[1] - w[0];
            assert!(
                (9..=11).contains(&gap),
                "gap {gap} not ~step (uneven spacing)"
            );
        }
    }

    #[test]
    fn budget_zero_is_empty() {
        assert!(sampled_positions(100, 0).is_empty());
    }
}
