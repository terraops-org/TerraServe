// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! WMTS 1.0.0 front-end (KVP-GET + RESTful). Reuses the tile-service core with **no y-flip** — WMTS
//! `TileRow` counts from the top-left, which is the core's stored convention (contrast the TMS
//! front-end, which flips). No SOAP/POST.
//!
//! Deliberate leniencies (fine for QGIS/owslib; recorded for CITE): `STYLE` defaults to `default`;
//! no `VERSION`/`AcceptVersions` negotiation (`VersionNegotiationFailed` not implemented).

use std::collections::HashMap;

use crate::server::ServeState;
use crate::tms::{strip_size_suffix, TileMatrixSet};

#[derive(Debug)]
pub enum WmtsRequest {
    GetCapabilities,
    GetTile {
        layer: String,
        style: String,
        tms: String,
        z: u32,
        row: u32,
        col: u32,
        /// The requested TILE format — `image/png` (default) or, for a vector layer, the MVT
        /// content type (Task 5 — WMTS-MVT). Only these two values reach here (see `parse_kvp`).
        format: String,
    },
    GetFeatureInfo {
        layer: String,
        style: String,
        tms: String,
        z: u32,
        row: u32,
        col: u32,
        i: u32,
        j: u32,
        info_format: String,
    },
    Exception {
        code: String,
        text: String,
        locator: Option<String>,
    },
}

/// A GetTile failure carrying its OWS exception code. The KVP binding renders it as an
/// `ows:ExceptionReport`; the RESTful binding uses `http` directly (bare status).
#[derive(Debug)]
pub struct WmtsErr {
    pub http: u16,
    pub code: String,
    pub text: String,
    pub locator: Option<String>,
}

/// The WMTS TILE format for MVT (Task 5 — WMTS-MVT).
pub const MVT_FORMAT: &str = "application/vnd.mapbox-vector-tile";

fn kvp(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.to_ascii_lowercase(), crate::wms::percent_decode(v)))
        .collect()
}

/// Parse a WMTS KVP query into a request (or an OWS exception describing the fault).
pub fn parse_kvp(query: &str) -> WmtsRequest {
    let m = kvp(query);
    // SERVICE, when present, must be WMTS.
    if let Some(s) = m.get("service") {
        if !s.eq_ignore_ascii_case("WMTS") {
            return WmtsRequest::Exception {
                code: "InvalidParameterValue".into(),
                text: format!("SERVICE '{s}'"),
                locator: Some("SERVICE".into()),
            };
        }
    }
    let req = m.get("request").map(|s| s.as_str()).unwrap_or("");
    if req.eq_ignore_ascii_case("GetCapabilities") {
        return WmtsRequest::GetCapabilities;
    }
    let is_tile = req.eq_ignore_ascii_case("GetTile");
    let is_fi = req.eq_ignore_ascii_case("GetFeatureInfo");
    if !is_tile && !is_fi {
        return WmtsRequest::Exception {
            code: "OperationNotSupported".into(),
            text: format!("REQUEST '{req}'"),
            locator: Some("REQUEST".into()),
        };
    }
    // FORMAT (the TILE format) default image/png; also accept the MVT content type on GetTile
    // (Task 5 — WMTS-MVT, only meaningful for a vector layer). Reject anything else. Applies to
    // both operations (GetFeatureInfo just never reads it back).
    let format = m
        .get("format")
        .cloned()
        .unwrap_or_else(|| "image/png".into());
    if !format.eq_ignore_ascii_case("image/png") && !format.eq_ignore_ascii_case(MVT_FORMAT) {
        return WmtsRequest::Exception {
            code: "InvalidParameterValue".into(),
            text: format!("FORMAT '{format}'"),
            locator: Some("FORMAT".into()),
        };
    }
    macro_rules! need {
        ($k:expr) => {
            match m.get($k) {
                Some(v) => v.clone(),
                None => {
                    return WmtsRequest::Exception {
                        code: "MissingParameterValue".into(),
                        text: $k.to_uppercase(),
                        locator: Some($k.to_uppercase()),
                    }
                }
            }
        };
    }
    let layer = need!("layer");
    let style = m
        .get("style")
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| "default".into());
    let tms = need!("tilematrixset");
    let zs = need!("tilematrix");
    let rows = need!("tilerow");
    let cols = need!("tilecol");
    let (z, row, col) = match (zs.parse::<u32>(), rows.parse::<u32>(), cols.parse::<u32>()) {
        (Ok(z), Ok(row), Ok(col)) => (z, row, col),
        _ => {
            return WmtsRequest::Exception {
                code: "InvalidParameterValue".into(),
                text: "TILEMATRIX/TILEROW/TILECOL must be integers".into(),
                locator: Some("TILEMATRIX".into()),
            }
        }
    };
    if is_tile {
        return WmtsRequest::GetTile {
            layer,
            style,
            tms,
            z,
            row,
            col,
            format,
        };
    }
    // GetFeatureInfo: the tile above + the in-tile pixel (I,J) + the INFOFORMAT.
    let is = need!("i");
    let js = need!("j");
    let (i, j) = match (is.parse::<u32>(), js.parse::<u32>()) {
        (Ok(i), Ok(j)) => (i, j),
        _ => {
            return WmtsRequest::Exception {
                code: "InvalidParameterValue".into(),
                text: "I/J must be integers".into(),
                locator: Some("I".into()),
            }
        }
    };
    let info_format = m
        .get("infoformat")
        .cloned()
        .unwrap_or_else(|| "text/plain".into());
    WmtsRequest::GetFeatureInfo {
        layer,
        style,
        tms,
        z,
        row,
        col,
        i,
        j,
        info_format,
    }
}

