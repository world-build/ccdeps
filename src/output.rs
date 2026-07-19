//! Output formatting matching `gcc -M` / `clang -M`.
//!
//! Produces Makefile-format dependency rules with gcc-compatible line wrapping
//! and support for all `-M` family flag variants.

use std::path::Path;

use crate::service::{ResolvedDep, ScanOutput, ScanService};

/// Maximum column before wrapping to a continuation line (matches gcc's mkdeps.cc).
const MAX_COLUMNS: usize = 72;

/// Options controlling dependency output formatting.
#[derive(Debug, Clone, Default)]
pub struct OutputOptions {
    /// Custom target names from `-MT` flags. Empty = derive from source filename.
    pub targets: Vec<String>,
    /// Custom target names from `-MQ` flags (Make-quoted before use).
    pub quoted_targets: Vec<String>,
    /// Whether to add phony targets for each dep (`-MP`).
    pub phony: bool,
    /// Whether to omit system headers from output (`-MM`/`-MMD`).
    pub omit_system: bool,
}

/// Format a scan result as a `gcc -M`-style Makefile rule. `service` resolves
/// each [`FileId`](crate::FileId) to a path; a dep's recorded display spelling
/// (from `..`/`.` includes) wins over its canonical path, as gcc does.
pub fn format_makefile(
    service: &ScanService,
    source: &Path,
    out: &ScanOutput,
    options: &OutputOptions,
) -> String {
    let mut result = String::new();

    let target = build_target_string(source, options);
    result.push_str(&target);
    result.push(':');
    let mut col = target.len() + 1;

    wrap_and_push(&mut result, &mut col, &escape(&source.to_string_lossy()));

    let shown = || {
        out.resolved
            .iter()
            .filter(|d| !(options.omit_system && d.is_system))
    };
    // Unresolved names are only recorded under `-MG`; gcc lists them verbatim.
    let unresolved = || out.missing.iter().chain(out.sys_includes.iter());

    for dep in shown() {
        wrap_and_push(&mut result, &mut col, &dep_spelling(service, dep));
    }
    for name in unresolved() {
        wrap_and_push(&mut result, &mut col, &escape(name));
    }
    result.push('\n');

    if options.phony {
        for dep in shown() {
            result.push('\n');
            result.push_str(&dep_spelling(service, dep));
            result.push_str(":\n");
        }
        for name in unresolved() {
            result.push('\n');
            result.push_str(&escape(name));
            result.push_str(":\n");
        }
    }

    result
}

/// The escaped path to print for a dependency: its display spelling if it kept
/// one, otherwise its canonical path.
fn dep_spelling(service: &ScanService, dep: &ResolvedDep) -> String {
    match &dep.display {
        Some(d) => escape(d),
        None => escape(&service.path(dep.file).to_string_lossy()),
    }
}

/// Build the target string from options or derive from source filename.
fn build_target_string(source: &Path, options: &OutputOptions) -> String {
    let mut parts: Vec<String> = Vec::new();

    // -MT targets are used verbatim
    for t in &options.targets {
        parts.push(t.clone());
    }

    // -MQ targets are Make-quoted
    for t in &options.quoted_targets {
        parts.push(quote_target(t));
    }

    if parts.is_empty() {
        // Default: replace source extension with .o
        let stem = source.file_stem().unwrap_or_default().to_string_lossy();
        format!("{}.o", stem)
    } else {
        parts.join(" ")
    }
}

