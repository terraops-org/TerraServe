// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! The placement kernel (spec §6) — the bespoke differentiator.
//!
//! Priority-greedy with a uniform-grid collision index. Determinism is load-bearing:
//! - features are processed in a **stable total order** `(priority ASC, fid ASC)`;
//! - all boxes are **quantized to integer px** before any collision test (§8/B2), so sub-pixel
//!   coordinate wobble cannot flip a placement;
//! - candidates are offset from the **marker edge**, so a feature's own marker never blocks its
//!   own label (§6.1/B3);
//! - obstacles are markers + already-placed labels (fills are never obstacles).

use super::index::{Aabb, Grid};
use super::shape::ShapedLabel;

pub struct LabelItem {
    pub fid: u64,
    /// Lower = more important. Null/missing priority → `f64::INFINITY` (sorts last).
    pub priority: f64,
    /// Marker centre, in output pixels.
    pub anchor: [f32; 2],
    pub marker_r: f32,
    pub label: ShapedLabel,
    /// Gap from the marker edge to the label box (px). Set from the SLD `<PointPlacement>
    /// <Displacement>` magnitude at lower time (default 4.0). Per-item so different Text
    /// symbolizers can carry different displacements.
    pub offset: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// Index into the input `items` slice (NOT the fid) — the caller reads the right label even
    /// if two features share a fid (a fid-keyed map would collapse them → wrong text drawn).
    pub item: usize,
    /// Top-left px of the placed label box (quantized).
    pub origin: [f32; 2],
}

#[inline]
fn q(v: f32) -> f32 {
    v.round()
}

fn marker_box(it: &LabelItem) -> Aabb {
    let [cx, cy] = it.anchor;
    let r = it.marker_r;
    Aabb {
        min: [q(cx - r), q(cy - r)],
        max: [q(cx + r), q(cy + r)],
    }
}

/// Candidate label boxes, offset from the **marker edge** (never overlap the own marker), in
/// fixed cartographic-preference order (upper-right first): E, NE, SE, NW, SW, W, N, S.
fn candidates(it: &LabelItem, offset: f32) -> [Aabb; 8] {
    let [cx, cy] = it.anchor;
    let r = it.marker_r;
    let w = it.label.width;
    let h = it.label.height;
    let e = r + offset; // horizontal gap centre→(marker edge + offset)
    let v = r + offset; // vertical gap
    let mk = |ox: f32, oy: f32| Aabb {
        min: [q(ox), q(oy)],
        max: [q(ox + w), q(oy + h)],
    };
    [
        mk(cx + e, cy - h / 2.0),     // E
        mk(cx + e, cy - v - h),       // NE
        mk(cx + e, cy + v),           // SE
        mk(cx - e - w, cy - v - h),   // NW
        mk(cx - e - w, cy + v),       // SW
        mk(cx - e - w, cy - h / 2.0), // W
        mk(cx - w / 2.0, cy - v - h), // N
        mk(cx - w / 2.0, cy + v),     // S
    ]
}

/// Place labels; returns one `Placement` per feature that got a non-colliding slot (others are
/// dropped — their marker is still drawn by the caller). Borrows `items` (sorts indices) so the
/// caller keeps the shaped labels for the drawing pass.
pub fn place_labels(items: &[LabelItem]) -> Vec<Placement> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    // total_cmp gives a total order even for NaN priorities (a future non-JSON source could feed
    // one), so the sort can't violate strict-weak-ordering.
    order.sort_by(|&a, &b| {
        items[a]
            .priority
            .total_cmp(&items[b].priority)
            .then(items[a].fid.cmp(&items[b].fid))
    });
    let mut grid = Grid::new(64.0);
    // markers are always drawn → seed them as obstacles before placing any label.
    for it in items {
        grid.insert(marker_box(it));
    }
    let mut out = Vec::new();
    for &i in &order {
        let it = &items[i];
        if it.label.width <= 0.0 || it.label.glyphs.is_empty() {
            continue; // no label text → marker only
        }
        for c in candidates(it, it.offset) {
            if !grid.overlaps(c) {
                grid.insert(c);
                out.push(Placement {
                    item: i,
                    origin: [c.min[0], c.min[1]],
                });
                break;
            }
        }
    }
    out
}
