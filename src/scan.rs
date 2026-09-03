use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail, ensure};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{DirEntry, WalkBuilder, WalkState};
use rayon::prelude::*;
use serde_json::Value;

const DEFAULT_IGNORES: &[&str] = &[
    ".git",
    ".git/",
    ".gitignore",
    ".gitattributes",
    ".gitmodules",
    ".onefilerepoignore",
    ".svn/",
    ".hg/",
    ".DS_Store",
    "Thumbs.db",
    "desktop.ini",
    ".env",
    ".env.*",
    "!.env.example",
    "!.env.sample",
    "!.env.template",
    ".envrc",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "*.pem",
    "*.p12",
    "*.pfx",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "digest.txt",
    ".onefilerepo-*.tmp",
    "*.py[cod]",
    "__pycache__/",
    ".pytest_cache/",
    ".mypy_cache/",
    ".ruff_cache/",
    ".tox/",
    ".nox/",
    ".coverage",
    ".venv/",
    "venv/",
    "node_modules/",
    "bower_components/",
    ".npm/",
    ".yarn/",
    ".pnpm-store/",
    "target/",
    ".gradle/",
    ".build/",
    "build/",
    "dist/",
    "out/",
    "bin/",
    "obj/",
    "vendor/",
    "site-packages/",
    ".next/",
    ".nuxt/",
    ".docusaurus/",
    "*.class",
    "*.o",
    "*.obj",
    "*.a",
    "*.so",
    "*.dll",
    "*.dylib",
    "*.exe",
    "*.pdb",
    "*.jar",
    "*.war",
    "*.whl",
    "*.egg",
    "*.egg-info/",
    "*.gem",
    "*.nupkg",
    "*.bin",
    "*.db",
    "*.sqlite",
    "*.sqlite3",
    "*.log",
    "*.bak",
    "*.swp",
    "*.swo",
    "*.tmp",
    "*.temp",
    "*.min.js",
    "*.min.css",
    "*.map",
    "*.tfstate*",
    "*.png",
    "*.jpg",
    "*.jpeg",
    "*.gif",
    "*.ico",
    "*.pdf",
    "*.mov",
    "*.mp4",
    "*.mp3",
    "*.wav",
    "*.zip",
    "*.gz",
    "*.bz2",
    "*.xz",
    "*.7z",
    ".idea/",
    ".vscode/",
    ".vs/",
    ".cache/",
    ".sass-cache/",
    ".eslintcache",
    "poetry.lock",
    "Pipfile.lock",
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    "Gemfile.lock",
    "*.svg",
];

#[derive(Debug, Clone)]
pub struct ScanConfig {
    pub root: PathBuf,
    pub include_patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub include_ignored: bool,
    pub excluded_roots: Vec<PathBuf>,
    pub max_file_size: u64,
    pub max_total_size: u64,
    pub max_files: usize,
    pub max_depth: usize,
    pub jobs: usize,
    pub output_path: Option<PathBuf>,
}

