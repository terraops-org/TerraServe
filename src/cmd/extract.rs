// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! `terraserve extract` -- materialize a per-zoom pre-generalized SUBSET of a vector source as a
//! GeoPackage.
//!
//! **Why this exists.** A per-TILE feature cap makes the surviving fraction `cap / candidates`, a
//! per-tile quantity, so two adjacent tiles over continuous geography keep different proportions
//! and a hard density seam appears at the boundary. Tippecanoe documents this against its own
//! per-tile limiter: "It will probably look ugly at the tile boundaries." A per-ZOOM threshold has
//! no such property, because it is a constant and gives an identical keep/drop answer everywhere.
//!
//! So instead of selecting while tiling, select ONCE per zoom band over the whole dataset and
//! write the result out. Tiling then becomes a pure spatial cut of an already-selected dataset,
//! with no selection logic left in it to vary. That is what imposm3's `generalized_tables`,
//! osm2pgsql's `osm2pgsql-gen` and OpenMapTiles' `..._gen_z11`..`_z4` tables all do; see
//! `docs/tile-seam-research-2026-08-30.md`.
//!
//! **The gate runs on the UNCLIPPED feature**, which is the whole reason this belongs in an
//! extract step rather than in the encoder: a feature clipped small at a tile edge would otherwise
//! fail a threshold its whole-geometry neighbour passes -- a seam that survives every other fix.
//!
//! **Chaining is an invocation pattern, not machinery.** `--vector` accepts the `.gpkg` this
//! writes, so the z_n subset is extracted from the z_{n+1} subset and only the deepest band ever
//! touches the source database. imposm3 chains `generalized_tables` the same way, and
//! OpenMapTiles declares `gen_z4` as `LIKE gen_z5`.

use crate::cmd::render::parse_bbox;
use crate::layer::build_vector_layer;
use crate::s3;
use crate::server;
use crate::tms;
use crate::vector::gpkg_write::GpkgWriter;
use crate::vector::mvt::tile::{min_area_src_for_grid, min_len_src_for_grid, passes_size_gate};
use crate::Error;
use crate::ServeArgs;
use clap::Args;

/// Every windowed reader caps one query at this many features by default
/// (`TERRASERVE_{GPKG,FGB,PG}_MAX_QUERY_FEATURES`). An extract reads the WHOLE layer in one go, so
/// the cap is far more likely to bind here than on any tile request -- and if it does, the subset
/// is silently short. Hitting it exactly is the signal.
const DEFAULT_QUERY_CAP: usize = 500_000;

#[derive(Args, Debug)]
pub struct ExtractArgs {
    /// Vector source to select from: a `.gpkg` / `.fgb` / `.geojson` path, an `s3://` URI, or a
    /// PostGIS connection URI. Accepts a GeoPackage this command wrote, which is how zoom bands
    /// are chained (extract z_n from the z_{n+1} subset, never from the full table).
    #[arg(long = "vector")]
    pub vector: String,

    /// Vector style. Required only because `extract` builds its layer through the same path
    /// `serve` and `build-pmtiles` use; nothing here renders, and the style does not affect which
    /// features are selected.
    #[arg(long = "vec-style")]
    pub vec_style: String,

    /// Output `.gpkg` path. Overwritten if it exists.
    #[arg(long = "out")]
    pub out: String,

    /// Layer/table name written into the GeoPackage. Defaults to `subset`.
    #[arg(long = "name", default_value = "subset")]
    pub name: String,

    /// TileMatrixSet the thresholds are computed against: a preset id, or a path to an OGC
    /// TileMatrixSet 2.0 JSON. Must be the SAME grid the tiles will be cut on, since the
    /// threshold is derived from that grid's resolution at each zoom.
    #[arg(long = "grid", default_value = "WebMercatorQuad")]
    pub grid: String,

    /// Shallowest zoom this subset must serve.
    #[arg(long = "min-zoom")]
    pub min_zoom: u32,

    /// Deepest zoom this subset must serve. The band's threshold is the LOOSEST of the band, which
    /// is normally this zoom's, so the subset is a superset of every zoom in it.
    #[arg(long = "max-zoom")]
    pub max_zoom: u32,

    /// Minimum on-screen feature size in pixels, the polygon size gate. `0` disables it, leaving
    /// only the always-on one-grid-cell floor.
    #[arg(long = "mvt-min-feature-px", default_value_t = 0.0)]
    pub mvt_min_feature_px: f64,

