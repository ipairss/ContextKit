# Changelog

All notable changes are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.0.0] - 2026-11-27

First public release.

### Security

- Files whose contents contain a recognizable credential are no longer indexed, and are refused when an assistant asks for them. New setting: `index.scan_secrets` (default `true`).
- Every file read for an assistant re-checks containment in the project root, so a file replaced by a symbolic link after indexing is refused.
- `memory import` stores records as unverified and keeps local verified records unless `--trust` is passed.
- MCP messages are size-limited while they are read, not after.
- Parsing a single file is limited to 2 seconds.
- Changing `scan_secrets` or `max_file_bytes`, or upgrading the extractor, now triggers a full reindex.
- CI: fuzzing, a weekly advisory scan, Dependabot, and SHA-pinned actions.

### Added

- `contextkit setup <claude|codex|cursor|vscode>` writes the client's MCP configuration and a usage section in `CLAUDE.md` or `AGENTS.md`. It merges with existing files, is idempotent, and supports `--dry-run`.
- Memory records track who made the last change (`updated_by`), for example when one assistant closes another's task.
- `get_context` includes open tasks and known bugs even when their wording does not match the task.
- Test code ranks below the code it tests, unless the task is about tests.
- Release workflow: tagged versions build Windows, Linux and macOS archives with checksums and build provenance, published as a draft release.
- Incremental, language-aware indexing with tree-sitter for Rust, JavaScript, TypeScript/TSX, Python, Lua and Luau, plus Markdown headings.
- Dependency extraction from `Cargo.toml`, `package.json`, `pyproject.toml`, `requirements*.txt` and `wally.toml`.
- Lexical import resolution, used to find related files.
- SQLite storage with FTS5 full-text search and versioned schema migrations.
- Persistent project memory with categories, verified/unverified status, file links, JSON and Markdown export, and JSON import.
- Deterministic context retrieval within an estimated-token budget.
- CLI: `init`, `index`, `status`, `overview`, `search`, `symbol`, `outline`, `context`, `memory`, `watch`, `mcp`.
- MCP stdio server with ten tools and an agent trust model.

[Unreleased]: https://github.com/ipairss/ContextKit/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/ipairss/ContextKit/releases/tag/v1.0.0
