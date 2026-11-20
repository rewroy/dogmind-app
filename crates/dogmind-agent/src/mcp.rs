//! MCP servers, listed in this app's own `mcp.json` (in the `mcpServers` form other apps use too, so that theirs can be imported): a program run over
//! stdio, with its arguments and its environment, or a server at a URL that speaks Streamable HTTP, with the headers it is sent. They start with the
//! app, start again when the person changes them in Settings, and their tools join the session's as `mcp__server__tool`.
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
/// A server's tool list is read to these and no further: one that always has another page is not worth the app's waiting on it, and what was read is kept.
const MAX_PAGES: usize = 100;
const MAX_TOOLS: usize = 1000;
/// How much of a server's name goes into its tools' names.
const MAX_PART: usize = 20;
const ENDED: &str = "the server has ended";
const PIPE: &str = "the server's pipe";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// `http` for a server at `url` (Streamable HTTP); none, or `stdio`, for a program.
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// Sent with every request to `url`: an `Authorization` among them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// The person's word that what this server returns is not a stranger's. In this app's own file, which neither a model's tools nor a command can write.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trusted: bool,
}

impl ServerConfig {
    /// A server at a URL, and not a program.
    pub fn is_http(&self) -> bool {
        !self.url.is_empty() && self.command.is_empty()
    }

    /// What is wrong with it as the person gave it, if anything.
    pub fn problem(&self) -> Option<String> {
        match (self.command.trim().is_empty(), self.url.trim().is_empty()) {
            (true, true) => Some("a command to run, or the URL of a server, is needed".to_string()),
            (false, false) => Some("a command or a URL, not both".to_string()),
            (true, false) if !(self.url.starts_with("http://") || self.url.starts_with("https://")) => Some(format!("{}: a URL begins with http:// or https://", self.url)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(rename = "mcpServers", default)]
    pub servers: BTreeMap<String, ServerConfig>,
}

pub fn load_config(file: &Path) -> Config {
    std::fs::read_to_string(file).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

/// Claude Code's own list of servers: the `mcpServers` of `~/.claude.json`, programs and HTTP ones (an SSE one, of the protocol's older transport, is left out).
pub fn servers_of_claude(text: &str) -> BTreeMap<String, ServerConfig> {
    let value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let mut found = BTreeMap::new();
    for (name, server) in value.get("mcpServers").and_then(Value::as_object).into_iter().flatten() {
        if let Ok(mut config) = serde_json::from_value::<ServerConfig>(server.clone()) {
            if config.kind == "sse" {
                continue;
            }
            if config.kind == "stdio" {
                config.kind.clear();
            }
            if config.problem().is_none() {
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
    let (mut current, mut in_env, mut in_headers) = (None, false, false);
    for line in text.lines().map(str::trim) {
        if let Some(table) = line.strip_prefix('[').and_then(|line| line.strip_suffix(']')) {
            (current, in_env, in_headers) = match table.strip_prefix("mcp_servers.") {
                Some(rest) => match (rest.strip_suffix(".env"), rest.strip_suffix(".http_headers")) {
                    (Some(name), _) => (Some(name.trim_matches('"').to_string()), true, false),
                    (_, Some(name)) => (Some(name.trim_matches('"').to_string()), false, true),
                    _ if rest.contains('.') => (None, false, false),
                    _ => (Some(rest.trim_matches('"').to_string()), false, false),
                },
                None => (None, false, false),
            };
            continue;
        }
        let (Some(name), Some((key, value))) = (&current, line.split_once('=')) else { continue };
        let (key, value) = (key.trim(), value.trim());
        let server = found.entry(name.clone()).or_default();
        if in_env {
            server.env.insert(key.to_string(), unquote(value));
        } else if in_headers {
            server.headers.insert(key.trim_matches('"').to_string(), unquote(value));
        } else if key == "url" {
            server.url = unquote(value);
        } else if key == "command" {
            server.command = unquote(value);
        } else if key == "args" {
            let inner = value.trim_start_matches('[').trim_end_matches(']');
            server.args = inner.split("\",").map(unquote).filter(|arg| !arg.is_empty()).collect();
        }
    }
    found.retain(|_, server| server.problem().is_none());
    found
}

/// The person's server, as they gave it in Settings, in place of the one called `before` (none for a new one). The file is written whole, through a partial one, so
/// that it is never left half written; one that cannot be read is not written over with less in it.
pub fn save_server(file: &Path, before: Option<&str>, name: &str, server: ServerConfig) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a name is needed".to_string());
    }
    if let Some(problem) = server.problem() {
        return Err(problem);
    }
    let mut config = readable_config(file)?;
    if before != Some(name) && config.servers.contains_key(name) {
        return Err(format!("there is a server called {name} already"));
    }
    if let Some(before) = before {
        config.servers.remove(before);
    }
    config.servers.insert(name.to_string(), server);
    write_config(file, &config)
}

/// Arguments as one line for a person to read and change: one with a space or a quote in it is quoted, as Windows reads a command line, so that
/// [`args_of_line`] gives them back as they were.
pub fn line_of_args(args: &[String]) -> String {
    args.iter().map(|arg| quoted_arg(arg)).collect::<Vec<_>>().join(" ")
}

fn quoted_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| c.is_whitespace() || c == '"') {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
        } else {
            if c == '"' {
                out.extend(std::iter::repeat_n('\\', backslashes + 1));
            }
            backslashes = 0;
        }
        out.push(c);
    }
    out.extend(std::iter::repeat_n('\\', backslashes));
    out.push('"');
    out
}

/// A line of arguments read as Windows reads a program's: separated by spaces, a quoted one kept whole, and a backslash literal unless quotes follow it.
pub fn args_of_line(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            return args;
        }
        let (mut arg, mut quoted) = (String::new(), false);
        while let Some(c) = chars.peek().copied() {
            if !quoted && c.is_whitespace() {
                break;
            }
            chars.next();
            match c {
                '\\' => {
                    let mut count = 1;
                    while chars.peek() == Some(&'\\') {
                        chars.next();
                        count += 1;
                    }
                    if chars.peek() == Some(&'"') {
                        arg.extend(std::iter::repeat_n('\\', count / 2));
                        if count % 2 == 1 {
                            chars.next();
                            arg.push('"');
                        }
                    } else {
                        arg.extend(std::iter::repeat_n('\\', count));
                    }
                }
                '"' if quoted && chars.peek() == Some(&'"') => {
                    chars.next();
                    arg.push('"');
                }
                '"' => quoted = !quoted,
                c => arg.push(c),
            }
        }
        args.push(arg);
    }
}

/// Names and values a line each, `NAME=value` for a program's environment or `Name: value` for a server's headers, as Settings shows them.
pub fn lines_of_pairs(pairs: &BTreeMap<String, String>, separator: char) -> String {
    let between = if separator == ':' { ": " } else { "=" };
    pairs.iter().map(|(name, value)| format!("{name}{between}{value}")).collect::<Vec<_>>().join("\r\n")
}

/// The same lines read back. A line with no separator, or with no name before it, is the error: its number (from 1) and what it says.
pub fn pairs_of_lines(text: &str, separator: char) -> Result<BTreeMap<String, String>, (usize, String)> {
    let mut pairs = BTreeMap::new();
    for (at, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line.split_once(separator) {
            Some((name, value)) if !name.trim().is_empty() => {
                pairs.insert(name.trim().to_string(), value.trim().to_string());
            }
            _ => return Err((at + 1, line.to_string())),
        }
    }
    Ok(pairs)
}

