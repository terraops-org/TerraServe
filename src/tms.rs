// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Generic tile-grid model (TileMatrixSet) + presets + (later) the TileFactory seam.
//!
//! One internal model serves TMS, WMTS, and XYZ. Stored in the WMTS/top-left convention
//! (origin = top-left corner, row increases south). A tile request is a `GetMap` with a
//! computed bbox — `TileFactory::render_tile` (a later task) reuses the existing render path.
//! Verified grid numbers: docs/tilematrixset-reference.md (authoritative OGC registry JSON).

use crate::cog::Cog;

/// One zoom level of a grid. `resolution` = CRS units per pixel (== OGC cellSize == TMS upp).
#[derive(Clone, Debug)]
pub struct TmLevel {
    pub z: u32,
    pub resolution: f64,
    pub matrix_w: u32,
    pub matrix_h: u32,
}

/// A tile grid: CRS + top-left origin + tile size + the per-zoom pyramid.
#[derive(Clone, Debug)]
pub struct TileMatrixSet {
    pub id: String,
    pub crs: String,
    pub origin_x: f64,
    pub origin_y: f64,
    pub tile_w: u32,
    pub tile_h: u32,
    pub levels: Vec<TmLevel>,
}

/// Meters per CRS unit for the 0.28 mm scale-denominator rule: degrees for geographic, else 1.
///
/// NOTE: string-matches only 4326/CRS84 today (the only geographic CRS among the presets). A custom
/// geographic grid in another CRS gets `1.0` and a wrong scaleDenominator — add a proj-unit lookup
/// when WMTS (piece C) exposes scaleDenominator broadly.
pub fn meters_per_unit(crs: &str) -> f64 {
    match crs {
        "EPSG:4326" | "CRS:84" | "OGC:CRS84" | "urn:ogc:def:crs:OGC:1.3:CRS84" => 111319.4907932736,
        _ => 1.0,
    }
}

impl TileMatrixSet {
    pub fn level(&self, z: u32) -> Option<&TmLevel> {
        self.levels.iter().find(|l| l.z == z)
    }

    /// Tile bbox `[minx, miny, maxx, maxy]` in CRS units. `row` is TOP-LEFT (0 = north).
    /// `None` when z/col/row fall outside the grid.
    pub fn tile_bounds(&self, z: u32, col: u32, row: u32) -> Option<[f64; 4]> {
        let lvl = self.level(z)?;
        if col >= lvl.matrix_w || row >= lvl.matrix_h {
            return None;
        }
        let span_x = self.tile_w as f64 * lvl.resolution;
        let span_y = self.tile_h as f64 * lvl.resolution;
        let minx = self.origin_x + col as f64 * span_x;
        let maxy = self.origin_y - row as f64 * span_y;
        Some([minx, maxy - span_y, minx + span_x, maxy])
    }

    /// True if `matrix·tile·resolution` (the grid's covered extent) is the same at every level —
    /// the property that makes the single advertised bottom-left TMS `<Origin>` correct at every
    /// zoom. Presets and `from_cog` satisfy this by construction; custom grids are validated against
    /// it at startup (a non-invariant grid misindexes standards TMS clients).
    pub fn is_level_invariant(&self) -> bool {
        let Some(l0) = self.levels.first() else {
            return true;
        };
        let ext_x0 = l0.matrix_w as f64 * self.tile_w as f64 * l0.resolution;
        let ext_y0 = l0.matrix_h as f64 * self.tile_h as f64 * l0.resolution;
        self.levels.iter().all(|l| {
            let ex = l.matrix_w as f64 * self.tile_w as f64 * l.resolution;
            let ey = l.matrix_h as f64 * self.tile_h as f64 * l.resolution;
            (ex - ext_x0).abs() <= ext_x0.abs() * 1e-6 + 1e-6
                && (ey - ext_y0).abs() <= ext_y0.abs() * 1e-6 + 1e-6
        })
    }

