//! Task 4: the tile assembler (`encode_tile`) end-to-end, via a bespoke test-only MVT *decoder*
//! (varint reader -> Tile{layers} -> Layer{name,extent,keys,values,features} -> Feature{type,tags,
//! geometry} -> the geometry command stream back to absolute i32 coords). This decoder exists ONLY
//! here — no runtime MVT-decode crate/dependency is added anywhere in `src/`.
//!
//! The committed golden (`fixtures/goldens/mini_wmq_z0.pbf`) was additionally validated OFFLINE
//! against a real, independent MVT parser:
//!
//!     pip install mapbox_vector_tile
//!     python3 -c "
//!     import mapbox_vector_tile
//!     data = open('fixtures/goldens/mini_wmq_z0.pbf', 'rb').read()
//!     print(mapbox_vector_tile.decode(data))
//!     "
//!
//! See task-4-report.md for the captured output (3 features: Point/LineString/Polygon-with-hole,
//! attrs round-tripped, `mini` layer, extent 4096).

use terraserve::tms;
use terraserve::vector::geojson::GeoJsonSource;
use terraserve::vector::mvt::{encode_tile, encode_tile_opt, MvtOptimizations};
use terraserve::vector::source::FeatureSource;

/// A minimal, bespoke, READ-ONLY MVT decoder — test-only, not part of the crate.
mod testdec {
    use std::collections::HashMap;

    pub struct DTile {
        pub layers: Vec<DLayer>,
    }
    pub struct DLayer {
        pub name: String,
        pub extent: u32,
        pub features: Vec<DFeature>,
    }
    pub struct DFeature {
        pub geom_type: u32,
        pub props: HashMap<String, DValue>,
        /// Decoded geometry: one `Vec<[i32;2]>` per MoveTo-started part/ring (points: one part
        /// holding all the disconnected points; lines: one part per LineString piece; polygons:
        /// one ring per part, explicitly closed back to the first vertex).
        pub rings: Vec<Vec<[i32; 2]>>,
    }
    #[derive(Debug, Clone, PartialEq)]
    pub enum DValue {
        Str(String),
        Num(f64),
    }
    impl DValue {
        pub fn as_str(&self) -> Option<&str> {
            match self {
                DValue::Str(s) => Some(s),
                _ => None,
            }
        }
    }

