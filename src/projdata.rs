// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Makes the embedded PROJ database available to a statically linked PROJ.
//!
//! Only active under the `bundled-proj` feature (the standalone Linux binary and the Python
//! wheel). A build against the system libproj compiles this away to an empty call: that
//! PROJ has its own `/usr/share/proj`, and we must not embed 9.4 MB or touch a cache
//! directory on that path.
//!
//! Static linking removes the *library* dependency but not the *data* one. PROJ needs
//! `proj.db` for every datum transform, and looks for it in the prefix it was configured
//! with, a path that exists only on the build machine. `build.rs` embeds the file; this
//! module writes it out once and points PROJ at it via `PROJ_DATA`.
//!
//! The vendored PROJ is built with `PROJ_DATA_ENV_VAR_TRIED_LAST=OFF`, so `PROJ_DATA` is
//! consulted BEFORE the compiled-in prefix. Setting it is therefore sufficient.
//!
//! Failure here is never fatal. If the filesystem is read-only and no temp directory works
//! (a distroless container, a locked-down pod), we warn and return: PROJ then fails per
//! transform with its own message, which is a far better outcome than a server that
//! refuses to start and cannot serve the requests that need no reprojection at all.

/// Point PROJ at the embedded database, extracting it to a user cache directory on first
/// run. Idempotent and cheap after that (one `stat`).
///
/// MUST be called before the first PROJ context is created, and before any thread is
/// spawned, because it sets an environment variable. In practice that means the first
/// statement of `main`, and the body of the PyO3 module initialiser. Nothing in this crate
/// builds a PROJ context at module scope, so those two entry points cover every caller.
#[cfg(feature = "bundled-proj")]
pub fn ensure_proj_data() {
    use std::path::PathBuf;

    const DB: &[u8] = include_bytes!(env!("TERRASERVE_PROJ_DB_PATH"));
    const ID: &str = env!("TERRASERVE_PROJ_DB_ID");

    // A user who set PROJ_DATA (or the legacy PROJ_LIB) meant it: they may be pointing at a
    // newer EPSG registry or a directory holding downloaded transformation grids. Never
    // override that.
    for var in ["PROJ_DATA", "PROJ_LIB"] {
        if std::env::var_os(var).is_some_and(|v| !v.is_empty()) {
            return;
        }
    }

    let dir = match cache_dir(ID) {
        Some(d) => d,
        None => {
            eprintln!(
                "warning: no writable cache or temp directory for the bundled proj.db; \
                 reprojection will fail. Set PROJ_DATA to a directory holding proj.db."
            );
            return;
        }
    };
    let db: PathBuf = dir.join("proj.db");

    // Fast path. Size is the cheap half of the check; the directory name already carries a
    // hash of the contents, so a same-size file in a same-named directory IS our file.
    let fresh = std::fs::metadata(&db).is_ok_and(|m| m.len() == DB.len() as u64);
    if !fresh {
        if let Err(e) = write_atomically(&dir, &db, DB) {
            eprintln!(
                "warning: could not stage the bundled proj.db at {}: {e}; \
                 reprojection will fail. Set PROJ_DATA to a directory holding proj.db.",
                db.display()
            );
            return;
        }
    }

    // Safe here in edition 2021, and correct because we run before any thread exists.
    std::env::set_var("PROJ_DATA", &dir);
}

/// No-op when linking the system libproj, which finds its own data.
#[cfg(not(feature = "bundled-proj"))]
pub fn ensure_proj_data() {}

/// `<cache>/terraserve/proj-<content id>`, created if absent.
///
/// Keyed on the database contents so an upgrade that changes proj.db lands in a new
/// directory (no stale reads) while one that does not lands in the same one (no needless
/// 9.4 MB rewrite). Falls back to the temp directory when there is no usable HOME, which
/// is the common case inside a container.
#[cfg(feature = "bundled-proj")]
fn cache_dir(id: &str) -> Option<std::path::PathBuf> {
    let leaf = format!("terraserve/proj-{id}");
    let roots = [
        std::env::var_os("XDG_CACHE_HOME").map(std::path::PathBuf::from),
        std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")),
        Some(std::env::temp_dir()),
    ];
    for root in roots.into_iter().flatten() {
        if root.as_os_str().is_empty() {
            continue;
        }
        let dir = root.join(&leaf);
        if std::fs::create_dir_all(&dir).is_ok() {
            return Some(dir);
        }
    }
    None
}

