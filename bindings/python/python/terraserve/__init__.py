"""TerraServe — raster map rendering (COG → PNG) from the Rust engine.

The compiled core is ``terraserve._terraserve``; ``render_png`` is re-exported here so
callers write ``import terraserve; terraserve.render_png(...)``. The pygeoapi
OGC API - Maps provider lives in ``terraserve.pygeoapi``.
"""

from ._terraserve import proj_info, render_png

__all__ = ["proj_info", "render_png"]
