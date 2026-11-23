//! Chat completions over SSE, as Dogmind speaks to Straydog: `tool_choice` only when it is one Straydog takes, no
//! `/tokenize`, no `max_tokens` or `temperature` on a main turn, the reasoning effort by name.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::{json, Value};

use std::path::Path;

use crate::http::{self, Failure, Url};
use crate::images::{self, ImageLimit};
use crate::message::{Message, ToolCall};
use crate::safety;

#[derive(Debug)]
pub enum LlmError {
    Cancelled,
    /// The server cannot be reached or broke off: worth trying again.
    Transient(String),
    /// The prompt does not fit the context.
    Overflow(String),
    Rejected(String),
    /// The server has failed for good, or nothing listens on this machine: no use asking it again, and no reason to touch the conversation.
    Down(String),
    /// This model reads no pictures: asking again changes nothing until they are out of the conversation.
    NoVision(String),
}

// What a person reads when a provider fails, and not what the failure said: a raw socket error tells them nothing they can act on.
// English here as all the agent's words; `lang` in the app has each in Japanese, and a test there holds it to [`WORDS`].
pub const SERVER_GONE: &str = "The server failed or stopped answering. Your conversation is kept: check that the server is running, then send your message again.";
pub const SERVER_OUT_OF_MEMORY: &str = "The server ran out of memory and stopped. Your conversation is kept: free some memory on it, then send your message again.";
pub const ENGINE_GONE: &str = "The engine stopped. Your conversation is kept: write again and the engine starts, and you can go on from here. /log shows why.";
pub const ENGINE_OUT_OF_MEMORY: &str = "The engine ran out of memory and stopped. Your conversation is kept: close other apps and write again, and the engine starts and you can go on from here.";
pub const SERVER_BUSY: &str = "The server is busy or still loading, so it did not take this request. Your conversation is kept: send your message again in a moment.";
pub const KEY_REFUSED: &str = "The server refused the API key. Your conversation is kept: check the key in Settings, then send your message again.";
pub const NOT_FOUND: &str = "The server does not know this address or model. Your conversation is kept: check the URL and the model in Settings.";
pub const CONTEXT_FULL: &str = "This conversation is too long for the model, even summed up. It is kept as it was: start a new session with /new, or go back with /rewind.";
pub const MEMORY_FULL: &str = "There is not enough memory for a conversation this long, even compacted. It is kept as it was: close other apps and send your message again, or start a new session with /new.";
pub const NO_VISION: &str = "This model cannot read pictures, so the picture was not sent. Your conversation and your words are kept. To use pictures, choose a model that reads them; a local model needs its vision file beside it.";
pub const NOT_READY: &str = "the engine is not ready yet";
pub const UNFINISHED: &str = "the server ended the answer without finishing it";
pub const UNREADABLE: &str = "the server sent something that could not be read";
pub const BROKE_OFF: &str = "The answer broke off: asking for the rest.";
pub const SERVER_RESTARTING: &str = "The server is restarting. The request is kept and goes when it is ready, for up to 10 minutes; Stop ends the wait.";
pub const NO_CHECKPOINT: &str = "The model could not write a checkpoint, so the oldest turns go.";
pub const START_FAILED: &str = "the engine stopped while loading: /log shows why, and writing again tries once more";
pub const START_OUT_OF_MEMORY: &str = "the engine ran out of memory while loading: close other apps and write again";
/// These begin a message; what the server said follows them.
pub const SERVER_ERROR: &str = "The server reported an error. Your conversation is kept: send your message again. It said: ";
pub const REFUSED: &str = "The server refused this request. Your conversation is kept. It said: ";
pub const NO_ANSWER: &str = "no answer from the server: check that it is running and that its address is right: ";
/// Every whole message above, for the test that each has its Japanese.
pub const WORDS: [&str; 18] = [NO_VISION, SERVER_GONE, SERVER_OUT_OF_MEMORY, ENGINE_GONE, ENGINE_OUT_OF_MEMORY, SERVER_BUSY, KEY_REFUSED, NOT_FOUND, CONTEXT_FULL, MEMORY_FULL, NOT_READY, UNFINISHED, UNREADABLE, BROKE_OFF, SERVER_RESTARTING, NO_CHECKPOINT, START_FAILED, START_OUT_OF_MEMORY];
/// Every message above that begins one.
pub const OPENINGS: [&str; 3] = [SERVER_ERROR, REFUSED, NO_ANSWER];

fn says_out_of_memory(said: &str) -> bool {
    let lower = said.to_lowercase();
    lower.contains("out of memory") || (lower.contains("memory") && lower.contains("not enough"))
}

/// The words for a server that has gone, from what it last said.
pub fn gone_words(said: &str) -> &'static str {
    if says_out_of_memory(said) { SERVER_OUT_OF_MEMORY } else { SERVER_GONE }
}

/// [`gone_words`], with the server's own line under them.
pub fn gone_with_line(said: &str) -> String {
    with_line(gone_words(said), said)
}

/// Whether these words say the server has gone.
pub fn is_gone(text: &str) -> bool {
    matches!(sentence_of(text), SERVER_GONE | SERVER_OUT_OF_MEMORY)
}

/// The same words for an engine this app starts: writing brings it back. The server's line under them, if there is one, stays.
pub fn for_the_engine(text: &str) -> String {
    let (sentence, line) = split(text);
    let sentence = match sentence {
        SERVER_GONE => ENGINE_GONE,
        SERVER_OUT_OF_MEMORY => ENGINE_OUT_OF_MEMORY,
        other => other,
    };
    line.map_or_else(|| sentence.to_string(), |line| format!("{sentence}{LINE_BREAK}{line}"))
}

/// What a server said, as the tail of a message: a web page's HTML says nothing.
fn detail(said: &str) -> String {
    let said = said.trim();
    if said.starts_with('<') { "(no details)".to_string() } else { safety::hide_secrets(said).chars().take(300).collect() }
}

/// Between one of the app's sentences and the server's line under it, in a message that holds both.
const LINE_BREAK: char = '\n';

/// The most characters of a server's line that are shown under a sentence.
pub const LINE_MOST: usize = 200;

/// A server's words as the one line shown under the app's sentence: its first line, as it came (in English), with what reads as a key or a header in it hidden, and cut to [`LINE_MOST`]
/// characters. Too long, it keeps its start and its end with a mark between, as what names the cause is often at the end. None for a web page's HTML, which is not the server's, and for nothing.
pub fn line_of(said: &str) -> Option<String> {
    let said = said.trim();
    if said.is_empty() || said.starts_with('<') {
        return None;
    }
    let first = said.lines().map(str::trim).find(|line| !line.is_empty())?;
    let shown = safety::hide_secrets(first);
    let length = shown.chars().count();
    if length <= LINE_MOST {
        return Some(shown);
    }
    let (head, tail) = ((LINE_MOST - 1) / 2, LINE_MOST - 1 - (LINE_MOST - 1) / 2);
    Some(format!("{}…{}", shown.chars().take(head).collect::<String>(), shown.chars().skip(length - tail).collect::<String>()))
}

/// One of the app's sentences, with the server's line under it where the server said anything to show.
pub fn with_line(sentence: &str, said: &str) -> String {
    match line_of(said) {
        Some(line) => format!("{sentence}{LINE_BREAK}{line}"),
        None => sentence.to_string(),
    }
}

/// A message taken apart: the app's sentence (one of [`WORDS`]) and the server's line under it, if it has one. Any other text is all sentence.
pub fn split(text: &str) -> (&str, Option<&str>) {
    for words in WORDS {
        if let Some(line) = text.strip_prefix(words).and_then(|rest| rest.strip_prefix(LINE_BREAK)).filter(|line| !line.is_empty()) {
            return (words, Some(line));
        }
    }
    (text, None)
}

/// The app's sentence of a message, whether the server's line is under it or not.
pub fn sentence_of(text: &str) -> &str {
    split(text).0
}

/// The server's line under the sentence of a message.
pub fn line_under(text: &str) -> Option<&str> {
    split(text).1
}

/// What the server said of a failure in a message: its line under one of [`WORDS`], or its words after [`REFUSED`] or [`SERVER_ERROR`], which
/// carry them in the sentence ([`NO_ANSWER`] carries an address, never kept). A Straydog's GPU wait timeout is a refusal (it says the request
/// can be sent again), and so was never kept for a report.
pub fn server_said(text: &str) -> Option<&str> {
    line_under(text).or_else(|| [REFUSED, SERVER_ERROR].iter().find_map(|opening| text.strip_prefix(opening)).map(str::trim).filter(|said| !said.is_empty()))
}

impl LlmError {
    /// The same error with a secret that is known (the key this request was sent with) hidden in its words: a server may say back what it was sent. A word too short to be told from the
    /// rest of the words is left.
    pub fn without(self, secret: &str) -> LlmError {
        if secret.chars().count() < 6 {
            return self;
        }
        let hide = |text: String| text.replace(secret, safety::HIDDEN);
        match self {
            LlmError::Cancelled => LlmError::Cancelled,
            LlmError::Transient(text) => LlmError::Transient(hide(text)),
            LlmError::Overflow(text) => LlmError::Overflow(hide(text)),
            LlmError::Rejected(text) => LlmError::Rejected(hide(text)),
            LlmError::Down(text) => LlmError::Down(hide(text)),
            LlmError::NoVision(text) => LlmError::NoVision(hide(text)),
        }
    }
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::Cancelled => write!(f, "cancelled"),
            LlmError::Transient(message) | LlmError::Overflow(message) | LlmError::Rejected(message) | LlmError::Down(message) | LlmError::NoVision(message) => write!(f, "{message}"),
        }
    }
}

impl From<Failure> for LlmError {
    fn from(failure: Failure) -> LlmError {
        match failure {
            Failure::Cancelled => LlmError::Cancelled,
            Failure::Connect(_) | Failure::Stream(_) => LlmError::Transient(SERVER_GONE.to_string()),
        }
    }
}

pub enum Delta<'a> {
    Reasoning(&'a str),
    Content(&'a str),
    /// The name of a tool call whose arguments are arriving, and which call of this answer it is.
    Tool(usize, &'a str),
    /// A piece of the arguments of that call.
    Arguments(usize, &'a str),
    /// The server reading the prompt, before any word of the answer: how many of its tokens it has read, of how many (Straydog says so
    /// about once a second while it reads, in a comment of the stream: `: prefill 4096/90724`).
    Prefill(u64, u64),
}

/// What a comment of the stream says of the prompt's reading (`: prefill 4096/90724`, the text after the colon): read, of how many.
fn prefill_said(comment: &str) -> Option<(u64, u64)> {
    let (done, total) = comment.trim().strip_prefix("prefill ")?.split_once('/')?;
    Some((done.parse().ok()?, total.parse().ok()?))
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
    /// What the server said of its own work at the stream's end, as it came: Straydog's engine puts its metrics there. For the trajectory.
    pub timings: Option<Value>,
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
            images: Vec::new(),
            // (the conversation gives it one as it goes in)
            at: None,
            notice: None,
            helper: None,
        }
    }
}

/// Assembles a completion from a stream's `data:` payloads.
#[derive(Default)]
struct Assembly {
    completion: Completion,
    rate: Option<f64>,
    /// Per call, what Straydog's pieces of it told as it was written (see [`WRITING`]). They are display, never part of the call.
    written: Vec<String>,
}

impl Assembly {
    /// The speed may have come before or after the usage.
    fn finish(mut self) -> Completion {
        if let Some(usage) = &mut self.completion.usage {
            usage.tokens_per_second = self.rate;
        }
        // A result with no id, or for a call with no name, is refused with every later request.
        self.completion.tool_calls.retain(|call| !call.function.name.is_empty());
        for (at, call) in self.completion.tool_calls.iter_mut().enumerate() {
            if call.id.is_empty() {
                call.id = format!("call-{at}");
            }
        }
        self.completion
    }

    fn event(&mut self, data: &str, on: &mut dyn FnMut(Delta)) -> Result<(), LlmError> {
        let event: Value = serde_json::from_str(data).map_err(|_| LlmError::Transient(UNREADABLE.to_string()))?;
        if let Some(error) = event.get("error").filter(|error| !error.is_null()) {
            return Err(if event.get("fatal") == Some(&Value::Bool(true)) { LlmError::Down(with_line(gone_words(&error.to_string()), said_in(error))) } else { classify(0, &error.to_string()) });
        }
        let timings = event.get("timings");
        if let Some(whole) = timings.filter(|whole| whole.is_object()) {
            self.completion.timings = Some(whole.clone());
        }
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
        // Pieces of the calls being written are told as they come and held to nothing more: a piece that is not as expected is not one, and no call is read from them.
        for piece in delta.get(WRITING).and_then(Value::as_array).into_iter().flatten() {
            let Some(index) = piece.get("index").and_then(Value::as_u64).filter(|index| *index < MAX_WRITING_CALLS) else { continue };
            let index = index as usize;
            if let Some(name) = piece.get("name").and_then(Value::as_str).filter(|name| !name.is_empty()) {
                on(Delta::Tool(index, name));
            }
            if let Some(text) = piece.get("arguments").and_then(Value::as_str).filter(|text| !text.is_empty()) {
                while self.written.len() <= index {
                    self.written.push(String::new());
                }
                self.written[index].push_str(text);
                on(Delta::Arguments(index, text));
            }
        }
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
            // (some servers send the id empty and the name again with each piece of the arguments)
            if let Some(id) = call.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
                slot.id = id.to_string();
            }
            if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                if slot.function.name != name {
                    slot.function.name.push_str(name);
                }
                on(Delta::Tool(index, &slot.function.name));
            }
            if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
                slot.function.arguments.push_str(arguments);
                // What was told as it was written is not told again: of the call as it comes whole, what it adds to that (none where that is not the start of it).
                let told = self.written.get_mut(index).map(std::mem::take).unwrap_or_default();
                let new = if told.is_empty() { arguments } else { arguments.strip_prefix(told.as_str()).unwrap_or("") };
                if !new.is_empty() {
                    on(Delta::Arguments(index, new));
                }
            }
        }
        Ok(())
    }
}

