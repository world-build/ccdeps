//! Dependency graph walk.
//!
//! Walks a translation unit's include closure, following only the taken branch
//! of each conditional (or, in conservative mode, both branches of an
//! unknown one). The heavy per-file state lives in the [`ScanService`] (content
//! cache) and the reusable [`ScanScratch`](crate::ScanScratch); this module is
//! the traversal that ties them together and classifies each edge into the
//! [`ScanOutput`].

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::directive::{self, Directive, IncludeDirective, IncludeStyle};
use crate::eval::{self, Cond};
use crate::intern::{ContentHash, FileId};
use crate::macros::{MacroState, MacroTable};
use crate::minimize;
use crate::resolve::ResolvedInclude;
use crate::service::{FileInfo, ScanEnv, ScanOutput, ScanService, Visited};

/// Immutable references shared across the whole walk. Bundling them keeps the
/// mutable state (`macros`, `visited`, `cond_stack`, `out`, `observe`) as
/// separate `&mut` bindings so the borrow checker sees them as disjoint.
pub(crate) struct Ctx<'a> {
    pub service: &'a ScanService,
    pub env: &'a ScanEnv<'a>,
    pub vfs: &'a dyn crate::vfs::Vfs,
}

/// The observe callback: fired once per file whose content a TU depends on.
type Observe<'o> = dyn FnMut(FileId, ContentHash) + 'o;

// ---------------------------------------------------------------------------
// Conditional stack
// ---------------------------------------------------------------------------

/// State for one level of `#if`/`#ifdef`/`#ifndef` nesting.
///
/// In exact mode a condition is never [`Cond::Unknown`], so `active` follows
/// exactly one branch per chain. In conservative mode an Unknown condition
/// leaves a branch active (maybe-taken) without marking it definitely taken, so
/// later branches stay active too and both sides contribute dependencies.
pub(crate) struct ConditionalState {
    /// A branch in this chain has DEFINITELY been taken (True condition).
    definitely_taken: bool,
    /// Every condition seen so far in this chain was definitely False.
    all_prior_false: bool,
    /// The current branch should be processed (maybe- or definitely-taken).
    active: bool,
    /// The current branch is CERTAINLY the one real compilation takes.
    certain: bool,
    parent_active: bool,
    parent_certain: bool,
}

/// One file's conditionals occupy `cstack[base..]`; `base` is the stack length
/// when the file's walk began, so nested includes don't see each other's
/// conditionals. `last()` of that slice is the current level (or none → active).
fn cond_active(cstack: &[ConditionalState], base: usize) -> bool {
    cstack[base..].last().is_none_or(|s| s.active)
}

fn cond_certain(cstack: &[ConditionalState], base: usize) -> bool {
    cstack[base..].last().is_none_or(|s| s.certain)
}

fn cond_push_if(cstack: &mut Vec<ConditionalState>, base: usize, condition: Cond) {
    let parent_active = cond_active(cstack, base);
    let parent_certain = cond_certain(cstack, base);
    let active = parent_active && condition != Cond::False;
    cstack.push(ConditionalState {
        definitely_taken: parent_active && condition == Cond::True,
        all_prior_false: condition == Cond::False,
        active,
        certain: parent_certain && condition == Cond::True,
        parent_active,
        parent_certain,
    });
}

fn cond_elif(cstack: &mut [ConditionalState], base: usize, condition: Cond) {
    if cstack.len() <= base {
        return;
    }
    let state = cstack.last_mut().unwrap();
    if state.definitely_taken || !state.parent_active {
        state.active = false;
        state.certain = false;
    } else {
        state.active = condition != Cond::False;
        state.certain = state.parent_certain && state.all_prior_false && condition == Cond::True;
        state.definitely_taken |= condition == Cond::True;
        state.all_prior_false &= condition == Cond::False;
    }
}

fn cond_else(cstack: &mut [ConditionalState], base: usize) {
    if cstack.len() <= base {
        return;
    }
    let state = cstack.last_mut().unwrap();
    state.active = state.parent_active && !state.definitely_taken;
    state.certain = state.parent_certain && state.all_prior_false;
    state.definitely_taken = true;
}

