//! Output formatting matching `gcc -M` / `clang -M`.
//!
//! Produces Makefile-format dependency rules:
//! ```text
//! target.o: source.c header1.h header2.h \
//!   header3.h
//! ```

use std::path::Path;

/// Format dependencies in Makefile format, matching `gcc -M` output.
pub fn format_makefile_deps(
    source: &Path,
    deps: &[impl AsRef<Path>],
    target: Option<&str>,
) -> String {
    let target_name = match target {
        Some(t) => t.to_string(),
        None => {
            // Default: replace source extension with .o
            let stem = source.file_stem().unwrap_or_default().to_string_lossy();
            format!("{}.o", stem)
        }
    };

    let mut out = String::new();
    out.push_str(&target_name);
    out.push(':');

    // Source file first
    out.push(' ');
    push_escaped_path(&mut out, source);

    // Then all dependencies
    for dep in deps {
        out.push_str(" \\\n  ");
        push_escaped_path(&mut out, dep.as_ref());
    }

    out.push('\n');
    out
}

/// Escape a path for Makefile output (escape spaces and special chars).
fn push_escaped_path(out: &mut String, path: &Path) {
    let s = path.to_string_lossy();
    for c in s.chars() {
        match c {
            ' ' | '\t' | '#' | '\\' => {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_simple_output() {
        let source = Path::new("main.c");
        let deps: Vec<PathBuf> = vec!["foo.h".into(), "bar.h".into()];
        let result = format_makefile_deps(source, &deps, None);
        assert_eq!(result, "main.o: main.c \\\n  foo.h \\\n  bar.h\n");
    }

    #[test]
    fn test_custom_target() {
        let source = Path::new("src/main.c");
        let deps: Vec<PathBuf> = vec!["inc/foo.h".into()];
        let result = format_makefile_deps(source, &deps, Some("build/main.o"));
        assert_eq!(result, "build/main.o: src/main.c \\\n  inc/foo.h\n");
    }

    #[test]
    fn test_path_with_spaces() {
        let source = Path::new("my project/main.c");
        let deps: Vec<PathBuf> = vec![];
        let result = format_makefile_deps(source, &deps, None);
        assert!(result.contains("my\\ project/main.c"));
    }
}
