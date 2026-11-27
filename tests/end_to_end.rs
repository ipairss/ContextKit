//! End-to-end tests over a temporary project: indexing lifecycle, retrieval, memory
//! trust rules and the MCP stdio protocol.

use std::fs;
use std::path::Path;

use contextkit::context::{self, ContextRequest, ItemBody};
use contextkit::index::{IndexOptions, index_project};
use contextkit::mcp::Server;
use contextkit::memory::{self, Actor, Category, Filter, NewMemory, Status};
use contextkit::search::{self, NameMatch};
use contextkit::{Project, db};
use serde_json::{Value, json};

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, text).expect("write");
}

fn sample_project() -> (tempfile::TempDir, Project) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write(
        root,
        "src/lib.rs",
        "mod auth;\npub use auth::RateLimiter;\n\npub fn start() { auth::check(); }\n",
    );
    write(
        root,
        "src/auth.rs",
        "/// Token bucket limiter.\npub struct RateLimiter { capacity: u32 }\n\nimpl RateLimiter {\n    pub fn allow(&mut self) -> bool { self.capacity > 0 }\n}\n\npub fn check() {}\n",
    );
    write(
        root,
        "web/api.ts",
        "import { log } from \"./log\";\nexport async function fetchUser(id: string) { log(id); }\n",
    );
    write(root, "web/log.ts", "export function log(msg: string) {}\n");
    write(
        root,
        "README.md",
        "# Demo\n\n## Rate limiting\nRequests are limited per user.\n",
    );
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"demo\"\n\n[dependencies]\nserde = \"1\"\n",
    );
    write(root, ".env", "API_KEY=super-secret\n");
    write(root, "node_modules/x/index.js", "module.exports = 1;\n");
    let (project, created) = Project::init(root).expect("init");
    assert!(created);
    (dir, project)
}

