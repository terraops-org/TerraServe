// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Geometry clipping to a tile rect. Polygons via Sutherland-Hodgman (against the 4 edges);
//! lines via Liang-Barsky per segment (a line may split into several pieces). This is the shared
//! clip the MVT encoder needs; the same algorithm fixes `render.rs::project_and_cull`'s vertex-drop
//! (wiring that is a separate, out-of-scope follow-up).

/// `rect = [minx, miny, maxx, maxy]`.
type Rect = [f64; 4];

/// Sutherland-Hodgman clip of each ring against the rect. Returns clipped rings (empty rings
/// dropped). A ring is treated as closed (the caller may pass it open or closed).
pub fn clip_polygon(rings: &[Vec<[f64; 2]>], rect: Rect) -> Vec<Vec<[f64; 2]>> {
    rings
        .iter()
        .map(|ring| clip_ring(ring, rect))
        .filter(|r| r.len() >= 3)
        .collect()
}

fn clip_ring(ring: &[[f64; 2]], rect: Rect) -> Vec<[f64; 2]> {
    // edges: 0 = x>=minx (left), 1 = x<=maxx (right), 2 = y>=miny (bottom), 3 = y<=maxy (top).
    let mut poly = ring.to_vec();
    // normalize: drop an explicit closing duplicate if present.
    if poly.len() >= 2 && poly.first() == poly.last() {
        poly.pop();
    }
    for edge in 0..4 {
        if poly.is_empty() {
            break;
        }
        poly = clip_ring_edge(&poly, rect, edge);
    }
    poly
}

/// `inside`/`intersect` for one rect edge, Sutherland-Hodgman.
fn clip_ring_edge(poly: &[[f64; 2]], rect: Rect, edge: usize) -> Vec<[f64; 2]> {
    let inside = |p: [f64; 2]| match edge {
        0 => p[0] >= rect[0],
        1 => p[0] <= rect[2],
        2 => p[1] >= rect[1],
        _ => p[1] <= rect[3],
    };
    // intersection of segment a->b with the edge line.
    let intersect = |a: [f64; 2], b: [f64; 2]| -> [f64; 2] {
        match edge {
            0 | 1 => {
                let xe = if edge == 0 { rect[0] } else { rect[2] };
                let t = (xe - a[0]) / (b[0] - a[0]);
                [xe, a[1] + t * (b[1] - a[1])]
            }
            _ => {
                let ye = if edge == 2 { rect[1] } else { rect[3] };
                let t = (ye - a[1]) / (b[1] - a[1]);
                [a[0] + t * (b[0] - a[0]), ye]
            }
        }
    };
    let mut out = Vec::new();
    let n = poly.len();
    for i in 0..n {
        let cur = poly[i];
        let prev = poly[(i + n - 1) % n];
        let (ci, pi) = (inside(cur), inside(prev));
        if ci {
            if !pi {
                out.push(intersect(prev, cur));
            }
            out.push(cur);
        } else if pi {
            out.push(intersect(prev, cur));
        }
    }
    out
}

/// Liang-Barsky clip of a polyline; returns the in-rect pieces (a line entering/leaving the rect
/// several times yields several pieces). Each output piece has >= 2 points.
pub fn clip_line(line: &[[f64; 2]], rect: Rect) -> Vec<Vec<[f64; 2]>> {
    let mut pieces = Vec::new();
    let mut cur: Vec<[f64; 2]> = Vec::new();
    for w in line.windows(2) {
        if let Some((a, b)) = clip_segment(w[0], w[1], rect) {
            if cur.is_empty() {
                cur.push(a);
            } else if cur.last() != Some(&a) {
                // a gap (the previous segment left the rect) → flush and start a new piece.
                if cur.len() >= 2 {
                    pieces.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
                cur.push(a);
            }
            cur.push(b);
        } else if cur.len() >= 2 {
            pieces.push(std::mem::take(&mut cur));
        } else {
            cur.clear();
        }
    }
    if cur.len() >= 2 {
        pieces.push(cur);
    }
    pieces
}

/// Liang-Barsky clip of a single segment to the rect. `None` if fully outside.
fn clip_segment(a: [f64; 2], b: [f64; 2], rect: Rect) -> Option<([f64; 2], [f64; 2])> {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let mut t0 = 0.0f64;
    let mut t1 = 1.0f64;
    let p = [-dx, dx, -dy, dy];
    let q = [
        a[0] - rect[0],
        rect[2] - a[0],
        a[1] - rect[1],
        rect[3] - a[1],
    ];
    for i in 0..4 {
        if p[i].abs() < 1e-12 {
            if q[i] < 0.0 {
                return None; // parallel and outside
            }
        } else {
            let r = q[i] / p[i];
            if p[i] < 0.0 {
                if r > t1 {
                    return None;
                }
                if r > t0 {
                    t0 = r;
                }
            } else {
                if r < t0 {
                    return None;
                }
                if r < t1 {
                    t1 = r;
                }
            }
        }
    }
    // Return the EXACT endpoint when a segment end isn't clipped (`t0==0` / `t1==1`) rather than the
    // recomputed `a + t*d` — otherwise float drift (`a + 1.0*(b-a) != b`) makes an interior segment's
    // end differ from the next segment's exact start, and `clip_line`'s continuity check
    // (`cur.last() != Some(&a)`) sees a false gap and shatters a continuous polyline into pieces.
    // `t0`/`t1` are only moved off their 0.0/1.0 initial values by an actual clip, so the `==` is exact.
    Some((
        if t0 == 0.0 {
            a
        } else {
            [a[0] + t0 * dx, a[1] + t0 * dy]
        },
        if t1 == 1.0 {
            b
        } else {
            [a[0] + t1 * dx, a[1] + t1 * dy]
        },
    ))
}
