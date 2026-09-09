//! `build-pmtiles --columns`: the attribute columns a bake must fetch.
//!
//! The same gap `extract --columns` closed, one command over. A `postgis://` layer's column list
//! is derived from the SERVER-side style's referenced fields, so a client-side `--mvt-style`'s
//! `["get","building"]` is invisible to a bake and the archive ships with no such property --
//! which is exactly why the shipped eu5 v3 archive draws every building in its style's default
//! colour.
//!
//! It matters more once bands exist: a subset cut by `extract --columns building,name` carries
//! those attributes, so without the same flag here a single archive would theme correctly at the
//! banded zooms and not above them.

use clap::Parser;
use terraserve::BuildPmtilesArgs;

#[derive(Parser)]
struct Wrap {
    #[command(flatten)]
    args: BuildPmtilesArgs,
}

fn parse(extra: &[&str]) -> BuildPmtilesArgs {
    let mut argv: Vec<String> = vec![
        "build-pmtiles".into(),
        "--vector".into(),
        "fixtures/vector/mini_mvt.geojson".into(),
        "--vec-style".into(),
        "fixtures/styles/airports.vec.json".into(),
        "--out".into(),
        "/dev/null".into(),
    ];
    argv.extend(extra.iter().map(|s| (*s).to_string()));
    Wrap::parse_from(argv).args
}

#[test]
fn columns_is_accepted_and_split_on_commas() {
    let a = parse(&["--columns", "building,name"]);
    assert_eq!(
        a.clean_columns(),
        vec!["building".to_string(), "name".to_string()]
    );
}

#[test]
fn columns_defaults_to_empty_so_existing_bakes_are_unchanged() {
    assert!(parse(&[]).clean_columns().is_empty());
}

/// A trailing comma must not become a column named "", which Postgres would reject at startup
/// with a syntax error rather than anything legible.
#[test]
fn a_trailing_comma_does_not_produce_an_empty_column() {
    let a = parse(&["--columns", "building,"]);
    assert_eq!(a.clean_columns(), vec!["building".to_string()]);
}
