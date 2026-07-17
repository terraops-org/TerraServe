// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! `--mvt-dissolve` — on-the-fly per-tile same-class polygon dissolve by **edge-cancellation** on the
//! rounded i32 tile grid (the `ST_CoverageUnion` / "buffer-zero" union *result* — merged polygon,
//! internal borders gone, true boundaries — via shared-edge annihilation, bespoke, no GEOS). Adjacent
//! same-class polygons traverse their shared border in opposite directions, so a directed edge and
//! its reverse cancel; what survives is the true outer/hole boundary. See
//! `docs/superpowers/specs/2026-07-14-mvt-dissolve-design.md` (Fable-5-reviewed).

use std::collections::BTreeMap;

use super::tile::{to_i32_ring, value_dedup_key, EXTENT};
use crate::vector::feature::{Feature, Geometry, Value};
use crate::vector::geom::Projector;

/// An integer tile-grid vertex.
pub(crate) type Vertex = [i32; 2];
/// A directed edge `from → to`.
pub(crate) type Edge = (Vertex, Vertex);
/// A ring: vertices in order, NOT explicitly closed (no repeated first==last).
pub(crate) type Ring = Vec<Vertex>;
/// One polygon: `[exterior, hole, hole, …]`.
pub(crate) type PolyGroup = Vec<Ring>;

/// Per-tile diagnostics (Fable-5 F3) — logged so "the coverage is pristine" is measured, not trusted.
#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct Diag {
    /// Rings dropped: zero-area weld junk, an odd-depth ring with no container, or a walk that
    /// couldn't close. A rising count on real data flags the mismatched-subdivision welds (F3).
    pub dropped_rings: u32,
}

/// Emit every ring's consecutive directed edges (closing `last→first` via the `% n` wrap; degenerate
/// `a==b` edges skipped) and cancel reverse pairs, returning the surviving directed-edge **multiset**
/// as counts. COUNTS, not a set (Fable-5 F2): same-direction duplicate edges are real at low zoom
/// (welded borders) and a set would collapse their multiplicity, unbalancing the graph so the
/// ring-walk can't close. For edge `(a,b)`: if `(b,a)` has a positive count, decrement it (both
/// cancel); else increment `(a,b)`. The surviving multiset is Eulerian-balanced.
pub(crate) fn cancel_edges(rings: &[Ring]) -> BTreeMap<Edge, u32> {
    let mut counts: BTreeMap<Edge, u32> = BTreeMap::new();
    for ring in rings {
        let n = ring.len();
        if n < 2 {
            continue;
        }
        for i in 0..n {
            let a = ring[i];
            let b = ring[(i + 1) % n]; // the `% n` wrap closes the ring; a closed ring's last
            if a == b {
                continue; // …edge is (last,first)==(a,a) → skipped here, so open or closed both work
            }
            let rev = (b, a);
            match counts.get_mut(&rev) {
                Some(c) => {
                    *c -= 1;
                    if *c == 0 {
                        counts.remove(&rev);
                    }
                }
                None => *counts.entry((a, b)).or_insert(0) += 1,
            }
        }
    }
    counts
}

/// Reassemble the surviving directed edges into closed rings. Deterministic (Fable-5 F4): a
/// `BTreeMap` adjacency (sorted), the smallest unused edge starts each ring, and at a junction
/// (>1 outgoing) the **rightmost turn** relative to the incoming direction is taken — the standard
/// planar face-walk, which yields non-self-crossing rings (earcut-safe). Because the input multiset
/// is Eulerian-balanced (Stage 2), every walk closes; a defensive step bound handles malformed input.
pub(crate) fn walk_rings(edges: &BTreeMap<Edge, u32>) -> Vec<Ring> {
    let mut rem = edges.clone();
    let mut total: u64 = rem.values().map(|&c| c as u64).sum();
    let mut rings: Vec<Ring> = Vec::new();
    while total > 0 {
        // Smallest unused edge starts the ring (BTreeMap iterates in sorted key order → deterministic).
        let (start, first) = match rem.iter().find(|(_, &c)| c > 0).map(|(&e, _)| e) {
            Some(e) => e,
            None => break,
        };
        consume(&mut rem, (start, first));
        total -= 1;
        let mut ring: Ring = vec![start];
        let mut prev = start;
        let mut current = first;
        let guard = total + 1; // defensive: a balanced walk closes within the remaining edge count
        let mut steps = 0u64;
        while current != start {
            ring.push(current);
            let Some(next) = pick_next(&rem, current, prev) else {
                break; // dead end (only on malformed/unbalanced input) → drop this partial ring
            };
            consume(&mut rem, (current, next));
            total -= 1;
            prev = current;
            current = next;
            steps += 1;
            if steps > guard {
                break;
            }
        }
        if current == start && ring.len() >= 3 {
            rings.push(ring);
        }
    }
    rings
}