/// Whether a refusal says the model takes no pictures, as Straydog, llama.cpp and the services word it.
pub(crate) fn refuses_images(lower: &str) -> bool {
    lower.contains("reads no images") || lower.contains("vision encoder") || lower.contains("multimodal")
        || ((lower.contains("image") || lower.contains("vision")) && ["not support", "unsupported", "only supported", "not accept", "not enabled", "not available"].iter().any(|word| lower.contains(word)))
}

/// The words an error object holds: its message, or the error itself where it is a string. None where it has none.
fn said_in(error: &Value) -> &str {
    error.get("message").and_then(Value::as_str).or(error.as_str()).unwrap_or("")
}

/// `status` 0: an error inside a stream, whose own `code` and `type` say what a status would have. Straydog's `fatal` does not
/// reach a client: a fatal error comes as code 500, `server_error`, whatever its words, and the server then ends; the others as 400.
fn classify(status: u16, body: &str) -> LlmError {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let error = parsed.get("error").filter(|error| error.is_object()).unwrap_or(&parsed);
    let said = said_in(error);
    let said = if said.is_empty() { parsed.get("error").and_then(Value::as_str).or(parsed.as_str()).unwrap_or(body) } else { said };
    let code = error.get("code").and_then(Value::as_u64).unwrap_or(0);
    let server_failed = error.get("type").and_then(Value::as_str) == Some("server_error");
    let lower = said.to_lowercase();
    // First, as its words may read like a full context, and a compaction would then be asking a server that is gone.
    if error.get("fatal") == Some(&Value::Bool(true)) || (status == 0 && (server_failed || code >= 500)) || lower.contains("restart the server") {
        return LlmError::Down(gone_with_line(said));
    }
    if refuses_images(&lower) {
        return LlmError::NoVision(with_line(NO_VISION, said));
    }
    if !server_failed {
        // (Straydog with no memory left for a longer K/V: as a full context to the conversation, which a compaction makes shorter)
        if lower.contains("not enough host memory for the k/v") {
            return LlmError::Overflow(with_line(MEMORY_FULL, said));
        }
        if lower.contains("context") && (lower.contains("exceed") || lower.contains("too long") || lower.contains("too many") || lower.contains("does not fit")) {
            return LlmError::Overflow(with_line(CONTEXT_FULL, said));
        }
    }
    let status = if status == 0 { code as u16 } else { status };
    match status {
        503 | 429 => LlmError::Transient(with_line(SERVER_BUSY, said)),
        401 | 403 => LlmError::Rejected(with_line(KEY_REFUSED, said)),
        404 => LlmError::Rejected(with_line(NOT_FOUND, said)),
        500.. => LlmError::Transient(format!("{SERVER_ERROR}{}", detail(said))),
        _ => LlmError::Rejected(format!("{REFUSED}{}", detail(said))),
    }
}

/// A message as a server reads it back. Arguments that are no JSON object (a call cut off, a model's slip) are kept in the session and go here as
/// none: Straydog's template refuses a request that holds them, and so every request after one such call. The same each time, so the prefix holds.
/// With no pictures it is the message as it is; with them its content is parts, read from the session's folder. `held`: the digests of the pictures the server
/// keeps, which go as those, and where the digest of each picture sent goes.
fn as_sent(message: &Message, folder: Option<&Path>, held: Option<(&HashSet<String>, &mut Vec<String>)>) -> Value {
    let mut sent = message.without_provider_blocks();
    for call in sent.tool_calls.iter_mut().flatten() {
        let text = call.function.arguments.trim();
        if !text.is_empty() && !serde_json::from_str::<Value>(text).is_ok_and(|arguments| arguments.is_object()) {
            call.function.arguments = "{}".to_string();
        }
    }
    let content = (!sent.images.is_empty()).then(|| images::openai_content_held(&sent, folder, held));
    sent.images.clear();
    let mut value = serde_json::to_value(&sent).unwrap_or(Value::Null);
    if let Some(content) = content {
        value["content"] = content;
    }
    value
}

/// `stream_options` and the rest of a chat request, with no folder for pictures. Dogmind's rule: what is not needed is not sent.
pub fn request_body(model: &str, messages: &[Message], tools: Option<&Value>, effort: Option<&str>) -> Value {
    request_body_in(model, messages, tools, effort, None)
}

/// `folder`: where the session keeps its pictures.
pub fn request_body_in(model: &str, messages: &[Message], tools: Option<&Value>, effort: Option<&str>, folder: Option<&Path>) -> Value {
    request_body_held(model, messages, tools, effort, folder, None, Reminders::AS_USER)
}

/// How the app's own messages (its facts beside the person's message, its notes and notices) go to a server: in a role of their own (`role`), so that
/// whose a message is rests on its role and not only on its words, or as the user's (none), marked in their words only; and the facts after the person's
/// message or before it (`before`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reminders {
    pub role: Option<&'static str>,
    pub before: bool,
}

impl Reminders {
    pub const AS_USER: Reminders = Reminders { role: None, before: false };
}

/// The role a server that says so takes them in (`/props`'s `latest_reminder`: Straydog's, for DeepSeek, whose template has it).
pub const REMINDER_ROLE: &str = "latest_reminder";
/// The role Straydog is sent them in for any other model: a chat API's own role for what is not the user's, which its templates place where it is in the
/// conversation. A server whose templates are not known (one may take a system message only first, or move it there) is sent them as the user's, marked
/// in their words; one that refuses the role is sent them so from then on.
pub const APP_ROLE: &str = "system";
/// OpenAI's own API: its role for what the application says, as opposed to the user.
pub const OPENAI_APP_ROLE: &str = "developer";

/// The switch that puts the app's facts before the person's message, to compare where a model takes them best (DeepSeek's own example puts the date and
/// the place before the user's turn).
pub const REMINDER_BEFORE_VARIABLE: &str = "DOGMIND_REMINDER_BEFORE";

/// The app's messages in the role `reminders` names, and its facts before the person's message where it says so.
fn as_reminders(messages: &mut [Value], reminders: Reminders) {
    let Some(role) = reminders.role else { return };
    for at in 0..messages.len() {
        let message = &messages[at];
        if message["role"] != "user" || !message["content"].as_str().is_some_and(crate::prompt::is_from_app) {
            continue;
        }
        // (latest_reminder is the facts that go with the person's message, right after it: its template opens the answer after that message, and a reminder
        // with nothing of the person's before it, as the note after a stopped draft is, was the last thing in the prompt with no answer opened. DeepSeek went on
        // writing the note itself, 47,000 characters of it as its answer, before its call. Every other note of the app's stays the user's, marked in its words.)
        let after_the_person = at > 0 && messages[at - 1]["role"] == "user" && !messages[at - 1]["content"].as_str().is_some_and(crate::prompt::is_from_app);
        if role == REMINDER_ROLE && !(after_the_person && message["content"].as_str().is_some_and(crate::prompt::is_app_facts)) {
            continue;
        }
        messages[at]["role"] = json!(role);
    }
    if reminders.before {
        for at in 1..messages.len() {
            let facts = messages[at]["content"].as_str().is_some_and(crate::prompt::is_app_facts);
            if facts && messages[at]["role"] == role && messages[at - 1]["role"] == "user" {
                messages.swap(at - 1, at);
            }
        }
    }
}

/// Whether a body holds a message of the app's in a role of its own, as `as_reminders` gives it.
fn has_app_role(body: &Value) -> bool {
    body["messages"].as_array().is_some_and(|messages| messages.iter().any(|message| matches!(message["role"].as_str(), Some(REMINDER_ROLE | APP_ROLE | OPENAI_APP_ROLE)) && message["content"].as_str().is_some_and(crate::prompt::is_from_app)))
}

/// `request_body_in`, for a server that keeps the pictures it was sent (`/props`'s `image_digests`): those it holds (`held`, by their digests) go as `sha256:`
/// and the digest, and the digest of every picture sent goes in `sent`.
fn request_body_held(model: &str, messages: &[Message], tools: Option<&Value>, effort: Option<&str>, folder: Option<&Path>, mut held: Option<(&HashSet<String>, &mut Vec<String>)>, reminders: Reminders) -> Value {
    let mut messages: Vec<Value> = messages.iter().map(|message| as_sent(message, folder, held.as_mut().map(|(holds, sent)| (*holds, &mut **sent)))).collect();
    as_reminders(&mut messages, reminders);
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
    /// The folder the messages' pictures are in; none when this conversation has no file yet.
    pub images: Option<&'a Path>,
    /// This answer is to hold no letter Japanese is not written with, for a server that can see to that.
    pub japanese_letters: bool,
    /// What this request is called in the trajectory, which the server is also told, as `X-Request-Id`: `<session id>/<n>`. A provider with no HTTP to carry it leaves it out.
    pub request_id: Option<&'a str>,
}

/// The header that names a request to the server. What the server logs of it is printable ASCII with no space (0x21 to 0x7E) and at most 64 characters, so that is all that is sent of it:
/// none when nothing is left.
pub fn request_id_header(id: &str) -> Option<(String, String)> {
    let kept: String = id.chars().filter(|c| ('\u{21}'..='\u{7e}').contains(c)).take(64).collect();
    (!kept.is_empty()).then(|| ("X-Request-Id".to_string(), kept))
}

/// What the agent needs of a model. `on` hears the answer as it comes; a provider that only has it at the end says it then.
pub trait Provider: Send + Sync {
    fn stream(&self, request: &Request, cancel: &AtomicBool, on: &mut dyn FnMut(Delta)) -> Result<Completion, LlmError>;
    /// Whether anyone is there, asked before something is given up that cannot be had back.
    fn answers(&self) -> bool {
        true
    }
    /// What it takes of a picture: what is larger is made smaller when it is attached.
    fn image_limit(&self) -> ImageLimit {
        ImageLimit::WIDE
    }
    /// Whether a tool's result can hold a picture, so that `read_file` can show one.
    fn images_in_tool_results(&self) -> bool {
        false
    }
    /// Whether the model reads pictures at all, where its server says: Some(false) is one a picture would only be refused by.
    fn reads_pictures(&self) -> Option<bool> {
        None
    }
    /// Whether a tool call comes in pieces as the model writes it, which the app shows and with which it cuts a very long file into parts
    /// on disk. A provider that gives a call only once it is whole says no.
    fn shows_calls_as_written(&self) -> bool {
        false
    }
    /// Whether the server is reading or writing for nobody now: a warm-up is sent only then, so that it never waits behind, or delays, a
    /// request of another conversation or another client. One that cannot tell says yes.
    fn idle(&self) -> bool {
        true
    }
    /// A warm-up: [`Provider::stream`] for a request no later one goes on from, where the server can be told so.
    fn warm(&self, request: &Request, cancel: &AtomicBool, on: &mut dyn FnMut(Delta)) -> Result<Completion, LlmError> {
        self.stream(request, cancel, on)
    }
    /// The next request is to be answered with its thinking closed, where the server can be asked so (Straydog's `straydog.reasoning`):
    /// whether it will be. The request that writes after a stopped draft is one.
    fn close_thinking_once(&self) -> bool {
        false
    }
    /// The next request, the first after a draft stop, is to answer with a call (`tool_choice: "required"`), where the server is one that closes thinking: one that
    /// cannot write its thinking on in the answer's text. Says whether it will be.
    fn require_call_once(&self) -> bool {
        false
    }
    /// Whether a call it was still writing can join the conversation as far as it had come. A provider with blocks of its own to send back cannot make those up.
    fn keeps_cut_writes(&self) -> bool {
        false
    }
    /// Asked after a request was refused or broke off: whether the server is being restarted, and so is worth waiting for. A provider with no such thing to ask says it does not know.
    fn restart(&self, _cancel: &AtomicBool) -> Restart {
        Restart::Unknown
    }

