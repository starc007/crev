use crate::config::Config;
use crate::context::ReviewContext;
use crate::git::{DiffHunk, DiffLine, ParsedDiff};
use crate::linters::LinterFinding;
use crate::llm::PromptParts;

/// Owned counterpart to [`PromptParts`] — built here so each backend can
/// borrow the parts when it streams a completion. The split is chosen so that
/// Anthropic prompt caching reuses the stable `system` and `cacheable` blocks
/// across runs while only the `dynamic` block changes per review.
pub struct PromptPartsOwned {
    pub system: String,
    pub cacheable: String,
    pub dynamic: String,
}

impl PromptPartsOwned {
    pub fn as_parts(&self) -> PromptParts<'_> {
        PromptParts {
            system: &self.system,
            cacheable: &self.cacheable,
            dynamic: &self.dynamic,
        }
    }

    pub fn to_combined(&self) -> String {
        format!("{}\n\n{}\n\n{}", self.system, self.cacheable, self.dynamic)
    }
}

const SYSTEM_INSTRUCTIONS: &str = "\
You are a senior engineer doing a focused code review of a diff.
Only report: bugs, security vulnerabilities, logic errors, missing error \
handling, race conditions, and performance issues.
Do NOT comment on: style, formatting, naming conventions, or anything \
a linter would catch.
If the change looks correct, say LGTM with one sentence of explanation.

GROUNDING RULES — failures here make findings worse than useless:
1. Only cite line numbers that appear in the diff you are shown. Each diff \
   line is prefixed with its line number; never invent a number.
2. Only name functions, variables, or symbols that appear in the diff or in \
   the context blocks below. Do not refer to code you have not seen.
3. Findings must describe the ADDED code, not unchanged context. Context \
   lines are shown for orientation only.
4. If you cannot anchor a concern to a specific shown line, omit it — silence \
   is better than a hallucinated citation.

SEVERITY RUBRIC:
- [HIGH]: data loss, auth bypass, RCE, financial bug, panic on user input, \
  resource exhaustion, race condition that corrupts shared state.
- [MED]: correctness bug on a non-critical path, silently swallowed error, \
  obvious performance regression on a hot path, missing input validation.
- [LOW]: real but minor concern (e.g. off-by-one in a debug-only path, \
  defensive check that helps future readers).

For performance findings: only report if you can identify a specific hot path \
where the cost is significant AND avoidable given the surrounding constraints. \
Do not flag allocations or copies that are structurally required by the \
language, the API contract, or the calling convention.

Every finding must name the specific variable, function, or value involved. \
If you cannot name it specifically, do not output the finding.
Maximum 5 findings per review. If you find more, report only the 5 highest \
severity ones. Quality over quantity.

BAD:  [MED] src/main.rs:42 — Missing error handling
GOOD: [MED] src/main.rs:42 — db.execute() result is silently ignored; \
if the INSERT fails, the caller receives a success response \
while the data was never written

BAD:  [MED] src/main.rs:99 — Unnecessary allocation on this line
GOOD: [MED] src/main.rs:99 — buffer is re-allocated inside the loop on every \
iteration; moving the allocation before the loop would reduce it to once

BAD (line not in diff, fabricated):
  [HIGH] src/auth.rs:999 — token comparison is timing-unsafe
GOOD (concern is real but you cannot point to a shown line):
  <omit the finding rather than invent a line number>";

const SECURITY_INSTRUCTIONS: &str = "\
You are a security engineer doing a targeted vulnerability review.
Focus EXCLUSIVELY on:
- Injection vulnerabilities (SQL, command, path traversal)
- Authentication/authorization bypasses
- Integer overflow/underflow in financial or size calculations
- Unvalidated external input used in sensitive operations
- Secrets or credentials appearing in code
- Insecure deserialization
Ignore all non-security issues.";

const OUTPUT_FORMAT: &str = "\
Respond with one finding per line in this exact format:
[HIGH] path/to/file.rs:42 — description of the specific issue
[MED]  path/to/file.rs:67 — description
[LOW]  path/to/file.rs:88 — description
LGTM: brief note if no issues found.
Every finding must name the specific variable, function, or value involved.
Do not output vague findings like 'add error handling' without specifics.";

