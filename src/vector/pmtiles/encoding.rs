// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Tile content encodings: identity, gzip, brotli, zstd. One type for both places an encoding
//! lives: the PMTiles header (`tile_compression`, byte 98) and the HTTP `Content-Encoding`.
//!
//! Levels are measured, not guessed (2026-09-16, three real tiles: vida z12 246 KB, eu5 roads z5
//! 913 KB, cos2023 z9 8.1 MB, against the gzip-6 shipped in 0.3.2):
//! - LIVE brotli 5: 13 %, 1 % and 4 % smaller than gzip-6, and faster to produce on all three
//!   (5.3 vs 8.7 ms, 31 vs 54 ms, 407 vs 580 ms). brotli 11 would be 20-40x slower per request.
//! - LIVE zstd 3: 3-6 % BIGGER than gzip-6 but 10-20x faster; the choice for CPU, not bytes.
//! - ARCHIVE zstd 19: 7-14 % smaller than gzip-6, paid once at bake time (192 ms on the eu5 tile,
//!   3.4 s on cos2023). The default archive level; brotli 11 is 15-19 % smaller but 1.3 s and 13 s.
//! - ARCHIVE brotli 11 and gzip 6 remain selectable.

use std::io::{Read, Write};

use super::PmResult;

/// A tile's content encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TileEncoding {
    Identity,
    Gzip,
    Brotli,
    Zstd,
}

/// Same bomb cap as `codec::gunzip`: a tiny crafted blob must not decompress into an
/// allocation that aborts the process. Far above any legitimate MVT tile.
const MAX_DECODED: u64 = 256 * 1024 * 1024;

/// Compression effort for one call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    /// Serving a tile now: brotli 5, zstd 3, gzip 6 (gzip 6 is what 0.3.2 served).
    Live,
    /// Writing an archive once: brotli 11, zstd 19, gzip 6 (gzip 6 is what every archive held).
    Archive,
}

impl TileEncoding {
    /// PMTiles v3 `tile_compression` byte (1 none, 2 gzip, 3 brotli, 4 zstd).
    pub fn to_pmtiles(self) -> u8 {
        match self {
            TileEncoding::Identity => 1,
            TileEncoding::Gzip => 2,
            TileEncoding::Brotli => 3,
            TileEncoding::Zstd => 4,
        }
    }

    /// From the archive header. `0` (unknown) and anything unassigned is an error: brotli has no
    /// magic bytes, so the header is the only honest source of what a blob is.
    pub fn from_pmtiles(b: u8) -> PmResult<Self> {
        match b {
            1 => Ok(TileEncoding::Identity),
            2 => Ok(TileEncoding::Gzip),
            3 => Ok(TileEncoding::Brotli),
            4 => Ok(TileEncoding::Zstd),
            other => Err(format!(
                "pmtiles: tile_compression {other} is not supported \
                 (1 = none, 2 = gzip, 3 = brotli, 4 = zstd)"
            )),
        }
    }

    /// The HTTP content-coding token, `None` for identity (no `Content-Encoding` header).
    pub fn http_token(self) -> Option<&'static str> {
        match self {
            TileEncoding::Identity => None,
            TileEncoding::Gzip => Some("gzip"),
            TileEncoding::Brotli => Some("br"),
            TileEncoding::Zstd => Some("zstd"),
        }
    }

    /// Parse a CLI value: `gzip`, `br`/`brotli`, `zstd`, `none`/`identity`.
    pub fn parse_cli(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gzip" => Ok(TileEncoding::Gzip),
            "br" | "brotli" => Ok(TileEncoding::Brotli),
            "zstd" => Ok(TileEncoding::Zstd),
            "none" | "identity" => Ok(TileEncoding::Identity),
            other => Err(format!(
                "unknown tile encoding '{other}' (expected gzip, br, zstd or none)"
            )),
        }
    }

    /// Compress raw MVT bytes. `level` overrides the effort's default when given.
    pub fn compress(self, raw: &[u8], effort: Effort, level: Option<i32>) -> Vec<u8> {
        match self {
            TileEncoding::Identity => raw.to_vec(),
            TileEncoding::Gzip => {
                let l = level.unwrap_or(6).clamp(0, 9) as u32;
                let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(l));
                e.write_all(raw).expect("gzip write");
                e.finish().expect("gzip finish")
            }
            TileEncoding::Brotli => {
                let q = level
                    .unwrap_or(match effort {
                        Effort::Live => 5,
                        Effort::Archive => 11,
                    })
                    .clamp(0, 11) as u32;
                let mut out = Vec::new();
                {
                    // lgwin 22 is brotli's default window (4 MiB).
                    let mut w = brotli::CompressorWriter::new(&mut out, 4096, q, 22);
                    w.write_all(raw).expect("brotli write");
                }
                out
            }
            TileEncoding::Zstd => {
                let l = level
                    .unwrap_or(match effort {
                        Effort::Live => 3,
                        Effort::Archive => 19,
                    })
                    .clamp(1, 22);
                zstd::encode_all(raw, l).expect("zstd encode")
            }
        }
    }

    /// Decompress to raw bytes, capped against decompression bombs.
    pub fn decompress(self, bytes: &[u8]) -> PmResult<Vec<u8>> {
        decompress_capped(self, bytes, MAX_DECODED)
    }
}

