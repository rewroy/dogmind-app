//! Chat completions over SSE, as Dogmind speaks to Straydog: `tool_choice` only when it is one Straydog takes, no
//! `/tokenize`, no `max_tokens` or `temperature` on a main turn, the reasoning effort by name.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::{json, Value};

use crate::http::{self, Failure, Url};
use crate::message::{Message, ToolCall};

#[derive(Debug)]
pub enum LlmError {
    Cancelled,
    /// The server cannot be reached or broke off: worth trying again.
    Transient(String),
    /// The prompt does not fit the context.
    Overflow(String),
    Rejected(String),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::Cancelled => write!(f, "cancelled"),
            LlmError::Transient(message) | LlmError::Overflow(message) | LlmError::Rejected(message) => write!(f, "{message}"),
        }
    }
}

impl From<Failure> for LlmError {
    fn from(failure: Failure) -> LlmError {
        match failure {
            Failure::Cancelled => LlmError::Cancelled,
            other => LlmError::Transient(other.to_string()),
        }
    }
}

pub enum Delta<'a> {
    Reasoning(&'a str),
    Content(&'a str),
    /// The name of a tool call whose arguments are arriving.
    Tool(&'a str),
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub prompt: u64,
    pub cached: u64,
    pub completion: u64,
    pub tokens_per_second: Option<f64>,
    /// The context, when the provider says it with its usage.
    pub window: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    /// `stop`, `tool_calls` or `length`; empty when the stream ended without one.
    pub finish: String,
    pub usage: Option<Usage>,
    /// The provider's own form of the answer, to send back to it.
    pub blocks: Option<Value>,
}

impl Completion {
    /// The assistant message that goes in the conversation.
    pub fn message(&self) -> Message {
        Message {
            role: "assistant".into(),
            content: (!self.content.is_empty()).then(|| self.content.clone()),
            reasoning_content: (!self.reasoning.is_empty()).then(|| self.reasoning.clone()),
            tool_calls: (!self.tool_calls.is_empty()).then(|| self.tool_calls.clone()),
            tool_call_id: None,
            provider_blocks: self.blocks.clone(),
        }
    }
}

/// Assembles a completion from a stream's `data:` payloads.
#[derive(Default)]
struct Assembly {
    completion: Completion,
    rate: Option<f64>,
}

impl Assembly {
    /// The speed may have come before or after the usage.
    fn finish(mut self) -> Completion {
        if let Some(usage) = &mut self.completion.usage {
            usage.tokens_per_second = self.rate;
        }
        self.completion
    }

    fn event(&mut self, data: &str, on: &mut dyn FnMut(Delta)) -> Result<(), LlmError> {
        let event: Value = serde_json::from_str(data).map_err(|error| LlmError::Transient(format!("bad event from the server: {error}")))?;
        if let Some(error) = event.get("error") {
            return Err(classify(0, &error.to_string()));
        }
        let timings = event.get("timings");
        if let Some(rate) = ["predicted_per_second", "decode_tokens_per_second"].iter().find_map(|name| timings?.get(*name)?.as_f64()) {
            self.rate = Some(rate);
        }
        if let Some(usage) = event.get("usage").filter(|usage| usage.is_object()) {
            let count = |value: Option<&Value>| value.and_then(Value::as_u64).unwrap_or(0);
            self.completion.usage = Some(Usage {
                prompt: count(usage.get("prompt_tokens")),
                cached: count(usage.pointer("/prompt_tokens_details/cached_tokens")),
                completion: count(usage.get("completion_tokens")),
                tokens_per_second: None,
                window: usage.get("model_context_window").and_then(Value::as_u64),
            });
        }
        let Some(choice) = event.pointer("/choices/0") else { return Ok(()) };
        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
            self.completion.finish = finish.to_string();
        }
        let Some(delta) = choice.get("delta") else { return Ok(()) };
        if let Some(text) = delta.get("reasoning_content").or_else(|| delta.get("reasoning")).and_then(Value::as_str) {
            self.completion.reasoning.push_str(text);
            on(Delta::Reasoning(text));
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            self.completion.content.push_str(text);
            on(Delta::Content(text));
        }
        for call in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let calls = &mut self.completion.tool_calls;
            while calls.len() <= index {
                calls.push(ToolCall { kind: "function".into(), ..ToolCall::default() });
            }
            let slot = &mut calls[index];
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                slot.id = id.to_string();
            }
            if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                slot.function.name.push_str(name);
                on(Delta::Tool(name));
            }
            if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
                slot.function.arguments.push_str(arguments);
            }
        }
        Ok(())
    }
}