/// Build a [`PromptPartsOwned`] from a full review context.
///
/// Cache split:
///   - `system`:     the static reviewer instructions (severity rubric,
///                   grounding rules, few-shot examples). Never varies.
///   - `cacheable`:  team rules + output-format trailer. Stable per repo
///                   across many runs, so still worth caching even though
///                   `dynamic` always invalidates the suffix.
///   - `dynamic`:    diff, called-fn bodies, type defs, related tests,
///                   linter findings. Changes every review.
pub fn build_review_prompt_parts_ctx(
    ctx: &ReviewContext,
    config: &Config,
    security_mode: bool,
    linter_findings: &[LinterFinding],
) -> PromptPartsOwned {
    let system = if security_mode { SECURITY_INSTRUCTIONS } else { SYSTEM_INSTRUCTIONS }.to_string();

    let mut cacheable = String::new();
    if !config.rules.is_empty() {
        cacheable.push_str("=== TEAM RULES ===\n");
        cacheable.push_str("Also check for these team-specific rules:\n");
        for rule in &config.rules {
            cacheable.push_str(&format!("- [{}]: {}\n", rule.name, rule.description));
        }
        cacheable.push('\n');
    }
    cacheable.push_str("=== OUTPUT FORMAT ===\n");
    cacheable.push_str(OUTPUT_FORMAT);
    cacheable.push('\n');

    let mut dynamic = String::new();
    dynamic.push_str("=== CHANGED CODE ===\n");
    dynamic.push_str(&format_diff(&ctx.diff));
    dynamic.push('\n');

    if !ctx.called_functions.is_empty() {
        dynamic.push_str("=== FUNCTIONS CALLED BY CHANGED CODE ===\n");
        for f in &ctx.called_functions {
            dynamic.push_str(&f.full_text);
            dynamic.push_str("\n\n");
        }
    }

    if !ctx.types_used.is_empty() {
        dynamic.push_str("=== TYPES USED ===\n");
        for t in &ctx.types_used {
            dynamic.push_str(&format!(
                "{} {} {{ {} }}\n",
                format!("{:?}", t.kind).to_lowercase(),
                t.name,
                t.fields.join(", ")
            ));
        }
        dynamic.push('\n');
    }

    if !ctx.test_functions.is_empty() {
        dynamic.push_str("=== RELATED TESTS ===\n");
        for f in &ctx.test_functions {
            dynamic.push_str(&f.full_text);
            dynamic.push_str("\n\n");
        }
    }

    if !linter_findings.is_empty() {
        dynamic.push_str("=== LINTER FINDINGS ===\n");
        for f in linter_findings {
            dynamic.push_str(&format!(
                "{} at {}:{}\n",
                f.code,
                f.file.display(),
                f.line
            ));
        }
        dynamic.push_str(
            "For each linter finding above, assess: is this a genuine risk or a \
             false positive given the context? Explain the actual consequence if real.\n\n",
        );
    }

    PromptPartsOwned { system, cacheable, dynamic }
}

/// Build a prompt from a full ReviewContext (Phase 2+) with optional linter findings.
pub fn build_review_prompt_ctx(
    ctx: &ReviewContext,
    config: &Config,
    security_mode: bool,
    linter_findings: &[LinterFinding],
) -> String {
    let mut prompt = String::new();

    // 1. System instructions
    prompt.push_str(if security_mode { SECURITY_INSTRUCTIONS } else { SYSTEM_INSTRUCTIONS });
    prompt.push_str("\n\n");

    // 2. Changed code
    prompt.push_str("=== CHANGED CODE ===\n");
    prompt.push_str(&format_diff(&ctx.diff));
    prompt.push('\n');

    // 3. Called function bodies
    if !ctx.called_functions.is_empty() {
        prompt.push_str("=== FUNCTIONS CALLED BY CHANGED CODE ===\n");
        for f in &ctx.called_functions {
            prompt.push_str(&f.full_text);
            prompt.push_str("\n\n");
        }
    }

    // 4. Types used
    if !ctx.types_used.is_empty() {
        prompt.push_str("=== TYPES USED ===\n");
        for t in &ctx.types_used {
            prompt.push_str(&format!(
                "{} {} {{ {} }}\n",
                format!("{:?}", t.kind).to_lowercase(),
                t.name,
                t.fields.join(", ")
            ));
        }
        prompt.push('\n');
    }

    // 5. Related tests
    if !ctx.test_functions.is_empty() {
        prompt.push_str("=== RELATED TESTS ===\n");
        for f in &ctx.test_functions {
            prompt.push_str(&f.full_text);
            prompt.push_str("\n\n");
        }
    }

    // 6. Linter findings
    if !linter_findings.is_empty() {
        prompt.push_str("=== LINTER FINDINGS ===\n");
        for f in linter_findings {
            prompt.push_str(&format!(
                "{} at {}:{}\n",
                f.code,
                f.file.display(),
                f.line
            ));
        }
        prompt.push_str(
            "For each linter finding above, assess: is this a genuine risk or a \
             false positive given the context? Explain the actual consequence if real.\n\n",
        );
    }

    // 7. Team rules
    if !config.rules.is_empty() {
        prompt.push_str("=== TEAM RULES ===\n");
        prompt.push_str("Also check for these team-specific rules:\n");
        for rule in &config.rules {
            prompt.push_str(&format!("- [{}]: {}\n", rule.name, rule.description));
        }
        prompt.push('\n');
    }

    // 8. Output format (always last)
    prompt.push_str("=== OUTPUT FORMAT ===\n");
    prompt.push_str(OUTPUT_FORMAT);
    prompt.push('\n');

    prompt
}

