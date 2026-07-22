// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! SP2a — turn a built topology into a *simplified*, seam-free coverage that the existing render
//! stack can serve. `simplify_topology` runs the Weighted-Visvalingam kernel over the shared arc pool
//! ONCE per arc (junction endpoints pinned → seam-free by construction); `materialize` reconstructs
//! world-coordinate `Feature`s from the simplified pool; `TopologyFeatureSource` wraps them as a
//! `FeatureSource` so WMS/MVT/X-ray/GFI render them unchanged. See
//! `docs/superpowers/specs/2026-07-14-topology-simplify-serve-design.md`.

use super::{rebuild_ring_from, ArcLine, GridPt, Topology};
use crate::vector::feature::{Feature, Geometry};
use crate::vector::mvt::simplify::simplify_visvalingam;
use crate::vector::source::FeatureSource;

/// Simplify every arc in the pool once (junction endpoints pinned → seam-free by construction).
/// Returns a new pool with the SAME indices, so existing `ArcRef`s stay valid.
///
/// Each arc is passed through `simplify_arc_guarded`, so a Weighted-Visvalingam result that turned
/// self-crossing is rejected — the materialised coverage stays valid even on messy input + aggressive
/// tolerances (SP2a task 6, reject-on-violation).
pub fn simplify_topology(topo: &Topology, min_area: f64) -> Vec<ArcLine> {
    // Each arc simplifies independently (pure function of the arc + min_area, no shared state, no
    // libproj) so this is embarrassingly parallel; par_iter().collect() preserves arc order, keeping
    // every `ArcRef` index valid. This spreads the dominant startup cost (Visvalingam + the O(m²)
    // self-intersection guard on the longest arcs) across all cores.
    use rayon::prelude::*;
    topo.arcs
        .par_iter()
        .map(|arc| simplify_arc_guarded(arc, min_area))
        .collect()
}

/// Simplify one arc, but REJECT the result when it self-intersects. Weighted-Visvalingam does not
/// preserve simplicity (removing a low-area vertex can jump a segment across a distant part of the
/// arc); on a valid input coverage the *original* arc is simple, so falling back to it keeps every
/// materialised ring valid — without a global spatial index. (Neighbour gaps/overlaps can't appear:
/// each shared arc is simplified ONCE and both features reference the identical result; the only
/// residual hazard a per-arc guard must catch is an arc crossing *itself*.)
fn simplify_arc_guarded(arc: &[GridPt], min_area: f64) -> ArcLine {
    let simplified = simplify_visvalingam(arc, min_area);
    if arc_self_intersects(&simplified) {
        arc.to_vec()
    } else {
        simplified
    }
}

/// Exact orientation of `a→b` vs `a→c` (i128 so world-scale i64 coords never overflow). `> 0` left
/// turn, `< 0` right, `0` collinear.
fn orient(a: GridPt, b: GridPt, c: GridPt) -> i128 {
    (b[0] - a[0]) as i128 * (c[1] - a[1]) as i128 - (b[1] - a[1]) as i128 * (c[0] - a[0]) as i128
}

/// Do open segments `p1-p2` and `p3-p4` *properly* cross (interiors intersect at a single point)?
/// Shared endpoints and collinear touching return `false` — adjacent arc segments legitimately share
/// a vertex, and a bare touch is not a topology violation.
fn segments_properly_cross(p1: GridPt, p2: GridPt, p3: GridPt, p4: GridPt) -> bool {
    let d1 = orient(p3, p4, p1);
    let d2 = orient(p3, p4, p2);
    let d3 = orient(p1, p2, p3);
    let d4 = orient(p1, p2, p4);
    (d1 > 0) != (d2 > 0) && (d3 > 0) != (d4 > 0) && d1 != 0 && d2 != 0 && d3 != 0 && d4 != 0
}

