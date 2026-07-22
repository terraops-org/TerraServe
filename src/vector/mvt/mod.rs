// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Bespoke Mapbox Vector Tile (MVT) encoder — the vector-tile *output* differentiator. Clean-room:
//! a hand-rolled protobuf writer (`wire`) + MVT geometry commands (`geom`) + a tile clip (`clip`),
//! assembled by `tile::encode_tile`. No `mvt`/`geozero`/`prost` crate. See
//! `docs/superpowers/specs/2026-07-13-mvt-vector-tiles-design.md`.

pub mod cell;
pub mod clip;
pub mod dissolve;
pub mod geom;
pub mod opts;
pub mod simplify;
pub mod tile;
pub mod wire;

pub use opts::{cell_units, validate_cell_flags, MvtOptimizations};
pub use tile::{
    encode_tile, encode_tile_opt, features_for_tile, layer_area_scale, min_area_src_for_zoom,
    DEFAULT_MAX_FEATURES_PER_TILE,
};
