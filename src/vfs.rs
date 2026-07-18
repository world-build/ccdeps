//! Virtual filesystem abstraction.
//!
//! Every file read, existence probe, and symlink resolution performed by the
//! scanner goes through the [`Vfs`] trait so embedders can virtualize the
//! tree (and observe every access).

use std::io;
use std::path::{Path, PathBuf};

/// Files at or above this size use memory-mapped I/O. Below this threshold,
/// a plain `read` is faster because mmap has fixed per-call overhead (~2 µs
/// for the VMA setup + page fault) that dominates for tiny files.
const MMAP_THRESHOLD: u64 = 4096;

/// Filesystem abstraction for the scanner. Every file read, existence
/// probe, and symlink resolution goes through this trait so embedders
/// can virtualize the tree (and observe every access).
pub trait Vfs: Send + Sync {
    /// Read a file's entire contents.
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Whether `path` exists and is a regular file (following symlinks).
    fn is_file(&self, path: &Path) -> bool;
    /// Resolve symlinks to a canonical absolute path.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
}

/// The real filesystem. `read` keeps the existing strategy: mmap for
/// files >= 4 KiB, plain read below (copying the mmap into the returned
/// Vec — the copy is one memcpy and keeps the trait object-safe).
pub struct OsVfs;

impl Vfs for OsVfs {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let file = std::fs::File::open(path)?;
        let metadata = file.metadata()?;
        let file_size = metadata.len();

        if file_size >= MMAP_THRESHOLD {
            let mmap = unsafe { memmap2::Mmap::map(&file)? };

            #[cfg(unix)]
            {
                mmap.advise(memmap2::Advice::Sequential).ok();
            }

            Ok(mmap.to_vec())
        } else {
            std::fs::read(path)
        }
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }
}