    /// The whole grid's extent `[minx, miny, maxx, maxy]` in CRS units (from level 0; level-invariant
    /// so any level gives the same). `None` for an empty grid. Bottom-left = `[minx, miny]` is the TMS
    /// `<Origin>`.
    pub fn full_extent(&self) -> Option<[f64; 4]> {
        let l = self.levels.first()?;
        let ex = l.matrix_w as f64 * self.tile_w as f64 * l.resolution;
        let ey = l.matrix_h as f64 * self.tile_h as f64 * l.resolution;
        Some([
            self.origin_x,
            self.origin_y - ey,
            self.origin_x + ex,
            self.origin_y,
        ])
    }

    /// Inclusive tile row/col range (TOP-LEFT convention) covering `bounds` (grid CRS) at zoom z,
    /// clamped to the level matrix — `(mincol, maxcol, minrow, maxrow)`. A tile the data only touches
    /// on its edge is excluded (max via `ceil-1`). `None` if the level is absent OR the data is
    /// disjoint from the matrix. Feeds WMTS `TileMatrixSetLimits` so clients skip empty tiles.
    pub fn tile_limits(&self, bounds: [f64; 4], z: u32) -> Option<(u32, u32, u32, u32)> {
        let lvl = self.level(z)?;
        let span_x = self.tile_w as f64 * lvl.resolution;
        let span_y = self.tile_h as f64 * lvl.resolution;
        let [minx, miny, maxx, maxy] = bounds;
        // Tiles overlapping the bounds; row counts from the north edge (maxy).
        let col_lo = ((minx - self.origin_x) / span_x).floor();
        let col_hi = ((maxx - self.origin_x) / span_x).ceil() - 1.0;
        let row_lo = ((self.origin_y - maxy) / span_y).floor();
        let row_hi = ((self.origin_y - miny) / span_y).ceil() - 1.0;
        let (mw, mh) = (lvl.matrix_w as f64, lvl.matrix_h as f64);
        let mincol = col_lo.max(0.0);
        let maxcol = col_hi.min(mw - 1.0);
        let minrow = row_lo.max(0.0);
        let maxrow = row_hi.min(mh - 1.0);
        if maxcol < mincol || maxrow < minrow {
            return None; // clamped range empty -> data disjoint from the matrix
        }
        Some((mincol as u32, maxcol as u32, minrow as u32, maxrow as u32))
    }

    /// OGC scale denominator at zoom z (0.28 mm/pixel rule).
    pub fn scale_denominator(&self, z: u32) -> f64 {
        match self.level(z) {
            Some(l) => l.resolution * meters_per_unit(&self.crs) / 0.00028,
            None => f64::NAN,
        }
    }

    pub fn web_mercator_quad(tile_px: u32) -> TileMatrixSet {
        build_quad(
            &suffix_id("WebMercatorQuad", tile_px),
            "EPSG:3857",
            -20037508.3427892,
            20037508.3427892,
            40075016.6855784,
            tile_px,
            25,
            false,
        )
    }

    pub fn world_crs84_quad(tile_px: u32) -> TileMatrixSet {
        build_quad(
            &suffix_id("WorldCRS84Quad", tile_px),
            "EPSG:4326",
            -180.0,
            90.0,
            180.0, // LAT span; the axis with 2^z tiles (matrix_h). matrix_w = 2^(z+1).
            tile_px,
            24,
            true,
        )
    }

    pub fn ups_wgs84_quad(crs: &str, tile_px: u32) -> TileMatrixSet {
        let base = match crs {
            "EPSG:5041" => "UPSArcticWGS84Quad",
            "EPSG:5042" => "UPSAntarcticWGS84Quad",
            _ => "UPSWGS84Quad",
        };
        build_quad(
            &suffix_id(base, tile_px),
            crs,
            -14440759.350252,
            18440759.350252,
            32881518.700504,
            tile_px,
            25,
            false,
        )
    }

