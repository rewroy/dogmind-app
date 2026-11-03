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
/// What is kept of a request whose turn went with the oldest messages.
const MOST_OF_A_REQUEST: usize = 4000;

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

/// Old pictures, oldest first, past the head unless `head` too, until `want` characters' worth have gone: a word stays in each one's place, and its file. Returns those characters.
pub fn trim_images(messages: &mut [Message], want: usize, head: bool) -> usize {
    let end = messages.len().saturating_sub(RECENT);
    let mut saved = 0;
    for message in messages.iter_mut().take(end).skip(if head { 1 } else { 1 + HEAD }) {
        if saved >= want {
            break;
        }
        saved += picture_notes(message, "an earlier turn was compacted");
    }
    saved
}

/// Every picture in the conversation becomes a word. Returns how many messages changed.
pub fn images_to_notes(messages: &mut [Message], why: &str) -> usize {
    // (by what was there: the note on a very small picture is longer than the picture counted)
    messages.iter_mut().filter(|message| !message.images.is_empty()).map(|message| {
        picture_notes(message, why);
    }).count()
}

fn picture_notes(message: &mut Message, why: &str) -> usize {
    let before = message.chars();
    let pictures = std::mem::take(&mut message.images);
    if pictures.is_empty() {
        return 0;
    }
    let mut text = message.content.take().unwrap_or_default();
    for picture in &pictures {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&crate::images::note_for(&picture.file, why));
    }
    message.content = Some(text);
    before.saturating_sub(message.chars())
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
    let mut summary = format!("{SUMMARY_HEADER}{}", checkpoint.trim());
    // A model's sum of it may not have the words: what is being worked on must be there in the person's own.
    summary.push_str(&latest_request(&messages[head_end..recent_start], &messages[recent_start..]));
    out.push(Message::text("assistant", summary));
    out.extend(messages[recent_start..].iter().cloned().map(|message| Message { reasoning_content: None, ..message }));
    out
}

/// What the person last asked for, to say in a summary, if it is among what goes and not among what stays.
fn latest_request(going: &[Message], staying: &[Message]) -> String {
    let typed = |message: &&Message| message.role == "user" && !message.content.as_deref().is_some_and(crate::prompt::is_nudge);
    if staying.iter().any(|message| typed(&message)) {
        return String::new();
    }
    going.iter().rev().find(typed).and_then(|message| message.content.as_deref()).map_or(String::new(), |asked| {
        let asked: String = crate::prompt::typed_text(asked).chars().take(MOST_OF_A_REQUEST).collect();
        format!("\nThe user's latest request was among them:\n{asked}")
    })
}

