// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

use super::{GridPt, GridRing};
use std::collections::{BTreeSet, HashMap};

/// Unordered neighbour pair of a vertex (direction-independent, so a shared edge traversed opposite
/// ways compares equal).
fn pair(a: GridPt, b: GridPt) -> (GridPt, GridPt) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// A vertex is a JUNCTION when it appears in the coverage with two DIFFERENT neighbour pairs
/// (shared by ≥2 distinct boundaries). Bostock's neighbour-set rule; no geometric intersection.
pub fn find_junctions(rings: &[GridRing]) -> BTreeSet<GridPt> {
    let mut seen: HashMap<GridPt, (GridPt, GridPt)> = HashMap::new();
    let mut junc: BTreeSet<GridPt> = BTreeSet::new();
    for ring in rings {
        let n = ring.len();
        if n < 3 {
            continue;
        }
        for i in 0..n {
            let p = ring[i];
            let prev = ring[(i + n - 1) % n];
            let next = ring[(i + 1) % n];
            let np = pair(prev, next);
            match seen.get(&p) {
                None => {
                    seen.insert(p, np);
                }
                Some(&old) => {
                    if old != np {
                        junc.insert(p);
                    }
                }
            }
        }
    }
    junc
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two unit squares sharing the vertical edge x=10 (y from 0..10).
    // Left:  (0,0)(10,0)(10,10)(0,10)   Right: (10,0)(20,0)(20,10)(10,10)
    #[test]
    fn shared_edge_endpoints_are_junctions_midpoints_are_not() {
        let left = vec![[0, 0], [10, 0], [10, 10], [0, 10]];
        let right = vec![[10, 0], [20, 0], [20, 10], [10, 10]];
        let j = find_junctions(&[left, right]);
        // The shared edge is a single segment (10,0)-(10,10); both are corners shared by the two
        // squares with DIFFERENT neighbour sets → junctions. No interior vertex exists on that edge.
        assert!(j.contains(&[10, 0]));
        assert!(j.contains(&[10, 10]));
        // A non-shared corner is visited once → not a junction.
        assert!(!j.contains(&[0, 0]));
    }

    #[test]
    fn midpoint_on_shared_edge_is_not_a_junction() {
        // Insert a matching midpoint (10,5) into BOTH squares' shared edge.
        let left = vec![[0, 0], [10, 0], [10, 5], [10, 10], [0, 10]];
        let right = vec![[10, 0], [20, 0], [20, 10], [10, 10], [10, 5]];
        let j = find_junctions(&[left, right]);
        assert!(!j.contains(&[10, 5])); // same neighbour set {(10,0),(10,10)} both sides
        assert!(j.contains(&[10, 0]));
        assert!(j.contains(&[10, 10]));
    }

    #[test]
    fn isolated_ring_has_no_junctions() {
        let island = vec![[0, 0], [5, 0], [5, 5], [0, 5]];
        assert!(find_junctions(&[island]).is_empty());
    }

    #[test]
    fn deterministic() {
        let a = vec![[0, 0], [10, 0], [10, 10], [0, 10]];
        let b = vec![[10, 0], [20, 0], [20, 10], [10, 10]];
        assert_eq!(
            find_junctions(&[a.clone(), b.clone()]),
            find_junctions(&[a, b])
        );
    }
}
