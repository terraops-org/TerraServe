// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! OSGeo TMS 1.0.0 HTTP front-end. A thin adapter over the tile-service core: the resource
//! documents + the tile route. **This is the ONLY place the TMS bottom-left y-axis lives** — the
//! core is stored top-left (WMTS convention). Reference: docs/tilematrixset-reference.md §"OSGeo TMS".
//!
//! Layout (owner decision D1 — the standard, interoperable OSGeo hierarchy):
//!   `/tms/1.0.0/`                    → `<TileMapService>` listing every layer×grid `<TileMap>`
//!   `/tms/1.0.0/{layer}[@{grid}]`    → `<TileMap>` (SRS, BoundingBox, bottom-left Origin, TileSets)
//!   `/tms/1.0.0/{layer}[@{grid}]/{z}/{x}/{y}.png` → the tile (y is bottom-left)

use crate::server::{Layer, PublishedGrid, ServeState};
use crate::tms::{strip_size_suffix, TileFactory, TileMatrixSet, TileRequest};

/// The TMS root URL, derived from the advertised WMS base (`…/wms` → `…/tms/1.0.0`).
pub fn tms_root(base_url: &str) -> String {
    let origin = base_url
        .strip_suffix("/wms")
        .unwrap_or(base_url)
        .trim_end_matches('/');
    format!("{origin}/tms/1.0.0")
}

/// TMS tile y (bottom-left, 0 at south) → core row (top-left, 0 at north). `None` if out of range.
pub fn tms_y_to_core_row(grid: &TileMatrixSet, z: u32, y: u32) -> Option<u32> {
    let lvl = grid.level(z)?;
    if y >= lvl.matrix_h {
        return None;
    }
    Some(lvl.matrix_h - 1 - y)
}

/// Split `"layer@grid"` → `(layer, Some(grid))`; `"layer"` → `(layer, None)`.
pub fn parse_layer_spec(spec: &str) -> (String, Option<String>) {
    match spec.split_once('@') {
        Some((l, g)) => (l.to_string(), Some(g.to_string())),
        None => (spec.to_string(), None),
    }
}

/// OSGeo TMS `profile` for a grid: the canonical 256-px WebMercator/CRS84 grids map to the
/// well-known profiles clients key on; everything else (from_cog, custom, non-256) is `local`.
pub fn tms_profile(grid: &TileMatrixSet) -> &'static str {
    match strip_size_suffix(&grid.id) {
        "WebMercatorQuad" if grid.tile_w == 256 => "global-mercator",
        "WorldCRS84Quad" if grid.tile_w == 256 => "global-geodetic",
        _ => "local",
    }
}

fn pick_grid<'a>(layer: &'a Layer, grid_id: &Option<String>) -> Option<&'a PublishedGrid> {
    match grid_id {
        // Match the stored id OR its base name, so `@WebMercatorQuad` finds a `WebMercatorQuad_512`.
        Some(id) => layer
            .grids
            .iter()
            .find(|g| g.tms.id == *id || strip_size_suffix(&g.tms.id) == id),
        None => layer.grids.first(),
    }
}

/// `/tms/1.0.0/` — the TileMapService: one `<TileMap>` per layer×grid across all layers (D1).
pub fn tilemapservice_xml(state: &ServeState, root: &str) -> String {
    let mut maps = String::new();
    for layer in &state.layers {
        for g in &layer.grids {
            maps.push_str(&format!(
                "    <TileMap title=\"{n}\" srs=\"{crs}\" profile=\"{prof}\" href=\"{root}/{n}@{gid}\"/>\n",
                n = layer.name,
                crs = g.tms.crs,
                prof = tms_profile(&g.tms),
                gid = g.tms.id,
            ));
        }
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<TileMapService version=\"1.0.0\" services=\"{root}\">\n  <Title>TerraServe TMS</Title>\n  <Abstract>Cloud-Optimized GeoTIFF tiles</Abstract>\n  <TileMaps>\n{maps}  </TileMaps>\n</TileMapService>\n"
    )
}

