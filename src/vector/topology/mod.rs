// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Shared-arc topology for a polygon coverage (TopoJSON Extract→Join→Cut→Dedup), bespoke, no
//! geometry crate. SP1: build + lossless (up to ring rotation) reconstruct + a diagnostic report.
//! Feeds SP2's per-arc Weighted-Visvalingam simplification. See
//! `docs/superpowers/specs/2026-07-14-arc-topology-core-design.md`.
//!
//! SP1 is built up across tasks; wired by Task 6.
#![allow(dead_code)]

pub mod cut;
pub mod dedup;
pub mod dissolve;
pub mod join;
pub mod lod;
pub mod materialize;
pub mod snap;

pub type GridPt = [i64; 2];
pub type GridRing = Vec<GridPt>; // OPEN — no repeated closing vertex
pub type GridPoly = Vec<GridRing>; // [0] = exterior, [1..] = holes
pub type ArcLine = Vec<GridPt>;
pub type ArcRef = i32; // a >= 0 → arc a forward; a < 0 → arc !a reversed (!a == -a-1)

pub use cut::cut_ring;
pub use dedup::canonicalize_arc;
pub use join::find_junctions;
pub use snap::{snap_coverage, snap_only, SnapStats, SnappedFeature};

use crate::vector::feature::Feature;
use crate::vector::feature::Props;
use std::collections::HashMap;

/// One polygon in a `FeatureTopo`: each ring is a sequence of `ArcRef`s that reconstruct it
/// end-to-end (arc[i]'s last vertex == arc[i+1]'s first vertex).
#[derive(Debug, Clone)]
pub struct Poly {
    pub rings: Vec<Vec<ArcRef>>,
}

/// One input feature's topology-side representation: its polygons (as arc refs) + its original
/// attributes, carried through unchanged.
#[derive(Debug, Clone)]
pub struct FeatureTopo {
    pub polys: Vec<Poly>,
    pub props: Props,
}

/// The whole shared-arc topology for a coverage: the deduplicated arc pool, every feature rewritten
/// in terms of arc refs, the snap tolerance used to build it, and each arc's reference count.
#[derive(Debug, Clone)]
pub struct Topology {
    pub arcs: Vec<ArcLine>,
    pub features: Vec<FeatureTopo>,
    pub tolerance: f64,
    pub degree: Vec<u32>,
}

/// Diagnostics from a `build_topology` run: arc/shared/boundary/junction counts, the snap-stat
/// counts, the total absolute area delta introduced by grid-snapping (Σ|snapped area − source
/// area| across features), and a capped warning log.
#[derive(Default, Debug, Clone)]
pub struct BuildReport {
    pub features_in: usize,
    pub rings_in: usize,
    pub arcs: usize,
    pub shared_arcs: usize,   // degree >= 2
    pub boundary_arcs: usize, // degree == 1
    pub junctions: usize,
    pub vertices_in: usize,
    pub vertices_after_snap: usize,
    pub degenerate_rings_dropped: usize,
    pub nonfinite_dropped: usize,
    pub total_abs_area_delta: f64,
    pub warnings: Vec<String>,
}

/// Intern `arc` into the shared pool, keyed by its canonical form, bumping its degree. `is_ring`
/// (from `cut_ring`) is threaded straight into `canonicalize_arc` — only a junctionless whole ring
/// may be rotated to its lex-min start; a junction-bounded arc (even a closed one) must keep its
/// junction anchor (Defect 2, `.superpowers/sdd/mismatch-diagnosis.md`). Returns the `ArcRef`
/// (signed: forward if the input matched the canonical direction, `!id` if reversed). Arc IDs are
/// assigned in first-encounter order — deterministic given input order.
fn intern(
    pool: &mut Vec<ArcLine>,
    index: &mut HashMap<ArcLine, usize>,
    degree: &mut Vec<u32>,
    arc: &ArcLine,
    is_ring: bool,
) -> ArcRef {
    let (canon, reversed) = canonicalize_arc(arc, is_ring);
    let id = *index.entry(canon.clone()).or_insert_with(|| {
        pool.push(canon);
        degree.push(0);
        pool.len() - 1
    });
    degree[id] += 1;
    if reversed {
        !(id as i32)
    } else {
        id as i32
    }
}

