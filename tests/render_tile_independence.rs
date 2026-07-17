//! Tile-independence guard (Step-0 P0 GAP-7): a mosaic of independently-rendered adjacent
//! sub-windows must equal the single whole-window render, byte-for-byte.
//!
//! This is the invariant that underpins *every* golden-image oracle in the matrix: if the
//! renderer leaked cross-tile state or was non-deterministic, per-tile goldens would be flaky
//! or blind. With nearest resampling and pixel-aligned splits the whole and the 2×2 mosaic map
//! each output pixel to the same source pixel, so they must be identical.
//!
//! (Bilinear/cubic would additionally need a 1–2 px source halo at seams; that stricter case
//! belongs with the deferred phase-correlation/seam test-rig. This guards the nearest path,
//! which is the WMS GetMap default.)
//!
//! Uses the real fixture COG; skips if absent.

use terraserve::backend::Resample;
use terraserve::cog::{self, LocalFileRangeSource};
use terraserve::render::{self, RenderRequest};
use terraserve::style::Style;

const COG: &str = "../cogs/cascais.cog.deflate.tif";
const STYLE: &str = "fixtures/styles/rgb.json";
const CRS: &str = "EPSG:3763";
// A native-resolution window (from the seam-test manifest region).
const BBOX: [f64; 4] = [-112701.25, -106296.25, -112573.25, -106168.25];
const N: usize = 256; // whole render is N×N; quadrants are (N/2)×(N/2)

fn have_fixtures() -> bool {
    std::path::Path::new(COG).exists() && std::path::Path::new(STYLE).exists()
}

fn place(
    dst: &mut [u8],
    dst_w: usize,
    quad: &[u8],
    qw: usize,
    qh: usize,
    off_col: usize,
    off_row: usize,
) {
    for r in 0..qh {
        for c in 0..qw {
            let s = (r * qw + c) * 4;
            let d = ((off_row + r) * dst_w + (off_col + c)) * 4;
            dst[d..d + 4].copy_from_slice(&quad[s..s + 4]);
        }
    }
}

#[test]
fn mosaic_of_quadrants_equals_whole() {
    if !have_fixtures() {
        eprintln!("skipping mosaic_of_quadrants_equals_whole: fixtures absent");
        return;
    }
    let src = LocalFileRangeSource::open(COG).unwrap();
    let cog = cog::parse(&src).unwrap();
    let style = Style::load(STYLE).unwrap();

    let index_cache = terraserve::cache::new_index_cache(terraserve::cache::index_cache_bytes());
    let render = |bbox: [f64; 4], w: u32, h: u32| {
        render::render_with_cog(
            &RenderRequest {
                cog_path: COG,
                bbox,
                crs: CRS,
                src_crs: CRS,
                width: w,
                height: h,
                resample: Resample::Nearest,
                style: &style,
                band_math: None,
                index_cache: index_cache.clone(),
            },
            &cog,
            &src,
            None,
        )
        .unwrap()
    };

    let [minx, miny, maxx, maxy] = BBOX;
    let (midx, midy) = ((minx + maxx) / 2.0, (miny + maxy) / 2.0);
    let q = (N / 2) as u32;

    // Whole render.
    let whole = render(BBOX, N as u32, N as u32);

    // Four independent quadrant renders (row 0 = top = maxy).
    let tl = render([minx, midy, midx, maxy], q, q); // top-left
    let tr = render([midx, midy, maxx, maxy], q, q); // top-right
    let bl = render([minx, miny, midx, midy], q, q); // bottom-left
    let br = render([midx, miny, maxx, midy], q, q); // bottom-right

    let (qw, qh) = (N / 2, N / 2);
    let mut mosaic = vec![0u8; N * N * 4];
    place(&mut mosaic, N, &tl, qw, qh, 0, 0);
    place(&mut mosaic, N, &tr, qw, qh, qw, 0);
    place(&mut mosaic, N, &bl, qw, qh, 0, qh);
    place(&mut mosaic, N, &br, qw, qh, qw, qh);

    assert_eq!(whole.len(), mosaic.len(), "size mismatch");
    // Count differing pixels for a useful message rather than a bare assert.
    let diff = whole
        .chunks_exact(4)
        .zip(mosaic.chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(
        diff,
        0,
        "tile-independence broken: {diff}/{} pixels differ between whole render and 2×2 mosaic",
        N * N
    );
}
