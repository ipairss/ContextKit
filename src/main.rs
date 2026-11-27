//! `contextkit` command-line interface.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Parser, Subcommand};
use rusqlite::Connection;

use contextkit::context::{self, ContextRequest};
use contextkit::index::extract::SymbolKind;
use contextkit::index::{self, IndexOptions, IndexStats};
use contextkit::memory::{self, Actor, Category, Filter, Memory, MemoryPatch, NewMemory, Status};
use contextkit::search::{self, NameMatch};
use contextkit::util::format_rfc3339;
use contextkit::{Project, db, mcp, overview, setup, watch};

#[derive(Debug, Parser)]
#[command(
    name = "contextkit",
    version,
    about = "Local-first context and memory for AI coding assistants"
)]
struct Cli {
    /// Project directory (default: the nearest ancestor of the current directory containing `.contextkit/`).
    #[arg(long, global = true, env = "CONTEXTKIT_PROJECT", value_name = "PATH")]
    project: Option<PathBuf>,

    /// Increase log verbosity on stderr (-v info, -vv debug). `RUST_LOG` overrides this.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create `.contextkit/` in a project root and build the first index.
    Init {
        /// Project root (default: current directory).
        path: Option<PathBuf>,
        /// Do not index after initializing.
        #[arg(long)]
        no_index: bool,
    },
    /// Index new and changed files and drop deleted ones.
    Index {
        /// Re-read and re-parse every file.
        #[arg(long)]
        full: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show index and memory statistics.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Summarize languages, layout, dependencies, docs and open tasks.
    Overview {
        #[arg(long)]
        json: bool,
    },
    /// Full-text search over indexed files.
    Search {
        query: String,
        /// Restrict to a project-relative directory or file.
        #[arg(long)]
        path: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Find symbol definitions by name.
    Symbol {
        name: String,
        /// Match the whole name instead of a substring.
        #[arg(long)]
        exact: bool,
        /// Only this kind (function, method, class, struct, enum, interface, trait, type, ...).
        #[arg(long)]
        kind: Option<SymbolKind>,
        /// Also list files that call this name.
        #[arg(long)]
        refs: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// List the symbols defined in a file.
    Outline {
        path: String,
        #[arg(long)]
        json: bool,
    },
    /// Retrieve context for a task within a token budget.
    Context {
        /// Task description.
        #[arg(required = true, num_args = 1..)]
        task: Vec<String>,
        /// Estimated-token budget (default from config).
        #[arg(long)]
        budget: Option<usize>,
        /// Prefer results under this path (repeatable).
        #[arg(long = "focus", value_name = "PATH")]
        focus: Vec<String>,
        #[arg(long)]
        no_code: bool,
        #[arg(long)]
        no_memory: bool,
        #[arg(long)]
        json: bool,
    },
    /// Manage persistent project memory.
    #[command(subcommand)]
    Memory(MemoryCommand),
    /// Watch the project and reindex on changes (Ctrl-C to stop).
    Watch,
    /// Configure an AI assistant (claude, codex, cursor, vscode) to use ContextKit here.
    Setup {
        /// claude, codex, cursor or vscode.
        client: setup::Client,
        /// Show what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Do not add usage notes to CLAUDE.md / AGENTS.md.
        #[arg(long)]
        no_instructions: bool,
    },
    /// Run the MCP server on stdin/stdout.
    Mcp,
}

#[derive(Debug, Subcommand)]
enum MemoryCommand {
    /// Record a memory. User records are verified unless --unverified is given.
    Add(AddArgs),
    /// List memories, most recently updated first.
    List(ListArgs),
    /// Full-text search over memories.
    Search {
        query: String,
        #[command(flatten)]
        filter: ListArgs,
    },
    /// Show one memory.
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Change fields of a memory.
    Update(UpdateArgs),
    /// Mark a memory as a verified project fact.
    Verify { id: String },
    /// Mark a memory as unverified.
    Unverify { id: String },
    /// Delete a memory permanently.
    Delete {
        id: String,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Export all memories (JSON or Markdown).
    Export {
        #[arg(long, value_parser = ["json", "markdown"], default_value = "json")]
        format: String,
        /// Write to a file instead of stdout.
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Import memories from a JSON export. Newer records win; nothing is deleted.
    /// Imported records are stored as unverified and never overwrite verified ones
    /// unless --trust is given.
    Import {
        file: PathBuf,
        /// Keep the verification status from the file. Only for exports you made yourself.
        #[arg(long)]
        trust: bool,
    },
}

#[derive(Debug, Args)]
struct AddArgs {
    /// decision, convention, requirement, task_done, task_open, bug, discovery, dependency, instruction.
    #[arg(long, short)]
    category: Category,
    #[arg(long, short)]
    title: String,
    #[arg(long, short, conflicts_with = "body_file")]
    body: Option<String>,
    /// Read the body from a file (`-` for stdin).
    #[arg(long)]
    body_file: Option<PathBuf>,
    /// Related project-relative path (repeatable).
    #[arg(long = "path", value_name = "PATH")]
    paths: Vec<String>,
    /// Tag (repeatable).
    #[arg(long = "tag", value_name = "TAG")]
    tags: Vec<String>,
    /// Store as unverified.
    #[arg(long)]
    unverified: bool,
}

#[derive(Debug, Args)]
struct ListArgs {
    /// Only these categories (repeatable).
    #[arg(long = "category", short)]
    categories: Vec<Category>,
    #[arg(long)]
    status: Option<Status>,
    /// Only memories linked to this file or directory.
    #[arg(long)]
    path: Option<String>,
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct UpdateArgs {
    id: String,
    #[arg(long, short)]
    category: Option<Category>,
    #[arg(long, short)]
    title: Option<String>,
    #[arg(long, short, conflicts_with = "body_file")]
    body: Option<String>,
    #[arg(long)]
    body_file: Option<PathBuf>,
    /// Replace linked paths (repeatable).
    #[arg(long = "path", value_name = "PATH")]
    paths: Vec<String>,
    /// Remove all linked paths.
    #[arg(long, conflicts_with = "paths")]
    clear_paths: bool,
    /// Replace tags (repeatable).
    #[arg(long = "tag", value_name = "TAG")]
    tags: Vec<String>,
    /// Remove all tags.
    #[arg(long, conflicts_with = "tags")]
    clear_tags: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(verbose: u8) {
    let default = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

fn open_project(cli_path: Option<&Path>) -> Result<Project> {
    let start = match cli_path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("cannot determine the current directory")?,
    };
    Ok(Project::discover(&start)?)
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}

fn init(path: Option<PathBuf>, no_index: bool) -> Result<()> {
    let root = match path {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let (project, created) = Project::init(&root)?;
    if created {
        println!("Initialized ContextKit in {}", project.data_dir().display());
    } else {
        println!(
            "ContextKit already initialized in {}",
            project.data_dir().display()
        );
    }
    if !no_index {
        let mut conn = project.connect()?;
        let stats = index::index_project(&project, &mut conn, IndexOptions::default())?;
        print_index_stats(&stats);
    }
    println!(
        "Next: add `contextkit mcp` to your assistant's MCP configuration (see docs/integrations.md)."
    );
    Ok(())
}

fn run(cli: Cli) -> Result<()> {
    let command = match cli.command {
        Command::Init { path, no_index } => return init(path.or(cli.project), no_index),
        other => other,
    };

    let project = open_project(cli.project.as_deref())?;
    let mut conn = project.connect()?;
    let project_id = db::project_id(&conn, project.root())?;

    match command {
        // Returned early above, before a project is opened.
        Command::Init { .. } => {}
        Command::Index { full, json } => {
            let stats = index::index_project(&project, &mut conn, IndexOptions { force: full })?;
            if json {
                print_json(&stats)?;
            } else {
                print_index_stats(&stats);
            }
        }
        Command::Status { json } => status(&project, &conn, &project_id, json)?,
        Command::Overview { json } => {
            let o = overview::overview(&conn, &project_id)?;
            if json {
                print_json(&o)?;
            } else {
                print!("{}", overview::render_markdown(&o));
            }
        }
        Command::Search {
            query,
            path,
            limit,
            json,
        } => {
            let path = path
                .map(|p| contextkit::util::validate_rel_path(&p))
                .transpose()?;
            let hits = search::search_code(&conn, &query, path.as_deref(), limit.max(1))?;
            if json {
                print_json(&hits)?;
            } else if hits.is_empty() {
                println!("No matches.");
            } else {
                for h in hits {
                    let symbols = if h.symbols.is_empty() {
                        String::new()
                    } else {
                        format!("  [{}]", h.symbols)
                    };
                    println!("{}:{}-{}{symbols}", h.path, h.start_line, h.end_line);
                }
            }
        }
        Command::Symbol {
            name,
            exact,
            kind,
            refs,
            limit,
            json,
        } => {
            let mode = if exact {
                NameMatch::Exact
            } else {
                NameMatch::Contains
            };
            let hits = search::find_symbols(&conn, &name, mode, kind, limit.max(1))?;
            let references = if refs {
                search::find_references(&conn, &name, limit.max(1))?
            } else {
                Vec::new()
            };
            if json {
                print_json(&serde_json::json!({ "definitions": hits, "references": references }))?;
            } else {
                if hits.is_empty() {
                    println!("No definitions found.");
                }
                for h in &hits {
                    let container = h
                        .container
                        .as_deref()
                        .map(|c| format!("{c}::"))
                        .unwrap_or_default();
                    println!(
                        "{:<9} {container}{}  {}:{}-{}",
                        h.kind, h.name, h.path, h.start_line, h.end_line
                    );
                }
                if refs {
                    println!("\nCall sites (by name, not type-resolved):");
                    for r in &references {
                        println!("  {}:{} ({}x)", r.path, r.first_line, r.count);
                    }
                }
            }
        }
        Command::Outline { path, json } => {
            let path = contextkit::util::validate_rel_path(&path)?;
            let symbols = search::file_outline(&conn, &path)?;
            if json {
                print_json(&symbols)?;
            } else if symbols.is_empty() {
                println!("No symbols in {path} (is it indexed?).");
            } else {
                for s in symbols {
                    let indent = if s.container.is_some() { "  " } else { "" };
                    println!(
                        "{indent}{} {} ({}-{})",
                        s.kind, s.name, s.start_line, s.end_line
                    );
                }
            }
        }
        Command::Context {
            task,
            budget,
            focus,
            no_code,
            no_memory,
            json,
        } => {
            let config = project.config();
            let request = ContextRequest {
                task: task.join(" "),
                budget_tokens: config.resolve_budget(budget),
                focus_paths: focus
                    .iter()
                    .map(|p| contextkit::util::validate_rel_path(p))
                    .collect::<contextkit::Result<_>>()?,
                include_code: !no_code,
                include_memory: !no_memory,
                max_snippet_lines: config.retrieval.max_snippet_lines,
            };
            let pack = context::retrieve(
                &conn,
                &contextkit::files::SafeReader::new(&project),
                &request,
            )?;
            if json {
                print_json(&pack)?;
            } else {
                print!("{}", context::render_markdown(&pack));
            }
        }
        Command::Memory(cmd) => memory_command(&mut conn, &project_id, cmd)?,
        Command::Watch => {
            eprintln!("Watching {} (Ctrl-C to stop)", project.root().display());
            watch::watch(&project, &mut conn, |stats| {
                if stats.changed() {
                    print_index_stats(stats);
                }
                true
            })?;
        }
        Command::Setup {
            client,
            dry_run,
            no_instructions,
        } => run_setup(&project, client, dry_run, !no_instructions)?,
        Command::Mcp => {
            drop(conn);
            let mut server = mcp::Server::new(project)?;
            let stdin = std::io::stdin().lock();
            let stdout = std::io::stdout().lock();
            server.serve(stdin, stdout)?;
        }
    }
    Ok(())
}

fn print_index_stats(s: &IndexStats) {
    println!(
        "Indexed {} files in {} ms: {} added, {} updated, {} removed, {} renamed, {} unchanged.",
        s.scanned,
        s.duration.as_millis(),
        s.added,
        s.updated,
        s.removed,
        s.renamed,
        s.unchanged
    );
    let skipped =
        s.skipped_large + s.skipped_binary + s.skipped_sensitive + s.skipped_secret_content;
    if skipped > 0 {
        println!(
            "Skipped {skipped}: {} too large, {} binary or non-UTF-8, {} secret file names, {} containing secrets.",
            s.skipped_large, s.skipped_binary, s.skipped_sensitive, s.skipped_secret_content
        );
    }
    if s.failed > 0 {
        println!(
            "{} file(s) could not be read; their previous index data was kept.",
            s.failed
        );
    }
    for w in s.warnings.iter().take(20) {
        eprintln!("warning: {w}");
    }
    if s.warnings.len() > 20 {
        eprintln!("warning: … {} more", s.warnings.len() - 20);
    }
}

fn status(project: &Project, conn: &Connection, project_id: &str, json: bool) -> Result<()> {
    let count = |sql: &str| -> Result<i64> { Ok(conn.query_row(sql, [], |r| r.get(0))?) };
    let db_path = project.db_path();
    let db_bytes = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    let last = index::last_indexed_at(conn)?.map(format_rfc3339);
    let value = serde_json::json!({
        "root": project.root(),
        "project_id": project_id,
        "database": db_path,
        "database_bytes": db_bytes,
        "schema_version": db::SCHEMA_VERSION,
        "last_indexed_at": last,
        "files": count("SELECT count(*) FROM files")?,
        "symbols": count("SELECT count(*) FROM symbols")?,
        "chunks": count("SELECT count(*) FROM chunks")?,
        "memories": count("SELECT count(*) FROM memories")?,
        "unverified_memories": count("SELECT count(*) FROM memories WHERE status = 'unverified'")?,
    });
    if json {
        return print_json(&value);
    }
    println!("Project:   {}", project.root().display());
    println!("ID:        {project_id}");
    println!(
        "Database:  {} ({:.1} MiB)",
        db_path.display(),
        db_bytes as f64 / 1_048_576.0
    );
    println!("Indexed:   {}", last.as_deref().unwrap_or("never"));
    println!(
        "Contents:  {} files, {} symbols, {} chunks",
        value["files"], value["symbols"], value["chunks"]
    );
    println!(
        "Memory:    {} records ({} unverified)",
        value["memories"], value["unverified_memories"]
    );
    Ok(())
}

fn read_body(body: Option<String>, body_file: Option<PathBuf>) -> Result<Option<String>> {
    match (body, body_file) {
        (Some(b), _) => Ok(Some(b)),
        (None, Some(path)) if path.as_os_str() == "-" => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .context("reading body from stdin")?;
            Ok(Some(s))
        }
        (None, Some(path)) => Ok(Some(
            std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?,
        )),
        (None, None) => Ok(None),
    }
}

fn filter_from(args: &ListArgs) -> Result<Filter> {
    Ok(Filter {
        categories: args.categories.clone(),
        status: args.status,
        path: args
            .path
            .as_deref()
            .map(contextkit::util::validate_rel_path)
            .transpose()?,
        limit: Some(args.limit.max(1)),
    })
}

fn print_memory_line(m: &Memory) {
    let flag = if m.status == Status::Unverified {
        " [unverified]"
    } else {
        ""
    };
    println!("{}  {:<11} {}{flag}", m.id, m.category, m.title);
}

fn print_memory(m: &Memory) {
    println!("{}", m.title);
    println!("  id:       {}", m.id);
    println!("  category: {} ({})", m.category, m.category.description());
    println!("  status:   {}", m.status);
    println!("  source:   {}", m.source);
    println!("  created:  {}", format_rfc3339(m.created_at));
    println!("  updated:  {}", format_rfc3339(m.updated_at));
    if let Some(by) = &m.updated_by {
        println!("  updated by: {by}");
    }
    if !m.tags.is_empty() {
        println!("  tags:     {}", m.tags.join(", "));
    }
    if !m.paths.is_empty() {
        println!("  paths:    {}", m.paths.join(", "));
    }
    if !m.body.is_empty() {
        println!("\n{}", m.body);
    }
}

fn memory_command(conn: &mut Connection, project_id: &str, cmd: MemoryCommand) -> Result<()> {
    let user = Actor::User;
    match cmd {
        MemoryCommand::Add(a) => {
            let body = read_body(a.body, a.body_file)?.unwrap_or_default();
            let m = memory::add(
                conn,
                project_id,
                &user,
                NewMemory {
                    category: a.category,
                    title: a.title,
                    body,
                    status: if a.unverified {
                        Status::Unverified
                    } else {
                        Status::Verified
                    },
                    tags: a.tags,
                    paths: a.paths,
                },
            )?;
            println!("Added {} {} ({})", m.category, m.id, m.status);
        }
        MemoryCommand::List(args) => {
            let list = memory::list(conn, &filter_from(&args)?)?;
            if args.json {
                print_json(&list)?;
            } else if list.is_empty() {
                println!("No memories.");
            } else {
                list.iter().for_each(print_memory_line);
            }
        }
        MemoryCommand::Search { query, filter } => {
            let hits = memory::search(conn, &query, &filter_from(&filter)?)?;
            if filter.json {
                print_json(&hits)?;
            } else if hits.is_empty() {
                println!("No matches.");
            } else {
                hits.iter().for_each(|h| print_memory_line(&h.memory));
            }
        }
        MemoryCommand::Show { id, json } => {
            let m = memory::get(conn, &id)?;
            if json {
                print_json(&m)?;
            } else {
                print_memory(&m);
            }
        }
        MemoryCommand::Update(u) => {
            let body = read_body(u.body, u.body_file)?;
            let patch = MemoryPatch {
                category: u.category,
                title: u.title,
                body,
                tags: if u.clear_tags {
                    Some(Vec::new())
                } else {
                    (!u.tags.is_empty()).then_some(u.tags)
                },
                paths: if u.clear_paths {
                    Some(Vec::new())
                } else {
                    (!u.paths.is_empty()).then_some(u.paths)
                },
            };
            let m = memory::update(conn, &u.id, &user, patch)?;
            println!("Updated {} {}", m.category, m.id);
        }
        MemoryCommand::Verify { id } => {
            let m = memory::set_status(conn, &id, &user, Status::Verified)?;
            println!("Verified {} \"{}\"", m.id, m.title);
        }
        MemoryCommand::Unverify { id } => {
            let m = memory::set_status(conn, &id, &user, Status::Unverified)?;
            println!("Marked {} \"{}\" as unverified", m.id, m.title);
        }
        MemoryCommand::Delete { id, yes } => {
            let m = memory::get(conn, &id)?;
            if !yes {
                if !std::io::stdin().is_terminal() {
                    bail!("refusing to delete without confirmation; pass --yes");
                }
                eprint!("Permanently delete {} \"{}\"? [y/N] ", m.id, m.title);
                std::io::stderr().flush()?;
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim(), "y" | "Y" | "yes") {
                    println!("Cancelled.");
                    return Ok(());
                }
            }
            let m = memory::delete(conn, &m.id, &user)?;
            println!("Deleted {} \"{}\"", m.id, m.title);
        }
        MemoryCommand::Export { format, output } => {
            let doc = memory::export(conn, project_id)?;
            let text = if format == "markdown" {
                memory::render_markdown(&doc.memories)
            } else {
                let mut s = serde_json::to_string_pretty(&doc)?;
                s.push('\n');
                s
            };
            match output {
                Some(path) => {
                    std::fs::write(&path, text)
                        .with_context(|| format!("writing {}", path.display()))?;
                    eprintln!(
                        "Exported {} memories to {}",
                        doc.memories.len(),
                        path.display()
                    );
                }
                None => print!("{text}"),
            }
        }
        MemoryCommand::Import { file, trust } => {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let doc: memory::ExportDocument = serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", file.display()))?;
            let trust = if trust {
                memory::ImportTrust::Trusted
            } else {
                memory::ImportTrust::Untrusted
            };
            let stats = memory::import(conn, project_id, &doc, trust)?;
            println!(
                "Imported: {} new, {} updated, {} unchanged, {} verified records kept as they were",
                stats.inserted, stats.updated, stats.unchanged, stats.kept_verified
            );
            if trust == memory::ImportTrust::Untrusted && stats.inserted + stats.updated > 0 {
                println!(
                    "Imported records are unverified. Review them with `contextkit memory list --status unverified`."
                );
            }
        }
    }
    Ok(())
}

fn run_setup(
    project: &Project,
    client: setup::Client,
    dry_run: bool,
    instructions: bool,
) -> Result<()> {
    let codex_config = setup::default_codex_config()
        .context("cannot locate the home directory for Codex's config.toml; set CODEX_HOME")?;
    let options = setup::SetupOptions {
        client,
        command: setup::resolve_command(),
        instructions,
        codex_config,
    };
    let changes = setup::plan(project.root(), &options)?;
    if changes.is_empty() {
        println!("{client} is already set up for ContextKit. Nothing to change.");
        return Ok(());
    }
    for change in &changes {
        let verb = if change.created { "create" } else { "update" };
        if dry_run {
            println!("Would {verb} {}: {}", change.path.display(), change.summary);
            println!(
                "----- new content -----
{}-----------------------",
                change.content
            );
        } else {
            println!(
                "{} {}: {}",
                if change.created { "Created" } else { "Updated" },
                change.path.display(),
                change.summary
            );
        }
    }
    if dry_run {
        println!("Dry run: nothing was written.");
        return Ok(());
    }
    setup::apply(&changes)?;
    let next = match client {
        setup::Client::Claude => {
            "Start a new Claude Code session in this folder and approve the `contextkit` server."
        }
        setup::Client::Codex => {
            "Start Codex from this project folder; run `codex mcp list` to check the server."
        }
        setup::Client::Cursor => "Reload Cursor, then enable `contextkit` under Settings > MCP.",
        setup::Client::Vscode => {
            "Reload VS Code, then start `contextkit` from the MCP servers list."
        }
    };
    println!("Done. {next}");
    Ok(())
}
