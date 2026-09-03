# onefilerepo

`onefilerepo` turns a local directory or Git repository into one deterministic,
LLM-ready text file. It reads local working trees directly, clones GitHub
repositories through the authenticated GitHub CLI, and uses Git for all other
remotes. Private repositories and recursive submodules therefore use the same
credentials you already use locally.

The utility is written in Rust and designed for large repositories: clone-time
blob filtering and sparse checkout reduce network transfer, directory walking
uses native ignore semantics, file reads run in parallel, and results are sorted
before output so concurrency never changes the digest.

## Install

Requirements:

- Rust 1.88 or newer
- Git
- [GitHub CLI](https://cli.github.com/) for GitHub sources, authenticated with
  `gh auth login`

Install directly from the repository:

```sh
cargo install --git https://github.com/jowharshamshiri/onefilerepo.git
```

## Usage

```sh
# Current directory to digest.txt
onefilerepo

# A local repository, including initialized submodules
onefilerepo ~/work/my-project

# A public or private GitHub repository through your gh login
onefilerepo owner/repository
onefilerepo https://github.com/owner/repository

# A branch, tag, or full commit ID and a repository-relative subtree
onefilerepo owner/repository --ref feature/api --path crates/server

# GitHub tree and blob URLs are understood directly, including branch names
# that contain slashes
onefilerepo https://github.com/owner/repository/tree/feature/api/crates/server

# Any other Git remote uses your normal Git credential and SSH configuration
onefilerepo ssh://git@git.example.com/team/repository.git

# Stream only the digest to another command; progress and summary stay on stderr
onefilerepo owner/repository --output - | llm-tool
```

Run `onefilerepo --help` for the complete option list.

### Filtering and limits

Include and exclude options are repeatable. Include globs are an allowlist;
excludes are Git-wildmatch patterns and take precedence.

```sh
onefilerepo . \
  --include '*.rs' \
  --include '*.toml' \
  --exclude 'fixtures/**' \
  --max-file-size 2MiB \
  --max-total-size 100MiB \
  --max-files 50000
```

The size parser accepts bytes and binary `KiB`, `MiB`, or `GiB` suffixes (the
short forms `k`, `m`, and `g` are also accepted). Limits are applied after a
stable path sort. Omitted-file counts are reported in the summary, so a digest
can never be silently and nondeterministically truncated.

The built-in exclusions remove VCS internals, dependency trees, build output,
caches, lock files, databases, archives, and common binary media. Repository
`.gitignore` files, global Git ignores, `.ignore` files, and nested
`.onefilerepoignore` files are honored. Pass `--include-ignored` to disable those
repository/user ignore files; the safety-oriented built-in exclusions and
explicit `--exclude` rules still apply.

### Submodules

Submodules are included recursively by default.

- Remote repositories initialize them recursively, with shallow, blob-filtered
  fetches and parallel checkout.
- Local repositories are never mutated. Every relevant submodule must already
  be initialized; otherwise the command exits with the exact remediation.
- A locally checked-out submodule commit may differ from the superproject's
  recorded commit. Its current working tree is intentionally digested.

Use `--no-submodules` to exclude all submodule trees.

### Output contract

The output is UTF-8 and contains two sections:

1. A Unicode directory tree.
2. File records separated by 48 `=` characters, with an explicit `FILE` or
   `SYMLINK` path header.

Files are ordered deterministically. UTF-8, UTF-8 BOM, and BOM-marked UTF-16 are
decoded; other byte streams are represented as binary without embedding their
contents. Empty files and symlinks have explicit markers. Symlink targets are
reported but never followed. Notebook files contain only ordered cell sources,
without output blobs or metadata.

Writing to a file is atomic: data is flushed and synced in the destination
directory before replacing the requested path. Existing output located inside
the scanned tree is excluded from its own digest. Any traversal, decoding,
repository-state, or write error terminates with a nonzero exit status instead
of producing a partially valid result.

The completion summary is printed separately from the digest. It includes the
resolved commit, local dirty-state information, selected subpath, analyzed file
and submodule counts, source size, every limit omission, and a deterministic
lexical token estimate.

## Development

The test suite contains unit coverage for parsing, filtering, deterministic
limits, text decoding, notebook normalization, formatting, and integration
coverage for process-level CLI behavior and Git submodule state. Network access
is not required by the tests.

When you are ready to validate a checkout:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features
cargo test --all-features
```

The repository workflow is manually dispatched so cloning or pushing never
starts a build without an explicit request.

## License

MIT
