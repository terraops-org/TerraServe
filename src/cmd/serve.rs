// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! The `serve` subcommand: the live HTTP server, and the flag surface every demo runs on.
//!
//! `run_serve` is startup, not request handling. It resolves the layer set (a single `--cog` /
//! `--vector`, or N layers from `--config`), builds each one through `crate::layer`, assembles the
//! immutable `ServeState`, and hands it to `server::run`. Nothing here executes per request.
//!
//! ⚠ **`ServeArgs` is a FROZEN external contract** — score.sh and the deployed compose files drive
//! the binary by these flag names. It also has 43 fields, which is the root of the shape problem
//! documented in `crate::layer`: passing the whole struct into layer construction is why that
//! function is ~540 lines. Narrowing it is the planned follow-up; the field list itself stays.

use crate::Error;
use crate::{assets, config, layer, mvt_http, reproj, s3, server, style, vector};
use clap::Args;

/// `serve` arguments — run the live HTTP WMS server.
#[derive(Args, Debug)]
pub struct ServeArgs {
    /// Multi-layer config (`layers.yaml`). When set, publishes all its layers and the
    /// single-layer flags below are ignored.
    #[arg(long)]
    pub config: Option<String>,
    /// Path to the COG served as the single layer (or use `--config`).
    #[arg(long)]
    pub cog: Option<String>,
    /// Path to `style.json` (single-layer mode).
    #[arg(long)]
    pub style: Option<String>,
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    #[arg(long, default_value_t = 8080)]
    pub port: u16,
    /// Public base URL advertised in GetCapabilities (default: `http://host:port/wms`).
    #[arg(long)]
    pub public_url: Option<String>,
    /// LRU tile-cache cap in MiB — the hard memory ceiling for the decoded-tile cache
    /// (0 disables). Named for the specific cache, leaving room for other caches later.
    #[arg(long, default_value_t = 256)]
    pub cache_lru: u64,
    /// Disable the LRU tile cache entirely (same as `--cache-lru 0`).
    #[arg(long)]
    pub no_cache_lru: bool,
    /// The COG's own CRS (e.g. `EPSG:32629`). Defaults to the cascais grid `EPSG:3763`.
    #[arg(long)]
    pub src_crs: Option<String>,
    /// Band-math expression over named bands, e.g. `(B08 - B04) / (B08 + B04)`. When set, the
    /// layer is served as on-the-fly band math + value-domain pseudocolor instead of RGBA.
    #[arg(long)]
    pub expression: Option<String>,
    /// Comma-separated band names in physical order, mapping the expression's names to the
    /// COG's bands, e.g. `B02,B03,B04,B08`. Required with `--expression`.
    #[arg(long)]
    pub bands: Option<String>,
    /// Source nodata value; pixels where any referenced band equals it are transparent.
    #[arg(long, allow_hyphen_values = true)]
    pub nodata: Option<f64>,
    /// S3 endpoint URL for an `s3://` COG (overrides `AWS_ENDPOINT_URL`), e.g.
    /// `https://s3.gra.io.cloud.ovh.net`. Credentials come from the environment
    /// (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`).
    #[arg(long)]
    pub s3_endpoint: Option<String>,
    /// S3 region for an `s3://` COG (overrides `AWS_REGION`), e.g. `gra`.
    #[arg(long)]
    pub s3_region: Option<String>,
    /// Layer name for single-`--cog` mode (the `LAYERS=` / TMS `{layer}` value). Default `cascais`.
    #[arg(long)]
    pub name: Option<String>,
    /// Serve a **vector** layer from a GeoJSON file instead of a `--cog`. Served over WMS GetMap
    /// (the only path that draws labels), MVT, and — on any grid the layer publishes — WMTS/TMS
    /// raster tiles.
    /// Pair with `--vec-style` and `--font`; `--src-crs` is the feature CRS (default EPSG:4326).
    #[arg(long)]
    pub vector: Option<String>,
    /// Serve MVT tiles from a pre-built PMTiles archive (read-through); a tile not in the archive is
    /// live-encoded from `--vector`. Requires `--vector`. Opt-in, repeatable — pass once per grid
    /// (each archive self-describes the grid it was baked on via its `grid_id` metadata; serve
    /// auto-maps grid -> archive, so there's no `grid=path` flag grammar). Two archives naming the
    /// same grid is an error.
    #[arg(long)]
    pub pmtiles: Vec<String>,
    /// Serve WMTS/TMS **raster** tiles from a pre-baked PNG PMTiles archive (read-through); a tile
    /// not in the matching-grid archive is rendered live from `--vector`. Requires `--vector`.
    /// Opt-in, repeatable — one archive per grid, exactly like `--pmtiles` (which is the MVT twin of
    /// this flag; the two are separate because an archive handed to the wrong one would serve bytes
    /// of the wrong format behind a 200, so each refuses the other's `tile_type` at startup).
    /// Bake one with `build-pmtiles --tile-format png`.
    #[arg(long)]
    pub raster_pmtiles: Vec<String>,
    /// Enable the write-through cache: a tile not in --pmtiles is live-encoded then persisted to a
    /// crash-safe overlay log beside it, so it's a hit next time. Requires --pmtiles + --vector. One
    /// overlay is opened per `--pmtiles` archive, keyed by the grid it self-describes (mirrors the
    /// read-through `grid_id -> archive` map) — a miss for grid G persists only into G's overlay,
    /// never a differently-gridded one, even though two grids' z/x/y can collide under
    /// `zxy_to_tileid`. SIMPLIFICATION: under `--pmtiles-cache`, every `--pmtiles` path MUST already
    /// exist on disk (its `grid_id` has to be readable to key its overlay) — unlike plain
    /// (non-cache) `--pmtiles`, a not-yet-existing path is a startup error here, not an empty archive.
    #[arg(long)]
    pub pmtiles_cache: bool,
    /// Compact the write-through overlay into the `.pmtiles` every N seconds (0 = only on a size-cap
    /// breach, an explicit `POST /mvt/{layer}/flush`, or shutdown). Requires `--pmtiles-cache`;
    /// ignored otherwise.
    #[arg(long, default_value_t = 0)]
    pub pmtiles_flush_interval: u64,
    /// Compact the write-through overlay once its log exceeds this many MiB (0 = off). Bounds the
    /// overlay footprint between compactions. Requires `--pmtiles-cache`; ignored otherwise.
    #[arg(long, default_value_t = 0)]
    pub pmtiles_overlay_max_mib: u64,
    /// Vector style JSON (marker + text symbolizer) for a `--vector` layer.
    #[arg(long = "vec-style")]
    pub vec_style: Option<String>,
    /// Snap tolerance (source-CRS units) for the shared-arc topology build when `--topology-simplify`
    /// is set. Fine default leaves a clean coverage untouched.
    #[arg(long, default_value_t = 0.01)]
    pub snap_tolerance: f64,
    /// Build a shared-arc topology at startup and serve each border simplified ONCE (seam-free), with
    /// this Weighted-Visvalingam tolerance in source-CRS units (e.g. `25` ≈ drop detail finer than
    /// ~25 m). A `--vector` `.gpkg` layer only; unset = serve the raw coverage.
    #[arg(long)]
    pub topology_simplify: Option<f64>,
    /// Dissolve same-class neighbours into per-class regions at startup (offline; distinct from the
    /// on-the-fly `--mvt-dissolve`) on this attribute field. Composes with `--topology-simplify` (runs
    /// before the topology build). A `--vector` `.gpkg` layer only.
    #[arg(long)]
    pub topology_dissolve: Option<String>,
    /// Roll the `--topology-dissolve` field up to its first N dot-separated levels before dissolving
    /// (e.g. `1` merges COS `1.1.2.1` sub-classes into megaclass `1`) — a coarser, cleaner, lighter
    /// overview. Requires `--topology-dissolve`.
    #[arg(long)]
    pub topology_dissolve_rollup: Option<usize>,
    /// Keep ONLY these comma-separated attribute columns (the `--topology-dissolve` field is always
    /// kept) and drop the rest → smaller tiles + lower memory. A `.gpkg` `--vector` layer only.
    #[arg(long)]
    pub keep_fields: Option<String>,
    /// TrueType font for label text (a `--vector` layer). Default `fixtures/fonts/DejaVuSans.ttf`.
    #[arg(long)]
    pub font: Option<String>,
    /// TMS/WMTS grid id(s) to publish the single `--cog` layer on (repeatable): `from_cog` (default),
    /// `WebMercatorQuad`, `WorldCRS84Quad`, `UPSArcticWGS84Quad`, `UPSAntarcticWGS84Quad` (optionally
    /// with a `_{tile_px}` suffix). Config layers set their own `grids:` instead.
    #[arg(long = "tms-grid")]
    pub tms_grids: Vec<String>,
    /// Tile pixel size for the preset / `from_cog` grids (128/256/512). Default 512.
    #[arg(long = "tms-tile-px", default_value_t = 512)]
    pub tms_tile_px: u32,
    /// Max CONCURRENT renders (admission control). Excess requests QUEUE rather than fail, so peak
    /// memory is hard-bounded no matter how many connections a burst opens. 0 = auto (2× CPU cores).
    #[arg(long, default_value_t = 0)]
    pub max_inflight: usize,
    /// Max features an MVT vector tile emits before the encoder uniformly samples down to keep the
    /// tile bounded (0 = unlimited). Only bites at low/overview zoom; higher = denser overview but
    /// bigger/slower tiles (~2.5 MB per 20k features). Applies to every vector layer.
    #[arg(long, default_value_t = crate::vector::mvt::DEFAULT_MAX_FEATURES_PER_TILE)]
    pub mvt_max_features: usize,
    /// Minimum on-screen feature size (in 256-px display-pixels², WebMercatorQuad-calibrated) for a
    /// POLYGON feature to be drawn at a given zoom. A per-feature, per-layer/per-zoom-constant
    /// selection: every tile makes the identical keep/drop decision, so it thins overview tiles
    /// WITHOUT the density seam that `--mvt-max-features` sampling causes on a complete coverage.
    /// `0` = off (default). Try `1.0` for a wall-to-wall land-cover overview. Zero-area geometries
    /// (points/lines) are exempt.
    ///
    /// Applies to MVT tiles AND to the RASTER rendering of a vector layer — WMS GetMap, WMTS/TMS
    /// PNG tiles, and `build-pmtiles --tile-format png` — which derive the same threshold from the
    /// request's own resolution. (Despite the name; the flag is kept as-is because the CLI is a
    /// frozen contract.) GetFeatureInfo is deliberately NOT gated. On a `postgis://` layer the
    /// threshold is pushed into the SQL `WHERE`, so gated rows are never fetched at all.
    ///
    /// On the MVT path it runs BEFORE `--mvt-max-features`: if the surviving polygons still exceed
    /// that budget the sampler runs too (re-introducing a seam), so set the budget high enough that
    /// selection alone bounds the tile. Applies to every vector layer.
    #[arg(long, default_value_t = 0.0)]
    pub mvt_min_feature_px: f64,
    /// Override `--mvt-min-feature-px` for the RASTER paths only (WMS GetMap, WMTS/TMS PNG tiles,
    /// `build-pmtiles --tile-format png`). Unset = the raster paths use `--mvt-min-feature-px`.
    ///
    /// Why the two want different numbers, measured on EU5 `buildings` (107.9M rows):
    ///
    /// * MVT wants a LARGE value (2 is typical) as a **cartographic** choice — it empties the
    ///   low-zoom vector tiles a browser would otherwise choke on (an ungated z6 tile of this data
    ///   is 157 MB).
    /// * Raster wants a SMALL value as a **safety** one. Because the threshold is a pixel budget
    ///   converted to ground area, it scales with resolution²: `0.05` is ~36 km² at z1 (so an
    ///   accidental world-wide request still costs 0.07 GiB instead of 52.6 GiB) but only ~73 m² at
    ///   z11, where it leaves 99.6 % of the drawn tile intact. `2` at z11 is ~1,400 m² and removes
    ///   over half of it.
    ///
    /// ⚠ A raster tile pyramid at low zoom is MADE of sub-pixel features: at z8 the map is the
    /// aggregate texture of thousands of buildings none of which fills a pixel, so ANY gate thins it
    /// hard (531 KB ungated -> 131 KB at 0.05 -> effectively blank at 2). Bake low zoom UNGATED, and
    /// keep the serve-time value small enough that the first live zoom above the archive still
    /// matches it — otherwise the map visibly pops at the archive boundary.
    #[arg(long)]
    pub raster_min_feature_px: Option<f64>,
    /// Disable the always-on MVT geometry optimizations (currently grid-snap vertex dedup), emitting
    /// the RAW rounded rings. The opt-in thinning flags (`--mvt-min-feature-px`, `--mvt-cell-px`)
    /// are independent and still apply. NOTE: an A/B DIAGNOSTIC — raw output may carry zero-delta
    /// segments a strict MVT decoder rejects; not a production mode. Applies to every vector layer.
    #[arg(long = "no-optimizations", default_value_t = false)]
    pub mvt_no_optimizations: bool,
    /// Lift the per-tile feature cap entirely (unlimited), overriding `--mvt-max-features`. Prints a
    /// prominent WARNING: uncapped tiles may OOM the server or crash the browser (OpenLayers
    /// allocates one JS object per feature). Applies to every vector layer.
    #[arg(long = "no-safety-limit", default_value_t = false)]
    pub mvt_no_safety_limit: bool,
    /// Overview CELL MOSAIC: fill the black holes a size filter leaves on a wall-to-wall coverage by
    /// replacing polygons with a dominant-class grid of N display-pixel cells (rounded to a power of
    /// 2 in {4..256}). Seam-free and hard-caps tile weight; blocky at EVERY zoom (an overview tool —
    /// band it with `--mvt-cell-max-zoom`). REQUIRES `--mvt-cell-field`. `0` = off. Applies to the
    /// polygons of every vector layer that carries the field; points/lines pass through.
    #[arg(long, default_value_t = 0.0)]
    pub mvt_cell_px: f64,
    /// The thematic class attribute the cell mosaic (`--mvt-cell-px`) votes on per cell, e.g.
    /// `COS18_n4_C`. Validated at load against each layer's schema; a layer lacking it renders real
    /// geometry (mosaic disabled there).
    #[arg(long)]
    pub mvt_cell_field: Option<String>,
    /// Restrict the cell mosaic (`--mvt-cell-px`) to zoom ≤ this (a per-zoom constant → still
    /// seam-free); real geometry renders above it. `0` = every zoom (blocky when zoomed in).
    #[arg(long, default_value_t = 0)]
    pub mvt_cell_max_zoom: u32,
    /// Same-class **DISSOLVE**: merge adjacent polygons of the same `<FIELD>` value by
    /// edge-cancellation → true class boundaries (no squares), hole-free, interactive vector. The
    /// quality hole-fill (vs the blocky `--mvt-cell-px`). Validated per layer; **mutually exclusive
    /// with `--mvt-cell-px`** (dissolve wins). Polygons-only; points/lines pass through. Costliest at
    /// low zoom — band it with `--mvt-dissolve-max-zoom`.
    #[arg(long)]
    pub mvt_dissolve: Option<String>,
    /// Restrict the dissolve (`--mvt-dissolve`) to zoom ≤ this (per-zoom constant → seam-safe); real
    /// geometry with full attributes renders above it. `0` = every zoom.
    #[arg(long, default_value_t = 0)]
    pub mvt_dissolve_max_zoom: u32,
    /// Bounded cache of encoded MVT tile bytes — max **N MiB** (`0` = off). Computes each
    /// `layer/tms/z/x/y` once (single-flight) and reuses it — the mitigation for costly passes like
    /// `--mvt-dissolve` at low zoom (warm requests instant). Byte-weighted → RSS hard-bounded. Shared
    /// by the `/mvt` + WMTS routes.
    #[arg(long, default_value_t = 256)]
    pub mvt_cache: u64,
    /// Bounded cache of rendered WMS GetMap PNG bytes — max **N MiB** (`0` = off). Renders each
    /// GetMap once (keyed by its query) and reuses it — the mitigation for a costly vector render
    /// (e.g. the X-ray raster underlay); revisited tiles become instant. Byte-weighted → RSS bounded.
    #[arg(long, default_value_t = 256)]
    pub wms_cache: u64,
    /// Path to a MapLibre GL style for `/mvt/{layer}/style.json` — a JSON object
    /// `{ "layers": [...], "metadata": { "legend": [...] } }` (or a bare `[...]` layer array). The
    /// server injects `version`/`sources`/source-binding. Without it, a generic X-ray style is served.
    #[arg(long = "mvt-style")]
    pub mvt_style: Option<String>,
}