/// Build the shared-arc topology for a polygon coverage: Snap (Task 1) → Join (Task 2) → Cut →
/// Dedup. A border shared by two adjacent features is interned once and referenced by both (forward
/// by one, reversed by the other where they wind oppositely).
pub fn build_topology(features: &[Feature], tol: f64) -> (Topology, BuildReport) {
    let (snapped, sstats) = snap_coverage(features, tol);

    // flat ring list for junction detection
    let mut all_rings: Vec<GridRing> = Vec::new();
    for sf in &snapped {
        for poly in &sf.polys {
            for ring in poly {
                all_rings.push(ring.clone());
            }
        }
    }
    let junc = find_junctions(&all_rings);

    // intern arcs; key = canonical vertex sequence
    let mut pool: Vec<ArcLine> = Vec::new();
    let mut index: HashMap<ArcLine, usize> = HashMap::new();
    let mut degree: Vec<u32> = Vec::new();

    let mut feats = Vec::with_capacity(snapped.len());
    for sf in &snapped {
        let mut polys = Vec::with_capacity(sf.polys.len());
        for poly in &sf.polys {
            let mut rings = Vec::with_capacity(poly.len());
            for ring in poly {
                let refs: Vec<ArcRef> = cut_ring(ring, &junc)
                    .iter()
                    .map(|(a, is_ring)| intern(&mut pool, &mut index, &mut degree, a, *is_ring))
                    .collect();
                rings.push(refs);
            }
            polys.push(Poly { rings });
        }
        feats.push(FeatureTopo {
            polys,
            props: sf.props.clone(),
        });
    }

    let shared = degree.iter().filter(|&&d| d >= 2).count();
    let boundary = degree.iter().filter(|&&d| d == 1).count();
    let mut rep = BuildReport::default();
    rep.features_in = features.len();
    rep.rings_in = sstats.rings_in;
    rep.arcs = pool.len();
    rep.shared_arcs = shared;
    rep.boundary_arcs = boundary;
    rep.junctions = junc.len();
    rep.vertices_in = sstats.vertices_in;
    rep.vertices_after_snap = sstats.vertices_after;
    rep.degenerate_rings_dropped = sstats.degenerate_rings_dropped;
    rep.nonfinite_dropped = sstats.nonfinite_dropped;

    // area delta: snapped grid area (in grid units²) × tol² vs the input feature area
    let mut delta = 0.0f64;
    for (sf, feat) in snapped.iter().zip(features.iter()) {
        let mut grid_area = 0.0f64;
        for poly in &sf.polys {
            for (ri, ring) in poly.iter().enumerate() {
                let a = shoelace_abs(ring); // grid units²
                grid_area += if ri == 0 { a } else { -a }; // exterior − holes
            }
        }
        delta += (grid_area * tol * tol - feat.area).abs();
    }
    rep.total_abs_area_delta = delta;

    const WARN_CAP: usize = 50;
    // (optional heuristic warnings — a boundary arc interior to the bbox; keep bounded)
    if rep.warnings.len() > WARN_CAP {
        let extra = rep.warnings.len() - WARN_CAP;
        rep.warnings.truncate(WARN_CAP);
        rep.warnings.push(format!("… ({extra} more suppressed)"));
    }

    (
        Topology {
            arcs: pool,
            features: feats,
            tolerance: tol,
            degree,
        },
        rep,
    )
}

