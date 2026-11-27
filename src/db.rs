//! SQLite connection setup and schema migrations.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};
use crate::util::now_ms;

/// Ordered list of migrations. Index `i` upgrades the schema from version `i` to `i + 1`.
/// Never edit a released migration; append a new one instead.
const MIGRATIONS: &[&str] = &[
    // Version 1: initial schema.
    r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE project (
    id         TEXT PRIMARY KEY,
    root       TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE files (
    id         INTEGER PRIMARY KEY,
    path       TEXT NOT NULL UNIQUE,
    kind       TEXT NOT NULL,
    language   TEXT,
    size       INTEGER NOT NULL,
    mtime_ns   INTEGER NOT NULL,
    hash       BLOB NOT NULL,
    line_count INTEGER NOT NULL,
    indexed_at INTEGER NOT NULL
) STRICT;
CREATE INDEX files_hash ON files(hash);

CREATE TABLE symbols (
    id         INTEGER PRIMARY KEY,
    file_id    INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    kind       TEXT NOT NULL,
    container  TEXT,
    start_line INTEGER NOT NULL,
    end_line   INTEGER NOT NULL,
    signature  TEXT NOT NULL,
    exported   INTEGER NOT NULL
) STRICT;
CREATE INDEX symbols_name ON symbols(name COLLATE NOCASE);
CREATE INDEX symbols_file ON symbols(file_id);

CREATE TABLE imports (
    id             INTEGER PRIMARY KEY,
    file_id        INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    module         TEXT NOT NULL,
    kind           TEXT NOT NULL,
    line           INTEGER NOT NULL,
    target_file_id INTEGER REFERENCES files(id) ON DELETE SET NULL
) STRICT;
CREATE INDEX imports_file ON imports(file_id);
CREATE INDEX imports_target ON imports(target_file_id);

CREATE TABLE refs (
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name    TEXT NOT NULL,
    line    INTEGER NOT NULL,
    count   INTEGER NOT NULL
) STRICT;
CREATE INDEX refs_name ON refs(name COLLATE NOCASE);
CREATE INDEX refs_file ON refs(file_id);

CREATE TABLE dependencies (
    file_id     INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    ecosystem   TEXT NOT NULL,
    name        TEXT NOT NULL,
    version_req TEXT NOT NULL,
    kind        TEXT NOT NULL
) STRICT;
CREATE INDEX dependencies_file ON dependencies(file_id);

CREATE TABLE chunks (
    id         INTEGER PRIMARY KEY,
    file_id    INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    start_line INTEGER NOT NULL,
    end_line   INTEGER NOT NULL,
    content    TEXT NOT NULL,
    symbols    TEXT NOT NULL,
    terms      TEXT NOT NULL
) STRICT;
CREATE INDEX chunks_file ON chunks(file_id);

-- Contentless index: chunk text lives in `chunks`, the FTS table only stores the inverted index.
CREATE VIRTUAL TABLE chunks_fts USING fts5(
    path, symbols, content, terms,
    content = '', contentless_delete = 1,
    tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, path, symbols, content, terms)
    SELECT new.id, f.path, new.symbols, new.content, new.terms FROM files f WHERE f.id = new.file_id;
END;
CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
    DELETE FROM chunks_fts WHERE rowid = old.id;
END;

CREATE TABLE memories (
    seq        INTEGER PRIMARY KEY,
    id         TEXT NOT NULL UNIQUE,
    project_id TEXT NOT NULL REFERENCES project(id),
    category   TEXT NOT NULL,
    title      TEXT NOT NULL,
    body       TEXT NOT NULL,
    status     TEXT NOT NULL CHECK (status IN ('verified', 'unverified')),
    source     TEXT NOT NULL,
    tags       TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
CREATE INDEX memories_category ON memories(category);

CREATE TABLE memory_paths (
    memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE ON UPDATE CASCADE,
    path      TEXT NOT NULL,
    PRIMARY KEY (memory_id, path)
) STRICT;
CREATE INDEX memory_paths_path ON memory_paths(path);

CREATE VIRTUAL TABLE memories_fts USING fts5(
    title, body, tags,
    content = 'memories', content_rowid = 'seq',
    tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, title, body, tags) VALUES (new.seq, new.title, new.body, new.tags);
END;
CREATE TRIGGER memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, body, tags)
    VALUES ('delete', old.seq, old.title, old.body, old.tags);
END;
CREATE TRIGGER memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, body, tags)
    VALUES ('delete', old.seq, old.title, old.body, old.tags);
    INSERT INTO memories_fts(rowid, title, body, tags) VALUES (new.seq, new.title, new.body, new.tags);
END;
"#,
    // Version 2: record who made the last change to a memory.
    r#"ALTER TABLE memories ADD COLUMN updated_by TEXT;"#,
];

/// Schema version produced by this build.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Opens (creating if necessary) the database at `path` and applies pending migrations.
pub fn open(path: &Path) -> Result<Connection> {
    let mut conn = Connection::open(path)?;
    configure(&conn)?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// Opens an in-memory database with the full schema; intended for tests.
pub fn open_in_memory() -> Result<Connection> {
    let mut conn = Connection::open_in_memory()?;
    configure(&conn)?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<()> {
    // Several MCP servers (one per assistant) and the CLI may share one database.
    conn.busy_timeout(Duration::from_secs(10))?;
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") && !mode.eq_ignore_ascii_case("memory") {
        tracing::warn!(journal_mode = %mode, "SQLite WAL mode unavailable; concurrent access may block");
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if current > SCHEMA_VERSION {
        return Err(Error::SchemaTooNew {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
        tx.commit()?;
        tracing::debug!(version = index + 1, "applied schema migration");
    }
    Ok(())
}

/// Returns the stable project identifier, creating it on first use.
pub fn project_id(conn: &Connection, root: &Path) -> Result<String> {
    if let Some(id) = conn
        .query_row("SELECT id FROM project LIMIT 1", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
    {
        return Ok(id);
    }
    let id = uuid::Uuid::now_v7().to_string();
    conn.execute(
        "INSERT INTO project (id, root, created_at) VALUES (?1, ?2, ?3)",
        params![id, root.to_string_lossy(), now_ms()],
    )?;
    Ok(id)
}

pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()?)
}

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_and_are_idempotent() {
        let mut conn = open_in_memory().expect("open");
        migrate(&mut conn).expect("re-run migrations");
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn rejects_newer_schema() {
        let mut conn = open_in_memory().expect("open");
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .expect("set version");
        assert!(matches!(
            migrate(&mut conn),
            Err(Error::SchemaTooNew { .. })
        ));
    }

    #[test]
    fn project_id_is_stable() {
        let conn = open_in_memory().expect("open");
        let a = project_id(&conn, Path::new("/tmp/x")).expect("id");
        let b = project_id(&conn, Path::new("/tmp/x")).expect("id");
        assert_eq!(a, b);
    }
}
