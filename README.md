# TerraServe

**A clean-room map tile engine in Rust. No GDAL, no MapServer, no GeoServer.**

TerraServe reads Cloud-Optimized GeoTIFFs and GeoPackage, FlatGeoBuf or GeoJSON vectors, reprojects
and rasterizes just the window you ask for, styles it, and serves it over **WMS · WMTS · TMS ·
vector tiles (MVT) · PMTiles**, with none of the usual stack underneath. Every file reader, the
reprojection plumbing, the tiling, the styling and the OGC protocol layer are written from scratch.
The result is small, kind to your memory (a request's buffers are freed the instant it returns: no
garbage collector, no caches that grow forever), and certifiable against the OGC standards.

[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue)](LICENSE) · **OGC CITE WMS 1.3.0: 135 / 0** (certifiable) · **Rust**

### 🌍 Live demos, docs & benchmarks → **[terraserve.io](https://terraserve.io)**  ·  built by **[TerraOps](https://terraops.org)**

## Get it running

The quickest path is Docker. Build the image, serve a dataset, then point QGIS at
`http://localhost:8080/wms`, or open the viewer at `http://localhost:8080/viewer`:

```bash
docker build -t terraserve .
docker run -p 8080:8080 -v "$PWD/data:/data:ro" terraserve \
  serve --cog /data/ortho.cog.tif --style /data/rgb.json --host 0.0.0.0 --port 8080
```

Prefer to build **from source**? You will need Rust and `libproj`:

```bash
cargo build --release
./target/release/terraserve serve --cog ortho.cog.tif --style fixtures/styles/rgb.json --port 8080
cargo test && ./score.sh    # tests + banned-crate gate + fixture regression, expect 39/39 (+2/2 optional)
```