/// Shoelace area of a grid-space ring (OPEN — no repeated closing vertex), in grid units².
/// i128 accumulation avoids overflow on `i64` grid coordinates × `i64` grid coordinates. Rings
/// with fewer than 3 vertices (degenerate/garbage input) have zero area.
fn shoelace_abs(ring: &[GridPt]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut s: i128 = 0;
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        s += a[0] as i128 * b[1] as i128 - b[0] as i128 * a[1] as i128;
    }
    (s.abs() as f64) / 2.0
}

/// Lexicographically compare the rotation of `ring` starting at `i` against the one starting at
/// `j`, over the full length, without materializing either rotated array — O(n) per comparison.
fn rotation_cmp(ring: &[GridPt], i: usize, j: usize, n: usize) -> std::cmp::Ordering {
    for k in 0..n {
        let a = ring[(i + k) % n];
        let b = ring[(j + k) % n];
        match a.cmp(&b) {
            std::cmp::Ordering::Equal => continue,
            ord => return ord,
        }
    }
    std::cmp::Ordering::Equal
}

/// Rotate a ring (in place) to its TRUE minimal cyclic rotation — a rotation-invariant comparison
/// for round-trip checks, correct even when the lexicographically-smallest vertex appears more than
/// once (a duplicated extreme vertex — common in COS2023 land-cover pinch points, Defect 1 in
/// `.superpowers/sdd/mismatch-diagnosis.md`). Rotating to the FIRST array index achieving the
/// minimum (the old `min_by_key` rule) is NOT a cyclic invariant in that case: two arrays that store
/// the exact same cycle at different offsets can each have their duplicated min vertex's first
/// occurrence land at index 0 and still disagree after "normalizing". Instead: among every index
/// where the ring equals the minimum vertex (the true minimal rotation must start at one of them),
/// pick the one whose full rotated sequence compares lexicographically smallest — O(n·k), k = the
/// min vertex's occurrence count (k=1, i.e. O(n), for the overwhelmingly common case of a simple
/// ring with a unique minimum). Shared by the `recon_tests::norm` test helper and
/// `Topology::verify_roundtrip` so the rotation rule lives in exactly one place.
pub(crate) fn normalize_ring_rotation(ring: &mut GridRing) {
    let n = ring.len();
    if n == 0 {
        return;
    }
    let min_val = *ring.iter().min().unwrap();
    let mut best: Option<usize> = None;
    for i in 0..n {
        if ring[i] != min_val {
            continue;
        }
        best = Some(match best {
            None => i,
            Some(b) => {
                if rotation_cmp(ring, i, b, n) == std::cmp::Ordering::Less {
                    i
                } else {
                    b
                }
            }
        });
    }
    if let Some(s) = best {
        ring.rotate_left(s);
    }
}

#[cfg(test)]
mod normalize_tests {
    use super::*;

    #[test]
    fn normalize_ring_rotation_is_cyclic_invariant_with_duplicate_min() {
        // Two arrays storing the SAME cyclic sequence [M,a,M,b] (M appears twice, is the lex-min
        // vertex) at different starting offsets. `min_by_key`'s "first occurrence" rule rotates
        // BOTH to their own index-0 (since M is already at index 0 in both arrays as stored) and
        // leaves them looking different — a comparison artifact (Defect 1). A true minimal cyclic
        // rotation must normalize both to the identical array.
        let m = [0, 0];
        let a = [1, 1];
        let b = [2, 2];
        let mut r1: GridRing = vec![m, a, m, b];
        let mut r2: GridRing = vec![m, b, m, a]; // same cycle, rotated by 2
        normalize_ring_rotation(&mut r1);
        normalize_ring_rotation(&mut r2);
        assert_eq!(r1, r2);
    }
}

fn normalize_coverage_rotation(cov: &mut [Vec<GridPoly>]) {
    for f in cov.iter_mut() {
        for p in f.iter_mut() {
            for ring in p.iter_mut() {
                normalize_ring_rotation(ring);
            }
        }
    }
}