    struct Reader<'a> {
        buf: &'a [u8],
        pos: usize,
    }
    impl<'a> Reader<'a> {
        fn new(buf: &'a [u8]) -> Self {
            Reader { buf, pos: 0 }
        }
        fn eof(&self) -> bool {
            self.pos >= self.buf.len()
        }
        fn varint(&mut self) -> u64 {
            let mut result = 0u64;
            let mut shift = 0;
            loop {
                let b = self.buf[self.pos];
                self.pos += 1;
                result |= ((b & 0x7f) as u64) << shift;
                if b & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            result
        }
        fn tag(&mut self) -> (u32, u32) {
            let t = self.varint();
            ((t >> 3) as u32, (t & 0x7) as u32)
        }
        fn skip(&mut self, wire: u32) {
            match wire {
                0 => {
                    self.varint();
                }
                1 => self.pos += 8,
                5 => self.pos += 4,
                2 => {
                    let len = self.varint() as usize;
                    self.pos += len;
                }
                _ => panic!("bad wire type {wire}"),
            }
        }
        fn bytes_field(&mut self) -> &'a [u8] {
            let len = self.varint() as usize;
            let s = &self.buf[self.pos..self.pos + len];
            self.pos += len;
            s
        }
        fn double(&mut self) -> f64 {
            let b: [u8; 8] = self.buf[self.pos..self.pos + 8].try_into().unwrap();
            self.pos += 8;
            f64::from_le_bytes(b)
        }
        fn packed_u32(&mut self) -> Vec<u32> {
            let bytes = self.bytes_field();
            let mut r = Reader::new(bytes);
            let mut out = Vec::new();
            while !r.eof() {
                out.push(r.varint() as u32);
            }
            out
        }
    }

    pub fn decode(buf: &[u8]) -> DTile {
        let mut r = Reader::new(buf);
        let mut layers = Vec::new();
        while !r.eof() {
            let (field, wire) = r.tag();
            if field == 3 && wire == 2 {
                let bytes = r.bytes_field();
                layers.push(decode_layer(bytes));
            } else {
                r.skip(wire);
            }
        }
        DTile { layers }
    }

    fn decode_layer(buf: &[u8]) -> DLayer {
        let mut r = Reader::new(buf);
        let mut name = String::new();
        let mut extent = 0u32;
        let mut keys: Vec<String> = Vec::new();
        let mut values: Vec<DValue> = Vec::new();
        let mut feature_bufs: Vec<Vec<u8>> = Vec::new();
        while !r.eof() {
            let (field, wire) = r.tag();
            match field {
                1 => name = String::from_utf8(r.bytes_field().to_vec()).unwrap(),
                2 => feature_bufs.push(r.bytes_field().to_vec()),
                3 => keys.push(String::from_utf8(r.bytes_field().to_vec()).unwrap()),
                4 => values.push(decode_value(r.bytes_field())),
                5 => extent = r.varint() as u32,
                _ => r.skip(wire),
            }
        }
        let features = feature_bufs
            .iter()
            .map(|b| decode_feature(b, &keys, &values))
            .collect();
        DLayer {
            name,
            extent,
            features,
        }
    }

    fn decode_value(buf: &[u8]) -> DValue {
        let mut r = Reader::new(buf);
        let mut out = DValue::Num(0.0);
        while !r.eof() {
            let (field, wire) = r.tag();
            match field {
                1 => out = DValue::Str(String::from_utf8(r.bytes_field().to_vec()).unwrap()),
                3 => out = DValue::Num(r.double()),
                _ => r.skip(wire),
            }
        }
        out
    }

    fn decode_feature(buf: &[u8], keys: &[String], values: &[DValue]) -> DFeature {
        let mut r = Reader::new(buf);
        let mut geom_type = 0u32;
        let mut tags: Vec<u32> = Vec::new();
        let mut geometry: Vec<u32> = Vec::new();
        while !r.eof() {
            let (field, wire) = r.tag();
            match field {
                1 => {
                    r.varint();
                } // id — not needed by these tests
                2 => tags = r.packed_u32(),
                3 => geom_type = r.varint() as u32,
                4 => geometry = r.packed_u32(),
                _ => r.skip(wire),
            }
        }
        let mut props = HashMap::new();
        let mut i = 0;
        while i + 1 < tags.len() {
            let k = keys[tags[i] as usize].clone();
            let v = values[tags[i + 1] as usize].clone();
            props.insert(k, v);
            i += 2;
        }
        DFeature {
            geom_type,
            props,
            rings: decode_geometry(&geometry),
        }
    }

    fn zigzag_decode(z: u32) -> i32 {
        ((z >> 1) as i32) ^ -((z & 1) as i32)
    }

    fn decode_geometry(cmds: &[u32]) -> Vec<Vec<[i32; 2]>> {
        let mut rings: Vec<Vec<[i32; 2]>> = Vec::new();
        let mut cur: Vec<[i32; 2]> = Vec::new();
        let (mut x, mut y) = (0i32, 0i32);
        let mut i = 0;
        while i < cmds.len() {
            let cmd_int = cmds[i];
            i += 1;
            let id = cmd_int & 0x7;
            let count = cmd_int >> 3;
            match id {
                1 => {
                    // MoveTo: starts a new part — flush whatever was accumulating.
                    if !cur.is_empty() {
                        rings.push(std::mem::take(&mut cur));
                    }
                    for _ in 0..count {
                        let dx = zigzag_decode(cmds[i]);
                        i += 1;
                        let dy = zigzag_decode(cmds[i]);
                        i += 1;
                        x += dx;
                        y += dy;
                        cur.push([x, y]);
                    }
                }
                2 => {
                    for _ in 0..count {
                        let dx = zigzag_decode(cmds[i]);
                        i += 1;
                        let dy = zigzag_decode(cmds[i]);
                        i += 1;
                        x += dx;
                        y += dy;
                        cur.push([x, y]);
                    }
                }
                7 => {
                    if let Some(&first) = cur.first() {
                        cur.push(first);
                    }
                }
                other => panic!("bad geometry command id {other}"),
            }
        }
        if !cur.is_empty() {
            rings.push(cur);
        }
        rings
    }
}

