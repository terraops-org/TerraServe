#!/usr/bin/env bash
# Stand up a PostGIS test database for the live integration tests.
#
# The live tests self-skip without TERRASERVE_PG_TEST_URL, so this is optional -- but code that
# has never spoken to a database is code whose SQL has never been proven. Run this before
# `cargo test --test postgis_live`.
#
#   ./tests/postgis-fixture.sh
#   export TERRASERVE_PG_TEST_PASSWORD=terraserve_test_pw
#   export TERRASERVE_PG_TEST_URL='postgis://postgres:${TERRASERVE_PG_TEST_PASSWORD}@localhost:5433/postgres/public.mini_feats'
#   cargo test --test postgis_live
#
# Tear down with: docker rm -f ts-postgis-test
set -euo pipefail
cd "$(dirname "$0")/.."

CONTAINER=ts-postgis-test
PORT=5433
PW=terraserve_test_pw
IMAGE=postgis/postgis:17-3.5

echo "== container"
docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
docker run -d --name "$CONTAINER" -e POSTGRES_PASSWORD="$PW" -p "$PORT:5432" "$IMAGE" >/dev/null
# Readiness must be probed over TCP, and neither `pg_isready` nor a socket `psql` does that.
# The postgres image runs a TEMPORARY server during initdb/extension setup with
# `listen_addresses=''`, so it answers on the unix socket (satisfying both) while refusing every
# TCP connection -- and ogr2ogr, which connects over TCP, lands in "server closed the connection
# unexpectedly". Forcing `-h 127.0.0.1` makes the probe test the same path ogr2ogr will use.
for _ in $(seq 90); do
  docker exec -i -e PGPASSWORD="$PW" "$CONTAINER" \
    psql -h 127.0.0.1 -U postgres -qAtc "SELECT 1" >/dev/null 2>&1 && break
  sleep 1
done
docker exec -i -e PGPASSWORD="$PW" "$CONTAINER" \
  psql -h 127.0.0.1 -U postgres -qAtc "SELECT 'server accepting TCP connections'"

echo "== mini.gpkg -> PostGIS (the SAME fixture the GeoPackage tests use)"
# This is the point of using mini.gpkg rather than synthetic rows: the PostGIS source and the
# GeoPackage source can then be asserted to return the SAME features for the SAME window. A
# backend that decodes differently shows up as a diff, not as a plausible-looking map.
#
# This box's own GDAL has NO PostgreSQL driver, so ogr2ogr runs inside an OSGeo GDAL image that
# does. Verified equivalent to the PGDump (SQL-text) route on this fixture -- same 3 rows, same
# extent -- but the direct driver is the one that scales: PGDump materialises the whole dataset
# as SQL text, which is fine for 106 KB and hopeless for the 358 MB Swiss extract we will want
# for realistic numbers.
#
# --network host so the container reaches the published Postgres port. Fixtures mount read-only.
docker run --rm --network host -v "$PWD/fixtures:/fx:ro" -e PGPASSWORD="$PW" \
  ghcr.io/osgeo/gdal:ubuntu-small-latest \
  ogr2ogr -f PostgreSQL "PG:host=127.0.0.1 port=$PORT user=postgres dbname=postgres" \
  /fx/gpkg/mini.gpkg feats -nln mini_feats -lco GEOMETRY_NAME=geom -overwrite

echo "== metadata edge cases"
# Each of these exists because resolve_metadata has to tell them apart. MEASURED, not assumed:
# a pass-through view keeps its SRID, a COMPUTED-geometry view reports srid 0, and so does a
# bare `geometry` column. srid 0 is a MISS, not an answer -- accepting it would build a layer
# that reprojects from SRID 0 and misplaces every feature silently.
docker exec -i "$CONTAINER" psql -U postgres -q <<'SQL' >/dev/null
DROP VIEW IF EXISTS mini_passthrough;
CREATE VIEW mini_passthrough AS SELECT fid, name, rank, geom FROM mini_feats;

DROP VIEW IF EXISTS mini_computed;
CREATE VIEW mini_computed AS SELECT fid, name, ST_Centroid(geom) AS geom FROM mini_feats;

