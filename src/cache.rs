// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Bounded decoded-tile cache for the async server.
//!
//! Decoded RGBA tiles are keyed by `(level index, tile index)` — unique per tile for a
//! single COG. The cache has a HARD memory ceiling: each entry's weight is its decoded
//! byte length, and `max_capacity` caps the total resident weight, so RSS stays bounded
//! no matter the load (moka evicts the least-valuable tiles; TinyLFU + LRU).
//!
//! This is the ONE deliberate, bounded exception to the engine's "no accumulating caches"
//! rule: a warm tile is served with zero read and zero decode, yet total cache memory can
//! never exceed the cap and is reclaimed — unlike a heap-accumulating server.
//!
//! (Serving multiple COGs later would fold a COG id into the key.)

use std::sync::Arc;

use crate::backend::DeviceBuffer;

/// `(level index, tile index within that level)`.
pub type TileKey = (u32, u32);

/// Decoded RGBA tiles, shared across requests. `moka::sync::Cache` is internally `Arc`-ed,
/// `Clone`, `Send + Sync` — store it by value in the shared server state.
pub type TileCache = moka::sync::Cache<TileKey, Arc<DeviceBuffer>>;

/// Default hard cap: ~256 MiB of decoded tiles.
pub const DEFAULT_CAP_BYTES: u64 = 256 * 1024 * 1024;

/// Build a tile cache with a hard `cap_bytes` memory ceiling. The weigher is each tile's
/// decoded byte length, so total resident tile bytes never exceed `cap_bytes`.
pub fn new_tile_cache(cap_bytes: u64) -> TileCache {
    moka::sync::Cache::builder()
        .max_capacity(cap_bytes)
        .weigher(|_k: &TileKey, v: &Arc<DeviceBuffer>| v.data.len().min(u32::MAX as usize) as u32)
        .build()
}

/// Build the server's tile cache from CLI options: `None` (caching off) when `no_cache`
/// is set or the cap is zero, otherwise a cache with a hard `cap_mb` MiB ceiling.
pub fn from_options(no_cache: bool, cap_mb: u64) -> Option<TileCache> {
    if no_cache || cap_mb == 0 {
        None
    } else {
        Some(new_tile_cache(cap_mb * 1024 * 1024))
    }
}

/// Bounded chunk cache for lazily-read tile-index slabs (`cog::TileIndex::Lazy`).
///
/// `(source_id, array file offset, chunk number) -> decoded u64 entries of that chunk`. Keyed
/// per-COG (`source_id`) + per-array (`offsets_off`/`bytecounts_off` differ) so a hit only ever
/// answers the exact chunk it was built from. Weight = decoded bytes (`entries.len() * 8`), so
/// total resident chunk bytes stay hard-bounded like every other cache in this module.
///
/// Cheap when unused: an all-`Resident` COG (today's only path — a future task adds the
/// lazy-open threshold) never calls into `cog::index_chunk_entry`, so this cache never grows.
pub type IndexChunkKey = (u64, u64, u64);
pub type IndexCache = moka::sync::Cache<IndexChunkKey, Arc<Vec<u64>>>;

/// Build an index-chunk cache with a hard `cap_bytes` memory ceiling.
pub fn new_index_cache(cap_bytes: u64) -> IndexCache {
    moka::sync::Cache::builder()
        .max_capacity(cap_bytes)
        .weigher(|_k: &IndexChunkKey, v: &Arc<Vec<u64>>| {
            (v.len().saturating_mul(8)).min(u32::MAX as usize) as u32
        })
        .build()
}

/// Default `IndexCache` cap: env `TERRASERVE_INDEX_CACHE_BYTES`, default 64 MiB.
pub fn index_cache_bytes() -> u64 {
    std::env::var("TERRASERVE_INDEX_CACHE_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(64 * 1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(bytes: usize) -> Arc<DeviceBuffer> {
        // A square-ish RGBA buffer whose data length is ~`bytes`.
        let px = (bytes / 4).max(1);
        let side = (px as f64).sqrt() as u32;
        Arc::new(DeviceBuffer::new(side.max(1), side.max(1), 4))
    }

    #[test]
    fn cache_stays_bounded() {
        // Cap of 1 MiB; insert far more than that in ~256 KiB tiles.
        let cap = 1024 * 1024;
        let cache = new_tile_cache(cap);
        for i in 0..64u32 {
            cache.insert((0, i), tile(256 * 1024));
        }
        // Force pending eviction/maintenance, then assert the hard ceiling holds.
        cache.run_pending_tasks();
        assert!(
            cache.weighted_size() <= cap,
            "cache exceeded its cap: {} > {cap}",
            cache.weighted_size()
        );
        assert!(cache.entry_count() < 64, "nothing was evicted");
    }

    #[test]
    fn hit_returns_same_tile() {
        let cache = new_tile_cache(DEFAULT_CAP_BYTES);
        let t = tile(256 * 1024);
        cache.insert((3, 7), t.clone());
        let got = cache.get(&(3, 7)).expect("expected a hit");
        assert!(Arc::ptr_eq(&got, &t), "cache returned a different Arc");
        assert!(
            cache.get(&(3, 8)).is_none(),
            "unexpected hit for absent key"
        );
    }

    #[test]
    fn from_options_toggles_and_honours_cap() {
        assert!(from_options(true, 256).is_none(), "--no-cache must disable");
        assert!(from_options(false, 0).is_none(), "zero cap must disable");

        let c = from_options(false, 1).expect("non-zero cap must enable");
        for i in 0..64u32 {
            c.insert((0, i), tile(256 * 1024));
        }
        c.run_pending_tasks();
        assert!(
            c.weighted_size() <= 1024 * 1024,
            "configured 1 MiB cap not honoured: {}",
            c.weighted_size()
        );
    }
}
