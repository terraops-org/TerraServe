//! Self-test for the ZNCC sub-pixel shift detector: inject a KNOWN translational shift into a
//! synthetic textured image and assert the detector recovers it. This validates the detector
//! independently of GDAL — so a green Tier-2 (vs gdalwarp) means the measured shift is real, not a
//! detector artifact. The worst failure of a correctness rig is FALSE CONFIDENCE; this guards it.

mod common;
use common::{gray_shift, zncc_shift};

/// A richly-textured, APERIODIC image (gradients + radial term break periodicity) so the ZNCC peak
/// is unambiguous within the search window.
fn synth(w: usize, h: usize) -> Vec<f32> {
    let mut v = vec![0f32; w * h];
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    for y in 0..h {
        for x in 0..w {
            let (xf, yf) = (x as f32, y as f32);
            let r = ((xf - cx).powi(2) + (yf - cy).powi(2)).sqrt();
            v[y * w + x] = 128.0
                + 40.0 * (xf * 0.13).sin()
                + 30.0 * (yf * 0.17).cos()
                + 25.0 * ((xf + yf) * 0.09).sin()
                + 18.0 * (r * 0.21).sin()
                + 0.15 * xf
                + 0.10 * yf;
        }
    }
    v
}

#[test]
fn recovers_known_subpixel_shift() {
    let (w, h) = (128usize, 128usize);
    let a = synth(w, h);
    // `b` is `a` translated by (dx,dy): b[x,y] = a[x-dx, y-dy]. Then zncc_shift(b, a) == (dx,dy).
    for &(dx, dy) in &[
        (0.0, 0.0),
        (3.0, -2.0),
        (0.5, 1.5),
        (-1.25, 0.75),
        (2.4, 2.6),
    ] {
        let b = gray_shift(&a, w, h, dx, dy);
        let s = zncc_shift(&b, &a, w, h, 6).expect("detector should find a peak");
        assert!(
            (s.dx - dx).abs() < 0.15 && (s.dy - dy).abs() < 0.15,
            "injected shift ({dx},{dy}) recovered as ({:.3},{:.3})",
            s.dx,
            s.dy
        );
        // A clean same-content alignment has a near-perfect peak — the reliability signal callers gate on.
        assert!(
            s.peak > 0.98,
            "expected a strong peak for identical content, got {:.3}",
            s.peak
        );
    }
}

#[test]
fn flat_image_yields_no_confident_shift() {
    // A constant image has no structure -> ZNCC is degenerate -> None (never a false 0,0 claim).
    let (w, h) = (64usize, 64usize);
    let flat = vec![100.0f32; w * h];
    assert!(zncc_shift(&flat, &flat, w, h, 4).is_none());
}
