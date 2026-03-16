use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::ast::{AstParser, FunctionInfo, TypeDef};
use crate::git::{DiffHunk, ParsedDiff};
use crate::prompt::estimate_tokens;

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

impl ContextBuilder {
    pub fn new(repo_root: PathBuf, max_tokens: usize) -> Self {
        Self {
            parser: AstParser::new(),
            repo_root,
            max_tokens,
        }
    }

    pub async fn build(&self, diff: ParsedDiff) -> Result<ReviewContext> {
        // 1. For each changed file, parse with tree-sitter
        let mut functions_changed: Vec<FunctionInfo> = Vec::new();
        let mut all_called_names: Vec<String> = Vec::new();
        let mut types_used: Vec<TypeDef> = Vec::new();

        for file in &diff.files {
            let abs_path = self.repo_root.join(&file.path);
            let source = match std::fs::read_to_string(&abs_path) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let parsed = match self.parser.parse_file(&abs_path, &source) {
                Ok(p) => p,
                Err(_) => continue,
            };

            // 2. Find functions that overlap with diff hunks
            let changed_fns = self.functions_overlapping_hunks(&parsed, &file.hunks);

            // 3. Collect all calls made by those functions
            for f in &changed_fns {
                all_called_names.extend(f.called_functions.iter().cloned());
            }
            functions_changed.extend(changed_fns);

            // 4. Collect type defs from this file
            let file_types = self.parser.extract_type_definitions(&parsed);
            types_used.extend(file_types);
        }

        all_called_names.sort();
        all_called_names.dedup();

        // 5. Search repo for definitions of called functions
        let called_functions = self.find_called_function_defs(&all_called_names);

        // 6. Find related tests
        let changed_fn_names: Vec<&str> = functions_changed.iter().map(|f| f.name.as_str()).collect();
        let test_functions = self.find_related_tests(&changed_fn_names);

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

    fn find_called_function_defs(&self, names: &[String]) -> Vec<FunctionInfo> {
        if names.is_empty() {
            return Vec::new();
        }

        let mut results = Vec::new();
        let search_dirs = ["src", "lib", "pkg", "internal", "cmd"];

        for dir_name in &search_dirs {
            let dir = self.repo_root.join(dir_name);
            if dir.exists() {
                self.walk_for_functions(&dir, names, &mut results);
            }
        }

        // Also check repo root itself for single-file projects
        self.walk_dir_shallow(&self.repo_root, names, &mut results);

        results
    }

    fn walk_for_functions(&self, dir: &Path, names: &[String], out: &mut Vec<FunctionInfo>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();

            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if SKIP_DIRS.contains(&name) {
                        continue;
                    }
                }
                self.walk_for_functions(&path, names, out);
            } else if is_source_file(&path) {
                self.extract_matching_fns(&path, names, out);
            }
        }
    }

    fn walk_dir_shallow(&self, dir: &Path, names: &[String], out: &mut Vec<FunctionInfo>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && is_source_file(&path) {
                self.extract_matching_fns(&path, names, out);
            }
        }
    }

    fn extract_matching_fns(&self, path: &Path, names: &[String], out: &mut Vec<FunctionInfo>) {
        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => return,
        };
        let parsed = match self.parser.parse_file(path, &source) {
            Ok(p) => p,
            Err(_) => return,
        };
        let fns = self.parser.extract_all_functions(&parsed);
        for f in fns {
            if names.contains(&f.name) && !out.iter().any(|e: &FunctionInfo| e.name == f.name) {
                out.push(f);
            }
        }
    }

    fn find_related_tests(&self, fn_names: &[&str]) -> Vec<FunctionInfo> {
        let mut tests = Vec::new();

        let test_dirs = ["tests", "test", "__tests__", "spec"];
        for dir_name in &test_dirs {
            let dir = self.repo_root.join(dir_name);
            if dir.exists() {
                self.walk_for_tests(&dir, fn_names, &mut tests);
            }
        }

        // Also inline tests in src (Rust's #[cfg(test)])
        let src_dir = self.repo_root.join("src");
        if src_dir.exists() {
            self.walk_for_tests(&src_dir, fn_names, &mut tests);
        }

        tests
    }

    fn walk_for_tests(&self, dir: &Path, fn_names: &[&str], out: &mut Vec<FunctionInfo>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if !SKIP_DIRS.contains(&name) {
                        self.walk_for_tests(&path, fn_names, out);
                    }
                }
            } else if is_source_file(&path) {
                let source = match std::fs::read_to_string(&path) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let parsed = match self.parser.parse_file(&path, &source) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let fns = self.parser.extract_all_functions(&parsed);
                for f in fns {
                    let is_test = f.name.starts_with("test_")
                        || f.name.ends_with("_test")
                        || fn_names.iter().any(|n| f.name.contains(n));
                    if is_test {
                        out.push(f);
                    }
                }
            }
        }
    }

    fn fit_to_budget(
        &self,
        diff: &ParsedDiff,
        types: &[TypeDef],
        called: &[FunctionInfo],
        tests: &[FunctionInfo],
    ) -> (Vec<TypeDef>, Vec<FunctionInfo>, Vec<FunctionInfo>) {
        use crate::git::DiffLine;

        // Estimate diff tokens from actual line content (same as it appears in the prompt)
        let diff_chars: usize = diff.files.iter().flat_map(|f| f.hunks.iter()).flat_map(|h| h.lines.iter()).map(|l| match l {
            DiffLine::Added(s) | DiffLine::Removed(s) | DiffLine::Context(s) => s.len() + 8,
        }).sum();
        let mut used = diff_chars / 4; // ~4 chars per token
        let budget = self.max_tokens;

        let mut kept_types = Vec::new();
        let mut kept_called = Vec::new();
        let mut kept_tests = Vec::new();

        // Types: estimate from field list
        for t in types {
            let cost = estimate_tokens(&t.fields.join(", ")) + estimate_tokens(&t.name) + 4;
            if used.saturating_add(cost) <= budget {
                used = used.saturating_add(cost);
                kept_types.push(t.clone());
            }
        }

        // Called functions (full body)
        for f in called {
            let cost = estimate_tokens(&f.full_text);
            if used.saturating_add(cost) <= budget {
                used = used.saturating_add(cost);
                kept_called.push(f.clone());
            }
        }

        // Tests (full body)
        for f in tests {
            let cost = estimate_tokens(&f.full_text);
            if used.saturating_add(cost) <= budget {
                used = used.saturating_add(cost);
                kept_tests.push(f.clone());
            }
        }

        (kept_types, kept_called, kept_tests)
    }
}

fn is_source_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go")
    )
}