/// The server called `name`, gone from the file.
pub fn remove_server(file: &Path, name: &str) -> Result<(), String> {
    let mut config = readable_config(file)?;
    config.servers.remove(name).ok_or_else(|| format!("{}: lists no server {name}", file.display()))?;
    write_config(file, &config)
}

/// The file's servers; a file that is there and cannot be read is an error, so that nothing is written over it.
fn readable_config(file: &Path) -> Result<Config, String> {
    match std::fs::read_to_string(file) {
        Ok(text) => serde_json::from_str(&text).map_err(|error| format!("{}: {error}", file.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(format!("{}: {error}", file.display())),
    }
}

fn write_config(file: &Path, config: &Config) -> Result<(), String> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(config).map_err(|error| error.to_string())?;
    let partial = file.with_extension("partial");
    std::fs::write(&partial, text).map_err(|error| format!("{}: {error}", partial.display()))?;
    std::fs::rename(&partial, file).map_err(|error| format!("{}: {error}", file.display()))
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

/// The JSON-RPC of one server, whichever pipes it runs over; or, for a server at a URL, over HTTP (`http`), where the pipes are not used.
struct Rpc {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    answers: Receiver<Value>,
    next: u64,
    http: Option<Mutex<HttpLink>>,
}

/// A server at a URL, as Streamable HTTP has it: each message a POST, answered in JSON or in an event stream that carries the answer; the session's id the
/// server gave in its answer to `initialize`, sent back with each request after it.
struct HttpLink {
    url: crate::http::Url,
    headers: Vec<(String, String)>,
    session: Option<String>,
    initialized: bool,
}

/// How long a connection to a server at a URL is waited for.
const HTTP_CONNECT: Duration = Duration::from_secs(10);

impl HttpLink {
    fn new(url: &str, headers: &BTreeMap<String, String>) -> Result<HttpLink, String> {
        let url = crate::http::Url::parse(url)?;
        Ok(HttpLink { url, headers: headers.iter().map(|(name, value)| (name.clone(), value.clone())).collect(), session: None, initialized: false })
    }

    /// Posts one message; its answer when it has an `id` (a request), none for a notification. Gives up at `limit`, or when `cancel` is set.
    fn post(&mut self, message: &Value, limit: Duration, cancel: Option<&AtomicBool>) -> Result<Option<Value>, String> {
        let id = message.get("id").and_then(Value::as_u64);
        let method = message.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let mut headers = self.headers.clone();
        headers.push(("Content-Type".into(), "application/json".into()));
        headers.push(("Accept".into(), "application/json, text/event-stream".into()));
        if self.initialized {
            headers.push(("MCP-Protocol-Version".into(), PROTOCOL.into()));
        }
        if let Some(session) = &self.session {
            headers.push(("Mcp-Session-Id".into(), session.clone()));
        }
        // (the request's own flag: set by the person's stop, or once the limit is past, by a watcher of both that ends with the request)
        let give_up = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let person = cancel.map(|flag| flag as *const AtomicBool as usize);
        {
            let (give_up, done) = (give_up.clone(), done.clone());
            let started = Instant::now();
            std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    // SAFETY: the flag outlives this request, which `done` ends this watch before returning from.
                    let stopped = person.is_some_and(|flag| unsafe { (*(flag as *const AtomicBool)).load(Ordering::Relaxed) });
                    if stopped || started.elapsed() > limit {
                        give_up.store(true, Ordering::Relaxed);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            });
        }
        let outcome = self.exchange(&message.to_string(), &headers, id, &give_up);
        done.store(true, Ordering::Relaxed);
        // (the watcher may still read the person's flag for one more look: it is told first, and the flag is not read after this)
        std::thread::sleep(Duration::from_millis(60));
        match outcome {
            Err(_) if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) => Err("stopped".to_string()),
            Err(_) if give_up.load(Ordering::Relaxed) => Err(format!("no answer to {method} within {} s", limit.as_secs())),
            Err(error) => Err(error),
            Ok(answer) => {
                if method == "initialize" {
                    self.initialized = true;
                }
                Ok(answer)
            }
        }
    }

    fn exchange(&mut self, body: &str, headers: &[(String, String)], id: Option<u64>, give_up: &AtomicBool) -> Result<Option<Value>, String> {
        let mut response = crate::http::request("POST", &self.url, headers, Some(body), HTTP_CONNECT, give_up).map_err(|error| format!("{}: {error}", self.url.host))?;
        if let Some(session) = response.header("mcp-session-id") {
            self.session = Some(session.to_string());
        }
        if response.status == 202 || id.is_none() {
            return if response.status < 300 { Ok(None) } else { Err(format!("the server answered {}", response.status)) };
        }
        if response.status >= 300 {
            let text: String = response.text().unwrap_or_default().chars().take(300).collect();
            return Err(format!("the server answered {}: {}", response.status, text.trim()));
        }
        let streamed = response.header("content-type").is_some_and(|kind| kind.to_ascii_lowercase().starts_with("text/event-stream"));
        let is_ours = |message: &Value| message.get("id").and_then(Value::as_u64) == id && message.get("method").is_none();
        if !streamed {
            let text = response.text().map_err(|error| error.to_string())?;
            let answer: Value = serde_json::from_str(&text).map_err(|error| format!("the server's answer is not JSON: {error}"))?;
            let found = match answer {
                Value::Array(messages) => messages.into_iter().find(is_ours),
                message => Some(message).filter(is_ours),
            };
            return found.map(Some).ok_or_else(|| "the server's answer was not to this request".to_string());
        }
        // An event stream: its events' data, until the answer to this request is among them.
        let mut pending = String::new();
        loop {
            let Some(bytes) = response.next().map_err(|error| error.to_string())? else {
                return Err("the server's event stream ended with no answer to this request".to_string());
            };
            pending.push_str(&String::from_utf8_lossy(&bytes).replace("\r\n", "\n"));
            while let Some(end) = pending.find("\n\n") {
                let event: String = pending.drain(..end + 2).collect();
                let data: Vec<&str> = event.lines().filter_map(|line| line.strip_prefix("data:")).map(|data| data.strip_prefix(' ').unwrap_or(data)).collect();
                if let Ok(message) = serde_json::from_str::<Value>(&data.join("\n")) {
                    if is_ours(&message) {
                        return Ok(Some(message));
                    }
                }
            }
        }
    }
}

impl Drop for HttpLink {
    /// The session ends with this side of it, as the protocol asks of a client that is done.
    fn drop(&mut self) {
        let Some(session) = self.session.take() else { return };
        let mut headers = self.headers.clone();
        headers.push(("Mcp-Session-Id".into(), session));
        let never = AtomicBool::new(false);
        let _ = crate::http::request_within("DELETE", &self.url, &headers, Duration::from_secs(2), &never, Duration::from_secs(3));
    }
}

