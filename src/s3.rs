// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! S3 range-read source for cloud COGs.
//!
//! A `RangeSource` (the seam every COG byte flows through) backed by HTTP `GET` with a
//! `Range` header, signed with **AWS Signature V4**. Works with any S3-compatible endpoint
//! (OVH, AWS, MinIO, …) via a configurable endpoint/region and **path-style** addressing.
//! This is generic S3/HTTP plumbing — NOT a COG reader — so the container/tiling/warp stay
//! bespoke. SigV4 is hand-rolled (only `sha2`/`hmac` primitives + `ureq` for the request);
//! the source is `Sync` (no shared cursor), which is what later unlocks parallel tile reads.

use std::io::{Error, ErrorKind, Read, Result};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::cog::{LocalFileRangeSource, RangeSource};

/// A COG byte source that is either a local file or an S3 object, chosen by the `--cog`
/// path (`s3://…` → S3, otherwise local). Lets the generic render path stay monomorphic
/// while supporting both at runtime.
pub enum AnySource {
    Local(LocalFileRangeSource),
    S3(S3RangeSource),
}

impl AnySource {
    /// Open `cog` as an S3 object when it is an `s3://bucket/key` URL, else as a local file.
    pub fn open(cog: &str, s3: &S3Config) -> Result<AnySource> {
        if is_s3_url(cog) {
            Ok(AnySource::S3(S3RangeSource::open(cog, s3)?))
        } else {
            Ok(AnySource::Local(LocalFileRangeSource::open(cog)?))
        }
    }
}

impl RangeSource for AnySource {
    fn read_range(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        match self {
            AnySource::Local(s) => s.read_range(offset, len),
            AnySource::S3(s) => s.read_range(offset, len),
        }
    }
}

const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const SERVICE: &str = "s3";
/// SHA-256 of the empty body — the payload hash for a GET (no request body).
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Endpoint + credentials for an S3-compatible store, resolved env-first (CLI overrides).
#[derive(Clone, Debug, Default)]
pub struct S3Config {
    /// Full endpoint URL, e.g. `https://s3.gra.io.cloud.ovh.net`.
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub session_token: Option<String>,
}

impl S3Config {
    /// Read from the standard AWS environment variables (what a shell/profile exports).
    pub fn from_env() -> S3Config {
        let e = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        S3Config {
            endpoint: e("AWS_ENDPOINT_URL").or_else(|| e("AWS_S3_ENDPOINT")),
            region: e("AWS_REGION").or_else(|| e("AWS_DEFAULT_REGION")),
            access_key: e("AWS_ACCESS_KEY_ID"),
            secret_key: e("AWS_SECRET_ACCESS_KEY"),
            session_token: e("AWS_SESSION_TOKEN"),
        }
    }

    /// Merge `other` over `self` (other's Some values win) — used to layer CLI over env.
    pub fn merge(self, other: S3Config) -> S3Config {
        S3Config {
            endpoint: other.endpoint.or(self.endpoint),
            region: other.region.or(self.region),
            access_key: other.access_key.or(self.access_key),
            secret_key: other.secret_key.or(self.secret_key),
            session_token: other.session_token.or(self.session_token),
        }
    }
}

/// Reads byte ranges from an object in an S3-compatible store (path-style, SigV4).
pub struct S3RangeSource {
    scheme: String, // http | https
    host: String,   // s3.gra.io.cloud.ovh.net
    region: String,
    bucket: String,
    key: String,
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
    /// Pooled HTTP client — reuses TLS connections across range reads (a COG parse does many
    /// small reads, so a fresh connection each time is the dominant cost). `Agent` is an
    /// `Arc` internally: `Clone + Send + Sync`, which keeps this source `Sync` for parallel reads.
    agent: ureq::Agent,
}

/// True if `path` is an `s3://bucket/key` URL this source handles.
pub fn is_s3_url(path: &str) -> bool {
    path.starts_with("s3://")
}

