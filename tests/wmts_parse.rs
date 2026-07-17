//! WMTS KVP parsing: GetTile/GetCapabilities, percent-decoding (WR1), and OWS exception codes.

use terraserve::wmts::{parse_kvp, WmtsRequest};

#[test]
fn parses_gettile_kvp_case_insensitive() {
    let q =
        "service=WMTS&VERSION=1.0.0&Request=GetTile&layer=arctic&style=default&format=image/png&\
             tilematrixset=WebMercatorQuad&TileMatrix=5&TileRow=7&TileCol=9";
    match parse_kvp(q) {
        WmtsRequest::GetTile {
            layer,
            tms,
            z,
            row,
            col,
            ..
        } => {
            assert_eq!(layer, "arctic");
            assert_eq!(tms, "WebMercatorQuad");
            assert_eq!((z, row, col), (5, 7, 9));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn percent_encoded_format_is_decoded() {
    // WR1: URL-encoded FORMAT must decode to image/png, not fail InvalidParameterValue.
    let q = "request=GetTile&layer=a&style=default&format=image%2Fpng&\
             tilematrixset=x&tilematrix=0&tilerow=0&tilecol=0";
    assert!(matches!(parse_kvp(q), WmtsRequest::GetTile { .. }));
}

#[test]
fn getcapabilities_and_error_codes() {
    assert!(matches!(
        parse_kvp("service=WMTS&request=GetCapabilities"),
        WmtsRequest::GetCapabilities
    ));
    // SERVICE!=WMTS -> InvalidParameterValue (WR10).
    match parse_kvp("service=WMS&request=GetCapabilities") {
        WmtsRequest::Exception { code, locator, .. } => {
            assert_eq!(code, "InvalidParameterValue");
            assert_eq!(locator.as_deref(), Some("SERVICE"));
        }
        o => panic!("{o:?}"),
    }
    // Missing TILEROW -> MissingParameterValue.
    match parse_kvp("request=GetTile&layer=a&tilematrixset=x&tilematrix=0&tilecol=0") {
        WmtsRequest::Exception { code, .. } => assert_eq!(code, "MissingParameterValue"),
        o => panic!("{o:?}"),
    }
    // Bad FORMAT -> InvalidParameterValue.
    match parse_kvp("request=GetTile&layer=a&style=default&format=image/tiff&tilematrixset=x&tilematrix=0&tilerow=0&tilecol=0") {
        WmtsRequest::Exception { code, locator, .. } => {
            assert_eq!(code, "InvalidParameterValue");
            assert_eq!(locator.as_deref(), Some("FORMAT"));
        }
        o => panic!("{o:?}"),
    }
}
