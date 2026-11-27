//! Deterministic, budgeted context retrieval.
//!
//! Candidates come from four sources: verified project rules, memory full-text hits,
//! symbol name matches, and code full-text hits. Each candidate gets a score from
//! simple, documented signals (see [`weights`]). Candidates are then sorted and packed
//! greedily into the token budget. The same index and query always produce the same output.

use crate::files::SafeReader;
use std::collections::HashMap;
use std::fmt::Write as _;

use rusqlite::Connection;
use serde::Serialize;

use crate::error::Result;
use crate::index::lang::Lang;
use crate::memory::{self, Category, Filter, Memory, Status};
use crate::search::{self, NameMatch, QueryTerms, fts_from_words};
use crate::tokens::{TokenCount, estimate};
use crate::util::format_rfc3339;

/// Scoring weights. Kept together so the ranking can be read and tuned in one place.
pub mod weights {
    /// Verified instructions and conventions: always considered first.
    pub const PINNED_RULE: f64 = 2.0;
    /// A symbol whose name equals a query identifier.
    pub const SYMBOL_EXACT: f64 = 1.2;
    /// A symbol whose name equals a plain English query word (`detect`, `files`).
    pub const SYMBOL_WORD: f64 = 0.5;
    /// Per distinct query word contained in a symbol name, capped at three words.
    pub const SYMBOL_PARTIAL: f64 = 0.35;
    /// Best code full-text hit; others scale down by relative BM25.
    pub const CODE_TEXT: f64 = 1.0;
    /// Best memory full-text hit; others scale down by relative BM25.
    pub const MEMORY_TEXT: f64 = 1.0;
    /// Multiplier for memory that a human has not verified.
    pub const UNVERIFIED_FACTOR: f64 = 0.75;
    /// Per query word found in a file path, capped at three words.
    pub const PATH_WORD: f64 = 0.15;
    /// Candidate lies under a caller-supplied focus path.
    pub const FOCUS_PATH: f64 = 0.6;
    /// Candidate's file imports, or is imported by, one of the top files.
    pub const IMPORT_NEIGHBOR: f64 = 0.1;
    /// Memory linked to one of the top files.
    pub const MEMORY_LINKED: f64 = 0.25;
    /// Memory updated recently; decays with a 30-day half-life.
    pub const MEMORY_RECENCY: f64 = 0.1;
    /// Lists of related files.
    pub const RELATED: f64 = 0.05;
    /// Open tasks and known bugs that do not match the task text.
    pub const OPEN_WORK: f64 = 0.35;
    /// Multiplier for test code, unless the task is about tests.
    pub const TEST_CODE_FACTOR: f64 = 0.5;
}

/// Lines of surrounding context kept around full-text matches.
const MATCH_CONTEXT_LINES: u32 = 3;
/// Snippets shorter than this are dropped rather than truncated to fit the budget.
const MIN_TRUNCATED_LINES: usize = 6;
const TOP_FILES_FOR_RELATIONS: usize = 5;

/// What to retrieve.
#[derive(Debug, Clone)]
pub struct ContextRequest {
    pub task: String,
    /// Estimated-token budget for the rendered result.
    pub budget_tokens: usize,
    /// Boost results under these project-relative paths.
    pub focus_paths: Vec<String>,
    pub include_code: bool,
    pub include_memory: bool,
    /// Longest snippet for a single item.
    pub max_snippet_lines: usize,
}

