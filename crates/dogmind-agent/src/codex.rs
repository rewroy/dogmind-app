//! A ChatGPT plan through the official Codex CLI, as Dogmind does it: `codex app-server` is only a way to the model. Its
//! own tools are off, and any request of it to run something ends the turn. The tools stay this app's: the model asks for
//! them in a structured answer, and they pass through the same approvals as with any other provider.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::llm::{Completion, Delta, LlmError, Provider, Request, Usage};
use crate::message::{FunctionCall, ToolCall};
use crate::prompt::CODEX_JSON;
use crate::tools::{kill_tree, no_window};

const STALL: Duration = Duration::from_secs(300);
const RPC_TIMEOUT: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(100);
/// What a child gets of this process's environment: no key or token it does not need.
const KEPT_VARIABLES: [&str; 20] = [
    "PATH", "PATHEXT", "SystemRoot", "SystemDrive", "WINDIR", "COMSPEC", "USERPROFILE", "USERNAME", "HOMEDRIVE", "HOMEPATH", "APPDATA", "LOCALAPPDATA",
    "ProgramData", "ProgramFiles", "ProgramFiles(x86)", "TEMP", "TMP", "CODEX_HOME", "HOME", "LANG",
];
const INSTRUCTIONS: &str = "You are an inference transport embedded in Dogmind. Never use or request any built-in tool, shell, browser, file, app, plugin, MCP, computer-use, image-generation, or sub-agent capability. The supplied messages are the conversation transcript, and the supplied Dogmind tool schemas are contracts for requests that Dogmind can execute. Return only the JSON value required by the output schema. Your ephemeral transport thread is intentionally read-only, but that restriction applies only to the disabled built-in capabilities: it does not make the owner's workspace read-only. When a Dogmind write-tool schema is supplied, request that tool instead of asking the owner to change permissions. The final `content` field contains only the user-visible answer. A tool call means a request for Dogmind to execute that tool; you never execute it yourself.";

fn policy() -> Value {
    serde_json::from_str(CODEX_JSON).expect("the generated Codex policy is JSON")
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

pub fn context_fallback() -> u64 {
    policy()["context_fallback"].as_u64().unwrap_or(131072)
}

/// `mcp_servers.<name>` in Codex's configuration: each is turned off for this process.
fn mcp_servers(config: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in config.lines() {
        let Some(rest) = line.trim().strip_prefix("[mcp_servers.") else { continue };
        let name = rest.split([']', '.']).next().unwrap_or("").trim_matches('"');
        if !name.is_empty() && !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }
    names
}

fn toml_key(name: &str) -> String {
    if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// The setting that keeps the app-server from searching the web by itself. What it found would be read by the model and never pass `web_read`, so the session would not be marked as having
/// read the web, and the approvals would stay as loose as before untrusted text came in.
const WEB_SEARCH_OFF: &str = "web_search=\"disabled\"";

pub fn arguments(config: &str) -> Vec<String> {
    let mut args: Vec<String> = ["app-server", "--stdio", "-c", "mcp_servers={}", "-c", WEB_SEARCH_OFF].iter().map(|text| text.to_string()).collect();
    for server in mcp_servers(config) {
        args.extend(["-c".to_string(), format!("mcp_servers.{}.enabled=false", toml_key(&server))]);
    }
    for feature in strings(&policy()["disabled_features"]) {
        args.extend(["--disable".to_string(), feature]);
    }
    args
}

/// `codex` on PATH: an npm install leaves `codex.cmd`, which `Command` runs only by its whole name.
pub fn find_executable() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) { &["codex.exe", "codex.cmd", "codex.bat"] } else { &["codex"] };
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|folder| names.iter().map(|name| folder.join(name)).find(|path| path.is_file()))
}

fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|home| Path::new(&home).join(".codex")))
}

enum Incoming {
    Line(Value),
    Closed,
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Incoming>,
    next_id: u64,
}

