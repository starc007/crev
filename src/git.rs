use anyhow::{Context, Result};
use git2::{DiffOptions, Repository};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ParsedDiff {
    pub files: Vec<ChangedFile>,
    pub stats: DiffStats,
}

#[derive(Debug, Clone)]
pub struct DiffStats {
    pub lines_added: usize,
    pub lines_removed: usize,
    pub files_changed: usize,
}

#[derive(Debug, Clone)]
pub struct ChangedFile {
    pub path: PathBuf,
    pub hunks: Vec<DiffHunk>,
    pub file_type: FileType,
}

#[derive(Debug, Clone)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
pub enum DiffLine {
    Added(String),
    Removed(String),
    Context(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum FileType {
    Rust,
    TypeScript,
    Python,
    Go,
    JavaScript,
    Other(String),
}

impl FileType {
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some("rs") => FileType::Rust,
            Some("ts") | Some("tsx") => FileType::TypeScript,
            Some("py") => FileType::Python,
            Some("go") => FileType::Go,
            Some("js") | Some("jsx") | Some("mjs") => FileType::JavaScript,
            Some(ext) => FileType::Other(ext.to_string()),
            None => FileType::Other(String::new()),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            FileType::Rust => "rust",
            FileType::TypeScript => "typescript",
            FileType::Python => "python",
            FileType::Go => "go",
            FileType::JavaScript => "javascript",
            FileType::Other(s) => s.as_str(),
        }
    }
}

const CONTEXT_LINES: u32 = 10;

pub fn get_staged_diff(repo_path: &Path) -> Result<ParsedDiff> {
    let repo = Repository::discover(repo_path).context("Failed to open git repository")?;
    let index = repo.index().context("Failed to get git index")?;

    let head_tree = match repo.head() {
        Ok(head) => {
            let commit = head.peel_to_commit()?;
            Some(commit.tree()?)
        }
        Err(_) => None, // no commits yet
    };

    let mut opts = DiffOptions::new();
    opts.context_lines(CONTEXT_LINES);
    opts.include_untracked(false);

    let diff = repo
        .diff_tree_to_index(head_tree.as_ref(), Some(&index), Some(&mut opts))
        .context("Failed to get staged diff")?;

    parse_diff(diff)
}

pub fn get_unstaged_diff(repo_path: &Path) -> Result<ParsedDiff> {
    let repo = Repository::discover(repo_path).context("Failed to open git repository")?;

    let mut opts = DiffOptions::new();
    opts.context_lines(CONTEXT_LINES);

    let diff = repo
        .diff_index_to_workdir(None, Some(&mut opts))
        .context("Failed to get unstaged diff")?;

    parse_diff(diff)
}

pub fn get_commit_diff(repo_path: &Path, commit_hash: &str) -> Result<ParsedDiff> {
    let repo = Repository::discover(repo_path).context("Failed to open git repository")?;

    let oid = repo
        .revparse_single(commit_hash)
        .context("Failed to resolve commit")?
        .id();

    let commit = repo.find_commit(oid).context("Failed to find commit")?;
    let tree = commit.tree()?;

    let parent_tree = if commit.parent_count() > 0 {
        Some(commit.parent(0)?.tree()?)
    } else {
        None
    };

    let mut opts = DiffOptions::new();
    opts.context_lines(CONTEXT_LINES);

    let diff = repo
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))
        .context("Failed to get commit diff")?;

    parse_diff(diff)
}

pub fn get_range_diff(repo_path: &Path, from: &str, to: &str) -> Result<ParsedDiff> {
    let repo = Repository::discover(repo_path).context("Failed to open git repository")?;

    let from_obj = repo
        .revparse_single(from)
        .with_context(|| format!("Failed to resolve ref: {}", from))?;
    let to_obj = repo
        .revparse_single(to)
        .with_context(|| format!("Failed to resolve ref: {}", to))?;

    let from_commit = from_obj.peel_to_commit()?;
    let to_commit = to_obj.peel_to_commit()?;

    let from_tree = from_commit.tree()?;
    let to_tree = to_commit.tree()?;

    let mut opts = DiffOptions::new();
    opts.context_lines(CONTEXT_LINES);

    let diff = repo
        .diff_tree_to_tree(Some(&from_tree), Some(&to_tree), Some(&mut opts))
        .context("Failed to get range diff")?;

    parse_diff(diff)
}

