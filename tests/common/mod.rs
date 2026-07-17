//! Shared helpers for the correctness oracle test-rig. Included per test binary via `mod common;`.
//! The alignment core (ZNCC + parabolic sub-pixel) is pure Rust and GDAL-free; GDAL oracle helpers
//! are added alongside and self-skip when the tools are absent.
#![allow(dead_code)]

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// True if the GDAL CLI oracle (`gdalwarp`) is on PATH. Oracle tests self-skip when it isn't, so the
/// fast `cargo test` path stays green on a lean/CI checkout without GDAL.
pub fn gdal_available() -> bool {
    Command::new("gdalwarp")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn unique_tmp(ext: &str) -> std::path::PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("ts_oracle_{}_{}.{ext}", std::process::id(), n))
}

/// GDAL oracle warp to a temp ESRI ASCII grid, parsed back to a row-major `Vec<f32>` (north row
/// first — same as TerraServe; nodata → `NaN`). `bbox` = `[minx, miny, maxx, maxy]` in `dst_crs`
/// units (easting/northing order). Single-band only (AAIGrid); use a 1-band COG (e.g. the DEM).
///
/// Oracle rigor (per the Fable review): `-et 0` (EXACT transformer — GDAL's default 0.125 px
/// approximation would be self-noise of the same order as the signal); `-ovr NONE` (read full-res,
/// so overview selection can't diverge from TerraServe's); `-s_srs {src_crs}` (both engines
/// transform from the SAME source-CRS interpretation — TerraServe is told `src_crs` explicitly).
/// `resample` should match the TerraServe render ("near" | "bilinear").
pub fn gdalwarp_grid(
    cog: &str,
    src_crs: &str,
    bbox: [f64; 4],
    dst_crs: &str,
    w: usize,
    h: usize,
    resample: &str,
) -> Option<Vec<f32>> {
    let out = unique_tmp("asc");
    let status = Command::new("gdalwarp")
        .args(["-q", "-overwrite", "-et", "0", "-ovr", "NONE"])
        .args(["-s_srs", src_crs, "-t_srs", dst_crs, "-te"])
        .args(bbox.map(|v| format!("{v:.6}")))
        .args([
            "-ts",
            &w.to_string(),
            &h.to_string(),
            "-r",
            resample,
            "-of",
            "AAIGrid",
        ])
        .args([cog, out.to_str()?])
        .status()
        .ok()?;
    if !status.success() {
        let _ = std::fs::remove_file(&out);
        return None;
    }
    let text = std::fs::read_to_string(&out).ok()?;
    // Clean up the AAIGrid + its sidecars (.prj, .asc.aux.xml).
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(out.with_extension("prj"));
    let _ = std::fs::remove_file(out.with_extension("asc.aux.xml"));

    // Header lines (ncols/nrows/xll/yll/cellsize | DX/DY /NODATA_value) start with a NON-numeric
    // keyword; data rows start with a number. This is robust to both the standard `cellsize` and the
    // "Golden Surfer" DX/DY header GDAL emits for non-square pixels.
    let mut nodata = f32::NAN;
    let mut vals: Vec<f32> = Vec::with_capacity(w * h);
    for line in text.lines() {
        let first = match line.split_whitespace().next() {
            Some(t) => t,
            None => continue,
        };
        if first.parse::<f32>().is_ok() {
            // Data row.
            for tok in line.split_whitespace() {
                if let Ok(v) = tok.parse::<f32>() {
                    vals.push(if v == nodata { f32::NAN } else { v });
                }
            }
        } else if first.eq_ignore_ascii_case("nodata_value") {
            nodata = line
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(f32::NAN);
        }
        // else: another header line (ncols/nrows/xll/yll/cellsize/dx/dy) -> skip.
    }
    if vals.len() != w * h {
        return None; // unexpected shape -> treat as oracle failure, not a silent pass
    }
    Some(vals)
}

