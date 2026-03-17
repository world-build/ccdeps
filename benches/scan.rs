use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use cppscan::minimize;
use cppscan::{ScanOptions, Scanner};
use std::fs;

fn bench_minimize(c: &mut Criterion) {
    // Synthetic source file with realistic mix of code and directives
    let mut source = String::new();
    source.push_str("#ifndef BENCH_H\n#define BENCH_H\n\n");
    source.push_str("#include <stdio.h>\n#include <stdlib.h>\n#include \"myheader.h\"\n\n");
    for i in 0..1000 {
        source.push_str(&format!(
            "int function_{}(int a, int b) {{ return a + b; }}\n",
            i
        ));
        if i % 100 == 0 {
            source.push_str(&format!("#include \"header_{}.h\"\n", i));
        }
    }
    source.push_str("#endif\n");

    let source_bytes = source.as_bytes();

    c.bench_function("minimize_synthetic_source", |b| {
        b.iter(|| minimize::minimize(source_bytes));
    });
}

fn bench_minimize_with_strings_and_comments(c: &mut Criterion) {
    // Source file with string literals and comments containing '#' chars
    let mut source = String::new();
    source.push_str("#include <stdio.h>\n");
    for i in 0..500 {
        source.push_str(&format!(
            "char *s_{} = \"#include <fake_{}.h>\";\n",
            i, i
        ));
        source.push_str(&format!(
            "// This is a comment with #define FAKE_{}\n",
            i
        ));
        source.push_str(&format!(
            "/* Block comment with #include \"fake_{}.h\" */\n",
            i
        ));
    }
    source.push_str("#include \"real.h\"\n");

    let source_bytes = source.as_bytes();

    c.bench_function("minimize_strings_comments", |b| {
        b.iter(|| minimize::minimize(source_bytes));
    });
}

fn bench_full_pipeline(c: &mut Criterion) {
    // Benchmark the full scan pipeline on a realistic header tree
    let dir = tempfile::tempdir().unwrap();

    // Create a chain of headers: h0.h -> h1.h -> ... -> h19.h
    for i in (0..20).rev() {
        let mut content = format!("#ifndef H{}_H\n#define H{}_H\n", i, i);
        if i < 19 {
            content.push_str(&format!("#include \"h{}.h\"\n", i + 1));
        }
        content.push_str(&format!("int var_{};\n#endif\n", i));
        fs::write(dir.path().join(format!("h{}.h", i)), &content).unwrap();
    }

    // Main source includes the root header
    let source_content = "#include \"h0.h\"\nint main() { return 0; }\n";
    let source_path = dir.path().join("main.c");
    fs::write(&source_path, source_content).unwrap();

    c.bench_function("full_pipeline_20_headers", |b| {
        b.iter(|| {
            let scanner = Scanner::new(ScanOptions {
                search_paths: cppscan::resolve::SearchPaths {
                    include_paths: vec![dir.path().to_path_buf()],
                    ..Default::default()
                },
                ..Default::default()
            });
            scanner.scan_file(&source_path).unwrap();
        });
    });
}

fn bench_full_pipeline_parallel(c: &mut Criterion) {
    // Benchmark parallel scanning of multiple TUs sharing headers
    let dir = tempfile::tempdir().unwrap();

    // Shared headers
    for i in 0..10 {
        let content = format!(
            "#ifndef SHARED_{}_H\n#define SHARED_{}_H\nint shared_{};\n#endif\n",
            i, i, i
        );
        fs::write(dir.path().join(format!("shared{}.h", i)), &content).unwrap();
    }

    // Multiple source files, each including a subset of shared headers
    let mut sources = Vec::new();
    for i in 0..50 {
        let mut content = String::new();
        for j in 0..10 {
            if (i + j) % 3 == 0 {
                content.push_str(&format!("#include \"shared{}.h\"\n", j));
            }
        }
        content.push_str(&format!("int main_{}() {{ return 0; }}\n", i));
        let path = dir.path().join(format!("src{}.c", i));
        fs::write(&path, &content).unwrap();
        sources.push(path);
    }

    c.bench_function("parallel_50_files", |b| {
        b.iter(|| {
            let scanner = Scanner::new(ScanOptions {
                search_paths: cppscan::resolve::SearchPaths {
                    include_paths: vec![dir.path().to_path_buf()],
                    ..Default::default()
                },
                ..Default::default()
            });
            scanner.scan_files(&sources);
        });
    });
}

fn bench_single_file_startup(c: &mut Criterion) {
    // Measure startup overhead for single-file scans (target: <5ms)
    let dir = tempfile::tempdir().unwrap();
    let source_path = dir.path().join("trivial.c");
    fs::write(&source_path, "int main() { return 0; }\n").unwrap();

    c.bench_function("single_file_trivial", |b| {
        b.iter(|| {
            let scanner = Scanner::new(ScanOptions::default());
            scanner.scan_file(&source_path).unwrap();
        });
    });
}

fn bench_minimize_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("minimize_by_size");

    for &size_kb in &[1, 4, 16, 64, 256] {
        let mut source = String::new();
        source.push_str("#include <stdio.h>\n");
        while source.len() < size_kb * 1024 {
            source.push_str("int x = 42; // filler code\n");
        }
        source.push_str("#include \"end.h\"\n");
        let source_bytes = source.into_bytes();

        group.bench_with_input(
            BenchmarkId::new("minimize", format!("{}KB", size_kb)),
            &source_bytes,
            |b, src| {
                b.iter(|| minimize::minimize(src));
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_minimize,
    bench_minimize_with_strings_and_comments,
    bench_minimize_sizes,
    bench_full_pipeline,
    bench_full_pipeline_parallel,
    bench_single_file_startup,
);
criterion_main!(benches);
