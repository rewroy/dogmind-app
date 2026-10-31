//! Keeping a conversation inside the context, as Dogmind does, in three stages: old tool outputs are trimmed first, then
//! the model writes a checkpoint of the middle, and last the oldest turns go. A checkpoint request extends the
//! conversation it sums up (same tools, same effort), so that Straydog prefills only what follows what it holds.

use crate::message::Message;

const SUMMARY_HEADER: &str = "[SUMMARY OF EARLIER TURNS]\n## Main-model Session Checkpoint\n";
pub const TRIMMED_TOOL_CHARS: usize = 600;
/// Messages kept verbatim after the system prompt, and the turns a checkpoint needs before it is worth writing.
const HEAD: usize = 6;
const CHECKPOINT_TURNS: usize = 16;
/// The newest messages stage 0 leaves alone.
const RECENT: usize = 8;

/// The output a turn may still need, of the context.
pub fn reserve(window: u64) -> u64 {
    16384u64.min(window / 4).max(1024)
}

pub fn needed(used: u64, window: u64) -> bool {
    window > 0 && used + reserve(window) >= window
}

/// Where the middle ends: a turn's first message, so that no tool result loses its call.
fn turn_start(messages: &[Message], mut at: usize) -> usize {
    while at < messages.len() && messages[at].role == "tool" {
        at += 1;
    }
    at
}

/// Stage 0. Returns the characters saved.
pub fn trim_tool_outputs(messages: &mut [Message], want: usize) -> usize {
    let end = messages.len().saturating_sub(RECENT);
    let mut saved = 0;
    for message in messages.iter_mut().take(end).skip(1 + HEAD) {
        if saved >= want {
            break;
        }
        if message.role != "tool" {
            continue;
        }
        let Some(content) = &message.content else { continue };
        if content.len() <= TRIMMED_TOOL_CHARS + 64 {
            continue;
        }
        let mut cut = TRIMMED_TOOL_CHARS;
        while !content.is_char_boundary(cut) {
            cut -= 1;
        }
        let trimmed = format!("{}\n--- trimmed by compaction: {} bytes ---", &content[..cut], content.len() - cut);
        saved += content.len() - trimmed.len();
        message.content = Some(trimmed);
    }
    saved
}

/// What a compaction does to Claude's thinking: its blocks are bound to the history they were written after, and that has
/// just changed. Text and calls stay. A turn whose calls were just answered keeps its blocks, as the round it is in goes on.
pub fn strip_thinking(messages: &mut [Message]) {
    let round = messages.iter().rposition(|message| message.role != "tool").filter(|&at| at + 1 < messages.len() && messages[at].tool_calls.is_some());
    for (at, message) in messages.iter_mut().enumerate() {
        if Some(at) == round {
            continue;
        }
        let Some(serde_json::Value::Array(blocks)) = &mut message.provider_blocks else { continue };
        blocks.retain(|block| !matches!(block.get("type").and_then(serde_json::Value::as_str), Some("thinking" | "redacted_thinking")));
        if blocks.is_empty() {
            message.provider_blocks = None;
        }
    }
}

/// Whether anything lies between the head and the newest `keep` messages, for a checkpoint to sum up.
pub fn has_a_middle(messages: &[Message], keep: usize) -> bool {
    let head_end = turn_start(messages, (1 + HEAD).min(messages.len()));
    turn_start(messages, messages.len().saturating_sub(keep).max(head_end)) > head_end
}

pub fn worth_a_checkpoint(messages: &[Message]) -> bool {
    messages.iter().filter(|message| matches!(message.role.as_str(), "user" | "assistant")).count() >= CHECKPOINT_TURNS
}

/// `[system, *head, summary, *recent]`, keeping the newest `keep` messages.
pub fn apply_checkpoint(messages: &[Message], checkpoint: &str, keep: usize) -> Vec<Message> {
    let head_end = turn_start(messages, (1 + HEAD).min(messages.len()));
    let recent_start = turn_start(messages, messages.len().saturating_sub(keep).max(head_end));
    let mut out = messages[..head_end].to_vec();
    out.push(Message::text("assistant", format!("{SUMMARY_HEADER}{}", checkpoint.trim())));
    out.extend_from_slice(&messages[recent_start..]);
    out
}