/// A retrieved context bundle.
#[derive(Debug, Clone, Serialize)]
pub struct ContextPack {
    pub task: String,
    pub budget: TokenCount,
    pub used: TokenCount,
    pub items: Vec<ContextItem>,
    /// Relevant candidates left out because they did not fit the budget.
    pub omitted: usize,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextItem {
    pub score: f64,
    pub reasons: Vec<String>,
    /// Estimated tokens of this item's Markdown rendering.
    pub tokens: usize,
    #[serde(flatten)]
    pub body: ItemBody,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ItemBody {
    Memory {
        id: String,
        category: Category,
        status: Status,
        source: String,
        updated_at: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        updated_by: Option<String>,
        title: String,
        body: String,
        paths: Vec<String>,
    },
    Code {
        path: String,
        language: Option<String>,
        start_line: u32,
        end_line: u32,
        /// `kind name` of the symbol, when the range is a definition.
        symbol: Option<String>,
        truncated: bool,
        content: String,
    },
    Related {
        path: String,
        imports: Vec<String>,
        imported_by: Vec<String>,
    },
}

#[derive(Debug, Clone)]
enum Source {
    Memory(Box<Memory>),
    Range {
        path: String,
        language: Option<String>,
        start: u32,
        end: u32,
        symbol: Option<String>,
    },
    Related {
        path: String,
        imports: Vec<String>,
        imported_by: Vec<String>,
    },
}

#[derive(Debug, Clone)]
struct Candidate {
    score: f64,
    reasons: Vec<String>,
    source: Source,
}

impl Candidate {
    fn path(&self) -> Option<&str> {
        match &self.source {
            Source::Range { path, .. } | Source::Related { path, .. } => Some(path),
            Source::Memory(_) => None,
        }
    }

    fn sort_key(&self) -> (&str, u32) {
        match &self.source {
            Source::Memory(m) => (m.id.as_str(), 0),
            Source::Range { path, start, .. } => (path.as_str(), *start),
            Source::Related { path, .. } => (path.as_str(), u32::MAX),
        }
    }
}

/// Retrieves context for `request`. File contents are read through `reader`.
pub fn retrieve(
    conn: &Connection,
    reader: &SafeReader,
    request: &ContextRequest,
) -> Result<ContextPack> {
    let terms = QueryTerms::parse(&request.task);
    let fts = fts_from_words(&terms.words);
    let mut notes = Vec::new();
    if terms.is_empty() {
        notes.push("the task contains no searchable terms; only project rules are included".into());
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    if request.include_memory {
        memory_candidates(conn, fts.as_deref(), &mut candidates)?;
    }
    if request.include_code && !terms.is_empty() {
        code_candidates(conn, &terms, fts.as_deref(), request, &mut candidates)?;
    }
    apply_path_signals(&terms, request, &mut candidates);
    let mut candidates = merge_overlapping(candidates);
    if request.include_code {
        apply_relations(conn, &mut candidates)?;
    }

    candidates.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.sort_key().cmp(&b.sort_key()))
    });

    pack(reader, request, candidates, notes)
}

fn memory_candidates(conn: &Connection, fts: Option<&str>, out: &mut Vec<Candidate>) -> Result<()> {
    let pinned = memory::list(
        conn,
        &Filter {
            categories: vec![Category::Instruction, Category::Convention],
            status: Some(Status::Verified),
            limit: Some(20),
            ..Filter::default()
        },
    )?;
    let now = crate::util::now_ms();
    for m in pinned {
        out.push(Candidate {
            score: weights::PINNED_RULE + recency(now, m.updated_at),
            reasons: vec![format!("verified {} (always included)", m.category)],
            source: Source::Memory(Box::new(m)),
        });
    }
    let hits = match fts {
        Some(fts) => memory::search_fts(conn, fts, 30)?,
        None => Vec::new(),
    };
    let best = hits.iter().map(|h| h.relevance).fold(0.0_f64, f64::max);
    for hit in hits {
        if contains_memory(out, &hit.memory.id) {
            continue;
        }
        let mut score = weights::MEMORY_TEXT * normalized(hit.relevance, best);
        let mut reasons = vec!["memory text matches the task".to_owned()];
        if hit.memory.status == Status::Unverified {
            score *= weights::UNVERIFIED_FACTOR;
            reasons.push("unverified: ranked lower".into());
        }
        score += recency(now, hit.memory.updated_at);
        out.push(Candidate {
            score,
            reasons,
            source: Source::Memory(Box::new(hit.memory)),
        });
    }

    // Unfinished work and known bugs are useful orientation for almost any task, even
    // when their wording does not match it. They rank below matching records.
    let open = memory::list(
        conn,
        &Filter {
            categories: vec![Category::TaskOpen, Category::Bug],
            limit: Some(10),
            ..Filter::default()
        },
    )?;
    for m in open {
        if contains_memory(out, &m.id) {
            continue;
        }
        let mut score = weights::OPEN_WORK + recency(now, m.updated_at);
        if m.status == Status::Unverified {
            score *= weights::UNVERIFIED_FACTOR;
        }
        let label = if m.category == Category::Bug {
            "known bug"
        } else {
            "open task"
        };
        out.push(Candidate {
            score,
            reasons: vec![format!("{label} in this project")],
            source: Source::Memory(Box::new(m)),
        });
    }
    Ok(())
}

