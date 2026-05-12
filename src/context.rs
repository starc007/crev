use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::ast::{distill_function, AstParser, FunctionInfo, TypeDef, MAX_CALLED_FN_LINES};
use crate::git::{DiffHunk, ParsedDiff};
use crate::prompt::{estimate_tokens, estimate_tokens_from_chars};

/// Cross-chunk repo index. Walks the source tree exactly once per review,
/// even when chunking forces multiple context builds. Without this, every
/// chunk re-parses the entire `src/`, `lib/`, … tree from scratch.
pub struct RepoIndex {
    /// All defined functions in the repo, grouped by name. A name can map to
    /// many functions (e.g. `new` exists on many types) — receiver-aware
    /// matching in `find_called_function_defs` picks the right one.
    pub functions_by_name: HashMap<String, Vec<IndexedFunction>>,
    /// Subset of `functions_by_name` flattened for test discovery.
    pub all_functions: Vec<IndexedFunction>,
}

#[derive(Debug, Clone)]
pub struct IndexedFunction {
    pub file: PathBuf,
    pub info: FunctionInfo,
}

impl RepoIndex {
    /// Walk the repo once and index every function we can parse. Bounded by
    /// the same per-file-size and total-file caps used elsewhere so a
    /// pathological repo can't make indexing run forever.
    pub fn build(repo_root: &Path, parser: &AstParser) -> Self {
        let mut by_name: HashMap<String, Vec<IndexedFunction>> = HashMap::new();
        let mut all: Vec<IndexedFunction> = Vec::new();
        let mut walked = 0usize;

        let dirs = ["src", "lib", "pkg", "internal", "cmd", "tests", "test", "__tests__", "spec"];
        for dir_name in &dirs {
            let dir = repo_root.join(dir_name);
            if dir.exists() {
                walk_index(&dir, parser, &mut by_name, &mut all, &mut walked);
            }
        }
        // Shallow scan of repo root for single-file projects.
        walk_index_shallow(repo_root, parser, &mut by_name, &mut all, &mut walked);

        Self { functions_by_name: by_name, all_functions: all }
    }
}

fn walk_index(
    dir: &Path,
    parser: &AstParser,
    by_name: &mut HashMap<String, Vec<IndexedFunction>>,
    all: &mut Vec<IndexedFunction>,
    walked: &mut usize,
) {
    if *walked >= MAX_FILES_WALKED {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if *walked >= MAX_FILES_WALKED {
            return;
        }
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if SKIP_DIRS.contains(&name) {
                    continue;
                }
            }
            walk_index(&path, parser, by_name, all, walked);
        } else if is_source_file(&path) {
            *walked += 1;
            index_file(&path, parser, by_name, all);
        }
    }
}

fn walk_index_shallow(
    dir: &Path,
    parser: &AstParser,
    by_name: &mut HashMap<String, Vec<IndexedFunction>>,
    all: &mut Vec<IndexedFunction>,
    walked: &mut usize,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if *walked >= MAX_FILES_WALKED {
            return;
        }
        let path = entry.path();
        if path.is_file() && is_source_file(&path) {
            *walked += 1;
            index_file(&path, parser, by_name, all);
        }
    }
}

fn index_file(
    path: &Path,
    parser: &AstParser,
    by_name: &mut HashMap<String, Vec<IndexedFunction>>,
    all: &mut Vec<IndexedFunction>,
) {
    let source = match read_capped(path) {
        Some(s) => s,
        None => return,
    };
    let parsed = match parser.parse_file(path, &source) {
        Ok(p) => p,
        Err(_) => return,
    };
    for info in parser.extract_all_functions(&parsed) {
        let indexed = IndexedFunction { file: path.to_path_buf(), info };
        by_name
            .entry(indexed.info.name.clone())
            .or_default()
            .push(indexed.clone());
        all.push(indexed);
    }
}

// ── public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ReviewContext {
    pub diff: ParsedDiff,
    pub functions_changed: Vec<FunctionInfo>,
    pub called_functions: Vec<FunctionInfo>,
    pub types_used: Vec<TypeDef>,
    pub test_functions: Vec<FunctionInfo>,
    pub token_count: usize,
    pub quality: ContextQuality,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ContextQuality {
    Rich,    // ≥2 type defs AND ≥3 called fn defs
    Partial, // some context but not rich
    Minimal, // diff only
}

impl ContextQuality {
    pub fn label(&self) -> &str {
        match self {
            ContextQuality::Rich => "Rich",
            ContextQuality::Partial => "Partial",
            ContextQuality::Minimal => "Minimal",
        }
    }
}

pub struct ContextBuilder {
    parser: AstParser,
    repo_root: PathBuf,
    max_tokens: usize,
}

// Directories to skip when walking the repo
const SKIP_DIRS: &[&str] = &["target", "node_modules", ".git", "vendor", "dist", "build"];

