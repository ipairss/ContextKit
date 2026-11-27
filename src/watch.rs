//! Reindexing on filesystem changes.
//!
//! Events are debounced, then a normal incremental index run brings the database up to
//! date. Reusing the incremental pipeline (instead of patching single paths) keeps ignore
//! handling, rename detection and import resolution identical to `contextkit index`;
//! unchanged files cost one `stat` each.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};
use rusqlite::Connection;

use crate::error::Result;
use crate::index::{self, IndexOptions, IndexStats};
use crate::project::{DATA_DIR, Project};

/// Quiet period after the last event before reindexing.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// Watches the project and calls `on_update` after every reindex.
/// Runs until the watcher's event channel closes or `on_update` returns `false`.
pub fn watch(
    project: &Project,
    conn: &mut Connection,
    mut on_update: impl FnMut(&IndexStats) -> bool,
) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        // A send error means the receiver is gone because watching stopped.
        let _ = tx.send(event);
    })?;
    watcher.watch(project.root(), RecursiveMode::Recursive)?;

    let stats = index::index_project(project, conn, IndexOptions::default())?;
    if !on_update(&stats) {
        return Ok(());
    }

    loop {
        let Ok(first) = rx.recv() else { return Ok(()) };
        let mut relevant = is_relevant(project.root(), first);
        // Debounce: keep draining until the tree has been quiet for DEBOUNCE.
        loop {
            match rx.recv_timeout(DEBOUNCE) {
                Ok(event) => relevant |= is_relevant(project.root(), event),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
        if !relevant {
            continue;
        }
        let stats = index::index_project(project, conn, IndexOptions::default())?;
        if !on_update(&stats) {
            return Ok(());
        }
    }
}

fn is_relevant(root: &Path, event: notify::Result<notify::Event>) -> bool {
    match event {
        Ok(event) => {
            if matches!(event.kind, EventKind::Access(_)) {
                return false;
            }
            event.paths.iter().any(|p| {
                let rel = p.strip_prefix(root).unwrap_or(p);
                let first = rel.components().next().and_then(|c| c.as_os_str().to_str());
                !matches!(first, Some(DATA_DIR) | Some(".git"))
            })
        }
        Err(e) => {
            // Overflowed or failed watches can miss events; a full incremental pass recovers.
            tracing::warn!(error = %e, "file watcher reported an error; rescanning");
            true
        }
    }
}
