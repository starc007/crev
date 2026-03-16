use crate::config::Config;
use crate::git::{DiffHunk, DiffLine, ParsedDiff};

const SYSTEM_INSTRUCTIONS: &str = "\
You are a senior engineer doing a focused code review.
Only report: bugs, security vulnerabilities, logic errors, missing error \
handling, race conditions, and performance issues.
Do NOT comment on: style, formatting, naming conventions, or anything \
a linter would catch.
If the change looks correct, say LGTM with one sentence of explanation.

Every finding must name the specific variable, function, or value involved. \
If you cannot name it specifically, do not output the finding.
Maximum 5 findings per review. If you find more, report only the 5 highest \
severity ones. Quality over quantity.

BAD:  [MED] src/main.rs:42 — Missing error handling
GOOD: [MED] src/main.rs:42 — db.execute() result is silently ignored; \
if the INSERT fails, the caller receives a success response \
while the data was never written";

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

pub fn build_review_prompt(diff: &ParsedDiff, config: &Config, security_mode: bool) -> String {
    let diff = if estimate_tokens(&format_diff(diff)) > config.review.max_tokens {
        truncate_to_budget(diff, config.review.max_tokens)
    } else {
        diff.clone()
    };

    let mut prompt = String::new();

    // 1. System instructions
    if security_mode {
        prompt.push_str(SECURITY_INSTRUCTIONS);
    } else {
        prompt.push_str(SYSTEM_INSTRUCTIONS);
    }
    prompt.push_str("\n\n");

    // 2. Changed code
    prompt.push_str("=== CHANGED CODE ===\n");
    prompt.push_str(&format_diff(&diff));
    prompt.push('\n');

    // 3. Output format
    prompt.push_str("=== OUTPUT FORMAT ===\n");
    prompt.push_str(OUTPUT_FORMAT);
    prompt.push('\n');

    // 4. Team rules (only if non-empty)
    if !config.rules.is_empty() {
        prompt.push_str("\n=== TEAM RULES ===\n");
        prompt.push_str("Also check for these team-specific rules:\n");
        for rule in &config.rules {
            prompt.push_str(&format!("- [{}]: {}\n", rule.name, rule.description));
        }
    }

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