    /// Whether a server that answered in the conversation, and now answers nothing at all ([`Restart::Silent`]), is waited for as one between two processes: not the
    /// app's own engine, which the app starts again itself.
    fn waits_out_silence(&self) -> bool {
        false
    }
    /// Whether the server is on this machine, by a loopback address: the app's own engine, or one the person runs here, which has the GPU's memory.
    fn on_this_machine(&self) -> bool {
        false
    }
}

/// Whether an address is of this machine: `localhost`, or a loopback IP.
pub fn is_loopback(url: &Url) -> bool {
    let host = url.host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    host == "localhost" || host.ends_with(".localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// What a server that refused a request says, when asked, of whether it is on its way back.
#[derive(Debug, Clone, PartialEq)]
pub enum Restart {
    /// `/health` says it is loading, with its progress; or the front of it answers 502, 503 or 504 with no health of its own, as a proxy does while the server behind it is replaced
    /// (the progress is then null).
    Coming(Value),
    /// `/health` says it is ready.
    Ready,
    /// Nothing answers `/health` at all: a server in the gap of a restart looks so, and so does one that is gone.
    Silent,
    /// It answers, and says nothing to go by (no `/health`, an error of its own): the usual retries go.
    Unknown,
}

impl Provider for Client {
    fn stream(&self, request: &Request, cancel: &AtomicBool, on: &mut dyn FnMut(Delta)) -> Result<Completion, LlmError> {
        // (a conversation's pictures go by their digests to a server that keeps them, once a request of it has sent them: what it is known by is the request's id)
        let conversation = request.request_id.and_then(|id| id.split('/').next()).filter(|id| self.image_digests && !id.is_empty());
        let Some(conversation) = conversation else {
            return match self.post_with_app_role(&self.body_for(request), &mut || self.body_for(request), request.request_id, cancel, on) {
                Ok(completion) => Ok(completion),
                Err(Refused::Failed(error)) => Err(error),
                Err(Refused::ImageNotHeld) => Err(LlmError::Rejected(IMAGE_NOT_HELD.to_string())),
            };
        };
        let holds = self.held.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(conversation).cloned().unwrap_or_default();
        let mut sent = Vec::new();
        let body = self.body_with(request, Some((&holds, &mut sent)));
        let mut again_sent = Vec::new();
        let first = self.post_with_app_role(&body, &mut || { again_sent.clear(); self.body_with(request, Some((&holds, &mut again_sent))) }, request.request_id, cancel, on);
        if !again_sent.is_empty() {
            sent = again_sent;
        }
        let answered = match first {
            // (it started again, or let one go: none of it is known to be held any more, and the request goes again with all the bytes)
            Err(Refused::ImageNotHeld) => {
                self.held.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(conversation);
                sent.clear();
                let body = self.body_with(request, Some((&HashSet::new(), &mut sent)));
                self.post(&body, request.request_id, cancel, on)
            }
            Err(Refused::Failed(error)) => Err(error),
            Ok(completion) => Ok(completion),
        };
        if answered.is_ok() && !sent.is_empty() {
            self.held.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).entry(conversation.to_string()).or_default().extend(sent);
        }
        answered
    }

    /// Straydog's: its body's `straydog.reasoning`, which a server that cannot close the thinking ignores.
    fn close_thinking_once(&self) -> bool {
        if self.replay_reasoning {
            self.close_next.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.replay_reasoning
    }

    fn require_call_once(&self) -> bool {
        let will = self.replay_reasoning && !self.required_refused.load(std::sync::atomic::Ordering::Relaxed);
        if will {
            self.require_next.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        will
    }

    /// Straydog's API takes them, and its template shows them inside the tool's response. The services' chat completions take text there.
    fn images_in_tool_results(&self) -> bool {
        self.replay_reasoning && self.reads_pictures() != Some(false)
    }

    /// What the server said when the session connected; where that was no, `/props` is asked again (at most every
    /// [`VISION_ASKED_AGAIN`]): a Straydog restarted with its vision encoder beside the model reads pictures, and a session connected
    /// before had refused every picture as "this model reads no pictures" until the app was started again.
    fn reads_pictures(&self) -> Option<bool> {
        if self.vision != Some(false) {
            return self.vision;
        }
        let mut asked = self.vision_asked.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((at, said)) = *asked {
            if at.elapsed() < VISION_ASKED_AGAIN {
                return said;
            }
        }
        let cancel = AtomicBool::new(false);
        // (no answer, or none about pictures: what the session was told stands)
        let said = match self.get_json(&self.root("/props"), &cancel, Duration::from_secs(3)) {
            Ok((200, props)) => props.pointer("/modalities/vision").and_then(Value::as_bool).or(Some(false)),
            _ => Some(false),
        };
        *asked = Some((std::time::Instant::now(), said));
        said
    }

    /// Straydog where it says it sends the calls as they are written (`tool_writing`); of other servers, OpenAI's own API, which streams
    /// a call's arguments. Any other may give a call only whole.
    fn shows_calls_as_written(&self) -> bool {
        if self.replay_reasoning { self.tool_writing } else { self.base.host.eq_ignore_ascii_case("api.openai.com") }
    }

    /// Straydog's `/slots`: idle where no slot is processing. No answer, or one it cannot read, is not idle: a warm-up can always be left out.
    fn idle(&self) -> bool {
        let cancel = AtomicBool::new(false);
        let Ok((200, slots)) = self.get_json(&self.root("/slots"), &cancel, IDLE_ASK_WITHIN) else { return false };
        slots.as_array().is_some_and(|slots| slots.iter().all(|slot| slot.get("is_processing").and_then(Value::as_bool) == Some(false)))
    }

    /// Straydog keeps of a request that says `checkpoint: false` only its system prompt's checkpoint: the checkpoints another
    /// conversation resumes from (where its last reply began) stay as they were.
    fn warm(&self, request: &Request, cancel: &AtomicBool, on: &mut dyn FnMut(Delta)) -> Result<Completion, LlmError> {
        match self.post_with_app_role(&self.warm_body(request), &mut || self.warm_body(request), request.request_id, cancel, on) {
            Ok(completion) => Ok(completion),
            Err(Refused::Failed(error)) => Err(error),
            Err(Refused::ImageNotHeld) => Err(LlmError::Rejected(IMAGE_NOT_HELD.to_string())),
        }
    }

    fn answers(&self) -> bool {
        http::answers(&self.base)
    }

    fn keeps_cut_writes(&self) -> bool {
        true
    }

    fn waits_out_silence(&self) -> bool {
        !self.own_engine
    }

    fn on_this_machine(&self) -> bool {
        self.own_engine || is_loopback(&self.base)
    }

    fn restart(&self, cancel: &AtomicBool) -> Restart {
        match self.get_json(&self.root("/health"), cancel, RESTART_ASK_WITHIN) {
            Err(_) => Restart::Silent,
            Ok((status, body)) => match body.get("status").and_then(Value::as_str) {
                Some("ok") => Restart::Ready,
                Some("loading") => Restart::Coming(body.get("progress").cloned().unwrap_or(Value::Null)),
                Some(_) => Restart::Unknown,
                None if matches!(status, 502 | 503 | 504) => Restart::Coming(Value::Null),
                None => Restart::Unknown,
            },
        }
    }
}

/// How long one question about a server's health is waited for while it is being waited for: the wait asks again soon after.
const RESTART_ASK_WITHIN: Duration = Duration::from_secs(8);

/// What a Straydog request names to have Han letters with no CP932 form refused.
const NOT_CP932_HAN: &str = "not_cp932_han";

/// The field of a streamed delta in which Straydog, asked to (`"straydog": {"tool_writing": true}`), sends the pieces of the tool calls the model is writing, which it holds back
/// and sends whole at the end: `[{"index": 0, "name": "create_file", "arguments": "{\"path\": \"a.rs"}]`, the name on a call's first piece only. The pieces are the start of the
/// call's arguments as far as the markup has come, and need not be JSON; only the call that comes whole is the call. They are for the person to see and for the app's cut of a
/// file too long, and nothing is parsed from them.
const WRITING: &str = "straydog_tool_writing";

/// The most calls one answer's pieces name: a piece for a place beyond is not one.
const MAX_WRITING_CALLS: u64 = 64;

/// How long a warm-up waits to hear whether the server is busy: a server slower than this to say so is left alone.
const IDLE_ASK_WITHIN: Duration = Duration::from_secs(2);

/// How long the answer to one of the questions asked of a server (`/health`, the models, `/props`) is waited for, from asking to its last byte. A server that is up answers these at
/// once, even while it loads a model.
const ANSWER_WITHIN: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct Client {
    pub base: Url,
    pub key: Option<String>,
    /// Sends the reasoning back with each assistant message, which keeps Straydog's conversation cache warm. Other
    /// services have no use for it, and some refuse a field they do not know.
    pub replay_reasoning: bool,
    /// The server says it can refuse those letters: another version, or another server, may turn the field down.
    pub suppresses_letters: bool,
    /// The server says it can send the calls as the model writes them (see [`WRITING`]): an older one refuses the request that asks.
    pub tool_writing: bool,
    /// Whether the model reads pictures, where the server says (`/props`): a picture is never sent to one that does not.
    pub vision: Option<bool>,
    /// What `/props` said of pictures when asked again, and when ([`Client::reads_pictures`]): a server that read none when the session
    /// connected may read them after a restart, its vision encoder put beside the model.
    pub vision_asked: std::sync::Arc<std::sync::Mutex<Option<(std::time::Instant, Option<bool>)>>>,
    /// The next request's thinking is to be closed ([`Provider::close_thinking_once`]).
    pub close_next: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The next request is to answer with a call ([`Provider::require_call_once`]); and the server refused that once, so it is not asked again.
    pub require_next: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub required_refused: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The engine this app starts and watches: nothing listening there is said at once, as the app knows why and starts it again. Any other server, on this
    /// machine too, may be on its way back, and is waited for as one.
    pub own_engine: bool,
    /// The server keeps the pictures it was sent, and takes one again by its digest (`/props`'s `image_digests`, Straydog's): a conversation sent all its
    /// pictures with every request, 97% of 4.33 GB in a day of page checks.
    pub image_digests: bool,
    /// The role the server takes the app's own messages in: `latest_reminder` where `/props` says so, `system` for any other model of Straydog's
    /// ([`ServerInfo::app_role`]); none for a server whose templates are not known. (Whose a message is then rests on its role, not only on its words.)
    pub app_role: Option<&'static str>,
    /// The server refused a request with the app's messages in a role of their own, and took it with them as the user's: they go so from then on.
    pub app_role_refused: std::sync::Arc<AtomicBool>,
    /// The digests of the pictures each conversation (by its id, the first part of a request's) sent this server in a request that went well: those go by
    /// their digests from then on. Forgotten when the server says it no longer holds one (`image_not_held`).
    pub held: std::sync::Arc<std::sync::Mutex<HashMap<String, HashSet<String>>>>,
}

/// The error type of a request that names by digest a picture the server no longer keeps (it started again, or let it go): nothing of it ran, and the same
/// request goes again with the pictures' bytes.
const IMAGE_NOT_HELD: &str = "image_not_held";

/// How a request was refused: as any refusal, or because a picture it named by digest is not held.
enum Refused {
    Failed(LlmError),
    ImageNotHeld,
}

impl From<LlmError> for Refused {
    fn from(error: LlmError) -> Refused {
        Refused::Failed(error)
    }
}

impl From<http::Failure> for Refused {
    fn from(failure: http::Failure) -> Refused {
        Refused::Failed(LlmError::from(failure))
    }
}

/// How long an answer of `/props` that the model reads no pictures holds before it is asked again ([`Client::reads_pictures`]).
const VISION_ASKED_AGAIN: Duration = Duration::from_secs(10);

impl Client {
    pub fn new(base_url: &str, key: Option<String>) -> Result<Client, String> {
        Ok(Client { base: Url::parse(base_url)?, key, replay_reasoning: true, suppresses_letters: false, tool_writing: false, vision: None,
                    vision_asked: Default::default(), close_next: Default::default(), require_next: Default::default(), required_refused: Default::default(), own_engine: false, image_digests: false, app_role: None, app_role_refused: Default::default(), held: Default::default() })
    }

    /// The request a model's turn is sent as, for this server.
    /// A warm-up's request: Straydog is told that no later request goes on from it (`checkpoint: false`).
    fn warm_body(&self, request: &Request) -> Value {
        let mut body = self.body_for(request);
        if self.replay_reasoning {
            if !body.get("straydog").is_some_and(Value::is_object) {
                body["straydog"] = json!({});
            }
            body["straydog"]["checkpoint"] = json!(false);
        }
        body
    }

    fn body_for(&self, request: &Request) -> Value {
        self.body_with(request, None)
    }

    /// How the app's own messages go to this server: in the role it is known to take them in, as `developer` messages to OpenAI's API, and as the user's
    /// to any other server or once it refused a role of their own.
    fn reminders(&self) -> Reminders {
        let role = if self.app_role_refused.load(std::sync::atomic::Ordering::Relaxed) {
            None
        } else if self.app_role.is_some() {
            self.app_role
        } else if self.base.host.eq_ignore_ascii_case("api.openai.com") {
            Some(OPENAI_APP_ROLE)
        } else {
            None
        };
        Reminders { role, before: std::env::var(REMINDER_BEFORE_VARIABLE).is_ok_and(|value| value == "1") }
    }

    /// `post_once`, and for a body with the app's messages in a role of their own that the server refused: once more with them as the user's (`again`
    /// makes that body), which, taken, is how they go to it from then on.
    fn post_with_app_role(&self, body: &Value, again: &mut dyn FnMut() -> Value, id: Option<&str>, cancel: &AtomicBool, on: &mut dyn FnMut(Delta)) -> Result<Completion, Refused> {
        let refused = match self.post_or_without_required(body, id, cancel, on) {
            // (a request it would not take, not a key it refused or an address it does not have)
            Err(Refused::Failed(LlmError::Rejected(said))) if said.starts_with(REFUSED) && has_app_role(body) && !cancel.load(std::sync::atomic::Ordering::SeqCst) => said,
            other => return other,
        };
        self.app_role_refused.store(true, std::sync::atomic::Ordering::Relaxed);
        let answered = self.post_once(&again(), id, cancel, on);
        // (refused again: it was not the role, and the first refusal is the one to tell)
        if matches!(answered, Err(Refused::Failed(LlmError::Rejected(_)))) {
            self.app_role_refused.store(false, std::sync::atomic::Ordering::Relaxed);
            return Err(Refused::Failed(LlmError::Rejected(refused)));
        }
        answered
    }

    /// A request asked to answer with a call (`tool_choice: "required"`) that the server refuses for that: sent again without it, the same request in all else,
    /// and not asked so again. (Not `again`: what closed its thinking is in this body alone.)
    fn post_or_without_required(&self, body: &Value, id: Option<&str>, cancel: &AtomicBool, on: &mut dyn FnMut(Delta)) -> Result<Completion, Refused> {
        let answered = self.post_once(body, id, cancel, on);
        match answered {
            Err(Refused::Failed(LlmError::Rejected(said))) if body.get("tool_choice") == Some(&json!("required")) && said.contains("tool_choice") && !cancel.load(std::sync::atomic::Ordering::SeqCst) => {
                self.required_refused.store(true, std::sync::atomic::Ordering::Relaxed);
                let mut plain = body.clone();
                if let Some(fields) = plain.as_object_mut() {
                    fields.remove("tool_choice");
                }
                self.post_once(&plain, id, cancel, on)
            }
            other => other,
        }
    }

    /// The body of a request, with the pictures the server holds of this conversation named by their digests (`held`), whose digests go in `sent`.
    fn body_with(&self, request: &Request, held: Option<(&HashSet<String>, &mut Vec<String>)>) -> Value {
        let reminders = self.reminders();
        let mut body = if self.replay_reasoning {
            request_body_held(request.model, request.messages, request.tools, request.effort, request.images, held, reminders)
        } else {
            let messages: Vec<Message> = request.messages.iter().map(|message| Message { reasoning_content: None, ..message.clone() }).collect();
            request_body_held(request.model, &messages, request.tools, request.effort, request.images, held, reminders)
        };
        if request.japanese_letters && self.suppresses_letters {
            body["suppress"] = json!(NOT_CP932_HAN);
        }
        // (a server that said it can: the calls come as the model writes them, beside the calls themselves, which come whole at the end)
        if self.tool_writing && request.tools.is_some() {
            body["straydog"] = json!({"tool_writing": true});
        }
        let require = self.require_next.swap(false, std::sync::atomic::Ordering::Relaxed);
        if self.close_next.swap(false, std::sync::atomic::Ordering::Relaxed) {
            if !body.get("straydog").is_some_and(Value::is_object) {
                body["straydog"] = json!({});
            }
            body["straydog"]["reasoning"] = json!(false);
            // (with no thinking, the first after a draft stop: a DeepSeek model went on with its thinking in the answer's text, 37,248 characters of it and "</think>" at
            // its end, shown to the person as the answer. Asked for a call, the server holds its first token to a call's form.)
            if require && request.tools.is_some() {
                body["tool_choice"] = json!("required");
            }
        }
        body
    }

    fn headers(&self) -> Vec<(String, String)> {
        self.key.iter().map(|key| ("Authorization".to_string(), format!("Bearer {key}"))).collect()
    }

    /// The server root: `/health` and `/props` sit beside `/v1`.
    fn root(&self, path: &str) -> Url {
        Url { path: path.to_string(), ..self.base.clone() }
    }

    /// Nothing listening where the app's own engine is will not change in a few seconds (the app starts it again); any other server, a person's own on this machine
    /// too, may be on its way back.
    fn unreached(&self, failure: Failure) -> LlmError {
        match failure {
            Failure::Connect(_) if self.own_engine => LlmError::Down(SERVER_GONE.to_string()),
            other => other.into(),
        }
    }

    /// Why this server could not be asked, with what a person can do about it. A system's socket error is left out; a certificate's is not.
    fn no_answer(&self, failure: Failure) -> String {
        let at = format!("{}:{}", self.base.host_for_header(), self.base.port);
        match failure {
            Failure::Connect(detail) if !detail.contains("os error") && !detail.contains(&at) => format!("{NO_ANSWER}{detail}"),
            _ => format!("{NO_ANSWER}{at}"),
        }
    }

    fn post(&self, body: &Value, id: Option<&str>, cancel: &AtomicBool, on: &mut dyn FnMut(Delta)) -> Result<Completion, LlmError> {
        match self.post_once(body, id, cancel, on) {
            Ok(completion) => Ok(completion),
            Err(Refused::Failed(error)) => Err(error),
            // (not one that named a picture by digest: the server's words for it, as any refusal of a request)
            Err(Refused::ImageNotHeld) => Err(LlmError::Rejected(IMAGE_NOT_HELD.to_string())),
        }
    }

    /// `post`, with a refusal for a picture named by digest that the server does not hold told apart.
    fn post_once(&self, body: &Value, id: Option<&str>, cancel: &AtomicBool, on: &mut dyn FnMut(Delta)) -> Result<Completion, Refused> {
        let text = body.to_string();
        let url = self.base.join("chat/completions");
        // (OpenAI-compatible servers ignore a header they do not know; Straydog writes it in its own line for the request)
        let mut headers = self.headers();
        headers.extend(id.and_then(request_id_header));
        let mut response = http::request("POST", &url, &headers, Some(&text), Duration::from_secs(10), cancel).map_err(|failure| self.unreached(failure))?;
        if response.status != 200 {
            let status = response.status;
            let said = response.text()?;
            let error_type = serde_json::from_str::<Value>(&said).ok().and_then(|said| said.pointer("/error/type").and_then(Value::as_str).map(str::to_string));
            if status == 400 && error_type.as_deref() == Some(IMAGE_NOT_HELD) {
                return Err(Refused::ImageNotHeld);
            }
            return Err(Refused::Failed(self.without_key(classify(status, &said))));
        }
        let mut assembly = Assembly::default();
        let mut pending = Vec::new();
        while let Some(bytes) = response.next()? {
            pending.extend_from_slice(&bytes);
            while let Some(at) = pending.iter().position(|&byte| byte == b'\n') {
                let line: Vec<u8> = pending.drain(..=at).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end();
                if let Some((done, total)) = line.strip_prefix(':').and_then(prefill_said) {
                    on(Delta::Prefill(done, total));
                    continue;
                }
                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim_start();
                if data == "[DONE]" {
                    return Ok(assembly.finish());
                }
                assembly.event(data, on).map_err(|error| self.without_key(error))?;
            }
        }
        // No [DONE] and no reason to end: a server that closes its connection to end a body died here, not finished.
        let completion = assembly.finish();
        if completion.finish.is_empty() {
            return Err(Refused::Failed(LlmError::Transient(SERVER_GONE.to_string())));
        }
        Ok(completion)
    }

    /// What a server said, without the key it was sent: it may say that back.
    fn without_key(&self, error: LlmError) -> LlmError {
        match &self.key {
            Some(key) => error.without(key),
            None => error,
        }
    }

    /// One GET, whose whole answer must be here within `within`: a server that takes the connection and says nothing (a hung server, a proxy holding it) would otherwise hold
    /// whoever asks for as long as a model's stream may be silent, half an hour. It is not asked again after that.
    fn get_json(&self, url: &Url, cancel: &AtomicBool, within: Duration) -> Result<(u16, Value), Failure> {
        let mut response = http::request_within("GET", url, &self.headers(), Duration::from_secs(5), cancel, within)?;
        let status = response.status;
        let text = response.text()?;
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
    }

    /// What `/health` says: `ok`, or `loading` with its progress, or an error.
    pub fn health(&self) -> Result<Health, String> {
        self.health_within(ANSWER_WITHIN)
    }

    fn health_within(&self, within: Duration) -> Result<Health, String> {
        let cancel = AtomicBool::new(false);
        let (_, body) = self.get_json(&self.root("/health"), &cancel, within).map_err(|failure| self.no_answer(failure))?;
        Ok(match body.get("status").and_then(Value::as_str) {
            Some("ok") => Health::Ready,
            Some("loading") => Health::Loading(body.get("progress").cloned().unwrap_or(Value::Null)),
            // (the log has what it said; a load that failed asks only for what a person can do)
            _ => {
                let words = if says_out_of_memory(&body.to_string()) { START_OUT_OF_MEMORY } else { START_FAILED };
                let said = body.pointer("/error/message").or_else(|| body.get("error")).or_else(|| body.get("message")).and_then(Value::as_str).unwrap_or("");
                Health::Failed(self.without_key(LlmError::Down(with_line(words, said))).to_string())
            }
        })
    }

    /// The model's id, the context and whether Straydog answers here.
    pub fn describe(&self) -> Result<ServerInfo, String> {
        self.describe_within(ANSWER_WITHIN)
    }

    /// Each of its two questions has `within` for its answer.
    fn describe_within(&self, within: Duration) -> Result<ServerInfo, String> {
        let cancel = AtomicBool::new(false);
        let (status, models) = self.get_json(&self.base.join("models"), &cancel, within).map_err(|failure| self.no_answer(failure))?;
        // A key refused or an address that is not one is known now, and calling this ready would only move the news to the first message.
        if let Some(words) = refused_at_the_door(status) {
            return Err(words.to_string());
        }
        // (a web page here is no API, whatever its status)
        if models.is_null() {
            return Err(NOT_FOUND.to_string());
        }
        let (status, props) = self.get_json(&self.root("/props"), &cancel, within).map_err(|failure| self.no_answer(failure))?;
        // (no `/props` is a server without that part of llama-server's interface, and nothing against it; a refused key is the same refusal)
        if matches!(status, 401 | 403) {
            return Err(KEY_REFUSED.to_string());
        }
        let model = models.pointer("/data/0/id").and_then(Value::as_str).unwrap_or("default").to_string();
        let context = props.pointer("/default_generation_settings/n_ctx").and_then(Value::as_u64).unwrap_or(0);
        let straydog = props.get("build_info").and_then(Value::as_str).is_some_and(|build| build.starts_with("straydog"))
            || models.pointer("/data/0/owned_by").and_then(Value::as_str) == Some("straydog");
        let vision = props.pointer("/modalities/vision").and_then(Value::as_bool);
        let suppresses_letters = props.get("suppress").and_then(Value::as_array).is_some_and(|names| names.iter().any(|name| name.as_str() == Some(NOT_CP932_HAN)));
        let tool_writing = props.get("tool_writing").and_then(Value::as_bool) == Some(true);
        let image_digests = props.get("image_digests").and_then(Value::as_bool) == Some(true);
        let latest_reminder = props.get("latest_reminder").and_then(Value::as_bool) == Some(true);
        Ok(ServerInfo { model, context, straydog, vision, suppresses_letters, tool_writing, image_digests, latest_reminder })
    }
}

/// What a status on asking for the models says about the provider's settings.
fn refused_at_the_door(status: u16) -> Option<&'static str> {
    match status {
        401 | 403 => Some(KEY_REFUSED),
        404 => Some(NOT_FOUND),
        _ => None,
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
    /// Whether the model reads pictures, where the server says: none where it does not, and then only a request can tell.
    pub vision: Option<bool>,
    pub suppresses_letters: bool,
    /// Whether the server sends the calls as the model writes them.
    pub tool_writing: bool,
    /// Whether the server keeps the pictures it was sent and takes one again by its digest.
    pub image_digests: bool,
    /// Whether the server takes the app's own messages in the role `latest_reminder`.
    pub latest_reminder: bool,
}

impl ServerInfo {
    /// The role the app's own messages go to this server in: `latest_reminder` where it says it takes that, `system` for Straydog's other models, none for
    /// a server whose templates are not known.
    pub fn app_role(&self) -> Option<&'static str> {
        if self.latest_reminder {
            Some(REMINDER_ROLE)
        } else if self.straydog {
            Some(APP_ROLE)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Straydog's comment while it reads a prompt is read as how far it has got; other comments, and a malformed one, are nothing.
    /// A server that refuses to be asked for a call is sent the same request without it, its thinking closed still, and not asked so again.
    #[test]
    fn a_refused_required_call_is_sent_again_without_it() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let kept = bodies.clone();
        std::thread::spawn(move || {
            for (at, stream) in listener.incoming().take(3).enumerate() {
                let mut stream = stream.unwrap();
                let (mut request, mut block) = (Vec::new(), [0u8; 65536]);
                let body = loop {
                    let count = std::io::Read::read(&mut stream, &mut block).unwrap();
                    request.extend_from_slice(&block[..count]);
                    let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    let length: usize = head.lines().find_map(|line| line.strip_prefix("content-length: ")?.parse().ok()).unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break serde_json::from_slice::<Value>(&request[end + 4..end + 4 + length]).unwrap();
                    }
                };
                kept.lock().unwrap().push(body);
                let answer = if at == 0 {
                    let said = r#"{"error":{"message":"tool_choice must be auto or none"}}"#;
                    format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{said}", said.len())
                } else {
                    let said = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{said}", said.len())
                };
                let _ = std::io::Write::write_all(&mut stream, answer.as_bytes());
            }
        });
        let mut client = Client::new(&format!("http://127.0.0.1:{port}/v1"), None).unwrap();
        client.replay_reasoning = true;
        let tools = json!([{"type": "function", "function": {"name": "read_file", "parameters": {"type": "object"}}}]);
        let messages = [Message::text("user", "go")];
        let request = Request { model: "m", messages: &messages, tools: Some(&tools), effort: None, images: None, japanese_letters: false, request_id: None };
        assert!(client.close_thinking_once() && client.require_call_once());
        let answered = client.stream(&request, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(answered.content, "ok");
        let sent = bodies.lock().unwrap().clone();
        assert_eq!((sent[0]["tool_choice"].clone(), sent[1].get("tool_choice").cloned()), (json!("required"), None));
        assert_eq!(sent[1]["straydog"]["reasoning"], false, "its thinking closed still: {}", sent[1]);
        assert!(!client.require_call_once(), "not asked so again");
    }

    #[test]
    fn the_prompts_reading_is_read_from_the_streams_comments() {
        assert_eq!(prefill_said(" prefill 4096/90724"), Some((4096, 90724)));
        assert_eq!(prefill_said("prefill 0/12"), Some((0, 12)));
        assert_eq!(prefill_said(" keep-alive"), None);
        assert_eq!(prefill_said(" prefill 4096"), None);
        assert_eq!(prefill_said(" prefill a/b"), None);
    }

    fn feed(events: &[&str]) -> (Completion, Vec<String>) {
        let mut assembly = Assembly::default();
        let mut seen = Vec::new();
        for event in events {
            assembly.event(event, &mut |delta| seen.push(match delta {
                Delta::Reasoning(text) => format!("r:{text}"),
                Delta::Content(text) => format!("c:{text}"),
                Delta::Tool(_, name) => format!("t:{name}"),
                Delta::Arguments(index, text) => format!("a{index}:{text}"),
                Delta::Prefill(done, total) => format!("p:{done}/{total}"),
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
        assert_eq!(seen, ["r:hm", "t:read_file", "a0:{\"pa", "a0:th\":1}"]);
        assert_eq!(completion.tool_calls[0].function.arguments, "{\"path\":1}");
        assert_eq!(completion.finish, "tool_calls");
        let usage = completion.usage.unwrap();
        assert_eq!((usage.prompt, usage.cached, usage.completion, usage.tokens_per_second), (7, 5, 3, Some(41.5)));
    }

    /// What Straydog sends of a call being written is told as it comes (the name once, the arguments in pieces), and the call that comes whole after is the call: its name, and only
    /// what the pieces had not told of its arguments, are told again. The pieces are in no call.
    #[test]
    fn a_call_straydog_writes_is_told_as_it_comes_and_its_whole_is_not_told_again() {
        let (completion, seen) = feed(&[
            r#"{"choices":[{"delta":{"reasoning_content":"hm"}}]}"#,
            r#"{"choices":[{"delta":{"straydog_tool_writing":[{"index":0,"name":"create_file","arguments":""},{"index":0,"arguments":"{\"path\": \"a"}]}}]}"#,
            r#"{"choices":[{"delta":{"straydog_tool_writing":[{"index":0,"arguments":"b.rs\", \"content\": \"x"}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"create_file","arguments":"{\"path\": \"ab.rs\", \"content\": \"xy\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert_eq!(seen, ["r:hm", "t:create_file", "a0:{\"path\": \"a", "a0:b.rs\", \"content\": \"x", "t:create_file", "a0:y\"}"]);
        assert_eq!(completion.tool_calls.len(), 1);
        assert_eq!(completion.tool_calls[0].function.arguments, r#"{"path": "ab.rs", "content": "xy"}"#);
        assert_eq!((completion.tool_calls[0].id.as_str(), completion.finish.as_str()), ("call_1", "tool_calls"));
        // Two calls: each one's own.
        let (completion, seen) = feed(&[
            r#"{"choices":[{"delta":{"straydog_tool_writing":[{"index":0,"name":"read_file","arguments":""},{"index":0,"arguments":"{\"path\": \"a\"}"},{"index":1,"name":"read_file","arguments":""},{"index":1,"arguments":"{\"path\": \"b"}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c0","function":{"name":"read_file","arguments":"{\"path\": \"a\"}"}},{"index":1,"id":"c1","function":{"name":"read_file","arguments":"{\"path\": \"b\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert_eq!(seen, ["t:read_file", "a0:{\"path\": \"a\"}", "t:read_file", "a1:{\"path\": \"b", "t:read_file", "t:read_file", "a1:\"}"]);
        assert_eq!(completion.tool_calls.iter().map(|call| call.function.arguments.as_str()).collect::<Vec<_>>(), [r#"{"path": "a"}"#, r#"{"path": "b"}"#]);
    }

    /// A server on this machine is one at a loopback address, or the app's own engine: pages that draw with WebGL are then drawn on the CPU.
    #[test]
    fn a_server_at_a_loopback_address_is_on_this_machine() {
        for here in ["http://127.0.0.1:8080", "http://localhost:8080/v1", "http://[::1]:8080", "http://127.0.0.2:1", "http://LOCALHOST", "http://engine.localhost:9"] {
            assert!(is_loopback(&Url::parse(here).unwrap()), "{here}");
        }
        for there in ["http://192.168.0.100:8080", "https://api.example.com", "http://localhost.example.com", "http://10.0.0.1"] {
            assert!(!is_loopback(&Url::parse(there).unwrap()), "{there}");
        }
    }

    /// The pieces are a partial string: a call that never comes whole (the server released the markup as text) is no call, a final call that is not what the pieces began is told
    /// in no part twice, and a piece that is not one is nothing and no error.
    #[test]
    fn pieces_make_no_call_and_a_piece_that_is_not_one_is_nothing() {
        let (completion, seen) = feed(&[
            r#"{"choices":[{"delta":{"straydog_tool_writing":[{"index":0,"name":"create_file","arguments":""},{"index":0,"arguments":"{\"path\": \"a"}]}}]}"#,
            r#"{"choices":[{"delta":{"content":"text after all"},"finish_reason":"stop"}]}"#,
        ]);
        assert_eq!(seen, ["t:create_file", "a0:{\"path\": \"a", "c:text after all"]);
        assert!(completion.tool_calls.is_empty());
        assert_eq!(completion.content, "text after all");

        let (completion, seen) = feed(&[
            r#"{"choices":[{"delta":{"straydog_tool_writing":[{"index":0,"name":"create_file","arguments":""},{"index":0,"arguments":"{\"path\": \"a"}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"create_file","arguments":"{\"file\": \"other\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert_eq!(seen, ["t:create_file", "a0:{\"path\": \"a", "t:create_file"], "what does not go on from the pieces is not told over them");
        assert_eq!(completion.tool_calls[0].function.arguments, r#"{"file": "other"}"#);

        // Not a list, not objects, no index, a place beyond the most, a name or arguments that are not text, empty ones: told as nothing, and the stream goes on.
        let (completion, seen) = feed(&[
            r#"{"choices":[{"delta":{"straydog_tool_writing":"x"}}]}"#,
            r#"{"choices":[{"delta":{"straydog_tool_writing":{"index":0,"name":"a"}}}]}"#,
            r#"{"choices":[{"delta":{"straydog_tool_writing":[1,null,"s",[],{},{"name":"a","arguments":"b"},{"index":"0","name":"a"},{"index":-1,"name":"a"},{"index":1000000000,"name":"a","arguments":"b"},{"index":0,"name":5,"arguments":7},{"index":0,"name":"","arguments":""}]}}]}"#,
            r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
        ]);
        assert_eq!(seen, ["c:ok"]);
        assert!(completion.tool_calls.is_empty());
        assert_eq!(completion.finish, "stop");
    }

    /// The request asks for the pieces only when the server said it sends them, and the request has tools; a request that does not ask is the request it always was.
    #[test]
    fn the_request_asks_for_a_calls_pieces_only_of_a_server_that_sends_them() {
        let tools = json!([{"type": "function", "function": {"name": "create_file", "parameters": {"type": "object", "properties": {}}}}]);
        let messages = [Message::text("user", "hi")];
        let asks = |client: &Client, tools: Option<&Value>| {
            let request = Request { model: "m", messages: &messages, tools, effort: None, images: None, japanese_letters: false, request_id: None };
            client.body_for(&request)
        };
        let mut client = Client::new("http://127.0.0.1:1/v1", None).unwrap();
        assert!(asks(&client, Some(&tools)).get("straydog").is_none(), "a server that has not said it can is not asked");
        client.tool_writing = true;
        assert_eq!(asks(&client, Some(&tools))["straydog"], json!({"tool_writing": true}));
        assert!(asks(&client, None).get("straydog").is_none(), "nothing to write without tools");
        assert_eq!(asks(&client, None), request_body("m", &messages, None, None));
        assert_eq!(asks(&Client { tool_writing: false, ..client }, Some(&tools)), request_body("m", &messages, Some(&tools), None));
    }

    /// `/props` lists what a server can do; one that does not list this is not asked.
    #[test]
    fn a_server_says_in_its_props_that_it_sends_a_calls_pieces() {
        assert!(described(r#"{"tool_writing": true, "suppress": ["not_cp932_han"]}"#).tool_writing);
        assert!(!described(r#"{"tool_writing": false}"#).tool_writing);
        assert!(!described(r#"{"tool_writing": "yes"}"#).tool_writing);
        assert!(!described("{}").tool_writing);
    }

    #[test]
    fn a_tool_result_holds_a_picture_only_for_a_model_that_is_not_said_to_read_none() {
        let mut client = Client::new("http://127.0.0.1:1/v1", None).unwrap();
        assert!(client.images_in_tool_results() && client.reads_pictures().is_none(), "a server that says nothing");
        client.vision = Some(true);
        assert!(client.images_in_tool_results());
        client.vision = Some(false);
        assert!(!client.images_in_tool_results() && client.reads_pictures() == Some(false));
        client.vision = None;
        client.replay_reasoning = false;
        assert!(!client.images_in_tool_results(), "a server that is not Straydog, as before");
    }

    /// A client of a server that answers `GET /props` with this, and anything else with `{}`.
    fn answering_props(props: &'static str) -> Client {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                std::thread::spawn(move || {
                    let mut request = [0u8; 4096];
                    let Ok(count) = stream.read(&mut request) else { return };
                    let body = if String::from_utf8_lossy(&request[..count]).starts_with("GET /props") { props } else { "{}" };
                    let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes());
                    while matches!(stream.read(&mut request), Ok(count) if count > 0) {}
                });
            }
        });
        Client::new(&format!("http://127.0.0.1:{port}/v1"), None).unwrap()
    }

    /// A session told when it connected that the model reads no pictures asks `/props` again, so that a server restarted with its vision
    /// encoder is seen without starting the app again; where nothing answers, or the server still says no, the no stands.
    #[test]
    fn a_session_told_no_pictures_asks_again_and_sees_a_server_that_reads_them_now() {
        let mut client = answering_props(r#"{"modalities": {"vision": true, "audio": false}}"#);
        client.vision = Some(false);
        assert_eq!(client.reads_pictures(), Some(true));
        assert!(client.images_in_tool_results());
        let mut silent = Client::new("http://127.0.0.1:1/v1", None).unwrap();
        silent.vision = Some(false);
        assert_eq!(silent.reads_pictures(), Some(false), "no answer");
        let mut still = answering_props(r#"{"modalities": {"vision": false, "audio": false}}"#);
        still.vision = Some(false);
        assert_eq!(still.reads_pictures(), Some(false), "the server still says no");
    }

    /// A client of a server that answers `GET /slots` with this, and anything else with `{}`.
    fn answering_slots(slots: &'static str) -> Client {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                std::thread::spawn(move || {
                    let mut request = [0u8; 4096];
                    let Ok(count) = stream.read(&mut request) else { return };
                    let body = if String::from_utf8_lossy(&request[..count]).starts_with("GET /slots") { slots } else { "{}" };
                    let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes());
                    while matches!(stream.read(&mut request), Ok(count) if count > 0) {}
                });
            }
        });
        Client::new(&format!("http://127.0.0.1:{port}/v1"), None).unwrap()
    }

    #[test]
    fn a_call_shows_as_it_is_written_from_straydog_that_says_so_and_from_openai() {
        let mut client = Client::new("http://127.0.0.1:1/v1", None).unwrap();
        assert!(!client.shows_calls_as_written(), "a Straydog that does not say tool_writing");
        client.tool_writing = true;
        assert!(client.shows_calls_as_written());
        client.replay_reasoning = false;
        client.tool_writing = false;
        assert!(!client.shows_calls_as_written(), "another server");
        let mut openai = Client::new("https://api.openai.com/v1", None).unwrap();
        openai.replay_reasoning = false;
        assert!(openai.shows_calls_as_written());
    }

    #[test]
    fn a_server_is_idle_only_where_it_says_that_no_slot_is_processing() {
        assert!(answering_slots(r#"[{"id": 0, "is_processing": false}]"#).idle());
        assert!(!answering_slots(r#"[{"id": 0, "is_processing": true}]"#).idle());
        assert!(!answering_slots(r#"[{"id": 0, "is_processing": false}, {"id": 1, "is_processing": true}]"#).idle(), "any slot at work");
        assert!(!answering_slots(r#"[{"id": 0}]"#).idle(), "a slot that does not say");
        assert!(!answering_slots(r#"{"error": "no"}"#).idle(), "an answer it cannot read");
        assert!(!Client::new("http://127.0.0.1:1/v1", None).unwrap().idle(), "no answer at all");
    }

    /// Asked once to close the thinking, Straydog's next request has `straydog.reasoning` false, beside what else it asks, and the one after it does not.
    /// A service's never has the field.
    #[test]
    fn a_thinking_closed_once_is_the_next_requests_alone() {
        let tools = json!([{"type": "function", "function": {"name": "read_file", "parameters": {"type": "object", "properties": {}}}}]);
        let messages = [Message::text("user", "write it")];
        let request = Request { model: "m", messages: &messages, tools: Some(&tools), effort: None, images: None, japanese_letters: false, request_id: None };
        let mut client = Client::new("http://127.0.0.1:1/v1", None).unwrap();
        client.tool_writing = true;
        assert!(client.close_thinking_once());
        assert_eq!(client.body_for(&request)["straydog"], json!({"tool_writing": true, "reasoning": false}));
        assert_eq!(client.body_for(&request)["straydog"], json!({"tool_writing": true}));
        client.replay_reasoning = false;
        assert!(!client.close_thinking_once());
        assert!(client.body_for(&request)["straydog"].get("reasoning").is_none());
    }

    #[test]
    fn a_warm_up_asks_straydog_to_keep_only_its_system_prompts_checkpoint() {
        let tools = json!([{"type": "function", "function": {"name": "read_file", "parameters": {"type": "object", "properties": {}}}}]);
        let messages = [Message::text("system", "s"), Message::text("user", ".")];
        let request = Request { model: "m", messages: &messages, tools: Some(&tools), effort: None, images: None, japanese_letters: false, request_id: None };
        let mut client = Client::new("http://127.0.0.1:1/v1", None).unwrap();
        assert_eq!(client.warm_body(&request)["straydog"], json!({"checkpoint": false}));
        client.tool_writing = true;
        assert_eq!(client.warm_body(&request)["straydog"], json!({"tool_writing": true, "checkpoint": false}));
        assert!(client.body_for(&request)["straydog"].get("checkpoint").is_none(), "a request of the conversation keeps its checkpoints");
        client.replay_reasoning = false;
        client.tool_writing = false;
        assert!(client.warm_body(&request).get("straydog").is_none(), "only Straydog takes the field");
    }

    /// The head of the request a server is sent by `stream`, as the server reads it, lower-cased.
    fn head_seen_by_a_server(request_id: Option<&str>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (told, heard) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (mut request, mut block) = (Vec::new(), [0u8; 65536]);
            let head = loop {
                let count = stream.read(&mut block).unwrap();
                request.extend_from_slice(&block[..count]);
                let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                let head = String::from_utf8_lossy(&request[..at]).to_lowercase();
                let length: usize = head.lines().find_map(|line| line.strip_prefix("content-length: ")?.parse().ok()).unwrap_or(0);
                if request.len() >= at + 4 + length {
                    break head;
                }
            };
            told.send(head).unwrap();
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"a\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes());
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            while matches!(stream.read(&mut block), Ok(count) if count > 0) {}
        });
        let client = Client::new(&format!("http://127.0.0.1:{port}/v1"), Some("key".into())).unwrap();
        let messages = [Message::text("user", "hi")];
        let request = Request { model: "m", messages: &messages, tools: None, effort: None, images: None, japanese_letters: false, request_id };
        assert_eq!(client.stream(&request, &AtomicBool::new(false), &mut |_| {}).unwrap().content, "a");
        heard.recv_timeout(Duration::from_secs(5)).unwrap()
    }

    /// A server that answers each request by `answers` in turn (a status and a body), and tells each request's body as it read it.
    fn answering_in_turn(answers: Vec<(u16, String)>) -> (Client, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (told, heard) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for (status, answer) in answers {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let (mut request, mut block) = (Vec::new(), [0u8; 65536]);
                let body = loop {
                    let count = stream.read(&mut block).unwrap();
                    request.extend_from_slice(&block[..count]);
                    let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&request[..at]).to_lowercase();
                    let length: usize = head.lines().find_map(|line| line.strip_prefix("content-length: ")?.parse().ok()).unwrap_or(0);
                    if request.len() >= at + 4 + length {
                        break String::from_utf8_lossy(&request[at + 4..at + 4 + length]).into_owned();
                    }
                };
                told.send(body).unwrap();
                let kind = if status == 200 { "text/event-stream" } else { "application/json" };
                let reason = if status == 200 { "OK" } else { "Bad Request" };
                let _ = stream.write_all(format!("HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\r\n{answer}", answer.len()).as_bytes());
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                while matches!(stream.read(&mut block), Ok(count) if count > 0) {}
            }
        });
        (Client::new(&format!("http://127.0.0.1:{port}/v1"), None).unwrap(), heard)
    }

    /// The app's facts and notes go in the role a server is known to take them in (`latest_reminder` after the person's message or, with the switch, before
    /// it; `system`; `developer` for OpenAI's API), and as the user's to any other or once it refused them; the person's own words stay the user's.
    #[test]
    fn the_apps_own_messages_go_in_a_role_of_their_own() {
        let facts = format!("{} The time: 03:00.", crate::prompt::APP_FACTS);
        let messages = [Message::text("system", "s"), Message::text("user", "make a page"), Message::text("user", &facts), Message::text("assistant", "ok"), Message::text("user", crate::prompt::CHECK_NUDGE)];
        let roles = |reminders: Reminders| -> Vec<String> {
            request_body_held("m", &messages, None, None, None, None, reminders)["messages"].as_array().unwrap().iter().map(|message| format!("{}:{}", message["role"].as_str().unwrap(), message["content"].as_str().unwrap().chars().take(4).collect::<String>())).collect()
        };
        assert_eq!(roles(Reminders::AS_USER), ["system:s", "user:make", "user:[Fro", "assistant:ok", "user:[Fro"]);
        // (latest_reminder only for the facts right after the person's message: a note after an answer stays the user's, as the template opens no answer after it)
        assert_eq!(roles(Reminders { role: Some(REMINDER_ROLE), before: false }), ["system:s", "user:make", "latest_reminder:[Fro", "assistant:ok", "user:[Fro"]);
        assert_eq!(roles(Reminders { role: Some(REMINDER_ROLE), before: true }), ["system:s", "latest_reminder:[Fro", "user:make", "assistant:ok", "user:[Fro"]);
        assert_eq!(roles(Reminders { role: Some(APP_ROLE), before: false }), ["system:s", "user:make", "system:[Fro", "assistant:ok", "system:[Fro"]);
        // (the person's own words that only look like it stay theirs: a message of theirs never begins so, as it is what they typed)
        let client = Client::new("http://127.0.0.1:1/v1", None).unwrap();
        assert_eq!(client.reminders().role, None, "a server whose templates are not known");
        assert_eq!(Client::new("https://api.openai.com/v1", None).unwrap().reminders().role, Some(OPENAI_APP_ROLE));
        assert_eq!(Client { app_role: Some(REMINDER_ROLE), ..client.clone() }.reminders().role, Some(REMINDER_ROLE));
        client.app_role_refused.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(Client { app_role: Some(REMINDER_ROLE), ..client }.reminders().role, None);
    }

    /// A server that keeps the pictures it was sent (`image_digests`) is sent a conversation's picture once, and then its digest; one that says it no longer
    /// holds it (`image_not_held`, nothing run) has the same request again with the bytes, and the digest again after that. Another conversation sends its bytes.
    /// A server that does not say it keeps them is never sent a digest.
    #[test]
    fn a_picture_sent_once_goes_by_its_digest_and_again_with_its_bytes_when_the_server_no_longer_holds_it() {
        let dir = crate::in_container::tempdir().unwrap();
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(4, 4).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        std::fs::write(dir.path().join("a.png"), &png).unwrap();
        let digest = images::digest(&png);
        let messages = [Message { images: vec![crate::message::ImageRef { file: "a.png".into(), media_type: "image/png".into(), width: 4, height: 4 }], ..Message::text("user", "what is this")}];
        let ok = "data: {\"choices\":[{\"delta\":{\"content\":\"a\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_string();
        let gone = r#"{"error": {"message": "Straydog does not hold 1 of the images named by digest; send their bytes", "type": "image_not_held"}}"#.to_string();
        let (mut client, heard) = answering_in_turn(vec![(200, ok.clone()), (400, gone), (200, ok.clone()), (200, ok.clone()), (200, ok.clone())]);
        client.image_digests = true;
        let ask = |client: &Client, id: &str| {
            let request = Request { model: "m", messages: &messages, tools: None, effort: None, images: Some(dir.path()), japanese_letters: false, request_id: Some(id) };
            client.stream(&request, &AtomicBool::new(false), &mut |_| {}).map(|done| done.content)
        };
        let by_digest = format!("\"sha256:{digest}\"");
        let bytes = "\"data:image/png;base64,";
        assert_eq!(ask(&client, "talk/1").unwrap(), "a");
        assert!(heard.recv().unwrap().contains(bytes), "the first time, its bytes");
        assert_eq!(ask(&client, "talk/2").unwrap(), "a", "refused for a picture not held, and then sent again");
        let (named, again) = (heard.recv().unwrap(), heard.recv().unwrap());
        assert!(named.contains(&by_digest) && !named.contains(bytes), "{named}");
        assert!(again.contains(bytes) && !again.contains("sha256:"), "{again}");
        assert_eq!(ask(&client, "talk/3").unwrap(), "a");
        assert!(heard.recv().unwrap().contains(&by_digest), "held again once its bytes went");
        assert_eq!(ask(&client, "other/1").unwrap(), "a");
        assert!(heard.recv().unwrap().contains(bytes), "another conversation has sent nothing yet");
        // (a server that does not say so)
        let (plain, heard) = answering_in_turn(vec![(200, ok.clone()), (200, ok)]);
        for id in ["talk/1", "talk/2"] {
            assert_eq!(ask(&plain, id).unwrap(), "a");
            assert!(!heard.recv().unwrap().contains("sha256:"));
        }
    }

    #[test]
    fn a_request_is_named_to_the_server_in_a_header_and_unnamed_has_none() {
        let head = head_seen_by_a_server(Some("20261105-120000/3"));
        assert!(head.lines().any(|line| line == "x-request-id: 20261105-120000/3"), "{head}");
        assert!(head.contains("authorization: bearer key"), "what a request had it still has: {head}");
        assert!(!head_seen_by_a_server(None).contains("x-request-id"));
    }

    #[test]
    fn the_name_sent_is_printable_ascii_with_no_space_and_at_most_sixty_four_long() {
        assert_eq!(request_id_header("20261105-120000/12"), Some(("X-Request-Id".to_string(), "20261105-120000/12".to_string())));
        assert_eq!(request_id_header("a b\r\nc\t\u{7f}d\u{3042}e").unwrap().1, "abcde", "no space, no control, no letter outside ASCII");
        let long = request_id_header(&format!("{}/1", "s".repeat(100))).unwrap().1;
        assert_eq!(long.len(), 64);
        assert!(long.bytes().all(|byte| (0x21..=0x7e).contains(&byte)));
        assert_eq!(request_id_header(" \u{3042}"), None);
    }

    #[test]
    fn what_the_server_says_of_its_own_work_is_kept_as_it_came() {
        let (completion, _) = feed(&[
            r#"{"choices":[{"delta":{"content":"a"},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3},"timings":{"ttft_seconds":1.5,"resumed_from":"live","prefill_experts":{"vram":4}}}"#,
        ]);
        assert_eq!(completion.timings, Some(json!({"ttft_seconds": 1.5, "resumed_from": "live", "prefill_experts": {"vram": 4}})));
        let (none, _) = feed(&[r#"{"choices":[{"delta":{"content":"a"},"finish_reason":"stop"}]}"#]);
        assert_eq!(none.timings, None);
    }

    #[test]
    fn a_call_is_whole_however_a_server_spells_its_pieces() {
        let (completion, _) = feed(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read_file","arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"","function":{"name":"read_file","arguments":"th\":1}"}}]}}]}"#,
        ]);
        assert_eq!((completion.tool_calls[0].id.as_str(), completion.tool_calls[0].function.name.as_str()), ("a", "read_file"));
        assert_eq!(completion.tool_calls[0].function.arguments, "{\"path\":1}");
        // No id at all, and an index that skips one.
        let (completion, _) = feed(&[r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"name":"list_files","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#]);
        assert_eq!(completion.tool_calls.len(), 1);
        assert_eq!((completion.tool_calls[0].id.as_str(), completion.tool_calls[0].function.name.as_str()), ("call-0", "list_files"));
    }

    #[test]
    fn arguments_a_server_could_not_read_back_are_not_sent() {
        let call = |arguments: &str| Message { role: "assistant".into(), tool_calls: Some(vec![ToolCall { id: "a".into(), kind: "function".into(), function: crate::message::FunctionCall { name: "t".into(), arguments: arguments.into() } }]), ..Message::default() };
        let sent = |arguments: &str| request_body("m", &[call(arguments)], None, None)["messages"][0]["tool_calls"][0]["function"]["arguments"].as_str().unwrap().to_string();
        assert_eq!(sent("{\"path\": \"a"), "{}");
        assert_eq!(sent("[1]"), "{}");
        assert_eq!(sent("{\"b\": 1,  \"a\": 2}"), "{\"b\": 1,  \"a\": 2}", "what can be read goes as it came, to the byte");
        assert_eq!(sent(""), "");
    }

    #[test]
    fn a_message_without_pictures_goes_as_it_always_did() {
        let messages = [Message::text("system", "s"), Message { reasoning_content: Some("r".into()), ..Message::text("assistant", "a") }, Message::tool_result("1", "t")];
        let plain: Vec<Message> = messages.to_vec();
        let before = json!({"model": "m", "messages": plain, "stream": true, "stream_options": {"include_usage": true}});
        assert_eq!(request_body("m", &messages, None, None).to_string(), before.to_string());
    }

    #[test]
    fn a_picture_goes_as_parts_and_the_same_bytes_each_time() {
        let dir = crate::in_container::tempdir().unwrap();
        let session = dir.path().join("s.jsonl");
        let bytes = images::tests::encoded(8, 8, image::ImageFormat::Png);
        let kept = images::save(&session, &images::attach_bytes("a", bytes.clone()).unwrap(), ImageLimit::WIDE).unwrap();
        let messages = [Message { images: vec![kept], ..Message::text("user", "what") }];
        let folder = images::folder_of(&session);
        let body = request_body_in("m", &messages, None, None, Some(&folder));
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["image_url"]["url"], format!("data:image/png;base64,{}", images::base64(&bytes)));
        assert_eq!(content[1]["text"], "what");
        assert!(body["messages"][0].get("images").is_none(), "that is the session's word, not the API's");
        assert_eq!(request_body_in("m", &messages, None, None, Some(&folder)).to_string(), body.to_string());
    }

    #[test]
    fn a_model_without_vision_is_told_from_a_full_context_and_from_a_failure() {
        let refused = |message: &str| classify(400, &json!({"error": {"message": message, "type": "invalid_request_error"}}).to_string());
        for words in ["this model reads no images", "this model reads no images: its vision encoder file is not beside it (`straydog fetch` builds it)", "Invalid content type. image_url is only supported by certain models.", "image input is not supported - hint: if this is unexpected, you may need to provide the mmproj"] {
            assert!(matches!(refused(words), LlmError::NoVision(_)), "{words}");
        }
        assert_eq!(sentence_of(&refused("this model reads no images").to_string()), NO_VISION);
        assert!(matches!(stream_error(json!({"error": {"code": 400, "message": "this model reads no images", "type": "invalid_request_error"}})), LlmError::NoVision(_)));
        assert!(matches!(refused("the prompt exceeds the context of 1024"), LlmError::Overflow(_)));
        assert!(matches!(refused("tool_choice must be auto or none"), LlmError::Rejected(_)));
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
        assert!(matches!(classify(500, "worker.rs:579 model.generate: not enough host memory for the K/V of this sequence"), LlmError::Overflow(_)));
        assert!(matches!(classify(400, "tool_choice must be auto or none"), LlmError::Rejected(_)));
        assert!(matches!(classify(503, "loading"), LlmError::Transient(_)));
    }

    /// As Straydog sends it: `event: error`, then `data: {"error": {...}}`.
    fn stream_error(body: Value) -> LlmError {
        let mut assembly = Assembly::default();
        assembly.event(&body.to_string(), &mut |_| {}).unwrap_err()
    }

    #[test]
    fn a_server_that_failed_is_told_from_a_full_context_whatever_its_words() {
        let fatal = |message: &str| json!({"error": {"code": 500, "message": message, "type": "server_error"}});
        for words in ["worker.rs:579 model.generate: not enough host memory for the K/V of this sequence", "the sequence exceeds the allocated context", "GPU fault"] {
            assert!(matches!(stream_error(fatal(words)), LlmError::Down(_)), "{words}");
        }
        assert!(matches!(stream_error(json!({"error": {"message": "context exceeded"}, "fatal": true})), LlmError::Down(_)));
        // What the engine says of a failure as it goes down, to anyone who asks.
        let closing = r#"{"error": {"code": 503, "message": "Straydog's worker exited (exit code: 1); restart the server", "type": "unavailable_error"}}"#;
        assert!(matches!(classify(503, closing), LlmError::Down(_)));
        // The same words from a server that goes on are what they say.
        let refused = |message: &str| json!({"error": {"code": 400, "message": message, "type": "invalid_request_error"}});
        assert!(matches!(stream_error(refused("not enough host memory for the K/V of this sequence")), LlmError::Overflow(_)));
        assert!(matches!(stream_error(refused("the prompt exceeds the allocated context")), LlmError::Overflow(_)));
        assert!(matches!(classify(500, r#"{"error": {"message": "the context was exceeded", "type": "server_error"}}"#), LlmError::Transient(_)));
    }

    #[test]
    fn what_a_person_reads_names_no_status_and_no_json() {
        // (the server's own line, if it has said one, is under the app's sentence: `what_the_server_said_goes_under_the_apps_sentence` has them)
        let shown = |error: LlmError| sentence_of(&error.to_string()).to_string();
        assert_eq!(shown(stream_error(json!({"error": {"code": 500, "message": "GPU: out of memory", "type": "server_error"}}))), SERVER_OUT_OF_MEMORY);
        assert_eq!(shown(classify(401, r#"{"error": {"message": "bad key"}}"#)), KEY_REFUSED);
        assert_eq!(shown(classify(503, r#"{"error": {"message": "Loading model"}}"#)), SERVER_BUSY);
        assert_eq!(shown(classify(400, r#"{"error": {"message": "no such tool"}}"#)), format!("{REFUSED}no such tool"));
        assert_eq!(shown(classify(502, "<html>bad gateway</html>")), format!("{SERVER_ERROR}(no details)"));
        assert_eq!(shown(LlmError::from(Failure::Connect("127.0.0.1:1: refused (os error 10061)".into()))), SERVER_GONE);
        assert_eq!(shown(LlmError::from(Failure::Stream("reset (os error 10054)".into()))), SERVER_GONE);
    }

    /// A server that answers `/props` with this and anything else with no model. Each connection is answered on a thread of its own: on this machine's loopback a client's close sometimes
    /// never reaches the server, and a server that waited for it before taking the next connection would never answer the next question.
    fn described(props: &'static str) -> ServerInfo {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                std::thread::spawn(move || {
                    let mut request = [0u8; 4096];
                    let Ok(count) = stream.read(&mut request) else { return };
                    let body = if String::from_utf8_lossy(&request[..count]).starts_with("GET /props") { props } else { "{}" };
                    let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes());
                    while matches!(stream.read(&mut request), Ok(count) if count > 0) {}
                });
            }
        });
        Client::new(&format!("http://127.0.0.1:{port}/v1"), None).unwrap().describe().unwrap()
    }

    /// What `describe` makes of a server that answers everything with this. Each connection is answered on a thread of its own (see `described`): the server never waits for one client's
    /// close to take the next question.
    fn described_at(status: &'static str, body: &'static str) -> Result<ServerInfo, String> {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                std::thread::spawn(move || {
                    let mut request = [0u8; 4096];
                    let _ = stream.read(&mut request);
                    let _ = stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes());
                    // (closed by the client: closed here, what it had not read yet could be lost)
                    while matches!(stream.read(&mut request), Ok(count) if count > 0) {}
                });
            }
        });
        Client::new(&format!("http://127.0.0.1:{port}/v1"), None).unwrap().describe()
    }

    /// What `health` makes of a server that answers everything with this (see `described_at`).
    fn health_at(status: &'static str, body: &'static str) -> Health {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                std::thread::spawn(move || {
                    let mut request = [0u8; 4096];
                    let _ = stream.read(&mut request);
                    let _ = stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes());
                    while matches!(stream.read(&mut request), Ok(count) if count > 0) {}
                });
            }
        });
        Client::new(&format!("http://127.0.0.1:{port}/v1"), None).unwrap().health().unwrap()
    }

    /// Each of the app's sentences that comes from a server's answer keeps its words and has the server's own line under it, in English as it came.
    #[test]
    fn what_the_server_said_goes_under_the_apps_sentence() {
        let under = |error: LlmError, sentence: &str, line: &str| {
            let text = error.to_string();
            assert_eq!((sentence_of(&text), line_under(&text)), (sentence, Some(line)), "{text}");
            assert_eq!(text, format!("{sentence}\n{line}"));
        };
        let worker = "Straydog's worker exited (exit code 0xC0000017 STATUS_NO_MEMORY: out of memory); its last lines: memory allocation of 1073741824 bytes failed; restart the server";
        under(stream_error(json!({"error": {"code": 500, "message": worker, "type": "server_error"}})), SERVER_OUT_OF_MEMORY, worker);
        under(classify(503, &json!({"error": {"code": 503, "message": "Straydog's worker exited (exit code: 1); restart the server", "type": "unavailable_error"}}).to_string()), SERVER_GONE, "Straydog's worker exited (exit code: 1); restart the server");
        under(stream_error(json!({"error": {"code": 500, "message": "GPU fault", "type": "server_error"}})), SERVER_GONE, "GPU fault");
        under(classify(500, "worker.rs:579 model.generate: not enough host memory for the K/V of this sequence"), MEMORY_FULL, "worker.rs:579 model.generate: not enough host memory for the K/V of this sequence");
        under(classify(400, "the prompt exceeds the context of 1024"), CONTEXT_FULL, "the prompt exceeds the context of 1024");
        under(classify(503, r#"{"error": {"message": "Loading model"}}"#), SERVER_BUSY, "Loading model");
        under(classify(429, r#"{"error": {"message": "Rate limit reached for requests", "type": "requests"}}"#), SERVER_BUSY, "Rate limit reached for requests");
        under(classify(401, r#"{"error": {"message": "bad key"}}"#), KEY_REFUSED, "bad key");
        under(classify(403, r#"{"error": "forbidden"}"#), KEY_REFUSED, "forbidden");
        under(classify(404, r#"{"error": {"message": "model 'x' not found"}}"#), NOT_FOUND, "model 'x' not found");
        under(classify(400, r#"{"error": {"message": "this model reads no images"}}"#), NO_VISION, "this model reads no images");
        // (a message of a few lines gives its first)
        under(classify(404, &json!({"error": {"message": "no such model\nlist: a, b"}}).to_string()), NOT_FOUND, "no such model");
        // What the load of an engine says, in `/health`.
        let Health::Failed(text) = health_at("500 Internal Server Error", r#"{"status": "error", "error": {"message": "Straydog's worker exited (exit code 0xC0000017 STATUS_NO_MEMORY: out of memory)", "type": "server_error"}}"#) else { panic!("failed") };
        assert_eq!((sentence_of(&text), line_under(&text)), (START_OUT_OF_MEMORY, Some("Straydog's worker exited (exit code 0xC0000017 STATUS_NO_MEMORY: out of memory)")));
        let Health::Failed(text) = health_at("500 Internal Server Error", r#"{"status": "error", "error": {"message": "the GPU stopped responding", "type": "server_error"}}"#) else { panic!("failed") };
        assert_eq!((sentence_of(&text), line_under(&text)), (START_FAILED, Some("the GPU stopped responding")));
        // ...and with nothing in it to show, only the sentence.
        assert_eq!(health_at("500 Internal Server Error", r#"{"status": "error"}"#), Health::Failed(START_FAILED.to_string()));
        // The class of a failure and the words that tell a gone server do not change with the line.
        assert!(is_gone(&format!("{SERVER_GONE}\nsomething")) && is_gone(&format!("{SERVER_OUT_OF_MEMORY}\nsomething")) && !is_gone(&format!("{KEY_REFUSED}\nsomething")));
        assert_eq!(crate::agent::failure_kind(&format!("{SERVER_GONE}\nsomething")), "server_gone");
        assert_eq!(crate::agent::failure_kind(&format!("{KEY_REFUSED}\nsomething")), "refused");
        assert_eq!(crate::agent::failure_kind(&format!("{CONTEXT_FULL}\nsomething")), "context_full");
    }

    /// The same words for the engine this app starts keep the line.
    #[test]
    fn the_engines_words_keep_the_line_under_them() {
        let text = format!("{SERVER_OUT_OF_MEMORY}\nGPU: out of memory");
        assert_eq!(for_the_engine(&text), format!("{ENGINE_OUT_OF_MEMORY}\nGPU: out of memory"));
        assert_eq!(for_the_engine(&format!("{SERVER_GONE}\nGPU fault")), format!("{ENGINE_GONE}\nGPU fault"));
        assert_eq!(for_the_engine(SERVER_GONE), ENGINE_GONE);
        assert_eq!(for_the_engine(KEY_REFUSED), KEY_REFUSED);
        assert_eq!(gone_with_line("the engine ended (exit code: 3)"), format!("{SERVER_GONE}\nthe engine ended (exit code: 3)"));
        assert_eq!(gone_with_line(""), SERVER_GONE);
        // (text that is not the app's sentence with a line is all sentence, whatever lines it has)
        assert_eq!(split("one\ntwo"), ("one\ntwo", None));
        assert_eq!(split(SERVER_GONE), (SERVER_GONE, None));
        let no_line = format!("{SERVER_GONE}\n");
        assert_eq!(split(&no_line), (no_line.as_str(), None), "no line, no split");
    }

    /// A web page is not the server's: nothing of it is shown, and the sentence stands alone.
    #[test]
    fn a_web_page_gives_no_line() {
        let page = "<html><head><title>502 Bad Gateway</title></head><body>nginx</body></html>";
        for (status, sentence) in [(503, SERVER_BUSY), (429, SERVER_BUSY), (401, KEY_REFUSED), (403, KEY_REFUSED), (404, NOT_FOUND)] {
            assert_eq!(classify(status, page).to_string(), sentence, "{status}");
        }
        assert_eq!(classify(0, &json!({"code": 500, "message": page, "type": "server_error"}).to_string()).to_string(), SERVER_GONE);
        assert_eq!(classify(500, page).to_string(), format!("{SERVER_ERROR}(no details)"));
        assert_eq!(line_of(page), None);
        assert_eq!(line_of("  \n\n "), None);
        assert_eq!(line_of(""), None);
        assert_eq!(health_at("502 Bad Gateway", page), Health::Failed(START_FAILED.to_string()));
    }

    /// A line is cut to 200 characters, and what names a cause is often at its end.
    #[test]
    fn a_long_line_is_cut_and_keeps_its_end() {
        let worker = format!("Straydog's worker exited (exit code 0xC0000017 STATUS_NO_MEMORY: out of memory); its last lines: {} | memory allocation of 1073741824 bytes failed; restart the server", "a line the worker wrote before it stopped | ".repeat(20));
        let line = line_of(&worker).unwrap();
        assert_eq!(line.chars().count(), LINE_MOST, "{line}");
        assert!(line.starts_with("Straydog's worker exited (exit code 0xC0000017 STATUS_NO_MEMORY") && line.ends_with("memory allocation of 1073741824 bytes failed; restart the server"), "{line}");
        assert!(line.contains('…'));
        let exactly = "x".repeat(LINE_MOST);
        assert_eq!(line_of(&exactly).unwrap(), exactly, "a line that fits is whole");
        assert_eq!(line_of(&"x".repeat(LINE_MOST + 1)).unwrap().chars().count(), LINE_MOST);
        // (by characters, not bytes: a Japanese line is cut where a character ends)
        let japanese = "メモリ不足".repeat(100);
        assert_eq!(line_of(&japanese).unwrap().chars().count(), LINE_MOST);
        let shown = classify(0, &json!({"code": 500, "message": worker, "type": "server_error"}).to_string()).to_string();
        assert_eq!(line_under(&shown), Some(line.as_str()));
    }

    /// A key, a header, a token never reaches the line: not as it opens, not half hidden by the server, not as the key this request was sent with.
    #[test]
    fn a_key_or_a_header_in_what_the_server_said_is_hidden() {
        let key = "sk-abcdefghijklmnopqrstuvwxyz123456";
        let sayings = [
            format!("Incorrect API key provided: {key}. You can find your API key at the dashboard."),
            "Incorrect API key provided: sk-proj-********************abcd.".to_string(),
            format!("invalid request: Authorization: Bearer {key}"),
            format!("bad header {{\"authorization\": \"Bearer {key}\"}}"),
            format!("x-api-key: {key} was refused"),
            format!("Bearer {key}"),
            "the api key is Zq81xKd0Lm".to_string(),
            "password=hunter2hunter2".to_string(),
            "token: ghp_abcdefghijklmnopqrstuvwxyz0123456789".to_string(),
            "cookie: session=abcdef123456; other=1".to_string(),
            "a long one AbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCdEf ends here".to_string(),
        ];
        for said in &sayings {
            for error in [classify(401, &json!({"error": {"message": said}}).to_string()), classify(0, &json!({"code": 500, "message": said, "type": "server_error"}).to_string()), classify(500, said)] {
                let shown = error.to_string();
                for secret in [key, "sk-proj-*****", "hunter2hunter2", "Zq81xKd0Lm", "ghp_abcdef", "session=abcdef123456", "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCdEf", "abcdefghijklmnopqrstuvwxyz123456"] {
                    assert!(!shown.contains(secret), "{secret} in {shown}");
                }
                assert!(shown.contains(safety::HIDDEN), "{shown}");
            }
        }
        // What stays: the words around what was hidden.
        let shown = classify(401, &json!({"error": {"message": format!("Incorrect API key provided: {key}. You can find your API key at the dashboard.")}}).to_string()).to_string();
        assert_eq!(line_under(&shown), Some("Incorrect API key provided: [hidden]. You can find your API key at the dashboard."));
        // The key this request was sent with, whatever its shape: a server may say back the whole of it.
        let odd = "correct horse battery staple";
        let client = Client::new("http://127.0.0.1:1/v1", Some(odd.to_string())).unwrap();
        let said = client.without_key(classify(401, &json!({"error": {"message": format!("unknown key {odd}")}}).to_string())).to_string();
        assert_eq!(line_under(&said), Some("unknown key [hidden]"));
        assert!(!said.contains(odd));
        // (a key too short to be told from the words is left)
        assert_eq!(LlmError::Rejected("abc".into()).without("abc").to_string(), "abc");
    }

    #[test]
    fn a_bad_key_or_a_wrong_address_is_not_called_ready() {
        assert_eq!(described_at("401 Unauthorized", r#"{"error": "no"}"#).unwrap_err(), KEY_REFUSED);
        assert_eq!(described_at("403 Forbidden", "{}").unwrap_err(), KEY_REFUSED);
        assert_eq!(described_at("404 Not Found", "nothing here").unwrap_err(), NOT_FOUND);
        assert_eq!(described_at("200 OK", "<html>a web page</html>").unwrap_err(), NOT_FOUND, "something answers, but no API");
        assert!(described_at("200 OK", r#"{"data": [{"id": "m"}]}"#).is_ok());
    }

    #[test]
    fn a_server_says_whether_its_model_reads_pictures_if_it_says_at_all() {
        assert_eq!(described(r#"{"modalities": {"vision": true, "audio": false}}"#).vision, Some(true));
        assert_eq!(described(r#"{"modalities": {"vision": false, "audio": false}}"#).vision, Some(false));
        assert_eq!(described("{}").vision, None);
    }

    /// A server that takes every connection and answers none, holding each open as a hung server does; how many it took is counted.
    fn takes_connections_and_says_nothing() -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let taken = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = taken.clone();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept() {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                held.push(stream);
            }
        });
        (port, taken)
    }

    #[test]
    fn a_server_that_takes_the_connection_and_says_nothing_is_given_up_on_and_not_asked_again() {
        use std::sync::atomic::Ordering;
        use std::time::Instant;
        let (port, taken) = takes_connections_and_says_nothing();
        let client = Client::new(&format!("http://127.0.0.1:{port}/v1"), None).unwrap();
        let (within, words) = (Duration::from_millis(400), format!("{NO_ANSWER}127.0.0.1:{port}"));
        let started = Instant::now();
        assert_eq!(client.health_within(within).unwrap_err(), words);
        assert!((within..Duration::from_secs(10)).contains(&started.elapsed()), "gave up at its deadline: {:?}", started.elapsed());
        assert_eq!(taken.load(Ordering::SeqCst), 1, "one connection, and no second ask");
        let started = Instant::now();
        assert_eq!(client.describe_within(within).unwrap_err(), words);
        assert!((within..Duration::from_secs(10)).contains(&started.elapsed()), "gave up at its deadline: {:?}", started.elapsed());
        assert_eq!(taken.load(Ordering::SeqCst), 2, "its first question ended it: the second was not asked");
    }

    /// The app's own engine: nothing listening there is said at once. A person's own server on this machine is asked again, as any server is: it may be on its way back.
    #[test]
    fn no_listener_here_is_no_use_asking_again_and_says_nothing_of_the_socket() {
        let (_reserved, port) = crate::test_net::bound_loopback(0);
        let client = Client { own_engine: true, ..Client::new(&format!("http://127.0.0.1:{port}/v1"), None).unwrap() };
        let cancel = AtomicBool::new(false);
        let request = Request { model: "m", messages: &[], tools: None, effort: None, images: None, japanese_letters: false, request_id: None };
        assert!(matches!(client.stream(&request, &cancel, &mut |_| {}), Err(LlmError::Down(_))));
        let persons = Client::new(&format!("http://127.0.0.1:{port}/v1"), None).unwrap();
        assert!(matches!(persons.stream(&request, &cancel, &mut |_| {}), Err(LlmError::Transient(_))));
        assert!(!client.answers());
        let said = client.describe().unwrap_err();
        assert!(said.starts_with(NO_ANSWER) && !said.contains("os error"), "{said}");
    }
}
