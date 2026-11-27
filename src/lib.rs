//! ContextKit: a local-first context and persistent memory layer for AI coding assistants.
//!
//! The library is organized as a pipeline:
//!
//! * [`project`] locates the project and its `.contextkit/` data directory.
//! * [`index`] walks the repository and stores files, symbols, imports and searchable
//!   chunks in SQLite ([`db`]).
//! * [`memory`] stores decisions, conventions, tasks and discoveries, with an explicit
//!   trust model separating verified facts from agent-written claims.
//! * [`search`] and [`context`] retrieve relevant information within a token budget.
//! * [`mcp`] exposes all of this to AI assistants over the Model Context Protocol.

pub mod config;
pub mod context;
pub mod db;
pub mod error;
pub mod files;
pub mod index;
pub mod mcp;
pub mod memory;
pub mod overview;
pub mod project;
pub mod search;
pub mod secrets;
pub mod setup;
pub mod tokens;
pub mod util;
pub mod watch;

pub use error::{Error, Result};
pub use project::Project;
