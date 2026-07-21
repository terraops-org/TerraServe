# TerraServe — production Docker image.
#
# Two-stage build:
#   1. `builder` — Debian trixie + Rust toolchain, compiles the release binary against the
#      OS-packaged libproj (the `proj` crate is a thin FFI adapter, NOT a vendored copy of PROJ).
#   2. `runtime` — slim Debian trixie with only the shared libs the binary actually links against
#      (libproj) + its data files (proj.db / grids) + curl (HEALTHCHECK only).
#
# Debian trixie is used for both stages to match the dev box (see CLAUDE.md) — same libproj
# version (9.6.x) at build and run time, and it's what `bench/Dockerfile.ts` already assumes for
# the sibling MapServer comparison image.
#
# No application data is baked into this image (see .dockerignore + DOCKER.md): COGs, GeoPackages,
# and any real style/config files are supplied at runtime via a mounted volume (default `/data`,
# see docker-compose.yml). Only the small, git-committed `fixtures/` convenience assets (fonts +
# demo styles) are copied in, so the binary's own `--font` default
# (`fixtures/fonts/DejaVuSans.ttf`, resolved relative to CWD) works out of the box.

########################################################################
# Stage 1: builder
########################################################################
FROM rust:1-trixie AS builder

# Build-time system deps for the `proj` crate (proj-sys, see Cargo.lock):
#   - libproj-dev : PROJ headers + .so + the .pc file proj-sys locates via pkg-config
#                   (proj-sys requires PROJ >= 9.2.0; trixie ships 9.6.x)
#   - pkg-config  : already present in the `rust:*-trixie` image, listed for clarity
#   - clang       : provides libclang, which `bindgen` (a proj-sys build dependency) needs to
#                   generate the PROJ FFI bindings against the system headers
# rusqlite is built with the "bundled" feature (vendors + compiles its own SQLite via the `cc`
# crate), so no libsqlite3-dev is needed — the `gcc` already in this image is enough.
RUN apt-get update && apt-get install -y --no-install-recommends \
      libproj-dev \
      pkg-config \
      clang \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# --- dependency layer -------------------------------------------------
# Build a throwaway binary+lib against only Cargo.toml/Cargo.lock first, so the (slow) crate
# dependency compilation — proj-sys, rusqlite bundled sqlite, harfrust, tiny-skia, etc. — is its
# own Docker layer and stays cached across rebuilds that only change src/.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo "fn main() {}" > src/main.rs \
    && echo "// dependency-warm dummy" > src/lib.rs \
    && cargo build --release \
    && rm -rf src

# --- real source --------------------------------------------------------
COPY src ./src
# Make sure cargo sees the real files as newer than the dummy ones it just compiled
# (belt-and-braces across filesystems with coarse mtime resolution).
RUN touch src/main.rs src/lib.rs && cargo build --release

########################################################################
# Stage 2: runtime
########################################################################
FROM debian:trixie-slim AS runtime

# OCI metadata. Without org.opencontainers.image.source a GHCR package has no link back to
# its repository, shows no README and no licence, and cannot inherit the repo's visibility
# — it just appears as an orphan under the org.
LABEL org.opencontainers.image.source="https://github.com/terraops-org/TerraServe" \
      org.opencontainers.image.description="Clean-room raster + vector map tile server in Rust — WMS/WMTS/TMS/MVT, no GDAL." \
      org.opencontainers.image.licenses="AGPL-3.0-or-later" \
      org.opencontainers.image.title="TerraServe"

# Runtime deps:
#   - libproj25       : the PROJ shared library the binary dynamically links against
#   - proj-data       : PROJ's transformation grids + proj.db — needed for CRS lookups/datum
#                        shifts beyond a bare identity transform
#   - ca-certificates : TLS roots, only exercised for optional `s3://` COG sources
#   - curl            : used solely by the HEALTHCHECK below (a few KB on top of the libcurl
#                        already pulled in transitively by libproj25's network-grid support)
RUN apt-get update && apt-get install -y --no-install-recommends \
      libproj25 \
      proj-data \
      ca-certificates \
      curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /build/target/release/terraserve /usr/local/bin/terraserve

# Small, git-committed convenience fixtures only (fonts + demo styles/vector) — NOT the real
# datasets. `fixtures/goldens/` (test-comparison PNGs, ~14 MiB, irrelevant at runtime) is
# excluded via .dockerignore. Real data (gpkg/, cogs/, production styles) is mounted at /data.
COPY fixtures ./fixtures

EXPOSE 8080

# Cheap liveness probe: `/` is a static text banner served with no layer/render work, so it
# proves the axum listener is up and (since layers load before the listener binds) that the
# configured layer(s) parsed successfully at startup.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/ || exit 1

ENTRYPOINT ["terraserve"]
# Default demo command — serves the committed airports fixture. Override via `docker run <image>
# serve --config /data/layers.yaml ...` (or the `command:` in docker-compose.yml) to point at
# real mounted data. --host 0.0.0.0 is required for the container's network to be reachable.
# Default: serve the airports fixture that is BAKED INTO the image, so a bare
# `docker run -p 8080:8080 <image>` works with no volume and no flags — that first
# command is most people's entire evaluation of the project. This previously pointed at
# /data/, which is a MOUNT POINT: with no -v it is empty, and the container exited with
# "read /data/airports.vec.json: No such file or directory".
# Override for real data: `docker run <image> serve --config /data/layers.yaml ...`
# (or the `command:` in docker-compose.yml). --host 0.0.0.0 is required for the
# container's port to be reachable from outside it.
CMD ["serve", \
     "--host", "0.0.0.0", "--port", "8080", \
     "--vector", "/app/fixtures/vector/airports.geojson", \
     "--vec-style", "/app/fixtures/styles/airports.vec.json", \
     "--name", "airports"]
