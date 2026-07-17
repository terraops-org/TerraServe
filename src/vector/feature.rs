// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! The internal feature model — what every `FeatureSource` yields into the pipeline.

use std::collections::BTreeMap;

/// A typed attribute value. Kept minimal: labels are strings, priorities are numbers.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Str(String),
    Num(f64),
    Null,
}

/// A feature's attribute bag. `BTreeMap` keeps iteration deterministic (never hash order).
#[derive(Clone, Debug, Default)]
pub struct Props(BTreeMap<String, Value>);

impl Props {
    pub fn new() -> Self {
        Props(BTreeMap::new())
    }
    pub fn insert(&mut self, key: String, value: Value) {
        self.0.insert(key, value);
    }
    /// A string attribute (e.g. the label field). `None` if absent or non-string.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        match self.0.get(key) {
            Some(Value::Str(s)) => Some(s.as_str()),
            _ => None,
        }
    }
    /// A numeric attribute (e.g. the priority field). `None` if absent or non-numeric.
    pub fn get_f64(&self, key: &str) -> Option<f64> {
        match self.0.get(key) {
            Some(Value::Num(n)) => Some(*n),
            _ => None,
        }
    }
    /// The raw typed value for a key. Unlike the coercing getters, this returns the `Value` as
    /// stored — the cell mosaic votes on the class attribute's exact value (`Str` vs `Num` matters
    /// for dedup and for tagging the emitted rectangle). `None` if the key is absent.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }
    /// A display string for any typed attribute — the string as-is for `Value::Str`, or a
    /// stringified number for `Value::Num` (an integer-valued float like `3.0` prints `"3"`,
    /// never `"3.0"`). `None` for `Value::Null` or an absent key.
    ///
    /// Unlike `get_str` (string-only, used by filter eval / priority lookups where a type
    /// mismatch should be silently absent), this is for **display** contexts — the label text
    /// and GetFeatureInfo — where a numeric attribute (`pop_max`, `scalerank`, …) used as a
    /// label field must still render/report, not go blank.
    pub fn get_display(&self, key: &str) -> Option<String> {
        match self.0.get(key) {
            Some(Value::Str(s)) => Some(s.clone()),
            Some(Value::Num(n)) => Some(format_num(*n)),
            Some(Value::Null) | None => None,
        }
    }
    /// All key/value pairs, in stable `BTreeMap` (key-sorted) order — used by the MVT encoder to
    /// build the layer-wide key/value attribute pools.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter()
    }
}

