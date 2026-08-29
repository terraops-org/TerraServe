// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Encode-time optimization knobs for one MVT layer — the composition seam for the MVT
//! optimization passes (min-feature-size selection, the budget/safety cap, grid-snap vertex dedup,
//! and the cell mosaic). Built once per request by [`MvtOptimizations::for_layer`], then threaded
//! by `&` through `encode_tile_opt`. Keeping every knob here (and deriving per-ZOOM values inside
//! the encoder from the tile's `z`) is what lets the MVT and WMTS routes produce identical bytes
//! with a single derivation site — see the design spec
//! `docs/superpowers/specs/2026-07-14-mvt-optimization-pipeline-design.md`.

use crate::server::{ServeState, VectorLayer};

/// The optimization knobs for one layer's MVT encode. NOT `Copy` (carries the cell-field name);
/// passed by `&`. The encoder derives all per-zoom values (`min_area_src`, cell geometry) from
/// these plus the tile `z`.
#[derive(Clone, Debug)]
pub struct MvtOptimizations {
    /// Uniform-sample cap per tile (0 = unlimited). The legacy budget pass / safety limit.
    pub max_features: usize,
    /// Grid-snap consecutive-vertex dedup (Phase 1b). DEFAULT-ON; `--no-optimizations` clears it.
    pub dedup: bool,
    /// Per-zoom seam-free min feature size, display-px² (0 = off). Opt-in `--mvt-min-feature-px`.
    pub min_feature_px: f64,
    /// Per-zoom min feature LENGTH steps for line geometry, display-px, as ascending
    /// `(from_zoom, value)` pairs; empty = off. The mirror of `min_feature_px` for lines, and a
    /// STEP LIST rather than a value-plus-band because a road network needs the opposite shape to a
    /// building layer: strong at overview zoom, weak deep in. See
    /// [`MvtOptimizations::min_feature_len_px_at`]. Opt-in `--mvt-min-feature-len-px`.
    pub min_feature_len_px: Vec<(u32, f64)>,
    /// `min_feature_px` applies only at `z >= min_feature_min_zoom` (per-ZOOM constant, so seam-safe,
    /// the same argument as `cell_max_zoom`). `0` = every zoom, which is what every config without
    /// `--mvt-min-feature-px-min-zoom` resolves to. See [`MvtOptimizations::min_feature_px_at`].
    pub min_feature_min_zoom: u32,
    /// Mercator-m² per source-unit² for this layer (precomputed on the layer). Used by the
    /// min-feature-size pass ONLY to turn `min_feature_px` into a source-CRS area threshold at a
    /// given `z`. `0.0` = not computable → that pass disables (fail-OPEN). The cell mosaic works in
    /// tile-4096 units and ignores it.
    pub area_scale: f64,
    /// Cell-mosaic size in tile-4096 units (`16·N` for the rounded power-of-2 `N`; 0 = mosaic off).
    pub cell_units: u32,
    /// The thematic class field the mosaic votes on (validated at load against the layer schema).
    /// `None` disables the mosaic even if `cell_units > 0`.
    pub cell_field: Option<String>,
    /// Mosaic active only at `z <= cell_max_zoom` (per-ZOOM constant → seam-safe). 0 = every zoom.
    pub cell_max_zoom: u32,
    /// The class field the same-class dissolve pass merges by (`--mvt-dissolve`, validated per layer).
    /// `None` = off. Mutually exclusive with the cell mosaic — dissolve wins (nulls the cell fields).
    pub dissolve_field: Option<String>,
    /// Dissolve active only at `z <= dissolve_max_zoom` (per-ZOOM constant → seam-safe). 0 = every zoom.
    pub dissolve_max_zoom: u32,
}

