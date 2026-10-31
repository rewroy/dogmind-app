//! Dogmind's agent, in Rust: the loop, its tools and approvals, and the conversation it keeps.
//!
//! It knows the engine only as an HTTP server (`http`, `llm`), so a change inside Straydog does not reach it. What
//! the model reads (the system prompt, the tool schemas) is generated from Dogmind into `assets/dogmind`.

pub mod agent;
pub mod anthropic;
pub mod checkpoint;
pub mod codex;
pub mod compaction;
pub mod http;
pub mod llm;
pub mod message;
pub mod prompt;
pub mod rules;
pub mod safety;
#[cfg(windows)]
pub mod sandbox;
pub mod session;
pub mod tools;
pub mod web;