    /// Zoom at which `--mvt-min-feature-px` starts applying. Below it only the cell floor applies.
    /// A px-denominated gate quarters per zoom in but feature sizes do not, so one value cannot
    /// serve a whole pyramid.
    #[arg(long = "mvt-min-feature-px-min-zoom", default_value_t = 0)]
    pub mvt_min_feature_min_zoom: u32,

    /// Minimum on-screen LENGTH in pixels for line geometry, as a bare number or a per-zoom step
    /// list like `0:2.0,7:0.3`.
    #[arg(long = "mvt-min-feature-len-px", default_value = "")]
    pub mvt_min_feature_len_px: String,

    /// Source CRS, when the file does not declare one.
    #[arg(long = "src-crs")]
    pub src_crs: Option<String>,

    /// Read only this window, `minx,miny,maxx,maxy` in the source CRS. Defaults to the layer's
    /// full extent, which is what a real subset wants. Note the output still DECLARES the source's
    /// full extent whatever this narrows the read to, because the per-zoom threshold is derived
    /// from the declared extent -- so a windowed subset self-describes as covering more ground
    /// than it holds.
    #[arg(long = "extent")]
    pub extent: Option<String>,

    /// Comma-separated attribute columns to carry into the subset. Defaults to all of them.
    #[arg(long = "keep-fields")]
    pub keep_fields: Option<String>,

    /// Comma-separated attribute columns the subset must carry, e.g. `building,highway`.
    ///
    /// This is the only knob that reaches a DATABASE. A `postgis://` layer's column list is
    /// derived from the server-side style's referenced fields, so anything only a CLIENT-side
    /// `--mvt-style` needs is never selected and the subset comes out with no attributes at all --
    /// which is what the first eu5 `water` extract did. For a file source it simply restricts the
    /// written schema. Unset = whatever the source yields.
    #[arg(long = "columns", conflicts_with = "no_columns")]
    pub columns: Option<String>,

    /// Write geometry only, carrying no attributes at all.
    ///
    /// Worth asking for explicitly rather than leaving to chance: a geometry-only subset is much
    /// smaller, and for a layer drawn in one colour it loses nothing. Saying so also distinguishes
    /// "no attributes wanted" from "no attributes arrived", which is the failure this pair exists
    /// to make visible.
    #[arg(long = "no-columns", default_value_t = false)]
    pub no_columns: bool,
}

/// Per-zoom thresholds, for the report and for picking the band minimum.
struct ZoomThreshold {
    zoom: u32,
    area: f64,
    len: f64,
}

