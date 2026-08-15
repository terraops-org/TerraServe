//! Live PostGIS tests. SELF-SKIP when TERRASERVE_PG_TEST_URL is unset -- CI has no Postgres, and
//! this repo has already gone red for a test that did not self-skip.
//!
//! To run: `./tests/postgis-fixture.sh` starts the container and seeds it FROM
//! `fixtures/gpkg/mini.gpkg` -- the same file `tests/gpkg_source.rs` reads -- plus a handful of
//! metadata edge-case tables/views (srid 0, two geometry columns, an empty geometry). It prints
//! the exact env vars to export. Then:
//!
//!   export TERRASERVE_PG_TEST_PASSWORD=terraserve_test_pw
//!   export TERRASERVE_PG_TEST_URL='postgis://postgres:${TERRASERVE_PG_TEST_PASSWORD}@localhost:5433/postgres/public.mini_feats'
//!   cargo test --test postgis_live
//!
//! Nothing in this file ever prints the URL or the resolved DSN: `TERRASERVE_PG_TEST_URL` holds
//! `${VAR}`-style password interpolation (see `pg_uri.rs`'s `Dsn`, which refuses to print itself),
//! and an assertion message that echoed the URL back would defeat that design.

use std::sync::Arc;

use terraserve::vector::feature::Feature;
use terraserve::vector::gpkg::GpkgSource;
use terraserve::vector::postgis::PostgisSource;
use terraserve::vector::source::{FeatureSource, WindowedSource};

/// `fixtures/gpkg/mini.gpkg` is the SAME file `postgis-fixture.sh` loads into `mini_feats` via
/// ogr2ogr -- so the two sources can be compared feature-for-feature, not just smoke-tested.
const MINI_GPKG: &str = "fixtures/gpkg/mini.gpkg";
/// The fixture's own extent (`gpkg_source.rs::loads_mini_gpkg_fixture` pins this), reused as the
/// `open()` extent argument everywhere below -- it has no effect on `query()` (see
/// `full_extent_is_the_configured_value_not_a_database_query`), so any value would do, but a real
/// one keeps every test's window meaningful.
const MINI_EXTENT: [f64; 4] = [0.0, 0.0, 30.0, 10.0];

fn url() -> Option<String> {
    std::env::var("TERRASERVE_PG_TEST_URL").ok()
}

/// Point the configured URL at a different table/view in the fixture, by swapping the trailing
/// `schema.table` path segment. Every other part (host, port, db, credentials) stays exactly what
/// the operator configured -- this file must never assemble a DSN of its own. Panics on a
/// malformed base URL rather than silently testing the wrong table; the message names the
/// expected shape, never the URL itself.
fn url_for(table: &str) -> Option<String> {
    let base = url()?;
    let (head, tail) = base.rsplit_once('/').expect(
        "TERRASERVE_PG_TEST_URL must be schema.table-shaped, e.g. .../postgres/public.mini_feats",
    );
    let schema = tail
        .split('?')
        .next()
        .unwrap_or(tail)
        .split_once('.')
        .map(|(s, _)| s)
        .unwrap_or("public");
    Some(format!("{head}/{schema}.{table}"))
}

/// Run `WindowedSource::query` the way `server.rs` actually calls it: from a blocking-pool thread
/// reached via `spawn_blocking`, under a tokio runtime. `query()`'s body bridges back into that
/// runtime with `Handle::current().block_on(..)` -- legal from a blocking-pool thread (pinned by
/// `postgis.rs::block_on_inside_spawn_blocking_does_not_deadlock`) but a hard PANIC from a plain
/// async task on the runtime (`block_on_from_a_reactor_thread_panics`). Calling `src.query(..)`
/// directly from a bare `#[test]` fn hits neither of those -- there is no runtime in scope at
/// all, so `query()`'s own `Handle::try_current()` guard fires and it silently returns an empty
/// `Vec` (see the `eprintln!("... no tokio runtime in scope ...")` arm) instead of ever touching
/// the database. This helper is what makes these tests exercise the real query path rather than
/// that fallback.
fn query_like_the_server_does(src: Arc<PostgisSource>, bbox: [f64; 4]) -> Vec<Feature> {
    in_blocking_pool(move || src.query(bbox)).expect("the query itself must succeed")
}

