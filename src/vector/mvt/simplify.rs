// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Weighted Visvalingam–Whyatt polyline simplification — the mapshaper-default coverage-simplifier
//! kernel, as a standalone pure unit (unwired: the seam-free *coverage* wiring — shared-arc topology
//! — is a separate architecture decision). Iteratively removes the point with the smallest WEIGHTED
//! effective area (triangle area × an angle weight that preserves sharp corners) until the smallest
//! remaining metric ≥ `min_area`. **Endpoints are always preserved**, so arcs meeting at junction
//! nodes stay connected (the seam-relevant guarantee this kernel provides). Deterministic. Bespoke.
//! Heap + doubly-linked list → `O(n log n)`.
//!
//! **Unwired on purpose** — this is the "kernel first, then decide" step: the algorithm lands and is
//! proven in isolation before we commit to an architecture (offline global-topology precompute vs
//! on-the-fly per-tile). `allow(dead_code)` until a caller wires it in.
//!
//! **Two obligations the coverage-wiring step must honour (out of scope for this kernel):**
//! 1. *Canonical arc orientation.* The coordinate tie-break makes simplification direction-symmetric
//!    for every realistic arc; the one residual hole is two DISTINCT interior vertices at the same
//!    grid point with a bit-identical metric (a self-touching arc). Feeding every shared arc in a
//!    canonical orientation (e.g. lexicographically-smaller endpoint first) before simplifying closes
//!    it completely — fwd and rev then reduce to the same call.
//! 2. *No self-intersection repair.* Visvalingam can make a simplified arc cross itself or a
//!    neighbour, and a thin sliver's two arcs can each collapse to their endpoints (zero-area ring).
//!    Guarding against that (mapshaper does an intersection-repair pass) is the wiring step's job.
#![allow(dead_code)]

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// Mapshaper's default weighting coefficient — `weight = 1 − k·cosθ` ∈ [1−k, 1+k].
const WEIGHT_K: f64 = 0.7;

/// Weighted effective area of vertex `b` with neighbours `a`, `c`: the triangle area × an angle
/// weight. `cosθ` is the turn between edges `a→b` and `b→c`: a straight run (cos=1 → weight 0.3) is
/// cheap to drop; a sharp turn (small cos → weight up to 1.7) is favoured for keeping. Collinear (or
/// degenerate zero-width-spike) points get area 0 and go first regardless of weight.
///
/// Direction-symmetric by construction: `weighted_area(a,b,c) == weighted_area(c,b,a)` bit-exactly —
/// the `unsigned_abs` cross-product is reversal-invariant and the cosine of `(b−a)·(c−b)` is unchanged
/// when the vectors map `(u,v) → (−v,−u)`. This is the algebraic foundation of the seam guarantee.
fn weighted_area(a: [i64; 2], b: [i64; 2], c: [i64; 2]) -> f64 {
    // Deltas in i64 (topology world-grid coords); the cross-product in i128 (i64² exceeds i64).
    let (abx, aby) = (b[0] - a[0], b[1] - a[1]); // a→b
    let (acx, acy) = (c[0] - a[0], c[1] - a[1]); // a→c
    let (bcx, bcy) = (c[0] - b[0], c[1] - b[1]); // b→c
    let area2 = (abx as i128 * acy as i128 - acx as i128 * aby as i128).unsigned_abs() as f64; // 2×area
    let (ux, uy) = (abx as f64, aby as f64);
    let (vx, vy) = (bcx as f64, bcy as f64);
    let (du, dv) = ((ux * ux + uy * uy).sqrt(), (vx * vx + vy * vy).sqrt());
    let cos = if du > 0.0 && dv > 0.0 {
        ((ux * vx + uy * vy) / (du * dv)).clamp(-1.0, 1.0)
    } else {
        1.0 // a zero-length edge → treat as straight
    };
    (area2 * 0.5) * (1.0 - WEIGHT_K * cos)
}

