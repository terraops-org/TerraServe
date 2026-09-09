#!/usr/bin/env bash
# Prove a built terraserve binary actually WORKS, not merely that it starts.
#
# Why this exists: `--version` and `--help` never touch PROJ, never decode anything and never
# encode a PNG, so a binary that cannot reproject passes both and then fails on the first real
# request. A release that ships such a binary is worse than shipping none. Three times while
# building the standalone binary an exit code of 0 came back from a run that had produced a
# fully transparent tile, so nothing here trusts an exit code alone: every check asserts on
# content.
#
# Runs against the SHIPPED artifact, so CI and a developer check the same thing:
#   tools/smoke_binary.sh ./target/release/terraserve
#
# Must be run from the crate root (it uses the committed fixtures/ tree).

set -euo pipefail

BIN="${1:?usage: smoke_binary.sh <path to terraserve binary>}"
[ -x "$BIN" ] || { echo "FAIL: $BIN is not executable"; exit 1; }
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"

PORT="${SMOKE_PORT:-18731}"
WORK="$(mktemp -d)"
SERVER_PID=""
cleanup() {
  # Kill only the PID we started. NEVER pkill/pgrep -f here: the pattern matches the
  # script's own shell and takes the session down with it.
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# A cache directory of our own, so a first run is exercised every time rather than silently
# reusing a proj.db some earlier run left in the real cache.
export XDG_CACHE_HOME="$WORK/cache"
unset PROJ_DATA PROJ_LIB || true

fail() { echo "FAIL: $*"; exit 1; }
ok()   { echo "  ok: $*"; }

echo "== 1. terraserve info: does PROJ have a database at all =="
INFO="$("$BIN" info)" || fail "terraserve info exited non-zero"
echo "$INFO" | sed 's/^/     /'
echo "$INFO" | grep -q '^proj.db      NOT FOUND' && fail "PROJ resolved no proj.db; every reprojection would fail"
DB_PATH="$(echo "$INFO" | awk '/^proj\.db/ {print $2}')"
[ -n "$DB_PATH" ] && [ -f "$DB_PATH" ] || fail "proj.db path '$DB_PATH' does not exist"
ok "PROJ resolved $DB_PATH"

# On a bundled build the database MUST come from our own extraction, not from a system copy
# that happens to exist on the build machine. That distinction is the entire point of
# embedding it, and it is invisible to every other check.
if echo "$INFO" | grep -q 'statically linked, vendored'; then
  case "$DB_PATH" in
    "$XDG_CACHE_HOME"/*) ok "bundled build used its OWN extracted copy" ;;
    *) fail "bundled build used $DB_PATH instead of its embedded copy under $XDG_CACHE_HOME" ;;
  esac
fi

echo "== 2. serve the committed vector fixtures =="
"$BIN" serve --config fixtures/fgb/multi.yaml --port "$PORT" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 50); do
  curl -fsS "http://127.0.0.1:$PORT/wms?SERVICE=WMS&REQUEST=GetCapabilities" -o /dev/null 2>/dev/null && break
  kill -0 "$SERVER_PID" 2>/dev/null || { sed 's/^/     /' "$WORK/server.log"; fail "server exited during startup"; }
  sleep 0.2
done
curl -fsS "http://127.0.0.1:$PORT/wms?SERVICE=WMS&REQUEST=GetCapabilities" -o "$WORK/caps.xml" \
  || { sed 's/^/     /' "$WORK/server.log"; fail "GetCapabilities never answered"; }
grep -q '<Name>lines</Name>' "$WORK/caps.xml" || fail "capabilities do not advertise the 'lines' layer"
ok "GetCapabilities advertises the fixture layers"

echo "== 3. GetMap in EPSG:3857 from EPSG:4326 data (forces a real datum transform) =="
# lines.fgb is EPSG:4326 covering [10,6.5]-[12,11]; this bbox is that extent in EPSG:3857, so
# answering it at all requires PROJ to resolve both CRSs out of proj.db.
URL="http://127.0.0.1:$PORT/wms?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=lines"
URL="$URL&CRS=EPSG:3857&BBOX=1113195,725000,1335834,1233000&WIDTH=256&HEIGHT=256"
URL="$URL&FORMAT=image/png&STYLES=&TRANSPARENT=TRUE"
curl -fsS "$URL" -o "$WORK/tile.png" -w '     http %{http_code}  %{content_type}  %{size_download} bytes\n' \
  || { sed 's/^/     /' "$WORK/server.log"; fail "GetMap request failed"; }

# The decisive check. A GetMap that silently produced an empty tile returns 200, a valid PNG
# and exit 0; only the pixels tell you whether anything was actually drawn.
python3 - "$WORK/tile.png" <<'PY'
import struct, sys, zlib
data = open(sys.argv[1], 'rb').read()
if data[:8] != b'\x89PNG\r\n\x1a\n':
    sys.exit("FAIL: response is not a PNG")
i, idat, w, h, depth, ctype = 8, b'', 0, 0, 0, 0
while i < len(data):
    ln = struct.unpack('>I', data[i:i + 4])[0]
    typ = data[i + 4:i + 8]
    if typ == b'IHDR':
        w, h, depth, ctype = struct.unpack('>IIBB', data[i + 8:i + 18])
    elif typ == b'IDAT':
        idat += data[i + 8:i + 8 + ln]
    i += 12 + ln
if (w, h) != (256, 256):
    sys.exit(f"FAIL: expected a 256x256 image, got {w}x{h}")
if depth != 8 or ctype not in (2, 6):
    sys.exit(f"FAIL: unexpected PNG format depth={depth} colortype={ctype}")
nch = 4 if ctype == 6 else 3
raw = zlib.decompress(idat)
stride = 1 + w * nch          # each scanline carries a leading filter byte
# Undo the PNG row filters; without this, filtered rows decode to noise and the pixel
# counts below would be meaningless.
prev = bytearray(w * nch)
drawn = 0
for r in range(h):
    f = raw[r * stride]
    row = bytearray(raw[r * stride + 1:(r + 1) * stride])
    for x in range(len(row)):
        a = row[x - nch] if x >= nch else 0
        b = prev[x]
        c = prev[x - nch] if x >= nch else 0
        if f == 1:   row[x] = (row[x] + a) & 0xFF
        elif f == 2: row[x] = (row[x] + b) & 0xFF
        elif f == 3: row[x] = (row[x] + (a + b) // 2) & 0xFF
        elif f == 4:
            p = a + b - c
            pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
            pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
            row[x] = (row[x] + pr) & 0xFF
    for x in range(0, len(row), nch):
        if nch == 4:
            if row[x + 3] != 0:
                drawn += 1
        elif row[x:x + 3] != b'\x00\x00\x00':
            drawn += 1
    prev = row
print(f"     {w}x{h}, {drawn} non-transparent pixels")
if drawn < 100:
    sys.exit(f"FAIL: only {drawn} pixels drawn - the tile is effectively blank, so the "
             f"reprojection or the rasterizer produced nothing")
print("  ok: the tile contains real rendered geometry")
PY

echo
echo "SMOKE TEST PASSED: $BIN reprojects, renders and encodes."
