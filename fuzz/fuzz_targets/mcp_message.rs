//! Arbitrary bytes as one MCP message: the server must answer or ignore, never panic.
#![no_main]

use std::sync::{Mutex, OnceLock};

use contextkit::Project;
use contextkit::mcp::Server;
use libfuzzer_sys::fuzz_target;

struct State {
    _dir: tempfile::TempDir,
    server: Server,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("lib.rs"), "pub fn demo() {}\n").expect("write");
        let (project, _) = Project::init(dir.path()).expect("init");
        let server = Server::new(project).expect("server");
        Mutex::new(State { _dir: dir, server })
    })
}

fuzz_target!(|data: &[u8]| {
    let Ok(line) = std::str::from_utf8(data) else { return };
    let mut guard = state().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(response) = guard.server.handle_line(line) {
        assert_eq!(response["jsonrpc"], "2.0");
    }
});
