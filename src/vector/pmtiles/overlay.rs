// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! PMTiles write-through overlay (append-log + index). `TileOverlay` layers a crash-recoverable
//! append-log (record codec + CRC32 from write-through task 2) over an optional swappable base
//! `PmtilesReader` — last-writer-wins puts, lock-free positioned log reads, and torn-tail recovery
//! on open (write-through task 3).

use crate::vector::pmtiles::codec::gunzip;
use crate::vector::pmtiles::read::PmtilesReader;
use crate::vector::pmtiles::write::{Counts, HeaderFields, PmtilesWriter};
use crate::vector::pmtiles::zxy_to_tileid;
use crate::vector::pmtiles::PmResult;
use std::collections::{BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) const LOG_MAGIC: &[u8; 8] = b"TSPMTLOG";
pub(crate) const LOG_VERSION: u8 = 1;
pub(crate) const LOG_HEADER_LEN: u64 = 16;

/// IEEE CRC32 (reflected), table built once.
fn crc32_table() -> &'static [u32; 256] {
    use std::sync::OnceLock;
    static T: OnceLock<[u32; 256]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u32; 256];
        let mut n = 0usize;
        while n < 256 {
            let mut c = n as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xEDB88320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            t[n] = c;
            n += 1;
        }
        t
    })
}
pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    let t = crc32_table();
    let mut c: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// Append `tile_id(8 LE) | len(4 LE) | crc(4 LE) | blob` to `buf`.
pub(crate) fn write_record(buf: &mut Vec<u8>, tile_id: u64, blob: &[u8]) {
    buf.extend_from_slice(&tile_id.to_le_bytes());
    buf.extend_from_slice(&(blob.len() as u32).to_le_bytes());
    buf.extend_from_slice(&crc32(blob).to_le_bytes());
    buf.extend_from_slice(blob);
}

/// Parse the record at `pos`. Returns (tile_id, blob_offset, blob_len, next_pos), or None if the
/// record is torn (runs past EOF) or its CRC mismatches — the caller stops scanning there.
pub(crate) fn parse_record(bytes: &[u8], pos: usize) -> Option<(u64, usize, usize, usize)> {
    if pos + 16 > bytes.len() {
        return None;
    }
    let tile_id = u64::from_le_bytes(bytes[pos..pos + 8].try_into().ok()?);
    let len = u32::from_le_bytes(bytes[pos + 8..pos + 12].try_into().ok()?) as usize;
    let crc = u32::from_le_bytes(bytes[pos + 12..pos + 16].try_into().ok()?);
    let blob_off = pos + 16;
    let next = blob_off.checked_add(len)?;
    if next > bytes.len() {
        return None; // torn tail
    }
    if crc32(&bytes[blob_off..next]) != crc {
        return None; // corrupt
    }
    Some((tile_id, blob_off, len, next))
}

struct OverlayInner {
    log_w: File,                     // append handle
    index: HashMap<u64, (u64, u32)>, // tile_id -> (blob_offset, blob_len)
    log_len: u64,
    base: Option<Arc<PmtilesReader>>,
    compacting: bool,
}

/// Write-through overlay: an append-only crash-recoverable log of superseding tile records,
/// layered over an optional read-only base `PmtilesReader`. Puts are last-writer-wins; gets check
/// the in-RAM index first, then fall through to the base. Log reads use positioned `read_at`
/// (lock-free, `&self`) so readers never contend with the writer's append lock.
pub struct TileOverlay {
    path: PathBuf,
    log_r: File, // read handle (read_at, lock-free)
    inner: Mutex<OverlayInner>,
    /// Compact-when-exceeded cap in bytes (0 = off). `put` wakes the compaction controller when the
    /// log grows past it. Atomic so the sync `put` path checks it without taking the inner lock.
    max_bytes: AtomicU64,
    /// Wake channel for the per-layer compaction controller (write-through task 6): the size-cap in
    /// `put` and the `/mvt/{layer}/flush` route `notify_one()` it; the controller awaits
    /// `compactor_woken()`. `notify_one` latches one permit even with no waiter, so a wake fired
    /// before the controller parks is not lost.
    compact_notify: tokio::sync::Notify,
    /// The layer's proper PMTiles metadata JSON (carries `vector_layers`), set by the caller after
    /// `open` via `set_metadata` and written on every compaction. Defaults to `"{}"` so an overlay
    /// nobody configures still compacts to something readable, just without `vector_layers`.
    layer_metadata: Mutex<String>,
}

