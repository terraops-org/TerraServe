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
use crate::vector::pmtiles::encoding::{Effort, TileEncoding};

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
/// The per-grid archive (or write-through overlay) that answers a request, found the way the
/// grid itself was: by the id as requested, then the PUBLISHED grid it resolved to, then that
/// grid's base name with any `_{px}` size suffix stripped.
///
/// Why all three: an MVT archive files itself under the grid id stamped at bake time, which for
/// the defaults (`build-pmtiles --grid WebMercatorQuad`) is the bare `WebMercatorQuad`. A layer
/// served at the default `--tms-tile-px 512` PUBLISHES `WebMercatorQuad_512`, and that is the id
/// the X-ray viewer asks for. Looking the archive up by the raw requested id meant a request for
/// `WebMercatorQuad` hit the archive while the viewer's `WebMercatorQuad_512` missed it and was
/// rendered live, every time. Found on the live cos2023 demo 2026-09-11: the same z7 tile was 983 KB
/// in 0.35 s by one name and 42 MB in 17 s by the other. The raster loader already resolved its
/// archives this way (`layer/mod.rs`, `strip_size_suffix`); MVT now matches it.
///
/// Stripping only a NUMERIC suffix keeps distinct grids apart: `WorldCRS84Quad` can never resolve to
/// `WebMercatorQuad`, and every `_{px}` variant of one grid shares its tile extents, which is what
/// makes serving an MVT tile (pixel-size independent) from the base grid's archive correct.
fn for_grid<'a, V>(
    map: &'a std::collections::BTreeMap<String, V>,
    requested: &str,
    grid: &TileMatrixSet,
) -> Option<&'a V> {
    map.get(requested)
        .or_else(|| map.get(grid.id.as_str()))
        .or_else(|| map.get(crate::tms::strip_size_suffix(&grid.id)))
}

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

/// A tile body plus the encoding its bytes are already in.
///
/// The PMTiles archives store their MVT compressed, and the serving path used to inflate every
/// hit only for the response to go out uncompressed. Carrying the encoding alongside the bytes lets
/// an archive hit reach a client that accepts that encoding untouched, while a tile in some other
/// encoding is transcoded once (and cached) rather than mislabelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileBody {
    pub bytes: Vec<u8>,
    /// The encoding `bytes` are in; the response's `Content-Encoding` says exactly this.
    pub encoding: TileEncoding,
}

impl TileBody {
    /// Plain, uncompressed MVT -- what the live encoder produces.
    pub fn identity(bytes: Vec<u8>) -> Self {
        TileBody {
            bytes,
            encoding: TileEncoding::Identity,
        }
    }

    pub fn is_gzip(&self) -> bool {
        self.encoding == TileEncoding::Gzip
    }
}

/// The content-codings a client offered with a non-zero q (`Accept-Encoding`). Identity is always
/// acceptable. `From<bool>` keeps the pre-0.3.3 meaning, where the only question was "gzip or not".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Accepted {
    pub gzip: bool,
    pub br: bool,
    pub zstd: bool,
}

impl From<bool> for Accepted {
    fn from(gzip: bool) -> Self {
        Accepted {
            gzip,
            ..Default::default()
        }
    }
}

impl Accepted {
    pub fn accepts(self, e: TileEncoding) -> bool {
        match e {
            TileEncoding::Identity => true,
            TileEncoding::Gzip => self.gzip,
            TileEncoding::Brotli => self.br,
            TileEncoding::Zstd => self.zstd,
        }
    }

    /// Which encoding to send a blob stored as `stored` in.
    ///
    /// Browsers send `gzip, deflate, br, zstd` with no q values, so the tie rule IS the policy:
    /// 1. the stored encoding, when accepted: free pass-through, the bytes on disk;
    /// 2. else the server's live preference (`--tile-encoding`), when accepted;
    /// 3. else the most widely supported accepted coding, gzip, then br, then zstd;
    /// 4. else identity.
    ///
    /// A stored gzip tile therefore stays gzip for a client that also takes br: it is never
    /// transcoded "up" at request time.
    pub fn choose(self, stored: TileEncoding, preferred: TileEncoding) -> TileEncoding {
        if self.accepts(stored) {
            return stored;
        }
        if preferred != TileEncoding::Identity && self.accepts(preferred) {
            return preferred;
        }
        [TileEncoding::Gzip, TileEncoding::Brotli, TileEncoding::Zstd]
            .into_iter()
            .find(|e| self.accepts(*e))
            .unwrap_or(TileEncoding::Identity)
    }
}

