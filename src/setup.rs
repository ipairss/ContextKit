//! `contextkit setup <client>`: writes an assistant's MCP configuration and usage notes.
//!
//! Planning is separate from writing: [`plan`] computes the new contents of every file
//! that would change, so the CLI can show a dry run and tests can check exact output.
//! Existing files are edited in place and never discarded: unrelated MCP servers and
//! settings are kept, and a file that cannot be parsed is left untouched with an error.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde_json::{Map, Value, json};

use crate::error::{Error, IoContext, Result};

/// Supported assistants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    Claude,
    Codex,
    Cursor,
    Vscode,
}

impl Client {
    pub const ALL: [Client; 4] = [
        Client::Claude,
        Client::Codex,
        Client::Cursor,
        Client::Vscode,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Client::Claude => "claude",
            Client::Codex => "codex",
            Client::Cursor => "cursor",
            Client::Vscode => "vscode",
        }
    }

    /// File, relative to the project root, that this assistant reads for instructions.
    pub const fn instructions_file(self) -> &'static str {
        match self {
            Client::Claude => "CLAUDE.md",
            Client::Codex | Client::Cursor | Client::Vscode => "AGENTS.md",
        }
    }
}

impl fmt::Display for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Client {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let lower = s.trim().to_ascii_lowercase();
        let alias = match lower.as_str() {
            "claude-code" | "claudecode" => "claude",
            "vs-code" | "code" | "copilot" => "vscode",
            other => other,
        };
        Self::ALL
            .into_iter()
            .find(|c| c.as_str() == alias)
            .ok_or_else(|| {
                Error::InvalidInput(format!(
                    "unknown client `{s}`; expected one of: claude, codex, cursor, vscode"
                ))
            })
    }
}

/// Inputs for [`plan`].
#[derive(Debug, Clone)]
pub struct SetupOptions {
    pub client: Client,
    /// Command used to start ContextKit (`contextkit`, or an absolute path).
    pub command: String,
    /// Also add usage notes to `CLAUDE.md` / `AGENTS.md`.
    pub instructions: bool,
    /// Codex's global configuration file (`$CODEX_HOME/config.toml`).
    pub codex_config: PathBuf,
}

/// A file that `setup` would create or change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: PathBuf,
    pub created: bool,
    pub summary: String,
    pub content: String,
}

/// Marks the section ContextKit owns in instruction files, so re-running is idempotent.
const START_MARKER: &str = "<!-- contextkit:start -->";
const END_MARKER: &str = "<!-- contextkit:end -->";

/// Usage notes for assistants.
pub const INSTRUCTIONS: &str = "## Project context (ContextKit)

This project uses ContextKit, an MCP server named `contextkit` that indexes the code and stores project memory shared by every assistant.

- At the start of a task, call `get_context` with a one-sentence description of the task. It returns project rules, open tasks, relevant memory and code.
- Use `find_symbol`, `search_code` and `read_file` before reading whole files.
- Record decisions, discoveries, finished work and new open tasks with `memory_add`, linking the relevant files in `paths`. When you finish an open task, move it to `task_done` with `memory_update`.
- Records marked UNVERIFIED are claims nobody has confirmed. Do not treat them as facts.
";

/// Computes the files to create or change. Files that would not change are omitted.
pub fn plan(project_root: &Path, options: &SetupOptions) -> Result<Vec<FileChange>> {
    let mut changes = Vec::new();
    let config_change = match options.client {
        Client::Claude => json_server(
            &project_root.join(".mcp.json"),
            "mcpServers",
            json!({ "command": options.command, "args": ["mcp"] }),
        )?,
        Client::Cursor => json_server(
            &project_root.join(".cursor").join("mcp.json"),
            "mcpServers",
            json!({
                "command": options.command,
                "args": ["mcp", "--project", "${workspaceFolder}"]
            }),
        )?,
        Client::Vscode => json_server(
            &project_root.join(".vscode").join("mcp.json"),
            "servers",
            json!({
                "type": "stdio",
                "command": options.command,
                "args": ["mcp", "--project", "${workspaceFolder}"]
            }),
        )?,
        Client::Codex => codex_server(&options.codex_config, &options.command)?,
    };
    changes.extend(config_change);
    if options.instructions {
        changes.extend(instructions_change(
            &project_root.join(options.client.instructions_file()),
        )?);
    }
    Ok(changes)
}