#[derive(Debug)]
pub struct ScanResult {
    pub root_name: String,
    pub root_is_file: bool,
    pub files: Vec<ScannedFile>,
    pub stats: ScanStats,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ScanStats {
    pub discovered: usize,
    pub included_bytes: u64,
    pub skipped_too_large: usize,
    pub skipped_by_limits: usize,
    pub skipped_by_depth: usize,
}

#[derive(Debug)]
pub struct ScannedFile {
    pub relative_path: String,
    pub kind: FileKind,
    pub content: FileContent,
    pub source_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Symlink,
}

#[derive(Debug)]
pub enum FileContent {
    Text(String),
    Empty,
    Binary,
    Symlink(String),
}

#[derive(Debug)]
struct Candidate {
    absolute_path: PathBuf,
    relative_path: String,
    kind: FileKind,
    size: u64,
}

pub fn scan(config: &ScanConfig) -> Result<ScanResult> {
    let root_metadata = config.root.symlink_metadata().with_context(|| {
        format!(
            "scan root does not exist or cannot be inspected: {}",
            config.root.display()
        )
    })?;
    let root_name = match config.root.file_name() {
        Some(name) => name
            .to_str()
            .with_context(|| {
                format!(
                    "scan root name is not valid UTF-8: {}",
                    config.root.display()
                )
            })?
            .to_owned(),
        None => "root".to_owned(),
    };
    let root_is_file = root_metadata.is_file() || root_metadata.file_type().is_symlink();
    let matcher_root = if root_is_file {
        config
            .root
            .parent()
            .context("single-file scan root has no parent directory")?
    } else {
        &config.root
    };
    let output_path = normalized_output_path(config.output_path.as_deref())?;
    let defaults = build_ignore(matcher_root, DEFAULT_IGNORES)?;
    let excludes = build_ignore(matcher_root, &config.exclude_patterns)?;
    let includes = build_includes(&config.include_patterns)?;

    let (mut candidates, mut stats) = if root_is_file {
        collect_single_file(
            config,
            &defaults,
            &excludes,
            includes.as_ref(),
            output_path.as_deref(),
        )?
    } else if config.root.is_dir() {
        collect_directory(
            config,
            &defaults,
            &excludes,
            includes.as_ref(),
            output_path.as_deref(),
        )?
    } else {
        bail!(
            "scan root is neither a regular file nor a directory: {}",
            config.root.display()
        );
    };

    candidates.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));
    stats.discovered = candidates.len();
    let mut selected = Vec::with_capacity(candidates.len().min(config.max_files));
    let mut total_size = 0_u64;
    for candidate in candidates {
        if selected.len() >= config.max_files {
            stats.skipped_by_limits += 1;
            continue;
        }
        if total_size
            .checked_add(candidate.size)
            .is_none_or(|sum| sum > config.max_total_size)
        {
            stats.skipped_by_limits += 1;
            continue;
        }
        total_size += candidate.size;
        selected.push(candidate);
    }
    ensure!(
        !root_is_file || !selected.is_empty(),
        "the selected file was excluded by the requested filters or limits: {}",
        config.root.display()
    );

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(config.jobs)
        .thread_name(|index| format!("onefilerepo-reader-{index}"))
        .build()
        .context("failed to create the file-reading worker pool")?;
    let files = pool.install(|| {
        selected
            .into_par_iter()
            .map(read_candidate)
            .collect::<Result<Vec<_>>>()
    })?;
    stats.included_bytes = total_size;

    Ok(ScanResult {
        root_name,
        root_is_file,
        files,
        stats,
    })
}

fn collect_single_file(
    config: &ScanConfig,
    defaults: &Gitignore,
    excludes: &Gitignore,
    includes: Option<&GlobSet>,
    output_path: Option<&Path>,
) -> Result<(Vec<Candidate>, ScanStats)> {
    let mut stats = ScanStats::default();
    if output_path == Some(config.root.as_path()) {
        return Ok((Vec::new(), stats));
    }
    let name = config
        .root
        .file_name()
        .and_then(|value| value.to_str())
        .context("source filename is not valid UTF-8")?;
    if path_is_filtered(Path::new(name), false, defaults, excludes, includes) {
        return Ok((Vec::new(), stats));
    }
    let metadata = config.root.symlink_metadata()?;
    if metadata.len() > config.max_file_size {
        stats.skipped_too_large = 1;
        return Ok((Vec::new(), stats));
    }
    let kind = if metadata.file_type().is_symlink() {
        FileKind::Symlink
    } else {
        FileKind::File
    };
    Ok((
        vec![Candidate {
            absolute_path: config.root.clone(),
            relative_path: name.to_owned(),
            kind,
            size: metadata.len(),
        }],
        stats,
    ))
}