/// Decrement (and remove at 0) one unit of a directed edge's count.
fn consume(rem: &mut BTreeMap<Edge, u32>, e: Edge) {
    if let Some(c) = rem.get_mut(&e) {
        *c -= 1;
        if *c == 0 {
            rem.remove(&e);
        }
    }
}

/// Pick the next vertex to walk to from `current`, arriving from `prev`. With one outgoing edge, take
/// it. At a junction, take the **rightmost turn** — the outgoing edge immediately clockwise from the
/// reversed incoming direction (the planar face-walk). `None` if `current` has no outgoing edge.
fn pick_next(rem: &BTreeMap<Edge, u32>, current: Vertex, prev: Vertex) -> Option<Vertex> {
    // Outgoing edges of `current`, sorted by `to` (BTreeMap range → deterministic tie-break).
    let lo = (current, [i32::MIN, i32::MIN]);
    let hi = (current, [i32::MAX, i32::MAX]);
    let outs: Vec<Vertex> = rem
        .range(lo..=hi)
        .filter(|(_, &c)| c > 0)
        .map(|(&(_, to), _)| to)
        .collect();
    match outs.len() {
        0 => None,
        1 => Some(outs[0]),
        _ => {
            let back = [prev[0] - current[0], prev[1] - current[1]]; // reversed incoming
            outs.into_iter().min_by(|&x, &y| {
                let ox = [x[0] - current[0], x[1] - current[1]];
                let oy = [y[0] - current[0], y[1] - current[1]];
                cw_dist(back, ox).total_cmp(&cw_dist(back, oy))
            })
        }
    }
}

/// A pseudo-angle of a direction, increasing counter-clockwise in `[0, 4)` (E=0, N=1, W=2, S=3). Cheap
/// and exact-for-integers (no `atan2`) — determinism-safe.
fn pseudo_angle(v: Vertex) -> f64 {
    let (x, y) = (v[0] as f64, v[1] as f64);
    let denom = x.abs() + y.abs();
    if denom == 0.0 {
        return 0.0; // zero vector shouldn't occur (a != b upstream)
    }
    let p = x / denom;
    if y >= 0.0 {
        1.0 - p
    } else {
        3.0 + p
    }
}

/// Clockwise angular distance from `back` to `o` in `(0, 4]` — the reversed-incoming edge itself maps
/// to 4 (last resort), so `min` picks the first edge clockwise from it (the rightmost turn).
fn cw_dist(back: Vertex, o: Vertex) -> f64 {
    let d = (pseudo_angle(back) - pseudo_angle(o)).rem_euclid(4.0);
    if d == 0.0 {
        4.0
    } else {
        d
    }
}

/// Twice the signed area (shoelace) in **i128** — at world-grid scale (SP2b's `dissolve_coverage` runs
/// the kernels on an i32 WORLD grid, not the 0..4096 tile grid) an i64 accumulator can overflow on a
/// large, detailed ring (products up to (2³¹)² summed over millions of vertices); i128 is always safe.
/// For tile-range values the result is identical, so the on-the-fly path + goldens are unchanged. The
/// magnitude feeds `group_polygons` hole-nesting; the sign feeds `normalise_ring`.
fn signed_area2(ring: &[Vertex]) -> i128 {
    let n = ring.len();
    if n < 3 {
        return 0;
    }
    let mut a: i128 = 0;
    for i in 0..n {
        let p = ring[i];
        let q = ring[(i + 1) % n];
        a += p[0] as i128 * q[1] as i128 - q[0] as i128 * p[1] as i128;
    }
    a
}