DROP TABLE IF EXISTS mini_untyped CASCADE;
CREATE TABLE mini_untyped (id serial PRIMARY KEY, geom geometry);
INSERT INTO mini_untyped (geom) SELECT geom FROM mini_feats;

-- Two geometry columns on one table. Without ?geom= this is ambiguous, and it used to surface
-- as tokio-postgres's "query returned an unexpected number of rows" rather than as an
-- explanation. With ?geom= each column must also resolve to ITS OWN srid, not the other's.
DROP TABLE IF EXISTS mini_twogeom CASCADE;
CREATE TABLE mini_twogeom (
  id serial PRIMARY KEY,
  geom geometry(Geometry, 4326),
  geom_3857 geometry(Geometry, 3857)
);
INSERT INTO mini_twogeom (geom, geom_3857)
  SELECT geom, ST_Transform(geom, 3857) FROM mini_feats;

-- An EMPTY geometry: decode_wkb must return Ok(None) (nothing to draw), never Err.
DROP TABLE IF EXISTS mini_empty CASCADE;
CREATE TABLE mini_empty (id serial PRIMARY KEY, geom geometry(Geometry, 4326));
INSERT INTO mini_empty (geom) VALUES (ST_GeomFromText('POLYGON EMPTY', 4326));

-- MIXED GEOMETRY TYPES, for the min-feature-size pushdown (`size_gate_sql`). mini.gpkg is all
-- polygons, so on its own it cannot catch the failure this table exists for: `ST_Area` is 0 for
-- every LineString and every Point, so a naive `ST_Area(geom) >= t` deletes every road and every
-- place and serves a 200 OK with an empty map.
--
-- Areas are chosen so ONE threshold separates them cleanly: big_poly is 100 units^2, tiny_poly is
-- 0.01. A threshold of 1.0 must drop exactly tiny_poly and nothing else.
--
-- The two lines are the other half of the trap. `ST_Area(ST_Envelope(geom))` looks like the tidy
-- branch-free alternative, but an AXIS-ALIGNED line has a degenerate envelope (area 0) while a
-- DIAGONAL line of the same length has a large one -- so that form would delete north-south and
-- east-west streets and keep the diagonals. Both spellings are here so either mistake fails.
DROP TABLE IF EXISTS mini_mixed CASCADE;
CREATE TABLE mini_mixed (id serial PRIMARY KEY, kind text, geom geometry(Geometry, 4326));
INSERT INTO mini_mixed (kind, geom) VALUES
  ('big_poly',    ST_GeomFromText('POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))', 4326)),
  ('tiny_poly',   ST_GeomFromText('POLYGON((0 0, 0.1 0, 0.1 0.1, 0 0.1, 0 0))', 4326)),
  ('ew_line',     ST_GeomFromText('LINESTRING(0 5, 10 5)', 4326)),
  ('ns_line',     ST_GeomFromText('LINESTRING(5 0, 5 10)', 4326)),
  ('diag_line',   ST_GeomFromText('LINESTRING(0 0, 10 10)', 4326)),
  ('multi_line',  ST_GeomFromText('MULTILINESTRING((1 1, 2 2),(3 3, 4 4))', 4326)),
  ('place_point', ST_GeomFromText('POINT(3 3)', 4326)),
  ('multi_point', ST_GeomFromText('MULTIPOINT((6 6),(7 7))', 4326));

ANALYZE mini_feats;
ANALYZE mini_mixed;
SQL

echo "== what the source will see"
docker exec -i "$CONTAINER" psql -U postgres -c \
  "SELECT f_table_name, srid, type FROM geometry_columns
    WHERE f_table_name LIKE 'mini%' ORDER BY 1;"

cat <<EOF
== ready

  export TERRASERVE_PG_TEST_PASSWORD=$PW
  export TERRASERVE_PG_TEST_URL='postgis://postgres:\${TERRASERVE_PG_TEST_PASSWORD}@localhost:$PORT/postgres/public.mini_feats'

Expect srid 4326 on mini_feats and mini_passthrough, and srid 0 on mini_computed and
mini_untyped -- those two are the fallback cases that require ?srid= in the URI.
mini_twogeom registers TWO rows (geom 4326, geom_3857 3857) and requires ?geom=.
EOF
