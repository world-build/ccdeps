//! Interning: canonical paths become a compact, `Copy` [`FileId`], plus a
//! content hash the embedder can key incremental facts on.
//!
//! The scanner's inner loop speaks `FileId`, not `PathBuf`: visited-sets,
//! dedup, the dep graph, and results all index by it. A path enters the
//! interner once, ever; `PathBuf` appears only at the VFS boundary and, for
//! display, at the very end.

use dashmap::DashMap;
use std::hash::Hasher;
use std::path::Path;
use std::sync::{Arc, RwLock};

/// A compact handle for a canonical path. `Copy`, 4 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(u32);

impl FileId {
    /// The dense index this id maps to (for `BitSet`/epoch arrays).
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl std::fmt::Debug for FileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FileId({})", self.0)
    }
}

/// A content identity: a hash of a file's bytes. Stable across scans, cheap to
/// compare. The embedder pairs it with the `FileId` (path identity) when
/// recording a dependency fact, so a header edit re-taints exactly the TUs that
/// read it. Not a security boundary; collisions only matter alongside the path.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentHash(pub u64);

impl std::fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ContentHash({:#018x})", self.0)
    }
}

/// Hash file contents with FxHash: fast, allocation-free, order-preserving over
/// bytes. Fine for detecting edits when paired with a `FileId`.
pub fn content_hash(bytes: &[u8]) -> ContentHash {
    let mut h = FxHasher::default();
    h.write(bytes);
    ContentHash(h.finish())
}

/// The FxHash algorithm (as used by rustc), inlined to avoid a dependency.
#[derive(Default)]
struct FxHasher {
    hash: u64,
}

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(FX_SEED);
    }
}

impl Hasher for FxHasher {
    fn write(&mut self, mut bytes: &[u8]) {
        while bytes.len() >= 8 {
            let mut word = [0u8; 8];
            word.copy_from_slice(&bytes[..8]);
            self.add(u64::from_le_bytes(word));
            bytes = &bytes[8..];
        }
        if !bytes.is_empty() {
            let mut word = [0u8; 8];
            word[..bytes.len()].copy_from_slice(bytes);
            self.add(u64::from_le_bytes(word));
        }
    }

    fn finish(&self) -> u64 {
        self.hash
    }
}

/// Thread-safe, persistent path interner. Lives in the [`ScanService`] and is
/// shared across every TU and rebuild, so each canonical path is stored once.
///
/// [`ScanService`]: crate::ScanService
#[derive(Default)]
pub struct PathInterner {
    /// Canonical path -> id. Sharded, so hits (the common case) rarely contend.
    forward: DashMap<Box<Path>, FileId>,
    /// id -> canonical path. Grown only when a new path is interned.
    reverse: RwLock<Vec<Arc<Path>>>,
}

impl PathInterner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern a canonical path, returning its (stable) id. Interning the same
    /// path again returns the same id with no allocation.
    pub fn intern(&self, path: &Path) -> FileId {
        if let Some(id) = self.forward.get(path) {
            return *id;
        }
        // New path: assign an id under the reverse lock so id assignment and
        // the reverse push stay consistent if two threads race the same path.
        let mut reverse = self.reverse.write().unwrap();
        if let Some(id) = self.forward.get(path) {
            return *id;
        }
        let id = FileId(reverse.len() as u32);
        reverse.push(Arc::from(path));
        self.forward.insert(Box::from(path), id);
        id
    }

    /// The canonical path for an id.
    pub fn path(&self, id: FileId) -> Arc<Path> {
        Arc::clone(&self.reverse.read().unwrap()[id.index()])
    }

    /// Number of distinct paths interned so far.
    pub fn len(&self) -> usize {
        self.reverse.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.reverse.read().unwrap().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_deduplicates() {
        let interner = PathInterner::new();
        let a = interner.intern(Path::new("/usr/include/stdio.h"));
        let b = interner.intern(Path::new("/usr/include/stdio.h"));
        let c = interner.intern(Path::new("/usr/include/stdlib.h"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(interner.len(), 2);
    }

    #[test]
    fn round_trips_paths() {
        let interner = PathInterner::new();
        let id = interner.intern(Path::new("/a/b.h"));
        assert_eq!(&*interner.path(id), Path::new("/a/b.h"));
    }

    #[test]
    fn content_hash_is_stable_and_distinguishing() {
        assert_eq!(content_hash(b"hello world"), content_hash(b"hello world"));
        assert_ne!(content_hash(b"hello world"), content_hash(b"hello worlx"));
        assert_ne!(content_hash(b""), content_hash(b"x"));
    }
}