impl Rpc {
    /// Reads the server's lines on a thread of its own; it answers a ping itself, as the server may wait on one.
    fn over(writer: Box<dyn Write + Send>, reader: impl BufRead + Send + 'static) -> Rpc {
        let writer = Arc::new(Mutex::new(writer));
        let (sender, answers) = channel();
        let replies = writer.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut line = Vec::new();
            loop {
                line.clear();
                // (a line is read as bytes: one that is not UTF-8, a script's message in the machine's own encoding, must not end the reader)
                if matches!(reader.read_until(b'\n', &mut line), Ok(0) | Err(_)) {
                    return;
                }
                let Ok(message) = serde_json::from_str::<Value>(&String::from_utf8_lossy(&line)) else { continue };
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
        Rpc { writer, answers, next: 1, http: None }
    }

    /// For a server at a URL: its messages go by HTTP, and the pipes are none.
    fn http(link: HttpLink) -> Rpc {
        let (_, answers) = channel();
        Rpc { writer: Arc::new(Mutex::new(Box::new(std::io::sink()))), answers, next: 1, http: Some(Mutex::new(link)) }
    }

    fn send(&self, message: &Value) -> Result<(), String> {
        if let Some(http) = &self.http {
            return http.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).post(message, Duration::from_secs(30), None).map(|_| ());
        }
        let mut out = self.writer.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        writeln!(out, "{message}").and_then(|()| out.flush()).map_err(|error| format!("{PIPE}: {error}"))
    }

