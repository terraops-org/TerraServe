//! Tier-3: exact point VALUE vs the GDAL oracle (`gdallocationinfo`). Independent of Tier-2's
//! image alignment: it cross-checks the pixel→value mapping itself. Per the Fable review, points are
//! snapped to SOURCE-PIXEL CENTERS (transformed to dataset coords) and fed to BOTH sides, so the
//! comparison is exact (no pixel-edge ambiguity that a "near a gradient" point would introduce).
//! TerraServe reads level-0 losslessly (`sample_point`); gdallocationinfo reads full-res by default.
//! Self-skips without GDAL / the fixture.

mod common;
use common::{gdal_available, gdal_value_at};

use terraserve::cog::{self, LocalFileRangeSource};
use terraserve::render::{sample_point_with_cog, InfoRequest};

const PATH: &str = "../cogs/polar/arcticdem_18_47_32m_gunnbjorn_dem.tif";

#[test]
fn point_values_match_gdallocationinfo_exactly() {
    if !gdal_available() || !std::path::Path::new(PATH).exists() {
        eprintln!("skipping: gdallocationinfo or fixture absent");
        return;
    }
    let src = LocalFileRangeSource::open(PATH).unwrap();
    let cog = cog::parse(&src).unwrap();
    let g = cog.levels[0].geo;
    let index_cache = terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes());

    // High-relief source pixels in the massif (data-rich; a shift/mapping bug shows most here).
    let mut checked = 0;
    for &(col, row) in &[(1550u32, 1550u32), (1600, 1500), (1500, 1600), (1650, 1650)] {
        // The pixel's CENTER in the dataset CRS (EPSG:3413).
        let x = g.origin_x + (col as f64 + 0.5) * g.px;
        let y = g.origin_y - (row as f64 + 0.5) * g.py;

        // TerraServe: a 1×1 identity map whose only pixel center is exactly (x, y) → source (col,row).
        let ir = InfoRequest {
            bbox: [
                x - g.px * 0.5,
                y - g.py * 0.5,
                x + g.px * 0.5,
                y + g.py * 0.5,
            ],
            crs: "EPSG:3413",
            src_crs: "EPSG:3413",
            width: 1,
            height: 1,
            i: 0,
            j: 0,
            band_math: None,
        };
        let info = sample_point_with_cog(&ir, &cog, &src, &index_cache).unwrap();
        assert!(info.in_image, "({col},{row}) mapped outside the image");
        assert_eq!(
            (info.source_col, info.source_row),
            (col as i64, row as i64),
            "pixel mapping drifted"
        );

        let gd = gdal_value_at(PATH, x, y).expect("gdallocationinfo returned no value");
        eprintln!(
            "({col},{row}) @ ({x:.1},{y:.1}): terraserve {} vs gdal {gd}",
            info.bands[0]
        );
        // Both read the SAME native pixel losslessly -> exact (tiny f32 slack for text round-trip).
        assert!(
            (info.bands[0] - gd).abs() < 1e-2,
            "({col},{row}): TerraServe {} != GDAL {gd} — pixel→value mapping is wrong",
            info.bands[0]
        );
        checked += 1;
    }
    assert!(checked >= 3, "expected to check several points");
}