impl TileOverlay {
    /// Open (or create) the log at `log_path`. On an existing log, replays records to rebuild the
    /// in-RAM index and truncates any torn tail (a crash mid-append) so the file ends on a clean
    /// record boundary.
    pub fn open(log_path: &Path, base: Option<Arc<PmtilesReader>>) -> PmResult<TileOverlay> {
        let exists = log_path.exists();
        if !exists {
            let mut f = File::create(log_path).map_err(|e| format!("overlay create: {e}"))?;
            let mut hdr = Vec::with_capacity(LOG_HEADER_LEN as usize);
            hdr.extend_from_slice(LOG_MAGIC);
            hdr.push(LOG_VERSION);
            hdr.extend_from_slice(&[0u8; 7]);
            f.write_all(&hdr)
                .map_err(|e| format!("overlay header: {e}"))?;
        }
        let mut index = HashMap::new();
        let valid_len;
        {
            let bytes = std::fs::read(log_path).map_err(|e| format!("overlay read: {e}"))?;
            if bytes.len() < LOG_HEADER_LEN as usize
                || &bytes[0..8] != LOG_MAGIC
                || bytes[8] != LOG_VERSION
            {
                return Err("overlay: bad log header".into());
            }
            let mut pos = LOG_HEADER_LEN as usize;
            while let Some((tile_id, blob_off, blob_len, next)) = parse_record(&bytes, pos) {
                index.insert(tile_id, (blob_off as u64, blob_len as u32));
                pos = next;
            }
            valid_len = pos as u64;
            if valid_len < bytes.len() as u64 {
                // truncate the torn tail
                let f = OpenOptions::new()
                    .write(true)
                    .open(log_path)
                    .map_err(|e| format!("overlay open-w: {e}"))?;
                f.set_len(valid_len)
                    .map_err(|e| format!("overlay truncate: {e}"))?;
            }
        }
        let log_w = OpenOptions::new()
            .append(true)
            .open(log_path)
            .map_err(|e| format!("overlay open-append: {e}"))?;
        let log_r = OpenOptions::new()
            .read(true)
            .open(log_path)
            .map_err(|e| format!("overlay open-read: {e}"))?;
        Ok(TileOverlay {
            path: log_path.to_path_buf(),
            log_r,
            inner: Mutex::new(OverlayInner {
                log_w,
                index,
                log_len: valid_len,
                base,
                compacting: false,
            }),
            max_bytes: AtomicU64::new(0),
            compact_notify: tokio::sync::Notify::new(),
            layer_metadata: Mutex::new("{}".to_string()),
        })
    }

    /// Set the layer metadata JSON (should carry `vector_layers`) written on every compaction.
    pub fn set_metadata(&self, m: String) {
        *self.layer_metadata.lock().unwrap() = m;
    }

    /// The currently-set layer metadata JSON (`"{}"` until `set_metadata` is called).
    pub fn metadata_str(&self) -> String {
        self.layer_metadata.lock().unwrap().clone()
    }

    /// Append a gzip'd tile blob under `tile_id`, superseding any prior entry for that id.
    pub fn put(&self, tile_id: u64, gzipped: &[u8]) -> PmResult<()> {
        let mut rec = Vec::with_capacity(gzipped.len() + 16);
        write_record(&mut rec, tile_id, gzipped);
        let log_len = {
            let mut inner = self.inner.lock().map_err(|_| "overlay lock poisoned")?;
            // Atomic with the compaction check: if a compaction owns this overlay, SKIP the
            // append+index under the same lock that `try_begin_compaction`/`clear` hold. The tile was
            // already served fresh to the client and re-encodes deterministically on the next request
            // (self-healing), so dropping this put is safe — and it can no longer add a record that a
            // concurrent `clear()` would then truncate (the lost-put window). Safe because the
            // `--vector` source is immutable per-process and the MVT encode is deterministic.
            if inner.compacting {
                return Ok(());
            }
            let blob_off = inner.log_len + 16; // record = 16-byte prefix then blob
            inner
                .log_w
                .write_all(&rec)
                .map_err(|e| format!("overlay append: {e}"))?;
            inner.log_len += rec.len() as u64;
            inner
                .index
                .insert(tile_id, (blob_off, gzipped.len() as u32));
            inner.log_len
        };
        // Size-cap trigger (task 6): once the log passes the configured cap, wake the compaction
        // controller so it folds the overlay back into the base and truncates the log. `notify_one`
        // is sync-callable (and latches a permit even with no waiter), so `put` stays sync.
        let cap = self.max_bytes.load(Ordering::Relaxed);
        if cap > 0 && log_len > cap {
            self.compact_notify.notify_one();
        }
        Ok(())
    }

