//! Output formatting matching `gcc -M` / `clang -M`.
//!
//! Produces Makefile-format dependency rules with gcc-compatible line wrapping
//! and support for all `-M` family flag variants.

use std::path::Path;

use crate::depgraph::DepEntry;

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

/// Format dependencies in Makefile format, matching `gcc -M` output.
///
/// This is the simple API for backward compatibility (equivalent to `-M`).
pub fn format_makefile_deps(
    source: &Path,
    deps: &[DepEntry],
    target: Option<&str>,
) -> String {
    let options = match target {
        Some(t) => OutputOptions {
            targets: vec![t.to_string()],
            ..Default::default()
        },
        None => OutputOptions::default(),
    };
    format_makefile_deps_with_options(source, deps, &options)
}

/// Format dependencies in Makefile format with full option support.
pub fn format_makefile_deps_with_options(
    source: &Path,
    deps: &[DepEntry],
    options: &OutputOptions,
) -> String {
    let mut out = String::new();

    // Build the target string
    let target_str = build_target_string(source, options);
    out.push_str(&target_str);
    out.push(':');
    let mut col = target_str.len() + 1;

    // Source file first
    let source_escaped = escape_path(source);
    wrap_and_push(&mut out, &mut col, &source_escaped);

    // Then all dependencies (filtering system if -MM)
    for dep in deps {
        if options.omit_system && dep.is_system {
            continue;
        }
        let dep_escaped = escape_path(&dep.path);
        wrap_and_push(&mut out, &mut col, &dep_escaped);
    }

    out.push('\n');

    // Phony targets (-MP)
    if options.phony {
        for dep in deps {
            if options.omit_system && dep.is_system {
                continue;
            }
            out.push('\n');
            let dep_escaped = escape_path(&dep.path);
            out.push_str(&dep_escaped);
            out.push_str(":\n");
        }
    }

    out
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

/// Escape a path for Makefile output (escape spaces and special chars).
fn escape_path(path: &Path) -> String {
    let s = path.to_string_lossy();
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
    use std::path::PathBuf;

    fn dep(path: &str) -> DepEntry {
        DepEntry { path: PathBuf::from(path), is_system: false }
    }

    fn sys_dep(path: &str) -> DepEntry {
        DepEntry { path: PathBuf::from(path), is_system: true }
    }

    #[test]
    fn test_simple_output() {
        let source = Path::new("main.c");
        let deps = vec![dep("foo.h"), dep("bar.h")];
        let result = format_makefile_deps(source, &deps, None);
        assert_eq!(result, "main.o: main.c foo.h bar.h\n");
    }

    #[test]
    fn test_custom_target() {
        let source = Path::new("src/main.c");
        let deps = vec![dep("inc/foo.h")];
        let result = format_makefile_deps(source, &deps, Some("build/main.o"));
        assert_eq!(result, "build/main.o: src/main.c inc/foo.h\n");
    }

    #[test]
    fn test_path_with_spaces() {
        let source = Path::new("my project/main.c");
        let deps: Vec<DepEntry> = vec![];
        let result = format_makefile_deps(source, &deps, None);
        assert!(result.contains("my\\ project/main.c"));
    }

    #[test]
    fn test_line_wrapping() {
        let source = Path::new("main.c");
        let deps: Vec<DepEntry> = (0..10)
            .map(|i| dep(&format!("/very/long/path/to/header{}.h", i)))
            .collect();
        let result = format_makefile_deps(source, &deps, None);
        assert!(result.contains(" \\\n "), "should contain continuation lines");
        // Verify no line exceeds MAX_COLUMNS (before the backslash)
        for line in result.lines() {
            let content = line.trim_end_matches(" \\");
            assert!(content.len() <= MAX_COLUMNS + 5, "line too long: {}", line);
        }
    }

    #[test]
    fn test_phony_targets() {
        let source = Path::new("main.c");
        let deps = vec![dep("foo.h"), dep("bar.h")];
        let options = OutputOptions { phony: true, ..Default::default() };
        let result = format_makefile_deps_with_options(source, &deps, &options);
        assert!(result.contains("\nfoo.h:\n"), "missing phony target for foo.h");
        assert!(result.contains("\nbar.h:\n"), "missing phony target for bar.h");
    }

    #[test]
    fn test_omit_system_headers() {
        let source = Path::new("main.c");
        let deps = vec![dep("user.h"), sys_dep("system.h")];
        let options = OutputOptions { omit_system: true, ..Default::default() };
        let result = format_makefile_deps_with_options(source, &deps, &options);
        assert!(result.contains("user.h"), "should include user header");
        assert!(!result.contains("system.h"), "should omit system header");
    }

    #[test]
    fn test_mq_quoting() {
        assert_eq!(quote_target("main.o"), "main.o");
        assert_eq!(quote_target("path with space/main.o"), "path\\ with\\ space/main.o");
        assert_eq!(quote_target("$var.o"), "$$var.o");
        assert_eq!(quote_target("file#1.o"), "file\\#1.o");
    }

    #[test]
    fn test_multiple_targets() {
        let source = Path::new("main.c");
        let deps = vec![dep("foo.h")];
        let options = OutputOptions {
            targets: vec!["target1.o".to_string(), "target2.o".to_string()],
            ..Default::default()
        };
        let result = format_makefile_deps_with_options(source, &deps, &options);
        assert!(result.starts_with("target1.o target2.o:"));
    }

    #[test]
    fn test_mt_and_mq_combined() {
        let source = Path::new("main.c");
        let deps = vec![dep("foo.h")];
        let options = OutputOptions {
            targets: vec!["a.o".to_string()],
            quoted_targets: vec!["$b.o".to_string()],
            ..Default::default()
        };
        let result = format_makefile_deps_with_options(source, &deps, &options);
        assert!(result.starts_with("a.o $$b.o:"));
    }

    #[test]
    fn test_phony_with_omit_system() {
        let source = Path::new("main.c");
        let deps = vec![dep("user.h"), sys_dep("sys.h")];
        let options = OutputOptions { phony: true, omit_system: true, ..Default::default() };
        let result = format_makefile_deps_with_options(source, &deps, &options);
        assert!(result.contains("\nuser.h:\n"), "should have phony for user.h");
        assert!(!result.contains("sys.h"), "should not have phony for sys.h");
    }
}
