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
        self.content.as_deref().map_or(0, str::len) + self.reasoning_content.as_deref().map_or(0, str::len) + calls
    }

    /// As an OpenAI-compatible server or Codex reads it.
    pub fn without_provider_blocks(&self) -> Message {
        Message { provider_blocks: None, ..self.clone() }
    }
}
