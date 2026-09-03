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
    let trimmed = source.trim();
    ensure!(!trimmed.is_empty(), "source cannot be empty");

    let local = Path::new(trimmed);
    if local.exists() {
        return prepare_local(local, options);
    }

    prepare_remote(parse_remote(trimmed)?, options)
}

fn prepare_local(path: &Path, options: &PrepareOptions) -> Result<PreparedSource> {
    ensure!(
        options.revision.is_none(),
        "--ref is only valid for remote sources; local sources always represent their current working tree"
    );

    let source_root = fs::canonicalize(path)
        .with_context(|| format!("failed to resolve local source {}", path.display()))?;
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

    let selected_subpath = options.subpath.clone().or(url_subpath);
    if let Some(path) = selected_subpath.as_deref() {
        validate_relative_path(path)?;
        configure_sparse_checkout(&clone_root, &revision.checkout, path)?;
    }
    git_status(
        &clone_root,
        [
            OsStr::new("checkout"),
            OsStr::new("--detach"),
            OsStr::new("--force"),
            OsStr::new(&revision.checkout),
            OsStr::new("--"),
        ],
        "check out the requested revision",
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
    if spec.selection_kind == Some(SelectionKind::Blob) {
        ensure!(
            scan_root.is_file(),
            "the /blob/ URL does not identify a file"
        );
    } else if spec.selection_kind == Some(SelectionKind::Tree) {
        ensure!(
            scan_root.is_dir(),
            "the /tree/ URL does not identify a directory"
        );
    }

    let commit = git_capture(&clone_root, ["rev-parse", "HEAD"])?;
    let label = if scan_root.file_name().is_none() {
        spec.repository_label.clone()
    } else {
        path_label(&scan_root)?
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

    if source.contains('@') && source.contains(':') && !source.contains("://") {
        let label = source
            .rsplit_once(':')
            .and_then(|(_, path)| repository_name_from_path(path))
            .unwrap_or_else(|| source.to_owned());
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
    if matches!(url.scheme(), "http" | "https") {
        ensure!(
            url.username().is_empty() && url.password().is_none(),
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
    ensure!(
        segments.len() >= 2,
        "Git URL must contain an owner and repository"
    );

    if host.eq_ignore_ascii_case("github.com") {
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

    ensure!(
        segments.len() == 2,
        "subpaths in non-GitHub URLs require --ref and --path"
    );
    let label = format!("{}/{}", segments[0], segments[1].trim_end_matches(".git"));
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
    let mut git_arguments = vec![
        OsString::from("--filter=blob:none"),
        OsString::from("--no-checkout"),
    ];
    if spec.url_tail.is_empty() {
        git_arguments.push(OsString::from("--depth=1"));
        git_arguments.push(OsString::from("--single-branch"));
        if let Some(revision) = requested_revision.filter(|value| !looks_like_commit(value)) {
            git_arguments.push(OsString::from("--branch"));
            git_arguments.push(OsString::from(revision));
        }
    }

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
        let candidates = if requested.starts_with("refs/") {
            vec![requested.to_owned()]
        } else {
            vec![
                format!("refs/heads/{requested}"),
                format!("refs/remotes/origin/{requested}"),
                format!("refs/tags/{requested}"),
                requested.to_owned(),
            ]
        };
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

fn configure_sparse_checkout(repository: &Path, revision: &str, subpath: &Path) -> Result<()> {
    let slash_path = path_to_slash_string(subpath)?;
    let mut selector = PathBuf::new();
    let mut found = false;
    let parts = slash_path.split('/').collect::<Vec<_>>();
    for (index, part) in parts.iter().enumerate() {
        selector.push(part);
        let object = format!("{revision}:{}", path_to_slash_string(&selector)?);
        if let Some(kind) = git_capture_optional(repository, ["cat-file", "-t", &object], &[128])? {
            match kind.as_str() {
                "commit" | "tree" => {
                    found = true;
                    if kind == "commit" || index + 1 == parts.len() {
                        break;
                    }
                }
                "blob" => {
                    selector.pop();
                    found = true;
                    break;
                }
                other => bail!("selected Git object has unsupported type {other:?}"),
            }
        } else {
            break;
        }
    }
    ensure!(
        found,
        "selected path {slash_path:?} does not exist at revision {revision}"
    );

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
    Ok(())
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

fn validate_revision(revision: Option<&str>) -> Result<()> {
    if let Some(revision) = revision {
        ensure!(!revision.is_empty(), "revision cannot be empty");
        ensure!(!revision.starts_with('-'), "revision cannot begin with '-'");
        ensure!(
            !revision.chars().any(char::is_control),
            "revision cannot contain control characters"
        );
    }
    Ok(())
}

fn discover_repository_root(path: &Path) -> Result<Option<PathBuf>> {
    let directory = if path.is_dir() {
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
        let root = String::from_utf8(output.stdout).context("git produced non-UTF-8 output")?;
        return Ok(Some(PathBuf::from(root.trim())));
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
                commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()),
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
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .with_context(|| "failed to execute `git`; install Git and make sure it is on PATH")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!("git command failed with status {}: {stderr}", output.status);
    }
    String::from_utf8(output.stdout)
        .context("git produced non-UTF-8 output")
        .map(|value| value.trim().to_owned())
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
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .context("failed to execute `git`; install Git and make sure it is on PATH")?;
    if output.status.success() {
        return String::from_utf8(output.stdout)
            .context("git produced non-UTF-8 output")
            .map(|value| Some(value.trim().to_owned()));
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
    let mut command = Command::new("git");
    command.arg("-C").arg(repository);
    if github_auth {
        command
            .arg("-c")
            .arg("credential.https://github.com.helper=")
            .arg("-c")
            .arg("credential.https://github.com.helper=!gh auth git-credential");
    }
    command.args(arguments);
    let status = command_status(&mut command, action)?;
    ensure!(
        status.success(),
        "failed to {action}; git exited with status {status}"
    );
    Ok(())
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
    percent_decode_str(segment)
        .decode_utf8()
        .context("Git URL path contains invalid UTF-8")
        .map(|value| value.into_owned())
}

fn repository_name_from_remote(remote: &str) -> Result<Option<String>> {
    if let Some(rest) = remote.strip_prefix("git@github.com:") {
        return normalize_slug(rest).map(Some);
    }
    if let Ok(url) = Url::parse(remote) {
        let segments = url
            .path_segments()
            .context("origin URL has no path")?
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        if segments.len() >= 2 {
            let repository_segment = segments.last().context("origin repository is missing")?;
            return Ok(Some(format!(
                "{}/{}",
                percent_decode_str(segments[segments.len() - 2])
                    .decode_utf8()
                    .context("origin owner is not valid UTF-8")?,
                percent_decode_str(repository_segment)
                    .decode_utf8()
                    .context("origin repository is not valid UTF-8")?
                    .trim_end_matches(".git")
            )));
        }
    }
    Ok(repository_name_from_path(remote))
}

fn repository_name_from_path(path: &str) -> Option<String> {
    let trimmed = path.trim_matches('/').trim_end_matches(".git");
    let mut parts = trimmed.rsplit('/');
    let repo = parts.next()?;
    let owner = parts.next()?;
    Some(format!("{owner}/{repo}"))
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
        Some(name) => name
            .to_str()
            .with_context(|| format!("path name is not valid UTF-8: {}", path.display()))
            .map(str::to_owned),
        None => Ok("root".to_owned()),
    }
}

fn path_to_slash_string(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))?;
    Ok(value.replace(std::path::MAIN_SEPARATOR, "/"))
}

fn looks_like_commit(value: &str) -> bool {
    (7..=40).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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

    #[test]
    fn parses_repository_and_tree_urls_without_guessing_the_ref() {
        let root = parse_remote("https://github.com/acme/widgets.git").unwrap();
        assert_eq!(root.github_slug.as_deref(), Some("acme/widgets"));
        assert!(root.url_tail.is_empty());

        let tree =
            parse_remote("https://github.com/acme/widgets/tree/feature/api/src/lib").unwrap();
        assert_eq!(tree.selection_kind, Some(SelectionKind::Tree));
        assert_eq!(tree.url_tail, ["feature", "api", "src", "lib"]);
    }

    #[test]
    fn rejects_ambiguous_or_unsafe_inputs() {
        assert!(parse_remote("acme/widgets/extra").is_err());
        assert!(parse_remote("https://github.com/acme/widgets/issues/1").is_err());
        assert!(validate_relative_path(Path::new("../secret")).is_err());
        assert!(validate_revision(Some("--upload-pack=evil")).is_err());
    }

    #[test]
    fn recognizes_commit_ids_without_mistaking_names_for_ids() {
        assert!(looks_like_commit(
            "0123456789abcdef0123456789abcdef01234567"
        ));
        assert!(looks_like_commit("deadbee"));
        assert!(!looks_like_commit("release-2026"));
        assert!(!looks_like_commit("abc"));
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
