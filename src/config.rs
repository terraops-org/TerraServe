// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Multi-layer server config (`layers.yaml`).
//!
//! A `serve --config layers.yaml` file lists the WMS layers to publish. Each layer names a
//! COG (local path or `s3://…`), a style, its source CRS, and — optionally — an on-the-fly
//! band-math expression over named bands. `GetMap&LAYERS=<name>` selects one; GetCapabilities
//! lists them all. The single-layer `serve` flags remain a convenience shorthand for one layer.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::cog::Cog;
use crate::tms::{TileMatrixSet, TmLevel};

/// The whole config: an ordered list of layers (the first is the default for a GetMap with
/// an unknown/missing LAYERS), plus optional custom tile grids referenced by layers.
#[derive(Debug, Deserialize)]
pub struct Config {
    pub layers: Vec<LayerConfig>,
    /// Custom TileMatrixSet definitions, keyed by id — referenced by a layer's `grids:` list.
    #[serde(default)]
    pub grids: BTreeMap<String, GridConfig>,
}

/// One published WMS layer.
#[derive(Debug, Deserialize)]
pub struct LayerConfig {
    /// WMS layer name (the `LAYERS=` value clients request).
    pub name: String,
    /// COG source: a local path or an `s3://bucket/key` URL. Mutually exclusive with `vector`.
    #[serde(default)]
    pub cog: Option<String>,
    /// Path to the style (`style.json`). Required with `cog`.
    #[serde(default)]
    pub style: Option<String>,
    /// Vector source: a local GeoJSON or GeoPackage path. Mutually exclusive with `cog`.
    #[serde(default)]
    pub vector: Option<String>,
    /// Vector style (point/text/polygon/line Style IR JSON) for a `vector` layer. Required with `vector`.
    #[serde(default)]
    pub vec_style: Option<String>,
    /// The layer's source CRS, as the operator declared it. `None` = NOT declared.
    ///
    /// Deliberately an `Option` with no serde default. It used to be a `String` defaulting to
    /// `EPSG:3763`, which made "the operator wrote `src_crs: EPSG:3763`" and "the operator wrote
    /// nothing" the same value. `build_vector_layer` needs to tell those apart -- an unset CRS
    /// means "adopt the file header's", an explicit one must win over it -- so with no way to
    /// ask this field, it consulted the GLOBAL `--src-crs` flag instead. Under `--config` that
    /// flag is normally unset, so EVERY per-layer `src_crs:` was silently discarded in favour of
    /// the file header. The live Swiss demo (7 vector layers, all `src_crs: EPSG:2056`) was
    /// masked only because its FlatGeoBuf headers happened to agree.
    ///
    /// The COG path applies `default_src_crs()` itself, so its behaviour is unchanged.
    #[serde(default)]
    pub src_crs: Option<String>,
    /// On-the-fly band-math expression, e.g. `(B08 - B04) / (B08 + B04)`. When set, the layer
    /// is served as band math + value-domain pseudocolor instead of RGB passthrough.
    #[serde(default)]
    pub expression: Option<String>,
    /// Band alias → 1-based physical band position, e.g. `{B02: 1, B08: 4}`. Required with
    /// `expression`; lets the expression read domain names regardless of physical order.
    #[serde(default)]
    pub bands: BTreeMap<String, usize>,
    /// Source nodata value; pixels where any referenced band equals it are transparent.
    #[serde(default)]
    pub nodata: Option<f64>,
    /// Per-layer S3 endpoint / region overrides (else the process env / CLI defaults apply).
    #[serde(default)]
    pub s3_endpoint: Option<String>,
    #[serde(default)]
    pub s3_region: Option<String>,
    /// Tile grids this layer publishes on (TMS/WMTS). Each is `from_cog` (native), a well-known
    /// preset (`WebMercatorQuad` / `WorldCRS84Quad` / `UPSArcticWGS84Quad` / `UPSAntarcticWGS84Quad`,
    /// optionally with a `_{tile_px}` size suffix), or a custom id from the top-level `grids:` map.
    #[serde(default = "default_grids")]
    pub grids: Vec<String>,
    /// Tile pixel size for the preset / `from_cog` grids this layer names (128/256/512). Default 512.
    #[serde(default = "default_tile_px")]
    pub tile_px: u32,
    /// Pre-built PMTiles archive path(s) for this layer (read-through; a tile not in the matching-grid
    /// archive is live-encoded). Each archive self-describes its grid via `grid_id` metadata, so this
    /// is just a plain path list — one entry per grid, never mixed (design commitment 1). Default empty.
    #[serde(default)]
    pub pmtiles: Vec<String>,
    /// Pre-baked **PNG** archive path(s) for this layer's WMTS/TMS raster path (read-through; a tile
    /// not in the matching-grid archive is rendered live). The raster twin of `pmtiles:` above, kept
    /// separate because the two feed different front-ends with different payload formats — an
    /// archive listed under the wrong key is refused at startup, naming both formats, rather than
    /// serving image bytes to an MVT client (or the reverse). Bake with
    /// `build-pmtiles --tile-format png`. Default empty.
    #[serde(default)]
    pub raster_pmtiles: Vec<String>,
    /// Layer bounds in the source CRS, `[minx, miny, maxx, maxy]`.
    ///
    /// REQUIRED for a `postgis://` layer and authoritative there: TerraServe never issues
    /// `ST_EstimatedExtent` (NULL on a never-ANALYZEd table) or `ST_Extent` (a full-table scan
    /// that reproduced the 101-second cos2023 startup). Making the config the source of truth
    /// buys a deterministic startup. A wrong value gives wrong ADVERTISED bounds; queries are
    /// unaffected.
    #[serde(default)]
    pub extent: Option<[f64; 4]>,
    /// Extra attribute columns to fetch for a `postgis://` layer, on top of the ones the layer's
    /// `vec_style` references.
    ///
    /// A file source (`.gpkg`/`.fgb`) reads whatever the feature carries, but a database query has
    /// to name its columns up front, and TerraServe derives that list from the SERVER-side style
    /// (`Style::referenced_fields`). Anything the server-side style cannot see needs naming here —
    /// above all `--mvt-style`, which is client-side JSON served verbatim and never parsed into a
    /// `Style`, so its `["get", FIELD]` expressions are invisible to the engine. Without this, such
    /// a layer ships tiles with no class attribute and the client draws the whole map in its
    /// fallback colour. Ignored (with no effect) for every non-PostGIS source. Every name is
    /// checked against the table at startup, so a typo fails loudly instead of blanking the map.
    #[serde(default)]
    pub columns: Vec<String>,
}