/// `--columns` as a list, or `None` when it was not given. Empty entries are dropped so a stray
/// trailing comma cannot turn into a column named "".
fn requested_columns(args: &ExtractArgs) -> Option<Vec<String>> {
    let list = args.columns.as_deref()?;
    Some(
        list.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

pub fn run_extract(args: &ExtractArgs) -> Result<(), Error> {
    if args.out.is_empty() {
        return Err("extract: --out is required".into());
    }
    // clap's `conflicts_with` guards argv; this guards the library entry point, which the tests
    // and any embedder call directly. Picking a winner silently is how a subset ends up missing
    // the one attribute its style needs.
    if args.no_columns && args.columns.is_some() {
        return Err("extract: --columns and --no-columns cannot both be given".into());
    }
    if args.min_zoom > args.max_zoom {
        return Err(format!(
            "extract: --min-zoom {} > --max-zoom {}",
            args.min_zoom, args.max_zoom
        )
        .into());
    }
    if args.max_zoom > 26 {
        return Err("extract: --max-zoom must be <= 26".into());
    }

    // Build the layer exactly as `serve` and `build-pmtiles` do, so a subset is selected from the
    // same source, in the same CRS, with the same area_scale the encoder will use.
    let serve_args = serve_args_for(args);
    let spec = crate::layer::VectorLayerSpec::from_serve_args(
        &serve_args,
        args.name.clone(),
        args.vector.clone(),
        args.vec_style.clone(),
        "fixtures/fonts/DejaVuSans.ttf".to_string(),
        args.src_crs.clone(),
    )
    .with_extent(match args.extent.as_deref() {
        Some(s) => Some(parse_bbox(s)?),
        None => None,
    })
    // The half that only matters for `postgis://`: a database query has to NAME its columns, and
    // the derived list sees only the server-side style. Harmless for a file source, which reads
    // whatever the feature carries.
    .with_columns(requested_columns(args).unwrap_or_default());
    let layer = build_vector_layer(&spec, &s3::S3Config::from_env())?;
    let vlayer = layer
        .vector
        .as_ref()
        .ok_or("extract: --vector did not produce a vector layer")?;

    let grid = resolve_grid(&args.grid)?;
    let src_crs = &layer.src_crs;

    // The per-zoom thresholds, from the encoder's OWN functions. Re-deriving this arithmetic here
    // is exactly how an extract would come to disagree with the tiles cut from it.
    let mut state = server::ServeState::new(vec![], "http://127.0.0.1/wms".into(), 1);
    // The size-gate flags MUST be copied onto the state, because `MvtOptimizations::for_layer`
    // reads them from there and not from the layer. Without this the state's defaults win and all
    // three flags are silently ignored -- every subset then comes out cut at the one-MVT-cell
    // floor instead of the requested threshold, which is how a 2.0 px landuse band produced 3.3M
    // features where 38k were expected. The baked tiles were never wrong (a too-loose subset is
    // still a valid superset and `build-pmtiles` re-applies the gate at encode time), but a flag
    // that does nothing is worse than one that errors.
    state.mvt_min_feature_px = args.mvt_min_feature_px;
    state.mvt_min_feature_min_zoom = args.mvt_min_feature_min_zoom;
    state.mvt_min_feature_len_px =
        crate::vector::mvt::parse_len_px_spec(&args.mvt_min_feature_len_px)?;
    let opts = crate::vector::mvt::MvtOptimizations::for_layer(&state, vlayer);
    let mut table: Vec<ZoomThreshold> = Vec::new();
    for z in args.min_zoom..=args.max_zoom {
        table.push(ZoomThreshold {
            zoom: z,
            area: min_area_src_for_grid(
                &grid,
                z,
                src_crs,
                opts.area_scale,
                opts.min_feature_px_at(z),
            ),
            len: min_len_src_for_grid(
                &grid,
                z,
                src_crs,
                opts.area_scale,
                opts.min_feature_len_px_at(z),
            ),
        });
    }

    eprintln!("extract: thresholds per zoom on grid `{}`", grid.id);
    eprintln!("  zoom   min area (src units^2)   min length (src units)");
    for t in &table {
        eprintln!("  {:>4}   {:>22.6}   {:>21.6}", t.zoom, t.area, t.len);
    }

    // A threshold should loosen as you zoom in. A step list can legally break that, and the config
    // shipped 2026-08-29 does, so SAY SO rather than assuming. It is not fatal: the band minimum is
    // the loosest gate in the band either way, so the subset stays a superset of every zoom in it.
    report_non_monotonic(&table);

    let min_area = table.iter().map(|t| t.area).fold(f64::INFINITY, f64::min);
    let min_len = table.iter().map(|t| t.len).fold(f64::INFINITY, f64::min);
    eprintln!(
        "extract: band z{}-z{} selects at the LOOSEST threshold in the band: area >= {min_area}, length >= {min_len}",
        args.min_zoom, args.max_zoom
    );

    // Read the whole window. The gate is offered to the source as a pushdown (a huge win on
    // PostGIS, where it becomes SQL) AND applied in Rust below, which is the contract
    // `features_in_gated` requires of any caller passing a non-zero threshold.
    let window = match args.extent.as_deref() {
        Some(s) => parse_bbox(s)?,
        None => vlayer.source.full_extent(),
    };
    let batch = vlayer
        .source
        .features_in_gated(window, min_area)
        .map_err(|e| format!("extract: reading source: {e}"))?;
    let read = batch.len();
    if candidate_caps().contains(&read) {
        return Err(format!(
            "extract: the source returned exactly {read} features, which is the per-query cap -- \
             the subset would be silently short. Raise TERRASERVE_GPKG_MAX_QUERY_FEATURES / \
             TERRASERVE_FGB_MAX_QUERY_FEATURES / TERRASERVE_PG_MAX_QUERY_FEATURES above the \
             layer's feature count and run again."
        )
        .into());
    }

    // The gate, on the UNCLIPPED feature. Same predicate the encoder applies, so what lands here
    // is exactly what a tile at any zoom in this band would have kept.
    let keep: Vec<&crate::vector::feature::Feature> = batch
        .as_slice()
        .iter()
        .filter(|f| passes_size_gate(f, min_area, min_len))
        .collect();

    eprintln!(
        "extract: {read} features read, {} pass the gate ({:.1}%)",
        keep.len(),
        if read == 0 {
            0.0
        } else {
            keep.len() as f64 * 100.0 / read as f64
        }
    );

    // An empty band is not a file to write: BOTH readers refuse a GeoPackage with no features
    // (`GpkgSource::load` has nothing drawable, and a windowed open cannot determine an extent),
    // so shipping one means a server that will not start. Catch it here, where the fix is obvious.
    if keep.is_empty() {
        return Err(format!(
            "extract: the gate selected 0 of {read} features for z{}-z{}. That subset is unservable \
             (both readers refuse a GeoPackage with no extent), and at a shallow zoom it is the \
             classic silent blank map. Lower --mvt-min-feature-px, or narrow the zoom band.",
            args.min_zoom, args.max_zoom
        )
        .into());
    }

    let mut fields = crate::mvt_http::feature_field_schema_vs(&vlayer.source);
    if args.no_columns {
        fields.clear();
    } else if let Some(want) = requested_columns(args) {
        // Exactly the named set. A column the source does not have simply does not appear -- for
        // `postgis://` the fetch would already have failed at startup on an unknown column, and
        // for a file source there is nothing to fail on.
        let want: std::collections::BTreeSet<&str> = want.iter().map(|s| s.as_str()).collect();
        fields.retain(|k, _| want.contains(k.as_str()));
    }
    if let Some(list) = args.keep_fields.as_deref() {
        let wanted: std::collections::BTreeSet<&str> = list
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        fields.retain(|k, _| wanted.contains(k.as_str()));
    }

    let out = std::path::Path::new(&args.out);
    // The subset declares the extent of the layer it was cut FROM, not the bounds of what
    // survived the gate. The per-zoom threshold is derived from the layer extent
    // (`layer_area_scale`), so a subset that declared its own shrunken bounds would compute a
    // TIGHTER threshold than its source and then select slightly differently from the very thing
    // it exists to reproduce. Measured on Swiss buildings: letting the extent shrink by 0.0355%
    // moved 2 features out of 4063 at z7, and the drift compounds once subsets are chained.
    let full = vlayer.source.full_extent();
    if window != full {
        eprintln!(
            "NOTE: --extent narrowed the read to {window:?}, but the subset still DECLARES the \
             source's full extent {full:?}. That is deliberate -- the per-zoom threshold is \
             derived from the declared extent, so shrinking it would make this subset select \
             differently from its source. The file therefore self-describes as covering more \
             ground than it holds."
        );
    }
    let mut w = GpkgWriter::create(out, &args.name, layer_crs(&layer, vlayer), &fields)?
        .with_declared_extent(Some(full))
        // Stamp the band, so `serve`'s `zoom_sources:` can check a declaration against the file
        // rather than trusting it. A subset served outside the zooms it was cut for loses features
        // silently.
        .with_zoom_band(Some((args.min_zoom, args.max_zoom)));
    for f in &keep {
        w.add(f)?;
    }
    let written = w.finish()?;
    eprintln!(
        "extract: wrote {written} features to {} (layer `{}`, {} attribute columns)",
        args.out,
        args.name,
        fields.len()
    );
    Ok(())
}

/// The CRS to stamp on the subset: whatever the source declared, so a chained extract keeps
/// declaring the same thing all the way down.
fn layer_crs<'a>(layer: &'a server::Layer, vlayer: &'a server::VectorLayer) -> Option<&'a str> {
    vlayer.source.crs().or(Some(layer.src_crs.as_str()))
}

/// EVERY per-query cap that could apply, not the lowest.
///
/// Taking the minimum had a hole: with `TERRASERVE_PG_MAX_QUERY_FEATURES=100000` set for some
/// other reason and a `.gpkg` source capped at its own 2,000,000, the minimum is 100,000 and a
/// truncation at 2,000,000 goes unnoticed. Reading a truncated subset silently is the failure this
/// whole check exists to prevent, so it tests membership in the set instead.
///
/// Note the PostGIS query is `WHERE bbox && ... AND (size_gate) LIMIT n` -- the LIMIT applies
/// AFTER the gate -- so a truncated read really does come back as exactly `n` rows even when the
/// gate is pushed down, which is what makes this check work on that backend at all.
fn candidate_caps() -> Vec<usize> {
    let mut caps = vec![DEFAULT_QUERY_CAP];
    for v in [
        "TERRASERVE_GPKG_MAX_QUERY_FEATURES",
        "TERRASERVE_FGB_MAX_QUERY_FEATURES",
        "TERRASERVE_PG_MAX_QUERY_FEATURES",
    ] {
        if let Some(n) = std::env::var(v).ok().and_then(|s| s.parse::<usize>().ok()) {
            caps.push(n);
        }
    }
    caps
}

/// Warn when a threshold does NOT loosen as zoom increases. Harmless for correctness (the band
/// minimum is still the loosest gate), but it means the operator's step list does something other
/// than what a reader of it would expect, and that is worth one line of output.
fn report_non_monotonic(table: &[ZoomThreshold]) {
    for w in table.windows(2) {
        if w[1].area > w[0].area {
            eprintln!(
                "WARNING: the area threshold TIGHTENS from z{} ({}) to z{} ({}). A gate normally \
                 loosens as you zoom in; check the --mvt-min-feature-px-min-zoom band.",
                w[0].zoom, w[0].area, w[1].zoom, w[1].area
            );
        }
        if w[1].len > w[0].len {
            eprintln!(
                "WARNING: the length threshold TIGHTENS from z{} ({}) to z{} ({}). Check the \
                 --mvt-min-feature-len-px step list.",
                w[0].zoom, w[0].len, w[1].zoom, w[1].len
            );
        }
    }
}

/// A preset id, else a path to an OGC TileMatrixSet 2.0 JSON. Presets resolve at the MVT encode
/// extent (4096) so the thresholds match what `build-pmtiles` and the live encoder compute.
fn resolve_grid(grid: &str) -> Result<tms::TileMatrixSet, Error> {
    match tms::preset(grid, 4096) {
        Some(mut g) => {
            g.id = grid.to_string();
            Ok(g)
        }
        None => {
            let json = std::fs::read_to_string(grid).map_err(|e| format!("--grid {grid}: {e}"))?;
            Ok(tms::from_ogc_json(&json).map_err(|e| format!("--grid {grid}: {e}"))?)
        }
    }
}

/// A `ServeArgs` carrying only what layer construction reads, so `extract` shares one code path
/// with `serve` and `build-pmtiles` instead of opening sources its own way.
fn serve_args_for(args: &ExtractArgs) -> ServeArgs {
    ServeArgs {
        config: None,
        cog: None,
        style: None,
        host: "127.0.0.1".into(),
        port: 8080,
        public_url: None,
        cache_lru: 0,
        no_cache_lru: true,
        src_crs: args.src_crs.clone(),
        expression: None,
        bands: None,
        nodata: None,
        s3_endpoint: None,
        s3_region: None,
        name: Some(args.name.clone()),
        vector: Some(args.vector.clone()),
        pmtiles: Vec::new(),
        raster_pmtiles: Vec::new(),
        pmtiles_cache: false,
        pmtiles_flush_interval: 0,
        pmtiles_overlay_max_mib: 0,
        vec_style: Some(args.vec_style.clone()),
        snap_tolerance: 0.0,
        topology_simplify: None,
        topology_dissolve: None,
        topology_dissolve_rollup: None,
        keep_fields: None,
        font: None,
        tms_grids: Vec::new(),
        tms_tile_px: 512,
        max_inflight: 1,
        mvt_max_features: crate::vector::mvt::DEFAULT_MAX_FEATURES_PER_TILE,
        mvt_min_feature_px: args.mvt_min_feature_px,
        mvt_min_feature_min_zoom: args.mvt_min_feature_min_zoom,
        mvt_min_feature_len_px: args.mvt_min_feature_len_px.clone(),
        raster_min_feature_px: None,
        mvt_no_optimizations: false,
        mvt_no_safety_limit: false,
        mvt_cell_px: 0.0,
        mvt_cell_field: None,
        mvt_cell_max_zoom: 0,
        mvt_dissolve: None,
        mvt_dissolve_max_zoom: 0,
        mvt_cache: 0,
        wms_cache: 0,
        tile_max_age: 0,
        mvt_style: None,
    }
}
