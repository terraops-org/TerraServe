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

/// Export the PROJ.4 string for any CRS libproj can resolve — an EPSG code, WKT, or a
/// `+proj=` string. `None` if PROJ can't build the object or can't render it as PROJ.4
/// (never panics). Used by the built-in viewer to register a CRS with proj4js on the fly:
/// the 4-entry hardcoded polar table in `tms_http.rs::proj4_def` only covers the CRSs the
/// polar `/viewer` ships with, and can't produce e.g. LV95 (EPSG:2056) or Homolosine.
///
/// `proj` (the safe wrapper crate) doesn't expose `proj_create`/`proj_as_proj_string`, so
/// this drops to the raw `proj-sys` FFI directly — pinned to the exact version `proj`
/// already pulls transitively (see Cargo.toml), so it's the same libproj build, not a
/// second one.
pub fn crs_to_proj4(crs: &str) -> Option<String> {
    use proj_sys::{
        proj_as_proj_string, proj_context_create, proj_context_destroy, proj_create,
        proj_crs_create_bound_crs_to_WGS84, proj_destroy, PJ_PROJ_STRING_TYPE_PJ_PROJ_4,
    };
    use std::ffi::{CStr, CString};

    // The CString must outlive the `proj_create` call — it's built and held here, not
    // dropped before use.
    let c_crs = CString::new(crs).ok()?;

    // SAFETY: every FFI pointer is null-checked before use. `ctx` and `pj` are freed on
    // EVERY exit path (success, a null `pj`, or a null/unreadable proj-string result) — no
    // early return skips their destroy call, so a garbage CRS can never leak either. The
    // string PROJ returns from `proj_as_proj_string` is owned by `pj` and only valid until
    // `pj` is destroyed (or the context is reused), so it is copied into an owned `String`
    // strictly before `proj_destroy` runs.
    unsafe {
        let ctx = proj_context_create();
        if ctx.is_null() {
            return None;
        }
        let pj = proj_create(ctx, c_crs.as_ptr());
        if pj.is_null() {
            proj_context_destroy(ctx);
            return None;
        }
        // PROJ 6+ models the datum relationship as a SEPARATE coordinate operation, and the
        // legacy PROJ.4 string of a *CRS* has nowhere to put it, so exporting the CRS directly
        // silently DROPS `+towgs84`. For EPSG:2056 (Swiss LV95, Bessel ellipsoid) that is a
        // **164 m** error in the definition handed to proj4js by `/viewer` and published as the
        // `"proj4"` field of `GET /tileMatrixSets/{id}` (measured 2026-07-27).
        //
        // A **BoundCRS** is PROJ's own answer to this: it pairs the CRS with its hub
        // transformation, and its legacy export therefore carries the Helmert parameters. This
        // is the same mechanism GDAL's `exportToProj4` relies on. Fall back to the bare CRS when
        // no bound form exists (already WGS84-based, or a grid-shift datum that genuinely cannot
        // be written as `+towgs84`).
        let bound = proj_crs_create_bound_crs_to_WGS84(ctx, pj, std::ptr::null());
        let export = if bound.is_null() { pj } else { bound };

        let raw = proj_as_proj_string(ctx, export, PJ_PROJ_STRING_TYPE_PJ_PROJ_4, std::ptr::null());
        let result = if raw.is_null() {
            None
        } else {
            CStr::from_ptr(raw).to_str().ok().map(|s| s.to_owned())
        };
        if !bound.is_null() {
            proj_destroy(bound);
        }
        proj_destroy(pj);
        proj_context_destroy(ctx);
        result
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

    #[test]
    fn crs_to_proj4_lv95_is_swiss_oblique_mercator() {
        // EPSG:2056 (CH1903+ / LV95) is a Swiss oblique Mercator — PROJ's `somerc`. This is
        // the CRS the hardcoded 4-entry polar table in tms_http.rs::proj4_def can't produce,
        // which is why the LV95 viewer can't register it with proj4js today.
        let s = crs_to_proj4("EPSG:2056").expect("PROJ must resolve EPSG:2056");
        assert!(s.contains("+proj=somerc"), "unexpected proj4 string: {s}");
    }

    #[test]
    fn crs_to_proj4_wgs84_is_longlat() {
        let s = crs_to_proj4("EPSG:4326").expect("PROJ must resolve EPSG:4326");
        assert!(s.contains("+proj=longlat"), "unexpected proj4 string: {s}");
    }

    #[test]
    fn crs_to_proj4_garbage_crs_is_none() {
        assert_eq!(crs_to_proj4("EPSG:not-a-real-crs-99999999"), None);
        assert_eq!(crs_to_proj4(""), None);
    }

    #[test]
    fn crs_to_proj4_keeps_the_datum_shift() {
        // PROJ's legacy proj4 export of a CRS DROPS `+towgs84`, because PROJ 6+ holds the datum
        // relationship as a separate operation. For EPSG:2056 (Bessel ellipsoid) that silently
        // costs 164 m in the definition handed to proj4js by `/viewer` and published as the
        // `"proj4"` field of `GET /tileMatrixSets/{id}`. Measured 2026-07-27.
        let s = crs_to_proj4("EPSG:2056").expect("PROJ must resolve EPSG:2056");
        assert!(s.contains("+proj=somerc"), "unexpected projection: {s}");
        assert!(
            s.contains("towgs84"),
            "EPSG:2056 lost its datum shift, this is the 164 m bug: {s}"
        );
        // The Swiss shift is ~674 m in X; assert the real value, not merely that a key exists,
        // so a zeroed or truncated clause still fails.
        assert!(
            s.contains("+towgs84=674.374"),
            "wrong CH1903+ -> WGS84 parameters: {s}"
        );
    }

    #[test]
    fn crs_to_proj4_introduces_no_shift_for_wgs84_based_crs() {
        // The BoundCRS export must not invent a shift where none exists. These are WGS84-based,
        // so any datum clause must be ALL ZEROS (PROJ states it explicitly for EPSG:3763, and
        // omits it entirely for the `+datum=WGS84` ones) — both mean "no shift". Asserting the
        // semantic property rather than string equality, because making a zero shift explicit is
        // a correctness improvement, not a regression.
        for crs in ["EPSG:4326", "EPSG:32629", "EPSG:3763", "EPSG:5041"] {
            let s = crs_to_proj4(crs).unwrap_or_else(|| panic!("PROJ must resolve {crs}"));
            if let Some(rest) = s.split("+towgs84=").nth(1) {
                let params = rest.split_whitespace().next().unwrap_or("");
                assert!(
                    params
                        .split(',')
                        .all(|v| v.trim().parse::<f64>() == Ok(0.0)),
                    "{crs} must not gain a non-zero datum shift: {s}"
                );
            }
        }
    }
}
