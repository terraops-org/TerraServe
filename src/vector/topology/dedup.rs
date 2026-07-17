// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

use super::ArcLine;

/// Canonical orientation (+ whether the input was reversed) so a shared arc keys identically from
/// both traversal directions. `is_ring` (from `cut_ring`) says whether `arc` is a junctionless
/// WHOLE RING (the only case it's safe to rotate away its start vertex) or a junction-bounded arc
/// that may happen to be closed (first == last == the shared junction) but whose anchor must be
/// preserved so `rebuild_ring` can rejoin it to its sibling arc at that junction (Defect 2,
/// `.superpowers/sdd/mismatch-diagnosis.md`).
///
/// - `is_ring == false` (open, OR closed-but-junction-anchored): min(seq, rev) — direction-only.
/// - `is_ring == true` (whole ring, always closed): rotate to lex-min, then min(rot, rev-rot).
pub fn canonicalize_arc(arc: &ArcLine, is_ring: bool) -> (ArcLine, bool) {
    let closed = arc.len() >= 2 && arc.first() == arc.last();
    if !closed || !is_ring {
        let mut rev = arc.clone();
        rev.reverse();
        if *arc <= rev {
            (arc.clone(), false)
        } else {
            (rev, true)
        }
    } else {
        // body without the closing dup
        let body = &arc[..arc.len() - 1];
        let m = body.len();
        let rotate = |seq: &[[i64; 2]]| -> ArcLine {
            let s = (0..m).min_by_key(|&i| seq[i]).unwrap();
            let mut out: ArcLine = (0..m).map(|k| seq[(s + k) % m]).collect();
            out.push(out[0]);
            out
        };
        let fwd = rotate(body);
        let mut rb = body.to_vec();
        rb.reverse();
        let rev = rotate(&rb);
        if fwd <= rev {
            (fwd, false)
        } else {
            (rev, true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_arc_canonicalizes_direction() {
        let fwd = vec![[0, 0], [5, 1], [10, 0]];
        let rev = vec![[10, 0], [5, 1], [0, 0]];
        let (ca, ra) = canonicalize_arc(&fwd, false);
        let (cb, rb) = canonicalize_arc(&rev, false);
        assert_eq!(ca, cb); // same canonical key from both directions
        assert_ne!(ra, rb); // opposite `reversed` flags
    }

    #[test]
    fn closed_ring_canonicalizes_rotation_and_direction() {
        // same triangle loop, started differently and wound oppositely — a shared WHOLE RING
        // (e.g. an island shared by two features), so is_ring=true: rotation must be allowed.
        let a = vec![[0, 0], [10, 0], [5, 8], [0, 0]];
        let b = vec![[5, 8], [10, 0], [0, 0], [5, 8]]; // reversed + rotated
        assert_eq!(canonicalize_arc(&a, true).0, canonicalize_arc(&b, true).0);
    }

    #[test]
    fn closed_junction_arc_does_not_rotate_away_its_anchor() {
        // A junction-bounded arc that happens to be closed (first == last == the shared junction,
        // e.g. a self-touching ring's cut arc) must keep its anchor as first/last — is_ring=false
        // must skip the lex-min rotation even though the arc is closed.
        let arc = vec![[5, 5], [0, 0], [-5, 5], [5, 5]]; // anchored at [5,5], NOT the lex-min [-5,5]
        let (canon, _) = canonicalize_arc(&arc, false);
        assert_eq!(canon.first(), Some(&[5, 5]));
        assert_eq!(canon.last(), Some(&[5, 5]));
    }
}
