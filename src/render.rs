// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Render pipeline orchestration: parse COG -> map output grid to source coords (via
//! PROJ) -> pick overview level -> decode covering tiles (batch) -> assemble a source
//! window -> warp/resample -> style. Keeps `decode_tiles` / `warp` / `colorize` isolated
//! behind the `RenderBackend` trait.

use std::sync::Arc;

use rayon::prelude::*;

use crate::backend::{CompressedTile, CpuBackend, DeviceBuffer, RenderBackend, Resample, WarpMap};
use crate::cache::{IndexCache, TileCache};
use crate::cog::{self, Cog, RangeSource};
use crate::decode;
use crate::expr::Program;
use crate::reproj::Transformer;
use crate::s3::{AnySource, S3Config};
use crate::style::{self, Style};

/// Threads in the dedicated I/O pool for blocking range reads. I/O threads mostly *wait* on
/// the network, so this is sized well above the CPU count — the goal is many S3 range requests
/// in flight, not CPU parallelism. Tunable via `TERRASERVE_IO_THREADS`; the default (32) matches
/// the S3 connection pool (`s3::S3RangeSource` idle connections), so reads don't churn sockets.
pub fn io_concurrency() -> usize {
    std::env::var("TERRASERVE_IO_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(32)
}

/// The dedicated I/O pool (built once, lazily). Tile *reads* run here via `install()`; decode /
/// warp / colorize stay on rayon's default (CPU) pool. This keeps a request's blocking network
/// reads from occupying the ~cores−2 CPU threads and starving decode of the same fanned-out
/// request — the two kinds of work no longer compete for the same threads.
fn io_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(io_concurrency())
            .thread_name(|i| format!("terraserve-io-{i}"))
            .build()
            .expect("build TerraServe I/O pool")
    })
}

/// Band-math spec for a layer: a compiled expression over the COG's bands plus the source
/// nodata value. When set on a request, the render pipeline computes the derived value
/// (e.g. NDVI) on the fly and colorizes it by value domain instead of doing RGBA passthrough.
pub struct BandMath {
    /// Compiled expression. `program.bands_used()` are the 0-based physical band indices it
    /// references (the config supplies band names in physical order).
    pub program: Program,
    /// Source nodata value; a pixel is transparent where any referenced band equals it.
    pub nodata: f64,
}

pub struct RenderRequest<'a> {
    pub cog_path: &'a str,
    pub bbox: [f64; 4], // minx, miny, maxx, maxy in `crs` units
    pub crs: &'a str,
    /// The COG's own CRS (per-layer). Defaults to `reproj::SRC_CRS` for the cascais data.
    pub src_crs: &'a str,
    pub width: u32,
    pub height: u32,
    pub resample: Resample,
    pub style: &'a Style,
    /// When set, render this layer as on-the-fly band math (e.g. NDVI) instead of RGBA.
    pub band_math: Option<&'a BandMath>,
    /// Bounded chunk cache backing `cog::Level::tile_location` for a `Lazy` tile index. Cheap to
    /// carry when unused — `moka::sync::Cache` is internally `Arc`-ed (cheap `Clone`), and an
    /// all-`Resident` COG (today's only path) never touches it.
    pub index_cache: IndexCache,
}

/// Render a window and return a flat RGBA8 buffer (width*height*4).
pub fn render(req: &RenderRequest) -> Result<Vec<u8>, String> {
    // Local file or S3 object, per the `--cog` path; S3 creds/endpoint come from the env.
    let src = AnySource::open(req.cog_path, &S3Config::from_env())
        .map_err(|e| format!("open cog {}: {e}", req.cog_path))?;
    let cog = cog::parse(&src).map_err(|e| format!("parse cog: {e}"))?;
    render_with_cog(req, &cog, &src, None)
}

/// A GetFeatureInfo query point (a pixel `(i,j)` in a `WIDTH×HEIGHT` map over `bbox` in `crs`).
pub struct InfoRequest<'a> {
    pub bbox: [f64; 4],
    pub crs: &'a str,
    pub src_crs: &'a str,
    pub width: u32,
    pub height: u32,
    pub i: u32,
    pub j: u32,
    pub band_math: Option<&'a BandMath>,
}

