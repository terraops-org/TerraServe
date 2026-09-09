//! GeoPackage feature-table WRITER -- the inverse of `gpkg.rs`, which only reads.
//!
//! Exists for `terraserve extract`: the per-zoom pre-generalized subset has to land somewhere that
//! every backend can read identically, that QGIS can open for eyeballing, and that is diffable and
//! re-bakeable without touching the source database. `gpkg.rs` + `GpkgWindowedSource` already read
//! exactly this format, so a subset written here is servable with no new reader code.
//!
//! What gets written is the minimum a GeoPackage needs to be valid AND to satisfy our own reader:
//! `gpkg_spatial_ref_sys` (the mandatory -1/0 rows plus the layer's own CRS),
//! `gpkg_contents` (`data_type='features'`, which is how `find_features_table` finds it),
//! `gpkg_geometry_columns`, the feature table itself, and an R-tree registered in
//! `gpkg_extensions` so `gpkg_has_rtree` says yes and windowed reads take the indexed path.
//!
//! The R-tree is built ONCE at `finish`, by `INSERT..SELECT` over the finished table, rather than
//! by the spec's per-row triggers. Triggers would fire millions of times during a bulk extract for
//! an index whose final contents are identical either way.

use crate::vector::feature::{Feature, Value};
use crate::vector::wkb::encode_gpkg_geometry;
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::Path;

/// GeoPackage `srs_id` for "undefined geographic" -- the fallback when the layer declares no CRS.
/// The spec REQUIRES rows -1 and 0 to exist in `gpkg_spatial_ref_sys`, so they are always written.
const SRS_UNDEFINED_GEOGRAPHIC: i32 = -1;

/// Rows inserted per transaction. One transaction for the whole extract would hold an unbounded
/// journal for a multi-million-row subset; committing in chunks keeps that bounded while still
/// paying the per-transaction cost once per 50k rows rather than once per row.
const COMMIT_EVERY: u64 = 50_000;

/// Writes a GeoPackage feature table. `create` -> `add` per feature -> `finish`.
///
/// `finish` is not optional and not a destructor: it builds the R-tree and commits the last
/// chunk. Dropping a writer without calling it leaves a file with rows but no spatial index,
/// which reads correctly and scans catastrophically -- so `finish` consumes `self`.
pub struct GpkgWriter {
    conn: Connection,
    table: String,
    /// (column name, is_number) in the order the INSERT binds them.
    cols: Vec<(String, bool)>,
    srs_id: i32,
    written: u64,
    in_tx: bool,
    /// Set by `with_declared_extent`: an extent to stamp INSTEAD of the data's own bounds.
    declared_extent: Option<[f64; 4]>,
    /// Set by `with_zoom_band`: the inclusive zoom band this subset was cut for, stamped into
    /// `gpkg_contents.description` so a server can check what an operator declared against what
    /// the file actually is.
    zoom_band: Option<(u32, u32)>,
    /// Running data extent, written into `gpkg_contents` at `finish`. Not decoration: QGIS frames
    /// the layer from it, and `GpkgWindowedSource::open` REFUSES to open a file whose extent it
    /// cannot determine from either `gpkg_contents` or a non-empty R-tree.
    extent: Option<[f64; 4]>,
}

