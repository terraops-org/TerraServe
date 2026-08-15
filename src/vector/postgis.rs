// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! PostGIS as a windowed vector source.
//!
//! PostGIS stores and indexes; TerraServe transforms, generalizes, styles and encodes. The only
//! PostGIS functions used are `ST_AsBinary` (output) and `ST_MakeEnvelope` (the filter). See
//! `docs/superpowers/specs/2026-08-06-postgis-vector-source-design.md` section 1 for why
//! `ST_Transform` and `ST_Simplify` are refused even though both would be faster.

use std::collections::BTreeMap;

use super::feature::{Feature, Props, Value};
use super::pg_uri::{parse_postgis_uri, PgTarget};
use super::source::WindowedSource;
use super::wkb;

/// Quote a SQL identifier by doubling any embedded double quotes.
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// The one statement this module issues. Kept a pure function so the forbidden-pushdown test
/// can assert on it without a database.
///
/// `min_area_src` is the per-zoom minimum feature area in SOURCE-CRS units², straight from
/// [`crate::vector::mvt::min_area_src_for_zoom`]. `0.0` (the default, and the only value the
/// non-tile paths ever pass) emits the statement byte-for-byte as it was before the gate existed
/// — see [`size_gate_sql`] for the predicate and why row-filtering is not one of the forbidden
/// pushdowns.
fn build_sql(
    schema: &str,
    table: &str,
    geom_col: &str,
    srid: i32,
    columns: &[String],
    limit: usize,
    min_area_src: f64,
) -> String {
    let mut sel = format!("ST_AsBinary({})", quote_ident(geom_col));
    for c in columns {
        sel.push_str(&format!(", {}", quote_ident(c)));
    }
    format!(
        "SELECT {sel} FROM {}.{} WHERE {} && ST_MakeEnvelope($1,$2,$3,$4,{srid}){} LIMIT {limit}",
        quote_ident(schema),
        quote_ident(table),
        quote_ident(geom_col),
        size_gate_sql(geom_col, min_area_src),
    )
}

/// The zoom-aware size predicate, as an ` AND (...)` fragment — or the EMPTY STRING when the gate
/// is off, which is what keeps the default statement byte-identical to the pre-gate one.
///
/// **This is not one of the forbidden pushdowns, and it must not be "cleaned up" as if it were.**
/// The spec (§1) refuses `ST_AsMVT`, `ST_Simplify` and `ST_Transform` because each concedes the
/// RENDERING to the database — the geometry that comes back would be the database's, not ours.
/// This concedes nothing: it transforms no geometry and returns no new geometry, it only declines
/// to SEND rows. That is the same category as the `&&` bbox filter directly above it, which is
/// what a spatial database is for. At z5 a Poland building is ~1/400 of a pixel; without this
/// those 17.8M rows are serialized, sent over the wire, WKB-decoded into `Feature`s, and then
/// discarded by the identical gate in `mvt/tile.rs`.
///
/// **The predicate is the literal translation of that Rust gate** (`mvt/tile.rs`, the candidate
/// loop): `!(f.area > 0.0 && f.area < min_area_src)`. Two consequences worth being explicit
/// about, because both are load-bearing:
///
/// * **`ST_Area` is 0 for every LineString and every Point**, so the `> 0` guard exempts them —
///   as the Rust gate's `f.area > 0.0` does. A naive `ST_Area(geom) >= t` would delete every road
///   and every place and serve a 200 OK with an empty map. Lines are not thinned here BY DESIGN:
///   thinning them would be a new generalization policy that the file/GeoPackage backends do not
///   apply, and the same layer must not render differently just because it is stored in PostGIS.
///   If line thinning is ever wanted, it belongs in the Rust gate first, for every backend.
/// * **`ST_Area(ST_Envelope(geom))` was rejected**, despite looking like the tidy branch-free
///   answer. It is 0 for an axis-aligned line (a degenerate envelope) but large for a diagonal one
///   of the same length, so it would delete north-south and east-west streets while keeping the
///   diagonals — a wrong map that looks plausible.
///
/// Because the SQL drops a strict SUBSET of what the Rust gate drops, turning this on cannot
/// change a rendered tile; it only makes it cheaper. (`ST_Area` on a polygon is exact, and so is
/// `Feature::area`'s shoelace, but they need not agree bit-for-bit on a feature sitting exactly at
/// the threshold — such a feature is sub-pixel either way.)
///
/// A negative, zero or non-finite threshold fails OPEN (no fragment), matching
/// [`crate::vector::mvt::min_area_src_for_zoom`]'s own off-switch. Emitting `< NaN` would be false
/// for every row and blank the layer.
///
/// The threshold is a formatted LITERAL, not a `$5` bind parameter, for two reasons: a bind would
/// make the gate-off statement differ from the pre-gate one (requirement: byte-identical), and a
/// generic plan would then evaluate `ST_Area` per row even when the gate is off. `{:?}` on `f64`
/// is the shortest round-tripping representation and always yields a valid SQL numeric literal
/// (`47.5`, `1e30`) — never a locale-dependent comma, and never `NaN`/`inf`, which the guard above
/// has already excluded.
///
/// ⚠ `ST_Area` is NOT index-assisted: Postgres evaluates it per row that the GiST bbox filter
/// already matched. That is expected and the win stands anyway, because the saving is in the rows
/// never transferred and never decoded. See `docs/postgis-layers.md` for the functional-index note.
fn size_gate_sql(geom_col: &str, min_area_src: f64) -> String {
    if !(min_area_src > 0.0) || !min_area_src.is_finite() {
        return String::new();
    }
    let g = quote_ident(geom_col);
    format!(" AND NOT (ST_Area({g}) > 0 AND ST_Area({g}) < {min_area_src:?})")
}

/// Trivial by design: it exists so a test can pin that the extent comes from config, never a query.
fn extent_of(configured: [f64; 4]) -> [f64; 4] {
    configured
}

