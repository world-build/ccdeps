//! Top-level scanner API.
//!
//! Orchestrates the full dependency scanning pipeline with parallelism.

use rayon::prelude::*;
use std::path::{Path, PathBuf};

use crate::depgraph::{self, FileCache};
use crate::output;
use crate::resolve::SearchPaths;

/// Options for the dependency scanner.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Include search paths.
    pub search_paths: SearchPaths,
    /// Custom target name format (None = derive from source filename).
    pub target: Option<String>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            search_paths: SearchPaths::default(),
            target: None,
        }
    }
}

/// Result of scanning a single translation unit.
#[derive(Debug)]
pub struct ScanResult {
    /// The source file that was scanned.
    pub source: PathBuf,
    /// All transitive dependencies (resolved, canonical paths).
    pub deps: Vec<PathBuf>,
    /// Makefile-format dependency output.
    pub makefile_output: String,
}

/// The main dependency scanner.
#[derive(Debug)]
pub struct Scanner {
    options: ScanOptions,
    cache: FileCache,
}

impl Scanner {
    pub fn new(options: ScanOptions) -> Self {
        Self {
            options,
            cache: depgraph::new_file_cache(),
        }
    }

    /// Scan a single translation unit for dependencies.
    pub fn scan_file(&self, source: &Path) -> Result<ScanResult, std::io::Error> {
        let deps = depgraph::collect_deps(source, &self.options.search_paths, &self.cache)?;
        let makefile_output = output::format_makefile_deps(
            source,
            &deps,
            self.options.target.as_deref(),
        );

        Ok(ScanResult {
            source: source.to_path_buf(),
            deps,
            makefile_output,
        })
    }

    /// Scan multiple translation units in parallel.
    ///
    /// Uses Rayon's work-stealing thread pool. The file cache is shared
    /// across all threads, so common headers are only read and minimized once.
    pub fn scan_files(&self, sources: &[PathBuf]) -> Vec<Result<ScanResult, std::io::Error>> {
        sources
            .par_iter()
            .map(|source| self.scan_file(source))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_scanner_basic() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("header.h"), "#pragma once\nint x;\n").unwrap();
        fs::write(dir.path().join("main.c"), "#include \"header.h\"\n").unwrap();

        let scanner = Scanner::new(ScanOptions::default());
        let result = scanner.scan_file(&dir.path().join("main.c")).unwrap();

        assert_eq!(result.deps.len(), 1);
        assert!(result.makefile_output.contains("main.o:"));
        assert!(result.makefile_output.contains("header.h"));
    }

    #[test]
    fn test_scanner_parallel() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("common.h"), "#pragma once\n").unwrap();
        fs::write(dir.path().join("a.c"), "#include \"common.h\"\n").unwrap();
        fs::write(dir.path().join("b.c"), "#include \"common.h\"\n").unwrap();

        let scanner = Scanner::new(ScanOptions::default());
        let sources = vec![dir.path().join("a.c"), dir.path().join("b.c")];
        let results = scanner.scan_files(&sources);

        assert_eq!(results.len(), 2);
        for r in &results {
            assert!(r.is_ok());
        }
    }
}