#[test]
fn encodes_a_polygon_feature_into_a_decodable_tile() {
    // one small polygon near (0,0) that falls inside the z0 WebMercatorQuad tile.
    let gj = r#"{"type":"FeatureCollection","features":[
      {"type":"Feature","properties":{"name":"a","rank":2},
       "geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]}}]}"#;
    let src = GeoJsonSource::from_str(gj).unwrap();
    let wmq = tms::preset("WebMercatorQuad", 4096).unwrap();
    let bytes = encode_tile(&src, &wmq, 0, 0, 0, "EPSG:4326", "test");
    assert!(!bytes.is_empty());
    let tile = testdec::decode(&bytes);
    assert_eq!(tile.layers.len(), 1);
    let layer = &tile.layers[0];
    assert_eq!(layer.name, "test");
    assert_eq!(layer.extent, 4096);
    assert_eq!(layer.features.len(), 1);
    let f = &layer.features[0];
    assert_eq!(f.geom_type, 3, "polygon");
    // attributes round-trip
    assert_eq!(f.props.get("name").map(|v| v.as_str()), Some(Some("a")));
    // geometry decodes to a closed ring inside the buffered tile rect.
    assert!(f.rings[0]
        .iter()
        .all(|p| p[0] >= -256 && p[0] <= 4096 + 256 && p[1] >= -256 && p[1] <= 4096 + 256));
}

#[test]
fn worldcrs84quad_tile_differs_from_webmercator() {
    let gj = r#"{"type":"FeatureCollection","features":[
      {"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[10,50]}}]}"#;
    let src = GeoJsonSource::from_str(gj).unwrap();
    let wmq = tms::preset("WebMercatorQuad", 4096).unwrap();
    let wcq = tms::preset("WorldCRS84Quad", 4096).unwrap();

    let z = 2;

    // WorldCRS84Quad's grid CRS is EPSG:4326 (identity) — compute the covering col/row directly.
    let lvl_wcq = wcq.level(z).unwrap();
    let span_x_wcq = wcq.tile_w as f64 * lvl_wcq.resolution;
    let span_y_wcq = wcq.tile_h as f64 * lvl_wcq.resolution;
    let col_wcq = ((10.0 - wcq.origin_x) / span_x_wcq).floor() as u32;
    let row_wcq = ((wcq.origin_y - 50.0) / span_y_wcq).floor() as u32;

    // WebMercatorQuad's grid CRS is EPSG:3857 — project (10,50) via the engine's reproj transformer.
    let t = terraserve::reproj::Transformer::new("EPSG:4326", "EPSG:3857").unwrap();
    let (mx, my) = t.to_source(10.0, 50.0).unwrap();
    let lvl_wmq = wmq.level(z).unwrap();
    let span_x_wmq = wmq.tile_w as f64 * lvl_wmq.resolution;
    let span_y_wmq = wmq.tile_h as f64 * lvl_wmq.resolution;
    let col_wmq = ((mx - wmq.origin_x) / span_x_wmq).floor() as u32;
    let row_wmq = ((wmq.origin_y - my) / span_y_wmq).floor() as u32;

    let bytes_wmq = encode_tile(&src, &wmq, z, col_wmq, row_wmq, "EPSG:4326", "test");
    let bytes_wcq = encode_tile(&src, &wcq, z, col_wcq, row_wcq, "EPSG:4326", "test");
    assert!(!bytes_wmq.is_empty());
    assert!(!bytes_wcq.is_empty());

    let tile_wmq = testdec::decode(&bytes_wmq);
    let tile_wcq = testdec::decode(&bytes_wcq);
    assert_eq!(tile_wmq.layers[0].features.len(), 1);
    assert_eq!(tile_wcq.layers[0].features.len(), 1);
    let p_wmq = tile_wmq.layers[0].features[0].rings[0][0];
    let p_wcq = tile_wcq.layers[0].features[0].rings[0][0];
    assert_ne!(
        p_wmq, p_wcq,
        "different grid math (3857 vs 4326) must yield different tile-local coords"
    );
}

/// Shoelace area of a decoded ring (explicitly closed by the test decoder). Sign = winding.
fn ring_area(ring: &[[i32; 2]]) -> i64 {
    let mut a = 0i64;
    for w in ring.windows(2) {
        a += (w[0][0] as i64) * (w[1][1] as i64) - (w[1][0] as i64) * (w[0][1] as i64);
    }
    a
}

#[test]
fn bbox_prefilter_keeps_in_tile_feature_and_survivor_pool_excludes_dropped() {
    // One point inside a z2 tile over central Europe; two far away (other hemisphere) with UNIQUE
    // attribute strings. Only the in-tile feature must be emitted, and — the 150 MB fix — the
    // dropped features' attribute values must NOT appear in the (survivor-scoped) value pool.
    let gj = r#"{"type":"FeatureCollection","features":[
      {"type":"Feature","properties":{"tag":"KEEP_ME"},"geometry":{"type":"Point","coordinates":[10,50]}},
      {"type":"Feature","properties":{"tag":"DROP_FAR_A"},"geometry":{"type":"Point","coordinates":[-120,-40]}},
      {"type":"Feature","properties":{"tag":"DROP_FAR_B"},"geometry":{"type":"Point","coordinates":[150,-60]}}
    ]}"#;
    let src = GeoJsonSource::from_str(gj).unwrap();
    let wcq = tms::preset("WorldCRS84Quad", 4096).unwrap();
    // WorldCRS84Quad's grid CRS is EPSG:4326 (identity) — compute the covering col/row directly.
    let lvl = wcq.level(2).unwrap();
    let span_x = wcq.tile_w as f64 * lvl.resolution;
    let span_y = wcq.tile_h as f64 * lvl.resolution;
    let col = ((10.0 - wcq.origin_x) / span_x).floor() as u32;
    let row = ((wcq.origin_y - 50.0) / span_y).floor() as u32;
    let bytes = encode_tile(&src, &wcq, 2, col, row, "EPSG:4326", "test");
    assert!(!bytes.is_empty());
    let tile = testdec::decode(&bytes);
    assert_eq!(
        tile.layers[0].features.len(),
        1,
        "only the in-tile feature survives"
    );
    assert_eq!(
        tile.layers[0].features[0]
            .props
            .get("tag")
            .and_then(|v| v.as_str()),
        Some("KEEP_ME")
    );
    let hay = String::from_utf8_lossy(&bytes);
    assert!(
        !hay.contains("DROP_FAR_A") && !hay.contains("DROP_FAR_B"),
        "dropped features' attribute values leaked into the value pool (pool not survivor-scoped)"
    );
}

