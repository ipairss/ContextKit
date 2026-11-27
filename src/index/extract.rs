//! Turns file contents into symbols, imports, references and searchable chunks.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use tree_sitter::{
    Node, ParseOptions, ParseState, Parser, Point, Query, QueryCursor, StreamingIterator, Tree,
};

use super::lang::{FileKind, Lang};
use super::manifest::{self, Dependency};
use crate::error::{Error, Result};
use crate::util::{collapse_whitespace, truncate_on_char_boundary};

/// Lines per searchable chunk.
const CHUNK_LINES: usize = 40;
/// Largest chunk body stored for full-text search.
const MAX_CHUNK_BYTES: usize = 16 * 1024;
/// Distinct call targets recorded per file.
const MAX_REFS_PER_FILE: usize = 2_000;
const MAX_SIGNATURE_BYTES: usize = 200;
/// Upper bound on parsing one file, so a pathological input cannot stall indexing.
const PARSE_TIME_LIMIT: Duration = Duration::from_secs(2);

/// Parses `text`, giving up once `limit` has elapsed.
fn parse_with_deadline(parser: &mut Parser, text: &str, limit: Duration) -> Option<Tree> {
    let started = Instant::now();
    let mut check = |_: &ParseState| {
        if started.elapsed() > limit {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let bytes = text.as_bytes();
    let mut read = |offset: usize, _: Point| bytes.get(offset..).unwrap_or_default();
    parser.parse_with_options(
        &mut read,
        None,
        Some(ParseOptions::new().progress_callback(&mut check)),
    )
}

/// Kind of an extracted symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Interface,
    Trait,
    Type,
    Constant,
    Module,
    Macro,
    Impl,
    Heading,
}

impl SymbolKind {
    pub const ALL: [SymbolKind; 13] = [
        SymbolKind::Function,
        SymbolKind::Method,
        SymbolKind::Class,
        SymbolKind::Struct,
        SymbolKind::Enum,
        SymbolKind::Interface,
        SymbolKind::Trait,
        SymbolKind::Type,
        SymbolKind::Constant,
        SymbolKind::Module,
        SymbolKind::Macro,
        SymbolKind::Impl,
        SymbolKind::Heading,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Class => "class",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Interface => "interface",
            SymbolKind::Trait => "trait",
            SymbolKind::Type => "type",
            SymbolKind::Constant => "constant",
            SymbolKind::Module => "module",
            SymbolKind::Macro => "macro",
            SymbolKind::Impl => "impl",
            SymbolKind::Heading => "heading",
        }
    }
}

impl fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SymbolKind {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| {
                let names: Vec<&str> = Self::ALL.iter().map(|k| k.as_str()).collect();
                Error::InvalidInput(format!(
                    "unknown symbol kind `{s}`; expected one of: {}",
                    names.join(", ")
                ))
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub container: Option<String>,
    /// 1-based, inclusive.
    pub start_line: u32,
    /// 1-based, inclusive.
    pub end_line: u32,
    pub signature: String,
    pub exported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    pub module: String,
    pub kind: ImportKind,
    pub line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    /// `import`, `use`, `require(...)`, `from x import y`.
    Import,
    /// Rust `mod foo;` declaration referring to another file.
    ModDecl,
}

impl ImportKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            ImportKind::Import => "import",
            ImportKind::ModDecl => "mod",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub name: String,
    pub first_line: u32,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub start_line: u32,
    pub end_line: u32,
    pub content: String,
    /// Space-separated names of symbols that start inside the chunk.
    pub symbols: String,
    /// Space-separated sub-words of camelCase identifiers, so `getUserName` matches `user`.
    pub terms: String,
}

/// Everything extracted from one file.
#[derive(Debug, Default)]
pub struct Analysis {
    pub line_count: u32,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<Import>,
    pub references: Vec<Reference>,
    pub dependencies: Vec<Dependency>,
    pub chunks: Vec<Chunk>,
    /// Non-fatal problems worth reporting (unparseable manifest, parser timeout, ...).
    pub warnings: Vec<String>,
}