fn cond_endif(cstack: &mut Vec<ConditionalState>, base: usize) {
    if cstack.len() > base {
        cstack.pop();
    }
}

// ---------------------------------------------------------------------------
// Walk
// ---------------------------------------------------------------------------

/// Walk one already-loaded file, following its directives. `info` is the file's
/// cached minimized form; `path` is its canonical path; `display_dir` carries a
/// non-canonical apparent directory for gcc-style display paths (CLI only).
#[allow(clippy::too_many_arguments)]
pub(crate) fn walk_file(
    ctx: &Ctx,
    macros: &mut MacroTable,
    visited: &mut Visited,
    cstack: &mut Vec<ConditionalState>,
    out: &mut ScanOutput,
    observe: &mut Observe,
    info: &FileInfo,
    path: &Path,
    mut is_system: bool,
    display_dir: Option<&Path>,
) {
    let base = cstack.len();
    let current_dir = path.parent().unwrap_or(Path::new("."));

    for line in info.directives.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Join split keywords like `#inc\nlude` (rare; see minimize).
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

        // Conditional directives are always processed (to track nesting), even
        // in a skipped branch.
        match directive {
            Directive::If { expr } => {
                let condition = if cond_active(cstack, base) {
                    let eval_ctx = eval::EvalContext {
                        search_paths: ctx.env.search_paths,
                        current_file_dir: current_dir,
                        vfs: ctx.vfs,
                    };
                    eval::evaluate_cond(expr, macros, &eval_ctx, ctx.env.conservative)
                } else {
                    Cond::False
                };
                cond_push_if(cstack, base, condition);
                continue;
            }
            Directive::Ifdef { name } => {
                let condition = if cond_active(cstack, base) {
                    match macros.state_of(name, ctx.env.conservative) {
                        MacroState::Defined => Cond::True,
                        MacroState::Undefined => Cond::False,
                        MacroState::Unknown => Cond::Unknown,
                    }
                } else {
                    Cond::False
                };
                cond_push_if(cstack, base, condition);
                continue;
            }
            Directive::Ifndef { name } => {
                let condition = if cond_active(cstack, base) {
                    match macros.state_of(name, ctx.env.conservative) {
                        MacroState::Defined => Cond::False,
                        MacroState::Undefined => Cond::True,
                        // Own-guard rule: a header's own include guard is
                        // assumed not to be a compiler predef, so treat the
                        // guard branch as certainly taken to keep its body's
                        // `#define`s tracked.
                        MacroState::Unknown if info.guard_macro.as_deref() == Some(name) => {
                            Cond::True
                        }
                        MacroState::Unknown => Cond::Unknown,
                    }
                } else {
                    Cond::False
                };
                cond_push_if(cstack, base, condition);
                continue;
            }
            Directive::Elif { expr } => {
                let condition = {
                    let eval_ctx = eval::EvalContext {
                        search_paths: ctx.env.search_paths,
                        current_file_dir: current_dir,
                        vfs: ctx.vfs,
                    };
                    eval::evaluate_cond(expr, macros, &eval_ctx, ctx.env.conservative)
                };
                cond_elif(cstack, base, condition);
                continue;
            }
            Directive::Else => {
                cond_else(cstack, base);
                continue;
            }
            Directive::Endif => {
                cond_endif(cstack, base);
                continue;
            }
            _ => {}
        }

        // Everything else is only processed in an active branch.
        if !cond_active(cstack, base) {
            continue;
        }

        match directive {
            Directive::Define { name, value } => {
                if ctx.env.conservative && !cond_certain(cstack, base) {
                    // Defined inside a maybe-taken branch: its real state now
                    // depends on that branch.
                    macros.mark_unknown(name);
                } else if let Some(val) = value {
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
                if ctx.env.conservative && !cond_certain(cstack, base) {
                    macros.mark_unknown(name);
                } else {
                    macros.undef(name);
                }
            }
            Directive::Pragma { tokens } => {
                handle_pragma(tokens, macros, &mut is_system);
            }
            Directive::Include(ref inc) => {
                handle_include(
                    ctx,
                    macros,
                    visited,
                    cstack,
                    out,
                    observe,
                    inc,
                    path,
                    current_dir,
                    is_system,
                    display_dir,
                );
            }
            Directive::Import(ref inc) => {
                // #import is include-once; the visited-set already gives every
                // file once-only semantics, so it is handled like #include.
                handle_include(
                    ctx,
                    macros,
                    visited,
                    cstack,
                    out,
                    observe,
                    inc,
                    path,
                    current_dir,
                    is_system,
                    display_dir,
                );
            }
            Directive::IncludeNext(ref inc) => {
                let resolved =
                    ctx.env
                        .search_paths
                        .resolve_include_next(inc.path, current_dir, ctx.vfs);
                if let Some(r) = resolved {
                    process_resolved(
                        ctx,
                        macros,
                        visited,
                        cstack,
                        out,
                        observe,
                        &r.path,
                        r.is_system || is_system,
                        r.display_path,
                        true,
                    );
                } else if ctx.env.missing_as_generated {
                    out.push_unresolved(inc.path, inc.style == IncludeStyle::Angle);
                }
            }
            Directive::Embed(ref inc) => {
                // #embed adds a dependency but does not recurse (binary data).
                let (resolved, style) = resolve_include(ctx, macros, inc, current_dir, display_dir);
                if let Some(r) = resolved {
                    process_resolved(
                        ctx,
                        macros,
                        visited,
                        cstack,
                        out,
                        observe,
                        &r.path,
                        r.is_system || is_system,
                        r.display_path,
                        false,
                    );
                } else if ctx.env.missing_as_generated {
                    out.push_unresolved(inc.path, style == IncludeStyle::Angle);
                }
            }
            _ => {}
        }
    }

    cstack.truncate(base);
}