/// Format a number for display: drop the trailing `.0` for an integer-valued float (`3.0` →
/// `"3"`), otherwise print the natural float representation.
fn format_num(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// Geometry in the source CRS. A `Polygon`'s ring 0 is the exterior; any further rings are
/// holes (GeoJSON winding order is not enforced here — the renderer decides how to fill).
#[derive(Clone, Debug)]
pub enum Geometry {
    Point([f64; 2]),
    LineString(Vec<[f64; 2]>),
    Polygon(Vec<Vec<[f64; 2]>>),
    MultiLineString(Vec<Vec<[f64; 2]>>),
    MultiPolygon(Vec<Vec<Vec<[f64; 2]>>>),
}

impl Geometry {
    /// Axis-aligned bbox of all vertices, `[minx, miny, maxx, maxy]` in the source CRS. `None` for a
    /// geometry with no vertices (which never overlaps anything). Cheap — no projection.
    pub fn compute_bbox(&self) -> Option<[f64; 4]> {
        fn walk(pts: &[[f64; 2]], b: &mut [f64; 4]) {
            for p in pts {
                if p[0] < b[0] {
                    b[0] = p[0];
                }
                if p[1] < b[1] {
                    b[1] = p[1];
                }
                if p[0] > b[2] {
                    b[2] = p[0];
                }
                if p[1] > b[3] {
                    b[3] = p[1];
                }
            }
        }
        let mut b = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        match self {
            Geometry::Point(p) => walk(std::slice::from_ref(p), &mut b),
            Geometry::LineString(l) => walk(l, &mut b),
            Geometry::Polygon(rings) => rings.iter().for_each(|r| walk(r, &mut b)),
            Geometry::MultiLineString(parts) => parts.iter().for_each(|r| walk(r, &mut b)),
            Geometry::MultiPolygon(polys) => polys
                .iter()
                .for_each(|poly| poly.iter().for_each(|r| walk(r, &mut b))),
        }
        if b[0] <= b[2] {
            Some(b)
        } else {
            None
        }
    }

    /// Absolute planar area in **source-CRS units²** (shoelace). A `Polygon` is `|exterior| −
    /// Σ|holes|` (ring 0 exterior, the rest holes — this type's convention), clamped ≥ 0; a
    /// `MultiPolygon` sums its polygons; `Point`/`LineString`/`MultiLineString` have zero area.
    /// Winding-independent (the sign is discarded). Used to rank features for the per-zoom
    /// min-feature-size selection (`serve --mvt-min-feature-px`). Cheap — no projection.
    pub fn area(&self) -> f64 {
        fn ring_area(r: &[[f64; 2]]) -> f64 {
            if r.len() < 3 {
                return 0.0;
            }
            let mut s = 0.0;
            for i in 0..r.len() {
                let a = r[i];
                let b = r[(i + 1) % r.len()];
                s += a[0] * b[1] - b[0] * a[1];
            }
            (s * 0.5).abs()
        }
        fn poly_area(rings: &[Vec<[f64; 2]>]) -> f64 {
            let mut it = rings.iter();
            let ext = it.next().map_or(0.0, |r| ring_area(r));
            let holes: f64 = it.map(|r| ring_area(r)).sum();
            (ext - holes).max(0.0)
        }
        match self {
            Geometry::Point(_) | Geometry::LineString(_) | Geometry::MultiLineString(_) => 0.0,
            Geometry::Polygon(rings) => poly_area(rings),
            Geometry::MultiPolygon(polys) => polys.iter().map(|p| poly_area(p)).sum(),
        }
    }
}

/// One vector feature. `fid` is a **stable** identifier (from an attribute like `ne_id`), the
/// determinism tie-breaker for placement (spec §6.2) — never a volatile array index. `bbox` is the
/// geometry's source-CRS bounding box, precomputed once at load so the per-request bbox pre-filter
/// (WMS render + MVT tile) reads it instead of re-walking every vertex on every request.
#[derive(Clone, Debug)]
pub struct Feature {
    pub geom: Geometry,
    pub props: Props,
    pub fid: u64,
    pub bbox: [f64; 4],
    /// Planar area in source-CRS units² (0 for points/lines), precomputed once at load — the
    /// per-feature rank for the per-zoom min-feature-size selection (`--mvt-min-feature-px`), which
    /// makes overview-tile thinning seam-free (a per-feature, tile-independent keep/drop). See
    /// `Geometry::area`.
    pub area: f64,
}

impl Feature {
    /// Build a feature, precomputing its source-CRS `bbox`. A vertex-less geometry gets an inverted
    /// (empty) bbox `[+inf, +inf, -inf, -inf]` that overlaps nothing.
    pub fn new(geom: Geometry, props: Props, fid: u64) -> Feature {
        let bbox = geom.compute_bbox().unwrap_or([
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ]);
        let area = geom.area();
        Feature {
            geom,
            props,
            fid,
            bbox,
            area,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sq(cx: f64, cy: f64, h: f64) -> Vec<[f64; 2]> {
        // A closed axis-aligned square of side `2h` centred at (cx,cy); area (2h)².
        vec![
            [cx - h, cy - h],
            [cx + h, cy - h],
            [cx + h, cy + h],
            [cx - h, cy + h],
            [cx - h, cy - h],
        ]
    }

    #[test]
    fn area_of_unit_square_is_one() {
        // side 1 (h = 0.5) → area 1.0, regardless of vertex winding direction.
        let g = Geometry::Polygon(vec![sq(0.0, 0.0, 0.5)]);
        assert!((g.area() - 1.0).abs() < 1e-9, "got {}", g.area());
        // Reversed winding must give the SAME (absolute) area.
        let mut ring = sq(0.0, 0.0, 0.5);
        ring.reverse();
        let gr = Geometry::Polygon(vec![ring]);
        assert!(
            (gr.area() - 1.0).abs() < 1e-9,
            "reversed winding got {}",
            gr.area()
        );
    }

    #[test]
    fn area_subtracts_holes() {
        // 10×10 exterior (area 100) with a 4×4 hole (area 16) → net 84.
        let ext = sq(0.0, 0.0, 5.0);
        let hole = sq(0.0, 0.0, 2.0);
        let g = Geometry::Polygon(vec![ext, hole]);
        assert!((g.area() - 84.0).abs() < 1e-9, "got {}", g.area());
    }

    #[test]
    fn points_and_lines_have_zero_area() {
        assert_eq!(Geometry::Point([3.0, 4.0]).area(), 0.0);
        assert_eq!(Geometry::LineString(sq(0.0, 0.0, 1.0)).area(), 0.0);
        assert_eq!(
            Geometry::MultiLineString(vec![sq(0.0, 0.0, 1.0)]).area(),
            0.0
        );
    }

    #[test]
    fn area_same_for_closed_and_unclosed_rings() {
        // The shoelace `% r.len()` wrap closes the ring implicitly, so an OPEN ring (no repeated
        // first vertex) gives the same area as the CLOSED form — neither under- nor over-counts.
        let closed = Geometry::Polygon(vec![sq(0.0, 0.0, 1.0)]); // side 2 → area 4, has repeat last pt
        let open = Geometry::Polygon(vec![vec![
            [-1.0, -1.0],
            [1.0, -1.0],
            [1.0, 1.0],
            [-1.0, 1.0],
        ]]); // same square, no closing vertex
        assert!(
            (closed.area() - 4.0).abs() < 1e-9,
            "closed {}",
            closed.area()
        );
        assert!((open.area() - 4.0).abs() < 1e-9, "open {}", open.area());
    }

    #[test]
    fn multipolygon_sums_its_parts() {
        // Two disjoint unit squares → total area 2.0.
        let g = Geometry::MultiPolygon(vec![vec![sq(0.0, 0.0, 0.5)], vec![sq(10.0, 10.0, 0.5)]]);
        assert!((g.area() - 2.0).abs() < 1e-9, "got {}", g.area());
    }

    #[test]
    fn feature_new_precomputes_area() {
        let f = Feature::new(Geometry::Polygon(vec![sq(0.0, 0.0, 1.5)]), Props::new(), 7);
        assert!((f.area - 9.0).abs() < 1e-9, "got {}", f.area); // side 3 → area 9
    }
}