impl Topology {
    /// Rebuild grid-space geometry (features → polys → rings → OPEN rings) from the arc pool.
    pub fn reconstruct(&self) -> Vec<Vec<GridPoly>> {
        self.features
            .iter()
            .map(|f| {
                f.polys
                    .iter()
                    .map(|p| p.rings.iter().map(|r| self.rebuild_ring(r)).collect())
                    .collect()
            })
            .collect()
    }

    /// The real-data round-trip oracle: reconstruct this topology, compute the expected side
    /// (`snap_only`) independently from `features`, and count how many features differ after
    /// rotation-normalizing every ring (ring equality up to which vertex it happens to start at).
    /// 0 = perfect round-trip. `features`/`tol` should be the same coverage/tolerance this topology
    /// was built from — this is the automated form of the `recon_tests` oracle, runnable from the
    /// `build-topology --verify` CLI on real data (not just fixtures).
    pub fn verify_roundtrip(&self, features: &[Feature], tol: f64) -> usize {
        let mut recon = self.reconstruct();
        let mut expected = snap_only(features, tol);
        normalize_coverage_rotation(&mut recon);
        normalize_coverage_rotation(&mut expected);
        let mismatched_by_index = recon
            .iter()
            .zip(expected.iter())
            .filter(|(a, b)| a != b)
            .count();
        // A length mismatch (shouldn't happen for the topology's own source features, but this is a
        // diagnostic oracle meant to be trustworthy on arbitrary real-data input) counts every extra
        // feature on the longer side as mismatched too, rather than silently truncating via zip.
        let len_mismatch = recon.len().abs_diff(expected.len());
        mismatched_by_index + len_mismatch
    }

    fn rebuild_ring(&self, refs: &[ArcRef]) -> GridRing {
        rebuild_ring_from(refs, &self.arcs)
    }
}

/// Rebuild one ring's OPEN grid vertices from a signed arc-ref loop and a given arc `pool`
/// (parameterised so SP2 can rebuild from a *simplified* pool). ArcRef sign: `a>=0` forward, else
/// `!a` reversed; the joint vertex shared with the previous arc is skipped; a closing dup is dropped.
pub(crate) fn rebuild_ring_from(refs: &[ArcRef], pool: &[ArcLine]) -> GridRing {
    let mut out: GridRing = Vec::new();
    for &r in refs {
        let (id, rev) = if r >= 0 {
            (r as usize, false)
        } else {
            ((!r) as usize, true)
        };
        let arc = &pool[id];
        let seq: Vec<GridPt> = if rev {
            arc.iter().rev().copied().collect()
        } else {
            arc.clone()
        };
        for p in seq {
            if out.last() != Some(&p) {
                out.push(p); // skip the joint vertex shared with the previous arc
            }
        }
    }
    // drop a closing dup so the result is an OPEN ring like snap_only
    if out.len() >= 2 && out.first() == out.last() {
        out.pop();
    }
    out
}

#[cfg(test)]
mod build_tests {
    use super::*;
    use crate::vector::feature::{Feature, Geometry, Props};

    fn sq_ring(ox: f64, oy: f64, s: f64) -> Vec<[f64; 2]> {
        vec![
            [ox, oy],
            [ox + s, oy],
            [ox + s, oy + s],
            [ox, oy + s],
            [ox, oy],
        ]
    }

