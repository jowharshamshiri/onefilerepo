//! Core library for the `onefilerepo` command.

pub mod digest;
pub mod scan;
pub mod source;

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::digest::Digest;
use crate::scan::{ScanConfig, scan};
use crate::source::{PrepareOptions, PreparedSource, prepare};

/// Complete, validated ingestion request.
#[derive(Debug, Clone)]
pub struct IngestOptions {
    pub source: String,
    pub revision: Option<String>,
    pub subpath: Option<PathBuf>,
    pub include_patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub include_ignored: bool,
    pub include_submodules: bool,
    pub max_file_size: u64,
    pub max_total_size: u64,
    pub max_files: usize,
    pub max_depth: usize,
    pub jobs: usize,
    pub output_path: Option<PathBuf>,
}

/// Prepare, scan, and format one local or remote source.
///
/// Remote repositories are cloned into a temporary directory owned by the
/// returned source preparation. The directory is removed automatically once
/// this function returns.
pub fn ingest(options: &IngestOptions) -> Result<Digest> {
    validate_options(options)?;

    let prepared = prepare(
        &options.source,
        &PrepareOptions {
            revision: options.revision.clone(),
            subpath: options.subpath.clone(),
            include_submodules: options.include_submodules,
            jobs: options.jobs,
        },
    )?;

    ingest_prepared(options, &prepared)
}

fn ingest_prepared(options: &IngestOptions, prepared: &PreparedSource) -> Result<Digest> {
    let mut scanned = scan(&ScanConfig {
        root: prepared.scan_root.clone(),
        include_patterns: options.include_patterns.clone(),
        exclude_patterns: options.exclude_patterns.clone(),
        include_ignored: options.include_ignored,
        excluded_roots: prepared.excluded_submodules.clone(),
        max_file_size: options.max_file_size,
        max_total_size: options.max_total_size,
        max_files: options.max_files,
        max_depth: options.max_depth,
        jobs: options.jobs,
        output_path: options.output_path.clone(),
    })
    .with_context(|| format!("failed to scan {}", prepared.scan_root.display()))?;
    scanned.root_name.clone_from(&prepared.metadata.label);

    Digest::new(prepared.metadata.clone(), scanned)
}

fn validate_options(options: &IngestOptions) -> Result<()> {
    anyhow::ensure!(
        options.max_file_size > 0,
        "--max-file-size must be greater than zero"
    );
    anyhow::ensure!(
        options.max_total_size > 0,
        "--max-total-size must be greater than zero"
    );
    anyhow::ensure!(
        options.max_files > 0,
        "--max-files must be greater than zero"
    );
    anyhow::ensure!(
        options.max_depth > 0,
        "--max-depth must be greater than zero"
    );
    anyhow::ensure!(options.jobs > 0, "--jobs must be greater than zero");
    anyhow::ensure!(
        options.max_file_size <= options.max_total_size,
        "--max-file-size cannot exceed --max-total-size"
    );
    Ok(())
}
