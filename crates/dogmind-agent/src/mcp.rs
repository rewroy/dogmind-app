//! MCP servers, listed in this app's own `mcp.json` (in the `mcpServers` form other apps use too, so that theirs can be imported): a program run over
//! stdio, with its arguments and its environment, or a server at a URL that speaks Streamable HTTP, with the headers it is sent. They start with the
//! app, start again when the person changes them in Settings, and their tools join the session's as `mcp__server__tool`.
//!
//! A server is a program the person chose to run, so it is not in the sandbox, and its tools ask like commands do. What one
//! returns may come from anywhere, so it goes to the model marked as data, as a web page does. A server the person says they trust
//! still has that mark, and its tools still ask as they did; only what it returns no longer counts as web content in the conversation.

use std::collections::{BTreeMap, BTreeSet};
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
pub struct ToolSelection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enabled_tools: Option<BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    disabled_tools: BTreeSet<String>,
}

impl ToolSelection {
    fn allows(&self, name: &str) -> bool {
        self.enabled_tools.as_ref().is_none_or(|enabled| enabled.contains(name)) && !self.disabled_tools.contains(name)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// `http` for a server at `url` (Streamable HTTP, or the older transport if it answers as one of those), `sse` for one of the older transport (HTTP
    /// with server-sent events, before Streamable HTTP); none, or `stdio`, for a program.
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// Sent with every request to `url`: an `Authorization` among them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(flatten)]
    pub tool_selection: ToolSelection,
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
    readable_config(file).unwrap_or_default()
}

/// Claude Code's own list of servers: the `mcpServers` of `~/.claude.json`, stdio, HTTP and SSE servers.
pub fn servers_of_claude(text: &str) -> BTreeMap<String, ServerConfig> {
    parse_claude_servers(text).unwrap_or_default()
}

fn parse_claude_servers(text: &str) -> Option<BTreeMap<String, ServerConfig>> {
    let mut value = serde_json::from_str::<serde_json::Map<String, Value>>(text.strip_prefix('\u{feff}').unwrap_or(text)).ok()?;
    let servers = value.remove("mcpServers").unwrap_or_else(|| Value::Object(Default::default()));
    let mut found = BTreeMap::new();
    for (name, server) in servers.as_object()? {
        if let Ok(mut config) = serde_json::from_value::<ServerConfig>(server.clone()) {
            if config.kind == "stdio" {
                config.kind.clear();
            }
            if config.problem().is_none() {
                found.insert(name.clone(), config);
            }
        }
    }
    Some(found)
}

/// Import the launch settings of individual servers from Codex's TOML configuration.
pub fn servers_of_codex(text: &str) -> BTreeMap<String, ServerConfig> {
    parse_codex_servers(text).unwrap_or_default()
}

fn parse_codex_servers(text: &str) -> Option<BTreeMap<String, ServerConfig>> {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct CodexServer {
        command: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        env: BTreeMap<String, String>,
        url: String,
        http_headers: BTreeMap<String, String>,
        #[serde(flatten)]
        tool_selection: ToolSelection,
        enabled: Option<bool>,
    }

    let mut config = toml::from_str::<toml::Table>(text).ok()?;
    let Some(servers) = config.remove("mcp_servers") else { return Some(BTreeMap::new()) };
    let toml::Value::Table(servers) = servers else { return None };
    Some(servers.into_iter().filter_map(|(name, value)| {
        let server: CodexServer = value.try_into().ok()?;
        if server.enabled == Some(false) {
            return None;
        }
        let config = ServerConfig {
            command: server.command, args: server.args, cwd: server.cwd, env: server.env, url: server.url,
            headers: server.http_headers, tool_selection: server.tool_selection, ..Default::default()
        };
        config.problem().is_none().then_some((name, config))
    }).collect())
}

/// Read the optional import sources in priority order. Report only filenames so malformed lines never expose settings.
pub fn external_servers(home: &Path) -> (BTreeMap<String, ServerConfig>, Vec<&'static str>) {
    let mut found = BTreeMap::new();
    let mut unreadable = Vec::new();
    type Parser = fn(&str) -> Option<BTreeMap<String, ServerConfig>>;
    for (file, parse) in [(".claude.json", parse_claude_servers as Parser), (".codex/config.toml", parse_codex_servers)] {
        let servers = match std::fs::read_to_string(home.join(file)) {
            Ok(text) => parse(&text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => None,
        };
        match servers {
            Some(servers) => {
                for (name, server) in servers {
                    found.entry(name).or_insert(server);
                }
            }
            None => unreadable.push(file),
        }
    }
    (found, unreadable)
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
pub fn readable_config(file: &Path) -> Result<Config, String> {
    match std::fs::read_to_string(file) {
        // (all a power cut left of it, which has nothing in it to keep)
        Ok(text) if crate::images::left_by_a_power_cut(text.as_bytes()) => Ok(Config::default()),
        Ok(text) => serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(&text)).map_err(|error| format!("{}: {error}", file.display())),
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
    // (on the disk before it takes the name: otherwise a power cut just after can leave the person's servers as zeros, which read as none)
    let written = std::fs::File::create(&partial).and_then(|mut out| out.write_all(text.as_bytes()).and_then(|()| out.sync_all()));
    written.map_err(|error| format!("{}: {error}", partial.display()))?;
    std::fs::rename(&partial, file).map_err(|error| format!("{}: {error}", file.display()))
}

/// Adds what is not there yet, and says how many.
pub fn import(file: &Path, more: BTreeMap<String, ServerConfig>) -> Result<usize, String> {
    // (a file that cannot be read is the person's to mend: written over, it would keep only what came in)
    let mut config = readable_config(file)?;
    let before = config.servers.len();
    for (name, server) in more {
        // Trust is chosen for this app. An existing local choice is kept on reimport.
        config.servers.entry(name).or_insert(ServerConfig { trusted: false, ..server });
    }
    let added = config.servers.len() - before;
    if added > 0 {
        write_config(file, &config)?;
    }
    Ok(added)
}

/// The JSON-RPC of one server, whichever pipes it runs over; or, for a server at a URL, over HTTP (`http`), where the pipes are not used.
struct Rpc {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    answers: Receiver<Value>,
    next: u64,
    http: Option<Mutex<HttpLink>>,
    /// Set as the server says its list of tools changed (`LIST_CHANGED`), whenever it says it.
    changed: Arc<AtomicBool>,
}

/// What a server says when it has tools it did not have, or no longer has some: as one does that adds tools on request.
const LIST_CHANGED: &str = "notifications/tools/list_changed";

/// How long a server is given to list its tools again, each page.
const LIST_LIMIT: Duration = Duration::from_secs(10);

/// A server at a URL, as Streamable HTTP has it: each message a POST, answered in JSON or in an event stream that carries the answer; the session's id the
/// server gave in its answer to `initialize`, sent back with each request after it.
struct HttpLink {
    url: crate::http::Url,
    headers: Vec<(String, String)>,
    session: Option<String>,
    initialized: bool,
    /// The connection's `Rpc::changed`: the server says it in a request's event stream, or in its session's (see `listen`).
    changed: Arc<AtomicBool>,
    /// The session's stream, read while the session lasts.
    listening: Option<Listening>,
}

/// The thread that reads a session's stream (see `listen`): it ends as this goes.
struct Listening(Arc<AtomicBool>);

impl Drop for Listening {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Server-sent events as their pieces come: the JSON-RPC message each event's data holds. An event's end may come split across two pieces.
#[derive(Default)]
struct Events {
    pending: Vec<u8>,
}

impl Events {
    fn read(&mut self, bytes: &[u8]) -> Vec<Value> {
        self.read_named(bytes).into_iter().filter_map(|(_, data)| message_of(&data).ok()).collect()
    }

    /// Each whole event's name (`message`, when it gives none) and data, of those that have data.
    fn read_named(&mut self, bytes: &[u8]) -> Vec<(String, String)> {
        self.pending.extend_from_slice(bytes);
        let find = |bytes: &[u8], what: &[u8]| bytes.windows(what.len()).position(|window| window == what).map(|at| (at, what.len()));
        let mut events = Vec::new();
        while let Some((at, length)) = [find(&self.pending, b"\n\n"), find(&self.pending, b"\r\n\r\n")].into_iter().flatten().min() {
            let event: Vec<u8> = self.pending.drain(..at + length).collect();
            let text = String::from_utf8_lossy(&event).replace("\r\n", "\n");
            let field = |name: &str| text.lines().filter_map(|line| line.strip_prefix(name)).map(|value| value.strip_prefix(' ').unwrap_or(value)).collect::<Vec<_>>();
            let data = field("data:");
            if data.is_empty() {
                continue;
            }
            let name = field("event:").first().map_or("message", |name| name.trim()).to_string();
            events.push((name, data.join("\n")));
        }
        events
    }
}

/// A server of the older transport (HTTP with server-sent events, before Streamable HTTP): a stream (a GET of its URL) that says first where to post, then
/// carries every answer; each message is posted there. Its JSON-RPC is that of a program's pipes (`Rpc::over`): the stream's messages are its lines, and a
/// line written is posted. `limit` is how long the stream is given to say where to post.
fn older_transport(url: &str, headers: &BTreeMap<String, String>, limit: Duration) -> Result<Rpc, String> {
    let stream = crate::http::Url::parse(url)?;
    let headers: Vec<(String, String)> = headers.iter().map(|(name, value)| (name.clone(), value.clone())).collect();
    let stop = Arc::new(AtomicBool::new(false));
    let (said, heard) = channel();
    let (told, where_to) = channel();
    {
        let (stream, headers, stop) = (stream.clone(), headers.clone(), stop.clone());
        std::thread::spawn(move || read_older_stream(&stream, &headers, &stop, &told, &said));
    }
    let endpoint = where_to.recv_timeout(limit).unwrap_or_else(|_| Err(format!("{} said nowhere to post within {} s", stream.host, limit.as_secs())));
    let endpoint = endpoint.inspect_err(|_| stop.store(true, Ordering::Relaxed))?;
    let writer = Posting { url: endpoint, headers, stop, written: Vec::new() };
    Ok(Rpc::over(Box::new(writer), BufReader::new(Heard { from: heard, pending: Vec::new() })))
}

/// The older transport's stream, read until `stop`: where to post, told once; each message after it, a line for `Rpc::over`'s reader. Its end ends the
/// server, as a program's pipe does.
fn read_older_stream(stream: &crate::http::Url, headers: &[(String, String)], stop: &AtomicBool, told: &std::sync::mpsc::Sender<Result<crate::http::Url, String>>, said: &std::sync::mpsc::Sender<Vec<u8>>) {
    let mut asking = headers.to_vec();
    asking.push(("Accept".into(), "text/event-stream".into()));
    let mut response = match crate::http::request("GET", stream, &asking, None, HTTP_CONNECT, stop) {
        Ok(response) => response,
        Err(error) => {
            let _ = told.send(Err(format!("{}: {error}", stream.host)));
            return;
        }
    };
    let streamed = response.header("content-type").is_some_and(|kind| kind.to_ascii_lowercase().starts_with("text/event-stream"));
    if response.status != 200 || !streamed {
        let _ = told.send(Err(format!("the server answered {} to a GET of its stream", response.status)));
        return;
    }
    let mut events = Events::default();
    while let Ok(Some(bytes)) = response.next() {
        for (name, data) in events.read_named(&bytes) {
            if name == "endpoint" {
                let _ = told.send(endpoint_of(stream, &data));
            } else if let Ok(message) = message_of(&data) {
                if said.send(format!("{message}\n").into_bytes()).is_err() {
                    return;
                }
            }
        }
    }
}

/// Where the older transport's stream says to post, as a URL: a path, or a whole URL of the stream's own origin, as what goes there carries the headers.
fn endpoint_of(stream: &crate::http::Url, said: &str) -> Result<crate::http::Url, String> {
    let said = said.trim();
    let url = if said.starts_with("http://") || said.starts_with("https://") {
        crate::http::Url::parse(said)?
    } else if said.starts_with('/') {
        crate::http::Url { path: said.to_string(), ..stream.clone() }
    } else {
        let path = stream.path.split('?').next().unwrap_or("/");
        crate::http::Url { path: format!("{}{said}", &path[..path.rfind('/').map_or(0, |at| at + 1)]), ..stream.clone() }
    };
    if (url.tls, &url.host, url.port) != (stream.tls, &stream.host, stream.port) {
        return Err(format!("the server said to post to {said}, which is not where its stream is"));
    }
    Ok(url)
}

/// The older transport's side that is written to: each line, a message, posted where the stream said. A post that fails is a pipe that broke.
struct Posting {
    url: crate::http::Url,
    headers: Vec<(String, String)>,
    /// The stream's: it ends as this goes.
    stop: Arc<AtomicBool>,
    written: Vec<u8>,
}

impl Write for Posting {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.written.extend_from_slice(bytes);
        while let Some(end) = self.written.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.written.drain(..=end).collect();
            let body = String::from_utf8_lossy(&line).trim().to_string();
            if body.is_empty() {
                continue;
            }
            let mut headers = self.headers.clone();
            headers.push(("Content-Type".into(), "application/json".into()));
            let mut response = crate::http::request("POST", &self.url, &headers, Some(&body), HTTP_CONNECT, &self.stop).map_err(|error| std::io::Error::other(error.to_string()))?;
            let _ = response.text();
            if response.status >= 300 {
                return Err(std::io::Error::other(format!("the server answered {}", response.status)));
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Posting {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// What a server at a URL said outside an answer: a request of this side answered by a POST of its own (as the protocol has it, which the server takes
/// with 202), sent with `posting`; its word that its tools changed kept in `changed`.
fn heard(url: &crate::http::Url, posting: &[(String, String)], message: &Value, changed: &AtomicBool, cancel: &AtomicBool) {
    if let Some(reply) = answer_to_server(message) {
        let _ = crate::http::request("POST", url, posting, Some(&reply.to_string()), HTTP_CONNECT, cancel).and_then(|mut answered| answered.text());
    }
    if message.get("method").and_then(Value::as_str) == Some(LIST_CHANGED) {
        changed.store(true, Ordering::Relaxed);
    }
}

/// Reads what a server at a URL says between requests, on its session's stream (a GET, as Streamable HTTP has it), until `stop`: that its tools changed,
/// as the official SDKs say it there, and what it asks, which is answered. `headers` are the session's. A server that offers no such stream (405), or no
/// longer knows the session, is not asked again; one whose stream ends or breaks is, a little later each time.
fn listen(url: &crate::http::Url, headers: &[(String, String)], changed: &AtomicBool, stop: &AtomicBool) {
    let mut asking = headers.to_vec();
    asking.push(("Accept".into(), "text/event-stream".into()));
    let mut posting = headers.to_vec();
    posting.push(("Content-Type".into(), "application/json".into()));
    posting.push(("Accept".into(), "application/json, text/event-stream".into()));
    let mut wait = Duration::from_secs(1);
    while !stop.load(Ordering::Relaxed) {
        if let Ok(mut response) = crate::http::request("GET", url, &asking, None, HTTP_CONNECT, stop) {
            let streamed = response.header("content-type").is_some_and(|kind| kind.to_ascii_lowercase().starts_with("text/event-stream"));
            if response.status != 200 || !streamed {
                return;
            }
            let mut events = Events::default();
            while let Ok(Some(bytes)) = response.next() {
                wait = Duration::from_secs(1);
                for message in events.read(&bytes) {
                    heard(url, &posting, &message, changed, stop);
                }
            }
        }
        let until = Instant::now() + wait;
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
        wait = (wait * 2).min(Duration::from_secs(60));
    }
}

/// How long a connection to a server at a URL is waited for.
const HTTP_CONNECT: Duration = Duration::from_secs(10);

/// What is said of a server whose program is neither where its command says nor in a folder on PATH, after the command: words the app has in the person's language.
pub const NOT_FOUND: &str = "the program is not there, nor in a folder on PATH";

/// What a request comes to when the server at a URL no longer knows its session, as a server may forget one that has been quiet.
const SESSION_GONE: &str = "the server no longer knows this session";

impl HttpLink {
    fn new(url: &str, headers: &BTreeMap<String, String>) -> Result<HttpLink, String> {
        let url = crate::http::Url::parse(url)?;
        let headers = headers.iter().map(|(name, value)| (name.clone(), value.clone())).collect();
        Ok(HttpLink { url, headers, session: None, initialized: false, changed: Arc::new(AtomicBool::new(false)), listening: None })
    }

    /// The session's stream read on a thread of its own, from now until the session ends (the stream of one before it ends here).
    fn listen(&mut self) {
        let Some(session) = self.session.clone() else { return };
        let mut headers = self.headers.clone();
        headers.push(("MCP-Protocol-Version".into(), PROTOCOL.into()));
        headers.push(("Mcp-Session-Id".into(), session));
        let stop = Arc::new(AtomicBool::new(false));
        self.listening = Some(Listening(stop.clone()));
        let (url, changed) = (self.url.clone(), self.changed.clone());
        std::thread::spawn(move || listen(&url, &headers, &changed, &stop));
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
        let (give_up, done, started) = (AtomicBool::new(false), AtomicBool::new(false), Instant::now());
        let outcome = std::thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) || started.elapsed() > limit {
                        give_up.store(true, Ordering::Relaxed);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            });
            let outcome = self.exchange(&message.to_string(), &headers, id, &give_up);
            done.store(true, Ordering::Relaxed);
            outcome
        });
        match outcome {
            Err(_) if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) => Err("stopped".to_string()),
            Err(_) if give_up.load(Ordering::Relaxed) => Err(format!("no answer to {method} within {} s", limit.as_secs())),
            Err(error) => Err(error),
            Ok(answer) => {
                if method == "initialize" {
                    self.initialized = true;
                }
                // (the session begun: what the server says between requests comes on a stream of its own)
                if method == "notifications/initialized" {
                    self.listen();
                }
                Ok(answer)
            }
        }
    }

    fn exchange(&mut self, body: &str, headers: &[(String, String)], id: Option<u64>, give_up: &AtomicBool) -> Result<Option<Value>, String> {
        let mut response = crate::http::request("POST", &self.url, headers, Some(body), HTTP_CONNECT, give_up).map_err(|error| format!("{}: {error}", self.url.host))?;
        // (as the protocol has a server say it no longer knows the session it was sent: this side begins another)
        if response.status == 404 && self.session.is_some() {
            (self.session, self.initialized, self.listening) = (None, false, None);
            return Err(SESSION_GONE.to_string());
        }
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
            let answer: Value = message_of(&text).map_err(|error| format!("the server's answer is not JSON: {error}"))?;
            let found = match answer {
                Value::Array(messages) => messages.into_iter().find(is_ours),
                message => Some(message).filter(is_ours),
            };
            return found.map(Some).ok_or_else(|| "the server's answer was not to this request".to_string());
        }
        // An event stream: its events' data, until the answer to this request is among them.
        let mut events = Events::default();
        loop {
            let Some(bytes) = response.next().map_err(|error| error.to_string())? else {
                return Err("the server's event stream ended with no answer to this request".to_string());
            };
            for message in events.read(&bytes) {
                if is_ours(&message) {
                    return Ok(Some(message));
                }
                heard(&self.url, headers, &message, &self.changed, give_up);
            }
        }
    }
}

impl Drop for HttpLink {
    /// The session ends with this side of it, as the protocol asks of a client that is done.
    fn drop(&mut self) {
        self.listening = None;
        let Some(session) = self.session.take() else { return };
        let mut headers = self.headers.clone();
        headers.push(("Mcp-Session-Id".into(), session));
        let never = AtomicBool::new(false);
        let _ = crate::http::request_within("DELETE", &self.url, &headers, Duration::from_secs(2), &never, Duration::from_secs(3));
    }
}

impl Rpc {
    /// Reads the server's lines on a thread of its own; it answers what the server asks of it itself (see `answer_to_server`), as the server may wait on that.
    fn over(writer: Box<dyn Write + Send>, reader: impl BufRead + Send + 'static) -> Rpc {
        let writer = Arc::new(Mutex::new(writer));
        let (sender, answers) = channel();
        let replies = writer.clone();
        let changed = Arc::new(AtomicBool::new(false));
        let told = changed.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut line = Vec::new();
            loop {
                line.clear();
                // (a line is read as bytes: one that is not UTF-8, a script's message in the machine's own encoding, must not end the reader)
                if matches!(reader.read_until(b'\n', &mut line), Ok(0) | Err(_)) {
                    return;
                }
                let text = String::from_utf8_lossy(&line);
                let message = match message_of(&text) {
                    Ok(message) => message,
                    // (an answer that cannot be read is said to the call that waits for it, which would otherwise wait out its limit, ten minutes)
                    Err(error) => match answer_id_in(&text) {
                        Some(id) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32700, "message": format!("the server's answer could not be read: {error}")}}),
                        None => continue,
                    },
                };
                if let Some(reply) = answer_to_server(&message) {
                    let mut out = replies.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    let _ = writeln!(out, "{reply}").and_then(|()| out.flush());
                    continue;
                }
                // (before an answer that follows it is passed on: the call it came in asks for the list again as it ends)
                if message.get("method").and_then(Value::as_str) == Some(LIST_CHANGED) {
                    told.store(true, Ordering::Relaxed);
                    continue;
                }
                if message.get("id").is_some() && message.get("method").is_none() && sender.send(message).is_err() {
                    return;
                }
            }
        });
        Rpc { writer, answers, next: 1, http: None, changed }
    }

    /// For a server at a URL: its messages go by HTTP, and the pipes are none.
    fn http(link: HttpLink) -> Rpc {
        let (_, answers) = channel();
        let changed = link.changed.clone();
        Rpc { writer: Arc::new(Mutex::new(Box::new(std::io::sink()))), answers, next: 1, http: Some(Mutex::new(link)), changed }
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
        if self.http.is_some() {
            let asked = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            let mut outcome = self.post(&asked, limit, cancel);
            // (a new session, and the request again in it: the old session never took it)
            if method != "initialize" && matches!(&outcome, Err(error) if error == SESSION_GONE) {
                outcome = self.begin_again(limit).and_then(|()| self.post(&asked, limit, cancel));
            }
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

    /// One message to a server at a URL; its answer, if it has one.
    fn post(&self, message: &Value, limit: Duration, cancel: Option<&AtomicBool>) -> Result<Option<Value>, String> {
        let http = self.http.as_ref().ok_or("not a server at a URL")?;
        http.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).post(message, limit, cancel)
    }

    /// A new session with a server at a URL that let the last one go: the handshake again, without its list of tools, which the session does not change.
    fn begin_again(&mut self, limit: Duration) -> Result<(), String> {
        let id = self.next;
        self.next += 1;
        let answer = self.post(&json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": hello()}), limit, None)?;
        if let Some(error) = answer.as_ref().and_then(|answer| answer.get("error")) {
            return Err(format!("a new session was refused: {error}"));
        }
        self.post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), limit, None).map(|_| ())
    }

    /// The handshake, and what tools the server has, each step given this long to answer.
    fn start(&mut self, limit: Duration) -> Result<Vec<Value>, String> {
        self.call("initialize", hello(), limit, None)?;
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        self.list_tools(limit, None)
    }

    /// What tools the server has, across its pages, each given `limit` to answer. What it said of its list before this is answered by it.
    fn list_tools(&mut self, limit: Duration, cancel: Option<&AtomicBool>) -> Result<Vec<Value>, String> {
        self.changed.store(false, Ordering::Relaxed);
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for page_number in 1.. {
            let params = cursor.as_ref().map_or(json!({}), |cursor| json!({"cursor": cursor}));
            let page = self.call("tools/list", params, limit, cancel)?;
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

/// A server's message as it is written, or with what JSON cannot hold mended. Python's `json.dumps` writes a byte it could not decode as half of a
/// surrogate pair alone (`\udc94`), as a Python server on a Japanese Windows does with the UTF-8 it reads as cp932; JSON readers refuse that, and
/// the answer it was in was lost. Each such half is read as U+FFFD.
fn message_of(text: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str(text).or_else(|error| match lone_halves_mended(text) {
        Some(mended) => serde_json::from_str(&mended),
        None => Err(error),
    })
}

/// `text` with each `\u` escape of half a surrogate pair that has not its other half after it written as `\ufffd`; none if it had none.
fn lone_halves_mended(text: &str) -> Option<String> {
    let code = |at: &str| at.strip_prefix("\\u").and_then(|hex| hex.get(..4)).and_then(|hex| u16::from_str_radix(hex, 16).ok());
    let (mut mended, mut rest, mut changed) = (String::with_capacity(text.len()), text, false);
    while let Some(at) = rest.find('\\') {
        mended.push_str(&rest[..at]);
        rest = &rest[at..];
        let taken = match code(rest) {
            Some(0xD800..=0xDBFF) if code(&rest[6..]).is_some_and(|low| (0xDC00..=0xDFFF).contains(&low)) => 12,
            Some(0xD800..=0xDFFF) => {
                mended.push_str("\\ufffd");
                changed = true;
                rest = &rest[6..];
                continue;
            }
            Some(_) => 6,
            // (an escape of one character, `\\` among them, whose second is no start of another)
            None => 1 + rest[1..].chars().next().map_or(0, char::len_utf8),
        };
        mended.push_str(&rest[..taken]);
        rest = &rest[taken..];
    }
    mended.push_str(rest);
    changed.then_some(mended)
}

/// The id of the answer a line that cannot be read was meant to be, as far as it can be told: a number after `"id":`, in a line that is not a request.
fn answer_id_in(line: &str) -> Option<u64> {
    if line.contains("\"method\"") {
        return None;
    }
    let after = line.split_once("\"id\"")?.1.trim_start().strip_prefix(':')?.trim_start();
    after[..after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len())].parse().ok()
}

/// This side's answer to a request the server makes of it, none for what is not a request. A ping is answered. Anything else (the person's folders, a model's
/// answer, a question for the person: what the handshake did not offer) is refused at once, as a server that waits for an answer never given holds the call
/// it is in until that call's limit, ten minutes.
fn answer_to_server(message: &Value) -> Option<Value> {
    let (id, method) = (message.get("id")?, message.get("method")?.as_str()?);
    Some(match method {
        "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        _ => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("Method not found: {method}")}}),
    })
}

/// What this side says of itself in the handshake.
fn hello() -> Value {
    json!({"protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": {"name": "dogmind-app", "version": env!("CARGO_PKG_VERSION")}})
}

/// A call's arguments as its tool's schema has them: a field the schema says is text, given as a number or as true or false, as its text. An engine
/// that reads a call's arguments by the tools it was offered reads those of a tool it was only told of (see `Agent::servers_caught_up`) as what
/// they look like: a path `2024` as a number, which the server refuses. The top fields only, as the engine reads them.
fn as_the_schema_has_it(arguments: &Value, schema: Option<&Value>) -> Value {
    let properties = schema.and_then(|schema| schema.pointer("/function/parameters/properties")).and_then(Value::as_object);
    let (Some(fields), Some(properties)) = (arguments.as_object(), properties) else { return arguments.clone() };
    let only_text = |schema: &Value| {
        let kinds: Vec<&Value> = match schema.get("type") {
            Some(Value::Array(kinds)) => kinds.iter().collect(),
            Some(kind) => vec![kind],
            None => Vec::new(),
        };
        kinds.iter().any(|kind| kind.as_str() == Some("string")) && kinds.iter().all(|kind| matches!(kind.as_str(), Some("string" | "null")))
    };
    let mut fixed = fields.clone();
    for (name, value) in &mut fixed {
        if (value.is_number() || value.is_boolean()) && properties.get(name).is_some_and(only_text) {
            *value = Value::String(value.to_string());
        }
    }
    Value::Object(fixed)
}

/// A tool's result as text: what it says in words, and a word for what it says otherwise.
/// A resource it holds is read as its text, with its URI (a file's contents come so from GitHub's server); one of bytes, and a link to one, by what they
/// are. (The tests' word for `result_parts` with no pictures.)
#[cfg(test)]
fn result_text(result: &Value) -> String {
    result_parts(result, false).0
}

/// A picture a tool returned, as its bytes.
pub struct Picture {
    pub bytes: Vec<u8>,
}

/// A tool's result as text (see `result_text`), and with `pictures` the pictures in it, each said in the text where it was: one whose data is not
/// base64 is not shown, and is said so.
fn result_parts(result: &Value, pictures: bool) -> (String, Vec<Picture>) {
    let said = |part: &Value, field: &str| part.get(field).and_then(Value::as_str).unwrap_or("").to_string();
    let mut taken = Vec::new();
    let parts: Vec<String> = result.get("content").and_then(Value::as_array).into_iter().flatten().map(|part| match part.get("type").and_then(Value::as_str) {
        Some("text") => said(part, "text"),
        Some("image") if pictures => {
            let data: String = said(part, "data").chars().filter(|c| !c.is_whitespace()).collect();
            match crate::conversation_key::b64d(&data) {
                Some(bytes) => {
                    taken.push(Picture { bytes });
                    format!("[picture {} ({}): shown with this result]", taken.len(), said(part, "mimeType"))
                }
                None => "[image content is not shown: its data is not base64]".to_string(),
            }
        }
        Some("resource") => {
            let resource = part.get("resource").unwrap_or(&Value::Null);
            match resource.get("text").and_then(Value::as_str) {
                Some(text) => format!("[resource {}]\n{text}", said(resource, "uri")),
                None => format!("[resource {}: {} bytes, not shown]", said(resource, "uri"), said(resource, "mimeType")),
            }
        }
        Some("resource_link") => {
            let about = [said(part, "name"), said(part, "description")].into_iter().filter(|text| !text.is_empty()).collect::<Vec<_>>().join(": ");
            format!("[resource link {}{}]", said(part, "uri"), if about.is_empty() { String::new() } else { format!(" ({about})") })
        }
        Some(kind) => format!("[{kind} content is not shown]"),
        None => part.to_string(),
    }).collect();
    let text = if parts.is_empty() { result.get("structuredContent").map_or(String::new(), Value::to_string) } else { parts.join("\n") };
    (crate::tools::spill("mcp", &text, MAX_RESULT), taken)
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
    words: LastWords,
}

/// The last lines a server wrote to its stderr. Why one did not start, or ended, is there more often than not (a module not found, a key not set), and a
/// person mends it by them.
#[derive(Clone, Default)]
struct LastWords(Arc<(Mutex<std::collections::VecDeque<String>>, AtomicBool)>);

impl LastWords {
    const LINES: usize = 6;
    const CHARS: usize = 300;

    /// Reads `stderr` on a thread of its own to its end, keeping the last lines.
    fn hear(&self, stderr: impl std::io::Read + Send + 'static) {
        let kept = self.clone();
        std::thread::spawn(move || {
            let (mut reader, mut line) = (BufReader::new(stderr), Vec::new());
            loop {
                line.clear();
                if matches!(reader.read_until(b'\n', &mut line), Ok(0) | Err(_)) {
                    break;
                }
                let text: String = String::from_utf8_lossy(&line).trim().chars().take(Self::CHARS).collect();
                if !text.is_empty() {
                    let mut lines = kept.0 .0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    lines.push_back(text);
                    while lines.len() > Self::LINES {
                        lines.pop_front();
                    }
                }
            }
            kept.0 .1.store(true, Ordering::Relaxed);
        });
    }

    /// `error`, with what the server last said, once it has said all it will: its end is waited for a moment.
    fn after(&self, error: String) -> String {
        let until = Instant::now() + Duration::from_millis(500);
        while !self.0 .1.load(Ordering::Relaxed) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        let lines = self.0 .0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if lines.is_empty() {
            return error;
        }
        format!("{error}. It said: {}", lines.iter().cloned().collect::<Vec<_>>().join(" | "))
    }
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

/// A server that is up, with its tools as it last listed them. One that lists them again (see `Hub::relisted`) is another of these, on the same connection
/// and process, which end with the last that has them.
struct Running {
    rpc: Arc<Mutex<Rpc>>,
    /// The connection's `Rpc::changed`, to be read without waiting for a call that holds it.
    changed: Arc<AtomicBool>,
    /// What each tool is called by its server, by the name the model knows it by.
    names: BTreeMap<String, String>,
    schemas: Vec<Value>,
    process: Option<Arc<Process>>,
}

impl Running {
    /// Up, with the tools it listed as `schemas` (under its own names, and none placed yet).
    fn new(rpc: Rpc, schemas: Vec<Value>, process: Option<Process>) -> Running {
        Running { changed: rpc.changed.clone(), rpc: Arc::new(Mutex::new(rpc)), names: BTreeMap::new(), schemas, process: process.map(Arc::new) }
    }

    /// The same server with its tools as it lists them now.
    fn with_tools(&self, names: BTreeMap<String, String>, schemas: Vec<Value>) -> Running {
        Running { rpc: self.rpc.clone(), changed: self.changed.clone(), names, schemas, process: self.process.clone() }
    }

    fn is(&self, other: &Running) -> bool {
        Arc::ptr_eq(&self.rpc, &other.rpc)
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
    /// The names of the tools of a server that has ended, so that a call for one can be told what became of it.
    had: Vec<String>,
    /// While it starts again, on a thread of its own.
    restarting: bool,
    /// The names of the tools it no longer lists, as it said its list changed: a call for one is told so.
    dropped: Vec<String>,
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
    /// Why that file could not be read, the last time it was.
    unreadable: Mutex<Option<String>>,
}

fn start_server(config: &ServerConfig) -> Result<Running, String> {
    start_server_within(config, START_LIMIT, true)
}

/// Starts a server, each step of its handshake given `limit` to answer. A job holds it unless `job` is false (for a test of a server that none does).
fn start_server_within(config: &ServerConfig, limit: Duration, job: bool) -> Result<Running, String> {
    if config.is_http() {
        if config.kind == "sse" {
            let mut rpc = older_transport(&config.url, &config.headers, limit)?;
            let tools = rpc.start(limit)?;
            return Ok(Running::new(rpc, tools, None));
        }
        let mut rpc = Rpc::http(HttpLink::new(&config.url, &config.headers)?);
        return match rpc.start(limit) {
            Ok(tools) => Ok(Running::new(rpc, tools, None)),
            // (one of the older transport answers a POST to its URL so, 405 or 404: the protocol has a client that would reach both try a GET of it next;
            // what is said if that fails too is why the POST did)
            Err(error) if error.starts_with("the server answered 4") => {
                let older = older_transport(&config.url, &config.headers, limit).and_then(|mut older| older.start(limit).map(|tools| (older, tools)));
                older.map(|(older, tools)| Running::new(older, tools, None)).map_err(|_| error)
            }
            Err(error) => Err(error),
        };
    }
    let (path, script) = program(&config.command);
    // Resolve explicit relative executable paths before spawning: Windows looks them up from the parent process.
    let path = match &config.cwd {
        Some(cwd) if path.is_relative() && Path::new(&config.command).components().count() > 1 => {
            std::path::absolute(cwd.join(&path)).map_err(|error| format!("{}: {error}", cwd.display()))?
        }
        _ => path,
    };
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
    // (the protocol's messages are UTF-8: a Python server reads and writes its pipes in the machine's own code page otherwise, cp932 on a Japanese
    // Windows, and Japanese in a call came to it garbled; what the configuration says comes after, and wins)
    command.env("PYTHONIOENCODING", "utf-8");
    if let Some(cwd) = &config.cwd {
        command.current_dir(cwd);
    }
    command.args(&config.args).envs(&config.env).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let child = command.spawn().map_err(|error| {
        if let Some(cwd) = config.cwd.as_ref().filter(|cwd| !cwd.is_dir()) {
            return format!("{}: {error}", cwd.display());
        }
        match error.kind() {
            std::io::ErrorKind::NotFound => format!("{}: {NOT_FOUND}", config.command),
            _ => format!("{}: {error}", config.command),
        }
    })?;
    #[cfg(windows)]
    let ending = if job { ends_with_this_process(&child) } else { None };
    #[cfg(not(windows))]
    let _ = job;
    // (from here on, whatever fails, the process ends as this is dropped: a handshake that gets no answer is otherwise a server that goes on running)
    let words = LastWords::default();
    let mut child = child;
    if let Some(stderr) = child.stderr.take() {
        words.hear(stderr);
    }
    let mut process = Process {
        child,
        #[cfg(windows)]
        ending,
        words: words.clone(),
    };
    let writer = process.child.stdin.take().ok_or("no stdin")?;
    let reader = process.child.stdout.take().ok_or("no stdout")?;
    let mut rpc = Rpc::over(Box::new(writer), BufReader::new(reader));
    let tools = rpc.start(limit).map_err(|error| words.after(error))?;
    Ok(Running::new(rpc, tools, Some(process)))
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
fn place_tools(server: &str, part: &str, tools: &[Value], selection: Option<&ToolSelection>) -> (BTreeMap<String, String>, Vec<Value>) {
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
    // Assign aliases before filtering so excluding a tool cannot give its name to a different tool.
    if let Some(selection) = selection.filter(|s| s.enabled_tools.is_some() || !s.disabled_tools.is_empty()) {
        names.retain(|_, original| selection.allows(original));
        schemas.retain(|schema| schema["function"]["name"].as_str().is_some_and(|name| names.contains_key(name)));
    }
    (names, schemas)
}

fn ready(name: &str, part: &str, config: &ServerConfig) -> Result<Arc<Running>, String> {
    let mut running = start_server(config)?;
    let tools = std::mem::take(&mut running.schemas);
    (running.names, running.schemas) = place_tools(name, part, &tools, Some(&config.tool_selection));
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
        self.reload_retrying(None)
    }

    /// Reload settings and retry the named server if its last connection attempt failed.
    pub fn reload_retrying(&self, failed_server: Option<&str>) -> Vec<String> {
        let Some(file) = self.file.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone() else { return Vec::new() };
        // (a file that cannot be read, as one written by hand can be, starts nothing and ends nothing: it is said, for the person to mend)
        let read = readable_config(&file);
        *self.unreadable.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = read.as_ref().err().cloned();
        let Ok(Config { servers }) = read else { return Vec::new() };
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
                let retry = failed_server == Some(name.as_str()) && slot.failed.is_some();
                if !retry && slot.config.as_ref() == Some(&same) && slot.part == part {
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

    /// Why the file could not be read, if it could not when it was last: then none of what it lists was started.
    pub fn unreadable(&self) -> Option<String> {
        self.unreadable.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// The tools of the servers up by now. A session that began before one was up is offered its tools from the next turn the person starts. A server that
    /// said its tools changed while no call was made of it is asked for them first, unless a call has it now: that call asks as it ends.
    pub fn schemas(&self) -> Vec<Value> {
        let told: Vec<Arc<Running>> = {
            let slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            slots.iter().filter_map(|slot| slot.running.clone()).filter(|running| running.changed.load(Ordering::Relaxed)).collect()
        };
        for running in told {
            let Ok(mut rpc) = running.rpc.try_lock() else { continue };
            let listed = rpc.list_tools(LIST_LIMIT, None);
            drop(rpc);
            self.relisted(&running, listed);
        }
        let slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        slots.iter().filter_map(|slot| slot.running.as_ref()).flat_map(|running| running.schemas.iter().cloned()).collect()
    }

    pub fn call(&self, tool: &str, arguments: &Value, cancel: &AtomicBool) -> Result<String, String> {
        self.call_with_pictures(tool, arguments, cancel, false).map(|(text, _)| text)
    }

    /// A call, and with `pictures` the pictures its result holds, said where they were in its text.
    pub fn call_with_pictures(&self, tool: &str, arguments: &Value, cancel: &AtomicBool, pictures: bool) -> Result<(String, Vec<Picture>), String> {
        let running = {
            let slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let up = slots.iter().filter_map(|slot| slot.running.clone()).find(|running| running.names.contains_key(tool));
            up.ok_or_else(|| match slots.iter().find(|slot| slot.had.iter().any(|had| had == tool)) {
                Some(slot) if slot.restarting => format!("{tool}: its server is starting again"),
                Some(Slot { failed: Some(reason), .. }) => format!("{tool}: its server did not start again: {reason}"),
                _ if slots.iter().any(|slot| slot.running.is_some() && slot.dropped.iter().any(|name| name == tool)) => format!("{tool}: its server no longer has this tool"),
                _ => format!("{tool}: its server is not running"),
            })?
        };
        let real = running.names.get(tool).cloned().unwrap_or_default();
        let schema = running.schemas.iter().find(|schema| schema.pointer("/function/name").and_then(Value::as_str) == Some(tool));
        let arguments = &as_the_schema_has_it(arguments, schema);
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
            let answer = rpc.call("tools/call", json!({"name": real, "arguments": arguments}), CALL_LIMIT, Some(cancel));
            // (said in the call, as a server says it that adds tools on request: asked for while it is held, so the next request has them)
            let listed = running.changed.load(Ordering::Relaxed).then(|| rpc.list_tools(LIST_LIMIT, Some(cancel)));
            drop(rpc);
            if let Some(listed) = listed {
                self.relisted(&running, listed);
            }
            answer
        };
        let result = match answer {
            Ok(result) => result,
            Err(error) if error == ENDED || error.starts_with(PIPE) => {
                let error = match &running.process {
                    Some(process) => process.words.after(error),
                    None => error,
                };
                return Err(self.start_again(running, error));
            }
            Err(error) => return Err(error),
        };
        let (text, taken) = result_parts(&result, pictures);
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(text);
        }
        Ok((text, taken))
    }

    /// What a server that said its tools changed lists now, in place of what it listed: for calls at once, and offered to a session from its next request.
    /// The names it no longer lists are kept, for a call of one to be told so. A list that could not be had leaves what was, unless the person stopped
    /// the call it was asked for in: it is asked for again.
    fn relisted(&self, running: &Running, listed: Result<Vec<Value>, String>) {
        let tools = match listed {
            Ok(tools) => tools,
            Err(error) => {
                if error == "stopped" {
                    running.changed.store(true, Ordering::Relaxed);
                }
                return;
            }
        };
        let mut slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(slot) = slots.iter_mut().find(|slot| slot.running.as_ref().is_some_and(|now| now.is(running))) else { return };
        let Some(now) = slot.running.clone() else { return };
        let (names, schemas) = place_tools(&slot.name, &slot.part, &tools, slot.config.as_ref().map(|c| &c.tool_selection));
        if names == now.names && schemas == now.schemas {
            return;
        }
        slot.dropped.retain(|name| !names.contains_key(name));
        slot.dropped.extend(now.names.keys().filter(|name| !names.contains_key(*name)).cloned());
        slot.running = Some(Arc::new(now.with_tools(names, schemas)));
    }

    /// A server that has ended starts again, on a thread of its own: its handshake can take a minute, and a person who stops the turn does not wait for that. This call is
    /// not made again: it may have done what it was asked before the server went.
    fn start_again(&self, ended: Arc<Running>, error: String) -> String {
        let start = {
            let mut slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let slot = slots.iter_mut().find(|slot| slot.running.as_ref().is_some_and(|running| running.is(&ended)));
            slot.and_then(|slot| {
                // (its tools as it last listed them, which the call that found it ended may have had before)
                slot.had = slot.running.as_ref().map_or_else(Vec::new, |running| running.names.keys().cloned().collect());
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
                        slot.dropped.retain(|name| !running.names.contains_key(name));
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
        self.add_running(server, tools, Box::new(std::io::sink()), std::io::Cursor::new(script.into_bytes()), trusted);
    }

    /// A server, up with these tools, that says each of `turns` as it is sent a request, in order: what it says in a call is said in it, and not before.
    #[cfg(test)]
    pub(crate) fn answering(server: &str, tools: &[Value], turns: Vec<Vec<Value>>, trusted: bool) -> Arc<Hub> {
        let (said, heard) = channel();
        let turns = turns.iter().map(|lines| lines.iter().map(|line| format!("{line}\n")).collect::<String>().into_bytes()).collect();
        let hub = Arc::new(Hub::default());
        hub.add_running(server, tools, Box::new(Turns { turns, said, written: Vec::new() }), BufReader::new(Heard { from: heard, pending: Vec::new() }), trusted);
        hub
    }

    #[cfg(test)]
    fn add_running(&self, server: &str, tools: &[Value], writer: Box<dyn Write + Send>, reader: impl BufRead + Send + 'static, trusted: bool) {
        let (names, schemas) = place_tools(server, &name_part(server), tools, None);
        let running = Arc::new(Running { names, schemas, ..Running::new(Rpc::over(writer, reader), Vec::new(), None) });
        self.slots.lock().unwrap().push(Slot { name: server.into(), part: name_part(server), running: Some(running), trusted, ..Slot::default() });
    }

    /// The desktop client's current view. Keep launch arguments, headers and server error messages out of the prompt.
    pub(crate) fn connection_facts(&self) -> String {
        let states: Vec<String> = self.states().into_iter().map(|(name, state)| {
            let state = match state {
                State::Up(count) => format!("connected, tools: {count}"),
                State::Starting => "starting".into(),
                State::Failed(_) => "failed to start".into(),
            };
            format!("{name}: {state}")
        }).collect();
        let mut status = states.join("; ");
        if self.unreadable().is_some() {
            if !status.is_empty() {
                status.push_str(". ");
            }
            status.push_str("Dogmind could not read its MCP configuration");
        } else if status.is_empty() {
            status.push_str("No servers configured in Dogmind. For a connection question, give the setup steps directly. Probing ports from the sandbox cannot establish this client's connection status");
        }
        format!("Dogmind has a built-in MCP client outside the command sandbox. {status}. The user can enter `/mcp` for status, `/mcp candidates` for importable names, and `/mcp import NAME` to add and start one. Bare `/mcp import` imports all available servers from Claude and Codex. To add one manually, the user opens Settings > Tools > MCP servers > Add a server. On connection its tools join this open conversation automatically, without a restart or reconnect command.")
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

/// The side of `Hub::answering`'s server that is written to: each request it is sent lets the next of its turns be said.
#[cfg(test)]
struct Turns {
    turns: std::collections::VecDeque<Vec<u8>>,
    said: std::sync::mpsc::Sender<Vec<u8>>,
    written: Vec<u8>,
}

#[cfg(test)]
impl Write for Turns {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.written.extend_from_slice(bytes);
        while let Some(end) = self.written.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.written.drain(..=end).collect();
            let request = serde_json::from_slice::<Value>(&line).is_ok_and(|message| message.get("id").is_some() && message.get("method").is_some());
            if let Some(turn) = self.turns.pop_front().filter(|_| request) {
                let _ = self.said.send(turn);
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What a server says, as it says it, read as a program's output is: its end as the other side goes. The older transport's stream (see `older_transport`),
/// and `Hub::answering`'s server.
struct Heard {
    from: Receiver<Vec<u8>>,
    pending: Vec<u8>,
}

impl std::io::Read for Heard {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.pending.is_empty() {
            match self.from.recv() {
                Ok(bytes) => self.pending = bytes,
                Err(_) => return Ok(0),
            }
        }
        let count = buffer.len().min(self.pending.len());
        buffer[..count].copy_from_slice(&self.pending[..count]);
        self.pending.drain(..count);
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn mcp_json_settings_accept_a_leading_utf8_bom() {
        let dir = crate::in_container::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        let text = "\u{feff}{\"mcpServers\":{\"editor\":{\"command\":\"local\",\"args\":[\"a\u{feff}b\"]}}}";
        std::fs::write(&file, text).unwrap();
        let own = readable_config(&file).unwrap().servers;
        assert_eq!(own["editor"].args, ["a\u{feff}b"]);
        assert_eq!(load_config(&file).servers, own);
        assert_eq!(servers_of_claude(text), own);

        std::fs::write(&file, "\u{feff}{broken}").unwrap();
        assert!(readable_config(&file).is_err());
        assert!(parse_claude_servers("\u{feff}{broken}").is_none());
    }

    #[test]
    fn claudes_file_is_read_as_it_is() {
        let text = r#"{"numStartups": 3, "mcpServers": {"files": {"type": "stdio", "command": "npx", "args": ["-y", "server"], "env": {"K": "v"}}, "remote": {"type": "http", "url": "https://x", "headers": {"Authorization": "Bearer t"}}, "old": {"type": "sse", "url": "https://y"}}}"#;
        let found = servers_of_claude(text);
        assert_eq!(found.keys().collect::<Vec<_>>(), ["files", "old", "remote"]);
        assert!(found["old"].is_http() && found["old"].kind == "sse", "one of the older transport is read, as that");
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
    fn codex_import_preserves_literal_windows_paths_and_argument_boundaries() {
        let text = r#"
[mcp_servers.unity_mcp]
command = "node"
args = [
    'C:\Tools\Unity MCP\build\index.js', # literal backslashes
    "--name=a,b", "",
]
[mcp_servers.off]
command = "node"
enabled = false
"#;
        let found = servers_of_codex(text);
        assert_eq!(found["unity_mcp"].args, [r"C:\Tools\Unity MCP\build\index.js", "--name=a,b", ""]);
        assert!(!found.contains_key("off"));
    }

    #[test]
    fn imported_launch_settings_leave_trust_to_the_person_in_dogmind() {
        let dir = crate::in_container::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        let sources = servers_of_claude(r#"{"mcpServers":{"editor":{"command":"editor","args":["--stdio"],"trusted":true}}}"#);
        assert_eq!(import(&file, sources.clone()).unwrap(), 1);
        let mut editor = readable_config(&file).unwrap().servers.remove("editor").unwrap();
        assert_eq!(editor.args, ["--stdio"]);
        assert!(!editor.trusted);
        editor.trusted = true;
        save_server(&file, Some("editor"), "editor", editor).unwrap();
        let mut repeated = sources;
        repeated.get_mut("editor").unwrap().trusted = false;
        assert_eq!(import(&file, repeated).unwrap(), 0);
        assert!(readable_config(&file).unwrap().servers["editor"].trusted);
    }

    #[test]
    fn an_import_adds_and_keeps_what_was_there() {
        let dir = crate::in_container::tempdir().unwrap();
        let file = dir.path().join("deep/mcp.json");
        let one = |command: &str| ServerConfig { command: command.into(), ..ServerConfig::default() };
        assert_eq!(import(&file, BTreeMap::from([("a".to_string(), one("first"))])).unwrap(), 1);
        assert_eq!(import(&file, BTreeMap::from([("a".to_string(), one("second")), ("b".to_string(), one("third"))])).unwrap(), 1);
        let config = load_config(&file);
        assert_eq!(config.servers["a"].command, "first");
        assert_eq!(config.servers["b"].command, "third");
        // A file the person wrote by hand, with a comma too many: it is said, and left as it was.
        let mine = "{\"mcpServers\": {\"mine\": {\"command\": \"x\"},}}";
        std::fs::write(&file, mine).unwrap();
        let said = import(&file, BTreeMap::from([("c".to_string(), one("fourth"))])).unwrap_err();
        assert!(said.contains("mcp.json"), "{said}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), mine);
    }

    /// A file written by hand with a mistake in it: the hub says why, and starts nothing; mended, it starts what it lists.
    #[test]
    fn a_file_that_cannot_be_read_is_said_and_starts_nothing_until_it_can() {
        let dir = crate::in_container::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        std::fs::write(&file, "{\"mcpServers\": {\"a\": {\"command\": \"no-such-program-here\"},}}").unwrap();
        let hub = Hub::start(&file);
        assert!(hub.configs().is_empty() && hub.states().is_empty());
        assert!(hub.unreadable().is_some_and(|why| why.contains("mcp.json")), "{:?}", hub.unreadable());
        std::fs::write(&file, "{\"mcpServers\": {\"a\": {\"command\": \"no-such-program-here\"}}}").unwrap();
        assert_eq!(hub.reload(), ["a"]);
        assert_eq!(hub.unreadable(), None);
        // (its program is not there: said in words the app has in the person's language)
        let started = Instant::now();
        while hub.status().iter().all(|line| !line.contains("did not start")) && started.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(hub.status(), [format!("a: did not start: no-such-program-here: {NOT_FOUND}")]);
    }

    /// What each request to a test server carried: its head, and its body as JSON.
    type Seen = Arc<Mutex<Vec<(String, Value)>>>;

    /// A request as a test server reads it: its head, and its body, as long as its length says.
    fn request_of(stream: &mut std::net::TcpStream) -> Option<(String, String)> {
        use std::io::Read;
        let (mut data, mut block) = (Vec::new(), [0u8; 4096]);
        loop {
            let count = stream.read(&mut block).ok().filter(|count| *count > 0)?;
            data.extend_from_slice(&block[..count]);
            if let Some(at) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&data[..at]).to_string();
                let length: usize = head.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length: ").and_then(|value| value.trim().parse().ok())).unwrap_or(0);
                while data.len() < at + 4 + length {
                    let count = stream.read(&mut block).ok().filter(|count| *count > 0)?;
                    data.extend_from_slice(&block[..count]);
                }
                return Some((head, String::from_utf8_lossy(&data[at + 4..]).to_string()));
            }
        }
    }

    /// A server of the older transport for the tests, at `/sse`: a GET of it opens the stream, which says first to post to `/messages?session=1` (as the
    /// official SDK says it) and then carries each answer; a POST to `/sse` is answered 405, as such a server answers one of Streamable HTTP. Its one tool,
    /// `echo`, says what it was given. What each request carried is kept.
    fn older_server() -> (String, Seen) {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/sse", listener.local_addr().unwrap().port());
        let (seen, out) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(None::<std::net::TcpStream>)));
        let kept = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let (kept, out) = (kept.clone(), out.clone());
                std::thread::spawn(move || {
                    let Some((head, body)) = request_of(&mut stream) else { return };
                    let message: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    kept.lock().unwrap().push((head.clone(), message.clone()));
                    if head.starts_with("GET /sse") {
                        // (where answers go before where to post is said: a post can follow at once)
                        *out.lock().unwrap() = stream.try_clone().ok();
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\nevent: endpoint\ndata: /messages?session=1\n\n");
                        let _ = stream.read(&mut [0u8; 16]);
                        return;
                    }
                    if !head.starts_with("POST /messages?session=1 ") {
                        let _ = stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n");
                        let _ = stream.read(&mut [0u8; 16]);
                        return;
                    }
                    let result = match message["method"].as_str() {
                        Some("initialize") => Some(json!({"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}})),
                        Some("tools/list") => Some(json!({"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]})),
                        Some("tools/call") => Some(json!({"content": [{"type": "text", "text": format!("{} over the older transport", message["params"]["arguments"])}]})),
                        _ => None,
                    };
                    let _ = stream.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n");
                    if let (Some(result), Some(out)) = (result, out.lock().unwrap().as_mut()) {
                        let _ = out.write_all(format!("event: message\ndata: {}\n\n", json!({"jsonrpc": "2.0", "id": message["id"], "result": result})).as_bytes());
                    }
                    let _ = stream.read(&mut [0u8; 16]);
                });
            }
        });
        (url, seen)
    }

    /// A server of the older transport is reached as its configuration says (`sse`), or as its URL answers a POST of Streamable HTTP (405): its stream
    /// says where to post, each message is posted there with the headers, and the answers come on the stream.
    #[test]
    fn a_server_of_the_older_transport_is_reached_as_its_configuration_says_or_as_its_url_answers() {
        for kind in ["sse", "http"] {
            let (url, seen) = older_server();
            let dir = crate::in_container::tempdir().unwrap();
            let file = dir.path().join("mcp.json");
            let headers = BTreeMap::from([("Authorization".to_string(), "Bearer t".to_string())]);
            save_server(&file, None, "old", ServerConfig { url, kind: kind.into(), headers, ..ServerConfig::default() }).unwrap();
            let hub = Hub::start(&file);
            waited("the server to be up", || !hub.schemas().is_empty() || hub.status().iter().any(|line| line.contains("did not start")));
            assert_eq!(hub.call("mcp__old__echo", &json!({"a": 1}), &AtomicBool::new(false)), Ok(r#"{"a":1} over the older transport"#.to_string()), "{kind}: {:?}", hub.status());
            let seen = seen.lock().unwrap();
            let posted: Vec<&String> = seen.iter().filter(|(head, _)| head.starts_with("POST /messages")).map(|(head, _)| head).collect();
            assert!(posted.len() >= 4 && posted.iter().all(|head| head.to_ascii_lowercase().contains("authorization: bearer t")), "{kind}: {posted:?}");
            assert_eq!(seen.iter().any(|(head, _)| head.starts_with("POST /sse")), kind == "http", "{kind}: a POST to its URL first only when the configuration does not say");
        }
    }

    /// Where the older transport says to post is of its stream's origin, whichever way it is written.
    #[test]
    fn where_the_older_transport_says_to_post_is_of_its_own_origin() {
        let stream = crate::http::Url::parse("http://127.0.0.1:9/mcp/sse?x=1").unwrap();
        assert_eq!(endpoint_of(&stream, "/messages?sessionId=a").unwrap().path, "/messages?sessionId=a");
        assert_eq!(endpoint_of(&stream, " messages?s=1\n").unwrap().path, "/mcp/messages?s=1");
        assert_eq!(endpoint_of(&stream, "http://127.0.0.1:9/m").unwrap().path, "/m");
        assert!(endpoint_of(&stream, "http://example.com/m").is_err());
        assert!(endpoint_of(&stream, "https://127.0.0.1:9/m").is_err());
    }

    /// A server of Streamable HTTP for the tests, on this machine: it answers `initialize` in JSON with a session's id, a notification with 202, `tools/list` in an
    /// event stream (a notification first, then the answer), and `tools/call` in JSON with what it was asked and the headers it was sent. What each request carried
    /// is kept, to be read back.
    fn http_server(tool: &'static str) -> (String, Seen) {
        http_server_with(tool, false, false)
    }

    /// The same, and with `forgets` it lets its first session go once it has answered a call in it: a request in it is answered 404 from then on, and the next
    /// `initialize` begins `s-2`. With `announces` it offers its session's stream (otherwise a GET is answered 405), and says on it that its tools changed
    /// once it has listed them: from then on it lists `secret` too.
    fn http_server_with(tool: &'static str, forgets: bool, announces: bool) -> (String, Seen) {
        use std::io::Read;
        let sessions = Arc::new(Mutex::new((0u32, false)));
        let (listed, announced) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/mcp", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let kept = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let (kept, sessions, listed, announced) = (kept.clone(), sessions.clone(), listed.clone(), announced.clone());
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
                    let sent = headers.iter().find_map(|line| line.to_ascii_lowercase().strip_prefix("mcp-session-id: ").map(str::to_string));
                    let mut known = sessions.lock().unwrap();
                    let unknown = sent.is_some_and(|sent| known.1 || sent != format!("s-{}", known.0));
                    let reply = match message["method"].as_str() {
                        _ if head.starts_with("DELETE") => "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_string(),
                        _ if head.starts_with("GET") && !announces => "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n".to_string(),
                        _ if head.starts_with("GET") => {
                            drop(known);
                            let until = Instant::now() + Duration::from_secs(10);
                            while !listed.load(Ordering::Relaxed) && Instant::now() < until {
                                std::thread::sleep(Duration::from_millis(20));
                            }
                            announced.store(true, Ordering::Relaxed);
                            let note = json!({"jsonrpc": "2.0", "method": LIST_CHANGED});
                            let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\nevent: message\r\ndata: {note}\r\n\r\n").as_bytes());
                            // (held open, as a session's stream is, until the client goes)
                            let _ = stream.read(&mut block);
                            return;
                        }
                        Some("initialize") => {
                            *known = (known.0 + 1, false);
                            let body = answer(json!({"protocolVersion": PROTOCOL, "capabilities": {"tools": {}}}));
                            format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: s-{}\r\nContent-Length: {}\r\n\r\n{body}", known.0, body.len())
                        }
                        _ if unknown => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string(),
                        Some("tools/list") => {
                            let mut tools = vec![json!({"name": tool, "inputSchema": {"type": "object"}})];
                            if announced.load(Ordering::Relaxed) {
                                tools.push(json!({"name": "secret", "inputSchema": {"type": "object"}}));
                            }
                            listed.store(true, Ordering::Relaxed);
                            let note = json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {}});
                            let body = format!("event: message\r\ndata: {note}\r\n\r\nevent: message\r\ndata: {}\r\n\r\n", answer(json!({"tools": tools})));
                            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                        }
                        Some("tools/call") if message["params"]["name"] == "ask" => {
                            let asked = json!({"jsonrpc": "2.0", "id": "srv-1", "method": "roots/list", "params": {}});
                            let changed = json!({"jsonrpc": "2.0", "method": LIST_CHANGED});
                            let body = format!("event: message\r\ndata: {asked}\r\n\r\nevent: message\r\ndata: {changed}\r\n\r\nevent: message\r\ndata: {}\r\n\r\n", answer(json!({"content": [{"type": "text", "text": "asked"}]})));
                            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                        }
                        Some("tools/call") => {
                            known.1 = forgets && known.0 == 1;
                            let said = format!("{} with {}", message["params"]["arguments"], headers.join(" | "));
                            let body = answer(json!({"content": [{"type": "text", "text": said}]}));
                            format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                        }
                        _ => "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n".to_string(),
                    };
                    drop(known);
                    let _ = stream.write_all(reply.as_bytes());
                    // (the client closes first, as with a keep-alive server: one that closes first can have Windows reset the answer the client has not read)
                    let _ = stream.read(&mut block);
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
        let dir = crate::in_container::tempdir().unwrap();
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

    /// What a server asks of this side in a call's stream is answered by a POST in its session: a ping, and a refusal of what was not offered (its folders here).
    /// What it says there of its tools has them asked for again in the session.
    #[test]
    fn what_a_server_at_a_url_asks_is_answered_in_its_session() {
        let (url, seen) = http_server("ask");
        let dir = crate::in_container::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        save_server(&file, None, "remote", ServerConfig { url, ..ServerConfig::default() }).unwrap();
        let hub = Hub::start(&file);
        waited("the server to be up", || !hub.schemas().is_empty());
        assert_eq!(hub.call("mcp__remote__ask", &json!({}), &AtomicBool::new(false)).unwrap(), "asked");
        let seen = seen.lock().unwrap();
        let (head, reply) = seen.iter().find(|(_, message)| message["id"] == "srv-1").expect("the request was answered");
        assert_eq!((reply["error"]["code"].as_i64(), reply.get("method")), (Some(-32601), None), "{reply}");
        assert!(head.starts_with("POST") && head.to_ascii_lowercase().contains("mcp-session-id: s-1"), "{head}");
        let listed = seen.iter().filter(|(head, message)| message["method"] == "tools/list" && head.to_ascii_lowercase().contains("mcp-session-id: s-1")).count();
        assert_eq!(listed, 2, "the list is asked for again in the session, after the handshake's");
    }

    /// A server over pipes is answered in them: its ping, and a refusal of what was not offered, each by the id it asked with.
    #[test]
    fn what_a_server_over_pipes_asks_is_answered() {
        let lines = [
            json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": "e", "method": "elicitation/create", "params": {"message": "sure?"}}),
            json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": "done"}]}}),
        ];
        let script: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let sent = Sent::default();
        let mut rpc = Rpc::over(Box::new(sent.clone()), Cursor::new(script.into_bytes()));
        assert_eq!(result_text(&rpc.call("tools/call", json!({}), Duration::from_secs(5), None).unwrap()), "done");
        waited("both answers", || sent.messages().len() == 3);
        // (the reader answers as it reads, which may be before the call is written)
        let answers: Vec<Value> = sent.messages().into_iter().filter(|message| message.get("method").is_none()).collect();
        assert_eq!(answers[0], json!({"jsonrpc": "2.0", "id": 7, "result": {}}));
        assert_eq!((answers[1]["id"].as_str(), answers[1]["error"]["code"].as_i64()), (Some("e"), Some(-32601)), "{}", answers[1]);
    }

    /// What a server at a URL says between requests, on its session's stream, is heard: its word that its tools changed has them asked for again, and the
    /// stream is asked for in the session, as an event stream.
    #[test]
    fn a_server_at_a_url_that_says_between_requests_that_its_tools_changed_is_heard() {
        let (url, seen) = http_server_with("echo", false, true);
        let dir = crate::in_container::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        save_server(&file, None, "remote", ServerConfig { url, ..ServerConfig::default() }).unwrap();
        let hub = Hub::start(&file);
        waited("the new tool", || hub.schemas().len() == 2);
        assert_eq!(hub.call("mcp__remote__secret", &json!({}), &AtomicBool::new(false)).map(|said| said.starts_with("{}")), Ok(true));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.iter().filter(|(_, message)| message["method"] == "tools/list").count(), 2, "asked for again, and not before it was said");
        let stream = &seen.iter().find(|(head, _)| head.starts_with("GET")).expect("the session's stream was asked for").0.to_ascii_lowercase();
        assert!(stream.contains("mcp-session-id: s-1") && stream.contains("accept: text/event-stream"), "{stream}");
    }

    /// Server-sent events are read whole, whichever pieces they come in.
    #[test]
    fn events_split_anywhere_are_read_whole() {
        let text = "event: message\r\ndata: {\"a\":\"\u{3042}\"}\r\n\r\n: a comment\n\ndata: {\"b\":2}\n\n";
        for cut in 0..=text.len() {
            let mut events = Events::default();
            let (one, two) = text.as_bytes().split_at(cut);
            let mut read = events.read(one);
            read.extend(events.read(two));
            assert_eq!(read, [json!({"a": "\u{3042}"}), json!({"b": 2})], "cut at {cut}");
        }
    }

    /// A server that lets its session go answers 404 in it: a new session is begun, and the call is made again in it, which the old one never took.
    #[test]
    fn a_session_the_server_let_go_is_begun_again_and_the_call_made_in_the_new_one() {
        let (url, seen) = http_server_with("echo", true, false);
        let dir = crate::in_container::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        save_server(&file, None, "remote", ServerConfig { url, ..ServerConfig::default() }).unwrap();
        let hub = Hub::start(&file);
        let started = Instant::now();
        while hub.schemas().is_empty() && started.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let call = |n: u32| hub.call("mcp__remote__echo", &json!({"n": n}), &AtomicBool::new(false)).unwrap();
        assert!(call(1).to_ascii_lowercase().contains("mcp-session-id: s-1"));
        assert!(call(2).to_ascii_lowercase().contains("mcp-session-id: s-2"), "the second call is made in the new session");
        let seen = seen.lock().unwrap();
        let methods: Vec<&str> = seen.iter().filter_map(|(_, message)| message["method"].as_str()).collect();
        assert_eq!(methods, ["initialize", "notifications/initialized", "tools/list", "tools/call", "tools/call", "initialize", "notifications/initialized", "tools/call"]);
        let again = seen.iter().filter(|(_, message)| message["method"] == "initialize").nth(1).unwrap();
        assert!(!again.0.to_ascii_lowercase().contains("mcp-session-id"), "the new handshake is sent with no session");
    }

    /// A change to the file, as Settings makes one, starts what is new and what changed again, ends what is gone, and leaves the rest as it was.
    #[test]
    fn a_change_to_the_servers_starts_and_ends_only_what_changed() {
        let ((one, _), (two, _)) = (http_server("first"), http_server("second"));
        let dir = crate::in_container::tempdir().unwrap();
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
        // (what a power cut leaves of the file, its length in zeros, is no list to keep: a server is added to an empty one)
        let length = std::fs::metadata(&file).unwrap().len() as usize;
        std::fs::write(&file, vec![0u8; length]).unwrap();
        save_server(&file, None, "d", at(&one)).unwrap();
        assert_eq!(load_config(&file).servers.keys().collect::<Vec<_>>(), ["d"]);
        assert!(save_server(&file, None, "x", at("ftp://x")).unwrap_err().contains("http"));
    }

    #[test]
    fn saving_a_failed_server_retries_only_that_server() {
        let dir = crate::in_container::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        std::fs::write(&file, r#"{"mcpServers":{"a":{},"b":{}}}"#).unwrap();
        let hub = Hub::start(&file);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !hub.states().iter().all(|(_, state)| matches!(state, State::Failed(_))) {
            assert!(Instant::now() < deadline, "waiting for the invalid configurations to fail");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(hub.reload().is_empty());
        assert_eq!(hub.reload_retrying(Some("a")), ["a"]);
        assert!(matches!(&hub.states()[1].1, State::Failed(_)));
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

    /// A field the schema says is text is sent as text, whatever it was read as; any other field as it was.
    #[test]
    fn arguments_go_as_the_schema_has_them() {
        let schema = json!({"function": {"name": "mcp__s__t", "parameters": {"type": "object", "properties": {
            "path": {"type": "string"}, "flag": {"type": ["string", "null"]}, "n": {"type": "integer"}, "any": {}, "s": {"type": "string"}
        }}}});
        let read = json!({"path": 2024, "flag": true, "n": 5, "any": 7, "s": "x", "other": 1.5});
        assert_eq!(as_the_schema_has_it(&read, Some(&schema)), json!({"path": "2024", "flag": "true", "n": 5, "any": 7, "s": "x", "other": 1.5}));
        assert_eq!(as_the_schema_has_it(&read, None), read);
        assert_eq!(as_the_schema_has_it(&json!("not an object"), Some(&schema)), json!("not an object"));
    }

    /// A lone half of a surrogate pair, as Python writes a byte it could not decode, is read as U+FFFD; a pair, and an escaped backslash before a `u`,
    /// as they are.
    #[test]
    fn lone_halves_of_surrogate_pairs_are_read_as_replacement_characters() {
        let line = r#"{"id":3,"result":{"text":"a\udc94b \ud83d\ude00 \\udc94 \ud800"}}"#;
        assert!(serde_json::from_str::<Value>(line).is_err());
        assert_eq!(message_of(line).unwrap()["result"]["text"], "a\u{fffd}b \u{1f600} \\udc94 \u{fffd}");
        assert_eq!(lone_halves_mended(r#"{"a":"\ud83d\ude00 \n"}"#), None, "nothing to mend");
        assert_eq!((answer_id_in(r#"{"jsonrpc":"2.0","id": 42,"result":"#), answer_id_in(r#"{"id":7,"method":"x"#)), (Some(42), None));
    }

    /// An answer over pipes that cannot be read at all ends the call that waits for it at once, with why; one with a lone half is read.
    #[test]
    fn an_answer_that_cannot_be_read_ends_its_call_at_once() {
        let lines = format!("{}\n{}\n", r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"x\udc94"}]}}"#, r#"{"jsonrpc":"2.0","id":2,"result":{]}"#);
        let mut rpc = Rpc::over(Box::new(std::io::sink()), Cursor::new(lines.into_bytes()));
        assert_eq!(result_text(&rpc.call("tools/call", json!({}), Duration::from_secs(30), None).unwrap()), "x\u{fffd}");
        let started = Instant::now();
        let said = rpc.call("tools/call", json!({}), Duration::from_secs(30), None).unwrap_err();
        assert!(said.starts_with("the server's answer could not be read") && started.elapsed() < Duration::from_secs(5), "{said}");
    }

    /// A tool's pictures are taken where they are asked for, and said where they were; data that is not base64 is said so.
    #[test]
    fn a_results_pictures_are_taken_and_said_where_they_were() {
        let result = json!({"content": [{"type": "text", "text": "look"}, {"type": "image", "mimeType": "image/png", "data": "iVBO\nRw0K"}, {"type": "image", "data": "*"}]});
        let (text, pictures) = result_parts(&result, true);
        assert_eq!(text, "look\n[picture 1 (image/png): shown with this result]\n[image content is not shown: its data is not base64]");
        assert_eq!(pictures.iter().map(|picture| picture.bytes.clone()).collect::<Vec<_>>(), [vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a]]);
        let (plain, none) = result_parts(&result, false);
        assert!(none.is_empty() && plain == result_text(&result) && plain.contains("[image content is not shown]"), "{plain}");
    }

    #[test]
    fn a_result_is_read_as_words() {
        let result = json!({"content": [{"type": "text", "text": "one"}, {"type": "image", "data": "x"}, {"type": "text", "text": "two"}]});
        assert_eq!(result_text(&result), "one\n[image content is not shown]\ntwo");
        let resources = json!({"content": [
            {"type": "resource", "resource": {"uri": "repo://o/r/contents/a.rs", "mimeType": "text/plain", "text": "fn main() {}"}},
            {"type": "resource", "resource": {"uri": "file:///x.png", "mimeType": "image/png", "blob": "iVBO"}},
            {"type": "resource_link", "uri": "file:///y.txt", "name": "y.txt", "description": "the log"}
        ]});
        assert_eq!(result_text(&resources), "[resource repo://o/r/contents/a.rs]\nfn main() {}\n[resource file:///x.png: image/png bytes, not shown]\n[resource link file:///y.txt (y.txt: the log)]");
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
    fn imported_tool_selection_applies_to_a_refreshed_catalog() {
        let config = servers_of_codex("[mcp_servers.clock]\ncommand='clock'\nenabled_tools=['time.local','time_local','zone']\ndisabled_tools=['time.local']").remove("clock").unwrap();
        let hub = Hub::scripted("clock", &[tool("zone")], &[], false);
        let running = {
            let mut slots = hub.slots.lock().unwrap();
            slots[0].config = Some(config);
            slots[0].running.clone().unwrap()
        };
        hub.relisted(&running, Ok(vec![tool("date"), tool("time.local"), tool("time_local"), tool("zone")]));
        let schemas = hub.schemas();
        let shown: Vec<_> = schemas.iter().map(|schema| schema["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(shown, ["mcp__clock__time_local_2", "mcp__clock__zone"]);
        let slots = hub.slots.lock().unwrap();
        let names = &slots[0].running.as_ref().unwrap().names;
        assert_eq!(names.len(), shown.len());
        assert_eq!(names["mcp__clock__time_local_2"], "time_local");
        assert_eq!(names["mcp__clock__zone"], "zone");
    }

    #[test]
    fn an_empty_allow_list_disables_all_tools_and_an_absent_one_allows_unlisted_tools() {
        for (settings, expected_count) in [("enabled_tools=[]", 0), ("disabled_tools=['date']", 1), ("", 2)] {
            let config = servers_of_codex(&format!("[mcp_servers.clock]\nurl='http://localhost'\n{settings}")).remove("clock").unwrap();
            let (names, schemas) = place_tools("clock", "clock", &[tool("date"), tool("time")], Some(&config.tool_selection));
            assert_eq!(names.len(), expected_count, "{settings}");
            assert_eq!(schemas.len(), expected_count, "{settings}");
            assert_eq!(names.contains_key("mcp__clock__time"), expected_count > 0);
        }
    }

    #[test]
    fn two_tools_never_share_a_name_and_the_order_is_the_tools_own() {
        let long = "t".repeat(70);
        let tools = [tool(&format!("{long}b")), tool(&format!("{long}a")), tool("do.it"), tool("do_it")];
        let (names, schemas) = place_tools("srv", "srv", &tools, None);
        assert_eq!((names.len(), schemas.len()), (4, 4), "{names:?}");
        assert!(names.keys().all(|name| name.len() <= 64), "{names:?}");
        let spelled: Vec<&str> = schemas.iter().map(|schema| schema["function"]["name"].as_str().unwrap()).collect();
        assert!(spelled.iter().all(|name| names.contains_key(*name)));
        let (_, reversed) = place_tools("srv", "srv", &[tool("do_it"), tool("do.it"), tool(&format!("{long}a")), tool(&format!("{long}b"))], None);
        assert_eq!(serde_json::to_string(&schemas).unwrap(), serde_json::to_string(&reversed).unwrap(), "whatever order it lists them in");
        assert_eq!(schemas[0]["function"]["parameters"]["type"], "object");
    }

    fn running(lines: &[Value], names: &[&str]) -> Arc<Running> {
        let script: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let (names, schemas) = place_tools("srv", "srv", &names.iter().map(|name| tool(name)).collect::<Vec<_>>(), None);
        Arc::new(Running { names, schemas, ..Running::new(Rpc::over(Box::new(std::io::sink()), Cursor::new(script.into_bytes())), Vec::new(), None) })
    }

    /// A server that says in a call that its tools changed is asked for them as the call ends: the new one is offered, and one it no longer lists is said to be gone.
    #[test]
    fn a_server_that_says_its_tools_changed_in_a_call_is_asked_for_them_as_it_ends() {
        let lines = [
            json!({"jsonrpc": "2.0", "method": LIST_CHANGED}),
            json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": "enabled"}]}}),
            json!({"jsonrpc": "2.0", "id": 2, "result": {"tools": [tool("u"), tool("v")]}}),
        ];
        let hub = Hub::default();
        hub.slots.lock().unwrap().push(Slot { name: "srv".into(), part: "srv".into(), running: Some(running(&lines, &["t", "u"])), ..Slot::default() });
        assert_eq!(hub.call("mcp__srv__t", &json!({}), &AtomicBool::new(false)).unwrap(), "enabled");
        let names: Vec<String> = hub.schemas().iter().map(|schema| schema["function"]["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["mcp__srv__u", "mcp__srv__v"]);
        assert_eq!(hub.states(), [("srv".to_string(), State::Up(2))]);
        let said = hub.call("mcp__srv__t", &json!({}), &AtomicBool::new(false)).unwrap_err();
        assert!(said.contains("no longer has this tool"), "{said}");
    }

    /// One that says it between calls is asked as the tools are next given, as a turn begins.
    #[test]
    fn a_server_that_says_its_tools_changed_between_calls_is_asked_as_they_are_next_given() {
        let lines = [json!({"jsonrpc": "2.0", "method": LIST_CHANGED}), json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": [tool("t"), tool("w")]}})];
        let server = running(&lines, &["t"]);
        let hub = Hub::default();
        hub.slots.lock().unwrap().push(Slot { name: "srv".into(), part: "srv".into(), running: Some(server.clone()), ..Slot::default() });
        waited("the server's word", || server.changed.load(Ordering::Relaxed));
        assert_eq!(hub.schemas().len(), 2);
        assert!(!server.changed.load(Ordering::Relaxed), "it was answered");
        assert_eq!(hub.schemas().len(), 2, "and is not asked again");
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
        let dir = crate::in_container::tempdir().unwrap();
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
            let (names, schemas) = place_tools(name, &part, &[tool("t")], None);
            let server = Arc::new(Running { names, schemas, ..Running::new(Rpc::over(Box::new(std::io::sink()), Cursor::new(Vec::new())), Vec::new(), None) });
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

    #[test]
    fn imported_stdio_server_reads_relative_files_in_its_working_directory() {
        let temp = crate::in_container::tempdir().unwrap();
        let cwd = temp.path().join("server files");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(cwd.join("server.py"), ANSWERING).unwrap();
        let python = serde_json::to_string(&fake("", &[]).command).unwrap();
        let folder = serde_json::to_string(&cwd).unwrap();
        let source = format!("[mcp_servers.fixture]\ncommand = {python}\ncwd = {folder}\nargs = ['-I', 'server.py', 'fixture', 'pid']");
        let imported = servers_of_codex(&source).remove("fixture").unwrap();
        assert_eq!(imported.cwd.as_ref(), Some(&cwd));
        let _server = start_server_within(&imported, Duration::from_secs(10), true).unwrap();
        assert!(pid_in(&cwd.join("pid")).is_some());
    }

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
        let dir = crate::in_container::tempdir().unwrap();
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

    /// What a server wrote to its stderr as it ended is said with why it did not start, and with why a call of it failed: a person mends it by that.
    #[test]
    fn what_a_server_said_as_it_ended_is_said_with_why() {
        let config = fake("import sys\nsys.stderr.write('starting\\n')\nsys.stderr.write('ModuleNotFoundError: No module named mcp\\n')\nsys.exit(1)\n", &[]);
        let Err(error) = start_server_within(&config, Duration::from_secs(20), false) else { panic!("a server that ends was started") };
        assert!(error.ends_with("It said: starting | ModuleNotFoundError: No module named mcp"), "{error}");
        let crashing = "import json, sys\nfor line in sys.stdin:\n    m = json.loads(line)\n    if 'id' not in m:\n        continue\n    if m['method'] == 'tools/call':\n        sys.stderr.write('KeyError: token\\n')\n        sys.exit(1)\n    r = {'tools': [{'name': 't', 'inputSchema': {'type': 'object'}}]} if m['method'] == 'tools/list' else {'protocolVersion': '2025-06-18', 'capabilities': {}}\n    print(json.dumps({'jsonrpc': '2.0', 'id': m['id'], 'result': r}), flush=True)\n";
        let dir = crate::in_container::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        save_server(&file, None, "srv", fake(crashing, &[])).unwrap();
        let hub = Hub::start(&file);
        waited("the server to be up", || !hub.schemas().is_empty());
        let said = hub.call("mcp__srv__t", &json!({}), &AtomicBool::new(false)).unwrap_err();
        assert!(said.contains("It said: KeyError: token"), "{said}");
    }

    #[cfg(windows)]
    #[test]
    fn a_server_that_fails_its_handshake_in_a_job_ends_with_the_job() {
        start_a_mute_one(true);
    }

    #[test]
    fn two_servers_whose_names_clean_to_one_keep_their_own_tools() {
        let dir = crate::in_container::tempdir().unwrap();
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