/// GDAL oracle point value: `gdallocationinfo -valonly -geoloc {cog} {x} {y}` where `(x,y)` are in
/// the DATASET's own CRS. `None` if GDAL fails or the point is outside/nodata.
pub fn gdal_value_at(cog: &str, x: f64, y: f64) -> Option<f64> {
    let out = Command::new("gdallocationinfo")
        .args([
            "-valonly",
            "-geoloc",
            cog,
            &format!("{x:.6}"),
            &format!("{y:.6}"),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .ok()
}

/// RGBA → luminance (Rec.601). A fully-transparent pixel (alpha 0) → `NaN` so ZNCC ignores it.
pub fn to_luma(rgba: &[u8], w: usize, h: usize) -> Vec<f32> {
    let mut o = vec![0f32; w * h];
    for i in 0..w * h {
        let p = &rgba[i * 4..i * 4 + 4];
        o[i] = if p[3] == 0 {
            f32::NAN
        } else {
            0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32
        };
    }
    o
}

/// Bilinear-resample `src` translated by `(dx, dy)`: `out[x,y] = src[x-dx, y-dy]`. For the
/// detector self-test (inject a known sub-pixel shift). Out-of-bounds → `NaN`.
pub fn gray_shift(src: &[f32], w: usize, h: usize, dx: f64, dy: f64) -> Vec<f32> {
    let sample = |x: f64, y: f64| -> f32 {
        if x < 0.0 || y < 0.0 || x >= (w - 1) as f64 || y >= (h - 1) as f64 {
            return f32::NAN;
        }
        let (x0f, y0f) = (x.floor(), y.floor());
        let (fx, fy) = ((x - x0f) as f32, (y - y0f) as f32);
        let (x0, y0) = (x0f as usize, y0f as usize);
        let g = |xx: usize, yy: usize| src[yy * w + xx];
        let top = g(x0, y0) * (1.0 - fx) + g(x0 + 1, y0) * fx;
        let bot = g(x0, y0 + 1) * (1.0 - fx) + g(x0 + 1, y0 + 1) * fx;
        top * (1.0 - fy) + bot * fy
    };
    let mut o = vec![f32::NAN; w * h];
    for y in 0..h {
        for x in 0..w {
            o[y * w + x] = sample(x as f64 - dx, y as f64 - dy);
        }
    }
    o
}

/// Zero-normalized cross-correlation of `a[x,y]` against `b[x-sx, y-sy]` over the mutually-valid
/// (non-NaN) overlap. Intensity-invariant, so different value ranges / colormaps don't fake a peak.
fn zncc(a: &[f32], b: &[f32], w: usize, h: usize, sx: i32, sy: i32) -> f64 {
    let mut pairs: Vec<(f32, f32)> = Vec::new();
    let (mut sa, mut sb) = (0f64, 0f64);
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let (bx, by) = (x - sx, y - sy);
            if bx < 0 || by < 0 || bx >= w as i32 || by >= h as i32 {
                continue;
            }
            let av = a[y as usize * w + x as usize];
            let bv = b[by as usize * w + bx as usize];
            if av.is_nan() || bv.is_nan() {
                continue;
            }
            sa += av as f64;
            sb += bv as f64;
            pairs.push((av, bv));
        }
    }
    let n = pairs.len() as f64;
    if n < 64.0 {
        return f64::NEG_INFINITY;
    }
    let (ma, mb) = (sa / n, sb / n);
    let (mut num, mut da, mut db) = (0f64, 0f64, 0f64);
    for (av, bv) in pairs {
        let (ca, cb) = (av as f64 - ma, bv as f64 - mb);
        num += ca * cb;
        da += ca * ca;
        db += cb * cb;
    }
    if da <= 0.0 || db <= 0.0 {
        return f64::NEG_INFINITY;
    }
    num / (da.sqrt() * db.sqrt())
}

/// The result of `zncc_shift`: the sub-pixel `(dx, dy)` shift, the **peak ZNCC** at that offset, and
/// whether the integer peak landed on the search-window boundary. Both `peak` (reliability) and
/// `on_edge` (true shift likely OUTSIDE the window) are false-confidence guards: a caller must reject
/// a low `peak` OR an `on_edge` peak before believing `(dx, dy)`, else the rig can report "shift ≈ 0"
/// while actually mis-aligned — the worst failure of a correctness oracle.
pub struct Shift {
    pub dx: f64,
    pub dy: f64,
    pub peak: f64,
    pub on_edge: bool,
}

/// Best translational shift by spatial ZNCC (NOT FFT phase correlation) such that `a[x,y] ≈
/// b[x-dx, y-dy]`: integer ZNCC peak over `±search`, then parabolic sub-pixel refinement. Returns
/// `None` if the surface is degenerate (no valid overlap / flat).
pub fn zncc_shift(a: &[f32], b: &[f32], w: usize, h: usize, search: i32) -> Option<Shift> {
    let at = |sx: i32, sy: i32| zncc(a, b, w, h, sx, sy);
    let (mut best, mut bx, mut by) = (f64::NEG_INFINITY, 0i32, 0i32);
    for sy in -search..=search {
        for sx in -search..=search {
            let c = at(sx, sy);
            if c > best {
                best = c;
                bx = sx;
                by = sy;
            }
        }
    }
    if !best.is_finite() {
        return None;
    }
    let on_edge = bx.abs() == search || by.abs() == search;
    // Parabolic sub-pixel refinement in x and y around the integer peak.
    let refine = |m: f64, l: f64, r: f64| -> f64 {
        if !l.is_finite() || !r.is_finite() {
            return 0.0;
        }
        let den = l - 2.0 * m + r;
        if den.abs() < 1e-9 {
            0.0
        } else {
            (0.5 * (l - r) / den).clamp(-0.5, 0.5)
        }
    };
    let dx = bx as f64 + refine(best, at(bx - 1, by), at(bx + 1, by));
    let dy = by as f64 + refine(best, at(bx, by - 1), at(bx, by + 1));
    Some(Shift {
        dx,
        dy,
        peak: best,
        on_edge,
    })
}
