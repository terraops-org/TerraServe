// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Native GeoPackage (`.gpkg`) reader — wires the SQLite container (`rusqlite`, bundled) to
//! the bespoke WKB decoder (`wkb::decode_gpkg_geometry`) into a `FeatureSource`. This module is
//! the intended future-default format for the label engine's vector sources (see `source.rs`).
//!
//! The load is **partitioned + parallel** behind the single public `GpkgSource::load` API: the
//! layer/geom-col/CRS/PK metadata is read once on a main connection, then the feature table is
//! split into contiguous `rowid` ranges and each range is decoded on its own read-only
//! connection (SQLite allows concurrent readers; a rusqlite `Connection` is `Send` but not
//! `Sync`, so never shared — one per worker). The per-range chunks are concatenated in range
//! order, so the merged feature vector is `rowid`/`fid`-ordered and byte-reproducible — the
//! render's draw order depends on that determinism. Small or non-contiguous-rowid tables fall
//! back to a single range through the exact same `load_range` path.
//!
//! ## GeoPackage metadata tables used
//!
//! - `gpkg_contents` — which table(s) hold `data_type='features'`; picks the layer.
//! - `gpkg_geometry_columns` — the geometry column name + `srs_id` for a feature table.
//! - `gpkg_spatial_ref_sys` — resolves `srs_id` → an EPSG code, when the `organization` is EPSG.
//!
//! `PRAGMA table_info("<table>")` finds the INTEGER PRIMARY KEY column (conventionally `fid`,
//! but not assumed by name — the GeoPackage spec only requires *some* integer PK column).

use std::collections::BTreeMap;

use rayon::prelude::*;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use super::feature::{Feature, Props, Value};
use super::source::{FeatureSource, WindowedSource};
use super::wkb::decode_gpkg_geometry;

/// Below this row count the table loads as a single range — partitioning only pays off once the
/// per-row WKB-decode + `Feature` construction cost dominates the fixed per-connection overhead.
const PARALLEL_THRESHOLD: i64 = 10_000;

pub struct GpkgSource {
    features: Vec<Feature>,
    extent: [f64; 4],
    crs: Option<String>,
}

impl GpkgSource {
    /// Parse-once load: opens `path` read-only, picks a features layer (`layer`, or the first
    /// `gpkg_contents` row with `data_type='features'` when `None`), decodes every row's
    /// geometry + attributes across a partitioned parallel load, and holds the result in memory
    /// for the source's lifetime.
    pub fn load(path: &str, layer: Option<&str>) -> Result<GpkgSource, String> {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        Self::load_partitioned(path, layer, PARALLEL_THRESHOLD, workers)
    }

    /// The partition seam behind [`load`](Self::load): read the metadata once on a main
    /// connection, split the feature table's `rowid` span into at most `n_ranges` contiguous
    /// ranges (a single range when the table has fewer than `threshold` rows, has non-contiguous
    /// rowids, or `n_ranges <= 1`), decode each range in parallel on its own read-only
    /// connection, and concatenate the chunks IN RANGE ORDER so the feature order is a stable,
    /// reproducible `rowid`/`fid` order. `load` passes the real threshold + the host CPU count;
    /// tests pass explicit values to force the fallback or a genuine N-way partition on a fixture
    /// smaller than the production threshold.
    #[doc(hidden)]
    pub fn load_partitioned(
        path: &str,
        layer: Option<&str>,
        threshold: i64,
        n_ranges: usize,
    ) -> Result<GpkgSource, String> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("gpkg: open `{path}`: {e}"))?;

        let table = find_features_table(&conn, layer)?;
        let (geom_col, srs_id) = geometry_column(&conn, &table)?;
        let crs = resolve_crs(&conn, srs_id)?;
        let pk_col = primary_key_column(&conn, &table)?;
        let ranges = row_ranges(&conn, &table, threshold, n_ranges)?;
        drop(conn); // release the metadata connection before the fan-out — workers open their own

        // One read-only connection per range, decoded in parallel. `par_iter().collect()`
        // preserves input (range) order, so concatenating the chunks below keeps the merged
        // vector `rowid`/`fid`-ordered and byte-reproducible.
        let chunks: Vec<(Vec<Feature>, [f64; 4])> = ranges
            .par_iter()
            .map(|&(lo, hi)| load_range(path, &table, &geom_col, &pk_col, lo, hi))
            .collect::<Result<Vec<_>, _>>()?;

        let mut features = Vec::new();
        let (mut w, mut s, mut e, mut n) = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        for (chunk, ext) in chunks {
            features.extend(chunk);
            w = w.min(ext[0]);
            s = s.min(ext[1]);
            e = e.max(ext[2]);
            n = n.max(ext[3]);
        }

        if features.is_empty() {
            return Err(format!(
                "gpkg: layer `{table}` in `{path}` yielded no drawable features"
            ));
        }

        Ok(GpkgSource {
            features,
            extent: [w, s, e, n],
            crs,
        })
    }

    /// The resolved CRS of the loaded layer (`Some("EPSG:<code>")`), or `None` when the
    /// `gpkg_spatial_ref_sys` row's organization isn't EPSG (or the code isn't positive) — the
    /// caller falls back to an explicit `--src-crs` in that case.
    pub fn crs(&self) -> Option<&str> {
        self.crs.as_deref()
    }
}

