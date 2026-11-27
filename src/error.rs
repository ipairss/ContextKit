//! Error type shared by the library.

use std::path::{Path, PathBuf};

/// Errors produced by ContextKit library code.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid configuration in {path}: {message}")]
    Config { path: PathBuf, message: String },

    #[error(
        "no ContextKit project found at or above {0}; run `contextkit init` in the project root"
    )]
    ProjectNotFound(PathBuf),

    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    InvalidInput(String),

    #[error("{0}")]
    PermissionDenied(String),

    #[error(
        "database schema version {found} is newer than this build supports ({supported}); upgrade ContextKit"
    )]
    SchemaTooNew { found: i64, supported: i64 },

    #[error("tree-sitter setup failed for {language}: {message}")]
    Grammar {
        language: &'static str,
        message: String,
    },

    #[error("file watcher error: {0}")]
    Watch(#[from] notify::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Convenience alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Attaches a path to an [`std::io::Error`].
pub(crate) trait IoContext<T> {
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}
