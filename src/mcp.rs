//! Model Context Protocol server over stdio.
//!
//! Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout, as specified for the MCP
//! stdio transport. Nothing but protocol messages is written to stdout; logs go to stderr.
//! The server is synchronous: an MCP stdio server serves exactly one client, and every
//! tool call is a short local SQLite operation, so an async runtime would add weight
//! without improving latency.

use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::{Value, json};

use crate::context::{self, ContextRequest};
use crate::db;
use crate::error::{Error, Result};
use crate::files::SafeReader;
use crate::index::{self, IndexOptions};
use crate::memory::{self, Actor, Category, Filter, MemoryPatch, NewMemory, Status};
use crate::overview;
use crate::project::Project;
use crate::search::{self, NameMatch};
use crate::util::{format_rfc3339, validate_rel_path};

/// Protocol revisions this server implements, newest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const MAX_READ_LINES: u32 = 400;
const MAX_LIMIT: u64 = 200;
/// Largest accepted JSON-RPC message.
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

const INSTRUCTIONS: &str = "ContextKit indexes this repository and stores persistent project memory \
shared by every assistant that works on it. At the start of a task call `get_context` with a short \
description of the task instead of reading many files. Use `search_code`, `find_symbol` and \
`read_file` for follow-up lookups. Record decisions, discoveries, finished and unfinished work with \
`memory_add` so later sessions and other assistants can continue. Records you add are stored as \
UNVERIFIED until the user confirms them; never present unverified records as established facts.";

/// A stdio MCP server bound to one project.
#[derive(Debug)]
pub struct Server {
    project: Project,
    conn: Connection,
    project_id: String,
    client_name: Option<String>,
    last_refresh: Option<Instant>,
    reader: SafeReader,
}

impl Server {
    pub fn new(project: Project) -> Result<Self> {
        let conn = project.connect()?;
        let project_id = db::project_id(&conn, project.root())?;
        Ok(Self {
            reader: SafeReader::new(&project),
            project,
            conn,
            project_id,
            client_name: None,
            last_refresh: None,
        })
    }

    /// Serves requests until `input` reaches end of file.
    pub fn serve(&mut self, input: impl BufRead, mut output: impl Write) -> Result<()> {
        let mut input = input;
        let mut buf = Vec::new();
        loop {
            let line = read_bounded_line(&mut input, &mut buf, MAX_MESSAGE_BYTES).map_err(|e| {
                Error::Io {
                    path: "<stdin>".into(),
                    source: e,
                }
            })?;
            let response = match line {
                Line::Eof => return Ok(()),
                Line::TooLong => Some(error_response(
                    Value::Null,
                    INVALID_REQUEST,
                    "message too large",
                )),
                Line::Complete => match std::str::from_utf8(&buf) {
                    Ok(text) if text.trim().is_empty() => continue,
                    Ok(text) => self.handle_line(text.trim()),
                    Err(_) => Some(error_response(
                        Value::Null,
                        PARSE_ERROR,
                        "message is not valid UTF-8",
                    )),
                },
            };
            if let Some(response) = response {
                let mut bytes = serde_json::to_vec(&response)?;
                bytes.push(b'\n');
                output
                    .write_all(&bytes)
                    .and_then(|()| output.flush())
                    .map_err(|e| Error::Io {
                        path: "<stdout>".into(),
                        source: e,
                    })?;
            }
        }
    }