    /// overlay-only lookup by TileID (gunzip'd), for the compactor + tests.
    pub fn get_by_id(&self, tile_id: u64) -> PmResult<Option<Vec<u8>>> {
        let (off, len) = {
            let inner = self.inner.lock().map_err(|_| "overlay lock poisoned")?;
            match inner.index.get(&tile_id) {
                Some(&v) => v,
                None => return Ok(None),
            }
        };
        let mut buf = vec![0u8; len as usize];
        self.log_r
            .read_exact_at(&mut buf, off)
            .map_err(|e| format!("overlay read_exact_at: {e}"))?;
        Ok(Some(gunzip(&buf)?))
    }

    /// overlay-only lookup returning the STORED gzip blob (no gunzip), for the compactor.
    pub fn get_by_id_raw(&self, tile_id: u64) -> PmResult<Option<Vec<u8>>> {
        let (off, len) = {
            let inner = self.inner.lock().map_err(|_| "overlay lock poisoned")?;
            match inner.index.get(&tile_id) {
                Some(&v) => v,
                None => return Ok(None),
            }
        };
        let mut buf = vec![0u8; len as usize];
        self.log_r
            .read_exact_at(&mut buf, off)
            .map_err(|e| format!("overlay read_exact_at: {e}"))?;
        Ok(Some(buf))
    }

    /// Overlay index first (gunzip'd); on a miss, falls through to the base reader if present.
    pub fn get(&self, z: u32, x: u32, y: u32) -> PmResult<Option<Vec<u8>>> {
        let id = zxy_to_tileid(z, x, y);
        if let Some(b) = self.get_by_id(id)? {
            return Ok(Some(b));
        }
        let base = {
            self.inner
                .lock()
                .map_err(|_| "overlay lock poisoned")?
                .base
                .clone()
        };
        match base {
            Some(r) => r.get(z, x, y),
            None => Ok(None),
        }
    }

    /// Every TileID currently superseded in the overlay index.
    pub fn snapshot_ids(&self) -> Vec<u64> {
        self.inner
            .lock()
            .map(|i| i.index.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Current log file length in bytes (header + all appended records).
    pub fn size_bytes(&self) -> u64 {
        self.inner.lock().map(|i| i.log_len).unwrap_or(0)
    }

    pub fn base(&self) -> Option<Arc<PmtilesReader>> {
        self.inner.lock().ok().and_then(|i| i.base.clone())
    }

    /// Set the size-cap (bytes) past which `put` wakes the compaction controller (0 = off).
    pub fn set_max_bytes(&self, n: u64) {
        self.max_bytes.store(n, Ordering::Relaxed);
    }

    /// Await the next compaction wake (a size-cap breach in `put` or an explicit `/flush`). The
    /// controller races this against its interval tick.
    pub async fn compactor_woken(&self) {
        self.compact_notify.notified().await;
    }

    /// The compaction output `.pmtiles` path — the overlay's log path (`<base>.pmtiles.wal`) with the
    /// trailing `.wal` stripped. `clear`/`open` keep the log beside the base, so this recovers the
    /// base path the compactor writes.
    pub fn base_path(&self) -> PathBuf {
        let s = self.path.to_string_lossy();
        match s.strip_suffix(".wal") {
            Some(base) => PathBuf::from(base),
            None => self.path.clone(),
        }
    }

    /// Cheap early-out: is a compaction in progress right now? No longer the correctness gate — that
    /// is `try_begin_compaction` (an atomic compare-and-set) + the `compacting`-skip in `put`, both
    /// under the same inner lock. Callers use this only to avoid obviously-redundant work.
    pub fn is_compacting(&self) -> bool {
        self.inner.lock().map(|i| i.compacting).unwrap_or(true)
    }

    /// Begin a compaction IFF none is running: atomic compare-and-set of the `compacting` flag under
    /// the inner lock. Returns true if this caller now owns the compaction (it MUST call
    /// `end_compaction` when done), false if one is already in progress (the caller must skip —
    /// starting a second would share `PmtilesWriter`'s PID-only scratch file and corrupt the
    /// intermediate). Serializes same-layer compactions across every trigger site.
    pub fn try_begin_compaction(&self) -> bool {
        match self.inner.lock() {
            Ok(mut inner) => {
                if inner.compacting {
                    false
                } else {
                    inner.compacting = true;
                    true
                }
            }
            Err(_) => false, // poisoned -> don't start
        }
    }

    /// Release the compaction ownership taken by a `true` return from `try_begin_compaction`. Must
    /// run even if the compaction errored or panicked, so `put` can resume persisting.
    pub fn end_compaction(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.compacting = false;
        }
    }

    /// Truncate the log back to just the header, empty the index, and swap in the new base
    /// (post-compaction). Reopens the append handle at the header boundary.
    pub fn clear(&self, new_base: Option<Arc<PmtilesReader>>) -> PmResult<()> {
        let mut inner = self.inner.lock().map_err(|_| "overlay lock poisoned")?;
        let f = OpenOptions::new()
            .write(true)
            .open(&self.path)
            .map_err(|e| format!("overlay open-w: {e}"))?;
        f.set_len(LOG_HEADER_LEN)
            .map_err(|e| format!("overlay truncate: {e}"))?;
        inner.log_w = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("overlay reopen: {e}"))?;
        inner.index.clear();
        inner.log_len = LOG_HEADER_LEN;
        inner.base = new_base;
        Ok(())
    }