impl MvtOptimizations {
    /// The configured `--mvt-min-feature-px` AS IT APPLIES AT ZOOM `z`: the value itself inside the
    /// band, `0.0` (read as "gate off" by every consumer) below it.
    ///
    /// The band exists because `min_feature_px` is denominated in display-px² and a pixel's ground
    /// footprint QUARTERS with every zoom in, while building sizes do not. One global value
    /// therefore cannot serve both ends of a z0-z10 pyramid: measured on EU5 buildings over the
    /// Paris column, `0.03` leaves 125,156 candidates on the worst z8 tile (under any sane feature
    /// cap, so the cap never samples and no density seam forms) but fewer than 100 on z3, where its
    /// threshold is 144,839 m² -- larger than any building on Earth. Banding it to `z >= 6` keeps
    /// the deep-zoom fix and leaves z0-z5 on the always-on cell floor, which already bounds those
    /// tiles at 7k-48k candidates.
    ///
    /// Returning `0.0` rather than a separate `Option` is deliberate: `min_area_src_for_grid`,
    /// `features_for_tile`'s pushdown test and `size_gate_sql` all already treat a non-positive
    /// threshold as "off", so the band needs no new off-switch anywhere downstream.
    ///
    /// Per-ZOOM constant, so every tile at a given zoom makes the identical keep/drop decision --
    /// the seam-free property this whole gate is built on is preserved.
    pub fn min_feature_px_at(&self, z: u32) -> f64 {
        if z >= self.min_feature_min_zoom {
            self.min_feature_px
        } else {
            0.0
        }
    }

    /// The configured `--mvt-min-feature-len-px` AS IT APPLIES AT ZOOM `z`: the value of the last
    /// step at or below `z`, or `0.0` ("gate off", the reading every consumer already gives a
    /// non-positive threshold) when no step covers it.
    ///
    /// A step LIST, where the area gate got a value plus a one-sided band, because line layers want
    /// the opposite shape. Measured on the EU5 roads table over the Paris column: a road network
    /// has 24.2M candidate lines in one z2 tile and 39,924 in one z10 tile, so the gate has to be
    /// AGGRESSIVE at overview zoom (2 px keeps 950 at z2) and GENTLE deep in (0.3 px keeps 29,583
    /// at z10). One value cannot do both, and a `min_zoom` band can only turn one value on or off.
    ///
    /// Per-ZOOM constant like every other gate here, so still seam-free by construction.
    pub fn min_feature_len_px_at(&self, z: u32) -> f64 {
        self.min_feature_len_px
            .iter()
            .rev()
            .find(|(from, _)| z >= *from)
            .map_or(0.0, |(_, v)| *v)
    }

    /// The default set: dedup on, no size/cell generalization, the default feature budget. Produces
    /// byte-identical output to today's `encode_tile` default path.
    pub fn defaults() -> Self {
        MvtOptimizations {
            max_features: super::DEFAULT_MAX_FEATURES_PER_TILE,
            dedup: true,
            min_feature_px: 0.0,
            min_feature_len_px: Vec::new(),
            min_feature_min_zoom: 0,
            area_scale: 0.0,
            cell_units: 0,
            cell_field: None,
            cell_max_zoom: 0,
            dissolve_field: None,
            dissolve_max_zoom: 0,
        }
    }

    /// The single construction site both the MVT and WMTS routes call — zero duplicated derivation.
    /// Reads `area_scale` off the (pre-computed) layer, resolves the cell-field against THIS layer's
    /// schema, and maps the server flags to knobs via the pure [`MvtOptimizations::resolve`].
    pub fn for_layer(state: &ServeState, layer: &VectorLayer) -> Self {
        Self::resolve(
            state.mvt_max_features,
            state.mvt_no_safety_limit,
            state.mvt_no_optimizations,
            state.mvt_min_feature_px,
            state.mvt_min_feature_len_px.clone(),
            state.mvt_min_feature_min_zoom,
            layer.area_scale,
            cell_units(state.mvt_cell_px),
            resolve_cell_field(&state.mvt_cell_field, &layer.fields),
            state.mvt_cell_max_zoom,
            resolve_cell_field(&state.mvt_dissolve_field, &layer.fields),
            state.mvt_dissolve_max_zoom,
        )
    }