fn classify(status: u16, body: &str) -> LlmError {
    let lower = body.to_lowercase();
    if lower.contains("context") && (lower.contains("exceed") || lower.contains("too long") || lower.contains("too many") || lower.contains("does not fit")) {
        return LlmError::Overflow(body.to_string());
    }
    if status == 503 || status == 429 || status >= 500 {
        return LlmError::Transient(format!("HTTP {status}: {body}"));
    }
    LlmError::Rejected(if status == 0 { body.to_string() } else { format!("HTTP {status}: {body}") })
}

/// `stream_options` and the rest of a chat request. Dogmind's rule: what is not needed is not sent.
pub fn request_body(model: &str, messages: &[Message], tools: Option<&Value>, effort: Option<&str>) -> Value {
    let messages: Vec<Message> = messages.iter().map(Message::without_provider_blocks).collect();
    let mut body = json!({"model": model, "messages": messages, "stream": true, "stream_options": {"include_usage": true}});
    if let Some(tools) = tools {
        body["tools"] = tools.clone();
    }
    if let Some(effort) = effort {
        body["reasoning_effort"] = json!(effort);
    }
    body
}

/// One request of a conversation, whoever serves it.
pub struct Request<'a> {
    pub model: &'a str,
    pub messages: &'a [Message],
    pub tools: Option<&'a Value>,
    pub effort: Option<&'a str>,
}

/// What the agent needs of a model. `on` hears the answer as it comes; a provider that only has it at the end says it then.
pub trait Provider: Send + Sync {
    fn stream(&self, request: &Request, cancel: &AtomicBool, on: &mut dyn FnMut(Delta)) -> Result<Completion, LlmError>;
}

impl Provider for Client {
    fn stream(&self, request: &Request, cancel: &AtomicBool, on: &mut dyn FnMut(Delta)) -> Result<Completion, LlmError> {
        if self.replay_reasoning {
            return self.post(&request_body(request.model, request.messages, request.tools, request.effort), cancel, on);
        }
        let messages: Vec<Message> = request.messages.iter().map(|message| Message { reasoning_content: None, ..message.clone() }).collect();
        self.post(&request_body(request.model, &messages, request.tools, request.effort), cancel, on)
    }
}

#[derive(Clone)]
pub struct Client {
    pub base: Url,
    pub key: Option<String>,
    /// Sends the reasoning back with each assistant message, which keeps Straydog's conversation cache warm. Other
    /// services have no use for it, and some refuse a field they do not know.
    pub replay_reasoning: bool,
}

impl Client {
    pub fn new(base_url: &str, key: Option<String>) -> Result<Client, String> {
        Ok(Client { base: Url::parse(base_url)?, key, replay_reasoning: true })
    }

    fn headers(&self) -> Vec<(String, String)> {
        self.key.iter().map(|key| ("Authorization".to_string(), format!("Bearer {key}"))).collect()
    }

    /// The server root: `/health` and `/props` sit beside `/v1`.
    fn root(&self, path: &str) -> Url {
        Url { path: path.to_string(), ..self.base.clone() }
    }

