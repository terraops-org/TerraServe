# TerraServe — Docker deployment

Production image for the `terraserve` binary (WMS/TMS/WMTS/MVT server). Files:
`Dockerfile`, `.dockerignore`, `docker-compose.yml` (this directory, build context = `.`).

## Design decisions

- **Base images**: `rust:1-trixie` (builder) → `debian:trixie-slim` (runtime). Debian trixie on
  both stages to match the dev box's PROJ 9.6.x (see root `CLAUDE.md`) — build-time and run-time
  `libproj` are the same version, and it lines up with `bench/Dockerfile.ts`'s trixie base.
- **libproj handling**: the `proj` crate (`proj-sys`) links dynamically against the *system*
  libproj via `pkg-config` (it does not vendor/build PROJ from source — see `proj-sys`'s
  `build.rs`; that path only triggers with an opt-in `bundled_proj` feature, which is not
  enabled here). So: builder installs `libproj-dev` + `pkg-config` + `clang` (the last is for
  `bindgen`, which needs `libclang` to generate the PROJ FFI bindings); runtime installs
  `libproj25` + `proj-data` (the shared lib + its transformation grids/`proj.db`, needed for
  anything beyond an identity transform). Confirmed via `ldd` against the dev box's
  `libproj.so.25` and the crate's own `build.rs`, not assumed.
- **rusqlite**: built with the `bundled` feature (vendors + statically compiles its own SQLite
  via the `cc` crate), so **no** `libsqlite3-dev`/`libsqlite3` package is needed at build or run
  time for the GeoPackage reader — only PROJ itself pulls in a *dynamic* `libsqlite3` as one of
  its own transitive deps (confirmed via `ldd`), which `apt` resolves automatically.
- **TLS**: `ureq` (the S3 client) resolves to `rustls` in `Cargo.lock`, not OpenSSL — no
  `libssl`/`libssl-dev` needed anywhere. `ca-certificates` is still installed at runtime for the
  TLS root store (only exercised by `s3://` COG sources).
- **Dependency-layer caching**: the builder compiles a throwaway `fn main(){}` against just
  `Cargo.toml`/`Cargo.lock` first, so the expensive part (proj-sys, rusqlite's vendored SQLite,
  harfrust, tiny-skia, …) is its own Docker layer and survives rebuilds that only touch `src/`.
- **No data baked in**: `gpkg/` and `cogs/` are large and gitignored, and are never `COPY`'d.
  Real data is supplied at runtime via a mounted volume (`/data` by convention — see
  `docker-compose.yml`). **Fixtures decision**: the whole `fixtures/` dir minus
  `fixtures/goldens/` (test-comparison PNGs, ~14 MiB, irrelevant at runtime) **is** `COPY`'d into
  the image at `/app/fixtures` (`WORKDIR /app`). It's small (~3 MiB) and makes the binary's own
  `--font` default (`fixtures/fonts/DejaVuSans.ttf`, resolved relative to CWD) and the demo
  styles/vector fixtures work with zero mounting — convenient for a quick `docker run` smoke
  test or the compose default below. Anything beyond that (real GPKG/COG data, production
  styles/`layers.yaml`) is always a mount, never baked in.
- **Image size**: see the measured size at the bottom of this file (filled in after the verified
  build below) — runtime is `debian:trixie-slim` + `libproj25`/`proj-data`/`curl`/
  `ca-certificates` + a single static-ish Rust binary + ~3 MiB of fixtures.
- **Healthcheck**: `GET /` — a static text banner handled with no layer/render work, so it's
  cheap; since layers are parsed at startup *before* the listener binds, a 200 here also proves
  the configured layer(s) loaded successfully. `/wms?SERVICE=WMS&REQUEST=GetCapabilities&
  VERSION=1.3.0` is a heavier alternative if you want the WMS stack itself exercised.

## Build

```bash
cd terraserve
docker build -t terraserve:latest .
```

## Run (plain `docker run`, no compose)

Mount read-only data at `/data` and bind the port; `--host 0.0.0.0` is required for the
container to be reachable from outside itself.

```bash
docker run --rm -d --name terraserve -p 8080:8080 \
  -v "$(pwd)/fixtures/vector:/data/vector:ro" \
  -v "$(pwd)/fixtures/styles:/data/styles:ro" \
  terraserve:latest \
  serve --host 0.0.0.0 --port 8080 \
        --vector /data/vector/airports.geojson --vec-style /data/styles/airports.vec.json
```

Other layer shapes (same image, different `command`):

```bash
# a COG (mount the directory containing it)
docker run --rm -p 8080:8080 -v /path/to/cogs:/data:ro terraserve:latest \
  serve --host 0.0.0.0 --port 8080 --cog /data/cascais.cog.deflate.tif --style /app/fixtures/styles/rgb.json

# multi-layer config
docker run --rm -p 8080:8080 -v /path/to/data:/data:ro terraserve:latest \
  serve --host 0.0.0.0 --port 8080 --config /data/layers.yaml

# vector tiles with a custom MapLibre style + a feature cap
docker run --rm -p 8080:8080 -v /path/to/data:/data:ro terraserve:latest \
  serve --host 0.0.0.0 --port 8080 --vector /data/big.gpkg --vec-style /data/style.json \
        --mvt-style /data/maplibre-style.json --mvt-max-features 20000
```

## Run (compose)

```bash
docker compose up          # serves the committed airports demo fixture on :8080, zero config
```

Point at real data:

```bash
TERRASERVE_DATA_DIR=/srv/terraserve-data \
TERRASERVE_VECTOR=parcels.gpkg TERRASERVE_STYLE=parcels.vec.json \
  docker compose up
```

or override `command:` entirely with `docker compose run --rm terraserve serve --config /data/layers.yaml --host 0.0.0.0 --port 8080`.

## Caveats

- The concurrent Rust-source edits noted in this deployment task mean a `docker build` can hit a
  transient *compile* error unrelated to the Dockerfile — that's the source mid-edit, not a
  packaging problem; re-run the build.
- `proj-data` in the runtime image is the full Debian PROJ datum/grid package (not a trimmed
  subset) — simplest and most correct, at the cost of a few extra MiB; revisit only if image size
  becomes a real constraint.
- The compose file's default volume mounts this repo's `fixtures/` directory itself (nested
  `vector/`/`styles/` subdirs) for the zero-config demo path; a real deployment's `/data` is
  expected to be a flat directory of your own files.
