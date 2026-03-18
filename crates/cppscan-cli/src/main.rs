use anyhow::{Context, Result};
use clap::Parser;
use cppscan::{DepMode, OutputOptions, ScanOptions, Scanner};
use std::io::BufRead;
use std::path::PathBuf;
use std::process::Command;

/// High-performance C preprocessor dependency scanner.
///
/// Produces output compatible with `gcc -M` / `clang -M`.
#[derive(Parser, Debug)]
#[command(name = "cppscan", version, about)]
struct Args {
    /// Source files to scan.
    #[arg(required = true)]
    files: Vec<PathBuf>,

    /// Add directory to quoted include search path (-iquote).
    #[arg(long = "iquote", value_name = "DIR")]
    iquote_paths: Vec<PathBuf>,

    /// Add directory to include search path (-I).
    #[arg(short = 'I', value_name = "DIR")]
    include_paths: Vec<PathBuf>,

    /// Add directory to system include search path (-isystem).
    #[arg(long = "isystem", value_name = "DIR")]
    system_paths: Vec<PathBuf>,

    /// Define a macro (-D NAME or -D NAME=VALUE).
    #[arg(short = 'D', value_name = "MACRO")]
    defines: Vec<String>,

    /// Undefine a macro (-U NAME).
    #[arg(short = 'U', value_name = "MACRO")]
    undefines: Vec<String>,

    /// Override the target name in output.
    #[arg(short = 'o', long = "target", value_name = "TARGET")]
    target: Option<String>,

    /// Number of threads (0 = auto-detect).
    #[arg(short = 'j', long = "jobs", default_value = "0")]
    jobs: usize,

    // ---- -M family flags ----

    /// Print dependencies to stdout (default mode).
    #[arg(short = 'M', long, conflicts_with_all = ["mm", "md", "mmd"])]
    m: bool,

    /// Like -M but omit system headers.
    #[arg(long = "MM", conflicts_with_all = ["m", "md", "mmd"])]
    mm: bool,

    /// Write dependencies to a .d file.
    #[arg(long = "MD", conflicts_with_all = ["m", "mm", "mmd"])]
    md: bool,

    /// Like -MD but omit system headers.
    #[arg(long = "MMD", conflicts_with_all = ["m", "mm", "md"])]
    mmd: bool,

    /// Override the dependency output filename (used with -MD/-MMD).
    #[arg(long = "MF", value_name = "FILE")]
    mf: Option<PathBuf>,

    /// Override the Make target name (can be specified multiple times).
    #[arg(long = "MT", value_name = "TARGET")]
    mt: Vec<String>,

    /// Override the Make target name with quoting (can be specified multiple times).
    #[arg(long = "MQ", value_name = "TARGET")]
    mq: Vec<String>,

    /// Add phony targets for each dependency.
    #[arg(long = "MP")]
    mp: bool,

    /// Treat missing headers as generated (add to deps instead of erroring).
    #[arg(long = "MG")]
    mg: bool,

    /// Process trigraphs (off by default, matching modern gcc/clang).
    #[arg(long = "trigraphs")]
    trigraphs: bool,

    /// Query a compiler for its predefined macros and use them during scanning.
    /// Runs `COMPILER -dM -E - < /dev/null` to extract built-in defines like
    /// __GNUC__, __linux__, __x86_64__, etc. Defaults to "cc" if no compiler
    /// is specified.
    #[arg(long = "compiler-macros", value_name = "COMPILER", num_args = 0..=1, default_missing_value = "cc")]
    compiler_macros: Option<String>,
}

