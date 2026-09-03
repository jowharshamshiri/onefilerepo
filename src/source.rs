use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use percent_encoding::percent_decode_str;
use tempfile::TempDir;
use url::Url;

#[derive(Debug, Clone)]
pub struct PrepareOptions {
    pub revision: Option<String>,
    pub subpath: Option<PathBuf>,
    pub include_submodules: bool,
    pub jobs: usize,
}

#[derive(Debug, Clone)]
pub struct SourceMetadata {
    pub label: String,
    pub repository: Option<String>,
    pub revision: Option<String>,
    pub commit: Option<String>,
    pub subpath: Option<String>,
    pub submodule_count: usize,
    pub working_tree_dirty: Option<bool>,
}

#[derive(Debug)]
pub struct PreparedSource {
    pub scan_root: PathBuf,
    pub metadata: SourceMetadata,
    pub excluded_submodules: Vec<PathBuf>,
    _temporary: Option<TempDir>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionKind {
    Tree,
    Blob,
}

#[derive(Debug)]
struct RemoteSpec {
    clone_target: String,
    github_slug: Option<String>,
    repository_label: String,
    selection_kind: Option<SelectionKind>,
    url_tail: Vec<String>,
}

pub fn prepare(source: &str, options: &PrepareOptions) -> Result<PreparedSource> {
    ensure!(!source.is_empty(), "source cannot be empty");

    let local = Path::new(source);
    match local.symlink_metadata() {
        Ok(_) => return prepare_local(local, options),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect local source {}", local.display()));
        }
    }

    prepare_remote(parse_remote(source)?, options)
}

fn prepare_local(path: &Path, options: &PrepareOptions) -> Result<PreparedSource> {
    ensure!(
        options.revision.is_none(),
        "--ref is only valid for remote sources; local sources always represent their current working tree"
    );

    let source_root = resolve_local_source(path)?;
    if options.subpath.is_some() {
        ensure!(
            source_root.symlink_metadata()?.file_type().is_dir(),
            "--path requires the local source to be a directory"
        );
    }
    let scan_root = resolve_subpath(&source_root, options.subpath.as_deref())?;
    let repository_root = discover_repository_root(&source_root)?;

    let mut excluded_submodules = Vec::new();
    let mut submodule_count = 0;
    let mut revision = None;
    let mut commit = None;
    let mut repository = None;
    let mut working_tree_dirty = None;

    if let Some(repo_root) = repository_root.as_deref() {
        let statuses = submodule_statuses(repo_root)?;
        let relevant = statuses
            .iter()
            .filter(|status| paths_overlap(&repo_root.join(&status.path), &scan_root))
            .collect::<Vec<_>>();

        if options.include_submodules {
            for status in &relevant {
                match status.state {
                    '-' => bail!(
                        "submodule {} is not initialized; run `git submodule update --init --recursive` or use --no-submodules",
                        status.path.display()
                    ),
                    'U' => bail!(
                        "submodule {} has unresolved merge conflicts",
                        status.path.display()
                    ),
                    ' ' | '+' => {}
                    state => bail!(
                        "submodule {} reported unsupported state {state:?}",
                        status.path.display()
                    ),
                }
            }
            submodule_count = relevant.len();
        } else {
            for status in relevant {
                let absolute = repo_root.join(&status.path);
                if scan_root.starts_with(&absolute) {
                    bail!(
                        "selected path is inside submodule {}; remove --no-submodules or select a path outside it",
                        status.path.display()
                    );
                }
                let relative = absolute.strip_prefix(&scan_root).with_context(|| {
                    format!(
                        "submodule path {} is not contained by the scan root",
                        status.path.display()
                    )
                })?;
                excluded_submodules.push(relative.to_path_buf());
            }
        }

        commit = Some(git_capture(repo_root, ["rev-parse", "HEAD"])?);
        revision = git_capture_optional(
            repo_root,
            ["symbolic-ref", "--quiet", "--short", "HEAD"],
            &[1],
        )?;
        repository = match git_capture_optional(repo_root, ["remote", "get-url", "origin"], &[2])? {
            Some(url) => repository_name_from_remote(&url)?,
            None => None,
        };
        working_tree_dirty = Some(
            !git_capture(
                repo_root,
                ["status", "--porcelain", "--untracked-files=normal"],
            )?
            .is_empty(),
        );
    }

    let label = path_label(&scan_root)?;
    let selected_subpath = relative_display(&source_root, &scan_root)?;

    Ok(PreparedSource {
        scan_root,
        metadata: SourceMetadata {
            label,
            repository,
            revision,
            commit,
            subpath: selected_subpath,
            submodule_count,
            working_tree_dirty,
        },
        excluded_submodules,
        _temporary: None,
    })
}

