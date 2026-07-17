# Vector serving path — audit + fix plan (2026-07-13)

Three read-only reviews (Fable-5) of the whole vector serving path, triggered by bringing DGT
polygon data (COS2018 land cover EPSG:3763 / 780k MultiPolygons; BUPi cadastre 3.4M) into QGIS and
repeatedly hitting point/raster-era assumptions. Findings deduplicated + prioritized below. Each
carries `file:line`. **Verdict: the engine core (CRS/axis, WKB, MVT winding, raster fill, parallel
load) is sound; the gaps are (a) point-only assumptions that silently drop polygon/line output, and
(b) OGC vector-serving features never built.**

## Correction to record
**Class-coloured WMS IS possible today, no code change** — `--vec-style file.sld` sniffs SLD
(`style.rs:285-293`) → the rule-based Style IR → `render.rs` draws per-rule polygon fills. The
"single-style" limit is only the *JSON* vec-style front-end. What's missing for COS-over-WMS is the
**asset** (a COS SLD) + **a legend**, not a render feature.

## A. Correctness bugs (silent wrong output — fix first)

1. **Polygon/line labels + markers silently dropped.** `render.rs:346` marker/label pass is
   `Point => p, _ => continue // MVP: points only`. A COS/BUPi SLD `TextSymbolizer` (land-cover
   labels, parcel numbers) or `PointSymbolizer` on polygons renders **nothing**, no warning. Needs a
   polygon interior-point (pole-of-inaccessibility) / line-midpoint anchor. **DGT-visible.**
2. **`clip_line` fragments continuous polylines** on exact float equality (`mvt/clip.rs:85`): an
   unclipped segment returns `a + 1.0*(b-a) != b`, the continuity compare fails, the polyline
   shatters into many pieces → MVT bloat + client cap/join/dash artifacts. Fix: return `b` exactly
   when `t1==1.0` (and `a` when `t0==0.0`). **Roads/boundaries.**
3. **MVT drops the WHOLE feature (and all Multi\* parts) on one unprojectable vertex**
   (`mvt/tile.rs:239,250,261,278` — `?` inside the parts loop). World/polar data loses features.
   `render.rs` drops only the vertex (inconsistent). Interim: `continue` per part; real fix: the
   domain clip.
4. **MultiPoint: GeoJSON layer-fatal, GPKG silent-drop** — no `MultiPoint` variant
   (`feature.rs:73`); `geojson.rs:96` returns `Err` (whole layer fails), `wkb.rs:187` → `Ok(None)`
   (row dropped). Add the variant + handle it.
5. **WMTS-MVT ignores `--mvt-max-features`** (`wmts.rs:313` calls `encode_tile`, the 20k default)
   while `/mvt` honors `state.mvt_max_features` — same z/x/y, different bytes per endpoint. Route
   WMTS through `encode_tile_budgeted`.
6. **GPKG with a non-EPSG SRS silently becomes EPSG:4326** (`gpkg.rs:166` `None` → `lib.rs:249`
   default) — an ESRI-authored export renders in the ocean, no warning. Make it a startup error
   demanding `--src-crs`.
