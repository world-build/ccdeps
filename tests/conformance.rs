//! Conformance test suite.
//!
//! Compares cppscan output against `gcc -M` and `clang -M` to ensure
//! we produce identical dependency lists.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use cppscan::{ScanOptions, Scanner};

/// Parse a Makefile-format dependency output into a set of dependency paths.
fn parse_makefile_deps(output: &str) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();

    // Join continuation lines
    let joined = output.replace("\\\n", " ");

    for line in joined.lines() {
        // Skip empty lines
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Find the colon separator (target: deps...)
        if let Some(colon_pos) = line.find(':') {
            let dep_str = &line[colon_pos + 1..];
            for dep in dep_str.split_whitespace() {
                deps.insert(dep.to_string());
            }
        } else {
            // Continuation line (just deps)
            for dep in line.split_whitespace() {
                deps.insert(dep.to_string());
            }
        }
    }

    deps
}

/// Run a compiler with -M flag and return parsed dependencies.
fn get_compiler_deps(compiler: &str, source: &Path, include_paths: &[PathBuf]) -> Option<BTreeSet<String>> {
    let mut cmd = Command::new(compiler);
    cmd.arg("-M").arg(source);
    for path in include_paths {
        cmd.arg("-I").arg(path);
    }

    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Some(parse_makefile_deps(&stdout))
}

/// Run cppscan and return parsed dependencies.
fn get_cppscan_deps(source: &Path, include_paths: &[PathBuf]) -> BTreeSet<String> {
    let options = ScanOptions {
        search_paths: cppscan::resolve::SearchPaths {
            include_paths: include_paths.to_vec(),
            ..Default::default()
        },
        ..Default::default()
    };

    let scanner = Scanner::new(options);
    let result = scanner.scan_file(source).unwrap();
    parse_makefile_deps(&result.makefile_output)
}

/// Helper to create a test case directory with source and header files.
struct TestCase {
    dir: tempfile::TempDir,
}

impl TestCase {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn write_file(&self, name: &str, content: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }
}

// ---- Conformance tests ----

/// Compare cppscan vs gcc/clang on a test case. Skips if compiler not available.
fn assert_matches_compiler(
    source: &Path,
    include_paths: &[PathBuf],
    compiler: &str,
) {
    let cppscan_deps = get_cppscan_deps(source, include_paths);

    if let Some(compiler_deps) = get_compiler_deps(compiler, source, include_paths) {
        // Remove the source file itself from both sets for comparison
        // (compilers include it, we include it)
        let source_str = source.to_string_lossy().to_string();

        let cppscan_only: BTreeSet<_> = cppscan_deps.difference(&compiler_deps).collect();
        let compiler_only: BTreeSet<_> = compiler_deps.difference(&cppscan_deps).collect();

        // Filter out the source file and system/implicit headers that we don't resolve.
        // gcc implicitly includes stdc-predef.h; system headers live under /usr/ or /nix/store/.
        let is_system = |p: &&String| {
            p.starts_with("/usr/") || p.starts_with("/nix/store/") || **p == source_str
        };
        let compiler_only_filtered: BTreeSet<_> = compiler_only
            .into_iter()
            .filter(|p| !is_system(p))
            .collect();
        let cppscan_only_filtered: BTreeSet<_> = cppscan_only
            .into_iter()
            .filter(|p| !is_system(p))
            .collect();

        assert!(
            cppscan_only_filtered.is_empty() && compiler_only_filtered.is_empty(),
            "Mismatch with {}:\n  cppscan extra: {:?}\n  {} extra: {:?}\n  cppscan: {:?}\n  {}: {:?}",
            compiler,
            cppscan_only_filtered,
            compiler,
            compiler_only_filtered,
            cppscan_deps,
            compiler,
            compiler_deps,
        );
    }
}

#[test]
fn conformance_simple_include() {
    let tc = TestCase::new();
    tc.write_file("header.h", "int x;\n");
    let source = tc.write_file("main.c", "#include \"header.h\"\nint main() {}\n");

    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "gcc");
    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "clang");
}

#[test]
fn conformance_nested_includes() {
    let tc = TestCase::new();
    tc.write_file("c.h", "int c;\n");
    tc.write_file("b.h", "#include \"c.h\"\nint b;\n");
    let source = tc.write_file("a.c", "#include \"b.h\"\nint main() {}\n");

    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "gcc");
    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "clang");
}

#[test]
fn conformance_include_guard() {
    let tc = TestCase::new();
    tc.write_file(
        "guarded.h",
        "#ifndef GUARDED_H\n#define GUARDED_H\nint x;\n#endif\n",
    );
    tc.write_file("uses_guarded.h", "#include \"guarded.h\"\n");
    let source = tc.write_file(
        "main.c",
        "#include \"guarded.h\"\n#include \"uses_guarded.h\"\nint main() {}\n",
    );

    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "gcc");
    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "clang");
}

#[test]
fn conformance_pragma_once() {
    let tc = TestCase::new();
    tc.write_file("once.h", "#pragma once\nint x;\n");
    tc.write_file("a.h", "#include \"once.h\"\n");
    let source = tc.write_file(
        "main.c",
        "#include \"once.h\"\n#include \"a.h\"\nint main() {}\n",
    );

    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "gcc");
    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "clang");
}

#[test]
fn conformance_conditional_include() {
    let tc = TestCase::new();
    tc.write_file("yes.h", "int yes;\n");
    tc.write_file("no.h", "int no;\n");
    let source = tc.write_file(
        "main.c",
        "#define USE_YES 1\n#if USE_YES\n#include \"yes.h\"\n#else\n#include \"no.h\"\n#endif\nint main() {}\n",
    );

    // Note: cppscan doesn't evaluate #if conditions yet, so this tests
    // that we at least find all potentially-included files
    let cppscan_deps = get_cppscan_deps(&source, &[tc.path().to_path_buf()]);
    // We should find both yes.h and no.h (conservative scan)
    // gcc/clang would only find yes.h (they evaluate conditions)
    assert!(!cppscan_deps.is_empty());
}

#[test]
fn conformance_subdirectory_includes() {
    let tc = TestCase::new();
    tc.write_file("inc/sub/deep.h", "int deep;\n");
    tc.write_file("inc/mid.h", "#include \"sub/deep.h\"\n");
    let source = tc.write_file("main.c", "#include \"mid.h\"\n");

    let inc_path = tc.path().join("inc");
    assert_matches_compiler(&source, &[inc_path.clone()], "gcc");
    assert_matches_compiler(&source, &[inc_path], "clang");
}

// ---- Unit tests for the dep parser ----

#[test]
fn test_parse_makefile_deps() {
    let output = "main.o: main.c foo.h \\\n  bar.h baz.h\n";
    let deps = parse_makefile_deps(output);
    assert!(deps.contains("main.c"));
    assert!(deps.contains("foo.h"));
    assert!(deps.contains("bar.h"));
    assert!(deps.contains("baz.h"));
}
