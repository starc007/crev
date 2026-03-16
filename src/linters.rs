use anyhow::Result;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use crate::git::{FileType, ParsedDiff};

#[derive(Debug, Clone)]
pub struct LinterFinding {
    pub linter: String,
    pub file: PathBuf,
    pub line: u32,
    pub code: String,
    pub message: String,
}

pub async fn run_linters(diff: &ParsedDiff, repo_root: &Path) -> Vec<LinterFinding> {
    // Detect which linters are needed based on file types in the diff
    let has_rust = diff.files.iter().any(|f| f.file_type == FileType::Rust);
    let has_ts = diff
        .files
        .iter()
        .any(|f| matches!(f.file_type, FileType::TypeScript | FileType::JavaScript));
    let has_py = diff.files.iter().any(|f| f.file_type == FileType::Python);
    let has_go = diff.files.iter().any(|f| f.file_type == FileType::Go);

    let mut handles = Vec::new();

    if has_rust && which("cargo").is_some() {
        let root = repo_root.to_path_buf();
        handles.push(tokio::spawn(async move { run_clippy(&root).await }));
    }

    if has_ts && which("eslint").is_some() {
        let root = repo_root.to_path_buf();
        handles.push(tokio::spawn(async move { run_eslint(&root).await }));
    }

    if has_py && which("ruff").is_some() {
        let root = repo_root.to_path_buf();
        handles.push(tokio::spawn(async move { run_ruff(&root).await }));
    }

    if has_go && which("golangci-lint").is_some() {
        let root = repo_root.to_path_buf();
        handles.push(tokio::spawn(async move { run_golangci(&root).await }));
    }

    // semgrep: run if config exists regardless of language
    let semgrep_config = repo_root.join(".semgrep.yml");
    let semgrep_dir = repo_root.join(".semgrep");
    if (semgrep_config.exists() || semgrep_dir.exists()) && which("semgrep").is_some() {
        let root = repo_root.to_path_buf();
        handles.push(tokio::spawn(async move { run_semgrep(&root).await }));
    }

    let mut all: Vec<LinterFinding> = Vec::new();
    for h in handles {
        match h.await {
            Ok(Ok(mut findings)) => all.append(&mut findings),
            Ok(Err(e)) => eprintln!("warning: linter error: {}", e),
            Err(e) => eprintln!("warning: linter task failed: {}", e),
        }
    }

    // Filter: only include findings that overlap with diff (±5 lines of a changed line)
    filter_to_diff(&all, diff)
}

fn filter_to_diff(findings: &[LinterFinding], diff: &ParsedDiff) -> Vec<LinterFinding> {
    findings
        .iter()
        .filter(|f| {
            diff.files.iter().any(|df| {
                // Normalise both paths for comparison
                let df_path = df.path.to_string_lossy();
                let f_path = f.file.to_string_lossy();
                let paths_match = f_path.ends_with(df_path.as_ref())
                    || df_path.ends_with(f_path.as_ref())
                    || f_path == df_path;

                if !paths_match {
                    return false;
                }

                df.hunks.iter().any(|h| {
                    let changed_lines: Vec<u32> = (h.new_start..h.new_start + h.new_lines).collect();
                    changed_lines
                        .iter()
                        .any(|&l| l.saturating_sub(5) <= f.line && f.line <= l + 5)
                })
            })
        })
        .cloned()
        .collect()
}

// ── Clippy ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ClippyMessage {
    reason: String,
    message: Option<ClippyDiagnostic>,
}

#[derive(Deserialize)]
struct ClippyDiagnostic {
    code: Option<ClippyCode>,
    message: String,
    spans: Vec<ClippySpan>,
}

#[derive(Deserialize)]
struct ClippyCode {
    code: String,
}

#[derive(Deserialize)]
struct ClippySpan {
    file_name: String,
    line_start: u32,
    is_primary: bool,
}

async fn run_clippy(repo_root: &Path) -> Result<Vec<LinterFinding>> {
    let output = Command::new("cargo")
        .args(["clippy", "--message-format=json", "--", "-D", "warnings"])
        .current_dir(repo_root)
        .output()
        .await?;

    let mut findings = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(msg) = serde_json::from_str::<ClippyMessage>(line) else {
            continue;
        };
        if msg.reason != "compiler-message" {
            continue;
        }
        let Some(diag) = msg.message else { continue };
        let primary = diag.spans.iter().find(|s| s.is_primary);
        let Some(span) = primary else { continue };

        findings.push(LinterFinding {
            linter: "clippy".to_string(),
            file: PathBuf::from(&span.file_name),
            line: span.line_start,
            code: diag
                .code
                .map(|c| format!("clippy::{}", c.code))
                .unwrap_or_default(),
            message: diag.message,
        });
    }

    Ok(findings)
}