impl GpkgWriter {
    /// Create `path` (replacing any existing file) with one empty features table.
    ///
    /// `crs` is an `EPSG:<code>` string, matching what `GpkgSource::crs()` returns, so a subset
    /// declares the same CRS its source did. `fields` is the reader's own schema shape
    /// (name -> "Number"/"String"), so a column's declared type round-trips through
    /// `field_schema` unchanged.
    pub fn create(
        path: &Path,
        table: &str,
        crs: Option<&str>,
        fields: &BTreeMap<String, String>,
    ) -> Result<GpkgWriter, String> {
        if table.contains('"') {
            return Err(format!(
                "gpkg_write: refusing table name with a quote: {table}"
            ));
        }
        let _ = std::fs::remove_file(path);
        let conn = Connection::open(path).map_err(|e| format!("gpkg_write: open {path:?}: {e}"))?;

        // Bulk-load pragmas. The extract is a rebuildable derived artifact: if the machine dies
        // mid-write the answer is to run it again, not to recover a torn file, so durability
        // guarantees are not worth their cost here.
        for p in [
            "PRAGMA application_id = 1196444487", // 'GPKG'
            "PRAGMA user_version = 10400",        // GeoPackage 1.4
            "PRAGMA journal_mode = OFF",
            "PRAGMA synchronous = OFF",
        ] {
            conn.execute_batch(p)
                .map_err(|e| format!("gpkg_write: {p}: {e}"))?;
        }

        let srs_id = srs_id_for(crs);
        Self::write_core_tables(&conn, table, srs_id, crs)?;

        let cols: Vec<(String, bool)> = fields
            .iter()
            .map(|(name, ty)| (name.clone(), ty.eq_ignore_ascii_case("Number")))
            .collect();
        for (name, _) in &cols {
            if name.contains('"') {
                return Err(format!(
                    "gpkg_write: refusing column name with a quote: {name}"
                ));
            }
        }

        // `fid INTEGER PRIMARY KEY` is the GeoPackage-required identity column, and being an
        // INTEGER PRIMARY KEY it aliases sqlite's rowid -- which is what the R-tree keys on, so
        // the index and the table agree with no extra bookkeeping.
        let mut ddl =
            format!("CREATE TABLE \"{table}\" (fid INTEGER PRIMARY KEY AUTOINCREMENT, geom BLOB");
        for (name, is_num) in &cols {
            ddl.push_str(&format!(
                ", \"{name}\" {}",
                if *is_num { "REAL" } else { "TEXT" }
            ));
        }
        ddl.push(')');
        conn.execute(&ddl, [])
            .map_err(|e| format!("gpkg_write: create table: {e}"))?;

        conn.execute(
            "INSERT INTO gpkg_contents
               (table_name, data_type, identifier, description, last_change, srs_id)
             VALUES (?1, 'features', ?1, '', strftime('%Y-%m-%dT%H:%M:%fZ','now'), ?2)",
            rusqlite::params![table, srs_id],
        )
        .map_err(|e| format!("gpkg_write: gpkg_contents: {e}"))?;
        conn.execute(
            "INSERT INTO gpkg_geometry_columns
               (table_name, column_name, geometry_type_name, srs_id, z, m)
             VALUES (?1, 'geom', 'GEOMETRY', ?2, 0, 0)",
            rusqlite::params![table, srs_id],
        )
        .map_err(|e| format!("gpkg_write: gpkg_geometry_columns: {e}"))?;

        Ok(GpkgWriter {
            conn,
            table: table.to_string(),
            cols,
            srs_id,
            written: 0,
            in_tx: false,
            extent: None,
            declared_extent: None,
            zoom_band: None,
        })
    }

    /// The four tables every GeoPackage must have before it holds anything.
    fn write_core_tables(
        conn: &Connection,
        _table: &str,
        srs_id: i32,
        crs: Option<&str>,
    ) -> Result<(), String> {
        conn.execute_batch(
            "CREATE TABLE gpkg_spatial_ref_sys (
                 srs_name TEXT NOT NULL, srs_id INTEGER PRIMARY KEY,
                 organization TEXT NOT NULL, organization_coordsys_id INTEGER NOT NULL,
                 definition TEXT NOT NULL, description TEXT);
             CREATE TABLE gpkg_contents (
                 table_name TEXT PRIMARY KEY, data_type TEXT NOT NULL,
                 identifier TEXT UNIQUE, description TEXT DEFAULT '',
                 last_change TEXT NOT NULL, min_x DOUBLE, min_y DOUBLE, max_x DOUBLE, max_y DOUBLE,
                 srs_id INTEGER);
             CREATE TABLE gpkg_geometry_columns (
                 table_name TEXT NOT NULL, column_name TEXT NOT NULL,
                 geometry_type_name TEXT NOT NULL, srs_id INTEGER NOT NULL,
                 z TINYINT NOT NULL, m TINYINT NOT NULL,
                 CONSTRAINT pk_geom_cols PRIMARY KEY (table_name, column_name));
             CREATE TABLE gpkg_extensions (
                 table_name TEXT, column_name TEXT, extension_name TEXT NOT NULL,
                 definition TEXT NOT NULL, scope TEXT NOT NULL,
                 CONSTRAINT ge_tce UNIQUE (table_name, column_name, extension_name));",
        )
        .map_err(|e| format!("gpkg_write: core tables: {e}"))?;

