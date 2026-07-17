use terraserve::vector::place::{place_labels, LabelItem, Placement};
use terraserve::vector::shape::Shaper;

fn shaper() -> Shaper {
    Shaper::from_font_bytes(&std::fs::read("fixtures/fonts/DejaVuSans.ttf").unwrap()).unwrap()
}
fn item(fid: u64, pri: f64, x: f32, y: f32, sh: &Shaper) -> LabelItem {
    LabelItem {
        fid,
        priority: pri,
        anchor: [x, y],
        marker_r: 3.0,
        label: sh.shape("Airport", 13.0),
        offset: 4.0,
    }
}

#[test]
fn single_feature_places_its_own_label() {
    let sh = shaper();
    // B3 guard: a feature's own marker must NOT block its own label.
    assert_eq!(place_labels(&[item(1, 1.0, 50.0, 50.0, &sh)]).len(), 1);
}

#[test]
fn far_apart_both_place() {
    let sh = shaper();
    let out = place_labels(&[
        item(1, 1.0, 50.0, 50.0, &sh),
        item(2, 1.0, 500.0, 500.0, &sh),
    ]);
    assert_eq!(out.len(), 2);
}

#[test]
fn placed_label_ink_is_inside_its_reserved_box() {
    // B3 on-screen: the drawn glyph ink must lie within the collision box the placer reserved
    // (regression guard for the box-vs-baseline mismatch Fable caught).
    let sh = shaper();
    let it = item(1, 1.0, 100.0, 100.0, &sh);
    let out = place_labels(std::slice::from_ref(&it));
    assert_eq!(out.len(), 1);
    let p = out[0];
    let (box_top, box_bot) = (p.origin[1], p.origin[1] + it.label.height);
    let base_y = p.origin[1] + it.label.ascent;
    for g in &it.label.glyphs {
        let (ink_top, ink_bot) = (base_y + g.dy, base_y + g.dy + g.h as f32);
        assert!(
            ink_top >= box_top - 1.0 && ink_bot <= box_bot + 1.0,
            "glyph ink [{ink_top},{ink_bot}] outside reserved box [{box_top},{box_bot}]"
        );
    }
}

#[test]
fn dense_cluster_drops_some_and_is_order_independent() {
    let sh = shaper();
    let fwd: Vec<_> = (0..20u64)
        .map(|i| item(i, i as f64, 200.0, 200.0, &sh))
        .collect();
    let rev: Vec<_> = (0..20u64)
        .rev()
        .map(|i| item(i, i as f64, 200.0, 200.0, &sh))
        .collect();
    let a = place_labels(&fwd);
    let b = place_labels(&rev);
    // Normalize to (fid, origin) sets — order-independent even though item indices differ.
    let norm = |ps: &[Placement], items: &[LabelItem]| {
        let mut v: Vec<(u64, i32, i32)> = ps
            .iter()
            .map(|p| (items[p.item].fid, p.origin[0] as i32, p.origin[1] as i32))
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        norm(&a, &fwd),
        norm(&b, &rev),
        "same features at same positions regardless of input order"
    );
    assert!(
        a.len() < 20,
        "some labels drop in a dense cluster (placed {})",
        a.len()
    );
    assert!(
        a.iter().any(|p| fwd[p.item].fid == 0),
        "highest-priority (fid 0) is placed"
    );
}

#[test]
fn empty_label_is_skipped() {
    let sh = shaper();
    let mut it = item(1, 1.0, 50.0, 50.0, &sh);
    it.label = sh.shape("", 13.0); // empty label → marker only
    assert_eq!(place_labels(&[it]).len(), 0);
}

#[test]
fn larger_offset_places_label_farther_from_marker() {
    // Task 6: the per-item `offset` (fed from the SLD `<Displacement>` magnitude) pushes the label
    // box farther from the marker edge. Same anchor + label, small vs large offset → the large-offset
    // label's placed origin is farther from the anchor. If the offset were ignored (not per-item),
    // the two origins would coincide — so this pins the per-item offset plumbing.
    let sh = shaper();
    let near = item(1, 1.0, 200.0, 200.0, &sh); // default offset 4.0
    let mut far = item(2, 1.0, 200.0, 200.0, &sh);
    far.offset = 40.0;
    let pn = place_labels(std::slice::from_ref(&near))[0].origin;
    let pf = place_labels(std::slice::from_ref(&far))[0].origin;
    let dx_near = (pn[0] - 200.0).abs();
    let dx_far = (pf[0] - 200.0).abs();
    assert!(
        dx_far > dx_near + 20.0,
        "offset 40 places the label farther ({dx_far}) than offset 4 ({dx_near})"
    );
}
