//! Dogmind's agent, in Rust: the loop, its tools and approvals, and the conversation it keeps.
//!
//! It knows the engine only as an HTTP server (`http`, `llm`), so a change inside Straydog does not reach it. What
//! the model reads (the system prompt in `prompt`, the tool schemas in `tools`) is this app's own, as small as a local model needs it. What Dogmind itself says is
//! generated into `assets/dogmind`, to read when it changes; only its checkpoint prompt and its Codex policy are taken from there as they are.

pub mod agent;
pub mod anthropic;
pub mod baseline;
#[cfg(windows)]
pub mod bundled;
pub mod checkpoint;
pub mod codex;
pub mod compaction;
pub mod http;
pub mod images;
pub mod llm;
pub mod mcp;
pub mod memory;
pub mod message;
#[cfg(windows)]
pub mod msvc;
#[cfg(windows)]
pub mod page_check;
pub mod prompt;
#[cfg(windows)]
pub mod read_folders;
pub mod rules;
pub mod safety;
#[cfg(windows)]
pub mod sandbox;
pub mod session;
pub mod skills;
pub mod tools;
pub mod web;
#[cfg(windows)]
pub mod workspace_grants;
