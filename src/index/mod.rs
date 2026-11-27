//! Incremental repository indexing.
//!
//! The pipeline is: walk → compare size/mtime with the database → read, hash and parse
//! changed files on worker threads → write results on the calling thread inside one
//! transaction → resolve imports. Unchanged files are never read.

pub mod extract;
pub mod lang;
pub mod manifest;
pub mod resolve;
pub mod walk;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rusqlite::{Connection, Transaction, params};
use serde::Serialize;

use crate::db;
use crate::error::Result;
use crate::project::Project;
use crate::util::now_ms;
use extract::{Analysis, Extractor};
use lang::{FileKind, Lang};
use walk::WalkEntry;

/// Bytes inspected when sniffing for binary content.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// Meta key holding the time of the last completed index run.
pub const META_LAST_INDEXED: &str = "last_indexed_at";
/// Meta key holding the fingerprint of the settings used for the last run.
const META_SETTINGS: &str = "index_settings";
/// Bump when extraction or filtering changes in a way that requires re-reading every file.
const EXTRACTOR_VERSION: u32 = 2;

/// Options for [`index_project`].
#[derive(Debug, Clone, Copy, Default)]
pub struct IndexOptions {
    /// Re-read and re-parse every file, ignoring size/mtime/hash shortcuts.
    pub force: bool,
}

/// Summary of an index run.
#[derive(Debug, Default, Clone, Serialize)]
pub struct IndexStats {
    pub scanned: usize,
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub renamed: usize,
    pub skipped_large: usize,
    pub skipped_binary: usize,
    pub skipped_sensitive: usize,
    /// Files skipped because their contents contain a recognizable credential.
    pub skipped_secret_content: usize,
    pub failed: usize,
    pub warnings: Vec<String>,
    #[serde(serialize_with = "serialize_ms")]
    pub duration: Duration,
}

fn serialize_ms<S: serde::Serializer>(d: &Duration, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_u128(d.as_millis())
}

impl IndexStats {
    pub fn changed(&self) -> bool {
        self.added + self.updated + self.removed + self.renamed > 0
    }
}

#[derive(Debug, Clone)]
struct Existing {
    id: i64,
    size: i64,
    mtime_ns: i64,
    hash: Vec<u8>,
}

enum Outcome {
    /// Content identical to the stored version; only metadata changed.
    Touched {
        mtime_ns: i64,
        size: i64,
    },
    Analyzed(Box<Parsed>),
    SkippedLarge,
    SkippedBinary,
    SkippedSecret(crate::secrets::SecretFinding),
    Failed(String),
}

struct Parsed {
    kind: FileKind,
    lang: Option<Lang>,
    size: i64,
    mtime_ns: i64,
    hash: Vec<u8>,
    analysis: Analysis,
}

