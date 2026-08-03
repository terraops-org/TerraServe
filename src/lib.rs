// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! TerraServe pilot library.
//!
//! The measured agent implements the bodies of `run_render` and `run_wms_handle`
//! and everything they call. Requirements live in SPEC-AGENT.md and CLAUDE.md.
//!
//! Keep intact (they are graded / frozen):
//!   * the CLI arg shapes (`RenderArgs`, `WmsArgs`),
//!   * the `RangeSource` trait (cog.rs) — all COG bytes flow through it,
//!   * the batch-first `RenderBackend` trait (backend.rs) — GPU-readiness seam.

pub mod backend;
pub mod cache;
pub mod cog;
pub mod config;
pub mod decode;
pub mod expr;
pub mod legend;
pub mod mvt_http;
pub mod pngio;
pub mod render;
pub mod reproj;
pub mod s3;
pub mod server;
pub mod sld;
pub mod style;
pub mod tms;
pub mod tms_http;
pub mod vector;
pub mod wms;
pub mod wmts;
pub mod xml;

use clap::Args;
use std::io::Write;

/// `render` arguments — FROZEN (the scoring harness depends on these flag names).
#[derive(Args, Debug)]
pub struct RenderArgs {
    /// Path to the source COG.
    #[arg(long)]
    pub cog: String,
    /// Output bounding box `minx,miny,maxx,maxy` in `--crs` units (values may be negative).
    #[arg(long, allow_hyphen_values = true)]
    pub bbox: String,
    /// Output CRS, e.g. `EPSG:3857`. (Source CRS is EPSG:3763, given as a constant.)
    #[arg(long)]
    pub crs: String,
    #[arg(long)]
    pub width: u32,
    #[arg(long)]
    pub height: u32,
    /// `nearest` | `bilinear`.
    #[arg(long)]
    pub resample: String,
    /// Path to `style.json` (mode: `rgb` | `pseudocolor`).
    #[arg(long)]
    pub style: String,
    /// Output PNG path.
    #[arg(long)]
    pub out: String,
    /// The COG's own CRS, e.g. `EPSG:32629`. Optional and additive: when unset it defaults to the
    /// sample grid `EPSG:3763`, so existing invocations are byte-for-byte unchanged. Set it to
    /// render a source in any other projection (the `--bbox` window is still given in `--crs` units).
    #[arg(long)]
    pub src_crs: Option<String>,
}

/// `wms-handle` arguments — FROZEN.
#[derive(Args, Debug)]
pub struct WmsArgs {
    #[arg(long)]
    pub cog: String,
    #[arg(long)]
    pub style: String,
    /// Raw WMS KVP query string (GetMap / GetCapabilities / ...).
    #[arg(long)]
    pub query: String,
}

/// Top-level error. The agent may replace this with a richer error type.
pub type Error = Box<dyn std::error::Error>;

/// Engine core. Parse the COG (IFDs, tile offsets, overviews) → select the tiles and
/// overview level for this window → decode (DEFLATE required, YCbCr-JPEG stretch) →
/// warp/resample into the requested grid → style (`rgb` passthrough or `pseudocolor`
/// ramp) → honor mask/alpha as transparency → encode PNG to `args.out`.
/// NO GDAL at runtime.
pub fn run_render(args: &RenderArgs) -> Result<(), Error> {
    let bbox = parse_bbox(&args.bbox)?;
    let resample = match args.resample.trim().to_ascii_lowercase().as_str() {
        "nearest" => backend::Resample::Nearest,
        "bilinear" => backend::Resample::Bilinear,
        other => return Err(format!("unknown resample '{other}'").into()),
    };
    let style = style::Style::load(&args.style)?;
    let req = render::RenderRequest {
        cog_path: &args.cog,
        bbox,
        crs: &args.crs,
        // Optional --src-crs, defaulting to the sample grid EPSG:3763 so existing invocations are
        // byte-for-byte unchanged; set it to render a source in any other projection.
        src_crs: args.src_crs.as_deref().unwrap_or(reproj::SRC_CRS),
        width: args.width,
        height: args.height,
        resample,
        style: &style,
        band_math: None,
        index_cache: cache::new_index_cache(cache::index_cache_bytes()),
    };
    let rgba = render::render(&req)?;
    let png = pngio::encode_rgba(&rgba, args.width, args.height)?;
    std::fs::write(&args.out, png)?;
    Ok(())
}

fn parse_bbox(s: &str) -> Result<[f64; 4], Error> {
    let parts: Vec<f64> = s
        .split(',')
        .map(|p| p.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| "invalid --bbox (need minx,miny,maxx,maxy)")?;
    if parts.len() != 4 {
        return Err("--bbox needs exactly 4 comma-separated values".into());
    }
    Ok([parts[0], parts[1], parts[2], parts[3]])
}

#[cfg(test)]
mod render_cli_tests {
    use super::*;

    const COG: &str = "../cogs/cascais.cog.deflate.tif";
    const STYLE: &str = "fixtures/styles/rgb.json";
    // A window known to hold real data, in the source CRS EPSG:3763 — the same window cog.rs renders
    // in its lazy-vs-resident test, so it is guaranteed non-empty.
    const W3763: [f64; 4] = [-112701.25, -106296.25, -112573.25, -106168.25];
    const BBOX_3763: &str = "-112701.25,-106296.25,-112573.25,-106168.25";