/// Writes planned changes to disk, creating parent directories as needed.
pub fn apply(changes: &[FileChange]) -> Result<()> {
    for change in changes {
        if let Some(parent) = change.path.parent() {
            std::fs::create_dir_all(parent).at(parent)?;
        }
        std::fs::write(&change.path, &change.content).at(&change.path)?;
    }
    Ok(())
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).at(path),
    }
}

/// Adds or replaces the `contextkit` entry under `key` in a JSON MCP configuration.
fn json_server(path: &Path, key: &str, server: Value) -> Result<Option<FileChange>> {
    let existing = read_optional(path)?;
    let mut root: Value = match &existing {
        None => Value::Object(Map::new()),
        Some(text) if text.trim().is_empty() => Value::Object(Map::new()),
        Some(text) => serde_json::from_str(text).map_err(|e| {
            Error::InvalidInput(format!(
                "{} is not plain JSON ({e}); it was left unchanged. Add this under \"{key}\":\n\"contextkit\": {}",
                path.display(),
                server
            ))
        })?,
    };
    let Some(object) = root.as_object_mut() else {
        return Err(Error::InvalidInput(format!(
            "{} does not contain a JSON object; it was left unchanged",
            path.display()
        )));
    };
    let servers = object
        .entry(key)
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(servers) = servers.as_object_mut() else {
        return Err(Error::InvalidInput(format!(
            "\"{key}\" in {} is not an object; it was left unchanged",
            path.display()
        )));
    };
    if servers.get("contextkit") == Some(&server) {
        return Ok(None);
    }
    let replaced = servers.insert("contextkit".to_owned(), server).is_some();
    let mut content = serde_json::to_string_pretty(&root)?;
    content.push('\n');
    Ok(Some(FileChange {
        path: path.to_path_buf(),
        created: existing.is_none(),
        summary: if replaced {
            "updated the contextkit MCP server".into()
        } else {
            "added the contextkit MCP server".into()
        },
        content,
    }))
}

/// Appends a `[mcp_servers.contextkit]` table to Codex's config, preserving the file as is.
fn codex_server(path: &Path, command: &str) -> Result<Option<FileChange>> {
    let existing = read_optional(path)?;
    let text = existing.clone().unwrap_or_default();
    let parsed: toml::Table = text.parse().map_err(|e| {
        Error::InvalidInput(format!(
            "{} is not valid TOML ({e}); it was left unchanged",
            path.display()
        ))
    })?;
    let already = parsed
        .get("mcp_servers")
        .and_then(|s| s.get("contextkit"))
        .is_some();
    if already {
        return Ok(None);
    }
    let quoted = toml::Value::String(command.to_owned()).to_string();
    let mut content = text;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    if !content.is_empty() {
        content.push('\n');
    }
    content.push_str(&format!(
        "# Added by `contextkit setup codex`. The server finds the project from Codex's\n\
         # working directory; add \"--project\", \"<path>\" to args to pin one project.\n\
         [mcp_servers.contextkit]\ncommand = {quoted}\nargs = [\"mcp\"]\n"
    ));
    Ok(Some(FileChange {
        path: path.to_path_buf(),
        created: existing.is_none(),
        summary: "added the contextkit MCP server (applies to every project)".into(),
        content,
    }))
}

/// Inserts or refreshes the marked ContextKit section of an instruction file.
fn instructions_change(path: &Path) -> Result<Option<FileChange>> {
    let existing = read_optional(path)?;
    let section = format!("{START_MARKER}\n{INSTRUCTIONS}{END_MARKER}\n");
    let text = existing.clone().unwrap_or_default();
    let content = match (text.find(START_MARKER), text.find(END_MARKER)) {
        (Some(start), Some(end)) if end > start => {
            let after = &text[end + END_MARKER.len()..];
            let after = after.strip_prefix('\n').unwrap_or(after);
            format!("{}{section}{after}", &text[..start])
        }
        _ if text.trim().is_empty() => section,
        _ => {
            let separator = if text.ends_with("\n\n") {
                ""
            } else if text.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            format!("{text}{separator}{section}")
        }
    };
    if existing.as_deref() == Some(content.as_str()) {
        return Ok(None);
    }
    Ok(Some(FileChange {
        path: path.to_path_buf(),
        created: existing.is_none(),
        summary: "added ContextKit usage notes for the assistant".into(),
        content,
    }))
}

