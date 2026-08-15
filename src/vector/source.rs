// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! The reader seam. Every feature enters the pipeline through `FeatureSource`; the source
//! parses **once** at construction and holds its features + extent for the layer's lifetime.
//! This is the seam the native GPKG reader (the future default format) slots into unchanged.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::feature::Feature;

pub trait FeatureSource: Send + Sync {
    /// Borrow the parsed features — never a per-request clone (fatal at 56k-road scale, spec §10).
    fn features(&self) -> &[Feature];
    /// `[west, south, east, north]` in the source CRS — used for the default grid + capabilities.
    fn full_extent(&self) -> [f64; 4];
}

/// Lets an already-`Arc`'d load-all source be wrapped as `VectorSource::LoadAll` without copying
/// feature data — `Arc::clone` is a refcount bump, and this impl just delegates through the `Arc`'s
/// `Deref`. Used by call sites that only hold the source behind an `Arc` (e.g. `VectorLayer`'s
/// LOD-resolved source, `build_vector_layer`'s intermediate sources) and want to read through the
/// seam for a single call without disturbing how the source is shared elsewhere.
impl<T: FeatureSource + ?Sized> FeatureSource for Arc<T> {
    fn features(&self) -> &[Feature] {
        (**self).features()
    }
    fn full_extent(&self) -> [f64; 4] {
        (**self).full_extent()
    }
}

/// The windowed reader seam (parallel to `FeatureSource`, which stays load-all-only): a source
/// that can answer "just the features overlapping this bbox" without holding the whole dataset in
/// memory. The future FlatGeoBuf reader is the first real implementer (packed Hilbert R-tree
/// traversal); `FeatureSource` and every existing reader are untouched by this trait's existence.
pub trait WindowedSource: Send + Sync {
    /// Features overlapping `bbox` (source CRS), read from just that window.
    ///
    /// **`Err` means the read FAILED; it never means "no features here".** That distinction is the
    /// whole reason this returns a `Result`. Every windowed reader used to answer a failed read
    /// with an empty `Vec` -- PostGIS logged a line, GeoPackage swallowed six different SQLite
    /// errors, and FlatGeoBuf returned silently -- so a dropped connection, a truncated S3 range
    /// read or a statement timeout arrived at the client as a blank map behind a **200 OK**. Worse,
    /// `build-pmtiles` froze that blank into an archive, where it then shadows the live path
    /// forever: two EU5 bakes lost 10 and 4 tiles that way, and the archive cannot tell you which.
    ///
    /// Every front-end above this seam already maps an error to a 500 / OWS exception, so
    /// propagating is all that was ever missing. Return `Ok(vec![])` ONLY for a genuinely empty
    /// window.
    fn query(&self, bbox: [f64; 4]) -> Result<Vec<Feature>, String>;

    /// The same window, but allowed to skip features whose source-CRS area is below
    /// `min_area_src` — the per-zoom threshold from `mvt::min_area_src_for_zoom` (raster callers
    /// go through `mvt::min_area_src_for_scale`, which is the same number derived from a
    /// resolution), in source-CRS units². This is the seam that lets a source push the
    /// min-feature-size gate down to where the data lives instead of fetching, transferring and
    /// decoding rows the caller is about to discard (`PostgisSource` puts it in the SQL `WHERE`;
    /// measured on a 17.8M-feature layer, the low-zoom tile is dominated by exactly those rows,
    /// and on a 107.9M-feature one an ungated z1 raster tile cost 52.6 GiB against 0.11 gated).
    ///
    /// **Implementing this is optional and skipping is always optional.** The default ignores the
    /// threshold entirely, so every existing reader keeps its behaviour with no edit — a source
    /// with no cheap area to test (FlatGeoBuf and GeoPackage know a bbox from their index, not an
    /// area) is right to fall through here rather than decode geometry just to measure it.
    ///
    /// **Contract for an override: it may only drop what the Rust gate would drop anyway** —
    /// `f.area > 0.0 && f.area < min_area_src`, i.e. sub-threshold POLYGONS only, never a line and
    /// never a point (both have zero area). That containment is what makes this a pure
    /// optimization: the caller re-applies the identical test (`encode_tile_opt` on the MVT path,
    /// `render::skip_below_min_area` on the raster one), so a gated read and an ungated one must
    /// produce the same tile. `min_area_src <= 0.0` means the gate is off and MUST behave
    /// exactly like `query`.
    fn query_gated(&self, bbox: [f64; 4], min_area_src: f64) -> Result<Vec<Feature>, String> {
        let _ = min_area_src;
        self.query(bbox)
    }

    fn full_extent(&self) -> [f64; 4];
    fn crs(&self) -> Option<&str>;

    /// Attribute (property) field schema derived from cheap source metadata alone — NEVER a
    /// feature read/decode. Same shape as `mvt_http::feature_field_schema`'s return: field name
    /// -> `"String"` | `"Number"`. This is the seam that keeps layer setup for a large windowed
    /// source (e.g. a multi-million-feature `.fgb`) from decoding the whole dataset just to list
    /// field names/types (see `FgbSource`'s override, which reads the FlatGeoBuf Header's
    /// already-parsed `columns()`). Default returns empty — safe for a future `WindowedSource`
    /// impl with no cheap metadata schema to expose; such a caller would need its own
    /// (documented) fallback rather than silently paying for a full scan here.
    fn field_schema(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }
}

