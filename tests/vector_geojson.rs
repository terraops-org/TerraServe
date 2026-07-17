use terraserve::vector::feature::Geometry;
use terraserve::vector::geojson::GeoJsonSource;
use terraserve::vector::source::FeatureSource;

#[test]
fn parses_airports_fixture() {
    let s = GeoJsonSource::load("fixtures/vector/airports.geojson").unwrap();
    assert_eq!(s.features().len(), 893);
    assert!(s
        .features()
        .iter()
        .all(|f| matches!(f.geom, Geometry::Point(_))));
    let named = s
        .features()
        .iter()
        .filter(|f| f.props.get_str("name").is_some())
        .count();
    assert!(named > 800, "most airports have a name, got {named}");
    // ne_id becomes the stable fid
    assert!(s.features().iter().any(|f| f.fid > 0));
    let ext = s.full_extent(); // [w,s,e,n] in 4326
    assert!(ext[0] >= -180.0 && ext[2] <= 180.0 && ext[1] >= -90.0 && ext[3] <= 90.0);
    assert!(
        ext[0] < -170.0 && ext[2] > 170.0,
        "global extent, got {ext:?}"
    );
}

#[test]
fn rejects_unsupported_geometry() {
    // GeometryCollection is still unsupported (LineString/Polygon/Multi* are, as of this test).
    let gc = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","geometry":{"type":"GeometryCollection","geometries":[]},"properties":{}}]}"#;
    assert!(GeoJsonSource::from_str(gc).is_err());
}

#[test]
fn parses_linestring() {
    let s = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1],[2,0]]},"properties":{}}]}"#;
    let src = GeoJsonSource::from_str(s).unwrap();
    assert_eq!(src.features().len(), 1);
    match &src.features()[0].geom {
        Geometry::LineString(pts) => {
            assert_eq!(pts, &vec![[0.0, 0.0], [1.0, 1.0], [2.0, 0.0]]);
        }
        other => panic!("expected LineString, got {other:?}"),
    }
}

#[test]
fn parses_polygon_with_hole() {
    let s = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","geometry":{"type":"Polygon","coordinates":[
            [[0,0],[4,0],[4,4],[0,4],[0,0]],
            [[1,1],[2,1],[2,2],[1,2],[1,1]]
        ]},"properties":{}}]}"#;
    let src = GeoJsonSource::from_str(s).unwrap();
    assert_eq!(src.features().len(), 1);
    match &src.features()[0].geom {
        Geometry::Polygon(rings) => {
            assert_eq!(rings.len(), 2, "exterior + one hole");
            assert_eq!(
                rings[0],
                vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0], [0.0, 0.0]]
            );
            assert_eq!(
                rings[1],
                vec![[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 2.0], [1.0, 1.0]]
            );
        }
        other => panic!("expected Polygon, got {other:?}"),
    }
}

#[test]
fn parses_multilinestring() {
    let s = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","geometry":{"type":"MultiLineString","coordinates":[
            [[0,0],[1,1]],
            [[2,2],[3,3],[4,4]]
        ]},"properties":{}}]}"#;
    let src = GeoJsonSource::from_str(s).unwrap();
    assert_eq!(src.features().len(), 1);
    match &src.features()[0].geom {
        Geometry::MultiLineString(lines) => {
            assert_eq!(lines.len(), 2);
            assert_eq!(lines[0], vec![[0.0, 0.0], [1.0, 1.0]]);
            assert_eq!(lines[1], vec![[2.0, 2.0], [3.0, 3.0], [4.0, 4.0]]);
        }
        other => panic!("expected MultiLineString, got {other:?}"),
    }
}

#[test]
fn parses_countries_fixture() {
    // NE admin-0 countries+lakes, clipped to an extended Iberia/W-Mediterranean window
    // (EPSG:4326, -12 34 13 45) — see fixtures/vector/ for the ogr2ogr command.
    let s = GeoJsonSource::load("fixtures/vector/countries.geojson").unwrap();
    assert_eq!(s.features().len(), 11);

    let poly_count = s
        .features()
        .iter()
        .filter(|f| matches!(f.geom, Geometry::Polygon(_)))
        .count();
    let multi_count = s
        .features()
        .iter()
        .filter(|f| matches!(f.geom, Geometry::MultiPolygon(_)))
        .count();
    assert_eq!(poly_count, 7);
    assert_eq!(multi_count, 4);

    // Spain: MultiPolygon (mainland + Balearic/Canary islands within the clip window).
    let spain = s
        .features()
        .iter()
        .find(|f| f.props.get_str("NAME") == Some("Spain"))
        .expect("Spain present");
    assert!(matches!(spain.geom, Geometry::MultiPolygon(_)));

    // Italy: MultiPolygon whose mainland part has a hole — San Marino + Vatican are
    // carved out as interior rings at NE's admin-0 generalization.
    let italy = s
        .features()
        .iter()
        .find(|f| f.props.get_str("NAME") == Some("Italy"))
        .expect("Italy present");
    match &italy.geom {
        Geometry::MultiPolygon(polys) => {
            assert!(
                polys.iter().any(|p| p.len() > 1),
                "Italy should have a part with >1 ring (San Marino/Vatican hole)"
            );
        }
        other => panic!("expected Italy as MultiPolygon, got {other:?}"),
    }

    let ext = s.full_extent();
    assert!(
        ext[0] >= -13.0 && ext[2] <= 14.0 && ext[1] >= 33.0 && ext[3] <= 46.0,
        "extent should be within the clip window (+ margin), got {ext:?}"
    );
}

#[test]
fn parses_roads_fixture() {
    // NE roads, clipped to the same window as countries.geojson.
    let s = GeoJsonSource::load("fixtures/vector/roads.geojson").unwrap();
    assert_eq!(s.features().len(), 1043);

    let line_count = s
        .features()
        .iter()
        .filter(|f| matches!(f.geom, Geometry::LineString(_)))
        .count();
    let multi_count = s
        .features()
        .iter()
        .filter(|f| matches!(f.geom, Geometry::MultiLineString(_)))
        .count();
    assert_eq!(line_count, 1041);
    assert_eq!(multi_count, 2);

    let ext = s.full_extent();
    assert!(
        ext[0] >= -13.0 && ext[2] <= 14.0 && ext[1] >= 33.0 && ext[3] <= 46.0,
        "extent should be within the clip window (+ margin), got {ext:?}"
    );
}

#[test]
fn parses_multipolygon() {
    let s = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","geometry":{"type":"MultiPolygon","coordinates":[
            [[[0,0],[1,0],[1,1],[0,1],[0,0]]],
            [[[2,2],[3,2],[3,3],[2,3],[2,2]]]
        ]},"properties":{}}]}"#;
    let src = GeoJsonSource::from_str(s).unwrap();
    assert_eq!(src.features().len(), 1);
    match &src.features()[0].geom {
        Geometry::MultiPolygon(polys) => {
            assert_eq!(polys.len(), 2);
            assert_eq!(polys[0].len(), 1, "single exterior ring, no holes");
            assert_eq!(
                polys[0][0],
                vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.0, 0.0]]
            );
            assert_eq!(
                polys[1][0],
                vec![[2.0, 2.0], [3.0, 2.0], [3.0, 3.0], [2.0, 3.0], [2.0, 2.0]]
            );
        }
        other => panic!("expected MultiPolygon, got {other:?}"),
    }
}