fn contains_memory(candidates: &[Candidate], id: &str) -> bool {
    candidates
        .iter()
        .any(|c| matches!(&c.source, Source::Memory(m) if m.id == id))
}

fn recency(now_ms: i64, updated_ms: i64) -> f64 {
    let age_days = (now_ms - updated_ms).max(0) as f64 / 86_400_000.0;
    weights::MEMORY_RECENCY * 0.5_f64.powf(age_days / 30.0)
}

fn normalized(value: f64, best: f64) -> f64 {
    if best > 0.0 {
        (value / best).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn code_candidates(
    conn: &Connection,
    terms: &QueryTerms,
    fts: Option<&str>,
    request: &ContextRequest,
    out: &mut Vec<Candidate>,
) -> Result<()> {
    let max_lines = u32::try_from(request.max_snippet_lines)
        .unwrap_or(u32::MAX)
        .max(1);
    let push_symbol =
        |hit: search::SymbolHit, score: f64, reason: String, out: &mut Vec<Candidate>| {
            let end = hit
                .end_line
                .min(hit.start_line.saturating_add(max_lines - 1));
            out.push(Candidate {
                score,
                reasons: vec![reason],
                source: Source::Range {
                    symbol: Some(format!("{} {}", hit.kind, hit.name)),
                    path: hit.path,
                    language: hit.language,
                    start: hit.start_line,
                    end,
                },
            });
        };
    let mut exact_names: Vec<String> = Vec::new();
    for ident in &terms.identifiers {
        // `parse_config` in a task is almost certainly a symbol; `detect` may just be English.
        let weight = if search::looks_like_code(ident) {
            weights::SYMBOL_EXACT
        } else {
            weights::SYMBOL_WORD
        };
        for hit in search::find_symbols(conn, ident, NameMatch::Exact, None, 10)? {
            exact_names.push(hit.name.to_lowercase());
            let reason = format!("defines `{}`", hit.name);
            push_symbol(hit, weight, reason, out);
        }
    }

    // Partial matches: score by how many distinct query stems a symbol name contains,
    // so `detect_renames` beats `detect` for "detect renamed files".
    let stems: Vec<&str> = terms
        .stems()
        .into_iter()
        .filter(|s| s.chars().count() >= 4)
        .collect();
    let mut partial: Vec<(search::SymbolHit, usize)> = Vec::new();
    for s in &stems {
        for hit in search::find_symbols(conn, s, NameMatch::Contains, None, 15)? {
            let lower = hit.name.to_lowercase();
            if exact_names.contains(&lower)
                || partial.iter().any(|(h, _)| {
                    h.path == hit.path && h.start_line == hit.start_line && h.name == hit.name
                })
            {
                continue;
            }
            let matched = stems.iter().filter(|st| lower.contains(*st)).count();
            partial.push((hit, matched));
        }
    }
    for (hit, matched) in partial {
        let reason = format!("symbol `{}` matches {matched} task word(s)", hit.name);
        push_symbol(
            hit,
            weights::SYMBOL_PARTIAL * matched.min(3) as f64,
            reason,
            out,
        );
    }

    let Some(fts) = fts else { return Ok(()) };
    let hits = search::search_code_fts(conn, fts, None, 40)?;
    let best = hits.iter().map(|h| h.relevance).fold(0.0_f64, f64::max);
    for hit in hits {
        let (start, end) = narrow_to_matches(&hit.content, hit.start_line, &terms.words, max_lines);
        out.push(Candidate {
            score: weights::CODE_TEXT * normalized(hit.relevance, best),
            reasons: vec!["file text matches the task".into()],
            source: Source::Range {
                path: hit.path,
                language: hit.language,
                start,
                end,
                symbol: None,
            },
        });
    }
    apply_test_penalty(conn, terms, out)
}

/// Ranks test code below the code it tests, unless the task itself is about tests.
fn apply_test_penalty(
    conn: &Connection,
    terms: &QueryTerms,
    candidates: &mut [Candidate],
) -> Result<()> {
    if terms
        .words
        .iter()
        .any(|w| matches!(w.as_str(), "test" | "tests" | "testing" | "spec" | "specs"))
    {
        return Ok(());
    }
    let mut in_test_module = conn.prepare_cached(
        "SELECT 1 FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE f.path = ?1 AND s.kind = 'module' AND s.name IN ('tests', 'test')
           AND ?2 BETWEEN s.start_line AND s.end_line
         LIMIT 1",
    )?;
    for c in candidates.iter_mut() {
        let Source::Range { path, start, .. } = &c.source else {
            continue;
        };
        if is_test_path(path) || in_test_module.exists(rusqlite::params![path, start])? {
            c.score *= weights::TEST_CODE_FACTOR;
            c.reasons.push("test code: ranked lower".into());
        }
    }
    Ok(())
}

/// Recognizes common test file and directory naming conventions.
fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let mut segments = lower.split('/');
    let file = segments.next_back().unwrap_or("");
    let in_test_dir = lower
        .split('/')
        .rev()
        .skip(1)
        .any(|d| matches!(d, "test" | "tests" | "__tests__" | "spec" | "specs"));
    let stem = file.split('.').next().unwrap_or(file);
    in_test_dir
        || file.contains(".test.")
        || file.contains(".spec.")
        || stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_spec")
}

/// Shrinks a chunk to the lines that mention a query word, plus a little context.
fn narrow_to_matches(
    content: &str,
    chunk_start: u32,
    words: &[String],
    max_lines: u32,
) -> (u32, u32) {
    let lines: Vec<String> = content.lines().map(str::to_lowercase).collect();
    let matching: Vec<u32> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| words.iter().any(|w| line.contains(w.as_str())))
        .filter_map(|(i, _)| u32::try_from(i).ok())
        .collect();
    let last_line = chunk_start + u32::try_from(lines.len().saturating_sub(1)).unwrap_or(0);
    let (first, last) = match (matching.first(), matching.last()) {
        (Some(&f), Some(&l)) => (chunk_start + f, chunk_start + l),
        _ => (chunk_start, last_line),
    };
    let start = first.saturating_sub(MATCH_CONTEXT_LINES).max(chunk_start);
    let end = (last + MATCH_CONTEXT_LINES).min(last_line);
    (start, end.min(start.saturating_add(max_lines - 1)))
}