    fn post(&self, body: &Value, cancel: &AtomicBool, on: &mut dyn FnMut(Delta)) -> Result<Completion, LlmError> {
        let text = body.to_string();
        let url = self.base.join("chat/completions");
        let mut response = http::request("POST", &url, &self.headers(), Some(&text), Duration::from_secs(10), cancel)?;
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
                let data = data.trim_start();
                if data == "[DONE]" {
                    return Ok(assembly.finish());
                }
                assembly.event(data, on)?;
            }
        }
        Ok(assembly.finish())
    }

    fn get_json(&self, url: &Url, cancel: &AtomicBool) -> Result<(u16, Value), Failure> {
        let mut response = http::request("GET", url, &self.headers(), None, Duration::from_secs(5), cancel)?;
        let status = response.status;
        let text = response.text()?;
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
    }

    /// What `/health` says: `ok`, or `loading` with its progress, or an error.
    pub fn health(&self) -> Result<Health, String> {
        let cancel = AtomicBool::new(false);
        let (_, body) = self.get_json(&self.root("/health"), &cancel).map_err(|failure| failure.to_string())?;
        Ok(match body.get("status").and_then(Value::as_str) {
            Some("ok") => Health::Ready,
            Some("loading") => Health::Loading(body.get("progress").cloned().unwrap_or(Value::Null)),
            _ => Health::Failed(body.get("error").map_or_else(|| body.to_string(), Value::to_string)),
        })
    }

    /// The model's id, the context and whether Straydog answers here.
    pub fn describe(&self) -> Result<ServerInfo, String> {
        let cancel = AtomicBool::new(false);
        let (_, models) = self.get_json(&self.base.join("models"), &cancel).map_err(|failure| failure.to_string())?;
        let (_, props) = self.get_json(&self.root("/props"), &cancel).map_err(|failure| failure.to_string())?;
        let model = models.pointer("/data/0/id").and_then(Value::as_str).unwrap_or("default").to_string();
        let context = props.pointer("/default_generation_settings/n_ctx").and_then(Value::as_u64).unwrap_or(0);
        let straydog = props.get("build_info").and_then(Value::as_str).is_some_and(|build| build.starts_with("straydog"))
            || models.pointer("/data/0/owned_by").and_then(Value::as_str) == Some("straydog");
        Ok(ServerInfo { model, context, straydog })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Health {
    Ready,
    Loading(Value),
    Failed(String),
}

#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    pub model: String,
    pub context: u64,
    pub straydog: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(events: &[&str]) -> (Completion, Vec<String>) {
        let mut assembly = Assembly::default();
        let mut seen = Vec::new();
        for event in events {
            assembly.event(event, &mut |delta| seen.push(match delta {
                Delta::Reasoning(text) => format!("r:{text}"),
                Delta::Content(text) => format!("c:{text}"),
                Delta::Tool(name) => format!("t:{name}"),
            })).unwrap();
        }
        (assembly.finish(), seen)
    }

    #[test]
    fn a_tool_call_assembles_across_its_deltas() {
        let (completion, seen) = feed(&[
            r#"{"choices":[{"delta":{"reasoning_content":"hm"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read_file","arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":1}"}}]},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":5}},"timings":{"predicted_per_second":41.5}}"#,
        ]);
        assert_eq!(seen, ["r:hm", "t:read_file"]);
        assert_eq!(completion.tool_calls[0].function.arguments, "{\"path\":1}");
        assert_eq!(completion.finish, "tool_calls");
        let usage = completion.usage.unwrap();
        assert_eq!((usage.prompt, usage.cached, usage.completion, usage.tokens_per_second), (7, 5, 3, Some(41.5)));
    }

    #[test]
    fn the_speed_counts_when_it_comes_before_the_usage() {
        let (completion, _) = feed(&[
            r#"{"choices":[{"delta":{"content":"a"},"finish_reason":"stop"}],"usage":null,"timings":{"decode_tokens_per_second":33.5}}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
        ]);
        assert_eq!(completion.usage.unwrap().tokens_per_second, Some(33.5));
    }

    #[test]
    fn nothing_unneeded_goes_in_a_request() {
        let body = request_body("m", &[Message::text("user", "hi")], None, None);
        for field in ["tools", "tool_choice", "max_tokens", "temperature", "reasoning_effort", "cache_prompt"] {
            assert!(body.get(field).is_none(), "{field}");
        }
        assert_eq!(request_body("m", &[], None, Some("low"))["reasoning_effort"], "low");
    }

    #[test]
    fn a_full_context_is_told_from_other_refusals() {
        assert!(matches!(classify(400, "the prompt exceeds the context of 1024"), LlmError::Overflow(_)));
        assert!(matches!(classify(400, "tool_choice must be auto or none"), LlmError::Rejected(_)));
        assert!(matches!(classify(503, "loading"), LlmError::Transient(_)));
    }
}
