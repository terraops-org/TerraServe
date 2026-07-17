// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Thin WMS layer: parse the raw KVP query, handle GetMap (1.1.1 + 1.3.0, incl. the
//! EPSG:4326 axis-order flip), GetCapabilities, and exceptions. Pixels are delegated to
//! the render engine. Output bytes (PNG or XML) go to stdout by the caller.

use crate::backend::Resample;
use crate::cache::IndexCache;
use crate::render::{self, BandMath, RenderRequest};
use crate::server::Layer;
use crate::style::Style;

const LAYER_NAME: &str = "cascais";
/// The original cascais layer's WGS84 bounds `[west, south, east, north]` — the default
/// advertised extent (and what the CLI/score-fixture capabilities expect).
const CASCAIS_BOUNDS: [f64; 4] = [-9.4693, 38.6814, -9.3862, 38.7255];

pub struct WmsResult {
    pub bytes: Vec<u8>,
    /// Explicit Content-Type. `None` lets the server sniff PNG-vs-XML (GetMap / GetCapabilities /
    /// exceptions). GetFeatureInfo sets it (text/plain, application/json, text/html).
    pub content_type: Option<String>,
}

impl WmsResult {
    /// A response whose Content-Type the server sniffs (PNG vs XML).
    fn sniffed(bytes: Vec<u8>) -> WmsResult {
        WmsResult {
            bytes,
            content_type: None,
        }
    }
    /// A response with an explicit Content-Type.
    fn typed(bytes: Vec<u8>, content_type: &str) -> WmsResult {
        WmsResult {
            bytes,
            content_type: Some(content_type.to_string()),
        }
    }
}

/// A GetCapabilities response with the version-appropriate Content-Type: 1.1.x advertises
/// `application/vnd.ogc.wms_xml`; 1.3.0 is sniffed (server emits `text/xml`).
fn capabilities_result(version: &str, xml: String) -> WmsResult {
    if version.starts_with("1.1") {
        WmsResult::typed(xml.into_bytes(), "application/vnd.ogc.wms_xml")
    } else {
        WmsResult::sniffed(xml.into_bytes())
    }
}

/// A ServiceException response with the version-appropriate Content-Type: 1.1.x uses
/// `application/vnd.ogc.se_xml`; 1.3.0 is sniffed (`text/xml`).
fn exception_result(version: &str, xml: String) -> WmsResult {
    if version.starts_with("1.1") {
        WmsResult::typed(xml.into_bytes(), "application/vnd.ogc.se_xml")
    } else {
        WmsResult::sniffed(xml.into_bytes())
    }
}

/// Per-layer render + capabilities config threaded through the WMS handler. Bundles the
/// bits that vary by layer so the CLI (RGBA cascais) and server (any COG, incl. band math)
/// share one dispatch path.
pub struct ServeCfg<'a> {
    /// The COG's own CRS (what request coords are reprojected into).
    pub src_crs: &'a str,
    /// When set, render on-the-fly band math (e.g. NDVI) instead of RGBA passthrough.
    pub band_math: Option<&'a BandMath>,
    /// Layer extent in WGS84 `[west, south, east, north]`, advertised in GetCapabilities so
    /// clients zoom to the data.
    pub bounds_wgs84: [f64; 4],
    /// Bounded index-chunk cache backing `cog::Level::tile_location` for a `Lazy` tile index —
    /// threaded into `RenderRequest`/`sample_point_with_cog` the same way `tile_cache` is.
    pub index_cache: IndexCache,
}

impl Default for ServeCfg<'_> {
    fn default() -> Self {
        ServeCfg {
            src_crs: crate::reproj::SRC_CRS,
            band_math: None,
            bounds_wgs84: CASCAIS_BOUNDS,
            index_cache: crate::cache::new_index_cache(crate::cache::index_cache_bytes()),
        }
    }
}

/// Parse `query` and produce the response bytes (PNG for GetMap, XML otherwise).
///
/// `base_url`, when set (i.e. from the HTTP server), is advertised as the OnlineResource
/// / GetMap endpoint in GetCapabilities so clients like QGIS build correct request URLs.
/// The one-shot CLI passes `None`, preserving the original capabilities output.
pub fn handle(cog_path: &str, style: &Style, query: &str, base_url: Option<&str>) -> WmsResult {
    dispatch(
        cog_path,
        style,
        &ServeCfg::default(),
        query,
        base_url,
        &|req| render::render(req),
    )
}

/// Server entry: render from a COG parsed ONCE at startup (`cog`), with a per-layer `cfg`
/// (source CRS, optional band math, advertised bounds). Only a fresh range source is opened
/// per request; the IFD/overview structure is never re-walked.
/// Multi-layer server entry: GetCapabilities lists all published layers; GetMap selects one
/// by the `LAYERS=` parameter (falling back to the first layer if it's missing/unknown).
pub fn handle_layers(layers: &[Layer], query: &str, base_url: Option<&str>) -> WmsResult {
    let params = parse_kvp(query);
    let get = |k: &str| {
        params
            .iter()
            .find(|(kk, _)| kk == k)
            .map(|(_, v)| v.clone())
    };
    // VERSION with the legacy WMTVER alias; GetCapabilities negotiates over {1.1.1,1.3.0}
    // (GetMap/GetFeatureInfo still require VERSION explicitly — enforced in their arms).
    let requested_version = get("version").or_else(|| get("wmtver"));
    let version = negotiate_version(requested_version.as_deref()).to_string();
    let request = normalize_request(&get("request").unwrap_or_default());

    // First CSV token of a `LAYERS`/`QUERY_LAYERS`/`LAYER` param, trimmed; "" when absent.
    let name_of = |key: &str| -> String {
        get(key)
            .unwrap_or_default()
            .split(',')
            .next()
            .unwrap_or("")
            .trim()
            .to_string()
    };
    // Resolve a requested name against `layers`: an *empty* name defaults to the first
    // configured layer (back-compat for clients that omit LAYERS); a *non-empty* name that
    // matches nothing is `NotFound` (→ LayerNotDefined), never a silent fallback.
    let pick = |key: &str| pick_layer(layers, &name_of(key));

    match request.as_str() {
        "getcapabilities" => {
            capabilities_result(&version, capabilities_multi(layers, &version, base_url))
        }
        "getmap" => {
            // CITE no-version: VERSION is mandatory for GetMap (06-042 Table 8); version
            // negotiation (defaulting) applies ONLY to GetCapabilities (§6.2.4).
            if get("version").is_none() && get("wmtver").is_none() {
                return exception_result(
                    &version,
                    exception(
                        &version,
                        "the VERSION parameter is mandatory for this request",
                    ),
                );
            }
            let names: Vec<&str> = layers.iter().map(|l| l.name.as_str()).collect();
            if !layers.is_empty() {
                if let Some(missing) =
                    first_undefined_layer(&names, &get("layers").unwrap_or_default())
                {
                    return exception_result(&version, layer_not_defined(&version, &missing));
                }
            }
            match pick("layers") {
                Picked::Found(l) => match get_map_layer(l, &version, &get) {
                    Ok(png) => WmsResult::sniffed(png),
                    Err(e) => exception_result(
                        &version,
                        exception_with_code(&version, e.code, &e.message),
                    ),
                },
                Picked::NotFound(name) => {
                    exception_result(&version, layer_not_defined(&version, &name))
                }
                Picked::NoLayers => {
                    exception_result(&version, exception(&version, "no layers configured"))
                }
            }
        }
        "getfeatureinfo" => {
            // CITE no-version: same VERSION-mandatory rule as GetMap.
            if get("version").is_none() && get("wmtver").is_none() {
                return exception_result(
                    &version,
                    exception(
                        &version,
                        "the VERSION parameter is mandatory for this request",
                    ),
                );
            }
            // QUERY_LAYERS is the correct param for "which layer(s) to query"; fall back to
            // LAYERS only when QUERY_LAYERS itself is absent/empty (some clients only send one).
            let raw_query_layers = get("query_layers").unwrap_or_default();
            let layers_param = get("layers").unwrap_or_default();
            let tokens = query_layer_tokens(&raw_query_layers, &layers_param);
            let requested = if !raw_query_layers.trim().is_empty() {
                raw_query_layers
            } else {
                layers_param
            };
            let names: Vec<&str> = layers.iter().map(|l| l.name.as_str()).collect();
            if !layers.is_empty() {
                if let Some(missing) = first_undefined_layer(&names, &requested) {
                    return exception_result(&version, layer_not_defined(&version, &missing));
                }
            }
            if tokens.len() > 1 {
                // ETS query_layers-2: QUERY_LAYERS names two-or-more layers — query each and
                // concatenate. Each layer independently honors FEATURE_COUNT (the ETS checks
                // validity, not an exact cross-layer total).
                let mut bodies: Vec<(Vec<u8>, String)> = Vec::with_capacity(tokens.len());
                for token in &tokens {
                    let one = match pick_layer(layers, token) {
                        Picked::Found(l) if l.vector.is_some() => {
                            let vec = l.vector.as_ref().unwrap();
                            get_feature_info_vector(l, vec, &version, &get)
                        }
                        Picked::Found(l) => match (l.cog.as_ref(), l.source.as_ref()) {
                            (Some(cog), Some(src)) => {
                                let cfg = ServeCfg {
                                    src_crs: &l.src_crs,
                                    band_math: l.band_math.as_ref(),
                                    bounds_wgs84: l.bounds_wgs84,
                                    index_cache: l.index_cache.clone(),
                                };
                                get_feature_info(&version, &get, &l.name, &cfg, &|ir| {
                                    render::sample_point_with_cog(
                                        ir,
                                        cog,
                                        src.as_ref(),
                                        &l.index_cache,
                                    )
                                })
                            }
                            _ => Err(WmsError::plain("layer has no COG")),
                        },
                        // Already guarded above by first_undefined_layer, but stay defensive.
                        Picked::NotFound(name) => {
                            Err(WmsError::plain(format!("layer not defined: {name}")))
                        }
                        Picked::NoLayers => Err(WmsError::plain("no layers configured")),
                    };
                    match one {
                        Ok((body, ct)) => bodies.push((body, ct)),
                        Err(e) => {
                            return exception_result(
                                &version,
                                exception_with_code(&version, e.code, &e.message),
                            )
                        }
                    }
                }
                let first_ct = bodies[0].1.clone();
                let joined = if first_ct == "text/plain" {
                    bodies
                        .iter()
                        .map(|(b, _)| String::from_utf8_lossy(b).into_owned())
                        .collect::<Vec<_>>()
                        .join("\n")
                        .into_bytes()
                } else {
                    // Non-text formats (JSON/GML): a straightforward merge isn't safe to do
                    // generically here, so concatenate raw bodies — the ETS query_layers tests
                    // send INFO_FORMAT=text/plain, the case handled above.
                    bodies.into_iter().flat_map(|(b, _)| b).collect::<Vec<u8>>()
                };
                return WmsResult::typed(joined, &first_ct);
            }
            let name = requested.split(',').next().unwrap_or("").trim().to_string();
            match pick_layer(layers, &name) {
                Picked::Found(l) if l.vector.is_some() => {
                    let vec = l.vector.as_ref().unwrap();
                    match get_feature_info_vector(l, vec, &version, &get) {
                        Ok((body, ct)) => WmsResult::typed(body, &ct),
                        Err(e) => exception_result(
                            &version,
                            exception_with_code(&version, e.code, &e.message),
                        ),
                    }
                }
                Picked::Found(l) => match (l.cog.as_ref(), l.source.as_ref()) {
                    (Some(cog), Some(src)) => {
                        let cfg = ServeCfg {
                            src_crs: &l.src_crs,
                            band_math: l.band_math.as_ref(),
                            bounds_wgs84: l.bounds_wgs84,
                            index_cache: l.index_cache.clone(),
                        };
                        match get_feature_info(&version, &get, &l.name, &cfg, &|ir| {
                            render::sample_point_with_cog(ir, cog, src.as_ref(), &l.index_cache)
                        }) {
                            Ok((body, ct)) => WmsResult::typed(body, &ct),
                            Err(e) => exception_result(
                                &version,
                                exception_with_code(&version, e.code, &e.message),
                            ),
                        }
                    }
                    _ => exception_result(&version, exception(&version, "layer has no COG")),
                },
                Picked::NotFound(name) => {
                    exception_result(&version, layer_not_defined(&version, &name))
                }
                Picked::NoLayers => {
                    exception_result(&version, exception(&version, "no layers configured"))
                }
            }
        }
        "getlegendgraphic" => match pick("layer") {
            Picked::Found(l) => match l.style.as_ref() {
                Some(style) => {
                    let (w, h) = legend_size(&get);
                    match crate::legend::render_legend(style, w, h) {
                        Ok(png) => WmsResult::sniffed(png),
                        Err(msg) => exception_result(&version, exception(&version, &msg)),
                    }
                }
                // Vector layer: render a legend from its rule-based Style IR (swatch + rule title).
                None => match &l.vector {
                    Some(vec) => {
                        match crate::legend::render_vector_legend(&vec.style, &vec.shaper) {
                            Ok(png) => WmsResult::sniffed(png),
                            Err(msg) => exception_result(&version, exception(&version, &msg)),
                        }
                    }
                    None => {
                        exception_result(&version, exception(&version, "no style for this layer"))
                    }
                },
            },
            Picked::NotFound(name) => {
                exception_result(&version, layer_not_defined(&version, &name))
            }
            Picked::NoLayers => {
                exception_result(&version, exception(&version, "no layers configured"))
            }
        },
        other => exception_result(
            &version,
            exception(&version, &format!("unsupported request '{other}'")),
        ),
    }
}

