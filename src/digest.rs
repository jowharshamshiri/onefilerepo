use std::collections::BTreeMap;
use std::io::Write;

use anyhow::{Context, Result, ensure};

use crate::scan::{FileContent, FileKind, ScanResult, ScannedFile};
use crate::source::SourceMetadata;

const SEPARATOR: &str = "================================================";

#[derive(Debug)]
pub struct Digest {
    metadata: SourceMetadata,
    scan: ScanResult,
    tree: String,
    estimated_tokens: usize,
}

impl Digest {
    pub fn new(metadata: SourceMetadata, scan: ScanResult) -> Result<Self> {
        ensure!(
            !scan.root_is_file || scan.files.len() == 1,
            "a single-file digest must contain exactly one file"
        );
        ensure!(
            scan.root_name == metadata.label,
            "source metadata label does not match the scanned root"
        );
        let tree = create_tree(&metadata.label, scan.root_is_file, &scan.files)?;
        let estimated_tokens =
            estimate_tokens(&tree) + scan.files.iter().map(estimate_file_tokens).sum::<usize>();
        Ok(Self {
            metadata,
            scan,
            tree,
            estimated_tokens,
        })
    }

    #[must_use]
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        if let Some(repository) = self.metadata.repository.as_deref() {
            lines.push(format!("Repository: {repository}"));
        } else {
            lines.push(format!("Directory: {}", self.metadata.label));
        }
        if let Some(revision) = self.metadata.revision.as_deref() {
            lines.push(format!("Reference: {revision}"));
        }
        if let Some(commit) = self.metadata.commit.as_deref() {
            lines.push(format!("Commit: {commit}"));
        }
        if let Some(subpath) = self.metadata.subpath.as_deref() {
            lines.push(format!("Subpath: {subpath}"));
        }
        if let Some(dirty) = self.metadata.working_tree_dirty {
            lines.push(format!(
                "Working tree: {}",
                if dirty { "dirty" } else { "clean" }
            ));
        }
        lines.push(format!("Files analyzed: {}", self.scan.files.len()));
        lines.push(format!(
            "Source size: {}",
            human_bytes(self.scan.stats.included_bytes)
        ));
        lines.push(format!(
            "Submodules analyzed: {}",
            self.metadata.submodule_count
        ));
        if self.scan.stats.skipped_too_large > 0 {
            lines.push(format!(
                "Files omitted by per-file size limit: {}",
                self.scan.stats.skipped_too_large
            ));
        }
        if self.scan.stats.skipped_by_limits > 0 {
            lines.push(format!(
                "Files omitted by aggregate limits: {}",
                self.scan.stats.skipped_by_limits
            ));
        }
        if self.scan.stats.skipped_by_depth > 0 {
            lines.push(format!(
                "Paths omitted by depth limit: {}",
                self.scan.stats.skipped_by_depth
            ));
        }
        lines.push(format!(
            "Estimated tokens: {}",
            human_count(self.estimated_tokens)
        ));
        lines.join("\n")
    }

    pub fn write_to(&self, writer: &mut dyn Write) -> Result<()> {
        writer
            .write_all(self.tree.as_bytes())
            .context("failed to write directory tree")?;
        writer
            .write_all(b"\n")
            .context("failed to write digest separator")?;
        for file in &self.scan.files {
            write_file(writer, file).with_context(|| {
                format!("failed to write digest record for {}", file.relative_path)
            })?;
        }
        Ok(())
    }

    #[must_use]
    pub fn files(&self) -> &[ScannedFile] {
        &self.scan.files
    }

    #[must_use]
    pub fn tree(&self) -> &str {
        &self.tree
    }

    #[must_use]
    pub const fn estimated_tokens(&self) -> usize {
        self.estimated_tokens
    }
}

#[derive(Debug, Default)]
struct TreeNode {
    children: BTreeMap<String, TreeNode>,
    file_kind: Option<FileKind>,
    symlink_target: Option<String>,
}

