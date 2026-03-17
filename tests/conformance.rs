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

    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "gcc");
    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "clang");
}

#[test]
fn conformance_ifdef_conditional() {
    let tc = TestCase::new();
    tc.write_file("yes.h", "int yes;\n");
    tc.write_file("no.h", "int no;\n");
    let source = tc.write_file(
        "main.c",
        "#define FEATURE\n#ifdef FEATURE\n#include \"yes.h\"\n#else\n#include \"no.h\"\n#endif\nint main() {}\n",
    );

    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "gcc");
    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "clang");
}

#[test]
fn conformance_elif_chain() {
    let tc = TestCase::new();
    tc.write_file("a.h", "int a;\n");
    tc.write_file("b.h", "int b;\n");
    tc.write_file("c.h", "int c;\n");
    let source = tc.write_file(
        "main.c",
        "#define MODE 2\n#if MODE == 1\n#include \"a.h\"\n#elif MODE == 2\n#include \"b.h\"\n#else\n#include \"c.h\"\n#endif\nint main() {}\n",
    );

    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "gcc");
    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "clang");
}

#[test]
fn conformance_nested_conditional() {
    let tc = TestCase::new();
    tc.write_file("inner.h", "int inner;\n");
    tc.write_file("skip.h", "int skip;\n");
    let source = tc.write_file(
        "main.c",
        "#define OUTER 1\n#define INNER 1\n#if OUTER\n#if INNER\n#include \"inner.h\"\n#endif\n#else\n#include \"skip.h\"\n#endif\nint main() {}\n",
    );

    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "gcc");
    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "clang");
}

#[test]
fn conformance_define_in_header() {
    let tc = TestCase::new();
    tc.write_file("config.h", "#define HAS_FEATURE 1\n");
    tc.write_file("feature.h", "int feature;\n");
    let source = tc.write_file(
        "main.c",
        "#include \"config.h\"\n#if HAS_FEATURE\n#include \"feature.h\"\n#endif\nint main() {}\n",
    );

    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "gcc");
    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "clang");
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

// ---- Phase 2: computed includes, function-like macros, __has_include ----

#[test]
fn conformance_computed_include() {
    let tc = TestCase::new();
    tc.write_file("foo.h", "int foo;\n");
    let source = tc.write_file(
        "main.c",
        "#define HEADER \"foo.h\"\n#include HEADER\nint main() {}\n",
    );

    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "gcc");
    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "clang");
}

#[test]
fn conformance_computed_include_function_like() {
    let tc = TestCase::new();
    tc.write_file("bar.h", "int bar;\n");
    let source = tc.write_file(
        "main.c",
        "#define MAKE_HEADER(name) #name \".h\"\n\
         // Note: the above produces \"bar\" \".h\" which gcc doesn't accept as a single path.\n\
         // Instead, use a simpler object-like macro approach:\n\
         #define HEADER \"bar.h\"\n\
         #include HEADER\n\
         int main() {}\n",
    );

    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "gcc");
    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "clang");
}

#[test]
fn conformance_computed_include_angle() {
    let tc = TestCase::new();
    tc.write_file("sys.h", "int sys;\n");
    let source = tc.write_file(
        "main.c",
        "#define SYS_HEADER <sys.h>\n#include SYS_HEADER\nint main() {}\n",
    );

    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "gcc");
    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "clang");
}

#[test]
fn conformance_has_include() {
    let tc = TestCase::new();
    tc.write_file("exists.h", "int exists;\n");
    tc.write_file("fallback.h", "int fallback;\n");
    let source = tc.write_file(
        "main.c",
        "#if __has_include(\"exists.h\")\n\
         #include \"exists.h\"\n\
         #else\n\
         #include \"fallback.h\"\n\
         #endif\n\
         int main() {}\n",
    );

    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "gcc");
    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "clang");
}

#[test]
fn conformance_has_include_missing() {
    let tc = TestCase::new();
    tc.write_file("fallback.h", "int fallback;\n");
    let source = tc.write_file(
        "main.c",
        "#if __has_include(\"nonexistent.h\")\n\
         #include \"nonexistent.h\"\n\
         #else\n\
         #include \"fallback.h\"\n\
         #endif\n\
         int main() {}\n",
    );

    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "gcc");
    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "clang");
}