impl FeatureSource for GpkgSource {
    fn features(&self) -> &[Feature] {
        &self.features
    }
    fn full_extent(&self) -> [f64; 4] {
        self.extent
    }
}

/// Pick the feature table: the given `layer` name (validated against `gpkg_contents`), or the
/// first `data_type='features'` row when `None`. Errors if a named layer isn't found, or if the
/// GeoPackage has no features layer at all.
fn find_features_table(conn: &Connection, layer: Option<&str>) -> Result<String, String> {
    const BASE: &str = "SELECT table_name FROM gpkg_contents WHERE data_type='features'";
    match layer {
        Some(name) => conn
            .query_row(&format!("{BASE} AND table_name = ?1"), [name], |r| {
                r.get::<_, String>(0)
            })
            .map_err(|e| format!("gpkg: no features layer named `{name}` in gpkg_contents: {e}")),
        None => conn
            .query_row(BASE, [], |r| r.get::<_, String>(0))
            .map_err(|e| {
                format!(
                    "gpkg: no features layer found in gpkg_contents (data_type='features'): {e}"
                )
            }),
    }
}

/// The geometry column name + `srs_id` for `table`, from `gpkg_geometry_columns`.
fn geometry_column(conn: &Connection, table: &str) -> Result<(String, i64), String> {
    conn.query_row(
        "SELECT column_name, srs_id FROM gpkg_geometry_columns WHERE table_name = ?1",
        [table],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    )
    .map_err(|e| format!("gpkg: gpkg_geometry_columns lookup for `{table}`: {e}"))
}

/// Resolve `srs_id` to an EPSG code via `gpkg_spatial_ref_sys`: `Some("EPSG:<code>")` when the
/// row's `organization` is (case-insensitively) `"EPSG"` and the code is positive; `None` for a
/// missing row, a non-EPSG organization, or a non-positive code (GeoPackage reserves `srs_id`
/// 0 and -1 for undefined-cartesian / undefined-geographic).
fn resolve_crs(conn: &Connection, srs_id: i64) -> Result<Option<String>, String> {
    let row = conn
        .query_row(
            "SELECT organization, organization_coordsys_id FROM gpkg_spatial_ref_sys WHERE srs_id = ?1",
            [srs_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|e| format!("gpkg: gpkg_spatial_ref_sys lookup for srs_id {srs_id}: {e}"))?;
    Ok(row.and_then(|(org, code)| {
        if org.eq_ignore_ascii_case("EPSG") && code > 0 {
            Some(format!("EPSG:{code}"))
        } else {
            None
        }
    }))
}

/// The INTEGER PRIMARY KEY column name for `table`, from `PRAGMA table_info`. GeoPackage
/// requires every feature table to have one (conventionally named `fid`, but the name isn't
/// assumed here — `pk != 0` is the actual signal).
fn primary_key_column(conn: &Connection, table: &str) -> Result<String, String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info(\"{table}\")"))
        .map_err(|e| format!("gpkg: PRAGMA table_info(`{table}`): {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("gpkg: PRAGMA table_info(`{table}`): {e}"))?;
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("gpkg: PRAGMA table_info(`{table}`) row: {e}"))?
    {
        let name: String = row
            .get(1)
            .map_err(|e| format!("gpkg: PRAGMA table_info(`{table}`) name: {e}"))?;
        let pk: i64 = row
            .get(5)
            .map_err(|e| format!("gpkg: PRAGMA table_info(`{table}`) pk flag: {e}"))?;
        if pk != 0 {
            return Ok(name);
        }
    }
    Err(format!(
        "gpkg: table `{table}` has no INTEGER PRIMARY KEY column"
    ))
}

/// Widen a running `[w,s,e,n]` bbox (as four scalars) to include `p` — mirrors
/// `geojson.rs`'s `extend_bbox`, over the source-CRS vertices. `load_range` calls this with a
/// decoded feature's own `bbox` corners (already computed once by `Feature::new`) rather than
/// re-walking the geometry's vertices a second time.
fn extend_bbox(w: &mut f64, s: &mut f64, e: &mut f64, n: &mut f64, p: [f64; 2]) {
    *w = w.min(p[0]);
    *e = e.max(p[0]);
    *s = s.min(p[1]);
    *n = n.max(p[1]);
}

