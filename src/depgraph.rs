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

use crate::directive::{self, Directive, IncludeDirective, IncludeStyle};
use crate::eval;
use crate::intern::PathInterner;
use crate::macros::MacroTable;
use crate::minimize::{self, MinimizeOptions};
use crate::resolve::{ResolvedInclude, SearchPaths};
use crate::vfs::Vfs;

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
    /// The canonical file path (interned, used for deduplication).
    pub path: Arc<Path>,
    /// The path to display in output (preserves `../` and `./` as gcc does).
    /// If `None`, `path` is used for display.
    pub display_path: Option<PathBuf>,
    /// Whether this dependency was found via a system include path (`-isystem`).
    pub is_system: bool,
}

/// Thread-safe cache of scanned files.
pub type FileCache = Arc<DashMap<PathBuf, Arc<FileInfo>>>;

/// Options for dependency collection.
#[derive(Debug, Clone, Default)]
pub struct DepCollectOptions {
    /// Treat missing headers as generated (`-MG`).
    pub missing_as_generated: bool,
    /// Process trigraphs in source files.
    pub trigraphs: bool,
}

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
    vfs: &dyn Vfs,
    search_paths: &SearchPaths,
    cache: &FileCache,
    initial_defines: &[(String, Option<String>)],
    options: &DepCollectOptions,
) -> Result<Vec<DepEntry>, std::io::Error> {
    let mut deps = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut macros = MacroTable::new();
    let interner = PathInterner::new();
    let mut import_once = std::collections::HashSet::new();
    let minimize_opts = MinimizeOptions {
        trigraphs: options.trigraphs,
    };

    // Apply command-line defines
    for (name, value) in initial_defines {
        macros.define(name, value.as_deref());
    }

    let canonical = vfs.canonicalize(source_path)?;
    visited.insert(canonical.clone());

    collect_deps_recursive(
        &canonical,
        vfs,
        search_paths,
        cache,
        &mut deps,
        &mut visited,
        &mut macros,
        &interner,
        &mut import_once,
        options,
        &minimize_opts,
        false, // is_system_header
        None,  // display_dir (root file has no display override)
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
        self.stack.last().is_none_or(|s| s.active)
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

#[allow(clippy::too_many_arguments)]
fn collect_deps_recursive(
    file_path: &Path,
    vfs: &dyn Vfs,
    search_paths: &SearchPaths,
    cache: &FileCache,
    deps: &mut Vec<DepEntry>,
    visited: &mut std::collections::HashSet<PathBuf>,
    macros: &mut MacroTable,
    interner: &PathInterner,
    import_once: &mut std::collections::HashSet<PathBuf>,
    options: &DepCollectOptions,
    minimize_opts: &MinimizeOptions,
    mut is_system_header: bool,
    display_dir: Option<&Path>,
) -> Result<(), std::io::Error> {
    let info = get_or_scan(file_path, vfs, cache, minimize_opts)?;

    let content = String::from_utf8_lossy(&info.minimized.content);
    let mut cond_stack = ConditionalStack::new();
    let current_dir = file_path.parent().unwrap_or(Path::new("."));
    let eval_ctx = eval::EvalContext {
        search_paths,
        current_file_dir: current_dir,
        vfs,
    };

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Join line continuations so split keywords like `#inc\nlude` work
        let joined;
        let line = if line.contains("\\\n") {
            joined = minimize::join_line_continuations(line);
            joined.as_str()
        } else {
            line
        };

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
            Directive::Pragma { tokens } => {
                handle_pragma(tokens, macros, &mut is_system_header);
            }
            Directive::Include(ref inc) => {
                handle_include(
                    inc,
                    file_path,
                    vfs,
                    search_paths,
                    cache,
                    deps,
                    visited,
                    macros,
                    interner,
                    import_once,
                    options,
                    minimize_opts,
                    is_system_header,
                    false, // is_import
                    display_dir,
                )?;
            }
            Directive::IncludeNext(ref inc) => {
                // #include_next: resolve starting from the next search directory
                let resolved = search_paths.resolve_include_next(inc.path, current_dir, vfs);
                if let Some(ResolvedInclude {
                    path: resolved_path,
                    is_system,
                    display_path,
                }) = resolved
                {
                    process_resolved_include(
                        &resolved_path,
                        is_system || is_system_header,
                        display_path,
                        vfs,
                        search_paths,
                        cache,
                        deps,
                        visited,
                        macros,
                        interner,
                        import_once,
                        options,
                        minimize_opts,
                        false,
                    )?;
                } else if options.missing_as_generated {
                    add_missing_dep(inc.path, deps, interner);
                }
            }
            Directive::Import(ref inc) => {
                handle_include(
                    inc,
                    file_path,
                    vfs,
                    search_paths,
                    cache,
                    deps,
                    visited,
                    macros,
                    interner,
                    import_once,
                    options,
                    minimize_opts,
                    is_system_header,
                    true, // is_import
                    display_dir,
                )?;
            }
            Directive::Embed(ref inc) => {
                // #embed is like #include for dependency purposes — resolve and add
                // to deps, but don't recurse (it embeds binary data, not source).
                let resolved = match inc.style {
                    IncludeStyle::Quoted => {
                        let dir = file_path.parent().unwrap_or(Path::new("."));
                        search_paths.resolve_quoted(inc.path, dir, display_dir, vfs)
                    }
                    IncludeStyle::Angle => search_paths.resolve_angle(inc.path, vfs),
                    IncludeStyle::Computed => {
                        let expanded = macros.expand(inc.path);
                        let expanded = expanded.trim().to_string();
                        if let Some(path) = extract_include_path(&expanded) {
                            let is_angle = expanded.starts_with('<');
                            if is_angle {
                                search_paths.resolve_angle(&path, vfs)
                            } else {
                                let dir = file_path.parent().unwrap_or(Path::new("."));
                                search_paths.resolve_quoted(&path, dir, display_dir, vfs)
                            }
                        } else {
                            None
                        }
                    }
                };
                if let Some(ResolvedInclude {
                    path: resolved_path,
                    is_system,
                    display_path: res_display,
                }) = resolved
                {
                    let canonical = match vfs.canonicalize(&resolved_path) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    let display_path = res_display.or_else(|| {
                        if resolved_path != canonical {
                            Some(resolved_path)
                        } else {
                            None
                        }
                    });
                    let interned_path = interner.intern(&canonical);
                    if !deps.iter().any(|d| *d.path == *interned_path) {
                        deps.push(DepEntry {
                            path: interned_path,
                            display_path,
                            is_system: is_system || is_system_header,
                        });
                    }
                } else if options.missing_as_generated {
                    add_missing_dep(inc.path, deps, interner);
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// Handle `#pragma` directives that affect dependency scanning.
fn handle_pragma(tokens: &str, macros: &mut MacroTable, is_system_header: &mut bool) {
    let tokens = tokens.trim();

    // #pragma GCC system_header
    if tokens == "GCC system_header" || tokens == "clang system_header" {
        *is_system_header = true;
        return;
    }

    // #pragma push_macro("name")
    if let Some(rest) = tokens.strip_prefix("push_macro") {
        if let Some(name) = extract_pragma_macro_name(rest) {
            macros.push_macro(name);
        }
        return;
    }

    // #pragma pop_macro("name")
    if let Some(rest) = tokens.strip_prefix("pop_macro") {
        if let Some(name) = extract_pragma_macro_name(rest) {
            macros.pop_macro(name);
        }
    }
}

/// Extract macro name from `("name")` in pragma push_macro/pop_macro.
fn extract_pragma_macro_name(s: &str) -> Option<&str> {
    let s = s.trim();
    let s = s.strip_prefix('(')?;
    let s = s.trim();
    let s = s.strip_prefix('"')?;
    let end = s.find('"')?;
    Some(&s[..end])
}

/// Handle an `#include` or `#import` directive.
#[allow(clippy::too_many_arguments)]
fn handle_include(
    inc: &IncludeDirective<'_>,
    file_path: &Path,
    vfs: &dyn Vfs,
    search_paths: &SearchPaths,
    cache: &FileCache,
    deps: &mut Vec<DepEntry>,
    visited: &mut std::collections::HashSet<PathBuf>,
    macros: &mut MacroTable,
    interner: &PathInterner,
    import_once: &mut std::collections::HashSet<PathBuf>,
    options: &DepCollectOptions,
    minimize_opts: &MinimizeOptions,
    is_system_header: bool,
    is_import: bool,
    display_dir: Option<&Path>,
) -> Result<(), std::io::Error> {
    let resolved = match inc.style {
        IncludeStyle::Quoted => {
            let dir = file_path.parent().unwrap_or(Path::new("."));
            search_paths.resolve_quoted(inc.path, dir, display_dir, vfs)
        }
        IncludeStyle::Angle => search_paths.resolve_angle(inc.path, vfs),
        IncludeStyle::Computed => {
            let expanded = macros.expand(inc.path);
            let expanded = expanded.trim().to_string();
            if let Some(path) = extract_include_path(&expanded) {
                let is_angle = expanded.starts_with('<');
                if is_angle {
                    search_paths.resolve_angle(&path, vfs)
                } else {
                    let dir = file_path.parent().unwrap_or(Path::new("."));
                    search_paths.resolve_quoted(&path, dir, display_dir, vfs)
                }
            } else {
                return Ok(());
            }
        }
    };

    if let Some(ResolvedInclude {
        path: resolved_path,
        is_system,
        display_path,
    }) = resolved
    {
        process_resolved_include(
            &resolved_path,
            is_system || is_system_header,
            display_path,
            vfs,
            search_paths,
            cache,
            deps,
            visited,
            macros,
            interner,
            import_once,
            options,
            minimize_opts,
            is_import,
        )?;
    } else if options.missing_as_generated {
        add_missing_dep(inc.path, deps, interner);
    }

    Ok(())
}

/// Process a resolved include path: check guards, add to deps, recurse.
#[allow(clippy::too_many_arguments)]
fn process_resolved_include(
    resolved_path: &Path,
    is_system: bool,
    resolved_display: Option<PathBuf>,
    vfs: &dyn Vfs,
    search_paths: &SearchPaths,
    cache: &FileCache,
    deps: &mut Vec<DepEntry>,
    visited: &mut std::collections::HashSet<PathBuf>,
    macros: &mut MacroTable,
    interner: &PathInterner,
    import_once: &mut std::collections::HashSet<PathBuf>,
    options: &DepCollectOptions,
    minimize_opts: &MinimizeOptions,
    is_import: bool,
) -> Result<(), std::io::Error> {
    let canonical = match vfs.canonicalize(resolved_path) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };

    // Use the display path from resolution if available, otherwise fall back
    // to the resolved path if it differs from canonical.
    let display_path = resolved_display.or_else(|| {
        if resolved_path != canonical {
            Some(resolved_path.to_path_buf())
        } else {
            None
        }
    });

    // #import: skip if already imported
    if import_once.contains(&canonical) {
        let interned = interner.intern(&canonical);
        if !deps.iter().any(|d| *d.path == *interned) {
            deps.push(DepEntry {
                path: interned,
                display_path: display_path.clone(),
                is_system,
            });
        }
        return Ok(());
    }

    if visited.contains(&canonical) {
        return Ok(());
    }

    // Check include guard / pragma once
    if let Some(cached) = cache.get(&canonical) {
        let dominated = if cached.minimized.has_pragma_once {
            true
        } else if cached.minimized.has_include_guard {
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
                deps.push(DepEntry {
                    path: interned,
                    display_path: display_path.clone(),
                    is_system,
                });
            }
            return Ok(());
        }
    }

    visited.insert(canonical.clone());

    // Mark as import-once if this is #import
    if is_import {
        import_once.insert(canonical.clone());
    }

    let interned = interner.intern_owned(canonical.clone());
    deps.push(DepEntry {
        path: interned,
        display_path: display_path.clone(),
        is_system,
    });

    // Compute the "apparent directory" for this file based on its display path.
    // This propagates non-canonical path segments (e.g., `inc/..`) so that
    // further includes from this file get correct display paths.
    let child_display_dir = display_path
        .as_ref()
        .and_then(|dp| dp.parent().map(|p| p.to_path_buf()));

    collect_deps_recursive(
        &canonical,
        vfs,
        search_paths,
        cache,
        deps,
        visited,
        macros,
        interner,
        import_once,
        options,
        minimize_opts,
        is_system,
        child_display_dir.as_deref(),
    )?;

    Ok(())
}

/// Add a missing header as a generated dependency (`-MG`).
fn add_missing_dep(path: &str, deps: &mut Vec<DepEntry>, interner: &PathInterner) {
    let p = Path::new(path);
    let interned = interner.intern(p);
    if !deps.iter().any(|d| *d.path == *interned) {
        deps.push(DepEntry {
            path: interned,
            display_path: None,
            is_system: false,
        });
    }
}

/// Extract the path from an expanded include token sequence.
/// Handles `"path"` and `<path>` forms.
fn extract_include_path(expanded: &str) -> Option<String> {
    let s = expanded.trim();
    let is_quoted = s.starts_with('"') && s.ends_with('"') && s.len() >= 2;
    let is_angle = s.starts_with('<') && s.ends_with('>') && s.len() >= 2;
    if is_quoted || is_angle {
        Some(s[1..s.len() - 1].to_string())
    } else {
        None
    }
}

/// Get file info from cache, or scan and cache it.
///
/// File contents are read through the [`Vfs`], which for the real filesystem
/// uses memory-mapped I/O for files >= 4 KB (avoids the allocation of
/// `std::fs::read`) and plain `read` for smaller files (avoids mmap's fixed
/// VMA setup overhead).
fn get_or_scan(
    path: &Path,
    vfs: &dyn Vfs,
    cache: &FileCache,
    minimize_opts: &MinimizeOptions,
) -> Result<Arc<FileInfo>, std::io::Error> {
    if let Some(info) = cache.get(path) {
        return Ok(Arc::clone(&info));
    }

    let source = vfs.read(path)?;
    let minimized = minimize::minimize_with_options(&source, minimize_opts);

    let info = Arc::new(FileInfo {
        minimized,
        original_size: source.len(),
    });

    cache.insert(path.to_path_buf(), Arc::clone(&info));
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::OsVfs;
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

        let deps = collect_deps(
            &dir.path().join("a.c"),
            &OsVfs,
            &search_paths,
            &cache,
            &[],
            &DepCollectOptions::default(),
        )
        .unwrap();
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

        let deps = collect_deps(
            &dir.path().join("main.c"),
            &OsVfs,
            &search_paths,
            &cache,
            &[],
            &DepCollectOptions::default(),
        )
        .unwrap();
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

        let deps = collect_deps(
            &dir.path().join("main.c"),
            &OsVfs,
            &search_paths,
            &cache,
            &[],
            &DepCollectOptions::default(),
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

        let deps = collect_deps(
            &dir.path().join("main.c"),
            &OsVfs,
            &search_paths,
            &cache,
            &[],
            &DepCollectOptions::default(),
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

        let deps = collect_deps(
            &dir.path().join("main.c"),
            &OsVfs,
            &search_paths,
            &cache,
            &[],
            &DepCollectOptions::default(),
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

        let deps = collect_deps(
            &dir.path().join("main.c"),
            &OsVfs,
            &search_paths,
            &cache,
            &[],
            &DepCollectOptions::default(),
        )
        .unwrap();
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

        let deps = collect_deps(
            &dir.path().join("main.c"),
            &OsVfs,
            &search_paths,
            &cache,
            &[],
            &DepCollectOptions::default(),
        )
        .unwrap();
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
            &OsVfs,
            &search_paths,
            &cache,
            &defines,
            &DepCollectOptions::default(),
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

        fs::write(dir.path().join("config.h"), "#define HAS_FEATURE 1\n").unwrap();
        fs::write(dir.path().join("feature.h"), "// feature\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#include \"config.h\"\n#if HAS_FEATURE\n#include \"feature.h\"\n#endif\n",
        )
        .unwrap();

        let search_paths = SearchPaths::default();
        let cache = new_file_cache();

        let deps = collect_deps(
            &dir.path().join("main.c"),
            &OsVfs,
            &search_paths,
            &cache,
            &[],
            &DepCollectOptions::default(),
        )
        .unwrap();
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

        let deps = collect_deps(
            &dir.path().join("main.c"),
            &OsVfs,
            &search_paths,
            &cache,
            &[],
            &DepCollectOptions::default(),
        )
        .unwrap();
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

        let deps = collect_deps(
            &dir.path().join("main.c"),
            &OsVfs,
            &search_paths,
            &cache,
            &[],
            &DepCollectOptions::default(),
        )
        .unwrap();
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

        let deps = collect_deps(
            &dir.path().join("main.c"),
            &OsVfs,
            &search_paths,
            &cache,
            &[],
            &DepCollectOptions::default(),
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

        let deps = collect_deps(
            &dir.path().join("main.c"),
            &OsVfs,
            &search_paths,
            &cache,
            &[],
            &DepCollectOptions::default(),
        )
        .unwrap();
        let names: Vec<_> = deps
            .iter()
            .map(|d| d.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"exists.h".to_string()));
        assert!(!names.contains(&"fallback.h".to_string()));
    }
}