struct LangState {
    parser: Parser,
    query: Query,
    name_idx: Option<u32>,
}

/// Reusable per-thread extraction state. Parsers and compiled queries are created lazily.
#[derive(Default)]
pub struct Extractor {
    langs: HashMap<Lang, LangState>,
}

impl fmt::Debug for Extractor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Extractor")
            .field("languages", &self.langs.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Extractor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Analyzes `text`, the contents of the file at project-relative `path`.
    pub fn analyze(&mut self, path: &str, kind: FileKind, text: &str) -> Result<Analysis> {
        let mut analysis = Analysis {
            line_count: count_lines(text),
            ..Analysis::default()
        };
        if let Some(lang) = Lang::from_path(path) {
            self.extract_code(lang, text, &mut analysis)?;
        } else if kind == FileKind::Doc && is_markdown(path) {
            analysis.symbols = markdown_headings(text);
        }
        if kind == FileKind::Manifest {
            match manifest::parse(path, text) {
                Ok(deps) => analysis.dependencies = deps,
                Err(message) => analysis.warnings.push(message),
            }
        }
        analysis.chunks = build_chunks(text, &analysis.symbols);
        Ok(analysis)
    }

    fn state(&mut self, lang: Lang) -> Result<&mut LangState> {
        if let std::collections::hash_map::Entry::Vacant(e) = self.langs.entry(lang) {
            let grammar = lang.grammar();
            let mut parser = Parser::new();
            parser.set_language(&grammar).map_err(|e| Error::Grammar {
                language: lang.name(),
                message: e.to_string(),
            })?;
            let query = Query::new(&grammar, lang.query()).map_err(|e| Error::Grammar {
                language: lang.name(),
                message: e.to_string(),
            })?;
            let name_idx = query.capture_index_for_name("name");
            e.insert(LangState {
                parser,
                query,
                name_idx,
            });
        }
        self.langs.get_mut(&lang).ok_or_else(|| Error::Grammar {
            language: lang.name(),
            message: "parser state missing after initialization".into(),
        })
    }

    fn extract_code(&mut self, lang: Lang, text: &str, out: &mut Analysis) -> Result<()> {
        let state = self.state(lang)?;
        let Some(tree) = parse_with_deadline(&mut state.parser, text, PARSE_TIME_LIMIT) else {
            // Cancelled parses leave the parser mid-document; reset before the next file.
            state.parser.reset();
            out.warnings.push(format!(
                "{lang} parsing exceeded {} ms or failed; symbols not extracted \
                 (the file is still full-text searchable)",
                PARSE_TIME_LIMIT.as_millis()
            ));
            return Ok(());
        };
        if tree.root_node().has_error() {
            // Still useful: tree-sitter recovers locally, so most definitions survive.
            tracing::debug!(language = %lang, "syntax errors in file; extraction is best-effort");
        }

        let source = text.as_bytes();
        let names = state.query.capture_names();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&state.query, tree.root_node(), source);
        let mut refs: HashMap<&str, (u32, u32)> = HashMap::new();

        while let Some(m) = matches.next() {
            let mut def: Option<(Node<'_>, SymbolKind)> = None;
            let mut name_node: Option<Node<'_>> = None;
            for capture in m.captures() {
                let capture_name = names[capture.index as usize];
                let node = capture.node;
                if Some(capture.index) == state.name_idx {
                    name_node = Some(node);
                } else if let Some(kind) = capture_name.strip_prefix("def.") {
                    def = def_kind(kind).map(|k| (node, k));
                } else if capture_name == "import" || capture_name == "import.mod" {
                    let module = clean_module(node_text(node, text));
                    if !module.is_empty() {
                        out.imports.push(Import {
                            module,
                            kind: if capture_name == "import.mod" {
                                ImportKind::ModDecl
                            } else {
                                ImportKind::Import
                            },
                            line: line_of(node),
                        });
                    }
                } else if capture_name == "ref" {
                    let name = node_text(node, text);
                    if refs.len() < MAX_REFS_PER_FILE || refs.contains_key(name) {
                        let entry = refs.entry(name).or_insert((line_of(node), 0));
                        entry.1 += 1;
                    }
                }
            }
            if let (Some((node, kind)), Some(name_node)) = (def, name_node)
                && let Some(symbol) = build_symbol(lang, node, name_node, kind, text)
            {
                out.symbols.push(symbol);
            }
        }

        out.symbols.sort_by(|a, b| {
            (a.start_line, a.end_line, a.kind, &a.name).cmp(&(
                b.start_line,
                b.end_line,
                b.kind,
                &b.name,
            ))
        });
        out.symbols
            .dedup_by(|a, b| a.start_line == b.start_line && a.name == b.name);
        out.imports
            .sort_by(|a, b| (a.line, &a.module).cmp(&(b.line, &b.module)));
        out.imports
            .dedup_by(|a, b| a.line == b.line && a.module == b.module);

        out.references = refs
            .into_iter()
            .map(|(name, (first_line, count))| Reference {
                name: name.to_owned(),
                first_line,
                count,
            })
            .collect();
        out.references.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(())
    }
}