/// Render a WMTS tile. **No y-flip** — WMTS `TileRow`/`TileCol` are the core's top-left row/col.
/// Byte-identical to `TileFactory::render_tile(grid, z, col, row)`. Errors carry an OWS code.
pub fn get_tile(
    state: &crate::server::ServeState,
    layer: &str,
    style: &str,
    tms: &str,
    z: u32,
    row: u32,
    col: u32,
) -> Result<Vec<u8>, WmtsErr> {
    let ipv = |text: String, loc: &str| WmtsErr {
        http: 400,
        code: "InvalidParameterValue".into(),
        text,
        locator: Some(loc.into()),
    };
    let l = state
        .layers
        .iter()
        .find(|l| l.name == layer)
        .ok_or_else(|| ipv(format!("no layer '{layer}'"), "LAYER"))?;
    if style != "default" && !style.is_empty() {
        return Err(ipv(format!("no style '{style}'"), "STYLE"));
    }
    // Published grids only — no `tms::preset` fallback, unlike the MVT routes. A raster tile served
    // on a grid GetCapabilities cannot advertise would be undiscoverable, so publishing is what makes
    // a grid servable here. That trips up vector layers in particular (`grids:` defaults to
    // `from_cog`, dropped for a layer with no COG), and the SAME id works over `/mvt` via its preset
    // fallback — so say what to add rather than just "no TileMatrixSet".
    let pg = l
        .grids
        .iter()
        .find(|g| g.tms.id == tms || crate::tms::strip_size_suffix(&g.tms.id) == tms)
        .ok_or_else(|| {
            let hint = if l.grids.is_empty() {
                format!(
                    " (layer '{layer}' publishes no tile grids; add e.g. `grids: [{tms}]` to it)"
                )
            } else {
                String::new()
            };
            ipv(format!("no TileMatrixSet '{tms}'{hint}"), "TILEMATRIXSET")
        })?;
    let lvl = pg
        .tms
        .level(z)
        .ok_or_else(|| ipv(format!("no TileMatrix {z}"), "TILEMATRIX"))?;
    if col >= lvl.matrix_w || row >= lvl.matrix_h {
        return Err(WmtsErr {
            http: 400,
            code: "TileOutOfRange".into(),
            text: format!("tile {z}/{row}/{col} out of range"),
            locator: Some("TILEROW".into()),
        });
    }
    // A vector layer rasters through the ONE vector renderer, shared with WMS GetMap and with the
    // TMS front-end (`VectorLayer::render_tile`) — geometry only, no labels
    // (docs/postgis-layers.md, "Labels are WMS-only"). The range checks above already ran, so a
    // failure below is a render/read error, not a bad request.
    if let Some(vec) = &l.vector {
        // Archive-first (opt-in `--raster-pmtiles`), through the SAME read-through the TMS
        // front-end uses — a hit is a disk read of a pre-baked PNG, a miss renders live below.
        // WMTS is already top-left, so (z, col, row) is the addressing the bake wrote.
        if let Some(png) = crate::tms_http::read_through_raster(l, &pg.tms.id, z, col, row) {
            return Ok(png);
        }
        return vec
            .render_tile(&l.src_crs, &pg.tms, z, col, row)
            .map_err(|e| WmtsErr {
                http: 500,
                code: "NoApplicableCode".into(),
                text: e,
                locator: None,
            });
    }
    // Neither a vector layer nor a COG one — a `Layer` with both unset never leaves layer building.
    let cog = l.cog.as_deref().ok_or(WmtsErr {
        http: 500,
        code: "NoApplicableCode".into(),
        text: "layer has neither a COG nor a vector source".into(),
        locator: None,
    })?;
    let source = l.source.as_deref().ok_or(WmtsErr {
        http: 500,
        code: "NoApplicableCode".into(),
        text: "layer has no source".into(),
        locator: None,
    })?;
    let style = l.style.as_ref().ok_or(WmtsErr {
        http: 500,
        code: "NoApplicableCode".into(),
        text: "layer has no style".into(),
        locator: None,
    })?;
    // WMTS is top-left: TileRow == core row, TileCol == core col. NO y-flip.
    let req = crate::tms::TileRequest {
        cog,
        source,
        cog_path: &l.cog_path,
        src_crs: &l.src_crs,
        style,
        band_math: l.band_math.as_ref(),
        cache: l.tile_cache.as_ref(),
        index_cache: &l.index_cache,
        data_bounds: pg.data_bounds,
        grid: &pg.tms,
        z,
        col,
        row,
    };
    crate::tms::TileFactory::render_tile(&req).map_err(|e| WmtsErr {
        http: 500,
        code: "NoApplicableCode".into(),
        text: e,
        locator: None,
    })
}