/// `decompress` with an explicit cap, for tests. Reads at most `max_out + 1` bytes so an
/// over-limit stream is detected without buffering it; zstd's declared frame size is not trusted.
pub(crate) fn decompress_capped(
    enc: TileEncoding,
    bytes: &[u8],
    max_out: u64,
) -> PmResult<Vec<u8>> {
    let mut out = Vec::new();
    let name = enc.http_token().unwrap_or("identity");
    let res = match enc {
        TileEncoding::Identity => {
            out.extend_from_slice(bytes);
            Ok(0)
        }
        TileEncoding::Gzip => flate2::read::GzDecoder::new(bytes)
            .take(max_out + 1)
            .read_to_end(&mut out),
        TileEncoding::Brotli => brotli::Decompressor::new(bytes, 4096)
            .take(max_out + 1)
            .read_to_end(&mut out),
        TileEncoding::Zstd => match zstd::stream::read::Decoder::new(bytes) {
            Ok(d) => d.take(max_out + 1).read_to_end(&mut out),
            Err(e) => Err(e),
        },
    };
    res.map_err(|e| format!("{name} decode: {e}"))?;
    if out.len() as u64 > max_out {
        return Err(format!(
            "{name} decode: decompressed size exceeds cap {max_out}"
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [TileEncoding; 4] = [
        TileEncoding::Identity,
        TileEncoding::Gzip,
        TileEncoding::Brotli,
        TileEncoding::Zstd,
    ];

    fn sample() -> Vec<u8> {
        (0..50_000u32)
            .flat_map(|i| (i % 251).to_le_bytes())
            .collect()
    }

    #[test]
    fn every_encoding_round_trips_at_both_efforts() {
        let raw = sample();
        for enc in ALL {
            for effort in [Effort::Live, Effort::Archive] {
                let c = enc.compress(&raw, effort, None);
                assert_eq!(enc.decompress(&c).unwrap(), raw, "{enc:?} {effort:?}");
                if enc != TileEncoding::Identity {
                    assert!(c.len() < raw.len(), "{enc:?} {effort:?} did not compress");
                }
            }
        }
    }

    #[test]
    fn pmtiles_byte_round_trips_and_rejects_unknown() {
        for enc in ALL {
            assert_eq!(TileEncoding::from_pmtiles(enc.to_pmtiles()).unwrap(), enc);
        }
        assert!(TileEncoding::from_pmtiles(0).is_err());
        assert!(TileEncoding::from_pmtiles(5).is_err());
        // The spec's numbering, which other readers depend on.
        assert_eq!(TileEncoding::Brotli.to_pmtiles(), 3);
        assert_eq!(TileEncoding::Zstd.to_pmtiles(), 4);
    }

    #[test]
    fn a_decompression_bomb_is_refused_for_every_codec() {
        let raw = vec![0u8; 1_000_000];
        for enc in [TileEncoding::Gzip, TileEncoding::Brotli, TileEncoding::Zstd] {
            let c = enc.compress(&raw, Effort::Live, None);
            assert!(c.len() < 10_000, "{enc:?} should shrink zeros");
            let err = decompress_capped(enc, &c, 100_000).unwrap_err();
            assert!(err.contains("exceeds cap"), "{enc:?}: {err}");
            assert_eq!(
                decompress_capped(enc, &c, 1_000_000).unwrap().len(),
                1_000_000
            );
        }
    }

    #[test]
    fn a_blob_in_the_wrong_encoding_is_an_error_not_garbage() {
        let gz = TileEncoding::Gzip.compress(&sample(), Effort::Live, None);
        assert!(TileEncoding::Zstd.decompress(&gz).is_err());
        assert!(TileEncoding::Brotli.decompress(&gz).is_err());
    }

    #[test]
    fn cli_names_parse() {
        assert_eq!(TileEncoding::parse_cli("br").unwrap(), TileEncoding::Brotli);
        assert_eq!(
            TileEncoding::parse_cli("Brotli").unwrap(),
            TileEncoding::Brotli
        );
        assert_eq!(TileEncoding::parse_cli("zstd").unwrap(), TileEncoding::Zstd);
        assert_eq!(TileEncoding::parse_cli("gzip").unwrap(), TileEncoding::Gzip);
        assert_eq!(
            TileEncoding::parse_cli("none").unwrap(),
            TileEncoding::Identity
        );
        assert!(TileEncoding::parse_cli("lz4").is_err());
    }
}
