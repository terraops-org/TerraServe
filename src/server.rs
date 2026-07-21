// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Async HTTP server (axum + tokio).
//!
//! The whole request/I/O path is async. The CPU-bound render work is dispatched to a
//! blocking worker pool via `tokio::task::spawn_blocking`, so tight pixel loops never
//! stall the async reactor — that's what keeps concurrent requests flowing.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Body,
    extract::{Path, RawQuery, State},
    http::{header, StatusCode},
    response::Response,
    routing::{get, post},
    Router,
};

use crate::cache::TileCache;
use crate::cog::Cog;
use crate::render::BandMath;
use crate::s3::AnySource;
use crate::style::Style;
use crate::vector::pmtiles::overlay::{compact_overlay_layer, TileOverlay};
use crate::{wms, Error};

/// A tile grid this layer publishes on, plus the layer's data extent PRECOMPUTED in that grid's CRS
/// (for the TMS `<BoundingBox>` and the empty-tile early-out). `data_bounds` is `None` when the
/// `src_crs → grid.crs` transform is unavailable.
pub struct PublishedGrid {
    pub tms: crate::tms::TileMatrixSet,
    pub data_bounds: Option<[f64; 4]>,
}

/// One published WMS layer: a COG parsed once at startup plus how to render and advertise it.
/// Each layer owns its tile cache, so the `(level, tile)` cache key can't collide between
/// layers (different COGs) without needing a layer id in the key.
pub struct Layer {
    /// WMS layer name (the `LAYERS=` value clients request).
    pub name: String,
    /// COG source path (for error messages / the render request). Empty for a vector layer.
    pub cog_path: String,
    /// The COG structure, parsed ONCE at startup and shared across all requests. `None` for a
    /// **vector** layer (which has no COG — see `vector`).
    pub cog: Option<Arc<Cog>>,
    /// The byte source (local file or S3), opened ONCE and **reused across requests** — the
    /// source is `Sync`, so for S3 its connection pool persists (no per-request TLS handshake)
    /// and concurrent requests + parallel tile reads all share it. `None` for a vector layer.
    pub source: Option<Arc<AnySource>>,
    /// The raster style. `None` for a vector layer (which uses `vector.style`).
    pub style: Option<Style>,
    /// The layer's own CRS (request coords are reprojected into it; for a vector layer this is the
    /// feature CRS, e.g. EPSG:4326).
    pub src_crs: String,
    /// When set, the layer is rendered as on-the-fly band math (e.g. NDVI).
    pub band_math: Option<BandMath>,
    /// Layer extent in WGS84 `[west, south, east, north]`, advertised in GetCapabilities.
    pub bounds_wgs84: [f64; 4],
    /// This layer's bounded decoded-tile cache (`None` when caching is disabled).
    pub tile_cache: Option<TileCache>,
    /// This layer's bounded index-chunk cache, backing `cog::Level::tile_location` for a `Lazy`
    /// tile index. Always present (cheap when unused — an all-`Resident` COG never touches it),
    /// unlike `tile_cache` which is opt-out.
    pub index_cache: crate::cache::IndexCache,
    /// Tile grids this layer publishes on (TMS/WMTS). May be empty for a WMS-only layer; the TMS
    /// front-end + viewer use the first grid as the default. Empty for the MVP vector layer
    /// (tiled vector serving is a later rung — vector is WMS GetMap only).
    pub grids: Vec<PublishedGrid>,
    /// When set, this is a **vector** layer (features + labels, WMS GetMap only). Mutually
    /// exclusive with `cog`/`source`/`style` being `Some`.
    pub vector: Option<VectorLayer>,
    /// When set, MVT tiles are served from this pre-built PMTiles archive first, falling back to live
    /// encoding on a miss. `None` = live encode only. Opt-in via `serve --pmtiles`.
    pub pmtiles: Option<std::sync::Arc<crate::vector::pmtiles::read::PmtilesReader>>,
    /// Write-through overlay (Spec 2): when set, a miss is live-encoded then persisted here, and this
    /// overlay owns the (swappable) base reader. `None` = no write-through. Opt-in via --pmtiles-cache.
    pub overlay: Option<std::sync::Arc<crate::vector::pmtiles::overlay::TileOverlay>>,
}

