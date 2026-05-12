//! Self-critique pass over LLM-emitted findings.
//!
//! The reviewer prompt asks for high-precision output, but in practice every
//! model produces some noise — vague suggestions, duplicates of linter
//! findings, false positives that look plausible until you re-read them.
//! Running the findings back through the same model with a focused "is this
//! a real, specific, actionable finding?" prompt cheaply filters most of
//! that noise. It costs one extra completion per review, so it is opt-out
//! rather than mandatory; running it against a local Ollama model also has
//! a much lower latency penalty than the user might expect.

use anyhow::Result;

use crate::llm::LlmBackend;
use crate::output::{Finding, Severity};

const CRITIQUE_INSTRUCTIONS: &str = "\
You are reviewing a junior reviewer's findings. For each finding below, decide:

KEEP — the finding is specific, grounded in the shown code, actionable, and \
       not a stylistic nit a linter would catch.
DROP — the finding is vague, fabricated, generic best-practice advice, \
       a duplicate of another finding, or doesn't describe a real defect.

Output one line per finding in this exact form (nothing else):
N: KEEP — one-sentence justification
N: DROP — one-sentence reason

Be strict. If you cannot point to the specific shown code that makes the \
finding true, DROP it. If two findings describe the same bug, KEEP the more \
specific one and DROP the other.";

/// Result of a critique pass — kept findings plus the dropped pairs so the
/// caller can surface them in the run summary.
pub struct CritiqueResult {
    pub kept: Vec<Finding>,
    pub dropped: Vec<(Finding, String)>,
}

/// Run the critique against `findings`. Returns the original list unchanged
/// when there is nothing to critique (no findings, or LGTM-only).
pub async fn run_critique(
    findings: Vec<Finding>,
    backend: &dyn LlmBackend,
    original_prompt: &str,
) -> Result<CritiqueResult> {
    let reviewable: Vec<(usize, &Finding)> = findings
        .iter()
        .enumerate()
        .filter(|(_, f)| f.severity != Severity::Lgtm)
        .collect();

    if reviewable.is_empty() {
        return Ok(CritiqueResult { kept: findings, dropped: Vec::new() });
    }

    let prompt = build_prompt(original_prompt, &reviewable);
    // No streaming callback — we want the full response and don't want the
    // critique to mix with the live finding output.
    let response = backend.complete(&prompt, &(|_token: &str| {})).await?;
    let decisions = parse_decisions(&response, reviewable.len());

    let mut kept = Vec::with_capacity(findings.len());
    let mut dropped = Vec::new();

    for (decision_idx, (orig_idx, finding)) in reviewable.iter().enumerate() {
        let decision = decisions.get(decision_idx).cloned().unwrap_or(Decision::Keep {
            reason: String::from("no critique decision returned; kept by default"),
        });
        match decision {
            Decision::Keep { .. } => kept.push((*orig_idx, (*finding).clone())),
            Decision::Drop { reason } => dropped.push(((*finding).clone(), reason)),
        }
    }

    // Re-insert LGTM and other non-reviewable findings in their original spot
    // so the user-facing order is preserved.
    let mut by_original_index: std::collections::HashMap<usize, Finding> =
        kept.into_iter().collect();
    for (i, f) in findings.iter().enumerate() {
        if f.severity == Severity::Lgtm {
            by_original_index.insert(i, f.clone());
        }
    }
    let mut ordered: Vec<(usize, Finding)> = by_original_index.into_iter().collect();
    ordered.sort_by_key(|(i, _)| *i);

    Ok(CritiqueResult {
        kept: ordered.into_iter().map(|(_, f)| f).collect(),
        dropped,
    })
}

fn build_prompt(original_prompt: &str, reviewable: &[(usize, &Finding)]) -> String {
    let mut out = String::new();
    out.push_str(CRITIQUE_INSTRUCTIONS);
    out.push_str("\n\n=== ORIGINAL REVIEW INPUT ===\n");
    // Truncate the original prompt to keep the critique cheap. The diff and
    // context are what matter; we don't need the entire system prompt twice.
    let snippet_limit = 8000;
    let snippet = if original_prompt.len() > snippet_limit {
        // Try to keep the trailing portion that includes the actual diff,
        // which is appended after the static system prompt.
        &original_prompt[original_prompt.len() - snippet_limit..]
    } else {
        original_prompt
    };
    out.push_str(snippet);
    out.push_str("\n\n=== FINDINGS TO JUDGE ===\n");
    for (display_idx, (_orig, f)) in reviewable.iter().enumerate() {
        let line = f.line.map(|l| format!(":{}", l)).unwrap_or_default();
        out.push_str(&format!(
            "{}: [{}] {}{} — {}\n",
            display_idx + 1,
            f.severity.as_str().to_uppercase(),
            f.file.display(),
            line,
            f.message
        ));
    }
    out.push_str("\n=== YOUR JUDGEMENTS ===\n");
    out
}

#[derive(Debug, Clone)]
enum Decision {
    Keep { #[allow(dead_code)] reason: String },
    Drop { reason: String },
}

fn parse_decisions(response: &str, expected: usize) -> Vec<Decision> {
    let mut out: Vec<Option<Decision>> = vec![None; expected];
    for raw in response.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // Expected shape: "N: KEEP — reason" or "N: DROP — reason"
        let Some((num_part, rest)) = line.split_once(':') else {
            continue;
        };
        let Ok(idx) = num_part.trim().parse::<usize>() else {
            continue;
        };
        if idx == 0 || idx > expected {
            continue;
        }
        let rest = rest.trim_start();
        let (verdict, reason) = if let Some(r) = rest.strip_prefix("KEEP") {
            ("KEEP", r.trim_start_matches(['—', '-', ' ']).trim().to_string())
        } else if let Some(r) = rest.strip_prefix("DROP") {
            ("DROP", r.trim_start_matches(['—', '-', ' ']).trim().to_string())
        } else {
            continue;
        };
        out[idx - 1] = Some(match verdict {
            "KEEP" => Decision::Keep { reason },
            "DROP" => Decision::Drop { reason },
            _ => unreachable!(),
        });
    }
    // Anything unparsed defaults to KEEP so a half-broken critique response
    // never hides a finding the user should see.
    out.into_iter()
        .map(|d| {
            d.unwrap_or(Decision::Keep {
                reason: String::from("no decision parsed; kept by default"),
            })
        })
        .collect()
}
