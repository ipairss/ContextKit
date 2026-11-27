//! Locating, initializing and opening a ContextKit project.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::config::{CONFIG_TEMPLATE, Config};
use crate::db;
use crate::error::{Error, IoContext, Result};

/// Name of the per-project data directory.
pub const DATA_DIR: &str = ".contextkit";
const CONFIG_FILE: &str = "config.toml";
const DB_FILE: &str = "contextkit.db";

/// A project root plus its loaded configuration.
#[derive(Debug, Clone)]
pub struct Project {
    root: PathBuf,
    config: Config,
}

impl Project {
    /// Creates `.contextkit/` under `root` (if needed) and opens the project.
    /// Existing configuration and data are never overwritten.
    pub fn init(root: &Path) -> Result<(Self, bool)> {
        let root = dunce_canonicalize(root)?;
        let data = root.join(DATA_DIR);
        let created = !data.exists();
        std::fs::create_dir_all(&data).at(&data)?;

        let config_path = data.join(CONFIG_FILE);
        if !config_path.exists() {
            std::fs::write(&config_path, CONFIG_TEMPLATE).at(&config_path)?;
        }
        let gitignore = data.join(".gitignore");
        if !gitignore.exists() {
            // The database holds machine-local state; config and exports may be committed.
            std::fs::write(&gitignore, "contextkit.db\ncontextkit.db-*\n").at(&gitignore)?;
        }

        let project = Self::open_at(root)?;
        // Create the database and the project row eagerly so `init` surfaces problems early.
        let conn = project.connect()?;
        db::project_id(&conn, &project.root)?;
        Ok((project, created))
    }

    /// Finds the nearest directory at or above `start` that contains `.contextkit/`.
    pub fn discover(start: &Path) -> Result<Self> {
        let start = dunce_canonicalize(start)?;
        let mut dir: Option<&Path> = Some(&start);
        while let Some(d) = dir {
            if d.join(DATA_DIR).is_dir() {
                return Self::open_at(d.to_path_buf());
            }
            dir = d.parent();
        }
        Err(Error::ProjectNotFound(start))
    }

    fn open_at(root: PathBuf) -> Result<Self> {
        let config = Config::load(&root.join(DATA_DIR).join(CONFIG_FILE))?;
        Ok(Self { root, config })
    }

    /// Absolute, canonical project root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn data_dir(&self) -> PathBuf {
        self.root.join(DATA_DIR)
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir().join(DB_FILE)
    }

    /// Opens a new database connection with migrations applied.
    pub fn connect(&self) -> Result<Connection> {
        db::open(&self.db_path())
    }
}

/// Canonicalizes a path without the `\\?\` prefix on Windows where possible,
/// so paths shown to users and written to client configs stay readable.
pub(crate) fn canonicalize(path: &Path) -> Result<PathBuf> {
    dunce_canonicalize(path)
}

fn dunce_canonicalize(path: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path).at(path)?;
    #[cfg(windows)]
    {
        if let Some(s) = canonical.to_str()
            && let Some(stripped) = s.strip_prefix(r"\\?\")
        {
            // Only strip for plain drive paths; UNC and device paths need the prefix.
            if stripped.as_bytes().get(1) == Some(&b':') {
                return Ok(PathBuf::from(stripped));
            }
        }
    }
    Ok(canonical)
}
