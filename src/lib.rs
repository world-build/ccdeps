//! ccdeps - High-performance C preprocessor dependency scanner
//!
//! A dependency scanner that produces output identical to `gcc -M` / `clang -M`,
//! but dramatically faster through parallelism and optimized scanning techniques.
//!
//! # Architecture
//!
//! The scanner is organized into a pipeline:
//!
//! 1. **Source Minimization** (`minimize`): Strip everything except preprocessor
//!    directives from source files. This is the highest-impact optimization,
//!    since most C/C++ files are >90% non-directive code. Uses SIMD-accelerated
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
//! # Embedding
//!
//! The engine is a long-lived [`ScanService`] plus per-worker [`ScanScratch`]
//! and per-result [`ScanOutput`]. [`ScanService::scan_tu`] scans one
//! translation unit and reports each dependency file through an `observe`
//! callback, so an embedder can record incremental facts as it goes. Paths are
//! interned to a compact [`FileId`]; file access goes through the [`Vfs`] trait.
//! [`Scanner`] is a thin wrapper over the engine that emits `gcc -M`-style
//! Makefile text for the CLI.
//!
//! # Performance techniques
//!
//! - `memchr` for SIMD-accelerated structural-character scanning
//! - `memmap2` for reading large files
//! - A content cache in the [`ScanService`], keyed by [`FileId`], so each file
//!   is read and minimized once across every TU
//! - Include-guard / `#pragma once` detection so guarded headers are not
//!   re-walked
//! - A reusable [`ScanScratch`] whose buffers keep capacity between TUs, so the
//!   warm path allocates nothing
//! - Skipping the body of a `#if` that evaluates to false

pub mod depgraph;
pub mod directive;
pub mod eval;
pub mod intern;
pub mod macros;
pub mod minimize;
pub mod output;
pub mod resolve;
pub mod scanner;
pub mod service;
pub mod vfs;

pub use directive::IncludeStyle;
pub use intern::{ContentHash, FileId};
pub use macros::MacroBase;
pub use output::OutputOptions;
pub use scanner::{DepMode, ScanOptions, ScanResult, Scanner};
pub use service::{
    FileInfo, ResolvedDep, ScanEnv, ScanOutput, ScanScratch, ScanService, ServiceOptions,
};
pub use vfs::{FileType, OsVfs, Stat, Vfs, VfsError};
