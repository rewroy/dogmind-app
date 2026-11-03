//! Claude through the Messages API, by raw HTTP as Rust has no official SDK. Thinking blocks go back unchanged and the
//! conversation is only appended to, as the API checks: `compaction::strip_thinking` is what a compaction does to them.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::http::{self, Url};
use crate::images::{self, ImageLimit};
use crate::llm::{self, Completion, Delta, LlmError, Provider, Request, Usage};
use crate::message::{FunctionCall, Message, ToolCall};

pub const DEFAULT_URL: &str = "https://api.anthropic.com/v1";
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
pub const DEFAULT_CONTEXT: u64 = 1_000_000;
const API_VERSION: &str = "2026-10-21";
/// A declined request is tried again by the server on a model it picks.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MAX_TOKENS: u64 = 64000;

pub struct Anthropic {
    base: Url,
    key: String,
}

fn assistant_blocks(message: &Message) -> Value {
    if let Some(blocks) = &message.provider_blocks {
        return blocks.clone();
    }
    // From another provider, or a summary: its text and calls, without thinking that was not written for this API.
    let mut blocks = Vec::new();
    if let Some(text) = message.content.as_deref().filter(|text| !text.is_empty()) {
        blocks.push(json!({"type": "text", "text": text}));
    }
    for call in message.tool_calls.iter().flatten() {
        // (the API takes an object here and nothing else)
        let input: Value = serde_json::from_str(&call.function.arguments).ok().filter(Value::is_object).unwrap_or_else(|| json!({}));
        blocks.push(json!({"type": "tool_use", "id": call.id, "name": call.function.name, "input": input}));
    }
    if blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": "(no answer)"}));
    }
    Value::Array(blocks)
}

/// A message's words and pictures as the API spells them: plain words when there is no picture.
fn user_content(message: &Message, folder: Option<&Path>) -> Value {
    let (pictures, text) = images::loaded(message, folder);
    if pictures.is_empty() {
        return Value::String(text);
    }
    let mut blocks: Vec<Value> = pictures.iter().map(|picture| json!({"type": "image", "source": {"type": "base64", "media_type": picture.media_type, "data": picture.data}})).collect();
    if !text.is_empty() {
        blocks.push(json!({"type": "text", "text": text}));
    }
    Value::Array(blocks)
}

/// The system prompt and the messages, with no folder for pictures.
pub fn convert(messages: &[Message]) -> (String, Vec<Value>) {
    convert_in(messages, None)
}

/// The system prompt and the messages as the API spells them: a response's tool results go in one user message.
pub fn convert_in(messages: &[Message], folder: Option<&Path>) -> (String, Vec<Value>) {
    let mut system = String::new();
    let mut out: Vec<Value> = Vec::new();
    let mut results: Vec<Value> = Vec::new();
    for message in messages {
        if message.role == "tool" {
            let text = message.content.clone().unwrap_or_default();
            let mut block = json!({"type": "tool_result", "tool_use_id": message.tool_call_id, "content": user_content(message, folder)});
            if text.starts_with("error: ") {
                block["is_error"] = json!(true);
            }
            results.push(block);
            continue;
        }
        if !results.is_empty() {
            out.push(json!({"role": "user", "content": std::mem::take(&mut results)}));
        }
        match message.role.as_str() {
            "system" => system.push_str(message.content.as_deref().unwrap_or("")),
            "assistant" => out.push(json!({"role": "assistant", "content": assistant_blocks(message)})),
            _ => out.push(json!({"role": "user", "content": user_content(message, folder)})),
        }
    }
    if !results.is_empty() {
        out.push(json!({"role": "user", "content": results}));
    }
    (system, out)
}

/// `none` and `minimal` are not levels here: thinking cannot be turned off on the current models, only lowered.
pub fn effort_level(effort: Option<&str>) -> &'static str {
    match effort {
        Some("none" | "minimal" | "low") => "low",
        Some("medium") => "medium",
        Some("xhigh") => "xhigh",
        Some("max") => "max",
        _ => "high",
    }
}

pub fn tools_of(schemas: &Value) -> Vec<Value> {
    schemas.as_array().into_iter().flatten().filter_map(|tool| {
        let function = tool.get("function")?;
        Some(json!({
            "name": function.get("name")?,
            "description": function.get("description")?,
            "input_schema": function.get("parameters")?,
            "eager_input_streaming": true,
        }))
    }).collect()
}

