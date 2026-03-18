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
    /// The display path (search_dir + raw include path), preserving `./` and `../`.
    /// Used for gcc-compatible output. `None` means use `path` as display.
    pub display_path: Option<PathBuf>,
}

/// Configuration for include path resolution.
#[derive(Debug, Clone, Default)]
pub struct SearchPaths {
    /// Paths from `-iquote` — searched only for `#include "..."`.
    pub quote_paths: Vec<PathBuf>,
    /// Paths from `-I` — searched for both `"..."` and `<...>`.
    pub include_paths: Vec<PathBuf>,
    /// Paths from `-isystem` — system include paths.
    pub system_paths: Vec<PathBuf>,
}

impl SearchPaths {
    /// Resolve a quoted include (`#include "path"`).
    ///
    /// Search order (matching gcc/clang):
    /// 1. Directory of the including file
    /// 2. `-iquote` paths
    /// 3. `-I` paths
    /// 4. `-isystem` paths
    ///
    /// `display_dir` is the "apparent directory" of the including file, used for
    /// constructing display paths that match gcc's behavior. When a file is found
    /// via a non-canonical path (e.g., `inc/../foo.h`), its display directory
    /// (`inc/..`) is passed here so that further includes from it also get
    /// non-canonical display paths.
    pub fn resolve_quoted(
        &self,
        path: &str,
        including_file_dir: &Path,
        display_dir: Option<&Path>,
    ) -> Option<ResolvedInclude> {
        // 1. Current file's directory
        let candidate = including_file_dir.join(path);
        if candidate.is_file() {
            let display = make_display_path_with_dir(
                display_dir.unwrap_or(including_file_dir),
                including_file_dir,
                path,
            );
            return Some(ResolvedInclude {
                path: candidate,
                is_system: false,
                display_path: display,
            });
        }

        // 2. -iquote paths
        for dir in &self.quote_paths {
            let candidate = dir.join(path);
            if candidate.is_file() {
                let display = make_display_path(dir, path);
                return Some(ResolvedInclude {
                    path: candidate,
                    is_system: false,
                    display_path: display,
                });
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
                let display = make_display_path(dir, path);
                return Some(ResolvedInclude {
                    path: candidate,
                    is_system: false,
                    display_path: display,
                });
            }
        }

        for dir in &self.system_paths {
            let candidate = dir.join(path);
            if candidate.is_file() {
                let display = make_display_path(dir, path);
                return Some(ResolvedInclude {
                    path: candidate,
                    is_system: true,
                    display_path: display,
                });
            }
        }

        None
    }

    /// Resolve `#include_next` — searches starting from the **next** directory
    /// after `found_in_dir` in the search path order.
    ///
    /// This is a GNU extension used heavily in system headers (e.g., glibc wrappers).
    pub fn resolve_include_next(&self, path: &str, found_in_dir: &Path) -> Option<ResolvedInclude> {
        let all_paths = self
            .quote_paths
            .iter()
            .chain(self.include_paths.iter())
            .chain(self.system_paths.iter());

        let mut found = false;
        for dir in all_paths {
            if !found {
                if dir == found_in_dir {
                    found = true;
                }
                continue;
            }
            let candidate = dir.join(path);
            if candidate.is_file() {
                let is_system = self.system_paths.iter().any(|sp| sp == dir);
                let display = make_display_path(dir, path);
                return Some(ResolvedInclude {
                    path: candidate,
                    is_system,
                    display_path: display,
                });
            }
        }

        None
    }
}

/// Construct a display path by concatenating the search directory and the raw
/// include path string. This preserves `./` and `../` segments as gcc does.
/// Returns `None` if the raw path has no special segments that `Path::join` would normalize.
fn make_display_path(dir: &Path, raw_include_path: &str) -> Option<PathBuf> {
    // Only produce a display path when the raw include path has segments that
    // Path::join would normalize away (../ or ./)
    if raw_include_path.contains("../") || raw_include_path.contains("./") {
        // Build the path as a string to preserve ./ and ../ segments
        let dir_str = dir.to_string_lossy();
        let sep = if dir_str.ends_with('/') { "" } else { "/" };
        Some(PathBuf::from(format!(
            "{}{}{}",
            dir_str, sep, raw_include_path
        )))
    } else {
        None
    }
}

/// Construct a display path considering a display directory override.
///
/// When `display_dir` differs from `canonical_dir`, we always produce a display
/// path (since the file's apparent location differs from its canonical one).
/// Otherwise, fall back to `make_display_path` behavior.
fn make_display_path_with_dir(
    display_dir: &Path,
    canonical_dir: &Path,
    raw_include_path: &str,
) -> Option<PathBuf> {
    if display_dir != canonical_dir {
        // Display dir differs from canonical — always produce a display path
        let dir_str = display_dir.to_string_lossy();
        let sep = if dir_str.ends_with('/') { "" } else { "/" };
        Some(PathBuf::from(format!(
            "{}{}{}",
            dir_str, sep, raw_include_path
        )))
    } else {
        make_display_path(display_dir, raw_include_path)
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
        let result = paths.resolve_quoted("foo.h", dir.path(), None);
        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(r.path, header);
        assert!(!r.is_system);
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
        let r = result.unwrap();
        assert_eq!(r.path, header);
        assert!(!r.is_system);
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
        let r = result.unwrap();
        assert_eq!(r.path, header);
        assert!(r.is_system);
    }

    #[test]
    fn test_resolve_not_found() {
        let paths = SearchPaths::default();
        assert_eq!(paths.resolve_angle("nonexistent.h"), None);
    }
}