/// Per-query feature cap. Mirrors `gpkg::max_query_features` / `fgb::max_query_features`, which
/// are each private to their own module. Read fresh each call so ops can tune it.
fn max_query_features() -> usize {
    std::env::var("TERRASERVE_PG_MAX_QUERY_FEATURES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(500_000)
}

/// Per-connection `statement_timeout`, in milliseconds. A runaway low-zoom query then dies at the
/// database instead of parking a blocking-pool thread that holds a `--max-inflight` permit.
/// Parsed (not passed through) because the value is spliced into the connection's `options`
/// string: a typo would otherwise make every connection fail with a backend syntax error rather
/// than fall back to the default.
fn statement_timeout_ms() -> u64 {
    std::env::var("TERRASERVE_PG_STATEMENT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(30_000)
}

/// How long to wait for a TCP connect + authentication handshake.
///
/// `statement_timeout` bounds EXECUTION only; it does nothing while a connection is being
/// established. `tokio_postgres` leaves `connect_timeout` unset by default, so a database that
/// accepts the SYN and then never answers (a black-holing firewall, a host that vanished, an
/// overloaded server) blocks forever. That hangs `open()` at startup, and — because deadpool
/// creates pool connections lazily — it also hangs the first tile that needs a new one.
fn connect_timeout_ms() -> u64 {
    std::env::var("TERRASERVE_PG_CONNECT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(10_000)
}

/// How long `pool.get()` may wait for a free connection.
///
/// This is the one that matters most under load. `query()` runs inside `spawn_blocking` while
/// holding a `--max-inflight` permit, so an unbounded wait here parks a blocking-pool thread AND
/// its permit indefinitely — the exact failure the `statement_timeout` guard was introduced to
/// prevent, just moved one step earlier. deadpool's `Timeouts::default()` is all-`None`, so this
/// has to be set explicitly.
fn pool_wait_timeout_ms() -> u64 {
    std::env::var("TERRASERVE_PG_POOL_WAIT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(5_000)
}

/// `None` when the pool is big enough. A pool smaller than `--max-inflight` silently BECOMES
/// admission control: every request past the pool's size queues in `pool.get()` (bounded by
/// `TERRASERVE_PG_POOL_WAIT_TIMEOUT_MS`, then fails) regardless of how many `--max-inflight`
/// permits are free, so the operator's admission-control reasoning is quietly wrong. Pure and
/// `pub(crate)` so `layer/mod.rs` (which is where BOTH numbers -- the pool's configured size and
/// the resolved `--max-inflight` -- are known at once) can call it without a database.
pub(crate) fn pool_sizing_warning(pool_size: usize, max_inflight: usize) -> Option<String> {
    (pool_size < max_inflight).then(|| {
        format!(
            "postgis: connection pool ({pool_size}) is smaller than --max-inflight \
             ({max_inflight}). The pool is now the real admission-control limit. Size the pool \
             to the DATABASE (Postgres degrades past roughly 2-4x its core count) and lower \
             --max-inflight to match, or put PgBouncer in front."
        )
    })
}

/// `None` when the operator's declared CRS agrees with the table's own SRID.
///
/// A `postgis://` layer is the one source that always knows its own CRS authoritatively, so a
/// disagreement is an operator mistake, never something to paper over. It also cannot be *made* to
/// work: geometry is never transformed in SQL (module docs, design §1), so a declared CRS that
/// differs from the table's does not reproject anything — it only mislabels what comes back, and
/// makes `envelope_in_srid` an identity transform between two CRSs that are not the same, which
/// tests a query box expressed in one unit against geometry stored in another. Degrees against
/// metres selects nothing at all: zero features, HTTP 200, a blank map, and not one log line.
///
/// Pure and `pub(crate)` so `layer/mod.rs` — the one place the declaration and the table's SRID are
/// both in hand — can call it without a database, exactly like `pool_sizing_warning`.
pub(crate) fn crs_mismatch_error(
    declared: &str,
    table_crs: &str,
    relation: &str,
) -> Option<String> {
    if crs_equivalent(declared, table_crs) {
        return None;
    }
    Some(format!(
        "declared source CRS {declared} disagrees with the PostGIS table's own SRID {table_crs} \
         (from {relation}). TerraServe never transforms geometry in SQL, so the declaration \
         cannot reproject anything: the bbox filter would be built by mapping {table_crs} to \
         {table_crs} (an identity) from coordinates that are actually {declared}, which typically \
         matches NO rows and serves a blank map behind a 200 OK. Either drop the declaration (the \
         SRID is auto-detected) or set it to {table_crs}. If {table_crs} is itself wrong, correct \
         it with `?srid=` on the layer URI so both agree."
    ))
}

/// `CRS:84` and `EPSG:4326` name the same datum and differ only in axis order, and PostGIS stores
/// 4326 in lon/lat — the order the vector path already assumes everywhere — so accepting the pair
/// is not laxity, it is avoiding a false positive on a config that is genuinely correct.
fn crs_equivalent(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        if s.eq_ignore_ascii_case("CRS:84") {
            "EPSG:4326".to_string()
        } else {
            s.to_ascii_uppercase()
        }
    };
    norm(a) == norm(b)
}

/// Run one startup-time metadata operation on a TEMPORARY current-thread runtime, on a thread that
/// provably has no runtime of its own.
///
/// Three facts force this shape, and each produces a confusing failure if ignored.
/// (1) There is no tokio runtime yet — `server::run` builds it AFTER layer construction, so
///     `Handle::current()` here would fail at startup.
/// (2) A connection taken from the POOL now would be bound to this temporary runtime and die when
///     it drops, surfacing as a mysterious "connection closed" on the first tile rather than as an
///     error at startup. Hence a STANDALONE connection inside, every time.
/// (3) Building a runtime while one is already in scope PANICS ("Cannot start a runtime from
///     within a runtime"). Layer construction is off-runtime in `server::run`, but a
///     `#[tokio::test]` (or any future async config path) is not, and a constructor that panics
///     depending on its caller's context is a trap. `std::thread::scope` moves the temporary
///     runtime onto a thread that provably has none, and still lets the closure borrow.
fn off_runtime<T: Send>(
    what: &str,
    f: impl FnOnce(&tokio::runtime::Runtime) -> Result<T, String> + Send,
) -> Result<T, String> {
    std::thread::scope(|s| {
        s.spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("postgis: temporary runtime: {e}"))?;
            f(&rt)
        })
        .join()
        .map_err(|_| format!("postgis: {what} panicked"))?
    })
}

pub struct PostgisSource {
    pool: deadpool_postgres::Pool,
    target: PgTarget,
    srid: i32,
    geom_col: String,
    /// The attribute columns to `SELECT` alongside the geometry, in `SELECT` order. Set by
    /// [`PostgisSource::set_columns`] and checked against the database by
    /// [`PostgisSource::validate_columns`] — a name the relation does not have makes the ONE
    /// statement this module issues unrunnable, which the render path can only express as an empty
    /// window. Validated at startup for exactly that reason.
    columns: Vec<String>,
    extent: [f64; 4],
    /// The table's own CRS, `EPSG:<srid>` — the CRS the `ST_MakeEnvelope` filter must be
    /// expressed in for the GiST index to be usable.
    table_crs: String,
    /// The CRS the `bbox` handed to [`WindowedSource::query`] arrives in, i.e. the *layer's*
    /// declared source CRS (`render_vector_from` reprojects the request bbox into it before
    /// calling `features_in`).
    ///
    /// Set to `table_crs`, which makes the envelope transform an identity — and that is now
    /// GUARANTEED, not merely assumed. Geometry is never transformed on the way out (§1), so a
    /// layer declaring anything other than the table's SRID would misplace every feature no matter
    /// what the filter did; the identity used to hide that, turning a mismatch into a degree-valued
    /// box tested against metre geometry, i.e. zero rows and a blank map with no log line.
    /// [`crs_mismatch_error`] now refuses such a layer at startup, so the two CRSs cannot disagree
    /// by the time anything queries. The transform stays on the real path so the four-corner rule
    /// is exercised rather than assumed, and so the day a layer *can* declare a different query CRS
    /// is a one-line change here instead of a new code path.
    bbox_crs: String,
}

impl PostgisSource {
    /// Open a `postgis://` layer URI. `extent` is the operator-declared extent, used verbatim —
    /// no `ST_Extent`/`ST_EstimatedExtent` probe at startup (design §6).
    pub fn open(spec: &str, extent: [f64; 4]) -> Result<Self, String> {
        let target = parse_postgis_uri(spec, &|k| std::env::var(k).ok())?;

        // Parse the connection string ONCE, up front. This is the single error that can quote a
        // fragment of the DSN back at us — and a fragment of the DSN can be a fragment of the
        // password (a password containing a space splits into a bogus `unknown option` key). Every
        // failure after this point is a genuine connect/auth/pool error that never sees the
        // string, so those stay printed verbatim and keep their diagnostic value.
        let mut pg_cfg = parse_dsn(&target)?;
        let connect_timeout = std::time::Duration::from_millis(connect_timeout_ms());
        // Bounds the handshake for the standalone metadata connection below. The pool gets the
        // same bound separately, via `deadpool_postgres::Config::connect_timeout`.
        pg_cfg.connect_timeout(connect_timeout);

        // Metadata resolution runs on a TEMPORARY runtime over a STANDALONE connection, on its own
        // thread — see `off_runtime` for the three facts that force it. Connect once, ask, drop.
        // The pool stays cold until the server runtime takes it.
        let (srid, geom_col) = off_runtime("metadata resolution", |rt| {
            rt.block_on(resolve_metadata(&target, &pg_cfg))
        })?;

        let mut cfg = deadpool_postgres::Config::new();
        cfg.url = Some(target.dsn.expose().to_string());
        cfg.options = Some(format!("-c statement_timeout={}", statement_timeout_ms()));
        cfg.connect_timeout = Some(connect_timeout);
        // Every deadpool timeout defaults to `None`, i.e. wait forever. `query()` holds a
        // blocking-pool thread and a `--max-inflight` permit for the whole of `pool.get()`, so
        // "forever" here is how a database outage turns into a wedged server rather than into
        // failing tiles. `wait` bounds queueing for a free connection; `create` bounds
        // establishing a new one (belt-and-braces over `connect_timeout`, which does not cover a
        // TLS/startup stall); `recycle` bounds the liveness check deadpool runs on a connection
        // it is handing back out, which otherwise blocks on a half-dead TCP connection.
        cfg.pool = Some(deadpool_postgres::PoolConfig {
            timeouts: deadpool_postgres::Timeouts {
                wait: Some(std::time::Duration::from_millis(pool_wait_timeout_ms())),
                create: Some(connect_timeout),
                recycle: Some(connect_timeout),
            },
            ..deadpool_postgres::PoolConfig::default()
        });
        // `parse_dsn` has already rejected an unparseable connection string, so `create_pool`'s
        // remaining failures (a missing/empty dbname, pool construction) never quote it.
        let pool = cfg
            .create_pool(
                Some(deadpool_postgres::Runtime::Tokio1),
                tokio_postgres::NoTls,
            )
            .map_err(|e| format!("postgis: pool for {}.{}: {e}", target.schema, target.table))?;

        let table_crs = format!("EPSG:{srid}");
        Ok(Self {
            pool,
            srid,
            geom_col,
            // Geometry-only until `set_columns` runs. `open()` stays a two-argument constructor
            // (a frozen contract the live integration tests call directly) — the attribute set
            // to fetch depends on the STYLE, which `layer/mod.rs` only finishes parsing after
            // `open()` returns, so it is supplied as a follow-up call rather than a parameter.
            columns: Vec::new(),
            extent,
            bbox_crs: table_crs.clone(),
            table_crs,
            target,
        })
    }

    /// Set the attribute columns to `SELECT` and decode alongside the geometry, in the given
    /// order. Called once, right after `open()`, with the field set the layer's style actually
    /// reads (`Style::referenced_fields`) plus any explicit `columns:` from the layer config.
    ///
    /// Pure — it does no I/O and validates nothing. [`validate_columns`](Self::validate_columns)
    /// is the half that checks the names against the database, and it MUST be called after this;
    /// see its doc comment for what happens if it is not.
    pub fn set_columns(&mut self, columns: Vec<String>) {
        self.columns = columns;
    }

