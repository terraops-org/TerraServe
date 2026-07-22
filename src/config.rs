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
    /// The COG's own CRS. Defaults to the cascais grid.
    #[serde(default = "default_src_crs")]
    pub src_crs: String,
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

fn default_src_crs() -> String {
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
fn resolve_one(
    id: &str,
    tile_px: u32,
    cog: Option<(&Cog, &str)>,
    custom: &BTreeMap<String, GridConfig>,
) -> Result<TileMatrixSet, String> {
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
        assert_eq!(ndvi.src_crs, "EPSG:32629");
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
        assert_eq!(cas.src_crs, "EPSG:3763");
    }

    #[test]
    fn src_crs_defaults_when_omitted() {
        let cfg: Config =
            serde_yaml::from_str("layers:\n  - name: a\n    cog: a.tif\n    style: s.json\n")
                .unwrap();
        assert_eq!(cfg.layers[0].src_crs, "EPSG:3763");
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
}