/// Stage 2: the oldest whole turns after the head go until the rest fits `target_chars`.
pub fn truncate(messages: &[Message], target_chars: usize) -> Vec<Message> {
    let head_end = turn_start(messages, (1 + HEAD).min(messages.len()));
    let total: usize = messages.iter().map(Message::chars).sum();
    let mut cut = head_end;
    let mut dropped = 0;
    while total - dropped > target_chars && cut < messages.len().saturating_sub(1) {
        dropped += messages[cut].chars();
        cut = turn_start(messages, cut + 1);
    }
    let mut out = messages[..head_end].to_vec();
    if cut > head_end {
        out.push(Message::text("assistant", format!("{SUMMARY_HEADER}{} earlier messages were dropped to fit the context.", cut - head_end)));
    }
    out.extend_from_slice(&messages[cut..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{FunctionCall, ToolCall};

    fn conversation(turns: usize) -> Vec<Message> {
        let mut messages = vec![Message::text("system", "prompt")];
        for turn in 0..turns {
            messages.push(Message::text("user", format!("question {turn}")));
            messages.push(Message {
                role: "assistant".into(),
                tool_calls: Some(vec![ToolCall { id: format!("c{turn}"), kind: "function".into(), function: FunctionCall { name: "read_file".into(), arguments: "{}".into() } }]),
                ..Message::default()
            });
            messages.push(Message::tool_result(&format!("c{turn}"), "x".repeat(5000)));
            messages.push(Message::text("assistant", format!("answer {turn}")));
        }
        messages
    }

    fn no_orphans(messages: &[Message]) {
        for (at, message) in messages.iter().enumerate() {
            if message.role == "tool" {
                assert!(messages[..at].iter().rev().find(|m| m.role != "tool").is_some_and(|m| m.tool_calls.is_some()), "orphan tool result at {at}");
            }
        }
    }

    #[test]
    fn a_compaction_takes_the_thinking_but_not_from_the_round_in_progress() {
        let thought = || serde_json::json!([{"type": "thinking", "thinking": "t", "signature": "s"}, {"type": "text", "text": "a"}]);
        let only = || serde_json::json!([{"type": "thinking", "thinking": "t", "signature": "s"}]);
        let mut messages = vec![
            Message::text("user", "q"),
            Message { role: "assistant".into(), provider_blocks: Some(thought()), ..Message::default() },
            Message { role: "assistant".into(), provider_blocks: Some(only()), ..Message::default() },
            Message { role: "assistant".into(), tool_calls: Some(vec![]), provider_blocks: Some(thought()), ..Message::default() },
            Message::tool_result("x", "r"),
        ];
        strip_thinking(&mut messages);
        assert_eq!(messages[1].provider_blocks.as_ref().unwrap().as_array().unwrap().len(), 1);
        assert!(messages[2].provider_blocks.is_none());
        assert_eq!(messages[3].provider_blocks.as_ref().unwrap().as_array().unwrap().len(), 2);
    }

    #[test]
    fn the_trigger_leaves_room_for_an_answer() {
        assert_eq!(reserve(262144), 16384);
        assert_eq!(reserve(8192), 2048);
        assert_eq!(reserve(1024), 1024);
        assert!(!needed(240_000, 262_144));
        assert!(needed(250_000, 262_144));
        assert!(!needed(1, 0));
    }

    #[test]
    fn stage_0_trims_only_old_tool_outputs() {
        let mut messages = conversation(10);
        let last_tool = messages.iter().rposition(|m| m.role == "tool").unwrap();
        let saved = trim_tool_outputs(&mut messages, usize::MAX);
        assert!(saved > 0);
        assert_eq!(messages[last_tool].content.as_ref().unwrap().len(), 5000);
        assert!(messages[3].content.as_ref().unwrap().len() == 5000, "the head stays");
        assert!(messages.iter().filter(|m| m.role == "tool").any(|m| m.content.as_ref().unwrap().contains("trimmed")));
    }

    #[test]
    fn a_checkpoint_keeps_head_and_recent_and_no_orphans() {
        let messages = conversation(10);
        assert!(worth_a_checkpoint(&messages));
        for keep in 1..12 {
            let out = apply_checkpoint(&messages, "the state", keep);
            no_orphans(&out);
            assert_eq!(out[0].role, "system");
            assert!(out.iter().any(|m| m.content.as_deref().is_some_and(|c| c.contains("the state"))));
            assert_eq!(out.last(), messages.last());
            assert!(out.len() < messages.len());
        }
    }

    #[test]
    fn truncation_fits_and_keeps_the_conversation_valid() {
        let messages = conversation(10);
        let out = truncate(&messages, 12000);
        no_orphans(&out);
        assert!(out.iter().map(Message::chars).sum::<usize>() <= 12000 + 200);
        assert_eq!(out.last(), messages.last());
    }
}