    /// Prove, at STARTUP, that the statement `query()` will issue can actually run.
    ///
    /// This exists because of the worst failure mode this source had. `build_sql` names the
    /// configured columns as quoted identifiers, and `quote_ident` preserves case, so a style that
    /// says `<PropertyName>Name</PropertyName>` against a column called `name` emits
    /// `SELECT ST_AsBinary("geom"), "Name"`. Postgres rejects that with 42703, `fetch` returns
    /// `Err`, and `query()`'s error arm logs one line and returns an empty `Vec` — which the render
    /// path cannot tell apart from a genuinely empty window. The server starts, GetCapabilities
    /// looks right, and every GetMap and every tile is a blank 200 OK, for the entire life of the
    /// process, from a single typo. That is the same shape as the `TERRASERVE_FGB_MAX_QUERY_FEATURES`
    /// truncation trap this project has already been burned by once.
    ///
    /// **Why a hard error and not a warn-and-drop.** `.gpkg`/`.fgb` degrade a missing field to
    /// `Value::Null` and still draw geometry, so dropping the column would be the more "consistent"
    /// choice. It is the wrong one here, on three counts. Every other PostGIS metadata problem on
    /// this path already refuses to start and names the fix (`extent:` missing, SRID 0, two
    /// geometry columns) — that is this source's established contract, and it is affordable because
    /// PostGIS, unlike a file header, can be *asked* and answers authoritatively. A dropped column
    /// is not a smaller working layer either: the styling that referenced it silently stops
    /// applying, which is a wrong map rather than a missing one. And a warning at startup competes
    /// with every other startup line, whereas this is knowable exactly, once, in milliseconds.
    ///
    /// PREPARE, not a lookup in `information_schema.columns`: preparing asks the same planner the
    /// real query will use, so it covers views, materialized views and foreign tables uniformly,
    /// and it validates the geometry column and the relation itself at the same time — including
    /// the case `resolve_metadata` skips entirely, where the URI supplies both `?geom=` and
    /// `?srid=` and no `geometry_columns` lookup ever happens. Nothing is executed: `PREPARE` plans
    /// the statement, it does not run it, so this costs one round trip regardless of table size.
    /// **Both statement SHAPES are prepared, not just the default one.** The min-feature-size
    /// pushdown (`size_gate_sql`) appends an `AND NOT (...)` that is absent when the gate is off,
    /// and the threshold is a formatted literal rather than a bind parameter — so the gated
    /// statement is a genuinely different string that no `PREPARE` would otherwise ever see until
    /// the first low-zoom MVT tile of a live server. A defect in it would take the same path every
    /// other error on this seam takes: `fetch` returns `Err`, `query` logs one line and returns an
    /// empty `Vec`, and the tile is a blank 200 OK. Validating both here costs one extra plan on
    /// the connection already open, and it is checked whether or not `--mvt-min-feature-px` is set,
    /// because that flag can be turned on later without anyone re-reading this comment.
    pub fn validate_columns(&self) -> Result<(), String> {
        // 1.0 is a stand-in for "some positive threshold": the SHAPE is what is being planned, and
        // the shape is identical for every positive value.
        let sqls = [self.sql_for(1, 0.0), self.sql_for(1, 1.0)];
        let mut cfg = parse_dsn(&self.target)?;
        cfg.connect_timeout(std::time::Duration::from_millis(connect_timeout_ms()));
        off_runtime("column validation", |rt| {
            rt.block_on(prepare_once(&self.target, &cfg, &sqls, &self.columns))
        })
    }

    /// The one statement, for a given cap and size-gate threshold. Shared by `validate_columns`
    /// and `query` so what is checked at startup is byte-identical (bar the LIMIT) to what is
    /// later executed.
    fn sql_for(&self, limit: usize, min_area_src: f64) -> String {
        build_sql(
            &self.target.schema,
            &self.target.table,
            &self.geom_col,
            self.srid,
            &self.columns,
            limit,
            min_area_src,
        )
    }

    /// `schema.table`, for error messages that need to name the relation the SRID came from.
    /// Never the URI: that can carry a literal password (see `pg_uri`'s module docs).
    pub fn qualified_name(&self) -> String {
        format!("{}.{}", self.target.schema, self.target.table)
    }

    /// The connection pool's configured maximum size (deadpool default: `cpu_count * 2`, unless
    /// overridden). Exposed so the caller can compare it against `--max-inflight` at startup — see
    /// `pool_sizing_warning`.
    pub fn pool_max_size(&self) -> usize {
        self.pool.status().max_size
    }

    /// The async half: one statement, STREAMED.
    ///
    /// `query_raw` yields a `RowStream` so rows decode as they arrive. The buffering `query()`
    /// would materialise the entire result set first, which at low zoom is the whole table —
    /// the bounded-memory claim rests on this choice, not on the LIMIT alone.
    ///
    /// Returns the features AND the number of ROWS the server sent. Those two numbers are not the
    /// same — a NULL, empty or undecodable geometry is skipped below — and it is the row count,
    /// never the feature count, that says whether the `LIMIT` was reached. Comparing the feature
    /// count against the cap under-reports: 500,000 rows of which 12 were skipped yields 499,988
    /// features, which is `< max_features`, so a genuinely truncated result would be served with no
    /// warning at all.
    async fn fetch(&self, sql: &str, env: [f64; 4]) -> Result<(Vec<Feature>, usize), String> {
        use futures_util::TryStreamExt;

        let client = self
            .pool
            .get()
            .await
            .map_err(|e| format!("postgis: pool: {e}"))?;
        let stmt = client
            .prepare_cached(sql)
            .await
            .map_err(|e| format!("postgis: prepare: {}", err_chain(&e)))?;
        let params: [&(dyn tokio_postgres::types::ToSql + Sync); 4] =
            [&env[0], &env[1], &env[2], &env[3]];
        let stream = client
            .query_raw(&stmt, params)
            .await
            .map_err(|e| format!("postgis: query: {}", err_chain(&e)))?;
        futures_util::pin_mut!(stream);

        let mut out = Vec::new();
        let mut fid = 0u64;
        let mut rows = 0usize;
        while let Some(row) = stream
            .try_next()
            .await
            .map_err(|e| format!("postgis: row: {}", err_chain(&e)))?
        {
            rows += 1;
            // `try_get` rather than `get`: a NULL geometry is legal in any nullable column and
            // must skip the row, not panic the render thread.
            let raw: &[u8] = match row.try_get(0) {
                Ok(Some(b)) => b,
                Ok(None) => continue,
                Err(e) => return Err(format!("postgis: geometry column: {e}")),
            };
            let geom = match wkb::decode_wkb(raw) {
                Ok(Some(g)) => g,
                // Empty, or a well-formed type we do not model. Nothing to draw, not an error.
                Ok(None) => continue,
                Err(e) => {
                    eprintln!("postgis: skipping malformed geometry (row {fid}): {e}");
                    continue;
                }
            };
            let mut props = Props::new();
            for (i, name) in self.columns.iter().enumerate() {
                props.insert(name.clone(), column_value(&row, i + 1));
            }
            // Feature::new precomputes bbox and area — do not hand-roll them.
            out.push(Feature::new(geom, props, fid));
            fid += 1;
        }
        Ok((out, rows))
    }
}

/// Run an async fetch from a SYNC context, whichever context that turns out to be.
///
/// `WindowedSource::query` is sync and its callers are not all inside a runtime:
/// - the SERVER enters `spawn_blocking` first, so a `Handle` exists and `block_on` on it is legal
///   from a blocking-pool thread (pinned by `block_on_inside_spawn_blocking_does_not_deadlock`);
/// - `build-pmtiles` is a plain sync CLI with NO runtime at all.
///
/// The second case used to return an empty `Vec`, so a bake produced a 260-byte archive and
/// reported SUCCESS. Returning nothing is never the right answer to "there is no runtime".
///
/// ⚠ ONE runtime for the whole process, NOT one per call — and it must OUTLIVE every query.
///
/// `tokio_postgres` runs each connection's socket on a background task owned by the runtime that
/// created the connection. This function used to build a fresh current-thread runtime per query and
/// drop it on return, which killed those tasks — while the `Client` handles went back into the
/// long-lived deadpool. deadpool's default recycling only asks `is_closed()`, which has not noticed
/// yet, so the next query was handed a corpse and failed with `connection closed`.
///
/// That is exactly the fault seen in the wild: 11 of 5,919 tiles in one EU5 bake, every one
/// recovering on retry (the dead client is discarded and a fresh one is built on the *current*
/// runtime), with NOTHING in the Postgres log — because the server never closed anything, we did.
/// It never hit the server path, because there a single runtime already spans the whole process.
///
/// A **multi-thread** runtime, deliberately. The old per-call runtime was current-thread, and a
/// shared current-thread runtime would be worse than the bug: its `block_on` can only be driven by
/// one thread at a time, so the bake's rayon workers would serialize on it. A multi-thread runtime
/// is safe to `block_on` concurrently from any number of external threads, and its own workers keep
/// the connection tasks polled between queries. Two worker threads is plenty — they only shuttle
/// socket bytes; the fetch future itself runs on whichever thread called `block_on`, so the bake's
/// parallelism is unchanged.
fn cli_runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    static RT: std::sync::OnceLock<Result<tokio::runtime::Runtime, String>> =
        std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("cannot build the PostGIS runtime: {e}"))
    })
    .as_ref()
    .map_err(|e| e.clone())
}

