# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's "Report a vulnerability" button on the repository's Security tab. Do not open a public issue. You should get a response within 7 days.

## Threat model

ContextKit runs locally with the permissions of the user who starts it. It makes no network connections.

### Secrets reaching an AI assistant

Several layers apply, in order:

1. **File names.** Common secret files (`.env`, `*.pem`, `id_rsa`, `credentials.json`, ...) are never indexed. See `is_sensitive` in `src/index/walk.rs`.
2. **File contents.** Files containing a recognizable credential (private key blocks, and AWS, GitHub, GitLab, Slack, Stripe, Google, Anthropic, OpenAI, npm, PyPI, Hugging Face and Discord tokens, plus Roblox security cookies) are not indexed. See `src/secrets.rs`. The warning names the file, line and kind of secret, never the value.
3. **Read time.** Every file an assistant receives, through `read_file` or `get_context`, goes through `SafeReader` (`src/files.rs`). It re-checks that the path is relative, that it still resolves inside the project root (a file swapped for a symbolic link after indexing is refused), the size limit, and the secret scan.
4. **Only indexed files.** `read_file` serves only files that are in the index, so ignore rules and exclusions apply to it.

Content scanning only recognizes formats with a distinctive structure. A password in plain text, or a token format it does not know, is not detected. Review your exclusions with `.contextkitignore` or `index.exclude`.

### An assistant corrupting project memory

Records written through MCP are always unverified. Agents cannot verify records, change or delete verified records, or delete anything. Only the CLI can do those things.

`contextkit memory import` stores every imported record as unverified, and never overwrites a record that is verified locally, unless you pass `--trust`. A tampered export file cannot plant or replace verified rules.

### Hostile input

- **MCP messages:** a message is never buffered beyond 4 MiB. Oversized lines are discarded as they stream in.
- **Source files:** parsing one file is limited to 2 seconds, and files over `index.max_file_bytes` (1 MiB by default) are skipped.
- **Search text:** user text is reduced to quoted terms before reaching SQLite FTS5, and all SQL uses bound parameters.

### Prompt injection through indexed content

Repository text is returned to assistants as data, labelled with its source path. ContextKit cannot stop an assistant from following instructions hidden in a file it reads.

### Storage

The database (`.contextkit/contextkit.db`) is not encrypted. Protect it like the source tree it describes.

## Assurance

- `unsafe` Rust is forbidden in this crate. SQLite and the tree-sitter grammars are C libraries bundled by their Rust bindings.
- CI runs `cargo deny` on every push and weekly, to check security advisories, licenses and crate sources.
- CI fuzzes the MCP message handler, source extraction and manifest parsing, search query construction, and path validation (see `fuzz/`).
- Third-party GitHub Actions are pinned to commit SHAs. Dependabot proposes updates.

No amount of testing proves the absence of vulnerabilities. If you find one, please report it.