#[test]
fn multipolygon_second_exterior_is_not_wound_as_a_hole() {
    // Two disjoint squares as ONE MultiPolygon — each is an exterior, neither a hole. The old
    // flat-list encoder treated only ring 0 as exterior, inverting the 2nd square's winding like a
    // hole. `encode_multipolygon` restarts the winding per polygon: both must wind the SAME way.
    let gj = r#"{"type":"FeatureCollection","features":[
      {"type":"Feature","properties":{},"geometry":{"type":"MultiPolygon","coordinates":[
        [[[0,0],[1,0],[1,1],[0,1],[0,0]]],
        [[[3,0],[4,0],[4,1],[3,1],[3,0]]]
      ]}}]}"#;
    let src = GeoJsonSource::from_str(gj).unwrap();
    let wmq = tms::preset("WebMercatorQuad", 4096).unwrap();
    let bytes = encode_tile(&src, &wmq, 0, 0, 0, "EPSG:4326", "test");
    let tile = testdec::decode(&bytes);
    assert_eq!(tile.layers[0].features.len(), 1);
    let f = &tile.layers[0].features[0];
    assert_eq!(f.geom_type, 3, "polygon");
    assert_eq!(f.rings.len(), 2, "two exterior rings");
    let a0 = ring_area(&f.rings[0]);
    let a1 = ring_area(&f.rings[1]);
    assert!(a0 != 0 && a1 != 0);
    assert_eq!(
        a0 > 0,
        a1 > 0,
        "both MultiPolygon exteriors must wind the same way (neither is a hole): {a0} vs {a1}"
    );
}