/// The same runtime + `spawn_blocking` staging, for any read (`query`, `query_gated`, ...).
///
/// ⚠ Read `query_like_the_server_does`'s comment above before calling a `PostgisSource` read from
/// a bare `#[test]`: with no runtime in scope, `query()`/`query_gated()` log a line and return an
/// EMPTY `Vec` without touching the database. A test that skipped this helper would therefore pass
/// while asserting on nothing -- and "a test whose trigger was a defect went vacuous when the
/// defect was fixed" has already happened twice on this seam.
fn in_blocking_pool<T, F>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let rt = tokio::runtime::Runtime::new().expect("build a tokio runtime for the test");
    rt.block_on(async move {
        tokio::task::spawn_blocking(f)
            .await
            .expect("the read panicked inside spawn_blocking")
    })
}

#[test]
fn queries_a_bbox_and_decodes_geometry() {
    let Some(u) = url() else {
        eprintln!("SKIP: TERRASERVE_PG_TEST_URL unset");
        return;
    };
    let src = PostgisSource::open(&u, MINI_EXTENT).expect("open");
    let feats = query_like_the_server_does(Arc::new(src), MINI_EXTENT);
    assert!(!feats.is_empty(), "expected features in the test table");
}

#[test]
fn a_bbox_outside_the_data_returns_empty_not_an_error() {
    let Some(u) = url() else {
        eprintln!("SKIP: TERRASERVE_PG_TEST_URL unset");
        return;
    };
    // POSITIVE CONTROL FIRST, and it is not optional decoration. `query()` returns an empty `Vec`
    // on ANY failure -- bad SQL, a dead connection, a missing column, no runtime in scope -- so an
    // emptiness assertion on its own passes against a totally broken source and proves nothing.
    // The same source must be shown to return features for a window that HAS data before its
    // emptiness elsewhere means anything.
    let src = Arc::new(PostgisSource::open(&u, MINI_EXTENT).expect("open"));
    let inside = query_like_the_server_does(Arc::clone(&src), MINI_EXTENT);
    assert!(
        !inside.is_empty(),
        "positive control: this source must return features for a window that has data, \
         otherwise the empty result below says nothing about the bbox filter"
    );

    let outside = query_like_the_server_does(src, [1e7, 1e7, 1e7 + 1.0, 1e7 + 1.0]);
    assert!(outside.is_empty());
}

#[test]
fn full_extent_is_the_configured_value_not_a_database_query() {
    let Some(u) = url() else {
        eprintln!("SKIP: TERRASERVE_PG_TEST_URL unset");
        return;
    };
    let e = [1.0, 2.0, 3.0, 4.0];
    let src = PostgisSource::open(&u, e).expect("open");
    assert_eq!(src.full_extent(), e);
}

/// The test worth having above all others: `mini_feats` in PostGIS and `fixtures/gpkg/mini.gpkg`
/// on disk hold the SAME data (the fixture script loads one from the other), so the two sources
/// must return the same features for the same window -- same geometry, same attributes. A
/// backend that decodes differently would otherwise show up as a diff in a real map, not as a
/// failing test. Order is NOT compared positionally: `build_sql` deliberately issues no
/// `ORDER BY` (§ design doc, section 1 -- ordering is not the database's job here), so features
/// are matched by their `name` attribute instead.
#[test]
fn matches_the_same_data_read_from_geopackage() {
    let Some(u) = url() else {
        eprintln!("SKIP: TERRASERVE_PG_TEST_URL unset");
        return;
    };

    let gpkg = GpkgSource::load(MINI_GPKG, None).expect("load fixtures/gpkg/mini.gpkg");
    assert_eq!(
        gpkg.features().len(),
        3,
        "the gpkg fixture itself changed shape -- fixture assumption is stale"
    );

    let mut src = PostgisSource::open(&u, gpkg.full_extent()).expect("open mini_feats");
    src.set_columns(vec!["name".to_string(), "rank".to_string()]);
    let pg_feats = query_like_the_server_does(Arc::new(src), gpkg.full_extent());
    assert_eq!(
        pg_feats.len(),
        3,
        "postgis returned a different feature count than the gpkg it was seeded from"
    );

    for gf in gpkg.features() {
        let name = gf
            .props
            .get_str("name")
            .expect("gpkg feature has a `name` attribute");
        let pf = pg_feats
            .iter()
            .find(|f| f.props.get_str("name") == Some(name))
            .unwrap_or_else(|| panic!("postgis is missing feature `{name}` the gpkg has"));
        assert_eq!(pf.geom, gf.geom, "geometry differs for `{name}`");
        assert_eq!(
            pf.props.get_f64("rank"),
            gf.props.get_f64("rank"),
            "rank attribute differs for `{name}`"
        );
    }
}