/// Resolve an `#include`/`#import`/`#embed` directive to a file, expanding a
/// computed include first. Returns the post-expansion style so an unresolved
/// include is bucketed correctly (`<...>` → toolchain, `"..."` → generated).
fn resolve_include(
    ctx: &Ctx,
    macros: &mut MacroTable,
    inc: &IncludeDirective<'_>,
    current_dir: &Path,
    display_dir: Option<&Path>,
) -> (Option<ResolvedInclude>, IncludeStyle) {
    let sp = ctx.env.search_paths;
    match inc.style {
        IncludeStyle::Quoted => (
            sp.resolve_quoted(inc.path, current_dir, display_dir, ctx.vfs),
            IncludeStyle::Quoted,
        ),
        IncludeStyle::Angle => (sp.resolve_angle(inc.path, ctx.vfs), IncludeStyle::Angle),
        IncludeStyle::Computed => {
            let expanded = macros.expand(inc.path);
            let expanded = expanded.trim();
            match extract_include_path(expanded) {
                Some(path) if expanded.starts_with('<') => {
                    (sp.resolve_angle(&path, ctx.vfs), IncludeStyle::Angle)
                }
                Some(path) => (
                    sp.resolve_quoted(&path, current_dir, display_dir, ctx.vfs),
                    IncludeStyle::Quoted,
                ),
                None => (None, IncludeStyle::Computed),
            }
        }
    }
}

/// Handle an `#include` / `#import`.
#[allow(clippy::too_many_arguments)]
fn handle_include(
    ctx: &Ctx,
    macros: &mut MacroTable,
    visited: &mut Visited,
    cstack: &mut Vec<ConditionalState>,
    out: &mut ScanOutput,
    observe: &mut Observe,
    inc: &IncludeDirective<'_>,
    _file_path: &Path,
    current_dir: &Path,
    is_system: bool,
    display_dir: Option<&Path>,
) {
    let (resolved, style) = resolve_include(ctx, macros, inc, current_dir, display_dir);
    if let Some(r) = resolved {
        process_resolved(
            ctx,
            macros,
            visited,
            cstack,
            out,
            observe,
            &r.path,
            r.is_system || is_system,
            r.display_path,
            true,
        );
    } else if ctx.env.missing_as_generated {
        out.push_unresolved(inc.path, style == IncludeStyle::Angle);
    }
}

