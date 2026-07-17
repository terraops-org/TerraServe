//! TMS 1.0.0 front-end unit checks: the y-flip, spec parsing, profile, and the TileMap XML
//! (BoundingBox + bottom-left Origin + one TileSet per zoom). Pure functions — no server needed.

use terraserve::tms::TileMatrixSet;
use terraserve::tms_http;

#[test]
fn y_flip_maps_bottom_left_to_top_left() {
    let g = TileMatrixSet::web_mercator_quad(256);
    // At z=2, matrix_h=4. TMS y=0 (bottom) -> core row 3 (top-left, south).
    assert_eq!(tms_http::tms_y_to_core_row(&g, 2, 0), Some(3));
    assert_eq!(tms_http::tms_y_to_core_row(&g, 2, 3), Some(0));
    assert_eq!(tms_http::tms_y_to_core_row(&g, 2, 4), None); // out of range
}

#[test]
fn parse_layer_spec_splits_grid() {
    assert_eq!(
        tms_http::parse_layer_spec("basemap"),
        ("basemap".to_string(), None)
    );
    assert_eq!(
        tms_http::parse_layer_spec("basemap@WebMercatorQuad"),
        ("basemap".to_string(), Some("WebMercatorQuad".to_string()))
    );
}

#[test]
fn profile_is_well_known_for_canonical_grids() {
    assert_eq!(
        tms_http::tms_profile(&TileMatrixSet::web_mercator_quad(256)),
        "global-mercator"
    );
    assert_eq!(
        tms_http::tms_profile(&TileMatrixSet::world_crs84_quad(256)),
        "global-geodetic"
    );
    // A 512 variant is not the canonical well-known grid -> local.
    assert_eq!(
        tms_http::tms_profile(&TileMatrixSet::web_mercator_quad(512)),
        "local"
    );
    assert_eq!(
        tms_http::tms_profile(&TileMatrixSet::ups_wgs84_quad("EPSG:5041", 256)),
        "local"
    );
}

#[test]
fn tilemap_xml_has_bbox_bottom_left_origin_and_tilesets() {
    let g = TileMatrixSet::web_mercator_quad(256);
    let xml = tms_http::tilemap_xml_for("basemap", &g, None, "http://h/tms/1.0.0");
    assert!(xml.contains("<SRS>EPSG:3857</SRS>"));
    assert!(
        xml.contains("<BoundingBox"),
        "spec-required BoundingBox missing"
    );
    assert!(xml.contains("<Origin"));
    assert!(xml.contains("profile=\"global-mercator\""));
    // Bottom-left Origin == the grid's SW corner (WebMercator full extent).
    assert!(xml.contains("x=\"-20037508.3427892\""));
    assert!(xml.contains("y=\"-20037508.3427892\""));
    // One TileSet per zoom (25 levels).
    assert_eq!(xml.matches("<TileSet ").count(), 25);
    // A tile href a client appends /{x}/{y}.png to.
    assert!(xml.contains("href=\"http://h/tms/1.0.0/basemap@WebMercatorQuad/0\""));
}

#[test]
fn tms_root_derives_from_wms_base() {
    assert_eq!(
        tms_http::tms_root("http://localhost:8080/wms"),
        "http://localhost:8080/tms/1.0.0"
    );
    assert_eq!(tms_http::tms_root("http://h/"), "http://h/tms/1.0.0");
}
