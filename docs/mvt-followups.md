# MVT vector-tile follow-ups

Tracking file for the bespoke MVT line (`src/vector/mvt/`, `src/mvt_http.rs`, `src/xray.html`).
Started from the Task-4/Task-5 reviews and the **2026-07-13 speed/efficiency audit** (BUPi 3.4M
parcels served unusable 23-28 s / 150-360 MB tiles). Grouped: **done** vs **open**.

## Done (2026-07-13, commit `aef9cc9` + the spinner commit)

- **Spatial bbox pre-filter** (`tile.rs`) — reproject the tile bbox into the source CRS once
  (`crs_bounds_expanded`, +1/16 BUF margin), then cheap-reject any feature whose source-CRS bbox
  misses the tile *before* the per-vertex libproj projection. O(all-features) → O(in-tile). This is
  the 23 s → sub-second win.
- **Survivor-scoped attribute pool** (`tile.rs`) — intern keys/values from the *emitted* features
  only, not all N. BUPi's ~3.4M unique parcel-ID strings no longer bloat every tile. 150-360 MB →
  few MB.
- **Per-tile feature budget** (`MAX_FEATURES_PER_TILE = 20_000`) — bounds genuinely dense low-zoom
  overview tiles. Over-budget tiles are **uniformly sampled** (`sampled_positions`, fixed float
  stride) across the whole candidate range, **not** first-N by insertion order. First-N clustered
  spatially (GPKG rowid ≈ concelho insertion order) and left **tile-aligned rectangular holes** at
  the overview (looked like "missing tiles"); uniform sampling spreads the kept features evenly.
  Verified: a z8 BUPi overview tile's 19,992 features now occupy 91% of a 16×16 grid of the tile
  (was clustered in a corner).
- **MultiPolygon winding fix** (`geom.rs` `encode_multipolygon` + `tile.rs` per-polygon clipping) —
  was the HIGH Task-4 follow-up. `encode_polygon` treated only ring 0 of a flattened list as
  exterior, so a MultiPolygon's 2nd+ piece was wound like a hole (inverted). Now each polygon
  restarts the exterior/hole winding. Verified live: BUPi z13 tile decodes (external
  `mapbox_vector_tile`) to 284 Polygon + 9 MultiPolygon, correct.
- **TileJSON schema cache** (`mvt_http.rs` `feature_field_schema` → `VectorLayer.fields`, computed
  once at load) — the field schema was recomputed by scanning all features on every TileJSON
  request (~1.6 s at BUPi scale, blocking viewer/QGIS layer-add). 1.6 s → 0.0004 s.
- **X-ray tile-loading spinner + tile-error indicator** (`xray.html`) — counts in-flight tile
  requests across every layer source (OL `tileloadstart`/`end`/`error`); a fresh load batch clears
  the prior error tally.

Measured (WebMercatorQuad, central-Portugal tiles): z6 27.7 s/361 MB → **0.11 s/2.6 MB**; z10
22.9 s/159 MB → **0.13 s/2.4 MB**; z12 23.7 s/153 MB → **0.46 s/0.87 MB**. `score.sh` 30/30 +2/2;
golden byte-stable (wire format unchanged); full suite green.

## Done (2026-07-13, session 2 — `997c30a..6ae7b6c`)

- **`--mvt-max-features N` runtime flag** — the per-tile budget was a `const`; now a `serve` flag
  (0 = unlimited), on `ServeState.mvt_max_features`. `encode_tile` stays a default-budget wrapper
  over `encode_tile_budgeted`.
- **Advertised host from the request `Host` header** (`advertised_origin`) — TileJSON/style URLs
  were `http://0.0.0.0:PORT/…` (the bind address, unreachable off-box). Now they self-describe on
  whatever host the client reached, falling back to `base_url`/`--public-url`.
- **`GET /mvt/{layer}/style.json`** — a MapLibre GL Style JSON (one URL for QGIS's *Style URL* /
  MapLibre / the X-ray): `sources` → the layer's TileJSON, `layers` → a generic X-ray style, OR an
  operator style via **`--mvt-style FILE`** (`{ layers, metadata }`; engine injects
  version/sources/source-binding, passes `metadata` through). `$type`-gated layers (no centroid dots).