/// Run the async HTTP WMS server (blocks until shutdown). Publishes either the layers from
/// `--config layers.yaml` or a single layer from the `--cog`/`--style`/… flags.
pub fn run_serve(args: &ServeArgs) -> Result<(), Error> {
    let base_url = args
        .public_url
        .clone()
        .unwrap_or_else(|| format!("http://{}:{}/wms", args.host, args.port));
    // S3 defaults: env vars, with the global CLI flags layered on top.
    let s3_env = s3::S3Config::from_env().merge(s3::S3Config {
        endpoint: args.s3_endpoint.clone(),
        region: args.s3_region.clone(),
        ..Default::default()
    });
    if args.no_cache_lru || args.cache_lru == 0 {
        println!("LRU tile cache: disabled");
    } else {
        println!("LRU tile cache: enabled ({} MiB per layer)", args.cache_lru);
    }

    if !args.pmtiles.is_empty() && args.vector.is_none() {
        return Err("--pmtiles requires --vector".into());
    }
    if !args.raster_pmtiles.is_empty() && args.vector.is_none() {
        return Err("--raster-pmtiles requires --vector".into());
    }
    if args.pmtiles_cache && (args.pmtiles.is_empty() || args.vector.is_none()) {
        return Err("--pmtiles-cache requires --pmtiles and --vector".into());
    }

    let layers = if let Some(vec_path) = &args.vector {
        // Single vector layer from --vector/--vec-style/--font (WMS GetMap + MVT + tiled raster).
        let vec_style = args
            .vec_style
            .as_deref()
            .ok_or("--vector needs --vec-style")?;
        let font = args
            .font
            .as_deref()
            .unwrap_or("fixtures/fonts/DejaVuSans.ttf");
        let name = args.name.clone().unwrap_or_else(|| "vector".to_string());
        // CLI custom grids come only via --config, so an empty map here — the flags select
        // presets/`from_cog` only (mirrors the `--cog` single-layer path below).
        let no_custom_grids = std::collections::BTreeMap::new();
        let spec = layer::VectorLayerSpec::from_serve_args(
            args,
            name,
            vec_path.to_string(),
            vec_style.to_string(),
            font.to_string(),
            // For a single `--vector`, the global `--src-crs` IS this layer's declaration.
            args.src_crs.clone(),
        )
        .with_grids(args.tms_grids.clone(), args.tms_tile_px, no_custom_grids)
        .with_pmtiles(args.pmtiles.clone())
        .with_raster_pmtiles(args.raster_pmtiles.clone());
        let mut layer = layer::build_vector_layer(&spec, &s3_env)?;
        // Spec 2 write-through supersedes Spec 1 read-through when --pmtiles-cache is set: each
        // overlay owns the (swappable) base reader for its own grid and Layer.pmtiles stays empty
        // (the overlays own the bases instead). One overlay is opened PER --pmtiles archive, keyed
        // by the grid_id that archive self-describes — mirrors build_vector_layer's read-through
        // `pmtiles` map above, so a miss for grid G persists only into G's overlay (never a
        // differently-gridded one, even though two grids' z/x/y can collide under `zxy_to_tileid`).
        // SIMPLIFICATION: unlike plain --pmtiles (Spec 1, which tolerates a not-yet-existing path
        // and reads through nothing until one appears), --pmtiles-cache requires every archive to
        // already exist on disk — its grid_id has to be readable up front to key its overlay, so a
        // missing path is a clear startup error here rather than an empty-cache no-op.
        if args.pmtiles_cache {
            let mut overlays: std::collections::BTreeMap<
                String,
                std::sync::Arc<vector::pmtiles::overlay::TileOverlay>,
            > = std::collections::BTreeMap::new();
            for p in &args.pmtiles {
                if !std::path::Path::new(p).exists() {
                    return Err(format!(
                        "--pmtiles-cache requires each --pmtiles archive to already exist \
                         (its grid_id must be readable to key its overlay): '{p}' not found"
                    )
                    .into());
                }
                let base = std::sync::Arc::new(vector::pmtiles::read::PmtilesReader::open(
                    std::path::Path::new(p),
                )?);
                // Same refusal `build_vector_layer` applies to a plain `--pmtiles` archive: this
                // open bypasses that loop, and an overlay over a PNG base would write MVT tiles
                // into a raster archive.
                base.require_tile_type(vector::pmtiles::write::TILE_TYPE_MVT, p)?;
                let grid_id = base.grid_id();
                if overlays.contains_key(&grid_id) {
                    return Err(format!(
                        "two --pmtiles archives both target grid '{grid_id}' under --pmtiles-cache: {p}"
                    )
                    .into());
                }
                let wal = format!("{p}.wal");
                let ov = std::sync::Arc::new(vector::pmtiles::overlay::TileOverlay::open(
                    std::path::Path::new(&wal),
                    Some(base),
                )?);
                // Size-cap trigger (task 6): a `put` past this many bytes wakes the compaction
                // controller for THIS grid's overlay.
                ov.set_max_bytes(args.pmtiles_overlay_max_mib.saturating_mul(1024 * 1024));
                ov.set_metadata(crate::mvt_http::pmtiles_metadata_json(
                    &layer,
                    Some(&grid_id),
                ));
                overlays.insert(grid_id, ov);
            }
            layer.pmtiles = std::collections::BTreeMap::new();
            layer.overlay = overlays;
        }
        vec![layer]
    } else if let Some(cfg_path) = &args.config {
        let cfg = config::Config::load(cfg_path)?;
        // Same font fallback as the single --vector path above, for any `vector:` layers here.
        let font = args
            .font
            .as_deref()
            .unwrap_or("fixtures/fonts/DejaVuSans.ttf");
        let mut layers = Vec::with_capacity(cfg.layers.len());
        for lc in &cfg.layers {
            let layer = if let Some(vpath) = &lc.vector {
                let vstyle = lc.vec_style.as_deref().ok_or_else(|| {
                    format!("layer '{}': a `vector` layer needs a `vec_style`", lc.name)
                })?;
                let s3 = s3_env.clone().merge(s3::S3Config {
                    endpoint: lc.s3_endpoint.clone(),
                    region: lc.s3_region.clone(),
                    ..Default::default()
                });
                let spec = layer::VectorLayerSpec::from_serve_args(
                    args,
                    lc.name.clone(),
                    vpath.clone(),
                    vstyle.to_string(),
                    font.to_string(),
                    // THIS layer's declaration, not the global flag. Reading the global here is
                    // what silently discarded every per-layer `src_crs:` before bc21155.
                    lc.src_crs.clone(),
                )
                .with_grids(lc.grids.clone(), lc.tile_px, cfg.grids.clone())
                .with_pmtiles(lc.pmtiles.clone())
                .with_raster_pmtiles(lc.raster_pmtiles.clone())
                .with_extent(lc.extent)
                .with_columns(lc.columns.clone());
                layer::build_vector_layer(&spec, &s3)?
            } else {
                let cog = lc.cog.as_deref().ok_or_else(|| {
                    format!("layer '{}': needs a `cog` or `vector` source", lc.name)
                })?;
                // Same per-layer S3Config the COG open below uses (CLI-global env, overridden by
                // this layer's `s3_endpoint`/`s3_region`) — the style path may itself be `s3://`.
                let s3 = s3_env.clone().merge(s3::S3Config {
                    endpoint: lc.s3_endpoint.clone(),
                    region: lc.s3_region.clone(),
                    ..Default::default()
                });
                let style_path = lc
                    .style
                    .as_deref()
                    .ok_or_else(|| format!("layer '{}': a `cog` layer needs a `style`", lc.name))?;
                let style =
                    style::Style::parse(style_path, &assets::read_config_string(style_path, &s3)?)?;
                let band_math = match &lc.expression {
                    Some(e) => {
                        let names = lc.band_names_ordered();
                        if names.iter().any(|n| n.is_empty()) || names.is_empty() {
                            return Err(format!(
                                "layer '{}': expression needs a `bands` map",
                                lc.name
                            )
                            .into());
                        }
                        Some(layer::build_band_math(e, &names, lc.nodata)?)
                    }
                    None => None,
                };
                layer::build_layer(
                    lc.name.clone(),
                    cog.to_string(),
                    style,
                    lc.src_crs.clone().unwrap_or_else(config::default_src_crs),
                    band_math,
                    s3,
                    &lc.grids,
                    lc.tile_px,
                    &cfg.grids,
                    args,
                )?
            };
            layers.push(layer);
        }
        layers
    } else {
        // Single layer from flags.
        let cog = args.cog.clone().ok_or("serve needs --cog (or --config)")?;
        let style_path = args.style.as_deref().ok_or("serve needs --style")?;
        let style = style::Style::parse(
            style_path,
            &assets::read_config_string(style_path, &s3_env)?,
        )?;
        let src_crs = args
            .src_crs
            .clone()
            .unwrap_or_else(|| reproj::SRC_CRS.to_string());
        let band_math = match (&args.expression, &args.bands) {
            (Some(e), Some(spec)) => {
                let names: Vec<String> = spec.split(',').map(|s| s.trim().to_string()).collect();
                Some(layer::build_band_math(e, &names, args.nodata)?)
            }
            (Some(_), None) => return Err("--expression requires --bands".into()),
            _ => None,
        };
        let grid_ids = if args.tms_grids.is_empty() {
            config::default_grids()
        } else {
            args.tms_grids.clone()
        };
        let no_custom = std::collections::BTreeMap::new();
        let name = args.name.clone().unwrap_or_else(|| "cascais".to_string());
        vec![layer::build_layer(
            name,
            cog,
            style,
            src_crs,
            band_math,
            s3_env.clone(),
            &grid_ids,
            args.tms_tile_px,
            &no_custom,
            args,
        )?]
    };

    println!(
        "serving {} layer(s): {}",
        layers.len(),
        layers
            .iter()
            .map(|l| l.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    // Admission control: bound concurrent renders so a connection burst can't balloon RSS. Default
    // 2× cores (enough to saturate the machine, tight enough to keep peak memory hard-bounded).
    // `layer::resolve_max_inflight` is the same formula `VectorLayerSpec.max_inflight` resolves
    // during layer construction above (for the PostGIS pool-sizing warning) — pulled into one
    // function so the two call sites can't drift apart.
    let max_inflight = layer::resolve_max_inflight(args.max_inflight);
    println!("admission control: max {max_inflight} concurrent renders (excess requests queue)");
    let mut state = server::ServeState::new(layers, base_url, max_inflight);
    // Kept separate from `base_url` (which falls back to the bind address) so the MVT
    // TileJSON/style.json origin can tell "operator declared the public URL" from "we guessed",
    // and prefer the declared one over the request's Host header. See `ServeState.public_url`.
    state.public_url = args.public_url.clone();
    state.pmtiles_flush_interval = args.pmtiles_flush_interval;
    state.mvt_max_features = args.mvt_max_features;
    state.mvt_min_feature_px = args.mvt_min_feature_px;
    state.mvt_no_optimizations = args.mvt_no_optimizations;
    if args.mvt_no_optimizations {
        println!(
            "MVT optimizations DISABLED (--no-optimizations): grid-snap dedup off; raw rings (diagnostic)"
        );
    }
    state.mvt_no_safety_limit = args.mvt_no_safety_limit;
    if args.mvt_no_safety_limit {
        eprintln!(
            "⚠ WARNING: --no-safety-limit — per-tile feature count is UNCAPPED; dense tiles may OOM \
             the server or crash the browser (OpenLayers allocates one JS object per feature)"
        );
    }
    // Cell mosaic (--mvt-cell-px): validate the flag pair, wire it, and emit one-time load logs.
    crate::vector::mvt::validate_cell_flags(args.mvt_cell_px, &args.mvt_cell_field)?;
    state.mvt_cell_px = args.mvt_cell_px;
    state.mvt_cell_field = args.mvt_cell_field.clone();
    state.mvt_cell_max_zoom = args.mvt_cell_max_zoom;
    if args.mvt_cell_px > 0.0 {
        let field = args.mvt_cell_field.as_deref().unwrap_or_default(); // validated present above
        let n = crate::vector::mvt::cell_units(args.mvt_cell_px) / 16;
        if (n as f64 - args.mvt_cell_px).abs() > f64::EPSILON {
            println!(
                "MVT cell mosaic: --mvt-cell-px {} rounded to {n} px (power of 2)",
                args.mvt_cell_px
            );
        }
        for lyr in &state.layers {
            if let Some(v) = &lyr.vector {
                if !v.fields.contains_key(field) {
                    eprintln!(
                        "⚠ --mvt-cell-field '{field}' not on layer '{}' — cell mosaic disabled there",
                        lyr.name
                    );
                }
            }
        }
        let band = if args.mvt_cell_max_zoom == 0 {
            "all zooms".to_string()
        } else {
            format!("z≤{}", args.mvt_cell_max_zoom)
        };
        println!("MVT cell mosaic: {n} px cells, field '{field}' ({band})");
    }
    // Dissolve (--mvt-dissolve): wire it + validate the field per layer + mutual-exclusion warning.
    state.mvt_dissolve_field = args.mvt_dissolve.clone();
    state.mvt_dissolve_max_zoom = args.mvt_dissolve_max_zoom;
    if let Some(field) = &args.mvt_dissolve {
        if args.mvt_cell_px > 0.0 {
            eprintln!(
                "⚠ --mvt-dissolve and --mvt-cell-px are mutually exclusive — dissolve wins (mosaic off)"
            );
        }
        for lyr in &state.layers {
            if let Some(v) = &lyr.vector {
                if !v.fields.contains_key(field) {
                    eprintln!(
                        "⚠ --mvt-dissolve '{field}' not on layer '{}' — dissolve disabled there",
                        lyr.name
                    );
                }
            }
        }
        let band = if args.mvt_dissolve_max_zoom == 0 {
            "all zooms".to_string()
        } else {
            format!("z≤{}", args.mvt_dissolve_max_zoom)
        };
        println!("MVT dissolve: same-class merge on field '{field}' ({band})");
    }
    if args.mvt_cache > 0 {
        state.mvt_cache = Some(mvt_http::build_byte_cache(args.mvt_cache));
        println!(
            "MVT tile cache: up to {} MiB (compute-once, single-flight)",
            args.mvt_cache
        );
    }
    if args.wms_cache > 0 {
        state.wms_cache = Some(mvt_http::build_byte_cache(args.wms_cache));
        println!(
            "WMS render cache: up to {} MiB (GetMap PNGs, compute-once)",
            args.wms_cache
        );
    }
    if args.mvt_min_feature_px > 0.0 {
        println!(
            "MVT min feature size: {} px² (per-zoom seam-free selection)",
            args.mvt_min_feature_px
        );
    }
    if let Some(path) = &args.mvt_style {
        let text =
            assets::read_config_string(path, &s3_env).map_err(|e| format!("--mvt-style {e}"))?;
        let val: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("--mvt-style {path}: {e}"))?;
        println!("MVT style: {path} (served at /mvt/{{layer}}/style.json)");
        // `--mvt-style` is opaque pass-through: it is served to the client verbatim and never
        // parsed into a `Style`, so its `["get", FIELD]` expressions are invisible to
        // `Style::referenced_fields`. That is harmless for a file source, which carries every
        // field anyway, and quietly fatal for a `postgis://` layer, whose SELECT column list is
        // built FROM `referenced_fields`: the tiles then ship with no class attribute and the
        // client draws the entire map in its fallback colour, 200 OK throughout. Warn per layer,
        // matching the standard `--mvt-cell-field` / `--mvt-dissolve` already set above, and name
        // the escape hatch.
        for field in mvt_http::mvt_style_fields(&val) {
            for lyr in &state.layers {
                let Some(v) = &lyr.vector else { continue };
                if !v.fields.contains_key(&field) {
                    eprintln!(
                        "⚠ --mvt-style reads field '{field}', which layer '{}' does not carry — \
                         it will style with its fallback paint. For a postgis:// layer, add it to \
                         that layer's `columns:` in the --config YAML.",
                        lyr.name
                    );
                }
            }
        }
        state.mvt_style = Some(val);
    }
    if args.mvt_max_features != crate::vector::mvt::DEFAULT_MAX_FEATURES_PER_TILE {
        let cap = if args.mvt_max_features == 0 {
            "unlimited".to_string()
        } else {
            args.mvt_max_features.to_string()
        };
        println!("MVT per-tile feature budget: {cap}");
    }
    server::run(state, &args.host, args.port)
}
