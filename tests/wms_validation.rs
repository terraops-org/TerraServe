//! WMS GetMap request-validation regression tests (Step-0 P0: non-finite / degenerate params).
//!
//! Guards the fix that made `wms::get_map` reject non-finite / inverted BBOX and zero-size
//! WIDTH/HEIGHT with a ServiceException instead of serving garbage/blank HTTP 200 (a Rust-
//! specific trap: "NaN"/"inf"/"1e999" all parse as valid f64, and a `min >= max` guard misses
//! NaN because every NaN comparison is false). Invalid requests are rejected BEFORE the COG is
//! opened, so this needs no fixture COG and runs in CI.

use terraserve::style::Style;
use terraserve::wms;

const STYLE: &str = "fixtures/styles/rgb.json";

fn query(bbox: &str, w: &str, h: &str) -> String {
    format!(
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=cascais&STYLES=\
         &CRS=EPSG:3857&WIDTH={w}&HEIGHT={h}&FORMAT=image/png&BBOX={bbox}"
    )
}

fn is_service_exception(bytes: &[u8]) -> bool {
    String::from_utf8_lossy(bytes).contains("ServiceException")
}

fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG")
}

#[test]
fn getmap_rejects_nonfinite_inverted_and_zero_size() {
    let style = Style::load(STYLE).expect("load rgb style");
    // Never opened — validation fails before the render closure touches it.
    let cog = "/does/not/exist.tif";

    // (bbox, width, height, why) — every one must be a ServiceException, never a PNG.
    let bad = [
        ("NaN,NaN,NaN,NaN", "256", "256", "NaN bbox"),
        ("-inf,0,inf,1", "256", "256", "inf bbox"),
        ("1e999,0,1,1", "256", "256", "1e999 -> inf bbox"),
        ("100,100,0,0", "256", "256", "inverted bbox (min > max)"),
        ("0,0,10,10", "0", "256", "zero width"),
        ("0,0,10,10", "256", "0", "zero height"),
    ];
    for (bbox, w, h, why) in bad {
        let out = wms::handle(cog, &style, &query(bbox, w, h), None);
        assert!(
            is_service_exception(&out.bytes),
            "{why}: expected a ServiceException, got {} bytes",
            out.bytes.len()
        );
        assert!(
            !is_png(&out.bytes),
            "{why}: returned a PNG instead of an exception"
        );
    }
}