/// A config-defined custom TileMatrixSet: explicit CRS + top-left origin + full extent + tile size
/// + an explicit resolution ladder (z0..zN). Matrix dims are derived from the extent so the grid is
/// TMS-indexable (validated at startup — see `resolve_grids`).
#[derive(Debug, Clone, Deserialize)]
pub struct GridConfig {
    pub crs: String,
    /// Top-left corner `[x, y]` in CRS units.
    pub origin: [f64; 2],
    /// Full grid extent `[minx, miny, maxx, maxy]` in CRS units (defines matrix coverage per level).
    pub extent: [f64; 4],
    #[serde(default = "default_tile_px")]
    pub tile_px: u32,
    /// CRS units per pixel, z0..zN (top-left convention). Use a dyadic ladder for a level-invariant
    /// (TMS-indexable) grid.
    pub resolutions: Vec<f64>,
}

impl GridConfig {
    pub fn to_tms(&self, id: &str) -> TileMatrixSet {
        let [minx, miny, maxx, maxy] = self.extent;
        let (w, h) = (maxx - minx, maxy - miny);
        let levels = self
            .resolutions
            .iter()
            .enumerate()
            .map(|(z, &r)| {
                let span = self.tile_px as f64 * r;
                TmLevel {
                    z: z as u32,
                    resolution: r,
                    matrix_w: ((w / span).ceil() as u32).max(1),
                    matrix_h: ((h / span).ceil() as u32).max(1),
                }
            })
            .collect();
        TileMatrixSet {
            id: id.to_string(),
            crs: self.crs.clone(),
            origin_x: self.origin[0],
            origin_y: self.origin[1],
            tile_w: self.tile_px,
            tile_h: self.tile_px,
            levels,
        }
    }
}

pub(crate) fn default_src_crs() -> String {
    "EPSG:3763".to_string()
}

pub fn default_grids() -> Vec<String> {
    vec!["from_cog".to_string()]
}

pub fn default_tile_px() -> u32 {
    512
}