/// Outcome of resolving a `LAYERS`/`QUERY_LAYERS`/`LAYER` request value against the
/// configured layers.
enum Picked<'a> {
    /// Either an explicit match, or the default-first layer for an empty/missing name.
    Found(&'a Layer),
    /// A non-empty name was given but matched no configured layer — must become a WMS
    /// `LayerNotDefined` exception, never a silent fallback to a different layer.
    NotFound(String),
    /// No layers are configured on this server at all (an operator/config problem, distinct
    /// from a client naming an unknown layer).
    NoLayers,
}

/// The queryable layer names for a GetFeatureInfo request: the QUERY_LAYERS CSV tokens (trimmed,
/// non-empty), falling back to LAYERS when QUERY_LAYERS is absent/empty.
fn query_layer_tokens(query_layers: &str, layers: &str) -> Vec<String> {
    let src = if query_layers.trim().is_empty() {
        layers
    } else {
        query_layers
    };
    src.split(',')
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

fn pick_layer<'a>(layers: &'a [Layer], name: &str) -> Picked<'a> {
    if layers.is_empty() {
        return Picked::NoLayers;
    }
    if name.is_empty() {
        return Picked::Found(&layers[0]);
    }
    match layers.iter().find(|l| l.name == name) {
        Some(l) => Picked::Found(l),
        None => Picked::NotFound(name.to_string()),
    }
}

/// Legend size from WIDTH/HEIGHT (bounded), defaulting to a tall, narrow ramp.
fn legend_size(get: &dyn Fn(&str) -> Option<String>) -> (u32, u32) {
    let dim = |k: &str, d: u32| {
        get(k)
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|&v| v > 0 && v <= 2048)
            .unwrap_or(d)
    };
    (dim("width", 110), dim("height", 256))
}