    /// Pure flag→knob mapping (no `ServeState`/`VectorLayer` — unit-testable in isolation).
    /// `--no-safety-limit` forces the cap off (unlimited) and wins over `max_features`;
    /// `--no-optimizations` clears the default-on dedup pass. **Dissolve and the cell mosaic are
    /// mutually exclusive — dissolve wins** (nulls the cell fields). Everything else passes through.
    #[allow(clippy::too_many_arguments)]
    fn resolve(
        max_features: usize,
        no_safety_limit: bool,
        no_optimizations: bool,
        min_feature_px: f64,
        min_feature_len_px: Vec<(u32, f64)>,
        min_feature_min_zoom: u32,
        area_scale: f64,
        cell_units: u32,
        cell_field: Option<String>,
        cell_max_zoom: u32,
        dissolve_field: Option<String>,
        dissolve_max_zoom: u32,
    ) -> Self {
        // Mutual exclusion: dissolve (true boundaries) wins over the cell mosaic (squares).
        let (cell_units, cell_field) = if dissolve_field.is_some() {
            (0, None)
        } else {
            (cell_units, cell_field)
        };
        MvtOptimizations {
            max_features: if no_safety_limit { 0 } else { max_features },
            dedup: !no_optimizations,
            min_feature_px,
            min_feature_len_px,
            min_feature_min_zoom,
            area_scale,
            cell_units,
            cell_field,
            cell_max_zoom,
            dissolve_field,
            dissolve_max_zoom,
        }
    }
}

/// Parse `--mvt-min-feature-len-px` into ascending `(from_zoom, value)` steps.
///
/// Two accepted forms, because most layers want one number and a road network wants two:
/// * a bare number -- `"2"` means `[(0, 2.0)]`, the gate at every zoom;
/// * a comma list of `zoom:value` -- `"0:2.0,7:0.3"` means 2 px from z0 and 0.3 px from z7 up.
///
/// An empty string is the off switch (no steps). A step's value may be `0`, which reads as "gate
/// off from this zoom up" -- the only way to express a gate that stops applying deep in.
///
/// Rejects anything ambiguous rather than guessing: a malformed pair, a duplicate zoom, a negative
/// or non-finite value. A silently-misparsed gate would bake a wrong pyramid for hours.
pub fn parse_len_px_spec(spec: &str) -> Result<Vec<(u32, f64)>, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok(Vec::new());
    }
    if !spec.contains(':') {
        let v: f64 = spec
            .parse()
            .map_err(|_| format!("--mvt-min-feature-len-px: {spec:?} is not a number"))?;
        if !v.is_finite() || v < 0.0 {
            return Err(format!(
                "--mvt-min-feature-len-px: {v} must be finite and >= 0"
            ));
        }
        return Ok(vec![(0, v)]);
    }
    let mut out: Vec<(u32, f64)> = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        let (z, v) = part
            .split_once(':')
            .ok_or_else(|| format!("--mvt-min-feature-len-px: {part:?} is not `zoom:value`"))?;
        let z: u32 = z
            .trim()
            .parse()
            .map_err(|_| format!("--mvt-min-feature-len-px: {z:?} is not a zoom"))?;
        let v: f64 = v
            .trim()
            .parse()
            .map_err(|_| format!("--mvt-min-feature-len-px: {v:?} is not a number"))?;
        if !v.is_finite() || v < 0.0 {
            return Err(format!(
                "--mvt-min-feature-len-px: {v} must be finite and >= 0"
            ));
        }
        if out.iter().any(|(zz, _)| *zz == z) {
            return Err(format!("--mvt-min-feature-len-px: zoom {z} listed twice"));
        }
        out.push((z, v));
    }
    out.sort_by_key(|(z, _)| *z);
    Ok(out)
}