fn resolve_local_source(path: &Path) -> Result<PathBuf> {
    let metadata = path
        .symlink_metadata()
        .with_context(|| format!("failed to inspect local source {}", path.display()))?;
    if !metadata.file_type().is_symlink() {
        return fs::canonicalize(path)
            .with_context(|| format!("failed to resolve local source {}", path.display()));
    }

    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let resolved_parent = fs::canonicalize(parent).with_context(|| {
        format!(
            "failed to resolve the parent of local source {}",
            path.display()
        )
    })?;
    let filename = path
        .file_name()
        .context("local symlink source has no filename")?;
    Ok(resolved_parent.join(filename))
}

fn prepare_remote(spec: RemoteSpec, options: &PrepareOptions) -> Result<PreparedSource> {
    if options.revision.is_some() && !spec.url_tail.is_empty() {
        bail!(
            "a URL containing /tree/ or /blob/ cannot be combined with --ref; use the repository root URL with --ref and --path"
        );
    }
    if options.subpath.is_some() && !spec.url_tail.is_empty() {
        bail!(
            "a URL containing /tree/ or /blob/ cannot be combined with --path; use the repository root URL with --ref and --path"
        );
    }
    validate_revision(options.revision.as_deref())?;

    let temporary = tempfile::Builder::new()
        .prefix("onefilerepo-")
        .tempdir()
        .context("failed to create a temporary clone directory")?;
    let clone_root = temporary.path().join("repository");
    clone_remote(&spec, &clone_root, options.revision.as_deref())?;
    let github_auth = spec.github_slug.is_some();

    let (revision, url_subpath) = if spec.url_tail.is_empty() {
        let requested = options.revision.as_deref();
        let resolved = resolve_revision(&clone_root, requested, github_auth)?;
        (resolved, None)
    } else {
        resolve_url_tail(&clone_root, &spec.url_tail, github_auth)?
    };

    let requested_subpath = options.subpath.clone().or(url_subpath);
    if let Some(path) = requested_subpath.as_deref() {
        validate_relative_path(path)?;
    }
    let selected_subpath = requested_subpath.filter(|path| !selects_root(path));
    if let Some(path) = selected_subpath.as_deref() {
        let enters_submodule =
            configure_sparse_checkout(&clone_root, &revision.checkout, path, github_auth)?;
        ensure!(
            options.include_submodules || !enters_submodule,
            "selected path is a Git submodule; remove --no-submodules or select a path outside it"
        );
    }
    git_status_with_auth(
        &clone_root,
        [
            OsStr::new("checkout"),
            OsStr::new("--detach"),
            OsStr::new("--force"),
            OsStr::new(&revision.checkout),
            OsStr::new("--"),
        ],
        "check out the requested revision",
        github_auth,
    )?;

    let mut submodule_count = 0;
    if options.include_submodules {
        git_status(
            &clone_root,
            [
                OsStr::new("submodule"),
                OsStr::new("sync"),
                OsStr::new("--recursive"),
            ],
            "synchronize submodule URLs",
        )?;
        let jobs = options.jobs.to_string();
        git_status_with_auth(
            &clone_root,
            [
                OsStr::new("submodule"),
                OsStr::new("update"),
                OsStr::new("--init"),
                OsStr::new("--recursive"),
                OsStr::new("--depth=1"),
                OsStr::new("--filter=blob:none"),
                OsStr::new("--jobs"),
                OsStr::new(&jobs),
            ],
            "initialize submodules",
            github_auth,
        )?;
        submodule_count = submodule_statuses(&clone_root)?.len();
    }

    let scan_root = resolve_subpath(&clone_root, selected_subpath.as_deref())?;
    validate_selection_kind(&scan_root, spec.selection_kind)?;

    let commit = git_capture(&clone_root, ["rev-parse", "HEAD"])?;
    let label = match selected_subpath.as_deref() {
        Some(_) => path_label(&scan_root)?,
        None => repository_basename(&spec.repository_label)?,
    };

    Ok(PreparedSource {
        scan_root,
        metadata: SourceMetadata {
            label,
            repository: Some(spec.repository_label),
            revision: revision.display,
            commit: Some(commit),
            subpath: selected_subpath
                .as_deref()
                .map(path_to_slash_string)
                .transpose()?,
            submodule_count,
            working_tree_dirty: None,
        },
        excluded_submodules: Vec::new(),
        _temporary: Some(temporary),
    })
}

