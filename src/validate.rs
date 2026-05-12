//! Validate LLM-emitted findings against the actual diff.
//!
//! The reviewer prompt asks the model to cite a path and a line number for
//! every finding. Even capable models will sometimes hallucinate a line that
//! isn't in the diff, or pick a number off by a few from the real one. This
//! module:
//!
//! 1. Indexes every file:line pair the model has been shown.
//! 2. Re-anchors a finding's line to the nearest real diff line when the
//!    cited line is close (±3).
//! 3. Drops findings whose line/file pair has no plausible match.
//!
//! Without this, a single bad line number makes a finding useless: the user
//! clicks through to the wrong code, distrust grows, and the recurring-pattern
//! detector logs phantom entries.

use std::collections::{HashMap, HashSet};

use crate::git::{DiffLine, ParsedDiff};
use crate::output::{Finding, Severity};

/// How far off a cited line can be from the nearest diff line before we drop
/// the finding entirely. Three lines is enough to absorb header-line off-by-one
/// errors but small enough that we don't silently re-anchor to unrelated code.
const REANCHOR_TOLERANCE: u32 = 3;

pub struct DiffIndex {
    /// Per-file sets of line numbers we can plausibly attribute findings to.
    /// We track Added and Context separately so re-anchoring can prefer real
    /// changes (Added) over surrounding code (Context).
    files: HashMap<String, FileLines>,
    /// Basename → full key map, used to short-circuit the linear suffix-match
    /// fallback when the model strips or adds a leading directory.
    basename_index: HashMap<String, String>,
}

#[derive(Default)]
struct FileLines {
    added: HashSet<u32>,
    context: HashSet<u32>,
    /// Original source text for every visible line, indexed by line number.
    /// Used to attach a code quote to each accepted finding so the user can
    /// see the cited code without leaving the terminal.
    content: HashMap<u32, String>,
}


impl DiffIndex {
    pub fn from_diff(diff: &ParsedDiff) -> Self {
        let mut files: HashMap<String, FileLines> = HashMap::new();
        let mut basename_index: HashMap<String, String> = HashMap::new();
        for file in &diff.files {
            let key = file.path.to_string_lossy().into_owned();
            if let Some(basename) = file
                .path
                .file_name()
                .and_then(|n| n.to_str())
            {
                // First entry wins on collision — rare, and the linear-suffix
                // fallback below still handles the duplicate-basename case.
                basename_index
                    .entry(basename.to_string())
                    .or_insert_with(|| key.clone());
            }
            let entry = files.entry(key).or_default();
            for hunk in &file.hunks {
                let mut line_num = hunk.new_start;
                for line in &hunk.lines {
                    match line {
                        DiffLine::Added(text) => {
                            entry.added.insert(line_num);
                            entry.content.insert(line_num, text.clone());
                            line_num += 1;
                        }
                        DiffLine::Context(text) => {
                            entry.context.insert(line_num);
                            entry.content.insert(line_num, text.clone());
                            line_num += 1;
                        }
                        DiffLine::Removed(_) => {
                            // Removed lines have no new-line number.
                        }
                    }
                }
            }
        }
        Self { files, basename_index }
    }

    /// Resolve a finding's file path to a stored `FileLines` entry. Tries the
    /// exact key first (zero-cost), then the basename map (O(1)), and only
    /// falls back to a linear suffix scan when both miss.
    fn lookup_file(&self, file: &str) -> Option<&FileLines> {
        if let Some(fl) = self.files.get(file) {
            return Some(fl);
        }
        if let Some(basename) = std::path::Path::new(file)
            .file_name()
            .and_then(|n| n.to_str())
        {
            if let Some(real_key) = self.basename_index.get(basename) {
                if let Some(fl) = self.files.get(real_key) {
                    return Some(fl);
                }
            }
        }
        self.files
            .iter()
            .find(|(k, _)| k.ends_with(file) || file.ends_with(k.as_str()))
            .map(|(_, v)| v)
    }

    /// Look up the source text for a (file, line) pair.
    fn line_content(&self, file: &str, line: u32) -> Option<String> {
        self.lookup_file(file).and_then(|fl| fl.content.get(&line).cloned())
    }

    /// Outcome of validating a single finding.
    pub fn validate(&self, finding: &Finding) -> Validation {
        // LGTM has no file/line and is always allowed.
        if finding.severity == Severity::Lgtm {
            return Validation::Accept { on_change: false };
        }

        // Findings without a file or line carry less weight but the prompt
        // explicitly allows them in some shapes — accept rather than drop.
        if finding.file.as_os_str().is_empty() {
            return Validation::Accept { on_change: false };
        }
        let Some(line) = finding.line else {
            return Validation::Accept { on_change: false };
        };

        let file_key = finding.file.to_string_lossy();
        let matches: Option<&FileLines> = self.lookup_file(file_key.as_ref());

        let Some(lines) = matches else {
            return Validation::Drop(DropReason::UnknownFile);
        };

        if lines.added.contains(&line) {
            return Validation::Accept { on_change: true };
        }
        if lines.context.contains(&line) {
            return Validation::Accept { on_change: false };
        }

        // Re-anchor preference: nearest Added line first (this is what the
        // change is actually about), then any visible line. Drop only when
        // both are out of tolerance.
        let nearest_added = lines.added.iter().min_by_key(|&&l| l.abs_diff(line)).copied();
        if let Some(real) = nearest_added {
            if real.abs_diff(line) <= REANCHOR_TOLERANCE {
                return Validation::Reanchor {
                    from: line,
                    to: real,
                    on_change: true,
                };
            }
        }
        let nearest_visible = lines
            .context
            .iter()
            .chain(lines.added.iter())
            .min_by_key(|&&l| l.abs_diff(line))
            .copied();
        match nearest_visible {
            Some(real) if real.abs_diff(line) <= REANCHOR_TOLERANCE => Validation::Reanchor {
                from: line,
                to: real,
                on_change: lines.added.contains(&real),
            },
            _ => Validation::Drop(DropReason::LineNotInDiff),
        }
    }

    /// Apply a [`Validation`] result, returning the possibly-mutated finding
    /// or [`None`] if it should be dropped.
    pub fn apply(&self, mut finding: Finding) -> (Option<Finding>, Validation) {
        let outcome = self.validate(&finding);
        match outcome {
            Validation::Accept { on_change } => {
                if let Some(line) = finding.line {
                    let key = finding.file.to_string_lossy().to_string();
                    finding.quote = self.line_content(&key, line);
                }
                (Some(finding), Validation::Accept { on_change })
            }
            Validation::Reanchor { from, to, on_change } => {
                finding.line = Some(to);
                let key = finding.file.to_string_lossy().to_string();
                finding.quote = self.line_content(&key, to);
                (Some(finding), Validation::Reanchor { from, to, on_change })
            }
            Validation::Drop(reason) => (None, Validation::Drop(reason)),
        }
    }

}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    UnknownFile,
    LineNotInDiff,
}

impl DropReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            DropReason::UnknownFile => "file not in diff",
            DropReason::LineNotInDiff => "line not in diff",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validation {
    Accept { on_change: bool },
    Reanchor { from: u32, to: u32, on_change: bool },
    Drop(DropReason),
}

