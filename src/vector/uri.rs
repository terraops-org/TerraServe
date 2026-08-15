// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! One place that decides what a vector source spec IS.
//!
//! The format decision used to be five `path.ends_with(".fgb")`-style string tests scattered
//! across two functions in `lib.rs`. That worked while every source was a file, and stops
//! working the moment one is not: a database source has no extension to sniff, so there is
//! nowhere for `postgis://…` to dispatch.
//!
//! ## Transport scheme vs format scheme
//!
//! The distinction that makes this work, and the reason a naive "match on the URI scheme"
//! registry would be wrong:
//!
//! - `s3://bucket/roads.fgb` — `s3` is a **transport**. It says where the bytes live, not what
//!   they are. The format is still FlatGeoBuf, and the reader is the same one a local `.fgb`
//!   uses, because every byte already flows through `RangeSource`.
//! - `postgis://host/db?table=roads` — `postgis` is a **format**. There is no file, no
//!   extension, and the reader is completely different.
//!
//! So: strip a known transport scheme, then classify what remains. An unknown scheme is
//! treated as a format, which is exactly the seam a new source type needs.

/// What kind of reader a vector source spec needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// FlatGeoBuf. Windowed (packed Hilbert R-tree); local path or `s3://`.
    FlatGeoBuf,
    /// GeoPackage. Load-all by default, windowed when the OGC rtree is present and no
    /// load-all-only transform was requested.
    GeoPackage,
    /// GeoJSON. Load-all; no spatial index.
    GeoJson,
    /// PostGIS. Windowed by definition — every query is a bbox filter against a GiST index.
    PostGis,
    /// A scheme this build does not know how to open. Carried rather than rejected here so the
    /// CALLER produces the error, with a message naming what it does support. This is the seam a
    /// new source type attaches to: adding one means adding a variant and a match arm, not
    /// another `ends_with` somewhere — `postgis://` is the first example, above.
    Unsupported(String),
}

/// Transport schemes: they say where bytes live, not what they are. Stripped before the format
/// is classified. `file://` is accepted for symmetry even though a bare path is the norm.
const TRANSPORT_SCHEMES: [&str; 2] = ["s3", "file"];

/// Split `scheme://rest` into `(scheme, rest)`. `None` when there is no scheme.
///
/// Deliberately not a general URI parser: a Windows path (`C:\data\x.fgb`) has a colon but no
/// `://`, and must not be mistaken for a scheme.
fn split_scheme(spec: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = spec.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
    {
        return None;
    }
    Some((scheme, rest))
}

/// Classify a vector source spec.
///
/// Extension matching is case-insensitive: `ROADS.FGB` off a Windows or S3 export is the same
/// format as `roads.fgb`, and the old `ends_with(".fgb")` silently classified it as GeoPackage
/// (the fallback arm), which then failed deep inside the SQLite opener with a confusing error.
/// Any query string is stripped first, so `s3://b/roads.fgb?versionId=…` still classifies.
pub fn classify(spec: &str) -> SourceKind {
    let after_transport = match split_scheme(spec) {
        // A known transport: keep going with the path part, the format is still in the name.
        Some((scheme, rest))
            if TRANSPORT_SCHEMES.contains(&scheme.to_ascii_lowercase().as_str()) =>
        {
            rest
        }
        // `postgis` is a format claim like any other unknown scheme, just one this build DOES
        // know how to open — checked before the generic fallback below.
        Some((scheme, _)) if scheme.eq_ignore_ascii_case("postgis") => return SourceKind::PostGis,
        // An unknown scheme IS the format claim, carried out to the caller.
        Some((scheme, _)) => return SourceKind::Unsupported(scheme.to_ascii_lowercase()),
        None => spec,
    };
    let path = after_transport
        .split(['?', '#'])
        .next()
        .unwrap_or(after_transport)
        .to_ascii_lowercase();

    if path.ends_with(".fgb") {
        SourceKind::FlatGeoBuf
    } else if path.ends_with(".geojson") || path.ends_with(".json") {
        SourceKind::GeoJson
    } else {
        // GeoPackage is the historical default for an unrecognised extension, and staying the
        // default keeps every existing invocation working. `.gpkg` is matched explicitly above
        // this comment only in spirit: anything unknown still reaches the GeoPackage opener,
        // which produces its own error if the file is not one.
        SourceKind::GeoPackage
    }
}