/// The command to put in client configurations: `contextkit` when it is on `PATH`,
/// otherwise the absolute path of the running executable.
pub fn resolve_command() -> String {
    let names: &[&str] = if cfg!(windows) {
        &["contextkit.exe", "contextkit"]
    } else {
        &["contextkit"]
    };
    let on_path = std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| names.iter().any(|n| dir.join(n).is_file()))
    });
    if on_path {
        return "contextkit".to_owned();
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_else(|| "contextkit".to_owned())
}

/// Codex's configuration file: `$CODEX_HOME/config.toml`, defaulting to `~/.codex`.
pub fn default_codex_config() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return Some(PathBuf::from(home).join("config.toml"));
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(home).join(".codex").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(client: Client, codex: &Path) -> SetupOptions {
        SetupOptions {
            client,
            command: "contextkit".into(),
            instructions: true,
            codex_config: codex.to_path_buf(),
        }
    }

    #[test]
    fn claude_setup_merges_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(
            root.join(".mcp.json"),
            r#"{"mcpServers":{"other":{"command":"x"}},"keep":true}"#,
        )
        .expect("write");
        std::fs::write(root.join("CLAUDE.md"), "# My rules\nBe nice.").expect("write");

        let opts = options(Client::Claude, &root.join("codex.toml"));
        let changes = plan(root, &opts).expect("plan");
        assert_eq!(changes.len(), 2);
        apply(&changes).expect("apply");

        let config: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join(".mcp.json")).expect("read"))
                .expect("json");
        assert_eq!(config["mcpServers"]["other"]["command"], "x");
        assert_eq!(config["keep"], true);
        assert_eq!(config["mcpServers"]["contextkit"]["args"], json!(["mcp"]));
        let notes = std::fs::read_to_string(root.join("CLAUDE.md")).expect("read");
        assert!(notes.starts_with("# My rules\nBe nice.\n\n<!-- contextkit:start -->"));

        assert!(
            plan(root, &opts).expect("plan").is_empty(),
            "second run changes nothing"
        );
    }

    #[test]
    fn refreshes_marked_section_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("AGENTS.md");
        std::fs::write(
            &path,
            format!("intro\n{START_MARKER}\nold notes\n{END_MARKER}\noutro\n"),
        )
        .expect("write");
        let change = instructions_change(&path).expect("plan").expect("change");
        assert!(
            change
                .content
                .starts_with("intro\n<!-- contextkit:start -->\n## Project context")
        );
        assert!(change.content.ends_with("<!-- contextkit:end -->\noutro\n"));
        assert!(!change.content.contains("old notes"));
    }

    #[test]
    fn codex_setup_appends_without_rewriting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let codex = dir.path().join("config.toml");
        let original = "# my settings\nmodel = \"gpt-5\"   # keep this comment\n";
        std::fs::write(&codex, original).expect("write");
        let mut opts = options(Client::Codex, &codex);
        opts.instructions = false;
        opts.command = r"C:\Tools\contextkit.exe".into();
        let changes = plan(dir.path(), &opts).expect("plan");
        apply(&changes).expect("apply");
        let text = std::fs::read_to_string(&codex).expect("read");
        assert!(text.starts_with(original));
        let parsed: toml::Table = text.parse().expect("valid toml");
        assert_eq!(
            parsed["mcp_servers"]["contextkit"]["command"].as_str(),
            Some(r"C:\Tools\contextkit.exe")
        );
        assert!(plan(dir.path(), &opts).expect("plan").is_empty());
    }

    #[test]
    fn refuses_to_overwrite_unparseable_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vscode = dir.path().join(".vscode");
        std::fs::create_dir_all(&vscode).expect("mkdir");
        std::fs::write(vscode.join("mcp.json"), "{ // comment\n }").expect("write");
        let opts = options(Client::Vscode, &dir.path().join("c.toml"));
        assert!(plan(dir.path(), &opts).is_err());
        assert_eq!(
            std::fs::read_to_string(vscode.join("mcp.json")).expect("read"),
            "{ // comment\n }"
        );
    }

    #[test]
    fn parses_client_names() {
        assert_eq!("Claude-Code".parse::<Client>().ok(), Some(Client::Claude));
        assert_eq!("vs-code".parse::<Client>().ok(), Some(Client::Vscode));
        assert!("emacs".parse::<Client>().is_err());
    }
}
