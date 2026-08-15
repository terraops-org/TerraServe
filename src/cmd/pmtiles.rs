// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! The `build-pmtiles` subcommand: bake an offline `.pmtiles` vector pyramid from a vector source.
//!
//! It drives the SAME `encode_tile_opt` over the same grid that the live `/mvt` route uses, so a
//! baked archive is byte-comparable with what the server would have rendered on demand. That is
//! the whole point of the verb: precomputing is only a safe substitute for live rendering if the
//! two paths cannot drift, so they share one encoder rather than having a "bake" variant.

use crate::cmd::render::parse_bbox;
use crate::layer::build_vector_layer;
use crate::s3;
use crate::server;
use crate::tms;
use crate::vector;
use crate::Error;
use crate::ServeArgs;
use clap::Args;

/// `build-pmtiles` arguments — offline pyramid generation (PMTiles task 6). Drives the SAME
/// `encode_tile_opt` over a WebMercatorQuad grid the live `/mvt` route uses, so archive bytes match a
/// live render. The MVT/topology optimization flags below are copied VERBATIM from `ServeArgs` so a
/// pyramid can be baked with the identical generalization an operator would serve interactively.
#[derive(Args, Debug)]
pub struct BuildPmtilesArgs {
    /// Vector source (.gpkg, .fgb, or GeoJSON) to bake into the pyramid.
    #[arg(long)]
    pub vector: Option<String>,
    /// Output `.pmtiles` path.
    #[arg(long)]
    pub out: String,
    /// Lowest zoom to generate (inclusive).
    #[arg(long, default_value_t = 0)]
    pub min_zoom: u8,
    /// Highest zoom to generate (inclusive). Capped at 26 (PMTiles Hilbert TileID interop cap).
    #[arg(long, default_value_t = 14)]
    pub max_zoom: u8,
    /// WGS84 bbox override `W,S,E,N` (values may be negative); default = the layer's own bounds.
    #[arg(long, allow_hyphen_values = true)]
    pub bbox: Option<String>,
    /// Source-CRS bounds `minx,miny,maxx,maxy` for a `postgis://` source.
    ///
    /// REQUIRED for PostGIS and ignored otherwise. A file source carries its own extent in its
    /// header or index; a database does not offer one cheaply, so TerraServe never asks:
    /// `ST_EstimatedExtent` is NULL on a never-`ANALYZE`d table and `ST_Extent` is a full-table
    /// scan. Compute it once with `ST_Extent` and pass it here.
    ///
    /// NOT the same as `--bbox`, which is a WGS84 clip on WHICH TILES to bake. This is the
    /// layer's own extent in its own CRS.
    #[arg(long, allow_hyphen_values = true)]
    pub extent: Option<String>,
    /// Temp dir for the streamed data section (default: system temp).
    #[arg(long)]
    pub tmpdir: Option<String>,
    /// Vector style JSON (marker + text symbolizer) for the layer.
    #[arg(long = "vec-style")]
    pub vec_style: Option<String>,
    /// The feature CRS (default EPSG:4326; a `.gpkg`'s own CRS is auto-detected when unset).
    #[arg(long)]
    pub src_crs: Option<String>,
    /// TrueType font for label text. Default `fixtures/fonts/DejaVuSans.ttf`.
    #[arg(long)]
    pub font: Option<String>,
    /// MVT layer name embedded in the tiles (the `source-layer` a client style targets) and the
    /// metadata layer id. Match whatever name you will `serve` / style this data under. Default
    /// `"vector"` (matching `serve --vector`'s default layer name).
    #[arg(long)]
    pub name: Option<String>,
    // --- MVT / topology optimization flags copied VERBATIM from ServeArgs (byte-parity with serve) ---
    #[arg(long, default_value_t = crate::vector::mvt::DEFAULT_MAX_FEATURES_PER_TILE)]
    pub mvt_max_features: usize,
    /// See `serve --mvt-min-feature-px`. Applies to BOTH bake formats: it selects features for the
    /// MVT encoder and, since the gate reached the raster paths, for a `--tile-format png` bake too
    /// (which renders through the same `VectorLayer::render_tile` the live server uses).
    #[arg(long, default_value_t = 0.0)]
    pub mvt_min_feature_px: f64,
    /// See `serve --raster-min-feature-px`. Overrides the value above for a `--tile-format png`
    /// bake only. ⚠ Low-zoom raster tiles of a dense layer are MADE of sub-pixel features -- their
    /// map IS the aggregate texture -- so a low-zoom pyramid usually wants this at `0`, even when
    /// the MVT bake of the same data wants `2`.
    #[arg(long)]
    pub raster_min_feature_px: Option<f64>,
    #[arg(long = "no-optimizations", default_value_t = false)]
    pub mvt_no_optimizations: bool,
    #[arg(long = "no-safety-limit", default_value_t = false)]
    pub mvt_no_safety_limit: bool,
    #[arg(long, default_value_t = 0.0)]
    pub mvt_cell_px: f64,
    #[arg(long)]
    pub mvt_cell_field: Option<String>,
    #[arg(long, default_value_t = 0)]
    pub mvt_cell_max_zoom: u32,
    #[arg(long)]
    pub mvt_dissolve: Option<String>,
    #[arg(long, default_value_t = 0)]
    pub mvt_dissolve_max_zoom: u32,
    #[arg(long, default_value_t = 0.01)]
    pub snap_tolerance: f64,
    #[arg(long)]
    pub topology_simplify: Option<f64>,
    #[arg(long)]
    pub topology_dissolve: Option<String>,
    #[arg(long)]
    pub topology_dissolve_rollup: Option<usize>,
    #[arg(long)]
    pub keep_fields: Option<String>,
    /// TileMatrixSet to bake on: a preset id (`WebMercatorQuad` default, `WorldCRS84Quad`,
    /// `UPSArcticWGS84Quad`, `UPSAntarcticWGS84Quad`) OR a path to an OGC TileMatrixSet 2.0 JSON
    /// file (a custom national/regional grid, e.g. swissLV95). The archive is baked with THIS grid's
    /// z/x/y and stamps its `grid_id` into metadata, so serve reads it only for matching-grid requests.
    #[arg(long, default_value = "WebMercatorQuad")]
    pub grid: String,
    /// What the tiles CONTAIN: `mvt` (default — vector tiles for `/mvt`, `serve --pmtiles`) or
    /// `png` (rendered raster tiles for WMTS/TMS, `serve --raster-pmtiles`).
    ///
    /// The archive's header declares this (`tile_type` 1 or 2), so a reader — ours or anyone
    /// else's — can refuse a payload it cannot draw instead of handing a client the wrong bytes.
    /// A PNG bake renders through the SAME `VectorLayer::render_tile` the live WMTS/TMS miss path
    /// uses, so a baked tile cannot drift from the one it replaces.
    #[arg(long = "tile-format", default_value = "mvt")]
    pub tile_format: String,
    /// Tile PIXEL size for a `--tile-format png` bake (ignored for MVT, whose encoder uses a fixed
    /// 4096-unit local extent). Default 512 — the same `tile_px` default `serve`/`--config` publish
    /// grids at, so a default bake matches a default serve. It must equal the served grid's tile
    /// size or serve refuses the archive at startup.
    #[arg(long = "tile-px", default_value_t = 512)]
    pub tile_px: u32,
}

