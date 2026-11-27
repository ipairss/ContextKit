//! Query parsing and code search over the index.

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::error::Result;
use crate::index::extract::{SymbolKind, split_camel};
use crate::util::escape_like;

/// Words that carry no signal in a task description.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "can", "do", "does", "for", "from",
    "how", "i", "if", "in", "into", "is", "it", "its", "me", "my", "of", "on", "or", "our",
    "please", "should", "so", "that", "the", "their", "then", "there", "these", "this", "to", "us",
    "was", "we", "what", "when", "where", "which", "while", "who", "why", "will", "with", "would",
    "you", "your",
];

/// Longest list of terms taken from one query, to keep FTS queries bounded.
const MAX_TERMS: usize = 24;

/// Terms extracted from free text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryTerms {
    /// Identifiers as written, e.g. `parse_config`, `UserService` (for symbol lookup).
    pub identifiers: Vec<String>,
    /// Lowercase search words, including camelCase sub-words.
    pub words: Vec<String>,
}

impl QueryTerms {
    pub fn parse(text: &str) -> Self {
        let mut terms = Self::default();
        for raw in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            let raw = raw.trim_matches('_');
            if raw.is_empty() {
                continue;
            }
            let lower = raw.to_lowercase();
            if raw.chars().count() >= 3
                && !STOPWORDS.contains(&lower.as_str())
                && !terms.identifiers.iter().any(|i| i == raw)
            {
                terms.identifiers.push(raw.to_owned());
            }
            let mut push = |w: String| {
                if w.chars().count() >= 2
                    && !STOPWORDS.contains(&w.as_str())
                    && !terms.words.contains(&w)
                {
                    terms.words.push(w);
                }
            };
            push(lower.clone());
            for part in split_camel(raw) {
                let stemmed = stem(&part).to_owned();
                push(part);
                // `renamed` should also find `renames` and `rename_file`.
                push(stemmed);
            }
        }
        terms.identifiers.truncate(MAX_TERMS);
        terms.words.truncate(MAX_TERMS);
        terms
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Distinct stems of the search words, for substring matching against names.
    pub fn stems(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for w in &self.words {
            if w.contains('_') {
                continue;
            }
            let s = stem(w);
            if !out.contains(&s) {
                out.push(s);
            }
        }
        out
    }
}

/// Returns true for identifiers that are clearly code rather than English words:
/// `parse_config`, `getUserName`, `HTTP2`, `MAX_LEN`.
pub fn looks_like_code(ident: &str) -> bool {
    ident.contains('_')
        || ident.chars().any(|c| c.is_ascii_digit())
        || ident.chars().skip(1).any(char::is_uppercase)
}

/// A deliberately tiny suffix stripper for English task descriptions. It only needs to
/// make `indexing`, `indexed` and `indexes` meet at `index`; it is not a real stemmer.
pub fn stem(word: &str) -> &str {
    for suffix in ["ing", "ed", "es", "s"] {
        if let Some(base) = word.strip_suffix(suffix)
            && base.chars().count() >= 4
        {
            return base;
        }
    }
    word
}

/// Builds a safe FTS5 `MATCH` expression (OR of quoted terms) from free text.
/// Returns `None` when the text contains no searchable terms.
pub fn fts_query(text: &str) -> Option<String> {
    fts_from_words(&QueryTerms::parse(text).words)
}

/// Builds an FTS5 expression from already-normalized words. Words of three or more
/// characters also match as prefixes (`config` matches `configuration`).
pub fn fts_from_words(words: &[String]) -> Option<String> {
    let parts: Vec<String> = words
        .iter()
        .flat_map(|w| {
            // FTS5's tokenizer splits on `_`, so `parse_config` must become a phrase.
            let phrase = w
                .split('_')
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            (!phrase.is_empty()).then(|| {
                let quoted = format!("\"{}\"", phrase.replace('"', ""));
                if phrase.chars().count() >= 3 && !phrase.contains(' ') {
                    format!("{quoted}*")
                } else {
                    quoted
                }
            })
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" OR "))
}

