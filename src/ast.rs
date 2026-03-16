use anyhow::{Context, Result};
use std::path::Path;
use tree_sitter::{Language, Node, Parser, Tree};

use crate::git::FileType;

// ── public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub path: std::path::PathBuf,
    pub source: String,
    pub tree: Tree,
    pub language: FileType,
}

#[derive(Debug, Clone)]
pub struct FunctionInfo {
    pub name: String,
    pub signature: String,
    pub body_range: (u32, u32),
    pub called_functions: Vec<String>,
    pub doc_comment: Option<String>,
    pub is_public: bool,
}

#[derive(Debug, Clone)]
pub struct TypeDef {
    pub name: String,
    pub kind: TypeKind,
    pub fields: Vec<String>,
    pub definition_range: (u32, u32),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeKind {
    Struct,
    Enum,
    Interface,
    TypeAlias,
    Trait,
    Class,
}

// ── parser ───────────────────────────────────────────────────────────────────

pub struct AstParser {
    rust_lang: Language,
    ts_lang: Language,
    tsx_lang: Language,
    python_lang: Language,
    go_lang: Language,
    js_lang: Language,
}

impl AstParser {
    pub fn new() -> Self {
        Self {
            rust_lang: tree_sitter_rust::language(),
            ts_lang: tree_sitter_typescript::language_typescript(),
            tsx_lang: tree_sitter_typescript::language_tsx(),
            python_lang: tree_sitter_python::language(),
            go_lang: tree_sitter_go::language(),
            js_lang: tree_sitter_javascript::language(),
        }
    }

    fn language_for(&self, ft: &FileType) -> Option<Language> {
        match ft {
            FileType::Rust => Some(self.rust_lang.clone()),
            FileType::TypeScript => Some(self.ts_lang.clone()),
            FileType::Python => Some(self.python_lang.clone()),
            FileType::Go => Some(self.go_lang.clone()),
            FileType::JavaScript => Some(self.js_lang.clone()),
            FileType::Other(_) => None,
        }
    }

    pub fn parse_file(&self, path: &Path, source: &str) -> Result<ParsedFile> {
        let ft = FileType::from_path(path);
        // tsx detection: .tsx extension
        let lang = if path.extension().and_then(|e| e.to_str()) == Some("tsx") {
            Some(self.tsx_lang.clone())
        } else {
            self.language_for(&ft)
        };

        let lang = lang.with_context(|| format!("No parser for {}", path.display()))?;

        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .context("Failed to set tree-sitter language")?;

        let tree = parser
            .parse(source, None)
            .context("tree-sitter parse returned None")?;

        Ok(ParsedFile {
            path: path.to_path_buf(),
            source: source.to_string(),
            tree,
            language: ft,
        })
    }

    pub fn extract_function_at_line(&self, file: &ParsedFile, line: u32) -> Option<FunctionInfo> {
        let fns = self.extract_all_functions(file);
        // Return the innermost function that contains the line
        fns.into_iter()
            .filter(|f| f.body_range.0 <= line && line <= f.body_range.1)
            .min_by_key(|f| f.body_range.1 - f.body_range.0)
    }

    pub fn extract_all_functions(&self, file: &ParsedFile) -> Vec<FunctionInfo> {
        match file.language {
            FileType::Rust => extract_rust_functions(file),
            FileType::TypeScript | FileType::JavaScript => extract_ts_functions(file),
            FileType::Python => extract_python_functions(file),
            FileType::Go => extract_go_functions(file),
            FileType::Other(_) => Vec::new(),
        }
    }

