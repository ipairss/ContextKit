//! Project overview and index status.

use std::fmt::Write as _;

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::error::Result;
use crate::index;
use crate::memory::{self, Category, Filter, Status};
use crate::util::format_rfc3339;

const MAX_LISTED_FILES: usize = 25;
const MAX_LISTED_DEPS: usize = 60;

#[derive(Debug, Clone, Serialize)]
pub struct Count {
    pub name: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeclaredDependency {
    pub manifest: String,
    pub ecosystem: String,
    pub name: String,
    pub version_req: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskSummary {
    pub id: String,
    pub title: String,
    pub status: Status,
}

/// A compact description of the project for orienting an assistant.
#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub project_id: String,
    pub last_indexed_at: Option<String>,
    pub file_count: usize,
    pub symbol_count: usize,
    pub languages: Vec<Count>,
    pub kinds: Vec<Count>,
    pub top_directories: Vec<Count>,
    pub docs: Vec<String>,
    pub configs: Vec<String>,
    pub dependencies: Vec<DeclaredDependency>,
    pub dependencies_truncated: bool,
    pub memory: Vec<Count>,
    pub open_tasks: Vec<TaskSummary>,
    pub known_bugs: Vec<TaskSummary>,
}

fn counts(conn: &Connection, sql: &str) -> Result<Vec<Count>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(Count {
            name: r.get(0)?,
            count: r.get::<_, i64>(1)?.try_into().unwrap_or(0),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn paths(conn: &Connection, kind: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT path FROM files WHERE kind = ?1
         ORDER BY (instr(path, '/') = 0) DESC, length(path), path LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![kind, MAX_LISTED_FILES as i64], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn summaries(conn: &Connection, category: Category) -> Result<Vec<TaskSummary>> {
    Ok(memory::list(
        conn,
        &Filter {
            categories: vec![category],
            limit: Some(15),
            ..Filter::default()
        },
    )?
    .into_iter()
    .map(|m| TaskSummary {
        id: m.id,
        title: m.title,
        status: m.status,
    })
    .collect())
}

pub fn overview(conn: &Connection, project_id: &str) -> Result<Overview> {
    let scalar = |sql: &str| -> Result<usize> {
        let v: i64 = conn.query_row(sql, [], |r| r.get(0))?;
        Ok(v.try_into().unwrap_or(0))
    };
    let mut stmt = conn.prepare(
        "SELECT f.path, d.ecosystem, d.name, d.version_req, d.kind
         FROM dependencies d JOIN files f ON f.id = d.file_id
         ORDER BY f.path, d.kind, d.name LIMIT ?1",
    )?;
    let dependencies: Vec<DeclaredDependency> = stmt
        .query_map([MAX_LISTED_DEPS as i64 + 1], |r| {
            Ok(DeclaredDependency {
                manifest: r.get(0)?,
                ecosystem: r.get(1)?,
                name: r.get(2)?,
                version_req: r.get(3)?,
                kind: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let dependencies_truncated = dependencies.len() > MAX_LISTED_DEPS;

    Ok(Overview {
        project_id: project_id.to_owned(),
        last_indexed_at: index::last_indexed_at(conn)?.map(format_rfc3339),
        file_count: scalar("SELECT count(*) FROM files")?,
        symbol_count: scalar("SELECT count(*) FROM symbols WHERE kind <> 'heading'")?,
        languages: counts(
            conn,
            "SELECT language, count(*) FROM files WHERE language IS NOT NULL
             GROUP BY language ORDER BY count(*) DESC, language",
        )?,
        kinds: counts(conn, "SELECT kind, count(*) FROM files GROUP BY kind ORDER BY count(*) DESC, kind")?,
        top_directories: counts(
            conn,
            "SELECT CASE WHEN instr(path, '/') = 0 THEN '.' ELSE substr(path, 1, instr(path, '/') - 1) END AS dir,
                    count(*) FROM files GROUP BY dir ORDER BY count(*) DESC, dir LIMIT 20",
        )?,
        docs: paths(conn, "doc")?,
        configs: paths(conn, "config")?,
        dependencies: dependencies.into_iter().take(MAX_LISTED_DEPS).collect(),
        dependencies_truncated,
        memory: counts(
            conn,
            "SELECT category || ' (' || status || ')', count(*) FROM memories
             GROUP BY category, status ORDER BY category, status",
        )?,
        open_tasks: summaries(conn, Category::TaskOpen)?,
        known_bugs: summaries(conn, Category::Bug)?,
    })
}

pub fn render_markdown(o: &Overview) -> String {
    let mut out = String::from("# Project overview\n\n");
    let _ = writeln!(
        out,
        "{} files, {} symbols indexed. Last indexed: {}.",
        o.file_count,
        o.symbol_count,
        o.last_indexed_at
            .as_deref()
            .unwrap_or("never (run `contextkit index`)")
    );
    let list = |out: &mut String, title: &str, items: &[Count]| {
        if items.is_empty() {
            return;
        }
        let _ = writeln!(out, "\n## {title}");
        for c in items {
            let _ = writeln!(out, "- {}: {}", c.name, c.count);
        }
    };
    list(&mut out, "Languages (files)", &o.languages);
    list(
        &mut out,
        "Top-level directories (files)",
        &o.top_directories,
    );
    if !o.dependencies.is_empty() {
        let _ = writeln!(out, "\n## Declared dependencies");
        let mut manifest = "";
        for d in &o.dependencies {
            if d.manifest != manifest {
                manifest = &d.manifest;
                let _ = writeln!(out, "\n{} ({}):", d.manifest, d.ecosystem);
            }
            let kind = if d.kind == "normal" {
                String::new()
            } else {
                format!(" [{}]", d.kind)
            };
            let _ = writeln!(out, "- {} {}{kind}", d.name, d.version_req);
        }
        if o.dependencies_truncated {
            let _ = writeln!(out, "- … more not shown");
        }
    }
    for (title, files) in [("Documentation", &o.docs), ("Configuration", &o.configs)] {
        if !files.is_empty() {
            let _ = writeln!(out, "\n## {title}\n{}", files.join(", "));
        }
    }
    list(&mut out, "Memory records", &o.memory);
    for (title, tasks) in [("Open tasks", &o.open_tasks), ("Known bugs", &o.known_bugs)] {
        if tasks.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n## {title}");
        for t in tasks {
            let mark = if t.status == Status::Verified {
                ""
            } else {
                " (unverified)"
            };
            let _ = writeln!(out, "- {}{mark} — `{}`", t.title, t.id);
        }
    }
    out
}