/// Compute the `rowid` ranges to load. Reads `COUNT(*)`, `MIN(rowid)`, `MAX(rowid)` once, then:
/// an empty table → no ranges; a table under `threshold` rows, with non-contiguous rowids
/// (`max-min+1 != count`, e.g. after deletes), or `n_ranges <= 1` → a single `[min,max]` range;
/// otherwise the contiguous `[min,max]` span is split into `n = min(n_ranges, count)` adjacent
/// ranges (the first `count % n` ranges get one extra row) so every rowid is covered exactly
/// once, in ascending order — the source of the deterministic feature order.
fn row_ranges(
    conn: &Connection,
    table: &str,
    threshold: i64,
    n_ranges: usize,
) -> Result<Vec<(i64, i64)>, String> {
    let (count, min, max): (i64, Option<i64>, Option<i64>) = conn
        .query_row(
            &format!("SELECT COUNT(*), MIN(rowid), MAX(rowid) FROM \"{table}\""),
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|e| format!("gpkg: rowid span for `{table}`: {e}"))?;

    // MIN/MAX are NULL only when the table is empty; `load_partitioned` turns "no ranges" into
    // the "no drawable features" error after the (empty) fan-out.
    let (min, max) = match (min, max) {
        (Some(min), Some(max)) => (min, max),
        _ => return Ok(Vec::new()),
    };

    let contiguous = max - min + 1 == count;
    if count < threshold || !contiguous || n_ranges <= 1 {
        return Ok(vec![(min, max)]);
    }

    let n = (n_ranges as i64).min(count); // never more ranges than rows → no empty ranges
    let base = count / n;
    let rem = count % n;
    let mut ranges = Vec::with_capacity(n as usize);
    let mut lo = min;
    for i in 0..n {
        let len = base + i64::from(i < rem);
        let hi = lo + len - 1;
        ranges.push((lo, hi));
        lo = hi + 1;
    }
    Ok(ranges)
}

/// Decode one already-fetched row into a `Feature`: read `pk_col` → `Feature::fid`, decode
/// `geom_col` via the bespoke WKB decoder (skip on SQL NULL or `Ok(None)` — GeoPackage's
/// empty-geometry flag, or a valid-but-unmodeled type like MultiPoint/GeometryCollection —
/// `Ok(None)` here, not an error), and fold every other column into `Props`. Shared by both
/// `load_range` (load-all, §load-all unchanged) and `GpkgWindowedSource::query_capped`
/// (windowed) — the ONE place a `gpkg` row becomes a `Feature`, so the two paths can never
/// silently diverge (e.g. a column-type mapping tweak made in one and forgotten in the other).
fn decode_feature_row(
    row: &rusqlite::Row<'_>,
    col_names: &[String],
    geom_idx: usize,
    pk_idx: usize,
) -> Result<Option<Feature>, String> {
    let fid: i64 = row
        .get(pk_idx)
        .map_err(|err| format!("reading pk: {err}"))?;

    // Decode the geometry directly from the borrowed BLOB slice — no per-row copy (this path
    // runs once per feature, up to ~56k roads; the decoded `Geometry` is owned, so the `row`
    // borrow ends with this match, freeing the Props loop below to re-borrow).
    let geom = match row.get_ref(geom_idx) {
        Ok(ValueRef::Blob(b)) => match decode_gpkg_geometry(b) {
            Ok(Some(g)) => g,
            Ok(None) => return Ok(None), // empty or valid-but-unmodeled geometry — skip the row
            Err(err) => return Err(format!("fid {fid}: {err}")),
        },
        Ok(ValueRef::Null) => return Ok(None), // no geometry — skip the row, not an error
        Ok(other) => {
            return Err(format!(
                "fid {fid}: geometry column is not a BLOB (got {other:?})"
            ))
        }
        Err(err) => return Err(format!("fid {fid}: reading geometry: {err}")),
    };

    let mut props = Props::new();
    for (i, name) in col_names.iter().enumerate() {
        if i == geom_idx || i == pk_idx {
            continue; // geometry decoded above; the PK becomes `fid`, not a Props entry
        }
        let value_ref = row
            .get_ref(i)
            .map_err(|err| format!("fid {fid}: reading `{name}`: {err}"))?;
        match value_ref {
            ValueRef::Text(t) => {
                props.insert(
                    name.clone(),
                    Value::Str(String::from_utf8_lossy(t).into_owned()),
                );
            }
            ValueRef::Integer(v) => props.insert(name.clone(), Value::Num(v as f64)),
            ValueRef::Real(v) => props.insert(name.clone(), Value::Num(v)),
            ValueRef::Null | ValueRef::Blob(_) => {} // skip, per the T3 brief's column mapping
        }
    }

    Ok(Some(Feature::new(geom, props, fid as u64)))
}