/// A vector + label layer (the label engine). Parsed once at startup; served over WMS GetMap.
pub struct VectorLayer {
    /// The feature source (GeoJSON/GPKG today, load-all; FlatGeoBuf the future windowed reader) —
    /// the `VectorSource` seam every vector-serving path (WMS GetMap/GetFeatureInfo, `/mvt`, WMTS
    /// GetTile) reads through via `features_in(bbox)`. Always `LoadAll` today; `Windowed` is
    /// unblocked but has no real implementer yet.
    pub source: crate::vector::source::VectorSource,
    /// The vector style (rule-based Style IR: scale + filter rule selection → symbolizers).
    pub style: crate::vector::style::Style,
    /// The shaper over the pinned font, shared across requests.
    pub shaper: Arc<crate::vector::shape::Shaper>,
    /// The TileJSON attribute schema (distinct property keys → "String"|"Number"), computed ONCE at
    /// load. The schema is static, so caching it avoids an O(all features × props) scan on every
    /// TileJSON request — that scan is ~1.6 s per request at BUPi's 3.4M-feature scale and blocks
    /// the viewer/QGIS on layer-add.
    pub fields: std::collections::BTreeMap<String, String>,
    /// Mercator-m² per source-CRS unit² for this layer (`mvt::layer_area_scale` of the layer's WGS84
    /// bounds + source extent), precomputed once at load. The SINGLE source of `area_scale` for the
    /// MVT/WMTS encode path — both routes read it via `MvtOptimizations::for_layer` instead of each
    /// recomputing it (removing the byte-lock duplication). `0.0` if not computable → the
    /// min-feature-size gate fails OPEN (see `mvt::min_area_src_for_zoom`).
    pub area_scale: f64,
    /// Optional per-zoom level-of-detail pools (topology serve): when `Some`, the MVT/WMTS/WMS paths
    /// pick a zoom/scale-appropriate `FeatureSource` from here instead of `source`. `None` = no LOD
    /// (serve `source` at all zooms — the raw / single-tolerance path).
    pub lod: Option<Arc<crate::vector::topology::lod::LodSet>>,
}

impl VectorLayer {
    /// The `VectorSource` to render for tile zoom `z`: the LOD pool for that zoom (wrapped as
    /// `LoadAll`, an `Arc::clone` — no data copy) when this layer has per-zoom LOD, else the layer's
    /// own `source` (cheap `Clone` — see `VectorSource`'s doc comment). The one home for the
    /// LOD-or-fallback rule shared by the `/mvt` and WMTS GetTile routes, and the offline PMTiles
    /// generator. Windowed sources have no LOD pool (a windowed reader's whole point is not
    /// materializing per-zoom pools), so the LOD branch always yields `LoadAll`.
    pub fn source_for_zoom(&self, z: u32) -> crate::vector::source::VectorSource {
        match &self.lod {
            Some(l) => crate::vector::source::VectorSource::LoadAll(l.for_zoom(z).clone()),
            None => self.source.clone(),
        }
    }

    /// The `VectorSource` for a WMS GetMap scale-denominator: the scale-appropriate LOD pool when this
    /// layer has LOD, else `source`. WMS has no integer tile zoom, so it maps scale → effective zoom.
    pub fn source_for_scale(&self, scale: f64) -> crate::vector::source::VectorSource {
        match &self.lod {
            Some(l) => {
                crate::vector::source::VectorSource::LoadAll(l.for_scale_denominator(scale).clone())
            }
            None => self.source.clone(),
        }
    }
}

