# Contributing to ContextKit

Thank you for helping. Before you start a substantial change, open an issue to discuss it. That saves everyone from work on a direction that won't be merged.

## Development setup

You need:

- The stable Rust toolchain. The minimum supported version is set by `rust-version` in `Cargo.toml`.
- A C compiler. SQLite and the tree-sitter grammars are compiled from C sources. On Windows, install the Visual Studio Build Tools with the "Desktop development with C++" workload.

Every pull request must pass these checks:

```sh
cargo build --all-targets
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

CI also runs [`cargo deny`](https://github.com/EmbarkStudios/cargo-deny) to check licenses and security advisories.

## Guidelines

- **Correctness first.** Recoverable failures return `Result`. Don't call `unwrap` or `expect` outside tests, and never silently discard an error: log it, count it, or return it.
- **No `unsafe`.** The crate forbids it.
- **Few dependencies.** Each new dependency needs a reason in the pull request: what it does, its license, how well it is maintained, and why the standard library or an existing dependency is not enough.
- **No async runtime** unless a change shows a measurable benefit.
- **Deterministic retrieval.** The same index and query must give the same result. Scoring weights live in `context::weights`. Explain any change to them.
- **Honest output.** Token counts are estimates and must say so. Never claim client compatibility that has not been tested.
- **Tests.** Bug fixes come with a regression test. New behavior comes with unit tests, and with an end-to-end test in `tests/` when it crosses modules.

## Adding a language

1. Add the grammar crate and a variant to `Lang` in `src/index/lang.rs`.
2. Write a query that follows the capture conventions documented at the top of that file.
3. If the language has classes or similar containers, add rules to `Lang::container`, and to `is_exported` in `src/index/extract.rs`.
4. Add import resolution in `src/index/resolve.rs` if it can be done lexically.
5. Add extraction tests. `every_query_compiles_against_its_grammar` catches node names the grammar does not have.

## Releasing

1. Update `version` in `Cargo.toml` and run `cargo build` so `Cargo.lock` follows.
2. Move the `[Unreleased]` entries in `CHANGELOG.md` under the new version and date.
3. Commit, then tag and push: `git tag v0.1.0 && git push origin v0.1.0`.
4. The Release workflow checks that the tag matches `Cargo.toml`, builds every platform, and creates a draft release. Review it on GitHub and publish it.

To test the build matrix without releasing, run the Release workflow manually from the Actions tab. It uploads the archives as workflow artifacts only.

## Schema changes

Append a new migration to `MIGRATIONS` in `src/db.rs`. Never edit a migration that has been released.
