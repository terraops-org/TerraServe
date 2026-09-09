//! `build-pmtiles --zoom-source`: bake each zoom from its own pre-generalized subset.
//!
//! This is what the whole extract chain was for. `generate.rs` already picks its source with
//! `VectorLayer::source_for_zoom`, so once a bake can DECLARE bands, one command produces one
//! archive whose every zoom was cut from the subset made for it -- and a zoom's tiles then involve
//! no per-tile selection at all, which is the seam-free property the plan is chasing.
//!
//! Proven the same way the read-through tests are: the base source and the band source hold
//! DIFFERENT geometry, so a tile encoded from one is provably not the other.

use clap::Parser;
use terraserve::{run_build_pmtiles, BuildPmtilesArgs};

/// Parsing real argv rather than filling a struct literal: it pins the FLAG NAMES too, which is
/// where this project has been bitten before (clap derives a flag from the field name, and a
/// five-layer production bake once died one second in on `--mvt-min-feature-min-zoom`).
#[derive(Parser)]
struct Wrap {
    #[command(flatten)]
    args: BuildPmtilesArgs,
}

fn base_args(out: &str, zoom_sources: &[&str]) -> BuildPmtilesArgs {
    let mut argv: Vec<String> = vec![
        "build-pmtiles".into(),
        "--vector".into(),
        "fixtures/vector/countries.geojson".into(),
        "--vec-style".into(),
        "fixtures/styles/countries.vec.json".into(),
        "--src-crs".into(),
        "EPSG:4326".into(),
        "--name".into(),
        "banded".into(),
        "--out".into(),
        out.into(),
        "--min-zoom".into(),
        "0".into(),
        "--max-zoom".into(),
        "1".into(),
    ];
    for z in zoom_sources {
        argv.push("--zoom-source".into());
        argv.push((*z).into());
    }
    Wrap::parse_from(argv).args
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("ts_bake_zs_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A z0 tile baked with a band declared for z0 must be the BAND's geometry, and must differ from
/// the same bake without the band.
#[test]
fn a_bake_reads_the_declared_band_instead_of_the_layers_own_source() {
    use terraserve::vector::pmtiles::read::PmtilesReader;

    let d = tmpdir("with");
    let with = d.join("with.pmtiles");
    let without = d.join("without.pmtiles");

    let mut a = base_args(
        &with.to_string_lossy(),
        &["0:1:fixtures/vector/mini_mvt.geojson"],
    );
    a.tmpdir = Some(d.to_string_lossy().to_string());
    run_build_pmtiles(&a).expect("bake with a band");

    let mut b = base_args(&without.to_string_lossy(), &[]);
    b.tmpdir = Some(d.to_string_lossy().to_string());
    run_build_pmtiles(&b).expect("bake without a band");

    let ra = PmtilesReader::open(&with).unwrap();
    let rb = PmtilesReader::open(&without).unwrap();
    let ta = ra.get(0, 0, 0).unwrap().expect("z0 tile, banded bake");
    let tb = rb.get(0, 0, 0).unwrap().expect("z0 tile, plain bake");
    assert!(!ta.is_empty() && !tb.is_empty(), "z0 covers both fixtures");
    assert_ne!(
        ta, tb,
        "if these match, --zoom-source was parsed and then never read"
    );

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn a_malformed_zoom_source_is_refused_with_the_expected_shape() {
    let d = tmpdir("bad");
    let out = d.join("bad.pmtiles");
    let mut a = base_args(&out.to_string_lossy(), &["0-1:some.gpkg"]);
    a.tmpdir = Some(d.to_string_lossy().to_string());
    let err = run_build_pmtiles(&a)
        .err()
        .expect("must be refused")
        .to_string();
    assert!(err.contains("min:max:path"), "{err}");
    std::fs::remove_dir_all(&d).ok();
}
