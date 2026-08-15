// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! TerraServe pilot library.
//!
//! The measured agent implements the bodies of `run_render` and `run_wms_handle`
//! and everything they call. Requirements live in SPEC-AGENT.md and CLAUDE.md.
//!
//! Keep intact (they are graded / frozen):
//!   * the CLI arg shapes (`RenderArgs`, `WmsArgs`),
//!   * the `RangeSource` trait (cog.rs) — all COG bytes flow through it,
//!   * the batch-first `RenderBackend` trait (backend.rs) — GPU-readiness seam.

pub mod assets;
pub mod backend;
pub mod cache;
pub mod cmd;
pub mod cog;
pub mod config;
pub mod decode;
pub mod expr;
pub mod layer;
pub mod legend;
pub mod mvt_http;
pub mod pngio;
pub mod render;
pub mod reproj;
pub mod s3;
pub mod server;
pub mod sld;
pub mod style;
pub mod tms;
pub mod tms_http;
pub mod vector;
pub mod wms;
pub mod wmts;
pub mod xml;

/// Top-level error. The agent may replace this with a richer error type.
pub type Error = Box<dyn std::error::Error>;

// The frozen crate-root contract: `main.rs` names these types and functions directly, and
// `score.sh` drives the binary by the flags they define. Moving them into `cmd/` must not move
// them out of the crate root.
pub use cmd::pmtiles::{run_build_pmtiles, BuildPmtilesArgs};
pub use cmd::render::{run_render, run_wms_handle, RenderArgs, WmsArgs};
pub use cmd::serve::{run_serve, ServeArgs};
pub use cmd::topology::{run_build_topology, BuildTopologyArgs};