/// Resolve ONE grid id → a validated `TileMatrixSet`. `cog` supplies the COG+CRS for `from_cog`
/// (None ⇒ `from_cog` errors — used by COG-less unit tests). Fails loudly if the id is unknown or the
/// resolved grid is not TMS-indexable (matrix·tile·resolution not level-invariant — the blocker class).
///
/// An id ENDING IN `.json` is a path to an OGC TileMatrixSet 2.0 document (read relative to the
/// process CWD — the same convention `--cog`/`--vector`/`--mvt-style` already use for a local path,
/// no `s3://` support here), loaded via `tms::from_ogc_json`. That path is checked FIRST (before
/// `from_cog`/preset/custom-map), and returns straight from the JSON without running the
/// level-invariance gate below: unlike a `GridConfig`, whose `matrixWidth`/`matrixHeight` are
/// DERIVED here from an extent + a resolution ladder (the gate exists to catch a bad derivation),
/// an OGC-JSON document's `matrixWidth`/`matrixHeight` are already explicit, authoritative, per-level
/// values straight from the file — there is no derivation step to protect. Real-world WMTS grids
/// (e.g. swisstopo's own LV95 pyramid, `fixtures/grids/swissLV95.json`) are routinely NOT
/// level-invariant (a non-dyadic resolution ladder over a fixed extent), which the single-`<Origin>`
/// TMS 1.0.0 client-indexing concern the gate protects against doesn't apply to — WMTS/MVT tile
/// lookups (`TileMatrixSet::tile_bounds`) read each level's own `matrix_w`/`matrix_h` and are correct
/// regardless of invariance; only a TMS 1.0.0 client computing tile position from Y + resolution
/// alone (without per-level dims) would care, and that front-end already only advertises `full_extent`
/// (level 0) for such a grid, a pre-existing, documented limitation (not one this task changes).
///
/// CAVEAT scoping the "correct regardless of invariance" claim above: only tile GEOMETRY is
/// invariance-agnostic. The OPT-IN, WebMercator-calibrated MVT feature-size heuristics —
/// `--mvt-min-feature-px` and the per-zoom LOD tolerance, both routed through
/// `vector::mvt::tile::merc_m_per_px` (a hardcoded `2^z` ladder) — ASSUME dyadic level doubling, so
/// on a non-dyadic `.json` grid they over/under-thin features by the ratio between the real
/// `cellSize` and the Mercator `2^z` resolution (an ~84x error at some `swissLV95` levels). This
/// knob is OFF by default (`min_feature_px = 0.0`) and never affects tile geometry or the default
/// path; the design spec declares these heuristics out-of-scope for v1. Fast-follow fix: make
/// `min_area_src_for_zoom`/the LOD tolerance read `level(z).resolution` instead of `2^z`.
fn resolve_one(
    id: &str,
    tile_px: u32,
    cog: Option<(&Cog, &str)>,
    custom: &BTreeMap<String, GridConfig>,
) -> Result<TileMatrixSet, String> {
    if id.ends_with(".json") {
        let json = std::fs::read_to_string(id).map_err(|e| format!("grid '{id}': {e}"))?;
        return crate::tms::from_ogc_json(&json).map_err(|e| format!("grid '{id}': {e}"));
    }
    let tms = if id == "from_cog" {
        let (cog, crs) = cog.ok_or("grid 'from_cog' requires a COG")?;
        TileMatrixSet::from_cog(cog, crs, tile_px)
    } else if let Some(g) = crate::tms::preset(id, tile_px) {
        g
    } else if let Some(gc) = custom.get(id) {
        gc.to_tms(id)
    } else {
        return Err(format!("unknown grid id '{id}'"));
    };
    if !tms.is_level_invariant() {
        return Err(format!(
            "grid '{id}' is not TMS-indexable: matrix×tile×resolution varies across zoom levels \
             (use a dyadic resolution ladder + an extent that is a power-of-two multiple of the tile)"
        ));
    }
    Ok(tms)
}

/// Resolve a layer's grid id list, EXCLUDING `from_cog` (no COG available). For unit tests.
pub fn resolve_grids_presets(
    ids: &[String],
    tile_px: u32,
    custom: &BTreeMap<String, GridConfig>,
) -> Result<Vec<TileMatrixSet>, String> {
    ids.iter()
        .map(|id| resolve_one(id, tile_px, None, custom))
        .collect()
}

/// Resolve a layer's full grid id list (including `from_cog`, which needs the parsed COG + CRS).
pub fn resolve_grids(
    ids: &[String],
    tile_px: u32,
    cog: &Cog,
    crs: &str,
    custom: &BTreeMap<String, GridConfig>,
) -> Result<Vec<TileMatrixSet>, String> {
    ids.iter()
        .map(|id| resolve_one(id, tile_px, Some((cog, crs)), custom))
        .collect()
}

impl Config {
    /// Load and parse a YAML config file.
    pub fn load(path: &str) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("read config {path}: {e}"))?;
        let cfg: Config = serde_yaml::from_str(&text).map_err(|e| format!("parse config: {e}"))?;
        if cfg.layers.is_empty() {
            return Err("config has no layers".into());
        }
        for l in &cfg.layers {
            l.validate()?;
        }
        Ok(cfg)
    }
}

impl LayerConfig {
    /// Exactly one source (cog XOR vector); each needs its matching style.
    pub fn validate(&self) -> Result<(), String> {
        match (self.cog.is_some(), self.vector.is_some()) {
            (true, true) => Err(format!(
                "layer '{}': set either `cog` or `vector`, not both",
                self.name
            )),
            (false, false) => Err(format!(
                "layer '{}': needs a `cog` or a `vector` source",
                self.name
            )),
            (true, false) => {
                if self.style.is_none() {
                    return Err(format!(
                        "layer '{}': a `cog` layer needs a `style`",
                        self.name
                    ));
                }
                Ok(())
            }
            (false, true) => {
                if self.vec_style.is_none() {
                    return Err(format!(
                        "layer '{}': a `vector` layer needs a `vec_style`",
                        self.name
                    ));
                }
                Ok(())
            }
        }
    }