fn def_kind(s: &str) -> Option<SymbolKind> {
    s.parse().ok()
}

fn build_symbol(
    lang: Lang,
    node: Node<'_>,
    name_node: Node<'_>,
    mut kind: SymbolKind,
    text: &str,
) -> Option<Symbol> {
    let raw_name = node_text(name_node, text);
    let name = match kind {
        // `impl<T> Display for Wrapper<T>`: report the self type without generics.
        SymbolKind::Impl => strip_generics(raw_name),
        // Luau `type Foo<T> = ...` captures the whole `Foo<T>` type node.
        SymbolKind::Type if lang == Lang::Luau => strip_generics(raw_name),
        _ => raw_name.trim(),
    };
    if name.is_empty() {
        return None;
    }

    let container = find_container(lang, node, text);
    if let Some((_, methods)) = &container
        && *methods
        && kind == SymbolKind::Function
    {
        kind = SymbolKind::Method;
    }

    Some(Symbol {
        name: name.to_owned(),
        kind,
        container: container.map(|(name, _)| name),
        start_line: line_of(node),
        end_line: end_line_of(node),
        signature: signature(node_text(node, text)),
        exported: is_exported(lang, node, name),
    })
}

fn find_container(lang: Lang, node: Node<'_>, text: &str) -> Option<(String, bool)> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if let Some(rule) = lang.container(parent.kind()) {
            let name = parent
                .child_by_field_name(rule.name_field)
                .map(|n| strip_generics(node_text(n, text)).to_owned())
                .unwrap_or_default();
            if !name.is_empty() {
                return Some((name, rule.methods));
            }
        }
        // Python functions nested in functions are local helpers, not methods of an outer class.
        if lang == Lang::Python && parent.kind() == "function_definition" {
            return None;
        }
        current = parent.parent();
    }
    None
}

fn is_exported(lang: Lang, node: Node<'_>, name: &str) -> bool {
    match lang {
        Lang::Rust => {
            let mut walker = node.walk();
            let has_pub = node
                .children(&mut walker)
                .any(|c| c.kind() == "visibility_modifier");
            has_pub || node.kind() == "impl_item"
        }
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
            let mut current = node.parent();
            while let Some(p) = current {
                if p.kind() == "export_statement" {
                    return true;
                }
                if p.kind() == "program" {
                    break;
                }
                current = p.parent();
            }
            false
        }
        Lang::Python => !name.starts_with('_'),
        Lang::Lua | Lang::Luau => {
            let first = node.child(0).map(|c| c.kind());
            match node.kind() {
                "type_definition" => first == Some("export"),
                _ => first != Some("local"),
            }
        }
    }
}

fn node_text<'a>(node: Node<'_>, text: &'a str) -> &'a str {
    text.get(node.byte_range()).unwrap_or("")
}

fn line_of(node: Node<'_>) -> u32 {
    u32::try_from(node.start_position().row + 1).unwrap_or(u32::MAX)
}