/// What a vector layer holds — the load-all vs windowed dispatch point. Both variants are
/// `Arc`-backed (not `Box`) so `VectorSource` itself is cheaply `Clone`: the windowed-seam
/// migration (Task 1b) needs `VectorLayer::source_for_zoom`/`source_for_scale` to hand back an
/// owned `VectorSource` per request (LOD picks a different per-zoom pool each time), and an
/// `Arc::clone` is a refcount bump — never a data copy — for either variant.
#[derive(Clone)]
pub enum VectorSource {
    LoadAll(Arc<dyn FeatureSource>),
    Windowed(Arc<dyn WindowedSource>),
}

/// A borrowed whole slice (load-all, no alloc) or an owned windowed batch.
pub enum FeatureBatch<'a> {
    Borrowed(&'a [Feature]),
    Owned(Vec<Feature>),
}
impl FeatureBatch<'_> {
    pub fn as_slice(&self) -> &[Feature] {
        match self {
            FeatureBatch::Borrowed(s) => s,
            FeatureBatch::Owned(v) => v,
        }
    }
    pub fn len(&self) -> usize {
        self.as_slice().len()
    }
    pub fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }
}

impl VectorSource {
    pub fn full_extent(&self) -> [f64; 4] {
        match self {
            VectorSource::LoadAll(s) => s.full_extent(),
            VectorSource::Windowed(s) => s.full_extent(),
        }
    }
    pub fn crs(&self) -> Option<&str> {
        match self {
            VectorSource::LoadAll(_) => None, // load-all CRS is threaded elsewhere today; keep as-is
            VectorSource::Windowed(s) => s.crs(),
        }
    }
    /// Features to consider for `bbox`. LoadAll borrows its whole slice (caller filters/clips as
    /// today); Windowed returns only the R-tree window. `Err` is a failed READ, never an empty
    /// window -- see [`WindowedSource::query`] for why that distinction is load-bearing.
    pub fn features_in(&self, bbox: [f64; 4]) -> Result<FeatureBatch<'_>, String> {
        self.features_in_gated(bbox, 0.0)
    }

    /// `features_in`, with the per-zoom min-feature-size threshold offered to the source (see
    /// [`WindowedSource::query_gated`]). `0.0` = no gate, and is what `features_in` passes.
    ///
    /// **A caller may pass a non-zero threshold only if it applies the matching Rust gate itself**,
    /// because a source is free to ignore the pushdown (the default `query_gated` does) — without
    /// the caller's own filter the layer would thin on one backend and not on another. Two callers
    /// qualify: the MVT path (`encode_tile_opt`) and, since 2026-08-14, the raster path
    /// (`render_vector_from` -> `render::skip_below_min_area`, which covers WMS GetMap, WMTS/TMS
    /// raster tiles and the PNG PMTiles bake). **GetFeatureInfo passes no threshold** and goes
    /// through `features_in`: the gate decides what is worth DRAWING, and clicking a sub-pixel
    /// parcel to read its attributes is legitimate at any zoom. `LoadAll` is unaffected either way:
    /// it borrows its whole slice and the caller filters it.
    pub fn features_in_gated(
        &self,
        bbox: [f64; 4],
        min_area_src: f64,
    ) -> Result<FeatureBatch<'_>, String> {
        match self {
            // A load-all source parsed at construction and cannot fail here -- the file was either
            // read at startup or the layer never came up. Always `Ok`.
            VectorSource::LoadAll(s) => Ok(FeatureBatch::Borrowed(s.features())),
            VectorSource::Windowed(s) => {
                Ok(FeatureBatch::Owned(s.query_gated(bbox, min_area_src)?))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINY_GEOJSON: &str = r#"{
        "type": "FeatureCollection",
        "features": [
            {"type": "Feature", "geometry": {"type": "Point", "coordinates": [1.0, 2.0]}, "properties": {"name": "a"}},
            {"type": "Feature", "geometry": {"type": "Point", "coordinates": [3.0, 4.0]}, "properties": {"name": "b"}}
        ]
    }"#;

    #[test]
    fn vectorsource_loadall_borrows_and_reports_extent() {
        // A GeoJsonSource (load-all) wrapped as VectorSource::LoadAll returns its whole slice
        // (Borrowed, no alloc) and its extent — behavior identical to calling features() directly.
        let gj = super::super::geojson::GeoJsonSource::from_str(TINY_GEOJSON).unwrap();
        let n = gj.features().len();
        let vs = VectorSource::LoadAll(Arc::new(gj));
        let batch = vs
            .features_in([-1e9, -1e9, 1e9, 1e9])
            .expect("LoadAll never fails");
        assert_eq!(batch.as_slice().len(), n);
        assert!(matches!(batch, FeatureBatch::Borrowed(_)));
    }
}