/// Regenerate the committed golden. Run intentionally: `cargo test --test mvt_tile gen_golden --
/// --ignored`, then validate offline (see the module doc comment above), then commit the `.pbf`.
#[test]
#[ignore]
fn gen_golden() {
    std::fs::create_dir_all("fixtures/goldens").unwrap();
    let src = GeoJsonSource::load("fixtures/vector/mini_mvt.geojson").unwrap();
    let wmq = tms::preset("WebMercatorQuad", 4096).unwrap();
    let bytes = encode_tile(&src, &wmq, 0, 0, 0, "EPSG:4326", "mini");
    std::fs::write("fixtures/goldens/mini_wmq_z0.pbf", bytes).unwrap();
}

#[test]
fn golden_tile_is_byte_stable() {
    let src = GeoJsonSource::load("fixtures/vector/mini_mvt.geojson").unwrap();
    let wmq = tms::preset("WebMercatorQuad", 4096).unwrap();
    let bytes = encode_tile(&src, &wmq, 0, 0, 0, "EPSG:4326", "mini");
    let golden = std::fs::read("fixtures/goldens/mini_wmq_z0.pbf").expect("golden — gen once");
    assert_eq!(
        bytes, golden,
        "MVT output drifted from the offline-validated golden"
    );

    // Structural check against the same bytes, via the test-only decoder (belt + suspenders on
    // top of the byte-for-byte compare — this is the part a real external parser also confirmed).
    let tile = testdec::decode(&golden);
    assert_eq!(tile.layers.len(), 1);
    let layer = &tile.layers[0];
    assert_eq!(layer.name, "mini");
    assert_eq!(layer.extent, 4096);
    assert_eq!(layer.features.len(), 3, "point + line + polygon-with-hole");

    let point = layer
        .features
        .iter()
        .find(|f| f.geom_type == 1)
        .expect("point feature");
    assert_eq!(point.props.get("name").and_then(|v| v.as_str()), Some("pt"));

    let line = layer
        .features
        .iter()
        .find(|f| f.geom_type == 2)
        .expect("line feature");
    assert_eq!(line.props.get("name").and_then(|v| v.as_str()), Some("ln"));

    let poly = layer
        .features
        .iter()
        .find(|f| f.geom_type == 3)
        .expect("polygon feature");
    assert_eq!(
        poly.props.get("name").and_then(|v| v.as_str()),
        Some("poly")
    );
    // The hole is what forces `encode_polygon`'s winding-reversal branch: 2 rings, opposite
    // winding (shoelace sign) between the exterior and the interior ring.
    assert_eq!(poly.rings.len(), 2, "exterior + one hole ring");
    let area = |ring: &[[i32; 2]]| -> i64 {
        let mut a = 0i64;
        for w in ring.windows(2) {
            a += (w[0][0] as i64) * (w[1][1] as i64) - (w[1][0] as i64) * (w[0][1] as i64);
        }
        a
    };
    let a0 = area(&poly.rings[0]);
    let a1 = area(&poly.rings[1]);
    assert!(a0 != 0 && a1 != 0);
    assert!(
        (a0 > 0) != (a1 > 0),
        "exterior and hole must wind oppositely: {a0} vs {a1}"
    );
}

