// SPDX-License-Identifier: MPL-2.0
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

#[derive(serde::Deserialize)]
struct OgcTms {
    id: String,
    crs: String,
    /// OGC TMS 2.0 `orderedAxes`: the axis order the document's OWN coordinates are written in.
    /// Optional; absent means "the CRS's declared order applies" (see `origin_is_northing_first`).
    #[serde(rename = "orderedAxes")]
    ordered_axes: Option<Vec<String>>,
    #[serde(rename = "tileMatrices")]
    tile_matrices: Vec<OgcTileMatrix>,
}

#[derive(serde::Deserialize)]
struct OgcTileMatrix {
    #[serde(rename = "cellSize")]
    cell_size: Option<f64>,
    #[serde(rename = "scaleDenominator")]
    scale_denominator: Option<f64>,
    #[serde(rename = "pointOfOrigin")]
    point_of_origin: [f64; 2],
    #[serde(rename = "tileWidth")]
    tile_width: u32,
    #[serde(rename = "tileHeight")]
    tile_height: u32,
    #[serde(rename = "matrixWidth")]
    matrix_width: u32,
    #[serde(rename = "matrixHeight")]
    matrix_height: u32,
}

/// Normalize an OGC CRS identifier (URI/URN/shortcode) to what libproj + the rest of the
/// engine use. A bare proj string / WKT passes through untouched.
fn normalize_crs(crs: &str) -> String {
    // http://www.opengis.net/def/crs/EPSG/0/2056  ->  EPSG:2056
    if let Some(rest) = crs.rsplit("/def/crs/").next().filter(|r| *r != crs) {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() == 3 {
            let (auth, code) = (parts[0].to_uppercase(), parts[2]);
            if auth == "OGC" && code.eq_ignore_ascii_case("CRS84") {
                return "CRS:84".to_string();
            }
            return format!("{auth}:{code}");
        }
    }
    crs.to_string()
}

/// Is this document's `pointOfOrigin` written NORTHING-FIRST?
///
/// ⚠ This is the difference between a working grid and 3.5 MILLION METRES of error, which renders
/// as empty tiles behind a 200 — the same failure the WMS 1.3.0 axis flip (`deb77cb`) and the Swiss
/// `+towgs84` drop produced. It went unnoticed here because `swissLV95` (EPSG:2056) is
/// easting-first, so the only custom grid ever shipped happened to be the case where the naive
/// `[0]`=x, `[1]`=y reading is correct.
///
/// Precedence, and it matters:
///  1. **The document's own `orderedAxes`**, when present. A TMS document declares the order its
///     numbers are in, and that declaration is authoritative for reading them — even if it
///     disagrees with the CRS registry, because the numbers are what they are.
///  2. Otherwise **the CRS's declared order**, via `reproj::crs_is_northing_first` (the same PROJ
///     query the WMS 1.3.0 path uses — one definition, so the two can never drift).
///  3. Otherwise easting-first, matching that helper's own documented fallback: it is right for the
///     easting-first majority, and it is what every grid parsed before this fix assumed.
///
/// The real case: OGC's registered `EuropeanETRS89_LAEAQuad` (EPSG:3035) declares
/// `orderedAxes: ["Y","X"]` and writes `pointOfOrigin: [5500000, 2000000]` — northing 5 500 000,
/// easting 2 000 000. Read naively that becomes x=5 500 000, y=2 000 000: off the grid entirely.
fn origin_is_northing_first(ordered_axes: Option<&Vec<String>>, crs: &str) -> bool {
    let from_crs = crate::reproj::crs_is_northing_first(crs);
    if let Some(axes) = ordered_axes {
        if let Some(first) = axes.first() {
            let a = first.trim().to_ascii_lowercase();
            // OGC documents spell it "Y" / "N" / "Lat" depending on the CRS kind.
            let declared = matches!(
                a.as_str(),
                "y" | "n" | "lat" | "north" | "northing" | "latitude"
            );
            // ⚠ WARN when the document contradicts the CRS registry, because one spelling really is
            // ambiguous and silently guessing wrong costs millions of metres. On EPSG:3301
            // (Estonia), 2180 (Poland) and the German Gauss-Krüger zones the axis literally NAMED
            // "X" is the NORTHING — so a hand-written `["X","Y"]` there means northing-first, the
            // opposite of what it means on EPSG:3035 where X genuinely is the easting.
            //
            // We still honour the declaration (the document owns its own numbers, and the
            // registered LAEA document's `["X","Y"]` override case is legitimate), but a mismatch
            // is far more often a typo than an intention, and it renders as an empty tile behind a
            // 200 rather than an error. Say so once, at load, where an operator will see it.
            if from_crs == Some(!declared) {
                eprintln!(
                    "WARNING: tile grid for {crs}: document declares orderedAxes[0]={first:?} \
                     ({}), but {crs} declares {} first. Honouring the document. If tiles come back \
                     blank behind a 200, this is the first thing to check — on some CRSs the axis \
                     named \"X\" IS the northing.",
                    if declared {
                        "northing-first"
                    } else {
                        "easting-first"
                    },
                    if from_crs == Some(true) {
                        "northing"
                    } else {
                        "easting"
                    },
                );
            }
            return declared;
        }
    }
    from_crs.unwrap_or(false)
}

