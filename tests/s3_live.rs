//! Gated live-S3 integration test.
//!
//! SKIPS cleanly (no network I/O, no creds needed) unless `TERRASERVE_S3_TEST_URL` is set —
//! CI without secrets stays green; run it locally / wherever secrets exist to actually
//! exercise a live S3-compatible endpoint end to end.
//!
//! Set `TERRASERVE_S3_TEST_URL=s3://bucket/key` (+ `AWS_ENDPOINT_URL` / `AWS_REGION` /
//! `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`) to run.

use terraserve::s3::{S3Config, S3RangeSource};

#[test]
fn live_read_whole_and_404_on_missing_sibling_key() {
    let url = match std::env::var("TERRASERVE_S3_TEST_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!(
                "skipping live_read_whole_and_404_on_missing_sibling_key: \
                 TERRASERVE_S3_TEST_URL not set"
            );
            return;
        }
    };

    let cfg = S3Config::from_env();

    // 1. The real object opens and reads whole, non-empty.
    let src = S3RangeSource::open(&url, &cfg).expect("open live s3 url");
    let bytes = src.read_whole().expect("read_whole on live s3 url");
    assert!(!bytes.is_empty(), "expected a non-empty object at {url}");

    // 2. A deliberately-missing sibling key 404s with a message naming the key/status.
    //    S3RangeSource::signed_get's ureq::Error::Status arm emits
    //    "s3 GET {key} -> {code}: {body}" — tolerant of the exact body, just checks the
    //    not-found shape (a 4xx status) is present.
    let missing_url = format!("{url}.nonexistent-xyz");
    let missing_src = S3RangeSource::open(&missing_url, &cfg).expect("open missing sibling url");
    let err = missing_src
        .read_whole()
        .expect_err("expected the missing sibling key to error")
        .to_string();
    assert!(
        err.contains("-> 4") || err.contains("NoSuchKey") || err.contains("404"),
        "expected a not-found style S3 error, got: {err}"
    );
}
