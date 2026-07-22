"""A pygeoapi *OGC API - Maps* provider backed by the TerraServe render engine.

Drop-in replacement for the ``mapscript`` provider. Reference it in a pygeoapi config by
its dotted path — no pygeoapi core change needed::

    providers:
      - type: map
        name: terraserve.pygeoapi.TerraServeProvider
        data: /data/cascais.cog.tif
        options:
          src_crs: EPSG:3763
          style: /data/rgb.json
          resample: bilinear

v1 renders the **raster** COG path. Styled, labeled **vector** maps (GeoPackage /
FlatGeoBuf / GeoJSON + SLD) are the next release; the ``query`` dispatch below already
branches on the data type so that becomes a fill-in, not a rewrite.
"""

from pygeoapi.provider.base import BaseProvider

from . import render_png
from ._crs import to_epsg

_RASTER_EXT = (".tif", ".tiff", ".cog")


class TerraServeProvider(BaseProvider):
    """OGC API - Maps provider that renders a COG window to PNG via TerraServe."""

    def __init__(self, provider_def):
        super().__init__(provider_def)
        opts = provider_def.get("options", {}) or {}
        # The COG's own CRS (per layer). pygeoapi does not carry this, so it is a provider option.
        self.src_crs = opts.get("src_crs", "EPSG:4326")
        self.style = opts["style"]
        self.resample = opts.get("resample", "bilinear")

    def query(self, style=None, bbox=[], width=500, height=300, crs=None,
              datetime_=None, format_="png", transparent=True, **kwargs):
        """Render the map window and return encoded image bytes.

        ``crs``/``bbox-crs`` arrive as OGC CRS URIs; ``**kwargs`` is kept because the API
        also passes hyphenated ``bbox-crs`` and other params.
        """
        if self.data.lower().endswith(_RASTER_EXT):
            return render_png(
                cog_path=self.data,
                bbox=tuple(bbox),
                crs=to_epsg(crs or kwargs.get("bbox-crs")),
                src_crs=self.src_crs,
                width=width,
                height=height,
                style=self.style,
                resample=self.resample,
            )
        raise NotImplementedError(
            "TerraServe v1 renders raster COGs only; styled/labeled vector maps "
            "(GeoPackage / FlatGeoBuf / GeoJSON) are a forthcoming release."
        )