/// Simplify an OPEN polyline by Weighted Visvalingam, preserving both endpoints. Removes interior
/// points while the smallest weighted effective area is `< min_area`. `min_area <= 0` or ≤ 2 points
/// returns the input unchanged.
pub(crate) fn simplify_visvalingam(pts: &[[i64; 2]], min_area: f64) -> Vec<[i64; 2]> {
    let n = pts.len();
    // `!(min_area > 0.0)` also rejects NaN (which would otherwise pass `<= 0.0` and delete everything).
    if n <= 2 || !(min_area > 0.0) {
        return pts.to_vec();
    }
    // Doubly-linked list over the original indices; `usize::MAX` is the endpoint sentinel.
    let mut prev: Vec<usize> = (0..n).map(|i| i.wrapping_sub(1)).collect();
    let mut next: Vec<usize> = (0..n).map(|i| i + 1).collect();
    prev[0] = usize::MAX;
    next[n - 1] = usize::MAX;
    let mut removed = vec![false; n];
    let mut ver = vec![0u32; n]; // bumped each time a point's metric is recomputed → stale-entry check

    // Seed the heap with every INTERIOR point (endpoints 0 and n-1 are never candidates).
    let mut heap: BinaryHeap<Item> = BinaryHeap::with_capacity(n);
    for i in 1..n - 1 {
        heap.push(Item {
            metric: weighted_area(pts[i - 1], pts[i], pts[i + 1]),
            pt: pts[i],
            idx: i,
            ver: 0,
        });
    }

    while let Some(it) = heap.pop() {
        if removed[it.idx] || it.ver != ver[it.idx] {
            continue; // stale — a neighbour changed after this entry was pushed
        }
        if it.metric >= min_area {
            break; // the smallest surviving metric is above the threshold → done
        }
        removed[it.idx] = true;
        let (p, q) = (prev[it.idx], next[it.idx]);
        next[p] = q;
        prev[q] = p;
        // Recompute the two neighbours; skip endpoints (a MAX on either side) — they stay pinned.
        for &j in &[p, q] {
            let (jp, jn) = (prev[j], next[j]);
            if jp != usize::MAX && jn != usize::MAX {
                ver[j] += 1;
                heap.push(Item {
                    metric: weighted_area(pts[jp], pts[j], pts[jn]),
                    pt: pts[j],
                    idx: j,
                    ver: ver[j],
                });
            }
        }
    }

    // Walk the surviving list from the first endpoint.
    let mut out = Vec::with_capacity(n);
    let mut i = 0usize;
    loop {
        out.push(pts[i]);
        i = next[i];
        if i == usize::MAX {
            break;
        }
    }
    out
}

/// Min-heap item (Rust's `BinaryHeap` is a max-heap; `Ord` is reversed so the SMALLEST metric pops
/// first). Ties break by **coordinate** — a direction-independent key, so the removal sequence is a
/// function of the geometry alone, not of which end the arc was numbered from. That is what keeps a
/// shared boundary arc simplifying identically whichever polygon walks it (seam-safe). `idx` is a
/// final total-order fallback for the degenerate case of two vertices at the same point; `ver`
/// lazily invalidates stale entries.
struct Item {
    metric: f64,
    pt: [i64; 2],
    idx: usize,
    ver: u32,
}
impl PartialEq for Item {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal // stay consistent with Ord (total_cmp orders ±0.0; `==` wouldn't)
    }
}
impl Eq for Item {}
impl Ord for Item {
    fn cmp(&self, o: &Self) -> Ordering {
        o.metric
            .total_cmp(&self.metric)
            .then(o.pt.cmp(&self.pt))
            .then(o.idx.cmp(&self.idx))
    }
}
impl PartialOrd for Item {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collinear_interior_points_are_removed() {
        // A straight run — the interior points have zero area → dropped, endpoints kept.
        let line = [[0, 0], [1, 0], [2, 0], [3, 0]];
        assert_eq!(simplify_visvalingam(&line, 0.5), vec![[0, 0], [3, 0]]);
    }

    #[test]
    fn corner_is_preserved_edge_midpoint_removed() {
        // [5,0] is a collinear midpoint on the bottom edge (removed); [10,0] is a real corner (kept).
        let l = [[0, 0], [5, 0], [10, 0], [10, 10]];
        assert_eq!(
            simplify_visvalingam(&l, 0.5),
            vec![[0, 0], [10, 0], [10, 10]]
        );
    }

