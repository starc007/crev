mod ast;
mod config;
mod context;
mod git;
mod history;
mod linters;
mod llm;
mod ollama;
mod output;
mod prompt;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Parser)]
#[command(name = "crev", about = "Local AI code review CLI", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Review code changes using a local LLM
    Review {
        /// Review staged changes (default)
        #[arg(long, default_value_t = true)]
        staged: bool,

        /// Review unstaged changes
        #[arg(long, conflicts_with = "staged")]
        unstaged: bool,

        /// Review a specific commit hash
        #[arg(long)]
        commit: Option<String>,

        /// Review a commit range (e.g. HEAD~3..HEAD)
        #[arg(long)]
        commits: Option<String>,

        /// Review a specific file
        #[arg(long)]
        file: Option<PathBuf>,

        /// Output findings as JSON
        #[arg(long)]
        json: bool,

        /// Fail with exit code 1 if any finding at or above this severity is found
        #[arg(long, value_parser = ["low", "med", "high"])]
        fail_on: Option<String>,

        /// Use security-focused review mode
        #[arg(long)]
        security: bool,

        /// Show context budget breakdown
        #[arg(long)]
        verbose: bool,

        /// Never fall back to cloud LLM
        #[arg(long)]
        no_cloud: bool,

        /// Model to use (e.g. qwen2.5-coder:14b, claude-sonnet-4-5, gpt-4o, gemini-1.5-pro)
        #[arg(long, short = 'm')]
        model: Option<String>,

        /// Path to git repo (default: current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },

    /// Initialize crev in the current git repo (install hooks, create .reviewrc)
    Init {
        /// Overwrite existing hooks without prompting
        #[arg(long)]
        force: bool,

        /// Only install hooks, skip .reviewrc creation
        #[arg(long)]
        hooks_only: bool,

        /// Print what would be done without doing it
        #[arg(long)]
        dry_run: bool,

        /// Remove installed hooks
        #[arg(long)]
        uninstall: bool,

        /// Print a GitHub Actions workflow to stdout
        #[arg(long)]
        ci: bool,
    },

    /// Show review history and recurring patterns
    History {
        /// Show recurring patterns sorted by frequency
        #[arg(long)]
        patterns: bool,

        /// Clear all history for the current repo
        #[arg(long)]
        clear: bool,
    },

    /// Manage rule packs
    Rules {
        #[command(subcommand)]
        action: RulesCommands,
    },

    /// Show or set configuration
    Config {
        /// Show effective configuration
        #[arg(long)]
        show: bool,

        /// Write default config to ~/.config/crev/config.toml
        #[arg(long)]
        init: bool,
    },

    /// Update crev to the latest version
    Update,
}

#[derive(Subcommand)]
enum RulesCommands {
    /// List all active rules
    List,
    /// Add a rule pack
    Add { name: String },
    /// Remove a rule pack
    Remove { name: String },
    /// Validate all rule pack files
    Check,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Review {
            staged,
            unstaged,
            commit,
            commits,
            file: _file,
            json,
            fail_on,
            security,
            verbose: _verbose,
            no_cloud,
            model,
            path,
        } => {
            run_review(&path, staged, unstaged, commit, commits, json, fail_on, security, no_cloud, model).await?;
        }

        Commands::Init {
            force,
            hooks_only,
            dry_run,
            uninstall,
            ci,
        } => {
            run_init(force, hooks_only, dry_run, uninstall, ci)?;
        }

        Commands::History { patterns, clear } => {
            run_history(patterns, clear)?;
        }

        Commands::Rules { action } => {
            run_rules(action)?;
        }

        Commands::Update => {
            run_update()?;
        }