fn parse_remote(source: &str) -> Result<RemoteSpec> {
    if let Some(rest) = source.strip_prefix("git@github.com:") {
        let slug = normalize_slug(rest)?;
        return Ok(RemoteSpec {
            clone_target: slug.clone(),
            github_slug: Some(slug.clone()),
            repository_label: slug,
            selection_kind: None,
            url_tail: Vec::new(),
        });
    }

    if !source.contains("://") && !source.contains('@') {
        let candidate = source.strip_prefix("github.com/").unwrap_or(source);
        if candidate.split('/').count() == 2 {
            let slug = normalize_slug(candidate)?;
            return Ok(RemoteSpec {
                clone_target: slug.clone(),
                github_slug: Some(slug.clone()),
                repository_label: slug,
                selection_kind: None,
                url_tail: Vec::new(),
            });
        }
    }

    if let Some(remote_path) = scp_remote_path(source)? {
        let label = repository_name_from_path(remote_path)
            .context("SCP-style Git remote has no repository name")?;
        validate_repository_label(&label)?;
        return Ok(RemoteSpec {
            clone_target: source.to_owned(),
            github_slug: None,
            repository_label: label,
            selection_kind: None,
            url_tail: Vec::new(),
        });
    }

    let url = Url::parse(source).with_context(|| {
        format!("source does not exist locally and is not a valid Git URL: {source}")
    })?;
    ensure!(
        matches!(url.scheme(), "https" | "http" | "ssh" | "git"),
        "unsupported Git URL scheme {:?}",
        url.scheme()
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "Git URLs cannot contain a query or fragment"
    );
    ensure!(
        url.password().is_none(),
        "Git URLs must not contain a password; use a credential helper or SSH agent instead"
    );
    if matches!(url.scheme(), "http" | "https") {
        ensure!(
            url.username().is_empty(),
            "HTTP Git URLs must not contain credentials; authenticate GitHub CLI or a Git credential helper instead"
        );
    }

    let host = url.host_str().context("Git URL has no host")?;
    let segments = url
        .path_segments()
        .context("Git URL has no path")?
        .filter(|segment| !segment.is_empty())
        .map(decode_url_segment)
        .collect::<Result<Vec<_>>>()?;
    ensure!(!segments.is_empty(), "Git URL has no repository path");

    if host.eq_ignore_ascii_case("github.com") && url.port().is_none() {
        ensure!(
            segments.len() >= 2,
            "GitHub URL must contain an owner and repository"
        );
        let slug = normalize_slug(&format!("{}/{}", segments[0], segments[1]))?;
        let (selection_kind, url_tail) = match segments.get(2).map(String::as_str) {
            None => (None, Vec::new()),
            Some("tree") => (Some(SelectionKind::Tree), segments[3..].to_vec()),
            Some("blob") => (Some(SelectionKind::Blob), segments[3..].to_vec()),
            Some(kind) => bail!(
                "unsupported GitHub URL path component {kind:?}; use a repository, /tree/, or /blob/ URL"
            ),
        };
        if selection_kind.is_some() {
            ensure!(
                !url_tail.is_empty(),
                "GitHub /tree/ and /blob/ URLs must include a revision"
            );
        }
        return Ok(RemoteSpec {
            clone_target: slug.clone(),
            github_slug: Some(slug.clone()),
            repository_label: slug,
            selection_kind,
            url_tail,
        });
    }

    let label =
        repository_name_from_path(&segments.join("/")).context("Git URL has no repository name")?;
    validate_repository_label(&label)?;
    Ok(RemoteSpec {
        clone_target: source.to_owned(),
        github_slug: None,
        repository_label: label,
        selection_kind: None,
        url_tail: Vec::new(),
    })
}

fn clone_remote(
    spec: &RemoteSpec,
    destination: &Path,
    requested_revision: Option<&str>,
) -> Result<()> {
    let git_arguments = clone_arguments(spec, requested_revision);

    let status = if let Some(slug) = spec.github_slug.as_deref() {
        let mut command = Command::new("gh");
        command
            .arg("repo")
            .arg("clone")
            .arg(slug)
            .arg(destination)
            .arg("--")
            .args(&git_arguments);
        command_status(&mut command, "clone the GitHub repository with `gh`")?
    } else {
        let mut command = Command::new("git");
        command
            .arg("clone")
            .args(&git_arguments)
            .arg(&spec.clone_target)
            .arg(destination);
        command_status(&mut command, "clone the repository with `git`")?
    };
    ensure!(
        status.success(),
        "repository clone failed with status {status}"
    );
    Ok(())
}

fn clone_arguments(spec: &RemoteSpec, requested_revision: Option<&str>) -> Vec<OsString> {
    let mut arguments = vec![
        OsString::from("--filter=blob:none"),
        OsString::from("--no-checkout"),
        OsString::from("--depth=1"),
    ];
    if spec.url_tail.is_empty() {
        arguments.push(OsString::from("--single-branch"));
        if let Some(revision) = requested_revision.filter(|value| !looks_like_commit(value)) {
            arguments.push(OsString::from("--branch"));
            arguments.push(OsString::from(revision));
        }
    } else {
        arguments.push(OsString::from("--no-single-branch"));
        arguments.push(OsString::from("--tags"));
    }
    arguments
}

#[derive(Debug)]
struct ResolvedRevision {
    checkout: String,
    display: Option<String>,
}