/// Per-file source-read ceiling (1 MiB). Larger files are skipped — they are
/// nearly always generated code or vendored bundles that would blow the
/// token budget on their own.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Cap on how many files we'll walk while searching for called-function
/// definitions and related tests. Prevents pathological repos (hundreds of
/// thousands of source files) from making `context::build` run for minutes.
const MAX_FILES_WALKED: usize = 5000;

fn read_capped(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

impl ContextBuilder {
    pub fn new(repo_root: PathBuf, max_tokens: usize) -> Self {
        Self {
            parser: AstParser::new(),
            repo_root,
            max_tokens,
        }
    }

    /// Borrow the internal parser so callers can build a [`RepoIndex`] using
    /// the same instance — no extra setup cost for the tree-sitter languages.
    pub fn parser(&self) -> &AstParser {
        &self.parser
    }

    pub async fn build(&self, diff: ParsedDiff, index: &RepoIndex) -> Result<ReviewContext> {
        // 1. For each changed file, parse with tree-sitter
        let mut functions_changed: Vec<FunctionInfo> = Vec::new();
        let mut all_calls: Vec<(PathBuf, String)> = Vec::new();
        let mut types_used: Vec<TypeDef> = Vec::new();

        for file in &diff.files {
            let abs_path = self.repo_root.join(&file.path);
            let source = match read_capped(&abs_path) {
                Some(s) => s,
                None => continue,
            };

            let parsed = match self.parser.parse_file(&abs_path, &source) {
                Ok(p) => p,
                Err(_) => continue,
            };

            // 2. Find functions that overlap with diff hunks
            let changed_fns = self.functions_overlapping_hunks(&parsed, &file.hunks);

            // 3. Collect all calls made by those functions — tagged with the
            //    caller's repo-relative file so we can disambiguate names that
            //    are defined in multiple places (e.g. `new`, `default`).
            for f in &changed_fns {
                for call in &f.called_functions {
                    all_calls.push((file.path.clone(), call.clone()));
                }
            }
            functions_changed.extend(changed_fns);

            // 4. Collect type defs from this file
            let file_types = self.parser.extract_type_definitions(&parsed);
            types_used.extend(file_types);
        }

        all_calls.sort();
        all_calls.dedup();

        // 5. Look up definitions from the prebuilt repo index — no walking.
        let called_functions = lookup_called_function_defs(&all_calls, index);

        // 6. Find related tests from the prebuilt index
        let changed_fn_names: Vec<&str> = functions_changed.iter().map(|f| f.name.as_str()).collect();
        let test_functions = related_tests_from_index(&changed_fn_names, index);

        // 7. Fit into token budget (priority: diff > changed sigs > types > called sigs > tests)
        let (types_used, called_functions, test_functions) =
            self.fit_to_budget(&diff, &types_used, &called_functions, &test_functions);

        // 8. Determine quality
        let quality = if types_used.len() >= 2 && called_functions.len() >= 3 {
            ContextQuality::Rich
        } else if !types_used.is_empty() || !called_functions.is_empty() {
            ContextQuality::Partial
        } else {
            ContextQuality::Minimal
        };

        let summary_text = format!(
            "{} types, {} called fns, {} tests",
            types_used.len(),
            called_functions.len(),
            test_functions.len()
        );
        eprintln!("context: {} ({})", quality.label(), summary_text);

        let token_count = estimate_tokens(&format!("{:?}", &diff));

        Ok(ReviewContext {
            diff,
            functions_changed,
            called_functions,
            types_used,
            test_functions,
            token_count,
            quality,
        })
    }

    fn functions_overlapping_hunks(
        &self,
        parsed: &crate::ast::ParsedFile,
        hunks: &[DiffHunk],
    ) -> Vec<FunctionInfo> {
        let all_fns = self.parser.extract_all_functions(parsed);

        all_fns
            .into_iter()
            .filter(|f| {
                hunks.iter().any(|h| {
                    let hunk_start = h.new_start;
                    let hunk_end = h.new_start + h.new_lines;
                    // Overlap check
                    f.body_range.0 <= hunk_end && f.body_range.1 >= hunk_start
                })
            })
            .collect()
    }


    /// Fit context into the token budget using a tiered allocation:
    /// types 25% · called fns 60% · tests 15% of the budget left over after
    /// the diff. Each tier overflows into the remaining global pool only
    /// after its own cap is reached, so no category can starve the others.
    fn fit_to_budget(
        &self,
        diff: &ParsedDiff,
        types: &[TypeDef],
        called: &[FunctionInfo],
        tests: &[FunctionInfo],
    ) -> (Vec<TypeDef>, Vec<FunctionInfo>, Vec<FunctionInfo>) {
        use crate::git::DiffLine;

        // Estimate diff tokens from the actual rendered line content; pass
        // through `estimate_tokens` so the conversion ratio stays in sync
        // with prompt.rs.
        let diff_chars: usize = diff.files.iter().flat_map(|f| f.hunks.iter()).flat_map(|h| h.lines.iter()).map(|l| match l {
            DiffLine::Added(s) | DiffLine::Removed(s) | DiffLine::Context(s) => s.len() + 8,
        }).sum();
        let diff_tokens = estimate_tokens_from_chars(diff_chars);
        let total_budget = self.max_tokens;
        let context_budget = total_budget.saturating_sub(diff_tokens);

        // Per-tier caps. Numerator/denominator written explicitly so the
        // allocation is easy to read and to tune.
        let types_cap = context_budget * 25 / 100;
        let called_cap = context_budget * 60 / 100;
        let tests_cap = context_budget * 15 / 100;

        // Each tier first spends from its own cap; whatever it leaves behind
        // gets recycled into the global pool the next tier can also draw on.
        let mut global_pool = context_budget;

        let (kept_types, types_spent) = fill_tier(types, types_cap, &mut global_pool, |t| {
            estimate_tokens(&t.fields.join(", ")) + estimate_tokens(&t.name) + 4
        });

        let (kept_called, called_spent) = fill_tier(called, called_cap, &mut global_pool, |f| {
            if f.full_text.is_empty() {
                return 0;
            }
            estimate_tokens(&distill_function(f, MAX_CALLED_FN_LINES))
        });

        let (kept_tests, tests_spent) = fill_tier(tests, tests_cap, &mut global_pool, |f| {
            if f.full_text.is_empty() {
                return 0;
            }
            estimate_tokens(&f.full_text)
        });

        let _ = (types_spent, called_spent, tests_spent);
        (kept_types, kept_called, kept_tests)
    }
}

fn is_source_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go")
    )
}