/// Load one contiguous `rowid` range `[lo,hi]` of `table` on its OWN read-only connection
/// (rusqlite `Connection` is `Send` but not `Sync`, so each parallel range needs its own),
/// decoding each row via `decode_feature_row`. `ORDER BY rowid` fixes the per-range order so the
/// concatenated result is fully deterministic. A malformed geometry blob (`Err`) propagates with
/// range/row/fid context.
fn load_range(
    path: &str,
    table: &str,
    geom_col: &str,
    pk_col: &str,
    lo: i64,
    hi: i64,
) -> Result<(Vec<Feature>, [f64; 4]), String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("gpkg: open `{path}` for range [{lo},{hi}]: {e}"))?;

    let mut stmt = conn
        .prepare(&format!(
            "SELECT * FROM \"{table}\" WHERE rowid BETWEEN ?1 AND ?2 ORDER BY rowid"
        ))
        .map_err(|e| format!("gpkg: SELECT range [{lo},{hi}] from `{table}`: {e}"))?;

    let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let geom_idx = col_names
        .iter()
        .position(|c| c == geom_col)
        .ok_or_else(|| format!("gpkg: geometry column `{geom_col}` not found in `{table}`"))?;
    let pk_idx = col_names
        .iter()
        .position(|c| c == pk_col)
        .ok_or_else(|| format!("gpkg: pk column `{pk_col}` not found in `{table}`"))?;

    let mut rows = stmt
        .query([lo, hi])
        .map_err(|e| format!("gpkg: querying range [{lo},{hi}] of `{table}`: {e}"))?;

    let mut features = Vec::new();
    let (mut w, mut s, mut e, mut n) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    let mut row_num: u64 = 0;

    while let Some(row) = rows
        .next()
        .map_err(|err| format!("gpkg: reading row from `{table}`: {err}"))?
    {
        row_num += 1;
        match decode_feature_row(row, &col_names, geom_idx, pk_idx) {
            Ok(Some(f)) => {
                extend_bbox(&mut w, &mut s, &mut e, &mut n, [f.bbox[0], f.bbox[1]]);
                extend_bbox(&mut w, &mut s, &mut e, &mut n, [f.bbox[2], f.bbox[3]]);
                features.push(f);
            }
            Ok(None) => continue,
            Err(err) => {
                return Err(format!("gpkg: row {row_num} in `{table}`: {err}"));
            }
        }
    }

    Ok((features, [w, s, e, n]))
}

/// True iff `path`'s features layer (`layer`, or the first `data_type='features'` row when
/// `None`) carries an OGC R-tree spatial index (`rtree_<table>_<geom_col>`) — the precondition
/// for `GpkgWindowedSource`. Cheap: opens read-only, resolves the table/geom-col via the same
/// metadata lookups `load`/`GpkgWindowedSource::open` use, then a single `sqlite_master` probe —
/// no feature read. Any error along the way (missing file, no features layer, no metadata) →
/// `false`, so the caller falls through to the load-all path, which will surface a precise error
/// of its own rather than this probe swallowing it silently.
pub fn gpkg_has_rtree(path: &str, layer: Option<&str>) -> bool {
    let conn = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let table = match find_features_table(&conn, layer) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let geom_col = match geometry_column(&conn, &table) {
        Ok((c, _)) => c,
        Err(_) => return false,
    };
    let rtree_table = format!("rtree_{table}_{geom_col}");
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1",
        [&rtree_table],
        |_| Ok(()),
    )
    .optional()
    .unwrap_or(None)
    .is_some()
}

/// Max features `GpkgWindowedSource::query`/`query_capped` will decode for one request, bounding
/// per-request memory on a wide/low-zoom query against a dense `.gpkg` — the SQL `LIMIT` this
/// backs makes SQLite itself stop scanning early, so the cap bounds work done, not just the
/// result vector's length. Env `TERRASERVE_GPKG_MAX_QUERY_FEATURES`, default 500_000. Read fresh
/// each call so tests/ops can tune it. Mirrors `fgb::max_query_features`.
fn max_query_features() -> usize {
    std::env::var("TERRASERVE_GPKG_MAX_QUERY_FEATURES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(500_000)
}

/// SQLite type-affinity classification (the 5-rule algorithm from the SQLite docs) collapsed to
/// the two-bucket model `field_schema` needs: `true` for INTEGER/REAL/NUMERIC affinity
/// (`"Number"`), `false` for TEXT/BLOB affinity (`"String"`) — matched on the declared column
/// type name, case-insensitively, exactly as SQLite itself does (a column's *declared* type is
/// advisory in SQLite, but every real GeoPackage writer emits a real, matching type name).
fn sqlite_type_is_numeric(declared_type: &str) -> bool {
    let t = declared_type.to_ascii_uppercase();
    if t.contains("INT") {
        return true; // rule 1: INTEGER affinity
    }
    if t.contains("CHAR") || t.contains("CLOB") || t.contains("TEXT") {
        return false; // rule 2: TEXT affinity
    }
    if t.contains("BLOB") || t.is_empty() {
        return false; // rule 3: BLOB affinity
    }
    if t.contains("REAL") || t.contains("FLOA") || t.contains("DOUB") {
        return true; // rule 4: REAL affinity
    }
    true // rule 5: NUMERIC affinity (the catch-all — NUMERIC/DECIMAL/BOOLEAN/DATE/DATETIME/…)
}

/// The attribute field schema for `table` from `PRAGMA table_info` alone — metadata, never a
/// feature read/decode (the `WindowedSource::field_schema` contract). Excludes `geom_col` and
/// `pk_col` (the PK becomes `Feature::fid`, not a Props entry — same exclusion `decode_feature_row`
/// applies per-row).
fn read_field_schema(
    conn: &Connection,
    table: &str,
    geom_col: &str,
    pk_col: &str,
) -> Result<BTreeMap<String, String>, String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info(\"{table}\")"))
        .map_err(|e| format!("gpkg: PRAGMA table_info(`{table}`): {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("gpkg: PRAGMA table_info(`{table}`): {e}"))?;
    let mut out = BTreeMap::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("gpkg: PRAGMA table_info(`{table}`) row: {e}"))?
    {
        let name: String = row
            .get(1)
            .map_err(|e| format!("gpkg: PRAGMA table_info(`{table}`) name: {e}"))?;
        let decl_type: String = row
            .get(2)
            .map_err(|e| format!("gpkg: PRAGMA table_info(`{table}`) type: {e}"))?;
        if name == geom_col || name == pk_col {
            continue;
        }
        let ty = if sqlite_type_is_numeric(&decl_type) {
            "Number"
        } else {
            "String"
        };
        out.insert(name, ty.to_string());
    }
    Ok(out)
}