/// Does the polyline `arc` self-intersect (any two non-adjacent segments properly cross)? A per-pair
/// bounding-box reject keeps this near-linear on the monotone-ish arcs simplification produces; the
/// worst case is O(m²) but only on the SHORT arcs that heavy simplification yields (where self-
/// crossings actually arise). Adjacent segments (shared vertex) — including the closing pair of a
/// closed ring — are never a self-intersection.
fn arc_self_intersects(arc: &[GridPt]) -> bool {
    let n = arc.len();
    if n < 4 {
        return false;
    }
    let closed = arc[0] == arc[n - 1];
    let seg_bbox = |i: usize| -> (i64, i64, i64, i64) {
        let (a, b) = (arc[i], arc[i + 1]);
        (
            a[0].min(b[0]),
            a[1].min(b[1]),
            a[0].max(b[0]),
            a[1].max(b[1]),
        )
    };
    for i in 0..n - 1 {
        let (ax0, ay0, ax1, ay1) = seg_bbox(i);
        for j in (i + 2)..n - 1 {
            if closed && i == 0 && j == n - 2 {
                continue; // closing segment shares S0's start vertex → adjacent, not a crossing
            }
            let (bx0, by0, bx1, by1) = seg_bbox(j);
            if ax1 < bx0 || bx1 < ax0 || ay1 < by0 || by1 < ay0 {
                continue; // disjoint bounding boxes → cannot cross
            }
            if segments_properly_cross(arc[i], arc[i + 1], arc[j], arc[j + 1]) {
                return true;
            }
        }
    }
    false
}

/// Grid point → world coordinate.
fn to_world(p: GridPt, tol: f64) -> [f64; 2] {
    [p[0] as f64 * tol, p[1] as f64 * tol]
}

/// `materialize` with no part-culling (SP2a behaviour).
pub fn materialize(topo: &Topology, pool: &[ArcLine], snap_tol: f64) -> Vec<Feature> {
    materialize_culled(topo, pool, snap_tol, 0.0)
}

/// Absolute area of a closed world ring (shoelace / 2).
fn ring_world_area(ring: &[[f64; 2]]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut a = 0.0;
    for i in 0..n {
        let p = ring[i];
        let q = ring[(i + 1) % n];
        a += p[0] * q[1] - q[0] * p[1];
    }
    a.abs() * 0.5
}

/// Rebuild every feature from the (simplified) `pool` as world-coordinate `Feature`s. Rings are
/// closed (OGC convention). A ring with < 3 distinct grid points is dropped; if an exterior drops,
/// its polygon drops. `min_part_area` (source-CRS units²; 0 = off) additionally drops any polygon
/// whose exterior world area is below it — the per-zoom sub-pixel part cull (LOD).
pub fn materialize_culled(
    topo: &Topology,
    pool: &[ArcLine],
    snap_tol: f64,
    min_part_area: f64,
) -> Vec<Feature> {
    let mut out = Vec::with_capacity(topo.features.len());
    for (fi, ft) in topo.features.iter().enumerate() {
        let mut polys: Vec<Vec<Vec<[f64; 2]>>> = Vec::with_capacity(ft.polys.len());
        for poly in &ft.polys {
            let mut rings: Vec<Vec<[f64; 2]>> = Vec::with_capacity(poly.rings.len());
            for (ri, refs) in poly.rings.iter().enumerate() {
                let grid = rebuild_ring_from(refs, pool); // OPEN grid ring
                if grid.len() < 3 {
                    if ri == 0 {
                        rings.clear();
                        break; // exterior gone → drop polygon
                    }
                    continue; // drop a degenerate hole
                }
                let mut ring: Vec<[f64; 2]> = grid.iter().map(|&g| to_world(g, snap_tol)).collect();
                ring.push(ring[0]); // close
                rings.push(ring);
            }
            // Sub-pixel cull: drop the whole polygon when its exterior is below the threshold.
            if !rings.is_empty()
                && !(min_part_area > 0.0 && ring_world_area(&rings[0]) < min_part_area)
            {
                polys.push(rings);
            }
        }
        if polys.is_empty() {
            // Every part was culled (or degenerate) → drop the feature entirely rather than carry a
            // zero-area ghost. At coarse LOD zooms the part-cull removes most of a national coverage,
            // so this keeps each pool (and the per-tile candidate scan) proportional to the survivors,
            // not to the full ~840k feature count. fid is `fi` (stable, unique) so GFI is unaffected.
            continue;
        }
        let geom = if polys.len() == 1 {
            Geometry::Polygon(polys.pop().unwrap())
        } else {
            Geometry::MultiPolygon(polys)
        };
        out.push(Feature::new(geom, ft.props.clone(), fi as u64));
    }
    out
}

/// A `FeatureSource` backed by an in-memory, already-materialised coverage — lets the whole existing
/// render stack (WMS/MVT/X-ray/GFI) serve a topology-simplified layer unchanged.
pub struct TopologyFeatureSource {
    features: Vec<Feature>,
    extent: [f64; 4],
}

