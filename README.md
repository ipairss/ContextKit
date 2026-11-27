<h1 align="center">
  <img src="assets/logo.png" alt="ContextKit logo" width="640" />
</h1>

<p align="center"><strong>Local-first context and persistent memory for AI coding agents.</strong></p>

<p align="center">Your AI assistant forgets your project every session. ContextKit makes it remember.</p>

<div align="center">

[![Release](https://img.shields.io/github/v/release/ipairss/ContextKit?style=flat-square)](https://github.com/ipairss/ContextKit/releases)
[![CI](https://img.shields.io/github/actions/workflow/status/ipairss/ContextKit/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/ipairss/ContextKit/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-stable-000000?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![MCP](https://img.shields.io/badge/MCP-protocol-555555?style=flat-square)](https://modelcontextprotocol.io/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE)

</div>

---

## Why ContextKit?

AI coding assistants are smart, but they start every session knowing nothing about your project. So you keep explaining the same things:

- "We use SQLite, not Postgres, because…"
- "Don't touch the generated files."
- "Last time we got halfway through the login rewrite."

Switch from Claude to Codex or Cursor, or run out of context, and you explain it all again.

**ContextKit fixes that.** It sits next to your project and gives every assistant:

- **The right code.** It finds only the parts that matter for your task, instead of the whole repository.
- **Your rules.** Conventions and instructions you set once are included every time.
- **A shared memory.** Decisions, finished work, open tasks and known bugs, readable by every assistant you use.

Everything stays on your computer: no account, no cloud, no telemetry.

## Install

### Download (recommended)

1. Go to the [latest release](https://github.com/ipairss/ContextKit/releases/latest) and download the file for your system:

   | System | File name ends with |
   | :--- | :--- |
   | Windows | `x86_64-pc-windows-msvc.zip` |
   | macOS (Apple silicon) | `aarch64-apple-darwin.tar.gz` |
   | macOS (Intel) | `x86_64-apple-darwin.tar.gz` |
   | Linux | `x86_64-unknown-linux-gnu.tar.gz` (or `aarch64-…` for ARM) |

2. Unpack it and put `contextkit` somewhere on your `PATH`. The [getting started guide](docs/getting-started.md#1-install) has copy-paste commands for each system.
3. Check it works:

   ```sh
   contextkit --version
   ```

### Build from source

You need [Rust](https://rustup.rs/) 1.91 or newer and a C compiler (on Windows: Visual Studio Build Tools, "Desktop development with C++").

```sh
git clone https://github.com/ipairss/ContextKit
cd ContextKit
cargo install --path .
```

## Get started in 3 steps

**1. Open a terminal in your project folder and set it up:**

```sh
contextkit init
```

This indexes your code. It skips `.gitignore`d files, dependency folders, build output and secret files.

**2. Connect your assistant:**

```sh
contextkit setup claude     # Claude Code
contextkit setup codex      # Codex
contextkit setup cursor     # Cursor
contextkit setup vscode     # VS Code
```

**3. Start a new session in your assistant.** That's it. It now uses ContextKit on its own.

## Using it every day

Once it's set up, you mostly don't think about it. A typical day looks like this:

**You ask your assistant for something, as usual.** For example, "fix the bug where users get logged out." Before it starts, the assistant asks ContextKit for context. It gets your project rules, the open tasks, and the code that's relevant to logins, so it doesn't have to read the whole project or ask you.

**The assistant takes notes as it works.** When it makes a decision, discovers something, finishes a task, or leaves one half done, it writes that to ContextKit's memory.

**You switch assistants.** Claude hit its limit, so you open Codex. Codex sees the same rules, decisions and open tasks, and continues where Claude stopped.

**Once in a while, you review the notes.** Notes written by an AI are marked *unverified* until you check them:

```sh
contextkit memory list --status unverified
contextkit memory verify <id>     # correct: make it an official project fact
contextkit memory delete <id>     # wrong: remove it
```

### Tell it your rules once

Write down what you're tired of repeating. Your assistant receives these every time:

```sh
contextkit memory add -c convention  -t "Use tabs, not spaces"
contextkit memory add -c instruction -t "Never edit files in src/generated/"
contextkit memory add -c decision    -t "We use SQLite" -b "No server to run; good enough for our data size."
```

### Share with your team

```sh
contextkit memory export -o .contextkit/memory.json   # commit this file
contextkit memory import .contextkit/memory.json      # teammates run this
```

Imported notes arrive as unverified, so nobody can sneak in "official" rules through a file.

## Commands

| Command | What it does |
| :--- | :--- |
| `contextkit init` | Set up ContextKit in the current project |
| `contextkit setup <assistant>` | Connect Claude Code, Codex, Cursor or VS Code |
| `contextkit status` | How many files, symbols and memories are stored |
| `contextkit overview` | Languages, folders, dependencies, open tasks |
| `contextkit context "task"` | See exactly what an assistant would receive for a task |
| `contextkit search "text"` | Search your code |
| `contextkit symbol <name>` | Find where a function or class is defined (add `--refs` for its callers) |
| `contextkit memory …` | `add`, `list`, `search`, `show`, `update`, `verify`, `delete`, `export`, `import` |
| `contextkit index` | Update the index by hand (normally automatic) |
| `contextkit watch` | Keep the index updated live while you edit |

Run `contextkit <command> --help` for all options.

<details>
<summary><b>Memory categories</b></summary>

| Category | Use it for |
| :--- | :--- |
| `decision` | A design choice and why it was made |
| `convention` | A coding style rule |
| `instruction` | Something assistants must always or never do |
| `requirement` | A project requirement or constraint |
| `task_open` | Unfinished work |
| `task_done` | Finished work |
| `bug` | A known bug |
| `discovery` | Something important learned about the code |
| `dependency` | Why a library was chosen or rejected |

Your own `convention` and `instruction` records are included in every context. Open tasks and known bugs are included too.

</details>

## Supported assistants and languages

| Assistant | Status |
| :--- | :--- |
| Codex | Tested |
| Claude Code | Supported, not yet tested with the real app |
| Cursor | Supported, not yet tested with the real app |
| VS Code (Copilot) | Supported, not yet tested with the real app |
| Any other MCP client | Works over the standard MCP protocol |

ContextKit understands the structure of **Rust, JavaScript, TypeScript, Python, Lua, Luau** and Markdown. Every other text file (Go, C#, config files, docs…) is still fully searchable.

Setup details for each assistant: [docs/integrations.md](docs/integrations.md).

## Privacy and security

- **Local only.** No account, no cloud, no telemetry. ContextKit never connects to the internet.
- **Secrets stay out.** `.env` files, private keys, certificates and credential files are never indexed. Files containing API keys or tokens (GitHub, AWS, OpenAI, Anthropic, Stripe and more) are skipped, and you get a warning with the file and line.
- **Assistants can't overrule you.** Anything an AI writes is marked unverified. Assistants can't verify notes, can't change verified ones, and can't delete anything.
- **Stays in your project.** Assistants can only read files inside the project, and only files that were indexed.

Secret detection recognizes common key formats, not every possible password, so keep real secrets out of your code. More detail: [SECURITY.md](SECURITY.md).

## FAQ

**Does it send my code anywhere?**
No. ContextKit itself never uses the network. Your assistant still sends what it reads to its own AI provider, as it always does. ContextKit just makes it read less.

**Does it cost anything or need an API key?**
No. It's free and open source, and it doesn't use any AI service itself.

**Will it slow down my computer?**
No. It's a small native program (about 11 MB). It starts in about 10 ms, uses around 6 MB of memory while idle, and only re-reads files that changed.

**Where is my data?**
In a `.contextkit/` folder inside your project. Deleting it removes everything, including memories, so export them first if you want to keep them.

**My assistant isn't using it.**
Run `contextkit setup <assistant>` in the project root, then restart the assistant. Ask it once to "call get_context for …" to check the connection. See [troubleshooting](docs/getting-started.md#if-something-doesnt-work).

## Learn more

| Document | For |
| :--- | :--- |
| [Getting started](docs/getting-started.md) | Step-by-step first setup |
| [Integrations](docs/integrations.md) | Each assistant in detail, plus the list of tools ContextKit gives them |
| [How it works](docs/architecture.md) | Architecture, ranking, performance numbers |
| [Security](SECURITY.md) | Threat model and how to report a vulnerability |
| [Contributing](CONTRIBUTING.md) | Building, testing and releasing |
| [Changelog](CHANGELOG.md) | What changed in each version |

## What's next

- Testing with Claude Code, Cursor and VS Code as real clients
- Packages for winget, Homebrew and Scoop
- More languages (Go, C#, Java, C/C++)
- Optional semantic search

Ideas and bug reports are welcome in [Issues](https://github.com/ipairss/ContextKit/issues).

## License

[MIT](LICENSE)

---

<div align="center">
    <sub>Created and maintained by <a href="https://github.com/ipairss">ipairss</a>.</sub>
</div>
