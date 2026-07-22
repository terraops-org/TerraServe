// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! GeoJSON reader (the MVP source; GPKG is the future default — see `source.rs`).
//!
//! Uses `serde_json` for the generic JSON tokenizing of untrusted external data (escaped
//! names, unicode); the **geo interpretation** (geometry, coordinate order, prop typing, the
//! stable `fid`) is ours. Supports `Point`/`LineString`/`Polygon`/`MultiLineString`/
//! `MultiPolygon`; anything else (e.g. `GeometryCollection`) is an error.

use super::feature::{Feature, Geometry, Props, Value};
use super::source::FeatureSource;

pub struct GeoJsonSource {
    features: Vec<Feature>,
    extent: [f64; 4],
}

/// A single `[lon, lat, ...]` coordinate pair. Extra elements (e.g. GeoJSON's optional
/// altitude) are ignored — the pipeline is 2D.
fn point(v: &serde_json::Value) -> Result<[f64; 2], String> {
    let c = v.as_array().ok_or("coordinates: not an array")?;
    let lon = c.first().and_then(|v| v.as_f64()).ok_or("bad lon")?;
    let lat = c.get(1).and_then(|v| v.as_f64()).ok_or("bad lat")?;
    Ok([lon, lat])
}

/// An array of coordinate pairs — a `LineString`'s points, or one `Polygon` ring.
fn ring(v: &serde_json::Value) -> Result<Vec<[f64; 2]>, String> {
    v.as_array()
        .ok_or("ring: not an array")?
        .iter()
        .map(point)
        .collect()
}

/// An array of rings — a `Polygon`'s coordinates (ring 0 = exterior, rest = holes), or one
/// `MultiLineString` member's shape is `Vec<Vec<[f64;2]>>` too, so this is shared by both.
fn polygon(v: &serde_json::Value) -> Result<Vec<Vec<[f64; 2]>>, String> {
    v.as_array()
        .ok_or("polygon: not an array")?
        .iter()
        .map(ring)
        .collect()
}

/// Widen a running `[w,s,e,n]` bbox (as four scalars) to include `p`.
fn extend_bbox(w: &mut f64, s: &mut f64, e: &mut f64, n: &mut f64, p: [f64; 2]) {
    *w = w.min(p[0]);
    *e = e.max(p[0]);
    *s = s.min(p[1]);
    *n = n.max(p[1]);
}

impl GeoJsonSource {
    pub fn load(path: &str) -> Result<GeoJsonSource, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
        Self::from_str(&text).map_err(|e| format!("{path}: {e}"))
    }

    pub fn from_str(text: &str) -> Result<GeoJsonSource, String> {
        let doc: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("json: {e}"))?;
        let feats = doc
            .get("features")
            .and_then(|f| f.as_array())
            .ok_or("no `features` array (not a FeatureCollection)")?;
        let mut out = Vec::with_capacity(feats.len());
        let (mut w, mut s, mut e, mut n) = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        for (i, f) in feats.iter().enumerate() {
            // A missing or null geometry is common in real datasets — skip it, don't fail the layer.
            let geom = match f.get("geometry") {
                Some(g) if !g.is_null() => g,
                _ => continue,
            };
            let gtype = geom.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let coords = geom
                .get("coordinates")
                .ok_or("geometry without coordinates")?;
            let parsed = match gtype {
                "Point" => Geometry::Point(point(coords)?),
                "LineString" => Geometry::LineString(ring(coords)?),
                "Polygon" => Geometry::Polygon(polygon(coords)?),
                "MultiLineString" => Geometry::MultiLineString(polygon(coords)?),
                "MultiPolygon" => {
                    let polys = coords
                        .as_array()
                        .ok_or("MultiPolygon: coordinates not an array")?
                        .iter()
                        .map(polygon)
                        .collect::<Result<Vec<_>, _>>()?;
                    Geometry::MultiPolygon(polys)
                }
                other => return Err(format!("unsupported geometry type `{other}`")),
            };
            match &parsed {
                Geometry::Point(p) => extend_bbox(&mut w, &mut s, &mut e, &mut n, *p),
                Geometry::LineString(pts) => {
                    for p in pts {
                        extend_bbox(&mut w, &mut s, &mut e, &mut n, *p);
                    }
                }
                Geometry::Polygon(rings) | Geometry::MultiLineString(rings) => {
                    for p in rings.iter().flatten() {
                        extend_bbox(&mut w, &mut s, &mut e, &mut n, *p);
                    }
                }
                Geometry::MultiPolygon(polys) => {
                    for p in polys.iter().flatten().flatten() {
                        extend_bbox(&mut w, &mut s, &mut e, &mut n, *p);
                    }
                }
            }
            let mut props = Props::new();
            let mut fid = i as u64; // fallback: parse order (fixture order is committed + stable)
            if let Some(obj) = f.get("properties").and_then(|p| p.as_object()) {
                for (k, v) in obj {
                    if let Some(sv) = v.as_str() {
                        props.insert(k.clone(), Value::Str(sv.to_string()));
                    } else if let Some(nv) = v.as_f64() {
                        props.insert(k.clone(), Value::Num(nv));
                        if k == "ne_id" {
                            fid = nv as u64; // stable id from the NE attribute
                        }
                    } else {
                        props.insert(k.clone(), Value::Null);
                    }
                }
            }
            out.push(Feature::new(parsed, props, fid));
        }
        if out.is_empty() {
            return Err("empty FeatureCollection".into());
        }
        Ok(GeoJsonSource {
            features: out,
            extent: [w, s, e, n],
        })
    }
}

impl FeatureSource for GeoJsonSource {
    fn features(&self) -> &[Feature] {
        &self.features
    }
    fn full_extent(&self) -> [f64; 4] {
        self.extent
    }
}