    // The COGs live in ../cogs and are NOT committed (too large), so CI runs without them. A test
    // that needs one must self-skip when it is absent — mirrors cog.rs's cascais test.
    fn cog_present(cog: &str) -> bool {
        if std::path::Path::new(cog).exists() {
            return true;
        }
        eprintln!("skipping: COG fixture {cog} absent");
        false
    }

    // Render one 64×64 window through the CLI entry point and return the PNG bytes.
    fn render_window(
        cog: &str,
        crs: &str,
        bbox: &str,
        src_crs: Option<&str>,
        tag: &str,
    ) -> Vec<u8> {
        let out = std::env::temp_dir().join(format!("ts_render_srccrs_{tag}.png"));
        let args = RenderArgs {
            cog: cog.to_string(),
            bbox: bbox.to_string(),
            crs: crs.to_string(),
            width: 64,
            height: 64,
            resample: "nearest".to_string(),
            style: STYLE.to_string(),
            out: out.to_string_lossy().into_owned(),
            src_crs: src_crs.map(|s| s.to_string()),
        };
        run_render(&args).expect("render should succeed");
        let bytes = std::fs::read(&out).expect("read rendered png");
        let _ = std::fs::remove_file(&out);
        bytes
    }

    #[test]
    fn render_src_crs_defaults_to_the_sample_grid() {
        // Omitting --src-crs must render exactly as passing the sample grid EPSG:3763 explicitly:
        // the new optional flag preserves the frozen default behavior byte-for-byte.
        if !cog_present(COG) {
            return;
        }
        let default = render_window(COG, "EPSG:3763", BBOX_3763, None, "default");
        let explicit = render_window(
            COG,
            "EPSG:3763",
            BBOX_3763,
            Some("EPSG:3763"),
            "explicit3763",
        );
        assert_eq!(
            default, explicit,
            "explicit EPSG:3763 must match the omitted default"
        );
    }

    #[test]
    fn render_src_crs_is_actually_used() {
        // Declaring a different source CRS must change the pixels, proving --src-crs threads through
        // to the reprojection instead of being ignored. Treating the 3763 COG as 4326 maps the
        // window off the data, so the correct render and the wrong one must differ.
        if !cog_present(COG) {
            return;
        }
        let correct = render_window(COG, "EPSG:3763", BBOX_3763, Some("EPSG:3763"), "correct");
        let wrong = render_window(COG, "EPSG:3763", BBOX_3763, Some("EPSG:4326"), "wrong");
        assert_ne!(
            correct, wrong,
            "a different --src-crs must produce different pixels"
        );
    }

    #[test]
    fn render_reprojects_a_non_standard_source_into_wgs84_and_web_mercator() {
        // Cascais is EPSG:3763 (the Portuguese national grid), which is neither WGS84 nor Web
        // Mercator. --src-crs must let render reproject that source INTO each standard CRS:
        // declaring the true source CRS renders the scene, while declaring the wrong one (the output
        // CRS itself, i.e. pretending the source is already in the output projection) maps the window
        // off the data. The output window is the known 3763 data window reprojected into the target
        // CRS, so the correct render is guaranteed to land on data and the two must differ.
        if !cog_present(COG) {
            return;
        }
        let [minx, miny, maxx, maxy] = W3763;
        for out in ["EPSG:4326", "EPSG:3857"] {
            let w = crate::reproj::crs_bounds("EPSG:3763", out, minx, miny, maxx, maxy)
                .unwrap_or_else(|| panic!("reproject the 3763 window into {out}"));
            let bbox = format!("{},{},{},{}", w[0], w[1], w[2], w[3]);
            let epsg = &out[5..]; // "EPSG:4326" -> "4326", for a unique temp filename
            let correct = render_window(COG, out, &bbox, Some("EPSG:3763"), &format!("{epsg}_ok"));
            let wrong = render_window(COG, out, &bbox, Some(out), &format!("{epsg}_bad"));
            assert_ne!(
                correct, wrong,
                "--src-crs must reproject 3763->{out}, not assume the source is already {out}"
            );
        }
    }

    #[test]
    fn render_src_crs_enables_a_utm_29n_source() {
        // The flagship non-default-projection COG: a Sentinel-2 stack in EPSG:32629 (UTM 29N). It is
        // not committed (~700 MB), so self-skip when it is absent — the same pattern the PRT .fgb
        // test uses. Where present: an interior window rendered with the true --src-crs EPSG:32629
        // lands on data (a fuller PNG), while declaring EPSG:3763 maps the window off the scene
        // (near-empty). This is the real proof of the feature: --src-crs renders a source the old
        // hardcoded EPSG:3763 default could never have handled.
        const S2: &str = "../cogs/s2_stack.cog.tif";
        if !cog_present(S2) {
            return;
        }
        // An interior window of the 32629 extent (UL 600000,4200000 -> LR 700080,4099920).
        let bbox = "620000,4120000,660000,4160000";
        let correct = render_window(S2, "EPSG:32629", bbox, Some("EPSG:32629"), "s2_ok");
        let wrong = render_window(S2, "EPSG:32629", bbox, Some("EPSG:3763"), "s2_bad");
        assert_ne!(
            correct, wrong,
            "--src-crs EPSG:32629 must render the UTM source where EPSG:3763 cannot"
        );
        assert!(
            correct.len() > wrong.len(),
            "the correctly-projected render should carry data (larger PNG): {} vs {}",
            correct.len(),
            wrong.len()
        );
    }
}

