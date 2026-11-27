# Getting started

This guide takes you from download to a working setup in about five minutes. You need a project folder and an AI coding assistant that supports MCP: Claude Code, Codex, Cursor or VS Code.

## 1. Install

Download the archive for your system from the [latest release](https://github.com/ipairss/ContextKit/releases/latest):

| System | File |
| :--- | :--- |
| Windows | `contextkit-<version>-x86_64-pc-windows-msvc.zip` |
| macOS (Apple silicon) | `contextkit-<version>-aarch64-apple-darwin.tar.gz` |
| macOS (Intel) | `contextkit-<version>-x86_64-apple-darwin.tar.gz` |
| Linux (x86_64) | `contextkit-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux (ARM64) | `contextkit-<version>-aarch64-unknown-linux-gnu.tar.gz` |

Unpack it and put the `contextkit` program somewhere on your `PATH`.

**Windows**, in PowerShell, from the folder where you unpacked the archive:

```powershell
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\Programs\contextkit" | Out-Null
Copy-Item .\contextkit.exe "$env:LOCALAPPDATA\Programs\contextkit\"
[Environment]::SetEnvironmentVariable("Path", [Environment]::GetEnvironmentVariable("Path", "User") + ";$env:LOCALAPPDATA\Programs\contextkit", "User")
```

Open a new terminal afterwards. Windows SmartScreen may warn about an unrecognized app the first time, because the binary is not code-signed. You can check where it came from with `gh attestation verify` (see the README).

**macOS and Linux:**

```sh
mkdir -p ~/.local/bin
cp contextkit ~/.local/bin/
```

Make sure `~/.local/bin` is on your `PATH`. On macOS, if Gatekeeper blocks the unsigned binary, run `xattr -d com.apple.quarantine ~/.local/bin/contextkit`.

Check that it works:

```sh
contextkit --version
```

## 2. Set up a project

In your project's root folder:

```sh
contextkit init
```

This creates a `.contextkit/` folder and indexes your code. Files listed in `.gitignore`, dependency folders, build output and secret files (such as `.env` and private keys) are skipped automatically.

Then connect your assistant:

```sh
contextkit setup claude     # Claude Code
contextkit setup codex      # Codex CLI
contextkit setup cursor     # Cursor
contextkit setup vscode     # VS Code
```

This registers ContextKit with the assistant and adds a short note to `CLAUDE.md` or `AGENTS.md` telling it how to use ContextKit. Restart the assistant or open a new session.

## 3. Tell it your rules once

Write down the things you are tired of repeating. They are included in every context the assistant receives:

```sh
contextkit memory add -c convention -t "Use tabs, not spaces" -b "Matches the existing code."
contextkit memory add -c instruction -t "Never edit generated files in src/gen/"
```

Rules you add yourself are **verified**. Rules an assistant records are **unverified** until you confirm them.

## 4. Work as usual

Ask your assistant for help the way you always do. Behind the scenes it:

- calls `get_context` at the start of a task, and gets your rules, open tasks and the relevant code;
- records decisions, discoveries, finished work and new open tasks as it goes.

If you switch to another assistant, it sees the same memory.

## 5. Review what assistants wrote down

Every now and then:

```sh
contextkit memory list --status unverified
contextkit memory show <id>
contextkit memory verify <id>     # it's correct: make it a project fact
contextkit memory delete <id>     # it's wrong: remove it
```

## Useful commands

| Command | What it does |
| :--- | :--- |
| `contextkit status` | Index and memory statistics |
| `contextkit overview` | Languages, layout, dependencies, open tasks |
| `contextkit context "task"` | See exactly what an assistant would receive for a task |
| `contextkit search "text"` | Search the code |
| `contextkit memory export -o memory.json` | Back up or share memory (commit it for your team) |
| `contextkit watch` | Keep the index updated while you edit (optional; the MCP server also refreshes on its own) |

## If something doesn't work

- **"no ContextKit project found":** run `contextkit init` in the project root.
- **The assistant doesn't use ContextKit:** check that `setup` ran in the project root, then restart the assistant. Ask it "call get_context for ..." once to confirm the server is connected.
- **A file is missing from results:** it may be ignored, too large (over 1 MiB), binary, or contain something that looks like a secret. `contextkit index` prints a warning for skipped secrets.
- **Still stuck:** open an issue at <https://github.com/ipairss/ContextKit/issues>.
