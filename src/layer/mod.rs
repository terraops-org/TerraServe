// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Turning operator configuration into a published `server::Layer`.
//!
//! This is where a `--cog` / `--vector` path plus its style, CRS, expression, grids and MVT flags
//! become the immutable per-layer state the request path reads. It sits between the CLI verbs
//! (`cmd/`) and the engine: `cmd::serve` calls it once per layer at startup, and
//! `cmd::pmtiles` calls it to build the same layer offline for a bake — which is exactly why it
//! is not inside either of them.
//!
//! ⚠ **Known shape problem, deliberately left alone for now.** `build_vector_layer` takes
//! `&ServeArgs` — a 43-field struct describing an entire *server* — because it grew alongside the
//! single-layer flags. That is why it is one ~540-line function with eleven parameters, why the
//! offline baker has to fabricate a `ServeArgs` it does not otherwise need, and why the
//! `--config` path and the single-layer path construct layers differently (the missing per-layer
//! `mvt_style` under `--config` is a direct consequence). Narrowing this to a `VectorLayerSpec`
//! built from either source is the planned follow-up; see
//! `docs/superpowers/plans/2026-08-06-lib-rs-split.md`. Moving the code first keeps that change
//! reviewable on its own.

use crate::Error;
use crate::ServeArgs;
use crate::{
    assets, cache, cog, config, expr, mvt_http, render, reproj, s3, server, style, tms, vector,
};

/// Compile a band-math expression against band names in physical order.
pub(crate) fn build_band_math(
    expression: &str,
    band_names: &[String],
    nodata: Option<f64>,
) -> Result<render::BandMath, Error> {
    let names: Vec<&str> = band_names.iter().map(|s| s.as_str()).collect();
    let program = expr::Program::compile(expression, &names)?;
    Ok(render::BandMath {
        program,
        nodata: nodata.unwrap_or(f64::NAN),
    })
}

/// Parse a COG once, compute its advertised WGS84 bounds, resolve its tile grids, and assemble a
/// published layer. `grid_ids`/`tile_px`/`custom_grids` select the TMS/WMTS grids this layer serves.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_layer(
    name: String,
    cog_path: String,
    style: style::Style,
    src_crs: String,
    band_math: Option<render::BandMath>,
    s3: s3::S3Config,
    grid_ids: &[String],
    tile_px: u32,
    custom_grids: &std::collections::BTreeMap<String, config::GridConfig>,
    args: &ServeArgs,
) -> Result<server::Layer, Error> {
    if s3::is_s3_url(&cog_path) {
        println!("layer '{name}': S3 {cog_path}");
    }
    // Open the source ONCE and keep it — reused across requests (S3 connection pool persists).
    let source = std::sync::Arc::new(s3::AnySource::open(&cog_path, &s3)?);
    let cog = std::sync::Arc::new(cog::parse(source.as_ref())?);
    let tile_cache = cache::from_options(args.no_cache_lru, args.cache_lru);
    let index_cache = cache::new_index_cache(cache::index_cache_bytes());
    let lvl = &cog.levels[0];
    let g = lvl.geo;
    let (cw, ch) = (lvl.width as f64, lvl.height as f64);
    let bounds_wgs84 = reproj::wgs84_bounds(
        &src_crs,
        g.origin_x,
        g.origin_y - ch * g.py,
        g.origin_x + cw * g.px,
        g.origin_y,
    )
    .unwrap_or([-180.0, -90.0, 180.0, 90.0]);
    // Resolve the layer's tile grids (validates level-invariance / unknown ids) and check each
    // grid CRS is transformable to the layer's source CRS — fail loudly at STARTUP, not per tile.
    let tms_grids = config::resolve_grids(grid_ids, tile_px, &cog, &src_crs, custom_grids)
        .map_err(|e| format!("layer '{name}': {e}"))?;
    let mut grids = Vec::with_capacity(tms_grids.len());
    for mut tms in tms_grids {
        // WR2: `from_cog` grids differ per layer (native CRS/origin/pyramid) but share the id
        // "from_cog" — a collision once WMTS embeds shared TileMatrixSets in one Contents. Give each
        // a per-layer-unique id. Presets/custom grids keep their (correctly shared) id.
        if tms.id == "from_cog" {
            tms.id = format!("{name}_native");
        }
        reproj::Transformer::new(&tms.crs, &src_crs)
            .map_err(|e| format!("layer '{name}' grid '{}': {e}", tms.id))?;
        let data_bounds = tms::bounds_in_grid_crs(&cog, &src_crs, &tms.crs);
        grids.push(server::PublishedGrid { tms, data_bounds });
    }
    println!(
        "layer '{name}': {}  bounds W {:.4} S {:.4} E {:.4} N {:.4}  grids: {}",
        if band_math.is_some() {
            "band-math"
        } else {
            "rgb"
        },
        bounds_wgs84[0],
        bounds_wgs84[1],
        bounds_wgs84[2],
        bounds_wgs84[3],
        grids
            .iter()
            .map(|g| g.tms.id.as_str())
            .collect::<Vec<_>>()
            .join(","),
    );
    Ok(server::Layer {
        name,
        cog_path,
        cog: Some(cog),
        source: Some(source),
        style: Some(style),
        src_crs,
        band_math,
        bounds_wgs84,
        tile_cache,
        index_cache,
        grids,
        vector: None,
        pmtiles: std::collections::BTreeMap::new(),
        raster_pmtiles: std::collections::BTreeMap::new(),
        overlay: std::collections::BTreeMap::new(),
    })
}

