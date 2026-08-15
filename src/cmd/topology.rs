// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! The `build-topology` subcommand: load a vector coverage, build its shared-arc topology, print
//! a report. Diagnostic only — no tiles are produced and nothing is stored or served.
//!
//! It exists to answer "is this coverage clean enough to dissolve?" before you spend a bake on
//! it: the report's arc and ring counts are what tell you whether shared boundaries were actually
//! detected or whether the source has slivers and near-duplicate vertices that a tolerance sweep
//! would have to absorb.

use crate::s3;
use crate::vector;
use crate::Error;
use clap::Args;

/// `build-topology` arguments — a diagnostic subcommand (SP1 task 6): load a vector coverage,
/// build the shared-arc topology, print a report. No tiles, no storage, no serving.
#[derive(Args)]
pub struct BuildTopologyArgs {
    /// Path to the vector coverage (.gpkg, .fgb or .geojson) to build shared-arc topology from.
    #[arg(long)]
    pub vector: String,
    /// Optional layer name (GeoPackage only; ignored for a single-layer .fgb or .geojson); default =
    /// the source's auto-detected layer.
    #[arg(long)]
    pub layer: Option<String>,
    /// Snap tolerance in source-CRS units. Fine default leaves a clean coverage untouched.
    #[arg(long, default_value_t = 0.01)]
    pub snap_tolerance: f64,
    /// Run the round-trip oracle (`Topology::verify_roundtrip`) after building and print the
    /// mismatch count — 0 = perfect round-trip. Makes the spec's primary correctness oracle
    /// runnable on real data, not just fixtures.
    #[arg(long)]
    pub verify: bool,
}

/// Load a coverage for `build-topology` as a `VectorSource`. A `.gpkg` is read whole (`LoadAll`);
/// a `.fgb` opens the windowed reader (`Windowed`) and the caller reads it across its full extent.
fn load_topology_source(
    path: &str,
    layer: Option<&str>,
) -> Result<vector::source::VectorSource, Error> {
    let kind = vector::uri::classify(path);
    if let vector::uri::SourceKind::Unsupported(scheme) = &kind {
        // NOT echoing `path`: any `scheme://user:pass@host/...`-shaped spec can carry a real,
        // literal, un-`${VAR}`'d password in its authority. `postgres://`/`postgresql://` for a
        // `postgis://` layer is exactly the mistake `pg_uri.rs`'s module doc calls out -- and a
        // typo like that lands HERE, never reaching `parse_postgis_uri`'s own protection, because
        // `postgres`/`postgresql` do not classify as `SourceKind::PostGis`. The scheme name alone
        // (already captured above) is enough to spot the typo without risking the rest of it.
        return Err(format!(
            "unsupported vector source scheme `{scheme}://`. Supported: a local path or \
             `s3://` pointing at .fgb, .gpkg or .geojson, or a `postgis://` connection URI."
        )
        .into());
    }
    // `postgis://` is a recognised format and `layer/mod.rs` can build one (Task 6), but
    // `build-topology` cannot: it has no `--extent` flag, and a `postgis://` layer requires an
    // explicit, operator-declared extent that TerraServe deliberately never derives from the
    // database (`ST_EstimatedExtent` is NULL pre-`ANALYZE`, `ST_Extent` is a full-table scan).
    // This is a permanent scope boundary, not a stale "not wired up yet": there is no `--extent`
    // flag this CLI could grow that would make the missing piece go away. Reject explicitly,
    // naming the reason, rather than falling through to the GeoPackage opener below, which would
    // try to open the connection string as a SQLite file and fail with a confusing error naming
    // the wrong format entirely.
    if kind == vector::uri::SourceKind::PostGis {
        // NOT echoing `path` either: this branch never calls `parse_postgis_uri` (build-topology
        // never constructs a `PostgisSource` at all), so nothing on this path has ever checked
        // whether the operator used `${VAR}` -- a literal password would otherwise round-trip
        // straight into this error with no validation in between.
        return Err(format!(
            "unsupported vector source scheme `postgis://`: build-topology does not support \
             PostGIS sources (it has no --extent flag, and a postgis:// layer requires one). \
             Supported: a local path or `s3://` pointing at .fgb, .gpkg or .geojson."
        )
        .into());
    }
    if kind == vector::uri::SourceKind::FlatGeoBuf {
        if layer.is_some() {
            eprintln!(
                "warning: --layer is ignored for a FlatGeoBuf source ({path}); it has a single layer"
            );
        }
        // `.fgb` opens through `s3::AnySource` (local path or `s3://`), exactly as the serve path
        // does, then wraps the windowed `FgbSource` as `VectorSource::Windowed`. The caller reads it
        // across its full extent, so the whole coverage feeds the topology build.
        let s3 = s3::S3Config::from_env();
        let range = s3::AnySource::open(path, &s3).map_err(|e| format!("open {path}: {e}"))?;
        let fgb = vector::fgb::FgbSource::open(range).map_err(|e| format!("fgb {path}: {e}"))?;
        return Ok(vector::source::VectorSource::Windowed(std::sync::Arc::new(
            fgb,
        )));
    }
    if kind == vector::uri::SourceKind::GeoJson {
        if layer.is_some() {
            eprintln!("warning: --layer is ignored for a GeoJSON source ({path})");
        }
        // GeoJSON is a whole-file read (no spatial index), so LoadAll like a `.gpkg`.
        let src = vector::geojson::GeoJsonSource::load(path)?;
        return Ok(vector::source::VectorSource::LoadAll(std::sync::Arc::new(
            src,
        )));
    }
    let src = vector::gpkg::GpkgSource::load(path, layer)?;
    Ok(vector::source::VectorSource::LoadAll(std::sync::Arc::new(
        src,
    )))
}

