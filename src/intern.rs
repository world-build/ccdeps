//! Path interning: deduplicate resolved paths into a global table.
//!
//! Avoids allocating a fresh `PathBuf` for every resolved include path.
//! Paths are interned as `Arc<Path>`, so cloning is a reference-count bump.

use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Thread-safe path interner backed by a `DashMap`.
///
/// Canonicalized include paths are stored once and shared via `Arc<Path>`.
#[derive(Debug, Default)]
pub struct PathInterner {
    table: DashMap<PathBuf, Arc<Path>>,
}

impl PathInterner {
    pub fn new() -> Self {
        Self {
            table: DashMap::new(),
        }
    }

    /// Intern a path, returning a shared reference. If the path was already
    /// interned, returns the existing `Arc` (no allocation).
    pub fn intern(&self, path: &Path) -> Arc<Path> {
        if let Some(existing) = self.table.get(path) {
            return Arc::clone(&existing);
        }
        let arc: Arc<Path> = Arc::from(path);
        self.table.insert(path.to_path_buf(), Arc::clone(&arc));
        arc
    }

    /// Intern an owned `PathBuf`, avoiding an extra clone when the caller
    /// already owns the buffer.
    pub fn intern_owned(&self, path: PathBuf) -> Arc<Path> {
        if let Some(existing) = self.table.get(&path) {
            return Arc::clone(&existing);
        }
        let arc: Arc<Path> = Arc::from(path.as_path());
        self.table.insert(path, Arc::clone(&arc));
        arc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intern_deduplicates() {
        let interner = PathInterner::new();
        let p1 = interner.intern(Path::new("/usr/include/stdio.h"));
        let p2 = interner.intern(Path::new("/usr/include/stdio.h"));
        assert!(Arc::ptr_eq(&p1, &p2));
    }

    #[test]
    fn test_intern_owned() {
        let interner = PathInterner::new();
        let p1 = interner.intern_owned(PathBuf::from("/foo/bar.h"));
        let p2 = interner.intern(Path::new("/foo/bar.h"));
        assert!(Arc::ptr_eq(&p1, &p2));
    }
}