/// Record a resolved include and, unless it is guard-dominated or `recurse` is
/// false, walk into it. The single choke point where a file is canonicalized,
/// interned, deduped, observed, and classified.
#[allow(clippy::too_many_arguments)]
fn process_resolved(
    ctx: &Ctx,
    macros: &mut MacroTable,
    visited: &mut Visited,
    cstack: &mut Vec<ConditionalState>,
    out: &mut ScanOutput,
    observe: &mut Observe,
    resolved_path: &Path,
    is_system: bool,
    resolved_display: Option<PathBuf>,
    recurse: bool,
) {
    let canonical: Cow<Path> = if ctx.env.trust_canonical {
        Cow::Borrowed(resolved_path)
    } else {
        match ctx.vfs.canonicalize(resolved_path) {
            Ok(c) => Cow::Owned(c),
            Err(_) => return,
        }
    };

    let file = ctx.service.intern(&canonical);
    if visited.is_visited(file) {
        return;
    }

    let info = match ctx.service.get_or_scan(file, &canonical, ctx.vfs) {
        Ok(info) => info,
        Err(_) => return,
    };

    let display: Option<Box<str>> = if ctx.env.record_display {
        resolved_display
            .as_deref()
            .map(path_to_boxed)
            .or_else(|| (resolved_path != &*canonical).then(|| path_to_boxed(resolved_path)))
    } else {
        None
    };

    // Mark visited before observing/recording so re-encounters this TU dedup.
    // The visited-set is what enforces `#pragma once` and include guards: a file
    // is walked once per TU, and a guard that is already defined makes the walk
    // skip the body through the `#ifndef` itself.
    visited.mark(file);
    observe(file, info.content_hash);
    out.push_resolved(file, is_system, display);

    if !recurse {
        return;
    }

    let child_display_dir = if ctx.env.record_display {
        resolved_display
            .as_ref()
            .and_then(|dp| dp.parent().map(Path::to_path_buf))
    } else {
        None
    };

    walk_file(
        ctx,
        macros,
        visited,
        cstack,
        out,
        observe,
        &info,
        &canonical,
        is_system,
        child_display_dir.as_deref(),
    );
}

/// Handle `#pragma` directives that affect scanning.
fn handle_pragma(tokens: &str, macros: &mut MacroTable, is_system: &mut bool) {
    let tokens = tokens.trim();

    if tokens == "GCC system_header" || tokens == "clang system_header" {
        *is_system = true;
        return;
    }

    if let Some(rest) = tokens.strip_prefix("push_macro") {
        if let Some(name) = extract_pragma_macro_name(rest) {
            macros.push_macro(name);
        }
        return;
    }

    if let Some(rest) = tokens.strip_prefix("pop_macro") {
        if let Some(name) = extract_pragma_macro_name(rest) {
            macros.pop_macro(name);
        }
    }
}

fn extract_pragma_macro_name(s: &str) -> Option<&str> {
    let s = s.trim().strip_prefix('(')?.trim().strip_prefix('"')?;
    let end = s.find('"')?;
    Some(&s[..end])
}

/// Extract the path from an expanded computed-include token sequence
/// (`"path"` or `<path>`).
fn extract_include_path(expanded: &str) -> Option<String> {
    let s = expanded.trim();
    let quoted = s.starts_with('"') && s.ends_with('"') && s.len() >= 2;
    let angle = s.starts_with('<') && s.ends_with('>') && s.len() >= 2;
    (quoted || angle).then(|| s[1..s.len() - 1].to_string())
}