/// Load a vector coverage, build its shared-arc topology, and print the diagnostic report.
/// No tiles, no storage, no serving — SP1 is unwired to serving by design.
pub fn run_build_topology(args: &BuildTopologyArgs) -> Result<(), Error> {
    // `Error` is `Box<dyn std::error::Error>`, which has `From<String>`, so `?` converts both the
    // `validate_tolerance` and `GpkgSource::load` `Result<_, String>`s directly (same as `run_serve`
    // line ~625).
    vector::topology::validate_tolerance(args.snap_tolerance)?;
    // `.gpkg`/`.geojson` load whole (LoadAll); a `.fgb` opens the windowed reader (Windowed). Either
    // way `vs` is read via `full_extent()` below (LoadAll ignores the bbox; Windowed queries the
    // whole R-tree), so the topology build sees every feature.
    let vs = load_topology_source(&args.vector, args.layer.as_deref())?;
    let (topo, rep) = vector::topology::build_topology(
        vs.features_in(vs.full_extent())?.as_slice(),
        args.snap_tolerance,
    );
    println!("{}", vector::topology::format_report(&rep));
    for w in &rep.warnings {
        eprintln!("warning: {w}");
    }
    if args.verify {
        let mismatched = topo.verify_roundtrip(
            vs.features_in(vs.full_extent())?.as_slice(),
            args.snap_tolerance,
        );
        println!(
            "round-trip: {mismatched} / {} features mismatched",
            rep.features_in
        );
    }
    Ok(())
}

