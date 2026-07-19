//! Virtual filesystem abstraction.
//!
//! Every file read, existence probe, and symlink resolution the scanner
//! performs goes through the [`Vfs`] trait, so embedders can virtualize the
//! tree and observe every access. Reads borrow (`Arc<[u8]>`) so a virtualizing
//! embedder never has to copy bytes out of its own store, and probes return a
//! real error type so a *negative* probe (tried, not found) is data the
//! embedder can act on rather than a discarded `false`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Files at or above this size use memory-mapped I/O. Below it, a plain `read`
/// wins because mmap has fixed per-call overhead (VMA setup + first fault).
const MMAP_THRESHOLD: u64 = 4096;

/// Why a [`Vfs`] operation failed.
///
/// `NotFound` is kept distinct from other errors because a not-found probe is
/// meaningful: include resolution walks a search path, and an embedder that
/// records probes must invalidate a scan when a file appears earlier in that
/// order than one it resolved to.
#[derive(Debug)]
pub enum VfsError {
    /// The path does not exist.
    NotFound,
    /// Any other failure (permissions, I/O error, ...).
    Io(io::Error),
}

impl std::fmt::Display for VfsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VfsError::NotFound => f.write_str("not found"),
            VfsError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for VfsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VfsError::NotFound => None,
            VfsError::Io(e) => Some(e),
        }
    }
}

impl From<io::Error> for VfsError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::NotFound {
            VfsError::NotFound
        } else {
            VfsError::Io(e)
        }
    }
}

impl From<VfsError> for io::Error {
    fn from(e: VfsError) -> Self {
        match e {
            VfsError::NotFound => io::Error::from(io::ErrorKind::NotFound),
            VfsError::Io(e) => e,
        }
    }
}

/// What a path names, as reported by [`Vfs::stat`] without following symlinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    Other,
}

/// Result of [`Vfs::stat`]: what the path is. Kept as a struct so the contract
/// can grow (content identity, size) without breaking impls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub file_type: FileType,
}

/// Filesystem abstraction for the scanner.
pub trait Vfs: Send + Sync {
    /// Read a file's entire contents as a shared, ref-counted buffer.
    fn read(&self, path: &Path) -> Result<Arc<[u8]>, VfsError>;

    /// Report what `path` is, without following symlinks. Returns
    /// [`VfsError::NotFound`] if it does not exist. Resolution probes many
    /// non-existent candidates, so this is the hot existence check.
    fn stat(&self, path: &Path) -> Result<Stat, VfsError>;

    /// Resolve symlinks to a canonical path. Trees with no symlinks (a Nix
    /// store) may return the input unchanged; embedders that set
    /// [`trust_canonical`](crate::ScanEnv::trust_canonical) skip this call
    /// altogether.
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, VfsError> {
        Ok(path.to_path_buf())
    }

    /// Whether `path` is a regular file, following one level of symlink.
    ///
    /// gcc's include resolution follows symlinks but [`stat`](Vfs::stat) does
    /// not, so this centralizes the one-level chase every embedder would
    /// otherwise hand-roll.
    fn is_regular_file(&self, path: &Path) -> bool {
        match self.stat(path) {
            Ok(Stat {
                file_type: FileType::Regular,
            }) => true,
            Ok(Stat {
                file_type: FileType::Symlink,
            }) => matches!(
                self.canonicalize(path).and_then(|t| self.stat(&t)),
                Ok(Stat {
                    file_type: FileType::Regular
                })
            ),
            _ => false,
        }
    }
}

/// The real filesystem. `read` uses mmap for files >= 4 KiB and a plain read
/// below that; both yield an owned `Arc<[u8]>` (one copy, as before).
pub struct OsVfs;

impl Vfs for OsVfs {
    fn read(&self, path: &Path) -> Result<Arc<[u8]>, VfsError> {
        let file = std::fs::File::open(path)?;
        let file_size = file.metadata()?.len();

        if file_size >= MMAP_THRESHOLD {
            let mmap = unsafe { memmap2::Mmap::map(&file)? };
            #[cfg(unix)]
            mmap.advise(memmap2::Advice::Sequential).ok();
            Ok(Arc::from(&mmap[..]))
        } else {
            Ok(Arc::from(std::fs::read(path)?))
        }
    }

    fn stat(&self, path: &Path) -> Result<Stat, VfsError> {
        let meta = std::fs::symlink_metadata(path)?;
        let ft = meta.file_type();
        let file_type = if ft.is_file() {
            FileType::Regular
        } else if ft.is_dir() {
            FileType::Directory
        } else if ft.is_symlink() {
            FileType::Symlink
        } else {
            FileType::Other
        };
        Ok(Stat { file_type })
    }

    fn canonicalize(&self, path: &Path) -> Result<PathBuf, VfsError> {
        Ok(std::fs::canonicalize(path)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn os_vfs_reads_and_stats() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.h");
        fs::write(&file, b"hello").unwrap();

        assert_eq!(&*OsVfs.read(&file).unwrap(), b"hello");
        assert_eq!(OsVfs.stat(&file).unwrap().file_type, FileType::Regular);
        assert!(OsVfs.is_regular_file(&file));
        assert!(!OsVfs.is_regular_file(dir.path()));
    }

    #[test]
    fn missing_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.h");
        assert!(matches!(OsVfs.stat(&missing), Err(VfsError::NotFound)));
        assert!(matches!(OsVfs.read(&missing), Err(VfsError::NotFound)));
        assert!(!OsVfs.is_regular_file(&missing));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_file_is_regular() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.h");
        let link = dir.path().join("link.h");
        fs::write(&target, b"x").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert_eq!(OsVfs.stat(&link).unwrap().file_type, FileType::Symlink);
        assert!(OsVfs.is_regular_file(&link));
    }
}