/// Greedy-pack `items` into a per-tier cap, drawing from `pool` (the shared
/// remaining budget) only after the tier's own cap is exhausted. Returns the
/// kept items and total cost spent. `pool` is decremented in place so the
/// next tier can see how much slack is left.
fn fill_tier<T: Clone>(
    items: &[T],
    tier_cap: usize,
    pool: &mut usize,
    cost_of: impl Fn(&T) -> usize,
) -> (Vec<T>, usize) {
    let mut kept = Vec::new();
    let mut tier_spent = 0usize;
    for item in items {
        let cost = cost_of(item);
        if cost == 0 {
            continue;
        }
        if tier_spent + cost <= tier_cap {
            tier_spent += cost;
            *pool = pool.saturating_sub(cost);
            kept.push(item.clone());
        } else if *pool >= cost {
            // Tier cap hit — keep drawing from the shared remainder if any.
            *pool -= cost;
            kept.push(item.clone());
        }
    }
    (kept, tier_spent)
}

/// Resolve called-function names against the prebuilt index. When multiple
/// definitions share a name (`new`, `default`, `build`), prefer one defined
/// in the same file as the call site, then any in the same directory, then
/// fall back to the first match. This avoids the worst of the
/// name-collision problem without doing real type resolution.
fn lookup_called_function_defs(
    calls: &[(PathBuf, String)],
    index: &RepoIndex,
) -> Vec<FunctionInfo> {
    let mut out: Vec<FunctionInfo> = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (caller_file, name) in calls {
        if !seen.insert(name.as_str()) {
            continue;
        }
        let Some(candidates) = index.functions_by_name.get(name) else {
            continue;
        };
        let pick = pick_best_candidate(candidates, caller_file);
        if let Some(p) = pick {
            out.push(p.info.clone());
        }
    }
    out
}

fn pick_best_candidate<'a>(
    candidates: &'a [IndexedFunction],
    caller_file: &Path,
) -> Option<&'a IndexedFunction> {
    if candidates.len() == 1 {
        return candidates.first();
    }
    // Same file wins.
    if let Some(same_file) = candidates.iter().find(|c| c.file.ends_with(caller_file)) {
        return Some(same_file);
    }
    // Then same directory.
    let caller_dir = caller_file.parent();
    if let Some(dir) = caller_dir {
        if let Some(same_dir) = candidates
            .iter()
            .find(|c| c.file.parent().is_some_and(|p| p.ends_with(dir)))
        {
            return Some(same_dir);
        }
    }
    candidates.first()
}

/// Pull related test functions from the index instead of walking again. A
/// function counts as a test when its name follows the `test_*` / `*_test`
/// convention or includes one of the changed function names.
fn related_tests_from_index(changed_names: &[&str], index: &RepoIndex) -> Vec<FunctionInfo> {
    let mut out: Vec<FunctionInfo> = Vec::new();
    for f in &index.all_functions {
        let name = &f.info.name;
        let is_test = name.starts_with("test_")
            || name.ends_with("_test")
            || changed_names.iter().any(|n| name.contains(n));
        if is_test {
            out.push(f.info.clone());
        }
    }
    out
}