#[cfg(test)]
mod build_topology_cli_tests {
    /// An unknown SCHEME must be rejected by name, up front. Before the URI registry it fell
    /// through every extension test to the GeoPackage opener and failed inside SQLite,
    /// reporting a file problem for something that was never a file. This is also the seam a
    /// PostGIS reader attaches to, so the message must name what IS supported.
    #[test]
    fn unsupported_scheme_is_rejected_by_name() {
        // `match`, not `expect_err`: the Ok type (`VectorSource`) is not `Debug`, by design -
        // it holds trait objects.
        let msg = match super::load_topology_source("postgis://user@host/db?table=roads", None) {
            Ok(_) => panic!("an unknown scheme must not reach a file opener"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("postgis://"), "must name the scheme: {msg}");
        assert!(
            msg.contains(".fgb") && msg.contains(".gpkg"),
            "must say what IS supported: {msg}"
        );
    }

    use super::*;
    use crate::vector::topology::BuildReport;

    #[test]
    fn format_report_lists_the_key_counts() {
        let mut r = BuildReport::default();
        r.features_in = 3;
        r.arcs = 7;
        r.shared_arcs = 4;
        r.boundary_arcs = 3;
        r.junctions = 5;
        let s = vector::topology::format_report(&r);
        assert!(s.contains("features") && s.contains("3"));
        assert!(s.contains("shared") && s.contains("4"));
        assert!(s.contains("boundary") && s.contains("3"));
        assert!(s.contains("arcs") && s.contains("7"));
    }

    #[test]
    fn validate_tolerance_rejects_zero() {
        assert!(vector::topology::validate_tolerance(0.0).is_err());
    }

    #[test]
    fn validate_tolerance_rejects_negative() {
        assert!(vector::topology::validate_tolerance(-1.0).is_err());
    }

    #[test]
    fn validate_tolerance_rejects_nan() {
        assert!(vector::topology::validate_tolerance(f64::NAN).is_err());
    }

    #[test]
    fn validate_tolerance_rejects_infinity() {
        // +inf snaps every finite coordinate to 0 (coverage collapses to the origin) → must reject,
        // as the message promises "finite".
        assert!(vector::topology::validate_tolerance(f64::INFINITY).is_err());
    }

    #[test]
    fn validate_tolerance_accepts_positive() {
        assert!(vector::topology::validate_tolerance(0.01).is_ok());
    }

    #[test]
    fn load_topology_source_dispatches_by_extension() {
        use crate::vector::source::VectorSource;
        // A `.gpkg` reads whole; a `.fgb` opens the windowed reader.
        let g = load_topology_source("fixtures/gpkg/mini.gpkg", None).expect("load gpkg");
        assert!(matches!(g, VectorSource::LoadAll(_)), "gpkg → LoadAll");
        let f = load_topology_source("fixtures/fgb/hole.fgb", None).expect("load fgb");
        assert!(matches!(f, VectorSource::Windowed(_)), "fgb → Windowed");
        let j = load_topology_source("fixtures/fgb/hole.geojson", None).expect("load geojson");
        assert!(matches!(j, VectorSource::LoadAll(_)), "geojson → LoadAll");
    }

    #[test]
    fn build_topology_reads_a_flatgeobuf_coverage() {
        // hole.fgb is one polygon: a 10×10 square exterior ring with a 4×4 square hole cut out —
        // so 1 feature and 2 rings. Building topology over the FlatGeoBuf's full extent must see
        // both rings and produce at least one arc (proving features were actually decoded, not an
        // empty read behind a 200).
        let vs = load_topology_source("fixtures/fgb/hole.fgb", None).expect("load fgb");
        let feats = vs.features_in(vs.full_extent()).expect("read the fixture");
        let (_topo, rep) = crate::vector::topology::build_topology(feats.as_slice(), 0.01);
        assert_eq!(rep.features_in, 1, "one polygon feature");
        assert_eq!(rep.rings_in, 2, "exterior + hole");
        assert!(
            rep.arcs >= 1,
            "topology built at least one arc, got {}",
            rep.arcs
        );
    }

    #[test]
    fn build_topology_reads_a_geojson_coverage() {
        // hole.geojson is the same donut as hole.fgb: one polygon, exterior + hole = 2 rings. The
        // GeoJSON path must build the identical topology (1 feature / 2 rings / >=1 arc).
        let vs = load_topology_source("fixtures/fgb/hole.geojson", None).expect("load geojson");
        let feats = vs.features_in(vs.full_extent()).expect("read the fixture");
        let (_topo, rep) = crate::vector::topology::build_topology(feats.as_slice(), 0.01);
        assert_eq!(rep.features_in, 1, "one polygon feature");
        assert_eq!(rep.rings_in, 2, "exterior + hole");
        assert!(
            rep.arcs >= 1,
            "topology built at least one arc, got {}",
            rep.arcs
        );
    }
}