    /// Merge base ∪ overlay into a fresh clustered `.pmtiles` (overlay wins on id conflict), atomically
    /// replace `out_path`, reopen it as the new base, and clear the overlay. A put landing on a NEW id
    /// between the `snapshot_ids()` read and `clear()`'s truncate would otherwise be silently dropped
    /// by the truncate. That lost-put window is genuinely closed by the atomic pair
    /// `try_begin_compaction` (a compare-and-set of `compacting` under the inner lock, taken before any
    /// compaction runs) + `put`'s `compacting`-skip under that SAME lock: while a compaction owns the
    /// overlay, every `put` is atomically skipped rather than appended, so the log cannot gain a record
    /// that `clear()` then truncates. A skipped put is re-encoded on the next request (deterministic
    /// MVT over the immutable per-process `--vector` source), so no tile is lost. The same CAS also
    /// serializes same-layer compactions, so two triggers never share `PmtilesWriter`'s scratch file.
    pub fn compact(
        &self,
        out_path: &Path,
        tmp_dir: &Path,
        hf: HeaderFields,
        metadata_json: &str,
    ) -> PmResult<Counts> {
        let base = self.base();
        // union of ids, ascending
        let mut ids: BTreeSet<u64> = BTreeSet::new();
        if let Some(b) = &base {
            for id in b.all_tile_ids()? {
                ids.insert(id);
            }
        }
        for id in self.snapshot_ids() {
            ids.insert(id);
        }
        let mut w = PmtilesWriter::new(tmp_dir)?;
        for id in ids {
            // BTreeSet iterates ascending -> clustered
            let blob = match self.get_by_id_raw(id)? {
                // overlay wins
                Some(b) => Some(b),
                None => match &base {
                    Some(r) => r.raw_tile_by_id(id)?,
                    None => None,
                },
            };
            if let Some(b) = blob {
                w.add(id, b)?;
            }
        }
        let counts = w.finish(hf, metadata_json, out_path)?;
        let new_base = Arc::new(PmtilesReader::open(out_path)?);
        self.clear(Some(new_base))?;
        Ok(counts)
    }
}

/// Derive PMTiles `HeaderFields` from a WGS84 bounds `[w,s,e,n]` and a zoom span: e7-scaled bounds
/// and a center at the bounds midpoint, center zoom = `min_zoom` (mirrors `generate.rs`'s math so a
/// compacted archive carries the same header a freshly baked one would).
pub(crate) fn header_fields_from_bounds(b: [f64; 4], min_zoom: u8, max_zoom: u8) -> HeaderFields {
    let e7 = |d: f64| (d * 1e7) as i32;
    HeaderFields {
        min_zoom,
        max_zoom,
        bounds_e7: [e7(b[0]), e7(b[1]), e7(b[2]), e7(b[3])],
        center: (min_zoom, e7((b[0] + b[2]) / 2.0), e7((b[1] + b[3]) / 2.0)),
    }
}

