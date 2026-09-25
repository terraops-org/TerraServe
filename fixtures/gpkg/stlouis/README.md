# StLouis multi-table GeoPackage (issue #22)

The regression fixture for public issue #22, "how does one reference a table within a
geopackage?". Used by `tests/gpkg_multi_table.rs`.

| File | What it is |
|---|---|
| `stlouis.gpkg` | `StLouis.gpkg` from [ngageoint/geopackage-js](https://github.com/ngageoint/geopackage-js/blob/master/docs/examples/GeoPackageToGo/StLouis.gpkg) (MIT), with its raster `tiles` table dropped (20 MB -> 80 KB). The three feature tables are untouched and in their original `gpkg_contents` order: `PointsOfInterest` (10), `Parks` (12), `Pizza` (9). No R-tree, like the original. |
| `LICENSE` | The upstream MIT licence of `stlouis.gpkg` (Copyright (c) 2015 National Geospatial-Intelligence Agency). It ships with the file, including inside the Docker image, which copies `fixtures/`. |
| `parks.sld`, `pizza.sld` | The reporter's two SLDs, verbatim from the issue. |
| `issue22.yaml` | The reporter's config, which names no table. Must fail at startup and list the tables. |
| `stlouis.yaml` | The same config with `vec_layer:` on each layer. |

Before the fix, both layers served `PointsOfInterest`, the first feature table: `Parks` rendered
blank (a polygon style over points), and `Pizza` only looked right because a point style happened
to fit the points of interest.