        Commands::Config { show, init } => {
            if init {
                let home = std::env::var("HOME")
                    .or_else(|_| std::env::var("USERPROFILE"))
                    .map(std::path::PathBuf::from)
                    .map_err(|_| anyhow::anyhow!("Could not determine home directory"))?;
                let global_config = home.join(".config/crev/config.toml");
                if global_config.exists() {
                    println!("Global config already exists at {}", global_config.display());
                    println!("Delete it manually to recreate it.");
                } else {
                    config::save_default_config(&global_config)?;
                    println!("Created {}", global_config.display());
                }
            } else if show {
                let cfg = config::load_config(&std::env::current_dir()?);
                println!("{:#?}", cfg);
            } else {
                println!("Use --show to display config or --init to create ~/.config/crev/config.toml");
            }
        }
    }

    Ok(())
}

async fn run_review(
    path: &PathBuf,
    staged: bool,
    unstaged: bool,
    commit: Option<String>,
    commits: Option<String>,
    json: bool,
    fail_on: Option<String>,
    security: bool,
    no_cloud: bool,
    cli_model: Option<String>,
) -> Result<()> {
    let cfg = config::load_config(path);

    // Resolve backend + model (CLI flag > config > auto-detect)
    let requested_model = cli_model.as_deref().or(cfg.review.model.as_deref());
    let (backend, model) = llm::resolve(
        requested_model,
        &cfg.review.backend,
        cfg.review.api_key_env.as_deref(),
        no_cloud,
    )
    .await?;

    eprintln!(
        "using {} via {} ({})",
        model,
        backend.name(),
        if backend.is_local() { "local" } else { "cloud" }
    );

    // Get the diff
    let diff = if unstaged {
        git::get_unstaged_diff(path)?
    } else if let Some(hash) = commit {
        git::get_commit_diff(path, &hash)?
    } else if let Some(range) = commits {
        let parts: Vec<&str> = range.splitn(2, "..").collect();
        if parts.len() == 2 {
            git::get_range_diff(path, parts[0], parts[1])?
        } else {
            anyhow::bail!("Invalid commit range: {}. Use format: from..to", range);
        }
    } else {
        let _ = staged;
        git::get_staged_diff(path)?
    };

    if diff.files.is_empty() {
        println!("No changes to review.");
        return Ok(());
    }

    eprintln!(
        "reviewing {} files ({} added, {} removed)",
        diff.stats.files_changed, diff.stats.lines_added, diff.stats.lines_removed
    );

    // Filter ignored files
    let filtered_files: Vec<_> = diff
        .files
        .into_iter()
        .filter(|f| !config::should_ignore_file(&f.path, &cfg))
        .collect();

    let diff = git::ParsedDiff {
        stats: git::DiffStats {
            files_changed: filtered_files.len(),
            lines_added: diff.stats.lines_added,
            lines_removed: diff.stats.lines_removed,
        },
        files: filtered_files,
    };

    // Build semantic context and run linters in parallel (Phase 2 + 3)
    let repo_root = git::find_repo_root(path)?;
    let ctx_builder = context::ContextBuilder::new(repo_root.clone(), cfg.review.max_tokens);

    let (ctx_result, linter_findings) = tokio::join!(
        ctx_builder.build(diff.clone()),
        linters::run_linters(&diff, &repo_root),
    );

    if !linter_findings.is_empty() {
        let by_tool: std::collections::HashMap<&str, usize> =
            linter_findings.iter().fold(std::collections::HashMap::new(), |mut m, f| {
                *m.entry(f.linter.as_str()).or_insert(0) += 1;
                m
            });
        let summary: Vec<String> = by_tool.iter().map(|(k, v)| format!("{}: {}", k, v)).collect();
        eprintln!("linters: {} findings in diff ({})", linter_findings.len(), summary.join(", "));
    }

    let prompt_text = match ctx_result {
        Ok(ctx) => prompt::build_review_prompt_ctx(&ctx, &cfg, security, &linter_findings),
        Err(e) => {
            eprintln!("context: Minimal (fallback to diff-only: {})", e);
            prompt::build_review_prompt(&diff, &cfg, security)
        }
    };

    // Show recurring patterns before the review output
    if let Ok(patterns) = history::detect_patterns(&repo_root) {
        for p in &patterns {
            eprintln!(
                "⚠ Recurring pattern detected ({}x in 30 days):\n  {}",
                p.count,
                p.pattern
            );
        }
    }

    // ── Spinner ──────────────────────────────────────────────────────────────
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let spinner_task = tokio::spawn(async move {
        let frames = ["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"];
        let mut i = 0usize;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(tokio::time::Duration::from_millis(80)) => {
                    use std::io::Write;
                    eprint!("\r{} analyzing...", frames[i % frames.len()]);
                    std::io::stderr().flush().ok();
                    i += 1;
                }
                _ = stop_rx.changed() => {
                    use std::io::Write;
                    eprint!("\r\x1b[K");
                    std::io::stderr().flush().ok();
                    break;
                }
            }
        }
    });

    // ── Stream completion, buffer complete lines ──────────────────────────────
    let line_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let line_buf2 = line_buf.clone();
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let line_tx2 = line_tx.clone();

    let start = Instant::now();
    let full_response = backend.complete(&prompt_text, &(move |token: &str| {
        let mut buf = line_buf2.lock().unwrap();
        buf.push_str(token);
        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].to_string();
            *buf = buf[nl + 1..].to_string();
            if !line.trim().is_empty() {
                let _ = line_tx2.send(line);
            }
        }
    })).await?;

    // Flush any remaining content not terminated with a newline
    {
        let buf = line_buf.lock().unwrap();
        if !buf.trim().is_empty() {
            let _ = line_tx.send(buf.trim().to_string());
        }
    }
    drop(line_tx); // close channel so receiver loop exits

    // ── Consume lines: stop spinner then print each finding ───────────────────
    let mut findings: Vec<output::Finding> = Vec::new();
    let mut spinner_task = Some(spinner_task);

    while let Some(line) = line_rx.recv().await {
        if let Some(f) = output::try_parse_finding_line(&line) {
            if let Some(task) = spinner_task.take() {
                stop_tx.send(true).ok();
                task.await.ok();
            }
            if !json {
                output::print_finding(&f);
            }
            findings.push(f);
        }
    }

    // Stop spinner if model returned nothing parseable
    if let Some(task) = spinner_task.take() {
        stop_tx.send(true).ok();
        task.await.ok();
    }

    let elapsed = start.elapsed();

    if json {
        output::print_findings_json(&findings)?;
    } else {
        output::print_summary(&findings, elapsed, &model);
    }

    // Save to history
    if let Err(e) = history::save_review(&history::CompletedReview {
        repo_path: repo_root.clone(),
        commit_hash: None,
        files_changed: diff.stats.files_changed,
        findings: findings.clone(),
        model_used: model.clone(),
        elapsed_ms: elapsed.as_millis() as u64,
    }) {
        eprintln!("warning: could not save review history: {}", e);
    }

    // Handle --fail-on
    if let Some(threshold) = fail_on {
        let threshold_sev = match threshold.as_str() {
            "high" => output::Severity::High,
            "med" => output::Severity::Med,
            _ => output::Severity::Low,
        };
        let has_failure = findings
            .iter()
            .any(|f| f.severity >= threshold_sev && f.severity != output::Severity::Lgtm);
        if has_failure {
            std::process::exit(1);
        }
    }

    Ok(())
}