    /// Native grid from a layer's COG: CRS = layer native, origin = COG top-left corner.
    ///
    /// **TMS-indexable by construction.** The grid is a DYADIC quad-tree with a LEVEL-INVARIANT
    /// extent: `matrix·tile_px·resolution` is the same at every z, so the single advertised
    /// bottom-left `<Origin>` is correct for all zooms (a standards client computing row from
    /// `(Y-origin)/tile_span` never misindexes). Resolutions are `native_px · 2^(L-1-z)` (finest z
    /// == native pixel size), decoupled from the overviews' (possibly non-dyadic) pixel sizes — the
    /// render path picks the nearest overview via `desired_factor`; the GRID stays clean/dyadic.
    /// Matrix dims come from the DATA EXTENT (px AND py — anisotropic COGs get extra rows, not lost
    /// data); tiles beyond the data render transparent (`render_with_cog` already handles that).
    pub fn from_cog(cog: &Cog, crs: &str, tile_px: u32) -> TileMatrixSet {
        let g0 = cog.levels[0].geo;
        let native_px = g0.px;
        let n_levels = (cog.levels.len().max(1)) as u32;
        // Full data extent in CRS units, respecting BOTH pixel dimensions.
        let data_w = cog.levels[0].width as f64 * g0.px;
        let data_h = cog.levels[0].height as f64 * g0.py;
        // Coarsest resolution (z0) = native · 2^(L-1); finest (z=L-1) = native.
        let res0 = native_px * 2f64.powi(n_levels as i32 - 1);
        // Matrix at the coarsest level covering the whole data; ×2 per level ⇒ invariant extent.
        let mw0 = ((data_w / (tile_px as f64 * res0)).ceil() as u32).max(1);
        let mh0 = ((data_h / (tile_px as f64 * res0)).ceil() as u32).max(1);
        let levels = (0..n_levels)
            .map(|z| TmLevel {
                z,
                resolution: res0 / 2f64.powi(z as i32),
                matrix_w: mw0 * 2u32.pow(z),
                matrix_h: mh0 * 2u32.pow(z),
            })
            .collect();
        TileMatrixSet {
            id: "from_cog".to_string(),
            crs: crs.to_string(),
            origin_x: g0.origin_x,
            origin_y: g0.origin_y,
            tile_w: tile_px,
            tile_h: tile_px,
            levels,
        }
    }
}

