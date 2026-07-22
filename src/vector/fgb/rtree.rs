// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Packed Hilbert R-tree ("flatbush") layout math + windowed bbox traversal — the FlatGeoBuf
//! `[packed Hilbert R-tree]` container section between the Header and the features. Reads
//! through `cog::RangeSource` (the same seam COG uses), so a windowed query touches only the
//! handful of 40-byte node entries the traversal actually visits — never the whole index, and
//! never the features section.
//!
//! ## On-disk layout
//! The tree is static and breadth-first-packed, fully derivable from `features_count` +
//! `index_node_size` alone (every FlatGeoBuf writer builds the same deterministic tree from
//! the same two inputs). Node entry = 4×`f64` bbox (minX, minY, maxX, maxY) + 1×`u64` byte
//! offset = [`NODE_ITEM_SIZE`] = 40 bytes. Node `k` lives at file offset
//! `index_start + k * NODE_ITEM_SIZE`, where `index_start = 12 + header_size` (the container
//! prefix + the Header FlatBuffer, both `mod.rs::FgbSource::open` already computed).
//!
//! Levels are computed bottom-up (level 0 = the `num_items` leaves; each level above is
//! `ceil(prev / node_size)` until a single root), but **stored top-down**: the array's first
//! entries are the root level, and the leaf level occupies the tail (the last `num_items`
//! entries) — confirmed empirically against `fixtures/fgb/tiny.fgb` (3 items, node_size 16):
//! node 0 is the root (bbox = the union of all 3 features, matching the Header's envelope),
//! nodes 1..4 are the 3 leaves (each bbox matching one feature's geometry exactly, `offset`
//! matching that feature's real byte offset into the features section). This top-down storage
//! order is also why the traversal below can push a *contiguous* child index range per inner
//! node without reading any "child pointer" field — the range is derived purely from the
//! node's position within its own level (see [`query`]), which the packed layout guarantees is
//! contiguous in the level below.

use std::io;

use crate::cog::RangeSource;

/// Byte size of one packed R-tree node entry: 4×`f64` bbox (minX, minY, maxX, maxY) +
/// 1×`u64` byte offset.
pub const NODE_ITEM_SIZE: u64 = 40;

/// The static packed-R-tree layout for `num_items` leaves at branching factor `node_size`.
/// `levels[0]` is the leaf level; `levels[levels.len() - 1]` is the 1-node root. Each entry is
/// `(start_index, count)` — that level's contiguous node-index range in the on-disk array
/// (root-first, leaves-last; see module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub num_nodes: u64,
    pub levels: Vec<(u64, u64)>,
}

impl Layout {
    fn empty() -> Self {
        Layout {
            num_nodes: 0,
            levels: Vec::new(),
        }
    }

    /// True for "no index" — `num_items == 0` or `node_size < 2` (FlatGeoBuf's sentinel for
    /// "the container has no `[packed R-tree]` section"; see `mod.rs`'s doc comment on the
    /// container layout).
    pub fn is_empty(&self) -> bool {
        self.num_nodes == 0
    }

    /// Byte size of the whole index section: `num_nodes * NODE_ITEM_SIZE`. `saturating_mul`
    /// (Task 7): `num_nodes` is derived from the Header's `features_count`, a raw untrusted
    /// `u64` an attacker fully controls (no upper bound is enforced when it's read) — an
    /// absurd value must saturate to `u64::MAX` rather than silently wrap to a small, wrong
    /// value in `--release` (or panic on the debug-mode `attempt to multiply with overflow`
    /// this multiplication used to risk). The caller (`FgbSource::open`) further turns a
    /// saturated/implausible result into a clean `Err` rather than acting on it.
    pub fn index_size(&self) -> u64 {
        self.num_nodes.saturating_mul(NODE_ITEM_SIZE)
    }

    fn leaf_level(&self) -> (u64, u64) {
        self.levels[0]
    }

    fn root_level(&self) -> (u64, u64) {
        self.levels[self.levels.len() - 1]
    }
}

