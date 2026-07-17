#!/usr/bin/env python3
"""Run the terraserve binary against every manifest case and score it.

Exit 0 iff all REQUIRED cases pass. Invoked by ./score.sh (which builds first).
"""
import json
import os
import subprocess
import sys
import xml.etree.ElementTree as ET

HERE = os.path.dirname(os.path.abspath(__file__))     # pilot/tools
PILOT = os.path.dirname(HERE)                         # pilot/
FIX = os.path.join(PILOT, "fixtures")
TMP = os.path.join(PILOT, "tmp")
BIN = os.path.join(PILOT, "target", "debug", "terraserve")
PIXDIFF = os.path.join(HERE, "pixdiff.py")

os.makedirs(TMP, exist_ok=True)
m = json.load(open(os.path.join(FIX, "manifest.json")))


def style_path(s):
    return os.path.join(FIX, "styles", f"{s}.json")


def cog(c):
    return m["cogs"][c["cog"]]


def _run(cmd, **kw):
    # Binary runs with cwd = pilot/ so the manifest's "../cogs/..." paths resolve.
    return subprocess.run(cmd, cwd=PILOT, capture_output=True, **kw)


def pixdiff(out, c):
    t = c["tolerance"]
    g = os.path.join(FIX, c["golden"])
    r = subprocess.run(
        [sys.executable, PIXDIFF, out, g, "--max-dn", str(t["max_dn"]),
         "--min-frac", str(t["min_frac"]), "--blur", str(t.get("blur", 0.0))],
        capture_output=True, text=True)
    return r.returncode == 0, r.stdout.strip()


def run_render(c):
    out = os.path.join(TMP, c["id"] + ".png")
    if os.path.exists(out):
        os.remove(out)
    cmd = [BIN, "render", "--cog", cog(c), "--bbox", ",".join(map(str, c["bbox"])),
           "--crs", c["crs"], "--width", str(c["width"]), "--height", str(c["height"]),
           "--resample", c["resample"], "--style", style_path(c["style"]), "--out", out]
    r = _run(cmd, text=True)
    if r.returncode != 0 or not os.path.exists(out):
        return False, (r.stderr.strip().splitlines()[-1] if r.stderr.strip() else "no output")[:80]
    return pixdiff(out, c)


def _render_to(c, cog_key, tag):
    out = os.path.join(TMP, c["id"] + "." + tag + ".png")
    if os.path.exists(out):
        os.remove(out)
    cmd = [BIN, "render", "--cog", m["cogs"][cog_key], "--bbox", ",".join(map(str, c["bbox"])),
           "--crs", c["crs"], "--width", str(c["width"]), "--height", str(c["height"]),
           "--resample", c["resample"], "--style", style_path(c["style"]), "--out", out]
    r = _run(cmd, text=True)
    if r.returncode != 0 or not os.path.exists(out):
        return None, (r.stderr.strip().splitlines()[-1] if r.stderr.strip() else "no output")[:80]
    return out, ""


def run_render_equiv(c):
    a, err = _render_to(c, c["cog"], "a")
    if a is None:
        return False, "render A: " + err
    b, err = _render_to(c, c["ref_cog"], "b")
    if b is None:
        return False, "render ref: " + err
    t = c["tolerance"]
    r = subprocess.run(
        [sys.executable, PIXDIFF, a, b, "--max-dn", str(t["max_dn"]),
         "--min-frac", str(t["min_frac"]), "--blur", str(t.get("blur", 0.0))],
        capture_output=True, text=True)
    return r.returncode == 0, r.stdout.strip()


def run_wms_getmap(c):
    out = os.path.join(TMP, c["id"] + ".png")
    if os.path.exists(out):
        os.remove(out)
    cmd = [BIN, "wms-handle", "--cog", cog(c), "--style", style_path(c["style"]), "--query", c["query"]]
    r = _run(cmd)
    if r.returncode != 0 or not r.stdout:
        return False, (r.stderr.decode(errors="replace").strip()[-80:] or "no output")
    open(out, "wb").write(r.stdout)
    return pixdiff(out, c)


def run_wms_getcap(c):
    cmd = [BIN, "wms-handle", "--cog", cog(c), "--style", style_path(c["style"]), "--query", c["query"]]
    r = _run(cmd)
    if r.returncode != 0 or not r.stdout:
        return False, "no output"
    txt = r.stdout.decode(errors="replace")
    try:
        ET.fromstring(r.stdout)
    except Exception as e:  # noqa: BLE001
        return False, f"invalid XML: {e}"[:70]
    ck = c["checks"]
    if ck["version"] not in txt:
        return False, f"missing version {ck['version']}"
    if ck["layer"] not in txt:
        return False, f"missing layer '{ck['layer']}'"
    for s in ck.get("contains", []):
        if s not in txt:
            return False, f"missing '{s}'"
    return True, "structural ok"


def run_wms_exception(c):
    cmd = [BIN, "wms-handle", "--cog", cog(c), "--style", style_path(c["style"]), "--query", c["query"]]
    r = _run(cmd)
    txt = r.stdout.decode(errors="replace") if r.stdout else ""
    if "ServiceException" not in txt:
        return False, "no ServiceException in output"
    try:
        ET.fromstring(txt)
    except Exception as e:  # noqa: BLE001
        return False, f"invalid XML: {e}"[:60]
    for s in c.get("checks", {}).get("contains", []):
        if s not in txt:
            return False, f"missing '{s}'"
    return True, "exception ok"


HANDLERS = {
    "render": run_render,
    "render_equiv": run_render_equiv,
    "wms_getmap": run_wms_getmap,
    "wms_getcap": run_wms_getcap,
    "wms_exception": run_wms_exception,
}

if not os.path.exists(BIN):
    print(f"binary not built: {BIN}")
    sys.exit(2)

req_pass = req_tot = opt_pass = opt_tot = 0
rows = []
for c in m["cases"]:
    ok, detail = HANDLERS[c["kind"]](c)
    req = c.get("required", True)
    if req:
        req_tot += 1
        req_pass += int(ok)
    else:
        opt_tot += 1
        opt_pass += int(ok)
    rows.append((ok, req, c["id"], detail))

for ok, req, cid, detail in rows:
    tag = "PASS" if ok else "FAIL"
    star = "     " if req else "(opt)"
    print(f"  [{tag}] {star} {cid:22s} {detail}")

print(f"\nREQUIRED {req_pass}/{req_tot}    OPTIONAL {opt_pass}/{opt_tot}")
sys.exit(0 if req_pass == req_tot else 1)
