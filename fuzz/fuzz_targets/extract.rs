//! Arbitrary source text through every grammar and every manifest parser.
#![no_main]

use contextkit::index::extract::Extractor;
use contextkit::index::lang::FileKind;
use contextkit::index::manifest;
use libfuzzer_sys::fuzz_target;

const PATHS: &[&str] = &[
    "a.rs", "a.js", "a.ts", "a.tsx", "a.py", "a.lua", "a.luau", "README.md",
    "Cargo.toml", "package.json", "pyproject.toml", "requirements.txt", "wally.toml",
];

fuzz_target!(|data: &[u8]| {
    let Some((&selector, rest)) = data.split_first() else { return };
    let path = PATHS[usize::from(selector) % PATHS.len()];
    let text = String::from_utf8_lossy(rest);
    let mut extractor = Extractor::new();
    if let Ok(analysis) = extractor.analyze(path, FileKind::detect(path), &text) {
        for s in &analysis.symbols {
            assert!(s.start_line <= s.end_line, "{s:?}");
        }
        for c in &analysis.chunks {
            assert!(c.start_line <= c.end_line);
        }
    }
    let _ = manifest::parse(path, &text);
});
