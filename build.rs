// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Build script. Its ONLY job is to locate `proj.db` when the `bundled-proj` feature is on,
//! so `src/projdata.rs` can `include_bytes!` it into the binary.
//!
//! Why this is needed at all: `bundled-proj` statically links PROJ, which removes the
//! *library* dependency but not the *data* one. PROJ cannot do a single datum transform
//! without its 9.4 MB SQLite database, and a statically linked PROJ looks for it in the
//! prefix it was configured with at build time, which does not exist on a user's machine.
//! Embedding the database is what makes the shipped Linux binary genuinely standalone.
//!
//! Why we hunt for the file instead of being told where it is: `proj-sys` declares
//! `links = "proj"`, but its build script emits only link directives
//! (`rustc-link-search` / `rustc-link-lib` / `rustc-cfg`). It publishes no path metadata,
//! so there is no `DEP_PROJ_*` variable carrying its OUT_DIR. Cargo does guarantee that a
//! dependency's build script has already run when ours does, so the staged file is on disk
//! by now; we just have to find it. It is staged at
//! `<target>/<profile>/build/proj-sys-<hash>/out/share/proj/proj.db`, a sibling of our own
//! OUT_DIR, so we walk up to the shared `build/` directory and look there.
//!
//! Two build directories exist per build-script crate (one holding the compiled script,
//! one holding its output); only the second has `out/share/proj/`, so matching on that
//! full path disambiguates them without guessing at hashes.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var_os("CARGO_FEATURE_BUNDLED_PROJ").is_none() {
        // System-libproj build: nothing to embed, PROJ finds its own data. This is the
        // default path and must stay free of any of the above.
        return;
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is always set"));
    let db = find_proj_db(&out_dir);

    println!("cargo:rerun-if-changed={}", db.display());
    println!("cargo:rustc-env=TERRASERVE_PROJ_DB_PATH={}", db.display());

    // Key the runtime cache directory on the file's content, not on our crate version:
    // a version bump that leaves proj.db unchanged should not force a re-extract, and a
    // PROJ upgrade that changes proj.db must invalidate the cache even if our version is
    // the same. FNV-1a is enough for a cache key (we are not defending against an
    // adversary, just against staleness) and costs no dependency.
    let bytes = std::fs::read(&db).unwrap_or_else(|e| panic!("cannot read {}: {e}", db.display()));
    println!(
        "cargo:rustc-env=TERRASERVE_PROJ_DB_ID={:016x}",
        fnv1a64(&bytes)
    );
    println!("cargo:rustc-env=TERRASERVE_PROJ_DB_LEN={}", bytes.len());
}

/// Find the `proj.db` staged by the `proj-sys` build script that ran for this build.
///
/// A clean target directory (every CI build, every fresh checkout) holds exactly one
/// candidate and this is trivial. A long-lived development target directory can hold
/// several, left behind by earlier `proj-sys` versions: cargo never garbage-collects them.
/// Those stale copies are the hazard, because pairing one PROJ version's database with
/// another version's library is a silent correctness bug, not a build error.
///
/// So: candidates that agree on a PROJ version are interchangeable and we take any of them.
/// Candidates that DISAGREE are a real ambiguity we refuse to resolve by guessing. The
/// version comes from the source tree proj-sys extracted next to the database
/// (`out/PROJSRC/proj/proj-<version>`), which is the version that produced it.
///
/// `TERRASERVE_PROJ_DB` overrides the whole search when a build needs to be explicit.
fn find_proj_db(out_dir: &Path) -> PathBuf {
    println!("cargo:rerun-if-env-changed=TERRASERVE_PROJ_DB");
    if let Some(p) = std::env::var_os("TERRASERVE_PROJ_DB") {
        let p = PathBuf::from(p);
        assert!(
            p.is_file(),
            "TERRASERVE_PROJ_DB={} is not a file",
            p.display()
        );
        return p;
    }

    // OUT_DIR is <target>/[<triple>/]<profile>/build/terraserve-<hash>/out, so the shared
    // build directory is two levels up. Walk a few ancestors rather than hard-code the
    // depth, in case cargo's layout shifts under us.
    let mut found: Vec<(PathBuf, String)> = Vec::new();
    for anc in out_dir.ancestors().take(4) {
        let Ok(entries) = std::fs::read_dir(anc) else {
            continue;
        };
        for e in entries.flatten() {
            if !e.file_name().to_string_lossy().starts_with("proj-sys-") {
                continue;
            }
            let out = e.path().join("out");
            let db = out.join("share/proj/proj.db");
            if db.is_file() {
                found.push((db, proj_version_of(&out)));
            }
        }
        if !found.is_empty() {
            break;
        }
    }

    let mut versions: Vec<&str> = found.iter().map(|(_, v)| v.as_str()).collect();
    versions.sort_unstable();
    versions.dedup();

    match versions.len() {
        1 => {
            // All candidates carry the same PROJ version, so any of them is the right file.
            let (db, ver) = found.swap_remove(0);
            println!(
                "cargo:warning=embedding PROJ {ver} proj.db from {}",
                db.display()
            );
            db
        }
        0 => panic!(
            "bundled-proj is enabled but no proj-sys build output was found near {}.\n\
             Expected <target>/<profile>/build/proj-sys-*/out/share/proj/proj.db.\n\
             If proj-sys stopped staging proj.db, this script needs updating - do NOT ship\n\
             a binary without it, every reprojection would fail at runtime.",
            out_dir.display()
        ),
        _ => {
            let list = found
                .iter()
                .map(|(p, v)| format!("  PROJ {v:8}  {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n");
            panic!(
                "found proj.db files from {} different PROJ versions and will not guess:\n\
                 {list}\n\n\
                 These are leftovers from earlier proj-sys versions; cargo never removes them.\n\
                 Shipping one version's database with another version's library is a silent\n\
                 correctness bug, so this build stops instead. Fix with either:\n\
                   cargo clean -p proj-sys\n\
                 or point at the right file explicitly:\n\
                   TERRASERVE_PROJ_DB=<path to proj.db> cargo build ...",
                versions.len()
            )
        }
    }
}

/// The PROJ version that produced a staged database, read from the source tree proj-sys
/// extracted beside it (`out/PROJSRC/proj/proj-9.6.0` -> `9.6.0`).
fn proj_version_of(out: &Path) -> String {
    std::fs::read_dir(out.join("PROJSRC/proj"))
        .ok()
        .and_then(|d| {
            d.flatten()
                .filter_map(|e| {
                    e.file_name()
                        .to_str()
                        .and_then(|n| n.strip_prefix("proj-"))
                        .map(str::to_owned)
                })
                .next()
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
