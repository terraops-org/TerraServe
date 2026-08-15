// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Reading the small operator-supplied files a layer is configured with.
//!
//! An "asset" here is a startup input that is NOT the data: a `style.json`, an SLD, a font, a
//! `--mvt-style`, a layer YAML. They share one property that the data does not — they are small
//! and read WHOLE, exactly once, before the server starts serving. So unlike a COG or an FGB
//! they need no `RangeSource`, no windowing and no cache; a single unconditional GET is the
//! right shape, and `s3://` support is just a branch on the scheme.
//!
//! This lives in its own module because its three callers sit in three different places —
//! `cmd::serve` (style, `--mvt-style`), `layer` (vector style, font) and the pmtiles builder —
//! so it cannot belong to any one of them.
//!
//! **The error format is pinned by tests:** `"{path}: {cause}"`, with the path FIRST and no
//! verb prefix. Call sites compose their own context on top (`format!("font {e}")`), so a verb
//! here would read as "font open /x: ..." at the call site.

use crate::s3;
use crate::Error;

/// Read a small startup config asset (style / font / mvt-style) from a local path OR an `s3://`
/// URL. Whole-object fetch (not windowed) — S3 uses one no-Range GET.
pub(crate) fn read_config_bytes(path: &str, s3: &s3::S3Config) -> Result<Vec<u8>, Error> {
    if s3::is_s3_url(path) {
        Ok(s3::S3RangeSource::open(path, s3)
            .map_err(|e| format!("{path}: {e}"))?
            .read_whole()
            .map_err(|e| format!("{path}: {e}"))?)
    } else {
        std::fs::read(path).map_err(|e| format!("{path}: {e}").into())
    }
}

pub(crate) fn read_config_string(path: &str, s3: &s3::S3Config) -> Result<String, Error> {
    if s3::is_s3_url(path) {
        String::from_utf8(read_config_bytes(path, s3)?)
            .map_err(|e| format!("{path}: not valid UTF-8: {e}").into())
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}").into())
    }
}

#[cfg(test)]
mod read_config_tests {
    use super::{read_config_bytes, read_config_string};

    #[test]
    fn read_config_string_local_matches_fs() {
        // A local path must load byte-for-byte the same via read_config_string as via std::fs.
        let path = "fixtures/styles/rgb.json"; // an existing committed style fixture
        let s3 = crate::s3::S3Config::from_env();
        let via_helper = read_config_string(path, &s3).expect("read_config_string local");
        let via_fs = std::fs::read_to_string(path).expect("fs read");
        assert_eq!(via_helper, via_fs);
    }

    #[test]
    fn read_config_string_missing_local_path_errors_with_path() {
        let path = "fixtures/does/not/exist-read-config-string.json";
        let s3 = crate::s3::S3Config::default();
        let err = read_config_string(path, &s3).unwrap_err().to_string();
        assert!(
            err.contains(path),
            "expected the path in the error, got: {err}"
        );
    }

    #[test]
    fn read_config_bytes_missing_local_path_errors_with_path() {
        let path = "fixtures/does/not/exist-read-config-bytes.bin";
        let s3 = crate::s3::S3Config::default();
        let err = read_config_bytes(path, &s3).unwrap_err().to_string();
        assert!(
            err.contains(path),
            "expected the path in the error, got: {err}"
        );
    }

    #[test]
    fn read_config_bytes_missing_local_path_error_has_no_verb_prefix() {
        // Pins the "{path}: {cause}" format (no "open "/"read " verb) — the message must
        // start with the path itself, not a verb.
        let path = "fixtures/does/not/exist-no-verb.bin";
        let s3 = crate::s3::S3Config::default();
        let err = read_config_bytes(path, &s3).unwrap_err().to_string();
        assert!(
            err.starts_with(&format!("{path}: ")),
            "expected '{path}: ...', got: {err}"
        );
    }

    #[test]
    fn read_config_bytes_s3_without_endpoint_errors_before_any_network_call() {
        // S3Config::default() has NO endpoint, so S3RangeSource::open must fail on
        // validation before issuing any HTTP request — proves the s3 arm surfaces a
        // clean, path-prefixed error with no network I/O. Runs unconditionally (no
        // TERRASERVE_S3_TEST_URL / creds needed).
        let path = "s3://some-bucket/some/key.json";
        let s3 = crate::s3::S3Config::default();
        let err = read_config_bytes(path, &s3).unwrap_err().to_string();
        assert!(
            err.starts_with(&format!("{path}: ")),
            "expected '{path}: ...', got: {err}"
        );
        assert!(
            err.contains("missing S3 endpoint"),
            "expected the missing-endpoint cause, got: {err}"
        );
    }

    #[test]
    fn font_prefix_composes_with_read_config_bytes_error() {
        // The "font " prefix is added at the build_vector_layer call site, which needs a
        // live serve() to reach — so this pins the composition directly, the way run_serve
        // actually builds it: format!("font {e}") over read_config_bytes's error.
        let path = "/no/such/font.ttf";
        let s3 = crate::s3::S3Config::default();
        let composed = format!("font {}", read_config_bytes(path, &s3).unwrap_err());
        assert!(
            composed.contains(&format!("font {path}:")),
            "expected 'font {path}: ...', got: {composed}"
        );
    }

    #[test]
    fn mvt_style_prefix_composes_with_read_config_string_error() {
        // Same idea for the --mvt-style call site in run_serve.
        let path = "/no/such/style.json";
        let s3 = crate::s3::S3Config::default();
        let composed = format!("--mvt-style {}", read_config_string(path, &s3).unwrap_err());
        assert!(
            composed.contains(&format!("--mvt-style {path}:")),
            "expected '--mvt-style {path}: ...', got: {composed}"
        );
    }
}