impl Drop for Server {
    fn drop(&mut self) {
        kill_tree(self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    /// `stand_in`: what a test runs in place of the Codex CLI.
    fn start(stand_in: Option<&StandIn>) -> Result<Server, LlmError> {
        let mut command = match stand_in {
            Some(stand_in) => stand_in(),
            None => {
                let executable = find_executable().ok_or_else(|| LlmError::Rejected("the Codex CLI was not found: install it, then sign in with ChatGPT".into()))?;
                let config = codex_home().and_then(|home| std::fs::read_to_string(home.join("config.toml")).ok()).unwrap_or_default();
                let mut command = Command::new(executable);
                command.args(arguments(&config));
                command
            }
        };
        command.env_clear().stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        for name in KEPT_VARIABLES {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        no_window(&mut command);
        let mut child = command.spawn().map_err(|error| LlmError::Transient(format!("codex: {error}")))?;
        let stdin = child.stdin.take().ok_or_else(|| LlmError::Transient("codex has no stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| LlmError::Transient("codex has no stdout".into()))?;
        let (sender, incoming) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(value) = serde_json::from_str(&line) {
                    if sender.send(Incoming::Line(value)).is_err() {
                        return;
                    }
                }
            }
            let _ = sender.send(Incoming::Closed);
        });
        let mut server = Server { child, stdin, incoming, next_id: 1 };
        let cancel = AtomicBool::new(false);
        server.request("initialize", json!({"clientInfo": {"name": "dogmind", "title": "Dogmind", "version": env!("CARGO_PKG_VERSION")}, "capabilities": {"experimentalApi": false}}), &cancel, &mut |_| Ok(()))?;
        server.send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))?;
        Ok(server)
    }

    fn send(&mut self, message: &Value) -> Result<(), LlmError> {
        writeln!(self.stdin, "{message}").and_then(|()| self.stdin.flush()).map_err(|error| LlmError::Transient(format!("codex: {error}")))
    }

    /// The next message, with what is forbidden turned down and ended on. Fails closed: a server that asks to run anything has gone outside what this transport is for.
    fn next(&mut self, cancel: &AtomicBool, limit: Duration) -> Result<Value, LlmError> {
        let started = Instant::now();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(LlmError::Cancelled);
            }
            match self.incoming.recv_timeout(POLL) {
                Ok(Incoming::Line(message)) => {
                    if let Some(refused) = refusal(&message) {
                        if let Some(id) = message.get("id").filter(|_| message.get("method").is_some()) {
                            let _ = self.send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "this transport rejects server tool requests"}}));
                        }
                        return Err(LlmError::Rejected(refused));
                    }
                    return Ok(message);
                }
                Ok(Incoming::Closed) | Err(RecvTimeoutError::Disconnected) => return Err(LlmError::Transient("the Codex app-server ended".into())),
                Err(RecvTimeoutError::Timeout) if started.elapsed() > limit => return Err(LlmError::Transient("the Codex app-server sent nothing for too long".into())),
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn request(&mut self, method: &str, params: Value, cancel: &AtomicBool, on_notification: &mut dyn FnMut(&Value) -> Result<(), LlmError>) -> Result<Value, LlmError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        loop {
            let message = self.next(cancel, RPC_TIMEOUT)?;
            if message.get("id").and_then(Value::as_u64) == Some(id) && message.get("method").is_none() {
                if let Some(error) = message.get("error") {
                    return Err(LlmError::Rejected(format!("codex {method}: {}", error.get("message").unwrap_or(error))));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            on_notification(&message)?;
        }
    }
}

/// The kinds of item (`params.item.type`) a turn of this transport has: what it was given, what the model said and thought, and a plan. Anything else is Codex doing something of its own (a command,
/// a file change, a tool of a server, a search of the web), which is not what this transport is for. Compared with case and underscores left out, as an app-server words them either way.
const EXPECTED_ITEM_KINDS: [&str; 5] = ["usermessage", "agentmessage", "reasoning", "plan", "contextcompaction"];

fn plain_kind(kind: &str) -> String {
    kind.chars().filter(|c| *c != '_' && *c != '-').flat_map(char::to_lowercase).collect()
}

