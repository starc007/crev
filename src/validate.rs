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
use std::path::PathBuf;

use crate::git::{DiffLine, ParsedDiff};
use crate::output::{Finding, Severity};

/// How far off a cited line can be from the nearest diff line before we drop
/// the finding entirely. Three lines is enough to absorb header-line off-by-one
/// errors but small enough that we don't silently re-anchor to unrelated code.
const REANCHOR_TOLERANCE: u32 = 3;

pub struct DiffIndex {
    /// Per-file set of line numbers we can plausibly attribute findings to.
    /// Includes both Added lines and surrounding Context the model was shown.
    visible: HashMap<String, HashSet<u32>>,
}

impl DiffIndex {
    pub fn from_diff(diff: &ParsedDiff) -> Self {
        let mut visible: HashMap<String, HashSet<u32>> = HashMap::new();
        for file in &diff.files {
            let key = file.path.to_string_lossy().into_owned();
            let entry = visible.entry(key).or_default();
            for hunk in &file.hunks {
                let mut line_num = hunk.new_start;
                for line in &hunk.lines {
                    match line {
                        DiffLine::Added(_) | DiffLine::Context(_) => {
                            entry.insert(line_num);
                            line_num += 1;
                        }
                        DiffLine::Removed(_) => {
                            // Removed lines have no new-line number.
                        }
                    }
                }
            }
        }
        Self { visible }
    }

    /// Outcome of validating a single finding.
    pub fn validate(&self, finding: &Finding) -> Validation {
        // LGTM has no file/line and is always allowed.
        if finding.severity == Severity::Lgtm {
            return Validation::Accept;
        }

        // Findings without a file or line carry less weight but the prompt
        // explicitly allows them in some shapes — accept rather than drop.
        if finding.file.as_os_str().is_empty() {
            return Validation::Accept;
        }
        let Some(line) = finding.line else {
            return Validation::Accept;
        };

        let file_key = finding.file.to_string_lossy();
        let matches: Option<&HashSet<u32>> = self
            .visible
            .get(file_key.as_ref())
            // Fall back to a suffix match — the model sometimes prepends or
            // strips a leading directory we already showed it.
            .or_else(|| {
                self.visible
                    .iter()
                    .find(|(k, _)| {
                        k.ends_with(file_key.as_ref()) || file_key.ends_with(k.as_str())
                    })
                    .map(|(_, v)| v)
            });

        let Some(lines) = matches else {
            return Validation::Drop(DropReason::UnknownFile);
        };

        if lines.contains(&line) {
            return Validation::Accept;
        }

        // Try to re-anchor to the nearest real line.
        let nearest = lines
            .iter()
            .min_by_key(|&&l| l.abs_diff(line))
            .copied();
        match nearest {
            Some(real) if real.abs_diff(line) <= REANCHOR_TOLERANCE => {
                Validation::Reanchor { from: line, to: real }
            }
            _ => Validation::Drop(DropReason::LineNotInDiff),
        }
    }

    /// Apply a [`Validation`] result, returning the possibly-mutated finding
    /// or [`None`] if it should be dropped.
    pub fn apply(&self, mut finding: Finding) -> (Option<Finding>, Validation) {
        let outcome = self.validate(&finding);
        match outcome {
            Validation::Accept => (Some(finding), Validation::Accept),
            Validation::Reanchor { from, to } => {
                finding.line = Some(to);
                (Some(finding), Validation::Reanchor { from, to })
            }
            Validation::Drop(reason) => (None, Validation::Drop(reason)),
        }
    }

    /// Number of files indexed — used to suppress validation when we have no
    /// diff to validate against (defensive guard).
    pub fn is_empty(&self) -> bool {
        self.visible.is_empty()
    }

    /// Helper for [`crate::main`] flag wiring: should a finding's location
    /// be silently kept when validation can't decide?
    #[allow(dead_code)]
    pub fn known_files(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.visible.keys().map(PathBuf::from)
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
    Accept,
    Reanchor { from: u32, to: u32 },
    Drop(DropReason),
}
