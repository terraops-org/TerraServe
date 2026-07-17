// SPDX-License-Identifier: AGPL-3.0-or-later
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
    fn query(&self, bbox: [f64; 4]) -> Vec<Feature>;
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
    /// today); Windowed returns only the R-tree window.
    pub fn features_in(&self, bbox: [f64; 4]) -> FeatureBatch<'_> {
        match self {
            VectorSource::LoadAll(s) => FeatureBatch::Borrowed(s.features()),
            VectorSource::Windowed(s) => FeatureBatch::Owned(s.query(bbox)),
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
        let batch = vs.features_in([-1e9, -1e9, 1e9, 1e9]);
        assert_eq!(batch.as_slice().len(), n);
        assert!(matches!(batch, FeatureBatch::Borrowed(_)));
    }
}
