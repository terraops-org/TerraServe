// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! The `--mvt-cell-px` dominant-class cell mosaic (Stage B of the MVT optimization pipeline): fills
//! the black holes a size filter leaves on a wall-to-wall coverage by rasterising the candidate
//! POLYGONS into a coarse tile-grid, voting a class per cell, and run-length-merging same-class rows
//! into rectangles that REPLACE the polygon geometry. Seam-free (world-aligned power-of-2 cells) and
//! browser-light (bounded cell count). See
//! `docs/superpowers/specs/2026-07-14-mvt-optimization-pipeline-design.md`.

use std::collections::HashMap;

use super::tile::{value_dedup_key, BUF, EXTENT};
use crate::vector::feature::{Feature, Geometry, Value};
use crate::vector::geom::Projector;

/// Build the class palette from the candidate polygons: each distinct value of the thematic `field`
/// (keyed by `value_dedup_key`, so `Str("3")` and `Num(3)` stay distinct like everywhere else) → a
/// `u16` index starting at 1 (index 0 is reserved = empty cell). Returns the palette
/// (`palette[idx - 1]` is the `Value` for cell index `idx`) plus a lookup from dedup-key → index for
/// the rasteriser. Features whose `field` is absent or `Null` contribute nothing (cells stay empty).
pub(crate) fn build_palette(polys: &[&Feature], field: &str) -> (Vec<Value>, HashMap<String, u16>) {
    let mut palette: Vec<Value> = Vec::new();
    let mut index: HashMap<String, u16> = HashMap::new();
    for f in polys {
        let Some(v) = f.props.get(field) else {
            continue;
        };
        if matches!(v, Value::Null) {
            continue;
        }
        let key = value_dedup_key(v);
        if index.contains_key(&key) {
            continue;
        }
        // A thematic class field never has this many distinct values; the cap keeps the u16 index
        // from wrapping (aliasing classes / hitting the reserved 0) if `--mvt-cell-field` is
        // mistakenly pointed at a high-cardinality column (e.g. a parcel ID). Overflow classes just
        // stay empty rather than silently mis-colouring.
        if palette.len() >= (u16::MAX - 1) as usize {
            continue;
        }
        palette.push(v.clone());
        index.insert(key, palette.len() as u16); // 1-based; 0 is the reserved "empty" index
    }
    (palette, index)
}

/// A coarse per-cell class grid for one tile: an `n×n` row-major grid of palette indices (0 = empty).
/// Cell `(col, row)` is world-cell `(col + k_min, row + k_min)`, spanning tile-units
/// `[k·cell, (k+1)·cell)` on each axis. The grid covers the buffered rect `[-BUF, EXTENT+BUF]` so a
/// stroke-bearing style paints on the buffer, not the visible tile edge (Fable-5 finding 7).
pub(crate) struct CellGrid {
    pub cell: u32,
    pub k_min: i32,
    pub n: usize,
    pub idx: Vec<u16>,
}

impl CellGrid {
    /// Palette index at grid coordinate `(col, row)` (0 = empty). Out-of-range panics (caller-owned).
    pub fn at(&self, col: usize, row: usize) -> u16 {
        self.idx[row * self.n + col]
    }
}

/// `(k_min, n)` for a cell size: the world-cell index of grid position 0 and the grid side length,
/// covering the buffered rect `[-BUF, EXTENT+BUF]`. `div_euclid` gives the floor for negatives.
pub(crate) fn grid_dims(cell: u32) -> (i32, usize) {
    let c = cell as i32;
    let k_min = (-(BUF as i32)).div_euclid(c);
    let k_max = (EXTENT as i32 + BUF as i32 - 1).div_euclid(c);
    (k_min, (k_max - k_min + 1) as usize)
}

/// Rasterise projected polygons into the cell grid by even-odd scanline at each cell-row centre
/// (cell-CENTRE coverage). `polys` = `(palette_index, rings)` with `rings` already in tile-4096
/// units (all rings of the feature: exterior + holes; a MultiPolygon flattens all parts' rings).
/// Even-odd across all of a feature's rings excludes holes; features are processed in order so a
/// later write overwrites an earlier one (painter's order — the deterministic, tile-independent
/// overlap tie-break). `O(features × rows × edges + cells)`.
pub(crate) fn rasterise_cells(polys: &[(u16, Vec<Vec<[f64; 2]>>)], cell: u32) -> CellGrid {
    let (k_min, n) = grid_dims(cell);
    let cf = cell as f64;
    let mut idx = vec![0u16; n * n];
    let mut xs: Vec<f64> = Vec::new();
    for (index, rings) in polys {
        for row in 0..n {
            let yc = ((k_min + row as i32) as f64 + 0.5) * cf; // cell-row centre (tile units)
            xs.clear();
            for ring in rings {
                let m = ring.len();
                if m < 2 {
                    continue;
                }
                for i in 0..m {
                    let a = ring[i];
                    let b = ring[(i + 1) % m];
                    let (y0, y1) = (a[1], b[1]);
                    // Half-open [min,max) crossing rule: each crossing counted once, horizontal
                    // edges (y0==y1) skipped — the standard even-odd scanline convention.
                    if (y0 <= yc && yc < y1) || (y1 <= yc && yc < y0) {
                        let t = (yc - y0) / (y1 - y0);
                        xs.push(a[0] + t * (b[0] - a[0]));
                    }
                }
            }
            if xs.len() < 2 {
                continue;
            }
            xs.sort_by(|p, q| p.total_cmp(q)); // total_cmp: never panics if a projected x is non-finite
                                               // Even-odd: the interior spans are the consecutive pairs (xs[0],xs[1]), (xs[2],xs[3]), …
            let mut j = 0;
            while j + 1 < xs.len() {
                let (xl, xr) = (xs[j], xs[j + 1]);
                for col in 0..n {
                    let xc = ((k_min + col as i32) as f64 + 0.5) * cf; // cell-col centre
                    if xc >= xl && xc < xr {
                        idx[row * n + col] = *index;
                    }
                }
                j += 2;
            }
        }
    }
    CellGrid {
        cell,
        k_min,
        n,
        idx,
    }
}