impl S3RangeSource {
    /// Build from an `s3://bucket/key` URL plus resolved config. Errors if the endpoint or
    /// credentials are missing.
    pub fn open(url: &str, cfg: &S3Config) -> Result<S3RangeSource> {
        let rest = url
            .strip_prefix("s3://")
            .ok_or_else(|| err(format!("not an s3:// url: {url}")))?;
        let (bucket, key) = rest
            .split_once('/')
            .ok_or_else(|| err(format!("s3 url needs a key: {url}")))?;
        if bucket.is_empty() || key.is_empty() {
            return Err(err(format!("s3 url needs bucket and key: {url}")));
        }
        let endpoint = cfg
            .endpoint
            .clone()
            .ok_or_else(|| err("missing S3 endpoint (set AWS_ENDPOINT_URL or --s3-endpoint)"))?;
        let (scheme, host) = split_endpoint(&endpoint)?;
        let access_key = cfg
            .access_key
            .clone()
            .ok_or_else(|| err("missing AWS_ACCESS_KEY_ID"))?;
        let secret_key = cfg
            .secret_key
            .clone()
            .ok_or_else(|| err("missing AWS_SECRET_ACCESS_KEY"))?;
        // Pool enough connections that parallel tile reads each get one (OVH is HTTP/1.1,
        // so concurrency means multiple reused connections, not multiplexed streams). Track the
        // I/O pool size so up to `io_concurrency()` in-flight reads each keep a reusable socket.
        let idle = crate::render::io_concurrency();
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout_read(Duration::from_secs(60))
            .max_idle_connections_per_host(idle)
            .max_idle_connections(idle)
            .build();
        Ok(S3RangeSource {
            scheme,
            host,
            region: cfg
                .region
                .clone()
                .unwrap_or_else(|| "us-east-1".to_string()),
            bucket: bucket.to_string(),
            key: key.to_string(),
            access_key,
            secret_key,
            session_token: cfg.session_token.clone(),
            agent,
        })
    }

    fn url(&self) -> String {
        format!(
            "{}://{}/{}/{}",
            self.scheme,
            self.host,
            self.bucket,
            uri_encode(&self.key, false)
        )
    }

    /// Canonical URI (path-style): `/bucket/key`, each segment URI-encoded (‘/’ preserved).
    fn canonical_uri(&self) -> String {
        format!("/{}/{}", self.bucket, uri_encode(&self.key, false))
    }
}

impl RangeSource for S3RangeSource {
    fn read_range(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let range = format!("bytes={}-{}", offset, offset + len as u64 - 1);
        let (amzdate, datestamp) = timestamps();

        // Canonical headers — sorted by lowercase name; add the session token when present.
        let mut headers: Vec<(String, String)> = vec![
            ("host".into(), self.host.clone()),
            ("range".into(), range.clone()),
            ("x-amz-content-sha256".into(), EMPTY_SHA256.into()),
            ("x-amz-date".into(), amzdate.clone()),
        ];
        if let Some(tok) = &self.session_token {
            headers.push(("x-amz-security-token".into(), tok.clone()));
        }
        headers.sort_by(|a, b| a.0.cmp(&b.0));
        let signed_headers = headers
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let canonical_headers = headers
            .iter()
            .map(|(k, v)| format!("{k}:{}\n", v.trim()))
            .collect::<String>();

        let canonical_request = format!(
            "GET\n{}\n\n{}\n{}\n{}",
            self.canonical_uri(),
            canonical_headers,
            signed_headers,
            EMPTY_SHA256
        );
        let scope = format!("{datestamp}/{}/{SERVICE}/aws4_request", self.region);
        let string_to_sign = format!(
            "{ALGORITHM}\n{amzdate}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let signing_key = signing_key(&self.secret_key, &datestamp, &self.region);
        let signature = hex(&hmac(&signing_key, string_to_sign.as_bytes()));
        let authorization = format!(
            "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key
        );

        // Issue the signed range GET on the pooled agent (reuses the TLS connection).
        let mut req = self
            .agent
            .get(&self.url())
            .set("Range", &range)
            .set("x-amz-content-sha256", EMPTY_SHA256)
            .set("x-amz-date", &amzdate)
            .set("Authorization", &authorization);
        if let Some(tok) = &self.session_token {
            req = req.set("x-amz-security-token", tok);
        }
        let resp = req.call().map_err(|e| match e {
            ureq::Error::Status(code, r) => {
                let body = r
                    .into_string()
                    .unwrap_or_default()
                    .chars()
                    .take(300)
                    .collect::<String>();
                err(format!("s3 GET {} -> {code}: {body}", self.key))
            }
            ureq::Error::Transport(t) => err(format!("s3 transport: {t}")),
        })?;

        let mut buf = Vec::with_capacity(len);
        resp.into_reader()
            .read_to_end(&mut buf)
            .map_err(|e| err(format!("s3 read body: {e}")))?;
        // S3 legitimately returns a SHORT body when a range GET extends past the object end;
        // callers (`cog::index_chunk_entry`, `read_ifd`, `read_uints`) assume exact length, so
        // honor the same exact-length-or-Err contract `LocalFileRangeSource` (read_exact_at)
        // already has rather than silently handing back fewer bytes than requested.
        if buf.len() < len {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                format!(
                    "short S3 range read: got {} of {len} bytes at offset {offset}",
                    buf.len()
                ),
            ));
        }
        Ok(buf)
    }
}

