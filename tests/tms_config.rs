//! Config wiring for tile grids: serde defaults + preset/custom grid resolution + validation.

use std::collections::BTreeMap;

use terraserve::config::{resolve_grids_presets, Config, GridConfig, LayerConfig};

#[test]
fn layer_grid_defaults_materialize_from_serde() {
    // Omitting `grids`/`tile_px` must yield the defaults (from_cog @ 512) — not a struct literal
    // (which wouldn't exercise the serde default fns).
    let lc: LayerConfig = serde_yaml::from_str("name: x\ncog: /a.tif\nstyle: s.json\n").unwrap();
    assert_eq!(lc.grids, vec!["from_cog".to_string()]);
    assert_eq!(lc.tile_px, 512);
}

#[test]
fn resolve_named_presets_and_size_suffix() {
    let custom = BTreeMap::<String, GridConfig>::new();
    let grids = resolve_grids_presets(
        &["WebMercatorQuad".into(), "UPSArcticWGS84Quad".into()],
        256,
        &custom,
    )
    .unwrap();
    assert_eq!(grids[0].id, "WebMercatorQuad");
    assert_eq!(grids[1].crs, "EPSG:5041");
    // A size suffix pins the size regardless of the tile_px arg.
    let g = resolve_grids_presets(&["WebMercatorQuad_256".into()], 512, &custom).unwrap();
    assert_eq!(g[0].id, "WebMercatorQuad");
    // Unknown id with no custom entry -> error.
    assert!(resolve_grids_presets(&["Nope".into()], 256, &custom).is_err());
    // from_cog cannot resolve without a COG.
    assert!(resolve_grids_presets(&["from_cog".into()], 256, &custom).is_err());
}

#[test]
fn custom_grid_resolves_and_rejects_non_indexable() {
    // A dyadic, level-invariant custom grid parses and resolves (power-of-two values, exact in f64).
    let yaml = r#"
layers:
  - name: a
    cog: a.tif
    style: s.json
    grids: [mygrid]
grids:
  mygrid:
    crs: EPSG:3857
    origin: [0.0, 256.0]
    extent: [0.0, 0.0, 256.0, 256.0]
    tile_px: 256
    resolutions: [1.0, 0.5, 0.25]
"#;
    let cfg: Config = serde_yaml::from_str(yaml).unwrap();
    let g = resolve_grids_presets(&cfg.layers[0].grids, cfg.layers[0].tile_px, &cfg.grids).unwrap();
    assert_eq!(g[0].id, "mygrid");
    assert_eq!(g[0].levels.len(), 3);
    assert!(g[0].is_level_invariant());

    // A NON-dyadic ladder over a fixed extent is not TMS-indexable -> resolve fails loudly.
    let bad_yaml = r#"
layers: []
grids:
  bad:
    crs: EPSG:3857
    origin: [0.0, 256.0]
    extent: [0.0, 0.0, 256.0, 256.0]
    tile_px: 256
    resolutions: [1.0, 0.4, 0.1]
"#;
    let bad: Config = serde_yaml::from_str(bad_yaml).unwrap();
    assert!(resolve_grids_presets(&["bad".into()], 256, &bad.grids).is_err());
}
