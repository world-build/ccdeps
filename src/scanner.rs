//! CLI-facing scanner.
//!
//! A thin adapter that drives the [`ScanService`] engine and formats its result
//! as `gcc -M`-style Makefile text. The library trunk is the engine; this is
//! the leaf that keeps the command-line tool byte-compatible.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::macros::MacroBase;
use crate::output::{self, OutputOptions};
use crate::resolve::SearchPaths;
use crate::service::{ScanEnv, ScanOutput, ScanScratch, ScanService, ServiceOptions};
use crate::vfs::{OsVfs, Vfs};

/// Dependency output mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DepMode {
    /// `-M`: print all deps to stdout.
    #[default]
    M,
    /// `-MM`: print non-system deps to stdout.
    MM,
    /// `-MD`: write all deps to a `.d` file (or `-MF` path).
    MD,
    /// `-MMD`: write non-system deps to a `.d` file (or `-MF` path).
    MMD,
}

impl DepMode {
    /// Whether system headers should be omitted from output.
    pub fn omit_system(self) -> bool {
        matches!(self, DepMode::MM | DepMode::MMD)
    }

    /// Whether output goes to a file (`.d` or `-MF`) instead of stdout.
    pub fn writes_to_file(self) -> bool {
        matches!(self, DepMode::MD | DepMode::MMD)
    }
}

/// Options for the dependency scanner.
#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// Include search paths.
    pub search_paths: SearchPaths,
    /// Legacy target name (`-o`); superseded by `output_options.targets`.
    pub target: Option<String>,
    /// Command-line macro definitions (`-D`): `(name, optional_value)`.
    pub defines: Vec<(String, Option<String>)>,
    /// Dependency output mode (`-M`, `-MM`, `-MD`, `-MMD`).
    pub dep_mode: DepMode,
    /// Output file override (`-MF`). Used only when `dep_mode` writes to a file.
    pub dep_file: Option<PathBuf>,
    /// Output formatting options (`-MT`, `-MQ`, `-MP`).
    pub output_options: OutputOptions,
    /// Record missing headers as generated files (`-MG`).
    pub missing_as_generated: bool,
    /// Process trigraphs (`-trigraphs`).
    pub trigraphs: bool,
    /// Conservative mode: conditionals on unknown macros follow both branches.
    pub conservative: bool,
}

/// Result of scanning a single translation unit.
#[derive(Debug)]
pub struct ScanResult {
    /// The source file that was scanned.
    pub source: PathBuf,
    /// Resolved dependency paths (display spelling where one was kept), in order.
    pub deps: Vec<PathBuf>,
    /// Makefile-format dependency output.
    pub makefile_output: String,
    /// Where to write the output, for `-MD`/`-MMD`.
    pub dep_file_path: Option<PathBuf>,
}

/// The dependency scanner. Owns a [`ScanService`], so a shared header is read
/// and minimized once across every file this scanner handles.
pub struct Scanner {
    options: ScanOptions,
    service: ScanService,
    base: Arc<MacroBase>,
    vfs: Arc<dyn Vfs>,
}

impl std::fmt::Debug for Scanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scanner")
            .field("options", &self.options)
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

impl Scanner {
    pub fn new(options: ScanOptions) -> Self {
        Self::with_vfs(options, Arc::new(OsVfs))
    }

    /// Create a scanner that performs all file access through `vfs`.
    pub fn with_vfs(options: ScanOptions, vfs: Arc<dyn Vfs>) -> Self {
        let service = ScanService::with_options(ServiceOptions {
            trigraphs: options.trigraphs,
        });
        let base = Arc::new(base_from(&options.defines));
        Self {
            options,
            service,
            base,
            vfs,
        }
    }

    /// Scan a single translation unit.
    pub fn scan_file(&self, source: &Path) -> Result<ScanResult, std::io::Error> {
        let mut scratch = ScanScratch::new();
        let mut out = ScanOutput::new();
        self.scan_into(source, &self.options, &self.base, &mut scratch, &mut out)
    }

    /// Scan a single file with per-entry options, reusing the shared service.
    pub fn scan_file_with_options(
        &self,
        source: &Path,
        options: &ScanOptions,
    ) -> Result<ScanResult, std::io::Error> {
        let base = Arc::new(base_from(&options.defines));
        let mut scratch = ScanScratch::new();
        let mut out = ScanOutput::new();
        self.scan_into(source, options, &base, &mut scratch, &mut out)
    }

    fn scan_into(
        &self,
        source: &Path,
        options: &ScanOptions,
        base: &Arc<MacroBase>,
        scratch: &mut ScanScratch,
        out: &mut ScanOutput,
    ) -> Result<ScanResult, std::io::Error> {
        let env = ScanEnv {
            search_paths: &options.search_paths,
            macro_base: Arc::clone(base),
            conservative: options.conservative,
            missing_as_generated: options.missing_as_generated,
            trust_canonical: false,
            record_display: true,
        };
        self.service
            .scan_tu(source, &env, &*self.vfs, scratch, out, |_, _| {})?;

        let mut out_opts = options.output_options.clone();
        out_opts.omit_system = options.dep_mode.omit_system();
        if out_opts.targets.is_empty() && out_opts.quoted_targets.is_empty() {
            if let Some(t) = &options.target {
                out_opts.targets.push(t.clone());
            }
        }
        let makefile_output = output::format_makefile(&self.service, source, out, &out_opts);

        let dep_file_path = options.dep_mode.writes_to_file().then(|| {
            options.dep_file.clone().unwrap_or_else(|| {
                let mut p = source.to_path_buf();
                p.set_extension("d");
                p
            })
        });

        let deps = out
            .resolved
            .iter()
            .map(|d| match &d.display {
                Some(s) => PathBuf::from(s.as_ref()),
                None => self.service.path(d.file).to_path_buf(),
            })
            .collect();

        Ok(ScanResult {
            source: source.to_path_buf(),
            deps,
            makefile_output,
            dep_file_path,
        })
    }
}

/// Build a shared macro base from command-line `-D` defines.
fn base_from(defines: &[(String, Option<String>)]) -> MacroBase {
    let mut base = MacroBase::new();
    base.extend_defines(defines.iter().map(|(n, v)| (n.as_str(), v.as_deref())));
    base
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
    fn test_scanner_shares_cache_across_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("common.h"), "#pragma once\n").unwrap();
        fs::write(dir.path().join("a.c"), "#include \"common.h\"\n").unwrap();
        fs::write(dir.path().join("b.c"), "#include \"common.h\"\n").unwrap();

        // One scanner over several files: common.h is minimized once.
        let scanner = Scanner::new(ScanOptions::default());
        scanner.scan_file(&dir.path().join("a.c")).unwrap();
        scanner.scan_file(&dir.path().join("b.c")).unwrap();
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

        assert!(result
            .dep_file_path
            .unwrap()
            .to_string_lossy()
            .ends_with(".d"));
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