/// The exact value(s) at a query point. Read **losslessly (f64)** from the **native full-resolution
/// level (level 0)** — never a downsampled overview — so the reported value is the true source pixel,
/// not an average. This is where piece A's lossless `read_sample` pays off.
#[derive(Debug, Clone)]
pub struct PointInfo {
    pub in_image: bool,
    pub nodata: bool,
    pub source_col: i64,
    pub source_row: i64,
    /// Raw per-band values in physical order (exact).
    pub bands: Vec<f64>,
    /// Band-math result (`None` when there's no band math, or the point is nodata).
    pub derived: Option<f64>,
}

/// GetFeatureInfo value read: open the COG and sample one point.
pub fn sample_point(req: &InfoRequest, cog_path: &str) -> Result<PointInfo, String> {
    let src = AnySource::open(cog_path, &S3Config::from_env())
        .map_err(|e| format!("open cog {cog_path}: {e}"))?;
    let cog = cog::parse(&src).map_err(|e| format!("parse cog: {e}"))?;
    // One-shot (CLI / no persistent layer state): a fresh cache per call is correct — it's only
    // ever populated on the `Lazy` path, which a single GetFeatureInfo query touches at most a
    // couple of chunks of.
    let index_cache = crate::cache::new_index_cache(crate::cache::index_cache_bytes());
    sample_point_with_cog(req, &cog, &src, &index_cache)
}