    #[test]
    fn endpoints_always_preserved() {
        // Even a huge threshold keeps the two endpoints (never removed).
        let l = [[0, 0], [5, 5], [10, 0]];
        let out = simplify_visvalingam(&l, 1e12);
        assert_eq!(out.first(), Some(&[0, 0]));
        assert_eq!(out.last(), Some(&[10, 0]));
        assert!(out.len() >= 2);
    }

    #[test]
    fn higher_threshold_removes_at_least_as_many() {
        let l = [[0, 0], [2, 1], [4, 0], [6, 3], [8, 0], [10, 1], [12, 0]];
        let lo = simplify_visvalingam(&l, 1.0);
        let hi = simplify_visvalingam(&l, 100.0);
        assert!(
            hi.len() <= lo.len(),
            "higher threshold must not keep MORE points"
        );
        assert!(hi.len() >= 2);
    }

    #[test]
    fn is_deterministic() {
        let l = [[0, 0], [2, 1], [4, 0], [6, 3], [8, 0], [10, 1], [12, 0]];
        assert_eq!(simplify_visvalingam(&l, 3.0), simplify_visvalingam(&l, 3.0));
    }

    #[test]
    fn handles_large_coordinates_without_overflow() {
        // A coordinate delta spanning > 2^31 must not overflow the subtraction inside weighted_area
        // (i32-first arithmetic panics in debug / wraps in release). The kernel documents no tight
        // range contract, so it must stay correct across the whole i32 domain.
        let l = [[-2_000_000_000, 0], [2_000_000_000, 5], [0, 0]];
        let out = simplify_visvalingam(&l, 1.0);
        assert_eq!(out.first(), Some(&[-2_000_000_000, 0]));
        assert_eq!(out.last(), Some(&[0, 0]));
    }

    #[test]
    fn handles_i64_range_coordinates_beyond_i32() {
        // A coordinate that does NOT fit in i32 — only representable after the i64 retype. Proves the
        // kernel now spans the topology's world-grid range.
        let l = [[0i64, 0], [3_000_000_000, 5], [6_000_000_000, 0]];
        let out = simplify_visvalingam(&l, 1.0);
        assert_eq!(out.first(), Some(&[0i64, 0]));
        assert_eq!(out.last(), Some(&[6_000_000_000i64, 0]));
    }

    #[test]
    fn non_positive_or_nan_threshold_is_a_noop() {
        let l = [[0, 0], [1, 5], [2, 0], [3, 5], [4, 0]];
        assert_eq!(simplify_visvalingam(&l, 0.0), l.to_vec());
        assert_eq!(simplify_visvalingam(&l, -3.0), l.to_vec());
        assert_eq!(simplify_visvalingam(&l, f64::NAN), l.to_vec());
    }

    #[test]
    fn simplification_is_direction_symmetric_seam_safe() {
        // THE seam guarantee. A shared boundary arc is walked FORWARD by one polygon and BACKWARD
        // by its neighbour. Simplifying both copies must leave coincident survivors (reverse of one
        // == the other) — otherwise a gap/sliver opens along the shared border. A left↔right
        // symmetric zigzag forces exact metric ties, so an index-based tie-break (which flips under
        // reversal) would desync the two copies; only a geometry-based tie-break stays seam-safe.
        let arc = [
            [0, 0],
            [2, 3],
            [4, 0],
            [6, 3],
            [8, 0],
            [10, 3],
            [12, 0],
            [14, 3],
            [16, 0],
        ];
        // 8.0 sits above the uniform initial metric (6) so removals actually happen and the
        // tie-break decides which vertices go — the case that exposes a direction-dependent order.
        let fwd = simplify_visvalingam(&arc, 8.0);
        let mut rev_in = arc.to_vec();
        rev_in.reverse();
        let mut rev = simplify_visvalingam(&rev_in, 8.0);
        rev.reverse();
        assert_eq!(
            fwd, rev,
            "opposite-direction arcs must coincide (seam-free)"
        );
    }
}
