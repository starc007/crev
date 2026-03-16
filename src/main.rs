mod config;
mod git;
mod ollama;
mod output;
mod prompt;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
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
    },
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
            no_cloud: _no_cloud,
            path,
        } => {
            run_review(&path, staged, unstaged, commit, commits, json, fail_on, security).await?;
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

        Commands::Config { show } => {
            if show {
                let cfg = config::load_config(&std::env::current_dir()?);
                println!("{:#?}", cfg);
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
) -> Result<()> {
    let cfg = config::load_config(path);

    // Check Ollama is running
    if !ollama::is_running().await {
        anyhow::bail!("Ollama is not running. Start it with: ollama serve");
    }

    // Get available models and pick the best one
    let models = ollama::list_models().await?;
    let model = cfg
        .review
        .model
        .clone()
        .or_else(|| ollama::detect_best_model(&models))
        .unwrap_or_else(|| "llama3:8b".to_string());

    eprintln!("using {} via Ollama (local)", model);

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

    // Build prompt
    let prompt_text = prompt::build_review_prompt(&diff, &cfg, security);

    // Stream completion
    let start = Instant::now();
    let full_response = ollama::stream_completion(&prompt_text, &model, |_| {}).await?;
    let elapsed = start.elapsed();

    // Parse and display findings
    let findings = output::parse_findings(&full_response);

    if json {
        output::print_findings_json(&findings)?;
    } else {
        output::print_findings(&findings, elapsed, &model);
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
    let pre_push_content = "#!/bin/sh\ncrev review --commits HEAD~3..HEAD --fail-on=high\n";

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

fn print_ci_workflow() {
    let workflow = concat!(
        "# Save as .github/workflows/crev.yml\n",
        "name: crev code review\n",
        "on:\n",
        "  pull_request:\n",
        "    types: [opened, synchronize]\n",
        "jobs:\n",
        "  review:\n",
        "    runs-on: ubuntu-latest\n",
        "    steps:\n",
        "      - uses: actions/checkout@v4\n",
        "        with:\n",
        "          fetch-depth: 0\n",
        "      - name: Install crev\n",
        "        run: cargo install crev --locked\n",
        "      - name: Install Ollama\n",
        "        run: curl -fsSL https://ollama.ai/install.sh | sh\n",
        "      - name: Start Ollama\n",
        "        run: ollama serve &\n",
        "      - name: Pull model\n",
        "        run: ollama pull qwen2.5-coder:7b\n",
        "      - name: Run review\n",
        "        run: |\n",
        "          crev review --commits ${{ github.event.pull_request.base.sha }}..${{ github.sha }} --json > findings.json\n",
        "      - name: Post PR comment\n",
        "        env:\n",
        "          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}\n",
        "        run: |\n",
        "          python3 post_review.py\n",
    );
    print!("{}", workflow);
}

fn run_history(_patterns: bool, _clear: bool) -> Result<()> {
    println!("History feature coming in Phase 3.");
    Ok(())
}

fn run_rules(_action: RulesCommands) -> Result<()> {
    println!("Rules feature coming in Phase 4.");
    Ok(())
}