        // The two rows the spec mandates, whatever the layer's own CRS is.
        conn.execute_batch(
            "INSERT INTO gpkg_spatial_ref_sys VALUES
               ('Undefined cartesian SRS', -1, 'NONE', -1, 'undefined', NULL),
               ('Undefined geographic SRS', 0, 'NONE', 0, 'undefined', NULL);",
        )
        .map_err(|e| format!("gpkg_write: mandatory srs rows: {e}"))?;

        // The layer's own CRS, when it has one. `organization`/`organization_coordsys_id` are what
        // `gpkg.rs::resolve_crs` reads back to rebuild the "EPSG:<code>" string.
        if let Some(code) = crs.and_then(epsg_code) {
            conn.execute(
                "INSERT INTO gpkg_spatial_ref_sys VALUES (?1, ?2, 'EPSG', ?3, ?4, NULL)",
                rusqlite::params![format!("EPSG:{code}"), srs_id, code, format!("EPSG:{code}")],
            )
            .map_err(|e| format!("gpkg_write: srs row for {code}: {e}"))?;
        }
        Ok(())
    }

    /// Stamp `extent` in `gpkg_contents` instead of the bounds of the features actually written.
    ///
    /// A per-zoom subset must declare the extent of the layer it was cut FROM. The size threshold
    /// is derived from the layer extent (`layer_area_scale`), so a subset that declared its own
    /// shrunken bounds would compute a tighter threshold than its source and select slightly
    /// differently from the thing it is meant to reproduce -- and the drift compounds when
    /// subsets are chained.
    pub fn with_declared_extent(mut self, extent: Option<[f64; 4]>) -> Self {
        self.declared_extent = extent;
        self
    }

    /// Record the inclusive zoom band this subset was cut for, in `gpkg_contents.description` as
    /// `terraserve-extract zoom=<min>-<max>`.
    ///
    /// Which zooms a subset is valid for is not derivable from its contents: the file holds
    /// whatever passed one threshold, and only the cutter knows which zooms that threshold was
    /// for. Serving it deeper than it was cut for is silent data loss -- features the deeper
    /// threshold would have kept are simply absent, and nothing downstream can tell. Stamping it
    /// makes the band a fact about the file rather than a claim in a config file.
    pub fn with_zoom_band(mut self, band: Option<(u32, u32)>) -> Self {
        self.zoom_band = band;
        self
    }

    /// Append one feature. Geometry is written with no envelope (see `encode_gpkg_geometry`);
    /// the R-tree built at `finish` carries the bounding box instead.
    pub fn add(&mut self, f: &Feature) -> Result<(), String> {
        if !self.in_tx {
            self.conn
                .execute_batch("BEGIN")
                .map_err(|e| format!("gpkg_write: begin: {e}"))?;
            self.in_tx = true;
        }

        let mut sql = format!("INSERT INTO \"{}\" (fid, geom", self.table);
        for (name, _) in &self.cols {
            sql.push_str(&format!(", \"{name}\""));
        }
        sql.push_str(") VALUES (?1, ?2");
        for i in 0..self.cols.len() {
            sql.push_str(&format!(", ?{}", i + 3));
        }
        sql.push(')');

        let blob = encode_gpkg_geometry(&f.geom, self.srs_id);
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(f.fid as i64), Box::new(blob)];
        for (name, is_num) in &self.cols {
            match f.props.get(name) {
                Some(Value::Str(s)) => binds.push(Box::new(s.clone())),
                Some(Value::Num(n)) => binds.push(Box::new(*n)),
                // A missing key and an explicit NULL are the same absence to every reader we
                // have, so both become SQL NULL rather than a zero or an empty string.
                Some(Value::Null) | None => binds.push(Box::new(Option::<String>::None)),
            }
            let _ = is_num; // declared type is set at CREATE; the value binds by its own variant
        }
        let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
        self.conn
            .execute(&sql, refs.as_slice())
            .map_err(|e| format!("gpkg_write: insert fid {}: {e}", f.fid))?;

        if let Some(b) = f.geom.compute_bbox() {
            self.extent = Some(match self.extent {
                None => b,
                Some(e) => [
                    e[0].min(b[0]),
                    e[1].min(b[1]),
                    e[2].max(b[2]),
                    e[3].max(b[3]),
                ],
            });
        }

        self.written += 1;
        if self.written % COMMIT_EVERY == 0 {
            self.conn
                .execute_batch("COMMIT")
                .map_err(|e| format!("gpkg_write: commit: {e}"))?;
            self.in_tx = false;
        }
        Ok(())
    }

    /// Commit, build the R-tree, and report how many features were written.
    ///
    /// The index is filled by one `INSERT..SELECT` over the finished table rather than by the
    /// spec's per-row triggers: the triggers exist to keep an index correct under later edits,
    /// and an extract is written once and never updated. For a multi-million-row subset that is
    /// the difference between a few seconds and firing millions of trigger bodies.
    pub fn finish(mut self) -> Result<u64, String> {
        if self.in_tx {
            self.conn
                .execute_batch("COMMIT")
                .map_err(|e| format!("gpkg_write: final commit: {e}"))?;
            self.in_tx = false;
        }
        if let Some((lo, hi)) = self.zoom_band {
            self.conn
                .execute(
                    "UPDATE gpkg_contents SET description=?2 WHERE table_name=?1",
                    rusqlite::params![self.table, format!("terraserve-extract zoom={lo}-{hi}")],
                )
                .map_err(|err| format!("gpkg_write: gpkg_contents description: {err}"))?;
        }
        if let Some(e) = self.declared_extent.or(self.extent) {
            self.conn
                .execute(
                    "UPDATE gpkg_contents SET min_x=?2, min_y=?3, max_x=?4, max_y=?5
                     WHERE table_name=?1",
                    rusqlite::params![self.table, e[0], e[1], e[2], e[3]],
                )
                .map_err(|err| format!("gpkg_write: gpkg_contents extent: {err}"))?;
        }

        let t = &self.table;
        self.conn
            .execute_batch(&format!(
                "CREATE VIRTUAL TABLE \"rtree_{t}_geom\" USING rtree(id, minx, maxx, miny, maxy);"
            ))
            .map_err(|e| format!("gpkg_write: create rtree: {e}"))?;
        self.fill_rtree()?;
        self.conn
            .execute(
                "INSERT INTO gpkg_extensions
                   (table_name, column_name, extension_name, definition, scope)
                 VALUES (?1, 'geom', 'gpkg_rtree_index',
                         'http://www.geopackage.org/spec120/#extension_rtree', 'write-only')",
                rusqlite::params![self.table],
            )
            .map_err(|e| format!("gpkg_write: register rtree extension: {e}"))?;
        Ok(self.written)
    }

    /// Populate the R-tree from the written geometries. Bounds come from decoding each stored
    /// blob, so the index describes exactly what is in the file rather than what the caller
    /// believed it passed in.
    fn fill_rtree(&self) -> Result<(), String> {
        let t = &self.table;
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT fid, geom FROM \"{t}\""))
            .map_err(|e| format!("gpkg_write: read back for rtree: {e}"))?;
        let rows: Vec<(i64, Vec<u8>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| format!("gpkg_write: rtree scan: {e}"))?
            .collect::<Result<_, _>>()
            .map_err(|e| format!("gpkg_write: rtree row: {e}"))?;
        drop(stmt);

        self.conn
            .execute_batch("BEGIN")
            .map_err(|e| format!("gpkg_write: rtree begin: {e}"))?;
        for (fid, blob) in rows {
            let g = match crate::vector::wkb::decode_gpkg_geometry(&blob) {
                Ok(Some(g)) => g,
                // An empty geometry indexes nothing: it overlaps no window, so leaving it out of
                // the R-tree is correct, not a gap.
                Ok(None) => continue,
                Err(e) => return Err(format!("gpkg_write: re-decoding fid {fid}: {e}")),
            };
            let Some(b) = g.compute_bbox() else { continue };
            self.conn
                .execute(
                    &format!(
                        "INSERT INTO \"rtree_{t}_geom\" (id, minx, maxx, miny, maxy)
                         VALUES (?1, ?2, ?3, ?4, ?5)"
                    ),
                    rusqlite::params![fid, b[0], b[2], b[1], b[3]],
                )
                .map_err(|e| format!("gpkg_write: rtree insert fid {fid}: {e}"))?;
        }
        self.conn
            .execute_batch("COMMIT")
            .map_err(|e| format!("gpkg_write: rtree commit: {e}"))?;
        Ok(())
    }
}

