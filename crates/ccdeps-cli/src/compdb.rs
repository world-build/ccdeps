//! Compilation database (`compile_commands.json`) parsing.
//!
//! Parses the [JSON Compilation Database](https://clang.llvm.org/docs/JSONCompilationDatabase.html)
//! format and extracts preprocessor-relevant flags (`-I`, `-D`, `-isystem`, etc.)
//! for use with the scanner.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use ccdeps::resolve::SearchPaths;
use ccdeps::ScanOptions;

/// A single entry in a `compile_commands.json` file.
#[derive(Debug, Deserialize)]
pub struct CompDbEntry {
    /// The working directory of the compilation.
    pub directory: PathBuf,
    /// The compile command as a single string (shell-quoted).
    #[serde(default)]
    pub command: Option<String>,
    /// The compile command as a list of arguments.
    #[serde(default)]
    pub arguments: Option<Vec<String>>,
    /// The source file being compiled.
    pub file: PathBuf,
}

/// Flags extracted from a compilation command.
#[derive(Debug, Default)]
pub struct ExtractedFlags {
    pub include_paths: Vec<PathBuf>,
    pub quote_paths: Vec<PathBuf>,
    pub system_paths: Vec<PathBuf>,
    pub defines: Vec<(String, Option<String>)>,
    pub undefines: Vec<String>,
}

