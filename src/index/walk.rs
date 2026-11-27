//! Repository traversal honoring `.gitignore`, `.contextkitignore` and built-in exclusions.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;

use crate::config::IndexConfig;
use crate::error::{Error, Result};
use crate::project::DATA_DIR;
use crate::util::{normalize_rel_path, system_time_ns};

/// File name for project-specific exclusion rules (gitignore syntax).
pub const IGNORE_FILE: &str = ".contextkitignore";

/// Directories and generated artifacts that are never useful context.
const BUILTIN_EXCLUDES: &[&str] = &[
    // Version control and ContextKit itself.
    ".git/",
    ".hg/",
    ".svn/",
    ".contextkit/",
    // Dependency and build directories.
    "node_modules/",
    "bower_components/",
    "target/",
    "dist/",
    "build/",
    "out/",
    ".next/",
    ".nuxt/",
    ".svelte-kit/",
    ".turbo/",
    ".parcel-cache/",
    "coverage/",
    "__pycache__/",
    ".venv/",
    "venv/",
    ".tox/",
    ".mypy_cache/",
    ".pytest_cache/",
    ".ruff_cache/",
    "*.egg-info/",
    ".gradle/",
    ".idea/",
    ".vs/",
    "vendor/",
    "Packages/",
    "DevPackages/",
    "ServerPackages/",
    // Lock files and generated bundles.
    "Cargo.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "poetry.lock",
    "uv.lock",
    "Pipfile.lock",
    "wally.lock",
    "*.min.js",
    "*.min.css",
    "*.map",
    "*.pyc",
    "*.pyo",
    // Compiled binaries and archives.
    "*.exe",
    "*.dll",
    "*.so",
    "*.dylib",
    "*.o",
    "*.obj",
    "*.a",
    "*.lib",
    "*.pdb",
    "*.rlib",
    "*.wasm",
    "*.class",
    "*.jar",
    "*.zip",
    "*.tar",
    "*.gz",
    "*.7z",
    "*.rbxl",
    "*.rbxm",
    // Media.
    "*.png",
    "*.jpg",
    "*.jpeg",
    "*.gif",
    "*.ico",
    "*.webp",
    "*.bmp",
    "*.pdf",
    "*.mp3",
    "*.mp4",
    "*.wav",
    "*.ogg",
    "*.ttf",
    "*.otf",
    "*.woff",
    "*.woff2",
];

/// A candidate file discovered by the walker.
#[derive(Debug, Clone)]
pub struct WalkEntry {
    /// Canonical project-relative path with `/` separators.
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
}

/// Outcome of a walk.
#[derive(Debug, Default)]
pub struct WalkOutput {
    pub entries: Vec<WalkEntry>,
    /// Files skipped because they look like secrets.
    pub sensitive_skipped: usize,
    /// Non-fatal traversal errors (unreadable directories, bad ignore files, ...).
    pub errors: Vec<String>,
}