/// `<TileMap>` for one layer×grid. `bounds` is the layer data extent in the grid CRS (falls back to
/// the grid's full extent). Origin is the **bottom-left** corner; one `<TileSet>` per zoom.
pub fn tilemap_xml_for(
    layer_name: &str,
    grid: &TileMatrixSet,
    bounds: Option<[f64; 4]>,
    root: &str,
) -> String {
    let ext = grid.full_extent().unwrap_or([0.0, 0.0, 0.0, 0.0]);
    let bb = bounds.unwrap_or(ext);
    let mut tilesets = String::new();
    for l in &grid.levels {
        tilesets.push_str(&format!(
            "      <TileSet href=\"{root}/{layer_name}@{gid}/{z}\" units-per-pixel=\"{upp}\" order=\"{z}\"/>\n",
            gid = grid.id,
            z = l.z,
            upp = l.resolution,
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<TileMap version=\"1.0.0\" tilemapservice=\"{root}\">\n  <Title>{layer_name}</Title>\n  <SRS>{crs}</SRS>\n  <BoundingBox minx=\"{bminx}\" miny=\"{bminy}\" maxx=\"{bmaxx}\" maxy=\"{bmaxy}\"/>\n  <Origin x=\"{ox}\" y=\"{oy}\"/>\n  <TileFormat width=\"{tw}\" height=\"{th}\" mime-type=\"image/png\" extension=\"png\"/>\n  <TileSets profile=\"{prof}\">\n{tilesets}  </TileSets>\n</TileMap>\n",
        crs = grid.crs,
        bminx = bb[0],
        bminy = bb[1],
        bmaxx = bb[2],
        bmaxy = bb[3],
        ox = ext[0], // bottom-left corner (TMS Origin)
        oy = ext[1],
        tw = grid.tile_w,
        th = grid.tile_h,
        prof = tms_profile(grid),
    )
}

/// The `<TileMap>` document for `/tms/1.0.0/{layerspec}`. `Err((404, msg))` for an unknown
/// layer/grid.
pub fn tilemap_doc(
    state: &ServeState,
    layerspec: &str,
    root: &str,
) -> Result<String, (u16, String)> {
    let (lname, gid) = parse_layer_spec(layerspec);
    let layer = state
        .layers
        .iter()
        .find(|l| l.name == lname)
        .ok_or((404u16, format!("no layer '{lname}'")))?;
    let pg = pick_grid(layer, &gid).ok_or((404u16, format!("no grid {gid:?} on '{lname}'")))?;
    Ok(tilemap_xml_for(&layer.name, &pg.tms, pg.data_bounds, root))
}

/// Render a TMS tile: parse the spec, pick the grid, apply the bottom-left→top-left y-flip, render.
/// `Err((status, msg))` — 404 for unknown layer/grid or out-of-range; 5xx for a render failure.
pub fn render_tms_tile(
    state: &ServeState,
    layerspec: &str,
    z: u32,
    x: u32,
    y: u32,
) -> Result<Vec<u8>, (u16, String)> {
    let (lname, gid) = parse_layer_spec(layerspec);
    let layer = state
        .layers
        .iter()
        .find(|l| l.name == lname)
        .ok_or((404u16, format!("no layer '{lname}'")))?;
    let pg = pick_grid(layer, &gid).ok_or((404u16, format!("no grid {gid:?} on '{lname}'")))?;
    let lvl = pg
        .tms
        .level(z)
        .ok_or((404u16, format!("no zoom level {z}")))?;
    if x >= lvl.matrix_w {
        return Err((
            404,
            format!("tile column {x} out of range (0..{})", lvl.matrix_w),
        ));
    }
    let row =
        tms_y_to_core_row(&pg.tms, z, y).ok_or((404u16, format!("tile row {y} out of range")))?;
    // Vector layers are not served on tile paths in the MVP (tiled labeling is a later rung) —
    // fail cleanly instead of panicking on the now-optional COG.
    let cog = layer.cog.as_deref().ok_or((
        400u16,
        "vector layer is not tiled — use WMS GetMap".to_string(),
    ))?;
    let source = layer
        .source
        .as_deref()
        .ok_or((500u16, "layer has no source".to_string()))?;
    let style = layer
        .style
        .as_ref()
        .ok_or((500u16, "layer has no style".to_string()))?;
    let req = TileRequest {
        cog,
        source,
        cog_path: &layer.cog_path,
        src_crs: &layer.src_crs,
        style,
        band_math: layer.band_math.as_ref(),
        cache: layer.tile_cache.as_ref(),
        index_cache: &layer.index_cache,
        data_bounds: pg.data_bounds,
        grid: &pg.tms,
        z,
        col: x,
        row,
    };
    // Pre-checks above cover the out-of-range 404s; a failure here is a render/IO error → 5xx.
    TileFactory::render_tile(&req).map_err(|e| (500u16, e))
}

/// proj4 definition for the CRSs the built-in viewer needs (OpenLayers ships 3857/4326; the polar
/// stereographic CRSs must be registered). `None` ⇒ rely on OL's built-in (or the viewer can't
/// project a truly custom CRS — a convenience-viewer limitation, not a service one).
fn proj4_def(crs: &str) -> Option<&'static str> {
    match crs.to_ascii_uppercase().as_str() {
        "EPSG:3413" => Some("+proj=stere +lat_0=90 +lat_ts=70 +lon_0=-45 +k=1 +x_0=0 +y_0=0 +datum=WGS84 +units=m +no_defs"),
        "EPSG:3031" => Some("+proj=stere +lat_0=-90 +lat_ts=-71 +lon_0=0 +k=1 +x_0=0 +y_0=0 +datum=WGS84 +units=m +no_defs"),
        "EPSG:5041" => Some("+proj=stere +lat_0=90 +lat_ts=90 +lon_0=0 +k=0.994 +x_0=2000000 +y_0=2000000 +datum=WGS84 +units=m +no_defs"),
        "EPSG:5042" => Some("+proj=stere +lat_0=-90 +lat_ts=-90 +lon_0=0 +k=0.994 +x_0=2000000 +y_0=2000000 +datum=WGS84 +units=m +no_defs"),
        _ => None,
    }
}

/// The built-in map viewer (`GET /viewer`). A self-contained OpenLayers page (OL + proj4 from a CDN
/// per owner decision D2 — this is served to a real browser, not an Artifact) pointed at the first
/// layer's first grid. A **convenience**, not a gated deliverable: the tile endpoint is what's tested.
/// Dark viewer for a **vector** (label) layer: an OpenLayers `ImageWMS` overlay (a single,
/// re-fetched WMS GetMap image — no tile seams, per the MVP scope) over a dimmed OSM basemap.
fn vector_viewer_html(base_url: &str, layer: &crate::server::Layer) -> String {
    let b = layer.bounds_wgs84;
    let clon = (b[0] + b[2]) / 2.0;
    let clat = (b[1] + b[3]) / 2.0;
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8">
<meta name="color-scheme" content="dark">
<title>TerraServe — {name} (vector)</title>
<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/ol@9/ol.css">
<style>html,body,#map{{margin:0;height:100%;width:100%}}
body,#map{{background:#0d1117}}
#hud{{position:absolute;z-index:1;top:8px;left:8px;background:rgba(13,17,23,.82);color:#e6edf3;
font:12px/1.4 system-ui,sans-serif;padding:6px 10px;border-radius:6px;border:1px solid rgba(255,255,255,.08)}}
#hud b{{color:#79c0ff}}
.ol-control button{{background:rgba(22,27,34,.9);color:#e6edf3;border-radius:4px}}
.ol-control button:hover,.ol-control button:focus{{background:rgba(48,54,61,.95);color:#fff}}
.ol-attribution{{background:rgba(13,17,23,.7)!important;color:#c9d1d9}}
.ol-attribution a{{color:#79c0ff}}</style></head>
<body><div id="map"></div><div id="hud">{name} · <b>vector</b> · WMS GetMap labels</div>
<script src="https://cdn.jsdelivr.net/npm/ol@9/dist/ol.js"></script>
<script>
var wms = new ol.source.ImageWMS({{
  url: '{wms}',
  params: {{'LAYERS': '{name}', 'FORMAT': 'image/png', 'TRANSPARENT': true, 'VERSION': '1.3.0'}},
  ratio: 1
}});
var map = new ol.Map({{
  target: 'map',
  layers: [
    new ol.layer.Tile({{source: new ol.source.OSM(), opacity: 0.5}}),
    new ol.layer.Image({{source: wms}})
  ],
  view: new ol.View({{center: ol.proj.fromLonLat([{clon}, {clat}]), zoom: 4}})
}});
</script></body></html>"#,
        name = layer.name,
        wms = base_url,
        clon = clon,
        clat = clat,
    )
}

pub fn viewer_html(state: &ServeState) -> String {
    let root = tms_root(&state.base_url);
    let Some(layer) = state.layers.first() else {
        return "<!doctype html><meta charset=utf-8><title>TerraServe</title><p>No layers published.".to_string();
    };
    // A vector (label) layer has no tile grids — serve it as a single WMS GetMap image
    // (OpenLayers ImageWMS) over an OSM basemap so the labels are visible.
    if layer.vector.is_some() {
        return vector_viewer_html(&state.base_url, layer);
    }
    let Some(pg) = layer.grids.first() else {
        return format!(
            "<!doctype html><meta charset=utf-8><title>TerraServe</title><p>Layer '{}' publishes no tile grids.",
            layer.name
        );
    };
    let g = &pg.tms;
    let res: Vec<String> = g.levels.iter().map(|l| l.resolution.to_string()).collect();
    let ext = g.full_extent().unwrap_or([0.0, 0.0, 0.0, 0.0]);
    // Frame the data if we know it in grid CRS, else the whole grid.
    let fit = pg.data_bounds.unwrap_or(ext);
    let mh0 = g.levels.first().map(|l| l.matrix_h).unwrap_or(1);
    let proj4 = proj4_def(&g.crs);
    let proj4_js = match proj4 {
        Some(def) => format!(
            "proj4.defs('{crs}', '{def}'); ol.proj.proj4.register(proj4);",
            crs = g.crs
        ),
        None => String::new(),
    };
    let layer_at_grid = format!("{}@{}", layer.name, g.id);

    format!(
        r#"<!doctype html><html><head><meta charset="utf-8">
<meta name="color-scheme" content="dark">
<title>TerraServe — {name} ({gid})</title>
<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/ol@9/ol.css">
<style>html,body,#map{{margin:0;height:100%;width:100%}}
body,#map{{background:#0d1117}}
#hud{{position:absolute;z-index:1;top:8px;left:8px;background:rgba(13,17,23,.82);color:#e6edf3;
font:12px/1.4 system-ui,sans-serif;padding:6px 10px;border-radius:6px;border:1px solid rgba(255,255,255,.08)}}
#hud b{{color:#79c0ff}}
.ol-control button{{background:rgba(22,27,34,.9);color:#e6edf3;border-radius:4px}}
.ol-control button:hover,.ol-control button:focus{{background:rgba(48,54,61,.95);color:#fff}}
.ol-attribution{{background:rgba(13,17,23,.7)!important;color:#c9d1d9}}
.ol-attribution a{{color:#79c0ff}}
.ol-attribution button{{color:#e6edf3}}</style></head>
<body><div id="map"></div><div id="hud">{name} · grid <b>{gid}</b> · {crs} · {tile}px</div>
<script src="https://cdn.jsdelivr.net/npm/proj4@2/dist/proj4.js"></script>
<script src="https://cdn.jsdelivr.net/npm/ol@9/dist/ol.js"></script>
<script>
{proj4_js}
var crs = '{crs}';
var resolutions = [{res}];
var origin = [{ox}, {oy}];
var extent = [{eminx}, {eminy}, {emaxx}, {emaxy}];
var fit = [{fminx}, {fminy}, {fmaxx}, {fmaxy}];
var tileSize = {tile};
var mh0 = {mh0};
var root = '{root}';
var layerAtGrid = '{lag}';
var proj = ol.proj.get(crs);
if (proj) {{ proj.setExtent(extent); }}
var tileGrid = new ol.tilegrid.TileGrid({{origin: origin, resolutions: resolutions, tileSize: tileSize, extent: extent}});
var source = new ol.source.TileImage({{
  projection: proj, tileGrid: tileGrid,
  tileUrlFunction: function(tc) {{
    if (!tc) return undefined;
    var z = tc[0], x = tc[1], coreRow = tc[2];
    if (x < 0 || coreRow < 0) return undefined;
    var nrows = mh0 * Math.pow(2, z);
    var tmsY = nrows - 1 - coreRow;   // OL top-left core row -> TMS bottom-left y
    if (tmsY < 0) return undefined;
    return root + '/' + layerAtGrid + '/' + z + '/' + x + '/' + tmsY + '.png';
  }}
}});
var map = new ol.Map({{
  target: 'map',
  layers: [new ol.layer.Tile({{source: source}})],
  view: new ol.View({{projection: proj, center: [(extent[0]+extent[2])/2, (extent[1]+extent[3])/2], zoom: 2}})
}});
map.getView().fit(fit, {{padding: [20,20,20,20]}});
</script></body></html>"#,
        name = layer.name,
        gid = g.id,
        crs = g.crs,
        tile = g.tile_w,
        res = res.join(","),
        ox = ext[0],
        oy = ext[3], // TileGrid origin = TOP-LEFT (top-left convention); OL flips to TMS in JS.
        eminx = ext[0],
        eminy = ext[1],
        emaxx = ext[2],
        emaxy = ext[3],
        fminx = fit[0],
        fminy = fit[1],
        fmaxx = fit[2],
        fmaxy = fit[3],
        mh0 = mh0,
        root = root,
        lag = layer_at_grid,
    )
}
