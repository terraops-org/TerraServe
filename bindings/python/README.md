# terraserve (Python)

Python bindings for [TerraServe](https://github.com/terraops-org/TerraServe) — a clean-room
raster **and vector** tile engine in Rust (no GDAL / MapServer / GeoServer). This package
exposes the render core to Python and ships a [pygeoapi](https://pygeoapi.io) *OGC API - Maps*
provider.

```python
import terraserve

png = terraserve.render_png(
    cog_path="cascais.cog.tif",          # local path or s3://bucket/key
    bbox=(-9.45, 38.68, -9.38, 38.72),   # (minx, miny, maxx, maxy) in `crs` units
    crs="EPSG:4326",
    src_crs="EPSG:3763",
    width=512, height=512,
    style="rgb.json",
)
open("cascais.png", "wb").write(png)
```

The wheel is self-contained — PROJ is compiled in, so it imports without a system libproj.

## pygeoapi

```yaml
providers:
  - type: map
    name: terraserve.pygeoapi.TerraServeProvider
    data: /data/cascais.cog.tif
    options:
      src_crs: EPSG:3763
      style: /data/rgb.json
```

v1 covers the **raster** map path (COG → PNG). Styled, labeled **vector** maps
(GeoPackage / FlatGeoBuf / GeoJSON + SLD) are the next release.

Licensed under MPL-2.0.