fn collect_directory(
    config: &ScanConfig,
    defaults: &Gitignore,
    excludes: &Gitignore,
    includes: Option<&GlobSet>,
    output_path: Option<&Path>,
) -> Result<(Vec<Candidate>, ScanStats)> {
    let candidates = Mutex::new(Vec::new());
    let errors = Mutex::new(Vec::new());
    let stats = Mutex::new(ScanStats::default());
    let excluded_roots = &config.excluded_roots;

    let mut builder = WalkBuilder::new(&config.root);
    builder
        .hidden(false)
        .follow_links(false)
        .threads(config.jobs)
        .max_depth(Some(config.max_depth.saturating_add(1)))
        .git_ignore(!config.include_ignored)
        .git_global(!config.include_ignored)
        .git_exclude(!config.include_ignored)
        .ignore(!config.include_ignored)
        .parents(!config.include_ignored)
        .require_git(false);
    if !config.include_ignored {
        builder.add_custom_ignore_filename(".onefilerepoignore");
    }

    builder.build_parallel().run(|| {
        Box::new(|entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    lock(&errors).push(error.to_string());
                    return WalkState::Continue;
                }
            };
            if entry.depth() == 0 {
                return WalkState::Continue;
            }
            let relative = match entry.path().strip_prefix(&config.root) {
                Ok(path) => path,
                Err(error) => {
                    lock(&errors).push(format!("walked path escaped the scan root: {error}"));
                    return WalkState::Continue;
                }
            };
            if relative.to_str().is_none() {
                lock(&errors).push(format!("path is not valid UTF-8: {}", relative.display()));
                return WalkState::Continue;
            }

            let is_directory = entry.file_type().is_some_and(|kind| kind.is_dir());
            if excluded_roots
                .iter()
                .any(|root| relative == root || relative.starts_with(root))
            {
                return if is_directory {
                    WalkState::Skip
                } else {
                    WalkState::Continue
                };
            }
            if entry.depth() > config.max_depth {
                lock(&stats).skipped_by_depth += 1;
                return if is_directory {
                    WalkState::Skip
                } else {
                    WalkState::Continue
                };
            }
            if path_is_filtered(relative, is_directory, defaults, excludes, includes) {
                return if is_directory {
                    WalkState::Skip
                } else {
                    WalkState::Continue
                };
            }
            if is_directory {
                return WalkState::Continue;
            }
            if output_path == Some(entry.path()) {
                return WalkState::Continue;
            }

            match candidate_from_entry(&entry, relative, config.max_file_size) {
                Ok(Some(candidate)) => lock(&candidates).push(candidate),
                Ok(None) => lock(&stats).skipped_too_large += 1,
                Err(error) => lock(&errors).push(format!("{}: {error:#}", entry.path().display())),
            }
            WalkState::Continue
        })
    });

    let mut errors = lock(&errors);
    if !errors.is_empty() {
        errors.sort_unstable();
        errors.dedup();
        bail!("filesystem traversal failed:\n{}", errors.join("\n"));
    }
    let candidates = std::mem::take(&mut *lock(&candidates));
    let stats = *lock(&stats);
    Ok((candidates, stats))
}

fn candidate_from_entry(
    entry: &DirEntry,
    relative: &Path,
    max_file_size: u64,
) -> Result<Option<Candidate>> {
    let file_type = entry
        .file_type()
        .context("directory entry has no file type")?;
    let kind = if file_type.is_file() {
        FileKind::File
    } else if file_type.is_symlink() {
        FileKind::Symlink
    } else {
        bail!("unsupported filesystem entry type");
    };
    let metadata = entry.metadata().context("failed to read file metadata")?;
    if metadata.len() > max_file_size {
        return Ok(None);
    }
    Ok(Some(Candidate {
        absolute_path: entry.path().to_path_buf(),
        relative_path: path_to_slash_string(relative)?,
        kind,
        size: metadata.len(),
    }))
}

