//! A conversation's messages, as the OpenAI API and the session files spell them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Sent back on every later request: a client that returns the reasoning keeps Straydog's conversation cache warm.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The provider's own form of this message, sent back to it unchanged: Claude's thinking blocks. Only the session
    /// file and that provider read it; every other request goes without.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_blocks: Option<serde_json::Value>,
    /// Pictures that go with this message, named and not held: the bytes are files in the session's folder.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageRef>,
    /// When it was written, `YYYY-MM-DD HH:MM` on this PC's clock, for the person to see beside it. Kept in the session; no request carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// What woke the session, when this message is the harness's notice of it and not anything the person wrote (see [`Wake`]): the window shows it as a notice row. The message's text is what the model reads of it.
    /// A reader that does not know the field leaves it out, and reads the message as one the app wrote and the person did not (its text begins as such messages do, [`crate::prompt::DRAFT_NOTE`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<Vec<Wake>>,
    /// On the result of an `explore` call: where the helper's own conversation is kept, and how its work went (see [`HelperRef`]). Kept in the session; no request carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper: Option<HelperRef>,
}

/// An `explore` helper's work as the call's result records it: the helper's conversation is a session of its own beside the one that started it
/// ([`crate::session::helper_file_of`], by `number`), and the rest is what a window says of it without opening that file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperRef {
    /// Which of the session's helper files it is, from 1.
    pub number: u32,
    /// The requests in which it called a tool.
    #[serde(default)]
    pub steps: u32,
    /// How many different files it read.
    #[serde(default)]
    pub files: u32,
    /// How long it worked, in milliseconds.
    #[serde(default)]
    pub ms: u64,
    /// How much of its context it had come to: what its last request read and wrote.
    #[serde(default)]
    pub tokens: u64,
    /// The person stopped it (or the turn) before it answered.
    #[serde(default)]
    pub stopped: bool,
}

/// What woke a session by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeCause {
    /// A command started in the background ended, any way it did.
    CommandEnded,
    /// A command started in the background printed the line its `wake_on` waits for.
    CommandLine,
    /// A wait the model set (`wake_at`) is up.
    Wait,
}

impl WakeCause {
    pub fn name(self) -> &'static str {
        match self {
            WakeCause::CommandEnded => "command_ended",
            WakeCause::CommandLine => "command_line",
            WakeCause::Wait => "wait",
        }
    }
}

/// One thing that woke a session, as the session keeps it and the window shows it: no more than that, and never what a command printed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wake {
    pub cause: WakeCause,
    /// `job1`, `wait2`.
    pub id: String,
    /// The command's first line, or the wait's note.
    pub what: String,
    /// How a command that ended did: its exit code, or none (stopped, timed out, not run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// Why there is no exit code: `timed out`, `cancelled`, `not run`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
    /// How long the command had run, or how long the wait was.
    #[serde(default)]
    pub seconds: u64,
}

/// A picture in the session's folder ([`crate::images::file_of`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    pub file: String,
    pub media_type: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

impl ImageRef {
    /// About what it takes of the context.
    pub fn tokens(&self) -> usize {
        self.width as usize * self.height as usize / (32 * 32) + 8
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

fn function_kind() -> String {
    "function".into()
}

impl Message {
    pub fn text(role: &str, content: impl Into<String>) -> Message {
        Message { role: role.into(), content: Some(content.into()), ..Message::default() }
    }

    pub fn tool_result(id: &str, content: impl Into<String>) -> Message {
        Message { role: "tool".into(), content: Some(content.into()), tool_call_id: Some(id.into()), ..Message::default() }
    }

    /// The characters a message puts in the prompt, as an estimate of its tokens where the server has not counted them.
    pub fn chars(&self) -> usize {
        let calls = self.tool_calls.iter().flatten().map(|call| call.function.name.len() + call.function.arguments.len()).sum::<usize>();
        // (a picture by what it takes of the context, at the three characters a token this estimate counts: not by its bytes, which are not here)
        let pictures = self.images.iter().map(ImageRef::tokens).sum::<usize>() * 3;
        // (Claude's thinking goes back with every request, in its blocks: its reasoning_content is empty)
        let thinking = self.provider_blocks.as_ref().and_then(serde_json::Value::as_array).into_iter().flatten()
            .filter_map(|block| block.get("thinking").or_else(|| block.get("data")).and_then(serde_json::Value::as_str))
            .map(str::len)
            .sum::<usize>();
        self.content.as_deref().map_or(0, str::len) + self.reasoning_content.as_deref().map_or(0, str::len) + calls + pictures + thinking
    }

    /// As an OpenAI-compatible server or Codex reads it: without Claude's own form, nor when it was written.
    pub fn without_provider_blocks(&self) -> Message {
        Message { provider_blocks: None, at: None, notice: None, helper: None, ..self.clone() }
    }
}

/// This PC's clock as a message keeps it: `YYYY-MM-DD HH:MM`.
pub fn written_now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()
}

/// When a message was written, as it stands beside it: `20:12` on the day `today` (`YYYY-MM-DD`), and `11/2 20:12` before it. `at` is
/// `YYYY-MM-DD HH:MM`, with seconds or a zone after it or not; anything else says nothing.
pub fn clock_words(at: &str, today: &str) -> String {
    let Some((day, rest)) = at.trim().split_once(' ') else { return String::new() };
    let time: String = rest.chars().take(5).collect();
    let parts: Vec<&str> = day.split('-').collect();
    if parts.len() != 3 || time.len() != 5 || !time.starts_with(|c: char| c.is_ascii_digit()) {
        return String::new();
    }
    if today.starts_with(day) {
        time
    } else {
        format!("{}/{} {time}", parts[1].trim_start_matches('0'), parts[2].trim_start_matches('0'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_reads_as_the_time_today_and_with_its_day_before() {
        assert_eq!(clock_words("2026-11-03 20:12", "2026-11-03"), "20:12");
        assert_eq!(clock_words("2026-11-02 20:01:18 +09:00", "2026-11-03 09:00"), "11/2 20:01");
        assert_eq!(clock_words("", "2026-11-03"), "");
        assert_eq!(clock_words("yesterday evening", "2026-11-03"), "");
    }

    /// Claude's thinking, which goes back in its blocks, counts toward what a message takes of the context; its text block is the content already.
    #[test]
    fn claudes_thinking_counts_toward_the_context() {
        let plain = Message::text("assistant", "answer");
        let thought = Message { provider_blocks: Some(serde_json::json!([{"type": "thinking", "thinking": "x".repeat(900), "signature": "s"}, {"type": "text", "text": "answer"}])), ..plain.clone() };
        assert_eq!(thought.chars(), plain.chars() + 900);
    }

    /// The time is the session's and the person's: a request goes without it, and so its prefix is the same whenever it was said.
    #[test]
    fn when_a_message_was_written_is_kept_and_not_sent() {
        let mut said = Message::text("user", "hi");
        said.at = Some("2026-11-03 20:12".into());
        let kept: Message = serde_json::from_str(&serde_json::to_string(&said).unwrap()).unwrap();
        assert_eq!(kept.at.as_deref(), Some("2026-11-03 20:12"));
        let body = crate::llm::request_body("m", &[said], None, None);
        assert!(body["messages"][0].get("at").is_none(), "{body}");
        assert_eq!(body["messages"][0]["content"], "hi");
    }
}