/// What a vector layer actually serves, for its startup line. This used to be the hard-coded
/// string "(WMS GetMap only)", which stopped being true when `/mvt` landed and again when the tile
/// paths learned to raster vector layers. An operator reading it would not know to point QGIS at
/// WMTS — and telling someone a working endpoint does not exist is the same failure as advertising
/// one that does not work.
fn vector_serves_note(grids: &[server::PublishedGrid]) -> String {
    if grids.is_empty() {
        // No published grid, so no WMTS/TMS raster tiles; `/mvt` still serves via its preset grids.
        "(WMS GetMap · MVT)".to_string()
    } else {
        format!(
            "(WMS GetMap · MVT · WMTS/TMS tiles: {})",
            grids
                .iter()
                .map(|g| g.tms.id.as_str())
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}

/// Build a **vector** (label) layer: parse the source once (GeoJSON, or a native GeoPackage when
/// `geojson_path` ends in `.gpkg`), load the vec-style + font, derive bounds from the feature
/// extent. Served over WMS GetMap (always) plus, when `grid_ids` resolves any, tiled MVT/WMTS
/// (`grid_ids`/`tile_px`/`custom_grids` mirror `build_layer`'s raster grid-publishing params).
#[allow(clippy::too_many_arguments)]
/// Everything `build_vector_layer` needs to construct one vector layer, and nothing else.
///
/// It replaces an eleven-parameter signature whose ninth parameter was `&ServeArgs`: a 43-field
/// struct describing an entire *server*, of which layer construction reads seven fields. That
/// was not merely untidy. Having the global argument set in scope meant the body could answer a
/// PER-LAYER question by consulting a GLOBAL flag, and it did: `--config`'s per-layer `src_crs:`
/// was silently discarded because precedence was decided from `args.src_crs` (fixed in bc21155).
/// With only this struct in scope, that mistake is not expressible.
///
/// The split below is the useful one. **Identity and source** are per-layer: every layer has its
/// own path, style, CRS and grids. **Tuning** is global: one `--cache-lru`, one `--keep-fields`
/// for the whole server, applied to each layer alike. `from_serve_args` is the single place the
/// two meet.
pub(crate) struct VectorLayerSpec {
    // ---- identity + source (per layer) ----
    pub name: String,
    pub vector_path: String,
    pub vec_style_path: String,
    pub font_path: String,
    /// The source CRS **as declared**; `None` = not declared, so adopt the file header's.
    /// Keeping declared-ness in the value is what the bug fix turned on.
    pub declared_crs: Option<String>,
    /// Layer bounds in the source CRS, `[minx, miny, maxx, maxy]`. REQUIRED for a `postgis://`
    /// layer (there is no file header / R-tree to derive it from, and TerraServe deliberately
    /// never queries the database for it — see `build_vector_layer`'s `SourceKind::PostGis` arm).
    /// `None` for every other source: their extent comes from the file itself.
    pub extent: Option<[f64; 4]>,
    /// Extra attribute columns to fetch, on top of the ones the server-side style references.
    /// Only a `postgis://` layer reads it — a file source already has every field in hand, but a
    /// database query must name its columns up front. The escape hatch for anything
    /// `Style::referenced_fields` cannot see, above all a client-side `--mvt-style`, which is
    /// opaque pass-through JSON. Every name here is validated against the table at startup.
    pub columns: Vec<String>,
    pub grid_ids: Vec<String>,
    pub tile_px: u32,
    pub custom_grids: std::collections::BTreeMap<String, config::GridConfig>,
    pub pmtiles_paths: Vec<String>,
    /// Pre-baked **PNG** archives for the WMTS/TMS raster path (`serve --raster-pmtiles`, or a
    /// layer's `raster_pmtiles:`). Kept separate from `pmtiles_paths` on purpose: the two feed
    /// different front-ends and different payload formats, and an archive handed to the wrong one
    /// is refused at startup rather than serving bytes of the wrong type behind a 200.
    pub raster_pmtiles_paths: Vec<String>,

    // ---- tuning (global: one flag, every layer) ----
    //
    // NOTE what is absent: `cache_lru` / `no_cache_lru`. The narrowing surfaced the already-known
    // gap as a compiler warning -- vector layer construction never reads them, because the LRU
    // tile cache is wired on the COG path only and every vector path hardcodes `tile_cache: None`.
    // `--cache-lru` is a documented no-op for vector layers. Leaving the fields out keeps the spec
    // honest about that rather than implying a knob that does nothing.
    pub keep_fields: Option<String>,
    /// The minimum on-screen size (display-px²) a POLYGON must cover to be RENDERED at a given
    /// resolution: `--raster-min-feature-px` when the operator set it, else `--mvt-min-feature-px`.
    /// Lives on the spec — and so on the built `VectorLayer` — rather than being read off
    /// `ServeState` at request time, because the raster paths that apply it do not all see a
    /// `ServeState`: `wms::handle_layers` takes a bare `&[Layer]`, and the `build-pmtiles` bake has
    /// no server at all. `0.0` = gate off (the default).
    ///
    /// Separate from the MVT knob because the two want different numbers for different reasons —
    /// see `--raster-min-feature-px`'s help. The fallback keeps a single-flag setup behaving
    /// exactly as before the override existed.
    pub min_feature_px: f64,
    pub snap_tolerance: f64,
    pub topology_simplify: Option<f64>,
    pub topology_dissolve: Option<String>,
    pub topology_dissolve_rollup: Option<usize>,
    /// Resolved `--max-inflight` (never the raw "0 means default" value `ServeArgs` carries) —
    /// read only by the `postgis://` arm, to warn at startup when the connection pool is smaller
    /// (`vector::postgis::pool_sizing_warning`). Every other vector path ignores it, same shape as
    /// the `cache_lru` gap noted above.
    pub max_inflight: usize,
}

/// `--max-inflight`'s effective value: `0` means "use the built-in default" (2x logical cores).
/// Pulled out so `run_serve`'s admission-control setup and a PostGIS layer's pool-sizing warning
/// (`VectorLayerSpec.max_inflight`) read the exact same number rather than two formulas that
/// could drift apart.
pub(crate) fn resolve_max_inflight(raw: usize) -> usize {
    if raw == 0 {
        2 * std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8)
    } else {
        raw
    }
}

impl VectorLayerSpec {
    /// Per-layer identity explicitly; global tuning lifted from `ServeArgs`. Grids and PMTiles
    /// archives default to empty and are added with the builders below, because two of the three
    /// call sites do not set them.
    pub(crate) fn from_serve_args(
        args: &ServeArgs,
        name: String,
        vector_path: String,
        vec_style_path: String,
        font_path: String,
        declared_crs: Option<String>,
    ) -> Self {
        Self {
            name,
            vector_path,
            vec_style_path,
            font_path,
            declared_crs,
            extent: None,
            columns: Vec::new(),
            grid_ids: Vec::new(),
            tile_px: args.tms_tile_px,
            custom_grids: std::collections::BTreeMap::new(),
            pmtiles_paths: Vec::new(),
            raster_pmtiles_paths: Vec::new(),
            keep_fields: args.keep_fields.clone(),
            min_feature_px: args
                .raster_min_feature_px
                .unwrap_or(args.mvt_min_feature_px),
            snap_tolerance: args.snap_tolerance,
            max_inflight: resolve_max_inflight(args.max_inflight),
            topology_simplify: args.topology_simplify,
            topology_dissolve: args.topology_dissolve.clone(),
            topology_dissolve_rollup: args.topology_dissolve_rollup,
        }
    }

    pub(crate) fn with_grids(
        mut self,
        grid_ids: Vec<String>,
        tile_px: u32,
        custom_grids: std::collections::BTreeMap<String, config::GridConfig>,
    ) -> Self {
        self.grid_ids = grid_ids;
        self.tile_px = tile_px;
        self.custom_grids = custom_grids;
        self
    }

    pub(crate) fn with_pmtiles(mut self, pmtiles_paths: Vec<String>) -> Self {
        self.pmtiles_paths = pmtiles_paths;
        self
    }

    /// Pre-baked PNG archives for the WMTS/TMS raster path — the raster twin of `with_pmtiles`.
    pub(crate) fn with_raster_pmtiles(mut self, raster_pmtiles_paths: Vec<String>) -> Self {
        self.raster_pmtiles_paths = raster_pmtiles_paths;
        self
    }

    /// Populated from `LayerConfig.extent` on the `--config` path; left `None` (the
    /// `from_serve_args` default) on the single-`--vector` path, which has no `--extent` flag.
    pub(crate) fn with_extent(mut self, extent: Option<[f64; 4]>) -> Self {
        self.extent = extent;
        self
    }

    /// Populated from `LayerConfig.columns` on the `--config` path; left empty on the
    /// single-`--vector` path, which has no equivalent flag (a single-layer `postgis://` server
    /// styled by `--mvt-style` should use `--config`, where the columns can be named per layer).
    pub(crate) fn with_columns(mut self, columns: Vec<String>) -> Self {
        self.columns = columns;
        self
    }
}

pub(crate) fn build_vector_layer(
    spec: &VectorLayerSpec,
    s3: &s3::S3Config,
) -> Result<server::Layer, Error> {
    // Bind the spec's fields to the names the body already uses. Deliberately by reference and
    // deliberately here, at the top: it keeps this a pure narrowing of the signature rather than
    // a 540-line rewrite, so the diff stays reviewable and the behaviour provably unchanged.
    let VectorLayerSpec {
        name,
        vector_path: geojson_path,
        vec_style_path,
        font_path,
        declared_crs,
        grid_ids,
        tile_px,
        custom_grids,
        pmtiles_paths,
        min_feature_px,
        ..
    } = spec;
    let (name, tile_px, min_feature_px) = (name.clone(), *tile_px, *min_feature_px);
    let (geojson_path, vec_style_path, font_path) = (
        geojson_path.as_str(),
        vec_style_path.as_str(),
        font_path.as_str(),
    );
    // The CRS to assume when nothing better is available. `declared_crs` stays an Option so
    // the arms below can still ask "did the operator actually say?" -- that question is what
    // the old code had to answer by peeking at the global --src-crs flag.
    let src_crs: String = declared_crs.clone().unwrap_or_else(config::default_src_crs);
    use vector::source::FeatureSource;
    // Resolve the layer's tile grids ONCE, up front — a vector layer has no COG, so `from_cog`
    // (raster's native-pyramid grid) is meaningless here; `resolve_grids_presets` is the no-COG
    // variant of `build_layer`'s `resolve_grids` and correctly rejects a `from_cog` id if it
    // reaches it. But `grid_ids` here is usually `LayerConfig.grids`/`ServeArgs.tms_grids`, whose
    // shared default (`config::default_grids()` = `["from_cog"]`) is written for the raster `cog:`
    // branch and reaches every `vector:` layer that doesn't override `grids:` too — unfiltered,
    // that would hard-error at startup on `from_cog` for every pre-existing vector config (e.g.
    // `fixtures/cite/wms13-layers.yaml`, `fixtures/fgb/multi.yaml`), none of which set `grids:`
    // explicitly. `from_cog` can never apply to a vector layer (there's no COG, ever — not just by
    // default), so drop it here unconditionally rather than erroring; any OTHER unknown/invalid id
    // still fails loudly below, unchanged.
    let grid_ids: Vec<String> = grid_ids
        .iter()
        .filter(|id| *id != "from_cog")
        .cloned()
        .collect();
    // Unlike the raster path, this doesn't depend on the layer's (possibly auto-detected-below)
    // `src_crs`, so it can run before the format-specific branching and be moved into whichever of
    // the 3 return sites below actually executes. `data_bounds` stays `None` (no COG geo to
    // reproject) — the TMS `<BoundingBox>` / empty-tile early-out for vector grids is a later task,
    // per the brief.
    let grids: Vec<server::PublishedGrid> =
        config::resolve_grids_presets(&grid_ids, tile_px, custom_grids)
            .map_err(|e| format!("layer '{name}': {e}"))?
            .into_iter()
            .map(|tms| server::PublishedGrid {
                tms,
                data_bounds: None,
            })
            .collect();
    // Open every `--pmtiles` archive and file it under the grid it self-describes (design commitment
    // 2: an archive's metadata carries its own `grid_id`; serve auto-maps grid -> archive, no
    // `grid=path` flag grammar). Independent of the format-specific branching below, so resolved once
    // up front for all 3 return sites. Two archives naming the SAME grid is a startup config error
    // (design commitment 1: one archive per grid — silently picking one would hide a real mistake).
    let mut pmtiles: std::collections::BTreeMap<
        String,
        std::sync::Arc<vector::pmtiles::read::PmtilesReader>,
    > = std::collections::BTreeMap::new();
    let mut pmtiles_path_by_grid: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    for p in pmtiles_paths {
        let reader = vector::pmtiles::read::PmtilesReader::open(std::path::Path::new(p))?;
        // What the archive HOLDS, before anything asks what grid it is on: `--pmtiles` feeds the
        // MVT route, so a PNG archive here would ship image bytes to a client expecting a vector
        // tile, behind a 200. Refuse it by name (see `require_tile_type`).
        reader.require_tile_type(vector::pmtiles::write::TILE_TYPE_MVT, p)?;
        let gid = reader.grid_id();
        if let Some(first_path) = pmtiles_path_by_grid.get(&gid) {
            return Err(format!(
                "two --pmtiles archives both target grid '{gid}': {first_path} and {p}"
            )
            .into());
        }
        pmtiles_path_by_grid.insert(gid.clone(), p.clone());
        pmtiles.insert(gid, std::sync::Arc::new(reader));
    }
    // The raster (PNG) twin of the loop above, feeding the WMTS/TMS tile paths instead of `/mvt`.
    // Three things are checked here rather than at request time, because a precomputed pyramid that
    // silently never gets read is indistinguishable from one that is working:
    //   1. the archive must DECLARE PNG (`tile_type` 2) — an MVT archive on this path would send
    //      protobuf bytes to a client that asked for image/png;
    //   2. its `grid_id` must match a grid this layer PUBLISHES (same asymmetric match the tile
    //      routes use: exact id, or the stored id's `_{px}` suffix stripped), else nothing would
    //      ever look it up;
    //   3. the baked `tile_px` must equal that grid's `tile_w` — a 256-px archive served on a
    //      512-px grid hands the client a quarter-size image for the ground the tile covers.
    // The map is keyed by the PUBLISHED grid's id, so `raster_pmtiles.get(&pg.tms.id)` at request
    // time is a direct hit with no re-derivation.
    let mut raster_pmtiles: std::collections::BTreeMap<
        String,
        std::sync::Arc<vector::pmtiles::read::PmtilesReader>,
    > = std::collections::BTreeMap::new();
    let mut raster_path_by_grid: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    for p in &spec.raster_pmtiles_paths {
        let reader = vector::pmtiles::read::PmtilesReader::open(std::path::Path::new(p))?;
        reader.require_tile_type(vector::pmtiles::write::TILE_TYPE_PNG, p)?;
        let gid = reader.grid_id();
        let pg = grids
            .iter()
            .find(|g| g.tms.id == gid || tms::strip_size_suffix(&g.tms.id) == gid)
            .ok_or_else(|| {
                let published: Vec<&str> = grids.iter().map(|g| g.tms.id.as_str()).collect();
                format!(
                    "layer '{name}': raster pmtiles archive {p} was baked on grid '{gid}', which \
                     this layer does not publish (grids: {published:?}) — nothing would ever read \
                     it. Add `grids: [{gid}]` to the layer, or re-bake with `--grid <published>`."
                )
            })?;
        if let Some(px) = reader.raster_tile_px() {
            if px != pg.tms.tile_w {
                return Err(format!(
                    "layer '{name}': raster pmtiles archive {p} was baked at {px} px tiles but \
                     grid '{}' serves {} px tiles — the stored image does not cover the tile it \
                     would answer. Re-bake with `build-pmtiles --tile-format png --tile-px {}`.",
                    pg.tms.id, pg.tms.tile_w, pg.tms.tile_w
                )
                .into());
            }
        } else {
            eprintln!(
                "WARNING: raster pmtiles archive {p} carries no `tile_px` in its metadata; \
                 assuming it matches grid '{}' ({} px)",
                pg.tms.id, pg.tms.tile_w
            );
        }
        let key = pg.tms.id.clone();
        if let Some(first_path) = raster_path_by_grid.get(&key) {
            return Err(format!(
                "two --raster-pmtiles archives both target grid '{key}': {first_path} and {p}"
            )
            .into());
        }
        raster_path_by_grid.insert(key.clone(), p.clone());
        raster_pmtiles.insert(key, std::sync::Arc::new(reader));
    }
    // Flag-consistency: --topology-dissolve-rollup only means something when --topology-dissolve is
    // also set (it rolls up the dissolve field's class codes). Warn on the silent no-op rather than
    // ignoring it, matching the --mvt-cell-field / validate_cell_flags pattern.
    if spec.topology_dissolve_rollup.is_some() && spec.topology_dissolve.is_none() {
        eprintln!(
            "WARNING: --topology-dissolve-rollup has no effect without --topology-dissolve; ignored"
        );
    }
    // Extension sniff: `.gpkg` selects the native GeoPackage reader, everything else the GeoJSON
    // path. CRS precedence: an explicit `--src-crs` always wins; only when the operator did NOT
    // pass one do we adopt the gpkg's own detected CRS (resolved via `gpkg_spatial_ref_sys`).
    // Carries (topo, finest min_area, snap) out of the `.gpkg` topology-simplify arm so the per-zoom
    // LOD pools are built once `area_scale` is known (below).
    let mut lod_inputs: Option<(
        vector::topology::Topology,
        f64,
        f64,
        std::sync::Arc<Vec<vector::topology::ArcLine>>,
    )> = None;
    // Fail fast on a bad --vec-style/--font BEFORE the (potentially minutes-long) source/topology
    // build below: a typo'd style or font path used to only error out after loading the gpkg and
    // building topology, wasting real wall-clock time on a real build (~2.7 min on a COS-sized
    // coverage) for a mistake that's knowable in milliseconds.
    // Reject an unknown source SCHEME before anything else. Without this an unrecognised
    // scheme falls through the format branches to the GeoPackage opener, which then fails deep
    // inside SQLite complaining about a file that was never a file -- an error naming the wrong
    // format entirely.
    //
    // NOT echoing `geojson_path`: any `scheme://user:pass@host/...`-shaped spec can carry a real,
    // literal, un-`${VAR}`'d password in its authority. `postgres://`/`postgresql://` for a
    // `postgis://` layer is exactly the mistake `pg_uri.rs`'s module doc calls out -- and a typo
    // like that lands HERE, never reaching `parse_postgis_uri`'s own protection, because
    // `postgres`/`postgresql` do not classify as `SourceKind::PostGis`. The scheme name alone is
    // enough to spot the typo without risking the rest of it.
    if let vector::uri::SourceKind::Unsupported(scheme) = vector::uri::classify(geojson_path) {
        return Err(format!(
            "layer '{name}': unsupported vector source scheme `{scheme}://`. Supported: a local \
             path or `s3://` pointing at .fgb, .gpkg or .geojson, or a `postgis://` connection URI."
        )
        .into());
    }
    let style = vector::style::Style::parse(
        vec_style_path,
        &assets::read_config_string(vec_style_path, s3)?,
    )?;
    let font = assets::read_config_bytes(font_path, s3).map_err(|e| format!("font {e}"))?;
    let shaper = std::sync::Arc::new(vector::shape::Shaper::from_font_bytes(&font)?);
    // Say it ONCE, at startup, for a style that asks for labels: the tile front-ends render this
    // layer's geometry without them (docs/postgis-layers.md, "Labels are WMS-only" — per-tile
    // placement would clip a label at the seam or draw it twice). Without this an operator adds
    // the WMTS endpoint in QGIS, sees no labels, and has nothing to go on.
    if style
        .all_rules()
        .flat_map(|r| &r.symbolizers)
        .any(|s| matches!(s, vector::style::Symbolizer::Text(_)))
    {
        eprintln!(
            "WARNING: layer '{name}': style has text labels; labels render on WMS GetMap only — \
             WMTS/TMS raster tiles and MVT carry geometry (per-tile placement clips or duplicates \
             labels at tile seams). Overlay the WMS layer, or label client-side from /mvt."
        );
    }

    // PostGIS: windowed by definition (every query is a bbox filter against a GiST index), so
    // this mirrors the `.fgb`/windowed-`.gpkg` arms below rather than the load-all path -- none
    // of the in-RAM-only transforms (topology-simplify/-dissolve, keep-fields, LOD) apply to a
    // source that never holds the whole table in memory. The one real difference: `extent:` is
    // REQUIRED here and never derived from the database (see `PostgisSource::open`'s doc comment
    // for why -- `ST_EstimatedExtent` is NULL pre-`ANALYZE`, `ST_Extent` is a full-table scan).
    if vector::uri::classify(geojson_path) == vector::uri::SourceKind::PostGis {
        if spec.topology_simplify.is_some()
            || spec.topology_dissolve.is_some()
            || spec.keep_fields.is_some()
        {
            eprintln!(
                "WARNING: --topology-simplify/--topology-dissolve/--keep-fields apply only to \
                 .gpkg vector sources; ignored for layer '{name}' (postgis://)"
            );
        }
        // A missing extent is a startup ERROR naming the layer, not a warning and not a silent
        // default -- an operator who omits it needs to know before the server ever accepts a
        // request, not discover it from a layer with the wrong advertised bounds.
        let extent = spec.extent.ok_or_else(|| {
            format!(
                "layer '{name}': a postgis:// layer needs an explicit `extent:` \
                 [minx, miny, maxx, maxy] in the source CRS. TerraServe never queries the \
                 database for it: ST_EstimatedExtent is NULL on a never-ANALYZEd table and \
                 ST_Extent is a full-table scan."
            )
        })?;
        // Deliberately NOT wrapped with `geojson_path` the way the `.fgb`/`.gpkg` arms below wrap
        // their open() errors with the file path: `geojson_path` here IS the connection URI, and
        // `parse_postgis_uri` already goes out of its way to never echo it back on a parse
        // failure (an operator who pastes a literal, un-`${VAR}`'d password must not have it
        // round-trip into a caller's error string). `PostgisSource::open`'s own errors already
        // name the schema/table for context.
        let mut src = vector::postgis::PostgisSource::open(geojson_path, extent)?;
        // The attribute columns to fetch: exactly the fields THIS style actually reads (filters +
        // labels), not the whole row -- there is no "whole row" cheap to ask for over a network
        // query the way there is for a file already open on disk. Must run after `open()` (its
        // two-argument shape is frozen -- Task 8's live tests call it directly) and after `style`
        // is parsed (above this whole `postgis://` arm), which is why this is a follow-up call
        // rather than a constructor parameter. Leaving this unset is not a smaller version of a
        // working layer, it is a BROKEN one: every filter silently evaluates false (`IsNull`
        // vacuously true), every label renders blank, GetFeatureInfo reports `{}` -- see
        // `Style::referenced_fields`'s doc comment.
        //
        // `columns:` from the layer config is UNIONed in. `referenced_fields` can only see what
        // the SERVER-side style reads; a client-side `--mvt-style` is opaque pass-through JSON
        // that is never parsed into a `Style`, so its `["get", FIELD]` expressions contribute
        // nothing here and an MVT layer would ship tiles with no class attribute at all. Rather
        // than reach into another style dialect, `columns:` lets the operator name them.
        let mut columns = style.referenced_fields();
        columns.extend(spec.columns.iter().cloned());
        src.set_columns(columns.into_iter().collect());
        // Prove the statement can actually RUN, before the server accepts a single request. A
        // column the relation does not have (a typo, or a case mismatch -- identifiers are quoted,
        // so `Name` is not `name`) makes every query fail with Postgres 42703, and `query()`'s only
        // way to report that is an empty feature list, i.e. a blank map behind a 200 OK forever.
        // See `PostgisSource::validate_columns` for why this fails loudly instead of dropping the
        // unknown column the way the `.gpkg`/`.fgb` readers do.
        src.validate_columns()
            .map_err(|e| format!("layer '{name}': {e}"))?;
        // Operational guard (design §7): if the pool is smaller than `--max-inflight`, the pool
        // silently BECOMES the real admission-control limit and the operator's `--max-inflight`
        // reasoning is quietly wrong. This is the one place both numbers are known at once.
        if let Some(w) =
            vector::postgis::pool_sizing_warning(src.pool_max_size(), spec.max_inflight)
        {
            eprintln!("{w}");
        }
        // CRS. This arm deliberately does NOT follow the `.fgb`/`.gpkg` precedence rule ("an
        // explicit declaration always wins over the source's own"), because PostGIS is the one
        // source whose CRS is authoritative rather than advisory: the SRID is what the geometry is
        // actually stored in, and TerraServe never transforms geometry in SQL. A declaration that
        // disagrees therefore cannot reproject anything -- it can only make the bbox filter test a
        // box in one CRS against geometry in another, which matches nothing and serves a blank map
        // with no error. So a disagreement is a startup error naming both, and an agreeing
        // declaration is simply redundant. (An operator who believes the registered SRID is wrong
        // fixes it at the source with `?srid=` on the URI, which moves BOTH numbers together.)
        let resolved_crs = match vector::source::WindowedSource::crs(&src) {
            // `open()` requires a resolved positive SRID, so this is always the arm taken.
            Some(table_crs) => {
                if let Some(declared) = declared_crs {
                    if let Some(e) = vector::postgis::crs_mismatch_error(
                        declared,
                        table_crs,
                        &src.qualified_name(),
                    ) {
                        return Err(format!("layer '{name}': {e}").into());
                    }
                }
                table_crs.to_string()
            }
            // Unreachable today; kept to match the shared `WindowedSource` contract the other two
            // arms rely on, so a future source change cannot silently skip the fallback.
            None => {
                eprintln!(
                    "WARNING: layer '{name}': could not determine the table's SRID; \
                     assuming {src_crs}. If the data is in a different CRS the map will be \
                     misplaced — pass --src-crs EPSG:XXXX."
                );
                src_crs
            }
        };
        let source = vector::source::VectorSource::Windowed(std::sync::Arc::new(src));
        let ext = source.full_extent();
        let bounds_wgs84 = if resolved_crs == "EPSG:4326" || resolved_crs == "CRS:84" {
            ext
        } else {
            reproj::wgs84_bounds(&resolved_crs, ext[0], ext[1], ext[2], ext[3])
                .unwrap_or([-180.0, -90.0, 180.0, 90.0])
        };
        let paths = vector_serves_note(&grids);
        println!(
            "layer '{name}': vector (postgis)  bounds W {:.4} S {:.4} E {:.4} N {:.4}  {paths}",
            bounds_wgs84[0], bounds_wgs84[1], bounds_wgs84[2], bounds_wgs84[3],
        );
        let area_scale = crate::vector::mvt::layer_area_scale(bounds_wgs84, ext);
        // NOT a query: `source` is `VectorSource::Windowed`, so `feature_field_schema_vs`
        // dispatches to `WindowedSource::field_schema`, which for PostGIS reports exactly the
        // `columns` set above (the style's referenced fields plus any config `columns:`) -- and
        // those have just been proven to exist by `validate_columns`. So `fields` is the truthful
        // per-layer attribute list every downstream field check reads (`--mvt-cell-field`,
        // `--mvt-dissolve`, the `--mvt-style` warning in `cmd::serve`).
        let fields = mvt_http::feature_field_schema_vs(&source);
        return Ok(server::Layer {
            name,
            cog_path: String::new(),
            cog: None,
            source: None,
            style: None,
            src_crs: resolved_crs,
            band_math: None,
            bounds_wgs84,
            tile_cache: None,
            index_cache: cache::new_index_cache(cache::index_cache_bytes()),
            grids,
            vector: Some(server::VectorLayer {
                fields,
                area_scale,
                min_feature_px,
                source,
                style,
                shaper,
                lod: None,
            }),
            pmtiles: pmtiles.clone(),
            raster_pmtiles: raster_pmtiles.clone(),
            overlay: std::collections::BTreeMap::new(),
        });
    }

    // FlatGeoBuf: the windowed-seam reader (FGB batch Task 5) — an early return, since none of
    // the GPKG-only knobs below (topology-simplify/-dissolve, keep-fields, in-RAM LOD) apply to
    // a windowed source that never holds the whole coverage in memory. Extension sniff only —
    // `config::LayerConfig` gains no new field. Opens via `s3::AnySource` (local or `s3://`) —
    // every byte already flows through `RangeSource`, so this reader was S3-capable by
    // construction; the local-only restriction was just the opener.
    if matches!(
        vector::uri::classify(geojson_path),
        vector::uri::SourceKind::FlatGeoBuf
    ) {
        if spec.topology_simplify.is_some()
            || spec.topology_dissolve.is_some()
            || spec.keep_fields.is_some()
        {
            eprintln!(
                "WARNING: --topology-simplify/--topology-dissolve/--keep-fields apply only to .gpkg vector sources; ignored for {geojson_path}"
            );
        }
        if s3::is_s3_url(geojson_path) {
            println!("layer '{name}': S3 {geojson_path}");
        }
        let range_src = s3::AnySource::open(geojson_path, s3)
            .map_err(|e| format!("open {geojson_path}: {e}"))?;
        let fgb = vector::fgb::FgbSource::open(range_src)
            .map_err(|e| format!("fgb {geojson_path}: {e}"))?;
        // CRS precedence mirrors the `.gpkg` arm below: an explicit `--src-crs` always wins; only
        // when the operator did NOT pass one do we adopt the file's own header CRS.
        let resolved_crs = if declared_crs.is_none() {
            match fgb.crs() {
                Some(c) => c.to_string(),
                None => {
                    eprintln!(
                        "WARNING: {geojson_path}: could not auto-detect an EPSG CRS from the \
                         FlatGeoBuf header; assuming {src_crs}. If the data is in a different CRS \
                         the map will be misplaced — pass --src-crs EPSG:XXXX."
                    );
                    src_crs
                }
            }
        } else {
            src_crs
        };
        // `fgb.full_extent()` (the inherent method) rather than going through `source` here —
        // `source` isn't built yet, since the resident-index warning below still needs `fgb` by
        // value (it's moved into `source` right after).
        let ext = fgb.full_extent();
        let bounds_wgs84 = if resolved_crs == "EPSG:4326" || resolved_crs == "CRS:84" {
            ext
        } else {
            reproj::wgs84_bounds(&resolved_crs, ext[0], ext[1], ext[2], ext[3])
                .unwrap_or([-180.0, -90.0, 180.0, 90.0])
        };
        if fgb.index_is_resident() && fgb.index_size() > 16 * 1024 * 1024 {
            let idx_mib = fgb.index_size() as f64 / (1024.0 * 1024.0);
            match std::fs::metadata(geojson_path).ok().map(|m| m.len()) {
                Some(total) if total > 0 => eprintln!(
                    "WARNING: layer '{name}': FlatGeoBuf R-tree index resident in RAM: {idx_mib:.1} MiB \
                     (~{:.0}% of the .fgb). Set TERRASERVE_FGB_INDEX_RESIDENT=0 to read it windowed \
                     (bounded RAM, more S3 round-trips).",
                    100.0 * fgb.index_size() as f64 / total as f64
                ),
                _ => eprintln!(
                    "WARNING: layer '{name}': FlatGeoBuf R-tree index resident in RAM: {idx_mib:.1} MiB. \
                     Set TERRASERVE_FGB_INDEX_RESIDENT=0 to read it windowed (bounded RAM, more round-trips)."
                ),
            }
        }
        let source = vector::source::VectorSource::Windowed(std::sync::Arc::new(fgb));
        let paths = vector_serves_note(&grids);
        println!(
            "layer '{name}': vector (windowed .fgb)  bounds W {:.4} S {:.4} E {:.4} N {:.4}  {paths}",
            bounds_wgs84[0],
            bounds_wgs84[1],
            bounds_wgs84[2],
            bounds_wgs84[3],
        );
        let area_scale = crate::vector::mvt::layer_area_scale(bounds_wgs84, ext);
        // Header-driven, NOT a whole-window feature scan: `source` is `VectorSource::Windowed`,
        // so `feature_field_schema_vs` dispatches to `WindowedSource::field_schema`
        // (`FgbSource`'s Header `columns()`, already parsed at `open()`) — no `query`/decode of
        // the file's features. This used to run `features_in(full_extent())` here, decoding
        // every feature just to list field names/types (~5.8 GB at a 6.1M-feature `.fgb`'s
        // scale) — see `WindowedSource::field_schema`'s doc comment for the fix.
        let fields = mvt_http::feature_field_schema_vs(&source);
        return Ok(server::Layer {
            name,
            cog_path: String::new(),
            cog: None,
            source: None,
            style: None,
            src_crs: resolved_crs,
            band_math: None,
            bounds_wgs84,
            tile_cache: None,
            index_cache: cache::new_index_cache(cache::index_cache_bytes()),
            grids,
            vector: Some(server::VectorLayer {
                fields,
                area_scale,
                min_feature_px,
                source,
                style,
                shaper,
                lod: None,
            }),
            pmtiles: pmtiles.clone(),
            raster_pmtiles: raster_pmtiles.clone(),
            overlay: std::collections::BTreeMap::new(),
        });
    }

    // Windowed GeoPackage: the same seam the `.fgb` branch above uses — a plain raw-serve `.gpkg`
    // that carries its own OGC R-tree (`rtree_<table>_<geom>`) is read windowed (just the request
    // bbox per request) instead of loaded whole into RAM at startup. None of the three
    // load-all-only transforms below (`--topology-simplify`/`--topology-dissolve`/`--keep-fields`)
    // can run on a windowed source (they need the whole feature set materialized), so this
    // early-return only fires when none of them are requested AND the file actually has a usable
    // rtree (`gpkg_has_rtree` — a cheap sqlite_master probe, no feature read); otherwise this
    // falls through, UNCHANGED, to the load-all `.gpkg` arm below.
    let windowed_gpkg = matches!(
        vector::uri::classify(geojson_path),
        vector::uri::SourceKind::GeoPackage
    ) && spec.topology_simplify.is_none()
        && spec.topology_dissolve.is_none()
        && spec.keep_fields.is_none()
        && vector::gpkg::gpkg_has_rtree(geojson_path, None);
    if windowed_gpkg {
        let gpkg = vector::gpkg::GpkgWindowedSource::open(geojson_path, None)
            .map_err(|e| format!("gpkg {geojson_path}: {e}"))?;
        // CRS precedence mirrors the load-all `.gpkg` arm below: an explicit `--src-crs` always
        // wins; only when the operator did NOT pass one do we adopt the gpkg's own detected CRS.
        let resolved_crs = if declared_crs.is_none() {
            match vector::source::WindowedSource::crs(&gpkg) {
                Some(c) => c.to_string(),
                None => {
                    eprintln!(
                        "WARNING: {geojson_path}: could not auto-detect an EPSG CRS from the \
                         GeoPackage; assuming {src_crs}. If the data is in a different CRS the \
                         map will be misplaced — pass --src-crs EPSG:XXXX."
                    );
                    src_crs
                }
            }
        } else {
            src_crs
        };
        let source = vector::source::VectorSource::Windowed(std::sync::Arc::new(gpkg));
        let ext = source.full_extent();
        let bounds_wgs84 = if resolved_crs == "EPSG:4326" || resolved_crs == "CRS:84" {
            ext
        } else {
            reproj::wgs84_bounds(&resolved_crs, ext[0], ext[1], ext[2], ext[3])
                .unwrap_or([-180.0, -90.0, 180.0, 90.0])
        };
        let paths = vector_serves_note(&grids);
        println!(
            "layer '{name}': vector (windowed .gpkg)  bounds W {:.4} S {:.4} E {:.4} N {:.4}  {paths}",
            bounds_wgs84[0],
            bounds_wgs84[1],
            bounds_wgs84[2],
            bounds_wgs84[3],
        );
        let area_scale = crate::vector::mvt::layer_area_scale(bounds_wgs84, ext);
        // Header-driven, NOT a whole-window feature scan: `source` is `VectorSource::Windowed`,
        // so `feature_field_schema_vs` dispatches to `WindowedSource::field_schema`
        // (`GpkgWindowedSource`'s `PRAGMA table_info` schema, already read at `open()`) — no
        // `query`/decode of the file's features.
        let fields = mvt_http::feature_field_schema_vs(&source);
        return Ok(server::Layer {
            name,
            cog_path: String::new(),
            cog: None,
            source: None,
            style: None,
            src_crs: resolved_crs,
            band_math: None,
            bounds_wgs84,
            tile_cache: None,
            index_cache: cache::new_index_cache(cache::index_cache_bytes()),
            grids,
            vector: Some(server::VectorLayer {
                fields,
                area_scale,
                min_feature_px,
                source,
                style,
                shaper,
                lod: None,
            }),
            pmtiles: pmtiles.clone(),
            raster_pmtiles: raster_pmtiles.clone(),
            overlay: std::collections::BTreeMap::new(),
        });
    }

    let (src, src_crs): (std::sync::Arc<dyn FeatureSource>, String) = if matches!(
        vector::uri::classify(geojson_path),
        vector::uri::SourceKind::GeoPackage
    ) {
        let g = vector::gpkg::GpkgSource::load(geojson_path, None)?;
        let crs = if declared_crs.is_none() {
            match g.crs() {
                Some(c) => c.to_string(),
                // The gpkg's SRS didn't resolve to an EPSG code (a non-EPSG organization CRS, or
                // missing srs metadata). We fall back to the default, but LOUDLY — a silent wrong
                // CRS renders every feature in the ocean. Pass `--src-crs` to be explicit.
                None => {
                    eprintln!(
                        "WARNING: {geojson_path}: could not auto-detect an EPSG CRS from the \
                             GeoPackage; assuming {src_crs}. If the data is in a different CRS the \
                             map will be misplaced — pass --src-crs EPSG:XXXX."
                    );
                    src_crs
                }
            }
        } else {
            src_crs
        };
        // Column-pruning: keep only the named attribute fields (+ the dissolve field) → smaller tiles
        // and lower memory. `gsrc` replaces `g` for dissolve/topology/raw serve below.
        let gsrc: std::sync::Arc<dyn FeatureSource> = match &spec.keep_fields {
            Some(csv) => {
                let mut keep: std::collections::HashSet<String> = csv
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if let Some(f) = &spec.topology_dissolve {
                    keep.insert(f.clone());
                }
                // Reads through the `VectorSource` seam (windowed-seam refactor): `g` isn't needed
                // again in this arm (the `None` arm below is the one that keeps it), so it's moved
                // into the wrapper — a whole-file read, hence `full_extent()` (LoadAll ignores it
                // anyway; `Windowed` doesn't exist yet, so this is behavior-identical).
                let g_vs = vector::source::VectorSource::LoadAll(std::sync::Arc::new(g));
                let pruned: Vec<vector::feature::Feature> = g_vs
                    .features_in(g_vs.full_extent())?
                    .as_slice()
                    .iter()
                    .map(|f| {
                        let mut props = vector::feature::Props::new();
                        for (k, v) in f.props.iter() {
                            if keep.contains(k) {
                                props.insert(k.clone(), v.clone());
                            }
                        }
                        vector::feature::Feature {
                            geom: f.geom.clone(),
                            props,
                            fid: f.fid,
                            bbox: f.bbox,
                            area: f.area,
                        }
                    })
                    .collect();
                eprintln!("keep-fields: pruned attributes to {} field(s)", keep.len());
                std::sync::Arc::new(vector::topology::materialize::TopologyFeatureSource::new(
                    pruned,
                ))
            }
            None => std::sync::Arc::new(g),
        };
        // Reads through the `VectorSource` seam (windowed-seam refactor): `gsrc` is read several
        // times below (field-presence check / dissolve / count) and also needed again afterward
        // (the `topology_simplify` fallback + the final `else` arm), so wrap once via `Arc::clone`
        // (a refcount bump, no data copy — `VectorSource::LoadAll` is itself `Arc`-backed) rather
        // than consuming `gsrc` itself. Each read below is a whole-file op, hence `full_extent()`
        // (LoadAll ignores it; behavior-identical to the old direct `.features()`).
        let gsrc_vs = vector::source::VectorSource::LoadAll(gsrc.clone());
        // Optional same-class dissolve first (offline); the SP2a topology-simplify path then runs on
        // the dissolved coverage (its class boundaries become the shared arcs).
        let dissolved: Option<Vec<vector::feature::Feature>> = match &spec.topology_dissolve {
            Some(field) => {
                vector::topology::validate_tolerance(spec.snap_tolerance)?;
                // Reject a typo'd field early: otherwise EVERY feature takes the null/missing
                // pass-through arm, dissolve is a silent no-op, and the un-dissolved 842k-feature
                // coverage feeds the heavy topology build — the exact path the operator meant to avoid.
                if !gsrc_vs
                    .features_in(gsrc_vs.full_extent())?
                    .as_slice()
                    .iter()
                    .any(|f| f.props.get(field).is_some())
                {
                    return Err(format!(
                        "--topology-dissolve field '{field}' is not present on any feature of {geojson_path}"
                    )
                    .into());
                }
                let feats = vector::topology::dissolve::dissolve_coverage(
                    gsrc_vs.features_in(gsrc_vs.full_extent())?.as_slice(),
                    field,
                    spec.snap_tolerance,
                    spec.topology_dissolve_rollup,
                );
                eprintln!(
                    "topology-dissolve '{field}': {} features -> {} class regions",
                    gsrc_vs.features_in(gsrc_vs.full_extent())?.as_slice().len(),
                    feats.len()
                );
                Some(feats)
            }
            None => None,
        };
        let src: std::sync::Arc<dyn FeatureSource> = if let Some(tol_len) = spec.topology_simplify {
            let snap = spec.snap_tolerance;
            // Guard the new consumers of these tolerances (mirrors run_build_topology): a zero/negative/
            // NaN snap collapses every vertex to the origin (silent empty map); a negative simplify
            // tolerance squares to a positive, huge min_area (silent over-simplify).
            vector::topology::validate_tolerance(snap)?;
            // Must be POSITIVE: it is the finest (max-zoom) tolerance and the FLOOR the per-zoom LOD
            // pools dedup at. `0` (no floor) makes every zoom's tolerance distinct → ~23 near-full-
            // resolution pools materialised + held → OOM on a large coverage.
            if !(tol_len > 0.0 && tol_len.is_finite()) {
                return Err(format!(
                    "--topology-simplify must be a positive finite number (it is the finest tolerance \
                     and the per-zoom LOD floor), got {tol_len}"
                )
                .into());
            }
            // `gsrc_batch` is bound here (not chained inline) so its `FeatureBatch` outlives
            // `base_features`'s borrow — `.features_in(bbox).as_slice()` chained directly inside the
            // `unwrap_or_else` closure would borrow from a temporary dropped at the closure's return,
            // a dangling-reference compile error for a hypothetical future `Owned` batch (harmless
            // today since `LoadAll` never allocates, but the borrow checker doesn't know that).
            let gsrc_batch = gsrc_vs.features_in(gsrc_vs.full_extent())?;
            let base_features: &[vector::feature::Feature] = dissolved
                .as_deref()
                .unwrap_or_else(|| gsrc_batch.as_slice());
            let (topo, report) = vector::topology::build_topology(base_features, snap);
            eprintln!("{}", vector::topology::format_report(&report));
            let min_area = (tol_len / snap).powi(2);
            let before: usize = topo.arcs.iter().map(|a| a.len()).sum();
            // Simplify the finest tolerance ONCE: this pool serves the un-LOD'd source (extent/fields
            // + the GFI fallback) AND is reused by build_lod for its floored tail — no double
            // Visvalingam+guard pass on the longest arcs.
            let pool: std::sync::Arc<Vec<vector::topology::ArcLine>> = std::sync::Arc::new(
                vector::topology::materialize::simplify_topology(&topo, min_area),
            );
            let after: usize = pool.iter().map(|a| a.len()).sum();
            let feats = vector::topology::materialize::materialize(&topo, pool.as_slice(), snap);
            eprintln!(
                    "topology-simplify {tol_len}: arc vertices {before} -> {after}  ({} features materialised)",
                    feats.len()
                );
            lod_inputs = Some((topo, min_area, snap, pool));
            std::sync::Arc::new(vector::topology::materialize::TopologyFeatureSource::new(
                feats,
            ))
        } else if let Some(feats) = dissolved {
            // dissolve WITHOUT simplify → serve the dissolved class regions directly.
            std::sync::Arc::new(vector::topology::materialize::TopologyFeatureSource::new(
                feats,
            ))
        } else {
            gsrc.clone()
        };
        (src, crs)
    } else {
        if spec.topology_simplify.is_some()
            || spec.topology_dissolve.is_some()
            || spec.keep_fields.is_some()
        {
            eprintln!(
                "WARNING: --topology-simplify/--topology-dissolve/--keep-fields apply only to .gpkg vector sources; ignored for {geojson_path}"
            );
        }
        (
            std::sync::Arc::new(vector::geojson::GeoJsonSource::load(geojson_path)?),
            src_crs,
        )
    };
    // Wrapped once for the (behavior-identical) `VectorSource` seam — `src` is still needed as an
    // `Arc<dyn FeatureSource>` below (the LOD/no-LOD `source` selection), so clone rather than move
    // (an `Arc::clone` refcount bump, no data copy — `VectorSource::LoadAll` is itself `Arc`-backed).
    let src_vs = vector::source::VectorSource::LoadAll(src.clone());
    let ext = src.full_extent();
    // full_extent is [west, south, east, north] in the source CRS; the fixtures are 4326.
    let bounds_wgs84 = if src_crs == "EPSG:4326" || src_crs == "CRS:84" {
        ext
    } else {
        reproj::wgs84_bounds(&src_crs, ext[0], ext[1], ext[2], ext[3])
            .unwrap_or([-180.0, -90.0, 180.0, 90.0])
    };
    let paths = vector_serves_note(&grids);
    println!(
        "layer '{name}': vector ({} features)  bounds W {:.4} S {:.4} E {:.4} N {:.4}  {paths}",
        src_vs.features_in(src_vs.full_extent())?.as_slice().len(),
        bounds_wgs84[0],
        bounds_wgs84[1],
        bounds_wgs84[2],
        bounds_wgs84[3],
    );
    // Per-zoom LOD pools (topology serve): build here, where the layer's area_scale is known.
    const MAX_LOD_ZOOM: u32 = 22;
    let area_scale = crate::vector::mvt::layer_area_scale(bounds_wgs84, src.full_extent());
    let lod = lod_inputs.map(|(topo, min_area, snap, finest_pool)| {
        std::sync::Arc::new(vector::topology::lod::build_lod(
            &topo,
            snap,
            area_scale,
            min_area,
            MAX_LOD_ZOOM,
            finest_pool,
        ))
    });
    let fields = mvt_http::feature_field_schema_vs(&src_vs);
    // When LOD is built its finest pool IS the full-detail coverage, so serve that as `source` and
    // DROP the separately-built SP2a pool (`src`) — otherwise the finest coverage sits in RAM twice.
    // Wrapped as `VectorSource::LoadAll` — the `VectorLayer.source` field type (the windowed-seam
    // migration, Task 1b).
    let source = vector::source::VectorSource::LoadAll(match &lod {
        Some(l) => l.finest(),
        None => src,
    });
    Ok(server::Layer {
        name,
        cog_path: String::new(),
        cog: None,
        source: None,
        style: None,
        src_crs,
        band_math: None,
        bounds_wgs84,
        tile_cache: None,
        // Never touched (a vector layer has no COG), but the field is unconditional — cheap to
        // build regardless (`cache::new_index_cache` is just a `moka::sync::Cache::builder()`).
        index_cache: cache::new_index_cache(cache::index_cache_bytes()),
        grids,
        vector: Some(server::VectorLayer {
            fields,
            area_scale,
            min_feature_px,
            source,
            style,
            shaper,
            lod,
        }),
        pmtiles,
        raster_pmtiles: std::collections::BTreeMap::new(),
        overlay: std::collections::BTreeMap::new(),
    })
}

/// `build_vector_layer`'s `.gpkg` windowed-vs-load-all dispatch (see the `windowed_gpkg` gate at
/// the top of the `.gpkg` handling): `fixtures/gpkg/mini.gpkg` carries an OGC R-tree
/// (`rtree_feats_geom`), so a plain raw-serve request must take the windowed path
/// (`VectorSource::Windowed`); any of the three load-all-only transforms
/// (`--topology-simplify`/`--topology-dissolve`/`--keep-fields`) must fall through to the
/// existing load-all arm (`VectorSource::LoadAll`) unchanged, even though the file has an rtree —
/// windowing is incompatible with those (they need the whole feature set in memory).
#[cfg(test)]
mod windowed_gpkg_dispatch_tests {
    use super::*;
    use crate::vector::source::VectorSource;

    const MINI: &str = "fixtures/gpkg/mini.gpkg";
    const VEC_STYLE: &str = "fixtures/styles/countries.vec.json";
    const FONT: &str = "fixtures/fonts/DejaVuSans.ttf";

    /// All-defaults `ServeArgs` (mirrors `run_build_pmtiles`'s reconstruction) — every field
    /// explicit, since `ServeArgs` derives no `Default`. `pub(super)` so the raster-PMTiles
    /// registration tests below build their layers from the exact same baseline.
    pub(super) fn base_serve_args() -> ServeArgs {
        ServeArgs {
            config: None,
            cog: None,
            style: None,
            host: "127.0.0.1".into(),
            port: 8080,
            public_url: None,
            cache_lru: 256,
            no_cache_lru: false,
            src_crs: None,
            expression: None,
            bands: None,
            nodata: None,
            s3_endpoint: None,
            s3_region: None,
            name: None,
            vector: None,
            pmtiles: Vec::new(),
            raster_pmtiles: Vec::new(),
            pmtiles_cache: false,
            pmtiles_flush_interval: 0,
            pmtiles_overlay_max_mib: 0,
            vec_style: None,
            snap_tolerance: 0.01,
            topology_simplify: None,
            topology_dissolve: None,
            topology_dissolve_rollup: None,
            keep_fields: None,
            font: None,
            tms_grids: Vec::new(),
            tms_tile_px: 512,
            max_inflight: 0,
            mvt_max_features: crate::vector::mvt::DEFAULT_MAX_FEATURES_PER_TILE,
            mvt_min_feature_px: 0.0,
            raster_min_feature_px: None,
            mvt_no_optimizations: false,
            mvt_no_safety_limit: false,
            mvt_cell_px: 0.0,
            mvt_cell_field: None,
            mvt_cell_max_zoom: 0,
            mvt_dissolve: None,
            mvt_dissolve_max_zoom: 0,
            mvt_cache: 256,
            wms_cache: 256,
            mvt_style: None,
        }
    }

    #[test]
    fn plain_raw_serve_of_an_rtree_gpkg_takes_the_windowed_path() {
        let args = base_serve_args();
        let s3 = crate::s3::S3Config::from_env();
        let layer = build_vector_layer(
            &VectorLayerSpec::from_serve_args(
                &args,
                "mini".to_string(),
                MINI.to_string(),
                VEC_STYLE.to_string(),
                FONT.to_string(),
                Some("EPSG:4326".to_string()),
            ),
            &s3,
        )
        .unwrap();
        let vector = layer.vector.expect("vector layer");
        assert!(
            matches!(vector.source, VectorSource::Windowed(_)),
            "an rtree-indexed .gpkg with no load-all-only flags must dispatch to the windowed \
             seam"
        );
    }

    /// The `--config` path passes the layer's OWN `src_crs:` as the parameter, but hands over the
    /// GLOBAL `ServeArgs` alongside it. If CRS precedence is decided by reading `args.src_crs`
    /// (the global `--src-crs` flag, normally `None` under `--config`), the per-layer value is
    /// silently discarded in favour of the file's header CRS.
    ///
    /// This pins the contract: the `src_crs` PARAMETER is what the caller resolved for THIS
    /// layer, and it must win. mini.gpkg declares EPSG:4326 in its own header, so asking for
    /// EPSG:2056 with no global flag set is exactly the config case.
    #[test]
    fn an_explicit_per_layer_src_crs_is_not_overridden_by_the_file_header() {
        let mut args = base_serve_args();
        args.src_crs = None; // as under `--config` with no global --src-crs
        let s3 = crate::s3::S3Config::from_env();
        let layer = build_vector_layer(
            &VectorLayerSpec::from_serve_args(
                &args,
                "mini".to_string(),
                MINI.to_string(),
                VEC_STYLE.to_string(),
                FONT.to_string(),
                Some("EPSG:2056".to_string()),
            ),
            &s3,
        )
        .unwrap();
        assert_eq!(
            layer.src_crs, "EPSG:2056",
            "the per-layer src_crs must win over the .gpkg header CRS"
        );
    }

    #[test]
    fn topology_simplify_falls_through_to_load_all_even_with_an_rtree() {
        let mut args = base_serve_args();
        args.topology_simplify = Some(1.0);
        let s3 = crate::s3::S3Config::from_env();
        let layer = build_vector_layer(
            &VectorLayerSpec::from_serve_args(
                &args,
                "mini".to_string(),
                MINI.to_string(),
                VEC_STYLE.to_string(),
                FONT.to_string(),
                Some("EPSG:4326".to_string()),
            ),
            &s3,
        )
        .unwrap();
        let vector = layer.vector.expect("vector layer");
        assert!(
            matches!(vector.source, VectorSource::LoadAll(_)),
            "--topology-simplify must fall through to load-all even on an rtree-indexed .gpkg"
        );
    }

    #[test]
    fn keep_fields_falls_through_to_load_all_even_with_an_rtree() {
        let mut args = base_serve_args();
        args.keep_fields = Some("name".to_string());
        let s3 = crate::s3::S3Config::from_env();
        let layer = build_vector_layer(
            &VectorLayerSpec::from_serve_args(
                &args,
                "mini".to_string(),
                MINI.to_string(),
                VEC_STYLE.to_string(),
                FONT.to_string(),
                Some("EPSG:4326".to_string()),
            ),
            &s3,
        )
        .unwrap();
        let vector = layer.vector.expect("vector layer");
        assert!(
            matches!(vector.source, VectorSource::LoadAll(_)),
            "--keep-fields must fall through to load-all even on an rtree-indexed .gpkg"
        );
    }

    #[test]
    fn topology_dissolve_falls_through_to_load_all_even_with_an_rtree() {
        let mut args = base_serve_args();
        args.topology_dissolve = Some("name".to_string());
        let s3 = crate::s3::S3Config::from_env();
        let layer = build_vector_layer(
            &VectorLayerSpec::from_serve_args(
                &args,
                "mini".to_string(),
                MINI.to_string(),
                VEC_STYLE.to_string(),
                FONT.to_string(),
                Some("EPSG:4326".to_string()),
            ),
            &s3,
        )
        .unwrap();
        let vector = layer.vector.expect("vector layer");
        assert!(
            matches!(vector.source, VectorSource::LoadAll(_)),
            "--topology-dissolve must fall through to load-all even on an rtree-indexed .gpkg"
        );
    }

    /// Task 2: a vector layer built with a custom grid id must resolve it and populate
    /// `layer.grids` — mirroring `build_layer`'s (raster) grid publishing, which this test would
    /// have caught was silently skipped for every vector layer (`grids: Vec::new()` unconditionally
    /// at all 3 `build_vector_layer` return sites).
    #[test]
    fn vector_layer_custom_grid_grids_non_empty() {
        let args = base_serve_args();
        // A small, deliberately level-invariant (dyadic) custom grid — LV95-like (a real projected
        // CRS id, EPSG:2056), 512km square, tile_px 256, halving resolutions 1000..125 m/px.
        let mut custom = std::collections::BTreeMap::new();
        custom.insert(
            "testgrid".to_string(),
            crate::config::GridConfig {
                crs: "EPSG:2056".to_string(),
                origin: [0.0, 512_000.0],
                extent: [0.0, 0.0, 512_000.0, 512_000.0],
                tile_px: 256,
                resolutions: vec![1000.0, 500.0, 250.0, 125.0],
            },
        );
        let grid_ids = vec!["testgrid".to_string()];
        let s3 = crate::s3::S3Config::from_env();
        let spec = VectorLayerSpec::from_serve_args(
            &args,
            "mini".to_string(),
            MINI.to_string(),
            VEC_STYLE.to_string(),
            FONT.to_string(),
            Some("EPSG:4326".to_string()),
        )
        .with_grids(grid_ids, 512, custom);
        let layer = build_vector_layer(&spec, &s3).unwrap();
        assert!(
            layer.grids.iter().any(|g| g.tms.id == "testgrid"),
            "expected a resolved 'testgrid' grid, got {:?}",
            layer
                .grids
                .iter()
                .map(|g| g.tms.id.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_postgis_layer_without_an_extent_is_a_startup_error() {
        // extent: is REQUIRED for postgis and authoritative -- we never query the database for
        // it. Failing loudly at startup beats a layer that silently advertises the wrong bounds.
        let mut args = base_serve_args();
        args.src_crs = Some("EPSG:2056".to_string());
        let s3 = crate::s3::S3Config::from_env();
        let spec = VectorLayerSpec::from_serve_args(
            &args,
            "pg".to_string(),
            "postgis://ts:${P}@db/gis/public.parcels".to_string(),
            VEC_STYLE.to_string(),
            FONT.to_string(),
            Some("EPSG:2056".to_string()),
        );
        // `.unwrap_err()` needs `server::Layer: Debug`, which it isn't (it carries trait-object
        // fields like `Arc<dyn FeatureSource>`/caches) — a `match` sidesteps that instead of
        // adding a Debug impl production code has no other use for.
        let err = match build_vector_layer(&spec, &s3) {
            Ok(_) => panic!("a postgis:// layer with no declared extent must not build"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("extent"),
            "the error must name the missing key: {err}"
        );
        assert!(err.contains("pg"), "the error must name the layer: {err}");
    }
}

/// Startup validation of `--raster-pmtiles` archives (Task 5). Every check here exists because the
/// failure it prevents is SILENT: an archive of the wrong payload type, on a grid nobody publishes,
/// or baked at the wrong tile size all produce a server that starts cleanly and then serves either
/// the wrong bytes or nothing from the pyramid at all, with no way to tell from the outside.
#[cfg(test)]
mod raster_pmtiles_registration_tests {
    use super::windowed_gpkg_dispatch_tests::base_serve_args;
    use super::*;

    const MINI: &str = "fixtures/gpkg/mini.gpkg";
    const VEC_STYLE: &str = "fixtures/styles/countries.vec.json";
    const FONT: &str = "fixtures/fonts/DejaVuSans.ttf";

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ts_raster_reg_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A mini.gpkg vector layer publishing `grids` at `tile_px`, with `raster` registered as its
    /// raster archives. Returns whatever `build_vector_layer` decides — Ok or the startup error.
    fn build(grids: &[&str], tile_px: u32, raster: Vec<String>) -> Result<server::Layer, Error> {
        let args = base_serve_args();
        let spec = VectorLayerSpec::from_serve_args(
            &args,
            "mini".to_string(),
            MINI.to_string(),
            VEC_STYLE.to_string(),
            FONT.to_string(),
            Some("EPSG:4326".to_string()),
        )
        .with_grids(
            grids.iter().map(|g| g.to_string()).collect(),
            tile_px,
            std::collections::BTreeMap::new(),
        )
        .with_raster_pmtiles(raster);
        build_vector_layer(&spec, &crate::s3::S3Config::from_env())
    }

    /// Bake a one-zoom PNG archive of mini.gpkg on `WebMercatorQuad` at `tile_px`.
    fn bake_png(dir: &std::path::Path, tile_px: u32) -> String {
        let layer = build(&["WebMercatorQuad"], tile_px, Vec::new()).unwrap();
        let grid = tms::preset("WebMercatorQuad", tile_px).unwrap();
        let out = dir.join(format!("raster_{tile_px}.pmtiles"));
        vector::pmtiles::raster::build_raster_pmtiles(
            &layer,
            &grid,
            0,
            0,
            layer.bounds_wgs84,
            &out,
            dir,
        )
        .unwrap();
        out.to_string_lossy().to_string()
    }

    #[test]
    fn a_matching_png_archive_is_filed_under_the_published_grid_id() {
        let dir = scratch("ok");
        let archive = bake_png(&dir, 512);
        let layer = build(&["WebMercatorQuad"], 512, vec![archive]).unwrap();
        // Keyed by the PUBLISHED id (suffix and all), which is what the tile routes look up.
        assert_eq!(
            layer.raster_pmtiles.keys().collect::<Vec<_>>(),
            vec!["WebMercatorQuad_512"],
            "the archive must be filed under the grid the tile routes resolve"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_mvt_archive_under_raster_pmtiles_is_refused_at_startup() {
        let dir = scratch("mvt");
        // A real MVT archive, baked exactly as `build-pmtiles` (default --tile-format) would.
        let layer = build(&["WebMercatorQuad"], 512, Vec::new()).unwrap();
        let state = server::ServeState::new(vec![], String::new(), 1);
        let opts =
            crate::vector::mvt::MvtOptimizations::for_layer(&state, layer.vector.as_ref().unwrap());
        let grid = tms::preset("WebMercatorQuad", 4096).unwrap();
        let out = dir.join("mvt.pmtiles");
        vector::pmtiles::generate::build_pmtiles(
            &layer,
            &opts,
            &grid,
            0,
            0,
            layer.bounds_wgs84,
            &out,
            &dir,
        )
        .unwrap();

        let path = out.to_string_lossy().to_string();
        let err = build(&["WebMercatorQuad"], 512, vec![path.clone()])
            .err()
            .expect("an MVT archive on the raster path must be refused")
            .to_string();
        assert!(err.contains(&path), "must name the file: {err}");
        assert!(
            err.contains("MVT") && err.contains("PNG"),
            "must name both formats: {err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_png_archive_baked_at_the_wrong_tile_size_is_refused() {
        let dir = scratch("px");
        let archive = bake_png(&dir, 256);
        // Published at 512: the ids still match (`WebMercatorQuad` vs `WebMercatorQuad_512`), so
        // only the pixel-size check stands between the operator and a quarter-size map.
        let err = build(&["WebMercatorQuad"], 512, vec![archive.clone()])
            .err()
            .expect("a 256-px archive on a 512-px grid must be refused")
            .to_string();
        assert!(err.contains(&archive), "must name the file: {err}");
        assert!(
            err.contains("256") && err.contains("512"),
            "must name both tile sizes: {err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_png_archive_for_an_unpublished_grid_is_refused() {
        let dir = scratch("grid");
        let archive = bake_png(&dir, 512);
        // The layer publishes a DIFFERENT grid, so nothing would ever look this archive up — a
        // precompute that silently never gets read is the failure worth failing loudly on.
        let err = build(&["WorldCRS84Quad"], 512, vec![archive.clone()])
            .err()
            .expect("an archive on an unpublished grid must be refused")
            .to_string();
        assert!(err.contains(&archive), "must name the file: {err}");
        assert!(
            err.contains("does not publish"),
            "must say the grid is not published: {err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
