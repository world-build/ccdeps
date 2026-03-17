use anyhow::Result;
use clap::Parser;
use cppscan::{ScanOptions, Scanner};
use std::path::PathBuf;

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
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env(),
        )
        .init();

    let args = Args::parse();

    // Configure thread pool
    if args.jobs > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(args.jobs)
            .build_global()
            .ok();
    }

    // Parse -D flags into (name, optional_value) pairs
    let mut defines: Vec<(String, Option<String>)> = args
        .defines
        .iter()
        .map(|d| {
            if let Some((name, value)) = d.split_once('=') {
                (name.to_string(), Some(value.to_string()))
            } else {
                (d.clone(), None)
            }
        })
        .collect();

    // -U flags: define then immediately undefine isn't quite right;
    // we represent them as defines that will be undef'd. For now,
    // just remove any -D that conflicts with -U.
    for u in &args.undefines {
        defines.retain(|(name, _)| name != u);
    }

    let options = ScanOptions {
        search_paths: cppscan::resolve::SearchPaths {
            quote_paths: args.iquote_paths,
            include_paths: args.include_paths,
            system_paths: args.system_paths,
        },
        target: args.target,
        defines,
    };

    let scanner = Scanner::new(options);
    let results = scanner.scan_files(&args.files);

    for result in results {
        match result {
            Ok(scan) => print!("{}", scan.makefile_output),
            Err(e) => eprintln!("cppscan: error: {}", e),
        }
    }

    Ok(())
}