/// GetFeatureInfo value read from an already-parsed COG (the server's parse-once seam).
pub fn sample_point_with_cog<R: RangeSource + Sync>(
    req: &InfoRequest,
    cog: &Cog,
    src: &R,
    index_cache: &IndexCache,
) -> Result<PointInfo, String> {
    let (w, h) = (req.width as usize, req.height as usize);
    if w == 0 || h == 0 {
        return Err("width/height must be > 0".into());
    }
    if req.i >= req.width || req.j >= req.height {
        return Err("I/J outside the map".into());
    }
    let out_of = |c: i64, r: i64| PointInfo {
        in_image: false,
        nodata: false,
        source_col: c,
        source_row: r,
        bands: vec![],
        derived: None,
    };
    // A pixel that IS inside the raster but whose tile carries no data (a sparse-COG hole):
    // in_image, but nodata — distinct from `out_of` so GetFeatureInfo reports "nodata", not
    // "outside coverage".
    let nodata_at = |c: i64, r: i64| PointInfo {
        in_image: true,
        nodata: true,
        source_col: c,
        source_row: r,
        bands: vec![],
        derived: None,
    };

    let transformer = Transformer::new(req.crs, req.src_crs)?;
    let [minx, miny, maxx, maxy] = req.bbox;
    let dx = (maxx - minx) / w as f64;
    let dy = (maxy - miny) / h as f64;
    // Pixel CENTER (matches the render sampling convention exactly).
    let ox = minx + (req.i as f64 + 0.5) * dx;
    let oy = maxy - (req.j as f64 + 0.5) * dy;
    let (sx, sy) = match transformer.to_source(ox, oy) {
        Some((sx, sy)) if sx.is_finite() && sy.is_finite() => (sx, sy),
        _ => return Ok(out_of(-1, -1)),
    };

    let level = &cog.levels[0]; // NATIVE resolution — the exact pixel, not an overview average.
    let (u, v) = level.geo.geo_to_pix(sx, sy);
    if !(u.is_finite() && v.is_finite()) {
        return Ok(out_of(-1, -1));
    }
    let (col, row) = (u.floor() as i64, v.floor() as i64);
    if col < 0 || row < 0 || col >= level.width as i64 || row >= level.height as i64 {
        return Ok(out_of(col, row));
    }
    let (col, row) = (col as u32, row as u32);

    // Decode the single tile containing the pixel and read its samples.
    let across = level.width.div_ceil(level.tile_w);
    let (gx, gy) = (col / level.tile_w, row / level.tile_h);
    let idx = (gy * across + gx) as usize;
    let (off, len) = match level
        .tile_location(src, index_cache, idx as u64)
        .map_err(|e| format!("tile index read: {e}"))?
    {
        Some(v) => v,
        // Absent tile (sparse COG "no data here"): col/row already passed the in-bounds check
        // above, so the pixel is INSIDE the raster — this is nodata, not "outside coverage".
        // (Mirrors render_bandmath, which treats None from tile_location as nodata.)
        None => return Ok(nodata_at(col as i64, row as i64)),
    };
    let bytes = src
        .read_range(off, len)
        .map_err(|e| format!("read tile {idx}: {e}"))?;
    let ct = CompressedTile {
        bytes,
        compression: level.compression,
        predictor: level.predictor,
        tile_w: level.tile_w,
        tile_h: level.tile_h,
        samples: level.samples_per_pixel,
        bits_per_sample: level.bits_per_sample,
        sample_format: level.sample_format,
        little_endian: cog.little_endian,
        photometric: level.photometric,
        jpeg_tables: cog.jpeg_tables.as_ref().map(|t| Arc::new(t.clone())),
        grid_col: gx,
        grid_row: gy,
        present: true,
    };
    let data = decode::decode_tile_samples(&ct);
    let spp = level.samples_per_pixel as usize;
    let bps = decode::bytes_per_sample(level.bits_per_sample);
    let (rx, ry) = ((col % level.tile_w) as usize, (row % level.tile_h) as usize);
    let sp = ry * level.tile_w as usize + rx;
    let mut bands = Vec::with_capacity(spp);
    for b in 0..spp {
        let voff = (sp * spp + b) * bps;
        bands.push(decode::read_sample(
            &data,
            voff,
            bps,
            level.sample_format,
            cog.little_endian,
        ));
    }

    // Band-math derived value + nodata (mirrors render_bandmath: nodata on any referenced raw band).
    let (derived, nodata) = match req.band_math {
        Some(bm) => {
            let mut nd = false;
            let planes: Vec<Vec<f32>> = bm
                .program
                .bands_used()
                .iter()
                .map(|&b| {
                    let val = bands.get(b).copied().unwrap_or(f64::NAN);
                    if (val - bm.nodata).abs() < 0.5 {
                        nd = true;
                    }
                    vec![val as f32]
                })
                .collect();
            if nd {
                (None, true)
            } else {
                let refs: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
                (Some(bm.program.eval(&refs, 1)[0] as f64), false)
            }
        }
        None => (None, false),
    };

    Ok(PointInfo {
        in_image: true,
        nodata,
        source_col: col as i64,
        source_row: row as i64,
        bands,
        derived,
    })
}