fn apply_path_signals(terms: &QueryTerms, request: &ContextRequest, candidates: &mut [Candidate]) {
    for c in candidates.iter_mut() {
        let Some(path) = c.path().map(str::to_owned) else {
            continue;
        };
        let lower = path.to_lowercase();
        let segments: Vec<&str> = lower
            .split(['/', '_', '-', '.'])
            .filter(|s| !s.is_empty())
            .collect();
        let matched = terms
            .words
            .iter()
            .filter(|w| segments.iter().any(|s| s == *w))
            .count()
            .min(3);
        if matched > 0 {
            c.score += weights::PATH_WORD * matched as f64;
            c.reasons.push("path matches the task".into());
        }
        if request
            .focus_paths
            .iter()
            .any(|f| path == *f || path.starts_with(&format!("{}/", f.trim_end_matches('/'))))
        {
            c.score += weights::FOCUS_PATH;
            c.reasons.push("inside a focus path".into());
        }
    }
}

/// Collapses ranges in the same file that overlap, keeping the best one and crediting it
/// with part of the others' score.
fn merge_overlapping(mut candidates: Vec<Candidate>) -> Vec<Candidate> {
    candidates.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.sort_key().cmp(&b.sort_key()))
    });
    let mut kept: Vec<Candidate> = Vec::with_capacity(candidates.len());
    for c in candidates {
        let overlap = match &c.source {
            Source::Range {
                path, start, end, ..
            } => kept.iter().position(|k| {
                matches!(&k.source, Source::Range { path: p, start: s, end: e, .. }
                    if p == path && s <= end && start <= e)
            }),
            Source::Memory(m) => kept
                .iter()
                .position(|k| matches!(&k.source, Source::Memory(o) if o.id == m.id)),
            Source::Related { .. } => None,
        };
        match overlap {
            Some(i) => {
                let target = &mut kept[i];
                target.score += 0.25 * c.score;
                for r in c.reasons {
                    if !target.reasons.contains(&r) {
                        target.reasons.push(r);
                    }
                }
            }
            None => kept.push(c),
        }
    }
    kept
}

