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

pub fn arguments(config: &str) -> Vec<String> {
    let mut args: Vec<String> = ["app-server", "--stdio", "-c", "mcp_servers={}"].iter().map(|text| text.to_string()).collect();
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
    fn start() -> Result<Server, LlmError> {
        let executable = find_executable().ok_or_else(|| LlmError::Rejected("the Codex CLI was not found: install it, then sign in with ChatGPT".into()))?;
        let config = codex_home().and_then(|home| std::fs::read_to_string(home.join("config.toml")).ok()).unwrap_or_default();
        let mut command = Command::new(executable);
        command.args(arguments(&config)).env_clear().stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
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

/// Why this message may not be accepted, if so.
fn refusal(message: &Value) -> Option<String> {
    let method = message.get("method")?.as_str()?;
    if message.get("id").is_some() {
        return Some(format!("the Codex app-server asked for something this transport does not allow: {method}"));
    }
    let lower = method.to_lowercase();
    strings(&policy()["forbidden_method_parts"]).iter().any(|part| lower.contains(part.as_str()))
        .then(|| format!("the Codex app-server tried to use a tool of its own: {method}"))
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

fn payload(request: &Request) -> String {
    let messages: Vec<Value> = request.messages.iter().map(|message| {
        let mut value = serde_json::to_value(message.without_provider_blocks()).unwrap_or(Value::Null);
        if let Some(fields) = value.as_object_mut() {
            fields.remove("reasoning_content");
        }
        value
    }).collect();
    json!({
        "instruction": "The Codex built-in tools are disabled, but the Dogmind tools listed in dogmind_tools are available. Request them through kind=tool_calls; Dogmind will execute them and return their results. Never claim that live workspace access is unavailable before requesting an appropriate Dogmind tool. A returned Dogmind tool error is specific to that call. Return a final answer, or request one or more Dogmind tools. Do not execute tools yourself.",
        "dogmind_tools": request.tools.cloned().unwrap_or(json!([])),
        "messages": messages,
        "tool_choice": "auto",
        "turn_instruction": "Always include content and tool_calls; use an empty string or empty array for unused fields. Each tool-call arguments value must be a JSON-encoded object string matching that tool's schema.",
    }).to_string()
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

#[derive(Default)]
pub struct Codex {
    server: Mutex<Option<Server>>,
}

impl Codex {
    /// The models this ChatGPT plan offers.
    pub fn models(&self) -> Result<Vec<String>, LlmError> {
        let mut slot = self.server.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let server = match &mut *slot {
            Some(server) => server,
            empty => empty.insert(Server::start()?),
        };
        let cancel = AtomicBool::new(false);
        let answer = server.request("model/list", json!({"includeHidden": false, "limit": 100}), &cancel, &mut |_| Ok(()))?;
        Ok(answer.get("data").and_then(Value::as_array).into_iter().flatten()
            .filter_map(|item| item.get("id").or_else(|| item.get("model")).and_then(Value::as_str).map(str::to_string)).collect())
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
        let mut params = json!({"threadId": thread_id, "input": [{"type": "text", "text": payload(request)}], "model": request.model, "outputSchema": output_schema(), "summary": "auto"});
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
    fn stream(&self, request: &Request, cancel: &AtomicBool, on: &mut dyn FnMut(Delta)) -> Result<Completion, LlmError> {
        let mut slot = self.server.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(Server::start()?);
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

    #[test]
    fn its_own_tools_and_every_mcp_server_are_off() {
        let args = arguments("model = \"x\"\n[mcp_servers.alpha]\ncommand = \"a\"\n[mcp_servers.alpha.env]\nA = \"1\"\n[mcp_servers.\"b c\"]\ncommand = \"b\"\n");
        assert_eq!(&args[..4], ["app-server", "--stdio", "-c", "mcp_servers={}"]);
        assert!(args.contains(&"mcp_servers.alpha.enabled=false".to_string()));
        assert!(args.contains(&"mcp_servers.\"b c\".enabled=false".to_string()));
        assert_eq!(args.iter().filter(|arg| arg.contains("alpha")).count(), 1);
        for feature in ["shell_tool", "unified_exec", "plugins"] {
            assert!(args.windows(2).any(|pair| pair[0] == "--disable" && pair[1] == feature), "{feature}");
        }
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
        let text = payload(&Request { model: "m", messages: &messages, tools: None, effort: None });
        assert!(!text.contains("secret") && text.contains("dogmind_tools"));
    }
}
