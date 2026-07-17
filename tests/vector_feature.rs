use terraserve::vector::feature::{Feature, Geometry, Props, Value};

#[test]
fn point_feature_exposes_typed_props_and_fid() {
    let mut props = Props::new();
    props.insert("name".into(), Value::Str("Lisboa".into()));
    props.insert("scalerank".into(), Value::Num(2.0));
    let f = Feature::new(Geometry::Point([-9.14, 38.72]), props, 42);
    assert_eq!(f.fid, 42);
    assert_eq!(f.props.get_str("name"), Some("Lisboa"));
    assert_eq!(f.props.get_f64("scalerank"), Some(2.0));
    // wrong-type access returns None, not a panic
    assert_eq!(f.props.get_str("scalerank"), None);
    assert_eq!(f.props.get_f64("name"), None);
    match f.geom {
        Geometry::Point(p) => assert_eq!(p, [-9.14, 38.72]),
        _ => panic!("expected a point"),
    }
}
