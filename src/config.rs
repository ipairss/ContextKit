//! Project configuration stored in `.contextkit/config.toml`.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};

/// Upper bound for any context budget, regardless of configuration.
pub const HARD_MAX_BUDGET_TOKENS: usize = 200_000;

/// Top-level configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub index: IndexConfig,
    pub retrieval: RetrievalConfig,
    pub mcp: McpConfig,
}

/// Controls which files are indexed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexConfig {
    /// Additional gitignore-style patterns to exclude, relative to the project root.
    pub exclude: Vec<String>,
    /// Files larger than this are skipped.
    pub max_file_bytes: u64,
    /// Follow symbolic links while walking. Links that leave the project root are never followed.
    pub follow_symlinks: bool,
    /// Index files that would otherwise be skipped as likely secrets (`.env`, `*.pem`, ...).
    /// Leave this disabled unless you are certain no secrets live in the repository.
    pub index_sensitive_files: bool,
    /// Skip files whose contents contain a recognizable credential (private keys, cloud and
    /// API tokens), and refuse to serve such files to assistants.
    pub scan_secrets: bool,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            exclude: Vec::new(),
            max_file_bytes: 1024 * 1024,
            follow_symlinks: false,
            index_sensitive_files: false,
            scan_secrets: true,
        }
    }
}

/// Controls context retrieval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetrievalConfig {
    /// Budget used when the caller does not specify one (estimated tokens).
    pub default_budget_tokens: usize,
    /// Largest budget a caller may request (estimated tokens).
    pub max_budget_tokens: usize,
    /// Longest code snippet included for a single item.
    pub max_snippet_lines: usize,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            default_budget_tokens: 4_000,
            max_budget_tokens: 32_000,
            max_snippet_lines: 80,
        }
    }
}

/// Controls the MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    /// Before answering a tool call, refresh the index incrementally if the last refresh
    /// is older than this many seconds. `0` disables automatic refresh.
    pub auto_refresh_secs: u64,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            auto_refresh_secs: 30,
        }
    }
}

/// Commented template written by `contextkit init`.
pub const CONFIG_TEMPLATE: &str = r#"# ContextKit project configuration.
# All keys are optional; the values shown are the defaults.

[index]
# Extra gitignore-style exclusion patterns. `.gitignore` and `.contextkitignore`
# files are always respected.
exclude = []
max_file_bytes = 1048576
follow_symlinks = false
# Files that commonly hold secrets (.env, *.pem, id_rsa, ...) are never indexed
# unless this is set to true.
index_sensitive_files = false
# Skip files whose contents look like they contain a credential (private keys,
# AWS/GitHub/Stripe/... tokens), and never send such files to assistants.
scan_secrets = true

[retrieval]
default_budget_tokens = 4000
max_budget_tokens = 32000
max_snippet_lines = 80

[mcp]
# Incrementally refresh the index before a tool call when the last refresh is
# older than this many seconds. 0 disables automatic refresh.
auto_refresh_secs = 30
"#;

impl Config {
    /// Loads configuration from `path`. A missing file yields the defaults.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).at(path),
        };
        let config: Self = toml::from_str(&text).map_err(|e| Error::Config {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        config.validate(path)?;
        Ok(config)
    }

    fn validate(&self, path: &Path) -> Result<()> {
        let invalid = |message: String| Error::Config {
            path: path.to_path_buf(),
            message,
        };
        let r = &self.retrieval;
        if r.max_budget_tokens == 0 || r.max_budget_tokens > HARD_MAX_BUDGET_TOKENS {
            return Err(invalid(format!(
                "retrieval.max_budget_tokens must be between 1 and {HARD_MAX_BUDGET_TOKENS}"
            )));
        }
        if r.default_budget_tokens == 0 || r.default_budget_tokens > r.max_budget_tokens {
            return Err(invalid(
                "retrieval.default_budget_tokens must be between 1 and retrieval.max_budget_tokens"
                    .into(),
            ));
        }
        if r.max_snippet_lines == 0 {
            return Err(invalid(
                "retrieval.max_snippet_lines must be at least 1".into(),
            ));
        }
        if self.index.max_file_bytes == 0 {
            return Err(invalid("index.max_file_bytes must be at least 1".into()));
        }
        Ok(())
    }

    /// Clamps a requested budget into the configured range.
    pub fn resolve_budget(&self, requested: Option<usize>) -> usize {
        requested
            .unwrap_or(self.retrieval.default_budget_tokens)
            .clamp(1, self.retrieval.max_budget_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_to_defaults() {
        let parsed: Config = toml::from_str(CONFIG_TEMPLATE).expect("template must parse");
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("[index]\nnope = 1\n").is_err());
    }

    #[test]
    fn clamps_budget() {
        let c = Config::default();
        assert_eq!(c.resolve_budget(None), 4_000);
        assert_eq!(c.resolve_budget(Some(0)), 1);
        assert_eq!(c.resolve_budget(Some(1_000_000)), 32_000);
    }
}