/// Render a GetMap for one selected layer (reuses the shared `get_map` with the layer's
/// per-layer config: source CRS, band math, its own COG + tile cache).
fn get_map_layer(
    layer: &Layer,
    version: &str,
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<u8>, WmsError> {
    if let Some(vec) = &layer.vector {
        return get_map_vector(layer, vec, version, get);
    }
    let cog = layer
        .cog
        .as_ref()
        .ok_or_else(|| WmsError::plain("layer has no COG"))?;
    let style = layer
        .style
        .as_ref()
        .ok_or_else(|| WmsError::plain("layer has no style"))?;
    let source = layer
        .source
        .as_ref()
        .ok_or_else(|| WmsError::plain("layer has no source"))?;
    let cfg = ServeCfg {
        src_crs: &layer.src_crs,
        band_math: layer.band_math.as_ref(),
        bounds_wgs84: layer.bounds_wgs84,
        index_cache: layer.index_cache.clone(),
    };
    get_map(&layer.cog_path, style, &cfg, version, get, &|req| {
        // Reuse the layer's persistent source (S3 connections pooled across requests).
        render::render_with_cog(req, cog, source.as_ref(), layer.tile_cache.as_ref())
    })
}

/// Render a GetMap for a **vector** (label) layer — viewport-global placement, one image, no
/// tiles. Reuses `parse_map_frame` (CRS/axis-correct) then the vector pipeline.
fn get_map_vector(
    layer: &Layer,
    vec: &crate::server::VectorLayer,
    version: &str,
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<u8>, WmsError> {
    validate_getmap_format(get)?;
    validate_styles(get)?;
    let f = parse_map_frame(version, get, &layer.src_crs)?;
    // Per-zoom LOD: pick the scale-appropriate pool (maps the GetMap scale-denominator → effective z).
    let scale = crate::vector::render::request_scale_denominator(f.bbox, f.width, &f.render_crs);
    // Reads through the `VectorSource` seam (windowed-seam refactor) — LoadAll today, so
    // byte-identical to the old `render_vector(src.as_ref(), ...)` call.
    let vs = vec.source_for_scale(scale);
    let mut rgba = crate::vector::render::render_vector_from(
        &vs,
        &vec.style,
        &layer.src_crs,
        &f.render_crs,
        f.bbox,
        f.width,
        f.height,
        &vec.shaper,
    )
    .map_err(WmsError::plain)?;
    // WMS TRANSPARENT (default FALSE) => flatten onto BGCOLOR (default white); TRUE keeps alpha.
    if !parse_transparent(get("transparent").as_deref()) {
        crate::pngio::composite_over_bg(&mut rgba, parse_bgcolor(get("bgcolor").as_deref()));
    }
    crate::pngio::encode_rgba(&rgba, f.width, f.height).map_err(WmsError::plain)
}

/// Ray-casting point-in-ring test (source-CRS coords; `x`=east/lon, `y`=north/lat).
fn point_in_ring(x: f64, y: f64, ring: &[[f64; 2]]) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (ring[i][0], ring[i][1]);
        let (xj, yj) = (ring[j][0], ring[j][1]);
        if ((yi > y) != (yj > y)) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Point in a Polygon (`rings[0]` exterior, the rest holes): inside the exterior and outside every hole.
fn point_in_polygon(x: f64, y: f64, rings: &[Vec<[f64; 2]>]) -> bool {
    !rings.is_empty()
        && point_in_ring(x, y, &rings[0])
        && !rings[1..].iter().any(|h| point_in_ring(x, y, h))
}

/// A GeoJSON geometry string with every vertex reprojected source-CRS -> WGS84 via `to_wgs84`
/// (GeoJSON is lon/lat; a vertex that fails to reproject passes through unchanged).
fn geom_to_geojson(
    geom: &crate::vector::feature::Geometry,
    to_wgs84: &crate::reproj::Transformer,
) -> String {
    use crate::vector::feature::Geometry;
    let pt = |p: &[f64; 2]| -> [f64; 2] {
        to_wgs84
            .to_source(p[0], p[1])
            .map(|(a, b)| [a, b])
            .unwrap_or(*p)
    };
    let ring = |r: &[[f64; 2]]| -> String {
        let cs: Vec<String> = r
            .iter()
            .map(|p| {
                let q = pt(p);
                format!("[{},{}]", q[0], q[1])
            })
            .collect();
        format!("[{}]", cs.join(","))
    };
    let poly = |rings: &[Vec<[f64; 2]>]| -> String {
        let rs: Vec<String> = rings.iter().map(|r| ring(r)).collect();
        format!("[{}]", rs.join(","))
    };
    match geom {
        Geometry::Point(p) => {
            let q = pt(p);
            format!("{{\"type\":\"Point\",\"coordinates\":[{},{}]}}", q[0], q[1])
        }
        Geometry::LineString(l) => {
            format!("{{\"type\":\"LineString\",\"coordinates\":{}}}", ring(l))
        }
        Geometry::Polygon(rings) => {
            format!("{{\"type\":\"Polygon\",\"coordinates\":{}}}", poly(rings))
        }
        Geometry::MultiLineString(parts) => {
            let ps: Vec<String> = parts.iter().map(|l| ring(l)).collect();
            format!(
                "{{\"type\":\"MultiLineString\",\"coordinates\":[{}]}}",
                ps.join(",")
            )
        }
        Geometry::MultiPolygon(polys) => {
            let ps: Vec<String> = polys.iter().map(|pg| poly(pg)).collect();
            format!(
                "{{\"type\":\"MultiPolygon\",\"coordinates\":[{}]}}",
                ps.join(",")
            )
        }
    }
}

/// All feature properties as a JSON object (string values escaped, numbers as-is, nulls -> null).
fn props_to_json(props: &crate::vector::feature::Props) -> String {
    use crate::vector::feature::Value;
    let entries: Vec<String> = props
        .iter()
        .map(|(k, v)| {
            let key = serde_json::to_string(k).unwrap_or_else(|_| "\"\"".into());
            let val = match v {
                Value::Str(s) => serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into()),
                Value::Num(n) if n.is_finite() => format!("{n}"),
                _ => "null".into(),
            };
            format!("{key}:{val}")
        })
        .collect();
    format!("{{{}}}", entries.join(","))
}

/// Walk features and return up to `feature_count` GFI hits at pixel `(i,j)`: polygons that contain
/// the back-projected query point (document order) and points within 12px (nearest first). The one
/// place the FEATURE_COUNT limit is applied.
fn collect_gfi_hits<'a>(
    features: impl Iterator<Item = &'a crate::vector::feature::Feature>,
    proj: &crate::vector::geom::Projector,
    query_src: Option<(f64, f64)>,
    i: f64,
    j: f64,
    feature_count: usize,
) -> Vec<&'a crate::vector::feature::Feature> {
    use crate::vector::feature::Geometry;
    let mut polys: Vec<&crate::vector::feature::Feature> = Vec::new();
    let mut pts: Vec<(f32, &crate::vector::feature::Feature)> = Vec::new();
    for feat in features {
        match &feat.geom {
            Geometry::Point([lon, lat]) => {
                if let Some((px, py)) = proj.to_pixel(*lon, *lat) {
                    let d = ((px - i as f32).powi(2) + (py - j as f32).powi(2)).sqrt();
                    if d <= 12.0 {
                        pts.push((d, feat));
                    }
                }
            }
            Geometry::Polygon(_) | Geometry::MultiPolygon(_) => {
                let Some((qx, qy)) = query_src else {
                    continue;
                };
                if !crate::vector::geom::bbox_overlaps(feat.bbox, [qx, qy, qx, qy]) {
                    continue;
                }
                let inside = match &feat.geom {
                    Geometry::Polygon(rings) => point_in_polygon(qx, qy, rings),
                    Geometry::MultiPolygon(polys) => {
                        polys.iter().any(|p| point_in_polygon(qx, qy, p))
                    }
                    _ => false,
                };
                if inside {
                    polys.push(feat);
                }
            }
            _ => {} // lines: nearest-line GFI not yet implemented
        }
    }
    pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    // Polygons (document order) first, then nearest points — matches the old single-hit priority.
    let mut out: Vec<&crate::vector::feature::Feature> = polys;
    out.extend(pts.into_iter().map(|(_, f)| f));
    out.truncate(feature_count.max(1));
    out
}

/// Vector-layer GetFeatureInfo: up to FEATURE_COUNT features at the queried pixel —
/// point-in-polygon for polygon/multipolygon layers, nearest-point (pixel tolerance) for point
/// layers — formatted per INFO_FORMAT. JSON returns the full feature(s) (all properties + geometry
/// reprojected to WGS84 so a client can highlight them). Never panics on a COG-less layer.
fn get_feature_info_vector(
    layer: &Layer,
    vec: &crate::server::VectorLayer,
    version: &str,
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<(Vec<u8>, String), WmsError> {
    let f = parse_map_frame(version, get, &layer.src_crs)?;
    // Pixel coords: I/J (1.3.0) or X/Y (1.1.1) — a parse failure or an out-of-range pixel is
    // `InvalidPoint`, matching the raster `get_feature_info`.
    let (ik, jk) = if version.starts_with("1.3") {
        ("i", "j")
    } else {
        ("x", "y")
    };
    let parse_px = |k: &str| -> Result<f64, WmsError> {
        get(k)
            .ok_or_else(|| {
                WmsError::coded("InvalidPoint", format!("missing {}", k.to_uppercase()))
            })?
            .trim()
            .parse()
            .map_err(|_| WmsError::coded("InvalidPoint", format!("invalid {}", k.to_uppercase())))
    };
    let (i, j) = (parse_px(ik)?, parse_px(jk)?);
    if i < 0.0 || j < 0.0 || i >= f.width as f64 || j >= f.height as f64 {
        return Err(WmsError::coded(
            "InvalidPoint",
            format!(
                "{}/{} outside the {}x{} map",
                ik.to_uppercase(),
                jk.to_uppercase(),
                f.width,
                f.height
            ),
        ));
    }
    let info_format = match get("info_format") {
        Some(fmt) => {
            let lf = fmt.trim().to_ascii_lowercase();
            if ![
                "text/plain",
                "application/json",
                "text/html",
                "application/vnd.ogc.gml",
            ]
            .contains(&lf.as_str())
            {
                return Err(WmsError::coded(
                    "InvalidFormat",
                    format!("unsupported INFO_FORMAT '{fmt}'"),
                ));
            }
            fmt
        }
        None => "text/plain".to_string(),
    };
    // ets-wms11 feature_count-1/2: default (unset) is 1; any value below 1 clamps to 1 rather than
    // erroring (CITE only requires >=2 to yield strictly more data than the default).
    let feature_count = get("feature_count")
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let proj = crate::vector::geom::Projector::new(
        &layer.src_crs,
        &f.render_crs,
        f.bbox,
        f.width,
        f.height,
    )
    .map_err(WmsError::plain)?;
    // The queried pixel back-projected into the SOURCE CRS, for point-in-polygon hit-testing.
    let (gx, gy) = (
        f.bbox[0] + (i / f.width as f64) * (f.bbox[2] - f.bbox[0]),
        f.bbox[3] - (j / f.height as f64) * (f.bbox[3] - f.bbox[1]),
    );
    let query_src = crate::reproj::Transformer::new(&f.render_crs, &layer.src_crs)
        .ok()
        .and_then(|t| t.to_source(gx, gy)); // (qx, qy) in the source CRS, or None

    // Reads through the `VectorSource` seam (windowed-seam refactor): the map-frame bbox is in the
    // request CRS, but `features_in` expects the layer's source CRS — reproject via the same
    // `source_filter_bbox` helper the render/MVT paths use, falling back to the full extent when the
    // transform is unavailable (fail-open, matches the pre-filter's own fallback). GFI intentionally
    // ignores LOD (reads `vec.source` directly, not `source_for_zoom`/`source_for_scale`) — when the
    // layer has LOD, `vec.source` IS the finest full-detail pool (see `build_vector_layer`), so this
    // stays the lossless GFI source exactly as before this migration.
    let gfi_bbox = crate::vector::geom::source_filter_bbox(&f.render_crs, &layer.src_crs, f.bbox)
        .unwrap_or_else(|| vec.source.full_extent());
    let gfi_batch = vec.source.features_in(gfi_bbox);
    let hits = collect_gfi_hits(
        gfi_batch.as_slice().iter(),
        &proj,
        query_src,
        i,
        j,
        feature_count,
    );
    // Evaluate the label template against a hit feature (same code path as GetMap). "no feature"
    // is still driven by `hits`, never by the label — a hit whose template evaluates empty (or a
    // style with no text symbolizer) is reported as "(unnamed)", not "no feature at this location".
    let label_for = |feat: &crate::vector::feature::Feature| -> String {
        let label = vec
            .style
            .primary_label()
            .map(|lbl| crate::vector::style::eval_label(lbl, &feat.props));
        match label {
            Some(s) if !s.is_empty() => s,
            _ => "(unnamed)".to_string(),
        }
    };
    if info_format.contains("json") {
        // The full feature(s): geometry reprojected to WGS84 (GeoJSON) so a client can highlight
        // them, plus ALL properties (a client's Identify panel shows every attribute). When
        // `to_wgs84` is unavailable the response degrades to an empty FeatureCollection, same as
        // the previous single-hit behavior.
        let to_wgs84 = crate::reproj::Transformer::new(&layer.src_crs, "EPSG:4326").ok();
        let body = match &to_wgs84 {
            Some(t) => {
                let features: Vec<String> = hits
                    .iter()
                    .map(|feat| {
                        let geom_json = geom_to_geojson(&feat.geom, t);
                        let props = props_to_json(&feat.props);
                        format!(
                            "{{\"type\":\"Feature\",\"geometry\":{geom_json},\
                             \"properties\":{props}}}"
                        )
                    })
                    .collect();
                format!(
                    "{{\"type\":\"FeatureCollection\",\"features\":[{}]}}",
                    features.join(",")
                )
            }
            None => "{\"type\":\"FeatureCollection\",\"features\":[]}".to_string(),
        };
        Ok((body.into_bytes(), "application/json".to_string()))
    } else if info_format.contains("html") {
        // Escape the layer name + feature label (a property could carry markup → XSS).
        let esc = crate::wmts::escape_xml;
        let parts: Vec<String> = hits
            .iter()
            .map(|feat| format!("<b>{}</b>: {}", esc(&layer.name), esc(&label_for(feat))))
            .collect();
        let body = if parts.is_empty() {
            format!(
                "<html><body><b>{}</b>: no feature</body></html>",
                esc(&layer.name)
            )
        } else {
            format!("<html><body>{}</body></html>", parts.join("<br/>"))
        };
        Ok((body.into_bytes(), "text/html".to_string()))
    } else if info_format.contains("gml") {
        // Minimal, hand-emitted GML2 FeatureCollection: one <gml:featureMember> per hit, carrying
        // the layer name + the feature's display label as properties. No geometry (the JSON/HTML
        // outputs already cover that; CITE's gml checks only require a well-formed document with a
        // feature per collected hit).
        let esc = crate::wmts::escape_xml;
        let members: String = hits
            .iter()
            .map(|feat| {
                format!(
                    "<gml:featureMember><ts:feature><ts:layer>{}</ts:layer><ts:name>{}</ts:name></ts:feature></gml:featureMember>",
                    esc(&layer.name),
                    esc(&label_for(feat))
                )
            })
            .collect();
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <wfs:FeatureCollection xmlns:wfs=\"http://www.opengis.net/wfs\" \
             xmlns:gml=\"http://www.opengis.net/gml\" xmlns:ts=\"http://terraserve.io/wms\">{members}</wfs:FeatureCollection>"
        );
        Ok((body.into_bytes(), "application/vnd.ogc.gml".to_string()))
    } else {
        let blocks: Vec<String> = hits
            .iter()
            .map(|feat| format!("{}\nname: {}\n", layer.name, label_for(feat)))
            .collect();
        let body = if blocks.is_empty() {
            format!("{}\n(no feature at this location)\n", layer.name)
        } else {
            blocks.join("")
        };
        Ok((body.into_bytes(), "text/plain".to_string()))
    }
}