/// Immutable shared state for the WMS service: the published layers plus the advertised URL.
pub struct ServeState {
    /// Published layers; the first is the default for a GetMap with an unknown/missing LAYERS.
    pub layers: Vec<Layer>,
    /// Advertised OnlineResource / GetMap endpoint (injected into GetCapabilities).
    pub base_url: String,
    /// **Admission control:** caps the number of CONCURRENT renders. A request acquires a permit
    /// before its `spawn_blocking` render and holds it until the response is built, so peak memory =
    /// (permits × per-request render buffers) + the bounded caches — hard-bounded regardless of how
    /// many connections a burst opens. Excess requests QUEUE on `acquire()`, they don't fail. Without
    /// this the async server would render *every* in-flight request at once and RSS scales with burst
    /// concurrency (measured 434 MB @ 12 → 1330 MB @ 48). Doc/capabilities/viewer routes don't render
    /// and take no permit.
    pub render_limiter: Arc<tokio::sync::Semaphore>,
    /// Max features an MVT tile emits before the encoder uniformly samples down (0 = unlimited).
    /// Set from `serve --mvt-max-features`; defaults to `mvt::DEFAULT_MAX_FEATURES_PER_TILE`.
    pub mvt_max_features: usize,
    /// Minimum on-screen feature size (display-pixels²) for a feature to appear in an MVT tile at a
    /// given zoom — a per-feature, per-layer/per-zoom-CONSTANT selection (see
    /// `mvt::min_area_src_for_zoom`) that thins overview tiles WITHOUT the density seam that
    /// `mvt_max_features` sampling causes on a complete coverage. `0.0` = off (default). Set from
    /// `serve --mvt-min-feature-px`.
    pub mvt_min_feature_px: f64,
    /// Disable the default-on MVT geometry optimizations (grid-snap dedup) — raw rounded rings. From
    /// `serve --no-optimizations`. Opt-in thinning flags stay independent. Default `false`.
    pub mvt_no_optimizations: bool,
    /// Lift the per-tile feature cap (unlimited), overriding `mvt_max_features`. From
    /// `serve --no-safety-limit`. Default `false`.
    pub mvt_no_safety_limit: bool,
    /// Overview cell-mosaic cell size in display-px (`serve --mvt-cell-px`; rounded to a power of 2
    /// at load). `0.0` = mosaic off. Requires `mvt_cell_field`.
    pub mvt_cell_px: f64,
    /// The thematic class attribute the cell mosaic votes on (`serve --mvt-cell-field`). `None` when
    /// unset; `for_layer` disables the mosaic for any layer lacking the field.
    pub mvt_cell_field: Option<String>,
    /// Cell mosaic active only at zoom ≤ this (`serve --mvt-cell-max-zoom`). `0` = every zoom.
    pub mvt_cell_max_zoom: u32,
    /// The class field the same-class dissolve merges by (`serve --mvt-dissolve`). `None` = off.
    /// Mutually exclusive with the cell mosaic — dissolve wins.
    pub mvt_dissolve_field: Option<String>,
    /// Dissolve active only at zoom ≤ this (`serve --mvt-dissolve-max-zoom`). `0` = every zoom.
    pub mvt_dissolve_max_zoom: u32,
    /// Operator-supplied MapLibre GL **layer array** (from `serve --mvt-style FILE`) served by
    /// `/mvt/{layer}/style.json` with the source binding injected. `None` → the generic X-ray style.
    pub mvt_style: Option<serde_json::Value>,
    /// Bounded cache of ENCODED MVT tile bytes (from `serve --mvt-cache N`; `None` = off), keyed by
    /// `layer/tms/z/x/y`. Since the encode is a pure function of that key + the fixed-per-run opts, a
    /// tile is computed once and reused — the mitigation for costly passes like `--mvt-dissolve` at
    /// low zoom. `get_with` gives single-flight (a cold tile isn't computed N times under a burst).
    /// Shared by the `/mvt` XYZ + WMTS GetTile routes. `moka::sync::Cache` is internally `Arc`-ed.
    pub mvt_cache: Option<moka::sync::Cache<String, Arc<Vec<u8>>>>,
    /// Bounded cache of rendered **WMS GetMap** PNG bytes (`serve --wms-cache N` MiB; `None` = off),
    /// keyed by the raw request query (a GetMap's PNG is a pure function of its query + the
    /// fixed-per-run layer config). The mitigation for a costly vector render (e.g. the X-ray raster
    /// underlay over a dense coverage) — each tile is rasterised once, revisits are instant. Only
    /// successful PNG renders are cached (error XML is not). Byte-weighted → RSS hard-bounded.
    pub wms_cache: Option<moka::sync::Cache<String, Arc<Vec<u8>>>>,
    /// Write-through overlay compaction interval in seconds (`serve --pmtiles-flush-interval`;
    /// `0` = compact only on a size-cap breach, an explicit `/flush`, or shutdown). Each
    /// `--pmtiles-cache` layer's controller ticks on this. Task 6.
    pub pmtiles_flush_interval: u64,
}

impl ServeState {
    /// Build the service state with an admission-control limiter of `max_inflight` permits (min 1).
    /// `mvt_max_features` starts at the default; callers that expose the flag set it afterwards.
    pub fn new(layers: Vec<Layer>, base_url: String, max_inflight: usize) -> ServeState {
        ServeState {
            layers,
            base_url,
            render_limiter: Arc::new(tokio::sync::Semaphore::new(max_inflight.max(1))),
            mvt_max_features: crate::vector::mvt::DEFAULT_MAX_FEATURES_PER_TILE,
            mvt_min_feature_px: 0.0,
            mvt_no_optimizations: false,
            mvt_no_safety_limit: false,
            mvt_cell_px: 0.0,
            mvt_cell_field: None,
            mvt_cell_max_zoom: 0,
            mvt_dissolve_field: None,
            mvt_dissolve_max_zoom: 0,
            mvt_style: None,
            mvt_cache: None,
            wms_cache: None,
            pmtiles_flush_interval: 0,
        }
    }
}

/// Blocking entry from the sync CLI: build a tokio runtime and run the server.
pub fn run(state: ServeState, host: &str, port: u16) -> Result<(), Error> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve(state, host, port))
}