/// `"EPSG:3035"` -> `3035`. Anything else (a WKT string, a bare name) has no EPSG code to record.
fn epsg_code(crs: &str) -> Option<i32> {
    crs.strip_prefix("EPSG:")
        .or_else(|| crs.strip_prefix("epsg:"))
        .and_then(|c| c.trim().parse::<i32>().ok())
        .filter(|&c| c > 0)
}

/// GeoPackage uses the EPSG code as `srs_id` by convention, which keeps a written file readable
/// by anything that looks the code up directly. With no usable CRS we fall back to the spec's
/// undefined-geographic row rather than inventing an id.
fn srs_id_for(crs: Option<&str>) -> i32 {
    crs.and_then(epsg_code).unwrap_or(SRS_UNDEFINED_GEOGRAPHIC)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::feature::{Feature, Geometry, Props, Value};
    use crate::vector::gpkg::{gpkg_has_rtree, GpkgSource, GpkgWindowedSource};
    use crate::vector::source::{FeatureSource, WindowedSource};
    use std::collections::BTreeMap;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ts_gpkgw_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d.join(name)
    }

    fn fields() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), "String".to_string());
        m.insert("area_m2".to_string(), "Number".to_string());
        m
    }

    fn feat(fid: u64, g: Geometry, name: &str, area: f64) -> Feature {
        let mut p = Props::new();
        p.insert("name".to_string(), Value::Str(name.to_string()));
        p.insert("area_m2".to_string(), Value::Num(area));
        Feature::new(g, p, fid)
    }

    /// The whole point: what we write, our own reader reads back. `GpkgSource::load` is a real
    /// oracle -- it is the code that opens actual GeoPackages in production.
    #[test]
    fn what_we_write_our_own_reader_reads_back() {
        let path = tmp("roundtrip.gpkg");
        let _ = std::fs::remove_file(&path);
        let p = path.to_string_lossy().to_string();

        let square = Geometry::Polygon(vec![vec![
            [0.0, 0.0],
            [10.0, 0.0],
            [10.0, 10.0],
            [0.0, 10.0],
            [0.0, 0.0],
        ]]);
        let line = Geometry::LineString(vec![[100.0, 100.0], [110.0, 120.0]]);

        let mut w = GpkgWriter::create(&path, "subset", Some("EPSG:3035"), &fields()).unwrap();
        w.add(&feat(1, square.clone(), "the square", 100.0))
            .unwrap();
        w.add(&feat(2, line.clone(), "the line", 0.0)).unwrap();
        let n = w.finish().unwrap();
        assert_eq!(n, 2, "finish reports what it wrote");

        let src = GpkgSource::load(&p, None).unwrap();
        assert_eq!(src.crs(), Some("EPSG:3035"), "CRS must survive");
        let got = src.features();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].geom, square, "geometry must survive");
        assert_eq!(got[1].geom, line);
        assert_eq!(got[0].props.get_str("name"), Some("the square"));
        assert_eq!(got[0].props.get_f64("area_m2"), Some(100.0));
        assert_eq!(got[0].fid, 1, "the source fid must be preserved");
        assert_eq!(got[1].fid, 2);
    }

    /// Windowed reads are the reason the subset is worth writing at all: the bake reads a tile's
    /// bbox, not the whole file. Without a registered R-tree the reader silently falls back to a
    /// full scan, so this asserts BOTH that the index exists and that it selects correctly.
    #[test]
    fn the_rtree_exists_and_a_bbox_query_returns_only_what_overlaps() {
        let path = tmp("windowed.gpkg");
        let _ = std::fs::remove_file(&path);
        let p = path.to_string_lossy().to_string();

        let near = Geometry::Point([1.0, 1.0]);
        let far = Geometry::Point([500.0, 500.0]);
        let mut w = GpkgWriter::create(&path, "subset", Some("EPSG:4326"), &fields()).unwrap();
        w.add(&feat(1, near, "near", 1.0)).unwrap();
        w.add(&feat(2, far, "far", 2.0)).unwrap();
        w.finish().unwrap();

        assert!(
            gpkg_has_rtree(&p, None),
            "no R-tree registered: every windowed read would silently become a full scan"
        );

        let ws = GpkgWindowedSource::open(&p, None).unwrap();
        let hit = ws.query([0.0, 0.0, 10.0, 10.0]).unwrap();
        assert_eq!(hit.len(), 1, "only the near point overlaps");
        assert_eq!(hit[0].props.get_str("name"), Some("near"));

        let none = ws.query([1000.0, 1000.0, 1010.0, 1010.0]).unwrap();
        assert!(none.is_empty(), "a window over nothing returns nothing");

        let both = ws.query([-1.0, -1.0, 1000.0, 1000.0]).unwrap();
        assert_eq!(both.len(), 2, "a window over everything returns everything");
    }

    /// Every geometry variant the model has, through the file and back. A subset of a real OSM
    /// layer is mostly MultiPolygon; getting that wrong would be discovered only after a
    /// multi-hour extract.
    #[test]
    fn every_geometry_variant_survives_the_file() {
        let path = tmp("variants.gpkg");
        let _ = std::fs::remove_file(&path);
        let p = path.to_string_lossy().to_string();

        let geoms = vec![
            Geometry::Point([1.0, 2.0]),
            Geometry::LineString(vec![[0.0, 0.0], [1.0, 1.0]]),
            Geometry::Polygon(vec![vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]]),
            Geometry::MultiLineString(vec![vec![[0.0, 0.0], [2.0, 2.0]]]),
            Geometry::MultiPolygon(vec![vec![vec![
                [0.0, 0.0],
                [3.0, 0.0],
                [3.0, 3.0],
                [0.0, 0.0],
            ]]]),
        ];
        let mut w = GpkgWriter::create(&path, "subset", Some("EPSG:3035"), &fields()).unwrap();
        for (i, g) in geoms.iter().enumerate() {
            w.add(&feat(i as u64 + 1, g.clone(), "g", 0.0)).unwrap();
        }
        w.finish().unwrap();

        let src = GpkgSource::load(&p, None).unwrap();
        let got: Vec<_> = src.features().iter().map(|f| f.geom.clone()).collect();
        assert_eq!(got, geoms, "a variant was mangled by the write/read cycle");
    }

    /// A NULL attribute must stay NULL rather than becoming the string "null" or a 0.0 -- a
    /// silent type change would show up much later as a style rule that stops matching.
    #[test]
    fn a_null_attribute_stays_null() {
        let path = tmp("nulls.gpkg");
        let _ = std::fs::remove_file(&path);
        let p = path.to_string_lossy().to_string();

        let mut props = Props::new();
        props.insert("name".to_string(), Value::Null);
        props.insert("area_m2".to_string(), Value::Num(7.0));
        let f = Feature::new(Geometry::Point([0.0, 0.0]), props, 1);

        let mut w = GpkgWriter::create(&path, "subset", Some("EPSG:4326"), &fields()).unwrap();
        w.add(&f).unwrap();
        w.finish().unwrap();

        let src = GpkgSource::load(&p, None).unwrap();
        let got = &src.features()[0];
        assert_eq!(
            got.props.get_str("name"),
            None,
            "NULL must not become a string"
        );
        assert_eq!(got.props.get_f64("area_m2"), Some(7.0));
    }

    /// A subset must be able to declare its SOURCE's extent rather than its own.
    ///
    /// This is not cosmetic. `layer_area_scale` derives the per-zoom size threshold from the
    /// layer's extent, so a subset whose extent shrank (because small edge features were gated
    /// away) computes a slightly TIGHTER threshold than the source did -- and then selects
    /// slightly differently from the very thing it is supposed to reproduce. Measured on Swiss
    /// buildings: a 0.0355% extent change moved 2 features out of 4063 at z7.
    ///
    /// Inheriting the source extent makes `area_scale` identical, which makes the thresholds
    /// identical, which is what lets a subset be re-baked -- or chained into a shallower subset --
    /// without drifting.
    #[test]
    fn a_subset_can_declare_its_sources_extent_instead_of_its_own() {
        let path = tmp("declared.gpkg");
        let _ = std::fs::remove_file(&path);

        // Data occupies a small box; the source layer it was cut from is much larger.
        let source_extent = [-100.0, -50.0, 100.0, 50.0];
        let mut w = GpkgWriter::create(&path, "subset", Some("EPSG:3035"), &fields())
            .unwrap()
            .with_declared_extent(Some(source_extent));
        w.add(&feat(1, Geometry::Point([1.0, 1.0]), "a", 0.0))
            .unwrap();
        w.add(&feat(2, Geometry::Point([2.0, 2.0]), "b", 0.0))
            .unwrap();
        w.finish().unwrap();

        let conn = rusqlite::Connection::open(&path).unwrap();
        let got: (f64, f64, f64, f64) = conn
            .query_row(
                "SELECT min_x, min_y, max_x, max_y FROM gpkg_contents WHERE table_name='subset'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            [got.0, got.1, got.2, got.3],
            source_extent,
            "the declared source extent must win over the data's own bounds"
        );
    }

    /// The layer extent must reach `gpkg_contents`. QGIS frames the layer from it, and
    /// `GpkgWindowedSource::open` REFUSES to open a file whose extent it cannot determine -- so
    /// this is load-bearing, not metadata politeness.
    #[test]
    fn the_layer_extent_is_written_to_gpkg_contents() {
        let path = tmp("extent.gpkg");
        let _ = std::fs::remove_file(&path);
        let p = path.to_string_lossy().to_string();

        let mut w = GpkgWriter::create(&path, "subset", Some("EPSG:3035"), &fields()).unwrap();
        w.add(&feat(1, Geometry::Point([10.0, 20.0]), "a", 0.0))
            .unwrap();
        w.add(&feat(2, Geometry::Point([-5.0, 40.0]), "b", 0.0))
            .unwrap();
        w.finish().unwrap();

        let conn = rusqlite::Connection::open(&path).unwrap();
        let (minx, miny, maxx, maxy): (f64, f64, f64, f64) = conn
            .query_row(
                "SELECT min_x, min_y, max_x, max_y FROM gpkg_contents WHERE table_name='subset'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("gpkg_contents must carry the extent");
        assert_eq!([minx, miny, maxx, maxy], [-5.0, 20.0, 10.0, 40.0]);
        drop(conn);

        assert!(GpkgWindowedSource::open(&p, None).is_ok());
    }

    /// An empty extract -- a zoom band whose threshold selects nothing -- produces a file that
    /// NEITHER reader will open. That is correct (zero features have no extent), and it is the
    /// contract `terraserve extract` has to respect: an empty band must fail loudly at extract
    /// time, because discovering it later means a server that will not start, or the standing
    /// trap where a gate tuned for deep zoom silently yields nothing at the shallow end and
    /// becomes a live 500 a real visitor hits.
    #[test]
    fn an_empty_extract_is_unservable_by_both_readers_so_extract_must_refuse_one() {
        let path = tmp("empty.gpkg");
        let _ = std::fs::remove_file(&path);
        let p = path.to_string_lossy().to_string();

        let w = GpkgWriter::create(&path, "subset", Some("EPSG:3035"), &fields()).unwrap();
        assert_eq!(
            w.finish().unwrap(),
            0,
            "an empty extract still finishes cleanly"
        );

        match GpkgSource::load(&p, None) {
            Ok(_) => panic!("load-all must refuse an empty layer"),
            Err(e) => assert!(e.contains("no drawable features"), "unexpected: {e}"),
        }
        match GpkgWindowedSource::open(&p, None) {
            Ok(_) => panic!("windowed open must refuse a layer with no determinable extent"),
            Err(e) => assert!(
                e.contains("could not determine an extent"),
                "unexpected: {e}"
            ),
        }
    }
}
