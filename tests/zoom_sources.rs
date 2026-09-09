//! Per-zoom pre-generalized sources on the LIVE path (build-order step 4).
//!
//! `terraserve extract` cuts one subset per zoom band so that tiling becomes a pure spatial cut.
//! Until now those subsets were only usable in an offline bake: the server still read the full
//! source at every zoom, so an archive miss at a shallow zoom went straight back to a
//! whole-continent query. `VectorLayer::zoom_sources` closes that -- a zoom inside a declared band
//! reads the band's file, a zoom outside every band reads the layer's own source.
//!
//! The two fixtures are deliberately DISJOINT in content, not just in size: `countries.geojson`
//! embeds country names, `mini_mvt.geojson` embeds its own. That is what makes "which source
//! answered" observable in the encoded tile rather than merely plausible.

use std::sync::Arc;

use terraserve::mvt_http::render_mvt_tile;
use terraserve::server::{Layer, ServeState, VectorLayer, ZoomBand};
use terraserve::vector::geojson::GeoJsonSource;
use terraserve::vector::shape::Shaper;
use terraserve::vector::source::{FeatureSource, VectorSource};
use terraserve::vector::style::Style;

const LAYER: &str = "banded";

fn load(path: &str) -> Arc<GeoJsonSource> {
    Arc::new(GeoJsonSource::load(path).unwrap())
}

/// A layer whose own source is `countries.geojson`, with `mini_mvt.geojson` declared as the
/// z0..=1 band. Both are EPSG:4326 GeoJSON, so the only difference the encoder can see is content.
fn banded_layer(bands: Vec<ZoomBand>) -> Layer {
    let src = load("fixtures/vector/countries.geojson");
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
            // Derived from the BASE source's extent, never a band's -- a band declares the extent
            // of the layer it was cut from precisely so this number does not move (the
            // `with_declared_extent` bug from build-order step 2).
            area_scale: terraserve::vector::mvt::layer_area_scale(ext, ext),
            min_feature_px: 0.0,
            source: VectorSource::LoadAll(src),
            style,
            shaper,
            lod: None,
            zoom_sources: bands,
        }),
        pmtiles: std::collections::BTreeMap::new(),
        raster_pmtiles: std::collections::BTreeMap::new(),
        overlay: std::collections::BTreeMap::new(),
    }
}

/// A plain layer whose own source is `path` and which declares no bands -- the control that says
/// what a given file encodes to on its own.
fn source_layer(path: &str) -> Layer {
    let mut l = banded_layer(Vec::new());
    let src = load(path);
    l.vector.as_mut().unwrap().source = VectorSource::LoadAll(src);
    l
}

fn band(min_zoom: u32, max_zoom: u32, path: &str) -> ZoomBand {
    ZoomBand {
        min_zoom,
        max_zoom,
        source: VectorSource::LoadAll(load(path)),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn a_zoom_inside_a_band_reads_the_band_and_a_zoom_outside_reads_the_source() {
    // Three servers, identical but for where the geometry comes from. Comparing whole encoded
    // tiles between them is a stronger discriminator than looking for a marker string: it does not
    // depend on which attributes the style happens to carry into the tile.
    let banded = ServeState::new(
        vec![banded_layer(vec![band(
            0,
            1,
            "fixtures/vector/mini_mvt.geojson",
        )])],
        "http://h/wms".into(),
        16,
    );
    let plain = ServeState::new(vec![banded_layer(Vec::new())], "http://h/wms".into(), 16);
    let only_band = ServeState::new(
        vec![source_layer("fixtures/vector/mini_mvt.geojson")],
        "http://h/wms".into(),
        16,
    );

    let tile = |st: &ServeState, z, x, y| {
        render_mvt_tile(st, LAYER, "WorldCRS84Quad", z, x, y, false)
            .unwrap()
            .bytes
    };

    // z0 is inside the declared band: the banded server must produce exactly what a server whose
    // ONLY source is the band file produces, and must NOT produce what the base source gives.
    let z0_banded = tile(&banded, 0, 0, 0);
    assert!(!z0_banded.is_empty(), "z0/0/0 covers both fixtures");
    assert_eq!(
        z0_banded,
        tile(&only_band, 0, 0, 0),
        "a zoom inside the band must be encoded from the band's file"
    );
    assert_ne!(
        z0_banded,
        tile(&plain, 0, 0, 0),
        "if these match, the band was never consulted"
    );

    // z3 is outside every band, so the layer's own source answers -- the fallback that keeps a
    // partially extracted layer serving at every zoom. 8/2 is the densest z3 tile over the
    // fixture's Iberia/W-Med extent.
    let z3_banded = tile(&banded, 3, 8, 2);
    assert!(!z3_banded.is_empty(), "z3/8/2 is over the fixture's data");
    assert_eq!(
        z3_banded,
        tile(&plain, 3, 8, 2),
        "a zoom outside every band must be encoded from the layer's own source"
    );
}

#[test]
fn band_bounds_are_inclusive_on_both_ends() {
    let l = banded_layer(vec![band(2, 4, "fixtures/vector/mini_mvt.geojson")]);
    let v = l.vector.as_ref().unwrap();
    let is_band =
        |z: u32| v.source_for_zoom(z).full_extent() == v.zoom_sources[0].source.full_extent();
    assert!(!is_band(1), "z1 sits below the band");
    assert!(is_band(2), "min_zoom is inclusive");
    assert!(is_band(4), "max_zoom is inclusive");
    assert!(!is_band(5), "z5 sits above the band");
}