pub fn request_body(request: &Request) -> Value {
    let (system, messages) = convert_in(request.messages, request.images);
    let mut body = json!({
        "model": request.model,
        "max_tokens": MAX_TOKENS,
        "stream": true,
        "messages": messages,
        "thinking": {"type": "adaptive", "display": "summarized"},
        "output_config": {"effort": effort_level(request.effort)},
        "cache_control": {"type": "ephemeral"},
        "fallbacks": "default",
    });
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if let Some(tools) = request.tools {
        body["tools"] = Value::Array(tools_of(tools));
    }
    body
}

fn classify(status: u16, body: &str) -> LlmError {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    classify_error(status, parsed.pointer("/error/type").and_then(Value::as_str).unwrap_or(""), parsed.pointer("/error/message").and_then(Value::as_str).unwrap_or(body))
}

fn classify_error(status: u16, kind: &str, message: &str) -> LlmError {
    if message.contains("prompt is too long") {
        return LlmError::Overflow(llm::CONTEXT_FULL.to_string());
    }
    if llm::refuses_images(&message.to_lowercase()) {
        return LlmError::NoVision(llm::NO_VISION.to_string());
    }
    if matches!(kind, "rate_limit_error" | "overloaded_error") || status == 429 {
        return LlmError::Transient(llm::SERVER_BUSY.to_string());
    }
    let text = format!("Claude: {message}");
    if matches!(kind, "api_error" | "timeout_error") || matches!(status, 408 | 409) || status >= 500 {
        return LlmError::Transient(text);
    }
    if matches!(kind, "authentication_error" | "permission_error") || matches!(status, 401 | 403) {
        return LlmError::Rejected(llm::KEY_REFUSED.to_string());
    }
    LlmError::Rejected(text)
}

/// A stream's content blocks by index, assembled, and what to send back.
#[derive(Default)]
struct Assembly {
    blocks: Vec<Map<String, Value>>,
    partial: Vec<String>,
    stop: String,
    prompt: u64,
    cached: u64,
    completion: u64,
    refused: Option<String>,
}

impl Assembly {
    fn event(&mut self, data: &str, on: &mut dyn FnMut(Delta)) -> Result<(), LlmError> {
        let event: Value = serde_json::from_str(data).map_err(|_| LlmError::Transient(llm::UNREADABLE.to_string()))?;
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                if let Some(usage) = event.pointer("/message/usage") {
                    let count = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
                    self.prompt = count("input_tokens") + count("cache_read_input_tokens") + count("cache_creation_input_tokens");
                    self.cached = count("cache_read_input_tokens");
                }
            }
            "content_block_start" => {
                let block = event.get("content_block").and_then(Value::as_object).cloned().unwrap_or_default();
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    if let Some(name) = block.get("name").and_then(Value::as_str) {
                        on(Delta::Tool(event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize, name));
                    }
                }
                let at = event.get("index").and_then(Value::as_u64).unwrap_or(self.blocks.len() as u64) as usize;
                while self.blocks.len() <= at {
                    self.blocks.push(Map::new());
                    self.partial.push(String::new());
                }
                self.blocks[at] = block;
            }
            "content_block_delta" => {
                let at = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let (Some(block), Some(delta)) = (self.blocks.get_mut(at), event.get("delta")) else { return Ok(()) };
                let text = |name: &str| delta.get(name).and_then(Value::as_str).unwrap_or("");
                let grow = |block: &mut Map<String, Value>, field: &str, more: &str| {
                    let known = block.get(field).and_then(Value::as_str).unwrap_or("").to_string();
                    block.insert(field.to_string(), Value::String(known + more));
                };
                match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text_delta" => {
                        grow(block, "text", text("text"));
                        on(Delta::Content(text("text")));
                    }
                    "thinking_delta" => {
                        grow(block, "thinking", text("thinking"));
                        on(Delta::Reasoning(text("thinking")));
                    }
                    "signature_delta" => grow(block, "signature", text("signature")),
                    "input_json_delta" => {
                        self.partial[at].push_str(text("partial_json"));
                        on(Delta::Arguments(at, text("partial_json")));
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop = reason.to_string();
                }
                if reason_is_refusal(&self.stop) {
                    let category = event.pointer("/delta/stop_details/category").and_then(Value::as_str).unwrap_or("unspecified");
                    self.refused = Some(format!("Claude declined this request ({category})"));
                }
                if let Some(count) = event.pointer("/usage/output_tokens").and_then(Value::as_u64) {
                    self.completion = count;
                }
            }
            "error" => {
                let kind = event.pointer("/error/type").and_then(Value::as_str).unwrap_or("");
                let message = event.pointer("/error/message").and_then(Value::as_str).unwrap_or(data);
                return Err(classify_error(0, kind, message));
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self) -> Result<Completion, LlmError> {
        if let Some(refused) = self.refused {
            return Err(LlmError::Rejected(refused));
        }
        let mut completion = Completion {
            finish: match self.stop.as_str() {
                "tool_use" => "tool_calls".to_string(),
                "max_tokens" => "length".to_string(),
                "" => String::new(),
                _ => "stop".to_string(),
            },
            usage: Some(Usage { prompt: self.prompt, cached: self.cached, completion: self.completion, tokens_per_second: None, window: None }),
            ..Completion::default()
        };
        let mut sent_back = Vec::new();
        for (mut block, partial) in self.blocks.into_iter().zip(self.partial) {
            match block.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => completion.content.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
                "thinking" => completion.reasoning.push_str(block.get("thinking").and_then(Value::as_str).unwrap_or("")),
                "tool_use" => {
                    // With eager input streaming the JSON is not checked by the server, and a call cut by max_tokens is not whole.
                    if completion.finish == "length" {
                        continue;
                    }
                    let id = block.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    let arguments = if partial.is_empty() { "{}".to_string() } else { partial };
                    let input: Value = serde_json::from_str(&arguments).unwrap_or_else(|_| json!({}));
                    block.insert("input".to_string(), input);
                    completion.tool_calls.push(ToolCall { id, kind: "function".into(), function: FunctionCall { name, arguments } });
                }
                _ => {}
            }
            if !block.is_empty() {
                sent_back.push(Value::Object(block));
            }
        }
        // A reasoning summary is shown; what goes back is the blocks themselves.
        completion.reasoning.clear();
        if !sent_back.is_empty() {
            completion.blocks = Some(Value::Array(sent_back));
        }
        Ok(completion)
    }
}

