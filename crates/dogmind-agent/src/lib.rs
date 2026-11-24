//! Dogmind's agent, in Rust: the loop, its tools and approvals, and the conversation it keeps.
//!
//! It knows the engine only as an HTTP server (`http`, `llm`), so a change inside Straydog does not reach it. What
//! the model reads (the system prompt in `prompt`, the tool schemas in `tools`) is this app's own, as small as a local model needs it. What Dogmind itself says is
//! generated into `assets/dogmind`, to read when it changes; only its checkpoint prompt and its Codex policy are taken from there as they are.

/// Fill numbered slots in interface messages. Values are literal text, including any braces they contain.
pub fn fill_template(template: &str, values: &[impl std::fmt::Display]) -> String {
    use std::fmt::Write;
    let mut parts = template.split('{');
    let mut text = String::from(parts.next().unwrap_or_default());
    for part in parts {
        if let Some((digits, rest)) = part.split_once('}') {
            if digits == "0" || digits.as_bytes().first().is_some_and(|c| matches!(c, b'1'..=b'9')) {
                if let Some(value) = digits.parse::<usize>().ok().and_then(|index| values.get(index)) {
                    write!(text, "{value}").expect("writing to a String");
                    text.push_str(rest);
                    continue;
                }
            }
        }
        text.push('{');
        text.push_str(part);
    }
    text
}

pub mod agent;
pub mod anthropic;
pub mod background;
pub mod baseline;
#[cfg(windows)]
pub mod bundled;
pub mod checkpoint;
pub mod code_mode;
pub mod codex;
pub mod compaction;
pub mod conversation_key;
pub mod conversion;
pub mod http;
#[cfg(test)]
mod test_net;
#[cfg(test)]
mod in_container;
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
pub mod script_syntax;
#[cfg(windows)]
pub mod sandbox;
pub mod session;
pub mod skills;
pub mod timeline;
pub mod temp_writes;
pub mod tools;
pub mod trajectory;
pub mod trajectory_report;
pub mod web;
#[cfg(windows)]
pub mod workspace_grants;
