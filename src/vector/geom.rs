// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Forward projection: a feature coordinate (source CRS) → the output pixel plane.
//!
//! Reuses the raster path's `reproj::Transformer`. Note the direction: `Transformer::new(A,B)`'s
//! `to_source` maps `A → B`, so to map feature(src) → grid we construct `new(src_crs, grid_crs)`
//! (the arg order flips the raster path's out→src into our src→grid).

use crate::reproj::Transformer;

/// The `out_crs` bounding box reprojected into `src_crs` and expanded by ~1/16 (a tile/render
/// overscan), for a cheap per-feature bbox **pre-filter** that rejects features outside the request
/// footprint BEFORE the expensive per-vertex projection. `None` if the transform is unavailable —
/// the caller then skips the filter (correct, just slower). Shared by the MVT tile assembler
/// (`vector::mvt::tile`) and the WMS vector renderer (`vector::render`) — the O(all-features) →
/// O(in-view) win in both paths.
pub fn source_filter_bbox(out_crs: &str, src_crs: &str, bbox: [f64; 4]) -> Option<[f64; 4]> {
    let b = crate::reproj::crs_bounds(out_crs, src_crs, bbox[0], bbox[1], bbox[2], bbox[3])?;
    let dx = (b[2] - b[0]) / 16.0;
    let dy = (b[3] - b[1]) / 16.0;
    Some([b[0] - dx, b[1] - dy, b[2] + dx, b[3] + dy])
}

/// Axis-aligned overlap of two bboxes `[minx, miny, maxx, maxy]`. Touching edges count as overlap;
/// an inverted (empty) bbox — `min > max`, e.g. a vertex-less `Feature::bbox` — never overlaps. Fed
/// the feature's PRECOMPUTED `Feature::bbox` (computed once at load), so the pre-filter is O(1)/feature
/// instead of re-walking every vertex per request.
pub fn bbox_overlaps(a: [f64; 4], b: [f64; 4]) -> bool {
    !(a[2] < b[0] || a[0] > b[2] || a[3] < b[1] || a[1] > b[3])
}

pub struct Projector {
    t: Transformer,
    minx: f64,
    maxy: f64,
    dx: f64,
    dy: f64,
}

impl Projector {
    pub fn new(
        src_crs: &str,
        grid_crs: &str,
        bbox: [f64; 4],
        width: u32,
        height: u32,
    ) -> Result<Projector, String> {
        let t = Transformer::new(src_crs, grid_crs)?; // to_source now maps src -> grid
        let [minx, miny, maxx, maxy] = bbox;
        let dx = (maxx - minx) / width as f64;
        let dy = (maxy - miny) / height as f64;
        if dx <= 0.0 || dy <= 0.0 {
            return Err("degenerate bbox/size".into());
        }
        Ok(Projector {
            t,
            minx,
            maxy,
            dx,
            dy,
        })
    }

    /// Map (lon, lat) in the source CRS to an output pixel (x right, y down). `None` if the
    /// transform fails, OR if it "succeeds" but yields a non-finite coordinate — the case an
    /// INTERRUPTED projection (e.g. `+proj=igh`, Goode Homolosine) can produce for a vertex that
    /// falls in a lobe gap: libproj doesn't always set an error code for a degenerate result, so the
    /// finiteness has to be checked explicitly here rather than relying on `to_source`'s `Option`
    /// alone. Without this, a NaN/inf `f32` silently saturates to `0` on the later cast to `i32`
    /// (Rust's float→int cast is saturating, not panicking) — a garbage in-bounds tile coordinate,
    /// not an obviously-wrong one. On-canvas checks are the caller's job (viewport-global culling).
    pub fn to_pixel(&self, lon: f64, lat: f64) -> Option<(f32, f32)> {
        let (gx, gy) = self.t.to_source(lon, lat)?;
        let px = ((gx - self.minx) / self.dx) as f32;
        let py = ((self.maxy - gy) / self.dy) as f32;
        if !px.is_finite() || !py.is_finite() {
            return None;
        }
        Some((px, py))
    }
}

#[cfg(test)]
mod tests {
    use super::Projector;

    /// A vertex whose transform "succeeds" (no PROJ error — e.g. `to_source` returns a value at all)
    /// but whose numeric result is non-finite must be DROPPED (`None`), never cast to a garbage
    /// integer downstream. This is exactly what an interrupted projection (`+proj=igh`) can hand back
    /// for a vertex that falls in a lobe gap. A NaN/inf INPUT through the identity transformer (no
    /// libproj involved — src_crs == grid_crs) reaches the identical `to_pixel` arithmetic a real
    /// non-finite `to_source` output would, so this isolates the exact guard without depending on
    /// finding a live PROJ edge case (verified empirically: this system's libproj 9.6.0 does not
    /// produce a non-finite `+proj=igh` forward transform anywhere on a 1° world grid).
    #[test]
    fn nan_safe_to_pixel_drops_non_finite_result() {
        let proj =
            Projector::new("EPSG:3857", "EPSG:3857", [0.0, 0.0, 100.0, 100.0], 256, 256).unwrap();
        assert!(
            proj.to_pixel(f64::NAN, 50.0).is_none(),
            "non-finite x must be dropped, not cast to a garbage int"
        );
        assert!(
            proj.to_pixel(50.0, f64::NAN).is_none(),
            "non-finite y must be dropped"
        );
        assert!(
            proj.to_pixel(f64::INFINITY, 50.0).is_none(),
            "infinite x must be dropped"
        );
        assert!(
            proj.to_pixel(50.0, f64::NEG_INFINITY).is_none(),
            "infinite y must be dropped"
        );
        // Sanity: a normal finite vertex still round-trips (no regression on the default path).
        assert!(proj.to_pixel(50.0, 50.0).is_some());
    }
}