fn create_tree(root_name: &str, root_is_file: bool, files: &[ScannedFile]) -> Result<String> {
    let mut output = String::from("Directory structure:\n");
    if root_is_file {
        let file = files.first().context("single-file digest has no file")?;
        ensure!(
            files.len() == 1,
            "single-file digest contains multiple files"
        );
        output.push_str("└── ");
        output.push_str(root_name);
        if let FileContent::Symlink(target) = &file.content {
            output.push_str(" -> ");
            output.push_str(target);
        }
        output.push('\n');
        return Ok(output);
    }

    let mut root = TreeNode::default();
    for file in files {
        insert_file(&mut root, file)?;
    }
    output.push_str("└── ");
    output.push_str(root_name);
    output.push_str("/\n");
    render_children(&root, "    ", &mut output);
    Ok(output)
}

fn insert_file(root: &mut TreeNode, file: &ScannedFile) -> Result<()> {
    let parts = file.relative_path.split('/').collect::<Vec<_>>();
    ensure!(!parts.is_empty(), "file path cannot be empty");
    ensure!(
        parts.iter().all(|part| !part.is_empty()),
        "file path contains an empty component"
    );

    let mut current = root;
    for (index, part) in parts.iter().enumerate() {
        current = current.children.entry((*part).to_owned()).or_default();
        if index + 1 == parts.len() {
            ensure!(
                current.file_kind.is_none(),
                "duplicate file path in scan: {}",
                file.relative_path
            );
            current.file_kind = Some(file.kind);
            if let FileContent::Symlink(target) = &file.content {
                current.symlink_target = Some(target.clone());
            }
        } else {
            ensure!(
                current.file_kind.is_none(),
                "file path is also used as a directory: {part}"
            );
        }
    }
    Ok(())
}

fn render_children(node: &TreeNode, prefix: &str, output: &mut String) {
    let mut children = node.children.iter().collect::<Vec<_>>();
    children.sort_unstable_by(|(left_name, left), (right_name, right)| {
        tree_sort_key(left_name, left).cmp(&tree_sort_key(right_name, right))
    });
    let last_index = children.len().saturating_sub(1);
    for (index, (name, child)) in children.into_iter().enumerate() {
        let last = index == last_index;
        output.push_str(prefix);
        output.push_str(if last { "└── " } else { "├── " });
        output.push_str(name);
        if child.file_kind.is_none() {
            output.push('/');
        } else if let Some(target) = child.symlink_target.as_deref() {
            output.push_str(" -> ");
            output.push_str(target);
        }
        output.push('\n');

        if child.file_kind.is_none() {
            let mut nested_prefix = prefix.to_owned();
            nested_prefix.push_str(if last { "    " } else { "│   " });
            render_children(child, &nested_prefix, output);
        }
    }
}

fn tree_sort_key<'a>(name: &'a str, node: &TreeNode) -> (u8, &'a str) {
    let lower = name.to_ascii_lowercase();
    let rank = if node.file_kind.is_some() {
        if lower == "readme" || lower.starts_with("readme.") {
            0
        } else if name.starts_with('.') {
            2
        } else {
            1
        }
    } else if name.starts_with('.') {
        4
    } else {
        3
    };
    (rank, name)
}

fn write_file(writer: &mut dyn Write, file: &ScannedFile) -> Result<()> {
    writer.write_all(SEPARATOR.as_bytes())?;
    writer.write_all(b"\n")?;
    let kind = match file.kind {
        FileKind::File => "FILE",
        FileKind::Symlink => "SYMLINK",
    };
    write!(writer, "{kind}: {}", file.relative_path)?;
    if let FileContent::Symlink(target) = &file.content {
        write!(writer, " -> {target}")?;
    }
    writer.write_all(b"\n")?;
    writer.write_all(SEPARATOR.as_bytes())?;
    writer.write_all(b"\n")?;
    match &file.content {
        FileContent::Text(text) => {
            writer.write_all(text.as_bytes())?;
            if !text.ends_with('\n') {
                writer.write_all(b"\n")?;
            }
        }
        FileContent::Empty => writer.write_all(b"[Empty file]\n")?,
        FileContent::Binary => writer.write_all(b"[Binary file omitted]\n")?,
        FileContent::Symlink(_) => writer.write_all(b"[Symlink content is not followed]\n")?,
    }
    writer.write_all(b"\n")?;
    Ok(())
}

