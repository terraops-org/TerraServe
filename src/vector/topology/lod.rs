// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Per-zoom level-of-detail (LOD) for topology serve. Build one simplified coverage per zoom at
//! startup (coarser at low zoom, floored at the operator's `--topology-simplify` at high zoom) so the
//! overview is browser-light (no 18 MB / 8.8M-vertex z6 tile) while high zoom stays detailed. Zoom →
//! tolerance uses the standard Web-Mercator ground-resolution-per-pixel; each zoom's pool is
//! `simplify_topology` run ONCE over the shared arcs → seam-free at that zoom. See
//! `docs/superpowers/specs/2026-07-14-per-zoom-lod-design.md`.

use crate::vector::mvt::tile::{merc_m_per_px, DISPLAY_TILE_PX, WORLD_MERC_M};

/// One display-pixel's length in SOURCE-CRS units at zoom `z`. `merc_m_per_px(z)` is the mercator m/px
/// (shared with the MVT size gate); `area_scale` = mercator-m² per source-unit², so `sqrt` converts
/// merc→source length.
fn px_len_src(z: u32, area_scale: f64) -> f64 {
    let px_merc = merc_m_per_px(z);
    if area_scale > 0.0 {
        px_merc / area_scale.sqrt()
    } else {
        // Degenerate/unknown extent → fail OPEN (keep everything), matching the size-gate contract:
        // 0 makes the tolerance floor at the finest and the part-cull vanish, rather than treating a
        // mercator-metre length as source units (which fails CLOSED → blank map on a geographic CRS).
        0.0
    }
}

/// The Weighted-Visvalingam `min_area` (grid-area, matching `simplify_topology`) for zoom `z`: a
/// ~1-pixel tolerance in source units, floored at the operator's finest (`finest_min_area` =
/// `(--topology-simplify / snap)²`). Non-increasing in `z`; equals `finest_min_area` at high zoom.
pub fn topology_min_area_for_zoom(z: u32, snap: f64, area_scale: f64, finest_min_area: f64) -> f64 {
    const PX_BUDGET: f64 = 1.0;
    let finest_tol = finest_min_area.max(0.0).sqrt() * snap; // source-CRS length
    let tol = (PX_BUDGET * px_len_src(z, area_scale)).max(finest_tol);
    (tol / snap).powi(2).max(finest_min_area)
}

/// The per-zoom sub-pixel part-cull threshold (source-CRS units²): drop whole polygons whose exterior
/// is smaller than ~1 display pixel² at zoom `z`.
pub fn part_area_src_for_zoom(z: u32, area_scale: f64) -> f64 {
    let l = px_len_src(z, area_scale);
    l * l
}

use crate::vector::source::FeatureSource;
use crate::vector::topology::materialize::{
    materialize_culled, simplify_topology, TopologyFeatureSource,
};
use crate::vector::topology::{ArcLine, Topology};
use std::sync::Arc;

/// A set of per-zoom simplified coverages (one `Arc<dyn FeatureSource>` per zoom level, deduplicated
/// where consecutive zooms derive the same tolerance). Selection happens BEFORE `.features()`, so the
/// `FeatureSource` trait stays unchanged.
pub struct LodSet {
    per_zoom: Vec<Arc<dyn FeatureSource>>,
}

impl LodSet {
    /// The pool for tile zoom `z` (clamped to the built range).
    pub fn for_zoom(&self, z: u32) -> &Arc<dyn FeatureSource> {
        let i = (z as usize).min(self.per_zoom.len().saturating_sub(1));
        &self.per_zoom[i]
    }

    /// The pool for a WMS GetMap scale-denominator. `request_scale_denominator` computes
    /// `scale = res_grid · metres_per_unit / 0.00028`, so `scale · 0.00028` = ground metres per pixel;
    /// invert the Web-Mercator px formula to an effective zoom.
    pub fn for_scale_denominator(&self, scale: f64) -> &Arc<dyn FeatureSource> {
        let res_merc = scale * 0.00028; // ground metres / pixel
        let z = if res_merc > 0.0 {
            (WORLD_MERC_M / (DISPLAY_TILE_PX * res_merc))
                .log2()
                .round()
                .clamp(0.0, u32::MAX as f64) as u32
        } else {
            u32::MAX
        };
        self.for_zoom(z)
    }

    /// The finest (highest-detail) pool — a sensible default source for any path that ignores LOD.
    pub fn finest(&self) -> Arc<dyn FeatureSource> {
        self.per_zoom.last().expect("non-empty LodSet").clone()
    }
}