/// Render a WMTS-MVT tile (`FORMAT=application/vnd.mapbox-vector-tile`, Task 5): same `{tms}/{z}/
/// {row}/{col}` addressing as `get_tile` (no y-flip), but for a **vector** layer, encoded via
/// `vector::mvt::encode_tile` instead of the raster render path. `{tms}` resolves against the
/// layer's OWN published grids first (Task 2/3 — `server::Layer::grids` IS populated for vector
/// layers too), falling back to the MVT preset grid (`crate::tms::preset`, the encoder's fixed
/// 4096-unit local extent) for the 4 built-ins — same lookup-then-preset order as `get_tile` above
/// and `mvt_http::resolve_grid`.
pub fn get_tile_mvt(
    state: &crate::server::ServeState,
    layer: &str,
    style: &str,
    tms: &str,
    z: u32,
    row: u32,
    col: u32,
) -> Result<Vec<u8>, WmtsErr> {
    let ipv = |text: String, loc: &str| WmtsErr {
        http: 400,
        code: "InvalidParameterValue".into(),
        text,
        locator: Some(loc.into()),
    };
    let l = state
        .layers
        .iter()
        .find(|l| l.name == layer)
        .ok_or_else(|| ipv(format!("no layer '{layer}'"), "LAYER"))?;
    if style != "default" && !style.is_empty() {
        return Err(ipv(format!("no style '{style}'"), "STYLE"));
    }
    let vector = l.vector.as_ref().ok_or(WmtsErr {
        http: 400,
        code: "OperationNotSupported".into(),
        text: "layer is not a vector layer — MVT requires --vector".into(),
        locator: None,
    })?;
    // Task 3: the layer's OWN published grids (Task 2's `l.grids`, e.g. a custom `--grid`/`--config`
    // grid) first — mirrors `get_tile`'s raster grid lookup above EXACTLY (asymmetric: only the
    // stored id is stripped, compared against the raw request id, so an explicit `_{px}` suffix on
    // the request never matches a bare-id stored grid — per R3 it falls through to the preset's own
    // suffix handling) — falling back to the MVT preset (4096-unit local extent) for the 4 built-ins,
    // matching `mvt_http::resolve_grid`.
    let grid = if let Some(g) = l
        .grids
        .iter()
        .find(|g| g.tms.id == tms || strip_size_suffix(&g.tms.id) == tms)
    {
        g.tms.clone()
    } else {
        crate::tms::preset(tms, 4096)
            .ok_or_else(|| ipv(format!("no TileMatrixSet '{tms}'"), "TILEMATRIXSET"))?
    };
    let lvl = grid
        .level(z)
        .ok_or_else(|| ipv(format!("no TileMatrix {z}"), "TILEMATRIX"))?;
    if col >= lvl.matrix_w || row >= lvl.matrix_h {
        return Err(WmtsErr {
            http: 400,
            code: "TileOutOfRange".into(),
            text: format!("tile {z}/{row}/{col} out of range"),
            locator: Some("TILEROW".into()),
        });
    }
    // The optimization set for this layer — built IDENTICALLY to the `/mvt` XYZ route
    // (mvt_http::render_mvt_tile) via the shared `for_layer` constructor, so the SAME z/x/y is
    // byte-identical whether fetched via `/mvt` or WMTS GetTile.
    let opts = crate::vector::mvt::MvtOptimizations::for_layer(state, vector);
    // Per-zoom LOD: pick the zoom-appropriate pool (matches the /mvt route).
    let vs = vector.source_for_zoom(z);
    // Reads through the `VectorSource` seam (windowed-seam refactor): reproject the tile bbox into
    // the source CRS before reading — a harmless no-op for `LoadAll`, correct once a windowed source
    // lands. `col`/`row` are x/y (see the comment below).
    // A failed source read is a 500, not an empty tile — the same rule the `/mvt` route applies.
    // Encoding whatever a broken query returned would emit a valid, EMPTY vector tile behind a 200.
    let batch = crate::vector::mvt::features_for_tile(&vs, &grid, z, col, row, &l.src_crs, &opts)
        .map_err(|e| WmtsErr {
        http: 500,
        code: "NoApplicableCode".into(),
        text: e,
        locator: None,
    })?;
    // Same `layer/tms/z/x/y` key as the `/mvt` route (x=col, y=row) → they share cache entries.
    Ok(crate::mvt_http::cached_or_encode(
        state,
        &l.name,
        tms,
        z,
        col,
        row,
        || {
            crate::vector::mvt::encode_tile_opt(
                batch.as_slice(),
                &grid,
                z,
                col,
                row,
                &l.src_crs,
                &l.name,
                &opts,
            )
        },
    ))
}

