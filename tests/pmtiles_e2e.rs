//! End-to-end: generate a tiny `.pmtiles` from a fixture, then read every tile back and assert it is
//! byte-identical to a live `encode_tile_opt` render — and that tiles the generator omitted (empty
//! MVT) are genuinely absent from the archive. The Layer + `MvtOptimizations` are built INLINE the
//! same way `tests/mvt_http.rs`'s `vector_layer()`/`state()` do, so the pyramid uses the exact
//! (Layer, opts) pair the live serve routes use.

use std::sync::Arc;

use terraserve::server::{Layer, ServeState, VectorLayer};
use terraserve::vector::geojson::GeoJsonSource;
use terraserve::vector::mvt::opts::MvtOptimizations;
use terraserve::vector::mvt::tile::{encode_tile_opt, features_for_tile};
use terraserve::vector::pmtiles::generate::build_pmtiles;
use terraserve::vector::pmtiles::read::PmtilesReader;
use terraserve::vector::shape::Shaper;
use terraserve::vector::source::{FeatureSource, VectorSource};
use terraserve::vector::style::Style;

const LAYER: &str = "countries";

/// Build a Layer from the worldwide countries polygon fixture (EPSG:4326), mirroring
/// `tests/mvt_http.rs`'s `vector_layer()`.
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
        grids: Vec::new(),
        vector: Some(VectorLayer {
            fields: terraserve::mvt_http::feature_field_schema(src.as_ref()),
            area_scale: terraserve::vector::mvt::layer_area_scale(ext, ext),
            source: VectorSource::LoadAll(src),
            style,
            shaper,
            lod: None,
        }),
        pmtiles: None,
        overlay: None,
    }
}

#[test]
fn generated_pmtiles_reads_back_identical_tiles() {
    let layer = countries_layer();
    let v = layer.vector.as_ref().unwrap();
    let grid = terraserve::tms::preset("WebMercatorQuad", 4096).unwrap();
    // The SAME optimization set the /mvt + WMTS routes build for this layer (default flags).
    let state = ServeState::new(vec![], "http://h/wms".into(), 16);
    let opts = MvtOptimizations::for_layer(&state, v);

    let out = std::env::temp_dir().join(format!("ts_e2e_{}.pmtiles", std::process::id()));
    let counts = build_pmtiles(
        &layer,
        &opts,
        &grid,
        0,
        4,
        layer.bounds_wgs84,
        &out,
        &std::env::temp_dir(),
    )
    .expect("build_pmtiles");
    assert!(counts.addressed > 0, "some tiles were generated");

    let r = PmtilesReader::open(&out).expect("open pmtiles");
    let bbox3857 = terraserve::reproj::crs_bounds(
        "EPSG:4326",
        "EPSG:3857",
        layer.bounds_wgs84[0],
        layer.bounds_wgs84[1],
        layer.bounds_wgs84[2],
        layer.bounds_wgs84[3],
    )
    .expect("reproject bounds to 3857");

    for z in 0..=4u32 {
        if let Some((c0, c1, r0, r1)) = grid.tile_limits(bbox3857, z) {
            let vs = v.source_for_zoom(z);
            for x in c0..=c1 {
                for y in r0..=r1 {
                    let feats = features_for_tile(&vs, &grid, z, x, y, &layer.src_crs);
                    let live = encode_tile_opt(
                        feats.as_slice(),
                        &grid,
                        z,
                        x,
                        y,
                        &layer.src_crs,
                        LAYER,
                        &opts,
                    );
                    let got = r.get(z, x, y).unwrap();
                    if live.is_empty() {
                        assert_eq!(got, None, "empty tile absent from archive: {z}/{x}/{y}");
                    } else {
                        assert_eq!(
                            got.as_deref(),
                            Some(&live[..]),
                            "archive tile == live encode: {z}/{x}/{y}"
                        );
                    }
                }
            }
        }
    }
    std::fs::remove_file(&out).ok();
}

/// Review fix (Task 6): `run_build_pmtiles` used to hardcode the embedded MVT layer name to
/// `"pmtiles"` regardless of `--name`, so a pyramid's `source-layer` / metadata `vector_layers[].id`
/// never matched what a client style expects. Drive the CLI entry point end-to-end with
/// `--name roads` and assert the archive's metadata carries that name.
#[test]
fn build_pmtiles_uses_the_given_layer_name() {
    let out = std::env::temp_dir().join(format!("ts_e2e_name_{}.pmtiles", std::process::id()));
    // Own tmp subdir: `PmtilesWriter::new` derives its scratch filename from `std::process::id()`
    // alone, which collides with `generated_pmtiles_reads_back_identical_tiles`'s default
    // `std::env::temp_dir()` scratch file when both tests run concurrently in this same process.
    let tmpdir = std::env::temp_dir().join(format!("ts_e2e_name_tmp_{}", std::process::id()));
    std::fs::create_dir_all(&tmpdir).expect("create test tmpdir");
    let args = terraserve::BuildPmtilesArgs {
        vector: Some("fixtures/vector/countries.geojson".into()),
        out: out.to_string_lossy().into_owned(),
        min_zoom: 0,
        max_zoom: 2,
        bbox: None,
        tmpdir: Some(tmpdir.to_string_lossy().into_owned()),
        vec_style: Some("fixtures/styles/countries.vec.json".into()),
        src_crs: None,
        font: None,
        name: Some("roads".into()),
        mvt_max_features: terraserve::vector::mvt::DEFAULT_MAX_FEATURES_PER_TILE,
        mvt_min_feature_px: 0.0,
        mvt_no_optimizations: false,
        mvt_no_safety_limit: false,
        mvt_cell_px: 0.0,
        mvt_cell_field: None,
        mvt_cell_max_zoom: 0,
        mvt_dissolve: None,
        mvt_dissolve_max_zoom: 0,
        snap_tolerance: 0.01,
        topology_simplify: None,
        topology_dissolve: None,
        topology_dissolve_rollup: None,
        keep_fields: None,
    };

    terraserve::run_build_pmtiles(&args).unwrap();

    let reader = PmtilesReader::open(&out).expect("open pmtiles");
    assert!(
        reader.metadata().contains("\"id\":\"roads\""),
        "metadata should carry the --name layer id: {}",
        reader.metadata()
    );

    std::fs::remove_file(&out).ok();
    std::fs::remove_dir_all(&tmpdir).ok();
}