fn apply_relations(conn: &Connection, candidates: &mut Vec<Candidate>) -> Result<()> {
    let mut top_files: Vec<String> = Vec::new();
    for c in candidates.iter() {
        if let Some(p) = c.path()
            && !top_files.iter().any(|t| t == p)
        {
            top_files.push(p.to_owned());
        }
        if top_files.len() >= TOP_FILES_FOR_RELATIONS {
            break;
        }
    }
    if top_files.is_empty() {
        return Ok(());
    }
    let mut neighbors: HashMap<String, String> = HashMap::new();
    let mut related = Vec::new();
    for (rank, file) in top_files.iter().enumerate() {
        let rel = search::file_relations(conn, file, 8)?;
        for n in rel.imports.iter().chain(&rel.imported_by) {
            neighbors.entry(n.clone()).or_insert_with(|| file.clone());
        }
        if rank < 3 && !(rel.imports.is_empty() && rel.imported_by.is_empty()) {
            related.push(Candidate {
                score: weights::RELATED,
                reasons: vec!["import graph of a top result".into()],
                source: Source::Related {
                    path: file.clone(),
                    imports: rel.imports,
                    imported_by: rel.imported_by,
                },
            });
        }
    }
    for c in candidates.iter_mut() {
        match &c.source {
            Source::Range { path, .. } => {
                if let Some(via) = neighbors.get(path) {
                    c.score += weights::IMPORT_NEIGHBOR;
                    c.reasons.push(format!("import neighbor of {via}"));
                }
            }
            Source::Memory(m) => {
                if let Some(p) = m.paths.iter().find(|p| {
                    top_files
                        .iter()
                        .any(|t| t == *p || t.starts_with(&format!("{}/", p.trim_end_matches('/'))))
                }) {
                    c.score += weights::MEMORY_LINKED;
                    c.reasons.push(format!("linked to {p}"));
                }
            }
            Source::Related { .. } => {}
        }
    }
    candidates.extend(related);
    Ok(())
}

/// Reads files lazily and caches their lines for the duration of one retrieval.
struct FileCache<'a> {
    reader: &'a SafeReader,
    files: HashMap<String, Option<Vec<String>>>,
}

impl FileCache<'_> {
    fn lines(&mut self, path: &str) -> Option<&[String]> {
        let reader = self.reader;
        self.files
            .entry(path.to_owned())
            .or_insert_with(|| match reader.read(path) {
                Ok(text) => Some(text.lines().map(str::to_owned).collect()),
                Err(e) => {
                    // Skipping is the safe default: the file moved, changed, gained a secret,
                    // or now resolves outside the project.
                    tracing::warn!(path, error = %e, "indexed file not included in context");
                    None
                }
            })
            .as_deref()
    }
}