fn path_is_filtered(
    relative: &Path,
    is_directory: bool,
    defaults: &Gitignore,
    excludes: &Gitignore,
    includes: Option<&GlobSet>,
) -> bool {
    if defaults
        .matched_path_or_any_parents(relative, is_directory)
        .is_ignore()
        || excludes
            .matched_path_or_any_parents(relative, is_directory)
            .is_ignore()
    {
        return true;
    }
    !is_directory && includes.is_some_and(|set| !set.is_match(relative))
}

fn build_ignore<T>(root: &Path, patterns: &[T]) -> Result<Gitignore>
where
    T: AsRef<str>,
{
    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        let pattern = pattern.as_ref();
        ensure!(
            !pattern.trim().is_empty(),
            "ignore patterns cannot be empty"
        );
        ensure!(
            !pattern.chars().any(char::is_control),
            "ignore patterns cannot contain control characters"
        );
        builder
            .add_line(None, pattern)
            .with_context(|| format!("invalid exclude pattern {pattern:?}"))?;
    }
    builder
        .build()
        .context("failed to compile exclude patterns")
}

fn build_includes(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        ensure!(
            !pattern.trim().is_empty(),
            "include patterns cannot be empty"
        );
        ensure!(
            !pattern.starts_with('!'),
            "include patterns cannot be negated; use --exclude for exclusions"
        );
        ensure!(
            !pattern.chars().any(char::is_control),
            "include patterns cannot contain control characters"
        );
        let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
        let normalized = if pattern.ends_with('/') {
            format!("{pattern}**")
        } else if pattern.contains('/') {
            pattern.to_owned()
        } else {
            format!("**/{pattern}")
        };
        let glob = GlobBuilder::new(&normalized)
            .literal_separator(true)
            .backslash_escape(true)
            .build()
            .with_context(|| format!("invalid include pattern {pattern:?}"))?;
        builder.add(glob);
    }
    Ok(Some(
        builder
            .build()
            .context("failed to compile include patterns")?,
    ))
}

fn read_candidate(candidate: Candidate) -> Result<ScannedFile> {
    let content = match candidate.kind {
        FileKind::Symlink => {
            let target = fs::read_link(&candidate.absolute_path).with_context(|| {
                format!(
                    "failed to read symlink {}",
                    candidate.absolute_path.display()
                )
            })?;
            let target = target.to_str().with_context(|| {
                format!("symlink target is not valid UTF-8: {}", target.display())
            })?;
            ensure!(
                !target.chars().any(char::is_control),
                "symlink target contains control characters: {}",
                candidate.absolute_path.display()
            );
            FileContent::Symlink(target.replace(std::path::MAIN_SEPARATOR, "/"))
        }
        FileKind::File => {
            let bytes = fs::read(&candidate.absolute_path)
                .with_context(|| format!("failed to read {}", candidate.absolute_path.display()))?;
            ensure!(
                u64::try_from(bytes.len()).ok() == Some(candidate.size),
                "file changed while it was being scanned: {}",
                candidate.absolute_path.display()
            );
            decode_content(&candidate.absolute_path, bytes)?
        }
    };
    Ok(ScannedFile {
        relative_path: candidate.relative_path,
        kind: candidate.kind,
        content,
        source_bytes: candidate.size,
    })
}