/// Thin WMS wrapper. Parse the WMS KVP query; handle GetMap for **1.1.1 and 1.3.0**
/// (including the EPSG:4326 axis-order flip), GetCapabilities, and one exception path;
/// delegate pixels to the engine core. Write PNG (GetMap) or XML (GetCapabilities /
/// exception) to stdout.
pub fn run_wms_handle(args: &WmsArgs) -> Result<(), Error> {
    let style = style::Style::load(&args.style)?;
    let result = wms::handle(&args.cog, &style, &args.query, None);
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    lock.write_all(&result.bytes)?;
    lock.flush()?;
    Ok(())
}

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
    /// Serve a **vector** (label) layer from a GeoJSON file instead of a `--cog` (WMS GetMap only).
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
    /// POLYGON feature to appear in an MVT tile at a given zoom. A per-feature, per-layer/per-zoom-
    /// constant selection: every tile makes the identical keep/drop decision, so it thins overview
    /// tiles WITHOUT the density seam that `--mvt-max-features` sampling causes on a complete
    /// coverage. `0` = off (default). Try `1.0` for a wall-to-wall land-cover overview. Zero-area
    /// geometries (points/lines) are exempt. Runs BEFORE `--mvt-max-features`: if the surviving
    /// polygons still exceed that budget the sampler runs too (re-introducing a seam), so set the
    /// budget high enough that selection alone bounds the tile. Applies to every vector layer.
    #[arg(long, default_value_t = 0.0)]
    pub mvt_min_feature_px: f64,
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
    if args.pmtiles_cache && (args.pmtiles.is_empty() || args.vector.is_none()) {
        return Err("--pmtiles-cache requires --pmtiles and --vector".into());
    }

    let layers = if let Some(vec_path) = &args.vector {
        // Single vector (label) layer from --vector/--vec-style/--font (WMS GetMap only).
        let vec_style = args
            .vec_style
            .as_deref()
            .ok_or("--vector needs --vec-style")?;
        let font = args
            .font
            .as_deref()
            .unwrap_or("fixtures/fonts/DejaVuSans.ttf");
        let src_crs = args
            .src_crs
            .clone()
            .unwrap_or_else(|| "EPSG:4326".to_string());
        let name = args.name.clone().unwrap_or_else(|| "vector".to_string());
        // CLI custom grids come only via --config, so an empty map here — the flags select
        // presets/`from_cog` only (mirrors the `--cog` single-layer path below).
        let no_custom_grids = std::collections::BTreeMap::new();
        let mut layer = build_vector_layer(
            name,
            vec_path,
            vec_style,
            src_crs,
            font,
            &args.tms_grids,
            args.tms_tile_px,
            &no_custom_grids,
            args,
            &s3_env,
            &args.pmtiles,
        )?;
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
                build_vector_layer(
                    lc.name.clone(),
                    vpath,
                    vstyle,
                    lc.src_crs.clone(),
                    font,
                    &lc.grids,
                    lc.tile_px,
                    &cfg.grids,
                    args,
                    &s3,
                    &lc.pmtiles,
                )?
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
                let style = style::Style::parse(style_path, &read_config_string(style_path, &s3)?)?;
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
                        Some(build_band_math(e, &names, lc.nodata)?)
                    }
                    None => None,
                };
                build_layer(
                    lc.name.clone(),
                    cog.to_string(),
                    style,
                    lc.src_crs.clone(),
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
        let style = style::Style::parse(style_path, &read_config_string(style_path, &s3_env)?)?;
        let src_crs = args
            .src_crs
            .clone()
            .unwrap_or_else(|| reproj::SRC_CRS.to_string());
        let band_math = match (&args.expression, &args.bands) {
            (Some(e), Some(spec)) => {
                let names: Vec<String> = spec.split(',').map(|s| s.trim().to_string()).collect();
                Some(build_band_math(e, &names, args.nodata)?)
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
        vec![build_layer(
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
    let max_inflight = if args.max_inflight == 0 {
        2 * std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8)
    } else {
        args.max_inflight
    };
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
        let text = read_config_string(path, &s3_env).map_err(|e| format!("--mvt-style {e}"))?;
        let val: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("--mvt-style {path}: {e}"))?;
        println!("MVT style: {path} (served at /mvt/{{layer}}/style.json)");
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
    #[arg(long, default_value_t = 0.0)]
    pub mvt_min_feature_px: f64,
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
    let src_crs = serve_args
        .src_crs
        .clone()
        .unwrap_or_else(|| "EPSG:4326".to_string());
    let layer_name = args.name.clone().unwrap_or_else(|| "vector".to_string());
    // build-pmtiles has no --config / custom-grid flags of its own; `serve_args.tms_grids` was
    // reconstructed empty above, so this mirrors the single --vector CLI path (no custom grids).
    let no_custom_grids = std::collections::BTreeMap::new();
    // build-pmtiles has no --s3-endpoint/--s3-region flags of its own (BuildPmtilesArgs carries
    // none), so env-only — same as `run_serve`'s `s3_env` before its CLI overlay is applied.
    let s3 = s3::S3Config::from_env();
    let layer = build_vector_layer(
        layer_name,
        vector_path,
        vec_style,
        src_crs,
        font,
        &serve_args.tms_grids,
        serve_args.tms_tile_px,
        &no_custom_grids,
        &serve_args,
        &s3,
        &serve_args.pmtiles,
    )?;

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
    let grid = match tms::preset(&args.grid, 4096) {
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
    let counts = vector::pmtiles::generate::build_pmtiles(
        &layer,
        &opts,
        &grid,
        args.min_zoom,
        args.max_zoom,
        bbox,
        std::path::Path::new(&args.out),
        &tmp,
    )?;
    println!(
        "build-pmtiles: {} -> addressed {} · entries {} · contents {} · {} bytes ({:.1}x dedup)",
        args.out,
        counts.addressed,
        counts.entries,
        counts.contents,
        counts.bytes,
        counts.addressed as f64 / counts.contents.max(1) as f64,
    );
    Ok(())
}

/// Read a small startup config asset (style / font / mvt-style) from a local path OR an `s3://`
/// URL. Whole-object fetch (not windowed) — S3 uses one no-Range GET.
pub(crate) fn read_config_bytes(path: &str, s3: &s3::S3Config) -> Result<Vec<u8>, Error> {
    if s3::is_s3_url(path) {
        Ok(s3::S3RangeSource::open(path, s3)
            .map_err(|e| format!("{path}: {e}"))?
            .read_whole()
            .map_err(|e| format!("{path}: {e}"))?)
    } else {
        std::fs::read(path).map_err(|e| format!("{path}: {e}").into())
    }
}

pub(crate) fn read_config_string(path: &str, s3: &s3::S3Config) -> Result<String, Error> {
    if s3::is_s3_url(path) {
        String::from_utf8(read_config_bytes(path, s3)?)
            .map_err(|e| format!("{path}: not valid UTF-8: {e}").into())
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}").into())
    }
}

/// Compile a band-math expression against band names in physical order.
fn build_band_math(
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
fn build_layer(
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
        overlay: std::collections::BTreeMap::new(),
    })
}

/// Build a **vector** (label) layer: parse the source once (GeoJSON, or a native GeoPackage when
/// `geojson_path` ends in `.gpkg`), load the vec-style + font, derive bounds from the feature
/// extent. Served over WMS GetMap (always) plus, when `grid_ids` resolves any, tiled MVT/WMTS
/// (`grid_ids`/`tile_px`/`custom_grids` mirror `build_layer`'s raster grid-publishing params).
#[allow(clippy::too_many_arguments)]
fn build_vector_layer(
    name: String,
    geojson_path: &str,
    vec_style_path: &str,
    src_crs: String,
    font_path: &str,
    grid_ids: &[String],
    tile_px: u32,
    custom_grids: &std::collections::BTreeMap<String, config::GridConfig>,
    args: &ServeArgs,
    s3: &s3::S3Config,
    pmtiles_paths: &[String],
) -> Result<server::Layer, Error> {
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
    // Flag-consistency: --topology-dissolve-rollup only means something when --topology-dissolve is
    // also set (it rolls up the dissolve field's class codes). Warn on the silent no-op rather than
    // ignoring it, matching the --mvt-cell-field / validate_cell_flags pattern.
    if args.topology_dissolve_rollup.is_some() && args.topology_dissolve.is_none() {
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
    // format entirely. `postgis://` lands here today; supporting it means adding a branch, not
    // another extension test.
    if let vector::uri::SourceKind::Unsupported(scheme) = vector::uri::classify(geojson_path) {
        return Err(format!(
            "layer '{name}': unsupported vector source scheme `{scheme}://` in {geojson_path}.              Supported: a local path or `s3://` pointing at .fgb, .gpkg or .geojson."
        )
        .into());
    }
    let style =
        vector::style::Style::parse(vec_style_path, &read_config_string(vec_style_path, s3)?)?;
    let font = read_config_bytes(font_path, s3).map_err(|e| format!("font {e}"))?;
    let shaper = std::sync::Arc::new(vector::shape::Shaper::from_font_bytes(&font)?);

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
        if args.topology_simplify.is_some()
            || args.topology_dissolve.is_some()
            || args.keep_fields.is_some()
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
        let resolved_crs = if args.src_crs.is_none() {
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
        println!(
            "layer '{name}': vector (windowed .fgb)  bounds W {:.4} S {:.4} E {:.4} N {:.4}  (WMS GetMap only)",
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
                source,
                style,
                shaper,
                lod: None,
            }),
            pmtiles: pmtiles.clone(),
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
    ) && args.topology_simplify.is_none()
        && args.topology_dissolve.is_none()
        && args.keep_fields.is_none()
        && vector::gpkg::gpkg_has_rtree(geojson_path, None);
    if windowed_gpkg {
        let gpkg = vector::gpkg::GpkgWindowedSource::open(geojson_path, None)
            .map_err(|e| format!("gpkg {geojson_path}: {e}"))?;
        // CRS precedence mirrors the load-all `.gpkg` arm below: an explicit `--src-crs` always
        // wins; only when the operator did NOT pass one do we adopt the gpkg's own detected CRS.
        let resolved_crs = if args.src_crs.is_none() {
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
        println!(
            "layer '{name}': vector (windowed .gpkg)  bounds W {:.4} S {:.4} E {:.4} N {:.4}  (WMS GetMap only)",
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
                source,
                style,
                shaper,
                lod: None,
            }),
            pmtiles: pmtiles.clone(),
            overlay: std::collections::BTreeMap::new(),
        });
    }

    let (src, src_crs): (std::sync::Arc<dyn FeatureSource>, String) = if matches!(
        vector::uri::classify(geojson_path),
        vector::uri::SourceKind::GeoPackage
    ) {
        let g = vector::gpkg::GpkgSource::load(geojson_path, None)?;
        let crs = if args.src_crs.is_none() {
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
        let gsrc: std::sync::Arc<dyn FeatureSource> = match &args.keep_fields {
            Some(csv) => {
                let mut keep: std::collections::HashSet<String> = csv
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if let Some(f) = &args.topology_dissolve {
                    keep.insert(f.clone());
                }
                // Reads through the `VectorSource` seam (windowed-seam refactor): `g` isn't needed
                // again in this arm (the `None` arm below is the one that keeps it), so it's moved
                // into the wrapper — a whole-file read, hence `full_extent()` (LoadAll ignores it
                // anyway; `Windowed` doesn't exist yet, so this is behavior-identical).
                let g_vs = vector::source::VectorSource::LoadAll(std::sync::Arc::new(g));
                let pruned: Vec<vector::feature::Feature> = g_vs
                    .features_in(g_vs.full_extent())
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
        let dissolved: Option<Vec<vector::feature::Feature>> = match &args.topology_dissolve {
            Some(field) => {
                validate_tolerance(args.snap_tolerance)?;
                // Reject a typo'd field early: otherwise EVERY feature takes the null/missing
                // pass-through arm, dissolve is a silent no-op, and the un-dissolved 842k-feature
                // coverage feeds the heavy topology build — the exact path the operator meant to avoid.
                if !gsrc_vs
                    .features_in(gsrc_vs.full_extent())
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
                    gsrc_vs.features_in(gsrc_vs.full_extent()).as_slice(),
                    field,
                    args.snap_tolerance,
                    args.topology_dissolve_rollup,
                );
                eprintln!(
                    "topology-dissolve '{field}': {} features -> {} class regions",
                    gsrc_vs.features_in(gsrc_vs.full_extent()).as_slice().len(),
                    feats.len()
                );
                Some(feats)
            }
            None => None,
        };
        let src: std::sync::Arc<dyn FeatureSource> = if let Some(tol_len) = args.topology_simplify {
            let snap = args.snap_tolerance;
            // Guard the new consumers of these tolerances (mirrors run_build_topology): a zero/negative/
            // NaN snap collapses every vertex to the origin (silent empty map); a negative simplify
            // tolerance squares to a positive, huge min_area (silent over-simplify).
            validate_tolerance(snap)?;
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
            let gsrc_batch = gsrc_vs.features_in(gsrc_vs.full_extent());
            let base_features: &[vector::feature::Feature] = dissolved
                .as_deref()
                .unwrap_or_else(|| gsrc_batch.as_slice());
            let (topo, report) = vector::topology::build_topology(base_features, snap);
            eprintln!("{}", format_report(&report));
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
        if args.topology_simplify.is_some()
            || args.topology_dissolve.is_some()
            || args.keep_fields.is_some()
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
    println!(
        "layer '{name}': vector ({} features)  bounds W {:.4} S {:.4} E {:.4} N {:.4}  (WMS GetMap only)",
        src_vs.features_in(src_vs.full_extent()).as_slice().len(),
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
            source,
            style,
            shaper,
            lod,
        }),
        pmtiles,
        overlay: std::collections::BTreeMap::new(),
    })
}

