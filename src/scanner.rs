//! Top-level scanner API.
//!
//! Orchestrates the full dependency scanning pipeline with parallelism.

use rayon::prelude::*;
use std::path::{Path, PathBuf};

use crate::depgraph::{self, DepEntry, FileCache};
use crate::output::{self, OutputOptions};
use crate::resolve::SearchPaths;

/// Dependency output mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DepMode {
    /// `-M`: Print all deps to stdout.
    #[default]
    M,
    /// `-MM`: Print non-system deps to stdout.
    MM,
    /// `-MD`: Write all deps to `.d` file (or `-MF` path).
    MD,
    /// `-MMD`: Write non-system deps to `.d` file (or `-MF` path).
    MMD,
}

impl DepMode {
    /// Whether system headers should be omitted from output.
    pub fn omit_system(self) -> bool {
        matches!(self, DepMode::MM | DepMode::MMD)
    }

    /// Whether output goes to a file (`.d` or `-MF` override) instead of stdout.
    pub fn writes_to_file(self) -> bool {
        matches!(self, DepMode::MD | DepMode::MMD)
    }
}

/// Options for the dependency scanner.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Include search paths.
    pub search_paths: SearchPaths,
    /// Custom target name format (None = derive from source filename).
    /// Deprecated in favor of `output_options.targets`; kept for backward compat.
    pub target: Option<String>,
    /// Command-line macro definitions (`-D`). Each entry is `(name, optional_value)`.
    pub defines: Vec<(String, Option<String>)>,
    /// Dependency output mode (`-M`, `-MM`, `-MD`, `-MMD`).
    pub dep_mode: DepMode,
    /// Output file override (`-MF`). Only used when `dep_mode` writes to file.
    pub dep_file: Option<PathBuf>,
    /// Output formatting options (`-MT`, `-MQ`, `-MP`).
    pub output_options: OutputOptions,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            search_paths: SearchPaths::default(),
            target: None,
            defines: Vec::new(),
            dep_mode: DepMode::default(),
            dep_file: None,
            output_options: OutputOptions::default(),
        }
    }
}

/// Result of scanning a single translation unit.
#[derive(Debug)]
pub struct ScanResult {
    /// The source file that was scanned.
    pub source: PathBuf,
    /// All transitive dependencies (resolved, canonical paths).
    pub deps: Vec<DepEntry>,
    /// Makefile-format dependency output.
    pub makefile_output: String,
    /// If the output should be written to a file, this is the path.
    pub dep_file_path: Option<PathBuf>,
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
        let deps = depgraph::collect_deps(
            source,
            &self.options.search_paths,
            &self.cache,
            &self.options.defines,
        )?;

        // Build effective output options
        let mut out_opts = self.options.output_options.clone();
        out_opts.omit_system = self.options.dep_mode.omit_system();

        // Legacy `target` field support: if no -MT/-MQ set, use legacy target
        if out_opts.targets.is_empty() && out_opts.quoted_targets.is_empty() {
            if let Some(ref t) = self.options.target {
                out_opts.targets.push(t.clone());
            }
        }

        let makefile_output = output::format_makefile_deps_with_options(
            source,
            &deps,
            &out_opts,
        );

        // Determine dep file path
        let dep_file_path = if self.options.dep_mode.writes_to_file() {
            Some(self.options.dep_file.clone().unwrap_or_else(|| {
                // Default: replace source extension with .d
                let mut p = source.to_path_buf();
                p.set_extension("d");
                p
            }))
        } else {
            None
        };

        Ok(ScanResult {
            source: source.to_path_buf(),
            deps,
            makefile_output,
            dep_file_path,
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
        assert!(result.dep_file_path.is_none());
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

    #[test]
    fn test_scanner_md_mode() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("header.h"), "#pragma once\nint x;\n").unwrap();
        fs::write(dir.path().join("main.c"), "#include \"header.h\"\n").unwrap();

        let scanner = Scanner::new(ScanOptions {
            dep_mode: DepMode::MD,
            ..Default::default()
        });
        let result = scanner.scan_file(&dir.path().join("main.c")).unwrap();

        assert!(result.dep_file_path.is_some());
        assert!(result.dep_file_path.unwrap().to_string_lossy().ends_with(".d"));
    }

    #[test]
    fn test_scanner_mf_override() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.c"), "int main() {}\n").unwrap();

        let custom_path = dir.path().join("custom.deps");
        let scanner = Scanner::new(ScanOptions {
            dep_mode: DepMode::MD,
            dep_file: Some(custom_path.clone()),
            ..Default::default()
        });
        let result = scanner.scan_file(&dir.path().join("main.c")).unwrap();

        assert_eq!(result.dep_file_path.unwrap(), custom_path);
    }

    #[test]
    fn test_scanner_mm_omits_system() {
        let dir = tempfile::tempdir().unwrap();
        let sys_dir = tempfile::tempdir().unwrap();

        fs::write(sys_dir.path().join("sys.h"), "int sys;\n").unwrap();
        fs::write(dir.path().join("user.h"), "int user;\n").unwrap();
        fs::write(
            dir.path().join("main.c"),
            "#include \"user.h\"\n#include <sys.h>\n",
        )
        .unwrap();

        let scanner = Scanner::new(ScanOptions {
            search_paths: SearchPaths {
                include_paths: vec![dir.path().to_path_buf()],
                system_paths: vec![sys_dir.path().to_path_buf()],
                ..Default::default()
            },
            dep_mode: DepMode::MM,
            ..Default::default()
        });
        let result = scanner.scan_file(&dir.path().join("main.c")).unwrap();

        assert!(result.makefile_output.contains("user.h"));
        assert!(!result.makefile_output.contains("sys.h"));
    }
}
