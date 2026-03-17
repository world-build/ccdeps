//! Dependency graph construction.
//!
//! Builds the transitive dependency graph for a translation unit by
//! recursively resolving includes. Uses include guard / `#pragma once`
//! detection to avoid re-processing files.

use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::directive::{self, Directive, IncludeStyle};
use crate::minimize;
use crate::resolve::SearchPaths;

/// Cached information about a scanned file.
#[derive(Debug)]
pub struct FileInfo {
    /// Minimized source content.
    pub minimized: minimize::MinimizedSource,
    /// Original file size (for stats).
    pub original_size: usize,
}

/// Thread-safe cache of scanned files.
pub type FileCache = Arc<DashMap<PathBuf, Arc<FileInfo>>>;

/// Create a new empty file cache.
pub fn new_file_cache() -> FileCache {
    Arc::new(DashMap::new())
}

/// Build the dependency set for a single translation unit.
///
/// Returns the list of all files (headers) that the source transitively depends on.
pub fn collect_deps(
    source_path: &Path,
    search_paths: &SearchPaths,
    cache: &FileCache,
) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut deps = Vec::new();
    let mut visited = std::collections::HashSet::new();

    let canonical = std::fs::canonicalize(source_path)?;
    visited.insert(canonical.clone());

    collect_deps_recursive(&canonical, search_paths, cache, &mut deps, &mut visited)?;

    Ok(deps)
}

fn collect_deps_recursive(
    file_path: &Path,
    search_paths: &SearchPaths,
    cache: &FileCache,
    deps: &mut Vec<PathBuf>,
    visited: &mut std::collections::HashSet<PathBuf>,
) -> Result<(), std::io::Error> {
    let info = get_or_scan(file_path, cache)?;

    // Parse directives from minimized source
    let content = String::from_utf8_lossy(&info.minimized.content);
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(Directive::Include(inc)) = directive::parse_directive(line) {
            let resolved = match inc.style {
                IncludeStyle::Quoted => {
                    let dir = file_path.parent().unwrap_or(Path::new("."));
                    search_paths.resolve_quoted(inc.path, dir)
                }
                IncludeStyle::Angle => search_paths.resolve_angle(inc.path),
                IncludeStyle::Computed => {
                    // Cannot resolve computed includes without macro evaluation
                    continue;
                }
            };

            if let Some(resolved) = resolved {
                let canonical = match std::fs::canonicalize(&resolved) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                if visited.contains(&canonical) {
                    continue;
                }

                // Check include guard / pragma once
                if let Some(cached) = cache.get(&canonical) {
                    if cached.minimized.has_pragma_once || cached.minimized.has_include_guard {
                        // Already processed and guarded — just add to deps, don't recurse
                        if !deps.contains(&canonical) {
                            deps.push(canonical);
                        }
                        continue;
                    }
                }

                visited.insert(canonical.clone());
                deps.push(canonical.clone());

                collect_deps_recursive(&canonical, search_paths, cache, deps, visited)?;
            }
        }
    }

    Ok(())
}

/// Get file info from cache, or scan and cache it.
fn get_or_scan(path: &Path, cache: &FileCache) -> Result<Arc<FileInfo>, std::io::Error> {
    if let Some(info) = cache.get(path) {
        return Ok(Arc::clone(&info));
    }

    let source = std::fs::read(path)?;
    let original_size = source.len();
    let minimized = minimize::minimize(&source);

    let info = Arc::new(FileInfo {
        minimized,
        original_size,
    });

    cache.insert(path.to_path_buf(), Arc::clone(&info));
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_simple_dependency_chain() {
        let dir = tempfile::tempdir().unwrap();

        // Create a.h -> b.h -> c.h
        fs::write(dir.path().join("c.h"), "// c\n").unwrap();
        fs::write(dir.path().join("b.h"), "#include \"c.h\"\n").unwrap();
        fs::write(dir.path().join("a.c"), "#include \"b.h\"\n").unwrap();

        let search_paths = SearchPaths::default();
        let cache = new_file_cache();

        let deps = collect_deps(&dir.path().join("a.c"), &search_paths, &cache).unwrap();
        assert_eq!(deps.len(), 2); // b.h, c.h
    }

    #[test]
    fn test_include_guard_prevents_reprocessing() {
        let dir = tempfile::tempdir().unwrap();

        fs::write(
            dir.path().join("guarded.h"),
            "#ifndef GUARDED_H\n#define GUARDED_H\n// content\n#endif\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("a.h"),
            "#include \"guarded.h\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#include \"guarded.h\"\n#include \"a.h\"\n",
        )
        .unwrap();

        let search_paths = SearchPaths::default();
        let cache = new_file_cache();

        let deps = collect_deps(&dir.path().join("main.c"), &search_paths, &cache).unwrap();
        // guarded.h should appear once, a.h once
        let guarded_count = deps
            .iter()
            .filter(|p| p.file_name().unwrap() == "guarded.h")
            .count();
        assert_eq!(guarded_count, 1);
    }
}