/// The mosaic cell size in tile-4096 units for `--mvt-cell-px N`: `0` (mosaic off) when `cell_px`
/// is not positive, else `16·N` for `N` = `cell_px` rounded to the nearest power of two in
/// `{4..=256}`. The power-of-two constraint makes `16·N` divide 4096, so cells align to tile edges
/// (seam-safe); the floor of 4 caps a tile at `(256/4)² = 4096` cells + inverse-projections, the
/// ceiling of 256 keeps at least one cell per tile.
pub fn cell_units(cell_px: f64) -> u32 {
    if !(cell_px > 0.0) {
        return 0;
    }
    // Round to the nearest power of two via log2, then clamp to [4, 256]. `INFINITY as u32`
    // saturates to u32::MAX in Rust, which the clamp pins to 256 — so huge inputs are safe.
    let n = 2f64.powi(cell_px.log2().round() as i32);
    let n = (n as u32).clamp(4, 256);
    16 * n
}

/// Resolve the requested cell-field name against a layer's field schema: `Some(name)` if the layer
/// carries that attribute, else `None` (mosaic disabled for that layer — real geometry renders).
/// Per-request (called by `for_layer`), so it does NOT warn; the load-time code warns once per layer.
fn resolve_cell_field(
    requested: &Option<String>,
    fields: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    requested
        .as_ref()
        .filter(|f| fields.contains_key(f.as_str()))
        .cloned()
}