    #[test]
    fn two_adjacent_squares_share_exactly_one_arc() {
        let l = Feature::new(
            Geometry::Polygon(vec![sq_ring(0.0, 0.0, 10.0)]),
            Props::new(),
            0,
        );
        let r = Feature::new(
            Geometry::Polygon(vec![sq_ring(10.0, 0.0, 10.0)]),
            Props::new(),
            1,
        );
        let (topo, rep) = build_topology(&[l, r], 1.0);
        // exactly one arc has degree 2 (the shared vertical border)
        assert_eq!(topo.degree.iter().filter(|&&d| d >= 2).count(), 1);
        assert_eq!(rep.shared_arcs, 1);
        // that shared arc is referenced +A by one feature and !A by the other
        let shared = topo.degree.iter().position(|&d| d >= 2).unwrap() as i32;
        let refs: Vec<i32> = topo
            .features
            .iter()
            .flat_map(|f| {
                f.polys
                    .iter()
                    .flat_map(|p| p.rings.iter().flatten().copied())
            })
            .collect();
        assert!(refs.contains(&shared) && refs.contains(&!shared));
    }

    #[test]
    fn build_is_deterministic() {
        let l = Feature::new(
            Geometry::Polygon(vec![sq_ring(0.0, 0.0, 10.0)]),
            Props::new(),
            0,
        );
        let r = Feature::new(
            Geometry::Polygon(vec![sq_ring(10.0, 0.0, 10.0)]),
            Props::new(),
            1,
        );
        let a = build_topology(&[l.clone(), r.clone()], 1.0).0;
        let b = build_topology(&[l, r], 1.0).0;
        assert_eq!(a.arcs, b.arcs);
        assert_eq!(a.degree, b.degree);
    }

    #[test]
    fn borders_further_apart_than_tol_do_not_pair() {
        // Right square's shared edge is at x=12, a 2-unit gap from the left's x=10; at tol=1 they
        // snap to DIFFERENT grid columns → no shared arc, both borders are boundary arcs.
        let l = Feature::new(
            Geometry::Polygon(vec![sq_ring(0.0, 0.0, 10.0)]),
            Props::new(),
            0,
        );
        let r = Feature::new(
            Geometry::Polygon(vec![sq_ring(12.0, 0.0, 10.0)]),
            Props::new(),
            1,
        );
        let (topo, rep) = build_topology(&[l, r], 1.0);
        assert_eq!(rep.shared_arcs, 0);
        assert!(topo.degree.iter().all(|&d| d == 1)); // every arc is a boundary arc
    }
}

#[cfg(test)]
mod report_tests {
    use super::*;
    use crate::vector::feature::{Feature, Geometry, Props};

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
    fn area_delta_is_populated_and_grows_with_coarser_snap() {
        // .5 offset forces the snap to actually move vertices; a coarser grid must move them more.
        let f = Feature::new(
            Geometry::Polygon(vec![sq(0.0, 0.0, 100.5)]),
            Props::new(),
            0,
        );
        let fine = build_topology(std::slice::from_ref(&f), 0.01)
            .1
            .total_abs_area_delta;
        let coarse = build_topology(std::slice::from_ref(&f), 10.0)
            .1
            .total_abs_area_delta;
        assert!(coarse > fine); // RED until total_abs_area_delta is actually computed (both 0.0 default)
    }

    #[test]
    fn area_delta_respects_hole_subtraction_sign() {
        // Exterior 100×100 (area 10000) with a 20×20 hole (area 400) → feature.area = 9600.
        // At a fine snap, grid area (exterior − holes) × tol² ≈ 9600, so delta ≈ 0.
        // If holes were ADDED instead of subtracted, the delta would be ≈ 2×hole = ≈800 → this fails.
        let ext = vec![
            [0.0, 0.0],
            [100.0, 0.0],
            [100.0, 100.0],
            [0.0, 100.0],
            [0.0, 0.0],
        ];
        let hole = vec![
            [40.0, 40.0],
            [40.0, 60.0],
            [60.0, 60.0],
            [60.0, 40.0],
            [40.0, 40.0],
        ];
        let f = Feature::new(Geometry::Polygon(vec![ext, hole]), Props::new(), 0);
        let (_, rep) = build_topology(std::slice::from_ref(&f), 0.01);
        assert!(
            rep.total_abs_area_delta < 100.0,
            "delta = {}",
            rep.total_abs_area_delta
        );
    }