fn reason_is_refusal(reason: &str) -> bool {
    reason == "refusal"
}

impl Anthropic {
    pub fn new(base_url: &str, key: String) -> Result<Anthropic, String> {
        Ok(Anthropic { base: Url::parse(base_url)?, key })
    }

    fn headers(&self) -> Vec<(String, String)> {
        vec![
            ("x-api-key".to_string(), self.key.clone()),
            ("anthropic-version".to_string(), API_VERSION.to_string()),
            ("anthropic-beta".to_string(), FALLBACK_BETA.to_string()),
        ]
    }

    /// The model's context, from the Models API.
    pub fn context_of(&self, model: &str) -> Result<u64, String> {
        let cancel = AtomicBool::new(false);
        let url = self.base.join(&format!("models/{model}"));
        let mut response = http::request("GET", &url, &self.headers(), None, Duration::from_secs(10), &cancel).map_err(|failure| LlmError::from(failure).to_string())?;
        let status = response.status;
        let text = response.text().map_err(|failure| LlmError::from(failure).to_string())?;
        if status != 200 {
            return Err(classify(status, &text).to_string());
        }
        let value: Value = serde_json::from_str(&text).map_err(|_| llm::UNREADABLE.to_string())?;
        Ok(value.get("max_input_tokens").and_then(Value::as_u64).unwrap_or(DEFAULT_CONTEXT))
    }
}

impl Provider for Anthropic {
    /// 5 MB as base64 is what it takes of one; past 2000 pixels a request of many pictures is refused.
    fn image_limit(&self) -> ImageLimit {
        ImageLimit { side: 2000, bytes: 3_750_000 }
    }

    fn images_in_tool_results(&self) -> bool {
        true
    }