/// `[w,s,e,n]` extent for `table`: `gpkg_contents(min_x,min_y,max_x,max_y)` when all four are
/// populated (the common case — every conforming GeoPackage writer sets them), else a fallback
/// aggregate over the R-tree's own stored bboxes (`rtree_table`), which is always populated once
/// `gpkg_has_rtree` gated this path. Errors only if BOTH sources are unusable (e.g. an empty
/// table with no rtree rows either).
fn read_extent(conn: &Connection, table: &str, rtree_table: &str) -> Result<[f64; 4], String> {
    let row: (Option<f64>, Option<f64>, Option<f64>, Option<f64>) = conn
        .query_row(
            "SELECT min_x, min_y, max_x, max_y FROM gpkg_contents WHERE table_name = ?1",
            [table],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(|e| format!("gpkg: gpkg_contents extent lookup for `{table}`: {e}"))?;
    if let (Some(w), Some(s), Some(e), Some(n)) = row {
        return Ok([w, s, e, n]);
    }

    let agg: (Option<f64>, Option<f64>, Option<f64>, Option<f64>) = conn
        .query_row(
            &format!("SELECT min(minx), min(miny), max(maxx), max(maxy) FROM \"{rtree_table}\""),
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(|e| format!("gpkg: rtree extent fallback for `{rtree_table}`: {e}"))?;
    match agg {
        (Some(w), Some(s), Some(e), Some(n)) => Ok([w, s, e, n]),
        _ => Err(format!(
            "gpkg: could not determine an extent for `{table}` (gpkg_contents unset and \
             `{rtree_table}` is empty)"
        )),
    }
}

/// A windowed GeoPackage reader: reads only the features overlapping a request bbox, via the
/// GeoPackage's own OGC R-tree (`rtree_<table>_<geom_col>`) — the counterpart to `GpkgSource`
/// (load-all). Holds no live `Connection` (rusqlite's is `Send` but not `Sync`, while
/// `WindowedSource: Send + Sync` and `query` is called concurrently across requests) — just the
/// plain metadata needed to open a fresh read-only connection per call, so the type is trivially
/// `Send + Sync`.
pub struct GpkgWindowedSource {
    path: String,
    table: String,
    geom_col: String,
    pk_col: String,
    rtree_table: String,
    crs: Option<String>,
    extent: [f64; 4],
    field_schema: BTreeMap<String, String>,
}

impl GpkgWindowedSource {
    /// Open `path`'s features layer (`layer`, or the first `data_type='features'` row when
    /// `None`) as a windowed source: resolve the table/geom-col/CRS/PK via the same metadata
    /// helpers `GpkgSource::load` uses, then read the extent and attribute schema — all from
    /// cheap metadata, no feature row is ever touched. The one-shot connection is dropped before
    /// returning (§ connection strategy A — `query_capped` opens its own per call).
    pub fn open(path: &str, layer: Option<&str>) -> Result<Self, String> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("gpkg: open `{path}`: {e}"))?;

        let table = find_features_table(&conn, layer)?;
        let (geom_col, srs_id) = geometry_column(&conn, &table)?;
        let crs = resolve_crs(&conn, srs_id)?;
        let pk_col = primary_key_column(&conn, &table)?;
        let rtree_table = format!("rtree_{table}_{geom_col}");

        let extent = read_extent(&conn, &table, &rtree_table)?;
        let field_schema = read_field_schema(&conn, &table, &geom_col, &pk_col)?;

        Ok(GpkgWindowedSource {
            path: path.to_string(),
            table,
            geom_col,
            pk_col,
            rtree_table,
            crs,
            extent,
            field_schema,
        })
    }

    /// Decode at most `cap` features overlapping `bbox` (see `WindowedSource::query`, which
    /// delegates here with `cap = max_query_features()`). Opens a fresh read-only connection
    /// (connection strategy A), joins the feature table against its own R-tree on the PK, and
    /// applies `cap` as the SQL `LIMIT` — SQLite itself stops scanning once `cap` rows match, so
    /// the cap bounds work done, not just a post-hoc truncation. The GeoPackage R-tree stores
    /// PER-FEATURE bboxes (unlike FGB's packed-node tree, which only proves node-level overlap),
    /// so the SQL `WHERE` already is the exact per-feature overlap set — no post-decode
    /// re-filter needed. Fail-open: any `rusqlite`/open/prepare/query error, or a column-lookup
    /// miss, returns `Vec::new()` rather than propagating — a request-time read failure must not
    /// panic the server (matches `FgbSource::query_capped`). A single malformed row is skipped,
    /// not fatal to the rest of the batch, same spirit.
    pub fn query_capped(&self, bbox: [f64; 4], cap: usize) -> Vec<Feature> {
        let [w, s, e, n] = bbox;
        let conn = match Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let sql = format!(
            "SELECT t.* FROM \"{}\" t JOIN \"{}\" r ON t.\"{}\" = r.id \
             WHERE r.maxx >= ?1 AND r.minx <= ?2 AND r.maxy >= ?3 AND r.miny <= ?4 LIMIT ?5",
            self.table, self.rtree_table, self.pk_col
        );
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let geom_idx = match col_names.iter().position(|c| c == &self.geom_col) {
            Some(i) => i,
            None => return Vec::new(),
        };
        let pk_idx = match col_names.iter().position(|c| c == &self.pk_col) {
            Some(i) => i,
            None => return Vec::new(),
        };
        let mut rows = match stmt.query(rusqlite::params![w, e, s, n, cap as i64]) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        let mut out = Vec::new();
        loop {
            match rows.next() {
                Ok(Some(row)) => {
                    if let Ok(Some(f)) = decode_feature_row(row, &col_names, geom_idx, pk_idx) {
                        out.push(f);
                    }
                    // A `Ok(None)` (empty/unmodeled geometry) or `Err` (malformed row) both just
                    // skip this row — fail-open per row, matching `FgbSource::query_capped`.
                }
                Ok(None) => break,
                Err(_) => break, // fail-open: a mid-iteration read error returns what we have
            }
        }
        if out.len() >= cap {
            eprintln!(
                "gpkg windowed query: hit cap {cap} features for table `{}` \
                 (raise TERRASERVE_GPKG_MAX_QUERY_FEATURES)",
                self.table
            );
        }
        out
    }
}

