use std::fs;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{ArgAction, Parser};
use tempfile::Builder;

use onefilerepo::{IngestOptions, ingest};

const DEFAULT_MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;
const DEFAULT_MAX_TOTAL_SIZE: u64 = 500 * 1024 * 1024;
const DEFAULT_MAX_FILES: usize = 100_000;
const DEFAULT_MAX_DEPTH: usize = 64;

#[derive(Debug, Parser)]
#[command(
    name = "onefilerepo",
    version,
    about = "Turn a local or remote Git repository into one LLM-ready text file",
    long_about = "Turn a local directory or Git repository into a deterministic, prompt-friendly text digest. GitHub repositories are cloned with GitHub CLI, so its authenticated access works for private repositories. Other remotes use git directly."
)]
struct Cli {
    /// Local path, owner/repo slug, or Git URL.
    #[arg(default_value = ".")]
    source: String,

    /// Branch, tag, or commit to ingest (remote sources only).
    #[arg(short = 'r', long = "ref", value_name = "REVISION")]
    revision: Option<String>,

    /// Repository-relative file or directory to ingest.
    #[arg(short = 'p', long, value_name = "PATH")]
    path: Option<PathBuf>,

    /// Git-wildmatch-style pattern to include; repeat for multiple patterns.
    #[arg(
        short = 'i',
        long = "include",
        alias = "include-pattern",
        value_name = "GLOB"
    )]
    include_patterns: Vec<String>,

    /// Git-wildmatch-style pattern to exclude; repeat for multiple patterns.
    #[arg(
        short = 'e',
        long = "exclude",
        alias = "exclude-pattern",
        value_name = "GLOB"
    )]
    exclude_patterns: Vec<String>,

    /// Include paths ignored by .gitignore, global Git ignores, and .onefilerepoignore.
    #[arg(long)]
    include_ignored: bool,

    /// Do not initialize or include Git submodules.
    #[arg(long = "no-submodules", action = ArgAction::SetFalse, default_value_t = true)]
    include_submodules: bool,

    /// Maximum size of any included file in bytes.
    #[arg(short = 's', long, default_value_t = DEFAULT_MAX_FILE_SIZE, value_parser = parse_byte_size)]
    max_file_size: u64,

    /// Maximum combined size of included files in bytes.
    #[arg(long, default_value_t = DEFAULT_MAX_TOTAL_SIZE, value_parser = parse_byte_size)]
    max_total_size: u64,

    /// Maximum number of included files.
    #[arg(long, default_value_t = DEFAULT_MAX_FILES)]
    max_files: usize,

    /// Maximum traversal depth below the selected root.
    #[arg(long, default_value_t = DEFAULT_MAX_DEPTH)]
    max_depth: usize,

    /// Worker threads used for traversal, reads, and submodule checkout.
    #[arg(short = 'j', long, default_value_t = default_jobs())]
    jobs: usize,

    /// Destination file, or '-' for stdout.
    #[arg(short = 'o', long, default_value = "digest.txt", value_name = "FILE")]
    output: String,

    /// Suppress the completion summary on stderr.
    #[arg(short = 'q', long)]
    quiet: bool,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let output_path = (cli.output != "-").then(|| PathBuf::from(&cli.output));

    if cli.output == "-" && io::stdout().is_terminal() && !cli.quiet {
        eprintln!("Analyzing source; digest will be written to stdout...");
    }

    let digest = ingest(&IngestOptions {
        source: cli.source,
        revision: cli.revision,
        subpath: cli.path,
        include_patterns: cli.include_patterns,
        exclude_patterns: cli.exclude_patterns,
        include_ignored: cli.include_ignored,
        include_submodules: cli.include_submodules,
        max_file_size: cli.max_file_size,
        max_total_size: cli.max_total_size,
        max_files: cli.max_files,
        max_depth: cli.max_depth,
        jobs: cli.jobs,
        output_path: output_path.clone(),
    })?;

    if let Some(path) = output_path.as_deref() {
        write_atomic(path, |writer| digest.write_to(writer))?;
        if !cli.quiet {
            println!("Analysis complete. Output written to: {}", path.display());
            println!("\n{}", digest.summary());
        }
    } else {
        let stdout = io::stdout();
        let mut writer = BufWriter::new(stdout.lock());
        digest.write_to(&mut writer)?;
        writer.flush().context("failed to flush stdout")?;
        if !cli.quiet {
            eprintln!("\nAnalysis complete.\n{}", digest.summary());
        }
    }

    Ok(())
}

fn write_atomic(path: &Path, write: impl FnOnce(&mut dyn Write) -> Result<()>) -> Result<()> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create output directory {}", parent.display()))?;

    let mut temporary = Builder::new()
        .prefix(".onefilerepo-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| {
            format!(
                "failed to create a temporary output in {}",
                parent.display()
            )
        })?;

    {
        let mut writer = BufWriter::new(temporary.as_file_mut());
        write(&mut writer)?;
        writer.flush().context("failed to flush temporary output")?;
    }
    temporary
        .as_file()
        .sync_all()
        .context("failed to sync temporary output")?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to atomically replace {}", path.display()))?;
    Ok(())
}

fn default_jobs() -> usize {
    std::thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

fn parse_byte_size(raw: &str) -> Result<u64, String> {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err("size cannot be empty".to_owned());
    }

    let split_at = normalized
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(normalized.len());
    let (digits, suffix) = normalized.split_at(split_at);
    if digits.is_empty() {
        return Err(format!("invalid byte size: {raw}"));
    }
    let value = digits
        .parse::<u64>()
        .map_err(|_| format!("invalid byte size: {raw}"))?;
    let multiplier = match suffix.trim() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        _ => {
            return Err(format!(
                "unknown size suffix in {raw:?}; use B, KiB, MiB, or GiB"
            ));
        }
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("byte size is too large: {raw}"))
}