async fn serve(state: ServeState, host: &str, port: u16) -> Result<(), Error> {
    let state = Arc::new(state);
    let app = Router::new()
        .route("/wms", get(wms_handler))
        // TMS 1.0.0 (standard OSGeo layout — D1): the version path IS the TileMapService.
        .route("/tms/1.0.0", get(tms_service_handler))
        .route("/tms/1.0.0/", get(tms_service_handler))
        .route("/tms/1.0.0/:layerspec", get(tms_tilemap_handler))
        .route("/tms/1.0.0/:layerspec/:z/:x/:yfile", get(tms_tile_handler))
        // WMTS 1.0.0 — KVP on /wmts, RESTful on /wmts/1.0.0/… (both bindings, one service).
        .route("/wmts", get(wmts_kvp_handler))
        .route("/wmts/1.0.0/WMTSCapabilities.xml", get(wmts_caps_handler))
        .route(
            "/wmts/1.0.0/:layer/:style/:tms/:z/:row/:colfile",
            get(wmts_rest_tile_handler),
        )
        // MVT (Task 5) — bespoke vector tiles, XYZ addressing (top-left row, like WMTS) + TileJSON.
        .route("/mvt/:layer/:tms/:z/:x/:yfile", get(mvt_tile_handler))
        // Write-through admin (task 6): force-compact the layer's overlay into its `.pmtiles` now.
        .route("/mvt/:layer/flush", post(mvt_flush_handler))
        // MapLibre GL style JSON (static path wins over the `:tmsjson` param route below).
        .route("/mvt/:layer/style.json", get(mvt_style_handler))
        .route("/mvt/:layer/:tmsjson", get(mvt_tilejson_handler))
        .route("/viewer", get(viewer_handler))
        .route("/xray", get(xray_handler))
        .route(
            "/",
            get(|| async {
                "TerraServe — WMS at /wms?SERVICE=WMS&REQUEST=GetCapabilities&VERSION=1.3.0 ; \
                 TMS at /tms/1.0.0/ ; MVT at /mvt/{layer}/{tms}.json ; viewer at /viewer ; \
                 X-ray MVT viewer at /xray\n"
            }),
        )
        .with_state(state.clone());

    // Write-through compaction (task 6): spawn one controller per `--pmtiles-cache` layer and collect
    // the roster for the final shutdown compaction. Every cache layer publishes on WebMercatorQuad, so
    // the compaction header zoom span is derived once here.
    let (min_z, max_z) = pmtiles_grid_zoom_range();
    let interval = state.pmtiles_flush_interval;
    let mut cache_layers: Vec<(Arc<TileOverlay>, [f64; 4])> = Vec::new();
    for layer in &state.layers {
        if let Some(ov) = layer.overlay.clone() {
            let bounds = layer.bounds_wgs84;
            cache_layers.push((ov.clone(), bounds));
            let ctrl = ov;
            tokio::spawn(async move {
                loop {
                    // Next trigger: the interval tick OR a size-cap/flush wake. With no interval, only
                    // an explicit wake drives a compaction.
                    if interval > 0 {
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
                            _ = ctrl.compactor_woken() => {}
                        }
                    } else {
                        ctrl.compactor_woken().await;
                    }
                    if ctrl.snapshot_ids().is_empty() {
                        continue; // nothing new to fold into the base
                    }
                    // Atomic CAS: take compaction ownership IFF none is already running (a `/flush` or
                    // shutdown may hold it). `put`'s compacting-skip (under the same inner lock) pauses
                    // write-through for the run, closing the new-id-dropped-by-truncate window; the CAS
                    // also serializes same-layer compactions so no two ever share the scratch file.
                    if ctrl.try_begin_compaction() {
                        let run = ctrl.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            compact_overlay_layer(&run, bounds, min_z, max_z)
                        })
                        .await; // JoinError on panic is swallowed here, so end_compaction still runs
                        ctrl.end_compaction();
                    }
                    // else: a compaction already owns this layer -> skip (a second would share
                    // `PmtilesWriter`'s PID-only scratch file and corrupt the intermediate).
                }
            });
        }
    }

    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!(
        "TerraServe serving on http://{addr}/  (WMS /wms · TMS /tms/1.0.0/ · MVT /mvt/{{layer}}/{{tms}}.json · viewer /viewer · X-ray /xray)"
    );
    // Hand-rolled accept loop (instead of `axum::serve`) so we can reach
    // `http1().title_case_headers(true)` — hyper's `axum::serve` wrapper doesn't expose the
    // connection builder. CITE `ets-wms11` GetFeatureInfo query_layers-1/2 do a case-SENSITIVE
    // `header[name='Content-Type']` XPath; hyper's default lowercase header names fail that
    // lookup even though HTTP header names are spec-case-insensitive. Title-Case is the
    // conventional wire form and unblocks the assertion.
    let mut builder =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    builder.http1().title_case_headers(true);
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    let mut shutdown = std::pin::pin!(shutdown_signal());
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _addr) = match accepted {
                    Ok(c) => c,
                    // A persistent accept() error (e.g. fd exhaustion, EMFILE) would hot-spin this
                    // loop and peg a core; back off briefly so the process stays responsive.
                    Err(_e) => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let io = hyper_util::rt::TokioIo::new(stream);
                let svc = hyper_util::service::TowerToHyperService::new(app.clone());
                let conn = builder.serve_connection_with_upgrades(io, svc);
                let fut = graceful.watch(conn.into_owned());
                tokio::spawn(async move {
                    let _ = fut.await;
                });
            }
            _ = &mut shutdown => break,
        }
    }
    // Bounded drain window for in-flight connections, then fall through to compaction below
    // regardless (the compaction must run even if some connection didn't finish draining).
    tokio::select! {
        _ = graceful.shutdown() => {}
        _ = tokio::time::sleep(Duration::from_secs(5)) => {}
    }
    // Best-effort final compaction (task 6) so each `.pmtiles` reflects the overlay at exit. The
    // atomic CAS replaces the old TOCTOU `is_compacting()` pre-check: it skips a layer a controller
    // tick still owns (it raced shutdown) AND serializes against it, so no two compactions ever share
    // the scratch file. Skip an empty overlay (nothing to fold in).
    for (ov, bounds) in &cache_layers {
        if ov.snapshot_ids().is_empty() {
            continue;
        }
        if !ov.try_begin_compaction() {
            continue; // a controller/flush compaction already owns this layer -> don't double-write
        }
        match compact_overlay_layer(ov, *bounds, min_z, max_z) {
            Ok(c) => println!(
                "shutdown compaction: addressed {} · entries {} · contents {} · {} bytes",
                c.addressed, c.entries, c.contents, c.bytes
            ),
            Err(e) => eprintln!("shutdown compaction failed: {e}"),
        }
        ov.end_compaction(); // runs on both the Ok and Err arms
    }
    Ok(())
}