/// `build-topology` arguments — a diagnostic subcommand (SP1 task 6): load a vector coverage,
/// build the shared-arc topology, print a report. No tiles, no storage, no serving.
#[derive(Args)]
pub struct BuildTopologyArgs {
    /// Path to the vector coverage (.gpkg, .fgb or .geojson) to build shared-arc topology from.
    #[arg(long)]
    pub vector: String,
    /// Optional layer name (GeoPackage only; ignored for a single-layer .fgb or .geojson); default =
    /// the source's auto-detected layer.
    #[arg(long)]
    pub layer: Option<String>,
    /// Snap tolerance in source-CRS units. Fine default leaves a clean coverage untouched.
    #[arg(long, default_value_t = 0.01)]
    pub snap_tolerance: f64,
    /// Run the round-trip oracle (`Topology::verify_roundtrip`) after building and print the
    /// mismatch count — 0 = perfect round-trip. Makes the spec's primary correctness oracle
    /// runnable on real data, not just fixtures.
    #[arg(long)]
    pub verify: bool,
}

/// Reject a non-positive or non-finite `--snap-tolerance` before touching the filesystem. Pure
/// (no file I/O) so it's unit-testable without a gpkg fixture. `!(tol > 0.0 && tol.is_finite())`
/// rejects zero, negative, NaN (every comparison with NaN is false), AND +inf (which passes `> 0.0`
/// but snaps all coordinates to 0 → the coverage collapses to the origin with no error).
fn validate_tolerance(tol: f64) -> Result<(), String> {
    if !(tol > 0.0 && tol.is_finite()) {
        return Err(format!(
            "--snap-tolerance must be a positive finite number, got {tol}"
        ));
    }
    Ok(())
}