/// Parse an OGC TileMatrixSet 2.0 JSON document into a `TileMatrixSet`.
pub fn from_ogc_json(json: &str) -> Result<TileMatrixSet, String> {
    let doc: OgcTms = serde_json::from_str(json).map_err(|e| format!("OGC TMS JSON: {e}"))?;
    if doc.tile_matrices.is_empty() {
        return Err("OGC TMS JSON: tileMatrices is empty".into());
    }
    let crs = normalize_crs(&doc.crs);
    let mpu = meters_per_unit(&crs);
    // OGC origin is shared across levels (single pointOfOrigin per matrix; take level 0's).
    // The pair is (x, y) ONLY for an easting-first CRS — see `origin_is_northing_first`.
    let raw = doc.tile_matrices[0].point_of_origin;
    let origin = if origin_is_northing_first(doc.ordered_axes.as_ref(), &crs) {
        [raw[1], raw[0]]
    } else {
        raw
    };
    let tile_w = doc.tile_matrices[0].tile_width;
    let tile_h = doc.tile_matrices[0].tile_height;
    let levels = doc
        .tile_matrices
        .iter()
        .enumerate()
        .map(|(z, m)| {
            // resolution (CRS units / px): cellSize if present, else scaleDenominator * 0.28mm / mpu.
            let resolution = m
                .cell_size
                .or_else(|| m.scale_denominator.map(|sd| sd * 0.00028 / mpu))
                .ok_or("OGC TMS JSON: tileMatrix needs cellSize or scaleDenominator")?;
            Ok(TmLevel {
                z: z as u32,
                resolution,
                matrix_w: m.matrix_width.max(1),
                matrix_h: m.matrix_height.max(1),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(TileMatrixSet {
        id: doc.id,
        crs,
        origin_x: origin[0],
        origin_y: origin[1],
        tile_w,
        tile_h,
        levels,
    })
}

/// Inverse of `normalize_crs`'s URI shortcut: `EPSG:<n>` → the OGC CRS URI form. Everything else
/// (a `CRS:84` shortcode, a proj string, WKT) passes through untouched — `from_ogc_json`'s
/// `normalize_crs` only rewrites the `/def/crs/` URI shape, so a non-EPSG string already round-trips.
fn to_ogc_crs_uri(crs: &str) -> String {
    let trimmed = crs.trim();
    if let Some(code) = trimmed
        .strip_prefix("EPSG:")
        .or_else(|| trimmed.strip_prefix("epsg:"))
    {
        if !code.is_empty() && code.chars().all(|c| c.is_ascii_digit()) {
            return format!("http://www.opengis.net/def/crs/EPSG/0/{code}");
        }
    }
    trimmed.to_string()
}

/// `to_ogc_json`'s body, as a `serde_json::Value` — `pub(crate)` so `tms_http`'s `/tileMatrixSets/{id}`
/// handler can inject the non-standard `"proj4"` convenience field without a stringify/reparse
/// round-trip. Mirrors `from_ogc_json`'s field mapping in reverse: `id`=z (as a string, per the OGC
/// schema), `cellSize`=resolution, `pointOfOrigin`=`[origin_x, origin_y]` (the ORIGIN IS SHARED across
/// levels in this model — `from_ogc_json` takes it from level 0 only, so emitting it on every level
/// is what makes the round-trip exact), `tileWidth`/`tileHeight`/`matrixWidth`/`matrixHeight` verbatim.
///
/// ⚠ ALWAYS EMITS `[x, y]`, and DECLARES `orderedAxes: ["X","Y"]` to say so. This is deliberate and
/// it is not the same choice as the parse side above.
///
/// A TMS 2.0 document declares the order of its own coordinates, so `["X","Y"]` + `[x, y]` is fully
/// spec-legal for ANY CRS — including a northing-first one — and it round-trips exactly through
/// `from_ogc_json`, which honours the declaration. What it also does is keep every NAIVE client
/// working: a reader that assumes `[x, y]` and ignores `orderedAxes` still gets the right grid.
///
/// That is not hypothetical. Mirroring the official northing-first byte order here (the first cut
/// of this change) broke two live things at once: `src/xray.html` reads `pointOfOrigin[0]` as x and
/// never looks at `orderedAxes`, and `/tileMatrixSets/WorldCRS84Quad` flipped from `[-180, 90]` to
/// `[90, -180]` because EPSG:4326 is latitude-first — a regression on already-shipping demos, for a
/// fidelity nobody had asked for. Byte-identity with the OGC registry document is a nice property;
/// not breaking every client that reads us is a requirement.
///
/// (`xray.html` was ALSO taught to honour `orderedAxes`, so it can consume a third-party document
/// that makes the other choice. Belt and braces: this side stays maximally compatible regardless.)
pub(crate) fn to_ogc_value(tms: &TileMatrixSet) -> serde_json::Value {
    let origin = [tms.origin_x, tms.origin_y];
    let tile_matrices: Vec<serde_json::Value> = tms
        .levels
        .iter()
        .map(|l| {
            serde_json::json!({
                "id": l.z.to_string(),
                "cellSize": l.resolution,
                "pointOfOrigin": origin,
                "tileWidth": tms.tile_w,
                "tileHeight": tms.tile_h,
                "matrixWidth": l.matrix_w,
                "matrixHeight": l.matrix_h,
            })
        })
        .collect();
    serde_json::json!({
        "id": tms.id,
        "crs": to_ogc_crs_uri(&tms.crs),
        // See the doc comment: we always write x,y and say so, rather than mirroring the CRS's
        // declared order. A reader that honours this gets it right; a reader that ignores it and
        // assumes x,y ALSO gets it right, which is the whole point.
        "orderedAxes": ["X", "Y"],
        "tileMatrices": tile_matrices,
    })
}

/// Serialize a `TileMatrixSet` to OGC TileMatrixSet 2.0 JSON — the inverse of `from_ogc_json`
/// (round-trips id/crs/origin/tile-size/levels; see the `tms.rs` test module). Served at
/// `GET /tileMatrixSets/{id}` (`tms_http::tile_matrix_set_doc`) so a client (the X-ray viewer, Task 3)
/// can read any published grid's CRS + tile geometry without hardcoding it.
pub fn to_ogc_json(tms: &TileMatrixSet) -> String {
    serde_json::to_string(&to_ogc_value(tms)).unwrap_or_else(|_| "{}".to_string())
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

#[cfg(test)]
mod tests {
    #[test]
    fn from_ogc_json_cellsize_and_scaledenominator() {
        // Minimal OGC TMS 2.0: two levels, one via cellSize, one via scaleDenominator.
        let json = r#"{
          "id": "TestQuad",
          "crs": "http://www.opengis.net/def/crs/EPSG/0/2056",
          "tileMatrices": [
            { "id": "0", "cellSize": 4000.0, "pointOfOrigin": [2420000.0, 1350000.0],
              "tileWidth": 256, "tileHeight": 256, "matrixWidth": 1, "matrixHeight": 1 },
            { "id": "1", "scaleDenominator": 7142857.142857143, "pointOfOrigin": [2420000.0, 1350000.0],
              "tileWidth": 256, "tileHeight": 256, "matrixWidth": 2, "matrixHeight": 1 }
          ]
        }"#;
        let tms = super::from_ogc_json(json).expect("parse");
        assert_eq!(tms.id, "TestQuad");
        assert_eq!(tms.crs, "EPSG:2056"); // OGC CRS URI normalized to shortcode
        assert_eq!(tms.tile_w, 256);
        assert_eq!(tms.levels.len(), 2);
        assert_eq!(tms.levels[0].z, 0);
        assert!((tms.levels[0].resolution - 4000.0).abs() < 1e-6); // cellSize path
                                                                   // scaleDenominator 7142857.14 * 0.00028 / meters_per_unit(EPSG:2056=1.0) = 2000.0
        assert!((tms.levels[1].resolution - 2000.0).abs() < 1e-3);
        assert_eq!(tms.origin_x, 2420000.0);
        assert_eq!(tms.origin_y, 1350000.0);
    }

    #[test]
    fn from_ogc_json_rejects_missing_tilematrices() {
        let json = r#"{ "id": "X", "crs": "EPSG:2056", "tileMatrices": [] }"#;
        assert!(super::from_ogc_json(json).is_err());
    }

    /// Task 2, Step 1: `to_ogc_json` is the INVERSE of `from_ogc_json` — the `WorldCRS84Quad` preset
    /// serialized then parsed back yields an identical `TileMatrixSet` (id/crs/origin/tile/levels).
    #[test]
    fn to_ogc_json_round_trips_world_crs84_quad() {
        let original = super::TileMatrixSet::world_crs84_quad(256);
        let json = super::to_ogc_json(&original);
        let round = super::from_ogc_json(&json).expect("to_ogc_json output must parse");

        assert_eq!(round.id, original.id);
        assert_eq!(round.crs, original.crs); // "EPSG:4326" -> OGC URI -> normalized back to "EPSG:4326"
        assert_eq!(round.origin_x, original.origin_x);
        assert_eq!(round.origin_y, original.origin_y);
        assert_eq!(round.tile_w, original.tile_w);
        assert_eq!(round.tile_h, original.tile_h);
        assert_eq!(round.levels.len(), original.levels.len());
        for (a, b) in original.levels.iter().zip(round.levels.iter()) {
            assert_eq!(a.z, b.z);
            assert!(
                (a.resolution - b.resolution).abs() < 1e-9,
                "resolution drift at z={}: {} vs {}",
                a.z,
                a.resolution,
                b.resolution
            );
            assert_eq!(a.matrix_w, b.matrix_w);
            assert_eq!(a.matrix_h, b.matrix_h);
        }
    }

    /// The OGC `crs` field is the EPSG URI form when the id is `EPSG:<n>` (mirrors
    /// `normalize_crs`'s inverse mapping) — verified directly on the JSON text, not just via the
    /// round-trip (which would also pass if both sides silently agreed on a non-standard form).
    #[test]
    fn to_ogc_json_emits_the_epsg_uri_form_of_crs() {
        let tms = super::TileMatrixSet::web_mercator_quad(256);
        let json = super::to_ogc_json(&tms);
        assert!(
            json.contains("http://www.opengis.net/def/crs/EPSG/0/3857"),
            "expected the OGC CRS URI in: {json}"
        );
    }

    /// Task 5, Step 1: the Swiss LV95 fixture (`fixtures/grids/swissLV95.json`, the swisstopo
    /// 27-level resolution ladder over the official CH extent) round-trips through `from_ogc_json`.
    #[test]
    fn from_ogc_json_round_trips_the_swiss_lv95_fixture() {
        let json = std::fs::read_to_string("fixtures/grids/swissLV95.json")
            .expect("fixture file readable");
        let tms = super::from_ogc_json(&json).expect("swissLV95.json parses");
        assert_eq!(tms.id, "swissLV95");
        assert_eq!(tms.crs, "EPSG:2056"); // OGC CRS URI normalized to shortcode
        assert_eq!(tms.origin_x, 2420000.0);
        assert_eq!(tms.origin_y, 1350000.0);
        assert_eq!(tms.tile_w, 256);
        assert_eq!(tms.tile_h, 256);
        assert_eq!(tms.levels.len(), 27);
        assert!((tms.levels[0].resolution - 4000.0).abs() < 1e-9);
        assert_eq!(tms.levels[0].matrix_w, 1);
        assert_eq!(tms.levels[0].matrix_h, 1);
        assert!((tms.levels[26].resolution - 0.5).abs() < 1e-9);
        assert_eq!(tms.levels[26].matrix_w, 3750);
        assert_eq!(tms.levels[26].matrix_h, 2500);
    }
}

#[cfg(test)]
mod axis_order_tests {
    use super::*;

    /// The real OGC-registered document, trimmed to two levels. Note `orderedAxes: ["Y","X"]` and
    /// `pointOfOrigin: [5500000, 2000000]` — northing FIRST, which is what EPSG:3035 declares.
    const LAEA_OFFICIAL: &str = r#"{
      "id": "EuropeanETRS89_LAEAQuad",
      "crs": "http://www.opengis.net/def/crs/EPSG/0/3035",
      "orderedAxes": ["Y", "X"],
      "tileMatrices": [
        { "id": "0", "scaleDenominator": 62779017.857142866, "cellSize": 17578.125,
          "pointOfOrigin": [5500000.0, 2000000.0], "tileWidth": 256, "tileHeight": 256,
          "matrixWidth": 1, "matrixHeight": 1 },
        { "id": "1", "scaleDenominator": 31389508.928571433, "cellSize": 8789.0625,
          "pointOfOrigin": [5500000.0, 2000000.0], "tileWidth": 256, "tileHeight": 256,
          "matrixWidth": 2, "matrixHeight": 2 }
      ]
    }"#;

    /// ⚠ THE BUG THIS FIXES. Read naively as `[x, y]` the origin becomes x=5 500 000,
    /// y=2 000 000 — 3.5 MILLION METRES off, which serves empty tiles behind a 200 rather than
    /// failing loudly. `swissLV95` never caught it because EPSG:2056 is easting-first.
    #[test]
    fn official_laea_grid_reads_its_northing_first_origin_correctly() {
        let g = from_ogc_json(LAEA_OFFICIAL).expect("parse the OGC document");
        assert_eq!(g.crs, "EPSG:3035");
        assert_eq!(g.origin_x, 2_000_000.0, "easting is the SECOND number here");
        assert_eq!(g.origin_y, 5_500_000.0, "northing is the FIRST number here");
    }

    /// The origin is the top-left, so the grid must cover a 4 500 km square reaching DOWN and
    /// RIGHT from it — and Europe (and our EU5 extent) must land inside. This is the assertion
    /// that would fail on a swapped origin even if someone "fixed" the field order by accident.
    #[test]
    fn official_laea_grid_covers_europe() {
        let g = from_ogc_json(LAEA_OFFICIAL).expect("parse");
        let b = g.tile_bounds(0, 0, 0).expect("z0 tile exists");
        assert_eq!(b, [2_000_000.0, 1_000_000.0, 6_500_000.0, 5_500_000.0]);
        // EU5's own extent in EPSG:3035, from ST_Extent at import time.
        for (x, y) in [(3_155_046.0, 2_026_265.0), (4_673_364.0, 3_550_864.0)] {
            assert!(
                x >= b[0] && x <= b[2] && y >= b[1] && y <= b[3],
                "EU5 corner ({x}, {y}) must fall inside the grid"
            );
        }
    }

    /// An easting-first CRS must be untouched: this is the swissLV95 shape, and every grid parsed
    /// before this fix assumed it.
    #[test]
    fn an_easting_first_grid_is_unchanged() {
        let doc = r#"{
          "id": "swissLV95", "crs": "http://www.opengis.net/def/crs/EPSG/0/2056",
          "tileMatrices": [{ "id": "0", "cellSize": 4000.0,
            "pointOfOrigin": [2420000.0, 1350000.0], "tileWidth": 256, "tileHeight": 256,
            "matrixWidth": 1, "matrixHeight": 1 }]
        }"#;
        let g = from_ogc_json(doc).expect("parse");
        assert_eq!((g.origin_x, g.origin_y), (2_420_000.0, 1_350_000.0));
    }

    /// `orderedAxes` absent → fall back to the CRS's declared order, which is what the OGC spec
    /// says and what `reproj::crs_is_northing_first` answers. Same document as above minus the
    /// declaration; EPSG:3035 is northing-first, so the result must be identical.
    #[test]
    fn a_missing_ordered_axes_falls_back_to_the_crs_declaration() {
        let doc = LAEA_OFFICIAL.replace("\"orderedAxes\": [\"Y\", \"X\"],", "");
        // Without this, a reformat of the fixture makes the replace a no-op and this test quietly
        // degenerates into a copy of `official_laea_grid_reads_...` — still green, testing nothing.
        assert!(
            !doc.contains("orderedAxes"),
            "the fixture changed shape; this test is no longer removing the declaration"
        );
        let g = from_ogc_json(&doc).expect("parse");
        assert_eq!((g.origin_x, g.origin_y), (2_000_000.0, 5_500_000.0));
    }

    /// The document's own declaration WINS over the CRS registry: the numbers are what they are.
    #[test]
    fn an_explicit_ordered_axes_overrides_the_crs_declaration() {
        let doc = LAEA_OFFICIAL.replace("[\"Y\", \"X\"]", "[\"X\", \"Y\"]");
        let g = from_ogc_json(&doc).expect("parse");
        assert_eq!((g.origin_x, g.origin_y), (5_500_000.0, 2_000_000.0));
    }

    /// The published document must stay readable by a NAIVE client — one that takes
    /// `pointOfOrigin` as `[x, y]` and never looks at `orderedAxes`. That is what `xray.html` did,
    /// and mirroring the registry's northing-first byte order broke it. `WorldCRS84Quad` is the
    /// sharpest case: EPSG:4326 is latitude-first, so a CRS-derived emit order would have flipped
    /// its long-published origin from `[-180, 90]` to `[90, -180]`.
    #[test]
    fn published_grids_keep_an_x_y_origin_for_naive_clients() {
        for id in ["WorldCRS84Quad", "WebMercatorQuad"] {
            let g = preset(id, 256).expect("preset exists");
            let v = to_ogc_value(&g);
            assert_eq!(v["orderedAxes"], serde_json::json!(["X", "Y"]), "{id}");
            assert_eq!(
                v["tileMatrices"][0]["pointOfOrigin"],
                serde_json::json!([g.origin_x, g.origin_y]),
                "{id}: origin must be published x,y"
            );
        }
    }

    /// ⚠ A round-trip alone CANNOT catch a swapped origin — swapping on both read and write is
    /// self-consistent. So this asserts the round trip AND the absolute value, plus that we
    /// publish `orderedAxes` so a client never has to guess.
    #[test]
    fn emitted_json_declares_its_axis_order_and_round_trips() {
        let g = from_ogc_json(LAEA_OFFICIAL).expect("parse");
        let v = to_ogc_value(&g);
        // We publish x,y ALWAYS and declare it, even for a northing-first CRS. Spec-legal, and it
        // is what keeps a naive reader correct — mirroring the registry's byte order instead broke
        // xray.html and flipped WorldCRS84Quad's published origin. See `to_ogc_value`.
        assert_eq!(v["orderedAxes"], serde_json::json!(["X", "Y"]));
        assert_eq!(
            v["tileMatrices"][0]["pointOfOrigin"],
            serde_json::json!([2_000_000.0, 5_500_000.0]),
            "easting first, matching the declaration we just made"
        );
        let back = from_ogc_json(&serde_json::to_string(&v).unwrap()).expect("reparse");
        assert_eq!((back.origin_x, back.origin_y), (2_000_000.0, 5_500_000.0));
    }
}