/// Compact one write-through overlay layer into its `.pmtiles` — the single helper the interval,
/// size-cap, shutdown, and `/flush` triggers all call. Derives the output path (`ov.base_path()`),
/// a UNIQUE per-call temp dir (`pid` + the output file stem, so two DIFFERENT layers compacting
/// concurrently never collide on the writer's scratch file), the header fields, and the metadata
/// (the layer's own `vector_layers`-carrying JSON set via `set_metadata`, NOT derived from the
/// possibly-empty base — a write-through cache starts with no base, so deriving from it would write
/// `"{}"` on the first compaction), then runs `ov.compact`.
pub(crate) fn compact_overlay_layer(
    ov: &TileOverlay,
    bounds_wgs84: [f64; 4],
    min_zoom: u8,
    max_zoom: u8,
) -> PmResult<Counts> {
    let out_path = ov.base_path();
    let stem = out_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "layer".to_string());
    let tmp = std::env::temp_dir().join(format!(
        "ts_pmtiles_compact_{}_{}",
        std::process::id(),
        stem
    ));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("compact tmp dir {tmp:?}: {e}"))?;
    let hf = header_fields_from_bounds(bounds_wgs84, min_zoom, max_zoom);
    let metadata = ov.metadata_str();
    ov.compact(&out_path, &tmp, hf, &metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_and_last_writer_wins() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("ts_ov_{}.wal", std::process::id()));
        std::fs::remove_file(&p).ok();
        let ov = TileOverlay::open(&p, None).unwrap();
        let id = crate::vector::pmtiles::zxy_to_tileid(2, 1, 1);
        ov.put(id, &crate::vector::pmtiles::codec::gzip(b"first"))
            .unwrap();
        assert_eq!(ov.get(2, 1, 1).unwrap().as_deref(), Some(&b"first"[..]));
        ov.put(id, &crate::vector::pmtiles::codec::gzip(b"second"))
            .unwrap(); // supersede
        assert_eq!(ov.get(2, 1, 1).unwrap().as_deref(), Some(&b"second"[..]));
        assert_eq!(ov.get(3, 0, 0).unwrap(), None);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn recovers_index_and_truncates_torn_tail() {
        use std::io::Write;
        let dir = std::env::temp_dir();
        let p = dir.join(format!("ts_ovrec_{}.wal", std::process::id()));
        std::fs::remove_file(&p).ok();
        let id = crate::vector::pmtiles::zxy_to_tileid(1, 0, 0);
        {
            // write a valid log with one good record, then append a torn (truncated) record by hand
            let ov = TileOverlay::open(&p, None).unwrap();
            ov.put(id, &crate::vector::pmtiles::codec::gzip(b"good"))
                .unwrap();
        }
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
            f.write_all(&99u64.to_le_bytes()).unwrap(); // tile_id
            f.write_all(&1000u32.to_le_bytes()).unwrap(); // len 1000 but no blob -> torn
            f.write_all(&0u32.to_le_bytes()).unwrap();
        }
        let ov2 = TileOverlay::open(&p, None).unwrap(); // must recover, not error
        assert_eq!(ov2.get(1, 0, 0).unwrap().as_deref(), Some(&b"good"[..]));
        assert_eq!(
            ov2.get_by_id(crate::vector::pmtiles::zxy_to_tileid(0, 0, 0))
                .unwrap(),
            None
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn record_round_trips_and_detects_corruption() {
        let mut buf = Vec::new();
        write_record(&mut buf, 42, b"hello");
        write_record(&mut buf, 99, b"world!!");
        // first record parses
        let (id0, off0, len0, next0) = parse_record(&buf, 0).unwrap();
        assert_eq!(id0, 42);
        assert_eq!(&buf[off0..off0 + len0], b"hello");
        // second record parses from next0
        let (id1, off1, len1, _next1) = parse_record(&buf, next0).unwrap();
        assert_eq!(id1, 99);
        assert_eq!(&buf[off1..off1 + len1], b"world!!");
        // flip a blob byte -> CRC mismatch -> None (torn/corrupt)
        let mut bad = buf.clone();
        bad[off0] ^= 0xff;
        assert!(parse_record(&bad, 0).is_none());
        // truncated record (declares len past EOF) -> None
        let truncated = &buf[..next0 + 6]; // header of record 2 but not its blob
        assert!(parse_record(truncated, next0).is_none());
    }
    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF43926); // canonical IEEE CRC32 check value
    }

    #[test]
    fn compaction_merges_base_and_overlay() {
        use crate::vector::pmtiles::codec::gzip;
        use crate::vector::pmtiles::read::PmtilesReader;
        use crate::vector::pmtiles::write::HeaderFields;
        use crate::vector::pmtiles::zxy_to_tileid;
        let dir = std::env::temp_dir();
        let out = dir.join(format!("ts_cmp_{}.pmtiles", std::process::id()));
        let log = dir.join(format!("ts_cmp_{}.wal", std::process::id()));
        // Dedicated writer-scratch subdir: PmtilesWriter names its scratch `ts_pmtiles_data_<pid>.tmp`
        // (pid-keyed, not per-instance), so compacting via the bare temp dir would collide with another
        // test's concurrent writer in this binary. Isolate per-test (mirrors the merge-wins test).
        let tmp_dir = dir.join(format!(
            "ts_cmp_tmp_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_file(&out).ok();
        std::fs::remove_file(&log).ok();
        std::fs::remove_dir_all(&tmp_dir).ok();
        std::fs::create_dir_all(&tmp_dir).unwrap();
        // start-empty: base None, overlay has two tiles
        let ov = TileOverlay::open(&log, None).unwrap();
        let id_a = zxy_to_tileid(1, 0, 0);
        let id_b = zxy_to_tileid(1, 1, 1);
        ov.put(id_a, &gzip(b"AAAA")).unwrap();
        ov.put(id_b, &gzip(b"BBBB")).unwrap();
        let hf = HeaderFields {
            min_zoom: 1,
            max_zoom: 1,
            bounds_e7: [0, 0, 0, 0],
            center: (1, 0, 0),
        };
        let counts = ov.compact(&out, &tmp_dir, hf, "{}").unwrap();
        assert_eq!(counts.addressed, 2);
        // after compaction: overlay cleared, base is the new archive, both tiles served from it
        assert_eq!(ov.size_bytes(), super::LOG_HEADER_LEN);
        let r = PmtilesReader::open(&out).unwrap();
        assert_eq!(r.get(1, 0, 0).unwrap(), Some(b"AAAA".to_vec()));
        assert_eq!(r.get(1, 1, 1).unwrap(), Some(b"BBBB".to_vec()));
        // overlay now reads them via its swapped-in base (index empty, base serves)
        assert_eq!(ov.get(1, 0, 0).unwrap(), Some(b"AAAA".to_vec()));
        std::fs::remove_file(&out).ok();
        std::fs::remove_file(&log).ok();
        std::fs::remove_dir_all(&tmp_dir).ok();
    }

    /// The definitive base+overlay merge test: an id present in BOTH must resolve to the OVERLAY
    /// blob after compaction, not the base blob (silent-staleness guard).
    #[test]
    fn compaction_base_overlay_merge_overlay_wins() {
        use crate::vector::pmtiles::codec::gzip;
        use crate::vector::pmtiles::read::PmtilesReader;
        use crate::vector::pmtiles::write::{HeaderFields, PmtilesWriter};
        use crate::vector::pmtiles::zxy_to_tileid;
        use std::sync::Arc;

        let dir = std::env::temp_dir();
        let base_path = dir.join(format!("ts_cmpbo_base_{}.pmtiles", std::process::id()));
        let log_path = dir.join(format!("ts_cmpbo_{}.wal", std::process::id()));
        let out_path = dir.join(format!("ts_cmpbo_out_{}.pmtiles", std::process::id()));
        // PmtilesWriter::new() names its scratch file from process::id() alone, which is shared by
        // every test thread in this binary -- use a private subdir so this test's writer scratch
        // file never collides with another concurrently-running test's writer scratch file.
        let tmp_dir = dir.join(format!(
            "ts_cmpbo_tmp_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_file(&base_path).ok();
        std::fs::remove_file(&log_path).ok();
        std::fs::remove_file(&out_path).ok();
        std::fs::remove_dir_all(&tmp_dir).ok();
        std::fs::create_dir_all(&tmp_dir).unwrap();

        let mk_hf = || HeaderFields {
            min_zoom: 1,
            max_zoom: 1,
            bounds_e7: [0, 0, 0, 0],
            center: (1, 0, 0),
        };

        // build the BASE archive: two tiles, A and B
        let id_a = zxy_to_tileid(1, 0, 0);
        let id_b = zxy_to_tileid(1, 1, 1);
        {
            let mut w = PmtilesWriter::new(&tmp_dir).unwrap();
            w.add(id_a, gzip(b"AAAA")).unwrap();
            w.add(id_b, gzip(b"BASE_B")).unwrap();
            w.finish(mk_hf(), "{}", &base_path).unwrap();
        }
        let base = Arc::new(PmtilesReader::open(&base_path).unwrap());

        let ov = TileOverlay::open(&log_path, Some(base)).unwrap();
        // override B in the overlay -- overlay must win over the base's "BASE_B"
        ov.put(id_b, &gzip(b"OVERLAY_B")).unwrap();
        // an overlay-only new tile, not present in the base at all
        let id_c = zxy_to_tileid(1, 0, 1);
        ov.put(id_c, &gzip(b"CCCC")).unwrap();

        let counts = ov.compact(&out_path, &tmp_dir, mk_hf(), "{}").unwrap();
        assert_eq!(counts.addressed, 3); // A, B, C all present

        let r = PmtilesReader::open(&out_path).unwrap();
        assert_eq!(r.get(1, 0, 0).unwrap(), Some(b"AAAA".to_vec())); // base-only tile survives
        assert_eq!(
            r.get(1, 1, 1).unwrap(),
            Some(b"OVERLAY_B".to_vec()),
            "overlay blob must win over the base blob for a shared id"
        );
        assert_eq!(r.get(1, 0, 1).unwrap(), Some(b"CCCC".to_vec())); // overlay-only tile survives

        std::fs::remove_file(&base_path).ok();
        std::fs::remove_file(&log_path).ok();
        std::fs::remove_file(&out_path).ok();
        std::fs::remove_dir_all(&tmp_dir).ok();
    }

    #[test]
    fn stale_offset_past_eof_errs() {
        let dir = std::env::temp_dir();
        let log_path = dir.join(format!("ts_ovstale_{}.wal", std::process::id()));
        std::fs::remove_file(&log_path).ok();
        let ov = TileOverlay::open(&log_path, None).unwrap();
        let id = crate::vector::pmtiles::zxy_to_tileid(4, 2, 3);
        ov.put(id, &crate::vector::pmtiles::codec::gzip(b"payload"))
            .unwrap();
        assert!(ov.get_by_id_raw(id).unwrap().is_some());
        // truncate the file under the live overlay (the index still has the stale offset)
        std::fs::OpenOptions::new()
            .write(true)
            .open(&log_path)
            .unwrap()
            .set_len(LOG_HEADER_LEN)
            .unwrap();
        // read_exact_at must now ERROR (not return a zero-padded buffer)
        assert!(
            ov.get_by_id_raw(id).is_err(),
            "stale offset past EOF must error, not zero-pad"
        );
        std::fs::remove_file(&log_path).ok();
    }

    #[test]
    fn header_fields_from_bounds_maps_e7_and_center() {
        // A known [w,s,e,n] over Lisbon maps to e7-scaled bounds + a midpoint center at min_zoom.
        let hf = super::header_fields_from_bounds([-9.5, 38.0, -9.0, 39.0], 3, 14);
        assert_eq!(hf.min_zoom, 3);
        assert_eq!(hf.max_zoom, 14);
        assert_eq!(
            hf.bounds_e7,
            [-95_000_000, 380_000_000, -90_000_000, 390_000_000]
        );
        assert_eq!(hf.center, (3, -92_500_000, 385_000_000)); // midpoint, center zoom = min_zoom
    }

    /// The size-cap predicate + wake: with a small cap set, enough puts grow the log past it and the
    /// crossing `put` fires the compaction wake — so `compactor_woken()` resolves promptly (the
    /// latched `notify_one` permit), which `timeout` asserts without depending on wall-clock timing.
    #[tokio::test]
    async fn size_cap_wakes_compactor() {
        use crate::vector::pmtiles::codec::gzip;
        let dir = std::env::temp_dir();
        let log = dir.join(format!(
            "ts_cap_{}_{:?}.wal",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_file(&log).ok();
        let ov = TileOverlay::open(&log, None).unwrap();
        let cap = 200u64;
        ov.set_max_bytes(cap);
        assert!(ov.size_bytes() < cap, "starts under the cap (header only)");
        for k in 0..50u64 {
            ov.put(k, &gzip(b"xxxxxxxx")).unwrap();
        }
        assert!(ov.size_bytes() > cap, "overlay grows past the cap");
        // The crossing put latched a wake permit; `compactor_woken()` must resolve without waiting.
        tokio::time::timeout(std::time::Duration::from_secs(2), ov.compactor_woken())
            .await
            .expect("size-cap breach should have woken the compactor");
        std::fs::remove_file(&log).ok();
    }

    /// The compaction CAS is exclusive: the first `try_begin_compaction` wins, a second (before
    /// `end_compaction`) loses, and after `end_compaction` a fresh caller wins again. This is what
    /// serializes same-layer compactions across every trigger site.
    #[test]
    fn try_begin_compaction_is_exclusive() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!(
            "ts_ovcas_{}_{:?}.wal",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_file(&p).ok();
        let ov = TileOverlay::open(&p, None).unwrap();
        assert!(ov.try_begin_compaction(), "first caller acquires");
        assert!(
            !ov.try_begin_compaction(),
            "a second caller is refused while one is in progress"
        );
        ov.end_compaction();
        assert!(
            ov.try_begin_compaction(),
            "after end_compaction a fresh caller acquires again"
        );
        ov.end_compaction();
        std::fs::remove_file(&p).ok();
    }

    /// A `put` arriving while a compaction owns the overlay is atomically skipped (not persisted),
    /// closing the lost-put window; once `end_compaction` runs, puts persist normally again.
    #[test]
    fn put_skipped_during_compaction() {
        use crate::vector::pmtiles::codec::gzip;
        let dir = std::env::temp_dir();
        let p = dir.join(format!(
            "ts_ovskip_{}_{:?}.wal",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_file(&p).ok();
        let ov = TileOverlay::open(&p, None).unwrap();
        let id = crate::vector::pmtiles::zxy_to_tileid(3, 2, 1);
        // Enter compaction, then put -> skipped (the record is never appended/indexed).
        assert!(ov.try_begin_compaction());
        ov.put(id, &gzip(b"x")).unwrap();
        assert_eq!(
            ov.get_by_id(id).unwrap(),
            None,
            "put during compaction must be skipped, not persisted"
        );
        // Leave compaction, put again -> now persisted.
        ov.end_compaction();
        ov.put(id, &gzip(b"x")).unwrap();
        assert_eq!(
            ov.get_by_id(id).unwrap().as_deref(),
            Some(&b"x"[..]),
            "put after end_compaction must persist"
        );
        std::fs::remove_file(&p).ok();
    }

    /// `base_path()` recovers `<base>.pmtiles` from `<base>.pmtiles.wal` (Fix A: `.log` -> `.wal`).
    #[test]
    fn base_path_strips_wal_suffix() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("ts_ovbp_{}.pmtiles.wal", std::process::id()));
        std::fs::remove_file(&p).ok();
        let ov = TileOverlay::open(&p, None).unwrap();
        assert_eq!(
            ov.base_path(),
            dir.join(format!("ts_ovbp_{}.pmtiles", std::process::id()))
        );
        std::fs::remove_file(&p).ok();
    }

    /// Fix B TDD: `set_metadata` carries the layer's `vector_layers` JSON through compaction — before
    /// the fix, `compact_overlay_layer`'s base-derived metadata is `"{}"` for a write-through cache
    /// that starts with no base, and GDAL/MapLibre/tippecanoe reject an archive with no
    /// `vector_layers`. This test drives `ov.compact` directly with `ov.metadata_str()` (what
    /// `compact_overlay_layer` now does) rather than needing a `Layer`/bounds plumbing.
    #[test]
    fn compaction_writes_the_set_layer_metadata() {
        use crate::vector::pmtiles::codec::gzip;
        use crate::vector::pmtiles::read::PmtilesReader;
        use crate::vector::pmtiles::write::HeaderFields;
        use crate::vector::pmtiles::zxy_to_tileid;

        let dir = std::env::temp_dir();
        let out = dir.join(format!("ts_ovmeta_out_{}.pmtiles", std::process::id()));
        let log = dir.join(format!("ts_ovmeta_{}.pmtiles.wal", std::process::id()));
        let tmp_dir = dir.join(format!(
            "ts_ovmeta_tmp_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_file(&out).ok();
        std::fs::remove_file(&log).ok();
        std::fs::remove_dir_all(&tmp_dir).ok();
        std::fs::create_dir_all(&tmp_dir).unwrap();

        let ov = TileOverlay::open(&log, None).unwrap();
        ov.set_metadata(r#"{"vector_layers":[{"id":"t"}]}"#.to_string());
        let id = zxy_to_tileid(1, 0, 0);
        ov.put(id, &gzip(b"tile")).unwrap();

        let hf = HeaderFields {
            min_zoom: 1,
            max_zoom: 1,
            bounds_e7: [0, 0, 0, 0],
            center: (1, 0, 0),
        };
        let metadata = ov.metadata_str();
        ov.compact(&out, &tmp_dir, hf, &metadata).unwrap();

        let r = PmtilesReader::open(&out).unwrap();
        assert!(
            r.metadata().contains("vector_layers"),
            "compacted archive metadata must carry vector_layers, got: {}",
            r.metadata()
        );

        std::fs::remove_file(&out).ok();
        std::fs::remove_file(&log).ok();
        std::fs::remove_dir_all(&tmp_dir).ok();
    }
}