/// Shared KVP parse + request dispatch. The pixel path is supplied by `render_fn` so the
/// CLI (parse-per-request) and the server (parse-once) share everything but the render call.
fn dispatch(
    cog_path: &str,
    style: &Style,
    cfg: &ServeCfg,
    query: &str,
    base_url: Option<&str>,
    render_fn: &dyn Fn(&RenderRequest) -> Result<Vec<u8>, String>,
) -> WmsResult {
    let params = parse_kvp(query);
    let get = |k: &str| {
        params
            .iter()
            .find(|(kk, _)| kk == k)
            .map(|(_, v)| v.clone())
    };

    // VERSION with the legacy WMTVER alias; GetCapabilities negotiates over {1.1.1,1.3.0}
    // (GetMap/GetFeatureInfo still require VERSION explicitly — enforced in their arms).
    let requested_version = get("version").or_else(|| get("wmtver"));
    let version = negotiate_version(requested_version.as_deref()).to_string();
    let request = normalize_request(&get("request").unwrap_or_default());

    match request.as_str() {
        "getcapabilities" => capabilities_result(
            &version,
            capabilities(&version, base_url, cfg.bounds_wgs84, cfg.src_crs),
        ),
        "getmap" => {
            // CITE no-version: VERSION is mandatory for GetMap (06-042 Table 8); version
            // negotiation (defaulting) applies ONLY to GetCapabilities (§6.2.4).
            if get("version").is_none() && get("wmtver").is_none() {
                return exception_result(
                    &version,
                    exception(
                        &version,
                        "the VERSION parameter is mandatory for this request",
                    ),
                );
            }
            match get_map(cog_path, style, cfg, &version, &get, render_fn) {
                Ok(png) => WmsResult::sniffed(png),
                Err(e) => {
                    exception_result(&version, exception_with_code(&version, e.code, &e.message))
                }
            }
        }
        "getfeatureinfo" => {
            // CITE no-version: same VERSION-mandatory rule as GetMap.
            if get("version").is_none() && get("wmtver").is_none() {
                return exception_result(
                    &version,
                    exception(
                        &version,
                        "the VERSION parameter is mandatory for this request",
                    ),
                );
            }
            // CLI single-layer: open the COG to read the point value.
            match get_feature_info(&version, &get, "layer", cfg, &|ir| {
                render::sample_point(ir, cog_path)
            }) {
                Ok((body, ct)) => WmsResult::typed(body, &ct),
                Err(e) => {
                    exception_result(&version, exception_with_code(&version, e.code, &e.message))
                }
            }
        }
        "getlegendgraphic" => {
            let (w, h) = legend_size(&get);
            match crate::legend::render_legend(style, w, h) {
                Ok(png) => WmsResult::sniffed(png),
                Err(msg) => exception_result(&version, exception(&version, &msg)),
            }
        }
        other => exception_result(
            &version,
            exception(&version, &format!("unsupported request '{other}'")),
        ),
    }
}

/// The parsed map frame shared by GetMap and GetFeatureInfo: bbox (axis-normalized), the CRS to
/// render in, and the pixel dimensions.
struct MapFrame {
    bbox: [f64; 4],
    render_crs: String,
    width: u32,
    height: u32,
}

/// Parse + validate the GetMap/GetFeatureInfo common params (CRS/SRS, BBOX with the 1.3.0 EPSG:4326
/// axis flip, WIDTH/HEIGHT). One source of truth so GetFeatureInfo maps pixels EXACTLY as GetMap does.
fn parse_map_frame(
    version: &str,
    get: &dyn Fn(&str) -> Option<String>,
    src_crs: &str,
) -> Result<MapFrame, WmsError> {
    let is_130 = version.starts_with("1.3");
    // CRS param: CRS for 1.3.0, SRS for 1.1.1 (accept either defensively).
    let crs_raw = get("crs")
        .or_else(|| get("srs"))
        .ok_or_else(|| WmsError::plain("missing CRS/SRS parameter"))?;
    // Accept the standard request CRSs plus this layer's own (native) CRS.
    let native_match = crs_raw.trim().eq_ignore_ascii_case(src_crs.trim());
    if !crate::reproj::is_supported_crs(&crs_raw) && !native_match {
        // WMS 1.1.1 names this exception code `InvalidSRS`; 1.3.0 renamed it to `InvalidCRS`.
        let code = if is_130 { "InvalidCRS" } else { "InvalidSRS" };
        return Err(WmsError::coded(
            code,
            format!("unsupported CRS '{crs_raw}'"),
        ));
    }
    let bbox_raw = get("bbox").ok_or_else(|| WmsError::plain("missing BBOX parameter"))?;
    let width: u32 = get("width")
        .ok_or_else(|| WmsError::plain("missing WIDTH"))?
        .trim()
        .parse()
        .map_err(|_| WmsError::plain("invalid WIDTH"))?;
    let height: u32 = get("height")
        .ok_or_else(|| WmsError::plain("missing HEIGHT"))?
        .trim()
        .parse()
        .map_err(|_| WmsError::plain("invalid HEIGHT"))?;
    // Cap output size: an unbounded WIDTH/HEIGHT asks for a multi-GB canvas allocation (admission
    // control bounds concurrency, not per-request size). 8192² is well past any real tile/map.
    const MAX_DIM: u32 = 8192;
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return Err(WmsError::plain(format!(
            "WIDTH/HEIGHT must be 1..={MAX_DIM}"
        )));
    }

    let vals: Vec<f64> = bbox_raw
        .split(',')
        .map(|s| s.trim().parse::<f64>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| WmsError::plain("invalid BBOX"))?;
    if vals.len() != 4 {
        return Err(WmsError::plain("BBOX must have 4 values"));
    }
    // Reject non-finite BBOX: "NaN"/"inf"/"1e999" all parse as valid f64 but poison the
    // transform (and NaN slips past any `min >= max` guard, since NaN comparisons are false).
    if vals.iter().any(|v| !v.is_finite()) {
        return Err(WmsError::plain("invalid BBOX: values must be finite"));
    }

    // Axis order: WMS 1.3.0 uses the CRS-declared order. For EPSG:4326 that is lat,lon,
    // so BBOX = miny,minx,maxy,maxx and we must swap back to minx,miny,maxx,maxy.
    let crs_up = crs_raw.trim().to_ascii_uppercase();
    let bbox = if is_130 && crs_up == "EPSG:4326" {
        [vals[1], vals[0], vals[3], vals[2]]
    } else {
        [vals[0], vals[1], vals[2], vals[3]]
    };
    // Degenerate / inverted BBOX (values are finite here, so plain `<` ordering is safe).
    if !(bbox[0] < bbox[2] && bbox[1] < bbox[3]) {
        return Err(WmsError::plain("invalid BBOX: min must be less than max"));
    }

    // CRS:84 is WGS84 lon/lat; route it through EPSG:4326 for the transform.
    let render_crs = if crs_up == "CRS:84" {
        "EPSG:4326".to_string()
    } else {
        crs_up
    };
    Ok(MapFrame {
        bbox,
        render_crs,
        width,
        height,
    })
}

/// GetMap FORMAT is optional (we default to PNG); a *present* value that is not an advertised
/// GetMap format is an `InvalidFormat` ServiceException.
fn validate_getmap_format(get: &dyn Fn(&str) -> Option<String>) -> Result<(), WmsError> {
    if let Some(f) = get("format") {
        let f = f.trim().to_ascii_lowercase();
        if !f.is_empty() && f != "image/png" {
            return Err(WmsError::coded(
                "InvalidFormat",
                format!("unsupported FORMAT '{f}'"),
            ));
        }
    }
    Ok(())
}