/// Stage 2: the oldest whole turns after the head go until the rest fits `target_chars`.
pub fn truncate(messages: &[Message], target_chars: usize) -> Vec<Message> {
    let head_end = turn_start(messages, (1 + HEAD).min(messages.len()));
    let total: usize = messages.iter().map(Message::chars).sum();
    let mut cut = head_end;
    let mut dropped = 0;
    while total - dropped > target_chars && cut < messages.len().saturating_sub(1) {
        let next = turn_start(messages, cut + 1);
        // (the newest round stays, whatever it weighs: without it the model would not know what it has just done)
        if next >= messages.len() {
            break;
        }
        // (a call's results go with it, and count)
        dropped += messages[cut..next].iter().map(Message::chars).sum::<usize>();
        cut = next;
    }
    let mut out = messages[..head_end].to_vec();
    if cut > head_end {
        // No checkpoint says what the work is for, so the request it answers does not go with the rest.
        let summary = format!("{SUMMARY_HEADER}{} earlier messages were dropped to fit the context.{}", cut - head_end, latest_request(&messages[head_end..cut], &messages[cut..]));
        out.push(Message::text("assistant", summary));
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
    fn a_checkpoint_says_the_request_in_the_persons_words_and_drops_the_thinking_after_the_head() {
        let mut messages = conversation(4);
        messages.push(Message::text("user", crate::prompt::user_message_in("the real request", "now", None)));
        for turn in 0..10 {
            messages.push(Message { role: "assistant".into(), reasoning_content: Some("x".repeat(5000)), tool_calls: Some(vec![ToolCall { id: format!("n{turn}"), kind: "function".into(), function: FunctionCall { name: "read_file".into(), arguments: "{}".into() } }]), ..Message::default() });
            messages.push(Message::tool_result(&format!("n{turn}"), "r"));
        }
        let out = apply_checkpoint(&messages, "the state", 4);
        no_orphans(&out);
        let said = out.iter().find(|message| message.content.as_deref().is_some_and(|text| text.starts_with(SUMMARY_HEADER))).unwrap();
        assert!(said.content.as_deref().unwrap().ends_with("The user's latest request was among them:\nthe real request"), "{said:?}");
        let after = out.iter().position(|message| message == said).unwrap();
        assert!(out[after..].iter().all(|message| message.reasoning_content.is_none()));
        // With the request among what stays, it is not said twice.
        let staying = apply_checkpoint(&conversation(10), "the state", 12);
        assert!(!staying.iter().any(|message| message.content.as_deref().is_some_and(|text| text.contains("latest request"))));
    }

    #[test]
    fn truncation_fits_and_keeps_the_conversation_valid() {
        let messages = conversation(10);
        let out = truncate(&messages, 12000);
        no_orphans(&out);
        assert!(out.iter().map(Message::chars).sum::<usize>() <= 12000 + 200);
        assert_eq!(out.last(), messages.last());
    }

    fn pictured(text: &str) -> Message {
        Message { images: vec![crate::message::ImageRef { file: "a.png".into(), media_type: "image/png".into(), width: 3200, height: 3200 }], ..Message::text("user", text) }
    }

    #[test]
    fn a_picture_counts_by_its_pixels_and_not_by_any_bytes() {
        assert_eq!(pictured("").chars(), (10_000 + 8) * 3);
    }

    #[test]
    fn old_pictures_become_words_and_the_newest_and_the_head_stay() {
        let mut messages = vec![Message::text("system", "s"), pictured("in the head")];
        messages.extend((0..8).map(|at| Message::text("user", format!("m{at}"))));
        messages.push(pictured("old"));
        messages.extend((0..7).map(|at| Message::text("user", format!("n{at}"))));
        messages.push(pictured("new"));
        let last = messages.len() - 1;
        let old = last - 8;
        let saved = trim_images(&mut messages, usize::MAX, false);
        assert!(saved > 29_000, "{saved}");
        assert!(messages[old].images.is_empty());
        let said = messages[old].content.as_deref().unwrap();
        assert!(said.starts_with("old\n") && said.contains("a.png") && said.contains("not shown"), "{said}");
        assert_eq!((messages[1].images.len(), messages[last].images.len()), (1, 1));
        assert!(trim_images(&mut messages, usize::MAX, true) > 29_000);
        assert!(messages[1].images.is_empty(), "when the server has refused, the head too");
        assert_eq!(messages[last].images.len(), 1);
    }

    #[test]
    fn even_a_very_small_picture_counts_as_a_message_changed() {
        let tiny = Message { images: vec![crate::message::ImageRef { file: "a.png".into(), media_type: "image/png".into(), width: 8, height: 8 }], ..Message::text("user", "x") };
        let mut messages = vec![tiny, Message::text("user", "y")];
        assert_eq!(images_to_notes(&mut messages, "no reason"), 1);
        assert!(messages[0].images.is_empty() && messages[0].content.as_deref().unwrap().contains("no reason"));
        assert_eq!(images_to_notes(&mut messages, "no reason"), 0);
    }

    #[test]
    fn no_more_goes_than_was_asked_for() {
        let mut messages = vec![Message::text("system", "s")];
        messages.extend((0..10).map(|at| pictured(&format!("m{at}"))));
        messages.extend((0..8).map(|at| Message::text("user", format!("n{at}"))));
        trim_images(&mut messages, 1000, false);
        assert_eq!(messages.iter().filter(|message| message.images.is_empty() && message.role == "user" && message.content.as_deref().is_some_and(|text| text.contains("not shown"))).count(), 1);
    }

    #[test]
    fn truncation_counts_the_results_that_go_with_a_call_and_keeps_what_it_can() {
        let messages = conversation(10);
        let out = truncate(&messages, 30_000);
        let left: usize = out.iter().map(Message::chars).sum();
        assert!((20_000..=30_200).contains(&left), "only as much goes as has to: {left}");
    }

    #[test]
    fn truncation_keeps_the_request_being_worked_on() {
        let mut messages = conversation(4);
        messages.push(Message::text("user", crate::prompt::user_message_in("the real request", "now", None)));
        for turn in 0..6 {
            messages.push(Message { role: "assistant".into(), tool_calls: Some(vec![ToolCall { id: format!("n{turn}"), kind: "function".into(), function: FunctionCall { name: "read_file".into(), arguments: "{}".into() } }]), ..Message::default() });
            messages.push(Message::tool_result(&format!("n{turn}"), "x".repeat(5000)));
        }
        let out = truncate(&messages, 6000);
        no_orphans(&out);
        let said = out.iter().find(|message| message.content.as_deref().is_some_and(|text| text.starts_with(SUMMARY_HEADER))).unwrap();
        assert!(said.content.as_deref().unwrap().ends_with("The user's latest request was among them:\nthe real request"), "{said:?}");
        assert_eq!(out.last(), messages.last());
    }
}
