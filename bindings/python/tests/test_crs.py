"""Unit tests for the OGC CRS parser — no pygeoapi / no compiled module required."""

import pytest

from terraserve._crs import to_epsg


@pytest.mark.parametrize(
    "value,expected",
    [
        ("http://www.opengis.net/def/crs/EPSG/0/3857", "EPSG:3857"),
        ("http://www.opengis.net/def/crs/OGC/1.3/CRS84", "CRS:84"),
        ("urn:ogc:def:crs:EPSG::3857", "EPSG:3857"),
        ("EPSG:4326", "EPSG:4326"),
        ("CRS:84", "CRS:84"),
        (None, "EPSG:4326"),
        ("", "EPSG:4326"),
    ],
)
def test_to_epsg(value, expected):
    assert to_epsg(value) == expected


def test_to_epsg_rejects_garbage():
    with pytest.raises(ValueError):
        to_epsg("not-a-crs")