/// Parse a `compile_commands.json` file.
pub fn parse(path: &Path) -> Result<Vec<CompDbEntry>, std::io::Error> {
    let contents = std::fs::read_to_string(path)?;
    serde_json::from_str(&contents)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Extract preprocessor-relevant flags from a compilation database entry.
pub fn extract_flags(entry: &CompDbEntry) -> ExtractedFlags {
    let tokens: Vec<String> = if let Some(ref args) = entry.arguments {
        args.clone()
    } else if let Some(ref cmd) = entry.command {
        shlex::split(cmd).unwrap_or_default()
    } else {
        return ExtractedFlags::default();
    };

    let mut flags = ExtractedFlags::default();
    let mut iter = tokens.iter();

    while let Some(tok) = iter.next() {
        if tok == "-I" {
            if let Some(dir) = iter.next() {
                flags.include_paths.push(resolve_dir(&entry.directory, dir));
            }
        } else if let Some(dir) = tok.strip_prefix("-I") {
            flags.include_paths.push(resolve_dir(&entry.directory, dir));
        } else if tok == "-isystem" {
            if let Some(dir) = iter.next() {
                flags.system_paths.push(resolve_dir(&entry.directory, dir));
            }
        } else if tok == "-iquote" {
            if let Some(dir) = iter.next() {
                flags.quote_paths.push(resolve_dir(&entry.directory, dir));
            }
        } else if tok == "-D" {
            if let Some(def) = iter.next() {
                flags.defines.push(parse_define(def));
            }
        } else if let Some(def) = tok.strip_prefix("-D") {
            flags.defines.push(parse_define(def));
        } else if tok == "-U" {
            if let Some(name) = iter.next() {
                flags.undefines.push(name.clone());
            }
        } else if let Some(name) = tok.strip_prefix("-U") {
            flags.undefines.push(name.to_string());
        }
    }

    flags
}

/// Convert a compilation database entry into `ScanOptions`.
///
/// `base_defines` are prepended (e.g., from `--compiler-macros`).
/// Entry-level `-D` flags are appended, and `-U` flags remove from the combined set.
pub fn entry_to_scan_options(
    entry: &CompDbEntry,
    base_defines: &[(String, Option<String>)],
) -> (PathBuf, ScanOptions) {
    let flags = extract_flags(entry);

    // Resolve source file path
    let source = if entry.file.is_relative() {
        entry.directory.join(&entry.file)
    } else {
        entry.file.clone()
    };

    // Combine defines: base (compiler macros) + entry-level -D
    let mut defines: Vec<(String, Option<String>)> = base_defines.to_vec();
    defines.extend(flags.defines);

    // Apply -U flags
    for u in &flags.undefines {
        defines.retain(|(name, _)| name != u);
    }

    let options = ScanOptions {
        search_paths: SearchPaths {
            include_paths: flags.include_paths,
            quote_paths: flags.quote_paths,
            system_paths: flags.system_paths,
        },
        defines,
        ..Default::default()
    };

    (source, options)
}

fn resolve_dir(working_dir: &Path, dir: &str) -> PathBuf {
    let path = Path::new(dir);
    if path.is_relative() {
        working_dir.join(path)
    } else {
        path.to_path_buf()
    }
}

fn parse_define(s: &str) -> (String, Option<String>) {
    if let Some((name, value)) = s.split_once('=') {
        (name.to_string(), Some(value.to_string()))
    } else {
        (s.to_string(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_parse_command_format() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"[
            {
                "directory": "/tmp/build",
                "command": "gcc -I/inc -Irel/inc -DFOO=1 -DBAR -isystem /sys/inc -o main.o main.c",
                "file": "main.c"
            }
        ]"#;
        let db_path = dir.path().join("compile_commands.json");
        fs::write(&db_path, json).unwrap();

        let entries = parse(&db_path).unwrap();
        assert_eq!(entries.len(), 1);

        let flags = extract_flags(&entries[0]);
        assert_eq!(
            flags.include_paths,
            vec![PathBuf::from("/inc"), PathBuf::from("/tmp/build/rel/inc")]
        );
        assert_eq!(flags.system_paths, vec![PathBuf::from("/sys/inc")]);
        assert_eq!(
            flags.defines,
            vec![
                ("FOO".to_string(), Some("1".to_string())),
                ("BAR".to_string(), None),
            ]
        );
    }

    #[test]
    fn test_parse_arguments_format() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"[
            {
                "directory": "/tmp/build",
                "arguments": ["clang", "-I", "/inc", "-DFOO", "-U", "BAR", "-o", "main.o", "main.c"],
                "file": "main.c"
            }
        ]"#;
        let db_path = dir.path().join("compile_commands.json");
        fs::write(&db_path, json).unwrap();

        let entries = parse(&db_path).unwrap();
        let flags = extract_flags(&entries[0]);
        assert_eq!(flags.include_paths, vec![PathBuf::from("/inc")]);
        assert_eq!(flags.defines, vec![("FOO".to_string(), None)]);
        assert_eq!(flags.undefines, vec!["BAR".to_string()]);
    }

    #[test]
    fn test_entry_to_scan_options_relative_file() {
        let entry = CompDbEntry {
            directory: PathBuf::from("/tmp/build"),
            command: Some("gcc -I../include -DTEST main.c".to_string()),
            arguments: None,
            file: PathBuf::from("src/main.c"),
        };

        let base_defines = vec![("__GNUC__".to_string(), Some("15".to_string()))];
        let (source, opts) = entry_to_scan_options(&entry, &base_defines);

        assert_eq!(source, PathBuf::from("/tmp/build/src/main.c"));
        assert_eq!(
            opts.search_paths.include_paths,
            vec![PathBuf::from("/tmp/build/../include")]
        );
        // base defines come first, then entry-level
        assert_eq!(
            opts.defines[0],
            ("__GNUC__".to_string(), Some("15".to_string()))
        );
        assert_eq!(opts.defines[1], ("TEST".to_string(), None));
    }

    #[test]
    fn test_entry_undefine_overrides_base() {
        let entry = CompDbEntry {
            directory: PathBuf::from("/tmp"),
            command: Some("gcc -UFOO main.c".to_string()),
            arguments: None,
            file: PathBuf::from("main.c"),
        };

        let base_defines = vec![("FOO".to_string(), Some("1".to_string()))];
        let (_, opts) = entry_to_scan_options(&entry, &base_defines);

        // FOO should be removed by -U
        assert!(opts.defines.is_empty());
    }
}