impl WindowedSource for GpkgWindowedSource {
    /// Delegates to `query_capped` with `max_query_features()` — the real serving path's entry
    /// point (every WMS/MVT/WMTS request against a windowed `.gpkg` layer reads through this).
    fn query(&self, bbox: [f64; 4]) -> Vec<Feature> {
        self.query_capped(bbox, max_query_features())
    }
    fn full_extent(&self) -> [f64; 4] {
        self.extent
    }
    fn crs(&self) -> Option<&str> {
        self.crs.as_deref()
    }
    fn field_schema(&self) -> BTreeMap<String, String> {
        self.field_schema.clone()
    }
}

#[cfg(test)]
mod windowed_tests {
    use super::*;
    use crate::vector::source::WindowedSource;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicU32, Ordering};

    // -- temp-gpkg builder (self-contained, rusqlite only, CI-safe) --------------------

    fn unique_tmp_gpkg() -> std::path::PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "ts_gpkg_windowed_{}_{}.gpkg",
            std::process::id(),
            n
        ))
    }

    /// The GeoPackageBinary header wrapper: magic + version + flags (little-endian header int,
    /// envelope indicator 0, not empty) + a placeholder srs_id (unused by the decoder — SRS is
    /// resolved from `gpkg_geometry_columns` instead), followed by `wkb_body`.
    fn gp_header(wkb_body: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"GP");
        b.push(0); // version
        b.push(0x01); // flags: header ints little-endian, envelope=0, empty=0
        b.extend_from_slice(&0i32.to_le_bytes()); // srs_id placeholder
        b.extend_from_slice(wkb_body);
        b
    }

    fn point_wkb(x: f64, y: f64) -> Vec<u8> {
        let mut b = Vec::new();
        b.push(1); // little-endian
        b.extend_from_slice(&1u32.to_le_bytes()); // wkbPoint
        b.extend_from_slice(&x.to_le_bytes());
        b.extend_from_slice(&y.to_le_bytes());
        b
    }

    /// A single-ring axis-aligned square WKB Polygon, side `2*half`, centred at `(cx,cy)`.
    fn square_wkb(cx: f64, cy: f64, half: f64) -> Vec<u8> {
        let ring = [
            [cx - half, cy - half],
            [cx + half, cy - half],
            [cx + half, cy + half],
            [cx - half, cy + half],
            [cx - half, cy - half],
        ];
        let mut b = Vec::new();
        b.push(1); // little-endian
        b.extend_from_slice(&3u32.to_le_bytes()); // wkbPolygon
        b.extend_from_slice(&1u32.to_le_bytes()); // numRings
        b.extend_from_slice(&(ring.len() as u32).to_le_bytes());
        for p in ring {
            b.extend_from_slice(&p[0].to_le_bytes());
            b.extend_from_slice(&p[1].to_le_bytes());
        }
        b
    }

    /// The 8 fixture features: `(fid, geom blob, bbox [w,s,e,n], name, rank)`, spread across a
    /// 4x2 grid (20-unit spacing, alternating Point/Polygon) so different probe bboxes select
    /// different, known subsets — the parity-oracle test's whole point.
    fn temp_features() -> Vec<(i64, Vec<u8>, [f64; 4], &'static str, i64)> {
        vec![
            (
                1,
                gp_header(&point_wkb(5.0, 5.0)),
                [5.0, 5.0, 5.0, 5.0],
                "a",
                1,
            ),
            (
                2,
                gp_header(&square_wkb(25.0, 5.0, 1.0)),
                [24.0, 4.0, 26.0, 6.0],
                "b",
                2,
            ),
            (
                3,
                gp_header(&point_wkb(45.0, 5.0)),
                [45.0, 5.0, 45.0, 5.0],
                "c",
                3,
            ),
            (
                4,
                gp_header(&square_wkb(65.0, 5.0, 1.0)),
                [64.0, 4.0, 66.0, 6.0],
                "d",
                4,
            ),
            (
                5,
                gp_header(&point_wkb(5.0, 25.0)),
                [5.0, 25.0, 5.0, 25.0],
                "e",
                5,
            ),
            (
                6,
                gp_header(&square_wkb(25.0, 25.0, 1.0)),
                [24.0, 24.0, 26.0, 26.0],
                "f",
                6,
            ),
            (
                7,
                gp_header(&point_wkb(45.0, 25.0)),
                [45.0, 25.0, 45.0, 25.0],
                "g",
                7,
            ),
            (
                8,
                gp_header(&square_wkb(65.0, 25.0, 1.0)),
                [64.0, 24.0, 66.0, 26.0],
                "h",
                8,
            ),
        ]
    }

    /// Build a temp GeoPackage (the metadata tables `find_features_table`/`geometry_column`/
    /// `resolve_crs`/`primary_key_column` read, plus a `feats` table with the 8 fixture features,
    /// EPSG:4326) under `std::env::temp_dir()`. Populates the OGC R-tree (`rtree_feats_geom`)
    /// iff `with_rtree`. Self-contained (rusqlite only) — no committed binary fixture, CI-safe,
    /// reproducible. Auto-deleted by `TempGpkg`'s `Drop`.
    fn build_temp_gpkg(with_rtree: bool) -> std::path::PathBuf {
        let path = unique_tmp_gpkg();
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).expect("open temp gpkg");
        conn.execute_batch(
            "CREATE TABLE gpkg_spatial_ref_sys (
                 srs_name TEXT NOT NULL,
                 srs_id INTEGER NOT NULL PRIMARY KEY,
                 organization TEXT NOT NULL,
                 organization_coordsys_id INTEGER NOT NULL,
                 definition TEXT NOT NULL,
                 description TEXT
             );
             CREATE TABLE gpkg_contents (
                 table_name TEXT NOT NULL PRIMARY KEY,
                 data_type TEXT NOT NULL,
                 identifier TEXT,
                 description TEXT DEFAULT '',
                 last_change TEXT NOT NULL DEFAULT '',
                 min_x DOUBLE, min_y DOUBLE, max_x DOUBLE, max_y DOUBLE,
                 srs_id INTEGER
             );
             CREATE TABLE gpkg_geometry_columns (
                 table_name TEXT NOT NULL,
                 column_name TEXT NOT NULL,
                 geometry_type_name TEXT NOT NULL,
                 srs_id INTEGER NOT NULL,
                 z TINYINT NOT NULL,
                 m TINYINT NOT NULL,
                 PRIMARY KEY (table_name, column_name)
             );
             CREATE TABLE feats (
                 fid INTEGER PRIMARY KEY AUTOINCREMENT,
                 geom BLOB,
                 name TEXT,
                 rank INTEGER
             );",
        )
        .expect("create metadata tables");

        conn.execute(
            "INSERT INTO gpkg_spatial_ref_sys VALUES ('WGS 84 geodetic', 4326, 'EPSG', 4326, '', NULL)",
            [],
        )
        .expect("insert srs row");
        conn.execute(
            "INSERT INTO gpkg_contents VALUES \
             ('feats','features','feats','','2026-01-01T00:00:00.000Z',5.0,4.0,66.0,26.0,4326)",
            [],
        )
        .expect("insert contents row");
        conn.execute(
            "INSERT INTO gpkg_geometry_columns VALUES ('feats','geom','GEOMETRY',4326,0,0)",
            [],
        )
        .expect("insert geometry_columns row");

        let feats = temp_features();
        for (fid, blob, _bbox, name, rank) in &feats {
            conn.execute(
                "INSERT INTO feats (fid, geom, name, rank) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![fid, blob, name, rank],
            )
            .expect("insert feature row");
        }

        if with_rtree {
            conn.execute_batch(
                "CREATE VIRTUAL TABLE rtree_feats_geom USING rtree(id, minx, maxx, miny, maxy)",
            )
            .expect("create rtree");
            for (fid, _blob, bbox, _name, _rank) in &feats {
                conn.execute(
                    "INSERT INTO rtree_feats_geom (id, minx, maxx, miny, maxy) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![fid, bbox[0], bbox[2], bbox[1], bbox[3]],
                )
                .expect("insert rtree row");
            }
        }

        path
    }

    /// RAII wrapper: deletes the temp `.gpkg` file when the test drops it, whatever the outcome.
    struct TempGpkg(std::path::PathBuf);
    impl TempGpkg {
        fn build(with_rtree: bool) -> Self {
            TempGpkg(build_temp_gpkg(with_rtree))
        }
        fn path(&self) -> &str {
            self.0.to_str().expect("temp path is valid UTF-8")
        }
    }
    impl Drop for TempGpkg {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    // -- 4. Dispatch: gpkg_has_rtree ----------------------------------------------------

    #[test]
    fn gpkg_has_rtree_true_for_mini_fixture() {
        // fixtures/gpkg/mini.gpkg carries `rtree_feats_geom` (built by ogr2ogr) — smoke only.
        assert!(gpkg_has_rtree("fixtures/gpkg/mini.gpkg", None));
    }

    #[test]
    fn gpkg_has_rtree_false_without_an_rtree_table() {
        let g = TempGpkg::build(false);
        assert!(!gpkg_has_rtree(g.path(), None));
    }

    #[test]
    fn gpkg_has_rtree_true_for_a_temp_gpkg_with_rtree() {
        let g = TempGpkg::build(true);
        assert!(gpkg_has_rtree(g.path(), None));
    }

    #[test]
    fn gpkg_has_rtree_false_for_a_missing_file() {
        assert!(!gpkg_has_rtree(
            "fixtures/gpkg/does_not_exist_at_all.gpkg",
            None
        ));
    }

    // -- 2. field_schema — metadata only, no feature scan --------------------------------

    #[test]
    fn windowed_field_schema_from_metadata_no_feature_scan() {
        let g = TempGpkg::build(true);
        let src = GpkgWindowedSource::open(g.path(), None).unwrap();
        let schema = src.field_schema();
        // geom + pk (fid) excluded; `name` TEXT -> String, `rank` INTEGER -> Number.
        assert_eq!(schema.len(), 2, "schema: {schema:?}");
        assert_eq!(schema.get("name").map(String::as_str), Some("String"));
        assert_eq!(schema.get("rank").map(String::as_str), Some("Number"));
    }

    // -- 1. Parity oracle: R-tree window == exact brute-force overlap --------------------

    #[test]
    fn windowed_query_matches_bruteforce_overlap() {
        let g = TempGpkg::build(true);
        let src = GpkgWindowedSource::open(g.path(), None).unwrap();
        let oracle = GpkgSource::load(g.path(), None).unwrap();

        let bruteforce = |bbox: [f64; 4]| -> BTreeSet<u64> {
            oracle
                .features()
                .iter()
                .filter(|f| {
                    f.bbox[2] >= bbox[0]
                        && f.bbox[0] <= bbox[2]
                        && f.bbox[3] >= bbox[1]
                        && f.bbox[1] <= bbox[3]
                })
                .map(|f| f.fid)
                .collect()
        };
        let windowed = |bbox: [f64; 4]| -> BTreeSet<u64> {
            WindowedSource::query(&src, bbox)
                .into_iter()
                .map(|f| f.fid)
                .collect()
        };

        let probes: [[f64; 4]; 5] = [
            [0.0, 0.0, 10.0, 10.0],           // just fid 1 (point at 5,5)
            [0.0, 0.0, 10.0, 30.0],           // left column, both rows: fid 1, 5
            [-1.0, -1.0, 71.0, 31.0],         // everything: all 8
            [20.0, 0.0, 30.0, 30.0],          // the x=25 column, both rows: fid 2, 6
            [1000.0, 1000.0, 1001.0, 1001.0], // nothing
        ];
        for bbox in probes {
            assert_eq!(
                windowed(bbox),
                bruteforce(bbox),
                "R-tree window must equal the brute-force overlap set for bbox {bbox:?}"
            );
        }
    }

    // -- 3. Per-query cap ------------------------------------------------------------------

    #[test]
    fn windowed_query_capped_truncates_to_the_cap() {
        let g = TempGpkg::build(true);
        let src = GpkgWindowedSource::open(g.path(), None).unwrap();
        let bbox = [-1.0, -1.0, 71.0, 31.0]; // covers all 8 fixture features

        let capped = src.query_capped(bbox, 3);
        assert!(
            capped.len() <= 3,
            "cap=3 must bound the result to at most 3, got {}",
            capped.len()
        );

        let uncapped = src.query_capped(bbox, 1000);
        assert_eq!(
            uncapped.len(),
            8,
            "a generous cap must not truncate below the true match count"
        );
    }

    // -- open(): extent + crs ---------------------------------------------------------------

    #[test]
    fn windowed_open_resolves_extent_and_crs() {
        let g = TempGpkg::build(true);
        let src = GpkgWindowedSource::open(g.path(), None).unwrap();
        assert_eq!(WindowedSource::crs(&src), Some("EPSG:4326"));
        assert_eq!(WindowedSource::full_extent(&src), [5.0, 4.0, 66.0, 26.0]);
    }

    #[test]
    fn windowed_open_errs_on_missing_file() {
        assert!(
            GpkgWindowedSource::open("fixtures/gpkg/does_not_exist_at_all.gpkg", None).is_err()
        );
    }
}