- **DGT COS2018 land-cover** served with zero engine change (EPSG:3763 auto-detected → 3857 MVT);
  `fixtures/styles/cos2018.mvt-style.json` = the official DGT legend (83 classes → `fill-color` match
  on `COS18_n4_C` + 9-megaclass legend), from the QGIS QML at PedroVenancio/cos_2018_dgt_symbology.
- **X-ray**: thematic fill from `style.json` (`parseFillMatch`) + a **Legend** section + **click-to-
  highlight** (selection overlay by feature id).
- **Fix — polar z0 pre-filter**: `reproj::crs_bounds` now grid-samples (interior + edges) so a
  pole-centred UPS projection's interior pole is captured (was emptying the polar-grid z0 tile).
  Regression test `crs_bounds_captures_an_interior_pole`.

## Open — performance (from the efficiency audit, ranked)

1. **Precompute the per-feature source-CRS bbox at load** (highest value, ~10× on the common
   zoomed-in case). The pre-filter currently calls `geom_bbox(&f.geom)` per tile, which walks every
   vertex of every feature *on every tile* — at high zoom few features are in-tile, so it scans all
   3.4M features' vertices to reject them (this is why z12 is 0.46 s vs z10's 0.13 s: z10 hits the
   20k budget and breaks early, z12 scans everything). Add `bbox: [f64;4]` to `Feature`, fill it
   once at construction (in `gpkg.rs::load_range` the vertices are already walked for the range
   extent — near-free), and replace the per-tile `geom_bbox_overlaps` with an inline test against
   `f.bbox`. Filter drops ~0.1-0.4 s → ~10 ms/tile. Cost: ~110 MB RAM (trivial on the 60 GB box).
   Byte-golden safe (identical overlap semantics). Touches `feature.rs`, `geojson.rs`, `gpkg.rs`
   (not a frozen contract).
2. **MVT LRU tile cache** — no MVT caching today (`mvt_tile_handler` calls `render_mvt_tile`
   directly). Mirror the raster `cache.rs`/moka pattern, keyed `(layer, tms, z, x, y)` weighed by
   `.pbf` byte length; wire a lookup before the `spawn_blocking`. Warm tile → 0 ms, 0 projection —
   a large multiplier for viewer pans and QGIS/repeat traffic; hard-bounded memory like raster.
3. **Complete-coverage maps (land cover) MUST NOT be sampled — per-zoom generalization needed.**
   The feature budget was designed for BUPi (a cadastre with real gaps between parcels, where
   dropping some is a tolerable overview). **COS2018 is a wall-to-wall tessellation** (every point is
   classified) — sampling it *punches white holes*, and because each tile has a different native
   feature count, adjacent tiles sample at different rates → **visible tile-seam density mismatch**
   (`screenshots/tile_miss_match.png`). Workaround today: serve with **`--mvt-max-features 0`**
   (unlimited → complete), but then low-zoom tiles are huge (COS z6 = 609k features / 147 MB; crisp
   only from ~z9). The real fix, required for any complete-coverage layer to work at low zoom:
   **simplify vertices (Douglas-Peucker) + DISSOLVE adjacent same-class polygons** so the tile stays
   complete while feature+vertex count drops (a per-zoom generalization, à la tippecanoe — vertex
   simplification alone is insufficient because the 609k *feature* count dominates z6, so dissolve is
   the load-bearing part). Alternative: a raster fallback for the land-cover overview, or a precooked
   pyramid. This is now the top DGT-facing MVT gap.
4. **Low-zoom quality for gappy layers (cadastre) beyond uniform sampling** — the budget samples
   *uniformly* (fixes the holes for BUPi), but a uniform thin still reads as stipple.
   Better overviews (optional, increasing effort): (a) **min-pixel-size cull** — for mixed layers,
   skip non-point features whose *projected* footprint < ~1 px (but note: for a pure cadastre every
   parcel is sub-pixel at the overview, so this alone empties it — pair with sampling); (b) keep the
   **largest-projected-area features** instead of a uniform sample (big parcels read better at
   overview); (c) proper **coalesce/merge** of adjacent parcels (tippecanoe-style) for a solid
   overview mass — the real generalization, more work. Whatever the strategy, keep bounding tile
   size and `log()` nothing silently.