impl TopologyFeatureSource {
    pub fn new(features: Vec<Feature>) -> Self {
        // union of feature bboxes ([W,S,E,N]); empty → a degenerate zero extent.
        let mut e = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        for f in &features {
            e[0] = e[0].min(f.bbox[0]);
            e[1] = e[1].min(f.bbox[1]);
            e[2] = e[2].max(f.bbox[2]);
            e[3] = e[3].max(f.bbox[3]);
        }
        if !e[0].is_finite() {
            e = [0.0, 0.0, 0.0, 0.0];
        }
        TopologyFeatureSource {
            features,
            extent: e,
        }
    }
}

impl FeatureSource for TopologyFeatureSource {
    fn features(&self) -> &[Feature] {
        &self.features
    }
    fn full_extent(&self) -> [f64; 4] {
        self.extent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::feature::{Props, Value};
    use crate::vector::topology::build_topology;

    fn sq(ox: f64, oy: f64, s: f64) -> Vec<[f64; 2]> {
        vec![
            [ox, oy],
            [ox + s, oy],
            [ox + s, oy + s],
            [ox, oy + s],
            [ox, oy],
        ]
    }

    #[test]
    fn materialize_culls_subpixel_polygon() {
        // MultiPolygon: big square (area 100) + tiny square (area 1). Cull at 10 → tiny dropped.
        let f = Feature::new(
            Geometry::MultiPolygon(vec![vec![sq(0.0, 0.0, 10.0)], vec![sq(100.0, 100.0, 1.0)]]),
            Props::new(),
            0,
        );
        let (topo, _) = build_topology(std::slice::from_ref(&f), 1.0);
        let n_polys = |f: &Feature| match &f.geom {
            Geometry::Polygon(_) => 1,
            Geometry::MultiPolygon(p) => p.len(),
            _ => 0,
        };
        assert_eq!(
            n_polys(&materialize_culled(&topo, &topo.arcs, 1.0, 0.0)[0]),
            2
        );
        assert_eq!(
            n_polys(&materialize_culled(&topo, &topo.arcs, 1.0, 10.0)[0]),
            1
        );
    }

    // a square with a redundant collinear midpoint on the bottom edge (a vertex simplify can drop)
    fn sq_midpt(ox: f64, oy: f64, s: f64) -> Vec<[f64; 2]> {
        vec![
            [ox, oy],
            [ox + s / 2.0, oy],
            [ox + s, oy],
            [ox + s, oy + s],
            [ox, oy + s],
            [ox, oy],
        ]
    }

    #[test]
    fn min_area_zero_is_identity() {
        let f = Feature::new(
            Geometry::Polygon(vec![sq_midpt(0.0, 0.0, 10.0)]),
            Props::new(),
            0,
        );
        let (topo, _) = build_topology(std::slice::from_ref(&f), 1.0);
        assert_eq!(simplify_topology(&topo, 0.0), topo.arcs); // byte-identical pool
    }

    #[test]
    fn simplify_reduces_total_vertices() {
        let f = Feature::new(
            Geometry::Polygon(vec![sq_midpt(0.0, 0.0, 10.0)]),
            Props::new(),
            0,
        );
        let (topo, _) = build_topology(std::slice::from_ref(&f), 1.0);
        let before: usize = topo.arcs.iter().map(|a| a.len()).sum();
        let after: usize = simplify_topology(&topo, 5.0).iter().map(|a| a.len()).sum();
        assert!(
            after < before,
            "midpoint should be dropped: {after} !< {before}"
        );
    }

    #[test]
    fn materialize_preserves_props_and_closes_rings() {
        let mut p = Props::new();
        p.insert("class".into(), Value::Str("water".into()));
        let f = Feature::new(Geometry::Polygon(vec![sq(0.0, 0.0, 10.0)]), p, 0);
        let (topo, _) = build_topology(std::slice::from_ref(&f), 1.0);
        let out = materialize(&topo, &topo.arcs, 1.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].props.get_str("class"), Some("water"));
        if let Geometry::Polygon(rings) = &out[0].geom {
            assert_eq!(rings[0].first(), rings[0].last()); // closed ring
        } else {
            panic!("expected Polygon");
        }
    }

