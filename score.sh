#!/usr/bin/env bash
# TerraServe pilot scorer.  Run from the pilot/ directory:  ./score.sh
#
#   1. builds the crate (debug),
#   2. enforces the clean-room BANNED-CRATE gate (no gdal/tiff/geotiff/COG reader),
#   3. runs the binary against every fixture case and reports PASS n/N.
#
# Exit codes: 0 = all required cases pass; 1 = some fail; 2 = build error;
#             3 = a banned crate is present.
set -uo pipefail
cd "$(dirname "$0")"

echo "== build (cargo build) =="
if ! cargo build -q 2>build.log; then
  echo "  BUILD FAILED:"; tail -25 build.log | sed 's/^/    /'
  exit 2
fi
echo "  ok"

echo "== banned-crate gate =="
# The COG container + IFD/tiling + windowed read must be BESPOKE. Ban GDAL and any
# GeoTIFF/COG/TIFF *reader* crate (the `tiff` crate is the usual shortcut). Codec
# crates (flate2/zstd/zune-jpeg/jpeg-decoder/png) and proj4rs/proj are allowed. The FlatGeoBuf
# container/header/R-tree/feature decode must also be BESPOKE — `flatbuffers` (the runtime
# codec, like zstd) is allowed, but `flatgeobuf`/`flatbush` (format-reader shortcuts) are not.
BANNED_RE='name = "(gdal|gdal-sys|tiff|geotiff|geotiff-rs|cog|cog-rs|async-tiff|tiff-decoder|libtiff-sys|geotiff2|flatgeobuf|flatbush)"'
if [ -f Cargo.lock ] && grep -qE "$BANNED_RE" Cargo.lock; then
  echo "  FAIL: a banned crate is present — build the COG reader bespoke:"
  grep -nE "$BANNED_RE" Cargo.lock | sed 's/^/    /'
  exit 3
fi
echo "  ok (no banned crates)"

echo "== sld module boundary =="
# The SLD parser (src/sld/) is rendering-agnostic + self-contained: it may depend on std +
# roxmltree ONLY, never on the rest of the crate. This keeps SLD-isms out of the renderer (the
# lowering pass in src/vector/ is the firewall) and keeps the module cleanly extractable later.
if grep -rqE 'use[[:space:]]+crate|crate::' src/sld/ 2>/dev/null; then
  echo "  FAIL: src/sld/ must depend on std + roxmltree only (no 'use crate' / 'crate::'):"
  grep -rnE 'use[[:space:]]+crate|crate::' src/sld/ | sed 's/^/    /'
  exit 3
fi
echo "  ok (sld module self-contained)"

echo "== cases =="
python3 tools/run_cases.py
