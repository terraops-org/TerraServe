// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! One module per CLI verb: its clap argument struct plus the orchestrator that runs it.
//!
//! These used to live together in `lib.rs`, which grew to 2321 lines doing four unrelated jobs —
//! argument definitions, command orchestration, layer construction, and format dispatch. Splitting
//! by verb means a change to one subcommand touches one file, and a new subcommand adds a file
//! instead of growing the crate root.
//!
//! ⚠ **The argument structs are a FROZEN external contract.** `score.sh` invokes the binary by
//! these flags and `main.rs` names the types at the crate root, so every one of them is
//! re-exported unchanged from `lib.rs`. Move them; do not rename or reshape them.

pub mod pmtiles;
pub mod render;
pub mod serve;
pub mod topology;