/// A guaranteed-interior point of a ring at **half-integer y** (Fable-5 F1): a ray at `y = min_y+0.5`
/// passes through no integer vertex, so classification is unambiguous. `None` for a zero-area ring.
fn interior_point(ring: &[Vertex]) -> Option<[f64; 2]> {
    if signed_area2(ring) == 0 {
        return None;
    }
    let min_y = ring.iter().map(|v| v[1]).min()?;
    let ys = min_y as f64 + 0.5;
    let mut xs: Vec<f64> = Vec::new();
    let n = ring.len();
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        let (y0, y1) = (a[1] as f64, b[1] as f64);
        if (y0 <= ys && ys < y1) || (y1 <= ys && ys < y0) {
            let t = (ys - y0) / (y1 - y0);
            xs.push(a[0] as f64 + t * (b[0] as f64 - a[0] as f64));
        }
    }
    xs.sort_by(|p, q| p.total_cmp(q));
    (xs.len() >= 2).then(|| [(xs[0] + xs[1]) / 2.0, ys])
}

/// `[minx, miny, maxx, maxy]` of a ring.
fn ring_bbox(ring: &[Vertex]) -> [i32; 4] {
    let mut b = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
    for v in ring {
        b[0] = b[0].min(v[0]);
        b[1] = b[1].min(v[1]);
        b[2] = b[2].max(v[0]);
        b[3] = b[3].max(v[1]);
    }
    b
}

/// Even-odd ray-cast point-in-ring on an i32 ring at an f64 point (the point comes from `interior_point`
/// at half-integer y, so no vertex lies on the ray → unambiguous).
fn point_in_ring(ring: &[Vertex], pt: [f64; 2]) -> bool {
    let (px, py) = (pt[0], pt[1]);
    let n = ring.len();
    let mut inside = false;
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        let (ay, by) = (a[1] as f64, b[1] as f64);
        if (ay <= py) != (by <= py) {
            let x = a[0] as f64 + (py - ay) / (by - ay) * (b[0] as f64 - a[0] as f64);
            if px < x {
                inside = !inside;
            }
        }
    }
    inside
}

/// Group rings into polygons `[exterior, hole, …]` by **even-odd nesting depth** (Fable-5 F1/F3/F6):
/// drop zero-area rings; each ring's depth = how many others contain its interior point (bbox
/// pre-check then `point_in_ring`); even depth = exterior (its own polygon), odd = a hole of its
/// smallest-area (tightest) containing exterior; an odd ring with no container is dropped.
pub(crate) fn group_polygons(rings: Vec<Ring>) -> Vec<PolyGroup> {
    // Keep non-zero-area rings with a precomputed interior point, bbox, and |area| (for tightest-
    // container selection). Preserves input order → deterministic.
    struct R {
        ring: Ring,
        pt: [f64; 2],
        bbox: [i32; 4],
        area2: i128,
    }
    let rs: Vec<R> = rings
        .into_iter()
        .filter_map(|ring| {
            let area2 = signed_area2(&ring);
            if area2 == 0 {
                return None; // zero-area weld junk (F3)
            }
            let pt = interior_point(&ring)?;
            let bbox = ring_bbox(&ring);
            Some(R {
                ring,
                pt,
                bbox,
                area2: area2.abs(),
            })
        })
        .collect();
    let n = rs.len();

    // Nesting depth = number of OTHER rings containing this ring's interior point (bbox pre-check
    // then point_in_ring — F6). The tightest (smallest-|area|) container is the immediately-enclosing
    // ring, whose depth is one less → for an odd (hole) ring it is always an even (exterior) ring.
    let mut depth = vec![0u32; n];
    let mut container = vec![None::<usize>; n];
    for i in 0..n {
        for j in 0..n {
            // A container must be strictly LARGER (a smaller ring can't enclose a bigger one) — this
            // guards the case where ring i's interior point coincidentally lands inside a smaller
            // nested ring j, which would otherwise miscount i as nested in j.
            if i == j || rs[j].area2 <= rs[i].area2 {
                continue;
            }
            if bbox_contains(rs[j].bbox, rs[i].pt) && point_in_ring(&rs[j].ring, rs[i].pt) {
                depth[i] += 1;
                if container[i].map_or(true, |b| rs[j].area2 < rs[b].area2) {
                    container[i] = Some(j);
                }
            }
        }
    }

    // Even depth = exterior (its own polygon); odd = a hole appended to its container exterior.
    let mut group_of: BTreeMap<usize, usize> = BTreeMap::new(); // exterior ring index → group idx
    let mut groups: Vec<PolyGroup> = Vec::new();
    for (i, r) in rs.iter().enumerate() {
        if depth[i] % 2 == 0 {
            group_of.insert(i, groups.len());
            groups.push(vec![r.ring.clone()]);
        }
    }
    for (i, r) in rs.iter().enumerate() {
        if depth[i] % 2 == 1 {
            if let Some(&g) = container[i].and_then(|c| group_of.get(&c)) {
                groups[g].push(r.ring.clone());
            } // odd-depth ring with no exterior container → dropped
        }
    }
    groups
}