5. **Douglas-Peucker per-zoom simplification** — trim survivor vertex count to ~1 grid unit (the
   4096 grid can't represent sub-pixel detail). Secondary for cadastre (parcels are low-vertex);
   essential for detailed line/polygon layers. Shares kernels with a future offline cooker.

## Open — multi-layer + config (surfaced bringing DGT data into QGIS)

- **Multi-VECTOR-layer YAML config** — `config.rs` `LayerConfig` is raster-only (`cog`/`style`/…, no
  `vector`/`vec_style`/`font`). `--vector` serves exactly one vector layer, so you can't yet publish
  BUPi *and* COS *and* roads from one server. The downstream already supports N layers (MVT is per
  `/mvt/{name}/…`, the X-ray takes `?layer=a,b,c`). Add vector fields to `LayerConfig` + route config
  entries through `build_vector_layer`. **User-requested** (green-lit); pairs with the next item.
- **X-ray layer picker** — the viewer takes `?layer=` from the URL; add a server `/layers` list
  endpoint + a sidebar pick-list of the published layers (user-requested: "cray at a higher level and
  then setting the layers available").
- **`--mvt-style`/legend per layer** — today `state.mvt_style` is server-global (one style for all
  vector layers); when multi-vector-layer lands, move it per-`VectorLayer` (like `fields`).
- **`--public-url` precedence on MVT endpoints** — `advertised_origin` lets the request Host win over
  an explicit `--public-url`; for a TLS-terminating proxy that's wrong (comes out `http://…`). Keep
  `public_url` (as `Option`) on `ServeState` and prefer it when set; optionally honor
  `X-Forwarded-Proto`. (From the 2026-07-13 review.)

## Open — protocol / breadth (from the Task-5 review)

- **WMTS GetCapabilities advertise vector/MVT layers** — a pre-existing `if grids.is_empty()
  { continue }` omits all grid-less vector layers from WMTS caps. Needs a grids representation for
  grid-less vector layers. MVT discovery today is via TileJSON + XYZ (what clients use).
- **RESTful-WMTS-MVT binding** — only the KVP GetTile path is wired for MVT.
- **Portugal TileMatrixSet (EPSG:3763)** — COS/BUPi are native PT-TM06; a national grid preset
  (`tms.rs`) avoids web-mercator reprojection/distortion and serves MVT+WMTS+TMS. See memory
  `project-portugal-tilematrixset`.

## Strategic — precooked archives (PMTiles / tippecanoe)

The minimal fix makes the *live* path usable now with zero new deps — ship it regardless. For big
*static* data (BUPi is a mostly-static cadastre) a precooked archive is the production-grade
follow-up, and the generalization kernels above (min-pixel cull + Douglas-Peucker + budget) are
exactly what a good cooker needs — so this work is a shared prerequisite, not throwaway.

- **Bespoke PMTiles *serving* mode** (recommended, on-identity) — range-read a `.pmtiles` archive
  reusing `s3.rs`'s range reader + a small directory decoder. O(1) range-read serving for static
  data, no external dep. Clean-room win.
- **Cooking** — prefer a **bespoke cooker** driving TerraServe's own `encode_tile` + the
  generalization kernels over the pyramid offline, writing `.pmtiles`. Use external **tippecanoe**
  (a Mapbox C++ CLI — a build-time data-prep tool, so it wouldn't trip the banned-*crate* gate, same
  posture as GDAL-as-external-oracle) only if its low-zoom cartographic quality
  (coalesce-smallest / gamma feature-dropping) outweighs the clean-room principle. Controller
  judgment call; a per-layer config flag can pick live-render vs PMTiles-serve per layer.

Related: the v2 GPKG/FlatGeobuf **windowed-read** (rtree bbox-per-request) solves a *different*
problem — datasets larger than RAM. BUPi fits in RAM (~1.7 GB), so it is NOT needed for BUPi speed;
it is the >100 GB scaling path. See `CLAUDE.md` "Next" + memory `project-next-vectortiles-xray`.