// ---- Stage B: the `--mvt-cell-px` dominant-class cell mosaic (Task B6) ----

fn mosaic_opts(field: &str, cell_units: u32, cell_max_zoom: u32) -> MvtOptimizations {
    MvtOptimizations {
        cell_units,
        cell_field: Some(field.to_string()),
        cell_max_zoom,
        ..MvtOptimizations::defaults()
    }
}

#[test]
fn mosaic_replaces_polygon_with_class_rects() {
    // A near-world polygon of class "A" at z0: the mosaic fills its cells and run-length-merges each
    // row → MANY class-A rectangles REPLACE the single input polygon (Stage B ran, no holes in the
    // covered rows, and the class tag rides along).
    let gj = r#"{"type":"FeatureCollection","features":[
      {"type":"Feature","properties":{"cls":"A"},
       "geometry":{"type":"Polygon","coordinates":[[[-170,-80],[170,-80],[170,80],[-170,80],[-170,-80]]]}}]}"#;
    let src = GeoJsonSource::from_str(gj).unwrap();
    let wmq = tms::preset("WebMercatorQuad", 4096).unwrap();
    let bytes = encode_tile_opt(
        src.features(),
        &wmq,
        0,
        0,
        0,
        "EPSG:4326",
        "t",
        &mosaic_opts("cls", 128, 0),
    );
    assert!(!bytes.is_empty());
    let tile = testdec::decode(&bytes);
    let layer = &tile.layers[0];
    assert!(
        layer.features.len() > 1,
        "one input polygon → many merged rects, got {}",
        layer.features.len()
    );
    for f in &layer.features {
        assert_eq!(f.geom_type, 3, "mosaic emits polygon rects");
        assert_eq!(
            f.props.get("cls").and_then(|v| v.as_str()),
            Some("A"),
            "each rect carries the class tag"
        );
    }
}

#[test]
fn mosaic_is_seam_free_across_adjacent_tiles() {
    // A polygon straddling lon=0 (the z1 x=0 | x=1 tile edge), northern hemisphere (row 0): both
    // adjacent tiles mosaic it to class "A" — same class both sides, no seam.
    let gj = r#"{"type":"FeatureCollection","features":[
      {"type":"Feature","properties":{"cls":"A"},
       "geometry":{"type":"Polygon","coordinates":[[[-20,20],[20,20],[20,60],[-20,60],[-20,20]]]}}]}"#;
    let src = GeoJsonSource::from_str(gj).unwrap();
    let wmq = tms::preset("WebMercatorQuad", 4096).unwrap();
    let opts = mosaic_opts("cls", 128, 0);
    let left = encode_tile_opt(src.features(), &wmq, 1, 0, 0, "EPSG:4326", "t", &opts);
    let right = encode_tile_opt(src.features(), &wmq, 1, 1, 0, "EPSG:4326", "t", &opts);
    assert!(
        !left.is_empty() && !right.is_empty(),
        "both adjacent tiles mosaic the shared polygon"
    );
    for bytes in [left, right] {
        let tile = testdec::decode(&bytes);
        for f in &tile.layers[0].features {
            assert_eq!(
                f.props.get("cls").and_then(|v| v.as_str()),
                Some("A"),
                "both sides resolve to class A (seam-free)"
            );
        }
    }
}

#[test]
fn mosaic_is_polygons_only_points_survive() {
    // Mixed geometry: the mosaic replaces the class-"A" polygon, but the Point passes through
    // untouched (Fable-5 finding 5 — replace is polygons-only).
    let gj = r#"{"type":"FeatureCollection","features":[
      {"type":"Feature","properties":{"cls":"A"},
       "geometry":{"type":"Polygon","coordinates":[[[-170,-80],[170,-80],[170,80],[-170,80],[-170,-80]]]}},
      {"type":"Feature","properties":{"name":"pt"},
       "geometry":{"type":"Point","coordinates":[10,10]}}]}"#;
    let src = GeoJsonSource::from_str(gj).unwrap();
    let wmq = tms::preset("WebMercatorQuad", 4096).unwrap();
    let bytes = encode_tile_opt(
        src.features(),
        &wmq,
        0,
        0,
        0,
        "EPSG:4326",
        "t",
        &mosaic_opts("cls", 128, 0),
    );
    let tile = testdec::decode(&bytes);
    let feats = &tile.layers[0].features;
    assert!(
        feats.iter().any(|f| f.geom_type == 1),
        "the Point survived the polygon mosaic"
    );
    assert!(
        feats.iter().any(|f| f.geom_type == 3),
        "mosaic rects are present too"
    );
}