fn end_line_of(node: Node<'_>) -> u32 {
    let end = node.end_position();
    // A node ending at column 0 actually ends on the previous line.
    let row = if end.column == 0 && end.row > node.start_position().row {
        end.row
    } else {
        end.row + 1
    };
    u32::try_from(row).unwrap_or(u32::MAX)
}

fn strip_generics(s: &str) -> &str {
    let s = s.trim();
    let end = s.find(['<', '(', ' ']).unwrap_or(s.len());
    &s[..end]
}

fn signature(def_text: &str) -> String {
    let first_line = def_text.lines().next().unwrap_or("");
    let trimmed = first_line.trim().trim_end_matches('{').trim_end();
    let sig = truncate_on_char_boundary(trimmed, MAX_SIGNATURE_BYTES);
    if sig.len() < trimmed.len() {
        format!("{sig}…")
    } else {
        sig.to_owned()
    }
}

/// Strips quotes from string literals and normalizes whitespace in paths like `crate::{a, b}`.
fn clean_module(raw: &str) -> String {
    let trimmed = raw.trim();
    let unquoted = if let Some(inner) = trimmed
        .strip_prefix("[[")
        .and_then(|s| s.strip_suffix("]]"))
    {
        inner
    } else {
        trimmed.trim_matches(|c| c == '"' || c == '\'' || c == '`')
    };
    collapse_whitespace(unquoted)
}

fn count_lines(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let newlines = text.bytes().filter(|&b| b == b'\n').count();
    let lines = if text.ends_with('\n') {
        newlines
    } else {
        newlines + 1
    };
    u32::try_from(lines).unwrap_or(u32::MAX)
}

fn is_markdown(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown") || lower.ends_with(".mdx")
}

/// Extracts ATX headings (`# Title`) outside fenced code blocks.
fn markdown_headings(text: &str) -> Vec<Symbol> {
    let mut headings: Vec<(usize, u32, String)> = Vec::new();
    let mut in_fence = false;
    let mut total = 0u32;
    for (i, line) in text.lines().enumerate() {
        total = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || !trimmed.starts_with('#') {
            continue;
        }
        let level = trimmed.bytes().take_while(|&b| b == b'#').count();
        let rest = &trimmed[level..];
        if level > 6 || !(rest.is_empty() || rest.starts_with(' ')) {
            continue;
        }
        let title = rest.trim().trim_end_matches('#').trim();
        if !title.is_empty() {
            headings.push((level, total, title.to_owned()));
        }
    }
    let mut symbols = Vec::with_capacity(headings.len());
    for (idx, (level, line, title)) in headings.iter().enumerate() {
        let end = headings[idx + 1..]
            .iter()
            .find(|(l, _, _)| l <= level)
            .map_or(total, |(_, next, _)| next.saturating_sub(1).max(*line));
        symbols.push(Symbol {
            name: title.clone(),
            kind: SymbolKind::Heading,
            container: None,
            start_line: *line,
            end_line: end,
            signature: format!("{} {title}", "#".repeat(*level)),
            exported: true,
        });
    }
    symbols
}

fn build_chunks(text: &str, symbols: &[Symbol]) -> Vec<Chunk> {
    let lines: Vec<&str> = text.lines().collect();
    let mut chunks = Vec::with_capacity(lines.len() / CHUNK_LINES + 1);
    for (index, window) in lines.chunks(CHUNK_LINES).enumerate() {
        let start = index * CHUNK_LINES + 1;
        let end = start + window.len() - 1;
        let joined = window.join("\n");
        if joined.trim().is_empty() {
            continue;
        }
        let content = truncate_on_char_boundary(&joined, MAX_CHUNK_BYTES).to_owned();
        let (start_line, end_line) = (
            u32::try_from(start).unwrap_or(u32::MAX),
            u32::try_from(end).unwrap_or(u32::MAX),
        );
        let symbol_names: Vec<&str> = symbols
            .iter()
            .filter(|s| s.start_line >= start_line && s.start_line <= end_line)
            .map(|s| s.name.as_str())
            .collect();
        chunks.push(Chunk {
            start_line,
            end_line,
            terms: identifier_subwords(&content),
            symbols: symbol_names.join(" "),
            content,
        });
    }
    chunks
}