/// One full-text hit in indexed file content.
#[derive(Debug, Clone, Serialize)]
pub struct CodeHit {
    pub path: String,
    pub language: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    /// Symbols defined in this range.
    pub symbols: String,
    /// BM25 relevance; higher is better. Only comparable within one query.
    pub relevance: f64,
    #[serde(skip)]
    pub content: String,
}

/// Searches indexed file content. `path_prefix` restricts results to one directory or file.
pub fn search_code(
    conn: &Connection,
    query: &str,
    path_prefix: Option<&str>,
    limit: usize,
) -> Result<Vec<CodeHit>> {
    let Some(fts) = fts_query(query) else {
        return Ok(Vec::new());
    };
    search_code_fts(conn, &fts, path_prefix, limit)
}

pub(crate) fn search_code_fts(
    conn: &Connection,
    fts: &str,
    path_prefix: Option<&str>,
    limit: usize,
) -> Result<Vec<CodeHit>> {
    let prefix = path_prefix.map(|p| format!("{}%", escape_like(p.trim_end_matches('/'))));
    let mut stmt = conn.prepare_cached(
        "SELECT f.path, f.language, c.start_line, c.end_line, c.symbols, c.content,
                bm25(chunks_fts, 4.0, 3.0, 1.0, 1.5) AS rank
         FROM chunks_fts
         JOIN chunks c ON c.id = chunks_fts.rowid
         JOIN files f ON f.id = c.file_id
         WHERE chunks_fts MATCH ?1 AND (?2 IS NULL OR f.path LIKE ?2 ESCAPE '\\')
         ORDER BY rank, f.path, c.start_line
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![fts, prefix, limit as i64], |r| {
        Ok(CodeHit {
            path: r.get(0)?,
            language: r.get(1)?,
            start_line: r.get(2)?,
            end_line: r.get(3)?,
            symbols: r.get(4)?,
            content: r.get(5)?,
            relevance: -r.get::<_, f64>(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// A symbol definition.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolHit {
    pub name: String,
    pub kind: String,
    pub container: Option<String>,
    pub path: String,
    pub language: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub signature: String,
    pub exported: bool,
}

/// How to match symbol names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameMatch {
    /// Case-insensitive equality.
    Exact,
    /// Case-insensitive substring.
    Contains,
}

/// Finds symbol definitions by name.
pub fn find_symbols(
    conn: &Connection,
    name: &str,
    mode: NameMatch,
    kind: Option<SymbolKind>,
    limit: usize,
) -> Result<Vec<SymbolHit>> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(Vec::new());
    }
    let (op, pattern) = match mode {
        NameMatch::Exact => ("s.name = ?1 COLLATE NOCASE", name.to_owned()),
        NameMatch::Contains => (
            "s.name LIKE ?1 ESCAPE '\\'",
            format!("%{}%", escape_like(name)),
        ),
    };
    let sql = format!(
        "SELECT s.name, s.kind, s.container, f.path, f.language, s.start_line, s.end_line,
                s.signature, s.exported
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE {op} AND (?2 IS NULL OR s.kind = ?2)
         ORDER BY (s.name = ?3) DESC, length(s.name), s.exported DESC, f.path, s.start_line
         LIMIT ?4"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(
        params![pattern, kind.map(SymbolKind::as_str), name, limit as i64],
        symbol_from_row,
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn symbol_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SymbolHit> {
    Ok(SymbolHit {
        name: r.get(0)?,
        kind: r.get(1)?,
        container: r.get(2)?,
        path: r.get(3)?,
        language: r.get(4)?,
        start_line: r.get(5)?,
        end_line: r.get(6)?,
        signature: r.get(7)?,
        exported: r.get(8)?,
    })
}

/// All symbols defined in one file, in source order.
pub fn file_outline(conn: &Connection, path: &str) -> Result<Vec<SymbolHit>> {
    let mut stmt = conn.prepare_cached(
        "SELECT s.name, s.kind, s.container, f.path, f.language, s.start_line, s.end_line,
                s.signature, s.exported
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE f.path = ?1 ORDER BY s.start_line, s.end_line DESC",
    )?;
    let rows = stmt.query_map([path], symbol_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// A call site of a name, aggregated per file.
#[derive(Debug, Clone, Serialize)]
pub struct ReferenceHit {
    pub path: String,
    pub first_line: u32,
    pub count: u32,
}

/// Files that call `name` (by callee name only; no type resolution).
pub fn find_references(conn: &Connection, name: &str, limit: usize) -> Result<Vec<ReferenceHit>> {
    let mut stmt = conn.prepare_cached(
        "SELECT f.path, r.line, r.count FROM refs r JOIN files f ON f.id = r.file_id
         WHERE r.name = ?1 COLLATE NOCASE ORDER BY r.count DESC, f.path LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![name.trim(), limit as i64], |r| {
        Ok(ReferenceHit {
            path: r.get(0)?,
            first_line: r.get(1)?,
            count: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Files directly connected to `path` through resolved imports.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FileRelations {
    pub imports: Vec<String>,
    pub imported_by: Vec<String>,
}

pub fn file_relations(conn: &Connection, path: &str, limit: usize) -> Result<FileRelations> {
    let collect = |sql: &str| -> Result<Vec<String>> {
        let mut stmt = conn.prepare_cached(sql)?;
        let rows = stmt.query_map(params![path, limit as i64], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    };
    Ok(FileRelations {
        imports: collect(
            "SELECT DISTINCT t.path FROM imports i
             JOIN files f ON f.id = i.file_id JOIN files t ON t.id = i.target_file_id
             WHERE f.path = ?1 ORDER BY t.path LIMIT ?2",
        )?,
        imported_by: collect(
            "SELECT DISTINCT f.path FROM imports i
             JOIN files f ON f.id = i.file_id JOIN files t ON t.id = i.target_file_id
             WHERE t.path = ?1 ORDER BY f.path LIMIT ?2",
        )?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_query_terms() {
        let t = QueryTerms::parse("Fix the getUserName bug in parse_config, please!");
        assert_eq!(
            t.identifiers,
            vec!["Fix", "getUserName", "bug", "parse_config"]
        );
        assert_eq!(
            t.words,
            vec![
                "fix",
                "getusername",
                "get",
                "user",
                "name",
                "bug",
                "parse_config",
                "parse",
                "config"
            ]
        );
        let r = QueryTerms::parse("renamed indexing");
        assert_eq!(r.words, vec!["renamed", "renam", "indexing", "index"]);
        assert_eq!(r.stems(), vec!["renam", "index"]);
        assert!(looks_like_code("parse_config") && looks_like_code("getUser"));
        assert!(!looks_like_code("Detect") && !looks_like_code("files"));
        assert!(QueryTerms::parse("the a of").is_empty());
    }

    #[test]
    fn builds_safe_fts_queries() {
        assert_eq!(
            fts_query("parse_config db").as_deref(),
            Some("\"parse config\" OR \"parse\"* OR \"config\"* OR \"db\"")
        );
        assert_eq!(
            fts_query("\"; DROP TABLE x; --"),
            Some("\"drop\"* OR \"table\"*".into())
        );
        assert_eq!(fts_query("  "), None);
    }

    /// A deterministic, dependency-free stand-in for the `search_query` fuzz target, so
    /// every CI run checks that hostile input never produces an FTS5 syntax error.
    #[test]
    fn random_queries_never_fail() {
        let conn = crate::db::open_in_memory().expect("db");
        const ALPHABET: &[char] = &[
            'a', 'Z', '0', '_', '-', ' ', '"', '\'', '*', '(', ')', ':', '^', '+', '{', '}', '.',
            ',', ';', '\\', '/', '%', 'é', '中', '\u{0}', '\n', 'N', 'E', 'A', 'R', 'O', 'T',
        ];
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..3_000 {
            let len = (next() % 24) as usize;
            let query: String = (0..len)
                .map(|_| ALPHABET[(next() % ALPHABET.len() as u64) as usize])
                .collect();
            search_code(&conn, &query, None, 3).expect(&query);
            crate::memory::search(&conn, &query, &crate::memory::Filter::default()).expect(&query);
            find_symbols(&conn, &query, NameMatch::Contains, None, 3).expect(&query);
        }
        for keyword in [
            "AND",
            "OR",
            "NOT",
            "NEAR",
            "NEAR(a b)",
            "a OR",
            "\"",
            "*",
            "^a",
        ] {
            search_code(&conn, keyword, None, 3).expect(keyword);
        }
    }
}