/// Write via a unique temporary name in the same directory, then rename into place.
///
/// Two processes starting together (a compose stack bringing up several layers, a test
/// suite) would otherwise race, and the loser could read a half-written 9.4 MB file and
/// get a corrupt SQLite database. `rename` within one directory is atomic, so a reader
/// sees either no file or the whole file.
#[cfg(feature = "bundled-proj")]
fn write_atomically(
    dir: &std::path::Path,
    dest: &std::path::Path,
    bytes: &[u8],
) -> std::io::Result<()> {
    use std::io::Write;

    let tmp = dir.join(format!("proj.db.tmp.{}", std::process::id()));
    let mut f = std::fs::File::create(&tmp)?;
    let r = f.write_all(bytes).and_then(|()| f.sync_all());
    drop(f);
    if let Err(e) = r {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(all(test, feature = "bundled-proj"))]
mod tests {
    /// The embedded database is present and is a real SQLite file, not an empty
    /// placeholder. Guards the failure that `--version` cannot see: a binary that starts
    /// and then cannot reproject.
    #[test]
    fn embedded_db_is_a_sqlite_database() {
        const DB: &[u8] = include_bytes!(env!("TERRASERVE_PROJ_DB_PATH"));
        assert!(
            DB.len() > 4_000_000,
            "proj.db is only {} bytes, that is not the real database",
            DB.len()
        );
        assert_eq!(&DB[..15], b"SQLite format 3", "not a SQLite file");
    }

    /// Extraction is idempotent and leaves no temporary file behind.
    #[test]
    fn ensure_is_idempotent() {
        // Isolate from the developer's real cache and from any inherited PROJ_DATA.
        let tmp = std::env::temp_dir().join(format!("ts-projdata-{}", std::process::id()));
        std::env::set_var("XDG_CACHE_HOME", &tmp);
        std::env::remove_var("PROJ_DATA");
        std::env::remove_var("PROJ_LIB");

        super::ensure_proj_data();
        let first = std::env::var("PROJ_DATA").expect("PROJ_DATA set on first run");
        let db = std::path::Path::new(&first).join("proj.db");
        assert!(db.is_file(), "proj.db was not staged at {}", db.display());
        let staged = std::fs::metadata(&db).unwrap().len();

        // Second call must be a no-op: PROJ_DATA is now set, so we return early.
        super::ensure_proj_data();
        assert_eq!(std::env::var("PROJ_DATA").unwrap(), first);
        assert_eq!(std::fs::metadata(&db).unwrap().len(), staged);

        let leftovers: Vec<_> = std::fs::read_dir(&first)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

/// `terraserve info`: report which PROJ and which `proj.db` this binary is actually using.
///
/// Exists because every other check is hollow. `--version` and `--help` never touch PROJ, so
/// a binary shipped without a usable database passes both and then fails on the first
/// reprojection a user attempts. This asks PROJ itself, after initialisation, and prints the
/// database path it resolved. CI asserts on that line; a user with a transform problem can
/// paste it into an issue.
pub fn run_info() -> Result<(), Box<dyn std::error::Error>> {
    let s = proj_status();

    println!("terraserve   {}", s.terraserve_version);
    println!(
        "PROJ         {} ({})",
        s.proj_release,
        if s.bundled {
            "statically linked, vendored"
        } else {
            "system libproj, dynamically linked"
        }
    );
    match (&s.database_path, s.database_bytes) {
        (Some(p), Some(n)) => {
            println!("proj.db      {p}");
            println!("             {n} bytes");
        }
        // Not a hard error: a caller may only ever do same-CRS work, which needs no
        // database. But it is the single most useful thing to see when a transform fails.
        _ => println!("proj.db      NOT FOUND - reprojection will fail"),
    }
    if let (Some(n), Some(id)) = (s.embedded_bytes, s.embedded_id) {
        println!("embedded     {n} bytes, id {id}");
    }
    println!(
        "PROJ_DATA    {}",
        s.proj_data.as_deref().unwrap_or("(unset)")
    );
    println!(
        "PROJ_LIB     {}",
        s.proj_lib.as_deref().unwrap_or("(unset)")
    );
    println!("search path  {}", s.search_path);
    Ok(())
}

/// What `terraserve info` reports, as data.
///
/// Shared with the Python binding: a wheel embeds `proj.db` exactly as the binary does, so a
/// pygeoapi user whose transforms fail needs the same answer and has no CLI to ask.
#[derive(Debug, Clone)]
pub struct ProjStatus {
    pub terraserve_version: &'static str,
    /// PROJ's own release string, e.g. "Rel. 9.6.0, March 15th, 2025".
    pub proj_release: String,
    /// True when PROJ is statically linked and its database is embedded in this binary.
    pub bundled: bool,
    /// The `proj.db` PROJ actually resolved. `None` means no database was found at all.
    pub database_path: Option<String>,
    pub database_bytes: Option<u64>,
    /// Size and content id of the copy compiled into this binary (bundled builds only).
    pub embedded_bytes: Option<usize>,
    pub embedded_id: Option<&'static str>,
    pub proj_data: Option<String>,
    pub proj_lib: Option<String>,
    pub search_path: String,
}

/// Ask PROJ what it is and what data it resolved. Cheap enough to call per request, though
/// it is meant for diagnostics rather than a hot path.
pub fn proj_status() -> ProjStatus {
    let (proj_release, search_path) = proj_build_info();
    let database_path = proj_database_path();
    let database_bytes = database_path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len());

    #[cfg(feature = "bundled-proj")]
    let (embedded_bytes, embedded_id) = {
        const DB: &[u8] = include_bytes!(env!("TERRASERVE_PROJ_DB_PATH"));
        (Some(DB.len()), Some(env!("TERRASERVE_PROJ_DB_ID")))
    };
    #[cfg(not(feature = "bundled-proj"))]
    let (embedded_bytes, embedded_id) = (None, None);

    ProjStatus {
        terraserve_version: env!("CARGO_PKG_VERSION"),
        proj_release,
        bundled: cfg!(feature = "bundled-proj"),
        database_path,
        database_bytes,
        embedded_bytes,
        embedded_id,
        proj_data: std::env::var("PROJ_DATA").ok(),
        proj_lib: std::env::var("PROJ_LIB").ok(),
        search_path,
    }
}

/// PROJ's own release string and search path, straight from `proj_info()`.
fn proj_build_info() -> (String, String) {
    use std::ffi::CStr;
    // SAFETY: `proj_info` takes no arguments and returns a struct of pointers into PROJ's own
    // static storage, valid for the process lifetime. We only read them, and copy before
    // returning. Null is possible in principle, so each pointer is checked.
    unsafe {
        let info = proj_sys::proj_info();
        let s = |p: *const std::os::raw::c_char| -> String {
            if p.is_null() {
                "(unknown)".to_owned()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        (s(info.release), s(info.searchpath))
    }
}

/// The `proj.db` PROJ actually resolved, or `None` if it found none.
fn proj_database_path() -> Option<String> {
    use std::ffi::CStr;
    // SAFETY: the context is created and destroyed on every path. The string PROJ returns is
    // owned by the context, so it is copied out strictly before the context is destroyed.
    unsafe {
        let ctx = proj_sys::proj_context_create();
        if ctx.is_null() {
            return None;
        }
        let p = proj_sys::proj_context_get_database_path(ctx);
        let out = if p.is_null() {
            None
        } else {
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };
        proj_sys::proj_context_destroy(ctx);
        out
    }
}