    pub fn extract_type_definitions(&self, file: &ParsedFile) -> Vec<TypeDef> {
        match file.language {
            FileType::Rust => extract_rust_types(file),
            FileType::TypeScript | FileType::JavaScript => extract_ts_types(file),
            FileType::Python => extract_python_types(file),
            FileType::Go => extract_go_types(file),
            FileType::Other(_) => Vec::new(),
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

fn node_start_line(node: &Node) -> u32 {
    node.start_position().row as u32 + 1
}

fn node_end_line(node: &Node) -> u32 {
    node.end_position().row as u32 + 1
}

fn find_children_by_kind<'a>(node: &'a Node, kind: &str) -> Vec<Node<'a>> {
    let mut results = Vec::new();
    let mut c = node.walk();
    for child in node.children(&mut c) {
        if child.kind() == kind {
            results.push(child);
        }
    }
    results
}

fn collect_calls(node: &Node, source: &str) -> Vec<String> {
    let mut calls = Vec::new();

    fn walk(node: &Node, source: &str, calls: &mut Vec<String>) {
        if node.kind() == "call_expression" || node.kind() == "function_call" {
            // First named child is usually the function being called
            if let Some(callee) = node.named_child(0) {
                let text = node_text(&callee, source);
                // Strip method chains: take last segment after `.`
                let name = text.split('.').last().unwrap_or(text).trim().to_string();
                if !name.is_empty() && name.chars().next().map(|c| c.is_alphabetic()).unwrap_or(false) {
                    calls.push(name);
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(&child, source, calls);
        }
    }

    walk(node, source, &mut calls);
    calls.sort();
    calls.dedup();
    calls
}

// ── Rust ─────────────────────────────────────────────────────────────────────

fn extract_rust_functions(file: &ParsedFile) -> Vec<FunctionInfo> {
    let mut fns = Vec::new();
    let root = file.tree.root_node();
    collect_rust_fns(&root, &file.source, &mut fns);
    fns
}

fn collect_rust_fns(node: &Node, source: &str, out: &mut Vec<FunctionInfo>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "function_item" {
            if let Some(info) = parse_rust_fn(&child, source) {
                out.push(info);
            }
        } else {
            collect_rust_fns(&child, source, out);
        }
    }
}

fn parse_rust_fn(node: &Node, source: &str) -> Option<FunctionInfo> {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(&n, source).to_string())?;

    // Signature: everything up to the body block
    let body = node.child_by_field_name("body");
    let sig_end = body
        .as_ref()
        .map(|b| b.start_byte())
        .unwrap_or(node.end_byte());
    let signature = source[node.start_byte()..sig_end].trim().to_string();

    let body_range = (node_start_line(node), node_end_line(node));

    let is_public = node
        .children(&mut node.walk())
        .any(|c| c.kind() == "visibility_modifier");

    // Doc comment: look at preceding siblings in parent
    let doc_comment = None; // simplified — doc extraction requires parent traversal

    let called_functions = body
        .map(|b| collect_calls(&b, source))
        .unwrap_or_default();

    Some(FunctionInfo {
        name,
        signature,
        body_range,
        called_functions,
        doc_comment,
        is_public,
    })
}

fn extract_rust_types(file: &ParsedFile) -> Vec<TypeDef> {
    let mut types = Vec::new();
    let root = file.tree.root_node();
    let source = &file.source;

    let mut cursor = root.walk();
    for node in root.children(&mut cursor) {
        match node.kind() {
            "struct_item" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let fields = find_children_by_kind(&node, "field_declaration")
                        .iter()
                        .filter_map(|f| f.child_by_field_name("name").map(|n| node_text(&n, source).to_string()))
                        .collect();
                    types.push(TypeDef {
                        name: node_text(&name, source).to_string(),
                        kind: TypeKind::Struct,
                        fields,
                        definition_range: (node_start_line(&node), node_end_line(&node)),
                    });
                }
            }
            "enum_item" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let fields = find_children_by_kind(&node, "enum_variant")
                        .iter()
                        .map(|v| {
                            v.child_by_field_name("name")
                                .map(|n| node_text(&n, source).to_string())
                                .unwrap_or_default()
                        })
                        .collect();
                    types.push(TypeDef {
                        name: node_text(&name, source).to_string(),
                        kind: TypeKind::Enum,
                        fields,
                        definition_range: (node_start_line(&node), node_end_line(&node)),
                    });
                }
            }
            "trait_item" => {
                if let Some(name) = node.child_by_field_name("name") {
                    types.push(TypeDef {
                        name: node_text(&name, source).to_string(),
                        kind: TypeKind::Trait,
                        fields: Vec::new(),
                        definition_range: (node_start_line(&node), node_end_line(&node)),
                    });
                }
            }
            "type_item" => {
                if let Some(name) = node.child_by_field_name("name") {
                    types.push(TypeDef {
                        name: node_text(&name, source).to_string(),
                        kind: TypeKind::TypeAlias,
                        fields: Vec::new(),
                        definition_range: (node_start_line(&node), node_end_line(&node)),
                    });
                }
            }
            _ => {}
        }
    }

    types
}

// ── TypeScript / JavaScript ───────────────────────────────────────────────────

fn extract_ts_functions(file: &ParsedFile) -> Vec<FunctionInfo> {
    let mut fns = Vec::new();
    collect_ts_fns(&file.tree.root_node(), &file.source, &mut fns);
    fns
}

fn collect_ts_fns(node: &Node, source: &str, out: &mut Vec<FunctionInfo>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_declaration"
            | "method_definition"
            | "arrow_function"
            | "function_expression" => {
                if let Some(info) = parse_ts_fn(&child, source) {
                    out.push(info);
                }
            }
            _ => collect_ts_fns(&child, source, out),
        }
    }
}

fn parse_ts_fn(node: &Node, source: &str) -> Option<FunctionInfo> {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "<anonymous>".to_string());

    let body = node.child_by_field_name("body");
    let sig_end = body
        .as_ref()
        .map(|b| b.start_byte())
        .unwrap_or(node.end_byte());
    let signature = source[node.start_byte()..sig_end].trim().to_string();

    let called_functions = body
        .map(|b| collect_calls(&b, source))
        .unwrap_or_default();

    Some(FunctionInfo {
        name,
        signature,
        body_range: (node_start_line(node), node_end_line(node)),
        called_functions,
        doc_comment: None,
        is_public: true,
    })
}

