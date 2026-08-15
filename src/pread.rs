// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Positional file reads, on every platform.
//!
//! The engine reads files at explicit offsets from many threads at once: `cog.rs` pulls TIFF tiles
//! out of a COG, and `pmtiles/overlay.rs` reads its append-only log while writers append to it.
//! Both did this with `std::os::unix::fs::FileExt::read_exact_at`, which does not exist on Windows,
//! so the crate simply did not compile there (`cannot find 'unix' in 'os'`). That is half of
//! terraops-org/TerraServe#9; the other half was jemalloc.
//!
//! The property both call sites depend on is that the read takes its offset as an ARGUMENT and does
//! not depend on a shared file cursor - otherwise two threads reading different tiles would race on
//! `seek`. This module preserves exactly that on both platforms and nothing more.

use std::fs::File;
use std::io;

/// Read exactly `buf.len()` bytes starting at `offset`, without using or disturbing a shared file
/// cursor. Errors if the file ends first - a short read is an error, never a zero-padded buffer.
///
/// Unix uses `pread` via `FileExt::read_exact_at`. Windows uses `FileExt::seek_read`, which is also
/// positional (it passes the offset through an `OVERLAPPED`), and loops because it is permitted to
/// return a short read. `seek_read` does additionally move the file pointer as a side effect, which
/// is harmless here precisely because every caller passes an explicit offset and none of them ever
/// reads from the current position.
pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0usize;
        while done < buf.len() {
            match file.seek_read(&mut buf[done..], offset + done as u64) {
                // A zero-length read at a non-EOF offset would spin forever, so treat it the same
                // way `read_exact` does: the file ended early.
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    ))
                }
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, buf, offset);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "positional file reads are not implemented for this platform",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::read_exact_at;
    use std::io::Write;

    fn temp_file(bytes: &[u8]) -> (std::path::PathBuf, std::fs::File) {
        let p = std::env::temp_dir().join(format!(
            "ts_pread_{}_{}.bin",
            std::process::id(),
            bytes.len()
        ));
        let mut w = std::fs::File::create(&p).unwrap();
        w.write_all(bytes).unwrap();
        w.sync_all().unwrap();
        let r = std::fs::File::open(&p).unwrap();
        (p, r)
    }

    #[test]
    fn reads_at_an_offset_without_moving_a_cursor() {
        let (p, f) = temp_file(b"0123456789");
        let mut a = [0u8; 3];
        let mut b = [0u8; 3];
        // Read the LATER range first: if this used a shared cursor, the second read would return
        // the bytes after it rather than the ones asked for.
        read_exact_at(&f, &mut a, 6).unwrap();
        read_exact_at(&f, &mut b, 1).unwrap();
        assert_eq!(&a, b"678");
        assert_eq!(&b, b"123");
        // And repeating a read gives the same bytes, which a cursor-based read would not.
        let mut again = [0u8; 3];
        read_exact_at(&f, &mut again, 6).unwrap();
        assert_eq!(&again, b"678");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn a_short_read_is_an_error_not_a_padded_buffer() {
        // The overlay reader depends on this: a truncated log must fail loudly rather than hand
        // back zeros that decode as an empty tile.
        let (p, f) = temp_file(b"abc");
        let mut buf = [0u8; 8];
        let err = read_exact_at(&f, &mut buf, 0).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn reading_entirely_past_the_end_errors() {
        let (p, f) = temp_file(b"abc");
        let mut buf = [0u8; 2];
        assert!(read_exact_at(&f, &mut buf, 99).is_err());
        let _ = std::fs::remove_file(p);
    }
}