/// Fallback: build a prompt from a raw diff only (Phase 1 behaviour).
pub fn build_review_prompt(diff: &ParsedDiff, config: &Config, security_mode: bool) -> String {
    let diff = if estimate_tokens(&format_diff(diff)) > config.review.max_tokens {
        let original_files: Vec<std::path::PathBuf> =
            diff.files.iter().map(|f| f.path.clone()).collect();
        let trimmed = truncate_to_budget(diff, config.review.max_tokens);
        let kept: std::collections::HashSet<_> =
            trimmed.files.iter().map(|f| f.path.clone()).collect();
        let dropped: Vec<_> = original_files
            .iter()
            .filter(|p| !kept.contains(*p))
            .collect();
        if !dropped.is_empty() {
            eprintln!(
                "warning: diff exceeded token budget; {} file(s) dropped from review:",
                dropped.len()
            );
            for p in &dropped {
                eprintln!("  - {}", p.display());
            }
        }
        trimmed
    } else {
        diff.clone()
    };

    let mut prompt = String::new();

    prompt.push_str(if security_mode { SECURITY_INSTRUCTIONS } else { SYSTEM_INSTRUCTIONS });
    prompt.push_str("\n\n");

    prompt.push_str("=== CHANGED CODE ===\n");
    prompt.push_str(&format_diff(&diff));
    prompt.push('\n');

    if !config.rules.is_empty() {
        prompt.push_str("\n=== TEAM RULES ===\n");
        for rule in &config.rules {
            prompt.push_str(&format!("- [{}]: {}\n", rule.name, rule.description));
        }
    }

    prompt.push_str("=== OUTPUT FORMAT ===\n");
    prompt.push_str(OUTPUT_FORMAT);
    prompt.push('\n');

    prompt
}

fn format_diff(diff: &ParsedDiff) -> String {
    let mut out = String::new();

    for file in &diff.files {
        out.push_str(&format!("=== FILE: {} ===\n", file.path.display()));

        for hunk in &file.hunks {
            out.push_str(&format_hunk(hunk));
        }

        out.push('\n');
    }

    out
}

fn format_hunk(hunk: &DiffHunk) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
    ));

    let mut line_num = hunk.new_start;

    for line in &hunk.lines {
        match line {
            DiffLine::Added(s) => {
                out.push_str(&format!("{:4} + {}\n", line_num, s));
                line_num += 1;
            }
            DiffLine::Removed(s) => {
                out.push_str(&format!("     - {}\n", s));
            }
            DiffLine::Context(s) => {
                out.push_str(&format!("{:4}   {}\n", line_num, s));
                line_num += 1;
            }
        }
    }

    out
}

pub fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}

/// Split a diff into chunks that each fit inside `max_tokens` worth of
/// rendered output. Files larger than the budget on their own end up alone
/// in a chunk; the truncator is the last line of defense for those.
///
/// This is the cheap alternative to truncation: instead of dropping the
/// largest files when a diff blows the token budget, we run several review
/// passes and merge their findings.
pub fn chunk_diff_by_files(diff: &ParsedDiff, max_tokens: usize) -> Vec<ParsedDiff> {
    use crate::git::DiffStats;

    let budget_chars = max_tokens.saturating_mul(4);
    if format_diff(diff).len() <= budget_chars || diff.files.len() <= 1 {
        return vec![diff.clone()];
    }

    let mut chunks: Vec<Vec<crate::git::ChangedFile>> = Vec::new();
    let mut current: Vec<crate::git::ChangedFile> = Vec::new();
    let mut current_chars: usize = 0;

    for file in &diff.files {
        let single = format_one_file_diff(file);
        let file_chars = single.len();

        if file_chars > budget_chars {
            // The file alone exceeds budget. Flush current chunk, then take
            // this file as its own chunk — the existing truncator will trim
            // its context lines further at prompt-build time.
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
                current_chars = 0;
            }
            chunks.push(vec![file.clone()]);
            continue;
        }

        if current_chars + file_chars > budget_chars && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        current.push(file.clone());
        current_chars += file_chars;
    }
    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
        .into_iter()
        .map(|files| {
            let lines_added: usize = files
                .iter()
                .flat_map(|f| f.hunks.iter())
                .flat_map(|h| h.lines.iter())
                .filter(|l| matches!(l, DiffLine::Added(_)))
                .count();
            let lines_removed: usize = files
                .iter()
                .flat_map(|f| f.hunks.iter())
                .flat_map(|h| h.lines.iter())
                .filter(|l| matches!(l, DiffLine::Removed(_)))
                .count();
            let files_changed = files.len();
            ParsedDiff {
                files,
                stats: DiffStats { lines_added, lines_removed, files_changed },
            }
        })
        .collect()
}

