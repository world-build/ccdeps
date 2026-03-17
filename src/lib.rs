//! cppscan - High-performance C preprocessor dependency scanner
//!
//! A dependency scanner that produces output identical to `gcc -M` / `clang -M`,
//! but dramatically faster through parallelism and optimized scanning techniques.
//!
//! # Architecture
//!
//! The scanner is organized into a pipeline:
//!
//! 1. **Source Minimization** (`minimize`): Strip everything except preprocessor
//!    directives from source files. This is the highest-impact optimization —
//!    most C/C++ files are >90% non-directive code. Uses SIMD-accelerated
//!    scanning via `memchr` to find `#` and newline characters in bulk.
//!
//! 2. **Directive Parsing** (`directive`): Parse minimized source into a stream
//!    of preprocessor directives (`#include`, `#define`, `#if`, etc.).
//!
//! 3. **Include Resolution** (`resolve`): Resolve `#include` paths against
//!    search paths, handling `""` vs `<>` includes, `-I` paths, and system paths.
//!
//! 4. **Dependency Graph** (`depgraph`): Build the full transitive dependency
//!    graph with include guard / `#pragma once` detection to avoid re-processing.
//!
//! 5. **Output** (`output`): Emit dependencies in Makefile format (matching
//!    `gcc -M` output exactly).
//!
//! # Performance Techniques
//!
//! - **SIMD scanning**: `memchr` crate for finding structural characters
//! - **Memory-mapped I/O**: Zero-copy file access via `memmap2`
//! - **File cache**: Shared across threads via `DashMap` — each file is
//!   read and minimized exactly once
//! - **Include guard detection**: Files with include guards are not re-opened
//! - **Rayon parallelism**: Work-stealing thread pool for scanning multiple TUs
//! - **Arena allocation**: Per-TU `bumpalo` arenas to minimize allocator pressure
//! - **Lexer skip optimization**: When `#if` evaluates to false, skip directly
//!   to matching `#endif` without processing contents

pub mod minimize;
pub mod directive;
pub mod eval;
pub mod macros;
pub mod resolve;
pub mod depgraph;
pub mod output;
pub mod scanner;

pub use scanner::{Scanner, ScanResult, ScanOptions};
