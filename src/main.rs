mod delete;
mod exclude;
mod json_export;
mod json_import;
mod model;
#[cfg(unix)]
mod scan;
mod sink;

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

    /// Write JSON export to this file instead of stdout. Use `-` for stdout.
    #[arg(short = 'o', long, default_value = "-")]
    output: String,

    /// Exclude pattern (rsync/gitignore-like glob). Repeatable.
    #[arg(long = "exclude", value_name = "PATTERN")]
    exclude: Vec<String>,
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
    let mut excl = exclude::ExcludeSet::new();
    for p in &cli.exclude {
        excl.add(p);
    }
    let opts = scan::ScanOptions {
        extended: cli.extended,
        exclude: excl,
    };
    let tree = scan::scan(&cli.path, &opts)?;

    let export_opts = json_export::ExportOptions {
        extended: cli.extended,
        ..json_export::ExportOptions::default()
    };

    if cli.output == "-" {
        let stdout = io::stdout();
        let mut w = BufWriter::new(stdout.lock());
        json_export::export_tree(&tree, tree.root, &mut w, &export_opts)?;
        w.flush()?;
    } else {
        let f = std::fs::File::create(&cli.output)?;
        let mut w = BufWriter::new(f);
        json_export::export_tree(&tree, tree.root, &mut w, &export_opts)?;
        w.flush()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn run(_cli: &Cli) -> anyhow::Result<()> {
    anyhow::bail!("ncdu-rs scan currently supports only Unix-like systems");
}
