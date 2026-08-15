// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! The `render` and `wms-handle` subcommands: the two one-shot verbs.
//!
//! Both write a single result and exit — a PNG for `render`, a PNG or an XML document for
//! `wms-handle` — with no server, no cache and no shared state. They share a file because
//! `run_wms_handle` is a twelve-line wrapper over the same engine core `run_render` drives; it
//! parses a WMS KVP query instead of flags and writes to stdout instead of a path.
//!
//! ⚠ **FROZEN.** `score.sh` invokes `terraserve render …` and `terraserve wms-handle …` by these
//! exact flag names. Nothing here may be renamed or reshaped.

use crate::wms;
use crate::Error;
use crate::{backend, cache, pngio, render, reproj, style};
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

pub(crate) fn parse_bbox(s: &str) -> Result<[f64; 4], Error> {
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