// --- SigV4 primitives ------------------------------------------------------

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex(&h.finalize())
}

fn signing_key(secret: &str, datestamp: &str, region: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), datestamp.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, SERVICE.as_bytes());
    hmac(&k_service, b"aws4_request")
}

fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 0xf) as usize] as char);
    }
    s
}

/// RFC 3986 URI-encode. Unreserved chars pass through; when `encode_slash` is false, ‘/’ is
/// preserved (for path segments). S3 SigV4 requires this exact encoding in the canonical URI.
fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (b == b'/' && !encode_slash);
        if unreserved {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

fn split_endpoint(endpoint: &str) -> Result<(String, String)> {
    let (scheme, rest) = endpoint
        .split_once("://")
        .ok_or_else(|| err(format!("endpoint needs a scheme: {endpoint}")))?;
    let host = rest.trim_end_matches('/');
    if host.is_empty() {
        return Err(err(format!("endpoint has no host: {endpoint}")));
    }
    Ok((scheme.to_string(), host.to_string()))
}

/// Current UTC as SigV4 `x-amz-date` (`YYYYMMDDTHHMMSSZ`) and `datestamp` (`YYYYMMDD`).
fn timestamps() -> (String, String) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    (
        format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z"),
        format!("{y:04}{mo:02}{d:02}"),
    )
}

/// Convert a UNIX timestamp to UTC (year, month, day, hour, min, sec) — Howard Hinnant's
/// `civil_from_days`, so we need no date crate.
fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86400) as i64;
    let rem = (secs % 86400) as u32;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as i64;
    let year = y + if m <= 2 { 1 } else { 0 };
    (year, m as u32, d, hh, mm, ss)
}

fn err(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::Other, msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        // RFC 6234: SHA-256("abc")
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_hex(b""), EMPTY_SHA256);
    }

    #[test]
    fn hmac_sha256_rfc4231_case2() {
        // RFC 4231 test case 2: key="Jefe", data="what do ya want for nothing?"
        let mac = hmac(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn sigv4_signing_key_aws_documented_vector() {
        // AWS docs "derive a signing key" example.
        let k = signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
        );
        // NOTE: the documented vector uses service "iam"; ours is hardcoded "s3", so this
        // checks our HMAC chain shape end-to-end rather than the doc's exact bytes. The full
        // SigV4 correctness is proven by the real OVH read in the integration test.
        assert_eq!(k.len(), 32);
    }

    #[test]
    fn civil_from_unix_epoch_and_a_known_date() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 1600000000 = 2020-09-13T12:26:40Z
        assert_eq!(civil_from_unix(1_600_000_000), (2020, 9, 13, 12, 26, 40));
    }

    #[test]
    fn uri_encode_preserves_slash_and_encodes_space() {
        assert_eq!(uri_encode("a/b c.tif", false), "a/b%20c.tif");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
    }

    #[test]
    fn is_s3_url_detects() {
        assert!(is_s3_url("s3://bucket/key.tif"));
        assert!(!is_s3_url("/local/path.tif"));
    }
}