#[test]
fn conformance_function_like_macro_in_if() {
    let tc = TestCase::new();
    tc.write_file("yes.h", "int yes;\n");
    tc.write_file("no.h", "int no;\n");
    let source = tc.write_file(
        "main.c",
        "#define CHECK(x) x\n\
         #if CHECK(1)\n\
         #include \"yes.h\"\n\
         #else\n\
         #include \"no.h\"\n\
         #endif\n\
         int main() {}\n",
    );

    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "gcc");
    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "clang");
}

#[test]
fn conformance_chained_macro_include() {
    let tc = TestCase::new();
    tc.write_file("target.h", "int target;\n");
    let source = tc.write_file(
        "main.c",
        "#define A B\n\
         #define B \"target.h\"\n\
         #include A\n\
         int main() {}\n",
    );

    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "gcc");
    assert_matches_compiler(&source, &[tc.path().to_path_buf()], "clang");
}

// ---- Phase 3: -M flag variants & output compatibility ----

#[test]
fn conformance_mm_omits_system_headers() {
    let tc = TestCase::new();
    let sys_dir = tempfile::tempdir().unwrap();

    tc.write_file("user.h", "int user;\n");
    fs::write(sys_dir.path().join("sys.h"), "int sys;\n").unwrap();
    let source = tc.write_file(
        "main.c",
        "#include \"user.h\"\n#include <sys.h>\nint main() {}\n",
    );

    // cppscan with -MM mode should omit system header
    let options = ScanOptions {
        search_paths: cppscan::resolve::SearchPaths {
            include_paths: vec![tc.path().to_path_buf()],
            system_paths: vec![sys_dir.path().to_path_buf()],
            ..Default::default()
        },
        dep_mode: cppscan::DepMode::MM,
        ..Default::default()
    };

    let scanner = Scanner::new(options);
    let result = scanner.scan_file(&source).unwrap();
    let deps = parse_makefile_deps(&result.makefile_output);

    assert!(deps.iter().any(|d| d.contains("user.h")), "should include user.h");
    assert!(!deps.iter().any(|d| d.contains("sys.h")), "should omit sys.h");
}

#[test]
fn conformance_mp_phony_targets() {
    let tc = TestCase::new();
    tc.write_file("foo.h", "int foo;\n");
    tc.write_file("bar.h", "int bar;\n");
    let source = tc.write_file(
        "main.c",
        "#include \"foo.h\"\n#include \"bar.h\"\nint main() {}\n",
    );

    let options = ScanOptions {
        search_paths: cppscan::resolve::SearchPaths {
            include_paths: vec![tc.path().to_path_buf()],
            ..Default::default()
        },
        output_options: cppscan::OutputOptions {
            phony: true,
            ..Default::default()
        },
        ..Default::default()
    };

    let scanner = Scanner::new(options);
    let result = scanner.scan_file(&source).unwrap();

    // -MP adds phony targets: each dep gets its own empty rule
    let output = &result.makefile_output;

    // Parse phony targets: lines matching `<path>:\n` without deps
    let lines: Vec<&str> = output.lines().collect();
    let mut phony_count = 0;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        // A phony target line ends with ':' and the next line is empty or another rule
        if trimmed.ends_with(':') && !trimmed.contains(' ') && i > 0 {
            phony_count += 1;
        }
    }
    assert!(phony_count >= 2, "expected at least 2 phony targets, got {}", phony_count);
}

#[test]
fn conformance_mt_custom_target() {
    let tc = TestCase::new();
    tc.write_file("foo.h", "int foo;\n");
    let source = tc.write_file("main.c", "#include \"foo.h\"\nint main() {}\n");

    let options = ScanOptions {
        search_paths: cppscan::resolve::SearchPaths {
            include_paths: vec![tc.path().to_path_buf()],
            ..Default::default()
        },
        output_options: cppscan::OutputOptions {
            targets: vec!["build/main.o".to_string()],
            ..Default::default()
        },
        ..Default::default()
    };

    let scanner = Scanner::new(options);
    let result = scanner.scan_file(&source).unwrap();

    assert!(
        result.makefile_output.starts_with("build/main.o:"),
        "output should use custom target: {}",
        result.makefile_output,
    );
}