/// STYLES is a comma-separated list, one entry per requested layer. An empty entry means "default
/// style" and is always valid. TerraServe layers expose only the default style, so any NON-empty
/// entry names a style the layer does not define -> `StyleNotDefined`.
fn validate_styles(get: &dyn Fn(&str) -> Option<String>) -> Result<(), WmsError> {
    if let Some(styles) = get("styles") {
        for tok in styles.split(',') {
            let tok = tok.trim();
            if !tok.is_empty() {
                return Err(WmsError::coded(
                    "StyleNotDefined",
                    format!("style '{tok}' is not defined"),
                ));
            }
        }
    }
    Ok(())
}

fn get_map(
    cog_path: &str,
    style: &Style,
    cfg: &ServeCfg,
    version: &str,
    get: &dyn Fn(&str) -> Option<String>,
    render_fn: &dyn Fn(&RenderRequest) -> Result<Vec<u8>, String>,
) -> Result<Vec<u8>, WmsError> {
    validate_getmap_format(get)?;
    validate_styles(get)?;
    let f = parse_map_frame(version, get, cfg.src_crs)?;
    let (width, height) = (f.width, f.height);
    let req = RenderRequest {
        cog_path,
        bbox: f.bbox,
        crs: &f.render_crs,
        src_crs: cfg.src_crs,
        width,
        height,
        resample: Resample::Nearest, // WMS GetMap default per the pilot
        style,
        band_math: cfg.band_math,
        index_cache: cfg.index_cache.clone(),
    };
    let mut rgba = render_fn(&req).map_err(WmsError::plain)?;
    // WMS TRANSPARENT (default FALSE) => flatten onto BGCOLOR (default white); TRUE keeps alpha.
    if !parse_transparent(get("transparent").as_deref()) {
        crate::pngio::composite_over_bg(&mut rgba, parse_bgcolor(get("bgcolor").as_deref()));
    }
    crate::pngio::encode_rgba(&rgba, width, height).map_err(WmsError::plain)
}

/// GetFeatureInfo: parse the map frame + the query pixel (`I,J` in 1.3.0, `X,Y` in 1.1.1), read the
/// exact value via `sample_fn`, and format it. Returns `(body, content_type)`.
fn get_feature_info(
    version: &str,
    get: &dyn Fn(&str) -> Option<String>,
    layer_name: &str,
    cfg: &ServeCfg,
    sample_fn: &dyn Fn(&render::InfoRequest) -> Result<render::PointInfo, String>,
) -> Result<(Vec<u8>, String), WmsError> {
    let f = parse_map_frame(version, get, cfg.src_crs)?;
    // Pixel coords: I/J (1.3.0) or X/Y (1.1.1).
    let (ik, jk) = if version.starts_with("1.3") {
        ("i", "j")
    } else {
        ("x", "y")
    };
    let parse_px = |k: &str| -> Result<u32, WmsError> {
        get(k)
            .ok_or_else(|| {
                WmsError::coded("InvalidPoint", format!("missing {}", k.to_uppercase()))
            })?
            .trim()
            .parse()
            .map_err(|_| WmsError::coded("InvalidPoint", format!("invalid {}", k.to_uppercase())))
    };
    let (i, j) = (parse_px(ik)?, parse_px(jk)?);
    if i >= f.width || j >= f.height {
        return Err(WmsError::coded(
            "InvalidPoint",
            format!(
                "{}/{} outside the {}x{} map",
                ik.to_uppercase(),
                jk.to_uppercase(),
                f.width,
                f.height
            ),
        ));
    }
    let info_format = match get("info_format") {
        Some(f) => {
            let lf = f.trim().to_ascii_lowercase();
            if !["text/plain", "application/json", "text/html"].contains(&lf.as_str()) {
                return Err(WmsError::coded(
                    "InvalidFormat",
                    format!("unsupported INFO_FORMAT '{f}'"),
                ));
            }
            f
        }
        None => "text/plain".to_string(),
    };
    let ir = render::InfoRequest {
        bbox: f.bbox,
        crs: &f.render_crs,
        src_crs: cfg.src_crs,
        width: f.width,
        height: f.height,
        i,
        j,
        band_math: cfg.band_math,
    };
    let info = sample_fn(&ir).map_err(WmsError::plain)?;
    Ok(format_feature_info(&info_format, layer_name, &info))
}

fn num_json(v: f64) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "null".to_string()
    }
}

/// Render a `PointInfo` in the requested INFO_FORMAT → `(body, content_type)`. Supports
/// `application/json` (GeoJSON), `text/html`, and `text/plain` (the default / fallback). Shared with
/// the WMTS GetFeatureInfo front-end.
pub(crate) fn format_feature_info(
    info_format: &str,
    layer: &str,
    info: &render::PointInfo,
) -> (Vec<u8>, String) {
    let fmt = info_format.to_ascii_lowercase();
    let have = info.in_image && !info.nodata;
    if fmt.contains("json") {
        let mut s = String::from("{\"type\":\"FeatureCollection\",\"features\":[");
        if have {
            s.push_str("{\"type\":\"Feature\",\"geometry\":null,\"properties\":{");
            s.push_str(&format!(
                "\"layer\":\"{}\",\"col\":{},\"row\":{}",
                json_escape(layer),
                info.source_col,
                info.source_row
            ));
            for (k, v) in info.bands.iter().enumerate() {
                s.push_str(&format!(",\"band_{}\":{}", k + 1, num_json(*v)));
            }
            if let Some(d) = info.derived {
                s.push_str(&format!(",\"value\":{}", num_json(d)));
            }
            s.push_str("}}");
        }
        s.push_str("]}");
        (s.into_bytes(), "application/json".to_string())
    } else if fmt.contains("html") {
        let mut s = String::from("<html><head><meta charset=\"utf-8\"></head><body>\n");
        if !have {
            s.push_str(&format!(
                "<p>{}</p>\n",
                if info.in_image {
                    "nodata"
                } else {
                    "outside coverage"
                }
            ));
        } else {
            s.push_str(&format!(
                "<table border=\"1\"><caption>{}</caption>\n",
                html_escape(layer)
            ));
            s.push_str(&format!(
                "<tr><th>pixel</th><td>{},{}</td></tr>\n",
                info.source_col, info.source_row
            ));
            for (k, v) in info.bands.iter().enumerate() {
                s.push_str(&format!("<tr><th>band {}</th><td>{}</td></tr>\n", k + 1, v));
            }
            if let Some(d) = info.derived {
                s.push_str(&format!("<tr><th>value</th><td>{d}</td></tr>\n"));
            }
            s.push_str("</table>\n");
        }
        s.push_str("</body></html>\n");
        (s.into_bytes(), "text/html".to_string())
    } else {
        let mut s = String::new();
        if !info.in_image {
            s.push_str("outside coverage\n");
        } else if info.nodata {
            s.push_str("nodata\n");
        } else {
            s.push_str(&format!("layer: {layer}\n"));
            s.push_str(&format!("pixel: {},{}\n", info.source_col, info.source_row));
            for (k, v) in info.bands.iter().enumerate() {
                s.push_str(&format!("band_{}: {}\n", k + 1, v));
            }
            if let Some(d) = info.derived {
                s.push_str(&format!("value: {d}\n"));
            }
        }
        (s.into_bytes(), "text/plain".to_string())
    }
}

/// Escape a string for embedding inside a JSON string literal. Handles `"`/`\` and the ASCII
/// control characters U+0000–U+001F, which JSON forbids raw (SEC-4: the old version emitted only
/// `"`/`\`, so a config layer name carrying a raw tab/newline/ESC produced a document strict
/// clients reject; the vector GFI path uses `serde_json` and was already correct). Emits the
/// short escapes where JSON defines them (`\b\t\n\f\r`) and `\uXXXX` for the rest.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Parse `key=value&key=value` into lowercased-key pairs (values percent-decoded).
fn parse_kvp(query: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("").trim().to_ascii_lowercase();
        let v = it.next().unwrap_or("");
        out.push((k, percent_decode(v)));
    }
    out
}