fn decode_content(path: &Path, bytes: Vec<u8>) -> Result<FileContent> {
    if bytes.is_empty() {
        return Ok(FileContent::Empty);
    }
    let text = if bytes.starts_with(&[0xFF, 0xFE]) {
        decode_utf16(&bytes[2..], true)?
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        decode_utf16(&bytes[2..], false)?
    } else if bytes.contains(&0) {
        return Ok(FileContent::Binary);
    } else {
        match String::from_utf8(bytes) {
            Ok(value) => value.strip_prefix('\u{feff}').unwrap_or(&value).to_owned(),
            Err(_) => return Ok(FileContent::Binary),
        }
    };

    if path.extension() == Some(std::ffi::OsStr::new("ipynb")) {
        return render_notebook(&text).map(FileContent::Text);
    }
    Ok(FileContent::Text(text))
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Result<String> {
    ensure!(
        bytes.len() % 2 == 0,
        "UTF-16 file has an incomplete final code unit"
    );
    let units = bytes
        .chunks_exact(2)
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect::<Vec<_>>();
    String::from_utf16(&units).context("UTF-16 file contains an invalid surrogate sequence")
}

fn render_notebook(text: &str) -> Result<String> {
    let notebook: Value = serde_json::from_str(text).context("invalid notebook JSON")?;
    let cells = notebook
        .get("cells")
        .and_then(Value::as_array)
        .context("notebook has no cells array")?;
    let mut rendered = String::new();
    for (index, cell) in cells.iter().enumerate() {
        let cell_type = cell
            .get("cell_type")
            .and_then(Value::as_str)
            .with_context(|| format!("notebook cell {index} has no cell_type"))?;
        ensure!(
            matches!(cell_type, "code" | "markdown" | "raw"),
            "notebook cell {index} has unsupported type {cell_type:?}"
        );
        let source = cell
            .get("source")
            .with_context(|| format!("notebook cell {index} has no source"))?;
        let body = match source {
            Value::String(value) => value.clone(),
            Value::Array(lines) => lines
                .iter()
                .enumerate()
                .map(|(line_index, line)| {
                    line.as_str().map(str::to_owned).with_context(|| {
                        format!("notebook cell {index} source line {line_index} is not a string")
                    })
                })
                .collect::<Result<String>>()?,
            _ => bail!("notebook cell {index} source is neither a string nor an array"),
        };
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        match cell_type {
            "markdown" => rendered.push_str("# %% [markdown]\n"),
            "raw" => rendered.push_str("# %% [raw]\n"),
            "code" => rendered.push_str("# %%\n"),
            _ => unreachable!(),
        }
        rendered.push_str(&body);
        if !body.ends_with('\n') {
            rendered.push('\n');
        }
    }
    Ok(rendered)
}

fn normalized_output_path(path: Option<&Path>) -> Result<Option<PathBuf>> {
    let Some(path) = path else {
        return Ok(None);
    };
    path_to_slash_string(path).context("output path is not safe for terminal reporting")?;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    match absolute.symlink_metadata() {
        Ok(metadata) => ensure!(
            !metadata.is_dir(),
            "output path is a directory: {}",
            absolute.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect output path {}", absolute.display()));
        }
    }
    let parent = absolute.parent().context("output path has no parent")?;
    let filename = absolute
        .file_name()
        .context("output path has no filename")?;
    match fs::canonicalize(parent) {
        Ok(parent) => Ok(Some(parent.join(filename))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("failed to resolve output directory {}", parent.display())),
    }
}

fn path_to_slash_string(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))?;
    ensure!(
        !value.chars().any(char::is_control),
        "path contains control characters: {}",
        path.display()
    );
    Ok(value.replace(std::path::MAIN_SEPARATOR, "/"))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().expect("scanner state mutex was poisoned")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn config(root: &Path) -> ScanConfig {
        ScanConfig {
            root: root.to_path_buf(),
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            include_ignored: false,
            excluded_roots: Vec::new(),
            max_file_size: 1024,
            max_total_size: 4096,
            max_files: 100,
            max_depth: 16,
            jobs: 2,
            output_path: None,
        }
    }

    #[test]
    fn applies_nested_ignore_rules_and_explicit_filters() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::write(directory.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(directory.path().join("ignored.txt"), "secret").unwrap();
        fs::write(directory.path().join("src/lib.rs"), "pub fn live() {}\n").unwrap();
        fs::write(directory.path().join("src/debug.log"), "noise").unwrap();

        let mut options = config(directory.path());
        options.include_patterns.push("*.rs".to_owned());
        let result = scan(&options).unwrap();

        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].relative_path, "src/lib.rs");
    }

    #[test]
    fn excludes_secret_files_but_keeps_environment_templates() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join(".env"), "TOKEN=secret\n").unwrap();
        fs::write(directory.path().join(".env.production"), "TOKEN=secret\n").unwrap();
        fs::write(directory.path().join(".env.example"), "TOKEN=replace-me\n").unwrap();
        fs::write(directory.path().join("server.pem"), "private material\n").unwrap();

        let result = scan(&config(directory.path())).unwrap();
        let paths = result
            .files
            .iter()
            .map(|file| file.relative_path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(paths, [".env.example"]);
    }

    #[test]
    fn include_globs_distinguish_basename_and_path_patterns() {
        let basenames = build_includes(&["*.rs".to_owned()]).unwrap().unwrap();
        assert!(basenames.is_match("lib.rs"));
        assert!(basenames.is_match("src/lib.rs"));

        let paths = build_includes(&["src/*.rs".to_owned()]).unwrap().unwrap();
        assert!(paths.is_match("src/lib.rs"));
        assert!(!paths.is_match("src/nested/lib.rs"));
        assert!(!paths.is_match("other/lib.rs"));
    }

    #[test]
    fn structural_paths_with_control_characters_are_rejected() {
        assert!(path_to_slash_string(Path::new("bad\nname.rs")).is_err());
        assert!(build_includes(&["bad\t*.rs".to_owned()]).is_err());
        assert!(build_ignore(Path::new("."), &["bad\rname"]).is_err());
    }

    #[test]
    fn limits_are_deterministic_after_path_sorting() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("c.txt"), "cc").unwrap();
        fs::write(directory.path().join("a.txt"), "aa").unwrap();
        fs::write(directory.path().join("b.txt"), "bb").unwrap();

        let mut options = config(directory.path());
        options.max_files = 2;
        let result = scan(&options).unwrap();

        assert_eq!(
            result
                .files
                .iter()
                .map(|file| file.relative_path.as_str())
                .collect::<Vec<_>>(),
            ["a.txt", "b.txt"]
        );
        assert_eq!(result.stats.skipped_by_limits, 1);
    }

    #[test]
    fn empty_directories_are_valid_scan_results() {
        let directory = tempfile::tempdir().unwrap();
        let result = scan(&config(directory.path())).unwrap();
        assert!(result.files.is_empty());
        assert_eq!(result.stats.discovered, 0);
        assert_eq!(result.stats.included_bytes, 0);
    }

    #[test]
    fn decodes_utf16_and_identifies_binary_data() {
        let directory = tempfile::tempdir().unwrap();
        let utf16_path = directory.path().join("utf16.txt");
        let mut utf16 = fs::File::create(&utf16_path).unwrap();
        utf16.write_all(&[0xFF, 0xFE, b'h', 0, b'i', 0]).unwrap();
        let binary_path = directory.path().join("asset.dat");
        fs::write(&binary_path, [0, 1, 2, 3]).unwrap();

        assert!(
            matches!(decode_content(&utf16_path, fs::read(&utf16_path).unwrap()).unwrap(), FileContent::Text(value) if value == "hi")
        );
        assert!(matches!(
            decode_content(&binary_path, fs::read(&binary_path).unwrap()).unwrap(),
            FileContent::Binary
        ));
    }

    #[test]
    fn strips_notebook_outputs_and_preserves_cell_sources() {
        let notebook = r#"{"cells":[{"cell_type":"code","source":["print(1)\n"],"outputs":[{"text":"1"}]},{"cell_type":"markdown","source":"hello"}]}"#;
        let rendered = render_notebook(notebook).unwrap();
        assert_eq!(rendered, "# %%\nprint(1)\n\n# %% [markdown]\nhello\n");
        assert!(!rendered.contains("outputs"));
    }
}