/// Render a window from an ALREADY-PARSED COG plus an open range source — the seam the
/// async server uses. The server parses the COG's structure once (at startup) and calls
/// this per request, so no request re-walks the IFD chain. `src` supplies the tile bytes;
/// keeping the source per-request (rather than caching it) avoids sharing its `!Sync`
/// seek cursor across blocking workers — and opening a local file is a cheap syscall.
pub fn render_with_cog<R: RangeSource + Sync>(
    req: &RenderRequest,
    cog: &Cog,
    src: &R,
    tile_cache: Option<&TileCache>,
) -> Result<Vec<u8>, String> {
    let backend = CpuBackend;

    let (w, h) = (req.width as usize, req.height as usize);
    if w == 0 || h == 0 {
        return Err("width/height must be > 0".into());
    }

    let transformer = Transformer::new(req.crs, req.src_crs)?;

    let [minx, miny, maxx, maxy] = req.bbox;
    let dx = (maxx - minx) / w as f64;
    let dy = (maxy - miny) / h as f64;
    let native = cog.levels[0].geo;

    // Pass 1: map each output pixel center to NATIVE source-pixel coordinates.
    let mut native_coords = vec![[f64::NAN; 2]; w * h];
    for j in 0..h {
        let oy = maxy - (j as f64 + 0.5) * dy;
        for i in 0..w {
            let ox = minx + (i as f64 + 0.5) * dx;
            if let Some((sx, sy)) = transformer.to_source(ox, oy) {
                if sx.is_finite() && sy.is_finite() {
                    let (u, v) = native.geo_to_pix(sx, sy);
                    native_coords[j * w + i] = [u, v];
                }
            }
        }
    }

    // Desired downsample factor: source pixels spanned per output pixel, at the center.
    let desired = desired_factor(
        &native_coords,
        w,
        h,
        &transformer,
        minx,
        miny,
        maxx,
        maxy,
        &native,
        dx,
        dy,
    );

    // Overview selection: coarsest level whose factor does not upsample (factor<=desired).
    let native_w = cog.native_width();
    let mut level_idx = 0usize;
    for (idx, lvl) in cog.levels.iter().enumerate() {
        let f = lvl.factor(native_w);
        if f <= desired * (1.0 + 1e-3) {
            level_idx = idx;
        } else {
            break;
        }
    }
    let level = &cog.levels[level_idx];
    let lw = level.width as f64;
    let lh = level.height as f64;
    let sx_ratio = lw / native_w as f64;
    let sy_ratio = lh / cog.levels[0].height as f64;

    // Pass 2: native coords -> chosen-level pixel coords; flag out-of-image as NaN.
    // Track the in-bounds source pixel bbox for the window.
    let mut level_coords = vec![[f64::NAN; 2]; w * h];
    let mut minc = f64::INFINITY;
    let mut maxc = f64::NEG_INFINITY;
    let mut minr = f64::INFINITY;
    let mut maxr = f64::NEG_INFINITY;
    let mut any = false;
    for p in 0..w * h {
        let [nu, nv] = native_coords[p];
        if !nu.is_finite() || !nv.is_finite() {
            continue;
        }
        let u = nu * sx_ratio;
        let v = nv * sy_ratio;
        if u >= 0.0 && u < lw && v >= 0.0 && v < lh {
            level_coords[p] = [u, v];
            any = true;
            if u < minc {
                minc = u;
            }
            if u > maxc {
                maxc = u;
            }
            if v < minr {
                minr = v;
            }
            if v > maxr {
                maxr = v;
            }
        }
    }

    // Fully outside the data -> transparent image.
    if !any {
        return Ok(vec![0u8; w * h * 4]);
    }

    // Source window: tile-aligned region covering the needed source pixels (+1px margin
    // for bilinear taps).
    let margin = 1.0;
    let c0 = (minc - margin).floor().max(0.0) as u32;
    let c1 = (maxc + margin).ceil().min(lw - 1.0) as u32;
    let r0 = (minr - margin).floor().max(0.0) as u32;
    let r1 = (maxr + margin).ceil().min(lh - 1.0) as u32;

    let tw = level.tile_w;
    let th = level.tile_h;
    let tiles_across = level.tiles_across();
    let tiles_down = level.tiles_down();
    let tx0 = c0 / tw;
    let tx1 = (c1 / tw).min(tiles_across.saturating_sub(1));
    let ty0 = r0 / th;
    let ty1 = (r1 / th).min(tiles_down.saturating_sub(1));

    let win_origin_x = (tx0 * tw) as f64;
    let win_origin_y = (ty0 * th) as f64;
    let win_cols = (tx1 - tx0 + 1) * tw;
    let win_rows = (ty1 - ty0 + 1) * th;

    // Band-math layers diverge here: decode numeric band planes, evaluate the expression at
    // source resolution, warp the derived value (nearest), then colorize by value domain. The
    // shared coordinate/overview/window math above is reused; only the decode→style tail differs.
    if let Some(bm) = req.band_math {
        let win = Win {
            level,
            tw,
            th,
            tiles_across,
            tx0,
            tx1,
            ty0,
            ty1,
            win_origin_x,
            win_origin_y,
            win_cols,
            win_rows,
        };
        return render_bandmath(
            req,
            cog,
            src,
            &win,
            &level_coords,
            bm,
            tile_cache,
            level_idx,
        );
    }

    // Gather the covering tiles: serve decoded tiles from the (bounded) cache on a hit,
    // and read + batch-decode only the misses. Each tile is keyed by (level, tile index),
    // unique for a single COG. Not-present tiles (past the image edge) are cheap transparent
    // buffers, made on the fly and never cached.
    let jpeg_tables = cog.jpeg_tables.as_ref().map(|t| Arc::new(t.clone()));
    let mut positions: Vec<(u32, u32)> = Vec::new();
    let mut tiles: Vec<Option<Arc<DeviceBuffer>>> = Vec::new();
    // Pass 1 (serial): classify each covering tile — place cache hits and absent (edge)
    // tiles now; record the misses. `(slot, tile_index, gx, gy, key, off, len)`. `tile_location`
    // resolves each tile's (offset, length) ONCE here — `None` is the "tile absent" path (a
    // sparse/edge tile), exactly the old `!present` branch.
    let mut misses: Vec<(usize, usize, u32, u32, (u32, u32), u64, usize)> = Vec::new();
    for gy in ty0..=ty1 {
        for gx in tx0..=tx1 {
            let tile_index = (gy * tiles_across + gx) as usize;
            let loc = level
                .tile_location(src, &req.index_cache, tile_index as u64)
                .map_err(|e| format!("tile index read: {e}"))?;
            let slot = positions.len();
            positions.push((gx, gy));
            let (off, len) = match loc {
                Some(v) => v,
                None => {
                    tiles.push(Some(Arc::new(DeviceBuffer::new(tw, th, 4))));
                    continue;
                }
            };
            let key = (level_idx as u32, tile_index as u32);
            if let Some(hit) = tile_cache.and_then(|c| c.get(&key)) {
                tiles.push(Some(hit));
                continue;
            }
            misses.push((slot, tile_index, gx, gy, key, off, len));
            tiles.push(None);
        }
    }

    // Pass 2 (parallel): read the miss tiles' bytes CONCURRENTLY on the dedicated I/O pool.
    // Reads dominate over the network (each is a round-trip); the source is `Sync` (positioned
    // reads), so the pool can fan them out. Running on the I/O pool (not rayon's default) lets
    // many reads block on the network without occupying the CPU threads that Pass 3 decode wants.
    let miss_ctiles: Vec<CompressedTile> = io_pool().install(|| {
        misses
            .par_iter()
            .map(|&(_, tile_index, gx, gy, _, off, len)| {
                let bytes = src
                    .read_range(off, len)
                    .map_err(|e| format!("read tile {tile_index}: {e}"))?;
                Ok(CompressedTile {
                    bytes,
                    compression: level.compression,
                    predictor: level.predictor,
                    tile_w: tw,
                    tile_h: th,
                    samples: level.samples_per_pixel,
                    bits_per_sample: level.bits_per_sample,
                    sample_format: level.sample_format,
                    little_endian: cog.little_endian,
                    photometric: level.photometric,
                    jpeg_tables: jpeg_tables.clone(),
                    grid_col: gx,
                    grid_row: gy,
                    present: true,
                })
            })
            .collect::<Result<Vec<_>, String>>()
    })?;

    // Pass 3 (parallel decode), then cache and place each decoded tile.
    let decoded = backend.decode_tiles(&miss_ctiles);
    for (i, buf) in decoded.0.into_iter().enumerate() {
        let arc = Arc::new(buf);
        if let Some(cache) = tile_cache {
            cache.insert(misses[i].4, arc.clone());
        }
        tiles[misses[i].0] = Some(arc);
    }

    // Assemble the decoded tiles into one flat RGBA window buffer.
    let mut window = DeviceBuffer::new(win_cols, win_rows, 4);
    let wstride = window.stride();
    for (slot, &(gx, gy)) in positions.iter().enumerate() {
        let buf = tiles[slot].as_ref().expect("every tile slot is filled");
        let ox = ((gx - tx0) * tw) as usize;
        let oy = ((gy - ty0) * th) as usize;
        let bstride = buf.stride();
        for ry in 0..th as usize {
            let dst = (oy + ry) * wstride + ox * 4;
            let s = ry * bstride;
            window.data[dst..dst + tw as usize * 4]
                .copy_from_slice(&buf.data[s..s + tw as usize * 4]);
        }
    }
    // Zero out padding beyond the true image extent so bilinear near the image edge
    // blends toward transparent instead of into tile padding.
    for ry in 0..win_rows as usize {
        let abs_r = win_origin_y as usize + ry;
        for rx in 0..win_cols as usize {
            let abs_c = win_origin_x as usize + rx;
            if abs_c >= level.width as usize || abs_r >= level.height as usize {
                let d = ry * wstride + rx * 4;
                window.data[d..d + 4].copy_from_slice(&[0, 0, 0, 0]);
            }
        }
    }

    // Build the warp map in window-local coordinates.
    let mut coords = vec![[f64::NAN; 2]; w * h];
    for p in 0..w * h {
        let [u, v] = level_coords[p];
        if u.is_finite() && v.is_finite() {
            coords[p] = [u - win_origin_x, v - win_origin_y];
        }
    }
    let map = WarpMap {
        width: req.width,
        height: req.height,
        coords,
    };

    let warped = backend.warp(&window, &map, req.resample);

    // Style.
    let rgba = match req.style {
        Style::Rgb { bands } => style::apply_rgb(&warped, *bands),
        Style::Pseudocolor { .. } => {
            let ramp = req.style.ramp_lut().ok_or("pseudocolor: no ramp")?;
            backend.colorize(&warped, &ramp).data
        }
    };
    Ok(rgba)
}