/// Regression pin: attribute decoding returned `{}` (empty props) until an hour before this test
/// was written. Checks known values directly against the fixture, rather than only through the
/// cross-backend comparison above, so a bug that happened to affect both sources identically
/// would still be caught here.
#[test]
fn attributes_come_back_populated() {
    let Some(u) = url() else {
        eprintln!("SKIP: TERRASERVE_PG_TEST_URL unset");
        return;
    };
    let mut src = PostgisSource::open(&u, MINI_EXTENT).expect("open");
    src.set_columns(vec!["name".to_string(), "rank".to_string()]);
    let feats = query_like_the_server_does(Arc::new(src), MINI_EXTENT);

    let square_a = feats
        .iter()
        .find(|f| f.props.get_str("name") == Some("square_a"))
        .expect("square_a present");
    assert_eq!(square_a.props.get_f64("rank"), Some(1.0));

    let with_hole = feats
        .iter()
        .find(|f| f.props.get_str("name") == Some("square_with_hole"))
        .expect("square_with_hole present");
    assert_eq!(with_hole.props.get_f64("rank"), Some(2.0));
}

/// `mini_computed` (a view whose geometry is `ST_Centroid(...)`, i.e. computed) and
/// `mini_untyped` (a bare `geometry` column with no typmod) both register in `geometry_columns`
/// with srid 0 -- MEASURED, not assumed, in `postgis-fixture.sh`'s own comments. Accepting that
/// would build a layer that reprojects from SRID 0 and misplaces every feature with no error
/// anywhere, so `open()` must refuse both and tell the operator to add `?srid=`.
#[test]
fn srid_zero_is_rejected_with_an_actionable_message() {
    let Some(_) = url() else {
        eprintln!("SKIP: TERRASERVE_PG_TEST_URL unset");
        return;
    };
    for table in ["mini_computed", "mini_untyped"] {
        let u = url_for(table).unwrap();
        // `.unwrap_err()`/`.expect_err()` both require `T: Debug` (to print it on the OTHER
        // branch), and `PostgisSource` deliberately has no `Debug` impl -- it would be an easy
        // way for a future field to leak into a log. Match by hand instead.
        let err = match PostgisSource::open(&u, MINI_EXTENT) {
            Err(e) => e,
            Ok(_) => panic!("{table} has srid 0 and must be rejected"),
        };
        assert!(
            err.contains("?srid="),
            "{table}: error must tell the operator to add ?srid=, got: {err}"
        );
    }
}

/// `mini_twogeom` registers two geometry columns (`geom` srid 4326, `geom_3857` srid 3857).
/// Without `?geom=` this used to surface as tokio-postgres's "query returned an unexpected
/// number of rows" -- an internal detail that says nothing about the operator's table. `open()`
/// must instead name both columns and point at the fix; naming the column then resolves it to
/// THAT column's own srid, not the other one's.
#[test]
fn two_geometry_columns_are_named_instead_of_failing_obscurely() {
    let Some(_) = url() else {
        eprintln!("SKIP: TERRASERVE_PG_TEST_URL unset");
        return;
    };
    let u = url_for("mini_twogeom").unwrap();
    let err = match PostgisSource::open(&u, MINI_EXTENT) {
        Err(e) => e,
        Ok(_) => {
            panic!("mini_twogeom has two geometry columns and must be rejected without ?geom=")
        }
    };
    assert!(
        err.contains("geom") && err.contains("geom_3857"),
        "name both: {err}"
    );
    assert!(err.contains("?geom="), "state the fix: {err}");

    // Naming the SECOND column must pick up ITS srid (3857), not the first column's (4326).
    let u_named = format!("{u}?geom=geom_3857");
    let src = PostgisSource::open(&u_named, MINI_EXTENT)
        .expect("naming the column via ?geom= must resolve it");
    assert_eq!(src.crs(), Some("EPSG:3857"));
}