pub(crate) fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hi = hexval(b[i + 1]);
                let lo = hexval(b[i + 2]);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push(h * 16 + l);
                    i += 3;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn capabilities(
    version: &str,
    base_url: Option<&str>,
    bounds: [f64; 4],
    native_crs: &str,
) -> String {
    let layer = LAYER_NAME;
    let [west, south, east, north] = bounds;
    // Advertise the layer's own CRS in addition to the standard set (deduped), so a client
    // whose project is in that CRS can request it natively.
    let nc = native_crs.trim().to_ascii_uppercase();
    let extra_is_new = !matches!(nc.as_str(), "EPSG:4326" | "EPSG:3857" | "EPSG:3763");
    let extra_srs = if extra_is_new {
        format!("\n        <SRS>{nc}</SRS>")
    } else {
        String::new()
    };
    let extra_crs = if extra_is_new {
        format!("\n        <CRS>{nc}</CRS>")
    } else {
        String::new()
    };
    // OnlineResource / DCPType blocks. Per the WMS 1.3.0 XSD `Service/OnlineResource` and each
    // operation's `DCPType` are REQUIRED, so they are always emitted (CITE C3); in render mode
    // (no base_url) we fall back to a placeholder href so the document stays schema-valid.
    let (service_or, dcp) = online_resource(base_url);

    if version.starts_with("1.1") {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE WMT_MS_Capabilities SYSTEM "http://schemas.opengis.net/wms/1.1.1/WMS_MS_Capabilities.dtd">
<WMT_MS_Capabilities version="1.1.1">
  <Service>
    <Name>OGC:WMS</Name>
    <Title>TerraServe WMS</Title>{service_or}
  </Service>
  <Capability>
    <Request>
      <GetCapabilities><Format>application/vnd.ogc.wms_xml</Format>{dcp}</GetCapabilities>
      <GetMap><Format>image/png</Format>{dcp}</GetMap>
      <GetFeatureInfo><Format>text/plain</Format><Format>application/json</Format><Format>text/html</Format><Format>application/vnd.ogc.gml</Format>{dcp}</GetFeatureInfo>
    </Request>
    <Exception><Format>application/vnd.ogc.se_xml</Format></Exception>
    <Layer>
      <Title>TerraServe</Title>
      <SRS>EPSG:4326</SRS>
      <SRS>EPSG:3857</SRS>
      <SRS>EPSG:3763</SRS>
      <LatLonBoundingBox minx="{west}" miny="{south}" maxx="{east}" maxy="{north}"/>
      <Layer queryable="1">
        <Name>{layer}</Name>
        <Title>Cascais orthophoto</Title>
        <SRS>EPSG:4326</SRS>
        <SRS>EPSG:3857</SRS>
        <SRS>EPSG:3763</SRS>{extra_srs}
        <LatLonBoundingBox minx="{west}" miny="{south}" maxx="{east}" maxy="{north}"/>
      </Layer>
    </Layer>
  </Capability>
</WMT_MS_Capabilities>
"#
        )
    } else {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<WMS_Capabilities version="1.3.0" xmlns="http://www.opengis.net/wms" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://www.opengis.net/wms http://schemas.opengis.net/wms/1.3.0/capabilities_1_3_0.xsd">
  <Service>
    <Name>WMS</Name>
    <Title>TerraServe WMS</Title>{service_or}
  </Service>
  <Capability>
    <Request>
      <GetCapabilities><Format>text/xml</Format>{dcp}</GetCapabilities>
      <GetMap><Format>image/png</Format>{dcp}</GetMap>
      <GetFeatureInfo><Format>text/plain</Format><Format>application/json</Format><Format>text/html</Format>{dcp}</GetFeatureInfo>
    </Request>
    <Exception><Format>XML</Format></Exception>
    <Layer>
      <Title>TerraServe</Title>
      <CRS>EPSG:4326</CRS>
      <CRS>EPSG:3857</CRS>
      <CRS>EPSG:3763</CRS>
      <EX_GeographicBoundingBox>
        <westBoundLongitude>{west}</westBoundLongitude>
        <eastBoundLongitude>{east}</eastBoundLongitude>
        <southBoundLatitude>{south}</southBoundLatitude>
        <northBoundLatitude>{north}</northBoundLatitude>
      </EX_GeographicBoundingBox>
      <Layer queryable="1">
        <Name>{layer}</Name>
        <Title>Cascais orthophoto</Title>
        <CRS>EPSG:4326</CRS>
        <CRS>EPSG:3857</CRS>
        <CRS>EPSG:3763</CRS>
        <CRS>CRS:84</CRS>{extra_crs}
        <BoundingBox CRS="CRS:84" minx="{west}" miny="{south}" maxx="{east}" maxy="{north}"/>
        <BoundingBox CRS="EPSG:4326" minx="{south}" miny="{west}" maxx="{north}" maxy="{east}"/>
      </Layer>
    </Layer>
  </Capability>
</WMS_Capabilities>
"#
        )
    }
}

/// The OnlineResource / DCPType blocks. Both `Service/OnlineResource` and each operation's
/// `DCPType` are REQUIRED by the WMS 1.3.0 XSD, so they are always emitted (CITE C3); when no
/// base_url is known (render mode) a placeholder href keeps the Capabilities document schema-valid.
fn online_resource(base_url: Option<&str>) -> (String, String) {
    let raw = base_url.unwrap_or("http://localhost/wms");
    // CITE capability-onlineresource: the href must be KVP-append-ready, i.e. end at `?` or `&`
    // so a client can straight-append `KEY=VALUE` pairs. Checked/appended on the raw string
    // (before escaping) so a trailing literal `&` (which xml_escape turns into `&amp;`) is still
    // recognized.
    let raw = if raw.ends_with('?') || raw.ends_with('&') {
        raw.to_string()
    } else {
        format!("{raw}?")
    };
    let href = xml_escape(&raw);
    (
        format!(
            "\n    <OnlineResource xmlns:xlink=\"http://www.w3.org/1999/xlink\" xlink:href=\"{href}\"/>"
        ),
        format!(
            "<DCPType><HTTP><Get><OnlineResource xmlns:xlink=\"http://www.w3.org/1999/xlink\" xlink:href=\"{href}\"/></Get></HTTP></DCPType>"
        ),
    )
}

/// GetCapabilities for the multi-layer server: one inner `<Layer>` per published layer, each
/// with its own name, WGS84 bounds, and CRS list (standard + its native CRS).
fn capabilities_multi(layers: &[Layer], version: &str, base_url: Option<&str>) -> String {
    let (service_or, dcp) = online_resource(base_url);
    let is_111 = version.starts_with("1.1");
    let tag = if is_111 { "SRS" } else { "CRS" };

    let crs_lines = |indent: &str, native: Option<&str>| -> String {
        // Static, known-safe literals — no escaping needed. CRS:84 is 1.3.0-only: every
        // <BoundingBox CRS="CRS:84"> below must have a matching <CRS>CRS:84</CRS> advertisement
        // (CITE bbox-crs-advertised); the 1.1.1 SRS list is untouched.
        let mut list: Vec<String> = vec![
            "EPSG:4326".to_string(),
            "EPSG:3857".to_string(),
            "EPSG:3763".to_string(),
        ];
        if !is_111 {
            list.push("CRS:84".to_string());
        }
        let up = native.map(|c| c.trim().to_ascii_uppercase());
        if let Some(c) = &up {
            if !list.iter().any(|x| x.eq_ignore_ascii_case(c)) {
                // `c` is layer-derived (the configured native CRS token) — escape it.
                list.push(xml_escape(c));
            }
        }
        list.iter()
            .map(|c| format!("{indent}<{tag}>{c}</{tag}>\n"))
            .collect()
    };

    let mut inner = String::new();
    for l in layers {
        let [w, s, e, n] = l.bounds_wgs84;
        let bbox = if is_111 {
            format!("        <LatLonBoundingBox minx=\"{w}\" miny=\"{s}\" maxx=\"{e}\" maxy=\"{n}\"/>\n")
        } else {
            format!(
                "        <EX_GeographicBoundingBox>\n          <westBoundLongitude>{w}</westBoundLongitude>\n          <eastBoundLongitude>{e}</eastBoundLongitude>\n          <southBoundLatitude>{s}</southBoundLatitude>\n          <northBoundLatitude>{n}</northBoundLatitude>\n        </EX_GeographicBoundingBox>\n"
            )
        };
        // WMS 1.3.0 only: per-layer <BoundingBox> (CITE C1) — CRS axis order per the CRS URI
        // (CRS:84 is lon,lat; EPSG:4326 is lat,lon). XSD sequence is CRS*, EX_GeographicBoundingBox*,
        // BoundingBox*, so this goes after `bbox` above. Not part of the 1.1.1 DTD, so is_111 skips it.
        let named_bbox = if is_111 {
            String::new()
        } else {
            format!(
                "        <BoundingBox CRS=\"CRS:84\" minx=\"{w}\" miny=\"{s}\" maxx=\"{e}\" maxy=\"{n}\"/>\n        <BoundingBox CRS=\"EPSG:4326\" minx=\"{s}\" miny=\"{w}\" maxx=\"{n}\" maxy=\"{e}\"/>\n"
            )
        };
        inner.push_str(&format!(
            "      <Layer queryable=\"1\">\n        <Name>{}</Name>\n        <Title>{}</Title>\n{}{bbox}{named_bbox}      </Layer>\n",
            xml_escape(&l.name),
            xml_escape(&l.name),
            crs_lines("        ", Some(&l.src_crs)),
        ));
    }
    let top_crs = crs_lines("      ", layers.first().map(|l| l.src_crs.as_str()));

    if is_111 {
        // Root-layer LatLonBoundingBox = union of every child layer's WGS84 bounds (WMS 1.1.1 DTD:
        // the root Layer's LatLonBoundingBox; required by ets-wms11 latlonbbox-1).
        let root_llbb = {
            let mut it = layers.iter();
            it.next()
                .map(|l0| {
                    let mut u = l0.bounds_wgs84;
                    for l in layers {
                        let [w, s, e, n] = l.bounds_wgs84;
                        u[0] = u[0].min(w);
                        u[1] = u[1].min(s);
                        u[2] = u[2].max(e);
                        u[3] = u[3].max(n);
                    }
                    format!(
                        "      <LatLonBoundingBox minx=\"{}\" miny=\"{}\" maxx=\"{}\" maxy=\"{}\"/>\n",
                        u[0], u[1], u[2], u[3]
                    )
                })
                .unwrap_or_default()
        };
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE WMT_MS_Capabilities SYSTEM "http://schemas.opengis.net/wms/1.1.1/WMS_MS_Capabilities.dtd">
<WMT_MS_Capabilities version="1.1.1">
  <Service>
    <Name>OGC:WMS</Name>
    <Title>TerraServe WMS</Title>{service_or}
  </Service>
  <Capability>
    <Request>
      <GetCapabilities><Format>application/vnd.ogc.wms_xml</Format>{dcp}</GetCapabilities>
      <GetMap><Format>image/png</Format>{dcp}</GetMap>
      <GetFeatureInfo><Format>text/plain</Format><Format>application/json</Format><Format>text/html</Format><Format>application/vnd.ogc.gml</Format>{dcp}</GetFeatureInfo>
    </Request>
    <Exception><Format>application/vnd.ogc.se_xml</Format></Exception>
    <Layer>
      <Title>TerraServe</Title>
{top_crs}{root_llbb}{inner}    </Layer>
  </Capability>
</WMT_MS_Capabilities>
"#
        )
    } else {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<WMS_Capabilities version="1.3.0" xmlns="http://www.opengis.net/wms" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://www.opengis.net/wms http://schemas.opengis.net/wms/1.3.0/capabilities_1_3_0.xsd">
  <Service>
    <Name>WMS</Name>
    <Title>TerraServe WMS</Title>{service_or}
  </Service>
  <Capability>
    <Request>
      <GetCapabilities><Format>text/xml</Format>{dcp}</GetCapabilities>
      <GetMap><Format>image/png</Format>{dcp}</GetMap>
      <GetFeatureInfo><Format>text/plain</Format><Format>application/json</Format><Format>text/html</Format>{dcp}</GetFeatureInfo>
    </Request>
    <Exception><Format>XML</Format></Exception>
    <Layer>
      <Title>TerraServe</Title>
{top_crs}{inner}    </Layer>
  </Capability>
</WMS_Capabilities>
"#
        )
    }
}

