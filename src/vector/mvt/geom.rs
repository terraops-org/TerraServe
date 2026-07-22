// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! MVT geometry-command encoding: tile-local integer coords → the packed command-integer stream
//! (MoveTo/LineTo/ClosePath, zigzag-delta params). Winding per the MVT spec: exterior rings have
//! positive signed area, interior rings negative (surveyor's formula in the y-down tile grid).

pub const GEOM_POINT: u32 = 1;
pub const GEOM_LINE: u32 = 2;
pub const GEOM_POLYGON: u32 = 3;

const MOVE_TO: u32 = 1;
const LINE_TO: u32 = 2;
const CLOSE_PATH: u32 = 7;

fn command(id: u32, count: u32) -> u32 {
    (id & 0x7) | (count << 3)
}
fn zigzag(n: i32) -> u32 {
    ((n << 1) ^ (n >> 31)) as u32
}

/// Points / MultiPoint: a single MoveTo with count = number of points, delta-encoded.
pub fn encode_points(pts: &[[i32; 2]]) -> Vec<u32> {
    let mut out = Vec::new();
    if pts.is_empty() {
        return out;
    }
    out.push(command(MOVE_TO, pts.len() as u32));
    let (mut cx, mut cy) = (0i32, 0i32);
    for p in pts {
        out.push(zigzag(p[0] - cx));
        out.push(zigzag(p[1] - cy));
        cx = p[0];
        cy = p[1];
    }
    out
}

/// LineString / MultiLineString: one MoveTo(1) + LineTo(n-1) run per part.
pub fn encode_line(parts: &[Vec<[i32; 2]>]) -> Vec<u32> {
    let mut out = Vec::new();
    let (mut cx, mut cy) = (0i32, 0i32);
    for part in parts {
        if part.len() < 2 {
            continue;
        }
        emit_moveto(&mut out, part[0], &mut cx, &mut cy);
        out.push(command(LINE_TO, (part.len() - 1) as u32));
        for p in &part[1..] {
            out.push(zigzag(p[0] - cx));
            out.push(zigzag(p[1] - cy));
            cx = p[0];
            cy = p[1];
        }
    }
    out
}

/// A single Polygon's rings: MoveTo(1)+LineTo(k)+ClosePath per ring, winding-corrected.
/// `rings[0]` is the exterior (positive area); the rest are holes (negative area).
pub fn encode_polygon(rings: &[Vec<[i32; 2]>]) -> Vec<u32> {
    let mut out = Vec::new();
    let (mut cx, mut cy) = (0i32, 0i32);
    for (i, ring) in rings.iter().enumerate() {
        emit_ring(&mut out, ring, i == 0, &mut cx, &mut cy);
    }
    out
}

/// A MultiPolygon (or any set of polygons) into ONE command stream — a feature has exactly one
/// geometry field. Each polygon's ring[0] is its own exterior (positive area), its remaining rings
/// holes (negative). Unlike `encode_polygon`, which would treat only the very first ring of a
/// flattened list as exterior, this **restarts the exterior/hole winding per polygon**, so a
/// MultiPolygon's 2nd-and-later pieces are wound correctly rather than inverted like holes. The
/// delta cursor runs continuously across every ring of every polygon (never resets), matching a
/// real decoder — see the ClosePath-cursor note in `emit_ring`.
pub fn encode_multipolygon(polys: &[Vec<Vec<[i32; 2]>>]) -> Vec<u32> {
    let mut out = Vec::new();
    let (mut cx, mut cy) = (0i32, 0i32);
    for poly in polys {
        for (i, ring) in poly.iter().enumerate() {
            emit_ring(&mut out, ring, i == 0, &mut cx, &mut cy);
        }
    }
    out
}

/// Emit one ring's MoveTo+LineTo+ClosePath into `out`, correcting winding to `want_positive`
/// (exterior wants positive signed area, holes negative), advancing the shared delta cursor
/// `(cx, cy)`. Rings with fewer than 3 distinct vertices are skipped (emit nothing).
fn emit_ring(
    out: &mut Vec<u32>,
    ring: &[[i32; 2]],
    want_positive: bool,
    cx: &mut i32,
    cy: &mut i32,
) {
    let mut r = ring.to_vec();
    if r.len() >= 2 && r.first() == r.last() {
        r.pop(); // MVT drops the explicit closing vertex; ClosePath implies it.
    }
    if r.len() < 3 {
        return;
    }
    if (signed_area(&r) > 0.0) != want_positive {
        r.reverse();
    }
    emit_moveto(out, r[0], cx, cy);
    out.push(command(LINE_TO, (r.len() - 1) as u32));
    for p in &r[1..] {
        out.push(zigzag(p[0] - *cx));
        out.push(zigzag(p[1] - *cy));
        *cx = p[0];
        *cy = p[1];
    }
    out.push(command(CLOSE_PATH, 1));
    // NOTE: ClosePath does NOT move the delta cursor — it stays at the last vertex drawn (the
    // LineTo loop above already left cx,cy there). Real MVT decoders (verified against the
    // `mapbox_vector_tile` Python reference implementation's `parse_geometry`) do not reset their
    // cursor on ClosePath either; a prior version reset cx,cy to the ring's first vertex here,
    // which desyncs the encoder from every real decoder for any 2nd-and-later ring (a hole, or a
    // MultiPolygon's 2nd+ polygon) — caught by the MVT golden (a polygon-with-hole) failing
    // external-parser validation.
}

fn emit_moveto(out: &mut Vec<u32>, p: [i32; 2], cx: &mut i32, cy: &mut i32) {
    out.push(command(MOVE_TO, 1));
    out.push(zigzag(p[0] - *cx));
    out.push(zigzag(p[1] - *cy));
    *cx = p[0];
    *cy = p[1];
}

/// Shoelace signed area (integer grid). Positive = counter-clockwise in y-up; in the y-down tile
/// grid the sign convention flips, which is exactly why the MVT spec calls exterior rings the
/// positive-area ones here. We only use the SIGN to decide reversal, so absolute convention is moot.
fn signed_area(ring: &[[i32; 2]]) -> f64 {
    let mut a = 0i64;
    let n = ring.len();
    for i in 0..n {
        let p = ring[i];
        let q = ring[(i + 1) % n];
        a += (p[0] as i64) * (q[1] as i64) - (q[0] as i64) * (p[1] as i64);
    }
    a as f64 / 2.0
}
