#!/usr/bin/env python3
"""Pixel-diff a candidate PNG against a golden PNG within a tolerance.

    pixdiff.py candidate.png golden.png --max-dn N --min-frac F [--blur S]

Exit 0 = pass, 1 = fail. Both images are normalized to RGBA. An optional gaussian
blur washes out sub-pixel geometry shifts (resampling/reprojection) before diffing;
gross errors (wrong region, axis-order flip, garbage decode) survive the blur.
Where the golden pixel is fully transparent, RGB is don't-care (only alpha checked).
"""
import argparse
import sys

import numpy as np
from PIL import Image, ImageFilter


def load_rgba(path, blur):
    im = Image.open(path).convert("RGBA")
    if blur and blur > 0:
        im = im.filter(ImageFilter.GaussianBlur(radius=blur))
    return np.asarray(im, dtype=np.int16)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("candidate")
    ap.add_argument("golden")
    ap.add_argument("--max-dn", type=int, default=2)
    ap.add_argument("--min-frac", type=float, default=0.99)
    ap.add_argument("--blur", type=float, default=0.0)
    a = ap.parse_args()

    try:
        c = load_rgba(a.candidate, a.blur)
    except Exception as e:  # noqa: BLE001
        print(f"FAIL  candidate unreadable: {e}")
        return 1
    g = load_rgba(a.golden, a.blur)
    if c.shape != g.shape:
        cs = f"{c.shape[1]}x{c.shape[0]}"
        gs = f"{g.shape[1]}x{g.shape[0]}"
        print(f"FAIL  size {cs} != golden {gs}")
        return 1

    diff = np.abs(c - g)
    a_ok = diff[..., 3] <= a.max_dn
    rgb_ok = (diff[..., :3] <= a.max_dn).all(axis=-1)
    transp = g[..., 3] == 0                       # golden transparent -> RGB don't-care
    within = np.where(transp, a_ok, a_ok & rgb_ok)
    frac = float(within.mean())
    ok = frac >= a.min_frac
    print(f"{'PASS' if ok else 'FAIL'}  within={frac:.4f} need>={a.min_frac} "
          f"maxdn={a.max_dn} blur={a.blur} size={c.shape[1]}x{c.shape[0]}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