/// A merged run of same-class cells in one grid row: a tile-unit box `[x0,x1]×[y0,y1]` (clamped to
/// the buffered rect) carrying the class `Value`.
pub(crate) struct CellRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
    pub class: Value,
}

/// Run-length-merge each grid row's runs of an equal non-zero index into one rectangle, clamped to
/// the buffered rect `[-BUF, EXTENT+BUF]` (so a huge cell can't overshoot far past the tile and a
/// stroke lands on the buffer, not the visible edge). Index 0 (empty) emits nothing; `palette[idx-1]`
/// is the class Value. Output size is bounded by (rows × class-runs-per-row) — the browser-light win.
pub(crate) fn merge_to_rects(grid: &CellGrid, palette: &[Value]) -> Vec<CellRect> {
    let lo = -(BUF as i32);
    let hi = EXTENT as i32 + BUF as i32;
    let c = grid.cell as i32;
    let mut out: Vec<CellRect> = Vec::new();
    for row in 0..grid.n {
        let y0 = ((grid.k_min + row as i32) * c).clamp(lo, hi);
        let y1 = ((grid.k_min + row as i32 + 1) * c).clamp(lo, hi);
        if y0 >= y1 {
            continue; // fully outside the buffered rect
        }
        let mut col = 0;
        while col < grid.n {
            let v = grid.at(col, row);
            if v == 0 {
                col += 1;
                continue;
            }
            let start = col;
            while col < grid.n && grid.at(col, row) == v {
                col += 1;
            }
            let x0 = ((grid.k_min + start as i32) * c).clamp(lo, hi);
            let x1 = ((grid.k_min + col as i32) * c).clamp(lo, hi);
            if x0 >= x1 {
                continue;
            }
            out.push(CellRect {
                x0,
                y0,
                x1,
                y1,
                class: palette[(v - 1) as usize].clone(),
            });
        }
    }
    out
}

/// Project a POLYGON/MultiPolygon feature's rings to tile-4096 units (all rings: exterior + holes;
/// a MultiPolygon flattens every part's rings for even-odd rasterisation). `None` for a non-polygon,
/// a Polygon whose ring fails to project, or a MultiPolygon with no projectable part.
fn project_feature_rings(proj: &Projector, geom: &Geometry) -> Option<Vec<Vec<[f64; 2]>>> {
    let ring = |r: &[[f64; 2]]| -> Option<Vec<[f64; 2]>> {
        r.iter()
            .map(|p| proj.to_pixel(p[0], p[1]).map(|(x, y)| [x as f64, y as f64]))
            .collect()
    };
    match geom {
        Geometry::Polygon(rings) => {
            let mut out = Vec::with_capacity(rings.len());
            for r in rings {
                out.push(ring(r)?);
            }
            Some(out)
        }
        Geometry::MultiPolygon(polys) => {
            let mut out = Vec::new();
            for poly in polys {
                // Project each PART all-or-nothing: dropping a part's exterior while keeping its hole
                // would invert even-odd parity (the hole would fill as inside). Skip a whole failing
                // part (a domain-edge vertex), never a single ring of a surviving part.
                let part: Option<Vec<Vec<[f64; 2]>>> = poly.iter().map(|r| ring(r)).collect();
                if let Some(part) = part {
                    out.extend(part);
                }
            }
            (!out.is_empty()).then_some(out)
        }
        _ => None, // points/lines are not polygons — handled on the normal path
    }
}

/// The full Stage-B pipeline for one tile: build the class palette from the candidate `polys`,
/// project + even-odd-rasterise them into the cell grid (painter's order), and run-length-merge the
/// rows into rectangles. Returns the rects (empty if no polygon carries a class value). The caller
/// emits each rect as an MVT polygon feature tagged `field = rect.class`.
pub(crate) fn mosaic_rects(
    proj: &Projector,
    polys: &[&Feature],
    field: &str,
    cell: u32,
) -> Vec<CellRect> {
    let (palette, index) = build_palette(polys, field);
    if palette.is_empty() {
        return Vec::new();
    }
    let mut projected: Vec<(u16, Vec<Vec<[f64; 2]>>)> = Vec::new();
    for f in polys {
        let Some(v) = f.props.get(field) else {
            continue;
        };
        if matches!(v, Value::Null) {
            continue;
        }
        let Some(&pidx) = index.get(&value_dedup_key(v)) else {
            continue;
        };
        if let Some(rings) = project_feature_rings(proj, &f.geom) {
            if !rings.is_empty() {
                projected.push((pidx, rings));
            }
        }
    }
    let grid = rasterise_cells(&projected, cell);
    merge_to_rects(&grid, &palette)
}