/// Brings the index for `project` up to date.
pub fn index_project(
    project: &Project,
    conn: &mut Connection,
    options: IndexOptions,
) -> Result<IndexStats> {
    let started = Instant::now();
    let config = &project.config().index;
    // Settings that change what gets stored. When they differ from the last run (or the
    // database was built by an older extractor), every file must be re-read so that, for
    // example, enabling secret scanning also removes secrets that were indexed before.
    let fingerprint = format!(
        "extractor={EXTRACTOR_VERSION};scan_secrets={};max_file_bytes={}",
        config.scan_secrets, config.max_file_bytes
    );
    let mut options = options;
    if db::get_meta(conn, META_SETTINGS)?.as_deref() != Some(fingerprint.as_str()) {
        options.force = true;
    }
    let walked = walk::walk(project.root(), config)?;
    let mut stats = IndexStats {
        scanned: walked.entries.len(),
        skipped_sensitive: walked.sensitive_skipped,
        warnings: walked.errors,
        ..IndexStats::default()
    };

    let existing = load_existing(conn)?;
    let mut seen: HashSet<&str> = HashSet::with_capacity(walked.entries.len());
    let mut work: Vec<&WalkEntry> = Vec::new();
    for entry in &walked.entries {
        seen.insert(entry.rel_path.as_str());
        match existing.get(&entry.rel_path) {
            Some(old)
                if !options.force
                    && old.size == to_i64(entry.size)
                    && old.mtime_ns == entry.mtime_ns =>
            {
                stats.unchanged += 1;
            }
            _ => work.push(entry),
        }
    }

    let tx = conn.transaction()?;
    let mut added_hashes: Vec<(Vec<u8>, String)> = Vec::new();
    let max_bytes = config.max_file_bytes;
    let scan_secrets = config.scan_secrets;
    let worker_count = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(work.len().max(1));
    let next = AtomicUsize::new(0);

    std::thread::scope(|scope| -> Result<()> {
        // Bounded so that a slow writer applies back-pressure instead of buffering the repo.
        let (sender, receiver) = mpsc::sync_channel::<(usize, Outcome)>(64);
        for _ in 0..worker_count {
            let sender = sender.clone();
            let (work, existing, next) = (&work, &existing, &next);
            scope.spawn(move || {
                let mut extractor = Extractor::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(entry) = work.get(i) else { break };
                    let previous_hash = if options.force {
                        None
                    } else {
                        existing.get(&entry.rel_path).map(|e| e.hash.as_slice())
                    };
                    let outcome = process(
                        &mut extractor,
                        entry,
                        previous_hash,
                        max_bytes,
                        scan_secrets,
                    );
                    if sender.send((i, outcome)).is_err() {
                        // The writer stopped (it hit an error); nothing left to do.
                        break;
                    }
                }
            });
        }
        drop(sender);

        for (i, outcome) in receiver {
            let entry = work[i];
            let old = existing.get(&entry.rel_path);
            match outcome {
                Outcome::Touched { mtime_ns, size } => {
                    if let Some(old) = old {
                        tx.execute(
                            "UPDATE files SET mtime_ns = ?1, size = ?2 WHERE id = ?3",
                            params![mtime_ns, size, old.id],
                        )?;
                    }
                    stats.unchanged += 1;
                }
                Outcome::Analyzed(parsed) => {
                    for w in &parsed.analysis.warnings {
                        stats.warnings.push(format!("{}: {w}", entry.rel_path));
                    }
                    if old.is_none() {
                        added_hashes.push((parsed.hash.clone(), entry.rel_path.clone()));
                        stats.added += 1;
                    } else {
                        stats.updated += 1;
                    }
                    write_file(&tx, &entry.rel_path, old.map(|o| o.id), &parsed)?;
                }
                Outcome::SkippedLarge | Outcome::SkippedBinary | Outcome::SkippedSecret(_) => {
                    match outcome {
                        Outcome::SkippedLarge => stats.skipped_large += 1,
                        Outcome::SkippedSecret(found) => {
                            stats.skipped_secret_content += 1;
                            stats.warnings.push(format!(
                                "{}:{}: looks like a {}; file not indexed (remove the secret, \
                                 exclude the file, or set index.scan_secrets = false)",
                                entry.rel_path, found.line, found.kind
                            ));
                        }
                        _ => stats.skipped_binary += 1,
                    }
                    // A file that became too large, binary or secret-bearing must not keep
                    // stale index data.
                    if let Some(old) = old {
                        tx.execute("DELETE FROM files WHERE id = ?1", [old.id])?;
                        stats.removed += 1;
                    }
                }
                Outcome::Failed(message) => {
                    // Keep the previous index data; a transient read error should not erase it.
                    stats.failed += 1;
                    stats
                        .warnings
                        .push(format!("{}: {message}", entry.rel_path));
                }
            }
        }
        Ok(())
    })?;

    let removed: Vec<(&String, &Existing)> = existing
        .iter()
        .filter(|(path, _)| !seen.contains(path.as_str()))
        .collect();
    let renames = detect_renames(&removed, &added_hashes);
    for (old_path, new_path) in &renames {
        // Keep memories attached to files that moved.
        tx.execute(
            "UPDATE OR IGNORE memory_paths SET path = ?1 WHERE path = ?2",
            params![new_path, old_path],
        )?;
    }
    stats.renamed = renames.len();
    for (_, old) in &removed {
        tx.execute("DELETE FROM files WHERE id = ?1", [old.id])?;
    }
    // Renames were counted as an add plus a removal; report them once.
    stats.removed += removed.len() - renames.len();
    stats.added -= renames.len();

    if stats.changed() || options.force {
        resolve_imports(&tx)?;
    }
    db::set_meta(&tx, META_LAST_INDEXED, &now_ms().to_string())?;
    db::set_meta(&tx, META_SETTINGS, &fingerprint)?;
    tx.commit()?;

    stats.duration = started.elapsed();
    Ok(stats)
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn load_existing(conn: &Connection) -> Result<HashMap<String, Existing>> {
    let mut stmt = conn.prepare("SELECT id, path, size, mtime_ns, hash FROM files")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(1)?,
            Existing {
                id: r.get(0)?,
                size: r.get(2)?,
                mtime_ns: r.get(3)?,
                hash: r.get(4)?,
            },
        ))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (path, existing) = row?;
        map.insert(path, existing);
    }
    Ok(map)
}