To serve **vector** data instead of a raster, swap `--cog`/`--style` for `--vector`/`--vec-style`
(see the [CLI](#cli) below). The full flag reference and the pitfalls worth knowing are at
**[terraserve.io/docs](https://terraserve.io/docs)**.

## Why it's different

- **No GDAL.** The whole server is a ~150 MB container with no heavyweight geospatial dependency,
  about a quarter the size of the alternatives, and free of the library behind most "my map server
  ate all the RAM and crashed" stories.
- **Bounded memory.** Under a burst it holds a firm ceiling and hands memory back when the rush
  passes. Admission control queues excess work instead of piling it up in RAM.
- **Windowed reads.** Point it at a multi-gigabyte file and it opens in *megabytes*, because it reads
  only the piece under the current view, not the whole file (a 3.8 GB, 14.9 M-feature dataset opens
  in about 4 MB).
- **Honest about speed.** On live vector rendering it is *on par* with MapServer 8.6 on throughput and
  ahead of GeoServer, and it holds a far tighter **tail latency under burst** (where a fixed worker
  pool falls off a cliff), from a fraction of the memory and image size. Full results at
  [terraserve.io](https://terraserve.io/#bench).
- **Certifiable.** OGC CITE WMS 1.3.0 = **135 / 0** (`areCoreConformanceClassesPassed = true`), and
  WMS 1.1.1 is core-certifiable, on par with MapServer, GeoServer and QGIS Server.

## Live demos

Three real datasets on a small server, each exercising a different part of the engine. Have a look at
[terraserve.io/examples](https://terraserve.io/examples/):

- **cos2023**: Portugal's official land cover (842 k shapes), with shared-border simplification and pre-baked tiles.
- **vida**: 14.9 M building outlines across Iberia, via windowed reads and precomputed low zoom.
- **ndvi**: vegetation health computed **live** per tile from raw Sentinel-2 bands, with no pre-made image.

## Sample data

TerraServe ships **no datasets**. Any Cloud-Optimized GeoTIFF or GeoPackage, FlatGeoBuf or GeoJSON
works. To reproduce the demos, the underlying data is all public:

- **Land cover (cos2023)**: Direção-Geral do Território (DGT), *Carta de Uso e Ocupação do Solo (COS),
  Série 2, 2023*, metadata at [Carta de Ocupação do Solo Conjuntural](https://snig.dgterritorio.gov.pt/rndg/srv/api/records/e9d25fc4-5a25-4c5e-9bce-d470f745d89e).
- **Buildings (vida)**: VIDA *Combined Open Buildings* (Google Open Buildings + Microsoft Global ML
  Buildings + OpenStreetMap), published as open data.
- **NDVI (ndvi)**: Sentinel-2 imagery from the Copernicus Data Space:
  [dataspace.copernicus.eu](https://dataspace.copernicus.eu).

> **Note:** `cargo test` and `score.sh` expect a small set of sample rasters and vectors next to the
> crate (the paths are in `tests/`). They aren't bundled, so supply your own, or run the engine
> directly with the CLI below against any dataset.

## Capabilities

**Raster**: bespoke COG / BigTIFF reader (DEFLATE + LZW + horizontal/float predictor + YCbCr-JPEG +
ZSTD + WEBP, overviews, mask/alpha, **8 dtypes lossless**), lazy/windowed open for huge files,
warp/resample (nearest and bilinear), reprojection via libproj (including **polar UPS**), styling
(RGB(A) passthrough and pseudocolor ramp), on-the-fly **band-math / NDVI**, **S3** cloud COGs, and
**multi-layer** YAML config.

**Vector**: GeoJSON, **native GeoPackage** and **FlatGeoBuf** readers (bespoke WKB decoder,
`rusqlite` container, OGC R-tree **windowed reads**), feeding **tiny-skia** polygon, line and point
rasterization, **SLD-first** styling (SLD 1.0 lowered to a Style IR) with a point-label engine,
per-zoom LOD (shared-arc simplification), and offline **PMTiles** baking. Proven live on the
Portuguese BUPi cadastre (**3.4 M parcels**) and the VIDA Iberia buildings (**14.9 M**).

**Protocols**: WMS 1.1.1 / 1.3.0 (GetMap · GetCapabilities · GetFeatureInfo · GetLegendGraphic, incl.
EPSG:4326 axis flip and exceptions), **WMTS 1.0.0** (KVP and RESTful), **OSGeo TMS 1.0.0**, and **MVT
vector tiles** (bespoke protobuf encoder and TileJSON), all over one engine. There is a raster viewer
at `/viewer` and a cyan-on-black **"X-ray"** vector-tile inspector at `/xray`.

## CLI

```bash
# render one raster window to a PNG (the engine core, no server)
terraserve render --cog ortho.cog.tif --bbox minx,miny,maxx,maxy --crs EPSG:3857 \
  --width 512 --height 512 --resample bilinear --style fixtures/styles/rgb.json --out out.png

# native GeoPackage vector over WMS + MVT (auto-detects the layer CRS)
terraserve serve --vector data.gpkg --vec-style fixtures/styles/cos2023.sld \
  --name mylayer --host 0.0.0.0 --port 8080

# on-the-fly NDVI band-math from a Sentinel-2 COG
terraserve serve --cog s2_stack.cog.tif --style fixtures/styles/ndvi.json --src-crs EPSG:32629 \
  --expression "(B08 - B04) / (B08 + B04)" --bands B02,B03,B04,B08 --port 8080

# many layers from one process
terraserve serve --config fixtures/layers.example.yaml --port 8080
```

Full flag reference, the multi-layer YAML, and the **pitfalls** worth knowing before you deploy are at
**[terraserve.io/docs](https://terraserve.io/docs)**. How to style layers (raster ramps, SLD,
MapLibre GL): **[terraserve.io/styling](https://terraserve.io/styling)**.

## The clean-room constraint

`score.sh` enforces a **banned-crate gate** on every build: it forbids `gdal` and every off-the-shelf
`tiff` / `geotiff` / `cog` / `flatgeobuf` **reader** crate. The COG container, IFD/tiling, windowed
reads, warp/resample kernels, WKB/GeoPackage decoder, spatial-index traversal, style engine and OGC
protocol layer are all bespoke. Only codec and infra crates (flate2 / zstd / weezl / zune-jpeg / png,
bundled `rusqlite`, tiny-skia) and the `proj` FFI (coordinate transforms only) are leaned on. The
constraint can't drift, because CI fails the moment a banned crate appears.

## Architecture (`src/`)

| module / dir | role |
|---|---|
| `cog.rs` | bespoke TIFF/BigTIFF container + IFD/tile/overview parsing (dual-mode: resident or lazy/windowed) |
| `decode.rs` | tile codecs: DEFLATE + LZW (+ predictors), YCbCr-JPEG / ZSTD / WEBP; 8 dtypes lossless |
| `reproj.rs` | CRS transforms: a thin adapter over libproj (PROJ is not reimplemented) |
| `render.rs` | pipeline: parse → grid via PROJ → overview → decode → warp/resample → style; `sample_point` (GFI) |
| `backend.rs` | `RenderBackend`: batch-first, buffer-oriented (CPU impl; a wgpu backend is a later port) |
| `style.rs`, `expr.rs` | RGB/pseudocolor styling; safe RPN band-math (NDVI), no code-exec |
| `wms.rs` | WMS 1.1.1 / 1.3.0 GetMap / GetCapabilities / GetFeatureInfo / GetLegendGraphic |
| `tms.rs`, `tms_http.rs`, `wmts.rs` | generic `TileMatrixSet` core (a tile IS a GetMap) + OSGeo TMS + WMTS |
| `sld/` | SLD 1.0 front-end (`roxmltree`, boundary-gated): parse → model → filter → lower to the Style IR |
| `vector/` | GeoJSON / GeoPackage / FlatGeoBuf readers, tiny-skia raster, label engine, Style IR + SLD lowering |
| `vector/mvt/`, `vector/pmtiles/` | bespoke MVT protobuf encoder + tile clip; PMTiles read + offline bake + write-through cache |
| `s3.rs`, `cache.rs`, `config.rs` | SigV4 S3 range reader; bounded LRU tile cache (moka); multi-layer YAML |
| `server.rs`, `pngio.rs`, `lib.rs`, `main.rs` | async axum/tokio server + PNG encode + CLI plumbing |

## Design principles

- **Async-first, CPU work off the reactor.** The request/I/O path is async; `decode` / `warp` /
  `colorize` / `rasterize` are sync kernels dispatched via `spawn_blocking` / rayon so the reactor
  never stalls.
- **I/O is the bottleneck, not the math.** Throughput is won in fetch scheduling and the tile cache,
  not micro-optimized arithmetic.
- **GPU-capable, CPU-first.** `RenderBackend` stays batch-first so a `wgpu` backend is a port, not a rewrite.
- **Correctness first**, validated against GDAL and PROJ as an *external* oracle (never linked):
  sub-pixel georegistration vs `gdalwarp`, exact point-values vs `gdallocationinfo`, plus OGC CITE.

## License

**MPL-2.0**, see [LICENSE](LICENSE). File-level copyleft: TerraServe can be used in a larger work
under a license of your choice (including proprietary), but modifications to TerraServe's own source
files stay under the MPL. This keeps the engine freely reusable, for example by the MIT-licensed
[pygeoapi](https://pygeoapi.io), while improvements to it come back to the project.

© 2026 TerraOps. TerraServe™.
