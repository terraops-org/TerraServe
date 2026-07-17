#!/usr/bin/env bash
# Run the OGC CITE WMS 1.1.1 ETS (ets-wms11) against TerraServe serving the CITE reference dataset.
#
# Result (2026-07-16): 104 passed / 1 failed / 0 skipped, areCoreConformanceClassesPassed = true (~99%).
# The 1 non-conformance is a documented known-limitation (optional-tier, NOT core):
#   - wmsops-getfeatureinfo-params-info_format-3: schema-valid GML GetFeatureInfo output vs the
#     Galdos GML validator. TerraServe emits a minimal (label-only) GML; a schema-valid GML2/
#     DescribeFeatureType output is deferred (needs its own WFS/GML spec). NOT IMPLEMENTED (later).
#
# Requires the ETS container running once:
#   docker run -d --name ets-wms11 --network host ogccite/ets-wms11:1.22-teamengine-5.4.1
# then, from anywhere:  terraserve/fixtures/cite/run-wms11.sh
set -euo pipefail
cd "$(dirname "$0")/../.."            # -> terraserve/
PORT="${PORT:-8091}"

cargo build --release

# Guard against the stale-server trap: kill any leftover server on the port, then serve the 4-layer
# CITE reference dataset (BasicPolygons/Forests/Lakes/NamedPlaces) via the multi-vector --config.
STALE="$(ss -tlnp 2>/dev/null | grep ":$PORT " | grep -oE 'pid=[0-9]+' | grep -oE '[0-9]+' | head -1 || true)"
[ -n "${STALE:-}" ] && kill "$STALE" 2>/dev/null || true
sleep 1
./target/release/terraserve serve --config fixtures/cite/wms11-layers.yaml \
  --host 0.0.0.0 --port "$PORT" --public-url "http://127.0.0.1:$PORT/wms" &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
sleep 2
# Verify OUR process owns the port before submitting (a stale server would silently be tested instead).
ss -tlnp 2>/dev/null | grep ":$PORT " | grep -q "pid=$SRV" || { echo "ERROR: :$PORT not owned by our server"; exit 1; }

CAP="http://127.0.0.1:$PORT/wms?service=WMS%26request=GetCapabilities%26version=1.1.1"
echo "submitting WMS 1.1.1 ETS run (queryable + GFI + recommended) ..."
curl -s -u ogctest:ogctest -H 'Accept: application/rdf+xml' \
  "http://localhost:8080/teamengine/rest/suites/wms11/run?profile=queryable&updatesequence=auto&getfeatureinfo=true&recommended=true&feesconstraints=false&bboxconstraints=either&capabilities-url=$CAP" \
  -o /tmp/wms11-earl.rdf --max-time 1200

for k in testsPassed testsFailed testsSkipped; do
  printf '%s=' "$k"; grep -oE "<cite:$k[^>]*>[0-9]+" /tmp/wms11-earl.rdf | grep -oE '[0-9]+$' | head -1
done