7. **1.3.0 axis-flip hard-coded to literal `EPSG:4326`** (`wms.rs:609`) though `parse_map_frame`
   accepts any layer-native CRS — a landmine for a future lat-first CRS (EPSG:4258/3035, exactly the
   SNIG CRS DGT will ask for). Flip by axis-order lookup, not string equality. (Not triggerable in
   today's 4326/3857/3763/CRS:84 set.)
8. **WMS render vertex-drop shortcut + f32 jitter** (`render.rs:101-151`, `geom.rs:107`): drops an
   unprojectable vertex (straight short-cut, filed) and casts unbounded px coords to f32 (deep-zoom
   jitter on a large COS polygon > 2^24 px). Both fixed by wiring the **existing** `mvt/clip.rs`
   Sutherland-Hodgman clip into `render.rs` in f64 before the f32 cast.
9. **GFI misc**: `FEATURE_COUNT` ignored (max 1, `wms.rs:433`); text/plain+html return only the
   label → **"(unnamed)"** for a no-Text style like COS/BUPi (only JSON is complete); silent empty
   FeatureCollection on a transform failure even on a hit; half-pixel bias + no i/j bounds check;
   line GFI unimplemented (`wms.rs:438`).
10. **Degenerate zero-area rings emitted** after i32 rounding (`mvt/geom.rs:89`) — trivially gated
    by `area != 0`. Scale denom for 4326 grids is equator-based (~1.3× off at PT latitude,
    `render.rs:23`) → SLD scale rules flip at the wrong zoom for 4326 requests.

## B. Missing OGC vector-serving features (DGT gaps)

### P0 — blocks a presentable DGT vector service
- **Vector GetLegendGraphic** — `wms.rs:166` returns a ServiceException for any vector layer;
  `legend.rs` renders only the raster pseudocolor ramp. Build: iterate `vector::style::Style` rules
  → swatch (PolygonSym/LineSym/PointSym) + rule name (carry the SLD rule `name`, currently dropped
  at `sld_lower.rs:122`). Also advertise `<Style><LegendURL>` in caps so it's discoverable. **For a
  land-cover product, no legend is disqualifying.**
- **COS class-coloured WMS asset** — author `fixtures/styles/cos2018.sld` (83
  `PropertyIsEqualTo` rules on `COS18_n4_C`, mechanically generated from the same colours as
  `cos2018.mvt-style.json`; a tiny generator keeps the two in sync). No engine change — routes
  through the existing SLD path.
- **Multi-VECTOR-layer config** — `config.rs LayerConfig` is raster-only; `--vector` is single-layer
  and excludes `--config` (`lib.rs:238`). DGT needs COS + BUPi + orthos on **one** endpoint; today
  that's N processes. This is the root cause of every "layer missing from capabilities" symptom.
  Also make `--mvt-style` per-layer (today one global `ServeState.mvt_style`).
- **GFI attributes in text/plain + html** (not just the label template).
- **WMS perf at national extent** — precompute per-feature bbox at load (see C1); an output/tile
  cache for repeat traffic.

### P1 — good, not just working
- **Complete-coverage generalization** (per-zoom simplify + dissolve same-class) — the real fix for
  both the MVT overview holes/seams AND slow national-extent WMS. Doubles as a precook step.
- **Polygon/line labels** (same as A1).
- **PT-TM06 (EPSG:3763) TileMatrixSet** — no 3763 preset (`tms.rs:218`); config custom grids are
  wired only into the raster path, so even a config 3763 grid can't serve vector tiles
  (`mvt_http.rs:37`/`wmts.rs:300` resolve presets only). Vector layers also publish no raster grids
  (`grids: Vec::new()`, `lib.rs:523`) → no WMTS/TMS PNG of a vector layer in any CRS.
- **WMTS/TMS discoverability** — vector layers skipped from WMTS `<Contents>` (`wmts.rs:513`) yet
  `get_tile_mvt` serves them; advertise the layer + `MVT_FORMAT` + grids, and wire vector GFI into
  WMTS. TMS: fix the misleading 404 for a vector layer.

### P2 — conformance / polish
- **CRS breadth** for the PT SDI (EPSG:4258/25829/25830) — needs A7 (axis flip) + `meters_per_unit`
  (`tms.rs:36`) fixed first, or both go wrong together.
- **WMS caps conformance** — missing mandatory `<Exception>` element (schema-invalid 1.3.0); no
  native-CRS `<BoundingBox>`; CRS:84 accepted but unadvertised; CLI-mode missing `DCPType`;
  `FORMAT`/`STYLES` never validated (silent wrong format/style); `LAYERS=a,b` renders only `a`.
- **dasharray in the `Stroke` model** (`sld/model.rs:139`) — cheap, DGT-visible (road/boundary
  styling; tiny-skia already supports dashing). Plus the rest of `docs/sld-followups.md`.
- **One-style-source pipeline** — SLD ↔ MapLibre lowering so WMS + MVT look identical from one asset.

## C. Perf (fine at 200 features, pathological at 780k/3.4M)

1. **Per-feature bbox recomputed from ALL vertices on EVERY request** — `geom_bbox` (`geom.rs:25`)
   called per feature: once/tile (`tile.rs:99`) and **twice per FTS per GetMap**
   (`render.rs:224,341`). O(all vertices) per request (~10^8 compares for BUPi) — the observed
   0.1-0.5 s/tile and ~0.8 s WMS floor. **Fix: precompute per-feature bbox at load (side array).**
   The single highest-value perf fix; applies to WMS doubly.
2. **Marker/label pass does a full 2nd dataset scan for polygon-only styles** — the bbox walk runs
   before the point check (`render.rs:341` before `:346`). Hoist the `matches!(Point)` test; skip
   the pass when no rule has a Point/Text symbolizer.
3. Per-vertex libproj FFI (batch with `proj_trans_array`, or cache projected coords for a static
   layer×grid); zoom-adaptive simplification/decimation (drop sub-pixel, decimate to grid);
   hot-loop allocs (`select_rules`/`polygons_of`/`value_dedup_key`); GPKG `SELECT *` loads all
   columns + duplicates String keys 3.4M× (column allowlist / interned keys).

## Recommended first batch (DGT-visible + high-value, mostly self-contained)
1. **Vector legend** (GetLegendGraphic from the Style IR) + `<LegendURL>` in caps. [P0, visible]
2. **COS SLD** asset + generator → class-coloured WMS + a real legend for the land-cover map. [P0]
3. **Precompute per-feature bbox at load** — the dominant WMS+MVT per-request cost. [C1]
4. Small correctness bugs shipped together: **line fragmentation** (A2), **MVT per-part drop** (A3),
   **WMTS budget consistency** (A5), **non-EPSG-SRS warning** (A6). [A]
5. **Polygon/line label anchors** (A1) — land-cover labels over WMS. [P1, visible]

Then the bigger pieces: **multi-vector-layer config** (P0, unblocks capabilities), **generalization**
(P1, overview), **PT-TM06 TMS** (P1). The MVT **LRU cache** (previously queued) drops in priority —
these DGT-visible gaps + the precomputed bbox matter more.
