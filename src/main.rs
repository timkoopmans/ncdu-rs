mod browser;
mod delete;
mod exclude;
mod json_export;
mod json_import;
mod model;
#[cfg(unix)]
mod scan;
#[cfg(unix)]
mod scan_parallel;
mod sink;
#[cfg(target_os = "linux")]
mod xfs_quota;

use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "ncdu-rs", version, about = "Rust port of ncdu v2 (work in progress)")]
struct Cli {
    /// Directory to scan.
    path: PathBuf,

    /// Capture mtime/uid/gid/mode into each entry.
    #[arg(short = 'e', long)]
    extended: bool,

    /// Write JSON export to this file. When unset, launches the interactive
    /// browser instead. Use `-` for stdout.
    #[arg(short = 'o', long)]
    output: Option<String>,

    /// Exclude pattern (rsync/gitignore-like glob). Repeatable.
    #[arg(long = "exclude", value_name = "PATTERN")]
    exclude: Vec<String>,

    /// Number of scanner threads. 1 = sequential walker. >1 = rayon parallel.
    #[arg(short = 't', long, default_value_t = 1)]
    threads: usize,

    /// Try the XFS project-quota fast-path before walking. Linux + xfsprogs
    /// only. Prints byte total and exits if successful; falls back to scan
    /// otherwise.
    #[arg(long)]
    xfs_quota: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ncdu-rs: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(unix)]
fn run(cli: &Cli) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    if cli.xfs_quota {
        match xfs_quota::try_quota_total(&cli.path)? {
            Some(bytes) => {
                println!("xfs-quota total: {bytes} bytes");
                return Ok(());
            }
            None => {
                eprintln!("xfs-quota fast-path unavailable, falling back to scan");
            }
        }
    }

    let mut excl = exclude::ExcludeSet::new();
    for p in &cli.exclude {
        excl.add(p);
    }
    let opts = scan::ScanOptions {
        extended: cli.extended,
        exclude: excl,
    };
    let tree = if cli.threads > 1 {
        scan_parallel::scan_parallel(&cli.path, &opts, cli.threads)?
    } else {
        scan::scan(&cli.path, &opts)?
    };

    match cli.output.as_deref() {
        None => {
            // Interactive browser.
            let abs = std::fs::canonicalize(&cli.path)?;
            browser::Browser::new(tree, abs).run()?;
        }
        Some("-") => {
            let stdout = io::stdout();
            let mut w = BufWriter::new(stdout.lock());
            export(&tree, &mut w, cli.extended)?;
            w.flush()?;
        }
        Some(path) => {
            let f = std::fs::File::create(path)?;
            let mut w = BufWriter::new(f);
            export(&tree, &mut w, cli.extended)?;
            w.flush()?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn export<W: Write>(tree: &model::Tree, w: &mut W, extended: bool) -> anyhow::Result<()> {
    let opts = json_export::ExportOptions {
        extended,
        ..json_export::ExportOptions::default()
    };
    json_export::export_tree(tree, tree.root, w, &opts)?;
    Ok(())
}

#[cfg(not(unix))]
fn run(_cli: &Cli) -> anyhow::Result<()> {
    anyhow::bail!("ncdu-rs scan currently supports only Unix-like systems");
}