/// Walks `root` and returns every file that passes the exclusion rules.
pub fn walk(root: &Path, config: &IndexConfig) -> Result<WalkOutput> {
    let mut overrides = OverrideBuilder::new(root);
    for pattern in BUILTIN_EXCLUDES.iter().map(|p| (*p).to_owned()).chain(
        config
            .exclude
            .iter()
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty()),
    ) {
        // In override globs a leading `!` means "exclude".
        let glob = format!("!{}", pattern.trim_start_matches('!'));
        overrides.add(&glob).map_err(|e| {
            Error::InvalidInput(format!("invalid exclude pattern `{pattern}`: {e}"))
        })?;
    }
    let overrides = overrides
        .build()
        .map_err(|e| Error::InvalidInput(format!("invalid exclude patterns: {e}")))?;

    let walker = WalkBuilder::new(root)
        .hidden(false)
        .parents(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .ignore(false)
        .add_custom_ignore_filename(IGNORE_FILE)
        .follow_links(config.follow_symlinks)
        .overrides(overrides)
        .build();

    let mut out = WalkOutput::default();
    for item in walker {
        let entry = match item {
            Ok(entry) => entry,
            Err(e) => {
                out.errors.push(e.to_string());
                continue;
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let abs_path = entry.path();
        let Ok(rel) = abs_path.strip_prefix(root) else {
            // A followed symlink resolved outside the root; never index it.
            continue;
        };
        let Some(rel_path) = normalize_rel_path(rel) else {
            out.errors
                .push(format!("skipping non-UTF-8 path {}", abs_path.display()));
            continue;
        };
        if rel_path
            .strip_prefix(DATA_DIR)
            .is_some_and(|rest| rest.starts_with('/'))
        {
            continue;
        }
        if !config.index_sensitive_files && is_sensitive(&rel_path) {
            out.sensitive_skipped += 1;
            continue;
        }
        if config.follow_symlinks && entry.path_is_symlink() && !resolves_inside(abs_path, root) {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                out.errors.push(format!("{rel_path}: {e}"));
                continue;
            }
        };
        let mtime_ns = metadata.modified().map(system_time_ns).unwrap_or(0);
        out.entries.push(WalkEntry {
            rel_path,
            abs_path: abs_path.to_path_buf(),
            size: metadata.len(),
            mtime_ns,
        });
    }
    out.entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(out)
}

fn resolves_inside(path: &Path, root: &Path) -> bool {
    match (std::fs::canonicalize(path), std::fs::canonicalize(root)) {
        (Ok(p), Ok(r)) => p.starts_with(r),
        _ => false,
    }
}

/// Returns true for file names that commonly contain credentials or private keys.
pub fn is_sensitive(rel_path: &str) -> bool {
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let lower = name.to_ascii_lowercase();

    if lower == ".env" || (lower.starts_with(".env.") && !is_env_template(&lower)) {
        return true;
    }
    const EXACT: &[&str] = &[
        ".npmrc",
        ".pypirc",
        ".netrc",
        ".pgpass",
        ".htpasswd",
        "credentials",
        "credentials.json",
        "secrets.json",
        "secrets.yaml",
        "secrets.yml",
        "secrets.toml",
        "service-account.json",
        "terraform.tfvars",
    ];
    if EXACT.contains(&lower.as_str()) {
        return true;
    }
    const PREFIXES: &[&str] = &["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"];
    if PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    const EXTENSIONS: &[&str] = &[
        ".pem",
        ".key",
        ".p12",
        ".pfx",
        ".jks",
        ".keystore",
        ".tfstate",
        ".kdbx",
        ".asc",
        ".gpg",
    ];
    EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

fn is_env_template(lower_name: &str) -> bool {
    [".example", ".sample", ".template", ".dist"]
        .iter()
        .any(|suffix| lower_name.ends_with(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_sensitive_files() {
        for path in [
            ".env",
            "app/.env.local",
            "certs/server.pem",
            "id_rsa",
            "x/id_ed25519.pub",
        ] {
            assert!(is_sensitive(path), "{path} should be sensitive");
        }
        for path in [
            ".env.example",
            "src/key.rs",
            "keys.md",
            "README.md",
            "environment.ts",
        ] {
            assert!(!is_sensitive(path), "{path} should not be sensitive");
        }
    }

    #[test]
    fn walk_respects_ignore_rules() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(p, text).expect("write");
        };
        write("src/main.rs", "fn main() {}");
        write("node_modules/x/index.js", "x");
        write("target/debug/out.rs", "x");
        write("ignored/a.rs", "x");
        write("custom/b.rs", "x");
        write("config/c.rs", "x");
        write(".env", "SECRET=1");
        write(".github/workflows/ci.yml", "on: push");
        write(".gitignore", "ignored/\n");
        write(IGNORE_FILE, "custom/\n");

        let config = IndexConfig {
            exclude: vec!["config/".into()],
            ..IndexConfig::default()
        };
        let out = walk(root, &config).expect("walk");
        let paths: Vec<&str> = out.entries.iter().map(|e| e.rel_path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                ".contextkitignore",
                ".github/workflows/ci.yml",
                ".gitignore",
                "src/main.rs"
            ]
        );
        assert_eq!(out.sensitive_skipped, 1);
    }
}
