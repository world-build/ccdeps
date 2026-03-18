use anyhow::Result;
use clap::Parser;
use cppscan::{DepMode, OutputOptions, ScanOptions, Scanner};
use std::path::PathBuf;
use std::io::BufRead;

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

    /// Predefine platform/compiler macros matching the host gcc (__GNUC__,
    /// __linux__, __x86_64__, etc.). Enables correct conditional evaluation
    /// for code that relies on compiler built-in defines.
    #[arg(long = "gcc-builtins")]
    gcc_builtins: bool,
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

/// Return predefined macros that mimic GCC's built-in defines for the host platform.
fn gcc_builtin_defines() -> Vec<(String, Option<String>)> {
    let mut defs: Vec<(&str, Option<&str>)> = Vec::new();

    // Compiler identity
    defs.push(("__GNUC__", Some("15")));
    defs.push(("__GNUC_MINOR__", Some("0")));
    defs.push(("__GNUC_PATCHLEVEL__", Some("0")));

    // Language standard
    defs.push(("__STDC__", Some("1")));
    defs.push(("__STDC_VERSION__", Some("201710L")));

    // ELF
    defs.push(("__ELF__", Some("1")));

    // Character width
    defs.push(("__CHAR_BIT__", Some("8")));

    // Architecture
    #[cfg(target_arch = "x86_64")]
    {
        defs.push(("__x86_64__", Some("1")));
        defs.push(("__x86_64", Some("1")));
        defs.push(("__amd64__", Some("1")));
        defs.push(("__amd64", Some("1")));
        defs.push(("__LP64__", Some("1")));
        defs.push(("_LP64", Some("1")));
        defs.push(("__SIZEOF_LONG__", Some("8")));
        defs.push(("__SIZEOF_POINTER__", Some("8")));
        defs.push(("__SIZEOF_INT__", Some("4")));
        defs.push(("__SIZEOF_SHORT__", Some("2")));
        defs.push(("__SIZEOF_LONG_LONG__", Some("8")));
        defs.push(("__BYTE_ORDER__", Some("1234")));
        defs.push(("__ORDER_LITTLE_ENDIAN__", Some("1234")));
        defs.push(("__ORDER_BIG_ENDIAN__", Some("4321")));
    }

    #[cfg(target_arch = "aarch64")]
    {
        defs.push(("__aarch64__", Some("1")));
        defs.push(("__LP64__", Some("1")));
        defs.push(("_LP64", Some("1")));
        defs.push(("__SIZEOF_LONG__", Some("8")));
        defs.push(("__SIZEOF_POINTER__", Some("8")));
        defs.push(("__SIZEOF_INT__", Some("4")));
        defs.push(("__SIZEOF_SHORT__", Some("2")));
        defs.push(("__SIZEOF_LONG_LONG__", Some("8")));
        defs.push(("__BYTE_ORDER__", Some("1234")));
        defs.push(("__ORDER_LITTLE_ENDIAN__", Some("1234")));
        defs.push(("__ORDER_BIG_ENDIAN__", Some("4321")));
    }

    // OS
    #[cfg(target_os = "linux")]
    {
        defs.push(("__linux__", Some("1")));
        defs.push(("__linux", Some("1")));
        defs.push(("linux", Some("1")));
        defs.push(("__unix__", Some("1")));
        defs.push(("__unix", Some("1")));
        defs.push(("unix", Some("1")));
        defs.push(("__gnu_linux__", Some("1")));
    }

    #[cfg(target_os = "macos")]
    {
        defs.push(("__APPLE__", Some("1")));
        defs.push(("__MACH__", Some("1")));
        defs.push(("__unix__", Some("1")));
        defs.push(("__unix", Some("1")));
    }

    #[cfg(target_os = "freebsd")]
    {
        defs.push(("__FreeBSD__", Some("1")));
        defs.push(("__unix__", Some("1")));
        defs.push(("__unix", Some("1")));
    }

    defs.into_iter()
        .map(|(k, v)| (k.to_string(), v.map(|s| s.to_string())))
        .collect()
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

    // Start with gcc builtins if requested, then layer user -D flags on top
    let mut defines: Vec<(String, Option<String>)> = if args.gcc_builtins {
        gcc_builtin_defines()
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