#[test]
fn mosaic_votes_on_raw_set_bypassing_size_gate_and_budget() {
    // A big class-"A" polygon + a smaller class-"B" polygon inside it (painter's order → B's cells
    // win where they overlap). With the budget forced to 1 AND a huge min-feature-px, a
    // budget-sampled or size-gated vote would drop B (and maybe A) — so asserting BOTH classes
    // survive proves the mosaic votes on the RAW candidate set (review finding 3 / Fable-5 finding 2).
    let gj = r#"{"type":"FeatureCollection","features":[
      {"type":"Feature","properties":{"cls":"A"},
       "geometry":{"type":"Polygon","coordinates":[[[5,5],[85,5],[85,80],[5,80],[5,5]]]}},
      {"type":"Feature","properties":{"cls":"B"},
       "geometry":{"type":"Polygon","coordinates":[[[20,20],[35,20],[35,35],[20,35],[20,20]]]}}]}"#;
    let src = GeoJsonSource::from_str(gj).unwrap();
    let wmq = tms::preset("WebMercatorQuad", 4096).unwrap();
    let opts = MvtOptimizations {
        cell_units: 128,
        cell_field: Some("cls".to_string()),
        cell_max_zoom: 0,
        max_features: 1,     // a budget of 1 — the mosaic must ignore it
        min_feature_px: 1e6, // a size gate that would drop these polygons if it applied
        area_scale: 1.0,
        ..MvtOptimizations::defaults()
    };
    let bytes = encode_tile_opt(src.features(), &wmq, 1, 1, 0, "EPSG:4326", "t", &opts);
    let tile = testdec::decode(&bytes);
    let classes: std::collections::HashSet<&str> = tile.layers[0]
        .features
        .iter()
        .filter_map(|f| f.props.get("cls").and_then(|v| v.as_str()))
        .collect();
    assert!(
        classes.contains("A"),
        "class A survives (raw vote, not budget/size-gated)"
    );
    assert!(
        classes.contains("B"),
        "class B survives — proves the mosaic bypasses the budget AND the size gate"
    );
}

// ---- Stage B: --mvt-dissolve same-class dissolve (Task 6) ----

fn dissolve_opts(field: &str) -> MvtOptimizations {
    MvtOptimizations {
        dissolve_field: Some(field.to_string()),
        ..MvtOptimizations::defaults()
    }
}

#[test]
fn dissolve_merges_adjacent_same_class() {
    // Two adjacent class-"A" squares sharing the lon=10 edge → dissolve to ONE polygon feature (the
    // internal border cancels), tagged with the class.
    let gj = r#"{"type":"FeatureCollection","features":[
      {"type":"Feature","properties":{"cls":"A"},"geometry":{"type":"Polygon","coordinates":[[[0,0],[10,0],[10,10],[0,10],[0,0]]]}},
      {"type":"Feature","properties":{"cls":"A"},"geometry":{"type":"Polygon","coordinates":[[[10,0],[20,0],[20,10],[10,10],[10,0]]]}}]}"#;
    let src = GeoJsonSource::from_str(gj).unwrap();
    let wmq = tms::preset("WebMercatorQuad", 4096).unwrap();
    let bytes = encode_tile_opt(
        src.features(),
        &wmq,
        0,
        0,
        0,
        "EPSG:4326",
        "t",
        &dissolve_opts("cls"),
    );
    let tile = testdec::decode(&bytes);
    let feats = &tile.layers[0].features;
    assert_eq!(feats.len(), 1, "merged to ONE feature, got {}", feats.len());
    assert_eq!(feats[0].geom_type, 3, "polygon");
    assert_eq!(
        feats[0].props.get("cls").and_then(|v| v.as_str()),
        Some("A")
    );
}