fn format_report(rep: &vector::topology::BuildReport) -> String {
    format!(
        "arc-topology build report\n\
         \x20 features in : {}\n\
         \x20 rings in    : {}\n\
         \x20 arcs        : {}  (shared {} · boundary {})\n\
         \x20 junctions   : {}\n\
         \x20 vertices    : {} → {} (after snap)\n\
         \x20 dropped     : {} degenerate rings · {} non-finite verts\n\
         \x20 area delta  : {:.3} (world units²)\n\
         \x20 warnings    : {}",
        rep.features_in,
        rep.rings_in,
        rep.arcs,
        rep.shared_arcs,
        rep.boundary_arcs,
        rep.junctions,
        rep.vertices_in,
        rep.vertices_after_snap,
        rep.degenerate_rings_dropped,
        rep.nonfinite_dropped,
        rep.total_abs_area_delta,
        rep.warnings.len(),
    )
}

/// Load a coverage for `build-topology` as a `VectorSource`. A `.gpkg` is read whole (`LoadAll`);
/// a `.fgb` opens the windowed reader (`Windowed`) and the caller reads it across its full extent.
fn load_topology_source(
    path: &str,
    layer: Option<&str>,
) -> Result<vector::source::VectorSource, Error> {
    let kind = vector::uri::classify(path);
    if let vector::uri::SourceKind::Unsupported(scheme) = &kind {
        return Err(format!(
            "unsupported vector source scheme `{scheme}://` in {path}.              Supported: a local path or `s3://` pointing at .fgb, .gpkg or .geojson."
        )
        .into());
    }
    if kind == vector::uri::SourceKind::FlatGeoBuf {
        if layer.is_some() {
            eprintln!(
                "warning: --layer is ignored for a FlatGeoBuf source ({path}); it has a single layer"
            );
        }
        // `.fgb` opens through `s3::AnySource` (local path or `s3://`), exactly as the serve path
        // does, then wraps the windowed `FgbSource` as `VectorSource::Windowed`. The caller reads it
        // across its full extent, so the whole coverage feeds the topology build.
        let s3 = s3::S3Config::from_env();
        let range = s3::AnySource::open(path, &s3).map_err(|e| format!("open {path}: {e}"))?;
        let fgb = vector::fgb::FgbSource::open(range).map_err(|e| format!("fgb {path}: {e}"))?;
        return Ok(vector::source::VectorSource::Windowed(std::sync::Arc::new(
            fgb,
        )));
    }
    if kind == vector::uri::SourceKind::GeoJson {
        if layer.is_some() {
            eprintln!("warning: --layer is ignored for a GeoJSON source ({path})");
        }
        // GeoJSON is a whole-file read (no spatial index), so LoadAll like a `.gpkg`.
        let src = vector::geojson::GeoJsonSource::load(path)?;
        return Ok(vector::source::VectorSource::LoadAll(std::sync::Arc::new(
            src,
        )));
    }
    let src = vector::gpkg::GpkgSource::load(path, layer)?;
    Ok(vector::source::VectorSource::LoadAll(std::sync::Arc::new(
        src,
    )))
}