fn file_paths(conn: &rusqlite::Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT path FROM files ORDER BY path")
        .expect("prepare");
    stmt.query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

#[test]
fn indexing_lifecycle() {
    let (dir, project) = sample_project();
    let root = dir.path();
    let mut conn = project.connect().expect("connect");

    let first = index_project(&project, &mut conn, IndexOptions::default()).expect("index");
    assert_eq!(first.added, 6, "{first:?}");
    assert_eq!(first.skipped_sensitive, 1);
    let paths = file_paths(&conn);
    assert!(
        !paths
            .iter()
            .any(|p| p == ".env" || p.starts_with("node_modules"))
    );
    assert!(!paths.iter().any(|p| p.starts_with(".contextkit")));

    // Imports resolve across files.
    let rel = search::file_relations(&conn, "src/auth.rs", 10).expect("relations");
    assert_eq!(rel.imported_by, vec!["src/lib.rs"]);
    let rel = search::file_relations(&conn, "web/api.ts", 10).expect("relations");
    assert_eq!(rel.imports, vec!["web/log.ts"]);

    // A second run reads nothing.
    let second = index_project(&project, &mut conn, IndexOptions::default()).expect("index");
    assert_eq!((second.added, second.updated, second.unchanged), (0, 0, 6));

    // Memory linked to a file follows a rename.
    let pid = db::project_id(&conn, root).expect("pid");
    memory::add(
        &mut conn,
        &pid,
        &Actor::User,
        NewMemory {
            category: Category::Decision,
            title: "Logging goes through one helper".into(),
            body: String::new(),
            status: Status::Verified,
            tags: vec![],
            paths: vec!["web/log.ts".into()],
        },
    )
    .expect("memory");
    fs::rename(root.join("web/log.ts"), root.join("web/logger.ts")).expect("rename");
    write(
        root,
        "src/auth.rs",
        "pub struct RateLimiter;\npub fn check_quota() {}\n",
    );
    fs::remove_file(root.join("README.md")).expect("remove");
    let third = index_project(&project, &mut conn, IndexOptions::default()).expect("index");
    assert_eq!(third.renamed, 1, "{third:?}");
    assert_eq!(third.updated, 1);
    assert_eq!(third.removed, 1);
    let linked = memory::list(
        &conn,
        &Filter {
            path: Some("web/logger.ts".into()),
            ..Filter::default()
        },
    )
    .expect("list");
    assert_eq!(linked.len(), 1);

    // Modified file: old symbols gone, new ones present.
    assert!(
        search::find_symbols(&conn, "allow", NameMatch::Exact, None, 5)
            .expect("symbols")
            .is_empty()
    );
    assert_eq!(
        search::find_symbols(&conn, "check_quota", NameMatch::Exact, None, 5)
            .expect("symbols")
            .len(),
        1
    );
    // Deleted file: its content is no longer searchable.
    assert!(
        search::search_code(&conn, "requests", None, 5)
            .expect("search")
            .is_empty()
    );
}

#[test]
fn context_retrieval_respects_budget_and_ranks_rules_first() {
    let (_dir, project) = sample_project();
    let mut conn = project.connect().expect("connect");
    index_project(&project, &mut conn, IndexOptions::default()).expect("index");
    let pid = db::project_id(&conn, project.root()).expect("pid");
    memory::add(
        &mut conn,
        &pid,
        &Actor::User,
        NewMemory {
            category: Category::Convention,
            title: "Limiters must be lock-free".into(),
            body: "Use atomics, never a Mutex, in RateLimiter.".into(),
            status: Status::Verified,
            tags: vec![],
            paths: vec![],
        },
    )
    .expect("memory");
    // Unrelated wording: must still surface as open work.
    memory::add(
        &mut conn,
        &pid,
        &Actor::User,
        NewMemory {
            category: Category::TaskOpen,
            title: "Migrate logging to structured JSON".into(),
            body: String::new(),
            status: Status::Verified,
            tags: vec![],
            paths: vec![],
        },
    )
    .expect("memory");
    memory::add(
        &mut conn,
        &pid,
        &Actor::Agent("test".into()),
        NewMemory {
            category: Category::Discovery,
            title: "RateLimiter capacity never refills".into(),
            body: String::new(),
            status: Status::Verified,
            tags: vec![],
            paths: vec!["src/auth.rs".into()],
        },
    )
    .expect("memory");

    let request = ContextRequest {
        task: "make RateLimiter refill its capacity".into(),
        budget_tokens: 2_000,
        focus_paths: vec![],
        include_code: true,
        include_memory: true,
        max_snippet_lines: 80,
    };
    let pack = context::retrieve(
        &conn,
        &contextkit::files::SafeReader::new(&project),
        &request,
    )
    .expect("retrieve");
    assert!(pack.used.value <= pack.budget.value);
    assert!(!pack.used.exact);
    match &pack.items[0].body {
        ItemBody::Memory { status, title, .. } => {
            assert_eq!(*status, Status::Verified);
            assert_eq!(title, "Limiters must be lock-free");
        }
        other => panic!("expected a pinned rule first, got {other:?}"),
    }
    assert!(pack.items.iter().any(|i| matches!(
        &i.body,
        ItemBody::Code { path, symbol: Some(s), .. } if path == "src/auth.rs" && s.contains("RateLimiter")
    )));
    assert!(pack.items.iter().any(|i| matches!(
        &i.body,
        ItemBody::Memory {
            status: Status::Unverified,
            ..
        }
    )));
    let markdown = context::render_markdown(&pack);
    assert!(markdown.contains("src/auth.rs:"));
    assert!(markdown.contains("UNVERIFIED"));
    assert!(
        markdown.contains("Migrate logging to structured JSON"),
        "open tasks belong in every context pack:\n{markdown}"
    );

    // A tiny budget still returns a valid pack and reports what was left out.
    let tiny = context::retrieve(
        &conn,
        &contextkit::files::SafeReader::new(&project),
        &ContextRequest {
            budget_tokens: 120,
            ..request
        },
    )
    .expect("retrieve");
    assert!(tiny.used.value <= 120 || tiny.items.is_empty());
    assert!(tiny.omitted > 0);
}

struct Session {
    server: Server,
    next_id: u64,
}

impl Session {
    fn start(project: Project, client: &str) -> Self {
        let mut s = Self {
            server: Server::new(project).expect("server"),
            next_id: 1,
        };
        let init = s.request(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": client, "version": "1" } }),
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(
            s.server
                .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none()
        );
        s
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let response = self
            .server
            .handle_line(&line.to_string())
            .expect("response");
        assert_eq!(response["id"], id);
        response
    }

    fn call(&mut self, tool: &str, args: Value) -> (bool, String) {
        let r = self.request("tools/call", json!({ "name": tool, "arguments": args }));
        let result = &r["result"];
        (
            result["isError"].as_bool().expect("isError"),
            result["content"][0]["text"]
                .as_str()
                .expect("text")
                .to_owned(),
        )
    }
}

#[test]
fn mcp_cross_agent_memory_and_safety() {
    let (_dir, project) = sample_project();

    let mut claude = Session::start(project.clone(), "Claude Code");
    let tools = claude.request("tools/list", json!({}));
    assert!(tools["result"]["tools"].as_array().expect("tools").len() >= 10);

    let (err, text) = claude.call(
        "memory_add",
        json!({ "category": "task_open", "title": "Add refill to RateLimiter", "paths": ["src/auth.rs"] }),
    );
    assert!(!err, "{text}");
    assert!(text.contains("UNVERIFIED"));

    // A second assistant sees the first one's record.
    let mut codex = Session::start(project.clone(), "codex");
    let (_, found) = codex.call("memory_search", json!({ "query": "refill" }));
    assert!(found.contains("Add refill to RateLimiter"), "{found}");
    assert!(found.contains("agent:claude-code"));

    // Agents can update unverified records but cannot touch verified ones.
    let conn = project.connect().expect("connect");
    let id = memory::list(&conn, &Filter::default()).expect("list")[0]
        .id
        .clone();
    let (err, _) = codex.call(
        "memory_update",
        json!({ "id": id, "category": "task_done" }),
    );
    assert!(!err);
    let updated = memory::get(&conn, &id).expect("get");
    assert_eq!(updated.source, "agent:claude-code");
    assert_eq!(updated.updated_by.as_deref(), Some("agent:codex"));
    let mut conn = conn;
    memory::set_status(&mut conn, &id, &Actor::User, Status::Verified).expect("verify");
    let (err, text) = codex.call("memory_update", json!({ "id": id, "title": "hijack" }));
    assert!(err);
    assert!(text.contains("verified"), "{text}");

    // Code tools work and the index is built on demand.
    let (err, text) = codex.call(
        "find_symbol",
        json!({ "name": "RateLimiter", "exact": true }),
    );
    assert!(!err);
    assert!(text.contains("src/auth.rs"), "{text}");
    let (err, text) = codex.call(
        "read_file",
        json!({ "path": "src/auth.rs", "start_line": 2, "end_line": 2 }),
    );
    assert!(!err);
    assert!(text.contains("pub struct RateLimiter"), "{text}");

    // Excluded, secret and out-of-root files are not readable.
    for path in [
        ".env",
        "node_modules/x/index.js",
        "../outside.txt",
        "/etc/passwd",
    ] {
        let (err, text) = codex.call("read_file", json!({ "path": path }));
        assert!(err, "{path} should be refused: {text}");
        assert!(!text.contains("super-secret"));
    }

    let (err, text) = codex.call(
        "get_context",
        json!({ "task": "RateLimiter refill", "budget_tokens": 1500 }),
    );
    assert!(!err);
    assert!(text.contains("RateLimiter"), "{text}");

    // Protocol errors.
    let r = codex.request("no/such/method", json!({}));
    assert_eq!(r["error"]["code"], -32601);
    let r = codex.request("tools/call", json!({ "name": "nope", "arguments": {} }));
    assert_eq!(r["error"]["code"], -32602);
    let r = codex.server.handle_line("{not json").expect("response");
    assert_eq!(r["error"]["code"], -32700);
}

#[test]
fn mcp_negotiates_protocol_version() {
    let (_dir, project) = sample_project();
    let mut server = Server::new(project).expect("server");
    let response = server
        .handle_line(
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                      "params": { "protocolVersion": "1999-01-01", "capabilities": {}, "clientInfo": { "name": "x", "version": "1" } } })
                .to_string(),
        )
        .expect("response");
    assert_eq!(
        response["result"]["protocolVersion"],
        contextkit::mcp::SUPPORTED_PROTOCOL_VERSIONS[0]
    );
}

#[test]
fn secrets_in_file_contents_are_never_indexed_or_served() {
    let (dir, project) = sample_project();
    let root = dir.path();
    // Assembled at runtime so this repository never contains a real-looking token.
    let token = format!("ghp_{}", "A".repeat(36));
    write(
        root,
        "src/config.rs",
        &format!("pub const TOKEN: &str = \"{token}\";\n"),
    );

    let mut conn = project.connect().expect("connect");
    let stats = index_project(&project, &mut conn, IndexOptions::default()).expect("index");
    assert_eq!(stats.skipped_secret_content, 1, "{stats:?}");
    assert!(!file_paths(&conn).iter().any(|p| p == "src/config.rs"));
    assert!(stats.warnings.iter().all(|w| !w.contains(&token)));

    // A file that gains a secret after indexing is never served either.
    write(
        root,
        "web/log.ts",
        &format!("export const t = \"{token}\";\n"),
    );
    let mut session = Session::start(project, "test");
    let (err, text) = session.call("read_file", json!({ "path": "web/log.ts" }));
    assert!(err);
    // Either the refresh drops it from the index or the read-time scan refuses it.
    assert!(!text.contains(&token), "{text}");
}