    fn ask_to_stop(&self, id: u64) {
        let _ = self.send(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": id}}));
    }

    fn call(&mut self, method: &str, params: Value, limit: Duration, cancel: Option<&AtomicBool>) -> Result<Value, String> {
        let id = self.next;
        self.next += 1;
        if let Some(http) = &self.http {
            let asked = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            let outcome = http.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).post(&asked, limit, cancel);
            return match outcome {
                Ok(Some(answer)) => match answer.get("error") {
                    Some(error) => Err(error.get("message").and_then(Value::as_str).map_or_else(|| error.to_string(), str::to_string)),
                    None => Ok(answer.get("result").cloned().unwrap_or(Value::Null)),
                },
                Ok(None) => Err(format!("no answer to {method}")),
                Err(error) => {
                    if method != "initialize" && (error == "stopped" || error.starts_with("no answer")) {
                        self.ask_to_stop(id);
                    }
                    Err(error)
                }
            };
        }
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let started = Instant::now();
        loop {
            if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                self.ask_to_stop(id);
                return Err("stopped".to_string());
            }
            if started.elapsed() > limit {
                let said = format!("no answer to {method} within {} s", limit.as_secs());
                // (the handshake is not cancelled, which the protocol does not allow: a server that does not answer it is ended)
                if method == "initialize" {
                    return Err(said);
                }
                self.ask_to_stop(id);
                return Err(format!("{said}: it may still be running, or have done it already; it was asked to stop"));
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

    /// The handshake, and what tools the server has, each step given this long to answer.
    fn start(&mut self, limit: Duration) -> Result<Vec<Value>, String> {
        self.call("initialize", json!({"protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": {"name": "dogmind-app", "version": env!("CARGO_PKG_VERSION")}}), limit, None)?;
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for page_number in 1.. {
            let params = cursor.as_ref().map_or(json!({}), |cursor| json!({"cursor": cursor}));
            let page = self.call("tools/list", params, limit, None)?;
            tools.extend(page.get("tools").and_then(Value::as_array).cloned().unwrap_or_default());
            cursor = page.get("nextCursor").and_then(Value::as_str).map(str::to_string);
            // (a server that goes on past this keeps what it has given, and does not fail over the rest)
            if cursor.is_none() || page_number >= MAX_PAGES || tools.len() >= MAX_TOOLS {
                break;
            }
        }
        Ok(tools)
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
    crate::tools::spill("mcp", &text, MAX_RESULT)
}

/// A name the model's API takes: letters, digits, `_` and `-`, 64 at most.
fn tool_name(server: &str, tool: &str) -> String {
    let clean = |text: &str| text.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>();
    format!("mcp__{}__{}", clean(server), clean(tool)).chars().take(64).collect()
}

/// The first `most` characters of a name, with no `_` left at the end of them.
fn cut(text: &str, most: usize) -> String {
    text.chars().take(most).collect::<String>().trim_end_matches('_').to_string()
}

/// What a server's tools are called after: its name with what a model API refuses made `_`, a run of them made one and none left at either end (so the first `__` after
/// `mcp__` ends it, whatever a tool is called), and at most `MAX_PART` characters. Two servers can come to one, which `name_parts` tells apart.
fn name_part(server: &str) -> String {
    let mut part = String::new();
    for c in server.chars() {
        let c = if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' };
        if c != '_' || !part.ends_with('_') {
            part.push(c);
        }
    }
    let part = cut(part.trim_matches('_'), MAX_PART);
    if part.is_empty() { "server".to_string() } else { part }
}

/// The name part of each server, in the order they are given, and no two the same: a later one that comes to the same as an earlier one gets a number. It goes by the
/// configuration and not by which server starts first, so a tool has the same name every time, and no two servers share a tool's name.
fn name_parts<'a>(servers: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    for server in servers {
        let base = name_part(server);
        let (mut part, mut again) = (base.clone(), 1);
        while parts.contains(&part) {
            again += 1;
            let suffix = format!("_{again}");
            part = format!("{}{suffix}", cut(&base, MAX_PART - suffix.len()));
        }
        parts.push(part);
    }
    parts
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

/// A server's process. One the job holds ends as the job closes, with all it started. One it does not hold (no job could be made, or the server was in one already)
/// is ended here, with what it started: a `Child` dropped leaves its process running, `npx` and `node` past the app.
struct Process {
    child: Child,
    #[cfg(windows)]
    ending: Option<Ending>,
}

impl Drop for Process {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.ending.is_some() {
            return;
        }
        // (its PID is not another's while its handle is held here)
        if matches!(self.child.try_wait(), Ok(None)) {
            crate::tools::kill_tree(self.child.id());
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

struct Running {
    rpc: Mutex<Rpc>,
    /// What each tool is called by its server, by the name the model knows it by.
    names: BTreeMap<String, String>,
    schemas: Vec<Value>,
    _process: Option<Process>,
}

type Starter = Arc<dyn Fn() -> Result<Arc<Running>, String> + Send + Sync>;

#[derive(Default)]
struct Slot {
    name: String,
    /// How to bring it back when it has ended.
    start: Option<Starter>,
    /// While it starts, none of these.
    running: Option<Arc<Running>>,
    /// The names of the tools of a server that has ended, so that a call for one can be told what became of it.
    had: Vec<String>,
    /// While it starts again, on a thread of its own.
    restarting: bool,
    failed: Option<String>,
    trusted: bool,
    /// What it was started from, and its part of its tools' names: a change of either starts it again.
    config: Option<ServerConfig>,
    part: String,
}

/// How a server is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Starting,
    /// Up, with this many tools.
    Up(usize),
    /// It did not start, or did not start again, and why.
    Failed(String),
}

/// The servers, shared by every session.
#[derive(Default)]
pub struct Hub {
    slots: Arc<Mutex<Vec<Slot>>>,
    /// The file the servers were listed in.
    file: Mutex<Option<PathBuf>>,
}

fn start_server(config: &ServerConfig) -> Result<Running, String> {
    start_server_within(config, START_LIMIT, true)
}

/// Starts a server, each step of its handshake given `limit` to answer. A job holds it unless `job` is false (for a test of a server that none does).
fn start_server_within(config: &ServerConfig, limit: Duration, job: bool) -> Result<Running, String> {
    if config.is_http() {
        let mut rpc = Rpc::http(HttpLink::new(&config.url, &config.headers)?);
        let tools = rpc.start(limit)?;
        return Ok(Running { rpc: Mutex::new(rpc), names: BTreeMap::new(), schemas: tools, _process: None });
    }
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
    let child = command.spawn().map_err(|error| format!("{}: {error}", config.command))?;
    #[cfg(windows)]
    let ending = if job { ends_with_this_process(&child) } else { None };
    #[cfg(not(windows))]
    let _ = job;
    // (from here on, whatever fails, the process ends as this is dropped: a handshake that gets no answer is otherwise a server that goes on running)
    let mut process = Process {
        child,
        #[cfg(windows)]
        ending,
    };
    let writer = process.child.stdin.take().ok_or("no stdin")?;
    let reader = process.child.stdout.take().ok_or("no stdout")?;
    let mut rpc = Rpc::over(Box::new(writer), BufReader::new(reader));
    let tools = rpc.start(limit)?;
    Ok(Running { rpc: Mutex::new(rpc), names: BTreeMap::new(), schemas: tools, _process: Some(process) })
}

fn schema_of(server: &str, part: &str, tool: &Value) -> Option<(String, String, Value)> {
    let real = tool.get("name")?.as_str()?.to_string();
    let name = tool_name(part, &real);
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
/// one name gets the second a number: the services refuse the whole request over a name twice, and so every request. A server's own part (see `name_parts`) keeps its
/// names from another server's.
fn place_tools(server: &str, part: &str, tools: &[Value]) -> (BTreeMap<String, String>, Vec<Value>) {
    let mut listed: Vec<(String, String, Value)> = tools.iter().filter_map(|tool| schema_of(server, part, tool)).collect();
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

fn ready(name: &str, part: &str, config: &ServerConfig) -> Result<Arc<Running>, String> {
    let mut running = start_server(config)?;
    let tools = std::mem::take(&mut running.schemas);
    (running.names, running.schemas) = place_tools(name, part, &tools);
    Ok(Arc::new(running))
}

impl Hub {
    /// Starts what the file lists, each on its own thread, as one that is slow or broken must not hold up the app.
    pub fn start(file: &Path) -> Arc<Hub> {
        let hub = Arc::new(Hub::default());
        *hub.file.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(file.to_path_buf());
        hub.reload();
        hub
    }

    /// The file read again, as the person changed it in Settings: a server no longer there ends, a new one starts, and one whose configuration changed starts
    /// again with it; one that did not change goes on as it was (its trust as the file now says). Says the names of those started.
    pub fn reload(&self) -> Vec<String> {
        let Some(file) = self.file.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone() else { return Vec::new() };
        let servers = load_config(&file).servers;
        let parts = name_parts(servers.keys());
        let mut started = Vec::new();
        let mut slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // (one that is gone ends as its last call is done with it: its process with the last hold on it)
        slots.retain(|slot| servers.contains_key(&slot.name));
        for ((name, config), part) in servers.into_iter().zip(parts) {
            let at = slots.iter().position(|slot| slot.name == name);
            if let Some(slot) = at.map(|at| &mut slots[at]) {
                let mut same = config.clone();
                same.trusted = slot.config.as_ref().map_or(config.trusted, |was| was.trusted);
                if slot.config.as_ref() == Some(&same) && slot.part == part {
                    slot.trusted = config.trusted;
                    slot.config = Some(config);
                    continue;
                }
            }
            let start: Starter = {
                let (name, part, config) = (name.clone(), part.clone(), config.clone());
                Arc::new(move || ready(&name, &part, &config))
            };
            let slot = Slot { name: name.clone(), start: Some(start.clone()), trusted: config.trusted, config: Some(config), part, ..Slot::default() };
            match at {
                Some(at) => slots[at] = slot,
                None => slots.push(slot),
            }
            started.push(name.clone());
            let shared = self.slots.clone();
            std::thread::spawn(move || {
                let outcome = start();
                let mut slots = shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                // (the slot this start is for, and not one that took its name since: a change made while it started has started another)
                if let Some(slot) = slots.iter_mut().find(|slot| slot.start.as_ref().is_some_and(|known| Arc::ptr_eq(known, &start))) {
                    match outcome {
                        Ok(running) => slot.running = Some(running),
                        Err(error) => slot.failed = Some(error),
                    }
                }
            });
        }
        // (in the order of the file, as the tools' names are given)
        let order = load_config(&file).servers.into_keys().collect::<Vec<_>>();
        slots.sort_by_key(|slot| order.iter().position(|name| *name == slot.name).unwrap_or(usize::MAX));
        started
    }

    /// What each server is, as the file gives it, in the order of the slots: for the person's Settings.
    pub fn configs(&self) -> Vec<(String, ServerConfig)> {
        let slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // (its trust as the person last set it, which is the slot's)
        slots.iter().filter_map(|slot| slot.config.clone().map(|mut config| {
            config.trusted = slot.trusted;
            (slot.name.clone(), config)
        })).collect()
    }

    /// The file the servers are listed in.
    pub fn file(&self) -> Option<PathBuf> {
        self.file.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// The tools of the servers up by now. A session that began before one was up is offered its tools from the next turn the person starts.
    pub fn schemas(&self) -> Vec<Value> {
        let slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        slots.iter().filter_map(|slot| slot.running.as_ref()).flat_map(|running| running.schemas.iter().cloned()).collect()
    }

    pub fn call(&self, tool: &str, arguments: &Value, cancel: &AtomicBool) -> Result<String, String> {
        let running = {
            let slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let up = slots.iter().filter_map(|slot| slot.running.clone()).find(|running| running.names.contains_key(tool));
            up.ok_or_else(|| match slots.iter().find(|slot| slot.had.iter().any(|had| had == tool)) {
                Some(slot) if slot.restarting => format!("{tool}: its server is starting again"),
                Some(Slot { failed: Some(reason), .. }) => format!("{tool}: its server did not start again: {reason}"),
                _ => format!("{tool}: its server is not running"),
            })?
        };
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
            Err(error) if error == ENDED || error.starts_with(PIPE) => return Err(self.start_again(running, error)),
            Err(error) => return Err(error),
        };
        let text = result_text(&result);
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(text);
        }
        Ok(text)
    }

    /// A server that has ended starts again, on a thread of its own: its handshake can take a minute, and a person who stops the turn does not wait for that. This call is
    /// not made again: it may have done what it was asked before the server went.
    fn start_again(&self, ended: Arc<Running>, error: String) -> String {
        let start = {
            let mut slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let slot = slots.iter_mut().find(|slot| slot.running.as_ref().is_some_and(|running| Arc::ptr_eq(running, &ended)));
            slot.and_then(|slot| {
                slot.had = ended.names.keys().cloned().collect();
                slot.running = None;
                slot.restarting = slot.start.is_some();
                slot.start.clone()
            })
        };
        let Some(start) = start else { return error };
        let slots = self.slots.clone();
        std::thread::spawn(move || {
            // (what is left of the old one ends before the new one begins, as it may hold what the new one needs)
            drop(ended);
            let outcome = start();
            let mut slots = slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(slot) = slots.iter_mut().find(|slot| slot.start.as_ref().is_some_and(|known| Arc::ptr_eq(known, &start))) {
                slot.restarting = false;
                match outcome {
                    Ok(running) => {
                        slot.running = Some(running);
                        slot.failed = None;
                        slot.had.clear();
                    }
                    Err(reason) => slot.failed = Some(reason),
                }
            }
        });
        format!("{error}. It is being started again: make the call again in a moment if it is still needed")
    }

    /// What each configured server's tools are called after (`mcp__<this>__tool`), in the order of the configuration.
    pub fn server_parts(&self) -> Vec<String> {
        let slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        name_parts(slots.iter().map(|slot| &slot.name))
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
        let mut config = readable_config(&file)?;
        config.servers.get_mut(server).ok_or_else(|| format!("{}: lists no server {server}", file.display()))?.trusted = trusted;
        write_config(&file, &config)?;
        if let Some(slot) = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter_mut().find(|slot| slot.name == server) {
            slot.trusted = trusted;
            if let Some(config) = &mut slot.config {
                config.trusted = trusted;
            }
        }
        Ok(())
    }

    /// A server that answers from a script and is already up, for the agent's tests.
    #[cfg(test)]
    pub(crate) fn scripted(server: &str, tools: &[Value], lines: &[Value], trusted: bool) -> Arc<Hub> {
        let hub = Arc::new(Hub::default());
        hub.add_scripted(server, tools, lines, trusted);
        hub
    }

    /// One more server, up with these tools, that answers with these lines: as one the person added while a session went on.
    #[cfg(test)]
    pub(crate) fn add_scripted(&self, server: &str, tools: &[Value], lines: &[Value], trusted: bool) {
        let script: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let (names, schemas) = place_tools(server, &name_part(server), tools);
        let running = Arc::new(Running { rpc: Mutex::new(Rpc::over(Box::new(std::io::sink()), std::io::Cursor::new(script.into_bytes()))), names, schemas, _process: None });
        self.slots.lock().unwrap().push(Slot { name: server.into(), running: Some(running), trusted, ..Slot::default() });
    }

    /// One line a server, for the person.
    pub fn status(&self) -> Vec<String> {
        self.states().into_iter().map(|(name, state)| match state {
            State::Up(tools) => format!("{name}: {tools} tools"),
            State::Failed(error) => format!("{name}: did not start: {error}"),
            State::Starting => format!("{name}: starting"),
        }).collect()
    }

    /// How each server is, in the order of the slots.
    pub fn states(&self) -> Vec<(String, State)> {
        self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter().map(|slot| {
            let state = match (&slot.running, &slot.failed) {
                (Some(running), _) => State::Up(running.names.len()),
                (_, Some(error)) => State::Failed(error.clone()),
                _ => State::Starting,
            };
            (slot.name.clone(), state)
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn claudes_file_is_read_as_it_is() {
        let text = r#"{"numStartups": 3, "mcpServers": {"files": {"type": "stdio", "command": "npx", "args": ["-y", "server"], "env": {"K": "v"}}, "remote": {"type": "http", "url": "https://x", "headers": {"Authorization": "Bearer t"}}, "old": {"type": "sse", "url": "https://y"}}}"#;
        let found = servers_of_claude(text);
        assert_eq!(found.keys().collect::<Vec<_>>(), ["files", "remote"], "an SSE server, of the older transport, is left out");
        assert_eq!(found["files"].args, ["-y", "server"]);
        assert_eq!(found["files"].env["K"], "v");
        assert!(!found["files"].is_http() && found["files"].kind.is_empty());
        assert!(found["remote"].is_http());
        assert_eq!(found["remote"].headers["Authorization"], "Bearer t");
        assert!(servers_of_claude("not json").is_empty());
    }

    #[test]
    fn codexs_tables_are_read_as_they_are() {
        let text = "model = \"x\"\n[mcp_servers.unity_mcp]\ncommand = \"C:\\\\tools\\\\unity.exe\"\nargs = [\"--stdio\", \"--port\", \"6400\"]\n[mcp_servers.unity_mcp.env]\nKEY = \"value\"\n[mcp_servers.url_only]\nurl = \"https://x\"\n[mcp_servers.url_only.http_headers]\n\"X-Key\" = \"k\"\n[other]\ncommand = \"no\"\n";
        let found = servers_of_codex(text);
        assert_eq!(found.keys().collect::<Vec<_>>(), ["unity_mcp", "url_only"]);
        assert!(found["url_only"].is_http());
        assert_eq!(found["url_only"].headers["X-Key"], "k");
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

    /// A server of Streamable HTTP for the tests, on this machine: it answers `initialize` in JSON with a session's id, a notification with 202, `tools/list` in an
    /// event stream (a notification first, then the answer), and `tools/call` in JSON with what it was asked and the headers it was sent. What each request carried
    /// is kept, to be read back.
    fn http_server(tool: &'static str) -> (String, Arc<Mutex<Vec<(String, Value)>>>) {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/mcp", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let kept = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let kept = kept.clone();
                std::thread::spawn(move || {
                    let mut data = Vec::new();
                    let mut block = [0u8; 4096];
                    let (head, body) = loop {
                        let count = stream.read(&mut block).unwrap_or(0);
                        if count == 0 {
                            return;
                        }
                        data.extend_from_slice(&block[..count]);
                        if let Some(at) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&data[..at]).to_string();
                            let length: usize = head.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length: ").and_then(|value| value.trim().parse().ok())).unwrap_or(0);
                            while data.len() < at + 4 + length {
                                let count = stream.read(&mut block).unwrap_or(0);
                                if count == 0 {
                                    break;
                                }
                                data.extend_from_slice(&block[..count]);
                            }
                            break (head, String::from_utf8_lossy(&data[at + 4..]).to_string());
                        }
                    };
                    let message: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    kept.lock().unwrap().push((head.clone(), message.clone()));
                    let headers: Vec<String> = head.lines().skip(1).map(str::to_string).collect();
                    let answer = |result: Value| json!({"jsonrpc": "2.0", "id": message["id"], "result": result}).to_string();
                    let reply = match message["method"].as_str() {
                        _ if head.starts_with("DELETE") => "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_string(),
                        Some("initialize") => {
                            let body = answer(json!({"protocolVersion": PROTOCOL, "capabilities": {"tools": {}}}));
                            format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: s-1\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                        }
                        Some("tools/list") => {
                            let note = json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {}});
                            let body = format!("event: message\r\ndata: {note}\r\n\r\nevent: message\r\ndata: {}\r\n\r\n", answer(json!({"tools": [{"name": tool, "inputSchema": {"type": "object"}}]})));
                            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                        }
                        Some("tools/call") => {
                            let said = format!("{} with {}", message["params"]["arguments"], headers.join(" | "));
                            let body = answer(json!({"content": [{"type": "text", "text": said}]}));
                            format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                        }
                        _ => "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n".to_string(),
                    };
                    let _ = stream.write_all(reply.as_bytes());
                });
            }
        });
        (url, seen)
    }

    /// A server at a URL is started and called over Streamable HTTP: its session's id and its headers go with each request after the handshake, and its tools are the session's.
    #[test]
    fn a_server_at_a_url_is_called_over_streamable_http_with_its_headers() {
        let (url, seen) = http_server("echo");
        let config = ServerConfig { url: url.clone(), kind: "http".into(), headers: BTreeMap::from([("Authorization".to_string(), "Bearer secret".to_string())]), ..ServerConfig::default() };
        assert!(config.is_http() && config.problem().is_none());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        save_server(&file, None, "remote", config).unwrap();
        let hub = Hub::start(&file);
        let started = Instant::now();
        while hub.schemas().is_empty() && started.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(hub.schemas()[0]["function"]["name"], "mcp__remote__echo", "{:?}", hub.status());
        let said = hub.call("mcp__remote__echo", &json!({"a": 1}), &AtomicBool::new(false)).unwrap();
        assert!(said.starts_with(r#"{"a":1} with"#), "{said}");
        let lower = said.to_ascii_lowercase();
        assert!(lower.contains("authorization: bearer secret") && lower.contains("mcp-session-id: s-1") && lower.contains(&format!("mcp-protocol-version: {PROTOCOL}")), "{said}");
        let seen = seen.lock().unwrap();
        let methods: Vec<&str> = seen.iter().filter_map(|(_, message)| message["method"].as_str()).collect();
        assert_eq!(methods, ["initialize", "notifications/initialized", "tools/list", "tools/call"]);
        assert!(!seen[0].0.to_ascii_lowercase().contains("mcp-session-id"), "the first request has no session yet");
    }

    /// A change to the file, as Settings makes one, starts what is new and what changed again, ends what is gone, and leaves the rest as it was.
    #[test]
    fn a_change_to_the_servers_starts_and_ends_only_what_changed() {
        let ((one, _), (two, _)) = (http_server("first"), http_server("second"));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        let at = |url: &str| ServerConfig { url: url.to_string(), ..ServerConfig::default() };
        save_server(&file, None, "a", at(&one)).unwrap();
        let hub = Hub::start(&file);
        let tools = |hub: &Hub, want: usize| {
            let started = Instant::now();
            while hub.schemas().len() != want && started.elapsed() < Duration::from_secs(20) {
                std::thread::sleep(Duration::from_millis(50));
            }
            let mut names: Vec<String> = hub.schemas().iter().map(|schema| schema["function"]["name"].as_str().unwrap().to_string()).collect();
            names.sort();
            names
        };
        assert_eq!(tools(&hub, 1), ["mcp__a__first"]);
        save_server(&file, None, "b", at(&two)).unwrap();
        assert_eq!(hub.reload(), ["b"], "only the new one starts");
        assert_eq!(tools(&hub, 2), ["mcp__a__first", "mcp__b__second"]);
        assert!(save_server(&file, None, "b", at(&one)).unwrap_err().contains("already"), "a name is one server's");
        save_server(&file, Some("b"), "c", at(&two)).unwrap();
        assert_eq!(hub.reload(), ["c"], "a renamed one is a new one, and the old name ends");
        assert_eq!(tools(&hub, 2), ["mcp__a__first", "mcp__c__second"]);
        let mut trusted = at(&one);
        trusted.trusted = true;
        save_server(&file, Some("a"), "a", trusted).unwrap();
        assert!(hub.reload().is_empty(), "trust alone starts nothing again");
        assert!(hub.trust().contains(&("a".to_string(), true)));
        remove_server(&file, "a").unwrap();
        hub.reload();
        assert_eq!(tools(&hub, 1), ["mcp__c__second"]);
        assert_eq!(hub.configs().len(), 1);
        hub.set_trusted("c", true).unwrap();
        assert!(hub.configs()[0].1.trusted, "Settings reads the trust back as the switch set it, so that a change made after keeps it");
        assert!(hub.reload().is_empty());
        assert!(save_server(&file, None, "x", ServerConfig::default()).unwrap_err().contains("needed"));
        assert!(save_server(&file, None, "x", at("ftp://x")).unwrap_err().contains("http"));
    }

    /// Arguments go to a line and back as they were, as Windows quotes them; a line typed by hand is read the same way.
    #[test]
    fn arguments_go_to_a_line_and_back_as_they_were() {
        let args: Vec<String> = ["-y", "@scope/server", "C:\\my files\\", "say \"hi\"", "", "a\\\\b", "plain\\"].iter().map(|arg| arg.to_string()).collect();
        let line = line_of_args(&args);
        assert_eq!(line, r#"-y @scope/server "C:\my files\\" "say \"hi\"" "" a\\b plain\"#);
        assert_eq!(args_of_line(&line), args);
        assert_eq!(args_of_line(r#"  --root "C:\Program Files\x"  --port 6400 "#), ["--root", r"C:\Program Files\x", "--port", "6400"]);
        assert!(args_of_line("   ").is_empty());
    }

    /// Variables and headers are a line each, and a line that is neither says which it is.
    #[test]
    fn variables_and_headers_are_a_line_each() {
        let headers = BTreeMap::from([("Authorization".to_string(), "Bearer a:b".to_string()), ("X-Key".to_string(), "k".to_string())]);
        let text = lines_of_pairs(&headers, ':');
        assert_eq!(text, "Authorization: Bearer a:b\r\nX-Key: k");
        assert_eq!(pairs_of_lines(&text, ':').unwrap(), headers);
        let env = pairs_of_lines("KEY=a=b\n\n  OTHER = x \n", '=').unwrap();
        assert_eq!(env, BTreeMap::from([("KEY".to_string(), "a=b".to_string()), ("OTHER".to_string(), "x".to_string())]));
        assert_eq!(lines_of_pairs(&env, '='), "KEY=a=b\r\nOTHER=x");
        assert_eq!(pairs_of_lines("A=1\nno separator", '=').unwrap_err(), (2, "no separator".to_string()));
        assert_eq!(pairs_of_lines("=1", '=').unwrap_err(), (1, "=1".to_string()));
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
        let tools = rpc.start(START_LIMIT).unwrap();
        assert_eq!(tools.len(), 2);
        let (known, real, schema) = schema_of("srv", "srv", &tools[1]).unwrap();
        assert_eq!((known.as_str(), real.as_str()), ("mcp__srv__b", "b"));
        assert_eq!(schema["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn a_server_that_ends_is_said_to_have_ended() {
        let mut rpc = Rpc::over(Box::new(std::io::sink()), Cursor::new(Vec::new()));
        assert_eq!(rpc.call("x", json!({}), Duration::from_secs(5), None).unwrap_err(), "the server has ended");
    }

    /// A server that says nothing, and has not ended either.
    struct Silent;

    impl std::io::Read for Silent {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_secs(60));
            Ok(0)
        }
    }

    /// What a server has been sent, to read back.
    #[derive(Clone, Default)]
    struct Sent(Arc<Mutex<Vec<u8>>>);

    impl Write for Sent {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Sent {
        fn messages(&self) -> Vec<Value> {
            String::from_utf8_lossy(&self.0.lock().unwrap()).lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
        }
    }

    #[test]
    fn a_call_can_be_stopped() {
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
        let (names, schemas) = place_tools("srv", "srv", &tools);
        assert_eq!((names.len(), schemas.len()), (4, 4), "{names:?}");
        assert!(names.keys().all(|name| name.len() <= 64), "{names:?}");
        let spelled: Vec<&str> = schemas.iter().map(|schema| schema["function"]["name"].as_str().unwrap()).collect();
        assert!(spelled.iter().all(|name| names.contains_key(*name)));
        let (_, reversed) = place_tools("srv", "srv", &[tool("do_it"), tool("do.it"), tool(&format!("{long}a")), tool(&format!("{long}b"))]);
        assert_eq!(serde_json::to_string(&schemas).unwrap(), serde_json::to_string(&reversed).unwrap(), "whatever order it lists them in");
        assert_eq!(schemas[0]["function"]["parameters"]["type"], "object");
    }

    fn running(lines: &[Value], names: &[&str]) -> Arc<Running> {
        let script: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let (names, schemas) = place_tools("srv", "srv", &names.iter().map(|name| tool(name)).collect::<Vec<_>>());
        Arc::new(Running { rpc: Mutex::new(Rpc::over(Box::new(std::io::sink()), Cursor::new(script.into_bytes()))), names, schemas, _process: None })
    }

    /// Waits for what another thread does.
    fn waited(what: &str, done: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < until, "gave up waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
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
        waited("the server to be up again", || hub.status() == ["srv: 1 tools"]);
        assert_eq!(hub.call("mcp__srv__t", &json!({}), &cancel).unwrap(), "back");
    }

    #[test]
    fn a_restart_does_not_hold_the_call_and_a_call_meanwhile_is_told_so() {
        // The server does not come up until the test lets it: a restart on the calling thread would wait here for it.
        let (open, gate) = std::sync::mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let again = json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": "back"}]}});
        let start: Starter = Arc::new(move || {
            gate.lock().unwrap().recv_timeout(Duration::from_secs(5)).map_err(|_| "it was never let up".to_string())?;
            Ok(running(std::slice::from_ref(&again), &["t"]))
        });
        let hub = Hub::default();
        hub.slots.lock().unwrap().push(Slot { name: "srv".into(), start: Some(start), running: Some(running(&[], &["t"])), ..Slot::default() });
        let cancel = AtomicBool::new(false);
        let said = hub.call("mcp__srv__t", &json!({}), &cancel).unwrap_err();
        assert!(said.contains("has ended") && said.contains("being started again") && said.contains("in a moment"), "{said}");
        let meanwhile = hub.call("mcp__srv__t", &json!({}), &cancel).unwrap_err();
        assert!(meanwhile.contains("is starting again") && !meanwhile.contains("not running"), "{meanwhile}");
        open.send(()).unwrap();
        waited("the server to be up again", || hub.status() == ["srv: 1 tools"]);
        assert_eq!(hub.call("mcp__srv__t", &json!({}), &cancel).unwrap(), "back");
    }

    #[test]
    fn a_server_that_will_not_start_again_says_so_and_why() {
        let start: Starter = Arc::new(|| Err("no such program".to_string()));
        let hub = Hub::default();
        hub.slots.lock().unwrap().push(Slot { name: "srv".into(), start: Some(start), running: Some(running(&[], &["t"])), ..Slot::default() });
        let cancel = AtomicBool::new(false);
        let said = hub.call("mcp__srv__t", &json!({}), &cancel).unwrap_err();
        assert!(said.contains("has ended") && said.contains("started again"), "{said}");
        waited("the server to fail", || hub.status() == ["srv: did not start: no such program"]);
        let later = hub.call("mcp__srv__t", &json!({}), &cancel).unwrap_err();
        assert!(later.contains("did not start again") && later.contains("no such program"), "{later}");
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
        // Two servers whose names clean to one: the one a call reaches.
        let both = Hub::default();
        let named = ["a b".to_string(), "a_b".to_string()];
        for ((name, trusted), part) in named.iter().zip([false, true]).zip(name_parts(&named)) {
            let (names, schemas) = place_tools(name, &part, &[tool("t")]);
            let server = Arc::new(Running { rpc: Mutex::new(Rpc::over(Box::new(std::io::sink()), Cursor::new(Vec::new()))), names, schemas, _process: None });
            both.slots.lock().unwrap().push(Slot { name: name.clone(), running: Some(server), trusted, ..Slot::default() });
        }
        assert!(!both.trusts_tool("mcp__a_b__t"));
        assert!(both.trusts_tool("mcp__a_b_2__t"));
        assert!(!both.trusts_tool("mcp__nothing__t"));
    }

    #[test]
    fn an_empty_hub_has_nothing_to_offer() {
        let hub = Hub::default();
        assert!(hub.schemas().is_empty() && hub.status().is_empty());
        assert!(hub.call("mcp__x__y", &json!({}), &AtomicBool::new(false)).unwrap_err().contains("not running"));
    }

    #[test]
    fn a_call_that_runs_out_of_time_is_asked_to_stop_and_says_it_may_have_run() {
        let sent = Sent::default();
        let mut rpc = Rpc::over(Box::new(sent.clone()), BufReader::new(Silent));
        let said = rpc.call("tools/call", json!({"name": "t"}), Duration::from_secs(1), None).unwrap_err();
        assert!(said.starts_with("no answer to tools/call within 1 s"), "{said}");
        assert!(said.contains("may still be running") && said.contains("have done it already") && said.contains("asked to stop"), "{said}");
        let asked: Vec<Value> = sent.messages().into_iter().filter(|message| message["method"] == "notifications/cancelled").collect();
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert_eq!(asked[0]["params"]["requestId"], 1);
    }

    #[test]
    fn a_handshake_that_gets_no_answer_is_not_cancelled() {
        let sent = Sent::default();
        let mut rpc = Rpc::over(Box::new(sent.clone()), BufReader::new(Silent));
        assert_eq!(rpc.start(Duration::from_secs(1)).unwrap_err(), "no answer to initialize within 1 s");
        let methods: Vec<Value> = sent.messages().into_iter().map(|message| message["method"].clone()).collect();
        assert_eq!(methods, ["initialize"], "the protocol does not allow the handshake to be cancelled");
    }

    #[test]
    fn a_line_that_is_not_utf8_does_not_end_the_reader() {
        // (what a `cmd /c` script prints in the machine's own encoding, here Shift-JIS, and a result with a byte of it in)
        let mut script = b"\x82\xb1\x82\xf1 is not UTF-8, nor JSON\n".to_vec();
        script.extend_from_slice(b"{\"jsonrpc\": \"2.0\", \"id\": 1, \"result\": {\"content\": [{\"type\": \"text\", \"text\": \"ok \x82\xb1\"}]}}\n");
        let mut rpc = Rpc::over(Box::new(std::io::sink()), Cursor::new(script));
        let result = rpc.call("tools/call", json!({}), Duration::from_secs(5), None).unwrap();
        assert!(result_text(&result).starts_with("ok "), "{result}");
    }

    /// A server's pages of tools, `per_page` each, every one saying there is another, which are as many as the test asks for.
    fn endless_pages(count: u64, per_page: usize) -> Cursor<Vec<u8>> {
        let mut lines = vec![json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": PROTOCOL, "capabilities": {"tools": {}}}})];
        for id in 2..2 + count {
            let tools: Vec<Value> = (0..per_page).map(|at| tool(&format!("t{id}_{at}"))).collect();
            lines.push(json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools, "nextCursor": format!("after {id}")}}));
        }
        Cursor::new(lines.iter().map(|line| format!("{line}\n")).collect::<String>().into_bytes())
    }

    #[test]
    fn a_server_that_always_has_another_page_is_read_to_a_limit_and_keeps_what_it_gave() {
        let mut rpc = Rpc::over(Box::new(std::io::sink()), endless_pages(3 * MAX_PAGES as u64, 1));
        assert_eq!(rpc.start(START_LIMIT).unwrap().len(), MAX_PAGES, "by pages");
        let mut rpc = Rpc::over(Box::new(std::io::sink()), endless_pages(3 * MAX_PAGES as u64, 30));
        let tools = rpc.start(START_LIMIT).unwrap();
        assert!((MAX_TOOLS..MAX_TOOLS + 30).contains(&tools.len()), "by tools: {}", tools.len());
    }

    #[test]
    fn servers_whose_names_come_to_one_get_a_part_each_in_the_files_order() {
        let list = |names: &[&str]| names.iter().map(|name| name.to_string()).collect::<Vec<_>>();
        assert_eq!(name_parts(&list(&["my.server", "my_server", "my_server_2"])), ["my_server", "my_server_2", "my_server_2_2"]);
        // (a cut to 20 characters that leaves the same, and the number kept within them; one with nothing left to say is a server)
        let long = "s".repeat(30);
        let parts = name_parts(&list(&[&long, &format!("{long}x"), "a b", "a__b_", "_a_b", "..."]));
        assert_eq!(parts[0], "s".repeat(20));
        assert_eq!(parts[1], format!("{}_2", "s".repeat(18)));
        assert_eq!(&parts[2..], ["a_b", "a_b_2", "a_b_3", "server"]);
        // The first `__` after `mcp__` ends a part, so that a part and a tool cannot be split two ways into two servers' names.
        assert_ne!(tool_name(&name_part("a"), "b__x"), tool_name(&name_part("a__b"), "x"));
    }

    /// A fake server is a program of Python's, run as a server is: `python -I -c CODE ARGUMENTS`. It writes its process id to the file it is given, so that a test can see
    /// whether it is still running.
    fn fake(code: &str, arguments: &[&str]) -> ServerConfig {
        // (by its full path, as the one a PATH finds first may only start the real one)
        let said = Command::new("python").args(["-I", "-c", "import sys; print(sys.executable)"]).output().expect("the tests need Python on PATH");
        let args = ["-I", "-c", code].into_iter().chain(arguments.iter().copied()).map(str::to_string).collect();
        ServerConfig { command: String::from_utf8_lossy(&said.stdout).trim().to_string(), args, ..ServerConfig::default() }
    }

    /// A server that tells its process id and then says nothing: a handshake that gets no answer.
    const MUTE: &str = "import os, sys, time\nopen(sys.argv[1], 'w').write(str(os.getpid()))\ntime.sleep(30)\n";

    /// A server that answers the handshake, lists a tool `read` and answers a call of it with its own name (the first argument).
    const ANSWERING: &str = "import json, os, sys\nwho = sys.argv[1]\nopen(sys.argv[2], 'w').write(str(os.getpid()))\nfor line in sys.stdin:\n    try:\n        m = json.loads(line)\n    except ValueError:\n        continue\n    if 'id' not in m:\n        continue\n    how = m.get('method')\n    if how == 'tools/list':\n        r = {'tools': [{'name': 'read', 'description': who, 'inputSchema': {'type': 'object'}}]}\n    elif how == 'tools/call':\n        r = {'content': [{'type': 'text', 'text': 'from ' + who}]}\n    else:\n        r = {'protocolVersion': '2025-06-18', 'capabilities': {'tools': {}}}\n    print(json.dumps({'jsonrpc': '2.0', 'id': m['id'], 'result': r}), flush=True)\n";

    fn pid_in(file: &Path) -> Option<u32> {
        std::fs::read_to_string(file).ok()?.trim().parse().ok()
    }

    #[cfg(windows)]
    fn alive(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
        // SAFETY: the handle is opened here and closed here; a process that is gone cannot be opened.
        unsafe {
            let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if process.is_null() {
                return false;
            }
            let running = WaitForSingleObject(process, 0) == WAIT_TIMEOUT;
            CloseHandle(process);
            running
        }
    }

    #[cfg(not(windows))]
    fn alive(pid: u32) -> bool {
        Command::new("kill").args(["-0", &pid.to_string()]).status().is_ok_and(|status| status.success())
    }

    /// Ends the fake servers a test started, if the test has not: by the process ids they wrote, and none of another's.
    struct Reaper(Vec<PathBuf>);

    impl Drop for Reaper {
        fn drop(&mut self) {
            for pid in self.0.iter().filter_map(|file| pid_in(file)).filter(|pid| alive(*pid)) {
                crate::tools::kill_tree(pid);
            }
        }
    }

    /// Starts a server that never answers, with 3 s to, and checks that its process has ended after.
    fn start_a_mute_one(job: bool) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mute.pid");
        let _reaper = Reaper(vec![file.clone()]);
        let config = fake(MUTE, &[&file.display().to_string()]);
        let Err(error) = start_server_within(&config, Duration::from_secs(3), job) else { panic!("a server that never answers was started") };
        assert!(error.contains("no answer to initialize"), "{error}");
        let pid = pid_in(&file).expect("the server ran and wrote its process id");
        if job {
            // (the job ends it as its handle closes, which is not before this returns)
            waited("the job to end the server", || !alive(pid));
        }
        assert!(!alive(pid), "the server that failed its handshake is still running");
    }

    #[test]
    fn a_server_that_fails_its_handshake_does_not_outlive_it() {
        // (no job holds it, as where none could be made, or on a system that has none: the process is then the app's to end)
        start_a_mute_one(false);
    }

    #[cfg(windows)]
    #[test]
    fn a_server_that_fails_its_handshake_in_a_job_ends_with_the_job() {
        start_a_mute_one(true);
    }

    #[test]
    fn two_servers_whose_names_clean_to_one_keep_their_own_tools() {
        let dir = tempfile::tempdir().unwrap();
        let (mut servers, mut files) = (BTreeMap::new(), Vec::new());
        for name in ["my.server", "my_server"] {
            let file = dir.path().join(format!("{name}.pid"));
            let mut config = fake(ANSWERING, &[name, &file.display().to_string()]);
            config.trusted = name == "my_server";
            servers.insert(name.to_string(), config);
            files.push(file);
        }
        let _reaper = Reaper(files.clone());
        let listed = dir.path().join("mcp.json");
        std::fs::write(&listed, serde_json::to_string(&Config { servers }).unwrap()).unwrap();
        let hub = Hub::start(&listed);
        waited("both servers to be up", || hub.schemas().len() == 2);
        let schemas = hub.schemas();
        let named = |server: &str| {
            let schema = schemas.iter().find(|schema| schema["function"]["description"].as_str().unwrap().starts_with(&format!("From the MCP server {server}."))).unwrap();
            schema["function"]["name"].as_str().unwrap().to_string()
        };
        let (dotted, underscored) = (named("my.server"), named("my_server"));
        assert_eq!((dotted.as_str(), underscored.as_str()), ("mcp__my_server__read", "mcp__my_server_2__read"));
        let cancel = AtomicBool::new(false);
        assert_eq!(hub.call(&dotted, &json!({}), &cancel).unwrap(), "from my.server");
        assert_eq!(hub.call(&underscored, &json!({}), &cancel).unwrap(), "from my_server");
        assert!(!hub.trusts_tool(&dotted) && hub.trusts_tool(&underscored), "trust goes to the server a call reaches");
        let pids: Vec<u32> = files.iter().filter_map(|file| pid_in(file)).collect();
        assert_eq!(pids.len(), 2);
        drop(hub);
        waited("the servers to end with the hub", || pids.iter().all(|pid| !alive(*pid)));
    }
}
