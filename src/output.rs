use anyhow::Result;
use colored::Colorize;
use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Severity {
    Lgtm,
    Low,
    Med,
    High,
}

impl Severity {
    pub fn as_str(&self) -> &str {
        match self {
            Severity::High => "High",
            Severity::Med => "Med",
            Severity::Low => "Low",
            Severity::Lgtm => "Lgtm",
        }
    }

    pub fn annotation_level(&self) -> &str {
        match self {
            Severity::High => "failure",
            Severity::Med => "warning",
            Severity::Low => "notice",
            Severity::Lgtm => "notice",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub file: PathBuf,
    pub line: Option<u32>,
    pub message: String,
}

pub fn parse_findings(llm_output: &str) -> Vec<Finding> {
    let mut findings = Vec::new();

    for line in llm_output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Match [HIGH], [MED], [LOW] patterns
        if let Some(finding) = parse_severity_line(line) {
            findings.push(finding);
            continue;
        }

        // Match LGTM: ...
        if line.starts_with("LGTM:") || line.starts_with("LGTM ") {
            let msg = line
                .trim_start_matches("LGTM:")
                .trim_start_matches("LGTM")
                .trim()
                .to_string();
            findings.push(Finding {
                severity: Severity::Lgtm,
                file: PathBuf::new(),
                line: None,
                message: if msg.is_empty() {
                    "No issues found.".to_string()
                } else {
                    msg
                },
            });
        }
    }

    findings
}

fn parse_severity_line(line: &str) -> Option<Finding> {
    // Patterns: [HIGH] path:42 — msg  or  [MED]  path:42 — msg  or [LOW] path:42 — msg
    let (severity, rest) = if let Some(r) = line.strip_prefix("[HIGH]") {
        (Severity::High, r.trim())
    } else if let Some(r) = line.strip_prefix("[MED]") {
        (Severity::Med, r.trim())
    } else if let Some(r) = line.strip_prefix("[LOW]") {
        (Severity::Low, r.trim())
    } else {
        return None;
    };

    // Split on " — " or " - " to get location and message
    let (location, message) = if let Some(idx) = rest.find(" — ") {
        (&rest[..idx], rest[idx + " — ".len()..].trim())
    } else if let Some(idx) = rest.find(" - ") {
        (&rest[..idx], rest[idx + " - ".len()..].trim())
    } else {
        (rest, "")
    };

    // Parse path:line_number
    let (file, line_num) = if let Some(colon_pos) = location.rfind(':') {
        let path_part = &location[..colon_pos];
        let line_part = &location[colon_pos + 1..];
        let line_num = line_part.trim().parse::<u32>().ok();
        (PathBuf::from(path_part.trim()), line_num)
    } else {
        (PathBuf::from(location.trim()), None)
    };

    Some(Finding {
        severity,
        file,
        line: line_num,
        message: message.to_string(),
    })
}

pub fn print_findings(findings: &[Finding], elapsed: Duration, model: &str) {
    if findings.is_empty() {
        println!("{}", "[✓] No findings — review output was empty.".green());
        return;
    }

    let mut high = 0;
    let mut med = 0;
    let mut low = 0;
    let mut has_lgtm = false;

    for finding in findings {
        match finding.severity {
            Severity::High => {
                high += 1;
                let prefix = "[!] HIGH ".bold().red();
                let location = format_location(&finding.file, finding.line);
                println!("{}{}", prefix, location.bold());
                println!("    {}", finding.message);
            }
            Severity::Med => {
                med += 1;
                let prefix = "[~] MED  ".yellow();
                let location = format_location(&finding.file, finding.line);
                println!("{}{}", prefix, location);
                println!("    {}", finding.message);
            }
            Severity::Low => {
                low += 1;
                let prefix = "[i] LOW  ".blue();
                let location = format_location(&finding.file, finding.line);
                println!("{}{}", prefix, location);
                println!("    {}", finding.message);
            }
            Severity::Lgtm => {
                has_lgtm = true;
                println!("{} {}", "[✓] LGTM".green().bold(), finding.message.green());
            }
        }
    }

    if !has_lgtm {
        let total = high + med + low;
        let elapsed_secs = elapsed.as_secs_f64();
        let summary = format!(
            "{} findings ({} high, {} med, {} low) · {:.1}s · {}",
            total, high, med, low, elapsed_secs, model
        );
        println!("\n{}", summary.dimmed());
    }
}

fn format_location(file: &PathBuf, line: Option<u32>) -> String {
    if file.as_os_str().is_empty() {
        return String::new();
    }
    match line {
        Some(l) => format!("{}:{}", file.display(), l),
        None => file.display().to_string(),
    }
}

#[derive(Serialize)]
pub struct JsonOutput {
    pub findings: Vec<JsonFinding>,
    pub github_annotations: Vec<GithubAnnotation>,
}

#[derive(Serialize)]
pub struct JsonFinding {
    pub severity: String,
    pub file: String,
    pub line: Option<u32>,
    pub message: String,
}

#[derive(Serialize)]
pub struct GithubAnnotation {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub annotation_level: String,
    pub message: String,
}

pub fn print_findings_json(findings: &[Finding]) -> Result<()> {
    let json_findings: Vec<JsonFinding> = findings
        .iter()
        .map(|f| JsonFinding {
            severity: f.severity.as_str().to_string(),
            file: f.file.display().to_string(),
            line: f.line,
            message: f.message.clone(),
        })
        .collect();

    // Only emit annotations when we have an actual line number; without one
    // GitHub would pin the annotation to line 1, which is misleading.
    let github_annotations: Vec<GithubAnnotation> = findings
        .iter()
        .filter(|f| {
            f.severity != Severity::Lgtm && !f.file.as_os_str().is_empty() && f.line.is_some()
        })
        .map(|f| {
            let line = f.line.expect("filtered above");
            GithubAnnotation {
                path: f.file.display().to_string(),
                start_line: line,
                end_line: line,
                annotation_level: f.severity.annotation_level().to_string(),
                message: f.message.clone(),
            }
        })
        .collect();

    let output = JsonOutput {
        findings: json_findings,
        github_annotations,
    };

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