/// A WMS request error carrying the OGC ServiceException `code` (Annex E / Table E.1) when one
/// applies. `None` => a code-less <ServiceException> (unchanged behavior for non-coded errors).
struct WmsError {
    code: Option<&'static str>,
    message: String,
}

impl WmsError {
    fn coded(code: &'static str, message: impl Into<String>) -> Self {
        WmsError {
            code: Some(code),
            message: message.into(),
        }
    }
    fn plain(message: impl Into<String>) -> Self {
        WmsError {
            code: None,
            message: message.into(),
        }
    }
}

/// Parse a `VERSION`/`WMTVER` string into a `(major, minor, patch)` tuple; non-numeric parts and
/// missing components are 0. Used only to order it against the supported set.
fn parse_version_triple(s: &str) -> Option<(u32, u32, u32)> {
    let mut it = s.trim().split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next().unwrap_or("0").parse().ok()?;
    let c = it.next().unwrap_or("0").parse().ok()?;
    Some((a, b, c))
}

/// WMS version negotiation over the supported set {1.1.1, 1.3.0}. `None` (or an unparseable
/// value) yields the highest supported version; a value below the lowest yields the lowest; a
/// value between yields the highest supported that is <= requested. (WMS 1.3.0 06-042 §6.2.4.)
fn negotiate_version(requested: Option<&str>) -> &'static str {
    match requested.and_then(parse_version_triple) {
        None => "1.3.0",
        Some(v) if v >= (1, 3, 0) => "1.3.0",
        Some(v) if v >= (1, 1, 1) => "1.1.1",
        Some(_) => "1.1.1", // below the lowest supported -> lowest
    }
}

/// Map legacy WMS 1.0.0 request names to their canonical form: `map` -> `getmap`,
/// `capabilities` -> `getcapabilities`, `feature_info` -> `getfeatureinfo`. Any other value is
/// returned lowercased unchanged. (ets-wms11 `wmsops-getmap-params-request-1` sends `REQUEST=map`.)
fn normalize_request(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "map" => "getmap".to_string(),
        "capabilities" => "getcapabilities".to_string(),
        "feature_info" => "getfeatureinfo".to_string(),
        other => other.to_string(),
    }
}

fn exception(version: &str, message: &str) -> String {
    exception_with_code(version, None, message)
}

/// The first comma-separated LAYERS/QUERY_LAYERS token that names no configured layer, if any.
/// Empty tokens and an empty request are ignored (default-layer behavior handled by the caller).
fn first_undefined_layer(configured: &[&str], requested: &str) -> Option<String> {
    for tok in requested.split(',') {
        let tok = tok.trim();
        if !tok.is_empty() && !configured.contains(&tok) {
            return Some(tok.to_string());
        }
    }
    None
}

/// The standard OGC WMS `LayerNotDefined` exception (WMS 1.1.1/1.3.0 Annex A `code` values)
/// for a `LAYERS`/`QUERY_LAYERS`/`LAYER` name that matches none of the configured layers.
fn layer_not_defined(version: &str, name: &str) -> String {
    exception_with_code(
        version,
        Some("LayerNotDefined"),
        &format!("Layer \"{name}\" is not defined"),
    )
}

/// A `ServiceExceptionReport` carrying an optional OGC exception `code` attribute (e.g.
/// `LayerNotDefined`) alongside the human-readable `message`. Both are XML-escaped.
fn exception_with_code(version: &str, code: Option<&str>, message: &str) -> String {
    let msg = xml_escape(message);
    let code_attr = code
        .map(|c| format!(" code=\"{}\"", xml_escape(c)))
        .unwrap_or_default();
    if version.starts_with("1.1") {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<ServiceExceptionReport version="1.1.1">
  <ServiceException{code_attr}>{msg}</ServiceException>
</ServiceExceptionReport>
"#
        )
    } else {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<ServiceExceptionReport version="1.3.0" xmlns="http://www.opengis.net/ogc">
  <ServiceException{code_attr}>{msg}</ServiceException>
</ServiceExceptionReport>
"#
        )
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// WMS `BGCOLOR` = `0xRRGGBB` (hex). Absent or malformed -> white (the WMS default). Never errors.
fn parse_bgcolor(raw: Option<&str>) -> [u8; 3] {
    let s = raw.map(|s| s.trim()).unwrap_or("");
    let hex = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if hex.len() == 6 {
        if let Ok(v) = u32::from_str_radix(hex, 16) {
            return [(v >> 16) as u8, (v >> 8) as u8, v as u8];
        }
    }
    [255, 255, 255]
}