#[cfg(test)]
mod tests {
    use super::{classify, SourceKind};

    #[test]
    fn plain_paths_classify_by_extension() {
        assert_eq!(classify("roads.fgb"), SourceKind::FlatGeoBuf);
        assert_eq!(classify("/data/cos2023v1.fgb"), SourceKind::FlatGeoBuf);
        assert_eq!(classify("airports.geojson"), SourceKind::GeoJson);
        assert_eq!(classify("layers.json"), SourceKind::GeoJson);
        assert_eq!(classify("/data/bupi.gpkg"), SourceKind::GeoPackage);
    }

    /// The transport/format split: `s3://` says WHERE, the extension says WHAT. A `.fgb` in a
    /// bucket is read by the same FlatGeoBuf reader as one on disk.
    #[test]
    fn s3_is_a_transport_not_a_format() {
        assert_eq!(classify("s3://bucket/roads.fgb"), SourceKind::FlatGeoBuf);
        assert_eq!(classify("s3://bucket/a/b/c.geojson"), SourceKind::GeoJson);
        assert_eq!(classify("file:///data/x.gpkg"), SourceKind::GeoPackage);
        assert_eq!(classify("S3://bucket/roads.fgb"), SourceKind::FlatGeoBuf);
    }

    /// An unknown scheme is a FORMAT claim, and is carried out to the caller rather than being
    /// silently misread as a GeoPackage path. `postgres` (not `postgis`) is used here as the
    /// still-unsupported example: `postgis` itself now classifies as `SourceKind::PostGis`,
    /// covered separately by `postgis_scheme_classifies_as_postgis_not_unsupported`.
    #[test]
    fn unknown_scheme_is_reported_as_a_format_not_guessed_as_a_file() {
        assert_eq!(
            classify("POSTGRES://host/db"),
            SourceKind::Unsupported("postgres".into())
        );
    }

    /// Regression: the old `ends_with(".fgb")` was case-SENSITIVE, so an uppercase export fell
    /// through to the GeoPackage arm and failed inside the SQLite opener with an error naming
    /// the wrong format entirely.
    #[test]
    fn extension_matching_is_case_insensitive() {
        assert_eq!(classify("ROADS.FGB"), SourceKind::FlatGeoBuf);
        assert_eq!(classify("Airports.GeoJSON"), SourceKind::GeoJson);
    }

    /// A query string must not defeat the extension match.
    #[test]
    fn query_and_fragment_are_stripped_before_matching() {
        assert_eq!(
            classify("s3://b/roads.fgb?versionId=abc123"),
            SourceKind::FlatGeoBuf
        );
        assert_eq!(classify("/data/x.geojson#frag"), SourceKind::GeoJson);
    }

    /// A Windows path has a colon but no `://`, and must not be read as a scheme.
    #[test]
    fn a_windows_path_is_not_a_scheme() {
        assert_eq!(classify(r"C:\data\roads.fgb"), SourceKind::FlatGeoBuf);
    }

    /// Unknown EXTENSIONS keep falling through to GeoPackage, which is the historical default
    /// and what keeps every existing invocation working. Only unknown SCHEMES are rejected.
    #[test]
    fn unknown_extension_still_defaults_to_geopackage() {
        assert_eq!(classify("/data/mystery"), SourceKind::GeoPackage);
        assert_eq!(classify("/data/thing.sqlite"), SourceKind::GeoPackage);
    }

    #[test]
    fn postgis_scheme_classifies_as_postgis_not_unsupported() {
        assert!(matches!(
            classify("postgis://ts:${P}@db/gis/public.parcels"),
            SourceKind::PostGis
        ));
    }

    #[test]
    fn an_unknown_scheme_is_still_unsupported() {
        assert!(matches!(
            classify("mysql://x/y"),
            SourceKind::Unsupported(_)
        ));
    }
}
