// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! CRS transforms. We lean entirely on the system libproj (via the `proj` crate FFI) —
//! no bespoke projection/datum math. The transform maps output-CRS coordinates to the
//! source CRS (EPSG:3763). `new_known_crs` normalizes axis order for visualization, so
//! coordinates are always (x = easting/lon, y = northing/lat).

use proj::Proj;

/// Default source CRS (the cascais pilot data). Per-layer configs override this — the COG
/// itself carries the real CRS (e.g. the Sentinel-2 stack is EPSG:32629).
pub const SRC_CRS: &str = "EPSG:3763";

pub enum Transformer {
    Identity,
    Crs(Proj),
}

impl Transformer {
    /// Build a transform from `out_crs` to `src_crs` (the COG's CRS). `Identity` when they
    /// match. The source CRS is per-layer, not a global constant — different COGs live in
    /// different projections.
    pub fn new(out_crs: &str, src_crs: &str) -> Result<Transformer, String> {
        if crs_eq(out_crs, src_crs) {
            return Ok(Transformer::Identity);
        }
        match Proj::new_known_crs(out_crs, src_crs, None) {
            Ok(p) => Ok(Transformer::Crs(p)),
            Err(e) => Err(format!(
                "proj: cannot transform {out_crs} -> {src_crs}: {e}"
            )),
        }
    }

    /// Transform an output-CRS coordinate to the source CRS.
    #[inline]
    pub fn to_source(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        match self {
            Transformer::Identity => Some((x, y)),
            Transformer::Crs(p) => p.convert((x, y)).ok(),
        }
    }
}

fn crs_eq(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// CRSs the service supports as request CRS (must map to the source).
pub fn is_supported_crs(crs: &str) -> bool {
    let c = crs.trim().to_ascii_uppercase();
    matches!(
        c.as_str(),
        "EPSG:4326" | "EPSG:3857" | "EPSG:3763" | "CRS:84"
    )
}

/// Geographic (EPSG:4326) bounding box of a source-CRS extent, as `[west, south, east, north]`
/// in degrees. Thin wrapper over `crs_bounds` (dst = EPSG:4326) — advertises a layer's extent in
/// GetCapabilities so clients (QGIS) zoom to the data.
pub fn wgs84_bounds(src_crs: &str, minx: f64, miny: f64, maxx: f64, maxy: f64) -> Option<[f64; 4]> {
    crs_bounds(src_crs, "EPSG:4326", minx, miny, maxx, maxy)
}

/// Densified bounding box of a `src_crs` extent reprojected into `dst_crs`, as
/// `[minx, miny, maxx, maxy]` in `dst_crs` units. Samples all four edges (not just corners) so a
/// curved projection still bounds correctly. `None` if the transform is unavailable or nothing
/// projects finitely. Used for the TMS `<BoundingBox>` + the tile-intersection early-out (in the
/// grid CRS) and for GetCapabilities (WGS84, via the wrapper above).
pub fn crs_bounds(
    src_crs: &str,
    dst_crs: &str,
    minx: f64,
    miny: f64,
    maxx: f64,
    maxy: f64,
) -> Option<[f64; 4]> {
    if crs_eq(src_crs, dst_crs) {
        return Some([minx, miny, maxx, maxy]);
    }
    let to = Proj::new_known_crs(src_crs, dst_crs, None).ok()?;
    let (mut w, mut s, mut e, mut n) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    let steps = 16;
    let mut saw = false;
    // Sample a full grid (interior + edges), not just the four edges. A pole-centred projection
    // (UPS) has its singular point in the tile INTERIOR at low zoom — edge-only sampling misses it,
    // and PROJ returns finite-but-wrong inverses for the out-of-domain edges, yielding a footprint
    // that excludes the pole entirely (the polar-grid z0 MVT pre-filter then drops every feature —
    // 2026-07-13 review). Adding interior samples captures the pole; any garbage edge samples only
    // make the footprint over-inclusive, which is safe (extra features are clipped by the tile rect).
    for i in 0..=steps {
        let tx = i as f64 / steps as f64;
        let x = minx + tx * (maxx - minx);
        for j in 0..=steps {
            let ty = j as f64 / steps as f64;
            let y = miny + ty * (maxy - miny);
            if let Ok((lon, lat)) = to.convert((x, y)) {
                if lon.is_finite() && lat.is_finite() {
                    w = w.min(lon);
                    e = e.max(lon);
                    s = s.min(lat);
                    n = n.max(lat);
                    saw = true;
                }
            }
        }
    }
    if saw {
        Some([w, s, e, n])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crs_bounds_captures_an_interior_pole() {
        // UPS North (EPSG:5041): the pole (lat 90) sits at the false origin (2_000_000, 2_000_000) —
        // the INTERIOR of a symmetric extent, on no edge. Edge-only sampling missed it (the old bug
        // that emptied the polar-grid z0 MVT tile); the grid sampling must now reach latitude ~90.
        let b = crs_bounds(
            "EPSG:5041",
            "EPSG:4326",
            1_000_000.0,
            1_000_000.0,
            3_000_000.0,
            3_000_000.0,
        )
        .expect("polar bounds");
        assert!(
            b[3] > 89.5,
            "north bound {} must reach the pole (~90); edge-only sampling would miss it",
            b[3]
        );
    }

    #[test]
    fn identity_when_out_equals_src() {
        // A per-layer source CRS other than the 3763 default still yields Identity when the
        // request CRS matches it — proving src_crs is honored, not the old constant.
        let t = Transformer::new("EPSG:32629", "EPSG:32629").unwrap();
        assert!(matches!(t, Transformer::Identity));
        assert_eq!(
            t.to_source(600000.0, 4200000.0),
            Some((600000.0, 4200000.0))
        );
    }

    #[test]
    fn transforms_wgs84_into_utm29n() {
        // 4326 (lon,lat) -> 32629 (UTM 29N, metres). At the zone's central meridian (-9°),
        // easting is the false-easting 500000; lat 37.945° lands the northing near 4.20 Mm.
        let t = Transformer::new("EPSG:4326", "EPSG:32629").unwrap();
        let (x, y) = t.to_source(-9.0, 37.945).expect("transform failed");
        assert!((x - 500000.0).abs() < 1.0, "easting off: {x}");
        assert!((y - 4_199_712.7).abs() < 100.0, "northing off: {y}");
    }
}
