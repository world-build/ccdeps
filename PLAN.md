# Implementation Plan: gcc/clang-Conformant C Preprocessor Dependency Scanner

## Goal

Produce output **byte-identical** to `gcc -M` / `clang -M` for any C/C++ translation unit,
at dramatically higher speed through parallelism and optimized scanning.

Primary consumer: **nix-ninja**, replacing the `deps-infer` crate. Since nix provides the
build graph, all search paths (`-I`, `-isystem`, `-iquote`) are passed in directly — no
need to query compilers for their built-in paths.

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Parser | **Handrolled Pratt parser** | Grammar is ~13 precedence levels, fixed by C standard. GCC's own `expr.cc` is a ~350-line operator-precedence parser. Chumsky adds compile-time cost (exponential trait-solver in `or` chains), allocation overhead, and integration friction — all for a grammar that fits in ~300 lines of Rust. |
| Allocation | **Zero-alloc hot path** | The `#if` evaluator returns `i64` directly (no AST). Directives borrow `&'a str` from input. File contents cached once in `Arc`. Only the dep result list and output formatting allocate. Bumpalo arenas for per-TU scratch space. |
| Search paths | **Caller-provided** | nix-ninja knows all paths from the build graph. No compiler querying. |
| Parallelism | **Rayon + DashMap file cache** | TU-level parallelism with shared file cache. Single-file scans skip thread pool setup. |

## Reference Implementations (cloned to /tmp)