/// Expand `@file` response file arguments into the argument list.
fn expand_response_files(args: impl Iterator<Item = String>) -> Vec<String> {
    let mut result = Vec::new();
    for arg in args {
        if let Some(path) = arg.strip_prefix('@') {
            if let Ok(file) = std::fs::File::open(path) {
                let reader = std::io::BufReader::new(file);
                for line in reader.lines() {
                    if let Ok(line) = line {
                        let line = line.trim().to_string();
                        if !line.is_empty() {
                            // Split on whitespace (simple response file format)
                            for token in line.split_whitespace() {
                                result.push(token.to_string());
                            }
                        }
                    }
                }
            } else {
                // If file can't be opened, keep the argument as-is
                result.push(arg);
            }
        } else {
            result.push(arg);
        }
    }
    result
}

/// Query a compiler for its predefined macros by running `compiler -dM -E - < /dev/null`.
/// Parses lines of the form `#define NAME` or `#define NAME VALUE`.
fn query_compiler_defines(compiler: &str) -> Result<Vec<(String, Option<String>)>> {
    let output = Command::new(compiler)
        .args(["-dM", "-E", "-"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .with_context(|| format!("failed to run '{compiler} -dM -E -'"))?;

    if !output.status.success() {
        anyhow::bail!(
            "'{compiler} -dM -E -' exited with status {}",
            output.status
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut defines = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        // Parse "#define NAME" or "#define NAME VALUE"
        if let Some(rest) = line.strip_prefix("#define ") {
            if let Some((name, value)) = rest.split_once(' ') {
                // Skip function-like macros (e.g. "#define __has_builtin(x) ...")
                // since our macro table only supports simple -D style defines
                if name.contains('(') {
                    continue;
                }
                defines.push((name.to_string(), Some(value.to_string())));
            } else {
                defines.push((rest.to_string(), None));
            }
        }
    }

    Ok(defines)
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env(),
        )
        .init();

    // Expand @file response file arguments before clap parsing
    let expanded_args = expand_response_files(std::env::args());
    let args = Args::parse_from(expanded_args);

    // Configure thread pool
    if args.jobs > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(args.jobs)
            .build_global()
            .ok();
    }

    // Start with compiler-queried macros if requested, then layer user -D flags on top
    let mut defines: Vec<(String, Option<String>)> = if let Some(ref compiler) = args.compiler_macros {
        query_compiler_defines(compiler)?
    } else {
        Vec::new()
    };

    // Parse -D flags into (name, optional_value) pairs (override builtins)
    defines.extend(args.defines.iter().map(|d| {
        if let Some((name, value)) = d.split_once('=') {
            (name.to_string(), Some(value.to_string()))
        } else {
            (d.clone(), None)
        }
    }));

    // -U flags: remove any -D that conflicts with -U.
    for u in &args.undefines {
        defines.retain(|(name, _)| name != u);
    }

    // Determine dep mode
    let dep_mode = if args.mmd {
        DepMode::MMD
    } else if args.md {
        DepMode::MD
    } else if args.mm {
        DepMode::MM
    } else {
        DepMode::M
    };

    // Build output options from -MT/-MQ/-MP, falling back to legacy -o target
    let output_options = OutputOptions {
        targets: args.mt,
        quoted_targets: args.mq,
        phony: args.mp,
        ..Default::default()
    };

    let options = ScanOptions {
        search_paths: cppscan::resolve::SearchPaths {
            quote_paths: args.iquote_paths,
            include_paths: args.include_paths,
            system_paths: args.system_paths,
        },
        target: args.target,
        defines,
        dep_mode,
        dep_file: args.mf,
        output_options,
        missing_as_generated: args.mg,
        trigraphs: args.trigraphs,
    };

    let scanner = Scanner::new(options);
    let results = scanner.scan_files(&args.files);

    for result in results {
        match result {
            Ok(scan) => {
                if let Some(ref dep_file) = scan.dep_file_path {
                    // -MD/-MMD: write to file
                    std::fs::write(dep_file, &scan.makefile_output)?;
                } else {
                    // -M/-MM: print to stdout
                    print!("{}", scan.makefile_output);
                }
            }
            Err(e) => eprintln!("cppscan: error: {}", e),
        }
    }

    Ok(())
}