fn format_one_file_diff(file: &crate::git::ChangedFile) -> String {
    let mut out = String::new();
    out.push_str(&format!("=== FILE: {} ===\n", file.path.display()));
    for hunk in &file.hunks {
        out.push_str(&format_hunk(hunk));
    }
    out.push('\n');
    out
}

pub fn truncate_to_budget(diff: &ParsedDiff, max_tokens: usize) -> ParsedDiff {
    use crate::git::{ChangedFile, DiffHunk, DiffStats};

    let budget_chars = max_tokens * 4;

    // First pass: try trimming context lines
    let mut trimmed_files: Vec<ChangedFile> = diff
        .files
        .iter()
        .map(|f| {
            let trimmed_hunks: Vec<DiffHunk> = f
                .hunks
                .iter()
                .map(|h| {
                    // Keep only added/removed lines + 3 context lines on each side
                    let mut new_lines = Vec::new();
                    let mut leading_context = 0;
                    let lines_rev: Vec<_> = h.lines.iter().rev().collect();
                    let trailing_skip = lines_rev
                        .iter()
                        .take_while(|l| matches!(l, DiffLine::Context(_)))
                        .count();

                    for (i, line) in h.lines.iter().enumerate() {
                        let from_end = h.lines.len() - 1 - i;
                        match line {
                            DiffLine::Context(_) => {
                                if leading_context < 3 && !new_lines.is_empty() {
                                    new_lines.push(line.clone());
                                } else if new_lines.is_empty() && leading_context < 3 {
                                    leading_context += 1;
                                    new_lines.push(line.clone());
                                } else if from_end < trailing_skip && from_end < 3 {
                                    new_lines.push(line.clone());
                                }
                            }
                            _ => {
                                leading_context = 0;
                                new_lines.push(line.clone());
                            }
                        }
                    }

                    DiffHunk {
                        old_start: h.old_start,
                        old_lines: h.old_lines,
                        new_start: h.new_start,
                        new_lines: h.new_lines,
                        lines: new_lines,
                    }
                })
                .collect();

            ChangedFile {
                path: f.path.clone(),
                hunks: trimmed_hunks,
                file_type: f.file_type.clone(),
            }
        })
        .collect();

    // Check if trimming context was enough
    let trimmed_text = format_diff(&ParsedDiff {
        files: trimmed_files.clone(),
        stats: diff.stats.clone(),
    });

    if trimmed_text.len() <= budget_chars {
        let files_changed = trimmed_files.len();
        return ParsedDiff {
            files: trimmed_files,
            stats: DiffStats {
                files_changed,
                ..diff.stats.clone()
            },
        };
    }

    // Second pass: sort files by size descending and drop largest until under budget
    trimmed_files.sort_by_key(|f| {
        let size: usize = f
            .hunks
            .iter()
            .map(|h| h.lines.iter().map(|l| line_content(l).len() + 10).sum::<usize>())
            .sum();
        std::cmp::Reverse(size)
    });

    while !trimmed_files.is_empty() {
        let check = ParsedDiff {
            files: trimmed_files.clone(),
            stats: diff.stats.clone(),
        };
        if format_diff(&check).len() <= budget_chars {
            break;
        }
        trimmed_files.remove(0);
    }

    let files_changed = trimmed_files.len();
    ParsedDiff {
        files: trimmed_files,
        stats: DiffStats {
            files_changed,
            ..diff.stats.clone()
        },
    }
}

fn line_content(line: &DiffLine) -> &str {
    match line {
        DiffLine::Added(s) | DiffLine::Removed(s) | DiffLine::Context(s) => s.as_str(),
    }
}