fn pack(
    reader: &SafeReader,
    request: &ContextRequest,
    candidates: Vec<Candidate>,
    mut notes: Vec<String>,
) -> Result<ContextPack> {
    let header = render_header(&request.task, request.budget_tokens);
    let mut used = estimate(&header);
    let mut items = Vec::new();
    let mut omitted = 0usize;
    let mut cache = FileCache {
        reader,
        files: HashMap::new(),
    };

    for c in candidates {
        let remaining = request.budget_tokens.saturating_sub(used);
        let Some(body) = materialize(&mut cache, &c.source, None) else {
            continue;
        };
        let mut item = ContextItem {
            score: round3(c.score),
            reasons: c.reasons,
            tokens: 0,
            body,
        };
        let mut tokens = estimate(&render_item(&item));
        if tokens > remaining {
            // Try a shorter snippet before giving up on a code item.
            match shrink_to_fit(&mut cache, &c.source, &item, remaining) {
                Some((body, shrunk_tokens)) => {
                    item.body = body;
                    tokens = shrunk_tokens;
                }
                None => {
                    omitted += 1;
                    continue;
                }
            }
        }
        item.tokens = tokens;
        used += tokens;
        items.push(item);
    }
    if omitted > 0 {
        notes.push(format!(
            "{omitted} relevant item(s) did not fit the budget; raise the budget or narrow the task"
        ));
    }
    Ok(ContextPack {
        task: request.task.clone(),
        budget: TokenCount::estimated(request.budget_tokens),
        used: TokenCount::estimated(used),
        items,
        omitted,
        notes,
    })
}

fn shrink_to_fit(
    cache: &mut FileCache<'_>,
    source: &Source,
    item: &ContextItem,
    remaining: usize,
) -> Option<(ItemBody, usize)> {
    let ItemBody::Code {
        start_line,
        end_line,
        ..
    } = &item.body
    else {
        return None;
    };
    let full = usize::try_from(end_line - start_line + 1).ok()?;
    let mut lines = full;
    while lines > MIN_TRUNCATED_LINES {
        lines = lines * 2 / 3;
        let body = materialize(cache, source, Some(lines.max(MIN_TRUNCATED_LINES)))?;
        let candidate = ContextItem {
            body,
            ..item.clone()
        };
        let tokens = estimate(&render_item(&candidate));
        if tokens <= remaining {
            return Some((candidate.body, tokens));
        }
    }
    None
}