fn run_init(force: bool, hooks_only: bool, dry_run: bool, uninstall: bool, ci: bool) -> Result<()> {
    let repo_root = find_git_root(&std::env::current_dir()?)?;

    if ci {
        print_ci_workflow();
        return Ok(());
    }

    if uninstall {
        let pre_commit = repo_root.join(".git/hooks/pre-commit");
        let pre_push = repo_root.join(".git/hooks/pre-push");
        if dry_run {
            println!("Would remove: {}", pre_commit.display());
            println!("Would remove: {}", pre_push.display());
        } else {
            for p in [&pre_commit, &pre_push] {
                if p.exists() {
                    std::fs::remove_file(p)?;
                    println!("Removed {}", p.display());
                }
            }
        }
        return Ok(());
    }

    if !hooks_only {
        let reviewrc = repo_root.join(".reviewrc");
        if reviewrc.exists() && !force {
            println!(".reviewrc already exists, skipping (use --force to overwrite)");
        } else if dry_run {
            println!("Would write: {}", reviewrc.display());
        } else {
            config::save_default_config(&reviewrc)?;
            println!("Created {}", reviewrc.display());
        }
    }

    let hooks_dir = repo_root.join(".git/hooks");
    let pre_commit = hooks_dir.join("pre-commit");
    let pre_push = hooks_dir.join("pre-push");

    let pre_commit_content = "#!/bin/sh\ncrev review --staged --fail-on=high\n";
    let pre_push_content = "\
#!/bin/sh
# Skip tag pushes — nothing to review.
while IFS=' ' read -r local_ref _a _b _c; do
  case \"$local_ref\" in refs/tags/*) exit 0 ;; esac
done

# Only review when pushing more than one new commit.
# A single commit was already reviewed by the pre-commit hook.
UPSTREAM=\"origin/$(git rev-parse --abbrev-ref HEAD 2>/dev/null)\"
COUNT=$(git rev-list \"$UPSTREAM..HEAD\" --count 2>/dev/null || echo 0)
if [ \"$COUNT\" -gt 1 ]; then
  crev review --commits \"$UPSTREAM..HEAD\" --fail-on=high
fi
";

    for (path, content, name) in [
        (&pre_commit, pre_commit_content, "pre-commit"),
        (&pre_push, pre_push_content, "pre-push"),
    ] {
        if path.exists() && !force {
            println!("{} hook already exists (use --force to overwrite)", name);
            continue;
        }
        if dry_run {
            println!("Would write: {}", path.display());
        } else {
            std::fs::write(path, content)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
            }
            println!("Installed {}", path.display());
        }
    }

    Ok(())
}

fn find_git_root(start: &std::path::Path) -> Result<PathBuf> {
    git::find_repo_root(start)
}

fn run_update() -> Result<()> {
    eprintln!("Updating crev to the latest version...");
    let status = std::process::Command::new("sh")
        .args([
            "-c",
            "curl -fsSL https://raw.githubusercontent.com/starc007/crev/main/install.sh | sh",
        ])
        .status()?;
    if !status.success() {
        anyhow::bail!("Update failed. Try running the install script manually:\n  curl -fsSL https://raw.githubusercontent.com/starc007/crev/main/install.sh | sh");
    }
    Ok(())
}

fn print_ci_workflow() {
    // Note: ${{ }} expressions are GitHub Actions syntax — they stay as-is in the output.
    let workflow = r#"# Save as .github/workflows/crev.yml
# Required secret: ANTHROPIC_API_KEY  (repo Settings → Secrets and variables → Actions)
# To use OpenAI instead: set OPENAI_API_KEY and change --model to gpt-4o
name: crev code review
on:
  pull_request:
    types: [opened, synchronize]
jobs:
  review:
    runs-on: ubuntu-latest
    permissions:
      pull-requests: write
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - name: Install crev
        run: curl -fsSL https://raw.githubusercontent.com/starc007/crev/main/install.sh | sh

      - name: Run crev review
        env:
          ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
        run: |
          crev review \
            --commits ${{ github.event.pull_request.base.sha }}..${{ github.sha }} \
            --model claude-sonnet-4-5 \
            --json > findings.json

      - name: Post findings to PR
        if: always()
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          PR_NUMBER: ${{ github.event.pull_request.number }}
        run: |
          python3 << 'PYEOF'
          import json, subprocess, os, sys

          with open('findings.json') as f:
              data = json.load(f)

          # Emit inline annotations on the PR diff
          for a in data.get('github_annotations', []):
              level = {'failure': 'error', 'warning': 'warning', 'notice': 'notice'}.get(
                  a['annotation_level'], 'notice'
              )
              print(f"::{level} file={a['path']},line={a['start_line']}::{a['message']}")

          # Build a summary PR comment
          findings = data.get('findings', [])
          high = [f for f in findings if f['severity'] == 'High']
          med  = [f for f in findings if f['severity'] == 'Med']
          low  = [f for f in findings if f['severity'] == 'Low']

          if not findings or all(f['severity'] == 'Lgtm' for f in findings):
              body = '**crev** \u2705 \u2014 no issues found'
          else:
              lines = ['**crev review**\n']
              for f in high:
                  lines.append(f"\U0001f534 **HIGH** `{f['file']}`:{f.get('line','?')} \u2014 {f['message']}")
              for f in med:
                  lines.append(f"\U0001f7e1 **MED**  `{f['file']}`:{f.get('line','?')} \u2014 {f['message']}")
              for f in low:
                  lines.append(f"\U0001f535 **LOW**  `{f['file']}`:{f.get('line','?')} \u2014 {f['message']}")
              counts = []
              if high: counts.append(f'{len(high)} high')
              if med:  counts.append(f'{len(med)} med')
              if low:  counts.append(f'{len(low)} low')
              lines.append(f'\n_{", ".join(counts)} \u00b7 powered by [crev](https://github.com/starc007/crev)_')
              body = '\n'.join(lines)

          pr = os.environ.get('PR_NUMBER')
          if pr:
              subprocess.run(
                  ['gh', 'pr', 'comment', pr, '--body', body],
                  check=False
              )
          PYEOF

      - name: Fail on HIGH findings
        run: |
          python3 -c "
          import json, sys
          with open('findings.json') as f:
              d = json.load(f)
          high = [x for x in d.get('findings', []) if x['severity'] == 'High']
          if high:
              print(f'Blocking merge: {len(high)} HIGH finding(s)')
              sys.exit(1)
          "
"#;
    print!("{}", workflow);
}

fn run_history(patterns: bool, clear: bool) -> Result<()> {
    let repo_root = git::find_repo_root(&std::env::current_dir()?)?;

    if clear {
        history::clear_history(&repo_root)?;
        println!("History cleared for {}", repo_root.display());
        return Ok(());
    }

    if patterns {
        let pats = history::detect_patterns(&repo_root)?;
        if pats.is_empty() {
            println!("No recurring patterns detected.");
        } else {
            println!("Recurring patterns (3+ occurrences in last 30 days):\n");
            for p in &pats {
                println!("  {}x  {}", p.count, p.pattern);
                if let Some(f) = &p.file_path {
                    println!("       in {}", f);
                }
            }
        }
        return Ok(());
    }

    // Default: show last 10 reviews
    let reviews = history::get_recent_reviews(&repo_root, 10)?;
    if reviews.is_empty() {
        println!("No review history found for this repo.");
    } else {
        println!("{:<6} {:<12} {:<8} {:<6} {}", "ID", "DATE", "FILES", "FINDS", "MODEL");
        println!("{}", "-".repeat(60));
        for r in &reviews {
            let date = format_unix_ts(r.timestamp);
            println!(
                "{:<6} {:<12} {:<8} {:<6} {}",
                r.id, date, r.files_changed, r.finding_count, r.model_used
            );
        }
    }

    Ok(())
}

fn format_unix_ts(ts: i64) -> String {
    // Proper Gregorian calendar calculation without external deps
    const DAYS_IN_MONTH: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut days = ts as u64 / 86400;
    let mut year = 1970u64;
    loop {
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let days_in_year = if leap { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let mut month = 0usize;
    for (i, &dim) in DAYS_IN_MONTH.iter().enumerate() {
        let dim = if i == 1 && leap { 29 } else { dim };
        if days < dim {
            month = i + 1;
            break;
        }
        days -= dim;
    }
    format!("{}-{:02}-{:02}", year, month, days + 1)
}

fn run_rules(_action: RulesCommands) -> Result<()> {
    println!("Rules feature coming in Phase 4.");
    Ok(())
}
