# crev — Codebase Context

Architecture reference for contributors and AI assistants working on this repo.

---

## Module map

| File | Purpose |
|---|---|
| `src/main.rs` | CLI entrypoint, clap commands, review orchestration, spinner, streaming output |
| `src/llm.rs` | Trait-based LLM backend system (Ollama, Anthropic, OpenAI, Gemini) |
| `src/ollama.rs` | Ollama HTTP client — streaming, model detection, health check |
| `src/git.rs` | git2 wrapper — staged/unstaged/commit/range diffs → `ParsedDiff` |
| `src/ast.rs` | tree-sitter multi-language parser — functions, types, call graph |
| `src/context.rs` | Builds `ReviewContext` from a diff: finds changed fns, resolves call defs, fits into token budget |
| `src/prompt.rs` | Assembles the final LLM prompt from `ReviewContext` + config rules |
| `src/output.rs` | Parses LLM output lines into `Finding` structs, pretty-prints with colors, JSON output |
| `src/config.rs` | Loads `.reviewrc` (repo-local) and `~/.config/crev/config.toml` (global) |
| `src/history.rs` | SQLite review history — saves reviews, detects recurring patterns |
| `src/linters.rs` | Runs language linters (clippy, eslint, ruff, golangci-lint) and filters findings to diff lines |

---

## Data flow

```
git diff
  └─▶ git.rs::get_*_diff()
        └─▶ ParsedDiff { files[], stats }
              └─▶ context.rs::ContextBuilder::build()
                    ├─ ast.rs  → functions_changed, called_functions, test_functions
                    ├─ token budget fit (priority: diff > types > called fns > tests)
                    └─▶ ReviewContext
                          └─▶ prompt.rs::build_review_prompt()
                                └─▶ String (prompt)
                                      └─▶ llm.rs::resolve() → backend.complete()
                                            └─▶ on_token callback (line buffering → mpsc channel)
                                                  └─▶ output.rs::try_parse_finding_line()
                                                        └─▶ Finding[] → print / JSON
```

---

## Key types

```rust
// git.rs
ParsedDiff { files: Vec<ChangedFile>, stats: DiffStats }
ChangedFile { path, hunks: Vec<DiffHunk>, file_type: FileType }
DiffHunk    { old_start, new_start, new_lines, lines: Vec<DiffLine> }
DiffLine    = Added(String) | Removed(String) | Context(String)

// ast.rs
FunctionInfo { name, signature, full_text, body_range, called_functions, is_public }
TypeDef      { name, kind, fields, definition_range }
ParsedFile   { tree, source, language }

// context.rs
ReviewContext { diff, functions_changed, called_functions, types_used, test_functions, token_count, quality }
ContextQuality = Rich | Partial | Minimal

// output.rs
Finding { severity: Severity, file, line, message }
Severity = High | Med | Low | Lgtm

// llm.rs
trait LlmBackend { complete(prompt, on_token) -> Result<String>; name(); is_local() }
```

---

## LLM backends (`src/llm.rs`)

Backend is resolved in this order:

1. `--model` flag name → inferred from substring (`claude` → Anthropic, `gpt`/`o1`/`o3`/`o4` → OpenAI, `gemini` → Gemini, else → Ollama)
2. `backend` field in `.reviewrc`
3. Auto-detect: Ollama running → Anthropic key → OpenAI key → Gemini key

Each backend implements `LlmBackend::complete()` which streams tokens via the `on_token` callback. No backend prints directly — all output goes through the callback.

**API key env vars:**
- Anthropic: `ANTHROPIC_API_KEY` (or `api_key_env` in config)
- OpenAI: `OPENAI_API_KEY`, base URL override: `OPENAI_BASE_URL`
- Gemini: `GEMINI_API_KEY` or `GOOGLE_API_KEY`

---

## Context building (`src/context.rs`)