    /// Handles one JSON-RPC message and returns the response, if one is due.
    pub fn handle_line(&mut self, line: &str) -> Option<Value> {
        let message: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(error_response(
                    Value::Null,
                    PARSE_ERROR,
                    &format!("parse error: {e}"),
                ));
            }
        };
        let Some(obj) = message.as_object() else {
            return Some(error_response(
                Value::Null,
                INVALID_REQUEST,
                "expected a single JSON-RPC object (batches are not supported)",
            ));
        };
        let id = obj.get("id").cloned();
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            // A response from the client (we never send requests) or garbage; ignore responses.
            return id
                .filter(|_| !obj.contains_key("result") && !obj.contains_key("error"))
                .map(|id| error_response(id, INVALID_REQUEST, "missing method"));
        };
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            // Notification: no response. `notifications/initialized` and
            // `notifications/cancelled` need no action from a synchronous server.
            tracing::debug!(method, "notification received");
            return None;
        };
        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => self.call_tool(&params),
            "resources/list" => Ok(json!({ "resources": [] })),
            "prompts/list" => Ok(json!({ "prompts": [] })),
            _ => Err((METHOD_NOT_FOUND, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => error_response(id, code, &message),
        })
    }

    fn initialize(&mut self, params: &Value) -> Value {
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let version = requested
            .filter(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(v))
            .unwrap_or(SUPPORTED_PROTOCOL_VERSIONS[0]);
        self.client_name = params
            .pointer("/clientInfo/name")
            .and_then(Value::as_str)
            .map(sanitize_client_name);
        tracing::info!(client = ?self.client_name, protocol = version, "client initialized");
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": {
                "name": "contextkit",
                "title": "ContextKit",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": INSTRUCTIONS,
        })
    }

    fn call_tool(&mut self, params: &Value) -> std::result::Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, "tools/call requires `name`".to_owned()))?;
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !args.is_object() {
            return Err((INVALID_PARAMS, "`arguments` must be an object".to_owned()));
        }
        if !tool_definitions().iter().any(|t| t["name"] == name) {
            return Err((INVALID_PARAMS, format!("unknown tool: {name}")));
        }
        // Tool failures are reported inside the result so the model can see and react to them.
        let outcome = self.run_tool(name, &args);
        Ok(match outcome {
            Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
            Err(e) => {
                tracing::warn!(tool = name, error = %e, "tool call failed");
                json!({ "content": [{ "type": "text", "text": format!("Error: {e}") }], "isError": true })
            }
        })
    }

    fn actor(&self) -> Actor {
        Actor::Agent(
            self.client_name
                .clone()
                .unwrap_or_else(|| "unknown".to_owned()),
        )
    }

    /// Keeps the index reasonably fresh without a background watcher.
    fn refresh_if_stale(&mut self) -> Result<Option<String>> {
        let interval = self.project.config().mcp.auto_refresh_secs;
        let due = match (self.last_refresh, interval) {
            (Some(_), 0) => false,
            (Some(t), secs) => t.elapsed() >= Duration::from_secs(secs),
            // With auto-refresh disabled, still build the index once if it was never built.
            (None, 0) => index::last_indexed_at(&self.conn)?.is_none(),
            (None, _) => true,
        };
        if !due {
            self.last_refresh.get_or_insert_with(Instant::now);
            return Ok(None);
        }
        let stats = index::index_project(&self.project, &mut self.conn, IndexOptions::default())?;
        self.last_refresh = Some(Instant::now());
        Ok(stats.changed().then(|| {
            format!(
                "(index refreshed: {} added, {} updated, {} removed)",
                stats.added, stats.updated, stats.removed
            )
        }))
    }

    fn run_tool(&mut self, name: &str, args: &Value) -> Result<String> {
        let needs_index = !name.starts_with("memory_") && name != "index_refresh";
        let refresh_note = if needs_index {
            self.refresh_if_stale()?
        } else {
            None
        };
        let mut text = match name {
            "get_context" => self.tool_get_context(args)?,
            "search_code" => self.tool_search_code(args)?,
            "find_symbol" => self.tool_find_symbol(args)?,
            "file_outline" => self.tool_file_outline(args)?,
            "read_file" => self.tool_read_file(args)?,
            "project_overview" => {
                overview::render_markdown(&overview::overview(&self.conn, &self.project_id)?)
            }
            "index_refresh" => {
                let stats =
                    index::index_project(&self.project, &mut self.conn, IndexOptions::default())?;
                self.last_refresh = Some(Instant::now());
                format!(
                    "Index up to date: {} files scanned, {} added, {} updated, {} removed, {} renamed, {} unchanged ({} ms).",
                    stats.scanned,
                    stats.added,
                    stats.updated,
                    stats.removed,
                    stats.renamed,
                    stats.unchanged,
                    stats.duration.as_millis()
                )
            }
            "memory_search" => self.tool_memory_search(args)?,
            "memory_add" => self.tool_memory_add(args)?,
            "memory_update" => self.tool_memory_update(args)?,
            other => return Err(Error::InvalidInput(format!("unknown tool: {other}"))),
        };
        if let Some(note) = refresh_note {
            text.push_str("\n\n");
            text.push_str(&note);
        }
        Ok(text)
    }

    fn tool_get_context(&mut self, args: &Value) -> Result<String> {
        let task = required_str(args, "task")?;
        let config = self.project.config();
        let budget =
            optional_u64(args, "budget_tokens")?.map(|b| usize::try_from(b).unwrap_or(usize::MAX));
        let focus_paths = string_list(args, "focus_paths")?
            .iter()
            .map(|p| validate_rel_path(p))
            .collect::<Result<Vec<_>>>()?;
        let request = ContextRequest {
            task: task.to_owned(),
            budget_tokens: config.resolve_budget(budget),
            focus_paths,
            include_code: optional_bool(args, "include_code")?.unwrap_or(true),
            include_memory: optional_bool(args, "include_memory")?.unwrap_or(true),
            max_snippet_lines: config.retrieval.max_snippet_lines,
        };
        let pack = context::retrieve(&self.conn, &self.reader, &request)?;
        Ok(context::render_markdown(&pack))
    }

    fn tool_search_code(&self, args: &Value) -> Result<String> {
        let query = required_str(args, "query")?;
        let limit = limit(args, 20)?;
        let path = optional_str(args, "path")?
            .map(validate_rel_path)
            .transpose()?;
        let hits = search::search_code(&self.conn, query, path.as_deref(), limit)?;
        if hits.is_empty() {
            return Ok(format!("No matches for `{query}`."));
        }
        let mut out = format!("{} match(es) for `{query}` (best first):\n", hits.len());
        for h in hits {
            let symbols = if h.symbols.is_empty() {
                String::new()
            } else {
                format!(" — defines {}", h.symbols)
            };
            out.push_str(&format!(
                "- {}:{}-{}{symbols}\n",
                h.path, h.start_line, h.end_line
            ));
        }
        out.push_str("\nUse `read_file` with a path and line range to see the code.");
        Ok(out)
    }

    fn tool_find_symbol(&self, args: &Value) -> Result<String> {
        let name = required_str(args, "name")?;
        let mode = if optional_bool(args, "exact")?.unwrap_or(false) {
            NameMatch::Exact
        } else {
            NameMatch::Contains
        };
        let kind = optional_str(args, "kind")?.map(str::parse).transpose()?;
        let limit = limit(args, 20)?;
        let hits = search::find_symbols(&self.conn, name, mode, kind, limit)?;
        let mut out = String::new();
        if hits.is_empty() {
            out.push_str(&format!("No symbol definitions match `{name}`.\n"));
        } else {
            out.push_str(&format!("Definitions matching `{name}`:\n"));
            for h in &hits {
                let container = h
                    .container
                    .as_deref()
                    .map(|c| format!("{c}::"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "- {} {container}{} — {}:{}-{} — `{}`\n",
                    h.kind, h.name, h.path, h.start_line, h.end_line, h.signature
                ));
            }
        }
        if optional_bool(args, "include_references")?.unwrap_or(false) {
            let refs = search::find_references(&self.conn, name, limit)?;
            out.push_str(&format!(
                "\nCall sites of `{name}` (by name only, not type-resolved):\n"
            ));
            if refs.is_empty() {
                out.push_str("- none found\n");
            }
            for r in refs {
                out.push_str(&format!(
                    "- {}:{} ({} call(s))\n",
                    r.path, r.first_line, r.count
                ));
            }
        }
        Ok(out)
    }

    fn tool_file_outline(&self, args: &Value) -> Result<String> {
        let path = validate_rel_path(required_str(args, "path")?)?;
        let symbols = search::file_outline(&self.conn, &path)?;
        let relations = search::file_relations(&self.conn, &path, 50)?;
        let mut out = format!("Outline of {path}:\n");
        if symbols.is_empty() {
            out.push_str("- no symbols extracted\n");
        }
        for s in symbols {
            let indent = if s.container.is_some() { "  " } else { "" };
            out.push_str(&format!(
                "{indent}- {} {} ({}-{})\n",
                s.kind, s.name, s.start_line, s.end_line
            ));
        }
        if !relations.imports.is_empty() {
            out.push_str(&format!("\nImports: {}\n", relations.imports.join(", ")));
        }
        if !relations.imported_by.is_empty() {
            out.push_str(&format!(
                "Imported by: {}\n",
                relations.imported_by.join(", ")
            ));
        }
        Ok(out)
    }

    fn tool_read_file(&self, args: &Value) -> Result<String> {
        let path = validate_rel_path(required_str(args, "path")?)?;
        // Only indexed files can be read: this applies every exclusion rule, including
        // the secret-file filter, to what an agent can see.
        let language: Option<Option<String>> = self
            .conn
            .query_row("SELECT language FROM files WHERE path = ?1", [&path], |r| {
                r.get(0)
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let Some(language) = language else {
            return Err(Error::NotFound(format!(
                "`{path}` is not in the index (it may be excluded, ignored, binary or too large)"
            )));
        };
        // Re-checks containment, size and secrets at read time: the file may have changed
        // since it was indexed.
        let text = self.reader.read(&path)?;
        let total = u32::try_from(text.lines().count()).unwrap_or(u32::MAX);
        let start = optional_u64(args, "start_line")?
            .map_or(1, |v| u32::try_from(v).unwrap_or(u32::MAX))
            .max(1);
        let requested_end = optional_u64(args, "end_line")?
            .map_or(start.saturating_add(MAX_READ_LINES - 1), |v| {
                u32::try_from(v).unwrap_or(u32::MAX)
            });
        let end = requested_end
            .min(total)
            .min(start.saturating_add(MAX_READ_LINES - 1));
        if start > total.max(1) || end < start {
            return Err(Error::InvalidInput(format!(
                "`{path}` has {total} lines; requested {start}-{requested_end}"
            )));
        }
        let body: Vec<&str> = text
            .lines()
            .skip(start as usize - 1)
            .take((end - start + 1) as usize)
            .collect();
        let fence = language
            .as_deref()
            .and_then(crate::index::lang::Lang::from_name)
            .map_or("", |l| l.fence());
        let mut out = format!(
            "{path}:{start}-{end} (of {total} lines)\n```{fence}\n{}\n```",
            body.join("\n")
        );
        if end < total {
            out.push_str(&format!("\n{} more line(s) after line {end}.", total - end));
        }
        Ok(out)
    }

    fn tool_memory_search(&self, args: &Value) -> Result<String> {
        let query = optional_str(args, "query")?.unwrap_or("");
        let filter = Filter {
            categories: string_list(args, "categories")?
                .iter()
                .map(|c| c.parse())
                .collect::<Result<_>>()?,
            status: optional_str(args, "status")?.map(str::parse).transpose()?,
            path: optional_str(args, "path")?
                .map(validate_rel_path)
                .transpose()?,
            limit: Some(limit(args, 20)?),
        };
        let memories: Vec<memory::Memory> = if query.trim().is_empty() {
            memory::list(&self.conn, &filter)?
        } else {
            memory::search(&self.conn, query, &filter)?
                .into_iter()
                .map(|s| s.memory)
                .collect()
        };
        if memories.is_empty() {
            return Ok("No matching memory records.".to_owned());
        }
        let mut out = String::new();
        for m in memories {
            let trust = if m.status == Status::Verified {
                "verified"
            } else {
                "UNVERIFIED"
            };
            out.push_str(&format!(
                "### [{}] {}\nid `{}` · {trust} · source {} · updated {}{}\n",
                m.category,
                m.title,
                m.id,
                m.source,
                format_rfc3339(m.updated_at),
                m.updated_by
                    .as_deref()
                    .map(|by| format!(" by {by}"))
                    .unwrap_or_default()
            ));
            if !m.paths.is_empty() {
                out.push_str(&format!("paths: {}\n", m.paths.join(", ")));
            }
            if !m.tags.is_empty() {
                out.push_str(&format!("tags: {}\n", m.tags.join(", ")));
            }
            if !m.body.is_empty() {
                out.push_str(&format!("\n{}\n", m.body));
            }
            out.push('\n');
        }
        Ok(out)
    }

    fn tool_memory_add(&mut self, args: &Value) -> Result<String> {
        let new = NewMemory {
            category: required_str(args, "category")?.parse::<Category>()?,
            title: required_str(args, "title")?.to_owned(),
            body: optional_str(args, "body")?.unwrap_or("").to_owned(),
            status: Status::Unverified,
            tags: string_list(args, "tags")?,
            paths: string_list(args, "paths")?,
        };
        let actor = self.actor();
        let m = memory::add(&mut self.conn, &self.project_id, &actor, new)?;
        Ok(format!(
            "Recorded {} `{}` as UNVERIFIED (source {}). The user can verify it with `contextkit memory verify {}`.",
            m.category, m.id, m.source, m.id
        ))
    }

    fn tool_memory_update(&mut self, args: &Value) -> Result<String> {
        let id = required_str(args, "id")?;
        let patch = MemoryPatch {
            category: optional_str(args, "category")?
                .map(str::parse)
                .transpose()?,
            title: optional_str(args, "title")?.map(str::to_owned),
            body: optional_str(args, "body")?.map(str::to_owned),
            tags: args
                .get("tags")
                .map(|_| string_list(args, "tags"))
                .transpose()?,
            paths: args
                .get("paths")
                .map(|_| string_list(args, "paths"))
                .transpose()?,
        };
        let actor = self.actor();
        let m = memory::update(&mut self.conn, id, &actor, patch)?;
        Ok(format!("Updated {} `{}` ({}).", m.category, m.id, m.status))
    }
}

/// Result of reading one newline-delimited message.
#[derive(Debug, PartialEq, Eq)]
enum Line {
    Complete,
    /// The line exceeded the limit; it was consumed and discarded without buffering it.
    TooLong,
    Eof,
}

/// Reads one line into `buf` (without the newline), never buffering more than `max` bytes.
fn read_bounded_line(
    input: &mut impl BufRead,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Line> {
    buf.clear();
    let mut overflow = false;
    let mut saw_any = false;
    loop {
        let available = match input.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            // End of input: a final line without a newline still counts.
            return Ok(match (saw_any, overflow) {
                (false, _) => Line::Eof,
                (true, true) => Line::TooLong,
                (true, false) => Line::Complete,
            });
        }
        saw_any = true;
        let (chunk, found_newline) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (&available[..i], true),
            None => (available, false),
        };
        if !overflow {
            if buf.len() + chunk.len() > max {
                overflow = true;
                buf.clear();
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        let consumed = chunk.len() + usize::from(found_newline);
        input.consume(consumed);
        if found_newline {
            return Ok(if overflow {
                Line::TooLong
            } else {
                Line::Complete
            });
        }
    }
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Restricts client names to a short, safe identifier for the `source` column.
fn sanitize_client_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(48)
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "unknown".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn bad(field: &str, expected: &str) -> Error {
    Error::InvalidInput(format!("`{field}` must be {expected}"))
}

fn required_str<'a>(args: &'a Value, field: &str) -> Result<&'a str> {
    match args.get(field) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s),
        _ => Err(bad(field, "a non-empty string")),
    }
}