fn resolve_revision(
    repository: &Path,
    requested: Option<&str>,
    github_auth: bool,
) -> Result<ResolvedRevision> {
    if let Some(requested) = requested {
        let candidates = vec![
            format!("refs/heads/{requested}"),
            format!("refs/remotes/origin/{requested}"),
            format!("refs/tags/{requested}"),
            requested.to_owned(),
        ];
        for candidate in candidates {
            if let Some(commit) = try_resolve_commit(repository, &candidate)? {
                return Ok(ResolvedRevision {
                    checkout: commit,
                    display: Some(requested.to_owned()),
                });
            }
        }
        if looks_like_commit(requested) {
            git_status_with_auth(
                repository,
                [
                    OsStr::new("fetch"),
                    OsStr::new("--depth=1"),
                    OsStr::new("--filter=blob:none"),
                    OsStr::new("origin"),
                    OsStr::new(requested),
                ],
                "fetch the requested commit",
                github_auth,
            )?;
            let commit = try_resolve_commit(repository, "FETCH_HEAD")?
                .context("the requested commit was not provided by the remote")?;
            return Ok(ResolvedRevision {
                checkout: commit,
                display: Some(requested.to_owned()),
            });
        }
        bail!("revision {requested:?} was not found in the cloned repository");
    }

    let commit = try_resolve_commit(repository, "HEAD")?
        .context("the remote repository has no resolvable default branch")?;
    let display = git_capture_optional(
        repository,
        [
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
        &[1],
    )?
    .and_then(|value| value.strip_prefix("origin/").map(str::to_owned));
    Ok(ResolvedRevision {
        checkout: commit,
        display,
    })
}

fn resolve_url_tail(
    repository: &Path,
    tail: &[String],
    github_auth: bool,
) -> Result<(ResolvedRevision, Option<PathBuf>)> {
    let refs = git_capture(repository, ["for-each-ref", "--format=%(refname)"])?;
    let mut names = BTreeSet::new();
    for full in refs.lines() {
        for prefix in ["refs/heads/", "refs/remotes/origin/", "refs/tags/"] {
            if let Some(name) = full.strip_prefix(prefix) {
                if name != "HEAD" {
                    names.insert(name.to_owned());
                }
            }
        }
    }

    for length in (1..=tail.len()).rev() {
        let candidate = tail[..length].join("/");
        if names.contains(&candidate) {
            let resolved = resolve_revision(repository, Some(&candidate), github_auth)?;
            let subpath = (length < tail.len()).then(|| tail[length..].iter().collect::<PathBuf>());
            return Ok((resolved, subpath));
        }
    }

    if looks_like_commit(&tail[0]) {
        let resolved = resolve_revision(repository, Some(&tail[0]), github_auth)?;
        let subpath = (tail.len() > 1).then(|| tail[1..].iter().collect::<PathBuf>());
        return Ok((resolved, subpath));
    }

    bail!(
        "could not separate the revision from the path in the repository URL; use the root URL with explicit --ref and --path"
    )
}

fn configure_sparse_checkout(
    repository: &Path,
    revision: &str,
    subpath: &Path,
    github_auth: bool,
) -> Result<bool> {
    let slash_path = path_to_slash_string(subpath)?;
    let mut selector = PathBuf::new();
    let mut enters_submodule = false;
    let parts = slash_path.split('/').collect::<Vec<_>>();
    for (index, part) in parts.iter().enumerate() {
        selector.push(part);
        let object = format!("{revision}:{}", path_to_slash_string(&selector)?);
        let kind = git_capture_with_auth(repository, ["cat-file", "-t", &object], github_auth)
            .with_context(|| {
                format!("failed to resolve selected path {slash_path:?} at {revision}")
            })?;
        match kind.as_str() {
            "commit" | "tree" => {
                enters_submodule = kind == "commit";
                if kind == "commit" || index + 1 == parts.len() {
                    break;
                }
            }
            "blob" => {
                selector.pop();
                break;
            }
            other => bail!("selected Git object has unsupported type {other:?}"),
        }
    }

    git_status(
        repository,
        [
            OsStr::new("sparse-checkout"),
            OsStr::new("init"),
            OsStr::new("--cone"),
        ],
        "initialize sparse checkout",
    )?;
    if selector.as_os_str().is_empty() {
        git_status(
            repository,
            [
                OsStr::new("sparse-checkout"),
                OsStr::new("set"),
                OsStr::new("--cone"),
                OsStr::new("--skip-checks"),
                OsStr::new("."),
            ],
            "configure sparse checkout",
        )?;
    } else {
        git_status(
            repository,
            [
                OsStr::new("sparse-checkout"),
                OsStr::new("set"),
                OsStr::new("--cone"),
                OsStr::new("--skip-checks"),
                selector.as_os_str(),
            ],
            "configure sparse checkout",
        )?;
    }
    Ok(enters_submodule)
}

fn resolve_subpath(root: &Path, subpath: Option<&Path>) -> Result<PathBuf> {
    let Some(subpath) = subpath else {
        return Ok(root.to_path_buf());
    };
    validate_relative_path(subpath)?;
    if subpath == Path::new(".") {
        return Ok(root.to_path_buf());
    }
    let joined = root.join(subpath);
    joined
        .symlink_metadata()
        .with_context(|| format!("selected path {} does not exist", subpath.display()))?;
    let parent = joined
        .parent()
        .context("selected path has no parent directory")?;
    let resolved_parent = fs::canonicalize(parent).with_context(|| {
        format!(
            "failed to resolve the parent of selected path {}",
            subpath.display()
        )
    })?;
    ensure!(
        resolved_parent.starts_with(root),
        "selected path escapes the source root"
    );
    let name = joined
        .file_name()
        .context("selected path has no final component")?;
    Ok(resolved_parent.join(name))
}

fn validate_selection_kind(path: &Path, selection_kind: Option<SelectionKind>) -> Result<()> {
    let Some(selection_kind) = selection_kind else {
        return Ok(());
    };
    let file_type = path
        .symlink_metadata()
        .with_context(|| format!("failed to inspect selected path {}", path.display()))?
        .file_type();
    match selection_kind {
        SelectionKind::Blob => ensure!(
            file_type.is_file() || file_type.is_symlink(),
            "the /blob/ URL does not identify a file"
        ),
        SelectionKind::Tree => ensure!(
            file_type.is_dir(),
            "the /tree/ URL does not identify a directory"
        ),
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty(),
        "selected path cannot be empty"
    );
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "selected path must be relative and cannot contain '..': {}",
                    path.display()
                );
            }
        }
    }
    path_to_slash_string(path)?;
    Ok(())
}