/// Whether an integer bbox `[minx,miny,maxx,maxy]` contains an f64 point.
fn bbox_contains(b: [i32; 4], pt: [f64; 2]) -> bool {
    pt[0] >= b[0] as f64 && pt[0] <= b[2] as f64 && pt[1] >= b[1] as f64 && pt[1] <= b[3] as f64
}

/// Reverse `ring` iff its signed-area sign disagrees with `want_positive` — so all exteriors get one
/// canonical winding and all holes the opposite, which is what makes adjacent same-class exteriors
/// traverse their shared border in opposite directions (→ cancellation). A zero-area ring has no
/// orientation to fix (Fable-5 F9).
pub(crate) fn normalise_ring(ring: Ring, want_positive: bool) -> Ring {
    let a = signed_area2(&ring);
    if a == 0 || (a > 0) == want_positive {
        ring
    } else {
        let mut r = ring;
        r.reverse();
        r
    }
}

/// Project one source polygon's rings (ring 0 = exterior, rest = holes) to the rounded i32 tile grid
/// and normalise winding by role (exterior canonical-positive, holes negative). `None` if a ring
/// fails to project — drops the WHOLE polygon (per-part all-or-nothing, so a dropped exterior can't
/// leave a stray hole) — or if nothing survives rounding. Pure → safe to run in parallel.
fn project_normalise_poly(proj: &Projector, rings: &[Vec<[f64; 2]>]) -> Option<Vec<Ring>> {
    let mut normed: Vec<Ring> = Vec::new();
    for (idx, ring) in rings.iter().enumerate() {
        let projected: Option<Vec<[f64; 2]>> = ring
            .iter()
            .map(|p| {
                proj.to_pixel(p[0], p[1]).map(|(x, y)| {
                    // Clamp far-off coords before rounding (Fable-5 review #6): world/continent-scale
                    // geometry at high z would otherwise overflow i32 on `as i32` and the i64 shoelace.
                    // ±2^24 is far outside any tile so it clips away harmlessly.
                    const LIM: f32 = (1 << 24) as f32;
                    [x.clamp(-LIM, LIM) as f64, y.clamp(-LIM, LIM) as f64]
                })
            })
            .collect();
        let projected = projected?; // a vertex outside the CRS domain → drop this whole polygon
        let i32ring = to_i32_ring(&projected, true); // grid-snap dedup ON (dissolve is dedup-invariant)
        if i32ring.len() < 3 {
            continue; // degenerate after rounding
        }
        normed.push(normalise_ring(i32ring, idx == 0));
    }
    (!normed.is_empty()).then_some(normed)
}

