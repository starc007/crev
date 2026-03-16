# crev

Local AI code review from the command line. Runs entirely on your machine — no API keys, no data leaving your network.

```
$ crev review --staged
using minimax-m2.5:cloud via Ollama (local)
reviewing 3 files (47 added, 12 removed)
context: Rich (4 types, 6 called fns, 2 tests)

[!] HIGH src/payments/processor.rs:142
    balance + amount can exceed i64::MAX when processing large transfers;
    use checked_add() and return Err on overflow

[~] MED  src/payments/processor.rs:98
    db.execute() result is silently ignored; if the INSERT fails, the caller
    receives a success response while the data was never written

[✓] LGTM src/utils/format.rs — change looks correct

3 findings (1 high, 1 med, 0 low) · 4.2s · minimax-m2.5:cloud
```

---

## Requirements

- [Ollama](https://ollama.ai) running locally (`ollama serve`)
- A code model pulled: `ollama pull qwen2.5-coder:7b`
- A git repository

---

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/starc007/crev/main/install.sh | sh
```

Or build from source:

```sh
cargo install --path .
```

---

## Quick start

```sh
# Stage some changes
git add -p

# Review them
crev review --staged

# Install git hooks so every commit is reviewed automatically
crev init
```

---

## Commands

### `crev review`

Reviews code changes using a local LLM.

```
Options:
  --staged            Review staged changes (default)
  --unstaged          Review unstaged changes
  --commit <HASH>     Review a specific commit
  --commits <RANGE>   Review a commit range  e.g. HEAD~3..HEAD
  --json              Output findings as JSON
  --fail-on <SEV>     Exit 1 if any finding at or above this severity [low|med|high]
  --security          Security-focused review mode
  --no-cloud          Never fall back to a cloud LLM
  --path <PATH>       Path to git repo (default: current directory)
```

**Examples**

```sh
crev review --staged
crev review --commit abc1234
crev review --commits main..feature-branch
crev review --staged --fail-on=high    # blocks git hooks on HIGH findings
crev review --staged --json            # machine-readable output
crev review --staged --security        # security-only scan
```

### `crev init`

Installs git hooks and creates a `.reviewrc` config in the repo root.

```sh
crev init              # install hooks + create .reviewrc
crev init --dry-run    # preview what would be installed
crev init --force      # overwrite existing hooks
crev init --hooks-only # skip .reviewrc creation
crev init --uninstall  # remove hooks
crev init --ci         # print a GitHub Actions workflow to stdout
```

Installs two hooks:
- **pre-commit**: `crev review --staged --fail-on=high`
- **pre-push**: `crev review --commits HEAD~3..HEAD --fail-on=high`

### `crev history`

Shows review history stored in a local SQLite database.

```sh
crev history              # last 10 reviews
crev history --patterns   # recurring findings (3+ times in 30 days)
crev history --clear      # delete history for this repo
```

History is stored at `~/Library/Application Support/crev/history.db` on macOS and `~/.local/share/crev/history.db` on Linux.

### `crev config`

```sh
crev config --show    # print effective config for current directory
crev config --init    # create ~/.config/crev/config.toml with defaults
```

---

## Configuration

`crev init` creates `.reviewrc` in your repo root. Commit it to share settings with your team.

```toml
[review]
# model = "qwen2.5-coder:14b"   # override auto-detected model
max_tokens = 8000
severity_threshold = "low"      # low | med | high

[privacy]
strip_comments = false           # remove comments before sending to LLM
strip_strings = false            # replace string literals with <REDACTED>

[ignore]
paths = [
  "migrations/",
  "*.generated.rs",
  "vendor/",
  "*.pb.go",
]

[[rules]]
name = "no-raw-sql"
description = "All DB queries must use QueryBuilder, never raw SQL strings"

[[rules]]
name = "no-unwrap"
description = "Never use .unwrap() in non-test code — use ? or expect()"
```

Personal defaults (model choice, API keys) go in `~/.config/crev/config.toml` — this file is never committed.

**Config lookup order:** `.reviewrc` (current dir → upward) → `~/.config/crev/config.toml` → built-in defaults.

---

## How it works

### 1. Diff extraction

Gets the diff via [git2](https://github.com/rust-lang/git2-rs) with 10 lines of surrounding context per hunk.

### 2. Semantic context

Before building the prompt, crev parses your source files with tree-sitter to extract:

- Functions that overlap with the changed lines
- Definitions of the functions those changed functions call
- Type definitions used in changed function signatures
- Related test functions

This gives the LLM enough context to reason about correctness rather than just syntax. Context quality is printed before each review:

```
context: Rich (4 types, 6 called fns, 2 tests)
context: Partial (1 type, 2 called fns, 0 tests)
context: Minimal (fallback to diff-only)
```

**Supported languages:** Rust, TypeScript, JavaScript (including TSX/JSX), Python, Go.

### 3. Static linter fusion

If linters are installed, crev runs them in parallel and includes their output in the prompt. The LLM assesses whether each finding is a genuine risk or a false positive.

| Language | Linter |
|---|---|
| Rust | `cargo clippy` |
| TypeScript / JavaScript | `eslint` |
| Python | `ruff` |
| Go | `golangci-lint` |
| Any | `semgrep` (if `.semgrep.yml` or `.semgrep/` exists) |

Linters that are not installed are silently skipped. Only findings within ±5 lines of a changed line are included.

### 4. Model selection

crev picks the best available model automatically:

| Model | Min RAM | Quality |
|---|---|---|
| `qwen2.5-coder:14b` | 16 GB | ★★★★★ |
| `qwen2.5-coder:7b` | 8 GB | ★★★★☆ |
| `deepseek-coder-v2:16b` | 16 GB | ★★★★★ |
| `codellama:13b` | 8 GB | ★★★★☆ |
| `llama3:8b` | 8 GB | ★★★☆☆ |

Override with `model = "..."` in `.reviewrc`. Set `OLLAMA_HOST` to point at a remote Ollama instance.

---

## Output formats

### Terminal (default)

```
[!] HIGH src/payments/processor.rs:142
    balance + amount can exceed i64::MAX — use checked_add()

[~] MED  src/auth/session.rs:67
    session token written to log at info level — strip before logging

[i] LOW  src/utils/retry.rs:23
    fixed sleep of 1000ms; use exponential backoff to avoid thundering herd

3 findings (1 high, 1 med, 1 low) · 4.2s · qwen2.5-coder:14b
```

### JSON (`--json`)

```json
{
  "findings": [
    {
      "severity": "High",
      "file": "src/payments/processor.rs",
      "line": 142,
      "message": "balance + amount can exceed i64::MAX — use checked_add()"
    }
  ],
  "github_annotations": [
    {
      "path": "src/payments/processor.rs",
      "start_line": 142,
      "end_line": 142,
      "annotation_level": "failure",
      "message": "balance + amount can exceed i64::MAX — use checked_add()"
    }
  ]
}
```

`github_annotations` is compatible with the [GitHub Check Runs API](https://docs.github.com/en/rest/checks/runs) and appears as inline comments on PR diffs.

---

## CI / GitHub Actions

```sh
crev init --ci > .github/workflows/crev.yml
```

Or add to an existing workflow:

```yaml
- name: Install Ollama
  run: curl -fsSL https://ollama.ai/install.sh | sh

- name: Review PR
  run: |
    ollama serve &
    ollama pull qwen2.5-coder:7b
    crev review --commits ${{ github.event.pull_request.base.sha }}..${{ github.sha }} \
      --json > findings.json
```

---

## Pattern detection

crev tracks every review in a local SQLite database. After the same finding appears 3+ times in a 30-day window, it warns you before the next review:

```
⚠ Recurring pattern detected (5x in 30 days):
  integer overflow in arithmetic operations
```

This surfaces systemic issues that keep slipping through code review.

---

## Privacy

- **Nothing leaves your machine by default.** The LLM runs locally via Ollama.
- The diff is sent only to `localhost:11434`.
- No telemetry, no accounts, no network requests outside Ollama.
- Use `strip_comments` and `strip_strings` in `.reviewrc` if the code contains sensitive data.
- Use `--no-cloud` to hard-disable cloud fallback even if configured.

---

## License

MIT
