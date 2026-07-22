// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Vector rendering + cartographic label placement (the label engine).
//!
//! Bespoke where it is the differentiator (placement, pipeline, Style IR, determinism);
//! crate-backed where it is plumbing (`harfrust` shaping, `swash` glyph raster). Produces a
//! flat RGBA8 buffer exactly like the raster path, encoded to PNG by `pngio::encode_rgba`.
//!
//! MVP scope (rung 1): point labels (airports) over WMS GetMap — see
//! `docs/superpowers/plans/2026-07-12-label-engine-mvp-point-labels.md`.

pub mod draw;
pub mod feature;
pub mod fgb;
pub mod geojson;
pub mod geom;
pub mod gpkg;
pub mod index;
pub mod mvt;
pub mod place;
pub mod pmtiles;
pub mod raster;
pub mod render;
pub mod shape;
pub mod sld_lower;
pub mod source;
pub mod style;
pub mod topology;
pub mod wkb;
