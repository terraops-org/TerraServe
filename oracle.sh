#!/usr/bin/env bash
# Shift-sensitive correctness oracle: TerraServe vs GDAL (gdalwarp + gdallocationinfo).
#
# Proves what score.sh's generous golden CANNOT: that georegistration is sub-pixel correct and
# point values are exact (the P0 "data rendered in the wrong place" failure class). GDAL is an
# EXTERNAL oracle (never linked into the engine) invoked via std::process::Command.
#
# Requires GDAL on PATH + the polar fixture (cogs/polar/). Without them the tests SELF-SKIP so the
# fast `cargo test` path stays green; set ORACLE_REQUIRE_GDAL=1 (e.g. in the nightly CI job) to fail
# loudly instead of skipping silently.
set -euo pipefail
cd "$(dirname "$0")"

FIXTURE=../cogs/polar/arcticdem_18_47_32m_gunnbjorn_dem.tif

if [ "${ORACLE_REQUIRE_GDAL:-0}" = "1" ]; then
  command -v gdalwarp >/dev/null 2>&1 || { echo "FATAL: gdalwarp not on PATH (ORACLE_REQUIRE_GDAL=1)"; exit 1; }
  command -v gdallocationinfo >/dev/null 2>&1 || { echo "FATAL: gdallocationinfo not on PATH"; exit 1; }
  [ -f "$FIXTURE" ] || { echo "FATAL: polar fixture missing: $FIXTURE (ORACLE_REQUIRE_GDAL=1)"; exit 1; }
elif ! command -v gdalwarp >/dev/null 2>&1; then
  echo "note: gdalwarp not found — oracle tests will self-skip (set ORACLE_REQUIRE_GDAL=1 to fail)."
fi

echo "== TerraServe correctness oracle =="
gdalwarp --version 2>/dev/null | head -1 || true
echo

cargo test --release \
  --test oracle_align \
  --test oracle_gdal_warp \
  --test oracle_gdal_point \
  -- --nocapture

echo
echo "oracle done. Tiers: detector self-test (no GDAL) · georegistration vs gdalwarp (< 0.3 px) ·"
echo "point value vs gdallocationinfo (exact). See docs/oracle-testrig.md."