/// `mini_empty` holds one row whose geometry is `POLYGON EMPTY`. An empty geometry has no
/// bounding box, so PostGIS's `&&` never matches it against any query window -- it is filtered
/// out at the SQL level, not decoded and then dropped. Either way the observable contract is the
/// same one `decode_wkb`'s own `Ok(None)` documents: nothing to draw is not a fault, so `open()`
/// and `query()` must both succeed cleanly with zero features, never an `Err` and never a panic.
#[test]
fn empty_geometry_is_skipped_not_an_error() {
    let Some(u) = url() else {
        eprintln!("SKIP: TERRASERVE_PG_TEST_URL unset");
        return;
    };
    // POSITIVE CONTROL: the SAME query path, over the SAME window, against a table that does have
    // drawable geometry. Without it, "empty" below is satisfied by a source that is simply broken,
    // and this test would keep passing after a change that made every PostGIS query fail.
    let feats = query_like_the_server_does(
        Arc::new(PostgisSource::open(&u, MINI_EXTENT).expect("open")),
        MINI_EXTENT,
    );
    assert!(
        !feats.is_empty(),
        "positive control: the query path must return features for a table that has them"
    );

    let u = url_for("mini_empty").unwrap();
    let src = PostgisSource::open(&u, MINI_EXTENT).expect("open mini_empty");
    // The table must genuinely EXIST and be a geometry table -- otherwise the emptiness below is
    // "there is nothing here" rather than "the empty geometry was skipped". `open()` already fails
    // on an unregistered relation, and this pins the SRID it registered, so a fixture that stopped
    // creating `mini_empty` (or created it differently) fails here instead of passing vacuously.
    assert_eq!(
        src.crs(),
        Some("EPSG:4326"),
        "mini_empty must exist and register srid 4326 -- re-run tests/postgis-fixture.sh"
    );
    let feats = query_like_the_server_does(Arc::new(src), MINI_EXTENT);
    assert!(feats.is_empty(), "an empty geometry must not be drawn");
}

/// CRITICAL-1 regression. A column the table does not have -- a typo, or a case mismatch, since
/// `quote_ident` preserves case -- used to make every single query fail with Postgres 42703 while
/// `query()` reported it as an empty feature list. Startup succeeded, GetCapabilities looked right,
/// and every GetMap and every tile was a blank 200 OK for the life of the process.
#[test]
fn a_column_the_table_does_not_have_fails_loudly_at_startup() {
    let Some(u) = url() else {
        eprintln!("SKIP: TERRASERVE_PG_TEST_URL unset");
        return;
    };
    // POSITIVE CONTROL: the real columns must validate cleanly, or "rejects a bad column" could be
    // "rejects every column".
    let mut good = PostgisSource::open(&u, MINI_EXTENT).expect("open");
    good.set_columns(vec!["name".to_string(), "rank".to_string()]);
    good.validate_columns()
        .expect("the table's REAL columns must validate");

    // Case mismatch: the column is `name`, the style asked for `Name`. This is the realistic
    // version (an SLD `<PropertyName>Name</PropertyName>`), not a contrived typo.
    let mut bad = PostgisSource::open(&u, MINI_EXTENT).expect("open");
    bad.set_columns(vec!["name".to_string(), "Name".to_string()]);
    let err = bad.validate_columns().expect_err("`Name` must be rejected");
    assert!(err.contains("Name"), "name the column asked for: {err}");
    assert!(
        err.contains("mini_feats"),
        "name the table it was asked of: {err}"
    );
    assert!(
        err.contains("rank"),
        "list the columns the table really has, so the fix is visible: {err}"
    );
    assert!(
        err.to_uppercase().contains("CASE-SENSITIVE"),
        "say why `Name` is not `name`: {err}"
    );
}