// ── ESLint ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct EslintFile {
    #[serde(rename = "filePath")]
    file_path: String,
    messages: Vec<EslintMessage>,
}

#[derive(Deserialize)]
struct EslintMessage {
    line: u32,
    #[serde(rename = "ruleId")]
    rule_id: Option<String>,
    message: String,
}

async fn run_eslint(repo_root: &Path) -> Result<Vec<LinterFinding>> {
    let output = Command::new("eslint")
        .args([".", "--format=json", "--ext", ".ts,.tsx,.js,.jsx"])
        .current_dir(repo_root)
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<EslintFile> = serde_json::from_str(&stdout).unwrap_or_default();

    let findings = files
        .into_iter()
        .flat_map(|f| {
            f.messages.into_iter().map(move |m| LinterFinding {
                linter: "eslint".to_string(),
                file: PathBuf::from(&f.file_path),
                line: m.line,
                code: m.rule_id.unwrap_or_default(),
                message: m.message,
            })
        })
        .collect();

    Ok(findings)
}

// ── Ruff ──────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RuffFinding {
    filename: String,
    location: RuffLocation,
    code: String,
    message: String,
}

#[derive(Deserialize)]
struct RuffLocation {
    row: u32,
}

async fn run_ruff(repo_root: &Path) -> Result<Vec<LinterFinding>> {
    let output = Command::new("ruff")
        .args(["check", ".", "--output-format=json"])
        .current_dir(repo_root)
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let raw: Vec<RuffFinding> = serde_json::from_str(&stdout).unwrap_or_default();

    Ok(raw
        .into_iter()
        .map(|r| LinterFinding {
            linter: "ruff".to_string(),
            file: PathBuf::from(&r.filename),
            line: r.location.row,
            code: r.code,
            message: r.message,
        })
        .collect())
}

// ── golangci-lint ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct GolangciOutput {
    #[serde(rename = "Issues")]
    issues: Option<Vec<GolangciIssue>>,
}

#[derive(Deserialize)]
struct GolangciIssue {
    #[serde(rename = "Text")]
    text: String,
    #[serde(rename = "FromLinter")]
    from_linter: String,
    #[serde(rename = "Pos")]
    pos: GolangciPos,
}

#[derive(Deserialize)]
struct GolangciPos {
    #[serde(rename = "Filename")]
    filename: String,
    #[serde(rename = "Line")]
    line: u32,
}

async fn run_golangci(repo_root: &Path) -> Result<Vec<LinterFinding>> {
    let output = Command::new("golangci-lint")
        .args(["run", "--out-format=json"])
        .current_dir(repo_root)
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: GolangciOutput = serde_json::from_str(&stdout).unwrap_or(GolangciOutput {
        issues: None,
    });

    Ok(parsed
        .issues
        .unwrap_or_default()
        .into_iter()
        .map(|i| LinterFinding {
            linter: "golangci-lint".to_string(),
            file: PathBuf::from(&i.pos.filename),
            line: i.pos.line,
            code: i.from_linter,
            message: i.text,
        })
        .collect())
}

// ── Semgrep ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SemgrepOutput {
    results: Vec<SemgrepResult>,
}

#[derive(Deserialize)]
struct SemgrepResult {
    path: String,
    start: SemgrepPos,
    #[serde(rename = "check_id")]
    check_id: String,
    extra: SemgrepExtra,
}

#[derive(Deserialize)]
struct SemgrepPos {
    line: u32,
}

#[derive(Deserialize)]
struct SemgrepExtra {
    message: String,
}

async fn run_semgrep(repo_root: &Path) -> Result<Vec<LinterFinding>> {
    let output = Command::new("semgrep")
        .args(["--json", "."])
        .current_dir(repo_root)
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: SemgrepOutput = serde_json::from_str(&stdout).unwrap_or(SemgrepOutput {
        results: Vec::new(),
    });

    Ok(parsed
        .results
        .into_iter()
        .map(|r| LinterFinding {
            linter: "semgrep".to_string(),
            file: PathBuf::from(&r.path),
            line: r.start.line,
            code: r.check_id,
            message: r.extra.message,
        })
        .collect())
}

// ── utility ───────────────────────────────────────────────────────────────────

fn which(binary: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path_var| {
        std::env::split_paths(&path_var).find_map(|dir| {
            let candidate = dir.join(binary);
            if candidate.is_file() {
                Some(candidate)
            } else {
                // On Windows, try with .exe
                let with_ext = dir.join(format!("{}.exe", binary));
                if with_ext.is_file() { Some(with_ext) } else { None }
            }
        })
    })
}
