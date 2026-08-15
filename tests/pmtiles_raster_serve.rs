// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Task 5: precomputed **raster** tiles. `serve --raster-pmtiles` read-through on the WMTS and TMS
//! tile paths, plus live fallback on a miss — the raster twin of `tests/pmtiles_serve.rs`, which
//! covers the same shape for MVT.
//!
//! Drives the real serving entry points (`wmts::get_tile`, `tms_http::render_tms_tile`) directly,
//! no HTTP listener, mirroring `tests/wmts_tile.rs` / `tests/tms_http.rs`.
//!
//! **How the read-through is PROVEN rather than assumed.** The archive registered on the served
//! layer holds a SENTINEL image: a valid PNG of a solid, unmistakable colour that the fixture's
//! style can never produce. So `served == sentinel` can only happen if the archive answered, and
//! `served != live` (the same layer with no archive) can only hold if the live path was skipped.
//! Asserting a 200 with PNG magic would pass even if the archive branch were deleted, which is the
//! class of vacuous test this project has twice been burned by.

use std::sync::Arc;

use terraserve::server::{Layer, PublishedGrid, ServeState, VectorLayer};
use terraserve::tms::TileMatrixSet;
use terraserve::vector::geojson::GeoJsonSource;
use terraserve::vector::pmtiles::raster::{build_raster_pmtiles, raster_metadata_json};
use terraserve::vector::pmtiles::read::PmtilesReader;
use terraserve::vector::pmtiles::write::{
    HeaderFields, PmtilesWriter, COMPRESSION_GZIP, COMPRESSION_NONE, TILE_TYPE_MVT, TILE_TYPE_PNG,
};
use terraserve::vector::pmtiles::zxy_to_tileid;
use terraserve::vector::shape::Shaper;
use terraserve::vector::source::{FeatureSource, VectorSource};
use terraserve::vector::style::Style;

const LAYER: &str = "countries";
/// Tile size for the whole test — small, so a bake of z0..=1 is quick.
const TILE_PX: u32 = 256;

fn grid() -> TileMatrixSet {
    // 256 px ⇒ no `_{px}` suffix, so the id is the bare "WebMercatorQuad".
    TileMatrixSet::web_mercator_quad(TILE_PX)
}

/// A vector layer over the worldwide countries fixture (EPSG:4326), publishing the grid above.
/// `raster_pmtiles` starts empty — each test wires up whatever archive it needs.
fn countries_layer() -> Layer {
    let src = Arc::new(GeoJsonSource::load("fixtures/vector/countries.geojson").unwrap());
    let style = Style::load("fixtures/styles/countries.vec.json").unwrap();
    let font = std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap();
    let shaper = Arc::new(Shaper::from_font_bytes(&font).unwrap());
    let ext = src.full_extent();
    Layer {
        name: LAYER.into(),
        cog_path: String::new(),
        cog: None,
        source: None,
        style: None,
        src_crs: "EPSG:4326".into(),
        band_math: None,
        bounds_wgs84: ext,
        tile_cache: None,
        index_cache: terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes()),
        grids: vec![PublishedGrid {
            tms: grid(),
            data_bounds: None,
        }],
        vector: Some(VectorLayer {
            fields: terraserve::mvt_http::feature_field_schema(src.as_ref()),
            area_scale: terraserve::vector::mvt::layer_area_scale(ext, ext),
            min_feature_px: 0.0, // size gate off (the default)
            source: VectorSource::LoadAll(src),
            style,
            shaper,
            lod: None,
        }),
        pmtiles: std::collections::BTreeMap::new(),
        raster_pmtiles: std::collections::BTreeMap::new(),
        overlay: std::collections::BTreeMap::new(),
    }
}

