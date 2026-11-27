//! Path validation must never accept anything that escapes the project root.
#![no_main]

use std::collections::HashSet;

use contextkit::index::lang::Lang;
use contextkit::index::resolve;
use contextkit::util::validate_rel_path;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    if let Ok(path) = validate_rel_path(text) {
        assert!(!path.is_empty());
        assert!(!path.starts_with('/'));
        assert!(path.split('/').all(|part| !part.is_empty() && part != ".." && part != "."));
        assert!(!path.contains('\\'));
    }
    let files: HashSet<String> = ["src/lib.rs", "src/a.ts", "pkg/__init__.py", "lib/x.lua"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    for lang in Lang::ALL {
        if let Some(found) = resolve::resolve(lang, "src/lib.rs", text, false, &files) {
            assert!(files.contains(&found));
        }
    }
});