/// Resolve when the process is asked to stop — SIGINT (Ctrl-C) **or** SIGTERM. The spec's exit-time
/// compaction (fold the write-through overlay into the base `.pmtiles`) must run on the real operator
/// stop paths, and Docker `stop` / systemd / `pkill` all send **SIGTERM**, not SIGINT — waiting only on
/// `ctrl_c()` would silently skip the "snapshot at exit" mode for every non-interactive deployment.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            // If we can't install the SIGTERM handler, fall back to SIGINT-only (never resolve here).
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

/// The WebMercatorQuad zoom span every `--pmtiles-cache` layer publishes on (min/max level `z`), for
/// the compaction `HeaderFields`. Falls back to a full `0..=22` span if the preset is unavailable.
fn pmtiles_grid_zoom_range() -> (u8, u8) {
    crate::tms::preset("WebMercatorQuad", 4096)
        .map(|g| {
            let lo = g.levels.iter().map(|l| l.z).min().unwrap_or(0) as u8;
            let hi = g.levels.iter().map(|l| l.z).max().unwrap_or(22) as u8;
            (lo, hi)
        })
        .unwrap_or((0, 22))
}

async fn wms_handler(State(state): State<Arc<ServeState>>, RawQuery(q): RawQuery) -> Response {
    let query = q.unwrap_or_default();
    // Log each incoming request so we can see exactly what a client (QGIS) sends.
    println!("--> WMS: {query}");
    // Admission control: acquire a render permit (queues under burst) so concurrent renders — and
    // thus peak memory — are hard-bounded. Held until the response is built.
    let _permit = state.render_limiter.acquire().await;
    let st = state.clone();
    // A GetMap's PNG is the expensive vector render and a pure function of its query → cache it
    // (not GetCapabilities/GetFeatureInfo). Cheap substring check avoids a full re-parse.
    let is_getmap = query.to_ascii_lowercase().contains("request=getmap");

    // Render on a blocking worker — never on the async reactor. Layers are parsed once at startup;
    // only a fresh range source is opened per request. The WMS render cache (if enabled) serves a
    // repeated GetMap without re-rendering.
    let result = tokio::task::spawn_blocking(move || {
        if is_getmap {
            if let Some(cache) = &st.wms_cache {
                if let Some(bytes) = cache.get(&query) {
                    return wms::WmsResult {
                        bytes: (*bytes).to_vec(),
                        content_type: Some("image/png".to_string()),
                    };
                }
                let r = wms::handle_layers(&st.layers, &query, Some(&st.base_url));
                // Cache ONLY a successful PNG — never an exception XML (it'd be served as image/png).
                if r.bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
                    cache.insert(query.clone(), Arc::new(r.bytes.clone()));
                }
                return r;
            }
        }
        wms::handle_layers(&st.layers, &query, Some(&st.base_url))
    })
    .await;

    match result {
        Ok(r) => {
            // GetFeatureInfo sets an explicit Content-Type; otherwise sniff PNG magic vs. XML.
            let is_png = r.bytes.starts_with(&[0x89, b'P', b'N', b'G']);
            let ct = r.content_type.clone().unwrap_or_else(|| {
                if is_png {
                    "image/png".to_string()
                } else {
                    "text/xml; charset=utf-8".to_string()
                }
            });
            println!("<-- 200 {} ({} bytes)", ct, r.bytes.len());
            Response::builder()
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(r.bytes))
                .unwrap()
        }
        Err(_) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("render task failed"))
            .unwrap(),
    }
}