/// The kinds of the items a message names: the one it is about, and those a finished turn lists. An item says its kind as `type`, or as `kind`.
fn item_kinds(message: &Value) -> Vec<String> {
    let kind_of = |item: &Value| item.get("type").or_else(|| item.get("kind")).and_then(Value::as_str).map(str::to_string);
    let Some(params) = message.get("params") else { return Vec::new() };
    let mut kinds: Vec<String> = params.get("item").and_then(kind_of).into_iter().collect();
    for list in [params.get("items"), params.pointer("/turn/items")] {
        kinds.extend(list.and_then(Value::as_array).into_iter().flatten().filter_map(kind_of));
    }
    kinds
}

/// Why this message may not be accepted, if so.
fn refusal(message: &Value) -> Option<String> {
    let method = message.get("method")?.as_str()?;
    if message.get("id").is_some() {
        return Some(format!("the Codex app-server asked for something this transport does not allow: {method}"));
    }
    let lower = method.to_lowercase();
    if strings(&policy()["forbidden_method_parts"]).iter().any(|part| lower.contains(part.as_str())) {
        return Some(format!("the Codex app-server tried to use a tool of its own: {method}"));
    }
    // (by what the item is, as the methods for items are the same for all: `item/started`, `item/completed`)
    for kind in item_kinds(message) {
        let plain = plain_kind(&kind);
        if plain.contains("websearch") {
            return Some(format!("the Codex app-server searched the web on its own ({kind}): what it finds would not pass the web tools' checks"));
        }
        if !EXPECTED_ITEM_KINDS.contains(&plain.as_str()) {
            return Some(format!("the Codex app-server used an item of its own: {kind}"));
        }
    }
    None
}

fn output_schema() -> Value {
    json!({
        "type": "object", "additionalProperties": false, "required": ["kind", "content", "tool_calls"],
        "properties": {
            "kind": {"type": "string", "enum": ["final", "tool_calls"]},
            "content": {"type": "string"},
            "tool_calls": {"type": "array", "items": {
                "type": "object", "additionalProperties": false, "required": ["id", "name", "arguments"],
                "properties": {"id": {"type": "string"}, "name": {"type": "string"}, "arguments": {"type": "string"}},
            }},
        },
    })
}

#[cfg(test)]
fn payload(request: &Request) -> String {
    transcript(request).0
}

/// What a turn is given: the conversation as text, and its pictures as items of their own, which it names by their number.
fn transcript(request: &Request) -> (String, Vec<Value>) {
    let mut pictures = Vec::new();
    let messages: Vec<Value> = request.messages.iter().map(|message| {
        let mut value = serde_json::to_value(message.without_provider_blocks()).unwrap_or(Value::Null);
        if let Some(fields) = value.as_object_mut() {
            fields.remove("reasoning_content");
            if fields.remove("images").is_some() {
                let (loaded, text) = crate::images::loaded(message, request.images);
                fields.insert("content".to_string(), json!(text));
                let first = pictures.len() + 1;
                pictures.extend(loaded.iter().map(|picture| json!({"type": "image", "url": picture.data_url()})));
                fields.insert("attached_images".to_string(), json!((first..=pictures.len()).collect::<Vec<usize>>()));
            }
        }
        value
    }).collect();
    let text = json!({
        "instruction": "The Codex built-in tools are disabled, but the Dogmind tools listed in dogmind_tools are available. Request them through kind=tool_calls; Dogmind will execute them and return their results. Never claim that live workspace access is unavailable before requesting an appropriate Dogmind tool. A returned Dogmind tool error is specific to that call. Return a final answer, or request one or more Dogmind tools. Do not execute tools yourself.",
        "dogmind_tools": request.tools.cloned().unwrap_or(json!([])),
        "messages": messages,
        "tool_choice": "auto",
        "turn_instruction": "Always include content and tool_calls; use an empty string or empty array for unused fields. Each tool-call arguments value must be a JSON-encoded object string matching that tool's schema. A message's attached_images are the numbers of the images that come with this input, counted in order.",
    }).to_string();
    (text, pictures)
}

