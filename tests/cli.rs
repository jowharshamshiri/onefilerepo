use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn executable() -> &'static str {
    env!("CARGO_BIN_EXE_onefilerepo")
}

fn run<I, S>(directory: &Path, arguments: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(executable())
        .current_dir(directory)
        .args(arguments)
        .output()
        .expect("the command must start")
}

fn git<I, S>(directory: &Path, arguments: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .current_dir(directory)
        .args(arguments)
        .output()
        .expect("git must start");
    assert!(
        output.status.success(),
        "git failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn initialize_repository(path: &Path) {
    fs::create_dir_all(path).unwrap();
    git(path, ["init", "-b", "main"]);
    git(path, ["config", "user.name", "Test Author"]);
    git(path, ["config", "user.email", "test@example.invalid"]);
}

fn commit_everything(path: &Path, message: &str) {
    git(path, ["add", "."]);
    git(path, ["commit", "-m", message]);
}

#[test]
fn stdout_is_machine_clean_and_filters_are_applied() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("src")).unwrap();
    fs::write(directory.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(directory.path().join("src/notes.txt"), "keep me\n").unwrap();
    fs::write(directory.path().join("src/private.txt"), "do not include\n").unwrap();
    fs::write(directory.path().join("asset.dat"), [0, 1, 2]).unwrap();

    let output = run(
        directory.path(),
        [
            ".",
            "--output",
            "-",
            "--quiet",
            "--include",
            "*.txt",
            "--exclude",
            "private.txt",
        ],
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let digest = String::from_utf8(output.stdout).unwrap();
    assert!(digest.starts_with("Directory structure:\n└── "));
    assert!(digest.contains("FILE: src/notes.txt"));
    assert!(digest.contains("keep me"));
    assert!(!digest.contains("private.txt"));
    assert!(!digest.contains("main.rs"));
    assert!(!digest.contains("asset.dat"));
}

#[test]
fn file_output_is_replaced_and_never_ingests_itself() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("input.txt"), "first\n").unwrap();
    fs::write(directory.path().join("result.txt"), "stale output\n").unwrap();

    let output = run(directory.path(), [".", "--output", "result.txt", "--quiet"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());

    let digest = fs::read_to_string(directory.path().join("result.txt")).unwrap();
    assert!(digest.contains("FILE: input.txt"));
    assert!(!digest.contains("FILE: result.txt"));
    assert!(!digest.contains("stale output"));
    let temporary_files = fs::read_dir(directory.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".onefilerepo-")
        })
        .count();
    assert_eq!(temporary_files, 0);
}

#[test]
fn invalid_limit_relationship_exits_without_creating_output() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("input.txt"), "data").unwrap();

    let output = run(
        directory.path(),
        [
            ".",
            "--max-file-size",
            "2MiB",
            "--max-total-size",
            "1MiB",
            "--output",
            "result.txt",
        ],
    );

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--max-file-size cannot exceed --max-total-size")
    );
    assert!(!directory.path().join("result.txt").exists());
}

#[test]
fn local_submodules_are_included_or_strictly_excluded() {
    let sandbox = tempfile::tempdir().unwrap();
    let child = sandbox.path().join("child");
    initialize_repository(&child);
    fs::write(child.join("child.txt"), "submodule payload\n").unwrap();
    commit_everything(&child, "Add child content");

    let parent = sandbox.path().join("parent");
    initialize_repository(&parent);
    fs::write(parent.join("root.txt"), "root payload\n").unwrap();
    git(
        &parent,
        [
            OsStr::new("-c"),
            OsStr::new("protocol.file.allow=always"),
            OsStr::new("submodule"),
            OsStr::new("add"),
            child.as_os_str(),
            OsStr::new("modules/child"),
        ],
    );
    commit_everything(&parent, "Add child submodule");

    let checkout = sandbox.path().join("checkout");
    git(
        sandbox.path(),
        [
            OsStr::new("clone"),
            OsStr::new("--no-recurse-submodules"),
            parent.as_os_str(),
            checkout.as_os_str(),
        ],
    );

    let uninitialized = run(&checkout, [".", "--output", "-", "--quiet"]);
    assert!(!uninitialized.status.success());
    assert!(String::from_utf8_lossy(&uninitialized.stderr).contains("is not initialized"));

    let excluded = run(
        &checkout,
        [".", "--no-submodules", "--output", "-", "--quiet"],
    );
    assert!(
        excluded.status.success(),
        "{}",
        String::from_utf8_lossy(&excluded.stderr)
    );
    let excluded_digest = String::from_utf8(excluded.stdout).unwrap();
    assert!(excluded_digest.contains("FILE: root.txt"));
    assert!(!excluded_digest.contains("child.txt"));

    git(
        &checkout,
        [
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--recursive",
        ],
    );
    let included = run(&checkout, [".", "--output", "-", "--quiet"]);
    assert!(
        included.status.success(),
        "{}",
        String::from_utf8_lossy(&included.stderr)
    );
    let included_digest = String::from_utf8(included.stdout).unwrap();
    assert!(included_digest.contains("FILE: modules/child/child.txt"));
    assert!(included_digest.contains("submodule payload"));
}