fn xml_response(xml: String) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
        .body(Body::from(xml))
        .unwrap()
}

fn png_response(png: Vec<u8>) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "image/png")
        .body(Body::from(png))
        .unwrap()
}

fn mvt_response(pbf: Vec<u8>) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/vnd.mapbox-vector-tile")
        .body(Body::from(pbf))
        .unwrap()
}

fn status_response(status: u16, msg: String) -> Response {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .body(Body::from(msg))
        .unwrap()
}

/// `GET /tms/1.0.0/` — the TileMapService document (all layer×grid TileMaps).
async fn tms_service_handler(State(state): State<Arc<ServeState>>) -> Response {
    let root = crate::tms_http::tms_root(&state.base_url);
    xml_response(crate::tms_http::tilemapservice_xml(&state, &root))
}

/// `GET /tms/1.0.0/{layer}[@{grid}]` — a TileMap document.
async fn tms_tilemap_handler(
    State(state): State<Arc<ServeState>>,
    Path(layerspec): Path<String>,
) -> Response {
    let root = crate::tms_http::tms_root(&state.base_url);
    match crate::tms_http::tilemap_doc(&state, &layerspec, &root) {
        Ok(xml) => xml_response(xml),
        Err((status, msg)) => status_response(status, msg),
    }
}

/// `GET /tms/1.0.0/{layer}[@{grid}]/{z}/{x}/{y}.png` — a tile (y is bottom-left).
async fn tms_tile_handler(
    State(state): State<Arc<ServeState>>,
    Path((layerspec, z, x, yfile)): Path<(String, u32, u32, String)>,
) -> Response {
    let ystr = yfile.strip_suffix(".png").unwrap_or(&yfile);
    let y: u32 = match ystr.parse() {
        Ok(v) => v,
        Err(_) => return status_response(400, format!("bad tile y '{yfile}'")),
    };
    let _permit = state.render_limiter.acquire().await; // admission control (bounded concurrent renders)
    let st = state.clone();
    // Render on a blocking worker — never on the async reactor.
    let result = tokio::task::spawn_blocking(move || {
        crate::tms_http::render_tms_tile(&st, &layerspec, z, x, y)
    })
    .await;
    match result {
        Ok(Ok(png)) => png_response(png),
        Ok(Err((status, msg))) => status_response(status, msg),
        Err(_) => status_response(500, "tile render task panicked".into()),
    }
}

/// `GET /mvt/{layer}/{tms}/{z}/{x}/{y}.pbf` — an MVT tile (XYZ addressing, top-left row — no y-flip,
/// unlike the TMS front-end). A `.pbf` body even when empty (no features / all clipped away) is a
/// valid 200; only an unknown layer/grid or an out-of-range tile is a 4xx (Task 5).
async fn mvt_tile_handler(
    State(state): State<Arc<ServeState>>,
    Path((layer, tms, z, x, yfile)): Path<(String, String, u32, u32, String)>,
) -> Response {
    let ystr = yfile.strip_suffix(".pbf").unwrap_or(&yfile);
    let y: u32 = match ystr.parse() {
        Ok(v) => v,
        Err(_) => return status_response(400, format!("bad tile y '{yfile}'")),
    };
    let _permit = state.render_limiter.acquire().await; // admission control (bounded concurrent renders)
    let st = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::mvt_http::render_mvt_tile(&st, &layer, &tms, z, x, y)
    })
    .await;
    match result {
        Ok(Ok(pbf)) => mvt_response(pbf),
        Ok(Err((status, msg))) => status_response(status, msg),
        Err(_) => status_response(500, "tile encode task panicked".into()),
    }
}

