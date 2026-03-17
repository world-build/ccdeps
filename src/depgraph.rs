//! Dependency graph construction.
//!
//! Builds the transitive dependency graph for a translation unit by
//! recursively resolving includes. Uses include guard / `#pragma once`
//! detection to avoid re-processing files.
//!
//! Evaluates `#if`/`#ifdef`/`#ifndef` conditionals using the macro table
//! and Pratt evaluator so that only the taken branch's includes are followed.

use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::directive::{self, Directive, IncludeStyle};
use crate::eval;
use crate::intern::PathInterner;
use crate::macros::MacroTable;
use crate::minimize;
use crate::resolve::{ResolvedInclude, SearchPaths};

/// Files at or above this size use memory-mapped I/O. Below this threshold,
/// a plain `read` is faster because mmap has fixed per-call overhead (~2 µs
/// for the VMA setup + page fault) that dominates for tiny files.
const MMAP_THRESHOLD: u64 = 4096;

/// Cached information about a scanned file.
#[derive(Debug)]
pub struct FileInfo {
    /// Minimized source content.
    pub minimized: minimize::MinimizedSource,
    /// Original file size (for stats).
    pub original_size: usize,
}

/// A dependency entry with metadata about how it was resolved.
#[derive(Debug, Clone)]
pub struct DepEntry {
    /// The canonical file path (interned to avoid redundant allocations).
    pub path: Arc<Path>,
    /// Whether this dependency was found via a system include path (`-isystem`).
    pub is_system: bool,
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
/// `initial_defines` provides command-line `-D` definitions.
pub fn collect_deps(
    source_path: &Path,
    search_paths: &SearchPaths,
    cache: &FileCache,
    initial_defines: &[(String, Option<String>)],
) -> Result<Vec<DepEntry>, std::io::Error> {
    let mut deps = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut macros = MacroTable::new();
    let interner = PathInterner::new();

    // Apply command-line defines
    for (name, value) in initial_defines {
        macros.define(name, value.as_deref());
    }

    let canonical = std::fs::canonicalize(source_path)?;
    visited.insert(canonical.clone());

    collect_deps_recursive(
        &canonical,
        search_paths,
        cache,
        &mut deps,
        &mut visited,
        &mut macros,
        &interner,
    )?;

    Ok(deps)
}

// ---------------------------------------------------------------------------
// Conditional stack
// ---------------------------------------------------------------------------

/// State for one level of `#if`/`#ifdef`/`#ifndef` nesting.
struct ConditionalState {
    /// Whether any branch in this `#if`/`#elif`/`#else` chain has been taken.
    any_branch_taken: bool,
    /// Whether the current branch is active (should process directives).
    active: bool,
    /// Whether the enclosing scope was active.
    parent_active: bool,
}

struct ConditionalStack {
    stack: Vec<ConditionalState>,
}

impl ConditionalStack {
    fn new() -> Self {
        Self { stack: Vec::new() }
    }

    /// Whether directives at the current position should be processed.
    fn is_active(&self) -> bool {
        self.stack.last().map_or(true, |s| s.active)
    }

    fn push_if(&mut self, condition: bool) {
        let parent_active = self.is_active();
        let active = parent_active && condition;
        self.stack.push(ConditionalState {
            any_branch_taken: active,
            active,
            parent_active,
        });
    }

    fn handle_elif(&mut self, condition: bool) {
        if let Some(state) = self.stack.last_mut() {
            if state.any_branch_taken || !state.parent_active {
                state.active = false;
            } else if condition {
                state.active = true;
                state.any_branch_taken = true;
            } else {
                state.active = false;
            }
        }
    }

    fn handle_else(&mut self) {
        if let Some(state) = self.stack.last_mut() {
            state.active = state.parent_active && !state.any_branch_taken;
            state.any_branch_taken = true;
        }
    }

