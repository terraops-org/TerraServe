# Third-party notices

TerraServe itself is MPL-2.0 (see `LICENSE`).

The **standalone Linux binary** and the **Python wheels** statically link several third-party
components and embed one third-party data file. Those distributions carry this notice, and
`proj/COPYING` beside it holds the upstream PROJ licence verbatim. A build against a system
libproj (the default `cargo build`) links none of this statically and needs none of it.

The Docker image is not affected either: it installs libproj from the distribution, under the
distribution's own packaging.

## PROJ, and the `proj.db` database embedded in the binary

Copyright (c) 2000, Frank Warmerdam, and the PROJ contributors.

PROJ is distributed under an MIT/X-style licence whose own words are
"All source, **data files** and other contents of the PROJ package are available under the
following terms", so the licence covers `proj.db` as well as the library. The full text is in
`proj/COPYING`, reproduced from the PROJ source tarball we build from.

`proj.db` is embedded in the binary and written to a per-user cache directory on first run,
unmodified, byte for byte as PROJ produced it. Nothing in it is altered.

The PROJ version is whichever the `proj-sys` crate vendors for the pinned release, NOT the
latest upstream release: `proj-sys` ships its own source tarball and lags upstream. As of
TerraServe 0.3.0 that is `proj-sys` 0.26.0 -> **PROJ 9.6.0** (upstream was 9.8.1). This applies
only to the bundled distributions; a build against a system libproj uses whatever version the
distribution provides.

### Data aggregated inside `proj.db`

`proj.db` is an aggregate. PROJ builds it from several public geodetic registries, and the
exact versions travel inside the file itself rather than being copied into this document,
where they would go stale. Each release tarball carries `proj/VERSIONS.txt`, generated from
the shipped database at build time; you can reproduce it from any copy with:

    sqlite3 proj.db "SELECT key, value FROM metadata ORDER BY key"

The registries and their custodians:

| Registry | Custodian |
|---|---|
| EPSG Dataset | IOGP (International Association of Oil & Gas Producers) |
| ESRI projection engine data | Esri |
| IGNF | Institut national de l'information geographique et forestiere (France) |
| NKG | Nordic Geodetic Commission |

**EPSG.** The EPSG Dataset is owned by IOGP and supplied under the EPSG terms of use
(<https://epsg.org/terms-of-use.html>). It is redistributed here as part of PROJ, unmodified.
IOGP does not warrant the data and accepts no liability for its use. The dataset is provided
free of charge and is not sold as part of this distribution.

**ESRI.** Projection-engine definitions published by Esri and redistributed by the PROJ project.

## SQLite

Embedded via `rusqlite`'s `bundled` feature, and also used by PROJ to read `proj.db`.
SQLite is in the **public domain** (<https://sqlite.org/copyright.html>); no attribution is
required, and it is named here only for completeness.

## jemalloc

Embedded via `tikv-jemallocator` as the global allocator in the server binary (not in the
Python extension module, which drops it). jemalloc is 2-clause BSD.
Copyright (C) 2002-present Jason Evans and others.

## Rust crates

The remaining dependencies are pure-Rust crates under permissive licences (MIT, Apache-2.0,
BSD, or dual MIT/Apache-2.0). `cargo tree` on the pinned `Cargo.lock` in a given release tag
reproduces the exact set, and `cargo license` or `cargo about` will print their texts.
