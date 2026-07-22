// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

use super::{ArcLine, GridPt, GridRing};
use std::collections::BTreeSet;

/// Split `ring` (OPEN, no closing dup) into arcs at its junction vertices. Each arc includes both
/// endpoints (so consecutive arcs share their junction vertex). Returns each arc paired with
/// `is_ring`: `true` only for the junctionless whole-ring branch (one CLOSED arc, first == last,
/// rotated to start at its lexicographically-smallest vertex); `false` for every junction-bounded
/// arc — including one that happens to be closed itself (a self-touching ring cut at both
/// occurrences of the same junction vertex produces two arcs each anchored first == last == that
/// junction). The flag matters downstream: `canonicalize_arc` must rotate a whole ring to its own
/// lex-min (so a shared island dedups regardless of start) but must NOT rotate a junction-bounded
/// arc even when closed, or it discards the junction anchor `rebuild_ring` needs to rejoin arcs
/// (Defect 2, `.superpowers/sdd/mismatch-diagnosis.md`).
pub fn cut_ring(ring: &GridRing, junc: &BTreeSet<GridPt>) -> Vec<(ArcLine, bool)> {
    let n = ring.len();
    let cuts: Vec<usize> = (0..n).filter(|&i| junc.contains(&ring[i])).collect();
    if cuts.is_empty() {
        // one closed arc, rotated to lex-min anchor, first == last — a WHOLE RING
        let start = (0..n).min_by_key(|&i| ring[i]).unwrap();
        let mut arc: ArcLine = (0..n).map(|k| ring[(start + k) % n]).collect();
        arc.push(ring[start]);
        return vec![(arc, true)];
    }
    let mut arcs = Vec::with_capacity(cuts.len());
    for w in 0..cuts.len() {
        let i = cuts[w];
        let j = cuts[(w + 1) % cuts.len()];
        let mut arc = vec![ring[i]];
        let mut k = i;
        loop {
            k = (k + 1) % n;
            arc.push(ring[k]);
            if k == j {
                break;
            }
        }
        arcs.push((arc, false)); // junction-bounded — never a whole ring, even if closed
    }
    arcs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn junctionless_ring_is_one_closed_arc_anchored_at_lex_min() {
        let ring = vec![[5, 5], [0, 0], [10, 0]]; // lex-min is [0,0]
        let arcs = cut_ring(&ring, &BTreeSet::new());
        assert_eq!(arcs.len(), 1);
        let (a, is_ring) = &arcs[0];
        assert_eq!(a.first(), Some(&[0, 0])); // anchored at lex-min
        assert_eq!(a.first(), a.last()); // CLOSED
        assert!(*is_ring); // junctionless whole ring
    }

    #[test]
    fn ring_with_two_junctions_splits_into_two_arcs_sharing_endpoints() {
        // square with junctions at [0,0] and [10,10] (opposite corners)
        let ring = vec![[0, 0], [10, 0], [10, 10], [0, 10]];
        let j: BTreeSet<_> = [[0, 0], [10, 10]].into_iter().collect();
        let arcs = cut_ring(&ring, &j);
        assert_eq!(arcs.len(), 2);
        // every arc starts and ends on a junction
        for (a, is_ring) in &arcs {
            assert!(j.contains(a.first().unwrap()) && j.contains(a.last().unwrap()));
            assert!(!is_ring); // junction-bounded arcs are never whole-ring arcs
        }
    }

    #[test]
    fn self_touching_ring_yields_junction_anchored_arcs_not_rings() {
        // A ring that revisits vertex [0,0] (a pinch point) — join.rs's neighbour-pair rule makes
        // [0,0] a junction. cut_ring must split at BOTH occurrences into two arcs that are each
        // individually closed (first == last == [0,0], the shared junction) but MUST be flagged
        // `is_ring = false`: they are junction-bounded, not a whole independent ring, and
        // `canonicalize_arc` must not rotate away the junction anchor (Defect 2 in
        // .superpowers/sdd/mismatch-diagnosis.md).
        let ring = vec![[0, 0], [10, 0], [10, 10], [0, 0], [-10, 0], [-10, -10]];
        let j: BTreeSet<_> = [[0, 0]].into_iter().collect();
        let arcs = cut_ring(&ring, &j);
        assert_eq!(arcs.len(), 2);
        for (a, is_ring) in &arcs {
            assert!(
                !is_ring,
                "junction-anchored arc must not be flagged is_ring"
            );
            assert_eq!(a.first(), Some(&[0, 0]));
            assert_eq!(a.last(), Some(&[0, 0]));
        }
    }
}