/// Resolve a well-known preset id → a `TileMatrixSet`. An id may carry an explicit `_{tile_px}`
/// size suffix (`WebMercatorQuad_256`) which overrides the `tile_px` argument (R3: lets one config
/// entry pin its size, and lets a URL request the un-suffixed base name). Returns `None` for an
/// unknown base id (the caller then falls through to config-custom grids).
pub fn preset(id: &str, tile_px: u32) -> Option<TileMatrixSet> {
    let (base, px) = match id.rsplit_once('_') {
        Some((b, n)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => {
            (b, n.parse().unwrap_or(tile_px))
        }
        _ => (id, tile_px),
    };
    match base {
        "WebMercatorQuad" => Some(TileMatrixSet::web_mercator_quad(px)),
        "WorldCRS84Quad" => Some(TileMatrixSet::world_crs84_quad(px)),
        "UPSArcticWGS84Quad" => Some(TileMatrixSet::ups_wgs84_quad("EPSG:5041", px)),
        "UPSAntarcticWGS84Quad" => Some(TileMatrixSet::ups_wgs84_quad("EPSG:5042", px)),
        _ => None,
    }
}

/// Strip a trailing `_{digits}` size suffix from a grid id, returning the base name. Used by the
/// TMS front-end so a URL `@WebMercatorQuad` matches a stored `WebMercatorQuad_512` (R3).
pub fn strip_size_suffix(id: &str) -> &str {
    match id.rsplit_once('_') {
        Some((b, n)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => b,
        _ => id,
    }
}

/// The layer's native data extent (COG geo) reprojected into `grid_crs`, densified — as
/// `[minx, miny, maxx, maxy]`. `None` if `src_crs → grid_crs` is unavailable. Precompute once per
/// layer×grid at startup; feeds the TMS `<BoundingBox>` (piece B), the empty-tile early-out below,
/// and (later) WMTS `TileMatrixSetLimits` — so WMTS reuses it without a core change.
pub fn bounds_in_grid_crs(cog: &Cog, src_crs: &str, grid_crs: &str) -> Option<[f64; 4]> {
    let g = cog.levels[0].geo;
    let (w, h) = (cog.levels[0].width as f64, cog.levels[0].height as f64);
    let minx = g.origin_x;
    let maxy = g.origin_y;
    let maxx = g.origin_x + w * g.px;
    let miny = g.origin_y - h * g.py;
    crate::reproj::crs_bounds(src_crs, grid_crs, minx, miny, maxx, maxy)
}

/// A tile to render, holding the render INGREDIENTS by reference (not a `server::Layer`) so the
/// core stays independent of the HTTP layer. `row` is TOP-LEFT convention (0 = north); the TMS
/// front-end converts its bottom-left y before constructing this.
pub struct TileRequest<'a> {
    pub cog: &'a Cog,
    pub source: &'a crate::s3::AnySource,
    pub cog_path: &'a str,
    pub src_crs: &'a str,
    pub style: &'a crate::style::Style,
    pub band_math: Option<&'a crate::render::BandMath>,
    pub cache: Option<&'a crate::cache::TileCache>,
    pub index_cache: &'a crate::cache::IndexCache,
    /// Layer data extent in the GRID's CRS (precompute once via `bounds_in_grid_crs`). When present,
    /// a tile that cannot intersect it short-circuits to a transparent PNG — skipping `tile_px²` proj
    /// transforms + a decode. `None` → skip the optimization (render still yields transparent).
    pub data_bounds: Option<[f64; 4]>,
    pub grid: &'a TileMatrixSet,
    pub z: u32,
    pub col: u32,
    pub row: u32,
}

/// The seam: a tile IS a `GetMap` with a computed bbox. Reuses the layer's parse-once COG, shared
/// source, band-math, style, and tile cache verbatim (tiles get caching for free).
pub struct TileFactory;

impl TileFactory {
    pub fn render_tile(req: &TileRequest) -> Result<Vec<u8>, String> {
        let bbox = req
            .grid
            .tile_bounds(req.z, req.col, req.row)
            .ok_or_else(|| {
                format!(
                    "tile out of range: z={} col={} row={}",
                    req.z, req.col, req.row
                )
            })?;
        let (tw, th) = (req.grid.tile_w, req.grid.tile_h);
        // Empty-tile early-out: a tile that can't intersect the data extent (in the grid CRS)
        // renders to a transparent PNG without tile_px² proj transforms + a decode.
        if let Some(db) = req.data_bounds {
            let [minx, miny, maxx, maxy] = bbox;
            let disjoint = minx >= db[2] || maxx <= db[0] || miny >= db[3] || maxy <= db[1];
            if disjoint {
                return crate::pngio::encode_rgba(
                    &vec![0u8; (tw as usize * th as usize) * 4],
                    tw,
                    th,
                );
            }
        }
        let rr = crate::render::RenderRequest {
            cog_path: req.cog_path,
            bbox,
            crs: &req.grid.crs,
            src_crs: req.src_crs,
            width: tw,
            height: th,
            resample: crate::backend::Resample::Bilinear,
            style: req.style,
            band_math: req.band_math,
            index_cache: req.index_cache.clone(),
        };
        let rgba = crate::render::render_with_cog(&rr, req.cog, req.source, req.cache)?;
        crate::pngio::encode_rgba(&rgba, tw, th)
    }
}

/// 256 keeps the canonical well-known id (CITE conformance); other sizes get a `_{tile_px}` variant.
fn suffix_id(base: &str, tile_px: u32) -> String {
    if tile_px == 256 {
        base.to_string()
    } else {
        format!("{base}_{tile_px}")
    }
}

/// Build a quad grid from the general formula: resolution(z) = base_span / tile_px / 2^z.
/// `crs84` selects the WorldCRS84Quad 2×1-at-z0 matrix (matrix_w = 2^(z+1)); else square.
fn build_quad(
    id: &str,
    crs: &str,
    origin_x: f64,
    origin_y: f64,
    base_span: f64,
    tile_px: u32,
    n_levels: u32,
    crs84: bool,
) -> TileMatrixSet {
    let res0 = base_span / tile_px as f64;
    let levels = (0..n_levels)
        .map(|z| {
            let f = 2f64.powi(z as i32);
            TmLevel {
                z,
                resolution: res0 / f,
                matrix_w: if crs84 { 2u32.pow(z + 1) } else { 2u32.pow(z) },
                matrix_h: 2u32.pow(z),
            }
        })
        .collect();
    TileMatrixSet {
        id: id.to_string(),
        crs: crs.to_string(),
        origin_x,
        origin_y,
        tile_w: tile_px,
        tile_h: tile_px,
        levels,
    }
}