fn estimate_file_tokens(file: &ScannedFile) -> usize {
    let header = match &file.content {
        FileContent::Symlink(target) => {
            format!(
                "{SEPARATOR}\nSYMLINK: {} -> {target}\n{SEPARATOR}\n",
                file.relative_path
            )
        }
        _ => format!("{SEPARATOR}\nFILE: {}\n{SEPARATOR}\n", file.relative_path),
    };
    let body = match &file.content {
        FileContent::Text(text) => text.as_str(),
        FileContent::Empty => "[Empty file]",
        FileContent::Binary => "[Binary file omitted]",
        FileContent::Symlink(_) => "[Symlink content is not followed]",
    };
    estimate_tokens(&header) + estimate_tokens(body)
}

fn estimate_tokens(text: &str) -> usize {
    let mut tokens = 0_usize;
    let mut word_length = 0_usize;
    for character in text.chars() {
        if character.is_alphanumeric() || character == '_' {
            word_length += 1;
            continue;
        }
        if word_length > 0 {
            tokens += word_length.div_ceil(4);
            word_length = 0;
        }
        if !character.is_whitespace() {
            tokens += 1;
        }
    }
    tokens + word_length.div_ceil(4)
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: &[(&str, u64)] = &[
        ("GiB", 1024 * 1024 * 1024),
        ("MiB", 1024 * 1024),
        ("KiB", 1024),
    ];
    for (unit, threshold) in UNITS {
        if bytes >= *threshold {
            return format!("{:.1} {unit}", bytes as f64 / *threshold as f64);
        }
    }
    format!("{bytes} B")
}

fn human_count(count: usize) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::ScanStats;

    fn file(path: &str, content: &str) -> ScannedFile {
        ScannedFile {
            relative_path: path.to_owned(),
            kind: FileKind::File,
            content: FileContent::Text(content.to_owned()),
            source_bytes: content.len() as u64,
        }
    }

    #[test]
    fn tree_orders_readme_then_files_then_directories() {
        let files = vec![
            file("src/lib.rs", "lib"),
            file("z.txt", "z"),
            file("README.md", "readme"),
        ];
        let tree = create_tree("sample", false, &files).unwrap();
        assert_eq!(
            tree,
            "Directory structure:\n└── sample/\n    ├── README.md\n    ├── z.txt\n    └── src/\n        └── lib.rs\n"
        );
    }

    #[test]
    fn output_has_unambiguous_file_boundaries_and_content() {
        let scan = ScanResult {
            root_name: "sample".to_owned(),
            root_is_file: false,
            files: vec![file("README.md", "hello")],
            stats: ScanStats {
                discovered: 1,
                included_bytes: 5,
                ..ScanStats::default()
            },
        };
        let digest = Digest::new(
            SourceMetadata {
                label: "sample".to_owned(),
                repository: None,
                revision: None,
                commit: None,
                subpath: None,
                submodule_count: 0,
                working_tree_dirty: None,
            },
            scan,
        )
        .unwrap();
        let mut output = Vec::new();
        digest.write_to(&mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(
            "FILE: README.md\n================================================\nhello\n"
        ));
        assert!(digest.estimated_tokens() > 0);
    }

    #[test]
    fn empty_directory_has_a_complete_tree_and_summary() {
        let scan = ScanResult {
            root_name: "empty".to_owned(),
            root_is_file: false,
            files: Vec::new(),
            stats: ScanStats::default(),
        };
        let digest = Digest::new(
            SourceMetadata {
                label: "empty".to_owned(),
                repository: None,
                revision: None,
                commit: None,
                subpath: None,
                submodule_count: 0,
                working_tree_dirty: None,
            },
            scan,
        )
        .unwrap();

        assert_eq!(digest.tree(), "Directory structure:\n└── empty/\n");
        assert!(digest.summary().contains("Files analyzed: 0"));
    }
}