/// Turn a STORED blob into a response body, given how it was stored and what this client accepts.
///
/// `stored` must come from the archive header (`tile_compression`) or from the code that
/// compressed the blob, never from sniffing: brotli has no magic bytes, and labelling a blob with
/// the wrong encoding hands the client something it cannot decode behind a 200.
///
/// A transcode to a compressed encoding is cached under `{key}#{token}` when `--mvt-cache` is on,
/// so a brotli archive served to a gzip-only client pays the transcode once per tile. An inflate
/// to identity is not cached (it is the cheap direction, and it was never cached before).
fn deliver(
    state: &ServeState,
    key: &str,
    bytes: Vec<u8>,
    stored: TileEncoding,
    accepted: Accepted,
) -> Result<TileBody, String> {
    let want = accepted.choose(stored, state.tile_encoding);
    if want == stored {
        return Ok(TileBody {
            bytes,
            encoding: stored,
        });
    }
    let transcode = || -> Result<Vec<u8>, String> {
        let raw = stored.decompress(&bytes)?;
        Ok(want.compress(&raw, Effort::Live, None))
    };
    let out = match (&state.mvt_cache, want.http_token()) {
        (Some(cache), Some(token)) => cache
            .try_get_with(format!("{key}#{token}"), || {
                transcode().map(std::sync::Arc::new)
            })
            .map(|a| (*a).clone())
            .map_err(|e| (*e).clone())?,
        _ => transcode()?,
    };
    Ok(TileBody {
        bytes: out,
        encoding: want,
    })
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
    accepted: impl Into<Accepted>,
) -> Result<TileBody, (u16, String)> {
    let accepted: Accepted = accepted.into();
    let key = format!("{layer}/{tms_id}/{z}/{x}/{y}");
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
    if let Some(ov) = for_grid(&l.overlay, tms_id, &grid) {
        // Read the STORED blob (plus how it was stored) rather than an inflated copy, so a
        // gzip-capable client can be handed it verbatim. `get_raw` reports the compression of
        // whichever source answered -- the overlay log is always gzip, the base archive may not be.
        match ov.get_raw(z, x, y) {
            Ok(Some((bytes, comp))) => match TileEncoding::from_pmtiles(comp)
                .and_then(|stored| deliver(state, &key, bytes, stored, accepted))
            {
                Ok(body) => return Ok(body),
                // A blob we cannot decode is a real failure, not a reason to silently re-render:
                // fall through to the live path, same as any other overlay read error.
                Err(e) => eprintln!("overlay decode {z}/{x}/{y}: {e}"),
            },
            Ok(None) => {}
            Err(e) => eprintln!("overlay read {z}/{x}/{y}: {e}"),
        }
        let opts = MvtOptimizations::for_layer(state, v);
        let vs = v.source_for_zoom(z);
        let batch =
            features_for_tile(&vs, &grid, z, x, y, &l.src_crs, &opts).map_err(read_failed)?;
        let live = encode_tile_opt(batch.as_slice(), &grid, z, x, y, &l.src_crs, &l.name, &opts);
        if live.is_empty() {
            return Ok(TileBody::identity(live));
        }
        // One gzip serves both purposes: the overlay log stores gzip (its compaction writes a
        // gzip archive), and a gzip-capable client can have the same bytes.
        let gz = crate::vector::pmtiles::codec::gzip(&live);
        if !ov.is_compacting() {
            let id = crate::vector::pmtiles::zxy_to_tileid(z, x, y);
            let _ = ov.put(id, &gz); // best-effort
        }
        if accepted.choose(TileEncoding::Gzip, state.tile_encoding) == TileEncoding::Identity {
            return Ok(TileBody::identity(live));
        }
        return deliver(state, &key, gz, TileEncoding::Gzip, accepted)
            .map_err(|e| (500u16, format!("mvt transcode: {e}")));
    }
    // Archive-first (opt-in): a hit is served straight from the pre-built PMTiles archive for the
    // REQUESTED grid (`tms_id`); a miss (or no archive registered for this grid) falls through to the
    // live encode path below. The reader returns raw (decompressed) MVT, the same shape as the live
    // path. Selecting by `tms_id` rather than "the" archive is what makes per-grid PMTiles work: a
    // layer with e.g. a WebMercatorQuad archive AND a swissLV95 archive serves each grid from its own
    // file (design commitment 1: never mixed in one archive).
    if let Some(reader) = for_grid(&l.pmtiles, tms_id, &grid) {
        // Same pass-through as the overlay branch: the archive already holds gzip, so an inflate
        // here would only be undone by the wire. `tile_compression()` is the archive's own header,
        // so a `COMPRESSION_NONE` archive still serves identity correctly.
        match reader.get_raw(z, x, y) {
            Ok(Some(bytes)) => match TileEncoding::from_pmtiles(reader.tile_compression())
                .and_then(|stored| deliver(state, &key, bytes, stored, accepted))
            {
                Ok(body) => return Ok(body),
                Err(e) => eprintln!("pmtiles decode {z}/{x}/{y}: {e}"), // degrade to live encode
            },
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
    // A live tile is gzip'd once and CACHED gzip'd, which is both the smaller cache entry and the
    // encoding almost every caller wants: an archive hit has always passed its stored gzip through,
    // while anything encoded here used to go out raw even to a client asking for gzip (a vida z12
    // tile: 1,028,450 B raw against 466,228 B gzip'd). Compressing before the cache means a warm
    // tile is never re-compressed; a client that does not offer gzip is served an inflate of the
    // same bytes, so both representations come from one cache entry and cannot disagree.
    //
    // Level: the flate2 default (6). On a real 246 KB vida tile that is 7.9 ms against 3.2 ms at
    // level 1 for 6.5% more bytes, and the encode that produced the tile costs far more than
    // either, so the bytes are worth more than the milliseconds here.
    //
    // 0.3.3: the cached encoding is `--tile-encoding` (default gzip, so the default is byte-identical
    // to 0.3.2). With `br` a live tile is brotli 5, measured smaller AND faster than gzip 6 on real
    // tiles; a client that does not take the cached encoding gets a transcode, cached per encoding.
    let live_enc = state.tile_encoding;
    let blob = cached_or_encode(state, &l.name, tms_id, z, x, y, || {
        let live = encode_tile_opt(batch.as_slice(), &grid, z, x, y, &l.src_crs, &l.name, &opts);
        // An empty tile stays an empty body: a valid, cheap 200 that says "nothing here", rather
        // than the 20-byte envelope of nothing.
        if live.is_empty() {
            live
        } else {
            live_enc.compress(&live, Effort::Live, None)
        }
    });
    if blob.is_empty() {
        return Ok(TileBody::identity(blob));
    }
    deliver(state, &key, blob, live_enc, accepted)
        .map_err(|e| (500u16, format!("mvt transcode: {e}")))
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
/// (as of 0.3.2 the live path stores GZIP'd bytes here, so a warm tile is never re-compressed and
/// the cache holds roughly three times as many tiles per MiB)
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
        // A style layer that names a `source-layer` is served to that layer only; an untagged one
        // to every layer (see `mvt_style_for_layer`).
        Some(v @ (serde_json::Value::Array(_) | serde_json::Value::Object(_))) => {
            mvt_style_for_layer(v, layer)
        }
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

/// The style layers of an operator `--mvt-style` document that apply to the served `layer`,
/// plus the document's `metadata`. A style layer that names a `source-layer` (plain MapLibre;
/// nothing TerraServe-specific) is served ONLY to the served layer of that name, so ONE file can
/// theme every layer of a multi-layer server. A style layer without one keeps the original
/// single-layer convention and is served to every layer, `source-layer` filled in by the handler.
/// A bare `[...]` array is accepted as the layer list with no metadata.
pub fn mvt_style_for_layer(
    style: &serde_json::Value,
    layer: &str,
) -> (Vec<serde_json::Value>, serde_json::Value) {
    let (all, metadata) = match style {
        serde_json::Value::Array(arr) => (arr.clone(), serde_json::Value::Null),
        serde_json::Value::Object(obj) => (
            obj.get("layers")
                .and_then(|l| l.as_array())
                .cloned()
                .unwrap_or_default(),
            obj.get("metadata")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        ),
        _ => (Vec::new(), serde_json::Value::Null),
    };
    let layers = all
        .into_iter()
        .filter(|l| match l.get("source-layer").and_then(|s| s.as_str()) {
            None => true,
            Some(s) => s == layer,
        })
        .collect();
    (layers, metadata)
}

/// [`mvt_style_fields`] restricted to the style layers that [`mvt_style_for_layer`] would serve
/// to `layer` — what the startup column warning must use, or a five-layer style file reports every
/// layer's fields against every other layer.
pub fn mvt_style_fields_for_layer(
    style: &serde_json::Value,
    layer: &str,
) -> std::collections::BTreeSet<String> {
    let (layers, _) = mvt_style_for_layer(style, layer);
    let mut out = std::collections::BTreeSet::new();
    for l in &layers {
        collect_get_fields(l, &mut out);
    }
    out
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
    use super::{mvt_style_fields_for_layer, mvt_style_for_layer};

    const MULTI: &str = r##"{
        "layers": [
          { "id": "roads-line", "type": "line", "source-layer": "roads",
            "paint": { "line-color": ["match", ["get", "highway"], "motorway", "#f00", "#888"] } },
          { "id": "landuse-fill", "type": "fill", "source-layer": "landuse",
            "paint": { "fill-color": ["match", ["get", "landuse"], "forest", "#0a0", "#ccc"] } },
          { "id": "any-label", "type": "symbol",
            "layout": { "text-field": ["get", "name"] } }
        ],
        "metadata": { "glow": true } }"##;

    #[test]
    fn a_style_layer_tagged_with_a_source_layer_is_served_only_to_that_layer() {
        // One --mvt-style file, five served layers: a MapLibre style layer that names its
        // `source-layer` belongs to that served layer alone; an untagged one (the pre-existing
        // single-layer convention) still goes to everybody. Metadata rides along unchanged.
        let v: serde_json::Value = serde_json::from_str(MULTI).unwrap();
        let (roads, md) = mvt_style_for_layer(&v, "roads");
        let ids: Vec<&str> = roads.iter().map(|l| l["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["roads-line", "any-label"]);
        assert_eq!(md["glow"], serde_json::json!(true));
        let (water, _) = mvt_style_for_layer(&v, "water");
        let ids: Vec<&str> = water.iter().map(|l| l["id"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            ["any-label"],
            "an unnamed layer gets only the untagged entries"
        );
    }

    #[test]
    fn an_untagged_style_still_reaches_every_layer_exactly_as_before() {
        // The cos2023 / vida / swiss styles carry no `source-layer`; a bare array form is legal
        // too. Both must serve to any layer unchanged, or three live demos lose their palette.
        let v: serde_json::Value =
            serde_json::from_str(r#"[{"id":"fill","type":"fill"},{"id":"line","type":"line"}]"#)
                .unwrap();
        let (layers, md) = mvt_style_for_layer(&v, "whatever");
        assert_eq!(layers.len(), 2);
        assert!(md.is_null());
    }

    #[test]
    fn style_fields_are_reported_per_served_layer_not_across_the_whole_file() {
        // The startup warning must not tell `roads` it lacks `landuse`: only the fields read by
        // style layers that apply to a served layer count against it.
        let v: serde_json::Value = serde_json::from_str(MULTI).unwrap();
        let roads = mvt_style_fields_for_layer(&v, "roads");
        assert_eq!(roads.iter().collect::<Vec<_>>(), ["highway", "name"]);
        let landuse = mvt_style_fields_for_layer(&v, "landuse");
        assert_eq!(landuse.iter().collect::<Vec<_>>(), ["landuse", "name"]);
        let places = mvt_style_fields_for_layer(&v, "places");
        assert_eq!(places.iter().collect::<Vec<_>>(), ["name"]);
    }

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
                zoom_sources: Vec::new(),
            }),
            pmtiles: std::collections::BTreeMap::new(),
            raster_pmtiles: std::collections::BTreeMap::new(),
            overlay: std::collections::BTreeMap::new(),
        };

        let st = ServeState::new(vec![layer], "http://h/wms".into(), 16);
        let bytes = super::render_mvt_tile(&st, "mini", "testgrid", 0, 0, 0, false)
            .expect("custom grid 'testgrid' should resolve, not 404")
            .bytes;
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
                zoom_sources: Vec::new(),
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
                zoom_sources: Vec::new(),
            }),
            pmtiles: std::collections::BTreeMap::new(),
            raster_pmtiles: std::collections::BTreeMap::new(),
            overlay: std::collections::BTreeMap::new(),
        };

        let st = ServeState::new(vec![layer], "http://h/wms".into(), 16);

        // WorldCRS84Quad z0: matrix_w=2, matrix_h=1; col 1 = [0,180] x [-90,90] (the whole eastern
        // hemisphere) — covers lon 8.2 / lat 46.8 with no per-feature arithmetic.
        let crs84_bytes = super::render_mvt_tile(&st, "mini", "WorldCRS84Quad", 0, 1, 0, false)
            .expect("WorldCRS84Quad z0/1/0 should render")
            .bytes;
        assert!(
            dec::feature_with_str_prop(&crs84_bytes, "name", "swiss_test"),
            "feature must appear in the WorldCRS84Quad (CRS84 baseline) tile"
        );

        // swissLV95 z0: matrixWidth=matrixHeight=1 — the single z0/0/0 tile covers the whole
        // official CH extent [2420000,1030000,2900000,1350000], guaranteed to contain the
        // reprojected feature.
        let lv95_bytes = super::render_mvt_tile(&st, "mini", "swissLV95", 0, 0, 0, false)
            .expect("swissLV95 z0/0/0 should render")
            .bytes;
        assert!(
            dec::feature_with_str_prop(&lv95_bytes, "name", "swiss_test"),
            "feature must appear in the swissLV95 (EPSG:2056, reprojected 4326->2056) tile"
        );
    }

    /// 0.3.2: a LIVE-encoded tile must go out gzip'd when the client accepts it.
    ///
    /// Until now only an archive hit passed its stored gzip through; anything the engine encoded
    /// on the spot went out as raw identity bytes even to a browser that asked for gzip. Measured
    /// on the live demos, a vida z12 tile is 1,028,450 B raw and 466,228 B gzip'd, and every tile
    /// above an archive's max zoom pays that. The traefik `tiles-compress` middleware is the
    /// stopgap this test exists to retire.
    ///
    /// The three properties that matter: the gzip response inflates to EXACTLY the identity bytes
    /// (same map, not a cheaper one), a client that does not offer gzip still gets identity, and
    /// the byte cache cannot mix the two encodings whichever order the requests arrive in.
    #[test]
    fn a_live_encoded_tile_is_gzipped_when_the_client_accepts_it() {
        let st = live_only_state(Some(64));
        let plain = super::render_mvt_tile(&st, "mini", "WorldCRS84Quad", 0, 1, 0, false)
            .expect("z0/1/0 renders");
        assert!(
            !plain.is_gzip(),
            "a client that did not offer gzip gets identity"
        );
        assert!(
            !plain.bytes.is_empty(),
            "the fixture tile must carry features"
        );

        let zipped = super::render_mvt_tile(&st, "mini", "WorldCRS84Quad", 0, 1, 0, true)
            .expect("z0/1/0 renders");
        assert!(
            zipped.is_gzip(),
            "a gzip-capable client must get Content-Encoding: gzip"
        );
        assert_eq!(
            crate::vector::pmtiles::codec::gunzip(&zipped.bytes).expect("valid gzip"),
            plain.bytes,
            "the gzip body must inflate to exactly the identity bytes"
        );
        // No size assertion here: this fixture tile is a few dozen bytes, and gzip's header
        // costs more than it saves at that size. The win is on real tiles, where it is 2.2x
        // (vida z12: 1,028,450 B raw against 466,228 B), and it is measured on the live demos
        // rather than asserted on a toy.

        // The cache was warmed by the identity request above. Warm it the other way round too:
        // a cache that stored one encoding and handed it out under the other flag would serve
        // gzip bytes labelled identity, which is a 200 the client cannot decode.
        let st2 = live_only_state(Some(64));
        let zipped_first = super::render_mvt_tile(&st2, "mini", "WorldCRS84Quad", 0, 1, 0, true)
            .expect("z0/1/0 renders");
        let plain_after = super::render_mvt_tile(&st2, "mini", "WorldCRS84Quad", 0, 1, 0, false)
            .expect("z0/1/0 renders");
        assert!(
            zipped_first.is_gzip() && !plain_after.is_gzip(),
            "each request gets its own encoding"
        );
        assert_eq!(
            plain_after.bytes, plain.bytes,
            "identity after a gzip hit is unchanged"
        );
        assert_eq!(
            crate::vector::pmtiles::codec::gunzip(&zipped_first.bytes).unwrap(),
            plain.bytes,
            "gzip from a cold cache inflates to the same bytes"
        );

        // And with no cache at all, both encodings still come out right.
        let nc = live_only_state(None);
        let a = super::render_mvt_tile(&nc, "mini", "WorldCRS84Quad", 0, 1, 0, true).unwrap();
        let b = super::render_mvt_tile(&nc, "mini", "WorldCRS84Quad", 0, 1, 0, false).unwrap();
        assert!(a.is_gzip() && !b.is_gzip());
        assert_eq!(
            crate::vector::pmtiles::codec::gunzip(&a.bytes).unwrap(),
            b.bytes
        );
    }

    /// One tiny vector layer on `WorldCRS84Quad`, no archive and no overlay, so every request
    /// takes the LIVE encode path. `cache_mib` mirrors `--mvt-cache`.
    fn live_only_state(cache_mib: Option<u64>) -> crate::server::ServeState {
        use crate::server::{Layer, ServeState, VectorLayer};
        use crate::vector::geojson::GeoJsonSource;
        use crate::vector::shape::Shaper;
        use crate::vector::source::{FeatureSource, VectorSource};
        use std::sync::Arc;

        let geojson = r#"{
          "type": "FeatureCollection",
          "features": [
            { "type": "Feature", "properties": { "name": "one" },
              "geometry": { "type": "Polygon", "coordinates": [[
                [8.15, 46.75], [8.25, 46.75], [8.25, 46.85], [8.15, 46.85], [8.15, 46.75]]] } },
            { "type": "Feature", "properties": { "name": "two" },
              "geometry": { "type": "Polygon", "coordinates": [[
                [9.15, 45.75], [9.45, 45.75], [9.45, 46.05], [9.15, 46.05], [9.15, 45.75]]] } }
          ]
        }"#;
        let src = Arc::new(GeoJsonSource::from_str(geojson).unwrap());
        let style = Style::load("fixtures/styles/airports.vec.json").unwrap();
        let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
        let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
        let ext = src.full_extent();
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
            grids: vec![crate::server::PublishedGrid {
                tms: crate::tms::TileMatrixSet::world_crs84_quad(256),
                data_bounds: None,
            }],
            vector: Some(VectorLayer {
                fields: super::feature_field_schema(src.as_ref()),
                area_scale: crate::vector::mvt::layer_area_scale(ext, ext),
                min_feature_px: 0.0,
                source: VectorSource::LoadAll(src),
                style,
                shaper,
                lod: None,
                zoom_sources: Vec::new(),
            }),
            pmtiles: std::collections::BTreeMap::new(),
            raster_pmtiles: std::collections::BTreeMap::new(),
            overlay: std::collections::BTreeMap::new(),
        };
        let mut st = ServeState::new(vec![layer], "http://h/wms".into(), 16);
        st.mvt_cache = cache_mib.map(super::build_byte_cache);
        st
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
                zoom_sources: Vec::new(),
            }),
            pmtiles,
            raster_pmtiles: std::collections::BTreeMap::new(),
            overlay: std::collections::BTreeMap::new(),
        };
        let st = ServeState::new(vec![layer], "http://h/wms".into(), 16);

        // End-to-end through `render_mvt_tile`: each grid's request must be served from ITS OWN
        // archive, not the other's (and not a live encode — a live tile would be valid MVT bytes,
        // never the literal `MERCATOR_TILE`/`CRS84_TILE` markers).
        let got_a = super::render_mvt_tile(&st, "mini", "WebMercatorQuad", 0, 0, 0, false)
            .unwrap()
            .bytes;
        let got_b = super::render_mvt_tile(&st, "mini", "WorldCRS84Quad", 0, 0, 0, false)
            .unwrap()
            .bytes;
        assert_eq!(got_a, b"MERCATOR_TILE");
        assert_eq!(got_b, b"CRS84_TILE");
        assert_ne!(got_a, got_b);

        // The size-suffixed variant of a grid must be answered by the archive baked on its base
        // name. This is the live cos2023 bug of 2026-09-11: the archive was baked on the default
        // `WebMercatorQuad`, the layer is served at the default 512 px so it PUBLISHES
        // `WebMercatorQuad_512`, and the X-ray viewer requests that id. The lookup used the raw
        // requested id, so every viewer request missed the archive and was encoded live (a z7
        // tile: 42 MB in 17 s instead of 983 KB in 0.35 s). A live encode here would return real
        // MVT bytes built from the fixture, never the literal marker, so this separates the two.
        let got_a512 = super::render_mvt_tile(&st, "mini", "WebMercatorQuad_512", 0, 0, 0, false)
            .unwrap()
            .bytes;
        assert_eq!(
            got_a512, b"MERCATOR_TILE",
            "WebMercatorQuad_512 must be served from the WebMercatorQuad archive, not encoded live"
        );
        // ...and stripping a size suffix must never cross into a different grid's archive.
        let got_b512 = super::render_mvt_tile(&st, "mini", "WorldCRS84Quad_512", 0, 0, 0, false)
            .unwrap()
            .bytes;
        assert_eq!(got_b512, b"CRS84_TILE");

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
    /// 0.3.3: brotli and zstd archives, and `--tile-encoding`, negotiated per client.
    ///
    /// The cases that matter: a stored encoding the client takes goes out as the bytes on disk; one
    /// it does not take is transcoded ONCE (cached, its own ETag-bearing representation) and never
    /// mislabelled; a stored gzip tile is not transcoded "up" for a client that also takes br; and
    /// every representation inflates to the same tile.
    #[test]
    fn brotli_and_zstd_archives_are_negotiated_per_client() {
        use crate::server::{Layer, ServeState, VectorLayer};
        use crate::vector::geojson::GeoJsonSource;
        use crate::vector::pmtiles::encoding::{Effort, TileEncoding};
        use crate::vector::pmtiles::read::PmtilesReader;
        use crate::vector::pmtiles::write::{HeaderFields, PmtilesWriter, TILE_TYPE_MVT};
        use crate::vector::pmtiles::zxy_to_tileid;
        use crate::vector::shape::Shaper;
        use crate::vector::source::{FeatureSource, VectorSource};
        use std::sync::Arc;
        let tmp = std::env::temp_dir().join(format!("ts_mvt_br_zstd_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let payload = b"ARCHIVED_TILE_BYTES ".repeat(50);

        let mk = |enc: TileEncoding| -> PmtilesReader {
            let dir = tmp.join(format!("{enc:?}"));
            std::fs::create_dir_all(&dir).unwrap();
            let out = dir.join("out.pmtiles");
            let mut w = PmtilesWriter::new(&dir)
                .unwrap()
                .tile_format(TILE_TYPE_MVT, enc.to_pmtiles());
            w.add(
                zxy_to_tileid(0, 0, 0),
                enc.compress(&payload, Effort::Archive, None),
            )
            .unwrap();
            w.finish(
                HeaderFields {
                    min_zoom: 0,
                    max_zoom: 0,
                    bounds_e7: [0, 0, 0, 0],
                    center: (0, 0, 0),
                },
                r#"{"vector_layers":[],"grid_id":"WebMercatorQuad"}"#,
                &out,
            )
            .unwrap();
            let r = PmtilesReader::open(&out).unwrap();
            assert_eq!(r.tile_compression(), enc.to_pmtiles());
            // The decoded read path (WMS/raster use) must understand the new codings too.
            assert_eq!(r.get(0, 0, 0).unwrap().unwrap(), payload);
            r
        };
        let mk_state = |reader: Option<PmtilesReader>| {
            let src = Arc::new(GeoJsonSource::load("fixtures/vector/mini_mvt.geojson").unwrap());
            let style = Style::load("fixtures/styles/airports.vec.json").unwrap();
            let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
            let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
            let ext = src.full_extent();
            let mut pmtiles = std::collections::BTreeMap::new();
            if let Some(r) = reader {
                pmtiles.insert("WebMercatorQuad".to_string(), Arc::new(r));
            }
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
                    min_feature_px: 0.0,
                    source: VectorSource::LoadAll(src),
                    style,
                    shaper,
                    lod: None,
                    zoom_sources: Vec::new(),
                }),
                pmtiles,
                raster_pmtiles: std::collections::BTreeMap::new(),
                overlay: std::collections::BTreeMap::new(),
            };
            let mut st = ServeState::new(vec![layer], "http://h/wms".into(), 16);
            st.mvt_cache = Some(super::build_byte_cache(16));
            st
        };
        let chrome = super::Accepted {
            gzip: true,
            br: true,
            zstd: true,
        };
        let gzip_only = super::Accepted::from(true);
        let br_only = super::Accepted {
            br: true,
            ..Default::default()
        };
        let get = |st: &ServeState, a: super::Accepted| {
            super::render_mvt_tile(st, "mini", "WebMercatorQuad", 0, 0, 0, a).unwrap()
        };
        let same_tile = |b: &super::TileBody| b.encoding.decompress(&b.bytes).unwrap() == payload;

        for enc in [TileEncoding::Brotli, TileEncoding::Zstd] {
            let st = mk_state(Some(mk(enc)));
            // Accepted: the stored bytes, verbatim.
            let hit = get(&st, chrome);
            assert_eq!(
                hit.encoding, enc,
                "{enc:?} archive to a client that takes it"
            );
            assert_eq!(hit.bytes, enc.compress(&payload, Effort::Archive, None));
            // Not accepted: transcoded to gzip, labelled gzip, the same tile, and cached.
            let gz = get(&st, gzip_only);
            assert_eq!(
                gz.encoding,
                TileEncoding::Gzip,
                "{enc:?} archive to a gzip-only client"
            );
            assert!(same_tile(&gz));
            let key = "mini/WebMercatorQuad/0/0/0#gzip".to_string();
            assert!(
                st.mvt_cache.as_ref().unwrap().contains_key(&key),
                "the transcode must be cached under its encoding"
            );
            assert_eq!(get(&st, gzip_only), gz, "a cached transcode is stable");
            // Nothing accepted: identity, never a label it does not deserve.
            let plain = get(&st, super::Accepted::default());
            assert_eq!(plain.encoding, TileEncoding::Identity);
            assert_eq!(plain.bytes, payload);
        }

        // A gzip archive stays gzip for a browser that also takes br: no request-time transcode up.
        let st = mk_state(Some(mk(TileEncoding::Gzip)));
        assert_eq!(get(&st, chrome).encoding, TileEncoding::Gzip);
        // ...and a br-only client gets a br transcode of it.
        let b = get(&st, br_only);
        assert_eq!(b.encoding, TileEncoding::Brotli);
        assert!(same_tile(&b));

        // Live tiles with --tile-encoding br: cached as br, served br to a br client, gzip to a
        // gzip-only one, both the same tile as the identity encode.
        let mut live = mk_state(None);
        live.tile_encoding = TileEncoding::Brotli;
        let plain =
            super::render_mvt_tile(&live, "mini", "WebMercatorQuad", 0, 0, 0, false).unwrap();
        if !plain.bytes.is_empty() {
            let br = get(&live, chrome);
            assert_eq!(br.encoding, TileEncoding::Brotli);
            assert_eq!(
                TileEncoding::Brotli.decompress(&br.bytes).unwrap(),
                plain.bytes
            );
            let gz = get(&live, gzip_only);
            assert_eq!(gz.encoding, TileEncoding::Gzip);
            assert_eq!(
                TileEncoding::Gzip.decompress(&gz.bytes).unwrap(),
                plain.bytes
            );
        }

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// The gzip pass-through, end to end through `render_mvt_tile`.
    ///
    /// The archives have always stored gzip'd MVT. Before this, every archive hit was inflated and
    /// then shipped uncompressed -- CPU spent to make the payload ~3x bigger. Now an archive hit
    /// goes to a gzip-capable client verbatim, while everything else is byte-for-byte unchanged.
    #[test]
    fn an_archive_hit_is_served_gzipped_only_when_the_client_accepts_it() {
        use crate::server::{Layer, ServeState, VectorLayer};
        use crate::vector::geojson::GeoJsonSource;
        use crate::vector::pmtiles::codec::{gunzip, gzip};
        use crate::vector::pmtiles::read::PmtilesReader;
        use crate::vector::pmtiles::write::{
            HeaderFields, PmtilesWriter, COMPRESSION_NONE, TILE_TYPE_MVT,
        };
        use crate::vector::pmtiles::zxy_to_tileid;
        use crate::vector::shape::Shaper;
        use crate::vector::source::{FeatureSource, VectorSource};
        use std::sync::Arc;

        let tmp = std::env::temp_dir().join(format!(
            "ts_mvt_gzip_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        // Compressible enough that gzip is unmistakably smaller -- otherwise the test could pass
        // while the pass-through saved nothing.
        let payload = b"ARCHIVED_TILE_BYTES ".repeat(50);

        let mk = |name: &str, compression_none: bool| -> PmtilesReader {
            let dir = tmp.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let out = dir.join("out.pmtiles");
            let mut w = PmtilesWriter::new(&dir).unwrap();
            let stored = if compression_none {
                w = w.tile_format(TILE_TYPE_MVT, COMPRESSION_NONE);
                payload.clone()
            } else {
                gzip(&payload)
            };
            w.add(zxy_to_tileid(0, 0, 0), stored).unwrap();
            w.finish(
                HeaderFields {
                    min_zoom: 0,
                    max_zoom: 0,
                    bounds_e7: [0, 0, 0, 0],
                    center: (0, 0, 0),
                },
                r#"{"vector_layers":[],"grid_id":"WebMercatorQuad"}"#,
                &out,
            )
            .unwrap();
            let r = PmtilesReader::open(&out).unwrap();
            r.require_tile_type(TILE_TYPE_MVT, "test").unwrap();
            r
        };

        let mk_state = |reader: PmtilesReader| {
            let src = Arc::new(GeoJsonSource::load("fixtures/vector/mini_mvt.geojson").unwrap());
            let style = Style::load("fixtures/styles/airports.vec.json").unwrap();
            let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
            let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
            let ext = src.full_extent();
            let mut pmtiles = std::collections::BTreeMap::new();
            pmtiles.insert("WebMercatorQuad".to_string(), Arc::new(reader));
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
                    min_feature_px: 0.0,
                    source: VectorSource::LoadAll(src),
                    style,
                    shaper,
                    lod: None,
                    zoom_sources: Vec::new(),
                }),
                pmtiles,
                raster_pmtiles: std::collections::BTreeMap::new(),
                overlay: std::collections::BTreeMap::new(),
            };
            ServeState::new(vec![layer], "http://h/wms".into(), 16)
        };

        // --- a gzip archive, client accepts gzip: pass the stored blob straight through ---
        let st = mk_state(mk("gz", false));
        let got = super::render_mvt_tile(&st, "mini", "WebMercatorQuad", 0, 0, 0, true).unwrap();
        assert!(got.is_gzip(), "an archive hit must be labelled gzip");
        assert_eq!(
            gunzip(&got.bytes).unwrap(),
            payload,
            "the gzip body must inflate to exactly the archived tile"
        );
        assert!(
            got.bytes.len() < payload.len(),
            "pass-through must be SMALLER than the tile ({} vs {}), or there is no win",
            got.bytes.len(),
            payload.len()
        );

        // --- same archive, client did NOT offer gzip: inflate, and never claim an encoding ---
        let plain = super::render_mvt_tile(&st, "mini", "WebMercatorQuad", 0, 0, 0, false).unwrap();
        assert!(!plain.is_gzip());
        assert_eq!(plain.bytes, payload);
        // The two encodings are two spellings of ONE tile. This is the invariant the whole change
        // rests on: a client must not see different map data depending on its Accept-Encoding.
        assert_eq!(gunzip(&got.bytes).unwrap(), plain.bytes);

        // --- an archive stored UNCOMPRESSED: identity, even for a gzip-capable client ---
        let st_none = mk_state(mk("none", true));
        let raw =
            super::render_mvt_tile(&st_none, "mini", "WebMercatorQuad", 0, 0, 0, true).unwrap();
        assert!(
            !raw.is_gzip(),
            "a COMPRESSION_NONE archive must never be labelled gzip"
        );
        assert_eq!(raw.bytes, payload);

        // --- an archive MISS falls through to the live encoder, which gzips too since 0.3.2 ---
        let miss = super::render_mvt_tile(&st, "mini", "WebMercatorQuad", 1, 0, 0, true).unwrap();
        let miss_plain =
            super::render_mvt_tile(&st, "mini", "WebMercatorQuad", 1, 0, 0, false).unwrap();
        assert!(
            !miss_plain.is_gzip(),
            "a client that did not offer gzip gets identity"
        );
        if !miss_plain.bytes.is_empty() {
            assert!(
                miss.is_gzip(),
                "since 0.3.2 a live-encoded tile is gzip'd for a client that accepts it"
            );
            assert_eq!(
                gunzip(&miss.bytes).unwrap(),
                miss_plain.bytes,
                "the two encodings of a live tile must be the same tile"
            );
        } else {
            // An empty tile stays an empty body in both encodings.
            assert!(!miss.is_gzip() && miss.bytes.is_empty());
        }

        std::fs::remove_dir_all(&tmp).ok();
    }
}