/// `POST /mvt/{layer}/flush` — force-compact the layer's write-through overlay into its `.pmtiles`
/// now (task 6 admin route). `404` for an unknown layer, `400` if the layer has no write-through
/// cache, `200` with the resulting `Counts`, or `500` on a compaction error.
async fn mvt_flush_handler(
    State(state): State<Arc<ServeState>>,
    Path(layer): Path<String>,
) -> Response {
    let Some(lyr) = state.layers.iter().find(|l| l.name == layer) else {
        return status_response(404, format!("unknown layer '{layer}'"));
    };
    let Some(ov) = lyr.overlay.clone() else {
        return status_response(400, "layer has no write-through cache".into());
    };
    let bounds = lyr.bounds_wgs84;
    let (min_z, max_z) = pmtiles_grid_zoom_range();
    // Atomic CAS: take compaction ownership IFF none is already running. A second concurrent
    // compaction (another `/flush`, or an interval/shutdown run) would share `PmtilesWriter`'s
    // PID-only scratch file and corrupt the intermediate, so refuse with 409 instead of starting one.
    // While held, `put`'s compacting-skip (same inner lock) pauses write-through for the run.
    if !ov.try_begin_compaction() {
        return status_response(409, "compaction already in progress".into());
    }
    let run = ov.clone();
    let result =
        tokio::task::spawn_blocking(move || compact_overlay_layer(&run, bounds, min_z, max_z))
            .await;
    ov.end_compaction(); // runs whether the compaction succeeded, errored, or the task panicked
    match result {
        Ok(Ok(c)) => status_response(
            200,
            format!(
                "compacted: addressed {} entries {} contents {} bytes {}",
                c.addressed, c.entries, c.contents, c.bytes
            ),
        ),
        Ok(Err(e)) => status_response(500, format!("compaction failed: {e}")),
        Err(_) => status_response(500, "compaction task panicked".into()),
    }
}

/// `GET /mvt/{layer}/{tms}.json` — a TileJSON 3.0.0 document for the layer×grid.
async fn mvt_tilejson_handler(
    State(state): State<Arc<ServeState>>,
    headers: axum::http::HeaderMap,
    Path((layer, tmsjson)): Path<(String, String)>,
) -> Response {
    let tms = tmsjson.strip_suffix(".json").unwrap_or(&tmsjson);
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    match crate::mvt_http::tilejson_doc(&state, &layer, tms, host) {
        Ok(json) => Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json))
            .unwrap(),
        Err((status, msg)) => status_response(status, msg),
    }
}

/// `GET /mvt/{layer}/style.json` — a MapLibre GL Style JSON (source + generic X-ray styling) so a
/// client (QGIS's *Style URL*, MapLibre, the X-ray viewer) is configured from one URL.
async fn mvt_style_handler(
    State(state): State<Arc<ServeState>>,
    headers: axum::http::HeaderMap,
    Path(layer): Path<String>,
) -> Response {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    match crate::mvt_http::style_json(&state, &layer, host) {
        Ok(json) => Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json))
            .unwrap(),
        Err((status, msg)) => status_response(status, msg),
    }
}