/// Load a vector coverage, build its shared-arc topology, and print the diagnostic report.
/// No tiles, no storage, no serving — SP1 is unwired to serving by design.
pub fn run_build_topology(args: &BuildTopologyArgs) -> Result<(), Error> {
    // `Error` is `Box<dyn std::error::Error>`, which has `From<String>`, so `?` converts both the
    // `validate_tolerance` and `GpkgSource::load` `Result<_, String>`s directly (same as `run_serve`
    // line ~625).
    validate_tolerance(args.snap_tolerance)?;
    // `.gpkg`/`.geojson` load whole (LoadAll); a `.fgb` opens the windowed reader (Windowed). Either
    // way `vs` is read via `full_extent()` below (LoadAll ignores the bbox; Windowed queries the
    // whole R-tree), so the topology build sees every feature.
    let vs = load_topology_source(&args.vector, args.layer.as_deref())?;
    let (topo, rep) = vector::topology::build_topology(
        vs.features_in(vs.full_extent()).as_slice(),
        args.snap_tolerance,
    );
    println!("{}", format_report(&rep));
    for w in &rep.warnings {
        eprintln!("warning: {w}");
    }
    if args.verify {
        let mismatched = topo.verify_roundtrip(
            vs.features_in(vs.full_extent()).as_slice(),
            args.snap_tolerance,
        );
        println!(
            "round-trip: {mismatched} / {} features mismatched",
            rep.features_in
        );
    }
    Ok(())
}