/// Build a per-zoom `LodSet` from a topology: for each `z` in `0..=max_zoom`, simplify the shared arcs
/// to that zoom's tolerance and materialize with sub-pixel part-culling.
///
/// The *simplification* is cached per distinct `min_area` (the expensive Weighted-Visvalingam + self-
/// intersection guard runs once per tolerance — the high-zoom tail all floors to the operator's
/// finest). The *part-cull* still varies every zoom (it shrinks monotonically with `z`), so each zoom
/// is materialised separately from the shared pool: reusing the whole coverage was a bug — the floored
/// tail then served the coarsest tail zoom's cull, so small junction-pinned cells were dropped and
/// never reappeared when zooming in. The finest zoom (`z == max_zoom`) is materialised with NO cull so
/// `finest()` is the lossless full-detail coverage (the GFI / ignore-LOD source).
pub fn build_lod(
    topo: &Topology,
    snap: f64,
    area_scale: f64,
    finest_min_area: f64,
    max_zoom: u32,
    finest_pool: Arc<Vec<ArcLine>>,
) -> LodSet {
    let mut per_zoom: Vec<Arc<dyn FeatureSource>> = Vec::with_capacity(max_zoom as usize + 1);
    for z in 0..=max_zoom {
        let ma = topology_min_area_for_zoom(z, snap, area_scale, finest_min_area);
        // Above the floor every zoom has a distinct tolerance (min_area quarters each zoom-out), so
        // each coarse pool is simplified fresh; the whole floored tail is `finest_min_area`, for which
        // the caller already built `finest_pool` (used for the un-LOD'd serve source) — reuse it here
        // instead of repeating the expensive Visvalingam + self-intersection guard.
        let pool = if ma == finest_min_area {
            finest_pool.clone()
        } else {
            Arc::new(simplify_topology(topo, ma))
        };
        let part_area = if z == max_zoom {
            0.0 // finest = lossless full-detail source (GFI / ignore-LOD)
        } else {
            part_area_src_for_zoom(z, area_scale)
        };
        let feats = materialize_culled(topo, &pool, snap, part_area);
        per_zoom.push(Arc::new(TopologyFeatureSource::new(feats)));
    }
    LodSet { per_zoom }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::feature::{Feature, Geometry, Props};

    #[test]
    fn min_area_is_monotonic_and_floored() {
        // snap 0.01, finest tolerance 25 m → finest_min_area = (25/0.01)^2 = 6.25e6.
        let (snap, area_scale, finest) = (0.01, 1.0, (25.0f64 / 0.01).powi(2));
        let a6 = topology_min_area_for_zoom(6, snap, area_scale, finest);
        let a10 = topology_min_area_for_zoom(10, snap, area_scale, finest);
        let a22 = topology_min_area_for_zoom(22, snap, area_scale, finest);
        assert!(a6 > a10 && a10 >= a22, "non-increasing: {a6} {a10} {a22}");
        assert!(
            (a22 - finest).abs() / finest < 1e-6,
            "high zoom floors at finest: {a22} vs {finest}"
        );
        assert!(a6 > finest * 10.0, "z6 much coarser than finest: {a6}");
        // area grows ~4x per zoom-out step (tolerance ~2x, area ~4x)
        let a7 = topology_min_area_for_zoom(7, snap, area_scale, finest);
        assert!((a6 / a7 - 4.0).abs() < 0.2, "≈4x per zoom-out: {}", a6 / a7);
    }

    #[test]
    fn part_area_positive_and_shrinks_with_zoom() {
        assert!(part_area_src_for_zoom(6, 1.0) > part_area_src_for_zoom(10, 1.0));
        assert!(part_area_src_for_zoom(14, 1.0) > 0.0);
    }

    fn small_topo() -> Topology {
        // a square with a mid-edge vertex so simplification actually reduces vertex count
        let ring = vec![
            [0.0, 0.0],
            [5.0, 0.0],
            [10.0, 0.0],
            [10.0, 10.0],
            [0.0, 10.0],
            [0.0, 0.0],
        ];
        let f = Feature::new(Geometry::Polygon(vec![ring]), Props::new(), 0);
        crate::vector::topology::build_topology(&[f], 1.0).0
    }

    fn verts(s: &Arc<dyn FeatureSource>) -> usize {
        s.features()
            .iter()
            .map(|f| match &f.geom {
                Geometry::Polygon(r) => r.iter().map(|x| x.len()).sum(),
                Geometry::MultiPolygon(p) => {
                    p.iter().flat_map(|poly| poly.iter().map(|r| r.len())).sum()
                }
                _ => 0,
            })
            .sum()
    }

    #[test]
    fn for_zoom_selects_and_clamps() {
        let mk = |n: f64| -> Arc<dyn FeatureSource> {
            let f = Feature::new(
                Geometry::Polygon(vec![vec![
                    [0.0, 0.0],
                    [n, 0.0],
                    [n, n],
                    [0.0, n],
                    [0.0, 0.0],
                ]]),
                Props::new(),
                0,
            );
            Arc::new(TopologyFeatureSource::new(vec![f]))
        };
        let set = LodSet {
            per_zoom: vec![mk(1.0), mk(2.0), mk(3.0)],
        };
        assert!(Arc::ptr_eq(set.for_zoom(0), &set.per_zoom[0]));
        assert!(Arc::ptr_eq(set.for_zoom(2), &set.per_zoom[2]));
        assert!(Arc::ptr_eq(set.for_zoom(9), &set.per_zoom[2])); // clamped
    }

    #[test]
    fn build_lod_shares_simplification_in_floored_tail_and_reduces_low_zoom() {
        let topo = small_topo();
        let fp = Arc::new(simplify_topology(&topo, 4.0));
        let set = build_lod(&topo, 1.0, 1.0, 4.0, 22, fp); // finest tol 2 → finest_min_area 4
        assert_eq!(
            verts(set.for_zoom(20)),
            verts(set.for_zoom(22)),
            "floored tail shares one simplification"
        );
        assert!(
            verts(set.for_zoom(4)) <= verts(set.for_zoom(22)),
            "low zoom no more detailed"
        );
    }

    fn rect(minx: f64, miny: f64, maxx: f64, maxy: f64, fid: u64) -> Feature {
        Feature::new(
            Geometry::Polygon(vec![vec![
                [minx, miny],
                [maxx, miny],
                [maxx, maxy],
                [minx, maxy],
                [minx, miny],
            ]]),
            Props::new(),
            fid,
        )
    }

    #[test]
    fn finest_zoom_is_lossless_and_low_zoom_part_culls() {
        // A 3-cell strip: Left | Mid | Right. Mid is a thin cell (area 100 000) whose four corners are
        // all degree-3 junctions (Left+Mid / Mid+Right meet there), so simplification CANNOT remove
        // them — Mid survives at full geometry regardless of tolerance, exactly like a small enclosed
        // parcel in a real land-cover coverage. finest_min_area 400 000 → tail floors ~z8, where
        // part_area(8)≈373 000 > Mid's 100 000, so the buggy dedup (reusing the coarse tail pool)
        // part-culled Mid at every high zoom → it never reappeared. The fix materialises each zoom with
        // its own part-cull (z=max lossless).
        let left = rect(0.0, 0.0, 100_000.0, 100_000.0, 0);
        let mid = rect(100_000.0, 0.0, 100_001.0, 100_000.0, 1); // area 100 000
        let right = rect(100_001.0, 0.0, 200_000.0, 100_000.0, 2);
        let topo = crate::vector::topology::build_topology(&[left, mid, right], 1.0).0;
        let finest = 400_000.0_f64;
        let fp = Arc::new(simplify_topology(&topo, finest));
        let set = build_lod(&topo, 1.0, 1.0, finest, 22, fp);

        let pool = simplify_topology(&topo, finest);
        let reference = materialize_culled(&topo, &pool, 1.0, 0.0); // cull=0 = lossless
        let nonempty = |fs: &[Feature]| fs.iter().filter(|f| f.geom.area() > 0.0).count();
        assert_eq!(
            nonempty(&reference),
            3,
            "Mid's junction-pinned corners survive simplification"
        );

        // FIX: the max-zoom pool is materialized with cull=0 → lossless (buggy dedup reused the coarse
        // tail pool, whose part-cull dropped Mid → it never reappeared when zooming in).
        assert_eq!(
            nonempty(set.for_zoom(22).features()),
            3,
            "max-zoom pool is lossless — the small junction-pinned cell is present"
        );
        // a coarse zoom part-culls the thin middle cell.
        assert_eq!(
            nonempty(set.for_zoom(6).features()),
            2,
            "coarse zoom part-culls the sub-pixel middle cell"
        );
    }

    #[test]
    fn for_scale_denominator_matches_equivalent_zoom() {
        let topo = small_topo();
        let fp = Arc::new(simplify_topology(&topo, 4.0));
        let set = build_lod(&topo, 1.0, 1.0, 4.0, 22, fp);
        let res_merc = WORLD_MERC_M / (2f64.powi(8) * DISPLAY_TILE_PX);
        let scale = res_merc / 0.00028; // the scale-denominator equivalent to z8
        assert!(Arc::ptr_eq(
            set.for_scale_denominator(scale),
            set.for_zoom(8)
        ));
    }
}