    #[test]
    fn never_panics_on_garbage() {
        let cases = vec![
            Feature::new(Geometry::Polygon(vec![]), Props::new(), 0), // no rings
            Feature::new(Geometry::Polygon(vec![vec![[0.0, 0.0]]]), Props::new(), 1), // 1 pt
            Feature::new(
                Geometry::Polygon(vec![vec![[0.0, 0.0], [1.0, 0.0]]]),
                Props::new(),
                2,
            ), // 2 pt
            Feature::new(
                Geometry::Polygon(vec![vec![
                    [0.0, 0.0],
                    [f64::INFINITY, 0.0],
                    [1.0, 1.0],
                    [0.0, 0.0],
                ]]),
                Props::new(),
                3,
            ),
            Feature::new(Geometry::Point([0.0, 0.0]), Props::new(), 4),
        ];
        let (topo, _rep) = build_topology(&cases, 1.0); // must not panic
        let _ = topo.reconstruct(); // must not panic
    }

    #[test]
    fn empty_input_is_empty_topology() {
        let (topo, rep) = build_topology(&[], 1.0);
        assert!(topo.arcs.is_empty() && topo.features.is_empty());
        assert_eq!(rep.features_in, 0);
    }
}

#[cfg(test)]
mod recon_tests {
    use super::*;
    use crate::vector::feature::{Feature, Geometry, Props};

    fn sq_ring(ox: f64, oy: f64, s: f64) -> Vec<[f64; 2]> {
        vec![
            [ox, oy],
            [ox + s, oy],
            [ox + s, oy + s],
            [ox, oy + s],
            [ox, oy],
        ]
    }
    // rotate a ring to lex-min start for rotation-invariant equality — shares
    // `normalize_ring_rotation` with `Topology::verify_roundtrip` (no duplicated rotation logic).
    fn norm(mut cov: Vec<Vec<GridPoly>>) -> Vec<Vec<GridPoly>> {
        normalize_coverage_rotation(&mut cov);
        cov
    }

    #[test]
    fn roundtrip_2x2_block_of_squares() {
        let mut fs = Vec::new();
        for gx in 0..2 {
            for gy in 0..2 {
                fs.push(Feature::new(
                    Geometry::Polygon(vec![sq_ring(gx as f64 * 10.0, gy as f64 * 10.0, 10.0)]),
                    Props::new(),
                    (gx * 2 + gy) as u64,
                ));
            }
        }
        let (topo, _) = build_topology(&fs, 1.0);
        assert_eq!(norm(topo.reconstruct()), norm(snap_only(&fs, 1.0)));
    }

    #[test]
    fn roundtrip_polygon_with_hole() {
        let ext = sq_ring(0.0, 0.0, 30.0);
        let hole = vec![
            [10.0, 10.0],
            [10.0, 20.0],
            [20.0, 20.0],
            [20.0, 10.0],
            [10.0, 10.0],
        ]; // CW-ish, any winding
        let f = Feature::new(Geometry::Polygon(vec![ext, hole]), Props::new(), 0);
        let (topo, _) = build_topology(std::slice::from_ref(&f), 1.0);
        assert_eq!(
            norm(topo.reconstruct()),
            norm(snap_only(std::slice::from_ref(&f), 1.0))
        );
    }

    #[test]
    fn verify_roundtrip_is_zero_for_clean_coverage() {
        let mut fs = Vec::new();
        for gx in 0..2 {
            for gy in 0..2 {
                fs.push(Feature::new(
                    Geometry::Polygon(vec![sq_ring(gx as f64 * 10.0, gy as f64 * 10.0, 10.0)]),
                    Props::new(),
                    (gx * 2 + gy) as u64,
                ));
            }
        }
        let (topo, _) = build_topology(&fs, 1.0);
        assert_eq!(topo.verify_roundtrip(&fs, 1.0), 0);
    }