/// Compute the packed R-tree layout for `num_items` leaves at branching factor `node_size`,
/// mirroring `mod.rs`'s original inline `index_size` formula's level-count loop exactly (a
/// do-while: it always adds at least one level above the leaves, even when `num_items == 1` —
/// verified against the real `fixtures/fgb/tiny.fgb` file, whose 3-item tree has a distinct
/// root node rather than treating a single leaf as its own root).
pub fn tree_layout(num_items: u64, node_size: u16) -> Layout {
    if num_items == 0 || node_size < 2 {
        return Layout::empty();
    }
    let ns = node_size as u64;

    // Level sizes bottom-up: level 0 = the leaves, then ceil(prev/node_size) per level above,
    // until a level of size 1 (the root).
    let mut level_sizes = vec![num_items];
    let mut n = num_items;
    loop {
        n = n.div_ceil(ns);
        level_sizes.push(n);
        if n == 1 {
            break;
        }
    }
    // Task 7: `num_items` (== `features_count`) is a raw, unbounded `u64` read straight from
    // the untrusted Header -- for an absurd value (e.g. near `u64::MAX`) this sum used to be
    // able to overflow (`level_sizes[0] == num_items` alone can already be within a level or
    // two's width of `u64::MAX`), which panics in debug and silently wraps in `--release`.
    // `saturating_add` instead: the count just clamps to `u64::MAX`, which `index_size` above
    // then saturates through its own multiply, and `FgbSource::open` turns into a clean `Err`.
    let num_nodes: u64 = level_sizes
        .iter()
        .fold(0u64, |acc, &x| acc.saturating_add(x));

    // Assign on-disk index ranges top-down: the root (last entry in `level_sizes`) gets the
    // first slot(s), each level below continues immediately after, and the leaf level (the
    // first entry in `level_sizes`) lands last. Same overflow risk (and same `saturating_add`
    // fix) as `num_nodes` above -- an absurd `num_items` makes these per-level start offsets
    // just as capable of overflowing a `u64` sum.
    let mut levels = vec![(0u64, 0u64); level_sizes.len()];
    let mut start = 0u64;
    for i in (0..level_sizes.len()).rev() {
        let count = level_sizes[i];
        levels[i] = (start, count);
        start = start.saturating_add(count);
    }

    Layout { num_nodes, levels }
}

/// Standard min/max bbox overlap test (touching edges count as overlap, matching every other
/// bbox test in this crate — e.g. `render.rs`'s tile/window intersection).
fn intersects(a: [f64; 4], b: [f64; 4]) -> bool {
    a[2] >= b[0] && a[0] <= b[2] && a[3] >= b[1] && a[1] <= b[3]
}

/// Read node `idx`'s 40-byte entry and split it into its bbox + stored `u64` offset. For a
/// leaf node the offset is the feature's byte offset into the features section (relative to
/// `features_start`); for an inner node it is unused by this traversal (child ranges are
/// derived positionally — see module docs) and simply ignored.
fn read_node<R: RangeSource>(src: &R, index_start: u64, idx: u64) -> io::Result<([f64; 4], u64)> {
    let off = index_start + idx * NODE_ITEM_SIZE;
    let bytes = src.read_range(off, NODE_ITEM_SIZE as usize)?;
    if bytes.len() != NODE_ITEM_SIZE as usize {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "fgb: short read on an R-tree node entry",
        ));
    }
    let f64_at = |lo: usize| f64::from_le_bytes(bytes[lo..lo + 8].try_into().unwrap());
    let bbox = [f64_at(0), f64_at(8), f64_at(16), f64_at(24)];
    let offset = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
    Ok((bbox, offset))
}

