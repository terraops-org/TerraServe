#!/usr/bin/env bash
# Run the OGC CITE WMS 1.3.0 ETS against TerraServe serving the official CITE reference dataset.
#
# Result (2026-07-15): 135 passed / 0 failed / 0 skipped, areCoreConformanceClassesPassed = true.
#
# Requires: docker with the ETS container running (once):
#   docker run -d --name ets-wms13 --network host ogccite/ets-wms13:1.33-teamengine-5.7
# then, from anywhere:  terraserve/fixtures/cite/run-wms13.sh
set -euo pipefail
cd "$(dirname "$0")/../.."            # -> terraserve/
PORT="${PORT:-8091}"

cargo build --release

# Guard against the stale-server trap: kill any leftover server on the port, then serve the 4-layer
# CITE reference dataset (BasicPolygons/Forests/Lakes/NamedPlaces) via the multi-vector --config.
STALE="$(ss -tlnp 2>/dev/null | grep ":$PORT " | grep -oE 'pid=[0-9]+' | grep -oE '[0-9]+' | head -1 || true)"
[ -n "${STALE:-}" ] && kill "$STALE" 2>/dev/null || true
sleep 1
./target/release/terraserve serve --config fixtures/cite/wms13-layers.yaml \
  --host 0.0.0.0 --port "$PORT" --public-url "http://127.0.0.1:$PORT/wms" &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
sleep 2
# Verify OUR process owns the port before submitting (a stale server would silently be tested instead).
ss -tlnp 2>/dev/null | grep ":$PORT " | grep -q "pid=$SRV" || { echo "ERROR: :$PORT not owned by our server"; exit 1; }

CAP="http://127.0.0.1:$PORT/wms?service=WMS%26request=GetCapabilities%26version=1.3.0"
echo "submitting WMS 1.3.0 ETS run ..."
curl -s -u ogctest:ogctest -H 'Accept: application/rdf+xml' \
  "http://localhost:8080/teamengine/rest/suites/wms13/run?basic=basic&queryable=queryable&capabilities-url=$CAP" \
  -o /tmp/wms13-earl.rdf --max-time 900

for k in testsPassed testsFailed testsSkipped; do
  printf '%s=' "$k"; grep -oE "<cite:$k[^>]*>[0-9]+" /tmp/wms13-earl.rdf | grep -oE '[0-9]+$' | head -1
done
printf 'areCoreConformanceClassesPassed='
grep -oE 'areCoreConformanceClassesPassed[^>]*>(true|false)' /tmp/wms13-earl.rdf | grep -oE '(true|false)$' | head -1