#[cfg(test)]
mod build_topology_cli_tests {
    /// An unknown SCHEME must be rejected by name, up front. Before the URI registry it fell
    /// through every extension test to the GeoPackage opener and failed inside SQLite,
    /// reporting a file problem for something that was never a file. This is also the seam a
    /// PostGIS reader attaches to, so the message must name what IS supported.
    #[test]
    fn unsupported_scheme_is_rejected_by_name() {
        // `match`, not `expect_err`: the Ok type (`VectorSource`) is not `Debug`, by design -
        // it holds trait objects.
        let msg = match super::load_topology_source("postgis://user@host/db?table=roads", None) {
            Ok(_) => panic!("an unknown scheme must not reach a file opener"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("postgis://"), "must name the scheme: {msg}");
        assert!(
            msg.contains(".fgb") && msg.contains(".gpkg"),
            "must say what IS supported: {msg}"
        );
    }

    use super::*;
    use crate::vector::topology::BuildReport;

    #[test]
    fn format_report_lists_the_key_counts() {
        let mut r = BuildReport::default();
        r.features_in = 3;
        r.arcs = 7;
        r.shared_arcs = 4;
        r.boundary_arcs = 3;
        r.junctions = 5;
        let s = format_report(&r);
        assert!(s.contains("features") && s.contains("3"));
        assert!(s.contains("shared") && s.contains("4"));
        assert!(s.contains("boundary") && s.contains("3"));
        assert!(s.contains("arcs") && s.contains("7"));
    }

    #[test]
    fn validate_tolerance_rejects_zero() {
        assert!(validate_tolerance(0.0).is_err());
    }

    #[test]
    fn validate_tolerance_rejects_negative() {
        assert!(validate_tolerance(-1.0).is_err());
    }

    #[test]
    fn validate_tolerance_rejects_nan() {
        assert!(validate_tolerance(f64::NAN).is_err());
    }

    #[test]
    fn validate_tolerance_rejects_infinity() {
        // +inf snaps every finite coordinate to 0 (coverage collapses to the origin) → must reject,
        // as the message promises "finite".
        assert!(validate_tolerance(f64::INFINITY).is_err());
    }

    #[test]
    fn validate_tolerance_accepts_positive() {
        assert!(validate_tolerance(0.01).is_ok());
    }

    #[test]
    fn load_topology_source_dispatches_by_extension() {
        use crate::vector::source::VectorSource;
        // A `.gpkg` reads whole; a `.fgb` opens the windowed reader.
        let g = load_topology_source("fixtures/gpkg/mini.gpkg", None).expect("load gpkg");
        assert!(matches!(g, VectorSource::LoadAll(_)), "gpkg → LoadAll");
        let f = load_topology_source("fixtures/fgb/hole.fgb", None).expect("load fgb");
        assert!(matches!(f, VectorSource::Windowed(_)), "fgb → Windowed");
        let j = load_topology_source("fixtures/fgb/hole.geojson", None).expect("load geojson");
        assert!(matches!(j, VectorSource::LoadAll(_)), "geojson → LoadAll");
    }

    #[test]
    fn build_topology_reads_a_flatgeobuf_coverage() {
        // hole.fgb is one polygon: a 10×10 square exterior ring with a 4×4 square hole cut out —
        // so 1 feature and 2 rings. Building topology over the FlatGeoBuf's full extent must see
        // both rings and produce at least one arc (proving features were actually decoded, not an
        // empty read behind a 200).
        let vs = load_topology_source("fixtures/fgb/hole.fgb", None).expect("load fgb");
        let feats = vs.features_in(vs.full_extent());
        let (_topo, rep) = crate::vector::topology::build_topology(feats.as_slice(), 0.01);
        assert_eq!(rep.features_in, 1, "one polygon feature");
        assert_eq!(rep.rings_in, 2, "exterior + hole");
        assert!(
            rep.arcs >= 1,
            "topology built at least one arc, got {}",
            rep.arcs
        );
    }

    #[test]
    fn build_topology_reads_a_geojson_coverage() {
        // hole.geojson is the same donut as hole.fgb: one polygon, exterior + hole = 2 rings. The
        // GeoJSON path must build the identical topology (1 feature / 2 rings / >=1 arc).
        let vs = load_topology_source("fixtures/fgb/hole.geojson", None).expect("load geojson");
        let feats = vs.features_in(vs.full_extent());
        let (_topo, rep) = crate::vector::topology::build_topology(feats.as_slice(), 0.01);
        assert_eq!(rep.features_in, 1, "one polygon feature");
        assert_eq!(rep.rings_in, 2, "exterior + hole");
        assert!(
            rep.arcs >= 1,
            "topology built at least one arc, got {}",
            rep.arcs
        );
    }
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
    /// explicit, since `ServeArgs` derives no `Default`.
    fn base_serve_args() -> ServeArgs {
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
            "mini".to_string(),
            MINI,
            VEC_STYLE,
            "EPSG:4326".to_string(),
            FONT,
            &[],
            512,
            &std::collections::BTreeMap::new(),
            &args,
            &s3,
            &[],
        )
        .unwrap();
        let vector = layer.vector.expect("vector layer");
        assert!(
            matches!(vector.source, VectorSource::Windowed(_)),
            "an rtree-indexed .gpkg with no load-all-only flags must dispatch to the windowed \
             seam"
        );
    }

