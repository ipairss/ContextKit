//! The single path through which content is read from the project for an assistant.
//!
//! Every read re-checks, at read time, that the path is a plain project-relative path,
//! that it still resolves inside the project root (a file may have been replaced by a
//! symbolic link after indexing), that it is not too large, and that it contains no
//! detectable secret.

use std::path::{Path, PathBuf};

use crate::config::IndexConfig;
use crate::error::{Error, IoContext, Result};
use crate::project::{Project, canonicalize};
use crate::secrets;
use crate::util::validate_rel_path;

/// Reads project files with containment, size and secret checks.
#[derive(Debug, Clone)]
pub struct SafeReader {
    root: PathBuf,
    max_bytes: u64,
    scan_secrets: bool,
}

impl SafeReader {
    pub fn new(project: &Project) -> Self {
        Self::with_config(project.root(), &project.config().index)
    }

    /// `root` must already be canonical (as [`Project::root`] is).
    pub fn with_config(root: &Path, config: &IndexConfig) -> Self {
        Self {
            root: root.to_path_buf(),
            max_bytes: config.max_file_bytes,
            scan_secrets: config.scan_secrets,
        }
    }

    /// Reads the file at project-relative `rel_path` as UTF-8 text.
    pub fn read(&self, rel_path: &str) -> Result<String> {
        let rel = validate_rel_path(rel_path)?;
        let abs = self.root.join(&rel);
        let real = canonicalize(&abs)?;
        if !real.starts_with(&self.root) {
            return Err(Error::PermissionDenied(format!(
                "`{rel}` resolves outside the project root"
            )));
        }
        let metadata = std::fs::metadata(&real).at(&real)?;
        if !metadata.is_file() {
            return Err(Error::InvalidInput(format!(
                "`{rel}` is not a regular file"
            )));
        }
        if metadata.len() > self.max_bytes {
            return Err(Error::PermissionDenied(format!(
                "`{rel}` is larger than index.max_file_bytes"
            )));
        }
        let text = std::fs::read_to_string(&real).at(&real)?;
        if self.scan_secrets
            && let Some(found) = secrets::find_secret(&text)
        {
            return Err(Error::PermissionDenied(format!(
                "`{rel}` appears to contain a secret ({} on line {}); it will not be shared",
                found.kind, found.line
            )));
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_escapes_and_secrets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = canonicalize(dir.path()).expect("canonical");
        std::fs::write(root.join("ok.txt"), "hello").expect("write");
        std::fs::write(
            root.join("leak.txt"),
            format!("token = {}", format_args!("ghp_{}", "x".repeat(36))),
        )
        .expect("write");
        let reader = SafeReader::with_config(&root, &IndexConfig::default());
        assert_eq!(reader.read("ok.txt").expect("read"), "hello");
        assert!(matches!(
            reader.read("leak.txt"),
            Err(Error::PermissionDenied(_))
        ));
        assert!(reader.read("../ok.txt").is_err());
        assert!(reader.read("missing.txt").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_out_of_root() {
        let outside = tempfile::tempdir().expect("tempdir");
        std::fs::write(outside.path().join("secret.txt"), "outside").expect("write");
        let dir = tempfile::tempdir().expect("tempdir");
        let root = canonicalize(dir.path()).expect("canonical");
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.join("link.txt"))
            .expect("symlink");
        let reader = SafeReader::with_config(&root, &IndexConfig::default());
        assert!(matches!(
            reader.read("link.txt"),
            Err(Error::PermissionDenied(_))
        ));
    }
}