fn parse_diff(diff: git2::Diff) -> Result<ParsedDiff> {
    use std::cell::RefCell;
    use std::collections::HashMap;

    struct FileData {
        path: PathBuf,
        hunks: Vec<DiffHunk>,
        current_hunk: Option<DiffHunk>,
    }

    let files_map: RefCell<HashMap<PathBuf, FileData>> = RefCell::new(HashMap::new());
    let file_order: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
    let lines_added: RefCell<usize> = RefCell::new(0);
    let lines_removed: RefCell<usize> = RefCell::new(0);

    diff.foreach(
        &mut |delta, _progress| {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(PathBuf::from)
                .unwrap_or_default();

            let mut map = files_map.borrow_mut();
            if !map.contains_key(&path) {
                file_order.borrow_mut().push(path.clone());
                map.insert(
                    path.clone(),
                    FileData {
                        path: path.clone(),
                        hunks: Vec::new(),
                        current_hunk: None,
                    },
                );
            }
            true
        },
        None,
        Some(&mut |delta, hunk| {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(PathBuf::from)
                .unwrap_or_default();

            let mut map = files_map.borrow_mut();
            if let Some(fd) = map.get_mut(&path) {
                if let Some(h) = fd.current_hunk.take() {
                    fd.hunks.push(h);
                }
                fd.current_hunk = Some(DiffHunk {
                    old_start: hunk.old_start(),
                    old_lines: hunk.old_lines(),
                    new_start: hunk.new_start(),
                    new_lines: hunk.new_lines(),
                    lines: Vec::new(),
                });
            }
            true
        }),
        Some(&mut |delta, _hunk, line| {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(PathBuf::from)
                .unwrap_or_default();

            let content = String::from_utf8_lossy(line.content()).into_owned();
            let content = content.trim_end_matches('\n').to_string();

            let diff_line = match line.origin() {
                '+' => {
                    *lines_added.borrow_mut() += 1;
                    DiffLine::Added(content)
                }
                '-' => {
                    *lines_removed.borrow_mut() += 1;
                    DiffLine::Removed(content)
                }
                _ => DiffLine::Context(content),
            };

            let mut map = files_map.borrow_mut();
            if let Some(fd) = map.get_mut(&path) {
                if let Some(hunk) = fd.current_hunk.as_mut() {
                    hunk.lines.push(diff_line);
                }
            }
            true
        }),
    )?;

    let lines_added = lines_added.into_inner();
    let lines_removed = lines_removed.into_inner();
    let file_order = file_order.into_inner();
    let mut files_map = files_map.into_inner();

    // Flush last hunks
    let mut files: Vec<ChangedFile> = file_order
        .into_iter()
        .filter_map(|path| {
            files_map.remove(&path).map(|mut fd| {
                if let Some(h) = fd.current_hunk.take() {
                    fd.hunks.push(h);
                }
                let file_type = FileType::from_path(&fd.path);
                ChangedFile {
                    path: fd.path,
                    hunks: fd.hunks,
                    file_type,
                }
            })
        })
        .collect();

    // Remove files with no hunks
    files.retain(|f| !f.hunks.is_empty());

    let files_changed = files.len();

    Ok(ParsedDiff {
        stats: DiffStats {
            lines_added,
            lines_removed,
            files_changed,
        },
        files,
    })
}

pub fn find_repo_root(start: &Path) -> Result<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            anyhow::bail!("Not inside a git repository");
        }
    }
}