/// WMTS GetFeatureInfo: the value at the in-tile pixel `(i,j)` of tile `(z,row,col)`. Reuses the
/// exact-value `render::sample_point` (level-0, lossless) over the tile's bbox + the shared WMS
/// formatters. Returns `(body, content_type)`. `Err((status, msg))` for a bad request.
#[allow(clippy::too_many_arguments)]
pub fn get_feature_info(
    state: &ServeState,
    layer: &str,
    style: &str,
    tms: &str,
    z: u32,
    row: u32,
    col: u32,
    i: u32,
    j: u32,
    info_format: &str,
) -> Result<(Vec<u8>, String), (u16, String)> {
    let l = state
        .layers
        .iter()
        .find(|l| l.name == layer)
        .ok_or((404u16, format!("no layer '{layer}'")))?;
    if style != "default" && !style.is_empty() {
        return Err((404, format!("no style '{style}'")));
    }
    let pg = l
        .grids
        .iter()
        .find(|g| g.tms.id == tms || strip_size_suffix(&g.tms.id) == tms)
        .ok_or((404u16, format!("no TileMatrixSet '{tms}'")))?;
    let bbox = pg
        .tms
        .tile_bounds(z, col, row)
        .ok_or((404u16, format!("tile {z}/{row}/{col} out of range")))?;
    if i >= pg.tms.tile_w || j >= pg.tms.tile_h {
        return Err((
            400,
            format!("I/J outside the {}x{} tile", pg.tms.tile_w, pg.tms.tile_h),
        ));
    }
    // The in-tile pixel maps onto the tile's bbox exactly as a WMS pixel maps onto a GetMap bbox.
    let ir = crate::render::InfoRequest {
        bbox,
        crs: &pg.tms.crs,
        src_crs: &l.src_crs,
        width: pg.tms.tile_w,
        height: pg.tms.tile_h,
        i,
        j,
        band_math: l.band_math.as_ref(),
    };
    // GetFeatureInfo is still raster-only. A vector layer now SERVES tiles here (`get_tile` above),
    // but identifying a feature under a pixel is a different operation: `wms::get_feature_info_vector`
    // owns it and is driven by the WMS KVP closure, so reusing it from this binding is a refactor,
    // not a wiring change. Deliberately left undone — say what to use instead rather than implying
    // the layer is untileable.
    let cog = l.cog.as_deref().ok_or((
        400u16,
        "GetFeatureInfo is not implemented for vector layers on the WMTS path — use WMS \
         GetFeatureInfo (the layer's WMTS tiles are unaffected)"
            .to_string(),
    ))?;
    let source = l
        .source
        .as_deref()
        .ok_or((500u16, "layer has no source".to_string()))?;
    let info = crate::render::sample_point_with_cog(&ir, cog, source, &l.index_cache)
        .map_err(|e| (500u16, e))?;
    Ok(crate::wms::format_feature_info(info_format, &l.name, &info))
}