fn wmts_exception_response(e: crate::wmts::WmtsErr) -> Response {
    Response::builder()
        .status(StatusCode::from_u16(e.http).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
        .body(Body::from(crate::wmts::exception_xml(
            &e.code,
            &e.text,
            e.locator.as_deref(),
        )))
        .unwrap()
}

/// `GET /wmts?…` — WMTS KVP binding (GetCapabilities / GetTile / OWS exception).
async fn wmts_kvp_handler(State(state): State<Arc<ServeState>>, RawQuery(q): RawQuery) -> Response {
    let query = q.unwrap_or_default();
    let (kvp_base, rest_base) = crate::wmts::bases(&state.base_url);
    match crate::wmts::parse_kvp(&query) {
        crate::wmts::WmtsRequest::GetCapabilities => {
            xml_response(crate::wmts::capabilities_xml(&state, &kvp_base, &rest_base))
        }
        crate::wmts::WmtsRequest::GetTile {
            layer,
            style,
            tms,
            z,
            row,
            col,
            format,
        } => {
            let _permit = state.render_limiter.acquire().await; // admission control
            let st = state.clone();
            // Task 5 — WMTS-MVT: FORMAT selects the vector-tile encoder instead of the raster
            // render path. Both branches return the same `Result<Vec<u8>, WmtsErr>` shape.
            let is_mvt = format.eq_ignore_ascii_case(crate::wmts::MVT_FORMAT);
            let result = tokio::task::spawn_blocking(move || {
                if is_mvt {
                    crate::wmts::get_tile_mvt(&st, &layer, &style, &tms, z, row, col)
                } else {
                    crate::wmts::get_tile(&st, &layer, &style, &tms, z, row, col)
                }
            })
            .await;
            match result {
                Ok(Ok(bytes)) => {
                    if is_mvt {
                        mvt_response(bytes)
                    } else {
                        png_response(bytes)
                    }
                }
                Ok(Err(e)) => wmts_exception_response(e), // OWS ExceptionReport on the KVP binding
                Err(_) => status_response(500, "tile render task panicked".into()),
            }
        }
        crate::wmts::WmtsRequest::GetFeatureInfo {
            layer,
            style,
            tms,
            z,
            row,
            col,
            i,
            j,
            info_format,
        } => {
            let _permit = state.render_limiter.acquire().await; // admission control
            let st = state.clone();
            let result = tokio::task::spawn_blocking(move || {
                crate::wmts::get_feature_info(
                    &st,
                    &layer,
                    &style,
                    &tms,
                    z,
                    row,
                    col,
                    i,
                    j,
                    &info_format,
                )
            })
            .await;
            match result {
                Ok(Ok((body, ct))) => Response::builder()
                    .header(header::CONTENT_TYPE, ct)
                    .body(Body::from(body))
                    .unwrap(),
                Ok(Err((status, msg))) => status_response(status, msg),
                Err(_) => status_response(500, "feature-info task panicked".into()),
            }
        }
        crate::wmts::WmtsRequest::Exception {
            code,
            text,
            locator,
        } => Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
            .body(Body::from(crate::wmts::exception_xml(
                &code,
                &text,
                locator.as_deref(),
            )))
            .unwrap(),
    }
}

/// `GET /wmts/1.0.0/WMTSCapabilities.xml` — WMTS RESTful GetCapabilities.
async fn wmts_caps_handler(State(state): State<Arc<ServeState>>) -> Response {
    let (kvp_base, rest_base) = crate::wmts::bases(&state.base_url);
    xml_response(crate::wmts::capabilities_xml(&state, &kvp_base, &rest_base))
}

/// `GET /wmts/1.0.0/{layer}/{style}/{tms}/{z}/{row}/{col}.png` — WMTS RESTful GetTile (top-left,
/// no y-flip). Segment order is `TileMatrix/TileRow/TileCol` = `z/row/col`.
async fn wmts_rest_tile_handler(
    State(state): State<Arc<ServeState>>,
    Path((layer, style, tms, z, row, colfile)): Path<(String, String, String, u32, u32, String)>,
) -> Response {
    let cstr = colfile.strip_suffix(".png").unwrap_or(&colfile);
    let col: u32 = match cstr.parse() {
        Ok(v) => v,
        Err(_) => return status_response(400, format!("bad tile col '{colfile}'")),
    };
    let _permit = state.render_limiter.acquire().await; // admission control
    let st = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::wmts::get_tile(&st, &layer, &style, &tms, z, row, col)
    })
    .await;
    match result {
        Ok(Ok(png)) => png_response(png),
        Ok(Err(e)) => status_response(e.http, e.text), // RESTful binding: bare status
        Err(_) => status_response(500, "tile render task panicked".into()),
    }
}

/// `GET /viewer` — the built-in map viewer (filled in Task 6).
async fn viewer_handler(State(state): State<Arc<ServeState>>) -> Response {
    let html = crate::tms_http::viewer_html(&state);
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(html))
        .unwrap()
}

/// `GET /xray` — the cyan-on-black X-ray MVT viewer (Task 6). A static, self-contained OpenLayers
/// page (`xray.html`, embedded at compile time — no per-request templating needed): it reads the
/// `{layer}`/`{tms}` to display from the `?layer=`/`?tms=` query string client-side and requests
/// tiles straight from `/mvt/{layer}/{tms}/{z}/{x}/{y}.pbf`, so no `ServeState` is needed here.
async fn xray_handler(State(state): State<Arc<ServeState>>) -> Response {
    // Substitute the server's FIRST layer as the viewer's default. Without this the page
    // defaulted to the literal "vector" and every tile 404'd whenever the layer was named
    // anything else (`--name airports` on the published demo image, for instance) — the
    // viewer loaded, then failed to fetch a single tile, which reads as a broken server.
    let default_layer = state
        .layers
        .first()
        .map(|l| l.name.as_str())
        .unwrap_or("vector");
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(
            xray_html().replace("__TS_DEFAULT_LAYER__", default_layer),
        ))
        .unwrap()
}

/// The embedded X-ray viewer page, exposed as a function (rather than inlining `include_str!` at
/// the call site) so it's directly unit-testable without spinning an HTTP server.
pub fn xray_html() -> &'static str {
    include_str!("xray.html")
}
