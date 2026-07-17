//! SIMD micro-benchmark — the bilinear-blend kernel (the 4-tap weighted average at the
//! heart of resampling), on SoA (pre-gathered) neighbour arrays so it's gather-free and
//! vectorizes cleanly. Same math three ways, to separate "what the compiler does" from
//! "what hand-SIMD adds":
//!
//!   1. scalar (no-vec)  — a per-element optimization barrier inhibits auto-vectorization
//!   2. scalar (autovec) — plain loop; LLVM auto-vectorizes under --release
//!   3. SIMD (wide x8)   — explicit 8-wide vectors
//!
//! Run it two ways to SEE the CPU-specific build effect:
//!   cargo run --release --example simd_bench                                   # baseline x86-64 (SSE2)
//!   RUSTFLAGS="-C target-cpu=native" cargo run --release --example simd_bench  # this CPU (AVX2/AVX-512)

use std::hint::black_box;
use std::time::Instant;

use wide::f32x8;

const N: usize = 8_000_000;
const REPS: usize = 40;

fn fill(seed: u64) -> Vec<f32> {
    (0..N as u64)
        .map(|i| (i.wrapping_mul(2654435761).wrapping_add(seed) % 997) as f32 / 997.0)
        .collect()
}

type Args<'a> = (
    &'a [f32],
    &'a [f32],
    &'a [f32],
    &'a [f32],
    &'a [f32],
    &'a [f32],
    &'a mut [f32],
);

#[inline(never)]
fn scalar_novec(
    tl: &[f32],
    tr: &[f32],
    bl: &[f32],
    br: &[f32],
    fx: &[f32],
    fy: &[f32],
    out: &mut [f32],
) {
    for i in 0..out.len() {
        let top = tl[i] + (tr[i] - tl[i]) * fx[i];
        let bot = bl[i] + (br[i] - bl[i]) * fx[i];
        out[i] = black_box(top + (bot - top) * fy[i]); // barrier -> no auto-vectorization
    }
}

#[inline(never)]
fn scalar(tl: &[f32], tr: &[f32], bl: &[f32], br: &[f32], fx: &[f32], fy: &[f32], out: &mut [f32]) {
    for i in 0..out.len() {
        let top = tl[i] + (tr[i] - tl[i]) * fx[i];
        let bot = bl[i] + (br[i] - bl[i]) * fx[i];
        out[i] = top + (bot - top) * fy[i];
    }
}

#[inline(never)]
fn simd(tl: &[f32], tr: &[f32], bl: &[f32], br: &[f32], fx: &[f32], fy: &[f32], out: &mut [f32]) {
    let n = out.len();
    let load = |s: &[f32], i: usize| f32x8::from(<[f32; 8]>::try_from(&s[i..i + 8]).unwrap());
    let mut i = 0;
    while i + 8 <= n {
        let (a, b, c, d) = (load(tl, i), load(tr, i), load(bl, i), load(br, i));
        let (x, y) = (load(fx, i), load(fy, i));
        let top = a + (b - a) * x;
        let bot = c + (d - c) * x;
        let r = top + (bot - top) * y;
        out[i..i + 8].copy_from_slice(&r.to_array());
        i += 8;
    }
    while i < n {
        let top = tl[i] + (tr[i] - tl[i]) * fx[i];
        let bot = bl[i] + (br[i] - bl[i]) * fx[i];
        out[i] = top + (bot - top) * fy[i];
        i += 1;
    }
}

// --- compute-bound contrast: a degree-15 polynomial per pixel (lots of FMAs, tiny
// memory traffic — 1 read + 1 write). This is where vectorization actually shines. ---
const C: [f32; 16] = [
    0.11, -0.23, 0.37, -0.41, 0.53, -0.61, 0.73, -0.79, 0.83, -0.89, 0.97, -1.01, 1.07, -1.09,
    1.13, -1.19,
];

#[inline(never)]
fn poly_novec(
    x: &[f32],
    _1: &[f32],
    _2: &[f32],
    _3: &[f32],
    _4: &[f32],
    _5: &[f32],
    out: &mut [f32],
) {
    for i in 0..out.len() {
        let xi = x[i];
        let mut acc = C[0];
        for k in 1..16 {
            acc = black_box(acc * xi + C[k]); // per-step barrier -> scalar
        }
        out[i] = acc;
    }
}

#[inline(never)]
fn poly_scalar(
    x: &[f32],
    _1: &[f32],
    _2: &[f32],
    _3: &[f32],
    _4: &[f32],
    _5: &[f32],
    out: &mut [f32],
) {
    for i in 0..out.len() {
        let xi = x[i];
        let mut acc = C[0];
        for k in 1..16 {
            acc = acc * xi + C[k];
        }
        out[i] = acc;
    }
}

#[inline(never)]
fn poly_simd(
    x: &[f32],
    _1: &[f32],
    _2: &[f32],
    _3: &[f32],
    _4: &[f32],
    _5: &[f32],
    out: &mut [f32],
) {
    let n = out.len();
    let mut i = 0;
    while i + 8 <= n {
        let xi = f32x8::from(<[f32; 8]>::try_from(&x[i..i + 8]).unwrap());
        let mut acc = f32x8::splat(C[0]);
        for k in 1..16 {
            acc = acc * xi + f32x8::splat(C[k]);
        }
        out[i..i + 8].copy_from_slice(&acc.to_array());
        i += 8;
    }
    while i < n {
        let xi = x[i];
        let mut acc = C[0];
        for k in 1..16 {
            acc = acc * xi + C[k];
        }
        out[i] = acc;
        i += 1;
    }
}

fn main() {
    let (tl, tr, bl, br, fx, fy) = (fill(1), fill(2), fill(3), fill(4), fill(5), fill(6));
    let mut out = vec![0f32; N];

    let run = |label: &str,
               f: fn(&[f32], &[f32], &[f32], &[f32], &[f32], &[f32], &mut [f32]),
               out: &mut Vec<f32>|
     -> f64 {
        f(&tl, &tr, &bl, &br, &fx, &fy, out); // warm
        let mut best = f64::INFINITY;
        for _ in 0..REPS {
            let t = Instant::now();
            f(&tl, &tr, &bl, &br, &fx, &fy, out);
            best = best.min(t.elapsed().as_secs_f64());
            black_box(out[0]);
        }
        println!(
            "  {label:18} {:8.3} ms   {:6.3} ns/px   {:6.2} Gpx/s",
            best * 1e3,
            best / N as f64 * 1e9,
            N as f64 / best / 1e9
        );
        best
    };

    println!("bilinear-blend kernel, {} px, best of {}:", N, REPS);
    let b0 = run("scalar (no-vec)", scalar_novec, &mut out);
    let b1 = run("scalar (autovec)", scalar, &mut out);
    let b2 = run("SIMD (wide x8)", simd, &mut out);

    println!(
        "  -> vectorization (best) vs no-vec : {:.2}x   (MEMORY-bound: bandwidth-limited)",
        b0 / b1.min(b2)
    );

    println!(
        "\ndegree-15 polynomial per px (compute-bound), {} px, best of {}:",
        N, REPS
    );
    let p0 = run("scalar (no-vec)", poly_novec, &mut out);
    let p1 = run("scalar (autovec)", poly_scalar, &mut out);
    let p2 = run("SIMD (wide x8)", poly_simd, &mut out);
    println!(
        "  -> vectorization (best) vs no-vec : {:.2}x   (COMPUTE-bound: this is where SIMD shines)",
        p0 / p1.min(p2)
    );
    let _: Args = (&tl, &tr, &bl, &br, &fx, &fy, &mut out); // keep types referenced
}
