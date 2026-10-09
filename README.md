# ccdeps

A C and C++ preprocessor dependency scanner. It emits the same dependency lists
as `gcc -M` and `clang -M` without running a full preprocess. Instead each file
is minimized to its directives, only the includes the conditionals actually
reach are followed, and every header is read once across the whole build.

`ccdeps` is intended to be consumed as a library crate. The engine exposes a
shared content cache and reports each dependency via a callback. This allows
a consuming build system to record incrementally as it scans.

## Usage

Each call scans a translation unit and hands each dependency to a callback:

```rust
let service = ccdeps::ScanService::new();
let mut scratch = ccdeps::ScanScratch::new();
let mut out = ccdeps::ScanOutput::new();

service.scan_tu(&source, &env, &vfs, &mut scratch, &mut out, |file, hash| {
    // this TU depends on `file` (a FileId) with content `hash`
})?;

for file in out.includes() {
    // resolved non-system headers, in first-encounter order
}
```

The `ccdeps` CLI wraps the engine and is used for benchmarking and for
conformance testing against the real compilers.

```sh
# gcc -M compatible output for one file
ccdeps -I include src/main.c

# from a compile_commands.json
ccdeps -p compile_commands.json
```

## License

ccdeps is licensed under the [MIT License](LICENSE).