`ContextBuilder::build()` pipeline:
1. Parse each changed file with tree-sitter
2. Find functions whose line range overlaps diff hunks
3. Collect all function calls made by those functions
4. Walk `src/`, `lib/`, `pkg/`, `internal/`, `cmd/` to find definitions of called functions
5. Find related tests (by name match) in `tests/`, `test/`, `__tests__/`, inline `#[cfg(test)]`
6. Fit everything into `max_tokens` — drop from lowest priority first

Token budget priority: diff content → type defs → called function bodies → tests

**Context quality labels:**
- `Rich` — ≥2 type defs AND ≥3 called function definitions found
- `Partial` — some context but not rich
- `Minimal` — diff only

---

## Streaming output (`src/main.rs`)

Findings stream to the terminal as soon as each line arrives from the LLM:

1. Spinner runs on a separate tokio task (watch channel stop signal)
2. `on_token` callback buffers tokens, sends complete lines via unbounded mpsc channel
3. Consumer loop receives lines, calls `output::try_parse_finding_line()`
4. On first finding: stops spinner, clears spinner line
5. Each finding is printed immediately via `output::print_finding()`
6. After `complete()` returns: print summary line

---

## Configuration

Lookup order: `.reviewrc` (current dir → upward) → `~/.config/crev/config.toml` → built-in defaults

Key config fields:
```toml
[review]
model = "qwen2.5-coder:14b"   # overrides auto-detection
backend = "auto"               # ollama | anthropic | openai | gemini | auto
max_tokens = 32000
severity_threshold = "low"

[privacy]
strip_comments = false
strip_strings = false

[ignore]
paths = ["migrations/", "*.generated.rs"]

[[rules]]
name = "no-unwrap"
description = "Never use .unwrap() in non-test code"
```

---

## Git hooks (`crev init`)

Installed to `.git/hooks/`:

- **pre-commit** — runs `crev review --staged --fail-on=high`
- **pre-push** — reads stdin to skip tag pushes (`refs/tags/*`), skips if only 1 new commit (already covered by pre-commit), otherwise runs `crev review --commits $UPSTREAM..HEAD --fail-on=high`

Remove with `crev init --uninstall` or `rm .git/hooks/pre-push`.

---

## CI / GitHub Actions (`crev init --ci`)

`crev init --ci` writes `.github/workflows/crev.yml` directly (creates directories, overwrites if exists).

Pass `--model` to get the correct secret name in the output:
```sh
crev init --ci --model gemini-2.0-flash   # prints: Name: GEMINI_API_KEY
crev init --ci --model gpt-4o             # prints: Name: OPENAI_API_KEY
crev init --ci                            # prints: Name: ANTHROPIC_API_KEY (default)
```

**Workflow triggers:**
- `pull_request: [opened]` — runs automatically when a PR is opened
- `issue_comment: /crev` — runs on demand when someone comments `/crev` on a PR; reacts with 👀 immediately to acknowledge

**Permissions:** `pull-requests: write`, `contents: read`

**Comment behaviour:** finds an existing `**crev` comment and updates it (PATCH) — no spam on repeated triggers. Falls back to creating a new comment if none exists.

---

## Review history (`src/history.rs`)

SQLite database at:
- macOS: `~/Library/Application Support/crev/history.db`
- Linux: `~/.local/share/crev/history.db`

Tables: `reviews` (full review records) + `patterns` (normalized finding messages for recurrence detection). A warning prints before review if the same pattern appears 3+ times in 30 days.

---

## Build

```sh
cargo build --release          # build
cargo install --path .         # install to PATH
```

Dependencies use `rustls-tls` (no OpenSSL) and `git2` with `default-features = false` (no HTTPS, local ops only) — enables clean cross-compilation for all targets.

Release builds: `.github/workflows/release.yml` builds 4 targets (x86_64/aarch64 × Linux/macOS) on push to `v*` tags or manual trigger via `workflow_dispatch`.
