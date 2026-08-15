// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! PyO3 bindings for the TerraServe render core.
//!
//! v1 exposes the **raster** map path only: `render_png` mirrors the CLI `render`
//! subcommand (build a `RenderRequest`, run the engine, PNG-encode) but returns the
//! encoded bytes to Python instead of writing a file. The vector + label path
//! (`render_vector_png`, wrapping `vector::render::render_vector_from` + the `Shaper`
//! label engine) is the decided fast-follow — see the design spec.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use terraserve::{backend::Resample, cache, pngio, render, reproj, style::Style};

/// Render a COG window to PNG bytes.
///
/// `cog_path` is a local path or `s3://bucket/key`. `bbox` is `[minx, miny, maxx, maxy]`
/// in `crs` units. `src_crs` defaults to the engine's built-in source CRS. `resample` is
/// `"bilinear"` (default) or `"nearest"`. The GIL is released for the whole render so
/// concurrent callers (e.g. pygeoapi workers) parallelise instead of serialising.
#[pyfunction]
#[pyo3(signature = (cog_path, bbox, crs, width, height, style, src_crs=None, resample=None))]
#[allow(clippy::too_many_arguments)]
fn render_png<'py>(
    py: Python<'py>,
    cog_path: String,
    bbox: [f64; 4],
    crs: String,
    width: u32,
    height: u32,
    style: String,
    src_crs: Option<String>,
    resample: Option<String>,
) -> PyResult<Bound<'py, PyBytes>> {
    let resample = match resample.as_deref().unwrap_or("bilinear").to_ascii_lowercase().as_str() {
        "nearest" => Resample::Nearest,
        "bilinear" => Resample::Bilinear,
        other => {
            return Err(PyValueError::new_err(format!(
                "unknown resample '{other}' (expected 'nearest' or 'bilinear')"
            )))
        }
    };
    let src_crs = src_crs.unwrap_or_else(|| reproj::SRC_CRS.to_string());

    // CPU-bound render off the GIL: nothing inside touches Python.
    let png = py
        .allow_threads(move || -> Result<Vec<u8>, String> {
            let sty = Style::load(&style)?;
            let req = render::RenderRequest {
                cog_path: &cog_path,
                bbox,
                crs: &crs,
                src_crs: &src_crs,
                width,
                height,
                resample,
                style: &sty,
                band_math: None,
                index_cache: cache::new_index_cache(cache::index_cache_bytes()),
            };
            let rgba = render::render(&req)?;
            pngio::encode_rgba(&rgba, width, height)
        })
        .map_err(PyValueError::new_err)?;

    Ok(PyBytes::new(py, &png))
}

/// The compiled core module — imported as `terraserve._terraserve`, re-exported from
/// the pure-Python `terraserve/__init__.py`.
#[pymodule]
fn _terraserve(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(render_png, m)?)?;
    Ok(())
}