/// XML-escape a dynamic value for interpolation into a document (`&` first).
/// Thin alias for the shared escaper (`crate::xml::escape`), kept so this module's call
/// sites (and `wms.rs`, which reaches in here in two places) read unchanged. It used to
/// cover `& < > "` while `wms::xml_escape` covered only `& < >`; having two that differed
/// is what let the weaker one end up building an attribute value. See `crate::xml`.
pub fn escape_xml(s: &str) -> String {
    crate::xml::escape(s)
}

/// An `ows:ExceptionReport` (OWS 1.1). `version="1.1.0"`.
pub fn exception_xml(code: &str, text: &str, locator: Option<&str>) -> String {
    let loc = locator
        .map(|l| format!(" locator=\"{}\"", escape_xml(l)))
        .unwrap_or_default();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ows:ExceptionReport xmlns:ows=\"http://www.opengis.net/ows/1.1\" version=\"1.1.0\">\n  <ows:Exception exceptionCode=\"{code}\"{loc}>\n    <ows:ExceptionText>{text}</ows:ExceptionText>\n  </ows:Exception>\n</ows:ExceptionReport>\n",
        code = escape_xml(code),
        text = escape_xml(text),
    )
}

/// Derive `(kvp_base, rest_base)` from the advertised WMS base (`…/wms` → `…/wmts`, `…/wmts/1.0.0`).
/// The WMTS KVP and RESTful bases, composed on an ALREADY-DERIVED origin.
///
/// This used to take `base_url` and derive the origin itself, with logic byte-identical to
/// `tms_http::tms_root`'s copy, which is why WMTS could not honour `X-Forwarded-Proto`: it
/// never saw the request. Derivation now lives in exactly one place,
/// `ServeState::advertised_origin`, and this only appends the paths.
pub fn bases(origin: &str) -> (String, String) {
    (format!("{origin}/wmts"), format!("{origin}/wmts/1.0.0"))
}

// ---- GetCapabilities ------------------------------------------------------------------------

fn crs_urn(crs: &str) -> String {
    match crs.to_ascii_uppercase().strip_prefix("EPSG:") {
        Some(code) => format!("urn:ogc:def:crs:EPSG::{code}"),
        None => crs.to_string(),
    }
}