    fn pop_endif(&mut self) {
        self.stack.pop();
    }
}

// ---------------------------------------------------------------------------
// Recursive dependency collection
// ---------------------------------------------------------------------------

fn collect_deps_recursive(
    file_path: &Path,
    search_paths: &SearchPaths,
    cache: &FileCache,
    deps: &mut Vec<DepEntry>,
    visited: &mut std::collections::HashSet<PathBuf>,
    macros: &mut MacroTable,
    interner: &PathInterner,
) -> Result<(), std::io::Error> {
    let info = get_or_scan(file_path, cache)?;

    let content = String::from_utf8_lossy(&info.minimized.content);
    let mut cond_stack = ConditionalStack::new();
    let current_dir = file_path.parent().unwrap_or(Path::new("."));
    let eval_ctx = eval::EvalContext {
        search_paths,
        current_file_dir: current_dir,
    };

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let directive = match directive::parse_directive(line) {
            Some(d) => d,
            None => continue,
        };

        // Conditional directives must always be processed (for nesting tracking),
        // even when we're in a skipped branch.
        match directive {
            Directive::If { expr } => {
                let condition = if cond_stack.is_active() {
                    eval::evaluate_with_context(expr, macros, &eval_ctx) != 0
                } else {
                    false
                };
                cond_stack.push_if(condition);
                continue;
            }
            Directive::Ifdef { name } => {
                let condition = if cond_stack.is_active() {
                    macros.is_defined(name)
                } else {
                    false
                };
                cond_stack.push_if(condition);
                continue;
            }
            Directive::Ifndef { name } => {
                let condition = if cond_stack.is_active() {
                    !macros.is_defined(name)
                } else {
                    false
                };
                cond_stack.push_if(condition);
                continue;
            }
            Directive::Elif { expr } => {
                let condition = eval::evaluate_with_context(expr, macros, &eval_ctx) != 0;
                cond_stack.handle_elif(condition);
                continue;
            }
            Directive::Else => {
                cond_stack.handle_else();
                continue;
            }
            Directive::Endif => {
                cond_stack.pop_endif();
                continue;
            }
            _ => {}
        }

        // All other directives are only processed when active.
        if !cond_stack.is_active() {
            continue;
        }

        match directive {
            Directive::Define { name, value } => {
                // Detect function-like macros: value starts with '(' immediately
                // after the name (no space). `#define FOO(x) x+1` → value="(x) x+1"
                // `#define FOO (x)` → value=" (x)" (space = object-like)
                if let Some(val) = value {
                    if val.starts_with('(') {
                        macros.define_function_like(name, val);
                    } else {
                        macros.define(name, Some(val.trim_start()));
                    }
                } else {
                    macros.define(name, None);
                }
            }
            Directive::Undef { name } => {
                macros.undef(name);
            }
            Directive::Include(inc) => {
                let resolved = match inc.style {
                    IncludeStyle::Quoted => {
                        let dir = file_path.parent().unwrap_or(Path::new("."));
                        search_paths.resolve_quoted(inc.path, dir)
                    }
                    IncludeStyle::Angle => search_paths.resolve_angle(inc.path),
                    IncludeStyle::Computed => {
                        // Macro-expand the tokens and re-parse as "path" or <path>
                        let expanded = macros.expand(inc.path);
                        let expanded = expanded.trim();
                        if let Some(path) = extract_include_path(expanded) {
                            let is_angle = expanded.starts_with('<');
                            if is_angle {
                                search_paths.resolve_angle(&path)
                            } else {
                                let dir = file_path.parent().unwrap_or(Path::new("."));
                                search_paths.resolve_quoted(&path, dir)
                            }
                        } else {
                            continue;
                        }
                    }
                };

                if let Some(ResolvedInclude { path: resolved_path, is_system }) = resolved {
                    let canonical = match std::fs::canonicalize(&resolved_path) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };

                    if visited.contains(&canonical) {
                        continue;
                    }

                    // Check include guard / pragma once
                    if let Some(cached) = cache.get(&canonical) {
                        let dominated = if cached.minimized.has_pragma_once {
                            true
                        } else if cached.minimized.has_include_guard {
                            // Guard is dominated if already defined
                            if let Some(ref guard) = cached.minimized.guard_macro {
                                macros.is_defined(guard)
                            } else {
                                false
                            }
                        } else {
                            false
                        };

                        if dominated {
                            let interned = interner.intern(&canonical);
                            if !deps.iter().any(|d| *d.path == *interned) {
                                deps.push(DepEntry { path: interned, is_system });
                            }
                            continue;
                        }
                    }

                    visited.insert(canonical.clone());
                    let interned = interner.intern_owned(canonical.clone());
                    deps.push(DepEntry { path: interned, is_system });

                    collect_deps_recursive(
                        &canonical,
                        search_paths,
                        cache,
                        deps,
                        visited,
                        macros,
                        interner,
                    )?;
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// Extract the path from an expanded include token sequence.
/// Handles `"path"` and `<path>` forms.
fn extract_include_path(expanded: &str) -> Option<String> {
    let s = expanded.trim();
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        Some(s[1..s.len() - 1].to_string())
    } else if s.starts_with('<') && s.ends_with('>') && s.len() >= 2 {
        Some(s[1..s.len() - 1].to_string())
    } else {
        None
    }
}

/// Get file info from cache, or scan and cache it.
///
/// Uses memory-mapped I/O for files >= 4 KB (avoids the allocation + copy of
/// `std::fs::read`) and plain `read` for smaller files (avoids mmap's fixed
/// VMA setup overhead).
fn get_or_scan(path: &Path, cache: &FileCache) -> Result<Arc<FileInfo>, std::io::Error> {
    if let Some(info) = cache.get(path) {
        return Ok(Arc::clone(&info));
    }

    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    let file_size = metadata.len();

    let minimized = if file_size >= MMAP_THRESHOLD {
        // Large file: memory-map for zero-copy access.
        // SAFETY: We hold the file open for the duration of the mmap, and the
        // minimizer only reads the mapped region. Concurrent modification by
        // another process would be a pre-existing build-system bug.
        let mmap = unsafe { memmap2::Mmap::map(&file)? };

        // Hint to the kernel that we'll read sequentially.
        #[cfg(unix)]
        {
            mmap.advise(memmap2::Advice::Sequential).ok();
        }

        let result = minimize::minimize(&mmap);
        // mmap is dropped here, unmapping the region
        result
    } else {
        // Small file: plain read is faster than mmap overhead.
        let source = std::fs::read(path)?;
        minimize::minimize(&source)
    };

    let info = Arc::new(FileInfo {
        minimized,
        original_size: file_size as usize,
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

        let deps =
            collect_deps(&dir.path().join("a.c"), &search_paths, &cache, &[]).unwrap();
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
        fs::write(dir.path().join("a.h"), "#include \"guarded.h\"\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#include \"guarded.h\"\n#include \"a.h\"\n",
        )
        .unwrap();

        let search_paths = SearchPaths::default();
        let cache = new_file_cache();

        let deps =
            collect_deps(&dir.path().join("main.c"), &search_paths, &cache, &[]).unwrap();
        let guarded_count = deps
            .iter()
            .filter(|d| d.path.file_name().unwrap() == "guarded.h")
            .count();
        assert_eq!(guarded_count, 1);
    }

    #[test]
    fn test_conditional_ifdef_taken() {
        let dir = tempfile::tempdir().unwrap();

        fs::write(dir.path().join("yes.h"), "// yes\n").unwrap();
        fs::write(dir.path().join("no.h"), "// no\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#define USE_YES\n#ifdef USE_YES\n#include \"yes.h\"\n#else\n#include \"no.h\"\n#endif\n",
        )
        .unwrap();

        let search_paths = SearchPaths::default();
        let cache = new_file_cache();

        let deps =
            collect_deps(&dir.path().join("main.c"), &search_paths, &cache, &[]).unwrap();
        let names: Vec<_> = deps
            .iter()
            .map(|d| d.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"yes.h".to_string()));
        assert!(!names.contains(&"no.h".to_string()));
    }

    #[test]
    fn test_conditional_if_expr() {
        let dir = tempfile::tempdir().unwrap();

        fs::write(dir.path().join("yes.h"), "// yes\n").unwrap();
        fs::write(dir.path().join("no.h"), "// no\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#define USE_YES 1\n#if USE_YES\n#include \"yes.h\"\n#else\n#include \"no.h\"\n#endif\n",
        )
        .unwrap();

        let search_paths = SearchPaths::default();
        let cache = new_file_cache();

        let deps =
            collect_deps(&dir.path().join("main.c"), &search_paths, &cache, &[]).unwrap();
        let names: Vec<_> = deps
            .iter()
            .map(|d| d.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"yes.h".to_string()));
        assert!(!names.contains(&"no.h".to_string()));
    }

    #[test]
    fn test_conditional_ifndef() {
        let dir = tempfile::tempdir().unwrap();

        fs::write(dir.path().join("yes.h"), "// yes\n").unwrap();
        fs::write(dir.path().join("no.h"), "// no\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#ifndef UNDEFINED_MACRO\n#include \"yes.h\"\n#else\n#include \"no.h\"\n#endif\n",
        )
        .unwrap();

        let search_paths = SearchPaths::default();
        let cache = new_file_cache();

        let deps =
            collect_deps(&dir.path().join("main.c"), &search_paths, &cache, &[]).unwrap();
        let names: Vec<_> = deps
            .iter()
            .map(|d| d.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"yes.h".to_string()));
        assert!(!names.contains(&"no.h".to_string()));
    }

    #[test]
    fn test_conditional_elif() {
        let dir = tempfile::tempdir().unwrap();

        fs::write(dir.path().join("a.h"), "// a\n").unwrap();
        fs::write(dir.path().join("b.h"), "// b\n").unwrap();
        fs::write(dir.path().join("c.h"), "// c\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#define X 2\n#if X == 1\n#include \"a.h\"\n#elif X == 2\n#include \"b.h\"\n#else\n#include \"c.h\"\n#endif\n",
        )
        .unwrap();

        let search_paths = SearchPaths::default();
        let cache = new_file_cache();

        let deps =
            collect_deps(&dir.path().join("main.c"), &search_paths, &cache, &[]).unwrap();
        let names: Vec<_> = deps
            .iter()
            .map(|d| d.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(!names.contains(&"a.h".to_string()));
        assert!(names.contains(&"b.h".to_string()));
        assert!(!names.contains(&"c.h".to_string()));
    }

    #[test]
    fn test_nested_conditionals() {
        let dir = tempfile::tempdir().unwrap();

        fs::write(dir.path().join("inner.h"), "// inner\n").unwrap();
        fs::write(dir.path().join("skip.h"), "// skip\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#define OUTER 1\n#if OUTER\n#define INNER 1\n#if INNER\n#include \"inner.h\"\n#endif\n#else\n#include \"skip.h\"\n#endif\n",
        )
        .unwrap();

        let search_paths = SearchPaths::default();
        let cache = new_file_cache();

        let deps =
            collect_deps(&dir.path().join("main.c"), &search_paths, &cache, &[]).unwrap();
        let names: Vec<_> = deps
            .iter()
            .map(|d| d.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"inner.h".to_string()));
        assert!(!names.contains(&"skip.h".to_string()));
    }

    #[test]
    fn test_cmdline_defines() {
        let dir = tempfile::tempdir().unwrap();

        fs::write(dir.path().join("yes.h"), "// yes\n").unwrap();
        fs::write(dir.path().join("no.h"), "// no\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#ifdef FEATURE\n#include \"yes.h\"\n#else\n#include \"no.h\"\n#endif\n",
        )
        .unwrap();

        let search_paths = SearchPaths::default();
        let cache = new_file_cache();
        let defines = vec![("FEATURE".to_string(), None)];

        let deps = collect_deps(
            &dir.path().join("main.c"),
            &search_paths,
            &cache,
            &defines,
        )
        .unwrap();
        let names: Vec<_> = deps
            .iter()
            .map(|d| d.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"yes.h".to_string()));
        assert!(!names.contains(&"no.h".to_string()));
    }

