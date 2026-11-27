//! Any user text must turn into a valid FTS5 query: searches may return nothing,
//! but must never fail with a syntax error.
#![no_main]

use contextkit::db;
use contextkit::memory::{self, Filter};
use contextkit::search;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(query) = std::str::from_utf8(data) else { return };
    let conn = db::open_in_memory().expect("db");
    search::search_code(&conn, query, None, 5).expect("code search must not fail");
    memory::search(&conn, query, &Filter::default()).expect("memory search must not fail");
    search::find_symbols(&conn, query, search::NameMatch::Contains, None, 5)
        .expect("symbol search must not fail");
});