fn process(
    extractor: &mut Extractor,
    entry: &WalkEntry,
    previous_hash: Option<&[u8]>,
    max_bytes: u64,
    scan_secrets: bool,
) -> Outcome {
    if entry.size > max_bytes {
        return Outcome::SkippedLarge;
    }
    let bytes = match std::fs::read(&entry.abs_path) {
        Ok(b) => b,
        Err(e) => return Outcome::Failed(format!("read failed: {e}")),
    };
    // The file may have grown between stat and read.
    if bytes.len() as u64 > max_bytes {
        return Outcome::SkippedLarge;
    }
    let sniff = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
    if sniff.contains(&0) {
        return Outcome::SkippedBinary;
    }
    let hash = blake3::hash(&bytes).as_bytes().to_vec();
    let size = to_i64(bytes.len() as u64);
    if previous_hash == Some(hash.as_slice()) {
        return Outcome::Touched {
            mtime_ns: entry.mtime_ns,
            size,
        };
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return Outcome::SkippedBinary;
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    if scan_secrets && let Some(found) = crate::secrets::find_secret(text) {
        return Outcome::SkippedSecret(found);
    }
    let kind = FileKind::detect(&entry.rel_path);
    match extractor.analyze(&entry.rel_path, kind, text) {
        Ok(analysis) => Outcome::Analyzed(Box::new(Parsed {
            kind,
            lang: Lang::from_path(&entry.rel_path),
            size,
            mtime_ns: entry.mtime_ns,
            hash,
            analysis,
        })),
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

fn write_file(tx: &Transaction<'_>, path: &str, old_id: Option<i64>, p: &Parsed) -> Result<()> {
    let now = now_ms();
    let lang = p.lang.map(Lang::name);
    let file_id = match old_id {
        Some(id) => {
            tx.execute(
                "UPDATE files SET kind = ?1, language = ?2, size = ?3, mtime_ns = ?4, hash = ?5,
                 line_count = ?6, indexed_at = ?7 WHERE id = ?8",
                params![
                    p.kind.as_str(),
                    lang,
                    p.size,
                    p.mtime_ns,
                    p.hash,
                    p.analysis.line_count,
                    now,
                    id
                ],
            )?;
            for table in ["symbols", "imports", "refs", "dependencies", "chunks"] {
                tx.execute(&format!("DELETE FROM {table} WHERE file_id = ?1"), [id])?;
            }
            id
        }
        None => {
            tx.execute(
                "INSERT INTO files (path, kind, language, size, mtime_ns, hash, line_count, indexed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![path, p.kind.as_str(), lang, p.size, p.mtime_ns, p.hash, p.analysis.line_count, now],
            )?;
            tx.last_insert_rowid()
        }
    };

    let a = &p.analysis;
    let mut stmt = tx.prepare_cached(
        "INSERT INTO symbols (file_id, name, kind, container, start_line, end_line, signature, exported)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for s in &a.symbols {
        stmt.execute(params![
            file_id,
            s.name,
            s.kind.as_str(),
            s.container,
            s.start_line,
            s.end_line,
            s.signature,
            s.exported
        ])?;
    }
    let mut stmt = tx.prepare_cached(
        "INSERT INTO imports (file_id, module, kind, line) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for i in &a.imports {
        stmt.execute(params![file_id, i.module, i.kind.as_str(), i.line])?;
    }
    let mut stmt =
        tx.prepare_cached("INSERT INTO refs (file_id, name, line, count) VALUES (?1, ?2, ?3, ?4)")?;
    for r in &a.references {
        stmt.execute(params![file_id, r.name, r.first_line, r.count])?;
    }
    let mut stmt = tx.prepare_cached(
        "INSERT INTO dependencies (file_id, ecosystem, name, version_req, kind) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for d in &a.dependencies {
        stmt.execute(params![file_id, d.ecosystem, d.name, d.version_req, d.kind])?;
    }
    let mut stmt = tx.prepare_cached(
        "INSERT INTO chunks (file_id, start_line, end_line, content, symbols, terms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for c in &a.chunks {
        stmt.execute(params![
            file_id,
            c.start_line,
            c.end_line,
            c.content,
            c.symbols,
            c.terms
        ])?;
    }
    Ok(())
}

/// Pairs removed files with added files that have identical content.
/// Only unambiguous matches (one removed, one added with the same hash) count as renames.
fn detect_renames(
    removed: &[(&String, &Existing)],
    added: &[(Vec<u8>, String)],
) -> Vec<(String, String)> {
    let mut removed_by_hash: HashMap<&[u8], Vec<&String>> = HashMap::new();
    for (path, e) in removed {
        removed_by_hash
            .entry(e.hash.as_slice())
            .or_default()
            .push(path);
    }
    let mut added_by_hash: HashMap<&[u8], Vec<&String>> = HashMap::new();
    for (hash, path) in added {
        added_by_hash.entry(hash.as_slice()).or_default().push(path);
    }
    let mut renames: Vec<(String, String)> = added_by_hash
        .iter()
        .filter_map(
            |(hash, new_paths)| match (removed_by_hash.get(hash), new_paths.as_slice()) {
                (Some(old_paths), [new_path]) if old_paths.len() == 1 => {
                    Some((old_paths[0].clone(), (*new_path).clone()))
                }
                _ => None,
            },
        )
        .collect();
    renames.sort();
    renames
}

fn resolve_imports(tx: &Transaction<'_>) -> Result<()> {
    let files: HashMap<String, i64> = {
        let mut stmt = tx.prepare("SELECT path, id FROM files")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let paths: HashSet<String> = files.keys().cloned().collect();
    let imports: Vec<(i64, String, String, String, Option<i64>)> = {
        let mut stmt = tx.prepare(
            "SELECT i.id, f.path, i.module, i.kind, i.target_file_id
             FROM imports i JOIN files f ON f.id = i.file_id WHERE f.language IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut update = tx.prepare_cached("UPDATE imports SET target_file_id = ?1 WHERE id = ?2")?;
    for (id, from, module, kind, current) in imports {
        let Some(lang) = Lang::from_path(&from) else {
            continue;
        };
        let target = resolve::resolve(lang, &from, &module, kind == "mod", &paths)
            .and_then(|p| files.get(&p).copied())
            .filter(|target| Some(*target) != files.get(&from).copied());
        if target != current {
            update.execute(params![target, id])?;
        }
    }
    Ok(())
}

/// Time of the last completed index run, in Unix milliseconds.
pub fn last_indexed_at(conn: &Connection) -> Result<Option<i64>> {
    Ok(db::get_meta(conn, META_LAST_INDEXED)?.and_then(|v| v.parse().ok()))
}