| Source | Path | Key files |
|--------|------|-----------|
| GCC libcpp | `/tmp/gcc/libcpp/` | `mkdeps.cc` (dep output), `files.cc` (include guard opt), `directives.cc` (directive handling), `expr.cc` (#if evaluation) |
| clang-scan-deps | `/tmp/clang/` (inside llvm-project monorepo) | `clang/tools/clang-scan-deps/ClangScanDeps.cpp` (CLI), `clang/lib/Tooling/DependencyScanning/` (scanning pipeline), `clang/lib/Lex/DependencyDirectivesScanner.cpp` (source minimization — the key optimization), `clang/test/ClangScanDeps/` (120 test files), `clang/unittests/Lex/DependencyDirectivesScannerTest.cpp` (unit tests) |
| Warp (Facebook, D lang) | `/tmp/warp/` | `directive.d`, `lexer.d`, `macros.d`, `outdeps.d`, `skip.d` |

## Conformance Test Sources

There is no standalone spec for `-M` output — the compilers **are** the spec. We test by comparing
our output against both gcc and clang on the same inputs.

**Existing test suites we can mine for test cases:**

| Suite | Path | Count | Notes |
|-------|------|-------|-------|
| GCC cpp tests | `/tmp/gcc/gcc/testsuite/gcc.dg/cpp/` | 565 files | `cmdlne-M.c`, `cmdlne-M-2.c`, `include1-7.c`, `if-*.c`, `pragma-once-1.c` |
| GCC common cpp | `/tmp/gcc/gcc/testsuite/c-c++-common/cpp/` | 174 files | `has-include-*.c`, `Wheader-guard-*.c`, `dir-only-*.c` |
| Clang scan-deps tests | `/tmp/clang/clang/test/ClangScanDeps/` | 120 files | Header guard opt, macro expansions, pragma once, symlinks |
| Clang preprocessor tests | `/tmp/clang/clang/test/Preprocessor/` | 351 files | `dependencies-and-pp.c` (-M/-MD/-MF/-MT/-MQ), `has_include.c`, `include-*.c` |
| Clang scanner unit tests | `/tmp/clang/clang/unittests/Lex/DependencyDirectivesScannerTest.cpp` | ~1500 lines | Comprehensive minimization coverage |
| Clang PP dep unit tests | `/tmp/clang/clang/unittests/Lex/PPDependencyDirectivesTest.cpp` | ~200 lines | Guard optimization, #embed integration |
| Warp tests | `/tmp/warp/tests/` | test dir | D-language test cases for directive/macro/include handling |

## Current State

We have a working skeleton with 5 modules (`minimize`, `directive`, `resolve`, `depgraph`, `output`)
and a CLI (`cppscan-cli`). All 30 tests pass, including 7 conformance tests that compare against
gcc and clang on simple cases. What's missing for full conformance:

## Phase 1: `#if` Expression Evaluation

**Goal:** Correctly evaluate conditional directives so we only follow the include paths that
a real compiler would follow. Without this, we conservatively include both branches of every
`#if`/`#else`, producing supersets of the real dependency list.

### 1a. Pratt parser / evaluator (`src/eval.rs`)

Handrolled Pratt parser following GCC's `expr.cc` (lines 1454-1810). Combined parse-and-evaluate
in a single pass — no AST, returns `i64` directly. Zero allocations.

**Tokenizer** (~100 lines): `next_token()` over `&str` recognizing:
- Integer constants: decimal, hex (`0x`), octal (`0`), binary (`0b`), with `L`/`LL`/`U` suffixes
- Character constants: `'a'`, `'\n'`, `'\x41'`, `L'x'`
- Operators: all C arithmetic, comparison, bitwise, logical, ternary
- Special: `defined`, `__has_include`, identifiers, parentheses

**Pratt core** (~150-200 lines): Assign binding powers per operator:
- Level 16: `!`, `~`, unary `+`/`-`
- Level 15: `*`, `/`, `%`
- Level 13: `<<`, `>>`
- Level 12: `<`, `<=`, `>`, `>=`
- Level 11: `==`, `!=`
- Level 9: `&`
- Level 8: `^`
- Level 7: `|`
- Level 6: `&&` (short-circuit)
- Level 5: `||` (short-circuit)
- Level 4: `?` `:`
- `defined(X)` and `__has_include(...)` as prefix operators in null denotation

**Evaluator** (~100 lines): Evaluate to `i64` in the recursive descent.
Undefined identifiers → `0` (per spec).

### 1b. Macro table (`src/macros.rs`)

- Track `#define` / `#undef` through file processing
- Support command-line `-D` and `-U` flags
- Object-like macros only initially (sufficient for include guard detection)
- Function-like macro expansion in phase 2

### 1c. Conditional directive handling (`src/depgraph.rs` updates)

- Walk directives sequentially, maintaining a conditional stack
- Evaluate `#if`/`#ifdef`/`#ifndef` conditions using the macro table + evaluator
- Skip `#else`/`#elif` branches when the taken branch is known
- Track `skip_eval` depth for short-circuit `&&`/`||`

### Verify

```bash
# Should now match gcc -M exactly on conditionally-included files:
echo '#define USE_FOO 1
#if USE_FOO
#include "foo.h"
#else
#include "bar.h"
#endif' > test.c
# gcc -M test.c -> only foo.h
# cppscan test.c -> only foo.h (not both)
```

## Phase 2: Full Macro Expansion

**Goal:** Handle computed includes (`#include MACRO`) and function-like macros in `#if` expressions.

### 2a. Function-like macro expansion (`src/macros.rs`)

- Argument parsing and substitution
- `#` (stringification) and `##` (token pasting) operators
- Recursive expansion with re-entrancy guard (GCC uses `NODE_DISABLED`)
- `__VA_ARGS__` and `__VA_OPT__` support
- Predefined macros: `__FILE__`, `__LINE__`, `__DATE__`, `__TIME__`, `__COUNTER__`

### 2b. Computed includes

- Macro-expand the tokens after `#include`
- Re-parse the expanded result as `"path"` or `<path>`
- Handle multi-token expansions that concatenate into a path

### 2c. `__has_include` in `#if` expressions

- Evaluate `__has_include("path")` / `__has_include(<path>)` by actually
  probing the filesystem using the configured search paths
- Required because `#if __has_include(<optional>)` is common in modern code

### Verify

```bash
# Computed include:
echo '#define HEADER "foo.h"
#include HEADER' > test.c
# Should resolve foo.h correctly

# __has_include:
echo '#if __has_include(<stdio.h>)
#include <stdio.h>
#endif' > test.c
# Should include stdio.h only if it exists in search paths
```

## Phase 3: `-M` Flag Variants & Output Compatibility

**Goal:** Support all `-M` family flags with byte-identical output formatting.

### 3a. Flag support

| Flag | Behavior |
|------|----------|
| `-M` | Print deps to stdout, don't compile |
| `-MM` | Like `-M` but omit system headers |
| `-MD` | Like `-M` but write to `.d` file alongside compilation |
| `-MMD` | Like `-MM` + `-MD` |
| `-MF <file>` | Override the `.d` output filename |
| `-MT <target>` | Override the Make target name |
| `-MQ <target>` | Like `-MT` but with Make-quoting of special chars |
| `-MP` | Add phony targets for each dependency (prevents Make errors on header deletion) |
| `-MG` | Treat missing headers as generated (add to deps instead of erroring) |

### 3b. Output formatting (match gcc/clang exactly)

- Line wrapping at column 76 with ` \` continuation
- Multiple `-MT`/`-MQ` targets on the same rule
- Make-escaping: spaces (`\ `), `#` (`\#`), `$` (`$$`)
- Path normalization (match whether compilers canonicalize or not)
- Trailing newline behavior

### 3c. Phony targets (`-MP`)

```makefile
main.o: main.c foo.h bar.h

foo.h:

bar.h:
```

### Verify

Extend conformance suite to test each flag variant against both gcc and clang,
comparing output byte-for-byte.

## Phase 4: Performance Optimization

**Goal:** Fastest possible wall time for both small (1 file) and large (10k+ files) inputs.

### 4a. Startup overhead minimization

- Lazy initialization: don't build thread pool for single-file scans
- Pre-allocate arena per TU with estimated size
- Avoid filesystem queries until needed

### 4b. Source minimization improvements

- SIMD scanning for `#` at line start (vectorized `memchr` for `\n` followed by optional
  whitespace then `#`)
- Skip string literals and comments in the minimizer to avoid false `#` matches
  (currently a known limitation)
- Benchmark against clang's `DependencyDirectivesScanner` on real codebases

### 4c. File I/O optimization

- `memmap2` for files above a size threshold (~4KB), `read` for smaller files
  (mmap has fixed overhead that dominates for tiny files)
- `readahead` / `posix_fadvise(SEQUENTIAL)` hints for large directory scans
- Batch `stat` calls where possible

### 4d. Parallelism tuning

- Rayon work-stealing for TU-level parallelism (already implemented)
- Shared `DashMap` file cache with sharding (already implemented)
- Profile lock contention and consider per-thread local caches with write-through
  (matching clang's `DependencyScanningFilesystemLocalCache` pattern)

### 4e. Zero-alloc hot path

- Pratt evaluator returns `i64` directly (no AST, no allocs) — phase 1
- `bumpalo` arena per TU for directive/token scratch space
- Intern resolved paths into a global table (avoid `PathBuf` per include)
- Pool small string allocations for include path segments
- Stack-allocated buffers with heap fallback for minimizer output (Warp's `Textbuf` pattern)

### Verify

```bash
# Benchmark on a real codebase (e.g., the Linux kernel headers):
hyperfine --warmup 3 \
  'gcc -M -I/usr/include kernel/*.c' \
  'cppscan -I/usr/include kernel/*.c'

# Target: 5-10x faster than gcc -M for large inputs
# Target: <5ms overhead for single-file inputs
```

## Phase 5: Edge Cases & Full Conformance

**Goal:** Handle every corner case that gcc/clang handle.

### 5a. Trigraphs and digraphs

- `??=` -> `#` (trigraph for hash, relevant in ancient code)
- `%:` -> `#` (digraph)
- Controlled by `-trigraphs` flag

### 5b. Line continuations in all contexts

- Backslash-newline inside directive keywords: `#inc\nlude`
- Backslash-newline inside include paths
- Multiple consecutive continuations

### 5c. `#include_next`

- GNU extension: search starting from the **next** directory in the search path
- Used heavily in system headers (e.g., glibc wrappers)

### 5d. `#import` (Objective-C)

- Like `#include` but with implicit once-only semantics

### 5e. `#pragma` interactions

- `#pragma GCC system_header` — changes diagnostic behavior and `-MM` filtering
- `#pragma GCC dependency "file"` — add file to dependency list
- `#pragma push_macro` / `pop_macro` — macro state save/restore

### 5f. Missing header behavior

- `-M` with missing headers: fatal error (match compiler behavior)
- `-MG` with missing headers: add to deps anyway
- `-MMD` with missing system headers: silently skip

### 5g. Response files

- `@file` argument syntax for passing large argument lists

### Verify

Build a comprehensive fuzz-test harness:
```bash
# For each .c file in gcc/clang test suites:
for f in /tmp/gcc/gcc/testsuite/gcc.dg/cpp/*.c; do
  diff <(gcc -M "$f" 2>/dev/null) <(cppscan "$f" 2>/dev/null) || echo "MISMATCH: $f"
done
```

## Architecture Principles

- **Compilers are the spec.** When gcc and clang disagree, document both behaviors
  and default to gcc (more widely used for dependency generation in build systems).
- **Correctness over speed.** Never trade correctness for performance — wrong
  dependencies cause silent build failures that waste far more time than scanning overhead.
- **Zero-alloc where it matters.** The hot path (minimize → parse directives → evaluate
  `#if` → resolve includes) should not allocate. Heap is fine for per-file caching and
  output formatting.
- **Each phase is independently verifiable.** Don't block on macro expansion
  to ship include guard detection — ship what works, extend incrementally.
- **Profile before optimizing.** Measure on real codebases (Linux kernel, LLVM itself)
  before adding complexity for speed.

## Key Performance Insights from Reference Implementations

**clang-scan-deps** (the state of the art, 10x faster than `clang -Eonly`):
1. Source minimization is the highest-impact single optimization
2. Shared file cache across threads — each file read+minimized once
3. Lexer skip optimization — bump pointer past false `#if` blocks
4. Sharded cache locks — N/4 shards to reduce contention

**Warp** (Facebook's preprocessor):
1. Zero-allocation hot path — stack buffers with heap fallback
2. Range-based phase — compiler can inline and optimize
3. Multi-file mode — cache reused across TUs

**GCC libcpp**:
1. Include guard detection is ~40% speedup on guarded-header-heavy projects
2. File hash keyed on (filename, start_dir) — handles quote vs angle efficiently
3. Once-only tracking separate from include guards (`#pragma once`, `#import`)
