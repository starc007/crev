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
    /// The exact source line the finding refers to, copied from the diff.
    /// Attached after validation so the user has the cited code right next
    /// to the description and never has to alt-tab to verify the citation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
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
        quote: None,
    })
}

/// Try to parse a single line into a Finding.
pub fn try_parse_finding_line(line: &str) -> Option<Finding> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if let Some(f) = parse_severity_line(line) {
        return Some(f);
    }
    if line.starts_with("LGTM:") || line.starts_with("LGTM ") {
        let msg = line
            .trim_start_matches("LGTM:")
            .trim_start_matches("LGTM")
            .trim()
            .to_string();
        return Some(Finding {
            severity: Severity::Lgtm,
            file: PathBuf::new(),
            line: None,
            message: if msg.is_empty() { "No issues found.".to_string() } else { msg },
            quote: None,
        });
    }
    None
}

/// Print a single finding (colored).
pub fn print_finding(f: &Finding) {
    match f.severity {
        Severity::High => {
            let prefix = "[!] HIGH ".bold().red();
            let location = format_location(&f.file, f.line);
            println!("{}{}", prefix, location.bold());
            println!("    {}", f.message);
            print_quote(f);
        }
        Severity::Med => {
            let prefix = "[~] MED  ".yellow();
            let location = format_location(&f.file, f.line);
            println!("{}{}", prefix, location);
            println!("    {}", f.message);
            print_quote(f);
        }
        Severity::Low => {
            let prefix = "[i] LOW  ".blue();
            let location = format_location(&f.file, f.line);
            println!("{}{}", prefix, location);
            println!("    {}", f.message);
            print_quote(f);
        }
        Severity::Lgtm => {
            println!("{} {}", "[✓] LGTM".green().bold(), f.message.green());
        }
    }
}

fn print_quote(f: &Finding) {
    if let Some(quote) = &f.quote {
        let trimmed = quote.trim_end();
        if trimmed.is_empty() {
            return;
        }
        // Indent under the message and dim the source so the user's eye
        // returns to the description by default. Trim very long lines.
        let max = 120usize;
        let display = if trimmed.chars().count() > max {
            let truncated: String = trimmed.chars().take(max).collect();
            format!("{}…", truncated)
        } else {
            trimmed.to_string()
        };
        println!("    {} {}", "│".dimmed(), display.dimmed());
    }
}

/// Print the summary line after all findings.
pub fn print_summary(findings: &[Finding], elapsed: Duration, model: &str) {
    if findings.is_empty() {
        println!("{}", "[✓] No findings — review output was empty.".green());
        return;
    }
    let has_lgtm = findings.iter().any(|f| f.severity == Severity::Lgtm);
    if has_lgtm {
        return;
    }
    let high = findings.iter().filter(|f| f.severity == Severity::High).count();
    let med  = findings.iter().filter(|f| f.severity == Severity::Med).count();
    let low  = findings.iter().filter(|f| f.severity == Severity::Low).count();
    let summary = format!(
        "{} findings ({} high, {} med, {} low) · {:.1}s · {}",
        high + med + low, high, med, low, elapsed.as_secs_f64(), model
    );
    println!("\n{}", summary.dimmed());
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
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
            quote: f.quote.clone(),
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