/// Validation must also cover the path `resolve_metadata` skips entirely: when the URI supplies
/// BOTH `?geom=` and `?srid=`, no `geometry_columns` lookup happens, so nothing before this had
/// ever confirmed that the geometry column -- or the relation itself -- exists.
#[test]
fn a_geometry_column_that_does_not_exist_is_caught_even_when_the_uri_supplies_both_hints() {
    let Some(_) = url() else {
        eprintln!("SKIP: TERRASERVE_PG_TEST_URL unset");
        return;
    };
    let u = format!(
        "{}?geom=nosuchgeom&srid=4326",
        url_for("mini_feats").unwrap()
    );
    let src = PostgisSource::open(&u, MINI_EXTENT)
        .expect("both hints given, so open() asks the database nothing and must succeed");
    let err = src
        .validate_columns()
        .expect_err("a geometry column that does not exist must be caught");
    assert!(err.contains("nosuchgeom"), "name the column: {err}");
}

// ---- the zoom-aware min-feature-size pushdown (`size_gate_sql`) --------------------------------

/// The mixed-geometry fixture's own extent, and a window that contains all of it.
const MIXED_EXTENT: [f64; 4] = [0.0, 0.0, 10.0, 10.0];

/// Read `mini_mixed` through `query_gated` and return the `kind` of every feature that came back.
fn mixed_kinds(min_area_src: f64) -> Vec<String> {
    let u = url_for("mini_mixed").unwrap();
    let mut src = PostgisSource::open(&u, MIXED_EXTENT).expect("open mini_mixed");
    src.set_columns(vec!["kind".to_string()]);
    src.validate_columns().expect("mini_mixed columns validate");
    let feats = in_blocking_pool(move || src.query_gated(MIXED_EXTENT, min_area_src))
        .expect("the query itself must succeed");
    let mut kinds: Vec<String> = feats
        .iter()
        .map(|f| f.props.get_str("kind").unwrap_or("?").to_string())
        .collect();
    kinds.sort();
    kinds
}

/// ⚠ THE TRAP, against a real PostGIS. `ST_Area` is 0 for every LineString and every Point, so a
/// naive `ST_Area(geom) >= t` returns a 200 OK with an empty map -- every road and every place
/// gone. This is the test that fails if that ever ships.
///
/// The threshold here (1.0 units^2) is 100x the tiny polygon's area and 1/100th of the big one, so
/// there is no floating-point knife edge: exactly one feature may disappear.
#[test]
fn the_size_gate_never_drops_a_line_or_a_point() {
    let Some(_) = url() else {
        eprintln!("SKIP: TERRASERVE_PG_TEST_URL unset");
        return;
    };
    // The ungated read is the BASELINE, and it is not decoration: without it, "the gate dropped
    // nothing" and "the source returned nothing" look identical.
    //
    // `multi_point` is absent here, and that is PRE-EXISTING and unrelated to the gate: `MultiPoint`
    // is valid WKB that this engine does not model yet, so `wkb::decode_wkb` returns `Ok(None)` and
    // it is skipped (wkb.rs, the `4 => Ok(None)` arm). It stays in the fixture so the day a
    // `Geometry::MultiPoint` variant lands, this test starts covering it instead of silently not.
    let ungated = mixed_kinds(0.0);
    assert_eq!(
        ungated,
        vec![
            "big_poly",
            "diag_line",
            "ew_line",
            "multi_line",
            "ns_line",
            "place_point",
            "tiny_poly"
        ],
        "the ungated read must return the whole fixture (less the unmodeled MULTIPOINT) -- \
         re-run tests/postgis-fixture.sh if mini_mixed is missing"
    );

    let gated = mixed_kinds(1.0);

    // Every line and every point survives. Named individually so a failure says WHICH shape was
    // deleted: `ew_line`/`ns_line` failing alone means someone reached for ST_Envelope (degenerate
    // for an axis-aligned line); ALL of them failing means the `> 0` guard is gone.
    for kind in [
        "ew_line",
        "ns_line",
        "diag_line",
        "multi_line",
        "place_point",
    ] {
        assert!(
            gated.iter().any(|k| k == kind),
            "{kind} was DELETED by the size gate -- ST_Area is 0 for lines and points, so they \
             must be exempt. Survivors: {gated:?}"
        );
    }
    // And the gate is not a no-op: the sub-threshold polygon, and ONLY it, is gone.
    assert!(!gated.iter().any(|k| k == "tiny_poly"), "{gated:?}");
    assert!(gated.iter().any(|k| k == "big_poly"), "{gated:?}");
    assert_eq!(
        gated.len(),
        ungated.len() - 1,
        "exactly one drop: {gated:?}"
    );
}

