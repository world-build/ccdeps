use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId, Throughput};
use cppscan::minimize;
use cppscan::{ScanOptions, Scanner};
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn collect_files_recursive(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(collect_files_recursive(&path, ext));
            } else if path.extension().map_or(false, |e| e == ext) {
                files.push(path);
            }
        }
    }
    files
}

const CLANG_LIB: &str = "/tmp/clang/clang/lib";
const CLANG_INC: &str = "/tmp/clang/clang/include";

fn has_real_data() -> bool {
    Path::new(CLANG_LIB).is_dir()
}

fn real_scan_options() -> ScanOptions {
    ScanOptions {
        search_paths: cppscan::resolve::SearchPaths {
            include_paths: vec![PathBuf::from(CLANG_INC)],
            ..Default::default()
        },
        missing_as_generated: true,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Synthetic benchmarks
// ---------------------------------------------------------------------------

fn bench_minimize(c: &mut Criterion) {
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

fn bench_full_pipeline(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();

    for i in (0..20).rev() {
        let mut content = format!("#ifndef H{}_H\n#define H{}_H\n", i, i);
        if i < 19 {
            content.push_str(&format!("#include \"h{}.h\"\n", i + 1));
        }
        content.push_str(&format!("int var_{};\n#endif\n", i));
        fs::write(dir.path().join(format!("h{}.h", i)), &content).unwrap();
    }

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
    let dir = tempfile::tempdir().unwrap();

    for i in 0..10 {
        let content = format!(
            "#ifndef SHARED_{}_H\n#define SHARED_{}_H\nint shared_{};\n#endif\n",
            i, i, i
        );
        fs::write(dir.path().join(format!("shared{}.h", i)), &content).unwrap();
    }

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

// ---------------------------------------------------------------------------
// Real-world benchmarks (require /tmp/clang)
// ---------------------------------------------------------------------------

fn bench_minimize_real(c: &mut Criterion) {
    if !has_real_data() {
        eprintln!("Skipping real-file benchmarks: /tmp/clang not found");
        return;
    }

    let files = [
        ("small_4KB", "/tmp/clang/clang/lib/DirectoryWatcher/DirectoryScanner.cpp"),
        ("medium_210KB", "/tmp/clang/clang/lib/AST/Expr.cpp"),
        ("large_886KB", "/tmp/clang/clang/lib/Sema/SemaExpr.cpp"),
        ("xlarge_1MB", "/tmp/clang/clang/lib/Sema/SemaOpenMP.cpp"),
    ];

    let mut group = c.benchmark_group("minimize_real");
    for (label, path) in &files {
        let path = Path::new(path);
        if !path.is_file() {
            continue;
        }
        let source = fs::read(path).unwrap();
        group.throughput(Throughput::Bytes(source.len() as u64));
        group.bench_function(*label, |b| {
            b.iter(|| minimize::minimize(&source));
        });
    }
    group.finish();
}

fn bench_scan_single_real(c: &mut Criterion) {
    if !has_real_data() {
        return;
    }

    let source = PathBuf::from("/tmp/clang/clang/lib/Sema/SemaExpr.cpp");
    if !source.is_file() {
        return;
    }

    let mut group = c.benchmark_group("scan_single_real");

    // Cold cache: new Scanner (and file cache) each iteration
    group.bench_function("cold_cache", |b| {
        b.iter(|| {
            let scanner = Scanner::new(real_scan_options());
            scanner.scan_file(&source).unwrap();
        });
    });

    // Warm cache: reuse the Scanner across iterations
    let scanner = Scanner::new(real_scan_options());
    // Prime the cache
    let _ = scanner.scan_file(&source);
    group.bench_function("warm_cache", |b| {
        b.iter(|| scanner.scan_file(&source).unwrap());
    });

    group.finish();
}

fn bench_scan_parallel_real(c: &mut Criterion) {
    if !has_real_data() {
        return;
    }

    let mut all_sources = collect_files_recursive(Path::new(CLANG_LIB), "cpp");
    all_sources.sort(); // deterministic ordering
    if all_sources.is_empty() {
        return;
    }

    let mut group = c.benchmark_group("scan_parallel_real");
    group.sample_size(10);

    for &count in &[10, 100, 1000] {
        let n = count.min(all_sources.len());
        let sources: Vec<PathBuf> = all_sources[..n].to_vec();
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(
            BenchmarkId::new("files", n),
            &sources,
            |b, srcs| {
                b.iter(|| {
                    let scanner = Scanner::new(real_scan_options());
                    scanner.scan_files(srcs);
                });
            },
        );
    }

    // All files
    let n = all_sources.len();
    group.throughput(Throughput::Elements(n as u64));
    group.bench_with_input(
        BenchmarkId::new("files", n),
        &all_sources,
        |b, srcs| {
            b.iter(|| {
                let scanner = Scanner::new(real_scan_options());
                scanner.scan_files(srcs);
            });
        },
    );

    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion setup
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_minimize,
    bench_minimize_with_strings_and_comments,
    bench_minimize_sizes,
    bench_full_pipeline,
    bench_full_pipeline_parallel,
    bench_single_file_startup,
    bench_minimize_real,
    bench_scan_single_real,
    bench_scan_parallel_real,
);
criterion_main!(benches);
