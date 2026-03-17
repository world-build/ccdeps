use criterion::{criterion_group, criterion_main, Criterion};
use cppscan::minimize;

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

criterion_group!(benches, bench_minimize);
criterion_main!(benches);