fn extract_ts_types(file: &ParsedFile) -> Vec<TypeDef> {
    let mut types = Vec::new();
    let root = file.tree.root_node();
    let source = &file.source;
    let mut cursor = root.walk();

    for node in root.children(&mut cursor) {
        match node.kind() {
            "interface_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let fields = find_children_by_kind(&node, "property_signature")
                        .iter()
                        .filter_map(|f| f.child_by_field_name("name").map(|n| node_text(&n, source).to_string()))
                        .collect();
                    types.push(TypeDef {
                        name: node_text(&name, source).to_string(),
                        kind: TypeKind::Interface,
                        fields,
                        definition_range: (node_start_line(&node), node_end_line(&node)),
                    });
                }
            }
            "type_alias_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    types.push(TypeDef {
                        name: node_text(&name, source).to_string(),
                        kind: TypeKind::TypeAlias,
                        fields: Vec::new(),
                        definition_range: (node_start_line(&node), node_end_line(&node)),
                    });
                }
            }
            "class_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    types.push(TypeDef {
                        name: node_text(&name, source).to_string(),
                        kind: TypeKind::Class,
                        fields: Vec::new(),
                        definition_range: (node_start_line(&node), node_end_line(&node)),
                    });
                }
            }
            _ => {}
        }
    }

    types
}

// ── Python ────────────────────────────────────────────────────────────────────

fn extract_python_functions(file: &ParsedFile) -> Vec<FunctionInfo> {
    let mut fns = Vec::new();
    collect_python_fns(&file.tree.root_node(), &file.source, &mut fns);
    fns
}

fn collect_python_fns(node: &Node, source: &str, out: &mut Vec<FunctionInfo>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "function_definition" {
            if let Some(info) = parse_python_fn(&child, source) {
                out.push(info);
            }
        } else {
            collect_python_fns(&child, source, out);
        }
    }
}

fn parse_python_fn(node: &Node, source: &str) -> Option<FunctionInfo> {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(&n, source).to_string())?;

    let body = node.child_by_field_name("body");
    let sig_end = body
        .as_ref()
        .map(|b| b.start_byte())
        .unwrap_or(node.end_byte());
    let signature = source[node.start_byte()..sig_end].trim().to_string();

    let called_functions = body
        .map(|b| collect_calls(&b, source))
        .unwrap_or_default();

    Some(FunctionInfo {
        name,
        signature,
        body_range: (node_start_line(node), node_end_line(node)),
        called_functions,
        doc_comment: None,
        is_public: true,
    })
}

fn extract_python_types(file: &ParsedFile) -> Vec<TypeDef> {
    let mut types = Vec::new();
    let root = file.tree.root_node();
    let source = &file.source;
    let mut cursor = root.walk();

    for node in root.children(&mut cursor) {
        if node.kind() == "class_definition" {
            if let Some(name) = node.child_by_field_name("name") {
                types.push(TypeDef {
                    name: node_text(&name, source).to_string(),
                    kind: TypeKind::Class,
                    fields: Vec::new(),
                    definition_range: (node_start_line(&node), node_end_line(&node)),
                });
            }
        }
    }

    types
}

// ── Go ────────────────────────────────────────────────────────────────────────

fn extract_go_functions(file: &ParsedFile) -> Vec<FunctionInfo> {
    let mut fns = Vec::new();
    let root = file.tree.root_node();
    let source = &file.source;
    let mut cursor = root.walk();

    for node in root.children(&mut cursor) {
        if node.kind() == "function_declaration" || node.kind() == "method_declaration" {
            if let Some(name) = node.child_by_field_name("name") {
                let body = node.child_by_field_name("body");
                let sig_end = body.as_ref().map(|b| b.start_byte()).unwrap_or(node.end_byte());
                let signature = source[node.start_byte()..sig_end].trim().to_string();
                let called_functions = body
                    .map(|b| collect_calls(&b, source))
                    .unwrap_or_default();

                fns.push(FunctionInfo {
                    name: node_text(&name, source).to_string(),
                    signature,
                    body_range: (node_start_line(&node), node_end_line(&node)),
                    called_functions,
                    doc_comment: None,
                    is_public: node_text(&name, source)
                        .chars()
                        .next()
                        .map(|c| c.is_uppercase())
                        .unwrap_or(false),
                });
            }
        }
    }

    fns
}

fn extract_go_types(file: &ParsedFile) -> Vec<TypeDef> {
    let mut types = Vec::new();
    let root = file.tree.root_node();
    let source = &file.source;
    let mut cursor = root.walk();

    for node in root.children(&mut cursor) {
        if node.kind() == "type_declaration" {
            let mut inner = node.walk();
            for spec in node.children(&mut inner) {
                if spec.kind() == "type_spec" {
                    if let Some(name) = spec.child_by_field_name("name") {
                        let kind = spec
                            .child_by_field_name("type")
                            .map(|t| match t.kind() {
                                "struct_type" => TypeKind::Struct,
                                "interface_type" => TypeKind::Interface,
                                _ => TypeKind::TypeAlias,
                            })
                            .unwrap_or(TypeKind::TypeAlias);

                        types.push(TypeDef {
                            name: node_text(&name, source).to_string(),
                            kind,
                            fields: Vec::new(),
                            definition_range: (node_start_line(&node), node_end_line(&node)),
                        });
                    }
                }
            }
        }
    }

    types
}