/// Windowed bbox query over the packed R-tree at `index_start` (`= 12 + header_size`,
/// `mod.rs::FgbSource::open`'s `index_start`): top-down from the root, pruning any subtree
/// whose stored bbox does not overlap `bbox`. Returns the byte offsets (into the *features*
/// section — add `features_start` to get an absolute file offset) of every leaf whose bbox
/// overlaps `bbox`. An empty `Layout` (no index: `num_items == 0` or `node_size < 2`) returns
/// `Ok(vec![])` — callers fall back to a sequential scan (`FgbSource::bruteforce_query`) in
/// that case, exactly as a real `.fgb` writer's `SPATIAL_INDEX=NO` output requires.
pub fn query<R: RangeSource>(
    src: &R,
    index_start: u64,
    num_items: u64,
    node_size: u16,
    bbox: [f64; 4],
) -> io::Result<Vec<u64>> {
    let layout = tree_layout(num_items, node_size);
    if layout.is_empty() {
        return Ok(Vec::new());
    }
    let (leaf_start, _) = layout.leaf_level();
    let (root_start, root_count) = layout.root_level();

    let mut stack: Vec<u64> = (root_start..root_start + root_count).collect();
    let mut hits = Vec::new();
    while let Some(idx) = stack.pop() {
        let (node_bbox, node_offset) = read_node(src, index_start, idx)?;
        if !intersects(node_bbox, bbox) {
            continue;
        }
        if idx >= leaf_start {
            // Leaf level (level 0): the stored offset is a matching feature's byte offset.
            hits.push(node_offset);
            continue;
        }
        // Inner node: find which level it's in, then derive its children's contiguous index
        // range in the level directly below (closer to the leaves) from its position within
        // its own level — the packed layout guarantees that range is exactly `node_size` wide
        // (clamped to the child level's end for a ragged last node).
        let lvl = layout
            .levels
            .iter()
            .position(|&(s, c)| idx >= s && idx < s + c)
            .expect("every node index falls within some level of its own layout");
        let (level_start, _) = layout.levels[lvl];
        let pos = idx - level_start;
        let (child_start, child_count) = layout.levels[lvl - 1];
        let from = child_start + pos * node_size as u64;
        let to = (child_start + (pos + 1) * node_size as u64).min(child_start + child_count);
        stack.extend(from..to);
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::fgb::FgbSource;
    use std::cell::Cell;
    use std::rc::Rc;

    fn sorted(mut v: Vec<u64>) -> Vec<u64> {
        v.sort_unstable();
        v
    }

    #[test]
    fn tree_layout_matches_tiny_fixture_by_hand() {
        // Hand-verified (Task 2's `index_size(3, 16) == 160` plus a byte-level walk of the
        // real `fixtures/fgb/tiny.fgb` index section): 3 leaves + 1 root = 4 nodes. The root
        // (index 0) is a distinct node whose bbox is the union of all 3 features (matching the
        // Header envelope [0,0,5,6]); the leaves are indices 1..4.
        let layout = tree_layout(3, 16);
        assert_eq!(layout.num_nodes, 4);
        assert_eq!(layout.index_size(), 160);
        assert_eq!(layout.levels, vec![(1, 3), (0, 1)]); // [leaf, root]
        assert!(!layout.is_empty());
    }

    #[test]
    fn tree_layout_empty_when_no_index() {
        assert!(tree_layout(0, 16).is_empty());
        assert!(tree_layout(5, 0).is_empty());
        assert!(tree_layout(5, 1).is_empty()); // node_size < 2 is also "no index"
    }

    /// Task 7: `features_count` (== `tree_layout`'s `num_items`) is a raw, unbounded `u64` read
    /// straight from the untrusted Header (no upper bound is enforced when it's parsed) -- an
    /// absurd value like `u64::MAX` used to risk an `attempt to add/multiply with overflow`
    /// panic in `num_nodes`'s level-size sum and `index_size`'s `num_nodes * NODE_ITEM_SIZE`
    /// (debug-mode panic; silent wraparound to a small, wrong byte size in `--release`). Both
    /// are `saturating_*` now: this must complete without panicking, for the worst case
    /// (`node_size=2`, the smallest allowed branching factor, which maximizes the number of
    /// tree levels/nodes for a given item count) as well as a typical `node_size`.
    #[test]
    fn tree_layout_and_index_size_never_panic_on_absurd_features_count() {
        for node_size in [2u16, 16] {
            let layout = tree_layout(u64::MAX, node_size);
            assert!(!layout.is_empty());
            // Saturated, not wrapped: a genuine byte size this large could never actually be
            // backed by a real file, which is exactly the point -- `FgbSource::open`'s
            // `checked_add` (see `mod.rs`) turns this into a clean `Err` rather than silently
            // continuing with a small/wrong `features_start`.
            assert_eq!(
                layout.index_size(),
                u64::MAX,
                "must saturate, not wrap, at node_size={node_size}"
            );
        }
        // A merely-large-but-plausible count (not the pathological MAX) must still compute a
        // normal, non-saturated byte size -- the guard must not affect ordinary large files.
        let big = tree_layout(10_000_000, 16);
        assert!(!big.is_empty());
        assert!(big.index_size() > 0 && big.index_size() < u64::MAX);
    }

    #[test]
    fn tree_layout_single_item_still_gets_a_root() {
        // The do-while level loop always runs at least once, so even 1 item gets a distinct
        // root node above it (2 nodes total) rather than serving as its own root — matches the
        // original `mod.rs::index_size` formula this function replaces.
        let layout = tree_layout(1, 16);
        assert_eq!(layout.num_nodes, 2);
        assert_eq!(layout.levels, vec![(1, 1), (0, 1)]);
    }

    #[test]
    fn rtree_query_matches_bruteforce_on_tiny() {
        // fixtures/fgb/tiny.fgb: Point a=(1,2), Point b=(5,6), Polygon c=(0,0)-(3,3). A bbox
        // around a+c must return exactly those two; a bbox around only b must return only b.
        let src = crate::cog::LocalFileRangeSource::open("fixtures/fgb/tiny.fgb").unwrap();
        let fgb = FgbSource::open(src).unwrap();

        let around_a_and_c = [-0.5, -0.5, 3.5, 3.5];
        let hits = fgb.rtree_query(around_a_and_c).unwrap();
        let brute = fgb.bruteforce_query(around_a_and_c).unwrap();
        assert_eq!(sorted(hits.clone()), sorted(brute));
        // Hand-verified (same byte-walk as Task 2): point a's feature starts at relative
        // offset 96, polygon c's at 192 — point b (offset 0) must NOT be in this set.
        assert_eq!(sorted(hits), vec![96, 192]);

        let around_b_only = [4.5, 5.5, 5.5, 6.5];
        let hits2 = fgb.rtree_query(around_b_only).unwrap();
        let brute2 = fgb.bruteforce_query(around_b_only).unwrap();
        assert_eq!(sorted(hits2.clone()), sorted(brute2));
        assert_eq!(hits2, vec![0]); // point b's feature is at relative offset 0

        // A bbox over the whole extent must return all 3; one that misses everything, none.
        let all = fgb.rtree_query([-1.0, -1.0, 6.0, 7.0]).unwrap();
        assert_eq!(sorted(all), vec![0, 96, 192]);
        let none = fgb.rtree_query([100.0, 100.0, 101.0, 101.0]).unwrap();
        assert!(none.is_empty());
        assert_eq!(
            fgb.bruteforce_query([100.0, 100.0, 101.0, 101.0]).unwrap(),
            Vec::<u64>::new()
        );
    }

    /// A small `RangeSource` wrapper, test-only (mirrors `cog.rs`'s test-only
    /// `CountingRangeSource`, not exposed to non-test code — see the Task 3 brief: "do not
    /// change cog.rs visibility unless trivial"). Uses an `Rc<Cell<u64>>` counter (rather than
    /// borrowing the inner source, as `cog.rs`'s version does) so the counter can be read
    /// *after* the wrapper has been moved into `FgbSource::open`.
    struct CountingRangeSource<R> {
        inner: R,
        bytes: Rc<Cell<u64>>,
    }

    impl<R: RangeSource> RangeSource for CountingRangeSource<R> {
        fn read_range(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
            let out = self.inner.read_range(offset, len)?;
            self.bytes.set(self.bytes.get() + len as u64);
            Ok(out)
        }
    }

    #[test]
    fn rtree_reads_less_than_whole_file() {
        let inner = crate::cog::LocalFileRangeSource::open("fixtures/fgb/tiny.fgb").unwrap();
        let counter = Rc::new(Cell::new(0u64));
        let counting = CountingRangeSource {
            inner,
            bytes: counter.clone(),
        };
        let fgb = FgbSource::open(counting).unwrap();

        // A point-sized bbox: the traversal reads the root + the leaves it doesn't prune, but
        // never the features section (rtree_query never touches feature bytes at all).
        let _ = fgb.rtree_query([1.0, 2.0, 1.0, 2.0]).unwrap();

        let total_file_bytes = std::fs::metadata("fixtures/fgb/tiny.fgb").unwrap().len();
        assert!(
            counter.get() < total_file_bytes,
            "counted {} bytes read, whole file is {} bytes — rtree_query should not need to \
             read the whole file",
            counter.get(),
            total_file_bytes
        );
    }
}