/// The window geometry computed by the shared coordinate/overview pass, handed to the
/// band-math tail so it reuses exactly the same overview level and source window.
struct Win<'a> {
    level: &'a cog::Level,
    tw: u32,
    th: u32,
    tiles_across: u32,
    tx0: u32,
    tx1: u32,
    ty0: u32,
    ty1: u32,
    win_origin_x: f64,
    win_origin_y: f64,
    win_cols: u32,
    win_rows: u32,
}

/// Band-math render tail: gather the covering tiles as NATIVE samples (cached in the LRU
/// like the RGBA path — dense, so no f32 bloat), de-interleave only the referenced bands
/// over the source window (+ a validity mask), evaluate the expression at source resolution,
/// warp the derived value to the output grid (nearest), and colorize by value domain. The
/// f32 band planes are transient per request; only the native tile bytes are cached.
#[allow(clippy::too_many_arguments)]
fn render_bandmath<R: RangeSource + Sync>(
    req: &RenderRequest,
    cog: &Cog,
    src: &R,
    win: &Win,
    level_coords: &[[f64; 2]],
    bm: &BandMath,
    tile_cache: Option<&TileCache>,
    level_idx: usize,
) -> Result<Vec<u8>, String> {
    let (w, h) = (req.width as usize, req.height as usize);
    let level = win.level;
    let tw = win.tw as usize;
    let th = win.th as usize;
    let win_cols = win.win_cols as usize;
    let win_rows = win.win_rows as usize;
    let spp = level.samples_per_pixel as usize;
    let bps = decode::bytes_per_sample(level.bits_per_sample);

    // Referenced physical bands (0-based); assemble one source-window plane per referenced
    // band, plus a validity mask (false = nodata / outside image extent -> transparent).
    let used: Vec<usize> = bm.program.bands_used().to_vec();
    let mut band_windows: Vec<Vec<f32>> = vec![vec![0f32; win_cols * win_rows]; used.len()];
    let mut valid = vec![false; win_cols * win_rows];

    // Phase 1 (serial): classify each present covering tile — place cache hits now, record
    // the misses `(slot, tile_index, gx, gy, key, off, len)`. `tile_location` resolves each
    // tile's (offset, length) ONCE here — `None` is the "tile absent" path, exactly the old
    // `!present` branch. Native tiles are cached DENSE in the LRU; a band-math layer never
    // shares this cache with the RGBA path, so keys can't collide.
    let jpeg_tables = cog.jpeg_tables.as_ref().map(|t| Arc::new(t.clone()));
    let mut slots: Vec<(u32, u32, Option<Arc<DeviceBuffer>>)> = Vec::new(); // (gx, gy, native)
    let mut misses: Vec<(usize, usize, u32, u32, (u32, u32), u64, usize)> = Vec::new();
    for gy in win.ty0..=win.ty1 {
        for gx in win.tx0..=win.tx1 {
            let tile_index = (gy * win.tiles_across + gx) as usize;
            let loc = level
                .tile_location(src, &req.index_cache, tile_index as u64)
                .map_err(|e| format!("tile index read: {e}"))?;
            let (off, len) = match loc {
                Some(v) => v,
                None => continue, // absent tile contributes nothing (stays invalid/transparent)
            };
            let si = slots.len();
            let key = (level_idx as u32, tile_index as u32);
            if let Some(hit) = tile_cache.and_then(|c| c.get(&key)) {
                slots.push((gx, gy, Some(hit)));
                continue;
            }
            misses.push((si, tile_index, gx, gy, key, off, len));
            slots.push((gx, gy, None));
        }
    }

    // Phase 2a (I/O pool): read each miss tile's raw bytes CONCURRENTLY. Network round-trips
    // block the I/O pool's threads, not the CPU pool — so Phase 2b decode is never starved by
    // reads of the same request. The source is `Sync`, so the reads fan out.
    let miss_ctiles: Vec<CompressedTile> = io_pool().install(|| {
        misses
            .par_iter()
            .map(|&(_, tile_index, gx, gy, _, off, len)| {
                let bytes = src
                    .read_range(off, len)
                    .map_err(|e| format!("read tile {tile_index}: {e}"))?;
                Ok(CompressedTile {
                    bytes,
                    compression: level.compression,
                    predictor: level.predictor,
                    tile_w: win.tw,
                    tile_h: win.th,
                    samples: level.samples_per_pixel,
                    bits_per_sample: level.bits_per_sample,
                    sample_format: level.sample_format,
                    little_endian: cog.little_endian,
                    photometric: level.photometric,
                    jpeg_tables: jpeg_tables.clone(),
                    grid_col: gx,
                    grid_row: gy,
                    present: true,
                })
            })
            .collect::<Result<Vec<_>, String>>()
    })?;

    // Phase 2b (default/CPU pool): inflate + predictor to native samples. Native samples go in a
    // DeviceBuffer (channels = bytes-per-pixel) so the byte-weighed LRU bounds it exactly.
    let bpp = (spp * bps) as u8;
    let decoded: Vec<Arc<DeviceBuffer>> = miss_ctiles
        .par_iter()
        .map(|ct| {
            Arc::new(DeviceBuffer {
                data: decode::decode_tile_samples(ct),
                width: win.tw,
                height: win.th,
                channels: bpp,
            })
        })
        .collect();
    for (i, db) in decoded.into_iter().enumerate() {
        if let Some(c) = tile_cache {
            c.insert(misses[i].4, db.clone());
        }
        slots[misses[i].0].2 = Some(db);
    }

    // Phase 3 (serial assemble): de-interleave ONLY the expression's referenced bands from
    // each tile's native samples into the source-window planes (+ validity mask).
    for (gx, gy, native) in &slots {
        let native = native.as_ref().expect("every slot filled after decode");
        let ox = ((gx - win.tx0) * win.tw) as usize;
        let oy = ((gy - win.ty0) * win.th) as usize;
        for ry in 0..th {
            for rx in 0..tw {
                let abs_c = win.win_origin_x as usize + ox + rx;
                let abs_r = win.win_origin_y as usize + oy + ry;
                if abs_c >= level.width as usize || abs_r >= level.height as usize {
                    continue; // beyond the true image extent -> stays invalid
                }
                let sp = ry * tw + rx; // tile-local pixel
                let wp = (oy + ry) * win_cols + (ox + rx); // window pixel
                let mut ok = true;
                for (k, &b) in used.iter().enumerate() {
                    let voff = (sp * spp + b) * bps;
                    let v = decode::read_sample(
                        &native.data,
                        voff,
                        bps,
                        level.sample_format,
                        cog.little_endian,
                    );
                    // NOTE (dtype-adaptive, deferred): `read_sample` is lossless (f64), but the
                    // processing plane is f32. Exact for u8/i8/u16/i16/f32 (all current data +
                    // the polar DEMs). For u32/i32/f64 sources this truncates values >2^24 — a
                    // documented footgun; f64 processing planes are deferred until real wide-int
                    // data lands. See docs/superpowers/specs/2026-07-11-generic-dtype-decoder-design.md.
                    band_windows[k][wp] = v as f32;
                    // Compare nodata in f64 (v is lossless) before the f32 store.
                    if (v - bm.nodata).abs() < 0.5 {
                        ok = false;
                    }
                }
                valid[wp] = ok;
            }
        }
    }

    // Evaluate the expression over the assembled window planes (vectorized per plane).
    let refs: Vec<&[f32]> = band_windows.iter().map(|v| v.as_slice()).collect();
    let derived_win = bm.program.eval(&refs, win_cols * win_rows);

    // Warp (nearest) the derived value + validity onto the output grid.
    let mut out_vals = vec![f32::NAN; w * h];
    let mut out_valid = vec![false; w * h];
    for p in 0..w * h {
        let [u, v] = level_coords[p];
        if !u.is_finite() || !v.is_finite() {
            continue;
        }
        let lc = (u - win.win_origin_x).floor() as i64;
        let lr = (v - win.win_origin_y).floor() as i64;
        if lc < 0 || lr < 0 || lc as usize >= win_cols || lr as usize >= win_rows {
            continue;
        }
        let wp = lr as usize * win_cols + lc as usize;
        out_vals[p] = derived_win[wp];
        out_valid[p] = valid[wp];
    }

    // Colorize by value domain (transparent where invalid / non-finite).
    req.style.colorize_values(&out_vals, &out_valid)
}