    fn stream(&self, request: &Request, cancel: &AtomicBool, on: &mut dyn FnMut(Delta)) -> Result<Completion, LlmError> {
        let text = request_body(request).to_string();
        let url = self.base.join("messages");
        let mut response = http::request("POST", &url, &self.headers(), Some(&text), Duration::from_secs(20), cancel)?;
        if response.status != 200 {
            let status = response.status;
            return Err(classify(status, &response.text()?));
        }
        let mut assembly = Assembly::default();
        let mut pending = Vec::new();
        while let Some(bytes) = response.next()? {
            pending.extend_from_slice(&bytes);
            while let Some(at) = pending.iter().position(|&byte| byte == b'\n') {
                let line: Vec<u8> = pending.drain(..=at).collect();
                let line = String::from_utf8_lossy(&line);
                let Some(data) = line.trim_end().strip_prefix("data:") else { continue };
                assembly.event(data.trim_start(), on)?;
            }
        }
        assembly.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(events: &[Value]) -> Assembly {
        let mut assembly = Assembly::default();
        for event in events {
            assembly.event(&event.to_string(), &mut |_| {}).unwrap();
        }
        assembly
    }

    #[test]
    fn thinking_and_calls_come_back_as_they_were_sent() {
        let completion = feed(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 5, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 10}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hm"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Reading."}}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "t1", "name": "read_file", "input": {}}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"path\""}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": ":\"a\"}"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 9}}),
        ]).finish().unwrap();
        assert_eq!(completion.finish, "tool_calls");
        assert_eq!(completion.content, "Reading.");
        assert_eq!(completion.tool_calls[0].function.arguments, "{\"path\":\"a\"}");
        let usage = completion.usage.clone().unwrap();
        assert_eq!((usage.prompt, usage.cached, usage.completion), (115, 100, 9));
        let message = completion.message();
        let blocks = assistant_blocks(&message);
        assert_eq!(blocks[0], json!({"type": "thinking", "thinking": "hm", "signature": "sig"}));
        assert_eq!(blocks[2]["input"], json!({"path": "a"}));
    }

    #[test]
    fn a_call_cut_by_the_limit_is_not_sent_back_and_broken_input_stays_broken_for_the_gate() {
        let cut = feed(&[
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t", "name": "create_file", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"pa"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}}),
        ]).finish().unwrap();
        assert_eq!(cut.finish, "length");
        assert!(cut.tool_calls.is_empty() && cut.blocks.is_none());
        let broken = feed(&[
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t", "name": "create_file", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"pa"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
        ]).finish().unwrap();
        assert_eq!(broken.tool_calls[0].function.arguments, "{\"pa");
        assert!(serde_json::from_str::<Value>(&broken.tool_calls[0].function.arguments).is_err());
    }

    #[test]
    fn a_refusal_discards_the_partial_answer_and_says_why() {
        let result = feed(&[
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "partial"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_details": {"type": "refusal", "category": "cyber"}}}),
        ]).finish();
        match result {
            Err(LlmError::Rejected(message)) => assert!(message.contains("cyber")),
            other => panic!("{:?}", other.map(|completion| completion.finish)),
        }
    }

    #[test]
    fn a_servers_fallback_block_goes_back_with_the_rest() {
        let completion = feed(&[
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "fallback", "from": {"model": "a"}, "to": {"model": "b"}}}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "hi"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        ]).finish().unwrap();
        assert_eq!(completion.finish, "stop");
        assert_eq!(completion.blocks.unwrap()[0]["type"], "fallback");
    }

    #[test]
    fn a_conversation_converts_with_one_message_of_results() {
        let call = |id: &str| ToolCall { id: id.into(), kind: "function".into(), function: FunctionCall { name: "read_file".into(), arguments: "{\"path\":\"a\"}".into() } };
        let messages = [
            Message::text("system", "prompt"),
            Message::text("user", "hi"),
            Message { role: "assistant".into(), content: Some("ok".into()), reasoning_content: Some("from another model".into()), tool_calls: Some(vec![call("1"), call("2")]), ..Message::default() },
            Message::tool_result("1", "fine"),
            Message::tool_result("2", "error: no such file"),
        ];
        let (system, out) = convert(&messages);
        assert_eq!(system, "prompt");
        assert_eq!(out.len(), 3);
        assert_eq!(out[1]["content"][1]["input"], json!({"path": "a"}));
        assert!(!out[1].to_string().contains("from another model"), "another model's reasoning is not thinking");
        assert_eq!(out[2]["content"].as_array().unwrap().len(), 2);
        assert!(out[2]["content"][0].get("is_error").is_none());
        assert_eq!(out[2]["content"][1]["is_error"], true);
    }

    #[test]
    fn a_request_is_what_the_current_api_takes() {
        let schemas = crate::tools::schemas();
        let messages = [Message::text("system", "s"), Message::text("user", "hi")];
        let body = request_body(&Request { model: DEFAULT_MODEL, messages: &messages, tools: Some(&schemas), effort: Some("none"), images: None, japanese_letters: false });
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "low");
        assert_eq!(body["system"], "s");
        assert_eq!(body["tools"].as_array().unwrap().len(), 13);
        assert_eq!(body["tools"][0]["eager_input_streaming"], true);
        assert!(body["tools"][0].get("input_schema").is_some());
        for field in ["temperature", "top_p", "top_k", "tool_choice"] {
            assert!(body.get(field).is_none(), "{field}");
        }
    }

    #[test]
    fn errors_are_told_apart() {
        assert!(matches!(classify(429, r#"{"error":{"type":"rate_limit_error","message":"slow"}}"#), LlmError::Transient(_)));
        assert!(matches!(classify(529, r#"{"error":{"type":"overloaded_error","message":"busy"}}"#), LlmError::Transient(_)));
        assert!(matches!(classify(401, r#"{"error":{"type":"authentication_error","message":"bad key"}}"#), LlmError::Rejected(_)));
        assert!(matches!(classify(400, r#"{"error":{"type":"invalid_request_error","message":"prompt is too long: 2000000 tokens"}}"#), LlmError::Overflow(_)));
    }
}