/// True if the CRS declares NORTHING (or latitude) as its first axis, so `TopLeftCorner` must be
/// written `Y X` rather than `X Y`.
///
/// ⚠ Unlike the TMS 2.0 JSON document, WMTS capabilities has NO `orderedAxes` escape hatch: the
/// `SupportedCRS` URN alone tells the client what order to read, and an axis-aware client (QGIS's
/// native WMTS is one) inverts per CRS accordingly. So here we have to match the CRS, not pick a
/// convenient order.
///
/// This used to hardcode `EPSG:4326` with a comment claiming "projected CRSs are X,Y". That is
/// simply false: EPSG:3035 (the EU LAEA grid), 2180 (Poland), 3301 (Estonia) and the German
/// Gauss-Krüger zones are all projected AND northing-first. Latent while every published grid was
/// easting-first; live the moment the LAEA grid ships, and the symptom would have been blank tiles
/// behind a 200 in the exact client we tell people to use. Now asks PROJ through the same helper
/// `wms.rs` uses for the 1.3.0 axis flip, so the two can never disagree.
fn axis_is_latlon(crs: &str) -> bool {
    crate::reproj::crs_is_northing_first(crs).unwrap_or(false)
}

fn top_left_corner(grid: &TileMatrixSet) -> String {
    if axis_is_latlon(&grid.crs) {
        format!("{} {}", grid.origin_y, grid.origin_x) // northing/lat first (Y X)
    } else {
        format!("{} {}", grid.origin_x, grid.origin_y) // X Y
    }
}

/// The OGC WellKnownScaleSet URN for the canonical 256-px presets (hint only in 1.0.0); else None.
fn well_known_scale_set(grid: &TileMatrixSet) -> Option<&'static str> {
    match strip_size_suffix(&grid.id) {
        "WebMercatorQuad" if grid.tile_w == 256 => {
            Some("http://www.opengis.net/def/wkss/OGC/1.0/GoogleMapsCompatible")
        }
        "WorldCRS84Quad" if grid.tile_w == 256 => {
            Some("http://www.opengis.net/def/wkss/OGC/1.0/GoogleCRS84Quad")
        }
        _ => None,
    }
}

/// A `<TileMatrixSet>` (Contents). No `cellSize` in 1.0.0; TileMatrix children in schema order.
pub fn tile_matrix_set_xml(grid: &TileMatrixSet) -> String {
    let wkss = well_known_scale_set(grid)
        .map(|u| format!("    <WellKnownScaleSet>{u}</WellKnownScaleSet>\n"))
        .unwrap_or_default();
    let mut lv = String::new();
    for l in &grid.levels {
        lv.push_str(&format!(
            "    <TileMatrix>\n      <ows:Identifier>{z}</ows:Identifier>\n      <ScaleDenominator>{sd}</ScaleDenominator>\n      <TopLeftCorner>{tlc}</TopLeftCorner>\n      <TileWidth>{tw}</TileWidth>\n      <TileHeight>{th}</TileHeight>\n      <MatrixWidth>{mw}</MatrixWidth>\n      <MatrixHeight>{mh}</MatrixHeight>\n    </TileMatrix>\n",
            z = l.z,
            sd = grid.scale_denominator(l.z),
            tlc = top_left_corner(grid),
            tw = grid.tile_w,
            th = grid.tile_h,
            mw = l.matrix_w,
            mh = l.matrix_h,
        ));
    }
    format!(
        "  <TileMatrixSet>\n    <ows:Identifier>{id}</ows:Identifier>\n    <ows:SupportedCRS>{crs}</ows:SupportedCRS>\n{wkss}{lv}  </TileMatrixSet>\n",
        id = escape_xml(&grid.id),
        crs = crs_urn(&grid.crs),
    )
}

/// A `<TileMatrixSetLink>` with `<TileMatrixSetLimits>` (row-first order) from the layer's
/// `data_bounds` in the grid CRS. Limits omitted when `data_bounds` is None.
fn tile_matrix_set_link(grid: &TileMatrixSet, data_bounds: Option<[f64; 4]>) -> String {
    let limits = match data_bounds {
        Some(b) => {
            let mut inner = String::new();
            for l in &grid.levels {
                if let Some((mincol, maxcol, minrow, maxrow)) = grid.tile_limits(b, l.z) {
                    inner.push_str(&format!(
                        "        <TileMatrixLimits>\n          <TileMatrix>{z}</TileMatrix>\n          <MinTileRow>{minrow}</MinTileRow>\n          <MaxTileRow>{maxrow}</MaxTileRow>\n          <MinTileCol>{mincol}</MinTileCol>\n          <MaxTileCol>{maxcol}</MaxTileCol>\n        </TileMatrixLimits>\n",
                        z = l.z,
                    ));
                }
            }
            format!("      <TileMatrixSetLimits>\n{inner}      </TileMatrixSetLimits>\n")
        }
        None => String::new(),
    };
    format!(
        "    <TileMatrixSetLink>\n      <TileMatrixSet>{id}</TileMatrixSet>\n{limits}    </TileMatrixSetLink>\n",
        id = escape_xml(&grid.id),
    )
}