    #[test]
    fn test_define_in_header_visible_after_include() {
        let dir = tempfile::tempdir().unwrap();

        fs::write(
            dir.path().join("config.h"),
            "#define HAS_FEATURE 1\n",
        )
        .unwrap();
        fs::write(dir.path().join("feature.h"), "// feature\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#include \"config.h\"\n#if HAS_FEATURE\n#include \"feature.h\"\n#endif\n",
        )
        .unwrap();

        let search_paths = SearchPaths::default();
        let cache = new_file_cache();

        let deps =
            collect_deps(&dir.path().join("main.c"), &search_paths, &cache, &[]).unwrap();
        let names: Vec<_> = deps
            .iter()
            .map(|d| d.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"config.h".to_string()));
        assert!(names.contains(&"feature.h".to_string()));
    }

    #[test]
    fn test_computed_include() {
        let dir = tempfile::tempdir().unwrap();

        fs::write(dir.path().join("target.h"), "// target\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#define HEADER \"target.h\"\n#include HEADER\n",
        )
        .unwrap();

        let search_paths = SearchPaths::default();
        let cache = new_file_cache();

        let deps =
            collect_deps(&dir.path().join("main.c"), &search_paths, &cache, &[]).unwrap();
        let names: Vec<_> = deps
            .iter()
            .map(|d| d.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"target.h".to_string()));
    }

    #[test]
    fn test_computed_include_chained() {
        let dir = tempfile::tempdir().unwrap();

        fs::write(dir.path().join("real.h"), "// real\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#define A B\n#define B \"real.h\"\n#include A\n",
        )
        .unwrap();

        let search_paths = SearchPaths::default();
        let cache = new_file_cache();

        let deps =
            collect_deps(&dir.path().join("main.c"), &search_paths, &cache, &[]).unwrap();
        let names: Vec<_> = deps
            .iter()
            .map(|d| d.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"real.h".to_string()));
    }

    #[test]
    fn test_function_like_define_in_if() {
        let dir = tempfile::tempdir().unwrap();

        fs::write(dir.path().join("yes.h"), "// yes\n").unwrap();
        fs::write(dir.path().join("no.h"), "// no\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#define CHECK(x) x\n#if CHECK(1)\n#include \"yes.h\"\n#else\n#include \"no.h\"\n#endif\n",
        )
        .unwrap();

        let search_paths = SearchPaths::default();
        let cache = new_file_cache();

        let deps =
            collect_deps(&dir.path().join("main.c"), &search_paths, &cache, &[]).unwrap();
        let names: Vec<_> = deps
            .iter()
            .map(|d| d.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"yes.h".to_string()));
        assert!(!names.contains(&"no.h".to_string()));
    }

    #[test]
    fn test_has_include_directive() {
        let dir = tempfile::tempdir().unwrap();

        fs::write(dir.path().join("exists.h"), "// exists\n").unwrap();
        fs::write(dir.path().join("fallback.h"), "// fallback\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#if __has_include(\"exists.h\")\n#include \"exists.h\"\n#else\n#include \"fallback.h\"\n#endif\n",
        )
        .unwrap();

        let search_paths = SearchPaths::default();
        let cache = new_file_cache();

        let deps =
            collect_deps(&dir.path().join("main.c"), &search_paths, &cache, &[]).unwrap();
        let names: Vec<_> = deps
            .iter()
            .map(|d| d.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"exists.h".to_string()));
        assert!(!names.contains(&"fallback.h".to_string()));
    }
}