    // THE seam-free proof: two adjacent squares share the border x=10 (with a mid-border vertex at
    // [10,5] present in BOTH). Simplify hard enough to drop [10,5]; the shared border must be
    // identical from both features — no gap.
    #[test]
    fn shared_border_is_seam_free_after_simplify() {
        let left = vec![
            [0.0, 0.0],
            [10.0, 0.0],
            [10.0, 5.0],
            [10.0, 10.0],
            [0.0, 10.0],
            [0.0, 0.0],
        ];
        let right = vec![
            [10.0, 0.0],
            [20.0, 0.0],
            [20.0, 10.0],
            [10.0, 10.0],
            [10.0, 5.0],
            [10.0, 0.0],
        ];
        let fs = [
            Feature::new(Geometry::Polygon(vec![left]), Props::new(), 0),
            Feature::new(Geometry::Polygon(vec![right]), Props::new(), 1),
        ];
        let (topo, _) = build_topology(&fs, 1.0);
        // min_area=10: drops [10,5] (collinear → metric 0) but keeps the 90° corners (metric 50).
        let pool = simplify_topology(&topo, 10.0);
        let out = materialize(&topo, &pool, 1.0);
        let on_border = |f: &Feature| -> Vec<[i64; 2]> {
            let Geometry::Polygon(rings) = &f.geom else {
                return vec![];
            };
            rings[0]
                .iter()
                .filter(|p| p[0] == 10.0)
                .map(|p| [p[0] as i64, p[1] as i64])
                .collect()
        };
        let mut a = on_border(&out[0]);
        let mut b = on_border(&out[1]);
        a.sort();
        b.sort();
        assert_eq!(
            a, b,
            "shared border must be identical from both sides (seam-free)"
        );
        assert!(
            !a.iter().any(|p| p[1] == 5),
            "the flat mid-border vertex should have been simplified away"
        );
    }

    #[test]
    fn topology_feature_source_exposes_features_and_extent() {
        let f = Feature::new(Geometry::Polygon(vec![sq(2.0, 3.0, 4.0)]), Props::new(), 0);
        let (topo, _) = build_topology(std::slice::from_ref(&f), 1.0);
        let feats = materialize(&topo, &topo.arcs, 1.0);
        let src = TopologyFeatureSource::new(feats);
        assert_eq!(src.features().len(), 1);
        let e = src.full_extent(); // [W,S,E,N] covering the 2,3..6,7 square
        assert!(e[0] <= 2.0 && e[1] <= 3.0 && e[2] >= 6.0 && e[3] >= 7.0);
    }

    #[test]
    fn detects_figure_eight_self_intersection() {
        // bowtie: the two diagonals of a square cross at its centre.
        let bowtie = vec![[0, 0], [10, 10], [10, 0], [0, 10], [0, 0]];
        assert!(arc_self_intersects(&bowtie));
    }

    #[test]
    fn simple_open_and_closed_arcs_are_not_self_intersecting() {
        let open_l = vec![[0, 0], [10, 0], [10, 10], [0, 10]];
        assert!(!arc_self_intersects(&open_l));
        // a closed square: the closing vertex shares S0's start — must NOT be read as a crossing.
        let closed_square = vec![[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]];
        assert!(!arc_self_intersects(&closed_square));
    }

    #[test]
    fn guard_falls_back_when_simplification_self_intersects() {
        // A valid (simple) input arc that raw Weighted-Visvalingam simplifies into a self-crossing
        // 4-point line at this tolerance (found by brute force — see git history).
        let arc: ArcLine = vec![
            [995, 890],
            [482, 318],
            [773, 400],
            [53, 61],
            [781, 915],
            [701, 608],
        ];
        assert!(!arc_self_intersects(&arc), "precondition: input is simple");
        let raw = simplify_visvalingam(&arc, 100_000.0);
        assert!(
            arc_self_intersects(&raw),
            "precondition: raw VW self-crosses at this tolerance"
        );
        // The guard rejects the self-crossing simplification and keeps the original simple arc.
        let guarded = simplify_arc_guarded(&arc, 100_000.0);
        assert_eq!(guarded, arc);
        assert!(!arc_self_intersects(&guarded));
    }

    #[test]
    fn guard_keeps_simplification_when_valid() {
        // a low-amplitude zigzag simplifies to a near-straight line — no self-crossing → kept.
        let arc: ArcLine = vec![[0, 0], [100, 10], [200, 0], [300, 10], [400, 0]];
        let g = simplify_arc_guarded(&arc, 2_000.0);
        assert!(g.len() < arc.len(), "valid simplification should be kept");
        assert!(!arc_self_intersects(&g));
    }
}
