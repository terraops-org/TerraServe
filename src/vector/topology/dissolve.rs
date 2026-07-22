// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! SP2b — global same-class dissolve. Reuse the `mvt::dissolve` edge-cancellation kernels on an i32
//! WORLD-snap grid (not a tile grid) to merge each class's adjacent features into one region, so the
//! SP2a topology-simplify/materialize/serve path then treats class BOUNDARIES as the shared arcs.
//!
//! i32 grid (not the topology's i64): at a 1 cm snap i32 spans a ~21,470 km extent, so every realistic
//! *projected* coverage (COS EPSG:3763, BUPi) fits with huge margin — and it lets us reuse the tested
//! i32 kernels verbatim (no invasive retype of the i32-tile-coupled on-the-fly path). The dissolve
//! grid is independent of the topology's i64 snap grid — `build_topology` re-snaps the output.

use crate::vector::feature::{Feature, Geometry, Props, Value};
use crate::vector::mvt::dissolve::{
    cancel_edges, group_polygons, normalise_ring, walk_rings, Ring,
};
use std::collections::BTreeMap;

/// i32 grid clamp bound (keeps a pathological extent from wrapping; never bites realistic data).
const GRID_LIM: f64 = i32::MAX as f64;

/// Snap + winding-normalise one polygon's rings to the i32 world grid; `None` if the exterior is
/// degenerate. Exterior forced positive-area, holes negative — so adjacent same-class shared edges
/// cancel and `group_polygons` can nest holes.
fn snap_normalise_poly(rings: &[Vec<[f64; 2]>], snap: f64) -> Option<Vec<Ring>> {
    let mut out: Vec<Ring> = Vec::new();
    for (ri, ring) in rings.iter().enumerate() {
        let mut g: Ring = Vec::with_capacity(ring.len());
        for &p in ring {
            if !p[0].is_finite() || !p[1].is_finite() {
                continue;
            }
            let x = (p[0] / snap).round().clamp(-GRID_LIM, GRID_LIM) as i32;
            let y = (p[1] / snap).round().clamp(-GRID_LIM, GRID_LIM) as i32;
            let v = [x, y];
            if g.last() != Some(&v) {
                g.push(v);
            }
        }
        if g.len() >= 2 && g.first() == g.last() {
            g.pop();
        }
        if g.len() < 3 {
            if ri == 0 {
                return None; // exterior gone → drop polygon
            }
            continue; // drop degenerate hole
        }
        out.push(normalise_ring(g, ri == 0));
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Deterministic per-class bucket key.
fn class_key(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("s:{s}"),
        Value::Num(n) => format!("n:{}", n.to_bits()),
        Value::Null => "null".into(),
    }
}

/// Roll a hierarchical dotted class code up to its first `n` levels: `"1.1.2.1"` @ 1 → `"1"`, @ 2 →
/// `"1.1"` — merging sub-classes into their megaclass. `None`/non-string values are returned as-is.
fn rollup_class(v: &Value, rollup: Option<usize>) -> Value {
    match (rollup, v) {
        (Some(n), Value::Str(s)) if n > 0 => {
            Value::Str(s.split('.').take(n).collect::<Vec<_>>().join("."))
        }
        _ => v.clone(),
    }
}

/// Merge same-class neighbours into one MultiPolygon `Feature` per distinct `field` value (internal
/// same-class edges cancelled → true class boundaries). `rollup` (e.g. `Some(1)`) merges at a coarser
/// hierarchy level (megaclass). Features with a missing/null `field` value, or non-polygon geometry,
/// pass through unchanged.
pub fn dissolve_coverage(
    features: &[Feature],
    field: &str,
    snap: f64,
    rollup: Option<usize>,
) -> Vec<Feature> {
    let mut buckets: BTreeMap<String, (Value, Vec<Ring>)> = BTreeMap::new();
    let mut passthrough: Vec<Feature> = Vec::new();
    for f in features {
        let cls = match f.props.get(field) {
            Some(v) if !matches!(v, Value::Null) => rollup_class(v, rollup),
            _ => {
                passthrough.push(f.clone());
                continue;
            }
        };
        let mut rings: Vec<Ring> = Vec::new();
        match &f.geom {
            Geometry::Polygon(r) => {
                if let Some(rr) = snap_normalise_poly(r, snap) {
                    rings.extend(rr);
                }
            }
            Geometry::MultiPolygon(parts) => {
                for part in parts {
                    if let Some(rr) = snap_normalise_poly(part, snap) {
                        rings.extend(rr);
                    }
                }
            }
            _ => {
                passthrough.push(f.clone()); // non-polygon → pass through
                continue;
            }
        }
        if rings.is_empty() {
            continue;
        }
        buckets
            .entry(class_key(&cls))
            .or_insert_with(|| (cls, Vec::new()))
            .1
            .extend(rings);
    }

    let mut out: Vec<Feature> = Vec::with_capacity(buckets.len() + passthrough.len());
    let mut fid: u64 = 0;
    for (_k, (value, rings)) in buckets {
        let groups = group_polygons(walk_rings(&cancel_edges(&rings)));
        if groups.is_empty() {
            continue;
        }
        let polys: Vec<Vec<Vec<[f64; 2]>>> = groups
            .into_iter()
            .map(|g| {
                g.into_iter()
                    .map(|ring| {
                        let mut r: Vec<[f64; 2]> = ring
                            .iter()
                            .map(|&v| [v[0] as f64 * snap, v[1] as f64 * snap])
                            .collect();
                        if let Some(&first) = r.first() {
                            r.push(first); // close (OGC)
                        }
                        r
                    })
                    .collect()
            })
            .collect();
        let mut props = Props::new();
        props.insert(field.to_string(), value);
        out.push(Feature::new(Geometry::MultiPolygon(polys), props, fid));
        fid += 1;
    }
    // Continue the fid counter through pass-through features so no two features in the layer share an
    // fid (which would conflate them in MVT feature-ids / GetFeatureInfo).
    for pf in passthrough {
        out.push(Feature::new(pf.geom, pf.props, fid));
        fid += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sq(ox: f64, oy: f64, s: f64) -> Vec<[f64; 2]> {
        vec![
            [ox, oy],
            [ox + s, oy],
            [ox + s, oy + s],
            [ox, oy + s],
            [ox, oy],
        ]
    }
    fn feat(rings: Vec<Vec<[f64; 2]>>, class: &str, fid: u64) -> Feature {
        let mut p = Props::new();
        p.insert("c".into(), Value::Str(class.into()));
        Feature::new(Geometry::Polygon(rings), p, fid)
    }

    #[test]
    fn rollup_merges_subclasses_into_megaclass() {
        // adjacent squares, classes "1.1" and "1.2" → rollup level 1 ("1") → one merged region.
        let fs = [
            feat(vec![sq(0.0, 0.0, 10.0)], "1.1", 0),
            feat(vec![sq(10.0, 0.0, 10.0)], "1.2", 1),
        ];
        assert_eq!(dissolve_coverage(&fs, "c", 1.0, None).len(), 2); // full level → 2 classes
        let rolled = dissolve_coverage(&fs, "c", 1.0, Some(1)); // megaclass → 1
        assert_eq!(rolled.len(), 1);
        assert_eq!(rolled[0].props.get_str("c"), Some("1"));
    }

    #[test]
    fn same_class_adjacent_squares_merge_to_one() {
        // two squares sharing the edge x=10, same class → ONE feature, area = both squares.
        let fs = [
            feat(vec![sq(0.0, 0.0, 10.0)], "A", 0),
            feat(vec![sq(10.0, 0.0, 10.0)], "A", 1),
        ];
        let out = dissolve_coverage(&fs, "c", 1.0, None);
        assert_eq!(out.len(), 1);
        assert!(
            (out[0].geom.area() - 200.0).abs() < 1.0,
            "merged area ~200, got {}",
            out[0].geom.area()
        );
    }

    #[test]
    fn different_class_squares_stay_separate() {
        let fs = [
            feat(vec![sq(0.0, 0.0, 10.0)], "A", 0),
            feat(vec![sq(10.0, 0.0, 10.0)], "B", 1),
        ];
        let out = dissolve_coverage(&fs, "c", 1.0, None);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn class_enclosing_another_keeps_a_hole() {
        // class A = big square with a square hole; class B fills the hole → A keeps its hole, B separate.
        let hole = vec![
            [10.0, 10.0],
            [20.0, 10.0],
            [20.0, 20.0],
            [10.0, 20.0],
            [10.0, 10.0],
        ];
        let a = feat(vec![sq(0.0, 0.0, 30.0), hole.clone()], "A", 0);
        let b = feat(vec![hole], "B", 1);
        let out = dissolve_coverage(&[a, b], "c", 1.0, None);
        assert_eq!(out.len(), 2);
        let a_out = out
            .iter()
            .find(|f| f.props.get_str("c") == Some("A"))
            .unwrap();
        if let Geometry::MultiPolygon(polys) = &a_out.geom {
            assert!(
                polys.iter().any(|p| p.len() > 1),
                "A should retain a hole ring"
            );
        } else {
            panic!("expected MultiPolygon");
        }
    }

    #[test]
    fn null_class_feature_passes_through() {
        let a = feat(vec![sq(0.0, 0.0, 10.0)], "A", 0);
        let plain = Feature::new(
            Geometry::Polygon(vec![sq(100.0, 100.0, 5.0)]),
            Props::new(),
            9,
        );
        let out = dissolve_coverage(&[a, plain], "c", 1.0, None);
        assert_eq!(out.len(), 2); // class A region + the pass-through
    }

    #[test]
    fn deterministic() {
        let fs = [
            feat(vec![sq(0.0, 0.0, 10.0)], "A", 0),
            feat(vec![sq(10.0, 0.0, 10.0)], "A", 1),
        ];
        let a = dissolve_coverage(&fs, "c", 1.0, None);
        let b = dissolve_coverage(&fs, "c", 1.0, None);
        assert_eq!(a.len(), b.len());
        assert_eq!(a[0].geom.area().to_bits(), b[0].geom.area().to_bits());
    }
}