fn selects_root(path: &Path) -> bool {
    path.components()
        .all(|component| component == Component::CurDir)
}

fn validate_revision(revision: Option<&str>) -> Result<()> {
    if let Some(revision) = revision {
        ensure!(!revision.is_empty(), "revision cannot be empty");
        ensure!(!revision.starts_with('-'), "revision cannot begin with '-'");
        ensure!(
            !revision.chars().any(char::is_control),
            "revision cannot contain control characters"
        );
        if !looks_like_commit(revision) {
            ensure!(
                !revision.starts_with("refs/"),
                "revision must be a branch name, tag name, or full commit ID, not a fully qualified ref"
            );
            let qualified = format!("refs/heads/{revision}");
            let status = Command::new("git")
                .args(["check-ref-format", &qualified])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("failed to execute `git`; install Git and make sure it is on PATH")?;
            ensure!(
                status.success(),
                "revision is not a valid branch or tag name: {revision:?}"
            );
        }
    }
    Ok(())
}

fn discover_repository_root(path: &Path) -> Result<Option<PathBuf>> {
    let is_directory = path
        .symlink_metadata()
        .with_context(|| format!("failed to inspect local source {}", path.display()))?
        .file_type()
        .is_dir();
    let directory = if is_directory {
        path
    } else {
        path.parent()
            .context("local file has no parent directory")?
    };
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .output()
        .context("failed to execute `git`; install Git and make sure it is on PATH")?;
    if output.status.success() {
        let root = decode_command_stdout(output.stdout)?;
        return Ok(Some(PathBuf::from(root)));
    }
    let has_git_marker = directory
        .ancestors()
        .any(|ancestor| ancestor.join(".git").symlink_metadata().is_ok());
    if has_git_marker {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!("local path is inside an invalid Git working tree: {stderr}");
    }
    Ok(None)
}

#[derive(Debug)]
struct SubmoduleStatus {
    state: char,
    path: PathBuf,
}

fn submodule_statuses(repository: &Path) -> Result<Vec<SubmoduleStatus>> {
    let output = git_capture(repository, ["submodule", "status", "--recursive"])?;
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let state = line
                .chars()
                .next()
                .context("submodule status line is empty")?;
            let remainder = line
                .get(state.len_utf8()..)
                .context("invalid submodule status")?
                .trim_start();
            let (commit, path_and_description) = remainder
                .split_once(' ')
                .context("submodule status is missing a path")?;
            ensure!(
                is_full_object_id(commit),
                "submodule status contains an invalid commit ID"
            );
            let path = path_and_description
                .rsplit_once(" (")
                .map_or(path_and_description, |(path, _)| path);
            validate_relative_path(Path::new(path))?;
            Ok(SubmoduleStatus {
                state,
                path: PathBuf::from(path),
            })
        })
        .collect()
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn try_resolve_commit(repository: &Path, revision: &str) -> Result<Option<String>> {
    let expression = format!("{revision}^{{commit}}");
    git_capture_optional(
        repository,
        ["rev-parse", "--verify", "--quiet", &expression],
        &[1],
    )
}