#[test]
fn dissolve_is_polygons_only() {
    let gj = r#"{"type":"FeatureCollection","features":[
      {"type":"Feature","properties":{"cls":"A"},"geometry":{"type":"Polygon","coordinates":[[[0,0],[20,0],[20,20],[0,20],[0,0]]]}},
      {"type":"Feature","properties":{"name":"pt"},"geometry":{"type":"Point","coordinates":[5,5]}}]}"#;
    let src = GeoJsonSource::from_str(gj).unwrap();
    let wmq = tms::preset("WebMercatorQuad", 4096).unwrap();
    let bytes = encode_tile_opt(
        src.features(),
        &wmq,
        0,
        0,
        0,
        "EPSG:4326",
        "t",
        &dissolve_opts("cls"),
    );
    let feats = &testdec::decode(&bytes).layers[0].features;
    assert!(
        feats.iter().any(|f| f.geom_type == 1),
        "point survived the dissolve"
    );
    assert!(
        feats.iter().any(|f| f.geom_type == 3),
        "dissolved polygon present"
    );
}

#[test]
fn dissolve_seam_free_across_adjacent_tiles() {
    // A class-"A" region straddling the z1 x=0|x=1 edge → both tiles emit A (continuous boundary).
    let gj = r#"{"type":"FeatureCollection","features":[
      {"type":"Feature","properties":{"cls":"A"},"geometry":{"type":"Polygon","coordinates":[[[-20,20],[20,20],[20,60],[-20,60],[-20,20]]]}}]}"#;
    let src = GeoJsonSource::from_str(gj).unwrap();
    let wmq = tms::preset("WebMercatorQuad", 4096).unwrap();
    let opts = dissolve_opts("cls");
    let left = encode_tile_opt(src.features(), &wmq, 1, 0, 0, "EPSG:4326", "t", &opts);
    let right = encode_tile_opt(src.features(), &wmq, 1, 1, 0, "EPSG:4326", "t", &opts);
    assert!(
        !left.is_empty() && !right.is_empty(),
        "both adjacent tiles emit the dissolved class"
    );
    for bytes in [left, right] {
        for f in &testdec::decode(&bytes).layers[0].features {
            assert_eq!(f.props.get("cls").and_then(|v| v.as_str()), Some("A"));
        }
    }
}

#[test]
fn dissolve_no_optimizations_invariance_and_deterministic() {
    // dedup on vs off → identical bytes (Fable-5 F10 — dissolve is dedup-invariant); and encoding the
    // same tile twice is byte-identical (F4 determinism).
    let gj = r#"{"type":"FeatureCollection","features":[
      {"type":"Feature","properties":{"cls":"A"},"geometry":{"type":"Polygon","coordinates":[[[0,0],[10,0],[10,10],[0,10],[0,0]]]}},
      {"type":"Feature","properties":{"cls":"A"},"geometry":{"type":"Polygon","coordinates":[[[10,0],[20,0],[20,10],[10,10],[10,0]]]}}]}"#;
    let src = GeoJsonSource::from_str(gj).unwrap();
    let wmq = tms::preset("WebMercatorQuad", 4096).unwrap();
    let enc = |dedup: bool| {
        let opts = MvtOptimizations {
            dissolve_field: Some("cls".to_string()),
            dedup,
            ..MvtOptimizations::defaults()
        };
        encode_tile_opt(src.features(), &wmq, 0, 0, 0, "EPSG:4326", "t", &opts)
    };
    assert_eq!(
        enc(true),
        enc(false),
        "dissolve is --no-optimizations invariant (F10)"
    );
    assert_eq!(
        enc(true),
        enc(true),
        "dissolve output is deterministic (F4)"
    );
}
