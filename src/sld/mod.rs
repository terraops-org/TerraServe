// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! SLD (Styled Layer Descriptor) support — a self-contained, rendering-agnostic module.
//!
//! **Hard constraint:** everything under `src/sld/` may depend on `std` (and, from Task 2,
//! `roxmltree`) ONLY — no imports from the rest of this crate. This module knows nothing about
//! the renderer; it is a faithful SLD/SE document model + parser. The renderer-facing lowering pass
//! (`vector::sld_lower`, Task 6) is the one place that bridges this AST into the engine's own
//! Style IR (`vector::style::Style`).
//!
//! Layout:
//!   * `model` — the document AST (Task 1).
//!   * `filter` — the `<ogc:Filter>` AST (Task 1) + parser (Task 3).
//!   * `parse` — `roxmltree` → `model` (Task 2).
//!   * `xml` — generic `roxmltree` helpers shared by `parse` + `filter` (SIMP-7).

pub mod filter;
pub mod model;
pub mod parse;
mod xml;

pub use model::*;

/// Parse-error type for the whole `sld` module. Deliberately minimal (a message string) — callers
/// that need structure can match on `.0`.
#[derive(Clone, Debug, PartialEq)]
pub struct SldError(pub String);

/// Parse an SLD 1.0.0 XML document into the document AST.
pub fn parse(xml: &str) -> Result<StyledLayerDescriptor, SldError> {
    parse::parse(xml)
}