/// Splits camelCase and PascalCase identifiers into lowercase sub-words.
/// snake_case needs no help: the FTS tokenizer already splits on `_`.
pub fn identifier_subwords(text: &str) -> String {
    let mut seen: Vec<String> = Vec::new();
    for ident in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if ident.len() < 4 || !has_case_transition(ident) {
            continue;
        }
        for part in split_camel(ident) {
            if part.len() >= 2 && !seen.contains(&part) {
                seen.push(part);
            }
        }
    }
    seen.join(" ")
}

fn has_case_transition(s: &str) -> bool {
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_uppercase() && prev_lower {
            return true;
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
    }
    false
}

/// `parseHTTPResponse` → `["parse", "http", "response"]`.
pub fn split_camel(ident: &str) -> Vec<String> {
    let chars: Vec<char> = ident.chars().collect();
    let mut parts = Vec::new();
    let mut current = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' || c == '-' {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current).to_lowercase());
            }
            continue;
        }
        let boundary = c.is_uppercase()
            && !current.is_empty()
            && (chars[i - 1].is_lowercase()
                || chars[i - 1].is_ascii_digit()
                || chars.get(i + 1).is_some_and(|n| n.is_lowercase()));
        if boundary {
            parts.push(std::mem::take(&mut current).to_lowercase());
        }
        current.push(c);
    }
    if !current.is_empty() {
        parts.push(current.to_lowercase());
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze(path: &str, text: &str) -> Analysis {
        Extractor::new()
            .analyze(path, FileKind::detect(path), text)
            .expect("analysis")
    }

    fn names(a: &Analysis) -> Vec<(String, SymbolKind, Option<String>)> {
        a.symbols
            .iter()
            .map(|s| (s.name.clone(), s.kind, s.container.clone()))
            .collect()
    }

    #[test]
    fn rust_symbols_imports_and_refs() {
        let src = r#"use std::collections::HashMap;
mod parser;
pub struct Config { a: u32 }
impl Config {
    pub fn load() -> Self { helper(); Self { a: 1 } }
}
trait Shape { fn area(&self) -> f64; }
fn helper() {}
"#;
        let a = analyze("src/lib.rs", src);
        let n = names(&a);
        assert!(n.contains(&("Config".into(), SymbolKind::Struct, None)));
        assert!(n.contains(&("Config".into(), SymbolKind::Impl, None)));
        assert!(n.contains(&("load".into(), SymbolKind::Method, Some("Config".into()))));
        assert!(n.contains(&("area".into(), SymbolKind::Method, Some("Shape".into()))));
        assert!(n.contains(&("helper".into(), SymbolKind::Function, None)));
        let config = a
            .symbols
            .iter()
            .find(|s| s.name == "Config" && s.kind == SymbolKind::Struct);
        assert!(config.is_some_and(|s| s.exported));
        assert!(a.symbols.iter().any(|s| s.name == "helper" && !s.exported));
        assert_eq!(a.imports[0].module, "std::collections::HashMap");
        assert!(
            a.imports
                .iter()
                .any(|i| i.module == "parser" && i.kind == ImportKind::ModDecl)
        );
        assert!(a.references.iter().any(|r| r.name == "helper"));
        let load = a.symbols.iter().find(|s| s.name == "load");
        assert_eq!(load.map(|s| (s.start_line, s.end_line)), Some((5, 5)));
    }

    #[test]
    fn typescript_symbols() {
        let src = r#"import { x } from "./util";
export interface User { id: string }
export class Service {
  getUser(id: string): User { return fetchUser(id); }
}
export const handler = async () => {};
type Alias = string;
"#;
        let a = analyze("src/service.ts", src);
        let n = names(&a);
        assert!(n.contains(&("User".into(), SymbolKind::Interface, None)));
        assert!(n.contains(&("Service".into(), SymbolKind::Class, None)));
        assert!(n.contains(&("getUser".into(), SymbolKind::Method, Some("Service".into()))));
        assert!(n.contains(&("handler".into(), SymbolKind::Function, None)));
        assert!(n.contains(&("Alias".into(), SymbolKind::Type, None)));
        assert_eq!(a.imports[0].module, "./util");
        assert!(a.symbols.iter().any(|s| s.name == "Service" && s.exported));
        assert!(a.symbols.iter().any(|s| s.name == "Alias" && !s.exported));
    }

    #[test]
    fn javascript_require_and_class() {
        let src = "const fs = require('fs');\nclass A { run() { go(); } }\nfunction b() {}\n";
        let a = analyze("index.js", src);
        assert!(a.imports.iter().any(|i| i.module == "fs"));
        assert!(names(&a).contains(&("run".into(), SymbolKind::Method, Some("A".into()))));
    }

    #[test]
    fn python_symbols() {
        let src = "import os\nfrom .models import User\n\nclass Repo:\n    def get(self):\n        def inner():\n            pass\n        return load()\n\ndef _private():\n    pass\n";
        let a = analyze("app/repo.py", src);
        let n = names(&a);
        assert!(n.contains(&("Repo".into(), SymbolKind::Class, None)));
        assert!(n.contains(&("get".into(), SymbolKind::Method, Some("Repo".into()))));
        assert!(n.contains(&("inner".into(), SymbolKind::Function, None)));
        assert!(
            a.symbols
                .iter()
                .any(|s| s.name == "_private" && !s.exported)
        );
        let modules: Vec<&str> = a.imports.iter().map(|i| i.module.as_str()).collect();
        assert_eq!(modules, vec!["os", ".models"]);
    }

    #[test]
    fn lua_and_luau_symbols() {
        let lua = "local M = {}\nlocal util = require(\"lib.util\")\nfunction M.start() util.go() end\nfunction M:stop() end\nlocal function helper() end\nreturn M\n";
        let a = analyze("src/mod.lua", lua);
        let n = names(&a);
        assert!(
            n.iter()
                .any(|(name, kind, _)| name == "start" && *kind == SymbolKind::Function)
        );
        assert!(
            n.iter()
                .any(|(name, kind, _)| name == "stop" && *kind == SymbolKind::Method)
        );
        assert!(a.symbols.iter().any(|s| s.name == "helper" && !s.exported));
        assert!(a.imports.iter().any(|i| i.module == "lib.util"));

        let luau = "export type Point = { x: number }\nlocal Signal = require(\"./Signal\")\nfunction connect(cb: () -> ()) end\n";
        let b = analyze("src/net.luau", luau);
        assert!(
            b.symbols
                .iter()
                .any(|s| s.name == "Point" && s.kind == SymbolKind::Type && s.exported)
        );
        assert!(b.symbols.iter().any(|s| s.name == "connect"));
        assert!(b.imports.iter().any(|i| i.module == "./Signal"));
    }

    #[test]
    fn markdown_headings_ignore_fences() {
        let md = "# Title\nintro\n## Setup\n```sh\n# not a heading\n```\n## Usage\ntext\n";
        let a = analyze("README.md", md);
        let n: Vec<(&str, u32, u32)> = a
            .symbols
            .iter()
            .map(|s| (s.name.as_str(), s.start_line, s.end_line))
            .collect();
        assert_eq!(n, vec![("Title", 1, 8), ("Setup", 3, 6), ("Usage", 7, 8)]);
    }

    #[test]
    fn splits_identifiers() {
        assert_eq!(
            split_camel("parseHTTPResponse"),
            vec!["parse", "http", "response"]
        );
        assert_eq!(split_camel("getUserName"), vec!["get", "user", "name"]);
        assert_eq!(
            identifier_subwords("let getUserName = snake_case;"),
            "get user name"
        );
    }

    #[test]
    fn chunks_cover_file() {
        let text: String = (1..=85).map(|i| format!("line {i}\n")).collect();
        let a = analyze("notes.txt", &text);
        let ranges: Vec<(u32, u32)> = a
            .chunks
            .iter()
            .map(|c| (c.start_line, c.end_line))
            .collect();
        assert_eq!(ranges, vec![(1, 40), (41, 80), (81, 85)]);
        assert_eq!(a.line_count, 85);
    }
}
