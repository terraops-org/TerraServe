// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

use super::{GridPoly, GridRing};
use crate::vector::feature::{Feature, Geometry, Props};

#[derive(Default, Debug, Clone)]
pub struct SnapStats {
    pub vertices_in: usize,
    pub vertices_after: usize,
    pub rings_in: usize,
    pub degenerate_rings_dropped: usize,
    pub nonfinite_dropped: usize,
}

#[derive(Debug, Clone)]
pub struct SnappedFeature {
    pub polys: Vec<GridPoly>,
    pub props: Props,
}

/// Quantize `p` to the world grid; `None` if non-finite.
fn snap_pt(p: [f64; 2], tol: f64) -> Option<[i64; 2]> {
    if !p[0].is_finite() || !p[1].is_finite() {
        return None;
    }
    Some([(p[0] / tol).round() as i64, (p[1] / tol).round() as i64])
}

/// Snap one ring to an OPEN grid ring (consecutive dups + closing dup removed); `None` if it
/// collapses to < 3 distinct points. Accumulates stats.
fn snap_ring(ring: &[[f64; 2]], tol: f64, st: &mut SnapStats) -> Option<GridRing> {
    st.rings_in += 1;
    st.vertices_in += ring.len();
    let mut out: GridRing = Vec::with_capacity(ring.len());
    for &p in ring {
        match snap_pt(p, tol) {
            None => st.nonfinite_dropped += 1,
            Some(g) => {
                if out.last() != Some(&g) {
                    out.push(g);
                }
            }
        }
    }
    if out.len() >= 2 && out.first() == out.last() {
        out.pop(); // drop closing dup → open ring
    }
    if out.len() < 3 {
        st.degenerate_rings_dropped += 1;
        return None;
    }
    st.vertices_after += out.len();
    Some(out)
}

/// Snap one polygon; empty `GridPoly` (dropped) if the exterior is degenerate.
fn snap_poly(rings: &[Vec<[f64; 2]>], tol: f64, st: &mut SnapStats) -> GridPoly {
    let mut it = rings.iter();
    let ext = match it.next() {
        Some(r) => match snap_ring(r, tol, st) {
            Some(e) => e,
            None => {
                // exterior gone: any holes are also moot; count them as dropped for honesty
                for h in it {
                    st.rings_in += 1;
                    st.vertices_in += h.len();
                    st.degenerate_rings_dropped += 1;
                }
                return Vec::new();
            }
        },
        None => return Vec::new(),
    };
    let mut poly = vec![ext];
    for h in it {
        if let Some(hole) = snap_ring(h, tol, st) {
            poly.push(hole);
        }
    }
    poly
}

fn snap_geom(g: &Geometry, tol: f64, st: &mut SnapStats) -> Vec<GridPoly> {
    match g {
        Geometry::Polygon(rings) => {
            let p = snap_poly(rings, tol, st);
            if p.is_empty() {
                Vec::new()
            } else {
                vec![p]
            }
        }
        Geometry::MultiPolygon(polys) => polys
            .iter()
            .map(|r| snap_poly(r, tol, st))
            .filter(|p| !p.is_empty())
            .collect(),
        _ => Vec::new(), // Point/LineString/MultiLineString → no coverage geometry
    }
}

pub fn snap_coverage(features: &[Feature], tol: f64) -> (Vec<SnappedFeature>, SnapStats) {
    let mut st = SnapStats::default();
    let out = features
        .iter()
        .map(|f| SnappedFeature {
            polys: snap_geom(&f.geom, tol, &mut st),
            props: f.props.clone(),
        })
        .collect();
    (out, st)
}

/// The oracle's expected side: snapped geometry only, one entry per feature.
pub fn snap_only(features: &[Feature], tol: f64) -> Vec<Vec<GridPoly>> {
    snap_coverage(features, tol)
        .0
        .into_iter()
        .map(|sf| sf.polys)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::feature::{Feature, Geometry, Props};

    fn poly(rings: Vec<Vec<[f64; 2]>>) -> Feature {
        Feature::new(Geometry::Polygon(rings), Props::new(), 0)
    }
    // a closed square ring (first == last), side `s` at origin `o`
    fn sq(o: f64, s: f64) -> Vec<[f64; 2]> {
        vec![[o, o], [o + s, o], [o + s, o + s], [o, o + s], [o, o]]
    }

    #[test]
    fn snaps_and_drops_closing_dup_and_near_coincident() {
        // tol = 1.0: two vertices 0.3 apart collapse to the same grid point.
        let f = poly(vec![vec![
            [0.0, 0.0],
            [0.3, 0.0],
            [10.0, 0.0],
            [10.0, 10.0],
            [0.0, 10.0],
            [0.0, 0.0],
        ]]);
        let (snapped, stats) = snap_coverage(&[f], 1.0);
        let ring = &snapped[0].polys[0][0];
        // open ring, closing dup removed, [0,0] and [0.3->0] collapsed to one [0,0]
        assert_eq!(ring, &vec![[0, 0], [10, 0], [10, 10], [0, 10]]);
        assert_eq!(stats.degenerate_rings_dropped, 0);
    }

    #[test]
    fn drops_degenerate_ring_after_snap() {
        // a sliver that collapses to < 3 distinct grid points at tol=10
        let f = poly(vec![vec![[0.0, 0.0], [1.0, 0.0], [2.0, 0.0], [0.0, 0.0]]]);
        let (snapped, stats) = snap_coverage(&[f], 10.0);
        assert!(snapped[0].polys.is_empty()); // exterior degenerate → poly dropped
        assert_eq!(stats.degenerate_rings_dropped, 1);
    }

    #[test]
    fn nonfinite_vertex_is_dropped_not_panicked() {
        let f = poly(vec![vec![
            [0.0, 0.0],
            [f64::NAN, 5.0],
            [10.0, 0.0],
            [10.0, 10.0],
            [0.0, 10.0],
            [0.0, 0.0],
        ]]);
        let (snapped, stats) = snap_coverage(&[f], 1.0);
        assert!(stats.nonfinite_dropped >= 1);
        assert!(!snapped[0].polys.is_empty()); // still a valid square
    }

    #[test]
    fn multipolygon_and_nonpolygon_handled() {
        let mp = Feature::new(
            Geometry::MultiPolygon(vec![vec![sq(0.0, 4.0)], vec![sq(100.0, 4.0)]]),
            Props::new(),
            1,
        );
        let pt = Feature::new(Geometry::Point([1.0, 1.0]), Props::new(), 2);
        let (snapped, _) = snap_coverage(&[mp, pt], 1.0);
        assert_eq!(snapped[0].polys.len(), 2); // two polygons
        assert!(snapped[1].polys.is_empty()); // point → no coverage geometry
    }

    #[test]
    fn snap_only_matches_snap_coverage_geometry() {
        let f = poly(vec![sq(0.0, 8.0)]);
        let (cov, _) = snap_coverage(&[f.clone()], 1.0);
        assert_eq!(snap_only(&[f], 1.0), vec![cov[0].polys.clone()]);
    }
}