/// Build a `.pmtiles` pyramid offline. Constructs the SAME `Layer` + `MvtOptimizations` `run_serve`
/// builds for a single `--vector` layer (so archived tiles are byte-identical to a live `/mvt`
/// render), then drives `vector::pmtiles::generate::build_pmtiles` over a WebMercatorQuad grid.
pub fn run_build_pmtiles(args: &BuildPmtilesArgs) -> Result<(), Error> {
    // Validate up front — cheap checks before any file I/O.
    if args.out.is_empty() {
        return Err("build-pmtiles: --out is required".into());
    }
    if args.min_zoom > args.max_zoom {
        return Err(format!(
            "--min-zoom {} > --max-zoom {}",
            args.min_zoom, args.max_zoom
        )
        .into());
    }
    if args.max_zoom > 26 {
        return Err("--max-zoom must be <= 26 (PMTiles Hilbert TileID interop cap)".into());
    }
    let raster = match args.tile_format.to_ascii_lowercase().as_str() {
        "mvt" => false,
        "png" => true,
        other => {
            return Err(format!("--tile-format {other}: expected `mvt` or `png`").into());
        }
    };
    let bbox_override = match &args.bbox {
        Some(s) => Some(parse_bbox(s)?),
        None => None,
    };

    // Reconstruct a ServeArgs so we can reuse `build_vector_layer` unchanged — the vector/style/crs/
    // font + every MVT/topology knob come from `args`; all serve-only fields take their defaults.
    let serve_args = ServeArgs {
        config: None,
        cog: None,
        style: None,
        host: "127.0.0.1".into(),
        port: 8080,
        public_url: None,
        cache_lru: 256,
        no_cache_lru: false,
        src_crs: args.src_crs.clone(),
        expression: None,
        bands: None,
        nodata: None,
        s3_endpoint: None,
        s3_region: None,
        name: args.name.clone(),
        vector: args.vector.clone(),
        pmtiles: Vec::new(),
        raster_pmtiles: Vec::new(),
        pmtiles_cache: false,
        pmtiles_flush_interval: 0,
        pmtiles_overlay_max_mib: 0,
        vec_style: args.vec_style.clone(),
        snap_tolerance: args.snap_tolerance,
        topology_simplify: args.topology_simplify,
        topology_dissolve: args.topology_dissolve.clone(),
        topology_dissolve_rollup: args.topology_dissolve_rollup,
        keep_fields: args.keep_fields.clone(),
        font: args.font.clone(),
        tms_grids: Vec::new(),
        tms_tile_px: 512,
        max_inflight: 0,
        mvt_max_features: args.mvt_max_features,
        mvt_min_feature_px: args.mvt_min_feature_px,
        raster_min_feature_px: args.raster_min_feature_px,
        mvt_no_optimizations: args.mvt_no_optimizations,
        mvt_no_safety_limit: args.mvt_no_safety_limit,
        mvt_cell_px: args.mvt_cell_px,
        mvt_cell_field: args.mvt_cell_field.clone(),
        mvt_cell_max_zoom: args.mvt_cell_max_zoom,
        mvt_dissolve: args.mvt_dissolve.clone(),
        mvt_dissolve_max_zoom: args.mvt_dissolve_max_zoom,
        mvt_cache: 0,
        wms_cache: 0,
        mvt_style: None,
    };

    // Build the layer exactly as `run_serve` does for a single `--vector` layer.
    let vector_path = serve_args
        .vector
        .as_deref()
        .ok_or("build-pmtiles needs --vector")?;
    let vec_style = serve_args
        .vec_style
        .as_deref()
        .ok_or("--vector needs --vec-style")?;
    let font = serve_args
        .font
        .as_deref()
        .unwrap_or("fixtures/fonts/DejaVuSans.ttf");
    // Pass the operator's DECLARATION through, Option and all, rather than resolving a default
    // here. Resolving early is what hid the config bug: once "unset" has been turned into a
    // concrete string, nothing downstream can tell it from an explicit choice.
    //
    // ONE behaviour change: when a vector file declares no CRS at all, the assumed fallback moves
    // from EPSG:4326 to the shared `config::default_src_crs()` (EPSG:3763). That arm already
    // prints a loud warning telling the operator to pass --src-crs.
    let declared_crs = serve_args.src_crs.clone();
    let layer_name = args.name.clone().unwrap_or_else(|| "vector".to_string());
    // build-pmtiles has no --config / custom-grid flags of its own; `serve_args.tms_grids` was
    // reconstructed empty above, so this mirrors the single --vector CLI path (no custom grids).
    let no_custom_grids = std::collections::BTreeMap::new();
    // build-pmtiles has no --s3-endpoint/--s3-region flags of its own (BuildPmtilesArgs carries
    // none), so env-only — same as `run_serve`'s `s3_env` before its CLI overlay is applied.
    let s3 = s3::S3Config::from_env();
    // Parsed with the same helper as --bbox so the two report errors identically.
    let extent = match args.extent.as_deref() {
        Some(s) => Some(parse_bbox(s)?),
        None => None,
    };
    let spec = crate::layer::VectorLayerSpec::from_serve_args(
        &serve_args,
        layer_name,
        vector_path.to_string(),
        vec_style.to_string(),
        font.to_string(),
        declared_crs,
    )
    .with_extent(extent)
    .with_grids(
        serve_args.tms_grids.clone(),
        serve_args.tms_tile_px,
        no_custom_grids,
    )
    .with_pmtiles(serve_args.pmtiles.clone());
    let layer = build_vector_layer(&spec, &s3)?;

    // Build the optimization set the SAME way `run_serve` does: a minimal ServeState carrying the
    // MVT/cell/dissolve flags, then `MvtOptimizations::for_layer` (reads the layer's `area_scale` +
    // schema). No server, no caches — just the knobs the encoder reads.
    let mut state = server::ServeState::new(vec![], String::new(), 1);
    state.mvt_max_features = serve_args.mvt_max_features;
    state.mvt_min_feature_px = serve_args.mvt_min_feature_px;
    state.mvt_no_optimizations = serve_args.mvt_no_optimizations;
    state.mvt_no_safety_limit = serve_args.mvt_no_safety_limit;
    crate::vector::mvt::validate_cell_flags(serve_args.mvt_cell_px, &serve_args.mvt_cell_field)?;
    state.mvt_cell_px = serve_args.mvt_cell_px;
    state.mvt_cell_field = serve_args.mvt_cell_field.clone();
    state.mvt_cell_max_zoom = serve_args.mvt_cell_max_zoom;
    state.mvt_dissolve_field = serve_args.mvt_dissolve.clone();
    state.mvt_dissolve_max_zoom = serve_args.mvt_dissolve_max_zoom;
    let vlayer = layer.vector.as_ref().unwrap();
    let opts = crate::vector::mvt::MvtOptimizations::for_layer(&state, vlayer);

    // Load the requested grid: a preset id, else a path to an OGC TileMatrixSet 2.0 JSON file.
    // Presets use tile_px 4096 (the MVT encode extent); the tile bbox is tile_w-invariant, so the
    // default `WebMercatorQuad` stays byte-identical. A custom JSON loads exactly as serve's
    // `--config grids:` do (`from_ogc_json`), so a baked tile matches the live one for that grid.
    // A raster bake needs REAL pixels: presets resolve at `--tile-px` (default 512, matching serve's
    // own `tile_px` default) instead of the MVT encoder's 4096-unit local extent, which as a pixel
    // count would mean 4096x4096 PNGs. The `.id` overwrite below applies to both formats.
    let preset_px = if raster { args.tile_px } else { 4096 };
    let grid = match tms::preset(&args.grid, preset_px) {
        // `tms::preset` suffixes `.id` with the tile_px it actually built (4096 — the MVT-baking
        // resolution passed above, unrelated to any client-facing grid-size convention) whenever
        // that differs from 256, e.g. "WebMercatorQuad" -> "WebMercatorQuad_4096". A live MVT
        // request for the SAME grid always asks for the bare preset name (`resolve_grid`'s preset
        // fallback in mvt_http.rs resolves it at 4096 too, but the REQUEST string stays whatever the
        // client sent). Stamping the suffixed id would make Task 3's `grid_id -> reader` lookup
        // silently miss the archive for exactly the default/back-compat grid this whole pipeline
        // exists to keep byte-identical — so stamp the id the operator actually asked for
        // (`args.grid`) instead. `.id` isn't read anywhere else in `build_pmtiles` (only in the
        // metadata stamp — see `generate.rs`), so overwriting it here is safe.
        Some(mut g) => {
            g.id = args.grid.clone();
            g
        }
        None => {
            let json = std::fs::read_to_string(&args.grid)
                .map_err(|e| format!("--grid {}: {e}", args.grid))?;
            tms::from_ogc_json(&json).map_err(|e| format!("--grid {}: {e}", args.grid))?
        }
    };
    let bbox = bbox_override.unwrap_or(layer.bounds_wgs84);
    let tmp = args
        .tmpdir
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let counts = if raster {
        // The MVT thinning knobs describe an ENCODER this path never runs — a raster tile is drawn
        // by the style, not sampled by a feature cap. Say so rather than letting a flag quietly do
        // nothing (`--no-safety-limit` in particular reads like it lifts the PostGIS query cap; it
        // does not — that is TERRASERVE_PG_MAX_QUERY_FEATURES).
        //
        // `--mvt-min-feature-px` is NOT in this list: it is a selection rule, not an encoder pass,
        // and it now applies to the raster render too (it reaches this bake through the layer, via
        // `VectorLayerSpec::min_feature_px`). It is the flag that makes a low-zoom bake of a dense
        // layer finish at all.
        let mvt_only = [
            (
                "--mvt-max-features",
                args.mvt_max_features != crate::vector::mvt::DEFAULT_MAX_FEATURES_PER_TILE,
            ),
            ("--no-safety-limit", args.mvt_no_safety_limit),
            ("--no-optimizations", args.mvt_no_optimizations),
            ("--mvt-cell-px", args.mvt_cell_px != 0.0),
            ("--mvt-dissolve", args.mvt_dissolve.is_some()),
        ];
        for (flag, set) in mvt_only.iter().filter(|(_, set)| *set) {
            let _ = set;
            eprintln!("WARNING: {flag} is an MVT encoder flag and has no effect on a --tile-format png bake");
        }
        vector::pmtiles::raster::build_raster_pmtiles(
            &layer,
            &grid,
            args.min_zoom,
            args.max_zoom,
            bbox,
            std::path::Path::new(&args.out),
            &tmp,
        )?
    } else {
        vector::pmtiles::generate::build_pmtiles(
            &layer,
            &opts,
            &grid,
            args.min_zoom,
            args.max_zoom,
            bbox,
            std::path::Path::new(&args.out),
            &tmp,
        )?
    };
    println!(
        "build-pmtiles ({fmt}): {} -> addressed {} · entries {} · contents {} · {} bytes ({:.1}x dedup)",
        args.out,
        counts.addressed,
        counts.entries,
        counts.contents,
        counts.bytes,
        counts.addressed as f64 / counts.contents.max(1) as f64,
        fmt = if raster { "png" } else { "mvt" },
    );
    Ok(())
}
