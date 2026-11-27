# Connecting AI assistants

ContextKit talks to assistants through the [Model Context Protocol](https://modelcontextprotocol.io/) over stdio. Every assistant starts its own `contextkit mcp` process. All of those processes share the project's database, so a decision one assistant records is visible to the others.

## Test status

| Client | Configuration documented | Tested against the real client |
| :--- | :--- | :--- |
| Claude Code | Yes | Not yet |
| Codex CLI | Yes | Yes: codex-cli 0.169.0, Windows 10 |
| Cursor | Yes | Not yet |
| VS Code (Copilot agent mode) | Yes | Not yet |

The configurations below follow each client's published MCP documentation. The protocol itself is covered by automated tests (`tests/end_to_end.rs`): initialization, version negotiation, tool calls, error codes, and two clients sharing memory. That is not the same as testing each client. If you confirm a client works, or find that it doesn't, please open an issue.

## Before you start

1. Put `contextkit` on your `PATH`, or use its absolute path in the configurations below.
2. Initialize the project once:

   ```sh
   cd /path/to/your/project
   contextkit init
   ```

The server finds the project by searching upward from its working directory for `.contextkit/`. If a client starts servers from a different directory, pass `--project <path>` or set `CONTEXTKIT_PROJECT`.

## Quick setup

Run one command in the project root:

```sh
contextkit setup claude     # or: codex, cursor, vscode
```

It adds the `contextkit` server to the client's MCP configuration and a short usage section to `CLAUDE.md` (Claude Code) or `AGENTS.md` (the others). Other servers and settings in those files are kept. A file that is not plain JSON or valid TOML is left untouched and the command prints what to add by hand. Running it again changes nothing. Use `--dry-run` to preview and `--no-instructions` to skip the notes.

The sections below show what `setup` writes, for manual configuration.

## Claude Code

Register the server for the current project. This writes a `.mcp.json` file that you can commit:

```sh
claude mcp add --scope project contextkit -- contextkit mcp
```

Or register it only for yourself:

```sh
claude mcp add contextkit -- contextkit mcp
```

Claude Code starts the server in the project directory, so `--project` is not needed.

## Codex CLI

Add the server to `~/.codex/config.toml`:

```toml
[mcp_servers.contextkit]
command = "contextkit"
args = ["mcp"]
```

This configuration is global. Codex starts the server in its working directory, so the server serves whichever ContextKit project Codex was started in (verified with codex-cli 0.169.0). To pin one project instead, add `"--project", "/path/to/project"` to `args`.

To try ContextKit for one run without editing `config.toml`, pass the same settings as overrides:

```sh
codex exec -s workspace-write \
  -c 'mcp_servers.contextkit.command="contextkit"' \
  -c 'mcp_servers.contextkit.args=["mcp","--project","/path/to/project"]' \
  "Use the contextkit tools to find the open tasks, then implement one."
```

Codex identifies itself as `codex-mcp-client`, so its memory records have the source `agent:codex-mcp-client`.

### Field test

Tested on 2026-11-27 with codex-cli 0.169.0 on Windows 10, using the `-c` overrides above:

1. In a new Rust crate, the user recorded two verified rules with the CLI: "inject time; never call `Instant::now()` in library code" and "every public item needs a doc comment and a unit test".
2. A first assistant called `get_context`, received both rules, built a `TokenBucket` type, and recorded a `decision` and a `task_open` ("Add a KeyedLimiter that keeps one TokenBucket per key", with notes about evicting idle keys).
3. Codex was then told only to find the open tasks through ContextKit and implement them. It called `get_context`, `memory_search` and `read_file`, and implemented `KeyedLimiter` with idle eviction. It took time as a parameter, as the rule requires, and added tests. All 10 tests passed. It then used `memory_update` to move the task to `task_done`, and recorded its eviction design with `memory_add`.

Both of Codex's records were stored as unverified, as intended.

## Cursor

Create `.cursor/mcp.json` in the project:

```json
{
  "mcpServers": {
    "contextkit": {
      "command": "contextkit",
      "args": ["mcp", "--project", "${workspaceFolder}"]
    }
  }
}
```

## VS Code

Create `.vscode/mcp.json` in the project:

```json
{
  "servers": {
    "contextkit": {
      "type": "stdio",
      "command": "contextkit",
      "args": ["mcp", "--project", "${workspaceFolder}"]
    }
  }
}
```

## Other MCP clients

Any client that can start a stdio MCP server works the same way: the command is `contextkit` and the arguments are `mcp --project <project root>`. The server supports protocol revisions `2025-11-25`, `2025-06-18`, `2025-03-26` and `2024-11-05`, and exposes tools only (no resources or prompts).

## Tools

| Tool | Writes data | Purpose |
| :--- | :--- | :--- |
| `get_context` | No | Relevant code, project rules and memory for a task, within a token budget. |
| `search_code` | No | Full-text search; returns paths and line ranges. |
| `find_symbol` | No | Symbol definitions by name, optionally with call sites. |
| `file_outline` | No | Symbols in one file, plus its imports and importers. |
| `read_file` | No | Up to 400 lines of an indexed file. |
| `project_overview` | No | Languages, layout, dependencies, docs, open tasks, known bugs. |
| `index_refresh` | Index only | Incremental reindex. |
| `memory_search` | No | Search or list memory records. |
| `memory_add` | Memory | Add a record. It is always stored as unverified. |
| `memory_update` | Memory | Change an unverified record. |

Before code tools run, the server refreshes the index incrementally if the last refresh is older than `mcp.auto_refresh_secs` (default 30 seconds). Agents cannot delete memory, verify records, or change verified records. Those actions are only available in the CLI.

## Telling the assistant to use ContextKit

The server sends usage instructions during initialization, but not every client shows them to the model. Adding a short note to the file your assistant reads (`CLAUDE.md`, `AGENTS.md`, `.cursor/rules`, ...) makes this reliable:

```markdown
## Project context

This project uses ContextKit (MCP server `contextkit`).

- At the start of a task, call `get_context` with a one-sentence description of the task.
- Use `find_symbol`, `search_code` and `read_file` before reading whole files.
- Record decisions, conventions you were told about, discoveries, finished work and open tasks
  with `memory_add`. Link the relevant files with `paths`.
- Records marked UNVERIFIED are claims nobody has confirmed. Do not treat them as facts.
```

## Troubleshooting

- **"no ContextKit project found"**: run `contextkit init` in the project root, or pass `--project`.
- **Server logs**: the server writes logs to stderr. Set `RUST_LOG=contextkit=debug` in the client's environment settings for detail.
- **Stale results**: run `contextkit index`, run `contextkit watch` in a terminal, or call the `index_refresh` tool.