/// Quote a target name for Make (`-MQ` behavior).
///
/// Escapes `$`, `#`, and ` ` which are special in Make target names.
/// `$` becomes `$$`, others get `\` prefix.
fn quote_target(target: &str) -> String {
    let mut out = String::with_capacity(target.len());
    for c in target.chars() {
        match c {
            '$' => {
                out.push('$');
                out.push('$');
            }
            '#' | ' ' | '\t' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Push a dependency path, wrapping to a continuation line if needed.
fn wrap_and_push(out: &mut String, col: &mut usize, escaped: &str) {
    // +1 for the leading space
    if *col + 1 + escaped.len() > MAX_COLUMNS && *col > 1 {
        out.push_str(" \\\n ");
        *col = 1;
    } else {
        out.push(' ');
        *col += 1;
    }
    out.push_str(escaped);
    *col += escaped.len();
}

/// Escape a path spelling for Makefile output (spaces, `#`, `$`).
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            ' ' | '\t' => {
                out.push('\\');
                out.push(c);
            }
            '#' => {
                out.push('\\');
                out.push(c);
            }
            '$' => {
                out.push('$');
                out.push('$');
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{ScanOutput, ScanService};

    /// Build a service + output from `(path, is_system)` pairs, in order.
    fn scan(deps: &[(&str, bool)]) -> (ScanService, ScanOutput) {
        let service = ScanService::new();
        let mut out = ScanOutput::new();
        for (path, is_system) in deps {
            let id = service.intern(Path::new(path));
            out.push_resolved(id, *is_system, None);
        }
        (service, out)
    }

    fn format(source: &str, deps: &[(&str, bool)], options: &OutputOptions) -> String {
        let (service, out) = scan(deps);
        format_makefile(&service, Path::new(source), &out, options)
    }

    #[test]
    fn test_simple_output() {
        let result = format(
            "main.c",
            &[("foo.h", false), ("bar.h", false)],
            &OutputOptions::default(),
        );
        assert_eq!(result, "main.o: main.c foo.h bar.h\n");
    }

    #[test]
    fn test_custom_target() {
        let options = OutputOptions {
            targets: vec!["build/main.o".to_string()],
            ..Default::default()
        };
        let result = format("src/main.c", &[("inc/foo.h", false)], &options);
        assert_eq!(result, "build/main.o: src/main.c inc/foo.h\n");
    }

    #[test]
    fn test_path_with_spaces() {
        let result = format("my project/main.c", &[], &OutputOptions::default());
        assert!(result.contains("my\\ project/main.c"));
    }

    #[test]
    fn test_line_wrapping() {
        let deps: Vec<(String, bool)> = (0..10)
            .map(|i| (format!("/very/long/path/to/header{}.h", i), false))
            .collect();
        let deps: Vec<(&str, bool)> = deps.iter().map(|(p, s)| (p.as_str(), *s)).collect();
        let result = format("main.c", &deps, &OutputOptions::default());
        assert!(
            result.contains(" \\\n "),
            "should contain continuation lines"
        );
        for line in result.lines() {
            let content = line.trim_end_matches(" \\");
            assert!(content.len() <= MAX_COLUMNS + 5, "line too long: {}", line);
        }
    }

    #[test]
    fn test_phony_targets() {
        let options = OutputOptions {
            phony: true,
            ..Default::default()
        };
        let result = format("main.c", &[("foo.h", false), ("bar.h", false)], &options);
        assert!(
            result.contains("\nfoo.h:\n"),
            "missing phony target for foo.h"
        );
        assert!(
            result.contains("\nbar.h:\n"),
            "missing phony target for bar.h"
        );
    }

    #[test]
    fn test_omit_system_headers() {
        let options = OutputOptions {
            omit_system: true,
            ..Default::default()
        };
        let result = format("main.c", &[("user.h", false), ("system.h", true)], &options);
        assert!(result.contains("user.h"), "should include user header");
        assert!(!result.contains("system.h"), "should omit system header");
    }

    #[test]
    fn test_mq_quoting() {
        assert_eq!(quote_target("main.o"), "main.o");
        assert_eq!(
            quote_target("path with space/main.o"),
            "path\\ with\\ space/main.o"
        );
        assert_eq!(quote_target("$var.o"), "$$var.o");
        assert_eq!(quote_target("file#1.o"), "file\\#1.o");
    }

    #[test]
    fn test_multiple_targets() {
        let options = OutputOptions {
            targets: vec!["target1.o".to_string(), "target2.o".to_string()],
            ..Default::default()
        };
        let result = format("main.c", &[("foo.h", false)], &options);
        assert!(result.starts_with("target1.o target2.o:"));
    }

    #[test]
    fn test_mt_and_mq_combined() {
        let options = OutputOptions {
            targets: vec!["a.o".to_string()],
            quoted_targets: vec!["$b.o".to_string()],
            ..Default::default()
        };
        let result = format("main.c", &[("foo.h", false)], &options);
        assert!(result.starts_with("a.o $$b.o:"));
    }

    #[test]
    fn test_phony_with_omit_system() {
        let options = OutputOptions {
            phony: true,
            omit_system: true,
            ..Default::default()
        };
        let result = format("main.c", &[("user.h", false), ("sys.h", true)], &options);
        assert!(
            result.contains("\nuser.h:\n"),
            "should have phony for user.h"
        );
        assert!(!result.contains("sys.h"), "should not have phony for sys.h");
    }
}