/// The structured answer as a completion.
pub fn completion_of(text: &str, usage: Option<Usage>) -> Result<Completion, LlmError> {
    let value: Value = serde_json::from_str(text).map_err(|_| LlmError::Transient("Codex returned malformed structured output".into()))?;
    let mut completion = Completion { usage, ..Completion::default() };
    match value.get("kind").and_then(Value::as_str) {
        Some("final") => {
            completion.content = value.get("content").and_then(Value::as_str).unwrap_or("").to_string();
            completion.finish = "stop".into();
        }
        Some("tool_calls") => {
            completion.finish = "tool_calls".into();
            completion.content = value.get("content").and_then(Value::as_str).unwrap_or("").to_string();
            let calls = value.get("tool_calls").and_then(Value::as_array).filter(|calls| !calls.is_empty())
                .ok_or_else(|| LlmError::Transient("Codex returned tool_calls with no call".into()))?;
            for (index, call) in calls.iter().enumerate() {
                let name = call.get("name").and_then(Value::as_str).filter(|name| !name.is_empty()).ok_or_else(|| LlmError::Transient("Codex returned a tool call with no name".into()))?;
                let id = call.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).map_or_else(|| format!("codex-{index}"), str::to_string);
                let arguments = call.get("arguments").and_then(Value::as_str).unwrap_or("{}").to_string();
                completion.tool_calls.push(ToolCall { id, kind: "function".into(), function: FunctionCall { name: name.to_string(), arguments } });
            }
        }
        _ => return Err(LlmError::Transient("Codex returned an answer of no known kind".into())),
    }
    Ok(completion)
}

fn usage_of(raw: &Value) -> Option<Usage> {
    let last = raw.get("last")?;
    let count = |name: &str| last.get(name).and_then(Value::as_u64).unwrap_or(0);
    Some(Usage {
        prompt: count("inputTokens"),
        cached: count("cachedInputTokens"),
        completion: count("outputTokens"),
        tokens_per_second: None,
        window: raw.get("modelContextWindow").and_then(Value::as_u64).filter(|&window| window > 0),
    })
}

/// The answer in a finished turn's items: the final message, else the last that is not commentary.
fn final_text(turn: &Value) -> Option<String> {
    let items = turn.get("items")?.as_array()?;
    let messages: Vec<&Value> = items.iter().filter(|item| item.get("type").and_then(Value::as_str) == Some("agentMessage")).collect();
    let text = |item: &&Value| item.get("text").and_then(Value::as_str).filter(|text| !text.is_empty()).map(str::to_string);
    messages.iter().rev().filter(|item| item.get("phase").and_then(Value::as_str) == Some("final_answer")).find_map(|item| text(item))
        .or_else(|| messages.iter().rev().filter(|item| item.get("phase").and_then(Value::as_str) != Some("commentary")).find_map(|item| text(item)))
}

type StandIn = Box<dyn Fn() -> Command + Send + Sync>;

/// A page of `model/list`, by id, with what the plan marks as its default ahead of the rest.
fn models_of(page: &Value, models: &mut Vec<String>) {
    for item in page.get("data").and_then(Value::as_array).into_iter().flatten() {
        let Some(id) = item.get("id").or_else(|| item.get("model")).and_then(Value::as_str) else { continue };
        if models.iter().any(|known| known == id) {
            continue;
        }
        if item.get("isDefault").and_then(Value::as_bool) == Some(true) {
            models.insert(0, id.to_string());
        } else {
            models.push(id.to_string());
        }
    }
}

#[derive(Default)]
pub struct Codex {
    server: Mutex<Option<Server>>,
    stand_in: Option<StandIn>,
}