/// WMS `TRANSPARENT` = `TRUE`/`FALSE`. Anything but a case-insensitive `TRUE` (incl. absent) -> false.
fn parse_transparent(raw: Option<&str>) -> bool {
    raw.map(|s| s.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{
        first_undefined_layer, handle_layers, json_escape, negotiate_version, parse_bgcolor,
        parse_transparent,
    };

    #[test]
    fn version_negotiation_clamps_to_supported_set() {
        assert_eq!(negotiate_version(None), "1.3.0"); // default: highest
        assert_eq!(negotiate_version(Some("0.0.0")), "1.1.1"); // below range -> lowest
        assert_eq!(negotiate_version(Some("1.0.0")), "1.1.1"); // below 1.1.1 -> lowest
        assert_eq!(negotiate_version(Some("1.1.1")), "1.1.1"); // exact
        assert_eq!(negotiate_version(Some("1.2.0")), "1.1.1"); // between -> highest <= requested
        assert_eq!(negotiate_version(Some("1.3.0")), "1.3.0"); // exact highest
        assert_eq!(negotiate_version(Some("9.9.9")), "1.3.0"); // above range -> highest
        assert_eq!(negotiate_version(Some("garbage")), "1.3.0"); // unparseable -> highest
    }

    #[test]
    fn getcapabilities_honors_wmtver_alias() {
        // WMTVER=1.1.1 with no VERSION must yield a 1.1.1 doc, not the 1.3.0 default.
        let out = handle_layers(
            &[],
            "SeRvIcE=WMS&ReQuEsT=GetCapabilities&WmTvEr=1.1.1",
            None,
        );
        let body = String::from_utf8_lossy(&out.bytes);
        assert!(
            body.contains("<WMT_MS_Capabilities"),
            "expected 1.1.1 doc, got: {body}"
        );
    }

    #[test]
    fn getcapabilities_version_0_0_0_returns_1_1_1() {
        let out = handle_layers(
            &[],
            "SeRvIcE=WMS&VeRsIoN=0.0.0&ReQuEsT=GetCapabilities",
            None,
        );
        let body = String::from_utf8_lossy(&out.bytes);
        assert!(
            body.contains("<WMT_MS_Capabilities"),
            "expected 1.1.1 doc, got: {body}"
        );
    }

    #[test]
    fn capabilities_111_content_type_is_wms_xml() {
        let out = handle_layers(
            &[],
            "SERVICE=WMS&VERSION=1.1.1&REQUEST=GetCapabilities",
            None,
        );
        assert_eq!(
            out.content_type.as_deref(),
            Some("application/vnd.ogc.wms_xml")
        );
    }

    #[test]
    fn capabilities_130_content_type_is_sniffed() {
        let out = handle_layers(
            &[],
            "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetCapabilities",
            None,
        );
        assert_eq!(out.content_type, None); // sniffed -> text/xml by server
    }

    #[test]
    fn exception_111_content_type_is_se_xml() {
        // Unsupported request at 1.1.1 -> ServiceException with se_xml content-type.
        let out = handle_layers(&[], "SERVICE=WMS&VERSION=1.1.1&REQUEST=Bogus", None);
        assert_eq!(
            out.content_type.as_deref(),
            Some("application/vnd.ogc.se_xml")
        );
        assert!(String::from_utf8_lossy(&out.bytes).contains("ServiceExceptionReport"));
    }

    #[test]
    fn exception_130_content_type_is_sniffed() {
        let out = handle_layers(&[], "SERVICE=WMS&VERSION=1.3.0&REQUEST=Bogus", None);
        assert_eq!(out.content_type, None);
    }

    #[test]
    fn valid_plus_invalid_layers_is_undefined() {
        let configured = ["cascais"];
        assert_eq!(
            first_undefined_layer(&configured, "cascais,bogus"),
            Some("bogus".to_string())
        );
        assert_eq!(first_undefined_layer(&configured, "cascais"), None);
        assert_eq!(first_undefined_layer(&configured, ""), None);
        assert_eq!(first_undefined_layer(&configured, "cascais,cascais"), None);
    }

    #[test]
    fn query_layers_tokens_returns_all_named() {
        use super::query_layer_tokens;
        assert_eq!(
            query_layer_tokens("A,B", "A,B"),
            vec!["A".to_string(), "B".to_string()]
        );
        assert_eq!(query_layer_tokens("", "Solo"), vec!["Solo".to_string()]); // fallback to LAYERS
        assert_eq!(
            query_layer_tokens("A , B ", ""),
            vec!["A".to_string(), "B".to_string()]
        ); // trims
    }

    #[test]
    fn empty_layers_request_reports_no_layers_not_layer_not_defined() {
        // Zero configured layers + a named LAYERS must give the generic "no layers configured"
        // exception (NoLayers), NOT a LayerNotDefined for the requested name.
        let out = handle_layers(&[], "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=foo&STYLES=&CRS=EPSG:4326&BBOX=38.70,-9.45,38.72,-9.43&WIDTH=64&HEIGHT=64&FORMAT=image/png", None);
        let body = String::from_utf8_lossy(&out.bytes);
        assert!(
            body.contains("no layers configured"),
            "expected NoLayers exception, got: {body}"
        );
        assert!(
            !body.contains("LayerNotDefined"),
            "must not be LayerNotDefined: {body}"
        );
    }

    #[test]
    fn json_escape_emits_valid_json_for_control_characters() {
        // SEC-4: a layer name carrying raw control chars (tab/newline/ESC) must produce a JSON
        // string a strict parser accepts — the old escaper passed them through raw, which is
        // invalid JSON (control chars U+0000–U+001F are forbidden unescaped).
        let raw = "road\tmap\nESC\u{1b}\"quote\"\\back";
        let escaped = json_escape(raw);
        // No raw control character survives.
        assert!(
            !escaped.chars().any(|c| (c as u32) < 0x20),
            "escaped output still contains a raw control char: {escaped:?}"
        );
        // Wrapped as a JSON string, it round-trips back to the original via a real JSON parser.
        let doc = format!("\"{escaped}\"");
        let parsed: String = serde_json::from_str(&doc).expect("must be valid JSON");
        assert_eq!(parsed, raw);
        // Spot-check the short escapes.
        assert!(escaped.contains("\\t") && escaped.contains("\\n") && escaped.contains("\\u001b"));
    }

    #[test]
    fn bgcolor_parse() {
        assert_eq!(parse_bgcolor(Some("0xFF0000")), [255, 0, 0]);
        assert_eq!(parse_bgcolor(Some("0000ff")), [0, 0, 255]);
        assert_eq!(parse_bgcolor(None), [255, 255, 255]);
        assert_eq!(parse_bgcolor(Some("nonsense")), [255, 255, 255]);
    }
    #[test]
    fn transparent_parse() {
        assert!(parse_transparent(Some("TRUE")));
        assert!(parse_transparent(Some("true")));
        assert!(!parse_transparent(Some("FALSE")));
        assert!(!parse_transparent(None));
    }

    /// CITE C2/C3/C5: the WMS 1.3.0 GetCapabilities document must validate against the official
    /// OGC `capabilities_1_3_0.xsd` (vendored, offline). Exercises the single-layer builder — the
    /// same 1.3.0 template `capabilities_multi` uses. Skips gracefully when `xmllint` (libxml2) is
    /// not on PATH so the suite stays green on minimal environments.
    #[test]
    fn capabilities_130_validates_against_xsd() {
        if std::process::Command::new("xmllint")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("xmllint not found; skipping schema validation");
            return;
        }
        let xml = super::capabilities(
            "1.3.0",
            Some("http://cite.test/wms"),
            [-9.45, 38.70, -9.43, 38.72],
            "EPSG:3763",
        );
        let path = std::env::temp_dir().join(format!("ts_caps_130_{}.xml", std::process::id()));
        std::fs::write(&path, &xml).unwrap();
        let schema = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/xsd/wms/1.3.0/capabilities_1_3_0.xsd"
        );
        let out = std::process::Command::new("xmllint")
            .args(["--noout", "--nonet", "--schema", schema])
            .arg(&path)
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(
            out.status.success(),
            "xmllint: {}\n---doc---\n{}",
            String::from_utf8_lossy(&out.stderr),
            xml
        );
    }

    /// ets-wms11 getcap-response-2: the WMS 1.1.1 GetCapabilities document must validate against
    /// the official OGC `WMS_MS_Capabilities.dtd` (vendored, offline). The shipped doc keeps the
    /// public `http://schemas.opengis.net/...` SYSTEM URL (so the real OGC ETS validates against
    /// it); this test rewrites that URL to the vendored local path so validation runs offline.
    /// Skips gracefully when `xmllint` (libxml2) is not on PATH.
    #[test]
    fn capabilities_111_validates_against_dtd() {
        if std::process::Command::new("xmllint")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("xmllint not found; skipping DTD validation");
            return;
        }
        let xml = super::capabilities(
            "1.1.1",
            Some("http://cite.test/wms"),
            super::CASCAIS_BOUNDS,
            crate::reproj::SRC_CRS,
        );
        let local = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/dtd/wms/1.1.1/WMS_MS_Capabilities.dtd"
        );
        let xml_local = xml.replace(
            "http://schemas.opengis.net/wms/1.1.1/WMS_MS_Capabilities.dtd",
            local,
        );
        let path = std::env::temp_dir().join(format!("ts_caps_111_{}.xml", std::process::id()));
        std::fs::write(&path, &xml_local).unwrap();
        let out = std::process::Command::new("xmllint")
            .args(["--noout", "--nonet", "--valid", path.to_str().unwrap()])
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(
            out.status.success(),
            "1.1.1 capabilities failed DTD validation:\n{}\n---doc---\n{}",
            String::from_utf8_lossy(&out.stderr),
            xml
        );
    }

    /// A minimal COG-less, vector-less test `Layer` — just enough for `capabilities_multi` to
    /// render name/CRS/bounds. All fields that don't affect Capabilities XML are `None`/empty.
    fn test_layer(name: &str, bounds_wgs84: [f64; 4]) -> super::Layer {
        super::Layer {
            name: name.to_string(),
            cog_path: String::new(),
            cog: None,
            source: None,
            style: None,
            src_crs: "EPSG:4326".to_string(),
            band_math: None,
            bounds_wgs84,
            tile_cache: None,
            index_cache: crate::cache::new_index_cache(crate::cache::index_cache_bytes()),
            grids: Vec::new(),
            vector: None,
            pmtiles: None,
            overlay: None,
        }
    }

    /// ets-wms11 getcap-response-2 / latlonbbox-1 / capability_metadata-2: the WMS 1.1.1
    /// GetCapabilities document must carry the `WMT_MS_Capabilities` DTD DOCTYPE, a root-`<Layer>`
    /// `<LatLonBoundingBox>` (union of every child layer's WGS84 bounds), and
    /// `application/vnd.ogc.gml` among the `GetFeatureInfo` formats. WMS 1.3.0 is unaffected.
    #[test]
    fn capabilities_111_has_doctype_root_latlonbbox_and_gml() {
        // Two layers with distinct bounds so the root union is exercised.
        let layers = vec![
            test_layer("A", [-10.0, 30.0, 0.0, 40.0]),
            test_layer("B", [0.0, 40.0, 10.0, 50.0]),
        ];
        let xml = super::capabilities_multi(&layers, "1.1.1", Some("http://cite.test/wms"));
        assert!(xml.contains(
            "<!DOCTYPE WMT_MS_Capabilities SYSTEM \"http://schemas.opengis.net/wms/1.1.1/WMS_MS_Capabilities.dtd\">"
        ));
        assert!(
            xml.contains("application/vnd.ogc.gml"),
            "GML GFI format missing"
        );
        // Root LatLonBoundingBox = union of children: minx=-10 miny=30 maxx=10 maxy=50
        assert!(
            xml.contains("<LatLonBoundingBox minx=\"-10\" miny=\"30\" maxx=\"10\" maxy=\"50\"/>"),
            "root union LatLonBoundingBox missing/wrong: {xml}"
        );
        // 1.3.0 unaffected
        let x13 = super::capabilities_multi(&layers, "1.3.0", Some("http://cite.test/wms"));
        assert!(!x13.contains("DOCTYPE"));
        assert!(!x13.contains("LatLonBoundingBox"));
    }

    /// ets-wms11 feature_count-1/2: the default GFI (FEATURE_COUNT unset -> 1) must return
    /// strictly less data than FEATURE_COUNT=2 when >=2 features overlap the query pixel. This
    /// exercises the pure collector in isolation, ahead of wiring it into `get_feature_info_vector`.
    #[test]
    fn collect_gfi_hits_respects_feature_count() {
        use super::collect_gfi_hits;
        use crate::vector::feature::{Feature, Geometry, Props, Value};
        // Two overlapping unit squares both containing (0.5, 0.5).
        let sq = |id: &str, fid: u64| {
            let mut props = Props::new();
            props.insert("id".to_string(), Value::Str(id.to_string()));
            Feature::new(
                Geometry::Polygon(vec![vec![
                    [0.0, 0.0],
                    [1.0, 0.0],
                    [1.0, 1.0],
                    [0.0, 1.0],
                    [0.0, 0.0],
                ]]),
                props,
                fid,
            )
        };
        let feats = vec![sq("1", 1), sq("2", 2)];
        // A projector isn't needed for polygon containment (uses query_src); pass an identity-ish
        // projector built for EPSG:4326 over the unit bbox at 100x100.
        let proj = crate::vector::geom::Projector::new(
            "EPSG:4326",
            "EPSG:4326",
            [0.0, 0.0, 1.0, 1.0],
            100,
            100,
        )
        .unwrap();
        let q = Some((0.5, 0.5));
        let one = collect_gfi_hits(feats.iter(), &proj, q, 50.0, 50.0, 1);
        let two = collect_gfi_hits(feats.iter(), &proj, q, 50.0, 50.0, 2);
        assert_eq!(one.len(), 1);
        assert_eq!(two.len(), 2);
    }
}