/// A per-test scratch dir (the writer's streamed temp file is keyed on pid alone, so two writers
/// on the shared temp dir in one binary would collide).
fn scratch(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("ts_raster_pmt_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A valid solid-magenta PNG of tile size — the sentinel. Nothing the countries style draws is
/// uniformly magenta, so these bytes cannot be produced by a live render.
fn sentinel_png() -> Vec<u8> {
    let px = TILE_PX as usize;
    let mut rgba = Vec::with_capacity(px * px * 4);
    for _ in 0..(px * px) {
        rgba.extend_from_slice(&[255, 0, 255, 255]);
    }
    terraserve::pngio::encode_rgba(&rgba, TILE_PX, TILE_PX).unwrap()
}

/// A one-tile PNG archive at (z,x,y) carrying `payload` verbatim, self-describing the grid + tile
/// size the way a real `build-pmtiles --tile-format png` archive does.
fn sentinel_archive(
    dir: &std::path::Path,
    z: u32,
    x: u32,
    y: u32,
    payload: &[u8],
) -> PmtilesReader {
    let out = dir.join("sentinel.pmtiles");
    let mut w = PmtilesWriter::new(dir)
        .unwrap()
        .tile_format(TILE_TYPE_PNG, COMPRESSION_NONE);
    w.add(zxy_to_tileid(z, x, y), payload.to_vec()).unwrap();
    w.finish(
        HeaderFields {
            min_zoom: z as u8,
            max_zoom: z as u8,
            bounds_e7: [-1_800_000_000, -850_000_000, 1_800_000_000, 850_000_000],
            center: (z as u8, 0, 0),
        },
        &raster_metadata_json(LAYER, "WebMercatorQuad", TILE_PX),
        &out,
    )
    .unwrap();
    PmtilesReader::open(&out).unwrap()
}

/// WMTS and TMS both serve the archived PNG on a hit, and both fall back to the live renderer on a
/// miss. One test so the two front-ends are proven to agree on the same archive.
#[test]
fn wmts_and_tms_serve_the_archived_png_and_fall_back_live() {
    let dir = scratch("readthrough");
    let sentinel = sentinel_png();
    // z1/0/0 is the archived tile; z1/1/1 is deliberately NOT in the archive (the miss).
    let reader = sentinel_archive(&dir, 1, 0, 0, &sentinel);
    assert_eq!(reader.tile_type(), TILE_TYPE_PNG);

    let mut served = countries_layer();
    served
        .raster_pmtiles
        .insert("WebMercatorQuad".to_string(), Arc::new(reader));
    let st = ServeState::new(vec![served], "http://h/wms".into(), 16);
    // The same layer with NO archive — what a live render actually produces.
    let st_live = ServeState::new(vec![countries_layer()], "http://h/wms".into(), 16);

    // --- HIT, WMTS (top-left row/col addressing, no y-flip).
    let wmts_hit = terraserve::wmts::get_tile(&st, LAYER, "default", "WebMercatorQuad", 1, 0, 0)
        .expect("WMTS GetTile on an archived tile");
    assert_eq!(&wmts_hit[..8], b"\x89PNG\r\n\x1a\n", "must be a PNG");
    assert_eq!(wmts_hit, sentinel, "WMTS must serve the ARCHIVED bytes");

    // --- HIT, TMS. Its y is bottom-left: at z1, matrix_h = 2, so core row 0 is TMS y = 1.
    let tms_hit = terraserve::tms_http::render_tms_tile(&st, "countries@WebMercatorQuad", 1, 0, 1)
        .expect("TMS tile on an archived tile");
    assert_eq!(
        tms_hit, sentinel,
        "TMS must serve the ARCHIVED bytes for the same (z, col, row)"
    );

    // The live render of that same tile must DIFFER, or the two assertions above prove nothing.
    let live_same_tile =
        terraserve::wmts::get_tile(&st_live, LAYER, "default", "WebMercatorQuad", 1, 0, 0).unwrap();
    assert_ne!(
        wmts_hit, live_same_tile,
        "the sentinel must be distinguishable from a live render, else this test is vacuous"
    );

    // --- MISS: a tile the archive does not hold falls through to the live renderer, byte-identical
    // to the no-archive server. A miss must not 404, and must not serve a neighbouring tile.
    let miss = terraserve::wmts::get_tile(&st, LAYER, "default", "WebMercatorQuad", 1, 1, 1)
        .expect("a miss must still render");
    let live_miss =
        terraserve::wmts::get_tile(&st_live, LAYER, "default", "WebMercatorQuad", 1, 1, 1).unwrap();
    assert_eq!(&miss[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(miss, live_miss, "a miss must render live, unchanged");
    assert_ne!(miss, sentinel);

    std::fs::remove_dir_all(&dir).ok();
}

/// The real bake, end to end: `build_raster_pmtiles` produces an archive that DECLARES PNG, holds
/// PNG bytes, and whose tiles are byte-identical to what the live tile path renders — which is the
/// whole claim behind precomputing (one renderer, so a baked tile cannot drift from a live one).
#[test]
fn a_baked_archive_declares_png_and_matches_the_live_render() {
    let dir = scratch("bake");
    let out = dir.join("baked.pmtiles");
    let layer = countries_layer();
    build_raster_pmtiles(&layer, &grid(), 0, 1, layer.bounds_wgs84, &out, &dir).unwrap();

    // The header must say PNG — an archive that claims MVT while holding PNG breaks every other
    // PMTiles reader, so this is read straight off the file rather than trusted.
    let mut head = [0u8; 127];
    {
        use std::io::Read;
        std::fs::File::open(&out)
            .unwrap()
            .read_exact(&mut head)
            .unwrap();
    }
    let header = terraserve::vector::pmtiles::codec::read_header(&head).unwrap();
    assert_eq!(header.tile_type, TILE_TYPE_PNG, "header must declare PNG");
    assert_eq!(header.tile_compression, COMPRESSION_NONE);

    let reader = PmtilesReader::open(&out).unwrap();
    assert_eq!(reader.grid_id(), "WebMercatorQuad");
    assert_eq!(reader.raster_tile_px(), Some(TILE_PX));
    let baked = reader.get(0, 0, 0).unwrap().expect("z0 tile baked");
    assert_eq!(&baked[..8], b"\x89PNG\r\n\x1a\n");

    // Byte-identical to the live path: same renderer, same grid, same tile.
    let st_live = ServeState::new(vec![countries_layer()], "http://h/wms".into(), 16);
    let live =
        terraserve::wmts::get_tile(&st_live, LAYER, "default", "WebMercatorQuad", 0, 0, 0).unwrap();
    assert_eq!(
        baked, live,
        "a baked tile must equal the live render it replaces"
    );

    // And it is not a blank image: a uniform tile is the failure this project keeps hitting.
    let decoded = decode_png_rgba(&baked);
    let distinct: std::collections::HashSet<[u8; 4]> = decoded
        .chunks_exact(4)
        .map(|c| [c[0], c[1], c[2], c[3]])
        .collect();
    assert!(
        distinct.len() > 1,
        "baked z0 tile is a single flat colour ({} distinct) — nothing was drawn",
        distinct.len()
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// An MVT archive registered on the RASTER path is refused, naming the file and both formats —
/// never served as if it were an image. (`build_vector_layer` applies this at startup; the check
/// itself lives on the reader, so it is asserted here on a real archive.)
#[test]
fn an_mvt_archive_is_refused_on_the_raster_path() {
    let dir = scratch("mismatch");
    let out = dir.join("mvt.pmtiles");
    let mut w = PmtilesWriter::new(&dir).unwrap(); // default: MVT + gzip
    w.add(zxy_to_tileid(0, 0, 0), gzip(b"not an image"))
        .unwrap();
    w.finish(
        HeaderFields {
            min_zoom: 0,
            max_zoom: 0,
            bounds_e7: [0, 0, 0, 0],
            center: (0, 0, 0),
        },
        r#"{"vector_layers":[],"grid_id":"WebMercatorQuad"}"#,
        &out,
    )
    .unwrap();

    let reader = PmtilesReader::open(&out).unwrap();
    assert_eq!(reader.tile_type(), TILE_TYPE_MVT);
    let path = out.to_string_lossy().to_string();
    let err = reader
        .require_tile_type(TILE_TYPE_PNG, &path)
        .expect_err("an MVT archive must be refused on the raster path");
    assert!(err.contains(&path));
    assert!(err.contains("MVT") && err.contains("PNG"), "{err}");
    // And the gzip'd MVT archive still reads back through the compression branch, unchanged.
    assert_eq!(header_compression(&out), COMPRESSION_GZIP);
    assert_eq!(reader.get(0, 0, 0).unwrap(), Some(b"not an image".to_vec()));

    std::fs::remove_dir_all(&dir).ok();
}

fn header_compression(path: &std::path::Path) -> u8 {
    use std::io::Read;
    let mut head = [0u8; 127];
    std::fs::File::open(path)
        .unwrap()
        .read_exact(&mut head)
        .unwrap();
    terraserve::vector::pmtiles::codec::read_header(&head)
        .unwrap()
        .tile_compression
}

/// gzip, as the MVT writer path stores tiles (the codec helper is crate-private, so this test
/// reaches for the same `flate2` it wraps).
fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

/// Minimal PNG decode to RGBA, for the "not blank" assertion.
fn decode_png_rgba(bytes: &[u8]) -> Vec<u8> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).unwrap();
    buf.truncate(info.buffer_size());
    buf
}