/// A threshold big enough to swallow every polygon in the fixture must STILL return every line and
/// point. The previous test could pass with an off-by-a-lot threshold; this one pins that the
/// exemption is structural (the `> 0` guard) and not an artefact of the numbers chosen.
#[test]
fn an_enormous_threshold_still_returns_every_line_and_point() {
    let Some(_) = url() else {
        eprintln!("SKIP: TERRASERVE_PG_TEST_URL unset");
        return;
    };
    // (`multi_point` is absent for a pre-existing reason unrelated to the gate -- see the baseline
    // assertion in `the_size_gate_never_drops_a_line_or_a_point`.)
    let gated = mixed_kinds(1e12);
    assert_eq!(
        gated,
        vec![
            "diag_line",
            "ew_line",
            "multi_line",
            "ns_line",
            "place_point"
        ],
        "every polygon should be gone and every line/point kept"
    );
}

/// Requirement: the gate is OPT-IN. A zero threshold must be indistinguishable from the old code
/// path -- same features, in the same order, with the same attributes.
#[test]
fn a_zero_threshold_reads_exactly_what_the_ungated_query_reads() {
    let Some(u) = url() else {
        eprintln!("SKIP: TERRASERVE_PG_TEST_URL unset");
        return;
    };
    let win = [0.0, 0.0, 30.0, 10.0];
    let plain = {
        let mut s = PostgisSource::open(&u, MINI_EXTENT).expect("open");
        s.set_columns(vec!["name".to_string()]);
        in_blocking_pool(move || s.query(win)).expect("the query itself must succeed")
    };
    let gated_off = {
        let mut s = PostgisSource::open(&u, MINI_EXTENT).expect("open");
        s.set_columns(vec!["name".to_string()]);
        in_blocking_pool(move || s.query_gated(win, 0.0)).expect("the query itself must succeed")
    };
    assert_eq!(plain.len(), gated_off.len());
    for (a, b) in plain.iter().zip(gated_off.iter()) {
        assert_eq!(a.fid, b.fid);
        assert_eq!(a.props.get_str("name"), b.props.get_str("name"));
        assert_eq!(a.bbox, b.bbox);
        assert_eq!(a.area, b.area);
    }
}

/// The containment property the whole design rests on: **the SQL drops a strict SUBSET of what the
/// Rust gate drops**, so turning the pushdown on cannot change a rendered tile. Asserted against
/// PostGIS's `ST_Area` and our own `Feature::area` on the SAME geometries -- two independent
/// implementations that must agree on which side of the threshold each feature falls.
#[test]
fn every_feature_the_sql_drops_would_also_have_been_dropped_by_the_rust_gate() {
    let Some(_) = url() else {
        eprintln!("SKIP: TERRASERVE_PG_TEST_URL unset");
        return;
    };
    let t = 1.0;
    let u = url_for("mini_mixed").unwrap();
    let mut src = PostgisSource::open(&u, MIXED_EXTENT).expect("open mini_mixed");
    src.set_columns(vec!["kind".to_string()]);
    let all = {
        let s = PostgisSource::open(&u, MIXED_EXTENT).expect("open");
        let mut s = s;
        s.set_columns(vec!["kind".to_string()]);
        in_blocking_pool(move || s.query_gated(MIXED_EXTENT, 0.0))
            .expect("the query itself must succeed")
    };
    let kept = in_blocking_pool(move || src.query_gated(MIXED_EXTENT, t))
        .expect("the query itself must succeed");

    for f in &all {
        let kind = f.props.get_str("kind").unwrap_or("?").to_string();
        let survived = kept
            .iter()
            .any(|k| k.props.get_str("kind") == Some(kind.as_str()));
        // `Feature::area` is our shoelace; this is exactly `mvt/tile.rs`'s gate.
        let rust_would_drop = f.area > 0.0 && f.area < t;
        assert_eq!(
            !survived,
            rust_would_drop,
            "{kind}: SQL {} it but the Rust gate would {} (area={})",
            if survived { "kept" } else { "dropped" },
            if rust_would_drop {
                "drop it"
            } else {
                "keep it"
            },
            f.area
        );
    }
}
