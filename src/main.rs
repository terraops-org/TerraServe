// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! TerraServe pilot CLI.
//!
//! The subcommand names and flags below are a FROZEN external contract — the
//! scoring harness (`score.sh`) invokes the binary exactly as documented in
//! SPEC-AGENT.md. Do not rename or remove them.

use clap::{Parser, Subcommand};

// jemalloc global allocator (opt-in via `--features jemalloc`). glibc keeps freed memory in-process
// (RSS = high-water mark), so a single whole-extent request leaves the pod inflated; jemalloc hands
// dirty pages back to the OS. `malloc_conf` enables the background purge thread + ~1s decay so RSS
// falls back within a second or two of a spike subsiding. No-op in the default (glibc) build.
//
// The `not(target_env = "msvc")` half matters just as much as the feature check: Cargo.toml puts
// `tikv-jemallocator` under a target-scoped `[target.'cfg(not(target_env = "msvc"))'.dependencies]`
// table, but that ONLY removes the crate from MSVC's dependency graph — it does not turn off
// `cfg(feature = "jemalloc")`, which stays true on every platform because `jemalloc` is a default
// feature and Cargo resolves features independently of per-target dependency tables. Gating on the
// feature alone left `tikv_jemallocator::Jemalloc` referenced here with nothing to link against on
// MSVC ("cannot find crate `tikv_jemallocator`") — the actual root cause behind
// terraops-org/TerraServe#9, found only once Windows CI got far enough to hit it.
#[cfg(all(feature = "jemalloc", not(target_env = "msvc")))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(all(feature = "jemalloc", not(target_env = "msvc")))]
#[allow(non_upper_case_globals)]
#[export_name = "malloc_conf"]
pub static malloc_conf: &[u8] = b"background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:1000\0";

#[derive(Parser)]
#[command(
    name = "terraserve",
    version,
    about = "TerraServe pilot: bespoke COG -> WMS raster slice"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Render a window of a COG to a PNG (the engine core).
    Render(terraserve::RenderArgs),
    /// Answer a one-shot WMS request (GetMap / GetCapabilities / exception) to stdout.
    WmsHandle(terraserve::WmsArgs),
    /// Run the live HTTP WMS server (axum/tokio).
    Serve(terraserve::ServeArgs),
    /// Build shared-arc topology from a vector coverage and print a report (diagnostic; no tiles).
    BuildTopology(terraserve::BuildTopologyArgs),
    /// Bake an offline `.pmtiles` vector pyramid from a vector source (PMTiles task 6).
    BuildPmtiles(terraserve::BuildPmtilesArgs),
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Render(args) => terraserve::run_render(&args),
        Cmd::WmsHandle(args) => terraserve::run_wms_handle(&args),
        Cmd::Serve(args) => terraserve::run_serve(&args),
        Cmd::BuildTopology(args) => terraserve::run_build_topology(&args),
        Cmd::BuildPmtiles(args) => terraserve::run_build_pmtiles(&args),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
