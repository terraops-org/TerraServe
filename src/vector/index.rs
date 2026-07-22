// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Uniform-grid spatial index — viewport cull + label-box collision (spec §6.3).
//!
//! `overlaps` returns a **boolean** (set membership), so the answer is independent of bucket /
//! vector iteration order: the index's internals cannot perturb placement output (determinism).

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aabb {
    pub min: [f32; 2],
    pub max: [f32; 2],
}

impl Aabb {
    pub fn intersects(&self, o: &Aabb) -> bool {
        self.min[0] < o.max[0]
            && self.max[0] > o.min[0]
            && self.min[1] < o.max[1]
            && self.max[1] > o.min[1]
    }
}

pub struct Grid {
    cell: f32,
    buckets: HashMap<(i32, i32), Vec<Aabb>>,
}

impl Grid {
    pub fn new(cell_px: f32) -> Grid {
        Grid {
            cell: cell_px.max(1.0),
            buckets: HashMap::new(),
        }
    }

    fn cells(&self, b: &Aabb) -> (i32, i32, i32, i32) {
        (
            (b.min[0] / self.cell).floor() as i32,
            (b.min[1] / self.cell).floor() as i32,
            (b.max[0] / self.cell).floor() as i32,
            (b.max[1] / self.cell).floor() as i32,
        )
    }

    pub fn insert(&mut self, b: Aabb) {
        let (x0, y0, x1, y1) = self.cells(&b);
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                self.buckets.entry((cx, cy)).or_default().push(b);
            }
        }
    }

    pub fn overlaps(&self, b: Aabb) -> bool {
        let (x0, y0, x1, y1) = self.cells(&b);
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                if let Some(v) = self.buckets.get(&(cx, cy)) {
                    for other in v {
                        if b.intersects(other) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
}