    #[test]
    fn topology_simplify_falls_through_to_load_all_even_with_an_rtree() {
        let mut args = base_serve_args();
        args.topology_simplify = Some(1.0);
        let s3 = crate::s3::S3Config::from_env();
        let layer = build_vector_layer(
            "mini".to_string(),
            MINI,
            VEC_STYLE,
            "EPSG:4326".to_string(),
            FONT,
            &[],
            512,
            &std::collections::BTreeMap::new(),
            &args,
            &s3,
            &[],
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
            "mini".to_string(),
            MINI,
            VEC_STYLE,
            "EPSG:4326".to_string(),
            FONT,
            &[],
            512,
            &std::collections::BTreeMap::new(),
            &args,
            &s3,
            &[],
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
            "mini".to_string(),
            MINI,
            VEC_STYLE,
            "EPSG:4326".to_string(),
            FONT,
            &[],
            512,
            &std::collections::BTreeMap::new(),
            &args,
            &s3,
            &[],
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
        let layer = build_vector_layer(
            "mini".to_string(),
            MINI,
            VEC_STYLE,
            "EPSG:4326".to_string(),
            FONT,
            &grid_ids,
            512,
            &custom,
            &args,
            &s3,
            &[],
        )
        .unwrap();
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
}

/// `read_config_{bytes,string}` — the s3:// / local dispatch used to load style/font/mvt-style
/// config assets on the serve path (Task 2).
#[cfg(test)]
mod read_config_tests {
    #[test]
    fn read_config_string_local_matches_fs() {
        // A local path must load byte-for-byte the same via read_config_string as via std::fs.
        let path = "fixtures/styles/rgb.json"; // an existing committed style fixture
        let s3 = crate::s3::S3Config::from_env();
        let via_helper = crate::read_config_string(path, &s3).expect("read_config_string local");
        let via_fs = std::fs::read_to_string(path).expect("fs read");
        assert_eq!(via_helper, via_fs);
    }

    #[test]
    fn read_config_string_missing_local_path_errors_with_path() {
        let path = "fixtures/does/not/exist-read-config-string.json";
        let s3 = crate::s3::S3Config::default();
        let err = crate::read_config_string(path, &s3)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(path),
            "expected the path in the error, got: {err}"
        );
    }

    #[test]
    fn read_config_bytes_missing_local_path_errors_with_path() {
        let path = "fixtures/does/not/exist-read-config-bytes.bin";
        let s3 = crate::s3::S3Config::default();
        let err = crate::read_config_bytes(path, &s3).unwrap_err().to_string();
        assert!(
            err.contains(path),
            "expected the path in the error, got: {err}"
        );
    }

    #[test]
    fn read_config_bytes_missing_local_path_error_has_no_verb_prefix() {
        // Pins the "{path}: {cause}" format (no "open "/"read " verb) — the message must
        // start with the path itself, not a verb.
        let path = "fixtures/does/not/exist-no-verb.bin";
        let s3 = crate::s3::S3Config::default();
        let err = crate::read_config_bytes(path, &s3).unwrap_err().to_string();
        assert!(
            err.starts_with(&format!("{path}: ")),
            "expected '{path}: ...', got: {err}"
        );
    }

    #[test]
    fn read_config_bytes_s3_without_endpoint_errors_before_any_network_call() {
        // S3Config::default() has NO endpoint, so S3RangeSource::open must fail on
        // validation before issuing any HTTP request — proves the s3 arm surfaces a
        // clean, path-prefixed error with no network I/O. Runs unconditionally (no
        // TERRASERVE_S3_TEST_URL / creds needed).
        let path = "s3://some-bucket/some/key.json";
        let s3 = crate::s3::S3Config::default();
        let err = crate::read_config_bytes(path, &s3).unwrap_err().to_string();
        assert!(
            err.starts_with(&format!("{path}: ")),
            "expected '{path}: ...', got: {err}"
        );
        assert!(
            err.contains("missing S3 endpoint"),
            "expected the missing-endpoint cause, got: {err}"
        );
    }

    #[test]
    fn font_prefix_composes_with_read_config_bytes_error() {
        // The "font " prefix is added at the build_vector_layer call site, which needs a
        // live serve() to reach — so this pins the composition directly, the way run_serve
        // actually builds it: format!("font {e}") over read_config_bytes's error.
        let path = "/no/such/font.ttf";
        let s3 = crate::s3::S3Config::default();
        let composed = format!("font {}", crate::read_config_bytes(path, &s3).unwrap_err());
        assert!(
            composed.contains(&format!("font {path}:")),
            "expected 'font {path}: ...', got: {composed}"
        );
    }

    #[test]
    fn mvt_style_prefix_composes_with_read_config_string_error() {
        // Same idea for the --mvt-style call site in run_serve.
        let path = "/no/such/style.json";
        let s3 = crate::s3::S3Config::default();
        let composed = format!(
            "--mvt-style {}",
            crate::read_config_string(path, &s3).unwrap_err()
        );
        assert!(
            composed.contains(&format!("--mvt-style {path}:")),
            "expected '--mvt-style {path}: ...', got: {composed}"
        );
    }
}