impl Codex {
    /// The models this ChatGPT plan offers, as Codex lists them, the default first.
    pub fn models(&self) -> Result<Vec<String>, LlmError> {
        const PAGES: usize = 20;
        let mut slot = self.server.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let server = match &mut *slot {
            Some(server) => server,
            empty => empty.insert(Server::start(self.stand_in.as_ref())?),
        };
        let cancel = AtomicBool::new(false);
        let (mut models, mut cursor): (Vec<String>, Option<String>) = (Vec::new(), None);
        for _ in 0..PAGES {
            let mut params = json!({"includeHidden": false, "limit": 100});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let answer = match server.request("model/list", params, &cancel, &mut |_| Ok(())) {
                Ok(answer) => answer,
                // (as after a turn: a server that failed here is not asked again)
                Err(error) => {
                    *slot = None;
                    return Err(error);
                }
            };
            models_of(&answer, &mut models);
            cursor = answer.get("nextCursor").and_then(Value::as_str).filter(|cursor| !cursor.is_empty()).map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(models)
    }

    fn turn(server: &mut Server, request: &Request, cancel: &AtomicBool, on: &mut dyn FnMut(Delta)) -> Result<Completion, LlmError> {
        let folder = std::env::temp_dir().join("dogmind-app-codex");
        std::fs::create_dir_all(&folder).map_err(|error| LlmError::Transient(format!("{}: {error}", folder.display())))?;
        let thread = server.request("thread/start", json!({
            "model": request.model, "cwd": folder, "sandbox": "read-only", "approvalPolicy": "never", "ephemeral": true, "baseInstructions": INSTRUCTIONS,
        }), cancel, &mut |_| Ok(()))?;
        let thread_id = thread.pointer("/thread/id").or_else(|| thread.get("threadId")).and_then(Value::as_str).unwrap_or("").to_string();
        if thread_id.is_empty() {
            return Err(LlmError::Transient("the Codex app-server returned no thread".into()));
        }
        let (text, pictures) = transcript(request);
        let mut input = vec![json!({"type": "text", "text": text})];
        input.extend(pictures);
        let mut params = json!({"threadId": thread_id, "input": input, "model": request.model, "outputSchema": output_schema(), "summary": "auto"});
        if let Some(effort) = request.effort.filter(|effort| ["none", "minimal", "low", "medium", "high", "xhigh"].contains(effort)) {
            params["effort"] = json!(effort);
        }
        let mut reasoning = String::new();
        let mut usage = None;
        let mut ended = None;
        {
            let mut note = |message: &Value| -> Result<(), LlmError> { observe(message, &thread_id, &mut reasoning, &mut usage, &mut ended, on) };
            server.request("turn/start", params, cancel, &mut note)?;
        }
        while ended.is_none() {
            let message = server.next(cancel, STALL)?;
            observe(&message, &thread_id, &mut reasoning, &mut usage, &mut ended, on)?;
        }
        let turn: Value = ended.unwrap_or(Value::Null);
        if turn.get("status").and_then(Value::as_str) != Some("completed") {
            let reason = turn.pointer("/error/message").and_then(Value::as_str).unwrap_or("the Codex turn did not complete");
            return Err(LlmError::Rejected(reason.chars().take(500).collect()));
        }
        let text = final_text(&turn).ok_or_else(|| LlmError::Transient("the Codex turn ended with no answer".into()))?;
        let mut completion = completion_of(&text, usage)?;
        completion.reasoning = String::new(); // a summary, not reasoning to send back
        if !completion.content.is_empty() {
            on(Delta::Content(&completion.content));
        }
        Ok(completion)
    }
}

fn observe(message: &Value, thread: &str, reasoning: &mut String, usage: &mut Option<Usage>, ended: &mut Option<Value>, on: &mut dyn FnMut(Delta)) -> Result<(), LlmError> {
    let Some(method) = message.get("method").and_then(Value::as_str) else { return Ok(()) };
    let Some(params) = message.get("params") else { return Ok(()) };
    if params.get("threadId").and_then(Value::as_str) != Some(thread) {
        return Ok(());
    }
    match method {
        "item/reasoning/summaryTextDelta" => {
            if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                reasoning.push_str(delta);
                on(Delta::Reasoning(delta));
            }
        }
        "thread/tokenUsage/updated" => {
            if let Some(found) = params.get("tokenUsage").and_then(usage_of) {
                *usage = Some(found);
            }
        }
        "turn/completed" => *ended = Some(params.get("turn").cloned().unwrap_or(Value::Null)),
        _ => {}
    }
    Ok(())
}

