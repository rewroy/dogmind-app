//! MCP servers over stdio, as the same `mcpServers` file Claude Code reads describes them: a command, its arguments and its
//! environment. They start with the app and their tools join the session's as `mcp__server__tool`.
//!
//! A server is a program the person chose to run, so it is not in the sandbox, and its tools ask like commands do. What one
//! returns may come from anywhere, so it goes to the model marked as data, as a web page does. A server the person says they trust
//! still has that mark, and its tools still ask as they did; only what it returns no longer counts as web content in the conversation.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const PROTOCOL: &str = "2025-06-18";
const START_LIMIT: Duration = Duration::from_secs(60);
const CALL_LIMIT: Duration = Duration::from_secs(600);
const MAX_DESCRIPTION: usize = 1024;
const MAX_RESULT: usize = 32 * 1024;
const ENDED: &str = "the server has ended";
const PIPE: &str = "the server's pipe";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// The person's word that what this server returns is not a stranger's. In this app's own file, which neither a model's tools nor a command can write.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trusted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(rename = "mcpServers", default)]
    pub servers: BTreeMap<String, ServerConfig>,
}

pub fn load_config(file: &Path) -> Config {
    std::fs::read_to_string(file).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

/// Claude Code's own list of servers: the `mcpServers` of `~/.claude.json`. Those with no command (HTTP ones) are left out.
pub fn servers_of_claude(text: &str) -> BTreeMap<String, ServerConfig> {
    let value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let mut found = BTreeMap::new();
    for (name, server) in value.get("mcpServers").and_then(Value::as_object).into_iter().flatten() {
        if let Ok(config) = serde_json::from_value::<ServerConfig>(server.clone()) {
            if !config.command.is_empty() {
                found.insert(name.clone(), config);
            }
        }
    }
    found
}

/// Codex's: the `[mcp_servers.NAME]` tables of `~/.codex/config.toml`, of which only these simple forms are read.
pub fn servers_of_codex(text: &str) -> BTreeMap<String, ServerConfig> {
    let unquote = |value: &str| value.trim().trim_matches('"').replace("\\\\", "\\").replace("\\\"", "\"");
    let mut found: BTreeMap<String, ServerConfig> = BTreeMap::new();
    let (mut current, mut in_env) = (None, false);
    for line in text.lines().map(str::trim) {
        if let Some(table) = line.strip_prefix('[').and_then(|line| line.strip_suffix(']')) {
            (current, in_env) = match table.strip_prefix("mcp_servers.") {
                Some(rest) => match rest.strip_suffix(".env") {
                    Some(name) => (Some(name.trim_matches('"').to_string()), true),
                    None if rest.contains('.') => (None, false),
                    None => (Some(rest.trim_matches('"').to_string()), false),
                },
                None => (None, false),
            };
            continue;
        }
        let (Some(name), Some((key, value))) = (&current, line.split_once('=')) else { continue };
        let (key, value) = (key.trim(), value.trim());
        let server = found.entry(name.clone()).or_default();
        if in_env {
            server.env.insert(key.to_string(), unquote(value));
        } else if key == "command" {
            server.command = unquote(value);
        } else if key == "args" {
            let inner = value.trim_start_matches('[').trim_end_matches(']');
            server.args = inner.split("\",").map(unquote).filter(|arg| !arg.is_empty()).collect();
        }
    }
    found.retain(|_, server| !server.command.is_empty());
    found
}

/// Adds what is not there yet, and says how many.
pub fn import(file: &Path, more: BTreeMap<String, ServerConfig>) -> Result<usize, String> {
    let mut config = load_config(file);
    let before = config.servers.len();
    for (name, server) in more {
        config.servers.entry(name).or_insert(server);
    }
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(&config).map_err(|error| error.to_string())?;
    std::fs::write(file, text).map_err(|error| format!("{}: {error}", file.display()))?;
    Ok(config.servers.len() - before)
}

/// The JSON-RPC of one server, whichever pipes it runs over.
struct Rpc {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    answers: Receiver<Value>,
    next: u64,
}

impl Rpc {
    /// Reads the server's lines on a thread of its own; it answers a ping itself, as the server may wait on one.
    fn over(writer: Box<dyn Write + Send>, reader: impl BufRead + Send + 'static) -> Rpc {
        let writer = Arc::new(Mutex::new(writer));
        let (sender, answers) = channel();
        let replies = writer.clone();
        std::thread::spawn(move || {
            for line in reader.lines().map_while(Result::ok) {
                let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                if message.get("method").and_then(Value::as_str) == Some("ping") {
                    if let Some(id) = message.get("id") {
                        let pong = json!({"jsonrpc": "2.0", "id": id, "result": {}});
                        let mut out = replies.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        let _ = writeln!(out, "{pong}").and_then(|()| out.flush());
                    }
                    continue;
                }
                if message.get("id").is_some() && message.get("method").is_none() && sender.send(message).is_err() {
                    return;
                }
            }
        });
        Rpc { writer, answers, next: 1 }
    }

    fn send(&self, message: &Value) -> Result<(), String> {
        let mut out = self.writer.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        writeln!(out, "{message}").and_then(|()| out.flush()).map_err(|error| format!("{PIPE}: {error}"))
    }

    fn call(&mut self, method: &str, params: Value, limit: Duration, cancel: Option<&AtomicBool>) -> Result<Value, String> {
        let id = self.next;
        self.next += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let started = Instant::now();
        loop {
            if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                let _ = self.send(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": id}}));
                return Err("stopped".to_string());
            }
            if started.elapsed() > limit {
                return Err(format!("no answer to {method} within {} s", limit.as_secs()));
            }
            match self.answers.recv_timeout(Duration::from_millis(100)) {
                Ok(answer) if answer.get("id").and_then(Value::as_u64) == Some(id) => {
                    if let Some(error) = answer.get("error") {
                        return Err(error.get("message").and_then(Value::as_str).map_or_else(|| error.to_string(), str::to_string));
                    }
                    return Ok(answer.get("result").cloned().unwrap_or(Value::Null));
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(ENDED.to_string()),
            }
        }
    }

    /// The handshake, and what tools the server has.
    fn start(&mut self) -> Result<Vec<Value>, String> {
        self.call("initialize", json!({"protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": {"name": "dogmind-app", "version": env!("CARGO_PKG_VERSION")}}), START_LIMIT, None)?;
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.as_ref().map_or(json!({}), |cursor| json!({"cursor": cursor}));
            let page = self.call("tools/list", params, START_LIMIT, None)?;
            tools.extend(page.get("tools").and_then(Value::as_array).cloned().unwrap_or_default());
            cursor = page.get("nextCursor").and_then(Value::as_str).map(str::to_string);
            if cursor.is_none() {
                return Ok(tools);
            }
        }
    }
}

/// A tool's result as text: what it says in words, and a word for what it says otherwise.
fn result_text(result: &Value) -> String {
    let parts: Vec<String> = result.get("content").and_then(Value::as_array).into_iter().flatten().map(|part| match part.get("type").and_then(Value::as_str) {
        Some("text") => part.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
        Some(kind) => format!("[{kind} content is not shown]"),
        None => part.to_string(),
    }).collect();
    let text = if parts.is_empty() { result.get("structuredContent").map_or(String::new(), Value::to_string) } else { parts.join("\n") };
    crate::tools::cap(&text, MAX_RESULT)
}

/// A name the model's API takes: letters, digits, `_` and `-`, 64 at most.
fn tool_name(server: &str, tool: &str) -> String {
    let clean = |text: &str| text.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>();
    format!("mcp__{}__{}", clean(server), clean(tool)).chars().take(64).collect()
}

/// `npx` is `npx.cmd` on Windows, which starting a program by name does not find.
fn program(command: &str) -> (PathBuf, bool) {
    let given = Path::new(command);
    let script = |path: &Path| path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"));
    if given.extension().is_some() || given.components().count() > 1 {
        return (given.to_path_buf(), script(given));
    }
    #[cfg(windows)]
    let search = crate::bundled::path_for_programs().map(std::ffi::OsString::from).or_else(|| std::env::var_os("PATH"));
    #[cfg(not(windows))]
    let search = std::env::var_os("PATH");
    let folders: Vec<PathBuf> = search.map_or(Vec::new(), |path| std::env::split_paths(&path).collect());
    for folder in folders {
        for ext in ["exe", "cmd", "bat"] {
            let candidate = folder.join(format!("{command}.{ext}"));
            if candidate.is_file() {
                let is_script = script(&candidate);
                return (candidate, is_script);
            }
        }
    }
    (given.to_path_buf(), false)
}

#[cfg(windows)]
struct Ending(windows_sys::Win32::Foundation::HANDLE);

// A handle is valid on any thread.
#[cfg(windows)]
unsafe impl Send for Ending {}
#[cfg(windows)]
unsafe impl Sync for Ending {}

#[cfg(windows)]
impl Drop for Ending {
    fn drop(&mut self) {
        // SAFETY: the handle is this value's; closing it ends what is in the job.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// So that a server, and what it started, ends with the app however the app ends.
#[cfg(windows)]
fn ends_with_this_process(child: &Child) -> Option<Ending> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::*;
    // SAFETY: the structure is as long as the call is told, and the handles are live.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let ending = Ending(job);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // (a job that does not hold it ends nothing: the server is then ended by its own Drop)
        let held = SetInformationJobObject(job, JobObjectExtendedLimitInformation, std::ptr::addr_of!(limits).cast(), std::mem::size_of_val(&limits) as u32) != 0
            && AssignProcessToJobObject(job, child.as_raw_handle() as _) != 0;
        held.then_some(ending)
    }
}

struct Running {
    rpc: Mutex<Rpc>,
    /// What each tool is called by its server, by the name the model knows it by.
    names: BTreeMap<String, String>,
    schemas: Vec<Value>,
    _child: Option<Child>,
    #[cfg(windows)]
    _ending: Option<Ending>,
}

impl Drop for Running {
    /// A server the job holds ends as the job closes, with all it started. One it does not hold (no job could be made, or the server was
    /// in one already) is ended here, with what it started: a `Child` dropped leaves its process running, `npx` and `node` past the app.
    fn drop(&mut self) {
        #[cfg(windows)]
        if self._ending.is_some() {
            return;
        }
        if let Some(child) = &mut self._child {
            // (its PID is not another's while its handle is held here)
            if matches!(child.try_wait(), Ok(None)) {
                crate::tools::kill_tree(child.id());
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

type Starter = Arc<dyn Fn() -> Result<Arc<Running>, String> + Send + Sync>;

#[derive(Default)]
struct Slot {
    name: String,
    /// How to bring it back when it has ended.
    start: Option<Starter>,
    /// While it starts, none of these.
    running: Option<Arc<Running>>,
    failed: Option<String>,
    trusted: bool,
}

/// The servers, shared by every session.
#[derive(Default)]
pub struct Hub {
    slots: Mutex<Vec<Slot>>,
    /// The file the servers were listed in.
    file: Mutex<Option<PathBuf>>,
}

fn start_server(config: &ServerConfig) -> Result<Running, String> {
    let (path, script) = program(&config.command);
    let mut command = if script {
        let mut shell = Command::new("cmd.exe");
        shell.arg("/c").arg(&path);
        shell
    } else {
        Command::new(&path)
    };
    // (a server started with `npx` or `python` has them on a machine with neither installed; what its configuration says of PATH comes after, and wins)
    #[cfg(windows)]
    if let Some(path) = crate::bundled::path_for_programs() {
        command.env("PATH", path);
    }
    command.args(&config.args).envs(&config.env).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn().map_err(|error| format!("{}: {error}", config.command))?;
    #[cfg(windows)]
    let ending = ends_with_this_process(&child);
    let writer = child.stdin.take().ok_or("no stdin")?;
    let reader = child.stdout.take().ok_or("no stdout")?;
    let mut rpc = Rpc::over(Box::new(writer), BufReader::new(reader));
    let tools = rpc.start()?;
    Ok(Running {
        rpc: Mutex::new(rpc),
        names: BTreeMap::new(),
        schemas: tools,
        _child: Some(child),
        #[cfg(windows)]
        _ending: ending,
    })
}

fn schema_of(server: &str, tool: &Value) -> Option<(String, String, Value)> {
    let real = tool.get("name")?.as_str()?.to_string();
    let name = tool_name(server, &real);
    let description: String = tool.get("description").and_then(Value::as_str).unwrap_or("").chars().take(MAX_DESCRIPTION).collect();
    let mut parameters = tool.get("inputSchema").cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    // (the services refuse the whole request over a tool without this)
    if let Some(fields) = parameters.as_object_mut().filter(|fields| !fields.contains_key("type")) {
        fields.insert("type".to_string(), json!("object"));
    }
    let schema = json!({"type": "function", "function": {"name": name, "description": format!("From the MCP server {server}. {description}"), "parameters": parameters}});
    Some((name, real, schema))
}

/// A server's tools under the names the model knows them by, in an order that does not depend on the server's mood. A cut or a cleaning that gives two tools
/// one name gets the second a number: the services refuse the whole request over a name twice, and so every request.
fn place_tools(server: &str, tools: &[Value]) -> (BTreeMap<String, String>, Vec<Value>) {
    let mut listed: Vec<(String, String, Value)> = tools.iter().filter_map(|tool| schema_of(server, tool)).collect();
    listed.sort_by(|a, b| a.1.cmp(&b.1));
    let (mut names, mut schemas) = (BTreeMap::new(), Vec::new());
    for (base, real, mut schema) in listed {
        let (mut known, mut again) = (base.clone(), 1);
        while names.contains_key(&known) {
            again += 1;
            let suffix = format!("_{again}");
            known = format!("{}{suffix}", base.chars().take(64 - suffix.len()).collect::<String>());
        }
        schema["function"]["name"] = json!(known);
        names.insert(known, real);
        schemas.push(schema);
    }
    (names, schemas)
}

fn ready(name: &str, config: &ServerConfig) -> Result<Arc<Running>, String> {
    let mut running = start_server(config)?;
    let tools = std::mem::take(&mut running.schemas);
    (running.names, running.schemas) = place_tools(name, &tools);
    Ok(Arc::new(running))
}

impl Hub {
    /// Starts what the file lists, each on its own thread, as one that is slow or broken must not hold up the app.
    pub fn start(file: &Path) -> Arc<Hub> {
        let hub = Arc::new(Hub::default());
        *hub.file.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(file.to_path_buf());
        for (name, config) in load_config(file).servers {
            let trusted = config.trusted;
            let start: Starter = {
                let name = name.clone();
                Arc::new(move || ready(&name, &config))
            };
            hub.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(Slot { name: name.clone(), start: Some(start.clone()), trusted, ..Slot::default() });
            let hub = hub.clone();
            std::thread::spawn(move || {
                let outcome = start();
                let mut slots = hub.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(slot) = slots.iter_mut().find(|slot| slot.name == name) {
                    match outcome {
                        Ok(running) => slot.running = Some(running),
                        Err(error) => slot.failed = Some(error),
                    }
                }
            });
        }
        hub
    }

    /// The tools of the servers up by now. A session started before one was up gets its tools in the next session.
    pub fn schemas(&self) -> Vec<Value> {
        let slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut seen = std::collections::HashSet::new();
        // (two servers whose names clean to one: the first keeps it, as it is the one a call reaches)
        slots.iter().filter_map(|slot| slot.running.as_ref()).flat_map(|running| running.schemas.iter().cloned())
            .filter(|schema| seen.insert(schema["function"]["name"].to_string())).collect()
    }

    pub fn call(&self, tool: &str, arguments: &Value, cancel: &AtomicBool) -> Result<String, String> {
        let found = {
            let slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            slots.iter().filter_map(|slot| slot.running.clone()).find(|running| running.names.contains_key(tool))
        };
        let running = found.ok_or_else(|| format!("{tool}: its server is not running"))?;
        let real = running.names.get(tool).cloned().unwrap_or_default();
        let answer = {
            // Another session's call may have it for minutes, and a person who stops the turn does not wait for that.
            let mut rpc = loop {
                match running.rpc.try_lock() {
                    Ok(rpc) => break rpc,
                    Err(TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
                    Err(TryLockError::WouldBlock) if cancel.load(Ordering::Relaxed) => return Err("stopped".to_string()),
                    Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(50)),
                }
            };
            rpc.call("tools/call", json!({"name": real, "arguments": arguments}), CALL_LIMIT, Some(cancel))
        };
        let result = match answer {
            Ok(result) => result,
            Err(error) if error == ENDED || error.starts_with(PIPE) => return Err(self.start_again(&running, error)),
            Err(error) => return Err(error),
        };
        let text = result_text(&result);
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(text);
        }
        Ok(text)
    }

    /// A server that has ended starts again for the next call. This call is not made again: it may have done what it was asked before the server went.
    fn start_again(&self, ended: &Arc<Running>, error: String) -> String {
        let start = {
            let mut slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let slot = slots.iter_mut().find(|slot| slot.running.as_ref().is_some_and(|running| Arc::ptr_eq(running, ended)));
            slot.and_then(|slot| {
                slot.running = None;
                slot.start.clone()
            })
        };
        let Some(start) = start else { return error };
        let outcome = start();
        let mut slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let slot = slots.iter_mut().find(|slot| slot.start.as_ref().is_some_and(|known| Arc::ptr_eq(known, &start)));
        match (outcome, slot) {
            (Ok(running), Some(slot)) => {
                slot.running = Some(running);
                slot.failed = None;
                format!("{error}. It has been started again: make the call again if it is still needed")
            }
            (Err(reason), Some(slot)) => {
                slot.failed = Some(reason.clone());
                format!("{error}, and it did not start again: {reason}")
            }
            _ => error,
        }
    }

    /// Each server's name, and whether the person trusts what it returns.
    pub fn trust(&self) -> Vec<(String, bool)> {
        self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter().map(|slot| (slot.name.clone(), slot.trusted)).collect()
    }

    /// Whether what this tool returns comes from a server the person trusts. The same server that a call of it reaches.
    pub fn trusts_tool(&self, tool: &str) -> bool {
        let slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        slots.iter().find(|slot| slot.running.as_ref().is_some_and(|running| running.names.contains_key(tool))).is_some_and(|slot| slot.trusted)
    }

    /// The person's word on a server, from now on and in its file. Nothing is written unless the file lists it: one that cannot be read must not be replaced by one with less in it.
    pub fn set_trusted(&self, server: &str, trusted: bool) -> Result<(), String> {
        let file = self.file.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone().ok_or("no file lists the servers")?;
        let mut config = load_config(&file);
        config.servers.get_mut(server).ok_or_else(|| format!("{}: lists no server {server}", file.display()))?.trusted = trusted;
        let text = serde_json::to_string_pretty(&config).map_err(|error| error.to_string())?;
        let partial = file.with_extension("partial");
        std::fs::write(&partial, text).map_err(|error| format!("{}: {error}", partial.display()))?;
        std::fs::rename(&partial, &file).map_err(|error| format!("{}: {error}", file.display()))?;
        if let Some(slot) = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter_mut().find(|slot| slot.name == server) {
            slot.trusted = trusted;
        }
        Ok(())
    }

    /// A server that answers from a script and is already up, for the agent's tests.
    #[cfg(test)]
    pub(crate) fn scripted(server: &str, tools: &[Value], lines: &[Value], trusted: bool) -> Arc<Hub> {
        let script: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let (names, schemas) = place_tools(server, tools);
        let running = Arc::new(Running { rpc: Mutex::new(Rpc::over(Box::new(std::io::sink()), std::io::Cursor::new(script.into_bytes()))), names, schemas, _child: None, #[cfg(windows)] _ending: None });
        let hub = Arc::new(Hub::default());
        hub.slots.lock().unwrap().push(Slot { name: server.into(), running: Some(running), trusted, ..Slot::default() });
        hub
    }

    /// One line a server, for the person.
    pub fn status(&self) -> Vec<String> {
        self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter().map(|slot| match (&slot.running, &slot.failed) {
            (Some(running), _) => format!("{}: {} tools", slot.name, running.names.len()),
            (_, Some(error)) => format!("{}: did not start: {error}", slot.name),
            _ => format!("{}: starting", slot.name),
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn claudes_file_is_read_as_it_is() {
        let text = r#"{"numStartups": 3, "mcpServers": {"files": {"type": "stdio", "command": "npx", "args": ["-y", "server"], "env": {"K": "v"}}, "remote": {"type": "http", "url": "https://x"}}}"#;
        let found = servers_of_claude(text);
        assert_eq!(found.keys().collect::<Vec<_>>(), ["files"]);
        assert_eq!(found["files"].args, ["-y", "server"]);
        assert_eq!(found["files"].env["K"], "v");
        assert!(servers_of_claude("not json").is_empty());
    }

    #[test]
    fn codexs_tables_are_read_as_they_are() {
        let text = "model = \"x\"\n[mcp_servers.unity_mcp]\ncommand = \"C:\\\\tools\\\\unity.exe\"\nargs = [\"--stdio\", \"--port\", \"6400\"]\n[mcp_servers.unity_mcp.env]\nKEY = \"value\"\n[mcp_servers.url_only]\nurl = \"https://x\"\n[other]\ncommand = \"no\"\n";
        let found = servers_of_codex(text);
        assert_eq!(found.keys().collect::<Vec<_>>(), ["unity_mcp"]);
        assert_eq!(found["unity_mcp"].command, "C:\\tools\\unity.exe");
        assert_eq!(found["unity_mcp"].args, ["--stdio", "--port", "6400"]);
        assert_eq!(found["unity_mcp"].env["KEY"], "value");
    }

    #[test]
    fn an_import_adds_and_keeps_what_was_there() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("deep/mcp.json");
        let one = |command: &str| ServerConfig { command: command.into(), ..ServerConfig::default() };
        assert_eq!(import(&file, BTreeMap::from([("a".to_string(), one("first"))])).unwrap(), 1);
        assert_eq!(import(&file, BTreeMap::from([("a".to_string(), one("second")), ("b".to_string(), one("third"))])).unwrap(), 1);
        let config = load_config(&file);
        assert_eq!(config.servers["a"].command, "first");
        assert_eq!(config.servers["b"].command, "third");
    }

    #[test]
    fn names_are_what_a_model_api_takes() {
        assert_eq!(tool_name("my server", "do.it"), "mcp__my_server__do_it");
        assert_eq!(tool_name(&"s".repeat(80), "t").len(), 64);
    }

    #[test]
    fn a_result_is_read_as_words() {
        let result = json!({"content": [{"type": "text", "text": "one"}, {"type": "image", "data": "x"}, {"type": "text", "text": "two"}]});
        assert_eq!(result_text(&result), "one\n[image content is not shown]\ntwo");
        assert_eq!(result_text(&json!({"structuredContent": {"a": 1}})), r#"{"a":1}"#);
    }

    #[test]
    fn the_handshake_lists_the_tools_across_pages() {
        // A server that has said its piece, in order, whatever it is asked: its reader ends when these do.
        let lines = [
            json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": PROTOCOL, "capabilities": {"tools": {}}}}),
            json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {}}),
            json!({"jsonrpc": "2.0", "id": 2, "result": {"tools": [{"name": "a"}], "nextCursor": "more"}}),
            json!({"jsonrpc": "2.0", "id": 3, "result": {"tools": [{"name": "b", "description": "d", "inputSchema": {"type": "object"}}]}}),
        ];
        let script: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let mut rpc = Rpc::over(Box::new(std::io::sink()), Cursor::new(script.into_bytes()));
        let tools = rpc.start().unwrap();
        assert_eq!(tools.len(), 2);
        let (known, real, schema) = schema_of("srv", &tools[1]).unwrap();
        assert_eq!((known.as_str(), real.as_str()), ("mcp__srv__b", "b"));
        assert_eq!(schema["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn a_server_that_ends_is_said_to_have_ended() {
        let mut rpc = Rpc::over(Box::new(std::io::sink()), Cursor::new(Vec::new()));
        assert_eq!(rpc.call("x", json!({}), Duration::from_secs(5), None).unwrap_err(), "the server has ended");
    }

    #[test]
    fn a_call_can_be_stopped() {
        let (_keep, reader) = std::sync::mpsc::channel::<u8>();
        drop(reader);
        struct Silent;
        impl std::io::Read for Silent {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_secs(60));
                Ok(0)
            }
        }
        let mut rpc = Rpc::over(Box::new(std::io::sink()), BufReader::new(Silent));
        let stop = AtomicBool::new(true);
        assert_eq!(rpc.call("x", json!({}), Duration::from_secs(30), Some(&stop)).unwrap_err(), "stopped");
    }

    fn tool(name: &str) -> Value {
        json!({"name": name, "inputSchema": {"properties": {}}})
    }

    #[test]
    fn two_tools_never_share_a_name_and_the_order_is_the_tools_own() {
        let long = "t".repeat(70);
        let tools = [tool(&format!("{long}b")), tool(&format!("{long}a")), tool("do.it"), tool("do_it")];
        let (names, schemas) = place_tools("srv", &tools);
        assert_eq!((names.len(), schemas.len()), (4, 4), "{names:?}");
        assert!(names.keys().all(|name| name.len() <= 64), "{names:?}");
        let spelled: Vec<&str> = schemas.iter().map(|schema| schema["function"]["name"].as_str().unwrap()).collect();
        assert!(spelled.iter().all(|name| names.contains_key(*name)));
        let (_, reversed) = place_tools("srv", &[tool("do_it"), tool("do.it"), tool(&format!("{long}a")), tool(&format!("{long}b"))]);
        assert_eq!(serde_json::to_string(&schemas).unwrap(), serde_json::to_string(&reversed).unwrap(), "whatever order it lists them in");
        assert_eq!(schemas[0]["function"]["parameters"]["type"], "object");
    }

    fn running(lines: &[Value], names: &[&str]) -> Arc<Running> {
        let script: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let (names, schemas) = place_tools("srv", &names.iter().map(|name| tool(name)).collect::<Vec<_>>());
        Arc::new(Running {
            rpc: Mutex::new(Rpc::over(Box::new(std::io::sink()), Cursor::new(script.into_bytes()))),
            names,
            schemas,
            _child: None,
            #[cfg(windows)]
            _ending: None,
        })
    }

    #[test]
    fn a_server_that_ended_starts_again_for_the_next_call_and_this_one_is_not_repeated() {
        let again = json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": "back"}]}});
        let start: Starter = Arc::new(move || Ok(running(std::slice::from_ref(&again), &["t"])));
        let hub = Hub::default();
        hub.slots.lock().unwrap().push(Slot { name: "srv".into(), start: Some(start), running: Some(running(&[], &["t"])), ..Slot::default() });
        let cancel = AtomicBool::new(false);
        let said = hub.call("mcp__srv__t", &json!({}), &cancel).unwrap_err();
        assert!(said.contains("has ended") && said.contains("started again"), "{said}");
        assert_eq!(hub.call("mcp__srv__t", &json!({}), &cancel).unwrap(), "back");
    }

    #[test]
    fn a_server_that_will_not_start_again_says_so_and_why() {
        let start: Starter = Arc::new(|| Err("no such program".to_string()));
        let hub = Hub::default();
        hub.slots.lock().unwrap().push(Slot { name: "srv".into(), start: Some(start), running: Some(running(&[], &["t"])), ..Slot::default() });
        let said = hub.call("mcp__srv__t", &json!({}), &AtomicBool::new(false)).unwrap_err();
        assert!(said.contains("no such program"), "{said}");
        assert_eq!(hub.status(), ["srv: did not start: no such program"]);
    }

    #[test]
    fn a_stop_does_not_wait_for_another_sessions_call() {
        let server = running(&[], &["t"]);
        let hub = Hub::default();
        hub.slots.lock().unwrap().push(Slot { name: "srv".into(), running: Some(server.clone()), ..Slot::default() });
        let _busy = server.rpc.lock().unwrap();
        assert_eq!(hub.call("mcp__srv__t", &json!({}), &AtomicBool::new(true)).unwrap_err(), "stopped");
    }

    #[test]
    fn two_servers_with_one_clean_name_offer_a_tool_once() {
        let hub = Hub::default();
        for name in ["a b", "a_b"] {
            let (names, schemas) = place_tools(name, &[tool("t")]);
            let server = Arc::new(Running { rpc: Mutex::new(Rpc::over(Box::new(std::io::sink()), Cursor::new(Vec::new()))), names, schemas, _child: None, #[cfg(windows)] _ending: None });
            hub.slots.lock().unwrap().push(Slot { name: name.into(), running: Some(server), ..Slot::default() });
        }
        assert_eq!(hub.schemas().len(), 1);
    }

    #[test]
    fn trust_is_the_persons_word_kept_in_the_file_and_for_the_server_a_call_reaches() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        std::fs::write(&file, r#"{"mcpServers": {"srv": {"command": "no-such-program-here", "args": ["a"]}}}"#).unwrap();
        let hub = Hub::start(&file);
        assert_eq!(hub.trust(), [("srv".to_string(), false)]);
        hub.set_trusted("srv", true).unwrap();
        assert_eq!(hub.trust(), [("srv".to_string(), true)]);
        let saved = load_config(&file).servers["srv"].clone();
        assert!(saved.trusted && saved.args == ["a"], "{saved:?}");
        assert!(Hub::start(&file).trust()[0].1, "and after a restart");
        hub.set_trusted("srv", false).unwrap();
        assert!(!std::fs::read_to_string(&file).unwrap().contains("trusted"), "nothing said when nothing is meant");
        assert!(hub.set_trusted("other", true).is_err());
        // A file that cannot be read is not written over.
        std::fs::write(&file, "not json").unwrap();
        assert!(hub.set_trusted("srv", true).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "not json");
        assert!(Hub::default().set_trusted("srv", true).is_err());
        // Two servers whose names clean to one: the one a call would reach.
        let both = Hub::default();
        for (name, trusted) in [("a b", false), ("a_b", true)] {
            let (names, schemas) = place_tools(name, &[tool("t")]);
            let server = Arc::new(Running { rpc: Mutex::new(Rpc::over(Box::new(std::io::sink()), Cursor::new(Vec::new()))), names, schemas, _child: None, #[cfg(windows)] _ending: None });
            both.slots.lock().unwrap().push(Slot { name: name.into(), running: Some(server), trusted, ..Slot::default() });
        }
        assert!(!both.trusts_tool("mcp__a_b__t"));
        assert!(!both.trusts_tool("mcp__nothing__t"));
    }

    #[test]
    fn an_empty_hub_has_nothing_to_offer() {
        let hub = Hub::default();
        assert!(hub.schemas().is_empty() && hub.status().is_empty());
        assert!(hub.call("mcp__x__y", &json!({}), &AtomicBool::new(false)).unwrap_err().contains("not running"));
    }
}