fn git_capture<I, S>(repository: &Path, arguments: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_capture_with_auth(repository, arguments, false)
}

fn git_capture_with_auth<I, S>(repository: &Path, arguments: I, github_auth: bool) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = git_command(repository, github_auth);
    let output = command
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .with_context(|| "failed to execute `git`; install Git and make sure it is on PATH")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!("git command failed with status {}: {stderr}", output.status);
    }
    decode_command_stdout(output.stdout)
}

fn git_capture_optional<I, S>(
    repository: &Path,
    arguments: I,
    absent_exit_codes: &[i32],
) -> Result<Option<String>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = git_command(repository, false);
    let output = command
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .context("failed to execute `git`; install Git and make sure it is on PATH")?;
    if output.status.success() {
        return decode_command_stdout(output.stdout).map(Some);
    }
    if output
        .status
        .code()
        .is_some_and(|code| absent_exit_codes.contains(&code))
    {
        return Ok(None);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    bail!("git command failed with status {}: {stderr}", output.status)
}

fn decode_command_stdout(stdout: Vec<u8>) -> Result<String> {
    let value = String::from_utf8(stdout).context("git produced non-UTF-8 output")?;
    Ok(value.trim_end_matches(['\r', '\n']).to_owned())
}

fn git_status<I, S>(repository: &Path, arguments: I, action: &str) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command.arg("-C").arg(repository).args(arguments);
    let status = command_status(&mut command, action)?;
    ensure!(
        status.success(),
        "failed to {action}; git exited with status {status}"
    );
    Ok(())
}

fn git_status_with_auth<I, S>(
    repository: &Path,
    arguments: I,
    action: &str,
    github_auth: bool,
) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = git_command(repository, github_auth);
    command.args(arguments);
    let status = command_status(&mut command, action)?;
    ensure!(
        status.success(),
        "failed to {action}; git exited with status {status}"
    );
    Ok(())
}

fn git_command(repository: &Path, github_auth: bool) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repository);
    if github_auth {
        command
            .arg("-c")
            .arg("credential.https://github.com.helper=")
            .arg("-c")
            .arg("credential.https://github.com.helper=!gh auth git-credential");
    }
    command
}

fn command_status(command: &mut Command, action: &str) -> Result<std::process::ExitStatus> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to {action}; the required executable may not be on PATH"))
}

fn normalize_slug(raw: &str) -> Result<String> {
    let trimmed = raw.trim_matches('/').trim_end_matches(".git");
    let parts = trimmed.split('/').collect::<Vec<_>>();
    ensure!(
        parts.len() == 2,
        "GitHub source must have the form owner/repository"
    );
    for part in &parts {
        ensure!(
            !part.is_empty(),
            "GitHub owner and repository cannot be empty"
        );
        ensure!(
            part.chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || matches!(character, '-' | '_' | '.')),
            "invalid character in GitHub owner or repository"
        );
    }
    Ok(format!("{}/{}", parts[0], parts[1]))
}

fn decode_url_segment(segment: &str) -> Result<String> {
    let decoded = percent_decode_str(segment)
        .decode_utf8()
        .context("Git URL path contains invalid UTF-8")
        .map(|value| value.into_owned())?;
    ensure!(
        !decoded.chars().any(char::is_control),
        "Git URL path cannot contain control characters"
    );
    Ok(decoded)
}

fn repository_name_from_remote(remote: &str) -> Result<Option<String>> {
    if let Some(rest) = remote.strip_prefix("git@github.com:") {
        return normalize_slug(rest).map(Some);
    }
    if remote.contains("://") {
        let url = Url::parse(remote).context("origin contains an invalid URL")?;
        let segments = url
            .path_segments()
            .context("origin URL has no path")?
            .filter(|part| !part.is_empty())
            .map(decode_url_segment)
            .collect::<Result<Vec<_>>>()?;
        let name = repository_name_from_path(&segments.join("/"))
            .context("origin URL does not identify a repository")?;
        validate_repository_label(&name)?;
        return Ok(Some(name));
    }
    if let Some(remote_path) = scp_remote_path(remote)? {
        let name = repository_name_from_path(remote_path)
            .context("SCP-style origin has no repository name")?;
        validate_repository_label(&name)?;
        return Ok(Some(name));
    }
    let name = repository_name_from_path(remote);
    ensure!(
        name.as_deref()
            .is_none_or(|value| !value.chars().any(char::is_control)),
        "origin repository name contains control characters"
    );
    Ok(name)
}

fn scp_remote_path(remote: &str) -> Result<Option<&str>> {
    if remote.contains("://") || !remote.contains('@') || !remote.contains(':') {
        return Ok(None);
    }
    let (authority, remote_path) = remote
        .rsplit_once(':')
        .context("SCP-style Git remote is missing a repository path")?;
    let (user, host) = authority
        .split_once('@')
        .context("SCP-style Git remote must have the form user@host:path")?;
    ensure!(
        !user.is_empty() && !host.is_empty() && !host.contains('/'),
        "SCP-style Git remote has an invalid user or host"
    );
    ensure!(
        !authority.chars().any(char::is_control),
        "SCP-style Git remote cannot contain control characters"
    );
    Ok(Some(remote_path))
}