fn materialize(
    cache: &mut FileCache<'_>,
    source: &Source,
    max_lines: Option<usize>,
) -> Option<ItemBody> {
    match source {
        Source::Memory(m) => Some(ItemBody::Memory {
            id: m.id.clone(),
            category: m.category,
            status: m.status,
            source: m.source.clone(),
            updated_at: format_rfc3339(m.updated_at),
            updated_by: m.updated_by.clone(),
            title: m.title.clone(),
            body: m.body.clone(),
            paths: m.paths.clone(),
        }),
        Source::Related {
            path,
            imports,
            imported_by,
        } => Some(ItemBody::Related {
            path: path.clone(),
            imports: imports.clone(),
            imported_by: imported_by.clone(),
        }),
        Source::Range {
            path,
            language,
            start,
            end,
            symbol,
        } => {
            let lines = cache.lines(path)?;
            let first = usize::try_from(*start).ok()?.max(1);
            if first > lines.len() {
                return None;
            }
            let mut last = usize::try_from(*end).ok()?.clamp(first, lines.len());
            let mut truncated = false;
            if let Some(max) = max_lines
                && last - first + 1 > max
            {
                last = first + max - 1;
                truncated = true;
            }
            Some(ItemBody::Code {
                path: path.clone(),
                language: language.clone(),
                start_line: u32::try_from(first).ok()?,
                end_line: u32::try_from(last).ok()?,
                symbol: symbol.clone(),
                truncated,
                content: lines[first - 1..last].join("\n"),
            })
        }
    }
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn render_header(task: &str, budget: usize) -> String {
    format!(
        "# Context for: {task}\n\nRetrieved by ContextKit. Budget: {budget} tokens (estimated). \
         Paths are relative to the project root; line ranges are 1-based and inclusive.\n"
    )
}

/// Renders one item as Markdown.
pub fn render_item(item: &ContextItem) -> String {
    let mut out = String::new();
    match &item.body {
        ItemBody::Memory {
            id,
            category,
            status,
            source,
            updated_at,
            updated_by,
            title,
            body,
            paths,
        } => {
            let trust = match status {
                Status::Verified => "verified".to_owned(),
                Status::Unverified => {
                    "UNVERIFIED: not confirmed by the user; treat as a claim, not a fact".to_owned()
                }
            };
            let by = updated_by
                .as_deref()
                .map(|by| format!(" by {by}"))
                .unwrap_or_default();
            let _ = writeln!(out, "\n### [{category}] {title}");
            let _ = writeln!(
                out,
                "id `{id}` · {trust} · source {source} · updated {updated_at}{by}"
            );
            if !paths.is_empty() {
                let _ = writeln!(out, "paths: {}", paths.join(", "));
            }
            if !body.is_empty() {
                let _ = writeln!(out, "\n{body}");
            }
        }
        ItemBody::Code {
            path,
            language,
            start_line,
            end_line,
            symbol,
            truncated,
            content,
        } => {
            let label = symbol
                .as_deref()
                .map(|s| format!(" ({s})"))
                .unwrap_or_default();
            let _ = writeln!(out, "\n### {path}:{start_line}-{end_line}{label}");
            let fence = language
                .as_deref()
                .and_then(Lang::from_name)
                .map_or("", Lang::fence);
            let ticks = if content.contains("```") {
                "````"
            } else {
                "```"
            };
            let _ = writeln!(out, "{ticks}{fence}\n{content}\n{ticks}");
            if *truncated {
                let _ = writeln!(out, "(truncated to fit the budget)");
            }
        }
        ItemBody::Related {
            path,
            imports,
            imported_by,
        } => {
            let _ = writeln!(out, "\n### Related files of {path}");
            if !imports.is_empty() {
                let _ = writeln!(out, "imports: {}", imports.join(", "));
            }
            if !imported_by.is_empty() {
                let _ = writeln!(out, "imported by: {}", imported_by.join(", "));
            }
        }
    }
    out
}

type Section = (&'static str, fn(&ItemBody) -> bool);

/// Renders a whole pack as Markdown, grouped into memory, code and related sections.
pub fn render_markdown(pack: &ContextPack) -> String {
    let mut out = render_header(&pack.task, pack.budget.value);
    let sections: [Section; 3] = [
        ("Project memory", |b| matches!(b, ItemBody::Memory { .. })),
        ("Code", |b| matches!(b, ItemBody::Code { .. })),
        ("Related files", |b| matches!(b, ItemBody::Related { .. })),
    ];
    for (title, belongs) in sections {
        let items: Vec<&ContextItem> = pack.items.iter().filter(|i| belongs(&i.body)).collect();
        if items.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n## {title}");
        for item in items {
            out.push_str(&render_item(item));
        }
    }
    if pack.items.is_empty() {
        out.push_str("\nNo relevant context found. Try other terms, or run `contextkit index`.\n");
    }
    let _ = writeln!(
        out,
        "\n---\nUsed ~{} of {} tokens ({}).",
        pack.used.value, pack.budget.value, pack.used.method
    );
    for note in &pack.notes {
        let _ = writeln!(out, "Note: {note}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrows_chunk_to_matching_lines() {
        let content: String = (1..=40)
            .map(|i| {
                if i == 20 {
                    "fn parse_config() {}\n".to_owned()
                } else {
                    format!("line {i}\n")
                }
            })
            .collect();
        let (s, e) = narrow_to_matches(&content, 41, &["parse_config".into()], 80);
        assert_eq!((s, e), (57, 63));
    }

    #[test]
    fn recognizes_test_paths() {
        for p in [
            "tests/end_to_end.rs",
            "src/__tests__/a.ts",
            "web/api.test.ts",
            "web/api.spec.js",
            "pkg/test_models.py",
            "pkg/models_test.go",
        ] {
            assert!(is_test_path(p), "{p}");
        }
        for p in [
            "src/testing_utils.rs",
            "src/contest.rs",
            "tests.md",
            "src/latest.ts",
        ] {
            assert!(!is_test_path(p), "{p}");
        }
    }

    #[test]
    fn recency_decays() {
        let now = 100 * 86_400_000;
        assert!(recency(now, now) > recency(now, now - 30 * 86_400_000));
        assert!((recency(now, now - 30 * 86_400_000) - weights::MEMORY_RECENCY / 2.0).abs() < 1e-9);
    }
}
