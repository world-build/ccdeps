//! Include path resolution.
//!
//! Resolves `#include` paths against search directories, matching gcc/clang
//! behavior for `""` vs `<>` include resolution order.

use std::path::{Path, PathBuf};

/// A resolved include with metadata about where it was found.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedInclude {
    /// The resolved file path.
    pub path: PathBuf,
    /// Whether this was found in a system include path (`-isystem`).
    pub is_system: bool,
}

/// Configuration for include path resolution.
#[derive(Debug, Clone)]
pub struct SearchPaths {
    /// Paths from `-iquote` — searched only for `#include "..."`.
    pub quote_paths: Vec<PathBuf>,
    /// Paths from `-I` — searched for both `"..."` and `<...>`.
    pub include_paths: Vec<PathBuf>,
    /// Paths from `-isystem` — system include paths.
    pub system_paths: Vec<PathBuf>,
}

impl Default for SearchPaths {
    fn default() -> Self {
        Self {
            quote_paths: Vec::new(),
            include_paths: Vec::new(),
            system_paths: Vec::new(),
        }
    }
}

impl SearchPaths {
    /// Resolve a quoted include (`#include "path"`).
    ///
    /// Search order (matching gcc/clang):
    /// 1. Directory of the including file
    /// 2. `-iquote` paths
    /// 3. `-I` paths
    /// 4. `-isystem` paths
    pub fn resolve_quoted(&self, path: &str, including_file_dir: &Path) -> Option<ResolvedInclude> {
        // 1. Current file's directory
        let candidate = including_file_dir.join(path);
        if candidate.is_file() {
            return Some(ResolvedInclude { path: candidate, is_system: false });
        }

        // 2. -iquote paths
        for dir in &self.quote_paths {
            let candidate = dir.join(path);
            if candidate.is_file() {
                return Some(ResolvedInclude { path: candidate, is_system: false });
            }
        }

        // 3-4. Same as angle bracket
        self.resolve_angle(path)
    }

    /// Resolve an angle-bracket include (`#include <path>`).
    ///
    /// Search order:
    /// 1. `-I` paths
    /// 2. `-isystem` paths
    pub fn resolve_angle(&self, path: &str) -> Option<ResolvedInclude> {
        for dir in &self.include_paths {
            let candidate = dir.join(path);
            if candidate.is_file() {
                return Some(ResolvedInclude { path: candidate, is_system: false });
            }
        }

        for dir in &self.system_paths {
            let candidate = dir.join(path);
            if candidate.is_file() {
                return Some(ResolvedInclude { path: candidate, is_system: true });
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_resolve_quoted_relative() {
        let dir = tempfile::tempdir().unwrap();
        let header = dir.path().join("foo.h");
        fs::write(&header, "// foo").unwrap();

        let paths = SearchPaths::default();
        let result = paths.resolve_quoted("foo.h", dir.path());
        assert_eq!(result, Some(ResolvedInclude { path: header, is_system: false }));
    }

    #[test]
    fn test_resolve_angle_with_include_path() {
        let dir = tempfile::tempdir().unwrap();
        let header = dir.path().join("bar.h");
        fs::write(&header, "// bar").unwrap();

        let paths = SearchPaths {
            include_paths: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        let result = paths.resolve_angle("bar.h");
        assert_eq!(result, Some(ResolvedInclude { path: header, is_system: false }));
    }

    #[test]
    fn test_resolve_system_path() {
        let dir = tempfile::tempdir().unwrap();
        let header = dir.path().join("sys.h");
        fs::write(&header, "// sys").unwrap();

        let paths = SearchPaths {
            system_paths: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        let result = paths.resolve_angle("sys.h");
        assert_eq!(result, Some(ResolvedInclude { path: header, is_system: true }));
    }

    #[test]
    fn test_resolve_not_found() {
        let paths = SearchPaths::default();
        assert_eq!(paths.resolve_angle("nonexistent.h"), None);
    }
}