/// The full per-tile dissolve: for each candidate polygon, project + round + normalise winding + group
/// its rings by class; then per class, `cancel_edges` → `walk_rings` → `group_polygons`. Returns the
/// dissolved `poly_groups` per class (in class order) plus per-tile diagnostics (Fable-5 F3). The
/// caller clips + `encode_multipolygon`s each `(class, poly_groups)`.
pub(crate) fn dissolve_features(
    polys: &[&Feature],
    field: &str,
    src_crs: &str,
    tile_crs: &str,
    bbox: [f64; 4],
) -> (Vec<(Value, Vec<PolyGroup>)>, Diag) {
    // 1. Project + round + normalise per polygon, bucket by class. This is SERIAL by design: the
    //    libproj transform serializes internally (a shared PROJ/proj.db context lock), so rayon-
    //    parallelising it only adds thread coordination + per-thread PROJ-context creation and runs
    //    SLOWER than serial (measured 110–144 s vs 98 s on a z6 COS tile). The real speed fix is
    //    offline precompute (project once), not threading — see bench/dissolve-2026-07-14.md.
    let proj = match Projector::new(src_crs, tile_crs, bbox, EXTENT, EXTENT) {
        Ok(p) => p,
        Err(_) => return (Vec::new(), Diag::default()),
    };
    // BTreeMap keyed by `value_dedup_key` → deterministic class order (Fable-5 F4).
    let mut buckets: BTreeMap<String, (Value, Vec<Ring>)> = BTreeMap::new();
    for f in polys {
        let Some(v) = f.props.get(field) else {
            continue;
        };
        if matches!(v, Value::Null) {
            continue;
        }
        let mut rings: Vec<Ring> = Vec::new();
        match &f.geom {
            Geometry::Polygon(r) => {
                if let Some(rr) = project_normalise_poly(&proj, r) {
                    rings.extend(rr);
                }
            }
            Geometry::MultiPolygon(parts) => {
                for part in parts {
                    if let Some(rr) = project_normalise_poly(&proj, part) {
                        rings.extend(rr);
                    }
                }
            }
            _ => continue,
        }
        if rings.is_empty() {
            continue;
        }
        buckets
            .entry(value_dedup_key(v))
            .or_insert_with(|| (v.clone(), Vec::new()))
            .1
            .extend(rings);
    }
    // 2. per class: cancel → walk → group.
    let mut out: Vec<(Value, Vec<PolyGroup>)> = Vec::new();
    let mut diag = Diag::default();
    for (_key, (value, rings)) in buckets {
        let edges = cancel_edges(&rings);
        let walked = walk_rings(&edges);
        let walked_n = walked.len();
        let groups = group_polygons(walked);
        let grouped_n: usize = groups.iter().map(|g| g.len()).sum();
        diag.dropped_rings += walked_n.saturating_sub(grouped_n) as u32;
        if !groups.is_empty() {
            out.push((value, groups));
        }
    }
    (out, diag)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A CCW rectangle ring `[min..max]` (NOT explicitly closed).
    fn rect(min: Vertex, max: Vertex) -> Ring {
        vec![
            [min[0], min[1]],
            [max[0], min[1]],
            [max[0], max[1]],
            [min[0], max[1]],
        ]
    }

    #[test]
    fn two_adjacent_squares_cancel_shared_edge() {
        // Unit squares [0,0]-[1,1] and [1,0]-[2,1] share the vertical edge at x=1. Same winding →
        // A walks it up (1,0)→(1,1), B walks it down (1,1)→(1,0): reverses → both cancel. The 6
        // outer edges of the merged 2×1 rectangle survive (bottom/top split at the shared vertices).
        let a = rect([0, 0], [1, 1]);
        let b = rect([1, 0], [2, 1]);
        let surv = cancel_edges(&[a, b]);
        assert!(
            !surv.contains_key(&([1, 0], [1, 1])),
            "shared edge must cancel"
        );
        assert!(
            !surv.contains_key(&([1, 1], [1, 0])),
            "shared edge reverse must cancel"
        );
        for e in [
            ([0, 0], [1, 0]),
            ([1, 0], [2, 0]),
            ([2, 0], [2, 1]),
            ([2, 1], [1, 1]),
            ([1, 1], [0, 1]),
            ([0, 1], [0, 0]),
        ] {
            assert_eq!(surv.get(&e), Some(&1), "outer edge {e:?} missing");
        }
        assert_eq!(surv.len(), 6);
    }

    #[test]
    fn same_direction_duplicate_edges_survive_with_count() {
        // Two identical squares → every edge is traversed TWICE in the same direction (no reverses),
        // so each survives with count 2. A HashSet would collapse to count 1 → graph imbalance (F2).
        let sq = rect([0, 0], [2, 2]);
        let surv = cancel_edges(&[sq.clone(), sq]);
        assert_eq!(surv.len(), 4, "the 4 distinct square edges");
        assert!(
            surv.values().all(|&c| c == 2),
            "each edge count 2, got {surv:?}"
        );
    }

    #[test]
    fn walk_single_square() {
        // The 4 edges of a unit square → one ring, starting at the smallest vertex, in walk order.
        let edges = cancel_edges(&[rect([0, 0], [1, 1])]);
        let rings = walk_rings(&edges);
        assert_eq!(rings.len(), 1);
        assert_eq!(rings[0], vec![[0, 0], [1, 0], [1, 1], [0, 1]]);
    }

    #[test]
    fn walk_two_disjoint_squares_gives_two_rings() {
        let edges = cancel_edges(&[rect([0, 0], [1, 1]), rect([10, 10], [11, 11])]);
        let rings = walk_rings(&edges);
        assert_eq!(rings.len(), 2);
    }

    #[test]
    fn walk_merged_pair_is_one_ring_and_deterministic() {
        // Two adjacent squares dissolve to one boundary → one ring; walking twice is byte-identical.
        let edges = cancel_edges(&[rect([0, 0], [3, 3]), rect([3, 0], [6, 3])]);
        let r1 = walk_rings(&edges);
        let r2 = walk_rings(&edges);
        assert_eq!(r1.len(), 1, "merged into one ring");
        assert_eq!(r1, r2, "walk must be deterministic");
    }

    #[test]
    fn group_hole_nested_under_exterior() {
        let ext = rect([0, 0], [10, 10]);
        let hole = rect([3, 3], [7, 7]);
        let groups = group_polygons(vec![ext.clone(), hole.clone()]);
        assert_eq!(groups.len(), 1, "one polygon");
        assert_eq!(groups[0].len(), 2, "[exterior, hole]");
        assert_eq!(groups[0][0], ext, "exterior first");
        assert_eq!(groups[0][1], hole, "hole second");
    }

    #[test]
    fn group_pinch_hole_touching_exterior_still_nests() {
        // A triangle hole whose vertex [5,0] lies EXACTLY on the exterior's bottom edge. First-vertex
        // sampling would test [5,0] (on ext's boundary → coin-flip); the half-integer interior point
        // [5,0.5] is unambiguous → still nested as a hole (Fable-5 F1, mutation-proof).
        let ext = rect([0, 0], [10, 10]);
        let hole = vec![[5, 0], [8, 4], [2, 4]];
        let groups = group_polygons(vec![ext, hole]);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].len(),
            2,
            "hole nested despite touching the exterior"
        );
    }

    #[test]
    fn group_drops_zero_area_ring() {
        let ext = rect([0, 0], [10, 10]);
        let degenerate = vec![[2, 2], [5, 2], [2, 2]]; // retraces → zero area
        let groups = group_polygons(vec![ext.clone(), degenerate]);
        assert_eq!(groups.len(), 1, "zero-area ring dropped");
        assert_eq!(groups[0][0], ext);
    }

    #[test]
    fn group_disjoint_rings_are_separate_polygons() {
        let groups = group_polygons(vec![rect([0, 0], [2, 2]), rect([10, 10], [12, 12])]);
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|g| g.len() == 1), "each a lone exterior");
    }

    #[test]
    fn normalise_ring_forces_canonical_winding() {
        let ccw = vec![[0, 0], [2, 0], [2, 2], [0, 2]]; // signed area2 = +8
        let cw = vec![[0, 0], [0, 2], [2, 2], [2, 0]]; // signed area2 = -8
        assert!(signed_area2(&normalise_ring(ccw, true)) > 0);
        assert!(
            signed_area2(&normalise_ring(cw, true)) > 0,
            "wrong winding reversed"
        );
    }

    // --- driver tests: EPSG:3857 source == tile CRS → near-identity onto 0..4096 (per-thread projs) ---
    fn dissolve_at(
        polys: &[&Feature],
        field: &str,
        z: u32,
        x: u32,
        y: u32,
    ) -> (Vec<(Value, Vec<PolyGroup>)>, Diag) {
        let grid = crate::tms::preset("WebMercatorQuad", 4096).unwrap();
        let bbox = grid.tile_bounds(z, x, y).unwrap();
        dissolve_features(polys, field, "EPSG:3857", "EPSG:3857", bbox)
    }
    fn class_poly(x0: f64, y0: f64, x1: f64, y1: f64, cls: &str, fid: u64, cw: bool) -> Feature {
        use crate::vector::feature::{Feature, Geometry, Props, Value};
        let mut props = Props::new();
        props.insert("cls".into(), Value::Str(cls.into()));
        let mut ring = vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]];
        if cw {
            ring.reverse();
        }
        Feature::new(Geometry::Polygon(vec![ring]), props, fid)
    }

    #[test]
    fn dissolve_two_same_class_squares_into_one_polygon() {
        let a = class_poly(
            100_000.0,
            4_500_000.0,
            300_000.0,
            4_700_000.0,
            "A",
            1,
            false,
        );
        let b = class_poly(
            300_000.0,
            4_500_000.0,
            500_000.0,
            4_700_000.0,
            "A",
            2,
            false,
        );
        let (out, _diag) = dissolve_at(&[&a, &b], "cls", 6, 32, 24);
        assert_eq!(out.len(), 1, "one class");
        assert!(matches!(&out[0].0, Value::Str(s) if s == "A"));
        assert_eq!(
            out[0].1.len(),
            1,
            "merged into ONE polygon (internal edge cancelled)"
        );
    }

    #[test]
    fn inconsistently_wound_pair_still_dissolves() {
        // A wound CCW, B wound CW — without winding normalisation the shared edge would NOT cancel.
        let a = class_poly(
            100_000.0,
            4_500_000.0,
            300_000.0,
            4_700_000.0,
            "A",
            1,
            false,
        );
        let b = class_poly(300_000.0, 4_500_000.0, 500_000.0, 4_700_000.0, "A", 2, true);
        let (out, _) = dissolve_at(&[&a, &b], "cls", 6, 32, 24);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].1.len(),
            1,
            "normalisation lets them merge despite opposite winding"
        );
    }

    #[test]
    fn different_classes_do_not_merge() {
        let a = class_poly(
            100_000.0,
            4_500_000.0,
            300_000.0,
            4_700_000.0,
            "A",
            1,
            false,
        );
        let b = class_poly(
            300_000.0,
            4_500_000.0,
            500_000.0,
            4_700_000.0,
            "B",
            2,
            false,
        );
        let (out, _) = dissolve_at(&[&a, &b], "cls", 6, 32, 24);
        assert_eq!(out.len(), 2, "two classes, each its own polygon");
    }

    #[test]
    fn dissolve_is_deterministic() {
        let a = class_poly(
            100_000.0,
            4_500_000.0,
            300_000.0,
            4_700_000.0,
            "A",
            1,
            false,
        );
        let b = class_poly(
            300_000.0,
            4_500_000.0,
            500_000.0,
            4_700_000.0,
            "A",
            2,
            false,
        );
        let (o1, _) = dissolve_at(&[&a, &b], "cls", 6, 32, 24);
        let (o2, _) = dissolve_at(&[&a, &b], "cls", 6, 32, 24);
        assert_eq!(o1.len(), o2.len());
        assert_eq!(o1[0].1, o2[0].1, "dissolve output must be deterministic");
    }
}