fn run_blocking<F, T>(fut: F, schema: &str, table: &str) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(h) => h.block_on(fut),
        Err(_) => match cli_runtime() {
            Ok(rt) => rt.block_on(fut),
            Err(e) => Err(format!("postgis: {e} (for {schema}.{table})")),
        },
    }
}

impl WindowedSource for PostgisSource {
    fn query(&self, bbox: [f64; 4]) -> Result<Vec<Feature>, String> {
        self.query_gated(bbox, 0.0)
    }

    /// Push the MVT min-feature-size gate into the `WHERE` clause. `min_area_src` is already in
    /// SOURCE-CRS units², and this source's `bbox_crs` is guaranteed equal to its `table_crs` at
    /// startup (`crs_mismatch_error`), so the threshold is directly comparable to `ST_Area` on the
    /// stored geometry with no unit conversion — see [`size_gate_sql`] for the predicate and the
    /// reasons it exempts lines and points. `0.0` reproduces `query` exactly.
    fn query_gated(&self, bbox: [f64; 4], min_area_src: f64) -> Result<Vec<Feature>, String> {
        // The filter envelope, in the TABLE's SRID (never the geometry — see the module docs and
        // `envelope_in_srid`). Fail-closed on a transform error: an untransformed envelope would
        // select the wrong window, or silently nothing at all.
        let env = envelope_in_srid(bbox, &self.bbox_crs, &self.table_crs).map_err(|e| {
            format!(
                "postgis: cannot place the query window in {}: {e}",
                self.table_crs
            )
        })?;
        // Read fresh on every call (not cached at `open()`), matching `gpkg::max_query_features` /
        // `fgb::max_query_features` and this function's own doc comment: an operator tuning
        // `TERRASERVE_PG_MAX_QUERY_FEATURES` must not need to restart the server for it to take
        // effect.
        let max_features = max_query_features();
        let sql = self.sql_for(max_features, min_area_src);
        // `WindowedSource::query` is SYNC, and its callers are not all inside a runtime.
        //
        // The server path is: a request thread enters `spawn_blocking`, so a `Handle` exists and
        // `block_on` on it is legal from a blocking-pool thread (Task 5 pins that).
        //
        // But `build-pmtiles` is a plain sync CLI with no runtime at all. This used to log a line
        // and RETURN AN EMPTY VEC, so a bake produced a 260-byte archive and reported SUCCESS —
        // the same silent-empty failure mode as a blank map behind a 200 OK, found 2026-08-06
        // trying to precompute an Estonian layer. Returning nothing is never the right answer to
        // "there is no runtime"; build one, exactly as `open()` already does for the same reason.
        //
        // ⚠ A current-thread runtime must be driven by `Runtime::block_on` ITSELF, never by
        // `block_on` on a cloned `Handle`. A Handle-driven current-thread runtime has nothing
        // turning its IO driver, so the future never progresses and the call HANGS FOREVER --
        // measured as a build-pmtiles bake that ran 25 minutes and produced no output. Hence the
        // two arms below run the future by different means rather than unifying on a Handle.
        // An outer safety net over the whole fetch. The specific timeouts each cover one phase,
        // but a server that accepts a connection and then stops answering (frozen host, black-holed
        // route) stalls the `query_raw` await itself — and `statement_timeout` cannot help there,
        // because it is enforced BY the server we have lost contact with. Without this, the
        // blocking-pool thread and its `--max-inflight` permit are parked until TCP keepalives
        // give up, which is two hours by default.
        //
        // Deliberately derived rather than another env var, and deliberately the SUM plus a
        // margin, so it always sits strictly above the server-side limit and can never pre-empt a
        // legitimately slow query that `statement_timeout` is already governing.
        let budget = std::time::Duration::from_millis(
            connect_timeout_ms() + pool_wait_timeout_ms() + statement_timeout_ms() + 5_000,
        );
        let fut = async {
            match tokio::time::timeout(budget, self.fetch(&sql, env)).await {
                Ok(r) => r,
                Err(_) => Err(format!(
                    "postgis: no response within {budget:?} (the database accepted the connection \
                     and then stopped answering)"
                )),
            }
        };
        // A failed query must not look like an empty region -- so it is REPORTED, not logged and
        // swallowed. This used to `return Vec::new()` because the render path had no error channel;
        // it has one now (`WindowedSource::query` returns `Result`), and every front-end above it
        // turns the error into a 500 / OWS exception instead of a blank tile behind a 200.
        let (feats, rows) = run_blocking(fut, &self.target.schema, &self.target.table)
            .map_err(|e| format!("postgis: query failed: {e}"))?;
        // Truncation is never silent: the `TERRASERVE_FGB_MAX_QUERY_FEATURES` trap was features
        // dropped in index order, blanking whole regions behind a 200 OK.
        //
        // Tested on ROWS, not on `feats.len()`. The `LIMIT` is applied by the server to rows, and
        // rows skipped above (NULL / empty / undecodable geometry) push the feature count BELOW the
        // cap while the result is still genuinely truncated — which is exactly the case where the
        // warning is needed most, and exactly the case a `feats.len() >= max_features` test misses.
        if rows >= max_features {
            let skipped = rows - feats.len();
            eprintln!(
                "postgis windowed query: hit cap {} rows for table `{}`.`{}` \
                 ({} features kept, {} rows skipped as null/empty/undecodable) \
                 (raise TERRASERVE_PG_MAX_QUERY_FEATURES)",
                max_features,
                self.target.schema,
                self.target.table,
                feats.len(),
                skipped
            );
        }
        Ok(feats)
    }

    fn full_extent(&self) -> [f64; 4] {
        extent_of(self.extent)
    }

    /// The table's own CRS, `EPSG:<srid>`.
    ///
    /// This can only ever act as a FALLBACK, never an override: `layer/mod.rs` resolves a vector
    /// layer's CRS as "an explicit `--src-crs`/config declaration always wins, otherwise adopt the
    /// source's own, otherwise WARN and assume the crate default" — the same precedence the `.fgb`
    /// and `.gpkg` arms use, and the contract the `bc21155` fix established. So an operator's
    /// declaration is still authoritative.
    ///
    /// Returning `None` would therefore not protect that declaration; it would only throw away a
    /// SRID we already know, and leave an operator who omitted `src_crs` with a warning and a map
    /// placed in an unrelated default CRS.
    fn crs(&self) -> Option<&str> {
        Some(&self.table_crs)
    }

    fn field_schema(&self) -> BTreeMap<String, String> {
        self.columns
            .iter()
            .map(|c| (c.clone(), "String".to_string()))
            .collect()
    }
}

/// Transform a query envelope into the table's CRS.
///
/// ALL FOUR corners, re-bounded. Transforming only the two diagonal corners is wrong for any
/// projection that rotates or curves the graticule: the resulting box can exclude data that the
/// real window covers. Cheap either way -- four points per request, against millions of vertices
/// if we transformed geometry instead.
fn envelope_in_srid(bbox: [f64; 4], from: &str, to: &str) -> Result<[f64; 4], String> {
    if from.eq_ignore_ascii_case(to) {
        return Ok(bbox);
    }
    // `Transformer::new(out, src)` maps `out` -> `src`, so the argument order here reads
    // from -> to, and `to_source` is that transform's forward direction.
    let t = crate::reproj::Transformer::new(from, to)?;
    let corners = [
        (bbox[0], bbox[1]),
        (bbox[2], bbox[1]),
        (bbox[2], bbox[3]),
        (bbox[0], bbox[3]),
    ];
    let (mut minx, mut miny) = (f64::MAX, f64::MAX);
    let (mut maxx, mut maxy) = (f64::MIN, f64::MIN);
    for (x, y) in corners {
        let (px, py) = t
            .to_source(x, y)
            .ok_or_else(|| format!("proj: {from} -> {to} failed at ({x}, {y})"))?;
        minx = minx.min(px);
        miny = miny.min(py);
        maxx = maxx.max(px);
        maxy = maxy.max(py);
    }
    Ok([minx, miny, maxx, maxy])
}

/// Flatten an error's `source()` chain into one line.
///
/// `tokio_postgres::Error`'s `Display` prints only its category — "error connecting to server",
/// "db error" — and drops the cause, which is the only part an operator can act on ("Connection
/// refused (os error 111)", "password authentication failed for user ..."). Safe for
/// connect/prepare/query errors specifically: the one error that quotes the connection string is
/// the config-PARSE error, and `parse_dsn` has already taken that out of play.
fn err_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(cause) = src {
        s.push_str(": ");
        s.push_str(&cause.to_string());
        src = cause.source();
    }
    s
}