    #[test]
    fn roundtrip_multipolygon() {
        // One feature, two disjoint squares (a real MultiPolygon geometry) — far enough apart
        // (90 units at tol=1.0) that they never share a grid point.
        let fs = vec![Feature::new(
            Geometry::MultiPolygon(vec![
                vec![sq_ring(0.0, 0.0, 10.0)],
                vec![sq_ring(100.0, 100.0, 10.0)],
            ]),
            Props::new(),
            0,
        )];
        let (topo, _) = build_topology(&fs, 1.0);
        assert_eq!(norm(topo.reconstruct()), norm(snap_only(&fs, 1.0)));
    }

    #[test]
    fn roundtrip_self_touching_ring() {
        // The minimal reproducer from Defect 2 (.superpowers/sdd/mismatch-diagnosis.md): a single
        // ring that revisits vertex [0,0] (a pinch point where two loops P-A-B and P-C-D touch).
        // [0,0] becomes a junction (join.rs); cut_ring splits the ring into two arcs that are each
        // individually closed (first == last == [0,0]). Before the fix, canonicalize_arc rotated
        // each junction arc to its OWN lex-min, discarding the [0,0] anchor, so rebuild_ring
        // couldn't rejoin them at [0,0] → reconstruct grew from 6 to 8 vertices (+2, duplicated).
        let ring = vec![
            [0.0, 0.0],
            [10.0, 0.0],
            [10.0, 10.0],
            [0.0, 0.0],
            [-10.0, 0.0],
            [-10.0, -10.0],
            [0.0, 0.0],
        ];
        let f = Feature::new(Geometry::Polygon(vec![ring]), Props::new(), 0);
        let fs = vec![f];
        let (topo, _) = build_topology(&fs, 1.0);
        assert_eq!(norm(topo.reconstruct()), norm(snap_only(&fs, 1.0)));
    }

    #[test]
    fn roundtrip_island_in_hole() {
        // Feature A: a big square WITH a square hole. Feature B: a square exactly filling that
        // hole — same coordinate set as A's hole, but B stores it starting at a DIFFERENT corner
        // and wound the OPPOSITE way. The rings stay junctionless (identical neighbour sets at
        // every vertex → no junction → is_ring=true whole rings), so both must dedup into ONE
        // shared closed arc referenced by both features. `cut_ring` anchors each whole ring at its
        // lex-min, and canonicalize_arc's direction path (min(seq, rev)) unifies the opposite
        // winding — so this exercises opposite-winding shared-ring dedup + round-trip end-to-end.
        // (The whole-ring lex-min ROTATION itself is guarded directly by the dedup.rs unit test
        // `closed_ring_canonicalizes_rotation_and_direction`; here cut_ring pre-anchors both rings,
        // so this integration test intentionally covers the winding half.)
        let big_ext = sq_ring(0.0, 0.0, 30.0);
        let hole_a = vec![
            [10.0, 10.0],
            [10.0, 20.0],
            [20.0, 20.0],
            [20.0, 10.0],
            [10.0, 10.0],
        ];
        // same square, started at [20,20] and traversed the other direction:
        let hole_b = vec![
            [20.0, 20.0],
            [10.0, 20.0],
            [10.0, 10.0],
            [20.0, 10.0],
            [20.0, 20.0],
        ];
        let a = Feature::new(Geometry::Polygon(vec![big_ext, hole_a]), Props::new(), 0);
        let b = Feature::new(Geometry::Polygon(vec![hole_b]), Props::new(), 1);
        let fs = vec![a, b];
        let (topo, _) = build_topology(&fs, 1.0);
        assert_eq!(norm(topo.reconstruct()), norm(snap_only(&fs, 1.0)));
        // the shared closed ring (A's hole == B's exterior) is stored ONCE, referenced by both
        // features → exactly one arc with degree 2.
        assert_eq!(topo.degree.iter().filter(|&&d| d == 2).count(), 1);
    }
}