fn optional_str<'a>(args: &'a Value, field: &str) -> Result<Option<&'a str>> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(bad(field, "a string")),
    }
}

fn optional_bool(args: &Value, field: &str) -> Result<Option<bool>> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(bad(field, "a boolean")),
    }
}

fn optional_u64(args: &Value, field: &str) -> Result<Option<u64>> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| bad(field, "a non-negative integer")),
    }
}

fn limit(args: &Value, default: u64) -> Result<usize> {
    let v = optional_u64(args, "limit")?
        .unwrap_or(default)
        .clamp(1, MAX_LIMIT);
    Ok(usize::try_from(v).unwrap_or(1))
}

fn string_list(args: &Value, field: &str) -> Result<Vec<String>> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| bad(field, "an array of strings"))
            })
            .collect(),
        Some(_) => Err(bad(field, "an array of strings")),
    }
}

/// JSON Schema descriptions of every tool.
pub fn tool_definitions() -> Vec<Value> {
    let categories: Vec<&str> = Category::ALL.iter().map(|c| c.as_str()).collect();
    let category_help: String = Category::ALL
        .iter()
        .map(|c| format!("{} = {}", c.as_str(), c.description()))
        .collect::<Vec<_>>()
        .join("; ");
    let read_only = json!({ "readOnlyHint": true, "openWorldHint": false });
    vec![
        json!({
            "name": "get_context",
            "title": "Get task context",
            "description": "Retrieve the most relevant code snippets, project rules and memory for a task, packed into a token budget. Call this first when starting a task. Token counts are estimates.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "What you are about to do, in a sentence. Include identifiers you know." },
                    "budget_tokens": { "type": "integer", "minimum": 1, "description": "Estimated-token budget. Defaults to the project setting." },
                    "focus_paths": { "type": "array", "items": { "type": "string" }, "description": "Project-relative files or directories to prefer." },
                    "include_code": { "type": "boolean", "default": true },
                    "include_memory": { "type": "boolean", "default": true }
                },
                "required": ["task"],
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
        json!({
            "name": "search_code",
            "title": "Search code",
            "description": "Full-text search over indexed files. Returns paths and line ranges, best match first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "path": { "type": "string", "description": "Restrict to a project-relative directory or file." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": 20 }
                },
                "required": ["query"],
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
        json!({
            "name": "find_symbol",
            "title": "Find symbol",
            "description": "Find definitions of functions, methods, classes, structs, interfaces, types and Markdown headings by name, optionally with call sites.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "exact": { "type": "boolean", "default": false, "description": "Match the whole name (case-insensitive) instead of a substring." },
                    "kind": { "type": "string", "enum": crate::index::extract::SymbolKind::ALL.iter().map(|k| k.as_str()).collect::<Vec<_>>() },
                    "include_references": { "type": "boolean", "default": false, "description": "Also list files that call this name." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": 20 }
                },
                "required": ["name"],
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
        json!({
            "name": "file_outline",
            "title": "File outline",
            "description": "List the symbols defined in one file and the files it imports or is imported by.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
        json!({
            "name": "read_file",
            "title": "Read file range",
            "description": format!("Read lines from an indexed project file (at most {MAX_READ_LINES} lines per call). Excluded, ignored and secret files cannot be read."),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": "integer", "minimum": 1, "default": 1 },
                    "end_line": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"],
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
        json!({
            "name": "project_overview",
            "title": "Project overview",
            "description": "Languages, top-level layout, declared dependencies, docs, open tasks and known bugs.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": read_only,
        }),
        json!({
            "name": "index_refresh",
            "title": "Refresh index",
            "description": "Incrementally reindex changed, added and deleted files. The server also refreshes automatically when the index is stale.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
        }),
        json!({
            "name": "memory_search",
            "title": "Search project memory",
            "description": "Search persistent project memory shared across sessions and assistants. Without a query, lists the most recently updated records. UNVERIFIED records are claims nobody has confirmed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "categories": { "type": "array", "items": { "type": "string", "enum": categories } },
                    "status": { "type": "string", "enum": ["verified", "unverified"] },
                    "path": { "type": "string", "description": "Only records linked to this file or directory." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": 20 }
                },
                "additionalProperties": false
            },
            "annotations": read_only,
        }),
        json!({
            "name": "memory_add",
            "title": "Record project memory",
            "description": format!("Record a decision, convention, task, bug or discovery so later sessions and other assistants can use it. Stored as UNVERIFIED until the user confirms it. Categories: {category_help}."),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "category": { "type": "string", "enum": categories },
                    "title": { "type": "string", "maxLength": 200 },
                    "body": { "type": "string", "description": "Details, rationale and evidence." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Related project-relative files or directories." },
                    "tags": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["category", "title"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false },
        }),
        json!({
            "name": "memory_update",
            "title": "Update project memory",
            "description": "Update an UNVERIFIED memory record, for example to move a task from task_open to task_done. Verified records cannot be changed by assistants; add a new record instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Full id or a unique prefix (at least 4 characters)." },
                    "category": { "type": "string", "enum": categories },
                    "title": { "type": "string", "maxLength": 200 },
                    "body": { "type": "string" },
                    "paths": { "type": "array", "items": { "type": "string" } },
                    "tags": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["id"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_reads_discard_oversized_lines() {
        let data = format!("short\n{}\nnext\nlast", "x".repeat(100));
        let mut input = std::io::BufReader::with_capacity(8, data.as_bytes());
        let mut buf = Vec::new();
        let mut read = || read_bounded_line(&mut input, &mut buf, 16).expect("read");
        assert_eq!(read(), Line::Complete);
        assert_eq!(read(), Line::TooLong);
        assert_eq!(read(), Line::Complete);
        assert_eq!(read(), Line::Complete);
        assert_eq!(read(), Line::Eof);
    }

    #[test]
    fn sanitizes_client_names() {
        assert_eq!(sanitize_client_name("Claude Code"), "claude-code");
        assert_eq!(sanitize_client_name("../../etc"), "..-..-etc");
        assert_eq!(sanitize_client_name("   "), "unknown");
    }

    #[test]
    fn tool_names_are_unique() {
        let defs = tool_definitions();
        let mut names: Vec<&str> = defs.iter().filter_map(|t| t["name"].as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len());
    }
}