fn repository_name_from_path(path: &str) -> Option<String> {
    let trimmed = path.trim_matches('/').trim_end_matches(".git");
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.rsplit('/');
    let repo = parts.next()?;
    Some(match parts.next() {
        Some(owner) => format!("{owner}/{repo}"),
        None => repo.to_owned(),
    })
}

fn validate_repository_label(label: &str) -> Result<()> {
    ensure!(!label.is_empty(), "repository name cannot be empty");
    ensure!(
        !label.chars().any(char::is_control),
        "repository name cannot contain control characters"
    );
    Ok(())
}

fn repository_basename(label: &str) -> Result<String> {
    let name = label
        .rsplit('/')
        .next()
        .context("remote repository has no display name")?;
    ensure!(!name.is_empty(), "remote repository has no display name");
    Ok(name.to_owned())
}

fn relative_display(root: &Path, selected: &Path) -> Result<Option<String>> {
    let relative = selected
        .strip_prefix(root)
        .context("selected path is not contained by the source root")?;
    if relative.as_os_str().is_empty() {
        Ok(None)
    } else {
        path_to_slash_string(relative).map(Some)
    }
}

fn path_label(path: &Path) -> Result<String> {
    match path.file_name() {
        Some(name) => {
            let name = name
                .to_str()
                .with_context(|| format!("path name is not valid UTF-8: {}", path.display()))?;
            ensure!(
                !name.chars().any(char::is_control),
                "path name contains control characters: {}",
                path.display()
            );
            Ok(name.to_owned())
        }
        None => Ok("root".to_owned()),
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

fn looks_like_commit(value: &str) -> bool {
    is_full_object_id(value)
}

fn is_full_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_git<I, S>(repository: &Path, arguments: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let status = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(arguments)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn prepare_options() -> PrepareOptions {
        PrepareOptions {
            revision: None,
            subpath: None,
            include_submodules: true,
            jobs: 1,
        }
    }

    #[test]
    fn parses_repository_and_tree_urls_without_guessing_the_ref() {
        let root = parse_remote("https://github.com/acme/widgets.git").unwrap();
        assert_eq!(root.github_slug.as_deref(), Some("acme/widgets"));
        assert!(root.url_tail.is_empty());

        let tree =
            parse_remote("https://github.com/acme/widgets/tree/feature/api/src/lib").unwrap();
        assert_eq!(tree.selection_kind, Some(SelectionKind::Tree));
        assert_eq!(tree.url_tail, ["feature", "api", "src", "lib"]);

        let nested = parse_remote("https://git.example/group/team/widgets.git").unwrap();
        assert_eq!(nested.repository_label, "team/widgets");

        let unnamespaced = parse_remote("ssh://git@git.example/widgets.git").unwrap();
        assert_eq!(unnamespaced.repository_label, "widgets");

        let custom_port = parse_remote("ssh://git@github.com:2222/acme/widgets.git").unwrap();
        assert!(custom_port.github_slug.is_none());
        assert_eq!(
            custom_port.clone_target,
            "ssh://git@github.com:2222/acme/widgets.git"
        );

        assert_eq!(
            repository_name_from_remote("git@git.example:group/widgets.git").unwrap(),
            Some("group/widgets".to_owned())
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_paths_preserve_surrounding_whitespace() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join(" repository ");
        fs::create_dir(&source).unwrap();

        let prepared = prepare(source.to_str().unwrap(), &prepare_options()).unwrap();

        assert_eq!(prepared.scan_root, source.canonicalize().unwrap());
        assert_eq!(prepared.metadata.label, " repository ");
    }

    #[cfg(unix)]
    #[test]
    fn local_symlink_sources_are_not_followed() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let link = directory.path().join("link");
        let dangling = directory.path().join("dangling");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("secret.txt"), "not followed").unwrap();
        symlink(&target, &link).unwrap();
        symlink("missing", &dangling).unwrap();

        let prepared = prepare(link.to_str().unwrap(), &prepare_options()).unwrap();

        assert!(
            prepared
                .scan_root
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(prepared.metadata.label, "link");
        let dangling = prepare(dangling.to_str().unwrap(), &prepare_options()).unwrap();
        assert!(
            dangling
                .scan_root
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            prepare(
                link.to_str().unwrap(),
                &PrepareOptions {
                    subpath: Some(PathBuf::from("secret.txt")),
                    ..prepare_options()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_ambiguous_or_unsafe_inputs() {
        assert!(parse_remote("acme/widgets/extra").is_err());
        assert!(parse_remote("https://github.com/acme/widgets/issues/1").is_err());
        assert!(validate_relative_path(Path::new("../secret")).is_err());
        assert!(validate_revision(Some("--upload-pack=evil")).is_err());
        assert!(validate_revision(Some("feature/api")).is_ok());
        assert!(validate_revision(Some("release~1")).is_err());
        assert!(validate_revision(Some("refs/heads/main")).is_err());
        assert!(repository_name_from_remote("https://[invalid").is_err());
        assert!(repository_name_from_remote("https://host/owner/%0Arepo").is_err());
        assert!(parse_remote("user@:owner/repository.git").is_err());
        assert!(parse_remote("@host:owner/repository.git").is_err());
        assert!(parse_remote("ssh://user:password@git.example/repository.git").is_err());
    }

    #[test]
    fn recognizes_commit_ids_without_mistaking_names_for_ids() {
        assert!(looks_like_commit(
            "0123456789abcdef0123456789abcdef01234567"
        ));
        assert!(looks_like_commit(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        ));
        assert!(!looks_like_commit("deadbee"));
        assert!(!looks_like_commit("release-2026"));
        assert!(!looks_like_commit("abc"));
    }

    #[test]
    fn recognizes_explicit_root_subpaths() {
        assert!(selects_root(Path::new(".")));
        assert!(selects_root(Path::new("./.")));
        assert!(!selects_root(Path::new("src")));
        assert_eq!(repository_basename("owner/widgets").unwrap(), "widgets");
    }

    #[test]
    fn url_selection_kinds_distinguish_regular_files_and_directories() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("file.txt");
        let child = directory.path().join("child");
        fs::write(&file, "content").unwrap();
        fs::create_dir(&child).unwrap();

        assert!(validate_selection_kind(&file, Some(SelectionKind::Blob)).is_ok());
        assert!(validate_selection_kind(&file, Some(SelectionKind::Tree)).is_err());
        assert!(validate_selection_kind(&child, Some(SelectionKind::Tree)).is_ok());
        assert!(validate_selection_kind(&child, Some(SelectionKind::Blob)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn git_symlinks_remain_blob_selections() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let link = directory.path().join("link");
        fs::create_dir(&target).unwrap();
        symlink(&target, &link).unwrap();

        assert!(validate_selection_kind(&link, Some(SelectionKind::Blob)).is_ok());
        assert!(validate_selection_kind(&link, Some(SelectionKind::Tree)).is_err());
    }

    #[test]
    fn url_selection_clones_all_ref_tips_without_full_history() {
        let spec = parse_remote("https://github.com/acme/widgets/tree/release/src").unwrap();
        let arguments = clone_arguments(&spec, None);
        let arguments = arguments
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            arguments,
            [
                "--filter=blob:none",
                "--no-checkout",
                "--depth=1",
                "--no-single-branch",
                "--tags",
            ]
        );
    }

    #[test]
    fn authenticated_git_commands_use_the_cli_credential_helper() {
        let command = git_command(Path::new("repository"), true);
        let arguments = command
            .get_args()
            .map(|value| value.to_str().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            arguments,
            [
                "-C",
                "repository",
                "-c",
                "credential.https://github.com.helper=",
                "-c",
                "credential.https://github.com.helper=!gh auth git-credential",
            ]
        );
    }

    #[test]
    fn git_output_decoder_preserves_leading_status_whitespace() {
        let output =
            decode_command_stdout(b" abcdef path/to/module (heads/main)\n".to_vec()).unwrap();
        assert_eq!(output, " abcdef path/to/module (heads/main)");
    }

    #[test]
    fn branch_names_take_precedence_over_colliding_tag_names() {
        let repository = tempfile::tempdir().unwrap();
        test_git(repository.path(), ["init", "-b", "main"]);
        test_git(repository.path(), ["config", "user.name", "Test Author"]);
        test_git(
            repository.path(),
            ["config", "user.email", "test@example.invalid"],
        );
        fs::write(repository.path().join("value.txt"), "tag\n").unwrap();
        test_git(repository.path(), ["add", "."]);
        test_git(repository.path(), ["commit", "-m", "tag target"]);
        test_git(repository.path(), ["tag", "release"]);
        test_git(repository.path(), ["switch", "-c", "release"]);
        fs::write(repository.path().join("value.txt"), "branch\n").unwrap();
        test_git(repository.path(), ["commit", "-am", "branch target"]);

        let branch_commit =
            git_capture(repository.path(), ["rev-parse", "refs/heads/release"]).unwrap();
        let tag_commit =
            git_capture(repository.path(), ["rev-parse", "refs/tags/release"]).unwrap();
        let resolved = resolve_revision(repository.path(), Some("release"), false).unwrap();

        assert_ne!(branch_commit, tag_commit);
        assert_eq!(resolved.checkout, branch_commit);
    }
}