/// The WMTS 1.0.0 `Capabilities` (ServiceMetadata) document. Both bindings are advertised: KVP via
/// `OperationsMetadata` `Get` + a `GetEncoding=KVP` constraint, RESTful via the per-layer `ResourceURL`
/// template. `kvp_base` e.g. `http://h/wmts`, `rest_base` e.g. `http://h/wmts/1.0.0`.
pub fn capabilities_xml(state: &ServeState, kvp_base: &str, rest_base: &str) -> String {
    let op = |name: &str| {
        format!(
            "    <ows:Operation name=\"{name}\">\n      <ows:DCP><ows:HTTP><ows:Get xlink:href=\"{kvp_base}?\">\n        <ows:Constraint name=\"GetEncoding\"><ows:AllowedValues><ows:Value>KVP</ows:Value></ows:AllowedValues></ows:Constraint>\n      </ows:Get></ows:HTTP></ows:DCP>\n    </ows:Operation>\n"
        )
    };

    let mut layers_xml = String::new();
    for l in &state.layers {
        // Vector layers are advertised too: `get_tile` rasters them (`VectorLayer::render_tile`),
        // so the <ResourceURL> below resolves to a real PNG. It used to skip them because every
        // vector GetTile 400'd, and advertising an always-400 endpoint is worse than silence — but
        // now the opposite holds: a tile endpoint that works and is not advertised is invisible to
        // QGIS, which builds its layer list from this document.
        //
        // A layer publishing NO grids is still skipped, vector or raster: 0 <TileMatrixSetLink> is
        // schema-invalid, and there is genuinely no grid to serve on. Advertised == servable.
        if l.grids.is_empty() {
            continue;
        }
        let [w, s, e, n] = l.bounds_wgs84;
        let mut links = String::new();
        for pg in &l.grids {
            links.push_str(&tile_matrix_set_link(&pg.tms, pg.data_bounds));
        }
        // <InfoFormat> is 0..* — omit it for a vector layer, whose GetFeatureInfo is raster-only
        // (see `get_feature_info`). Advertising the formats would offer an operation that 400s.
        let info_formats = if l.vector.is_some() {
            String::new()
        } else {
            "    <InfoFormat>text/plain</InfoFormat>\n    <InfoFormat>application/json</InfoFormat>\n    <InfoFormat>text/html</InfoFormat>\n".to_string()
        };
        layers_xml.push_str(&format!(
            "  <Layer>\n    <ows:Title>{title}</ows:Title>\n    <ows:WGS84BoundingBox>\n      <ows:LowerCorner>{w} {s}</ows:LowerCorner>\n      <ows:UpperCorner>{e} {n}</ows:UpperCorner>\n    </ows:WGS84BoundingBox>\n    <ows:Identifier>{id}</ows:Identifier>\n    <Style isDefault=\"true\">\n      <ows:Identifier>default</ows:Identifier>\n    </Style>\n    <Format>image/png</Format>\n{info_formats}{links}    <ResourceURL format=\"image/png\" resourceType=\"tile\" template=\"{rest_base}/{id}/{{style}}/{{TileMatrixSet}}/{{TileMatrix}}/{{TileRow}}/{{TileCol}}.png\"/>\n  </Layer>\n",
            title = escape_xml(&l.name),
            id = escape_xml(&l.name),
        ));
    }

    // One <TileMatrixSet> per DISTINCT grid id across all layers (from_cog ids are per-layer-unique).
    let mut seen = std::collections::BTreeSet::new();
    let mut tms_xml = String::new();
    for l in &state.layers {
        for pg in &l.grids {
            if seen.insert(pg.tms.id.clone()) {
                tms_xml.push_str(&tile_matrix_set_xml(&pg.tms));
            }
        }
    }

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Capabilities xmlns=\"http://www.opengis.net/wmts/1.0\" xmlns:ows=\"http://www.opengis.net/ows/1.1\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" version=\"1.0.0\">\n  <ows:ServiceIdentification>\n    <ows:Title>TerraServe WMTS</ows:Title>\n    <ows:ServiceType>OGC WMTS</ows:ServiceType>\n    <ows:ServiceTypeVersion>1.0.0</ows:ServiceTypeVersion>\n  </ows:ServiceIdentification>\n  <ows:ServiceProvider>\n    <ows:ProviderName>TerraServe</ows:ProviderName>\n  </ows:ServiceProvider>\n  <ows:OperationsMetadata>\n{getcap}{gettile}{getfi}  </ows:OperationsMetadata>\n  <Contents>\n{layers_xml}{tms_xml}  </Contents>\n  <ServiceMetadataURL xlink:href=\"{rest_base}/WMTSCapabilities.xml\"/>\n</Capabilities>\n",
        getcap = op("GetCapabilities"),
        gettile = op("GetTile"),
        getfi = op("GetFeatureInfo"),
    )
}