fn path_to_boxed(p: &Path) -> Box<str> {
    p.to_string_lossy().as_ref().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macros::MacroBase;
    use crate::resolve::SearchPaths;
    use crate::service::{ScanEnv, ScanOutput, ScanScratch, ScanService};
    use crate::vfs::OsVfs;
    use std::fs;
    use std::sync::Arc;

    fn base_from(defines: &[(String, Option<String>)]) -> Arc<MacroBase> {
        let mut base = MacroBase::new();
        base.extend_defines(defines.iter().map(|(n, v)| (n.as_str(), v.as_deref())));
        Arc::new(base)
    }

    /// Scan `main.c` in `dir`, returning dependency file basenames in order.
    fn scan_names(
        dir: &Path,
        defines: &[(String, Option<String>)],
        conservative: bool,
    ) -> Vec<String> {
        let service = ScanService::new();
        let sp = SearchPaths::default();
        let env = ScanEnv {
            macro_base: base_from(defines),
            conservative,
            ..ScanEnv::new(&sp)
        };
        let mut scratch = ScanScratch::new();
        let mut out = ScanOutput::new();
        service
            .scan_tu(
                &dir.join("main.c"),
                &env,
                &OsVfs,
                &mut scratch,
                &mut out,
                |_, _| {},
            )
            .unwrap();
        out.resolved
            .iter()
            .map(|d| {
                service
                    .path(d.file)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn simple_dependency_chain() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("c.h"), "// c\n").unwrap();
        fs::write(dir.path().join("b.h"), "#include \"c.h\"\n").unwrap();
        fs::write(dir.path().join("a.c"), "#include \"b.h\"\n").unwrap();

        let service = ScanService::new();
        let sp = SearchPaths::default();
        let env = ScanEnv::new(&sp);
        let mut scratch = ScanScratch::new();
        let mut out = ScanOutput::new();
        service
            .scan_tu(
                &dir.path().join("a.c"),
                &env,
                &OsVfs,
                &mut scratch,
                &mut out,
                |_, _| {},
            )
            .unwrap();
        assert_eq!(out.resolved.len(), 2); // b.h, c.h
    }

    #[test]
    fn include_guard_prevents_reprocessing() {
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

        let names = scan_names(dir.path(), &[], false);
        assert_eq!(names.iter().filter(|n| *n == "guarded.h").count(), 1);
    }

    #[test]
    fn pragma_once_file_still_recurses_into_its_includes() {
        // `#pragma once` means "process once per TU", not "never process". A
        // pragma-once header on first encounter must still follow its includes.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("c.h"), "#pragma once\nint c;\n").unwrap();
        fs::write(dir.path().join("b.h"), "#pragma once\n#include \"c.h\"\n").unwrap();
        fs::write(dir.path().join("main.c"), "#include \"b.h\"\n").unwrap();

        let names = scan_names(dir.path(), &[], false);
        assert!(names.contains(&"b.h".to_string()));
        assert!(
            names.contains(&"c.h".to_string()),
            "pragma-once b.h must still pull in c.h: {names:?}"
        );
    }

    #[test]
    fn conditional_ifdef_taken() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("yes.h"), "// yes\n").unwrap();
        fs::write(dir.path().join("no.h"), "// no\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#define USE_YES\n#ifdef USE_YES\n#include \"yes.h\"\n#else\n#include \"no.h\"\n#endif\n",
        )
        .unwrap();

        let names = scan_names(dir.path(), &[], false);
        assert!(names.contains(&"yes.h".to_string()));
        assert!(!names.contains(&"no.h".to_string()));
    }

    #[test]
    fn nested_conditionals() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("inner.h"), "// inner\n").unwrap();
        fs::write(dir.path().join("skip.h"), "// skip\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#define OUTER 1\n#if OUTER\n#define INNER 1\n#if INNER\n#include \"inner.h\"\n#endif\n#else\n#include \"skip.h\"\n#endif\n",
        )
        .unwrap();

        let names = scan_names(dir.path(), &[], false);
        assert!(names.contains(&"inner.h".to_string()));
        assert!(!names.contains(&"skip.h".to_string()));
    }

    #[test]
    fn computed_include() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("target.h"), "// target\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#define HEADER \"target.h\"\n#include HEADER\n",
        )
        .unwrap();
        let names = scan_names(dir.path(), &[], false);
        assert!(names.contains(&"target.h".to_string()));
    }

    #[test]
    fn cmdline_defines_via_base() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("yes.h"), "// yes\n").unwrap();
        fs::write(dir.path().join("no.h"), "// no\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#ifdef FEATURE\n#include \"yes.h\"\n#else\n#include \"no.h\"\n#endif\n",
        )
        .unwrap();

        let defines = vec![("FEATURE".to_string(), None)];
        let names = scan_names(dir.path(), &defines, false);
        assert!(names.contains(&"yes.h".to_string()));
        assert!(!names.contains(&"no.h".to_string()));
    }

    // -----------------------------------------------------------------------
    // The load-bearing invariant: warm content-cache hits still observe.
    // -----------------------------------------------------------------------

    #[test]
    fn warm_cache_hit_still_observes_every_tu() {
        // A shared header included by two TUs. The second TU is a pure content-
        // cache hit (the header is already minimized), yet it MUST still fire
        // `observe` for the header, or a virtualizing embedder would drop its
        // per-TU dependency fact and go stale on an edit.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("shared.h"), "#pragma once\nint s;\n").unwrap();
        fs::write(dir.path().join("a.c"), "#include \"shared.h\"\n").unwrap();
        fs::write(dir.path().join("b.c"), "#include \"shared.h\"\n").unwrap();

        let service = ScanService::new();
        let sp = SearchPaths::default();
        let env = ScanEnv::new(&sp);
        let mut scratch = ScanScratch::new();
        let mut out = ScanOutput::new();

        let has_shared =
            |ids: &[FileId]| ids.iter().any(|&id| service.path(id).ends_with("shared.h"));

        let mut observed_a: Vec<FileId> = Vec::new();
        service
            .scan_tu(
                &dir.path().join("a.c"),
                &env,
                &OsVfs,
                &mut scratch,
                &mut out,
                |id, _| observed_a.push(id),
            )
            .unwrap();
        assert!(has_shared(&observed_a));

        // shared.h is now warm in the content cache.
        let cached_after_first = service.cached_files();

        let mut observed_b: Vec<FileId> = Vec::new();
        service
            .scan_tu(
                &dir.path().join("b.c"),
                &env,
                &OsVfs,
                &mut scratch,
                &mut out,
                |id, _| observed_b.push(id),
            )
            .unwrap();

        // shared.h was NOT re-minimized, but WAS observed again for this TU.
        assert!(
            has_shared(&observed_b),
            "warm cache hit must still observe the shared header per-TU"
        );
        assert_eq!(
            service.cached_files(),
            cached_after_first + 1,
            "only b.c should be newly minimized; shared.h stayed cached"
        );
    }

    #[test]
    fn observe_hash_matches_content() {
        let dir = tempfile::tempdir().unwrap();
        let body = "#pragma once\nint x;\n";
        fs::write(dir.path().join("h.h"), body).unwrap();
        fs::write(dir.path().join("m.c"), "#include \"h.h\"\n").unwrap();

        let service = ScanService::new();
        let sp = SearchPaths::default();
        let env = ScanEnv::new(&sp);
        let mut scratch = ScanScratch::new();
        let mut out = ScanOutput::new();

        let want = crate::intern::content_hash(body.as_bytes());
        let mut seen: Vec<(FileId, crate::intern::ContentHash)> = Vec::new();
        service
            .scan_tu(
                &dir.path().join("m.c"),
                &env,
                &OsVfs,
                &mut scratch,
                &mut out,
                |id, hash| seen.push((id, hash)),
            )
            .unwrap();
        let got = seen
            .iter()
            .find(|(id, _)| service.path(*id).ends_with("h.h"))
            .map(|(_, h)| *h);
        assert_eq!(got, Some(want));
    }

    // -----------------------------------------------------------------------
    // Conservative mode
    // -----------------------------------------------------------------------

    #[test]
    fn conservative_predef_takes_both_branches() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("yes.h"), "// yes\n").unwrap();
        fs::write(dir.path().join("no.h"), "// no\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#ifdef __linux__\n#include \"yes.h\"\n#else\n#include \"no.h\"\n#endif\n",
        )
        .unwrap();

        let names = scan_names(dir.path(), &[], true);
        assert!(names.contains(&"yes.h".to_string()));
        assert!(names.contains(&"no.h".to_string()));
    }

    #[test]
    fn conservative_plain_unknown_macro_takes_else_only() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("yes.h"), "// yes\n").unwrap();
        fs::write(dir.path().join("no.h"), "// no\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#ifdef FOO\n#include \"yes.h\"\n#else\n#include \"no.h\"\n#endif\n",
        )
        .unwrap();

        // FOO is untracked and not predefinable, exact-like behavior.
        let names = scan_names(dir.path(), &[], true);
        assert!(!names.contains(&"yes.h".to_string()));
        assert!(names.contains(&"no.h".to_string()));
    }

    #[test]
    fn conservative_own_guard_keeps_defines_tracked() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("x.h"), "// x\n").unwrap();
        fs::write(dir.path().join("y.h"), "// y\n").unwrap();
        fs::write(
            dir.path().join("g.h"),
            "#ifndef _G_H\n#define _G_H\n#define HAVE_X 1\n#endif\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#include \"g.h\"\n#if HAVE_X\n#include \"x.h\"\n#endif\n#if !HAVE_X\n#include \"y.h\"\n#endif\n",
        )
        .unwrap();

        let names = scan_names(dir.path(), &[], true);
        assert!(names.contains(&"x.h".to_string()));
        assert!(!names.contains(&"y.h".to_string()));
    }

    #[test]
    fn conservative_function_like_predef() {
        // A function-like predef in the base decides an #if in exact-ish form.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("yes.h"), "// yes\n").unwrap();
        fs::write(dir.path().join("no.h"), "// no\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#if CHECK(1)\n#include \"yes.h\"\n#else\n#include \"no.h\"\n#endif\n",
        )
        .unwrap();

        let service = ScanService::new();
        let sp = SearchPaths::default();
        let mut base = MacroBase::new();
        base.define_function_like("CHECK", "(x) x");
        let env = ScanEnv {
            macro_base: Arc::new(base),
            conservative: true,
            ..ScanEnv::new(&sp)
        };
        let mut scratch = ScanScratch::new();
        let mut out = ScanOutput::new();
        service
            .scan_tu(
                &dir.path().join("main.c"),
                &env,
                &OsVfs,
                &mut scratch,
                &mut out,
                |_, _| {},
            )
            .unwrap();
        let names: Vec<_> = out
            .resolved
            .iter()
            .map(|d| {
                service
                    .path(d.file)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert!(names.contains(&"yes.h".to_string()));
        assert!(!names.contains(&"no.h".to_string()));
    }

    #[test]
    fn missing_include_recorded_when_requested() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#include \"gen.h\"\n#include <sys/only.h>\n",
        )
        .unwrap();

        let service = ScanService::new();
        let sp = SearchPaths::default();
        let env = ScanEnv {
            missing_as_generated: true,
            ..ScanEnv::new(&sp)
        };
        let mut scratch = ScanScratch::new();
        let mut out = ScanOutput::new();
        service
            .scan_tu(
                &dir.path().join("main.c"),
                &env,
                &OsVfs,
                &mut scratch,
                &mut out,
                |_, _| {},
            )
            .unwrap();
        assert!(out.missing.iter().any(|m| &**m == "gen.h"));
        assert!(out.sys_includes.iter().any(|m| &**m == "sys/only.h"));
    }

    #[test]
    fn scratch_and_output_reused_across_tus() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("h.h"), "#pragma once\n").unwrap();
        fs::write(dir.path().join("a.c"), "#include \"h.h\"\n").unwrap();
        fs::write(dir.path().join("b.c"), "int b;\n").unwrap();

        let service = ScanService::new();
        let sp = SearchPaths::default();
        let env = ScanEnv::new(&sp);
        let mut scratch = ScanScratch::new();
        let mut out = ScanOutput::new();

        service
            .scan_tu(
                &dir.path().join("a.c"),
                &env,
                &OsVfs,
                &mut scratch,
                &mut out,
                |_, _| {},
            )
            .unwrap();
        assert_eq!(out.resolved.len(), 1);

        // Reusing the same scratch + out must fully reset between TUs.
        service
            .scan_tu(
                &dir.path().join("b.c"),
                &env,
                &OsVfs,
                &mut scratch,
                &mut out,
                |_, _| {},
            )
            .unwrap();
        assert_eq!(out.resolved.len(), 0);
    }
}