/// Parse the DSN into a `tokio_postgres::Config`, converting the ONE error that can echo the
/// connection string into a message that cannot.
///
/// `tokio_postgres`'s connection-string parser reports an unrecognised keyword as ``unknown option
/// `x` `` — and with a keyword/value DSN, an unquoted password containing a space turns its own
/// tail into exactly such a keyword. `Dsn` refuses to print itself; this is the matching guard for
/// the errors of code that has legitimately called `expose()`.
fn parse_dsn(target: &PgTarget) -> Result<tokio_postgres::Config, String> {
    target
        .dsn
        .expose()
        .parse::<tokio_postgres::Config>()
        .map_err(|_| {
            format!(
            "postgis: the connection details for {}.{} are not a valid libpq connection string \
             (error withheld: it can quote the string, and the string holds the password). \
             A password containing a space is the usual cause.",
            target.schema, target.table
        )
        })
}

/// Decide the SRID and geometry column from the URI overrides and whatever `geometry_columns`
/// returned. Pure, so both rules below are testable without a database.
///
/// **Each URI value overrides independently.** `?geom=` and `?srid=` are documented as overrides,
/// so supplying one must take effect even when `geometry_columns` has an answer for it. (An
/// earlier version only honoured them when BOTH were present and a registered row was absent,
/// which silently discarded a lone `?srid=` on a registered table — exactly the case an operator
/// reaches for when the registered SRID is wrong.)
///
/// **`srid <= 0` is a MISS, not an answer.** MEASURED against PostGIS 3.5 rather than assumed:
///
/// | shape                                   | srid |
/// |-----------------------------------------|------|
/// | table with `geometry(Polygon, 2056)`    | 2056 |
/// | view passing that column through        | 2056 |
/// | view with a COMPUTED geom (ST_Centroid) | 0    |
/// | column declared bare `geometry`         | 0    |
///
/// So the fallback case is NOT "no row". A computed-geometry view IS registered, with srid 0.
/// Testing only for a missing row would accept 0 and build a layer reprojecting from SRID 0,
/// silently misplacing every feature.
fn resolve_from(
    uri_srid: Option<i32>,
    uri_geom: Option<String>,
    rows: &[(i32, String)],
    schema: &str,
    table: &str,
) -> Result<(i32, String), String> {
    let geom = match uri_geom {
        Some(g) => g,
        None => match rows {
            [] => {
                return Err(format!(
                    "postgis: {schema}.{table} has no geometry column in geometry_columns \
                     (a VIEW usually registers none); add ?geom=<column> to the layer URI."
                ))
            }
            [only] => only.1.clone(),
            // `query_opt` used to be used here, which turns this case into "query returned an
            // unexpected number of rows" — a tokio-postgres internal that tells the operator
            // nothing about their two-geometry-column table.
            many => {
                let names: Vec<&str> = many.iter().map(|(_, g)| g.as_str()).collect();
                return Err(format!(
                    "postgis: {schema}.{table} has {} geometry columns ({}); \
                     add ?geom=<column> to the layer URI to choose one.",
                    many.len(),
                    names.join(", ")
                ));
            }
        },
    };

    let srid = match uri_srid {
        Some(s) => s,
        None => {
            let registered = rows.iter().find(|(_, g)| *g == geom).map(|(s, _)| *s);
            match registered {
                Some(s) if s > 0 => s,
                _ => {
                    return Err(format!(
                        "postgis: {schema}.{table} column `{geom}` has no usable SRID in \
                         geometry_columns (absent, or 0 -- which is what a computed-geometry \
                         VIEW or a bare `geometry` column reports). \
                         Add ?srid=<EPSG code> to the layer URI."
                    ))
                }
            }
        }
    };
    Ok((srid, geom))
}

/// Resolve SRID and geometry column, asking `geometry_columns` only when the URI has not already
/// supplied both. The decision itself lives in [`resolve_from`]; this is just the I/O around it.
async fn resolve_metadata(
    target: &PgTarget,
    cfg: &tokio_postgres::Config,
) -> Result<(i32, String), String> {
    // Both supplied: nothing left to ask, so skip the round trip entirely. This is also the only
    // way to serve a table that `geometry_columns` does not register at all.
    if let (Some(s), Some(g)) = (target.srid, target.geom_col.clone()) {
        return Ok((s, g));
    }
    let (client, driver) = connect_standalone(target, cfg).await?;

    // `query`, not `query_opt`: a table may register more than one geometry column, and that is a
    // condition to explain (see `resolve_from`), not a row-count error to surface raw. When the
    // URI names a column, filter to it in SQL so the ambiguity simply does not arise.
    let rows = client
        .query(
            "SELECT srid, f_geometry_column FROM geometry_columns \
             WHERE f_table_schema = $1 AND f_table_name = $2 \
               AND ($3::text IS NULL OR f_geometry_column = $3::text) \
             ORDER BY f_geometry_column",
            &[&target.schema, &target.table, &target.geom_col],
        )
        .await
        .map_err(|e| format!("postgis: geometry_columns: {}", err_chain(&e)))?;
    drop(client);
    let _ = driver.await;

    let rows: Vec<(i32, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    resolve_from(
        target.srid,
        target.geom_col.clone(),
        &rows,
        &target.schema,
        &target.table,
    )
}

/// Ask the server to PLAN `sql` and throw the plan away, turning a planner rejection into a
/// startup error an operator can act on. The I/O half of [`PostgisSource::validate_columns`].
///
/// The two error codes worth translating are the two an operator actually causes:
/// `42703 undefined_column` (a typo or a case mismatch in the style's field names) and
/// `42P01 undefined_table` (wrong schema, wrong name, or no privilege to see it). Everything else
/// passes through with its cause chain, because a connect/auth/permission failure is already
/// self-explanatory and inventing a story about it would be worse.
async fn prepare_once(
    target: &PgTarget,
    cfg: &tokio_postgres::Config,
    sqls: &[String],
    columns: &[String],
) -> Result<(), String> {
    let (schema, table) = (&target.schema, &target.table);
    let (client, driver) = connect_standalone(target, cfg).await?;
    // Every shape, on the ONE connection — see `validate_columns` for why there is more than one.
    // First failure wins the diagnosis; they differ only by a generated predicate, so a second
    // error would be the same error.
    let mut first_err = None;
    for sql in sqls {
        if let Err(e) = client.prepare(sql).await {
            first_err = Some(e);
            break;
        }
    }
    let err = match first_err {
        None => {
            drop(client);
            let _ = driver.await;
            return Ok(());
        }
        Some(e) => e,
    };
    let code = err.code().cloned();
    // Only worth a second round trip when the failure is about a name; ask what the relation
    // really has so the message can end the diagnosis instead of starting it.
    let present = if code == Some(tokio_postgres::error::SqlState::UNDEFINED_COLUMN) {
        relation_columns(&client, schema, table).await
    } else {
        Vec::new()
    };
    drop(client);
    let _ = driver.await;

    let msg = match code {
        Some(tokio_postgres::error::SqlState::UNDEFINED_COLUMN) => {
            let asked = if columns.is_empty() {
                format!(
                    "geometry column `{}`",
                    target.geom_col.as_deref().unwrap_or("?")
                )
            } else {
                format!("columns [{}]", columns.join(", "))
            };
            let has = if present.is_empty() {
                String::new()
            } else {
                format!(" {schema}.{table} has: {}.", present.join(", "))
            };
            format!(
                "postgis: {schema}.{table} does not have every column this layer needs. \
                 The style (plus any `columns:` in the layer config) asks for {asked}.{has} \
                 Identifiers are quoted, so they are CASE-SENSITIVE: `Name` is not `name`. \
                 Postgres said: {}",
                err_chain(&err)
            )
        }
        Some(tokio_postgres::error::SqlState::UNDEFINED_TABLE) => format!(
            "postgis: relation {schema}.{table} does not exist, or this role cannot see it. \
             Check the schema (the URI defaults to `public`) and the role's SELECT privilege. \
             Postgres said: {}",
            err_chain(&err)
        ),
        _ => format!(
            "postgis: could not prepare the query for {schema}.{table}: {}",
            err_chain(&err)
        ),
    };
    Err(msg)
}

/// The column names a relation really has, for the "did you mean" half of the message above.
/// `pg_attribute` rather than `information_schema.columns` because it also covers MATERIALIZED
/// views, which `information_schema` omits entirely. Best-effort: if this itself fails the caller
/// simply reports the original error without the hint.
async fn relation_columns(
    client: &tokio_postgres::Client,
    schema: &str,
    table: &str,
) -> Vec<String> {
    let rows = client
        .query(
            "SELECT a.attname FROM pg_attribute a \
               JOIN pg_class c ON c.oid = a.attrelid \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
              WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
              ORDER BY a.attnum",
            &[&schema, &table],
        )
        .await;
    rows.map(|rs| rs.iter().map(|r| r.get::<_, String>(0)).collect())
        .unwrap_or_default()
}

/// Open a STANDALONE connection (never one from the pool — see `off_runtime`) and spawn the driver
/// task tokio-postgres requires. The returned handle finishes when the `Client` is dropped.
async fn connect_standalone(
    target: &PgTarget,
    cfg: &tokio_postgres::Config,
) -> Result<(tokio_postgres::Client, tokio::task::JoinHandle<()>), String> {
    // Connects from the already-parsed `Config`, never from the DSN string, so this error is a
    // real connect/auth failure and cannot contain the connection string. See `parse_dsn`.
    let (client, conn) = cfg.connect(tokio_postgres::NoTls).await.map_err(|e| {
        format!(
            "postgis: connecting for {}.{}: {}",
            target.schema,
            target.table,
            err_chain(&e)
        )
    })?;
    // tokio-postgres splits client from connection: the connection future must be driven or no
    // query ever completes. It finishes when `client` drops.
    let driver = tokio::spawn(async move {
        if let Err(e) = conn.await {
            eprintln!("postgis: connection closed: {e}");
        }
    });
    Ok((client, driver))
}

/// Which Rust type a Postgres column should be read as.
///
/// Split out of [`column_value`] so the mapping — the part that decides whether a column reaches
/// the Style IR as text, as a number, or silently as `Null` — is testable without a database.
/// `Type`'s constants are plain values, so a test names real Postgres types directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Conv {
    Text,
    F64,
    F32,
    I64,
    I32,
    I16,
    Bool,
    /// No conversion the Style IR can use. Yields `Value::Null` rather than failing the tile.
    Unsupported,
}