impl Provider for Codex {
    /// The whole conversation is one input: a tool's picture is as good as the person's.
    fn images_in_tool_results(&self) -> bool {
        true
    }

    fn stream(&self, request: &Request, cancel: &AtomicBool, on: &mut dyn FnMut(Delta)) -> Result<Completion, LlmError> {
        let mut slot = self.server.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(Server::start(self.stand_in.as_ref())?);
        }
        let server = slot.as_mut().ok_or_else(|| LlmError::Transient("the Codex app-server did not start".into()))?;
        let result = Codex::turn(server, request, cancel, on);
        // Anything but an answer leaves a server in an unknown state: the next request starts another. A cancelled turn ends here too.
        if result.is_err() {
            *slot = None;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This test program again, as the app-server of a plan with three models over two pages. It is what the tests below talk to; no Codex CLI and no account is involved.
    #[test]
    #[ignore = "started by the tests below, in place of the Codex CLI"]
    fn stand_in_app_server() {
        let mut out = std::io::stdout();
        // (the test harness has left its own words on this line)
        writeln!(out).unwrap();
        for line in BufReader::new(std::io::stdin()).lines().map_while(Result::ok) {
            let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
            let (id, method) = (message["id"].clone(), message["method"].as_str().unwrap_or("").to_string());
            let answer = match method.as_str() {
                "model/list" if message["params"].get("cursor").is_some() => json!({"data": [{"id": "gpt-chosen", "model": "gpt-chosen"}]}),
                "model/list" => json!({"data": [{"id": "gpt-first"}, {"id": "gpt-default", "isDefault": true}], "nextCursor": "more"}),
                "thread/start" => json!({"thread": {"id": format!("thread-{}", message["params"]["model"].as_str().unwrap_or(""))}}),
                "initialize" | "turn/start" => json!({}),
                _ => continue,
            };
            writeln!(out, "{}", json!({"jsonrpc": "2.0", "id": id, "result": answer})).unwrap();
            if method == "turn/start" {
                // What the conversation asks for is what it does: a search of the web of its own, or an ordinary message of the model's.
                let asked = message["params"]["input"][0]["text"].as_str().unwrap_or("");
                for (marker, kind) in [("search the web", "webSearch"), ("say it plainly", "agentMessage")] {
                    if asked.contains(marker) {
                        let item = json!({"type": kind, "id": "item-1"});
                        writeln!(out, "{}", json!({"jsonrpc": "2.0", "method": "item/started", "params": {"threadId": message["params"]["threadId"], "item": item}})).unwrap();
                    }
                }
                // What it answers is the model it was asked for, by the thread and by the turn.
                let said = format!("{} {}", message["params"]["threadId"].as_str().unwrap_or(""), message["params"]["model"].as_str().unwrap_or(""));
                let final_text = json!({"kind": "final", "content": said, "tool_calls": []}).to_string();
                let turn = json!({"status": "completed", "items": [{"type": "agentMessage", "phase": "final_answer", "text": final_text}]});
                writeln!(out, "{}", json!({"jsonrpc": "2.0", "method": "turn/completed", "params": {"threadId": message["params"]["threadId"], "turn": turn}})).unwrap();
            }
            out.flush().unwrap();
        }
    }

    fn stood_in() -> Codex {
        let stand_in: StandIn = Box::new(|| {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args(["codex::tests::stand_in_app_server", "--exact", "--ignored", "--nocapture", "--test-threads=1"]);
            command
        });
        Codex { stand_in: Some(stand_in), ..Codex::default() }
    }

    #[test]
    fn the_models_are_the_plans_own_list_all_of_it_with_its_default_first() {
        assert_eq!(stood_in().models().unwrap(), ["gpt-default", "gpt-first", "gpt-chosen"]);
    }

    #[test]
    fn the_model_chosen_is_the_one_the_request_asks_for() {
        let codex = stood_in();
        let messages = [crate::message::Message::text("user", "hi")];
        let request = Request { model: "gpt-chosen", messages: &messages, tools: None, effort: None, images: None, japanese_letters: false, request_id: None };
        let answer = codex.stream(&request, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(answer.content, "thread-gpt-chosen gpt-chosen");
    }

    #[test]
    fn its_own_tools_and_every_mcp_server_are_off() {
        let args = arguments("model = \"x\"\n[mcp_servers.alpha]\ncommand = \"a\"\n[mcp_servers.alpha.env]\nA = \"1\"\n[mcp_servers.\"b c\"]\ncommand = \"b\"\n");
        assert_eq!(&args[..6], ["app-server", "--stdio", "-c", "mcp_servers={}", "-c", "web_search=\"disabled\""]);
        assert!(args.contains(&"mcp_servers.alpha.enabled=false".to_string()));
        assert!(args.contains(&"mcp_servers.\"b c\".enabled=false".to_string()));
        assert_eq!(args.iter().filter(|arg| arg.contains("alpha")).count(), 1);
        for feature in ["shell_tool", "unified_exec", "plugins"] {
            assert!(args.windows(2).any(|pair| pair[0] == "--disable" && pair[1] == feature), "{feature}");
        }
    }

    /// A turn whose conversation says `asked`, through the stand-in, which answers it with an item of the kind the words name.
    fn answer_to(asked: &str) -> Result<Completion, LlmError> {
        let messages = [crate::message::Message::text("user", asked)];
        let request = Request { model: "gpt-chosen", messages: &messages, tools: None, effort: None, images: None, japanese_letters: false, request_id: None };
        stood_in().stream(&request, &AtomicBool::new(false), &mut |_| {})
    }

    #[test]
    fn a_search_of_the_web_by_the_app_server_ends_the_turn_and_an_ordinary_message_does_not() {
        let LlmError::Rejected(why) = answer_to("please search the web for it").unwrap_err() else { panic!("a refusal") };
        assert!(why.contains("searched the web") && why.contains("webSearch"), "{why}");
        assert!(answer_to("say it plainly").is_ok());
        assert!(answer_to("hello").is_ok());
    }

    #[test]
    fn items_are_refused_by_their_kind_whatever_the_method_and_the_spelling() {
        let item = |method: &str, item: Value| json!({"method": method, "params": {"threadId": "t", "item": item}});
        for web in [json!({"type": "webSearch"}), json!({"type": "web_search_call"}), json!({"kind": "WebSearch"}), json!({"type": "web-search"})] {
            let why = refusal(&item("item/started", web.clone())).unwrap_or_default();
            assert!(why.contains("searched the web"), "{web}: {why}");
            assert!(refusal(&item("item/completed", web)).is_some());
        }
        for other in [json!({"type": "commandExecution"}), json!({"type": "mcpToolCall"}), json!({"type": "imageGeneration"}), json!({"kind": "somethingNew"})] {
            assert!(refusal(&item("item/started", other.clone())).is_some_and(|why| why.contains("an item of its own")), "{other}");
        }
        for fine in [json!({"type": "agentMessage"}), json!({"type": "reasoning"}), json!({"type": "userMessage"}), json!({"type": "plan"}), json!({"kind": "agent_message"}), json!({"id": "no kind"})] {
            assert!(refusal(&item("item/started", fine.clone())).is_none(), "{fine}");
        }
        // (a finished turn lists its items too)
        let turn = |items: Value| json!({"method": "turn/completed", "params": {"threadId": "t", "turn": {"status": "completed", "items": items}}});
        assert!(refusal(&turn(json!([{"type": "agentMessage", "phase": "final_answer", "text": "{}"}, {"type": "reasoning"}]))).is_none());
        assert!(refusal(&turn(json!([{"type": "agentMessage", "text": "{}"}, {"type": "webSearch"}]))).is_some());
    }

    #[test]
    fn a_server_that_asks_to_run_anything_is_refused() {
        assert!(refusal(&json!({"id": 7, "method": "item/commandExecution/requestApproval", "params": {}})).is_some());
        assert!(refusal(&json!({"method": "item/fileChange/outputDelta", "params": {}})).is_some());
        assert!(refusal(&json!({"method": "item/agentMessage/delta", "params": {}})).is_none());
        assert!(refusal(&json!({"id": 7, "result": {}})).is_none());
    }

    #[test]
    fn a_structured_answer_becomes_a_completion() {
        let done = completion_of(r#"{"kind":"final","content":"hello","tool_calls":[]}"#, None).unwrap();
        assert_eq!((done.content.as_str(), done.finish.as_str(), done.tool_calls.len()), ("hello", "stop", 0));
        let calls = completion_of(r#"{"kind":"tool_calls","content":"","tool_calls":[{"id":"","name":"read_file","arguments":"{\"path\":\"a\"}"}]}"#, None).unwrap();
        assert_eq!(calls.finish, "tool_calls");
        assert_eq!(calls.tool_calls[0].id, "codex-0");
        assert_eq!(calls.tool_calls[0].function.arguments, "{\"path\":\"a\"}");
        assert!(completion_of("not json", None).is_err());
        assert!(completion_of(r#"{"kind":"tool_calls","content":"","tool_calls":[]}"#, None).is_err());
    }

    #[test]
    fn the_answer_is_the_final_message_not_the_commentary() {
        let turn = json!({"items": [{"type": "agentMessage", "phase": "commentary", "text": "working"}, {"type": "agentMessage", "phase": "final_answer", "text": "{}"}, {"type": "reasoning", "text": "x"}]});
        assert_eq!(final_text(&turn).unwrap(), "{}");
        let plain = json!({"items": [{"type": "agentMessage", "phase": "commentary", "text": "working"}, {"type": "agentMessage", "text": "old style"}]});
        assert_eq!(final_text(&plain).unwrap(), "old style");
        assert!(final_text(&json!({"items": [{"type": "agentMessage", "phase": "commentary", "text": "x"}]})).is_none());
    }

    #[test]
    fn usage_comes_with_the_context() {
        let usage = usage_of(&json!({"last": {"inputTokens": 10, "cachedInputTokens": 4, "outputTokens": 2}, "modelContextWindow": 1000})).unwrap();
        assert_eq!((usage.prompt, usage.cached, usage.completion, usage.window), (10, 4, 2, Some(1000)));
    }

    #[test]
    fn the_transcript_leaves_out_the_reasoning() {
        let message = crate::message::Message { role: "assistant".into(), content: Some("a".into()), reasoning_content: Some("secret".into()), ..Default::default() };
        let messages = [message];
        let text = payload(&Request { model: "m", messages: &messages, tools: None, effort: None, images: None, japanese_letters: false, request_id: None });
        assert!(!text.contains("secret") && text.contains("dogmind_tools"));
    }

    #[test]
    fn a_picture_goes_beside_the_transcript_and_is_named_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("s.jsonl");
        let bytes = crate::images::tests::encoded(8, 8, image::ImageFormat::Png);
        let kept = crate::images::save(&session, &crate::images::attach_bytes("a", bytes.clone()).unwrap(), crate::images::ImageLimit::WIDE).unwrap();
        let messages = [crate::message::Message { images: vec![kept], ..crate::message::Message::text("user", "what") }];
        let folder = crate::images::folder_of(&session);
        let (text, pictures) = transcript(&Request { model: "m", messages: &messages, tools: None, effort: None, images: Some(&folder), japanese_letters: false, request_id: None });
        assert_eq!(pictures, [json!({"type": "image", "url": format!("data:image/png;base64,{}", crate::images::base64(&bytes))})]);
        assert!(text.contains("\"attached_images\":[1]") && !text.contains("media_type") && !text.contains("base64"), "{text}");
    }
}
