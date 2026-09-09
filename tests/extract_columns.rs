//! `terraserve extract --columns / --no-columns`: which attributes a subset carries.
//!
//! The first real eu5 extract wrote `0 attribute columns`, because a `postgis://` layer's column
//! list is derived from the SERVER-side style's referenced fields and nothing else -- so anything
//! only a CLIENT-side `--mvt-style` needs (eu5's `["get","building"]`) never reaches the subset.
//! A subset with no attributes cannot be themed, which is harmless for a single-colour layer and
//! fatal for roads or landuse.
//!
//! Driven over the committed GeoJSON fixture rather than a database: the flags decide the WRITTEN
//! schema on every source, and for `postgis://` they additionally decide what is fetched, which is
//! the half no file source can exercise.

use std::collections::BTreeSet;

use terraserve::{run_extract, ExtractArgs};

fn args(out: &str) -> ExtractArgs {
    ExtractArgs {
        vector: "fixtures/vector/mini_mvt.geojson".into(),
        vec_style: "fixtures/styles/airports.vec.json".into(),
        out: out.into(),
        name: "subset".into(),
        grid: "WebMercatorQuad".into(),
        min_zoom: 0,
        max_zoom: 2,
        mvt_min_feature_px: 0.0,
        mvt_min_feature_min_zoom: 0,
        mvt_min_feature_len_px: String::new(),
        src_crs: Some("EPSG:4326".into()),
        extent: None,
        keep_fields: None,
        columns: None,
        no_columns: false,
    }
}

/// The columns of the written feature table, minus the GeoPackage's own two.
fn written_columns(path: &str) -> BTreeSet<String> {
    let conn = rusqlite::Connection::open(path).expect("open subset");
    let mut st = conn.prepare("PRAGMA table_info(subset)").unwrap();
    let cols: Vec<String> = st
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .map(|r| r.unwrap())
        .filter(|c| c != "fid" && c != "geom")
        .collect();
    cols.into_iter().collect()
}

fn tmp(name: &str) -> String {
    let d = std::env::temp_dir().join(format!("ts_extract_cols_{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join(name).to_string_lossy().to_string()
}

#[test]
fn by_default_a_subset_carries_the_sources_own_attributes() {
    let out = tmp("default.gpkg");
    run_extract(&args(&out)).expect("extract");
    let cols = written_columns(&out);
    assert!(cols.contains("name"), "{cols:?}");
    assert!(cols.contains("kind"), "{cols:?}");
}

#[test]
fn no_columns_writes_geometry_only() {
    let out = tmp("none.gpkg");
    let mut a = args(&out);
    a.no_columns = true;
    run_extract(&a).expect("extract");
    assert!(
        written_columns(&out).is_empty(),
        "--no-columns must write geometry only"
    );
}

#[test]
fn columns_selects_exactly_the_named_attributes() {
    let out = tmp("named.gpkg");
    let mut a = args(&out);
    a.columns = Some("kind".into());
    run_extract(&a).expect("extract");
    let cols = written_columns(&out);
    assert_eq!(
        cols,
        ["kind".to_string()].into_iter().collect::<BTreeSet<_>>(),
        "only the named column is carried"
    );
}

/// Asking for both is a contradiction, and silently letting one win is how a subset ends up
/// missing the attribute its style needs.
#[test]
fn columns_and_no_columns_together_are_refused() {
    let out = tmp("both.gpkg");
    let mut a = args(&out);
    a.columns = Some("kind".into());
    a.no_columns = true;
    let err = run_extract(&a).err().expect("must be refused").to_string();
    assert!(err.contains("--no-columns"), "{err}");
}

/// `extract --mvt-min-feature-px` must actually gate.
///
/// It did not: `run_extract` built a DEFAULT `ServeState`, and `MvtOptimizations::for_layer`
/// reads the size gate off that state rather than off the layer -- so all three gate flags were
/// silently ignored and every subset came out cut at the one-MVT-cell floor instead of the
/// requested threshold. Found on 2026-09-04 asking for a 2.0 px landuse band and getting the
/// floor's 18,859 m² back, i.e. 3.3M features where 38k were expected.
///
/// The baked TILES were never wrong -- `build-pmtiles` populates its state properly and re-applies
/// the gate at encode time, so a too-loose subset is still a valid superset. What was wrong is
/// that the flag did nothing, which is worse than erroring.
#[test]
fn a_large_min_feature_px_actually_thins_the_subset() {
    let loose = tmp("gate-off.gpkg");
    let mut a = args(&loose);
    a.mvt_min_feature_px = 0.0;
    run_extract(&a).expect("extract with no gate");

    let tight = tmp("gate-on.gpkg");
    let mut b = args(&tight);
    // Large enough to exceed even a 60x60-degree polygon's area once converted to the display-px²
    // the gate is denominated in. Points and lines (area 0) stay exempt by design, so the most
    // this can drop from the 3-feature fixture is the polygon.
    b.mvt_min_feature_px = 1.0e9;
    run_extract(&b).expect("extract with a large gate");

    let count = |p: &str| -> i64 {
        rusqlite::Connection::open(p)
            .unwrap()
            .query_row("SELECT count(*) FROM subset", [], |r| r.get(0))
            .unwrap()
    };
    let (n_loose, n_tight) = (count(&loose), count(&tight));
    assert!(n_loose > 0, "the ungated subset must hold something");
    assert!(
        n_tight < n_loose,
        "--mvt-min-feature-px must thin the subset: {n_tight} kept with the gate vs {n_loose} without"
    );
}