    /// Band names in **physical order** (index `i` is physical band `i+1`), derived from the
    /// `bands` map — the ordering the expression compiler and decoder expect.
    pub fn band_names_ordered(&self) -> Vec<String> {
        let maxpos = self.bands.values().copied().max().unwrap_or(0);
        let mut names = vec![String::new(); maxpos];
        for (name, &pos) in &self.bands {
            if (1..=maxpos).contains(&pos) {
                names[pos - 1] = name.clone();
            }
        }
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_two_layer_config() {
        let yaml = r#"
layers:
  - name: ndvi
    cog: s3://terraserve-cogs/s2_stack.cog.tif
    src_crs: EPSG:32629
    nodata: -32768
    bands: { B02: 1, B03: 2, B04: 3, B08: 4 }
    expression: "(B08 - B04) / (B08 + B04)"
    style: fixtures/styles/ndvi.json
  - name: cascais
    cog: ../cogs/cascais.cog.deflate.tif
    src_crs: EPSG:3763
    style: fixtures/styles/rgb.json
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.layers.len(), 2);
        let ndvi = &cfg.layers[0];
        assert_eq!(ndvi.name, "ndvi");
        assert_eq!(ndvi.src_crs.as_deref(), Some("EPSG:32629"));
        assert_eq!(ndvi.nodata, Some(-32768.0));
        assert_eq!(
            ndvi.expression.as_deref(),
            Some("(B08 - B04) / (B08 + B04)")
        );
        // bands map -> physical-order names
        assert_eq!(ndvi.band_names_ordered(), vec!["B02", "B03", "B04", "B08"]);
        // second layer defaults: no expression, src_crs from file
        let cas = &cfg.layers[1];
        assert!(cas.expression.is_none());
        assert_eq!(cas.src_crs.as_deref(), Some("EPSG:3763"));
    }

    #[test]
    fn src_crs_defaults_when_omitted() {
        let cfg: Config =
            serde_yaml::from_str("layers:\n  - name: a\n    cog: a.tif\n    style: s.json\n")
                .unwrap();
        // An omitted `src_crs:` is now None, NOT the cascais default. The COG path applies
        // that default itself; the vector path needs the None to mean "use the header".
        assert_eq!(cfg.layers[0].src_crs, None);
    }

    #[test]
    fn config_accepts_a_vector_layer() {
        let yaml = "layers:\n  - name: Lakes\n    vector: data/Lakes.geojson\n    vec_style: data/cite.vec.json\n    src_crs: EPSG:4326\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.layers.len(), 1);
        assert!(cfg.layers[0].cog.is_none());
        assert_eq!(cfg.layers[0].vector.as_deref(), Some("data/Lakes.geojson"));
        cfg.layers[0].validate().unwrap(); // ok
    }
    #[test]
    fn config_rejects_both_and_neither() {
        let both = "layers:\n  - name: x\n    cog: a.tif\n    style: s.json\n    vector: v.geojson\n    vec_style: c.json\n";
        let c: Config = serde_yaml::from_str(both).unwrap();
        assert!(c.layers[0].validate().is_err());
        let neither = "layers:\n  - name: y\n";
        let c2: Config = serde_yaml::from_str(neither).unwrap();
        assert!(c2.layers[0].validate().is_err());
    }

    // NOTE: the plan's brief called for `Config::parse_str(...)`, which does not exist on this
    // struct — every neighbouring test above parses via `serde_yaml::from_str::<Config>(...)`
    // directly, so these two follow that existing pattern instead of inventing a new helper.
    #[test]
    fn extent_parses_as_four_floats() {
        let cfg: Config = serde_yaml::from_str(
            "layers:\n  - name: p\n    vector: postgis://ts:${P}@db/gis/parcels\n    \
             vec_style: s.json\n    extent: [2485000.0, 1075000.0, 2834000.0, 1296000.0]\n",
        )
        .unwrap();
        assert_eq!(
            cfg.layers[0].extent,
            Some([2485000.0, 1075000.0, 2834000.0, 1296000.0])
        );
    }

    #[test]
    fn extent_is_none_when_omitted() {
        let cfg: Config = serde_yaml::from_str(
            "layers:\n  - name: p\n    vector: a.fgb\n    vec_style: s.json\n",
        )
        .unwrap();
        assert_eq!(cfg.layers[0].extent, None);
    }
}