fn conversion_for(ty: &tokio_postgres::types::Type) -> Conv {
    use tokio_postgres::types::Type;
    // `Type` cannot be used in a match pattern (it wraps an `Arc` for user-defined types, so it
    // is not structural-match), hence the comparison chain.
    if *ty == Type::VARCHAR || *ty == Type::TEXT || *ty == Type::BPCHAR || *ty == Type::NAME {
        Conv::Text
    } else if *ty == Type::FLOAT8 {
        Conv::F64
    } else if *ty == Type::FLOAT4 {
        Conv::F32
    } else if *ty == Type::INT8 {
        Conv::I64
    } else if *ty == Type::INT4 {
        Conv::I32
    } else if *ty == Type::INT2 {
        Conv::I16
    } else if *ty == Type::BOOL {
        Conv::Bool
    } else {
        Conv::Unsupported
    }
}

/// One row column to our `Value`, dispatched on the column's declared Postgres type. A type we do
/// not map, or a SQL NULL, becomes `Value::Null` rather than failing the whole tile.
fn column_value(row: &tokio_postgres::Row, i: usize) -> Value {
    let Some(col) = row.columns().get(i) else {
        return Value::Null;
    };
    let v = match conversion_for(col.type_()) {
        Conv::Text => row
            .try_get::<_, Option<String>>(i)
            .ok()
            .flatten()
            .map(Value::Str),
        Conv::F64 => row
            .try_get::<_, Option<f64>>(i)
            .ok()
            .flatten()
            .map(Value::Num),
        Conv::F32 => row
            .try_get::<_, Option<f32>>(i)
            .ok()
            .flatten()
            .map(|v| Value::Num(v as f64)),
        Conv::I64 => row
            .try_get::<_, Option<i64>>(i)
            .ok()
            .flatten()
            .map(|v| Value::Num(v as f64)),
        Conv::I32 => row
            .try_get::<_, Option<i32>>(i)
            .ok()
            .flatten()
            .map(|v| Value::Num(v as f64)),
        Conv::I16 => row
            .try_get::<_, Option<i16>>(i)
            .ok()
            .flatten()
            .map(|v| Value::Num(v as f64)),
        Conv::Bool => row
            .try_get::<_, Option<bool>>(i)
            .ok()
            .flatten()
            .map(|v| Value::Str(v.to_string())),
        Conv::Unsupported => None,
    };
    v.unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_filters_on_the_envelope_and_never_transforms_the_geometry() {
        // NOTE the quoting. The brief spelled these two assertions unquoted (`ST_AsBinary(geom)`),
        // which directly contradicts `identifiers_are_quoted_so_a_mixed_case_table_works` below;
        // both cannot hold. Quoting wins: it is the assertion that is explicit about WHY it
        // exists, and an unquoted identifier is what breaks a mixed-case table and what makes an
        // operator-supplied name from the layer URI a SQL-injection surface.
        let sql = build_sql(
            "public",
            "parcels",
            "geom",
            2056,
            &["cls".into()],
            5000,
            0.0,
        );
        assert!(sql.contains("ST_AsBinary(\"geom\")"));
        assert!(sql.contains("\"geom\" && ST_MakeEnvelope($1,$2,$3,$4,2056)"));
        assert!(sql.contains("LIMIT 5000"));
        // The three forbidden pushdowns. Spec section 1 -- these must NEVER appear.
        assert!(
            !sql.contains("ST_Transform"),
            "geometry must not be transformed in SQL"
        );
        assert!(
            !sql.contains("ST_Simplify"),
            "generalization is ours, not the DB's"
        );
        assert!(!sql.contains("ST_AsMVT"), "encoding is ours, not the DB's");
        assert!(!sql.contains("SELECT *"), "select only the styled columns");
    }

    #[test]
    fn identifiers_are_quoted_so_a_mixed_case_table_works() {
        let sql = build_sql("mySchema", "myTable", "theGeom", 4326, &[], 10, 0.0);
        assert!(sql.contains("\"mySchema\".\"myTable\""));
        assert!(sql.contains("ST_AsBinary(\"theGeom\")"));
    }

    // ---- the zoom-aware size gate -------------------------------------------------------------

    /// Requirement 1: OFF BY DEFAULT means byte-identical, not merely equivalent. `--mvt-min-feature-px`
    /// defaults to `0.0`, and every non-tile path (WMS GetMap, GetFeatureInfo, the raster tile
    /// front-ends) passes `0.0` forever — so this is the statement almost every deployment runs.
    /// Pinned as a whole string, not a `contains`, because "the SQL did not change" is the claim.
    #[test]
    fn a_zero_threshold_leaves_the_statement_byte_identical() {
        let pre_gate = "SELECT ST_AsBinary(\"geom\"), \"cls\" FROM \"public\".\"parcels\" \
                        WHERE \"geom\" && ST_MakeEnvelope($1,$2,$3,$4,2056) LIMIT 5000";
        assert_eq!(
            build_sql(
                "public",
                "parcels",
                "geom",
                2056,
                &["cls".into()],
                5000,
                0.0
            ),
            pre_gate
        );
        // A non-finite or negative threshold can only come from a bug upstream. It must fail OPEN
        // (keep every row) exactly like `min_area_src_for_zoom`'s own off-switch — never emit
        // `< NaN`, which is false for every row and would blank the layer behind a 200 OK.
        for bad in [-1.0, f64::NAN, f64::INFINITY, -0.0] {
            assert_eq!(
                build_sql(
                    "public",
                    "parcels",
                    "geom",
                    2056,
                    &["cls".into()],
                    5000,
                    bad
                ),
                pre_gate,
                "threshold {bad} must disable the gate, not emit it"
            );
        }
    }

    /// ⚠ THE TRAP. `ST_Area` is 0 for every LineString and every Point, so a naive
    /// `ST_Area(geom) >= t` deletes every road and every place and returns a 200 OK with an empty
    /// map. The `> 0` guard is the whole defence, and it is the exact translation of the Rust gate
    /// in `mvt/tile.rs` (`f.area > 0.0 && f.area < min_area_src`).
    #[test]
    fn the_size_gate_exempts_lines_and_points_from_the_area_test() {
        let sql = build_sql("public", "roads", "geom", 2180, &[], 100, 47.5);
        assert!(
            sql.contains("ST_Area(\"geom\") > 0"),
            "without the `> 0` guard every line and point is dropped: {sql}"
        );
        assert!(
            sql.contains("NOT ("),
            "the predicate must be `NOT (area > 0 AND area < t)`, i.e. keep-unless-provably-tiny: {sql}"
        );
        // The bbox filter and the LIMIT are untouched, and the gate lands between them.
        assert!(sql.contains("\"geom\" && ST_MakeEnvelope($1,$2,$3,$4,2180)"));
        assert!(sql.ends_with("LIMIT 100"));
        // Still none of the forbidden pushdowns: this filters ROWS, it does not render.
        for banned in ["ST_Transform", "ST_Simplify", "ST_AsMVT"] {
            assert!(!sql.contains(banned), "{banned} must never appear: {sql}");
        }
    }

    /// The predicate must be truth-table-identical to `mvt/tile.rs`'s per-feature gate for all
    /// three geometry dimensions. This is that truth table, evaluated in Rust against the same
    /// numbers PostGIS would see, so a future edit to the SQL that changes the SEMANTICS (rather
    /// than the spelling) has somewhere to fail.
    #[test]
    fn the_predicate_matches_the_rust_gate_for_every_geometry_dimension() {
        let t = 47.5;
        // `keep` mirrors `NOT (area > 0 AND area < t)`.
        let keep = |area: f64| !(area > 0.0 && area < t);
        // Points and lines: ST_Area == 0 for both, in PostGIS and in `Feature::area`.
        assert!(keep(0.0), "a point or line (area 0) is ALWAYS kept");
        // Polygons: dropped strictly below the threshold, kept at and above it.
        assert!(!keep(t / 2.0), "a sub-threshold polygon is dropped");
        assert!(keep(t), "a polygon exactly at the threshold is kept");
        assert!(keep(t * 2.0), "a large polygon is kept");
        // And the same rule as the Rust gate applies to `f.area`, which is what makes SQL-dropped
        // a SUBSET of Rust-dropped: nothing the encoder would have drawn is removed by the SQL.
        for area in [0.0, t / 2.0, t, t * 2.0] {
            let rust_drops = area > 0.0 && area < t;
            assert_eq!(!keep(area), rust_drops, "divergence at area={area}");
        }
    }

    /// `ST_Area(ST_Envelope(geom))` was the obvious alternative and it is WRONG for lines — this
    /// pins why, so nobody "simplifies" the dimension-safe form into it. An axis-aligned road has
    /// a DEGENERATE envelope (zero width or zero height), hence envelope area 0, hence dropped;
    /// a diagonal road of the same length has a large envelope and survives. That would delete
    /// every north-south and east-west street while keeping the diagonals — a wrong map, and one
    /// that looks plausible enough to ship.
    #[test]
    fn envelope_area_is_degenerate_for_axis_aligned_lines() {
        // A 1000-unit EAST-WEST road: envelope is 1000 x 0.
        let (dx, dy) = (1000.0_f64, 0.0_f64);
        assert_eq!(
            dx * dy,
            0.0,
            "axis-aligned envelope area is 0 -> would be dropped"
        );
        // The same road at 45 degrees: envelope is ~707 x ~707.
        let (dx, dy) = (707.1_f64, 707.1_f64);
        assert!(
            dx * dy > 400_000.0,
            "diagonal envelope area is large -> would be kept"
        );
    }

    #[test]
    fn the_envelope_is_transformed_into_the_table_srid_not_the_geometry() {
        // The whole reason the GiST index stays usable: we move the 4-corner query box INTO the
        // table's CRS, rather than moving every geometry out of it. A regression here does not
        // fail loudly -- it silently turns every query into a sequential scan.
        let env = envelope_in_srid(
            [2600000.0, 1200000.0, 2601000.0, 1201000.0],
            "EPSG:2056",
            "EPSG:2056",
        )
        .unwrap();
        assert_eq!(
            env,
            [2600000.0, 1200000.0, 2601000.0, 1201000.0],
            "identity must be exact"
        );

        // 4326 -> 3857 on a known point: lon 0, lat 0 maps to 0,0; lon 180 -> ~20037508.
        let e = envelope_in_srid([0.0, 0.0, 180.0, 0.0], "EPSG:4326", "EPSG:3857").unwrap();
        assert!((e[0]).abs() < 1.0, "minx ~0, got {}", e[0]);
        assert!(
            (e[2] - 20037508.34).abs() < 1.0,
            "maxx ~20037508, got {}",
            e[2]
        );
    }

    #[test]
    fn a_rotated_projection_uses_the_bounding_box_of_all_four_corners() {
        // Transforming only (minx,miny) and (maxx,maxy) is WRONG for any projection that rotates
        // or curves: the transformed box can miss data near the other two corners. All four
        // corners must be transformed and re-bounded.
        let e = envelope_in_srid([-10.0, 40.0, 10.0, 60.0], "EPSG:4326", "EPSG:3035").unwrap();
        assert!(
            e[0] < e[2] && e[1] < e[3],
            "envelope must stay well-formed: {e:?}"
        );

        // And it is genuinely wider than the two-diagonal-corner shortcut would produce: EPSG:3035
        // is a Lambert azimuthal projection centred on 10°E, so the box's own corners fan outward.
        // A two-corner implementation passes the well-formedness check above and still loses data,
        // which is exactly why this second assertion exists.
        let t = crate::reproj::Transformer::new("EPSG:4326", "EPSG:3035").unwrap();
        let (dx0, dy0) = t.to_source(-10.0, 40.0).unwrap();
        let (dx1, dy1) = t.to_source(10.0, 60.0).unwrap();
        let two_corner = [dx0.min(dx1), dy0.min(dy1), dx0.max(dx1), dy0.max(dy1)];
        assert!(
            e[0] <= two_corner[0] && e[1] <= two_corner[1],
            "four corners must not shrink the box: {e:?} vs {two_corner:?}"
        );
        assert!(
            e[0] < two_corner[0]
                || e[1] < two_corner[1]
                || e[2] > two_corner[2]
                || e[3] > two_corner[3],
            "on a rotating projection the four-corner box must be strictly wider than the \
             diagonal shortcut on at least one side: {e:?} vs {two_corner:?}"
        );
    }

    // -- identifier quoting -----------------------------------------------------------------

    #[test]
    fn quote_ident_doubles_embedded_quotes_so_a_name_cannot_escape_its_quoting() {
        assert_eq!(quote_ident("geom"), "\"geom\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
        // The reason this matters: schema/table/geom come from the operator-supplied layer URI.
        // A name that closed its own quote would be running SQL of the URI author's choosing.
        let sql = build_sql(
            "public",
            "t\"; DROP TABLE users; --",
            "geom",
            4326,
            &[],
            1,
            0.0,
        );
        assert!(
            sql.contains("\"t\"\"; DROP TABLE users; --\""),
            "the whole name must stay inside one quoted identifier: {sql}"
        );
        assert!(
            !sql.contains("\"t\"; DROP"),
            "the quote was not doubled, so the identifier terminated early: {sql}"
        );
    }

    // -- the password guard -----------------------------------------------------------------

    #[test]
    fn a_dsn_parse_failure_never_echoes_the_password() {
        // RETRIGGERED, for the SECOND time, and this time on nothing that can be fixed.
        //
        // Round 1 triggered on a password containing a space, which (unquoted) broke libpq
        // keyword/value syntax -- i.e. on the connection-string-injection bug itself. Quoting the
        // password fixed the bug and made the test vacuous. Round 2 then retriggered on
        // `db:notaport`, on the grounds that the port was interpolated unquoted and unvalidated --
        // which was ALSO a live defect (the same injection, reachable through the port instead of
        // the password), and validating the port has now made that trigger vacuous too.
        //
        // The lesson, twice over: a test whose trigger is a defect dies with the defect, silently.
        // So this no longer goes through `parse_postgis_uri` at all -- it CANNOT, by design: every
        // value that parser interpolates is quoted and the port is validated, precisely so it can
        // never emit an invalid DSN. The malformed input is therefore constructed directly, with
        // an unterminated quoted string, which is malformed by the libpq grammar itself and can
        // never become valid no matter what this crate does. The PROPERTY under test is unchanged
        // and is the one that matters: whatever makes `parse_dsn` fail, its error must not echo
        // the password the string contains.
        let target = PgTarget {
            dsn: crate::vector::pg_uri::Dsn::from_raw_for_test(
                "host='db' user='ts' dbname='gis' password='hunter2s3cret",
            ),
            schema: "public".to_string(),
            table: "parcels".to_string(),
            geom_col: None,
            srid: None,
        };
        let e = parse_dsn(&target).unwrap_err();
        assert!(
            !e.contains("hunter2s3cret"),
            "the error leaked the password: {e}"
        );
        assert!(
            e.contains("parcels"),
            "the error should still name the layer's table: {e}"
        );
        assert!(
            e.contains("password"),
            "the error should point at the likely cause: {e}"
        );
    }

    // -- SRID / geometry-column resolution ---------------------------------------------------

    fn rows(v: &[(i32, &str)]) -> Vec<(i32, String)> {
        v.iter().map(|(s, g)| (*s, g.to_string())).collect()
    }

    #[test]
    fn a_registered_table_needs_no_uri_hints() {
        let got = resolve_from(None, None, &rows(&[(2056, "geom")]), "public", "parcels").unwrap();
        assert_eq!(got, (2056, "geom".to_string()));
    }

    #[test]
    fn srid_zero_is_a_miss_not_an_answer() {
        // A computed-geometry VIEW and a bare `geometry` column both register WITH srid 0
        // (measured on PostGIS 3.5). Accepting it would build a layer reprojecting from SRID 0
        // and misplace every feature, with no error anywhere.
        let e = resolve_from(None, None, &rows(&[(0, "geom")]), "public", "v").unwrap_err();
        assert!(e.contains("?srid="), "the error must state the fix: {e}");
        // Negative is equally unusable and must take the same path.
        let e = resolve_from(None, None, &rows(&[(-1, "geom")]), "public", "v").unwrap_err();
        assert!(e.contains("?srid="), "{e}");
        // ...and the URI override is what makes such a view servable.
        let got = resolve_from(Some(4326), None, &rows(&[(0, "geom")]), "public", "v").unwrap();
        assert_eq!(got, (4326, "geom".to_string()));
    }

    #[test]
    fn each_uri_hint_overrides_independently() {
        // The bug this pins: an early return that required BOTH values meant a lone `?srid=` on a
        // REGISTERED table was silently discarded and the registered SRID won — precisely the
        // case an operator reaches for when the registered SRID is wrong.
        let db = rows(&[(2056, "geom")]);
        assert_eq!(
            resolve_from(Some(4326), None, &db, "public", "t").unwrap(),
            (4326, "geom".to_string()),
            "?srid= must override a registered SRID"
        );
        assert_eq!(
            resolve_from(
                None,
                Some("shape".into()),
                &rows(&[(2056, "shape")]),
                "public",
                "t"
            )
            .unwrap(),
            (2056, "shape".to_string()),
            "?geom= must select the column, and its SRID must still come from the DB"
        );
    }

    #[test]
    fn a_table_with_two_geometry_columns_names_them_instead_of_failing_obscurely() {
        // Previously `query_opt`, which turns this into "query returned an unexpected number of
        // rows" — a tokio-postgres internal that says nothing about the operator's table.
        let db = rows(&[(2056, "geom"), (3857, "geom_3857")]);
        let e = resolve_from(None, None, &db, "public", "parcels").unwrap_err();
        assert!(
            e.contains("geom") && e.contains("geom_3857"),
            "name both: {e}"
        );
        assert!(e.contains("?geom="), "state the fix: {e}");
        // Naming one resolves it, and picks up THAT column's SRID, not the other's.
        assert_eq!(
            resolve_from(None, Some("geom_3857".into()), &db, "public", "parcels").unwrap(),
            (3857, "geom_3857".to_string())
        );
    }

    #[test]
    fn an_unregistered_view_says_which_parameter_to_add() {
        let e = resolve_from(None, None, &[], "public", "v").unwrap_err();
        assert!(e.contains("?geom="), "{e}");
        // Both hints together serve a table `geometry_columns` does not register at all.
        assert_eq!(
            resolve_from(Some(4326), Some("g".into()), &[], "public", "v").unwrap(),
            (4326, "g".to_string())
        );
    }

    // -- attribute typing --------------------------------------------------------------------

    #[test]
    fn column_types_map_to_the_value_the_style_ir_can_use() {
        use tokio_postgres::types::Type;
        for t in [Type::VARCHAR, Type::TEXT, Type::BPCHAR, Type::NAME] {
            assert_eq!(conversion_for(&t), Conv::Text, "{t} should read as text");
        }
        assert_eq!(conversion_for(&Type::FLOAT8), Conv::F64);
        assert_eq!(conversion_for(&Type::FLOAT4), Conv::F32);
        assert_eq!(conversion_for(&Type::INT8), Conv::I64);
        assert_eq!(conversion_for(&Type::INT4), Conv::I32);
        assert_eq!(conversion_for(&Type::INT2), Conv::I16);
        assert_eq!(conversion_for(&Type::BOOL), Conv::Bool);
        // `int4` reading as text (or vice versa) is the failure that shows up as a blank label
        // rather than as an error, so pin the two apart explicitly.
        assert_ne!(conversion_for(&Type::INT4), Conv::Text);
        assert_ne!(conversion_for(&Type::TEXT), Conv::I32);
        // Types with no Style IR representation fall through to Null rather than failing a tile.
        // (Widening this set is filed as a follow-up, so this asserts today's behaviour, not an
        // aspiration.)
        for t in [
            Type::NUMERIC,
            Type::DATE,
            Type::UUID,
            Type::JSONB,
            Type::BYTEA,
        ] {
            assert_eq!(conversion_for(&t), Conv::Unsupported, "{t}");
        }
    }

    #[test]
    fn full_extent_is_the_configured_value_verbatim() {
        // No database needed: the extent must come from config and never from a query.
        let e = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(extent_of(e), e);
    }

    // `WindowedSource::query` is sync and always runs inside `spawn_blocking` (server.rs around
    // lines 523, 631, 875, 970), but `tokio-postgres` is async, so `query` bridges with
    // `Handle::current().block_on(...)`. That bridge is legal from a blocking-pool thread and
    // ILLEGAL from a runtime worker thread. The two tests below pin both halves of that boundary
    // -- not because either bit of tokio behaviour might be new, but because the whole
    // concurrency design (spec section 7) is only sound as long as the boundary holds and every
    // call site stays on the right side of it. A regression here is a compile-time-invisible,
    // CI-invisible property; if it ever breaks, this is where it should show up first, not as a
    // hung tile request in production.
    //
    // CHARACTERIZATION, not TDD: both tests pass on their first run. See task-5-brief.md.

    #[test]
    fn block_on_inside_spawn_blocking_does_not_deadlock() {
        // WindowedSource::query is sync and runs inside spawn_blocking, but tokio-postgres is async.
        // The design depends on block_on being legal from a blocking-pool thread (which is NOT a
        // runtime worker). This pins it: a regression shows up here, not as a hung tile request.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt.block_on(async {
            tokio::task::spawn_blocking(|| {
                tokio::runtime::Handle::current().block_on(async { 40 + 2 })
            })
            .await
            .unwrap()
        });
        assert_eq!(out, 42);
    }

    #[test]
    #[should_panic(
        expected = "Cannot start a runtime from within a runtime. This happens because a \
                    function (like `block_on`) attempted to block the current thread while the \
                    thread is being used to drive asynchronous tasks."
    )]
    fn block_on_from_a_reactor_thread_panics() {
        // The other half of the boundary above: called directly from a worker thread that is
        // already driving the async executor -- i.e. WITHOUT the spawn_blocking hop -- the exact
        // same `Handle::block_on` call panics instead of deadlocking. Panic, not deadlock, because
        // tokio detects the nested-runtime-entry via a thread-local guard and fails fast; see
        // tokio's own `rt_handle_block_on.rs::nesting` test, which this mirrors.
        //
        // This is why every WindowedSource::query call site MUST go through spawn_blocking and
        // never be invoked directly from an async handler on the reactor: that refactor is the
        // one that turns this from a latent risk into a production panic on the first tile
        // request. Asserted directly (not deferred to a comment) because the failure mode is a
        // hard panic with a fixed, stable message -- not a race or a timing-sensitive deadlock --
        // so there is nothing flaky about pinning it exactly.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            tokio::runtime::Handle::current().block_on(async { 40 + 2 });
        });
    }

    // -- the declared-CRS guard ---------------------------------------------------------------

    #[test]
    fn a_declared_crs_that_disagrees_with_the_table_srid_is_an_error_naming_both() {
        // The failure this prevents is the quietest one in the whole source: `src_crs: EPSG:4326`
        // on a 2056 table makes `envelope_in_srid("EPSG:2056", "EPSG:2056")` an identity, so a
        // degree-valued box is tested against metre geometry. `&&` matches nothing, `query()`
        // returns an empty Vec through its NORMAL path (not its error path, so not even the "query
        // failed" log line fires), and every tile is a blank 200 OK.
        let e = crs_mismatch_error("EPSG:4326", "EPSG:2056", "public.parcels")
            .expect("a disagreement must be an error");
        assert!(
            e.contains("EPSG:4326") && e.contains("EPSG:2056"),
            "name BOTH CRSs so the operator can see which is which: {e}"
        );
        assert!(
            e.contains("public.parcels"),
            "name the table the SRID came from: {e}"
        );
        assert!(e.contains("?srid="), "state both fixes: {e}");
    }

    #[test]
    fn a_declared_crs_that_agrees_is_accepted_including_case_and_the_crs84_spelling() {
        assert!(crs_mismatch_error("EPSG:2056", "EPSG:2056", "public.t").is_none());
        assert!(
            crs_mismatch_error("epsg:2056", "EPSG:2056", "public.t").is_none(),
            "CRS codes are not case-sensitive; rejecting on case alone would be a false alarm"
        );
        // CRS:84 and EPSG:4326 differ only in axis order, and PostGIS stores 4326 lon/lat -- the
        // order this crate's vector path assumes everywhere -- so the pair is genuinely the same
        // CRS here and must not be rejected.
        assert!(crs_mismatch_error("CRS:84", "EPSG:4326", "public.t").is_none());
        assert!(crs_mismatch_error("EPSG:4326", "CRS:84", "public.t").is_none());
        // ...but CRS:84 against a projected table is still a real mismatch.
        assert!(crs_mismatch_error("CRS:84", "EPSG:3857", "public.t").is_some());
    }

    #[test]
    fn a_pool_smaller_than_max_inflight_warns_with_both_numbers() {
        let w = pool_sizing_warning(4, 32).expect("must warn when pool < max_inflight");
        assert!(
            w.contains('4') && w.contains("32"),
            "name both numbers: {w}"
        );

        assert!(
            pool_sizing_warning(32, 32).is_none(),
            "equal is fine, no warning"
        );
        assert!(
            pool_sizing_warning(64, 32).is_none(),
            "a larger pool is fine"
        );
    }
}