/// Startup validation for the cell-mosaic flags: `--mvt-cell-px` needs a `--mvt-cell-field` (the
/// thematic attribute to vote on — the encoder has no default notion of "the class").
pub fn validate_cell_flags(cell_px: f64, cell_field: &Option<String>) -> Result<(), String> {
    if cell_px > 0.0 && cell_field.is_none() {
        return Err(
            "--mvt-cell-px requires --mvt-cell-field <name> (the thematic class attribute to vote on)"
                .to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::MvtOptimizations;

    #[test]
    fn defaults_match_todays_path() {
        let d = MvtOptimizations::defaults();
        assert!(d.dedup, "dedup must default ON");
        assert_eq!(d.max_features, super::super::DEFAULT_MAX_FEATURES_PER_TILE);
        assert_eq!(d.min_feature_px, 0.0);
        assert_eq!(d.min_feature_min_zoom, 0, "no band = every zoom");
        assert!(d.min_feature_len_px.is_empty(), "no length steps = off");
        assert_eq!(d.area_scale, 0.0);
        assert_eq!(d.cell_units, 0);
        assert!(d.cell_field.is_none());
        assert_eq!(d.cell_max_zoom, 0);
    }

    /// The per-zoom STEP LIST for `--mvt-min-feature-len-px`. Roads need the opposite shape to
    /// buildings -- a STRONG gate at overview zoom and a weak one deep in -- so a single value plus
    /// a one-sided band cannot express it. A step list can express either.
    #[test]
    fn min_feature_len_px_at_walks_the_zoom_steps() {
        let o = MvtOptimizations {
            min_feature_len_px: vec![(0, 2.0), (7, 0.3)],
            ..MvtOptimizations::defaults()
        };
        assert_eq!(o.min_feature_len_px_at(0), 2.0);
        assert_eq!(
            o.min_feature_len_px_at(6),
            2.0,
            "last step at or below 6 is z0"
        );
        assert_eq!(o.min_feature_len_px_at(7), 0.3, "z7 is its own step");
        assert_eq!(o.min_feature_len_px_at(10), 0.3);

        // A list that does not start at 0 leaves the lower zooms with NO configured gate.
        let late = MvtOptimizations {
            min_feature_len_px: vec![(5, 1.0)],
            ..MvtOptimizations::defaults()
        };
        assert_eq!(late.min_feature_len_px_at(4), 0.0);
        assert_eq!(late.min_feature_len_px_at(5), 1.0);

        // Default = empty = off at every zoom.
        assert_eq!(MvtOptimizations::defaults().min_feature_len_px_at(0), 0.0);
    }

    /// The CLI spec parser: a bare number means every zoom; a comma list is `zoom:value` steps.
    #[test]
    fn parse_len_px_spec_accepts_a_bare_number_and_a_step_list() {
        use super::parse_len_px_spec as p;
        assert_eq!(p("").unwrap(), vec![]);
        assert_eq!(p("2").unwrap(), vec![(0, 2.0)]);
        assert_eq!(p("2.0").unwrap(), vec![(0, 2.0)]);
        assert_eq!(p("0:2.0,7:0.3").unwrap(), vec![(0, 2.0), (7, 0.3)]);
        // Out of order is sorted, whitespace tolerated.
        assert_eq!(p(" 7:0.3 , 0:2 ").unwrap(), vec![(0, 2.0), (7, 0.3)]);
        // A zero value is a legitimate step: "gate OFF from here up".
        assert_eq!(p("0:2,8:0").unwrap(), vec![(0, 2.0), (8, 0.0)]);
        for bad in ["x", "0:", ":2", "0:2,0:3", "-1:2", "0:-2", "0:nan", "1:2:3"] {
            assert!(p(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    /// The zoom BAND for `--mvt-min-feature-px`. EU5 buildings needs the gate at z6-z10 (where a
    /// dense metro tile has 470k candidates and the feature cap would otherwise sample, seaming) and
    /// NOT at z0-z5 (where a px2-denominated threshold quarters per zoom until it exceeds any
    /// building on Earth and blanks the overview). Below the first banded zoom the configured value
    /// must read as "off" so `min_area_src_for_grid` falls back to its always-on cell floor.
    #[test]
    fn min_feature_px_at_respects_the_zoom_band() {
        let banded = MvtOptimizations {
            min_feature_px: 0.03,
            min_feature_min_zoom: 6,
            ..MvtOptimizations::defaults()
        };
        assert_eq!(banded.min_feature_px_at(5), 0.0, "z5 is below the band");
        assert_eq!(
            banded.min_feature_px_at(6),
            0.03,
            "z6 is the first banded zoom"
        );
        assert_eq!(
            banded.min_feature_px_at(10),
            0.03,
            "and it stays on above it"
        );

        // 0 = every zoom, which is what every pre-band config resolves to.
        let unbanded = MvtOptimizations {
            min_feature_px: 0.03,
            min_feature_min_zoom: 0,
            ..MvtOptimizations::defaults()
        };
        assert_eq!(unbanded.min_feature_px_at(0), 0.03);
        assert_eq!(unbanded.min_feature_px_at(10), 0.03);

        // A band without a gate value stays off -- the band selects zooms, it never turns a gate on.
        let no_gate = MvtOptimizations {
            min_feature_px: 0.0,
            min_feature_min_zoom: 6,
            ..MvtOptimizations::defaults()
        };
        assert_eq!(no_gate.min_feature_px_at(10), 0.0);
    }

    #[test]
    fn resolve_passes_knobs_through_by_default() {
        // no off-switches → cap kept, dedup on, knobs copied verbatim.
        let o = MvtOptimizations::resolve(
            5000,
            false,
            false,
            2.0,
            Vec::new(),
            0,
            1.5,
            128,
            Some("c".into()),
            6,
            None,
            0,
        );
        assert_eq!(o.max_features, 5000);
        assert!(o.dedup);
        assert_eq!(o.min_feature_px, 2.0);
        assert_eq!(o.area_scale, 1.5);
        assert_eq!(o.cell_units, 128);
        assert_eq!(o.cell_field.as_deref(), Some("c"));
        assert_eq!(o.cell_max_zoom, 6);
    }

    #[test]
    fn resolve_no_optimizations_clears_dedup_only() {
        // `--no-optimizations` clears dedup but leaves an opt-in min_feature_px in force.
        let o = MvtOptimizations::resolve(
            5000,
            false,
            true,
            2.0,
            Vec::new(),
            0,
            1.5,
            0,
            None,
            0,
            None,
            0,
        );
        assert!(!o.dedup, "no_optimizations must clear dedup");
        assert_eq!(o.min_feature_px, 2.0, "opt-in selection stays independent");
        assert_eq!(
            o.max_features, 5000,
            "safety cap untouched by no_optimizations"
        );
    }

    #[test]
    fn resolve_no_safety_limit_forces_unlimited() {
        // `--no-safety-limit` forces the cap to 0 (unlimited) even over a finite configured cap.
        let o = MvtOptimizations::resolve(
            5000,
            true,
            false,
            0.0,
            Vec::new(),
            0,
            0.0,
            0,
            None,
            0,
            None,
            0,
        );
        assert_eq!(o.max_features, 0, "no_safety_limit forces unlimited");
        let on = MvtOptimizations::resolve(
            5000,
            false,
            false,
            0.0,
            Vec::new(),
            0,
            0.0,
            0,
            None,
            0,
            None,
            0,
        );
        assert_eq!(on.max_features, 5000, "off = the configured cap");
    }

    #[test]
    fn resolve_dissolve_wins_over_cell_mosaic() {
        // Both passes requested → dissolve (true boundaries) wins, the cell mosaic is nulled.
        let o = MvtOptimizations::resolve(
            5000,
            false,
            false,
            0.0,
            Vec::new(),
            0,
            0.0,
            128,
            Some("c".into()),
            6,
            Some("d".into()),
            8,
        );
        assert_eq!(o.cell_units, 0, "cell mosaic disabled by dissolve");
        assert!(o.cell_field.is_none());
        assert_eq!(o.dissolve_field.as_deref(), Some("d"));
        assert_eq!(o.dissolve_max_zoom, 8);
    }

    #[test]
    fn cell_units_off_when_not_positive() {
        assert_eq!(super::cell_units(0.0), 0);
        assert_eq!(super::cell_units(-4.0), 0);
        assert_eq!(super::cell_units(f64::NAN), 0);
    }

    #[test]
    fn cell_units_rounds_to_power_of_two_times_16() {
        assert_eq!(super::cell_units(8.0), 128, "N=8 → 16·8");
        assert_eq!(super::cell_units(16.0), 256, "N=16 → 16·16");
        assert_eq!(
            super::cell_units(10.0),
            128,
            "nearest power of 2 to 10 is 8"
        );
    }

    #[test]
    fn cell_units_floors_at_4_and_caps_at_256() {
        assert_eq!(super::cell_units(1.0), 64, "floor N=4 → 16·4");
        assert_eq!(super::cell_units(3.0), 64, "rounds toward the floor of 4");
        assert_eq!(super::cell_units(1000.0), 4096, "cap N=256 → 16·256");
    }

    #[test]
    fn resolve_cell_field_present_absent_and_none() {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("COS18_n4_C".to_string(), "String".to_string());
        // Present on the layer → Some; absent → None (mosaic disabled here); no request → None.
        assert_eq!(
            super::resolve_cell_field(&Some("COS18_n4_C".into()), &fields).as_deref(),
            Some("COS18_n4_C")
        );
        assert_eq!(
            super::resolve_cell_field(&Some("nope".into()), &fields),
            None
        );
        assert_eq!(super::resolve_cell_field(&None, &fields), None);
    }

    #[test]
    fn validate_cell_flags_requires_a_field() {
        assert!(
            super::validate_cell_flags(8.0, &None).is_err(),
            "cell-px needs a field"
        );
        assert!(super::validate_cell_flags(8.0, &Some("c".into())).is_ok());
        assert!(
            super::validate_cell_flags(0.0, &None).is_ok(),
            "mosaic off → no field needed"
        );
    }
}