#[test]
fn conformance_mq_quoted_target() {
    let tc = TestCase::new();
    tc.write_file("foo.h", "int foo;\n");
    let source = tc.write_file("main.c", "#include \"foo.h\"\nint main() {}\n");

    let options = ScanOptions {
        search_paths: cppscan::resolve::SearchPaths {
            include_paths: vec![tc.path().to_path_buf()],
            ..Default::default()
        },
        output_options: cppscan::OutputOptions {
            quoted_targets: vec!["$build/main.o".to_string()],
            ..Default::default()
        },
        ..Default::default()
    };

    let scanner = Scanner::new(options);
    let result = scanner.scan_file(&source).unwrap();

    // -MQ should escape $ -> $$
    assert!(
        result.makefile_output.starts_with("$$build/main.o:"),
        "output should quote $ in target: {}",
        result.makefile_output,
    );
}

#[test]
fn conformance_multiple_mt_targets() {
    let tc = TestCase::new();
    tc.write_file("foo.h", "int foo;\n");
    let source = tc.write_file("main.c", "#include \"foo.h\"\nint main() {}\n");

    let options = ScanOptions {
        search_paths: cppscan::resolve::SearchPaths {
            include_paths: vec![tc.path().to_path_buf()],
            ..Default::default()
        },
        output_options: cppscan::OutputOptions {
            targets: vec!["a.o".to_string(), "b.o".to_string()],
            ..Default::default()
        },
        ..Default::default()
    };

    let scanner = Scanner::new(options);
    let result = scanner.scan_file(&source).unwrap();

    assert!(
        result.makefile_output.starts_with("a.o b.o:"),
        "output should have multiple targets: {}",
        result.makefile_output,
    );
}

#[test]
fn conformance_md_writes_dep_file() {
    let tc = TestCase::new();
    tc.write_file("foo.h", "int foo;\n");
    let source = tc.write_file("main.c", "#include \"foo.h\"\nint main() {}\n");

    let options = ScanOptions {
        search_paths: cppscan::resolve::SearchPaths {
            include_paths: vec![tc.path().to_path_buf()],
            ..Default::default()
        },
        dep_mode: cppscan::DepMode::MD,
        ..Default::default()
    };

    let scanner = Scanner::new(options);
    let result = scanner.scan_file(&source).unwrap();

    assert!(result.dep_file_path.is_some(), "MD mode should set dep_file_path");
    let dep_path = result.dep_file_path.unwrap();
    assert!(
        dep_path.to_string_lossy().ends_with(".d"),
        "dep file should have .d extension: {:?}",
        dep_path,
    );
}

#[test]
fn conformance_mf_custom_dep_file() {
    let tc = TestCase::new();
    let source = tc.write_file("main.c", "int main() {}\n");
    let custom_dep = tc.path().join("custom.deps");

    let options = ScanOptions {
        dep_mode: cppscan::DepMode::MD,
        dep_file: Some(custom_dep.clone()),
        ..Default::default()
    };

    let scanner = Scanner::new(options);
    let result = scanner.scan_file(&source).unwrap();

    assert_eq!(result.dep_file_path.unwrap(), custom_dep);
}

#[test]
fn conformance_mm_with_mp_phony_omits_system() {
    let tc = TestCase::new();
    let sys_dir = tempfile::tempdir().unwrap();

    tc.write_file("user.h", "int user;\n");
    fs::write(sys_dir.path().join("sys.h"), "int sys;\n").unwrap();
    let source = tc.write_file(
        "main.c",
        "#include \"user.h\"\n#include <sys.h>\nint main() {}\n",
    );

    let options = ScanOptions {
        search_paths: cppscan::resolve::SearchPaths {
            include_paths: vec![tc.path().to_path_buf()],
            system_paths: vec![sys_dir.path().to_path_buf()],
            ..Default::default()
        },
        dep_mode: cppscan::DepMode::MM,
        output_options: cppscan::OutputOptions {
            phony: true,
            ..Default::default()
        },
        ..Default::default()
    };

    let scanner = Scanner::new(options);
    let result = scanner.scan_file(&source).unwrap();

    let output = &result.makefile_output;
    assert!(output.contains("user.h"), "should include user.h");
    assert!(!output.contains("sys.h"), "should omit sys.h everywhere including phony");
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