/// Estimate source pixels spanned per output pixel (the desired downsample factor).
#[allow(clippy::too_many_arguments)]
fn desired_factor(
    native_coords: &[[f64; 2]],
    w: usize,
    h: usize,
    transformer: &Transformer,
    minx: f64,
    miny: f64,
    maxx: f64,
    maxy: f64,
    native: &cog::GeoTransform,
    _dx: f64,
    _dy: f64,
) -> f64 {
    let ic = (w / 2).min(w.saturating_sub(1));
    let jc = (h / 2).min(h.saturating_sub(1));
    let idx = |i: usize, j: usize| j * w + i;
    let mut fx: Option<f64> = None;
    let mut fy: Option<f64> = None;
    if w >= 2 {
        let a = native_coords[idx(ic, jc)];
        let b = native_coords[idx((ic + 1).min(w - 1), jc)];
        if a[0].is_finite() && b[0].is_finite() {
            fx = Some((b[0] - a[0]).abs());
        }
    }
    if h >= 2 {
        let a = native_coords[idx(ic, jc)];
        let b = native_coords[idx(ic, (jc + 1).min(h - 1))];
        if a[1].is_finite() && b[1].is_finite() {
            fy = Some((b[1] - a[1]).abs());
        }
    }
    // Fallback via bbox-corner spans when center diffs are unavailable/degenerate.
    let corner = || -> f64 {
        let pts = [
            (minx, miny),
            (maxx, miny),
            (maxx, maxy),
            (minx, maxy),
            ((minx + maxx) / 2.0, (miny + maxy) / 2.0),
        ];
        let mut us = Vec::new();
        let mut vs = Vec::new();
        for (x, y) in pts {
            if let Some((sx, sy)) = transformer.to_source(x, y) {
                let (u, v) = native.geo_to_pix(sx, sy);
                if u.is_finite() && v.is_finite() {
                    us.push(u);
                    vs.push(v);
                }
            }
        }
        if us.is_empty() {
            return 1.0;
        }
        let uw = us.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
            - us.iter().cloned().fold(f64::INFINITY, f64::min);
        let vh = vs.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
            - vs.iter().cloned().fold(f64::INFINITY, f64::min);
        let dfx = uw / w as f64;
        let dfy = vh / h as f64;
        // Use the finer axis so we never upsample in either direction (matches GDAL).
        dfx.min(dfy)
    };

    match (fx, fy) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => corner(),
    }
    .max(1e-9)
}