#[cfg(test)]
mod tests {
    use super::build_palette;
    use crate::vector::feature::{Feature, Geometry, Props, Value};

    fn poly_with(field: &str, val: Option<Value>) -> Feature {
        let mut props = Props::new();
        if let Some(v) = val {
            props.insert(field.to_string(), v);
        }
        let ring = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.0, 0.0]];
        Feature::new(Geometry::Polygon(vec![ring]), props, 1)
    }

    #[test]
    fn build_palette_indexes_distinct_classes_skips_missing_and_null() {
        let a = poly_with("c", Some(Value::Str("A".into())));
        let b = poly_with("c", Some(Value::Str("B".into())));
        let a2 = poly_with("c", Some(Value::Str("A".into()))); // duplicate class A
        let absent = poly_with("c", None); // field absent → contributes nothing
        let null = poly_with("c", Some(Value::Null)); // Null → contributes nothing
        let polys: Vec<&Feature> = vec![&a, &b, &a2, &absent, &null];

        let (palette, index) = build_palette(&polys, "c");
        assert_eq!(palette.len(), 2, "only the two distinct classes A, B");
        assert_eq!(index.len(), 2);
        // 1-based indices: A → 1, B → 2, so palette[idx-1] recovers the Value.
        assert!(matches!(&palette[0], Value::Str(s) if s == "A"));
        assert!(matches!(&palette[1], Value::Str(s) if s == "B"));
    }

    fn sq(lo: f64, hi: f64) -> Vec<[f64; 2]> {
        vec![[lo, lo], [hi, lo], [hi, hi], [lo, hi], [lo, lo]]
    }

    #[test]
    fn grid_dims_covers_core_plus_buffer() {
        // cell=1024, EXTENT=4096, BUF=256 → core cells k=0..3, plus buffer k=-1 and k=4 → n=6.
        assert_eq!(super::grid_dims(1024), (-1, 6));
    }

    #[test]
    fn scanline_fills_square_excludes_hole_and_honours_painters_order() {
        // Feature 1: exterior = full tile with a central hole [1024,3072]² (even-odd → the 2×2 centre
        // core cells stay empty). Feature 2 (later → painter's order) covers the (0,0) corner cell.
        let f1 = (1u16, vec![sq(0.0, 4096.0), sq(1024.0, 3072.0)]);
        let f2 = (2u16, vec![sq(0.0, 1024.0)]);
        let g = super::rasterise_cells(&[f1, f2], 1024);
        assert_eq!((g.k_min, g.n), (-1, 6));
        // core cell (kx,ky) → grid (col,row) = (kx - k_min, ky - k_min) = (kx+1, ky+1).
        let core = |kx: i32, ky: i32| g.at((kx + 1) as usize, (ky + 1) as usize);
        assert_eq!(core(1, 1), 0, "hole cell empty");
        assert_eq!(core(2, 2), 0, "hole cell empty");
        assert_eq!(core(0, 1), 1, "non-hole core cell filled by feature 1");
        assert_eq!(
            core(0, 0),
            2,
            "painter's order: later feature 2 overwrites (0,0)"
        );
        assert_eq!(g.at(0, 0), 0, "buffer cell k=-1 stays empty");
    }

    #[test]
    fn merge_to_rects_run_length_merges_a_row() {
        // Row 0 = [1,1,1,2,2] → 2 rects (NOT 5 cells): a class-A run of 3 + a class-B run of 2.
        let n = 5;
        let mut idx = vec![0u16; n * n];
        idx[0] = 1;
        idx[1] = 1;
        idx[2] = 1;
        idx[3] = 2;
        idx[4] = 2;
        let grid = super::CellGrid {
            cell: 512,
            k_min: 0,
            n,
            idx,
        };
        let palette = vec![Value::Str("A".into()), Value::Str("B".into())];
        let rects = super::merge_to_rects(&grid, &palette);
        assert_eq!(
            rects.len(),
            2,
            "run-length merge: 5 same-row cells → 2 rects"
        );
        assert_eq!(
            (rects[0].x0, rects[0].x1, rects[0].y0, rects[0].y1),
            (0, 1536, 0, 512),
            "class-A run spans cols 0..3"
        );
        assert!(matches!(&rects[0].class, Value::Str(s) if s == "A"));
        assert_eq!(
            (rects[1].x0, rects[1].x1),
            (1536, 2560),
            "class-B run spans cols 3..5"
        );
        assert!(matches!(&rects[1].class, Value::Str(s) if s == "B"));
    }
}
