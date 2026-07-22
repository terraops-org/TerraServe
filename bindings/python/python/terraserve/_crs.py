"""OGC CRS identifier parsing — no third-party dependencies, so it is unit-testable
without pygeoapi installed.

OGC API hands CRS as a URI (``http://www.opengis.net/def/crs/EPSG/0/3857``) or, from some
clients, a URN (``urn:ogc:def:crs:EPSG::3857``). The engine wants an ``EPSG:NNNN`` /
``CRS:84`` shortcode.
"""

import re

_URI = re.compile(r"/def/crs/(?P<auth>[^/]+)/[^/]*/(?P<code>[^/]+)/?$")
_URN = re.compile(r"^urn:ogc:def:crs:(?P<auth>[^:]+):[^:]*:(?P<code>.+)$")


def to_epsg(crs, default="EPSG:4326"):
    """Normalise a CRS identifier to an ``EPSG:NNNN`` / ``CRS:84`` shortcode.

    ``None`` / empty → *default*. Raises ``ValueError`` on an unrecognised value.
    """
    if not crs:
        return default
    crs = crs.strip()
    if crs.upper().startswith(("EPSG:", "CRS:")):
        return crs
    for pat in (_URI, _URN):
        m = pat.search(crs)
        if m:
            auth = m.group("auth").upper()
            code = m.group("code")
            if auth == "OGC" and code.upper() == "CRS84":
                return "CRS:84"
            return f"{auth}:{code}"
    raise ValueError(f"unrecognised CRS: {crs!r}")