#[cfg(test)]
mod topleftcorner_axis_tests {
    use super::top_left_corner;
    use crate::tms;

    /// ⚠ WMTS capabilities has NO `orderedAxes`: the `SupportedCRS` URN alone tells the client the
    /// axis order, and an axis-aware client (QGIS's native WMTS) inverts per CRS. So a
    /// northing-first CRS MUST publish `TopLeftCorner` as `Y X` or the client lands 3.5 million
    /// metres away and gets blank tiles behind a 200.
    ///
    /// This used to be hardcoded to `EPSG:4326` under the false claim that "projected CRSs are
    /// X,Y" — EPSG:3035, 2180 and 3301 are all projected AND northing-first.
    #[test]
    fn a_northing_first_projected_grid_publishes_y_x() {
        let doc = r#"{
          "id": "EuropeanETRS89_LAEAQuad", "crs": "http://www.opengis.net/def/crs/EPSG/0/3035",
          "orderedAxes": ["Y", "X"],
          "tileMatrices": [{ "id": "0", "cellSize": 17578.125,
            "pointOfOrigin": [5500000.0, 2000000.0], "tileWidth": 256, "tileHeight": 256,
            "matrixWidth": 1, "matrixHeight": 1 }]
        }"#;
        let g = tms::from_ogc_json(doc).expect("parse");
        assert_eq!((g.origin_x, g.origin_y), (2_000_000.0, 5_500_000.0));
        assert_eq!(top_left_corner(&g), "5500000 2000000", "northing first");
    }

    /// The easting-first majority is untouched — including swissLV95, the one custom grid shipped
    /// before this, whose capabilities must not move.
    #[test]
    fn easting_first_grids_still_publish_x_y() {
        for (id, px) in [("WebMercatorQuad", 256u32), ("swissLV95", 0)] {
            let g = if px > 0 {
                tms::preset(id, px).expect("preset")
            } else {
                tms::from_ogc_json(
                    &std::fs::read_to_string("fixtures/grids/swissLV95.json").unwrap(),
                )
                .expect("swissLV95")
            };
            assert_eq!(
                top_left_corner(&g),
                format!("{} {}", g.origin_x, g.origin_y),
                "{id} must stay X Y"
            );
        }
    }

    /// EPSG:4326 stays lat,lon — the behaviour the old hardcoded check got right, and the one CITE
    /// conformance depends on.
    #[test]
    fn wgs84_still_publishes_lat_lon() {
        let g = tms::preset("WorldCRS84Quad", 256).expect("preset");
        assert_eq!(g.crs, "EPSG:4326");
        assert_eq!(
            top_left_corner(&g),
            format!("{} {}", g.origin_y, g.origin_x)
        );
    }
}
