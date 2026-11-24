//! The tools, each behaving as its Dogmind namesake says it does in the schema the model reads.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(not(windows))]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regex::RegexBuilder;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::safety::resolve;

const MAX_OUTPUT: usize = 12 * 1024;
/// A command's report keeps as much of its end as of its start: that is where a build or a test run says how it went.
const MAX_COMMAND_OUT: usize = 12 * 1024;
const MAX_COMMAND_ERR: usize = 6 * 1024;
/// What one `read_file` brings back, whatever its `limit`: a local model pays for each token of it as it goes in, and it stays in the context. A page of a
/// thousand lines (47.8 KB, 42.6 KB in the owner's conversations) is read in one, not in two.
const MAX_READ: usize = 64 * 1024;
/// Lines one `read_file` brings back when no `limit` is given, and at most.
const READ_LINES: usize = 2000;
/// A line of a minified file, as read: 65 KB of one once came back from a request for two lines.
const MAX_LINE: usize = 2000;
/// What a turn reads of files before it is told how much that is, and of explore: 96 KB, some 30,000 tokens.
const READ_TOLD_PAST: usize = 96 * 1024;
/// Lines of a file's head shown with its outline: what it says it is, and what it brings in.
const OUTLINE_HEAD: usize = 30;
/// Lines of an outline: past this, only the ones at the left edge.
const OUTLINE_MOST: usize = 200;
const LIST_LIMIT: usize = 300;
const UNCHANGED_READ: &str = "--- the file has not changed since you read these same lines: they are above, and not repeated here ---";
const MATCH_LIMIT: usize = 1000;
const SEARCH_FILE_LIMIT: u64 = 16 << 20;
const MOST_READ_WHOLE: u64 = 256 << 20;

/// What an `explore` helper may call: it looks, and changes nothing.
pub const HELPER_TOOLS: [&str; 4] = ["read_file", "search_files", "find_files", "list_files"];

/// The helper's tools, as the session's are written.
pub fn helper_schemas() -> Value {
    Value::Array(schemas().as_array().into_iter().flatten().filter(|tool| tool["function"]["name"].as_str().is_some_and(|name| HELPER_TOOLS.contains(&name))).cloned().collect())
}

/// The tools every session has. Dogmind's names and meanings, in as few words as still say them: a local model reads all of this before a new session's first word, some 200 tokens a second, and it has no other use for them than knowing what to call.
/// `assets/dogmind/tools.json` is what Dogmind itself says, to read when it changes.
/// What create_file asks of a model whose provider gives a call only once it is whole: a long call shows nothing for minutes, and a
/// stop loses what it was writing.
const CREATE_FILE_IN_PARTS: &str = "Write a file, new or one you have read. Over about 300 lines, write the first part, then add each further part with append=true: what is on disk survives a stop, and a long call is minutes with nothing to show.";
/// What it asks where the call shows as it is written ([`crate::llm::Provider::shows_calls_as_written`]): there what a stop or a broken
/// connection cut off of a file is kept on disk to go on from, and each part a model wrote by its own choice cost a request and its thinking
/// again (on an RTX 5070 Ti with Qwen3.8 IQ2_XS, a file of some 400 lines took 174 s in 2 to 4 writes, and 143 s in one).
const CREATE_FILE_WHOLE: &str = "Write a file, new or one you have read, whole in one call: what you write shows as it comes. append=true adds to the end of the file.";

/// The paths an apply_patch call names, its edits' (given as a list or as a string of JSON).
fn patched_paths(arguments: &Value) -> Vec<String> {
    let edits = match arguments.get("edits") {
        Some(Value::String(text)) => serde_json::from_str::<Value>(text).ok(),
        other => other.cloned(),
    };
    let mut paths: Vec<String> = edits.as_ref().and_then(Value::as_array).into_iter().flatten().filter_map(|edit| edit.get("path").and_then(Value::as_str)).map(str::to_string).collect();
    paths.extend(arguments.get("path").and_then(Value::as_str).map(str::to_string));
    paths.sort();
    paths.dedup();
    paths
}

/// A write's result, with what a browser would refuse in the scripts of the pages and script files it wrote: their syntax errors, each at its line. A page written
/// whole is looked at once it is all there (its `</html>`), as one written in parts is not, part by part.
fn with_script_errors(mut said: String, paths: &[String], workspace: &Path, whole_write: bool) -> String {
    for path in paths.iter().filter(|path| !path.is_empty()) {
        let full = resolve(workspace, path);
        if whole_write && full.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm"))
            && !std::fs::read_to_string(&full).is_ok_and(|text| text.to_ascii_lowercase().contains("</html>")) {
            continue;
        }
        let errors = crate::script_syntax::errors_in(&full);
        if errors.is_empty() {
            continue;
        }
        let parts = if whole_write { " (if more of it is still to be appended, finish that first)" } else { "" };
        said.push_str(&format!("
note: as a browser reads it, {path} has a syntax error, and the script it is in does not run at all: {}. Fix it before you check the page{parts}.", errors.join("; ")));
    }
    said
}

/// The tools' schemas for a provider whose calls show as they are written: create_file asks for a file whole.
pub fn for_calls_shown_as_written(schemas: &mut Value) {
    for tool in schemas.as_array_mut().into_iter().flatten() {
        if tool.pointer("/function/name").and_then(Value::as_str) == Some("create_file") {
            tool["function"]["description"] = Value::from(CREATE_FILE_WHOLE);
        }
    }
    // (a program's declarations say what the tools are as they are offered: made again from them)
    redeclare_run_code(schemas);
}

/// `run_code`, where it is offered, made again from the tools offered beside it as they are now.
pub(crate) fn redeclare_run_code(schemas: &mut Value) {
    let Some(list) = schemas.as_array_mut() else { return };
    let Some(at) = list.iter().position(|tool| tool.pointer("/function/name").and_then(Value::as_str) == Some("run_code")) else { return };
    let others = Value::Array(list.iter().enumerate().filter(|(index, _)| *index != at).map(|(_, tool)| tool.clone()).collect());
    list[at] = crate::code_mode::schema(&others);
}

pub fn schemas() -> Value {
    let function = |name: &str, description: &str, properties: Value, required: &[&str]| json!({"type": "function", "function": {"name": name, "description": description, "parameters": {"type": "object", "properties": properties, "required": required}}});
    json!([
        function("apply_patch", "Replace exact text in existing files. Set path inside each edit to the file containing its old_string. All edits land together or none do. Read the target if you need its exact text. old_string must match once (or `count` times), without line numbers.", json!({
            "edits": {"type": "array", "items": {"type": "object", "properties": {"path": {"type": "string"}, "old_string": {"type": "string"}, "new_string": {"type": "string"}, "count": {"type": "integer", "minimum": 0}}, "required": ["path", "old_string", "new_string"]}}
        }), &["edits"]),
        function("create_file", CREATE_FILE_IN_PARTS, json!({
            "path": {"type": "string"},
            "content": {"type": "string"},
            "append": {"type": "boolean", "description": "Add to the end of the file."}
        }), &["path", "content"]),
        function("execute_command", "Run a program, script, test, build, package manager or git, through cmd.exe in the sandbox the Environment section describes. Output is captured whole, so no redirect to a log and no `| head`. Not for reading files or listing folders (read_file, search_files, find_files, list_files), nor for writing them (create_file, apply_patch). A one-off calculation: `python -c` or `node -e`, and no script left behind. Fetch files with plain `curl -sL -o FILE URL`, each URL written out (no variable): the user can then allow that site once.", json!({
            "command": {"type": "string"},
            "cwd": {"type": "string", "description": "Where to run, instead of `cd ... &&`. Default: the workspace."},
            "timeout_seconds": {"type": "integer", "description": "How long to wait for it, default 180. Past that it goes on in the background."},
            "purpose": {"type": "string", "description": "A few words on why, for the user when they are asked."},
            "network": {"type": "boolean", "description": "Needs the network for what the Environment section does not cover. Asks the user."},
            "outside_sandbox": {"type": "boolean", "description": "Run as the user: only for what the user asked for outside the workspace, never to check your work. Asks the user each time."},
            "background": {"type": "boolean", "description": "Start it and go on, for what takes minutes. Gives an id for command_status; its end wakes you."},
            "wake_on": {"type": "string", "description": "With background: a regex. The first line it prints that matches wakes you, once. A server: \"Listening on\"."}
        }), &["command"]),
        function("list_files", "List a folder. {} is the workspace.", json!({
            "path": {"type": "string"},
            "recursive": {"type": "boolean", "description": "Skips node_modules, .venv and the like."}
        }), &[]),
        function("read_file", "Read a text file as numbered lines, or see a picture where this model can be shown one.", json!({
            "path": {"type": "string"},
            "start_line": {"type": "integer", "description": "1-based. -N starts N lines from the end."},
            "limit": {"type": "integer", "description": "Lines. Default and max 2000."}
        }), &["path"]),
        function("search_files", "Search file contents by regex, ignoring case unless the pattern has capitals. Respects .gitignore. Search first, then read the lines you need.", json!({
            "pattern": {"type": "string"},
            "literal": {"type": "boolean", "description": "The pattern is plain text."},
            "path": {"type": "string"},
            "file_glob": {"type": "string"},
            "timeout_seconds": {"type": "integer", "description": "1-300 seconds, default 30. Returns partial matches on timeout."}
        }), &["pattern"]),
        function("show_diff", "Show file changes.", json!({
            "scope": {"type": "string", "enum": ["session", "workspace", "staged", "unstaged"], "description": "session: since this session began. workspace: against HEAD, or outside git as session. staged, unstaged: git's index."},
            "paths": {"type": "array", "items": {"type": "string"}}
        }), &["scope"]),
        function("ask_user", "Ask the person one question with 2-4 choices, the first shown as your pick. They may answer in their own words. Waits for the answer.", json!({
            "question": {"type": "string"},
            "options": {"type": "array", "items": {"type": "object", "properties": {"label": {"type": "string"}, "description": {"type": "string"}}, "required": ["label"]}}
        }), &["question", "options"]),
        function("submit_plan", "Plan mode: put the finished plan (Markdown) to the person. Approved, plan mode ends and you carry it out now; if not, you get what to change.", json!({
            "plan": {"type": "string"}
        }), &["plan"]),
        function("set_goal", "For a request of many steps (build, fix, investigate, change several files): what this turn works toward. The app keeps the turn going, round after round, until finish_goal. Not for a question answered at once.", json!({
            "objective": {"type": "string", "description": "The user's request in one sentence, in their language."},
            "rounds": {"type": "integer", "description": "At most, default 12."}
        }), &["objective"]),
        function("goal_status", "The turn's goal, its round, and how it stands.", json!({}), &[]),
        function("finish_goal", "done: with the tool results that showed all of the goal is met. blocked: only when the same obstacle that only the user can remove (a password, a decision, a device) stopped the last three rounds; failing tests, an error or work left are not.", json!({
            "result": {"type": "string", "enum": ["done", "blocked"]},
            "evidence": {"type": "string"},
            "blocker": {"type": "string"}
        }), &["result"]),
        function("todo", "A checklist the person sees beside the conversation, each item pending, doing, done or blocked. Items sent are added or changed by their id.", json!({
            "todos": {"type": "array", "items": {"type": "object", "properties": {"id": {"type": "string"}, "text": {"type": "string"}, "status": {"type": "string", "enum": ["pending", "doing", "done", "blocked"]}}, "required": ["id"]}},
            "merge": {"type": "boolean", "description": "false: replace the whole list."}
        }), &["todos"]),
        function("present", "Put files of the workspace before the person: a card in the conversation lists them, and each opens in its own app or is shown in Explorer.", json!({
            "paths": {"type": "array", "items": {"type": "string"}, "description": "From the workspace's root."}
        }), &["paths"]),
        function("find_files", "Find files by name with a glob such as `**/*.rs`. Skips what .gitignore skips.", json!({
            "pattern": {"type": "string"},
            "path": {"type": "string"}
        }), &["pattern"]),
        function("check_page", "Check a workspace page by path (no server needed). Reports console output, errors, visible content and pictures. Test the original page, clicking and typing as a user would. Only scripts, styles and fonts from well-known CDNs load from the network. Never download, vendor or build a library to check a page.", json!({
            "path": {"type": "string"},
            "url": {"type": "string", "description": "Instead of path: http://localhost:PORT/ on a running server, without steps."},
            "views": {"type": "array", "items": {"type": "string"}, "description": "desktop (default), mobile, or WxH"},
            "wait_ms": {"type": "integer", "description": "Virtual ms incl. waits. Default 1500 (steps 3000). Max 10000."},
            "steps": {"type": "string", "description": "With path only. JS after load: await click/fill/press/check/select(sel, ...), wait(ms); text/count/visible(sel); console.log results. A reload (await reload()) starts the next part in the new load; variables do not carry over."}
        }), &[]),
        function("command_status", "A background command's state, and what it printed since the last look. Also your waits.", json!({"id": {"type": "string"}}), &[]),
        function("command_stop", "End a background command, or cancel a wait, by its id.", json!({"id": {"type": "string"}}), &["id"]),
        function("wake_at", "Wake yourself later: after delay_seconds you get a turn with your note. At most 5 waits; they end with the app.", json!({
            "delay_seconds": {"type": "integer", "description": "60 to 86400."},
            "note": {"type": "string"}
        }), &["delay_seconds", "note"]),
        function("explore", "A look across many files by a helper with its own context: only its answer comes back. It only reads, and takes minutes while you wait: for a question across more files than you could read yourself (a review, an audit, what uses what); not for one file or a few, which you read yourself. One a turn: put all it is to look into in its task.", json!({"task": {"type": "string", "description": "All it needs: it saw none of this."}}), &["task"])
    ])
}

/// `edit`, for a model that was taught to change a file so: one exact piece of text replaced, or every one with `replace_all`. It is apply_patch with one edit
/// ([`edit_as_patch`]), and goes through all that apply_patch does.
pub fn edit_schema() -> Value {
    json!({"type": "function", "function": {"name": "edit", "description": "Replace an exact piece of text in a file: old_string must be there once, or set replace_all.", "parameters": {"type": "object", "properties": {
        "file_path": {"type": "string"},
        "old_string": {"type": "string"},
        "new_string": {"type": "string"},
        "replace_all": {"type": "boolean"}
    }, "required": ["file_path", "old_string", "new_string"]}}})
}

pub(crate) fn edit_path(arguments: &Value) -> Option<&str> {
    arguments.get("file_path").or_else(|| arguments.get("path")).and_then(Value::as_str)
}

/// An `edit` call as the apply_patch call it is: its one edit, as many times as old_string is there when it asks for all of them.
pub fn edit_as_patch(arguments: &Value, workspace: &Path) -> Value {
    let path = edit_path(arguments).unwrap_or("");
    let old = arguments.get("old_string").and_then(Value::as_str).unwrap_or("");
    let new = arguments.get("new_string").cloned().unwrap_or(Value::Null);
    let mut edit = json!({"path": path, "old_string": old, "new_string": new});
    if arguments.get("replace_all").and_then(Value::as_bool) == Some(true) && !old.is_empty() {
        let found = std::fs::read(resolve(workspace, path)).ok().and_then(|bytes| {
            let (text, encoding) = readable_file_text(&bytes)?;
            let body = if encoding == "utf-8" { text.strip_prefix('\u{feff}').unwrap_or(&text) } else { &text };
            let exact = body.matches(old).count();
            // Prefer the literal text, as apply_patch does in a mixed-ending file.
            Some(if exact == 0 && patch_has_crlf_variant(old, body) {
                body.matches(&old.replace('\n', "\r\n")).count()
            } else {
                exact
            })
        }).unwrap_or(0);
        if found > 1 {
            edit["count"] = json!(found);
        }
    }
    json!({"edits": [edit]})
}

/// All the tools of a session: those every session has, and those that depend on what is on.
pub fn schemas_for(web: bool, skills: bool, memory: bool, mcp: Vec<Value>, code_mode: bool) -> Value {
    let mut all = schemas();
    let Some(list) = all.as_array_mut() else { return all };
    let mut add = |more: Value| {
        if let Some(more) = more.as_array() {
            list.extend(more.iter().cloned());
        }
    };
    if web {
        add(crate::web::schemas());
    }
    if skills {
        add(json!([crate::skills::schema()]));
    }
    if memory {
        add(crate::memory::schemas());
    }
    add(Value::Array(mcp));
    // (last, as it tells what the others are)
    if code_mode {
        let run_code = crate::code_mode::schema(&all);
        if let Some(list) = all.as_array_mut() {
            list.push(run_code);
        }
    }
    all
}

/// Independent reads run together; everything else runs alone and in order.
pub fn is_read(tool: &str) -> bool {
    matches!(tool, "read_file" | "list_files" | "search_files" | "show_diff" | "find_files")
}

/// A command started in the background. The workbench holds it for the model's `command_status` and `command_stop`, and the session's [`crate::background::Background`] for what wakes the session and what the window lists.
pub struct Job {
    pub(crate) id: String,
    pub(crate) command: String,
    pub(crate) started: Instant,
    /// When it was started, on this PC's clock, for the person to read.
    pub(crate) started_at: String,
    pub(crate) cancel: Arc<AtomicBool>,
    result: Arc<Mutex<Option<Result<String, String>>>>,
    /// It was started to run in the session's scratch folder, and not in the workspace.
    in_scratch: bool,
    /// Its output has been in a report of all of them: the next such report says so in a line.
    told: AtomicBool,
    /// What it has printed while it runs, and how far a look at it has got.
    pub(crate) live: Arc<Live>,
    looked: AtomicU64,
    /// Whether command_status has told of it while it runs: a look after that with nothing new is a wait.
    looked_at: AtomicBool,
    /// Looks in a row since then with nothing new: after `QUIET_LOOKS_BEFORE_WAITING` of them a look waits for it (`LOOK_WAITS`).
    quiet_looks: AtomicU32,
    /// It was ended on purpose (`command_stop`, the person's Stop or a stop of the list, a session that was left): that wakes nothing.
    stopped: AtomicBool,
    /// Its end has been looked at for a notice, which was made or was not due (the model had read the end already).
    announced: AtomicBool,
    /// The pattern its `wake_on` gave: the first line it prints that matches wakes the session.
    pub(crate) wake_on: Option<String>,
}

impl Job {
    pub(crate) fn new(id: &str, command: &str, started: Instant, cancel: Arc<AtomicBool>, result: CommandResult, live: Arc<Live>, wake_on: Option<String>) -> Job {
        Job { id: id.to_string(), command: command.to_string(), started, started_at: chrono::Local::now().format("%H:%M").to_string(), cancel, result, in_scratch: false, told: AtomicBool::new(false), live, looked: AtomicU64::new(0), looked_at: AtomicBool::new(false), quiet_looks: AtomicU32::new(0), stopped: AtomicBool::new(false), announced: AtomicBool::new(false), wake_on }
    }

    #[cfg(test)]
    pub(crate) fn mark_told(&self) {
        self.told.store(true, Ordering::Relaxed);
    }

    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Ends it, as one who asked for that: its end wakes nothing.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub(crate) fn announced(&self) -> bool {
        self.announced.load(Ordering::Relaxed)
    }

    pub(crate) fn mark_announced(&self) {
        self.announced.store(true, Ordering::Relaxed);
    }

    /// The model has read how it ended (`command_status`).
    pub(crate) fn told(&self) -> bool {
        self.told.load(Ordering::Relaxed)
    }

    /// What it came to, once it has ended: the report, or why it did not run.
    pub(crate) fn outcome(&self) -> Option<Result<String, String>> {
        self.result.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    pub fn has_ended(&self) -> bool {
        self.result.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).is_some()
    }
}

/// What a command has printed so far, its two streams as they came, for a look while it runs. A dev server never ends, and what it said
/// (where it listens, the error it stopped on) was out of reach until it did. Only the end is kept.
#[derive(Default)]
pub struct Live {
    kept: Mutex<(std::collections::VecDeque<u8>, u64)>,
    /// What looks for the line a `wake_on` waits for.
    watch: Mutex<Option<Watch>>,
    /// When the command's thread had its result.
    ended: Mutex<Option<Instant>>,
}

/// The first printed line that matches a pattern, found as the bytes come.
struct Watch {
    pattern: regex::Regex,
    /// The line that is being printed, in the bytes it came in.
    line: Vec<u8>,
    found: Option<(String, Instant)>,
    /// The line has been given to the one who asked for it.
    taken: bool,
}

impl Watch {
    /// Whether this line is the one: it is kept, with when it came.
    fn sees(&mut self, line: &[u8]) -> bool {
        let text = decode_text(line);
        let text = text.trim_end_matches('\r');
        if !self.pattern.is_match(text) {
            return false;
        }
        self.found = Some((text.to_string(), Instant::now()));
        true
    }
}

/// A line that is longer is looked at as far as this, which no program's one line of news needs to pass.
const WATCHED_LINE: usize = 4096;

/// What is kept of a running command's output for a look, and what one look shows of what is new.
const LIVE_KEPT: usize = 64 * 1024;
const LIVE_SHOWN: usize = 4 * 1024;

impl Live {
    pub fn push(&self, bytes: &[u8]) {
        {
            let mut kept = self.kept.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            kept.0.extend(bytes);
            kept.1 += bytes.len() as u64;
            let over = kept.0.len().saturating_sub(LIVE_KEPT);
            kept.0.drain(..over);
        }
        let mut watch = self.watch.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(watch) = watch.as_mut().filter(|watch| watch.found.is_none()) else { return };
        for byte in bytes {
            if *byte == b'\n' {
                let line = std::mem::take(&mut watch.line);
                if watch.sees(&line) {
                    return;
                }
            } else if watch.line.len() < WATCHED_LINE {
                watch.line.push(*byte);
            }
        }
        // (a line that is not ended yet, as a prompt is, is looked at too: it may be all the line there will be)
        let line = watch.line.clone();
        watch.sees(&line);
    }

    /// Looks for the first line that matches `pattern`, from now on.
    pub fn watch_for(&self, pattern: regex::Regex) {
        *self.watch.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Watch { pattern, line: Vec::new(), found: None, taken: false });
    }

    /// The line that matched, and when, the one time it is asked for.
    pub fn take_line(&self) -> Option<(String, Instant)> {
        let mut watch = self.watch.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let watch = watch.as_mut().filter(|watch| !watch.taken)?;
        let found = watch.found.clone()?;
        watch.taken = true;
        Some(found)
    }

    /// The command's thread has what it came to.
    pub(crate) fn mark_ended(&self) {
        *self.ended.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
    }

    pub(crate) fn ended_at(&self) -> Option<Instant> {
        *self.ended.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// What came after its first `from` bytes, at most the last `most` of it, and where the next look starts: not past a character that has come only in part, which is kept for the next look
    /// and shown whole then.
    /// How many bytes it has printed in all.
    pub(crate) fn total(&self) -> u64 {
        self.kept.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).1
    }

    pub(crate) fn since(&self, from: u64, most: usize) -> (String, u64) {
        let kept = self.kept.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (buffer, total) = (&kept.0, kept.1);
        let new = total.saturating_sub(from);
        let shown = (new.min(buffer.len() as u64) as usize).min(most);
        let mut bytes: Vec<u8> = buffer.iter().skip(buffer.len() - shown).copied().collect();
        // (a look may start in the middle of a character: of UTF-8 it is dropped. Of the console's code page it is not told from a character of one byte, and what is there is read as it is)
        let begins_inside = new > shown as u64;
        let cut = bytes.iter().take_while(|byte| (0x80..0xC0).contains(*byte)).count();
        if begins_inside && cut > 0 && utf8_so_far(&bytes[cut..]) {
            bytes.drain(..cut);
        }
        let (decoded, used) = decode_so_far(&bytes);
        let mut text = String::new();
        if begins_inside {
            text.push_str(&format!("({} bytes before this are not shown)\n", new - shown as u64));
        }
        text.push_str(&decoded);
        (text, total - (bytes.len() - used) as u64)
    }
}

/// Whether these bytes are UTF-8, or UTF-8 that ends in the middle of a character.
fn utf8_so_far(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none(),
    }
}

/// A command's output as far as it has come, as the final output is read (`sandbox::decode`: UTF-8, as asked for, or for what ignored that the console's code page), and how many of its bytes
/// that is. A character that has come in part, at the end, is left out and left for the next time: UTF-8 that ends in one, or a double-byte character of the code page whose second byte has
/// not come.
fn decode_so_far(bytes: &[u8]) -> (String, usize) {
    match std::str::from_utf8(bytes) {
        Ok(text) => return (text.to_string(), bytes.len()),
        Err(error) if error.error_len().is_none() => {
            let upto = error.valid_up_to();
            return (String::from_utf8_lossy(&bytes[..upto]).into_owned(), upto);
        }
        Err(_) => {}
    }
    let whole = whole_characters(bytes);
    (decode_text(&bytes[..whole]), whole)
}

/// The length of these bytes without a lead byte of the console's code page at its end, which a second byte has still to follow.
#[cfg(windows)]
fn whole_characters(bytes: &[u8]) -> usize {
    use windows_sys::Win32::Globalization::{GetOEMCP, IsDBCSLeadByteEx};
    // SAFETY: both take numbers.
    let code_page = unsafe { GetOEMCP() };
    let mut at = 0;
    while at < bytes.len() {
        // SAFETY: as above.
        let lead = unsafe { IsDBCSLeadByteEx(code_page, bytes[at]) } != 0;
        if lead && at + 1 >= bytes.len() {
            return at;
        }
        at += if lead { 2 } else { 1 };
    }
    bytes.len()
}

#[cfg(not(windows))]
fn whole_characters(bytes: &[u8]) -> usize {
    bytes.len()
}

#[cfg(windows)]
fn decode_text(bytes: &[u8]) -> String {
    crate::sandbox::decode(bytes)
}

#[cfg(not(windows))]
fn decode_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Commands going on in the background. They do not outlive the session that started them.
#[derive(Default)]
struct Jobs(Vec<Arc<Job>>);

impl Drop for Jobs {
    fn drop(&mut self) {
        for job in &self.0 {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }
}

pub(crate) type CommandResult = Arc<Mutex<Option<Result<String, String>>>>;

/// What a command started in the background runs with: not its timeout, which is how long a call waits for it, and a background one
/// does not. As the model gave it, it was how long the job had before it was ended, and a long build was stopped in the middle without a word.
fn background_arguments(arguments: &Value) -> Value {
    let mut given = arguments.clone();
    if let Some(fields) = given.as_object_mut() {
        fields.remove("timeout_seconds");
    }
    given
}

/// The pattern a command's `wake_on` gives, as a regular expression. None when it gives none.
fn wake_pattern(arguments: &Value) -> Result<Option<regex::Regex>, String> {
    let Some(pattern) = arguments.get("wake_on").and_then(Value::as_str).map(str::trim).filter(|pattern| !pattern.is_empty()) else { return Ok(None) };
    RegexBuilder::new(pattern).size_limit(1 << 20).build().map(Some).map_err(|error| format!("wake_on is not a regular expression that can be used ({}): the command was not started", error.to_string().lines().last().unwrap_or("").trim()))
}

/// A command on a thread of its own, with what ends it and where its result will be. It has minutes to itself unless the call says otherwise.
fn spawn_command(arguments: &Value, workspace: &Path) -> (Arc<AtomicBool>, CommandResult, Arc<Live>) {
    let mut given = arguments.clone();
    if given.get("timeout_seconds").is_none() {
        given["timeout_seconds"] = json!(3600);
    }
    let (cancel, result, live): (_, CommandResult, _) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(None)), Arc::new(Live::default()));
    // (the line it is to wake the session for: a pattern that does not compile was refused before the command was started)
    if let Ok(Some(pattern)) = wake_pattern(&given) {
        live.watch_for(pattern);
    }
    let (stop, done, place, printed) = (cancel.clone(), result.clone(), workspace.to_path_buf(), live.clone());
    std::thread::spawn(move || {
        let outcome = execute_command(&given, &place, &stop, Some(printed.clone()));
        // (the time it ended is set before what it came to, which is what is looked for: the one is never there without the other)
        printed.mark_ended();
        *done.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(outcome);
    });
    (cancel, result, live)
}

/// Its result, once it has one. None when `wait` runs out first. A stop from the person ends it, and that is waited for too: what it printed is part of the answer.
fn wait_for_command(result: &CommandResult, stop: &AtomicBool, cancel: &AtomicBool, wait: Duration) -> Option<Result<String, String>> {
    let started = Instant::now();
    loop {
        if let Some(outcome) = result.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take() {
            return Some(outcome);
        }
        let stopping = cancel.load(Ordering::Relaxed);
        if stopping {
            stop.store(true, Ordering::Relaxed);
        } else if started.elapsed() >= wait {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[derive(Default)]
pub struct Workbench {
    /// The conversation is in Japanese, so what it writes is looked at for what is not.
    pub in_japanese: bool,
    /// The next MCP call is to take the pictures its result holds (where the model reads pictures in a tool's result), into `mcp_pictures`.
    pub mcp_pictures_wanted: bool,
    pub mcp_pictures: Vec<crate::mcp::Picture>,
    /// The hash of each file as this session last saw it: an edit needs a read of what is there now.
    seen: HashMap<PathBuf, String>,
    /// What `seen` was given since the session was last told, for its file: a conversation reopened has read what it read.
    seen_untold: Vec<(PathBuf, String)>,
    pub todos: Vec<Value>,
    /// Where the session began, for `show_diff`. None until it has a file to keep that beside.
    pub baseline: Option<Arc<crate::baseline::Baseline>>,
    pub web: crate::web::Web,
    pub skills: Vec<crate::skills::Skill>,
    /// The folder of the notes kept between sessions.
    pub memory: Option<PathBuf>,
    /// Whether the notes are kept sealed, and while the key is not unlocked, that nothing is written.
    pub policy: Arc<crate::conversation_key::Policy>,
    pub mcp: Option<Arc<crate::mcp::Hub>>,
    jobs: Jobs,
    /// What this session has going on or waiting while no turn runs: the commands of `jobs`, and the waits.
    pub background: Arc<crate::background::Background>,
    /// A file whose `create_file` the app cut off to get it on disk: the next write to it goes on from there, whether or not the model says append.
    cut_file: Option<PathBuf>,
    /// What has been read of each file and what it was then, while that is still in the conversation.
    reads: HashMap<PathBuf, (String, HashSet<[u8; 32]>)>,
    /// How many of each file's lines, from its first, the conversation has been shown as it is now: a file written whole over one
    /// seen only in part loses the rest without a word.
    covered: HashMap<PathBuf, usize>,
    /// What this turn has read of files into the conversation, in bytes, and how many times it has been told how much.
    pub read_this_turn: usize,
    pub read_told: usize,
    /// What holds keys besides what reads as such: the app's own folder, and the files its settings name.
    pub secrets: Vec<PathBuf>,
    /// While the person has asked for no changes to the files: the folder outside the workspace where a command may run, and where the write tools may write.
    pub scratch: Option<PathBuf>,
    /// A `run_code` program is making the calls: what they read goes to the program, not into the conversation.
    pub in_program: bool,
}

fn sha16(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

/// Where a file's new text is written whole before it takes the file's place: beside it, so that taking the place is a rename on one drive.
pub(crate) fn written_beside(path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(".dogmind-edit-{}", std::process::id()));
    path.with_file_name(name)
}

/// One file's new text in place of the old, from where it was written beside it: Windows's ReplaceFile, which leaves the file what it was
/// (its permissions, its attributes, when it was made) but its words.
pub(crate) fn take_the_place(new: &Path, path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{ReplaceFileW, REPLACEFILE_IGNORE_ACL_ERRORS, REPLACEFILE_IGNORE_MERGE_ERRORS};
        let wide = |path: &Path| -> Vec<u16> { std::os::windows::ffi::OsStrExt::encode_wide(path.as_os_str()).chain(std::iter::once(0)).collect() };
        let (replaced, replacement) = (wide(path), wide(new));
        // SAFETY: both names end with a zero, and live through the call.
        let done = unsafe { ReplaceFileW(replaced.as_ptr(), replacement.as_ptr(), std::ptr::null(), REPLACEFILE_IGNORE_MERGE_ERRORS | REPLACEFILE_IGNORE_ACL_ERRORS, std::ptr::null(), std::ptr::null()) };
        if done == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(new, path)
    }
}

/// A file's whole text, written beside it and then in its place: a write that fails on the way (a full disk, the app ended) leaves the file
/// as it was, where writing over it left it empty or cut.
fn written_whole(path: &Path, bytes: &[u8], exists: bool) -> std::io::Result<()> {
    let new = written_beside(path);
    let done = std::fs::File::create(&new)
        .and_then(|mut out| {
            std::io::Write::write_all(&mut out, bytes)?;
            out.sync_all()
        })
        .and_then(|()| if exists { take_the_place(&new, path) } else { std::fs::rename(&new, path) });
    if done.is_err() {
        let _ = std::fs::remove_file(&new);
    }
    done
}

/// Every file's new text, or none: each is written whole beside its file first, and only when all of them are on the disk do they take
/// the files' places, one by one. A failure on the way puts back what had changed. No file is ever left part written, and what is said
/// of a failure is what the files are.
fn write_all_or_none(files: &[(&Path, &[u8], Vec<u8>)]) -> Result<(), String> {
    let beside: Vec<PathBuf> = files.iter().map(|(path, _, _)| written_beside(path)).collect();
    let forget = |from: usize| {
        for new in &beside[from..] {
            let _ = std::fs::remove_file(new);
        }
    };
    for (at, (path, _, after)) in files.iter().enumerate() {
        let written = std::fs::File::create(&beside[at]).and_then(|mut out| {
            std::io::Write::write_all(&mut out, after)?;
            out.sync_all()
        });
        if let Err(error) = written {
            forget(0);
            return Err(format!("{}: {error}: nothing was changed", path.display()));
        }
    }
    for (at, (path, _, _)) in files.iter().enumerate() {
        if let Err(error) = take_the_place(&beside[at], path) {
            forget(at);
            // (the old text goes back as the new went in: whole, beside, then in place)
            let not_back: Vec<String> = files[..at]
                .iter()
                .filter(|(done, before, _)| {
                    let back = written_beside(done);
                    let put = std::fs::write(&back, before).and_then(|()| take_the_place(&back, done));
                    if put.is_err() {
                        let _ = std::fs::remove_file(&back);
                    }
                    put.is_err()
                })
                .map(|(done, _, _)| done.display().to_string())
                .collect();
            return Err(if not_back.is_empty() {
                format!("{}: {error}: the files are as they were", path.display())
            } else {
                format!("{}: {error}: these have the new text, and could not be put back: {}; the rest are as they were", path.display(), not_back.join(", "))
            });
        }
    }
    Ok(())
}

struct PatchFile {
    path: PathBuf,
    original: Vec<u8>,
    form: Form,
    before: String,
    after: String,
}

/// An edit replaces a directory entry. On Windows, different casing can name that same entry.
fn same_patch_path(one: &Path, other: &Path) -> bool {
    if one == other {
        return true;
    }
    if !cfg!(windows) {
        return false;
    }
    let (a, b) = (one.as_os_str(), other.as_os_str());
    if a.is_ascii() && b.is_ascii() && !a.eq_ignore_ascii_case(b) {
        return false;
    }
    // Ask the filesystem about Unicode casing and case-sensitive directories.
    // A symlink is itself replaced by a patch, so its target cannot identify the entry.
    !one.is_symlink() && !other.is_symlink()
        && matches!((one.canonicalize(), other.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

/// Head and tail, with what is left out said between them.
const MAX_APPLIED_DIFF: usize = 3000;

pub fn cap(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let boundary = |mut at: usize| {
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    let head = boundary(limit * 3 / 4);
    let tail = boundary(text.len() - limit / 4);
    format!("{}\n--- truncated: {} bytes left out ---\n{}", &text[..head], tail - head, &text[tail..])
}

/// As much of the end as of the start.
/// What a command printed, whole, when its report shows only part of it: in a file to read the rest of with read_file or search_files,
/// as Deepseek Harness keeps a long output, and not by running the command again (a build or a test run, minutes of it). Kept a week.
fn kept_whole(out: &str, err: &str) -> Option<PathBuf> {
    keep_whole_as("command", &format!("stdout:\n{out}\nstderr:\n{err}\n"))
}

/// A long result, whole, in a file of its own kind (`command`, `search`, `trimmed`) in the temporary folder.
pub(crate) fn keep_whole_as(kind: &str, whole: &str) -> Option<PathBuf> {
    const KEPT: Duration = Duration::from_secs(7 * 24 * 3600);
    let folder = std::env::temp_dir().join("dogmind-output");
    std::fs::create_dir_all(&folder).ok()?;
    for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
        if entry.metadata().ok().and_then(|meta| meta.modified().ok()).and_then(|at| at.elapsed().ok()).is_some_and(|age| age > KEPT) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    keep_whole_in(&folder, kind, &chrono::Local::now().format("%Y%m%d-%H%M%S%3f").to_string(), whole)
}

/// The file for one output, named by the time to the millisecond and made so that it cannot replace another: commands end on threads of their own, and two that end in the same
/// millisecond had one file between them, and both reports pointed at the one that was written last. On a clash the name has a number added.
fn keep_whole_in(folder: &Path, kind: &str, stamp: &str, whole: &str) -> Option<PathBuf> {
    for again in 0..1000 {
        let name = if again == 0 { format!("{kind}-{stamp}.txt") } else { format!("{kind}-{stamp}-{again}.txt") };
        let file = folder.join(name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&file) {
            Ok(mut out) => {
                return std::io::Write::write_all(&mut out, whole.as_bytes()).is_ok().then_some(file);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return None,
        }
    }
    None
}

/// A tool's long result as the model is shown it: its head and its tail, and where all of it is kept (a file of `kind` in the temporary folder,
/// [`keep_whole_as`]) to read the rest of with read_file or search_files. A result that was cut with no copy kept could not be had again: the
/// part of a diff, a background command's report, an MCP tool's answer or a helper's that fell between head and tail was lost to the model.
/// Where nothing could be written, the cut as [`cap`] makes it.
pub fn spill(kind: &str, text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    match keep_whole_as(kind, text) {
        Some(file) => cap_evenly(text, limit, Some(&file)),
        None => cap(text, limit),
    }
}

/// The head and the tail of a long output, and where all of it is when it was kept.
fn cap_evenly(text: &str, limit: usize, whole: Option<&Path>) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let (mut head, mut tail) = (limit / 2, text.len() - limit / 2);
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    let rest = match whole {
        Some(file) => format!("all of it is in {}: read the part you need from there with read_file or search_files, rather than run it again", file.display()),
        None => "run it again through findstr to see them".to_string(),
    };
    format!("{}\n--- truncated: {} bytes left out; {rest} ---\n{}", &text[..head], tail - head, &text[tail..])
}

fn text_arg<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    arguments.get(name).and_then(Value::as_str).ok_or_else(|| format!("{name} is required"))
}

impl Workbench {
    pub fn new(_workspace: &Path) -> Workbench {
        Workbench::default()
    }

    /// Runs a read that may share the workbench with others: what it saw comes back to record afterwards.
    pub fn run_read(tool: &str, arguments: &Value, workspace: &Path, baseline: Option<&crate::baseline::Baseline>, secrets: &[PathBuf], cancel: &AtomicBool) -> (Result<String, String>, Option<(PathBuf, String)>) {
        match tool {
            "read_file" => match read_file(arguments, workspace) {
                Ok((text, seen)) => (Ok(text), Some(seen)),
                Err(error) => (Err(error), None),
            },
            "list_files" => (list_files(arguments, workspace, cancel), None),
            "find_files" => (find_files(arguments, workspace, cancel), None),
            "search_files" => (search_files(arguments, workspace, secrets, cancel), None),
            "show_diff" => (show_diff(arguments, workspace, baseline), None),
            other => (Err(format!("{other} is not a read")), None),
        }
    }

    pub fn run(&mut self, tool: &str, arguments: &Value, workspace: &Path, cancel: &AtomicBool) -> Result<String, String> {
        self.run_tool(tool, arguments, workspace, cancel)
    }

    fn run_tool(&mut self, tool: &str, arguments: &Value, workspace: &Path, cancel: &AtomicBool) -> Result<String, String> {
        match tool {
            "apply_patch" => self.apply_patch(arguments, workspace).map(|said| with_script_errors(said, &patched_paths(arguments), workspace, false)),
            "create_file" => {
                let path = arguments.get("path").and_then(Value::as_str).unwrap_or("").to_string();
                self.create_file(arguments, workspace).map(|said| with_script_errors(said, &[path], workspace, true))
            }
            "execute_command" => {
                // A command whose folder is the scratch folder runs there as it would in a workspace: that folder is the only one it may write in.
                let place = self.scratch_for(arguments).unwrap_or_else(|| workspace.to_path_buf());
                // (the person has asked for no changes: outside the sandbox nothing would keep a command to the scratch folder)
                if self.scratch.is_some() && crate::safety::outside_sandbox(arguments) {
                    return Err("not run: the user has asked for no changes to the files, and outside the sandbox a command could change anything. Nothing was started.".to_string());
                }
                if arguments.get("background").and_then(Value::as_bool) == Some(true) {
                    self.start_job(arguments, &place)
                } else if arguments.get("wake_on").and_then(Value::as_str).is_some_and(|pattern| !pattern.trim().is_empty()) {
                    // (waited for, it would be no one's to wake: the turn is at the command)
                    Err("wake_on is for a command started with background: true, which goes on while you do other things. The command was not started.".to_string())
                } else {
                    self.run_command(arguments, &place, cancel)
                }
            }
            #[cfg(windows)]
            "check_page" => crate::page_check::check_page(arguments, workspace, cancel),
            "command_status" => self.job_status(arguments, cancel),
            "command_stop" => self.job_stop(arguments),
            "wake_at" => self.wake_at(arguments),
            "todo" => self.todo(arguments),
            "present" => present(arguments, workspace).map(|files| format!("The person sees a card with {}.", files.join(", "))),
            "skill" => self.read_skill(arguments),
            _ if tool.starts_with("mcp__") => {
                let hub = self.mcp.as_ref().ok_or("no MCP server is running")?;
                // What a server says may come from anywhere: it is data, as a page is.
                let (text, pictures) = hub.call_with_pictures(tool, arguments, cancel, std::mem::take(&mut self.mcp_pictures_wanted))?;
                self.mcp_pictures = pictures;
                Ok(crate::web::envelope(tool, &format!("{text}\n")))
            }
            "remember" | "forget" => {
                let folder = self.memory.as_deref().ok_or("notes are not kept here")?;
                if tool == "remember" { crate::memory::remember(folder, workspace, arguments, &self.policy) } else { crate::memory::forget(folder, workspace, arguments, &self.policy) }
            }
            "web_search" | "web_read" if !self.web.settings.enabled => Err("the web tools are turned off in Settings".to_string()),
            "web_search" => self.web.search(arguments, cancel),
            "web_read" => self.web.read(arguments, cancel),
            _ => {
                let (result, seen) = Workbench::run_read(tool, arguments, workspace, self.baseline.as_deref(), &self.secrets, cancel);
                if let Some((path, hash)) = seen {
                    return self.after_read(arguments, path, hash, result);
                }
                result
            }
        }
    }

    /// The scratch folder when this command is to run in it, and not in the workspace.
    pub fn scratch_for(&self, arguments: &Value) -> Option<PathBuf> {
        let scratch = self.scratch.as_ref()?;
        let cwd = resolve(Path::new(""), arguments.get("cwd").and_then(Value::as_str)?);
        (cwd.is_absolute() && crate::safety::within(scratch, &cwd)).then(|| scratch.clone())
    }

    /// Whether this call's files are the scratch folder's: a command that ran in it, or a look at jobs that all did. What such a call changed is no change to the workspace.
    pub fn in_scratch(&self, tool: &str, arguments: &Value) -> bool {
        match tool {
            "execute_command" => self.scratch_for(arguments).is_some(),
            "command_status" => {
                let wanted = arguments.get("id").and_then(Value::as_str);
                let mut jobs = self.jobs.0.iter().filter(|job| wanted.is_none_or(|id| job.id == id)).peekable();
                jobs.peek().is_some() && jobs.all(|job| job.in_scratch)
            }
            _ => false,
        }
    }

    fn read_skill(&mut self, arguments: &Value) -> Result<String, String> {
        let name = text_arg(arguments, "name")?;
        if name.eq_ignore_ascii_case("list") && !self.skills.iter().any(|skill| skill.name.eq_ignore_ascii_case("list")) {
            return Ok(crate::skills::listing(&self.skills));
        }
        let skill = self.skills.iter().find(|skill| skill.name.eq_ignore_ascii_case(name) && skill.for_the_model)
            .ok_or_else(|| format!("no skill is called {name}: the Skills list has the names"))?;
        crate::skills::instructions(skill)
    }

    fn start_job(&mut self, arguments: &Value, workspace: &Path) -> Result<String, String> {
        // (before anything is started: a pattern that cannot be used is the model's to correct)
        let watching = wake_pattern(arguments)?.is_some();
        let started = Instant::now();
        let (cancel, result, live) = spawn_command(&background_arguments(arguments), workspace);
        let id = self.add_job(arguments, started, cancel, result, live)?;
        let wakes = if watching { " The first line it prints that matches wake_on wakes you, once, and it goes on; its end wakes you too." } else { " Its end wakes you with what it came to." };
        Ok(format!("started as {id}.{wakes} Go on with other work, or end your turn if there is nothing else to do: use command_status to look at it, and do not wait in a loop."))
    }

    /// `started`: when it began, which for a command that was waited for and went on in the background is when it was started and not when it was left.
    pub(crate) fn add_job(&mut self, arguments: &Value, started: Instant, cancel: Arc<AtomicBool>, result: CommandResult, live: Arc<Live>) -> Result<String, String> {
        let id = format!("job{}", self.jobs.0.len() + 1);
        let command = text_arg(arguments, "command")?.to_string();
        let in_scratch = self.scratch_for(arguments).is_some();
        let wake_on = arguments.get("wake_on").and_then(Value::as_str).map(str::trim).filter(|pattern| !pattern.is_empty()).map(str::to_string);
        let job = Arc::new(Job { in_scratch, ..Job::new(&id, &command, started, cancel, result, live, wake_on) });
        self.background.add_job(job.clone());
        self.jobs.0.push(job);
        Ok(id)
    }

    /// A command waited for. One still at work when the time is up goes on in the background rather than end: what it had done would otherwise be done again, at the cost of a round
    /// and of a longer wait from the start, and a model has no way to know beforehand how long a first build takes here.
    fn run_command(&mut self, arguments: &Value, workspace: &Path, cancel: &AtomicBool) -> Result<String, String> {
        text_arg(arguments, "command")?;
        let wait = Duration::from_secs(arguments.get("timeout_seconds").and_then(Value::as_u64).unwrap_or(180).clamp(1, 3600));
        let mut given = arguments.clone();
        // (its timeout is how long to wait: in the background it has the same as any other)
        if let Some(fields) = given.as_object_mut() {
            fields.remove("timeout_seconds");
        }
        let started = Instant::now();
        let (stop, result, live) = spawn_command(&given, workspace);
        if let Some(outcome) = wait_for_command(&result, &stop, cancel, wait) {
            return outcome;
        }
        let id = self.add_job(arguments, started, stop, result, live)?;
        Ok(format!("still running after {} s, so it goes on in the background as {id}. Go on with other work and use command_status for it: do not run it again, and do not wait in a loop.", wait.as_secs()))
    }

    /// How one job stands, for `command_status`. Asked for all of them (`all`), an ended one whose output was in such a report already is a line: all of them again each time pushed the
    /// middle ones out of the report, and the model was not told.
    fn job_report(job: &Job, all: bool) -> String {
        match &*job.result.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
            Some(_) if all && job.told.load(Ordering::Relaxed) => format!("{} has ended, as reported before (command_status with its id says it again): {}", job.id, job.command),
            Some(Ok(output)) => {
                job.told.store(true, Ordering::Relaxed);
                format!("{} has ended: {}\n{output}", job.id, job.command)
            }
            Some(Err(error)) => {
                job.told.store(true, Ordering::Relaxed);
                format!("{} did not run: {}\n{error}", job.id, job.command)
            }
            None => {
                let (printed, next) = job.live.since(job.looked.load(Ordering::Relaxed), LIVE_SHOWN);
                let first = job.looked.swap(next, Ordering::Relaxed) == 0 && !job.looked_at.swap(true, Ordering::Relaxed);
                if printed.is_empty() && !first {
                    job.quiet_looks.fetch_add(1, Ordering::Relaxed);
                } else {
                    job.quiet_looks.store(0, Ordering::Relaxed);
                }
                let said = match (printed.is_empty(), first) {
                    (true, true) => "it has printed nothing yet".to_string(),
                    // (a look again with nothing new is a wait: a session on Dogmind's own source looked 15 times in three minutes at a test run, each a request)
                    (true, false) => "it has printed nothing since the last look. Its end wakes you with what it came to: do not look again to wait for it. Go on with other work, or end your turn.".to_string(),
                    (false, true) => format!("what it has printed:\n{printed}"),
                    (false, false) => format!("what it has printed since the last look:\n{printed}"),
                };
                format!("{} is running, for {} s: {}\n{said}", job.id, job.started.elapsed().as_secs(), job.command)
            }
        }
    }

    fn job_status(&self, arguments: &Value, cancel: &AtomicBool) -> Result<String, String> {
        let wanted = arguments.get("id").and_then(Value::as_str);
        // (a look again and again at a running job that prints nothing new is a wait, each a request: a dogfood turn looked at a cargo test 14 times in a row, though the
        // second look already said its end wakes the session. From the third such look, the look waits for it, up to `LOOK_WAITS`, and the person's Stop ends that)
        let mut waited = None;
        if let Some(job) = wanted.and_then(|id| self.jobs.0.iter().find(|job| job.id == id)).filter(|job| !job.has_ended() && job.quiet_looks.load(Ordering::Relaxed) >= QUIET_LOOKS_BEFORE_WAITING) {
            let (from, began) = (job.looked.load(Ordering::Relaxed), Instant::now());
            while began.elapsed() < LOOK_WAITS && !job.has_ended() && !cancel.load(Ordering::Relaxed) && job.live.total() <= from {
                std::thread::sleep(Duration::from_millis(200));
            }
            waited = Some(began.elapsed());
        }
        let mut lines: Vec<String> = self.jobs.0.iter().filter(|job| wanted.is_none_or(|id| job.id == id)).map(|job| Workbench::job_report(job, wanted.is_none())).collect();
        if let (Some(waited), Some(id)) = (waited, wanted) {
            lines.push(format!("(this look waited {} s for {id} to end or print more. Its end wakes you with what it came to: end your turn, or do other work, rather than look again.)", waited.as_secs()));
        }
        lines.extend(self.background.wait_lines(wanted));
        if lines.is_empty() {
            return Ok(wanted.map_or("no command has been started in the background, and no wait is set".to_string(), |id| format!("there is no {id}")));
        }
        Ok(spill("jobs", &lines.join("\n\n"), MAX_OUTPUT))
    }

    /// `wake_at`: the session wakes after this long, with the model's note.
    fn wake_at(&self, arguments: &Value) -> Result<String, String> {
        let delay = arguments.get("delay_seconds").and_then(Value::as_u64).ok_or("delay_seconds is required: a whole number of seconds, from 60 to 86400")?;
        let note = arguments.get("note").and_then(Value::as_str).unwrap_or("");
        self.background.add_wait(delay, note)
    }

    fn job_stop(&self, arguments: &Value) -> Result<String, String> {
        let id = text_arg(arguments, "id")?;
        // A wait is cancelled by its id, here too: it will not wake the session.
        if self.background.cancel_wait(id) {
            return Ok(format!("{id} is cancelled: it will not wake you"));
        }
        let job = self.jobs.0.iter().find(|job| job.id == id).ok_or_else(|| format!("there is no {id}"))?;
        // One that has ended is not being ended: nothing was stopped, and what it came to is what `command_status` says of it.
        if job.has_ended() {
            return Ok(format!("{id} had already ended, so nothing was stopped. As command_status gives it:\n{}", Workbench::job_report(job, false)));
        }
        // (a command that is stopped on purpose does not wake the session when it ends)
        job.stop();
        Ok(format!("{id} is being ended; it will not wake you"))
    }

    /// A read has come back. The same lines of the same file as they were are already in the conversation, and are not sent again to be paid for twice.
    pub fn after_read(&mut self, _arguments: &Value, path: PathBuf, hash: String, result: Result<String, String>) -> Result<String, String> {
        if self.saw(path.clone(), hash.clone()).is_some_and(|before| before != hash) {
            self.covered.remove(&path);
        }
        // A program's read: the file as it is now has been read (an edit may follow), but nothing of it is in the conversation, so it is not
        // what the conversation was shown, nor counted as read into it, and the program always has the text. (A program that read two files
        // again was given "already in the conversation" in their place each time, did nothing with them, and the model ran it six times.)
        if self.in_program {
            return result;
        }
        let text = result?;
        // (the lines shown in a row from where what was seen ends: an outline's head counts, and its parts after a gap do not)
        let mut covered = self.covered.get(&path).copied().unwrap_or(0);
        for number in text.lines().filter_map(|line| line.split_once('|')?.0.parse::<usize>().ok()) {
            if number > covered + 1 {
                break;
            }
            covered = covered.max(number);
        }
        self.covered.insert(path.clone(), covered);
        // Compare what the conversation actually receives. An outline and the first page can
        // start on the same line, while different limits can return exactly the same page.
        let (version, pages) = self.reads.entry(path).or_default();
        if *version != hash {
            *version = hash;
            pages.clear();
        }
        if !pages.insert(Sha256::digest(text.as_bytes()).into()) {
            return Ok(UNCHANGED_READ.to_string());
        }
        // At each so much more in a turn: what it read stays in every later request, and a look further can go to a helper. (Once was
        // not enough: a real model told at 96 KB went on to 390.)
        self.read_this_turn += text.len();
        if self.read_this_turn > READ_TOLD_PAST * (self.read_told + 1) {
            self.read_told = self.read_this_turn / READ_TOLD_PAST;
            Ok(format!("{text}--- this turn has read {} KB of files into the conversation, where it stays for every later request. To look further, give the question to explore: only its answer joins ---\n", self.read_this_turn / 1024))
        } else {
            Ok(text)
        }
    }

    /// A new turn: what it reads is counted from nothing.
    pub fn new_turn(&mut self) {
        self.read_this_turn = 0;
        self.read_told = 0;
    }

    /// What was read may no longer be in the conversation.
    pub fn forget_reads(&mut self) {
        self.reads.clear();
        self.covered.clear();
    }

    /// The file as this session now has seen it, to be told to its file too.
    fn saw(&mut self, path: PathBuf, hash: String) -> Option<String> {
        self.seen_untold.push((path.clone(), hash.clone()));
        self.seen.insert(path, hash)
    }

    /// What it has seen since it was last asked, for the session's file.
    pub fn take_seen(&mut self) -> Vec<(PathBuf, String)> {
        std::mem::take(&mut self.seen_untold)
    }

    /// A conversation reopened: what it had read, as its file kept it. An edit still checks each file as it is now.
    pub fn restore_seen(&mut self, seen: HashMap<PathBuf, String>) {
        self.seen = seen;
        self.seen_untold.clear();
    }

    /// Files went back: a patch must read them again first.
    pub fn forget_seen(&mut self) {
        self.seen.clear();
        self.seen_untold.clear();
        self.reads.clear();
        self.covered.clear();
        self.cut_file = None;
    }

    /// This file's write was cut off and is to go on.
    pub fn note_cut(&mut self, path: PathBuf) {
        self.cut_file = Some(path);
    }

    /// The file whose write was cut off and has not gone on yet.
    pub fn cut_file(&self) -> Option<&Path> {
        self.cut_file.as_deref()
    }

    /// Whether the next write to this file goes on from what is there.
    pub fn goes_on_with(&self, path: &Path) -> bool {
        self.cut_file.as_deref() == Some(path)
    }

    /// A new file, or one the session knows as it is now. That guard is `apply_patch`'s: a model does not write over what it has not seen.
    fn create_file(&mut self, arguments: &Value, workspace: &Path) -> Result<String, String> {
        let given = text_arg(arguments, "path")?;
        let path = resolve(workspace, given);
        let content = text_arg(arguments, "content")?;
        let said = arguments.get("append").and_then(Value::as_bool);
        // A write cut off goes on from where it was cut, unless the model says it is the whole (append false), or writes it again from its
        // first line: added on, that would be there twice.
        let going_on = self.cut_file.as_ref() == Some(&path) && said != Some(false) && !rewrites_from_the_top(&path, content);
        let append = going_on || said == Some(true);
        let mut before = Vec::new();
        let exists = path.exists();
        // (what of it the conversation has been shown, when there is less than all of it)
        let mut seen_in_part = None;
        if exists {
            if path.is_dir() {
                return Err(format!("{given} is a folder"));
            }
            // (first: what it has not read may be a model of 40 GB, and need not be held in memory to say so)
            let Some(known) = self.seen.get(&path) else {
                return Err(format!("{given} exists and has not been read in this session: read it first, then change it with apply_patch or write it again"));
            };
            before = std::fs::read(&path).map_err(|error| format!("{given}: {error}"))?;
            if *known != sha16(&before) {
                return Err(format!("{given} changed since it was read: read it again"));
            }
            // Written whole, it takes the place of all of it: what the conversation has not been shown of it would go without a word.
            let total = file_text(&before).0.lines().count();
            let covered = self.covered.get(&path).copied().unwrap_or(0);
            if covered < total {
                if !append {
                    return Err(format!("{given} has {total} lines, and this conversation has been shown {covered} of them from the top: written whole, the rest would be lost. Change a part with apply_patch, or read on from start_line={} before writing it whole. Nothing was written", covered + 1));
                }
                seen_in_part = Some(covered);
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| format!("{given}: {error}"))?;
        }
        // In the file's own form: its encoding, its byte order mark and its line ends. Appended as bytes, words went onto UTF-16 as UTF-8, and
        // LF lines onto CRLF ones; written over, a mark PowerShell 5.1 needs to read Japanese went.
        // (a new file is all ASCII before it is written)
        let was_ascii = before.is_ascii();
        let mut written = if exists {
            let form = Form::of(&before);
            let new = form.bytes(content, !append).map_err(|why| format!("{given}: {why}. Nothing was written"))?;
            if append { [before, new].concat() } else { new }
        } else {
            content.as_bytes().to_vec()
        };
        let marked = powershell_wants_a_mark(&path, was_ascii, &written);
        if marked {
            written.splice(0..0, UTF8_MARK.iter().copied());
        }
        written_whole(&path, &written, exists).map_err(|error| format!("{given}: {error}: {}", if exists { "the file is as it was" } else { "it was not made" }))?;
        let lines = file_text(&written).0.lines().count();
        self.saw(path.clone(), sha16(&written));
        if self.cut_file.as_ref() == Some(&path) {
            self.cut_file = None;
        }
        self.covered.insert(path, seen_in_part.unwrap_or(lines));
        let word = if !exists { "created" } else if append { "added to" } else { "replaced" };
        let mut said = format!("{word} {given}: now {lines} lines, {} bytes", written.len());
        if marked {
            said.push_str(POWERSHELL_MARK_NOTE);
        }
        if !append && lines >= WRITTEN_WHOLE_LINES && is_a_page(Path::new(given)) {
            said.push_str(&not_opened_yet(lines));
        }
        Ok(if self.in_japanese { said + &not_japanese_note(content) } else { said })
    }

    /// Every edit is validated against the text before anything is written; then the files change together, or, when a
    /// write fails, those already written go back.
    fn apply_patch(&mut self, arguments: &Value, workspace: &Path) -> Result<String, String> {
        // (a list written as a string of JSON is the list: a model sent its edits so twice in one conversation, and each was refused and sent again)
        let supplied = arguments.get("edits").ok_or("edits is required: a list of {path, old_string, new_string}")?;
        let decoded;
        let value = if let Some(text) = supplied.as_str() {
            decoded = serde_json::from_str::<Value>(text).map_err(|error| format!("edits contains invalid JSON: {error}. Send edits as an array of {{path, old_string, new_string}}."))?;
            &decoded
        } else {
            supplied
        };
        let edits = value.as_array().ok_or("edits must be an array of {path, old_string, new_string}")?;
        if edits.is_empty() {
            return Err("edits is empty: provide at least one edit".into());
        }
        let expected: Vec<&str> = arguments.get("expected_files").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        let mut files: Vec<PatchFile> = Vec::new();
        for (number, edit) in edits.iter().enumerate() {
            let given = edit.get("path").and_then(Value::as_str).or(if expected.len() == 1 { expected.first().copied() } else { None })
                .ok_or_else(|| format!("edit {}: path is required", number + 1))?;
            let path = resolve(workspace, given);
            if !expected.is_empty() && !expected.iter().any(|name| same_patch_path(&resolve(workspace, name), &path)) {
                return Err(format!("{given} is not in expected_files: nothing was changed"));
            }
            let old = text_arg(edit, "old_string").map_err(|error| format!("edit {}: {error}", number + 1))?;
            let new = text_arg(edit, "new_string").map_err(|error| format!("edit {}: {error}", number + 1))?;
            let count = match edit.get("count") {
                None => 1,
                Some(value) => value.as_u64().and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| format!("edit {}: count must be a non-negative integer, for example 2: nothing was changed", number + 1))?,
            };
            if old.is_empty() {
                return Err(format!("edit {}: old_string is empty", number + 1));
            }
            let slot = match files.iter().position(|file| same_patch_path(&file.path, &path)) {
                Some(slot) => slot,
                None => {
                    if !path.exists() {
                        return Err(format!("{}: it does not exist: nothing was changed", path.display()));
                    }
                    // (no read is asked for, nor that the file is as it was read: old_string has to be there as often as asked, as it is now, and that is
                    // what keeps an edit to what the model knows of the file. A file the model had written itself and knew to the byte was refused for not
                    // having been read in the session, and 5,207 characters were sent again after a read for nothing.)
                    let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}: nothing was changed", path.display()))?;
                    let Some((mut text, encoding)) = text_of_file(&bytes) else {
                        return Err(format!("{given} is binary: nothing was changed"));
                    };
                    let form = Form::from_text(&bytes, &text, encoding);
                    if encoding == "utf-8" && !form.mark.is_empty() {
                        text.drain(..'\u{feff}'.len_utf8());
                    }
                    // A readable file may still have been decoded lossily. Do not rewrite bytes the decoder cannot recover.
                    if !form.encode(&text, true).is_ok_and(|encoded| encoded == bytes) {
                        return Err(format!("{given} cannot be decoded and re-encoded without changing its bytes: nothing was changed"));
                    }
                    files.push(PatchFile { path, original: bytes, form, before: text.clone(), after: text });
                    files.len() - 1
                }
            };
            let text = &mut files[slot].after;
            // A file with CRLF lines takes an edit written with LF: as written first, and else with CRLF lines, whichever is there as often as
            // asked. (A file with both, as an editor that ended only its last line with CRLF leaves, took every edit as CRLF, and none of more
            // than one line of its LF part could be found: a real model gave up and wrote the whole file again.)
            let as_written = (old.to_string(), new.to_string());
            // (a new_string written with CRLF already keeps them, not one CR more)
            let with_crlf = patch_has_crlf_variant(old, text).then(|| (old.replace('\n', "\r\n"), new.replace("\r\n", "\n").replace('\n', "\r\n")));
            let written_count = text.matches(&as_written.0).count();
            let (old, new, found) = match with_crlf {
                Some((old, new)) if written_count != count => {
                    let found = text.matches(&old).count();
                    if found == count || written_count == 0 {
                        (old, new, found)
                    } else {
                        (as_written.0, as_written.1, written_count)
                    }
                }
                _ => (as_written.0, as_written.1, written_count),
            };
            if found != count {
                // (edit 1 is the first: a model takes it either way, and goes looking in the wrong one)
                let start: String = old.trim().chars().take(40).collect();
                let hint = patch_count_hint(text, &old, found, count);
                return Err(format!("edit {} of {} (`{start}`): old_string occurs {found} times in {given}, not {count}: nothing was changed. {hint}", number + 1, edits.len()));
            }
            let changed = replace_with_the_line_end_there(text, &old, &new);
            if changed.is_empty() && !edit.get("allow_empty_file").and_then(Value::as_bool).unwrap_or(false) {
                return Err(format!("edit {} would leave {given} empty: nothing was changed", number + 1));
            }
            *text = changed;
        }
        for file in &mut files {
            if file.form.encoding == "utf-8" && file.form.mark.is_empty() && powershell_wants_a_mark(&file.path, file.before.is_ascii(), file.after.as_bytes()) {
                file.form.mark = UTF8_MARK;
            }
        }
        let writes = files.iter().map(|file| {
            let bytes = file.form.encode(&file.after, true)
                .map_err(|error| format!("{}: {error}: nothing was changed", file.path.display()))?;
            Ok((file.path.as_path(), file.original.as_slice(), bytes))
        }).collect::<Result<Vec<_>, String>>()?;
        write_all_or_none(&writes)?;
        let mut report = String::new();
        for (file, (_, _, bytes)) in files.iter().zip(writes) {
            let PatchFile { path, before, after, .. } = file;
            // (the model wrote this change itself: enough to see that it landed where it meant, and a local model pays for every token of it, again in each later request's prefix)
            let diff = similar::TextDiff::from_lines(before, after).unified_diff().context_radius(2).to_string();
            let name = path.strip_prefix(workspace).unwrap_or(path).display().to_string();
            report.push_str(&format!("--- applied_diff {name} ---\n{}\n", cap(&diff, MAX_APPLIED_DIFF)));
            if bytes.starts_with(UTF8_MARK) && !file.original.starts_with(UTF8_MARK) {
                report.push_str(&format!("{name} was{}\n", POWERSHELL_MARK_NOTE.trim_start_matches(" (written").trim_end_matches(')')));
            }
            // (seen whole before, it is seen whole after: the model wrote what changed)
            if self.covered.get(path).is_some_and(|covered| *covered >= before.lines().count()) {
                self.covered.insert(path.clone(), after.lines().count());
            }
            // A write cut off and going on is not, once its file is patched: the next create_file of it is the model's own, whole or not as it says.
            if self.cut_file.as_ref() == Some(path) {
                self.cut_file = None;
            }
            self.saw(path.clone(), sha16(&bytes));
        }
        let mut said = spill("edits", &report, MAX_OUTPUT);
        if self.in_japanese {
            for edit in edits {
                said.push_str(&not_japanese_note(edit.get("new_string").and_then(Value::as_str).unwrap_or("")));
            }
        }
        Ok(said)
    }

    fn todo(&mut self, arguments: &Value) -> Result<String, String> {
        apply_todo(&mut self.todos, arguments)?;
        Ok(self.todos.iter().map(|item| format!("[{}] {}: {}", item["status"].as_str().unwrap_or("pending"), item["id"].as_str().unwrap_or(""), item["text"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n"))
    }
}

/// The files a `present` call names, as they are from the workspace's root with forward slashes: each has to be a file inside it.
pub fn present(arguments: &Value, workspace: &Path) -> Result<Vec<String>, String> {
    let paths = arguments.get("paths").and_then(Value::as_array).ok_or("paths is required: the files to put before the person")?;
    let mut files = Vec::new();
    for path in paths {
        let path = path.as_str().map(str::trim).filter(|path| !path.is_empty()).ok_or("each path is a file's path, from the workspace's root")?;
        let file = resolve(workspace, path);
        let inside = file.strip_prefix(workspace).map_err(|_| format!("{path} is not in the workspace"))?;
        if !file.is_file() {
            return Err(format!("{path} is not a file of the workspace"));
        }
        let shown = inside.to_string_lossy().replace('\\', "/");
        if !files.contains(&shown) {
            files.push(shown);
        }
    }
    if files.is_empty() {
        return Err("paths is empty: name the files to put before the person".to_string());
    }
    Ok(files)
}

/// A `todo` call's arguments applied to a checklist: an item with an id the list has changes the fields it sends, a new one is added (the first of an empty list `doing`, the others `pending`, unless it says),
/// and `merge: false` begins the list afresh. A call that fails part of the way leaves the items before the failing one in. A list is nothing but its calls, so whoever reads a conversation replays them with this.
pub fn apply_todo(todos: &mut Vec<Value>, arguments: &Value) -> Result<(), String> {
    let items = arguments.get("todos").and_then(Value::as_array).ok_or("todos is required")?;
    if arguments.get("merge").and_then(Value::as_bool) == Some(false) {
        todos.clear();
    }
    for item in items {
        let id = item.get("id").and_then(Value::as_str).ok_or("each todo needs an id")?;
        match todos.iter_mut().find(|known| known.get("id").and_then(Value::as_str) == Some(id)) {
            Some(known) => {
                for (name, value) in item.as_object().into_iter().flatten() {
                    known[name] = value.clone();
                }
            }
            None => {
                let mut fresh = item.clone();
                if fresh.get("status").is_none() {
                    fresh["status"] = json!(if todos.is_empty() { "doing" } else { "pending" });
                }
                todos.push(fresh);
            }
        }
    }
    Ok(())
}

/// `new` put in place of each `old` in `text`, its lines ending as the line it is put on ends. (A model writes its lines with LF: one line
/// replaced by three, in a file of CRLF lines, left LF lines among them. A `\n` that has a CR before it already keeps just that one.)
fn replace_with_the_line_end_there(text: &str, old: &str, new: &str) -> String {
    let bare_lf = new.char_indices().any(|(at, c)| c == '\n' && !new[..at].ends_with('\r'));
    if !bare_lf {
        return text.replace(old, new);
    }
    let with_crlf = new.replace("\r\n", "\n").replace('\n', "\r\n");
    let mut out = String::with_capacity(text.len() + new.len());
    let mut from = 0;
    for (at, found) in text.match_indices(old) {
        out.push_str(&text[from..at]);
        out.push_str(if line_ends_with_crlf(text, at) { &with_crlf } else { new });
        from = at + found.len();
    }
    out.push_str(&text[from..]);
    out
}

fn patch_has_crlf_variant(old: &str, text: &str) -> bool {
    old.contains('\n') && !old.contains("\r\n") && text.contains("\r\n")
}

/// Whether the line `text` has at `at` ends with CRLF; a last line with no end takes the one before it.
fn line_ends_with_crlf(text: &str, at: usize) -> bool {
    let end = text[at..].find('\n').map(|found| at + found).or_else(|| text[..at].rfind('\n'));
    end.is_some_and(|end| text[..end].ends_with('\r'))
}

/// Whether `content` begins as the file does: written again from its first line, not going on from its last.
fn rewrites_from_the_top(path: &Path, content: &str) -> bool {
    let Ok(bytes) = std::fs::read(path) else { return false };
    let plain = |text: &str| text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let (text, content) = (plain(&file_text(&bytes).0), plain(content));
    // (its first three lines, as far as 120 characters: a part written on from where a cut left it never begins so)
    let three: usize = text.split_inclusive('\n').take(3).map(str::len).sum();
    let head: String = text[..three].chars().take(120).collect();
    head.trim().chars().count() >= 20 && content.starts_with(&head)
}

/// UTF-8's byte order mark.
const UTF8_MARK: &[u8] = &[0xEF, 0xBB, 0xBF];

/// What a write says when it gave a PowerShell script the mark (`powershell_wants_a_mark`).
const POWERSHELL_MARK_NOTE: &str = " (written with a UTF-8 byte order mark: Windows PowerShell 5.1 reads a script without one in the ANSI code page, and its non-ASCII text would break)";

/// Whether a PowerShell script written as these bytes takes a UTF-8 byte order mark: it has text beyond ASCII in UTF-8, has no mark, and was all
/// ASCII before (or is new). Windows PowerShell 5.1 reads a script without a mark in the ANSI code page (932 on a Japanese Windows): written as plain
/// UTF-8, 「こんにちは。短い一文で返してください。」 came out as 「縺薙ｓ…」 and ended the string early (TerminatorExpectedAtEndOfString, two runs of
/// two, and each time the model put the words into English to get past it). The mark is what PSScriptAnalyzer's PSUseBOMForUnicodeEncodedFile asks
/// for and what the PowerShell extension of VS Code writes; PowerShell 7 reads the file the same with it. A script that already had more than ASCII
/// without one was written so on purpose (for PowerShell 7) and keeps its form, as Shift_JIS and UTF-16 ones do.
fn powershell_wants_a_mark(path: &Path, was_ascii: bool, bytes: &[u8]) -> bool {
    let script = path.extension().and_then(|kind| kind.to_str()).is_some_and(|kind| ["ps1", "psm1", "psd1"].iter().any(|one| kind.eq_ignore_ascii_case(one)));
    script && was_ascii && !bytes.is_ascii() && !bytes.starts_with(UTF8_MARK) && std::str::from_utf8(bytes).is_ok()
}

/// How a file is written, for what is written to it to be written the same way: its encoding, whether it begins with a byte order mark,
/// and whether its lines end with CRLF.
struct Form {
    encoding: &'static str,
    mark: &'static [u8],
    crlf: bool,
}

impl Form {
    fn of(bytes: &[u8]) -> Form {
        let (text, encoding) = file_text(bytes);
        Self::from_text(bytes, &text, encoding)
    }

    fn from_text(bytes: &[u8], text: &str, encoding: &'static str) -> Form {
        let mark: &'static [u8] = [&[0xEF, 0xBB, 0xBF][..], &[0xFF, 0xFE], &[0xFE, 0xFF]].into_iter().find(|mark| bytes.starts_with(mark)).unwrap_or(&[]);
        // (as most of its lines end: an editor may have ended only its last one otherwise)
        let ends = text.matches('\n').count();
        Form { encoding, mark, crlf: ends > 0 && text.matches("\r\n").count() * 2 > ends }
    }

    /// `text` as this file has it; with its mark when it is the whole of the file.
    fn bytes(&self, text: &str, whole: bool) -> Result<Vec<u8>, String> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        if self.crlf {
            self.encode(&text.replace("\r\n", "\n").replace('\n', "\r\n"), whole)
        } else {
            self.encode(text, whole)
        }
    }

    /// Encodes already edited text without changing its line endings.
    fn encode(&self, text: &str, whole: bool) -> Result<Vec<u8>, String> {
        let mut out = if whole { self.mark.to_vec() } else { Vec::new() };
        match self.encoding {
            "utf-8" => out.extend_from_slice(text.as_bytes()),
            "utf-16" if self.mark == [0xFE, 0xFF] => out.extend(text.encode_utf16().flat_map(u16::to_be_bytes)),
            "utf-16" => out.extend(text.encode_utf16().flat_map(u16::to_le_bytes)),
            "shift_jis" => out.extend(shift_jis(text)?),
            _ => return Err("it is in an encoding this cannot write (not UTF-8, UTF-16 or Shift_JIS)".into()),
        }
        Ok(out)
    }
}

/// Text in Shift_JIS, or which of its characters have no form there.
fn shift_jis(text: &str) -> Result<Vec<u8>, String> {
    in_code_page_932(text).ok_or_else(|| {
        let missing: String = text.chars().filter(|letter| !letter.is_ascii() && in_code_page_932(&letter.to_string()).is_none()).take(8).collect();
        format!("it is Shift_JIS, and these have no form there: {missing}. Leave them out, or ask the user whether it may become UTF-8")
    })
}

fn in_code_page_932(text: &str) -> Option<Vec<u8>> {
    use windows_sys::Win32::Globalization::{WideCharToMultiByte, WC_NO_BEST_FIT_CHARS};
    let wide: Vec<u16> = text.encode_utf16().collect();
    if wide.is_empty() {
        return Some(Vec::new());
    }
    let mut out = vec![0u8; wide.len() * 2];
    let mut replaced = 0;
    // SAFETY: both buffers are as long as the call is told, and a character takes at most two bytes in this code page.
    let written = unsafe { WideCharToMultiByte(932, WC_NO_BEST_FIT_CHARS, wide.as_ptr(), wide.len() as i32, out.as_mut_ptr(), out.len() as i32, std::ptr::null(), &mut replaced) };
    (written > 0 && replaced == 0).then(|| {
        out.truncate(written as usize);
        out
    })
}

/// A file's text and the name of what it was in. On a Japanese Windows that may be Shift_JIS, and what PowerShell's `>` writes is UTF-16.
pub(crate) fn file_text(bytes: &[u8]) -> (String, &'static str) {
    let (text, encoding) = decode_file_text(bytes);
    (text.into_owned(), encoding)
}

fn decode_file_text(bytes: &[u8]) -> (Cow<'_, str>, &'static str) {
    let wide = |pair: fn([u8; 2]) -> u16| String::from_utf16_lossy(&bytes[2..].chunks_exact(2).map(|two| pair([two[0], two[1]])).collect::<Vec<u16>>());
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return (Cow::Owned(wide(u16::from_le_bytes)), "utf-16");
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return (Cow::Owned(wide(u16::from_be_bytes)), "utf-16");
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return (Cow::Borrowed(text), "utf-8");
    }
    match crate::web::from_code_page(bytes, 932) {
        Some(text) => (Cow::Owned(text), "shift_jis"),
        None => (String::from_utf8_lossy(bytes), "unknown"),
    }
}

/// A file's bytes as text to be shown, as the agent reads files (`file_text`): UTF-16 with its mark, else UTF-8, else code page 932, with the name of what it was. None for a binary, which is
/// a NUL in the first 8 KB of what is not UTF-16.
pub fn text_of_file(bytes: &[u8]) -> Option<(String, &'static str)> {
    readable_file_text(bytes).map(|(text, encoding)| (text.into_owned(), encoding))
}

fn readable_file_text(bytes: &[u8]) -> Option<(Cow<'_, str>, &'static str)> {
    let utf16 = bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]);
    if !utf16 && bytes.iter().take(8192).any(|&byte| byte == 0) {
        return None;
    }
    Some(decode_file_text(bytes))
}

fn read_file(arguments: &Value, workspace: &Path) -> Result<(String, (PathBuf, String)), String> {
    let path = resolve(workspace, text_arg(arguments, "path")?);
    if crate::images::is_picture(&path) {
        return Err(format!("{} is a picture, and it cannot be read here: this model cannot be shown one by a tool. Say so, and ask the user to attach it to a message.", path.display()));
    }
    // (read whole to number its lines: one larger than memory would end the app, with all that is going on in it)
    let size = std::fs::metadata(&path).map_err(|error| format!("{}: {error}", path.display()))?.len();
    if size > MOST_READ_WHOLE {
        return Err(format!("{} is too large to read here, at {} MB: use a command that reads just the part you need", path.display(), size / 1_048_576));
    }
    let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let Some((text, encoding)) = readable_file_text(&bytes) else {
        return Err(format!("{} is binary: unsupported binaries are refused", path.display()));
    };
    let hash = sha16(&bytes);
    let text = if encoding == "utf-8" { text.strip_prefix('\u{feff}').unwrap_or(&text) } else { &text };
    let placed = arguments.get("start_line").is_some() || arguments.get("limit").is_some();
    // As in str::lines, a trailing newline terminates the last line.
    let line_count = memchr::memchr_iter(b'\n', text.as_bytes()).count()
        + usize::from(!text.is_empty() && !text.ends_with('\n'));
    let limit = arguments.get("limit").and_then(Value::as_i64).unwrap_or(READ_LINES as i64).clamp(1, READ_LINES as i64) as usize;
    let start = arguments.get("start_line").and_then(Value::as_i64).unwrap_or(1);
    let first = if start < 0 { line_count.saturating_sub(start.unsigned_abs() as usize) } else { (start.max(1) as usize) - 1 };
    // (the guard on edits keeps its own hash: the model has no use for one)
    let header = if encoding == "utf-8" { String::new() } else { format!("--- encoding={encoding} ---\n") };
    let mut out = header.clone();
    if first >= line_count {
        // (nothing below this line could mean a file that ends here or a read that went past it)
        out.push_str(&if line_count == 0 { "--- the file is empty ---\n".to_string() } else { format!("--- the file has only {} lines ---\n", line_count) });
        return Ok((out, (path, hash)));
    }
    let next = append_read_page(&mut text.lines().skip(first).peekable(), first, limit, &mut out);
    // Line numbers and the encoding header count towards the page budget. Check the
    // actual first page before offering an outline. An oversized file with abbreviated
    // lines still gets its outline even if every line number fits.
    if !placed && (next < line_count || text.len() > MAX_READ) {
        let parts = outline(text);
        if !parts.is_empty() {
            return Ok((outlined(text, line_count, &parts, size, &header), (path, hash)));
        }
    }
    if next > first && next < line_count {
        out.push_str(&format!("--- {} more lines; continue with start_line={} ---\n", line_count - next, next + 1));
    }
    Ok((out, (path, hash)))
}

/// Append one page to its encoding header. Leave the first line of the next page in the iterator.
fn append_read_page<'a>(lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>, first: usize, limit: usize, out: &mut String) -> usize {
    use std::fmt::Write;
    let mut next = first;
    while next - first < limit {
        let Some(&line) = lines.peek() else { break };
        let shown = if line.len() > MAX_LINE {
            let cut = line.floor_char_boundary(MAX_LINE);
            Cow::Owned(format!("{}... ({} more bytes on this line)", &line[..cut], line.len() - cut))
        } else {
            Cow::Borrowed(line)
        };
        if next > first && out.len() + shown.len() > MAX_READ {
            break;
        }
        lines.next();
        next += 1;
        let _ = writeln!(out, "{next}|{shown}");
    }
    next
}

/// Count short reads using the same pages we send. For large jobs give a proven lower bound
/// without formatting the entire file just to show the outline.
fn outline_read_count(text: &str, line_count: usize, header: &str) -> String {
    const COUNT_UP_TO: usize = 8;
    let minimum = line_count.div_ceil(READ_LINES);
    if minimum > COUNT_UP_TO {
        return format!("at least {minimum} reads");
    }
    let mut lines = text.lines().peekable();
    let mut page = String::new();
    let mut first = 0;
    for count in 1..=COUNT_UP_TO {
        page.clear();
        page.push_str(header);
        first = append_read_page(&mut lines, first, READ_LINES, &mut page);
        if first == line_count {
            return if count == 1 { "1 read".into() } else { format!("{count} reads") };
        }
    }
    format!("at least {} reads", COUNT_UP_TO + (line_count - first).div_ceil(READ_LINES))
}

/// Where a file's parts begin, by line: functions, types, classes, sections, in the languages a project is mostly written in. Empty
/// for a file of none of them, which is then read as any other.
fn outline(text: &str) -> Vec<(usize, &str)> {
    static STARTS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let starts = STARTS.get_or_init(|| {
        regex::Regex::new(concat!(
            r"^(?:",
            // Rust
            r"(?:pub(?:\([^)]*\))?\s+)?(?:(?:async|const|unsafe|extern(?:\s+\x22[^\x22]*\x22)?)\s+)*(?:fn|struct|enum|trait|impl|mod|union|type|macro_rules!)\b",
            // Python
            r"|(?:async\s+)?def\s|class\s",
            // JavaScript, TypeScript, Go, Java, C#, Kotlin, Swift
            r"|(?:export\s+)?(?:default\s+)?(?:abstract\s+)?(?:async\s+)?(?:function\*?|class|interface|enum|namespace)\b",
            r"|func\s|(?:(?:public|private|protected|internal|static|final|override|virtual|sealed|partial|open)\s+)+[\w<>\[\],\s]+\(",
            // Markdown
            r"|#{1,4}\s",
            r")"
        ))
        .unwrap()
    });
    let mut headings = text.lines().enumerate().filter(|(_, line)| {
        let trimmed = line.trim_start();
        line.len() - trimmed.len() <= 8 && starts.is_match(trimmed)
    });
    let mut parts: Vec<_> = headings.by_ref().take(OUTLINE_MOST + 1).collect();
    if parts.len() > OUTLINE_MOST {
        // Once the outline overflows, only the first top-level headings are shown.
        let top_level = |(_, line): &(usize, &str)| !line.starts_with(char::is_whitespace);
        parts.retain(top_level);
        parts.truncate(OUTLINE_MOST);
        parts.extend(headings.filter(top_level).take(OUTLINE_MOST - parts.len()));
    }
    parts
}

/// A large file, as its head and its outline: each part's first line with its number, to read with `start_line` and `limit`.
fn outlined(text: &str, line_count: usize, parts: &[(usize, &str)], size: u64, header: &str) -> String {
    let shown = |line: &str| -> String { if line.len() > 160 { format!("{}…", &line[..line.floor_char_boundary(160)]) } else { line.to_string() } };
    // (and the way to all of it, for a task that needs all of it, such as summing a document up: a model given only the outline once took
    // the long way round, a helper that read it all again, 105 s of a 186-s task)
    let reads = outline_read_count(text, line_count, header);
    let mut out = format!(
        "--- {} lines, {} KB: first lines and outline. For all numbered lines ({reads}), use read_file with start_line=1, then follow each continuation. Lines over {MAX_LINE} bytes are shortened ---\n",
        line_count,
        size / 1024
    );
    for (at, line) in text.lines().enumerate().take(OUTLINE_HEAD) {
        out.push_str(&format!("{}|{}\n", at + 1, shown(line)));
    }
    out.push_str("--- outline ---\n");
    for (at, line) in parts.iter().filter(|(at, _)| *at >= OUTLINE_HEAD) {
        out.push_str(&format!("{}|{}\n", at + 1, shown(line)));
    }
    out.push_str("--- read the part you need: read_file with start_line and limit, at a line above, or one search_files finds ---\n");
    out
}

/// What a walk through a project does not go into, as `.gitignore` does not always say so: outside a repository it is not read at all.
const HEAVY: [&str; 6] = ["node_modules", ".venv", "__pycache__", ".mypy_cache", ".pytest_cache", ".tox"];

/// `deep`: going down through folders. A folder named as the place to look is looked in, and one level shows what is there.
pub(crate) fn walker(root: &Path, deep: bool) -> ignore::WalkBuilder {
    walker_matching(root, deep, None)
}

/// Apply the search glob after the walker's ignore rules. An override on the walker would restore ignored files.
fn walker_matching(root: &Path, deep: bool, glob: Option<ignore::overrides::Override>) -> ignore::WalkBuilder {
    let mut walk = ignore::WalkBuilder::new(root);
    walk.hidden(false).filter_entry(move |entry| {
        let name = entry.file_name();
        name != ".git" && !(deep && entry.depth() > 0 && HEAVY.iter().any(|heavy| name == *heavy))
            && !glob.as_ref().is_some_and(|glob| glob.matched(entry.path(), entry.file_type().is_some_and(|kind| kind.is_dir())).is_ignore())
    });
    walk
}

/// Match locations refer to the working text for this edit, including earlier edits in the same patch.
fn patch_count_hint(text: &str, old: &str, found: usize, expected: usize) -> String {
    if found == 0 {
        return format!("Check this edit's path. If the file is correct, use its exact text without line numbers{}", nearest_line(text, old));
    }
    let mut lines = Vec::new();
    let (mut previous, mut line) = (0, 1);
    for (offset, _) in text.match_indices(old).take(5) {
        line += text.as_bytes()[previous..offset].iter().filter(|&&b| b == b'\n').count();
        lines.push(line.to_string());
        previous = offset;
    }
    if found > lines.len() {
        lines.push("...".to_string());
    }
    let advice = if found > expected { "Include surrounding text to identify the intended matches." } else { "Check the requested count and the text of the missing matches." };
    format!("Match start lines at this edit: {}. {advice}", lines.join(", "))
}

/// Where the first line of an edit that did not match is, if its words are there with other spacing.
fn nearest_line(text: &str, old: &str) -> String {
    let squeeze = |line: &str| line.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut requested = old.lines().skip_while(|line| line.trim().is_empty());
    let Some(first) = requested.next().map(squeeze) else { return String::new() };
    let places: Vec<usize> = text.lines().enumerate().filter(|(_, line)| squeeze(line) == first).map(|(at, _)| at).take(5).collect();
    if let [at] = places.as_slice() {
        // A short complete excerpt is safe to copy. For long or ambiguous matches, give the locations.
        let lines = 1 + requested.take(5).count();
        if lines <= 5 {
            let nearby: Vec<_> = text.lines().skip(*at).take(lines).collect();
            if nearby.iter().map(|line| line.len() + 1).sum::<usize>() <= 640 {
                return format!(". At this edit, the working text starting at line {} is:\n{}", at + 1, nearby.join("\n"));
            }
        }
    }
    if !places.is_empty() {
        let numbers: Vec<_> = places.iter().map(|at| (at + 1).to_string()).collect();
        return format!(". Its first line is at line {}, spaced differently or followed by other lines: read there", numbers.join(", "));
    }
    // One character different, as a Chinese form for a Japanese one, or one word, is what it cannot see in what it read: the line itself, to copy from, with no read for it.
    // How alike two lines are is how many of their pairs of letters they share, against how many they have: a long line that holds the same letters elsewhere is not
    // like the line. (By the letters alone, a long line of the file won over the line that differed by one word, and the model was shown that one.)
    let wanted = letter_pairs(&first);
    let best = text.lines().enumerate().map(|(at, line)| (alike(&wanted, &letter_pairs(&squeeze(line))), at, line)).max_by(|a, b| a.0.total_cmp(&b.0).then(b.1.cmp(&a.1)));
    match best {
        Some((score, at, line)) if score >= 0.6 => format!(". The nearest is line {}, character for character: {}", at + 1, line.trim().chars().take(240).collect::<String>()),
        _ => String::new(),
    }
}

/// A line's pairs of letters, each as often as it is there.
fn letter_pairs(line: &str) -> HashMap<(char, char), usize> {
    let letters: Vec<char> = line.chars().collect();
    let mut pairs = HashMap::new();
    for pair in letters.windows(2) {
        *pairs.entry((pair[0], pair[1])).or_insert(0) += 1;
    }
    pairs
}

/// How alike two lines are, from 0 to 1, by the pairs of letters they share (Dice's measure).
fn alike(one: &HashMap<(char, char), usize>, other: &HashMap<(char, char), usize>) -> f64 {
    let (total_one, total_other): (usize, usize) = (one.values().sum(), other.values().sum());
    if total_one + total_other == 0 {
        return 0.0;
    }
    let shared: usize = one.iter().map(|(pair, count)| (*count).min(other.get(pair).copied().unwrap_or(0))).sum();
    2.0 * shared as f64 / (total_one + total_other) as f64
}

/// Han characters no Japanese text is written with: a local model's Chinese comes through in the middle of good Japanese, one word at a time, and it does not see it unless told.
fn not_japanese(text: &str) -> Vec<char> {
    use windows_sys::Win32::Globalization::{WideCharToMultiByte, WC_NO_BEST_FIT_CHARS};
    const SHIFT_JIS: u32 = 932;
    // Japanese all the same, and outside Shift_JIS: the forms the 2010 list of everyday kanji and newspapers use.
    const JAPANESE_ANYWAY: &str = "塡剝頰噓俱繫鷗醬";
    let mut found: Vec<char> = Vec::new();
    for letter in text.chars().filter(|letter| ('\u{3400}'..='\u{9FFF}').contains(letter) && !JAPANESE_ANYWAY.contains(*letter)) {
        if found.contains(&letter) || found.len() == 8 {
            continue;
        }
        let unit = [letter as u32 as u16];
        let (mut bytes, mut replaced) = ([0u8; 4], 0);
        // SAFETY: both buffers are as long as the call is told.
        let written = unsafe { WideCharToMultiByte(SHIFT_JIS, WC_NO_BEST_FIT_CHARS, unit.as_ptr(), 1, bytes.as_mut_ptr(), bytes.len() as i32, std::ptr::null(), &mut replaced) };
        if written > 0 && replaced != 0 {
            found.push(letter);
        }
    }
    found
}

/// What a page written whole is told, before it has been opened: the next step is to open it. In the comparison runs a model wrote a page of 30 to 56 KB, read
/// it over in its reasoning, and wrote it whole again before it had opened it once, six times in five runs, each 300 to 400 s of writing; what opening it would
/// have shown was then still to be found.
fn not_opened_yet(lines: usize) -> String {
    format!("\nnote: open it with check_page before you change it: what it shows is what to fix. Change it with apply_patch, the parts that change: writing all {lines} lines again takes as long as writing them did.")
}
/// How long a page is, in lines, for [`not_opened_yet`]: one shorter is soon written again.
const WRITTEN_WHOLE_LINES: usize = 300;

/// Whether a file is a page, which check_page opens.
fn is_a_page(path: &Path) -> bool {
    let extension = path.extension().map_or(String::new(), |extension| extension.to_string_lossy().to_ascii_lowercase());
    matches!(extension.as_str(), "html" | "htm" | "xhtml")
}

fn not_japanese_note(text: &str) -> String {
    let found: String = not_japanese(text).into_iter().collect();
    if found.is_empty() { String::new() } else { format!("\nnote: {found} in this text is Chinese, not Japanese. Use the Japanese word, here and in your answer.") }
}

pub(crate) fn has_letters_not_japanese(text: &str) -> bool {
    text.lines().any(|line| !not_japanese(line).is_empty())
}

/// The letters of a Japanese conversation's answer worth asking about: not what it was given to read, and not a line that is Chinese on purpose, which says so or reads as Chinese throughout.
pub(crate) fn slips_in_answer(answer: &str, given: &str) -> String {
    const ABOUT_CHINESE: [&str; 5] = ["中国語", "中文", "簡体", "繁体", "台湾"];
    let mut slips: Vec<char> = Vec::new();
    for line in answer.lines().filter(|line| !ABOUT_CHINESE.iter().any(|word| line.contains(word))) {
        let found = not_japanese(line);
        let han = line.chars().filter(|letter| ('\u{3400}'..='\u{9FFF}').contains(letter)).count();
        let kana = line.chars().any(|letter| ('\u{3040}'..='\u{30FF}').contains(&letter));
        if !kana && han >= 8 && found.len() * 4 >= han {
            continue;
        }
        slips.extend(found.into_iter().filter(|letter| !given.contains(*letter)));
    }
    slips.sort_unstable();
    slips.dedup();
    slips.truncate(8);
    slips.into_iter().collect()
}

fn find_files(arguments: &Value, workspace: &Path, cancel: &AtomicBool) -> Result<String, String> {
    let pattern = text_arg(arguments, "pattern")?;
    let root = resolve(workspace, arguments.get("path").and_then(Value::as_str).unwrap_or("."));
    if !root.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    let mut globs = ignore::overrides::OverrideBuilder::new(&root);
    globs.add(pattern).map_err(|error| format!("{pattern}: {error}"))?;
    // Matched after the walk: given to the walk as an override, a glob would bring back what .gitignore leaves out.
    let globs = globs.build().map_err(|error| format!("{pattern}: {error}"))?;
    let mut names = Vec::new();
    for entry in walker(&root, true).build().flatten() {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        if entry.file_type().is_some_and(|kind| kind.is_file()) && globs.matched(entry.path(), false).is_whitelist() {
            names.push(entry.path().strip_prefix(&root).unwrap_or(entry.path()).to_string_lossy().replace('\\', "/"));
            if names.len() > LIST_LIMIT {
                break;
            }
        }
    }
    names.sort();
    if names.is_empty() {
        return Ok(format!("no file under {} matches {pattern}", root.display()));
    }
    let more = names.len() > LIST_LIMIT;
    names.truncate(LIST_LIMIT);
    Ok(format!("{}{}", names.join("\n"), if more { format!("\n--- truncated: the first {LIST_LIMIT}; narrow the pattern ---") } else { String::new() }))
}

/// What a listing gives the size of: what is not read in one go. (At 256 KB, a real model reviewing a crate of 25 files ran three
/// commands to count their lines before it read any.)
const BIG_FILE: u64 = 32 * 1024;

fn list_files(arguments: &Value, workspace: &Path, cancel: &AtomicBool) -> Result<String, String> {
    let root = resolve(workspace, arguments.get("path").and_then(Value::as_str).unwrap_or("."));
    if !root.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    let recursive = arguments.get("recursive").and_then(Value::as_bool).unwrap_or(false);
    let mut walk = walker(&root, recursive);
    if !recursive {
        // A direct listing shows the folder's actual children, including ignored build and package folders.
        walk.standard_filters(false).max_depth(Some(1));
    }
    let mut names = Vec::new();
    const COUNT_LIMIT: usize = 50_000;
    let (mut entries, mut files, mut bytes) = (0usize, 0usize, 0u64);
    for entry in walk.build().flatten() {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        if entry.depth() == 0 {
            continue;
        }
        entries += 1;
        let kind = entry.file_type();
        let size = if kind.is_some_and(|kind| kind.is_file()) {
            files += 1;
            let size = entry.metadata().map_or(0, |data| data.len());
            bytes += size;
            size
        } else {
            0
        };
        if names.len() < LIST_LIMIT {
            let folder = kind.is_some_and(|kind| kind.is_dir());
            let name = entry.path().strip_prefix(&root).unwrap_or(entry.path()).to_string_lossy().replace('\\', "/");
            // (a large one says so here: a model about to read a log first asks how long it is, in a round of its own)
            names.push(match (folder, size >= BIG_FILE) {
                (true, _) => format!("{name}/"),
                (false, true) if size < 1 << 20 => format!("{name} ({} KB)", size / 1024),
                (false, true) => format!("{name} ({:.1} MB)", size as f64 / 1_048_576.0),
                (false, false) => name,
            });
        }
        if entries >= COUNT_LIMIT {
            break;
        }
    }
    names.sort();
    // (no words at all would read as a tool that did not answer)
    if names.is_empty() {
        return Ok(format!("{} is empty", root.display()));
    }
    let mut out = names.join("\n");
    if entries > LIST_LIMIT {
        let lower_bound = if entries == COUNT_LIMIT { "at least " } else { "" };
        let unit = if files == 1 { "file" } else { "files" };
        out.push_str(&format!("\n--- truncated: showing first {LIST_LIMIT} of {lower_bound}{entries} entries; counted {files} {unit} ({bytes} bytes); use search_files for targeted lookup ---"));
    }
    Ok(out)
}

fn search_files(arguments: &Value, workspace: &Path, secrets_besides: &[PathBuf], cancel: &AtomicBool) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(arguments.get("timeout_seconds").and_then(Value::as_u64).unwrap_or(30).clamp(1, 300));
    search_files_until(arguments, workspace, secrets_besides, || {
        if cancel.load(Ordering::Relaxed) { Err("cancelled".into()) } else { Ok(Instant::now() >= deadline) }
    })
}

fn search_files_until(arguments: &Value, workspace: &Path, secrets_besides: &[PathBuf], mut time_is_up: impl FnMut() -> Result<bool, String>) -> Result<String, String> {
    let pattern = text_arg(arguments, "pattern")?;
    let literal = arguments.get("literal").and_then(Value::as_bool).unwrap_or(false);
    let root = resolve(workspace, arguments.get("path").and_then(Value::as_str).unwrap_or("."));
    let smart_case = !pattern.chars().any(char::is_uppercase);
    let build = |text: &str| RegexBuilder::new(text).case_insensitive(smart_case).build();
    let mut note = "";
    let regex = if literal { build(&regex::escape(pattern)) } else {
        build(pattern).or_else(|_| {
            note = "--- not a valid regex: searched as literal text ---\n";
            build(&regex::escape(pattern))
        })
    }.map_err(|error| error.to_string())?;
    let glob = if let Some(pattern) = arguments.get("file_glob").and_then(Value::as_str) {
        let mut builder = ignore::overrides::OverrideBuilder::new(&root);
        builder.add(pattern).map_err(|error| format!("file_glob: {error}"))?;
        Some(builder.build().map_err(|error| format!("file_glob: {error}"))?)
    } else {
        None
    };
    let walk = walker_matching(&root, true, glob);
    let mut out = note.to_string();
    let (mut matches, mut stopped, mut secrets) = (0usize, "", 0usize);
    'files: for entry in walk.build() {
        if time_is_up()? {
            stopped = "the time ran out";
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.depth() == Some(0) => return Err(error.to_string()),
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if entry.metadata().is_ok_and(|data| data.len() > SEARCH_FILE_LIMIT) {
            if entry.depth() == 0 {
                return Err(format!("{} exceeds the {} MiB search limit: use a command that reads just the part you need", root.display(), SEARCH_FILE_LIMIT >> 20));
            }
            continue;
        }
        // `read_file` asks a person before it opens one of these, and a pattern of `.` would otherwise print it without asking. Searching a place that is itself one has been asked about.
        // The same goes for what else holds keys here, of which a wider folder holds some: `%LOCALAPPDATA%` holds this app's sessions.
        let among_the_others = secrets_besides.iter().any(|secret| crate::safety::inside(secret, entry.path()) && !crate::safety::inside(secret, &root));
        if (crate::safety::sensitive(entry.path()) && !crate::safety::sensitive(&root)) || among_the_others {
            secrets += 1;
            continue;
        }
        let bytes = match std::fs::read(entry.path()) {
            Ok(bytes) => bytes,
            Err(error) if entry.depth() == 0 => return Err(format!("{}: {error}", root.display())),
            Err(_) => continue,
        };
        let Some((text, encoding)) = readable_file_text(&bytes) else {
            if entry.depth() == 0 {
                return Err(format!("{} is binary: search_files reads text", root.display()));
            }
            continue;
        };
        let text = if encoding == "utf-8" { text.strip_prefix('\u{feff}').unwrap_or(&text) } else { &text };
        let name = entry.path().strip_prefix(workspace).unwrap_or(entry.path()).to_string_lossy().replace('\\', "/");
        for (number, line) in text.lines().enumerate() {
            // A single large log can take the entire budget. Avoid reading the clock on every short line.
            if number % 256 == 0 && time_is_up()? {
                stopped = "the time ran out";
                break 'files;
            }
            if let Some(found) = regex.find(line) {
                let shown = search_excerpt(line, found);
                out.push_str(&format!("{name}:{}:{shown}\n", number + 1));
                matches += 1;
                if matches >= MATCH_LIMIT {
                    stopped = "the match limit was reached";
                    break 'files;
                }
            }
        }
    }
    if time_is_up()? && stopped.is_empty() {
        stopped = "the time ran out";
    }
    if matches == 0 {
        out.push_str("no matches\n");
    }
    if secrets > 0 {
        out.push_str(&format!("--- {secrets} files that may hold secrets were not searched: read_file asks the user for them ---\n"));
    }
    if !stopped.is_empty() {
        out.push_str(&format!("--- stopped: {stopped}; {matches} matches so far, narrow the path or pattern ---\n"));
    }
    // (more than is shown: all of it in a file, as a long command's output is)
    if out.len() > MAX_OUTPUT {
        if let Some(file) = keep_whole_as("search", &out) {
            return Ok(format!("{}\n--- all {matches} matches are in {}: read the part you need there, or narrow the pattern ---\n", cap(&out, MAX_OUTPUT), file.display()));
        }
    }
    Ok(cap(&out, MAX_OUTPUT))
}

/// Show the start of a matching line when the hit fits there, otherwise show the hit in context.
fn search_excerpt(line: &str, found: regex::Match<'_>) -> String {
    const LIMIT: usize = 300;
    let first_end = line.char_indices().nth(LIMIT).map_or(line.len(), |(at, _)| at);
    if first_end == line.len() {
        return line.to_owned();
    }
    if found.end() <= first_end {
        return format!("{}…", &line[..first_end]);
    }
    let match_len = found.as_str().chars().take(LIMIT).count();
    let before = ((LIMIT - match_len) / 2).min(40);
    let start = line[..found.start()].chars().rev().take(before).fold(found.start(), |at, ch| at - ch.len_utf8());
    let end = line[start..].char_indices().nth(LIMIT).map_or(line.len(), |(at, _)| start + at);
    let suffix = if end < line.len() { "…" } else { "" };
    let prefix = if start > 0 { "…" } else { "" };
    format!("{prefix}{}{suffix}", &line[start..end])
}

/// The workspace's files, as `@` offers them: what git ignores is left out.
pub fn workspace_files(workspace: &Path, limit: usize) -> Vec<String> {
    let mut names: Vec<String> = walker(workspace, true).build().flatten()
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .take(limit)
        .map(|entry| entry.path().strip_prefix(workspace).unwrap_or(entry.path()).to_string_lossy().replace('\\', "/"))
        .collect();
    names.sort();
    names
}

/// The branch checked out, if this is a git repository.
pub fn git_branch(workspace: &Path) -> Option<String> {
    git(workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).ok().map(|name| name.trim().to_string()).filter(|name| !name.is_empty())
}

/// What the panel on the right says of a repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitInfo {
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub staged: u32,
    pub changed: u32,
    pub untracked: u32,
    pub conflicted: u32,
    pub last_commit: Option<String>,
}

/// None outside a repository.
pub fn git_info(workspace: &Path) -> Option<GitInfo> {
    // (the files under the workspace: in a folder of a larger repository, that repository's other changes are not counted as this one's)
    let status = git(workspace, &["-c", "core.quotepath=off", "status", "--porcelain=v2", "--branch", "--", "."]).ok()?;
    let mut info = GitInfo::default();
    let mut oid = "";
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("# branch.oid ") {
            oid = rest;
        } else if let Some(rest) = line.strip_prefix("# branch.head ") {
            info.branch = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("# branch.upstream ") {
            info.upstream = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            let mut parts = rest.split_whitespace().map(|part| part.trim_start_matches(['+', '-']).parse().unwrap_or(0));
            info.ahead = parts.next().unwrap_or(0);
            info.behind = parts.next().unwrap_or(0);
        } else if line.starts_with("? ") {
            info.untracked += 1;
        } else if line.starts_with("u ") {
            info.conflicted += 1;
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let mut letters = line[2..].chars();
            info.staged += u32::from(letters.next().is_some_and(|letter| letter != '.'));
            info.changed += u32::from(letters.next().is_some_and(|letter| letter != '.'));
        }
    }
    match info.branch.as_str() {
        "(detached)" => info.branch = format!("detached at {}", oid.chars().take(8).collect::<String>()),
        "(initial)" => info.branch = "no commit yet".to_string(),
        _ => {}
    }
    info.last_commit = git(workspace, &["log", "-1", "--format=%s  ·  %cr"]).ok().map(|line| line.trim().to_string()).filter(|line| !line.is_empty());
    Some(info)
}

/// The top of the git repository that the workspace is a folder of, when that is above the workspace. The app's git runs outside the sandbox and finds it; a command in the sandbox cannot reach that
/// repository's `.git` (the person chose the folder, and what is above it stays closed), and fails. None when the workspace is the top itself, or is in no repository.
pub fn repository_above(workspace: &Path) -> Option<PathBuf> {
    let top = git(workspace, &["rev-parse", "--show-toplevel"]).ok()?;
    let (top, here) = (std::fs::canonicalize(top.trim()).ok()?, std::fs::canonicalize(workspace).ok()?);
    (top != here && here.starts_with(&top)).then(|| PathBuf::from(top.display().to_string().trim_start_matches("\\\\?\\")))
}

/// What a model is told, after where the repository is, when the workspace is a folder of one above it: what fails, what works and what to do about the rest.
pub(crate) fn git_above_words(top: &Path) -> String {
    format!(
        "git commands in `execute_command` cannot reach that repository's `.git` from the sandbox, so they fail (\"not a git repository\", \"error reading '.../.git'\") whatever is tried: `cd ..` and `git -C` fail the same, so do not try them. \
         `show_diff` with scope `workspace`, `staged` or `unstaged` works here, and shows this folder's changes. For the history, a commit or anything else of git, give the user the command to run themselves, as `git -C \"{}\" log -5`.",
        top.display()
    )
}

/// One file's changes, as the diff panel shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub diff: String,
}

impl FileChange {
    /// From the text before (none: a new file) to the text now (none: it was deleted).
    pub fn between(path: &str, before: Option<&str>, after: Option<&str>) -> FileChange {
        let diff = similar::TextDiff::from_lines(before.unwrap_or(""), after.unwrap_or("")).unified_diff().context_radius(3).to_string();
        FileChange::counted(path.to_string(), diff)
    }

    /// A file that changed, and of which no text is shown: it is binary, or too large to be read for it.
    pub fn without_diff(path: &str) -> FileChange {
        FileChange { path: path.to_string(), added: 0, removed: 0, diff: String::new() }
    }

    fn counted(path: String, diff: String) -> FileChange {
        let changed = |mark: char, header: &str| diff.lines().filter(|line| line.starts_with(mark) && !line.starts_with(header)).count();
        FileChange { added: changed('+', "+++"), removed: changed('-', "---"), path, diff }
    }
}

const MAX_UNTRACKED_SHOWN: usize = 200;
const MAX_UNTRACKED_BYTES: u64 = 256 * 1024;

/// Before the first commit there is no HEAD. Hash empty input (git_raw closes
/// stdin) so Git supplies the empty tree ID for this repository's object format.
fn workspace_diff_base(workspace: &Path) -> Result<String, String> {
    git(workspace, &["rev-parse", "--verify", "-q", "HEAD"])
        .or_else(|_| git(workspace, &["hash-object", "-t", "tree", "--stdin"]))
}

/// What git says differs from the last commit under the workspace, new files included. An error: this is no repository.
pub fn workspace_changes(workspace: &Path) -> Result<Vec<FileChange>, String> {
    git(workspace, &["rev-parse", "--is-inside-work-tree"])?;
    let base = workspace_diff_base(workspace)?;
    let text = git(workspace, &["-c", "core.quotepath=off", "diff", "--no-ext-diff", "--no-color", "--relative", base.trim(), "--"])?;
    let mut changes = split_git_diff(&text);
    changes.extend(untracked_changes(workspace, &[]));
    Ok(changes)
}

/// The new files git knows of and does not track, under these paths or all. What it ignores is left out.
pub(crate) fn untracked_names(workspace: &Path, paths: &[&str]) -> Vec<String> {
    let mut args = vec!["--no-optional-locks", "-c", "core.quotepath=off", "ls-files", "--others", "--exclude-standard"];
    if !paths.is_empty() {
        args.push("--");
        args.extend(paths);
    }
    git(workspace, &args).map_or(Vec::new(), |names| names.lines().map(str::to_string).collect())
}

fn untracked_changes(workspace: &Path, paths: &[&str]) -> Vec<FileChange> {
    untracked_names(workspace, paths).iter().take(MAX_UNTRACKED_SHOWN).map(|name| {
        let path = workspace.join(name);
        let small = std::fs::metadata(&path).is_ok_and(|meta| meta.is_file() && meta.len() <= MAX_UNTRACKED_BYTES);
        let content = if small { std::fs::read(&path).ok().and_then(|bytes| String::from_utf8(bytes).ok()) } else { None };
        match content {
            Some(content) => FileChange::between(name, None, Some(&content)),
            None => FileChange { path: name.to_string(), added: 0, removed: 0, diff: "A new file: too large, or not text.\n".to_string() },
        }
    }).collect()
}

fn split_git_diff(text: &str) -> Vec<FileChange> {
    let mut changes = Vec::new();
    let mut flush = |block: &mut Vec<&str>| {
        if block.is_empty() {
            return;
        }
        let named = |mark: &str| block.iter().find_map(|line| line.strip_prefix(mark)).map(str::to_string);
        let path = named("rename to ").or_else(|| named("+++ b/")).or_else(|| named("--- a/"))
            .or_else(|| block[0].rsplit_once(" b/").map(|(_, name)| name.to_string())).unwrap_or_default();
        // A person reads the hunks: the blob names say nothing to them.
        let diff: String = block.iter().skip(1).filter(|line| !line.starts_with("index ")).flat_map(|line| [*line, "\n"]).collect();
        changes.push(FileChange::counted(path, diff));
        block.clear();
    };
    let mut block: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            flush(&mut block);
        }
        block.push(line);
    }
    flush(&mut block);
    changes
}

pub(crate) fn git(workspace: &Path, args: &[&str]) -> Result<String, String> {
    git_raw(workspace, args).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// What the app's own git must not do. It runs outside the sandbox, on a `.git` that a sandboxed command can write, and a repository's own config can name programs: `core.fsmonitor`
/// runs one at a plain `git status`, and a `filter` at a plain `git diff`. So what the app asks of git never runs them, whoever planted them. Filters go by the names this repository's
/// config gives them; a person's own in their global config are theirs, and stay.
fn git_without_programs(workspace: &Path) -> Vec<String> {
    let mut settings: Vec<String> = ["core.fsmonitor=false", "core.hooksPath=.no-hooks-here", "diff.external=", "log.showSignature=false"].iter().map(|setting| setting.to_string()).collect();
    // (and no status of a submodule: git starts another git in it, on a config of its own that is not looked at here)
    settings.push("diff.ignoreSubmodules=all".to_string());
    // The worktree's config too, which `--local` leaves out and which a repository can turn on. (An older git has no `--show-scope`.)
    let ask = |words: &[&str]| {
        let mut command = Command::new(crate::bundled::git_program());
        command.args(["config", "--includes", "--name-only", "--list"]).args(words).current_dir(workspace).stdin(Stdio::null()).stderr(Stdio::null());
        no_window(&mut command);
        command.output().ok().filter(|output| output.status.success())
    };
    let scoped = ask(&["--show-scope"]);
    let told = scoped.as_ref().map_or_else(|| ask(&["--local"]).map_or(String::new(), |output| String::from_utf8_lossy(&output.stdout).into_owned()), |output| {
        String::from_utf8_lossy(&output.stdout).lines().filter_map(|line| line.strip_prefix("local\t").or_else(|| line.strip_prefix("worktree\t"))).collect::<Vec<_>>().join("\n")
    });
    {
        for name in told.lines() {
            let lower = name.to_ascii_lowercase();
            if lower.starts_with("filter.") && [".clean", ".smudge", ".process"].iter().any(|end| lower.ends_with(end)) {
                settings.push(format!("{name}="));
            }
            // (empty, it is a program git cannot start and the diff fails; a driver's `command` is left out by `--no-ext-diff` on every diff here)
            if lower.starts_with("diff.") && lower.ends_with(".textconv") {
                settings.push(format!("{name}=cat"));
            }
        }
    }
    settings
}

/// What git prints, to the byte: a file in it may not be UTF-8.
pub(crate) fn git_raw(workspace: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut command = Command::new(crate::bundled::git_program());
    for setting in git_without_programs(workspace) {
        command.arg("-c").arg(setting);
    }
    command.args(args).current_dir(workspace).stdin(Stdio::null());
    no_window(&mut command);
    let output = command.output().map_err(|error| format!("git: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(output.stdout)
}

fn show_diff(arguments: &Value, workspace: &Path, baseline: Option<&crate::baseline::Baseline>) -> Result<String, String> {
    let scope = arguments.get("scope").and_then(Value::as_str).unwrap_or("workspace");
    let paths: Vec<&str> = arguments.get("paths").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let repository = git(workspace, &["rev-parse", "--is-inside-work-tree"]).is_ok();
    if !repository && workspace.join(".git").exists() && matches!(scope, "staged" | "unstaged") {
        return Err(format!("scope {scope} needs git, and the app could not run git on this folder's .git: use scope session"));
    }
    // Git run outside a repository answers with its usage, which tells a model nothing: no git diff goes there.
    if !repository && matches!(scope, "staged" | "unstaged") {
        return Err(format!("scope {scope} needs a git repository, and this folder is not one: use scope session"));
    }
    let head = baseline.and_then(|baseline| baseline.head.as_deref());
    if scope == "session" && !repository && head.is_some() {
        return Err("this session began in a git repository that is no longer here: read the files themselves".to_string());
    }
    // Where the session began, in a repository too: what was already uncommitted then is not its work, and a commit since is.
    if scope == "session" || !repository {
        let baseline = baseline.ok_or("this session has nothing to compare with yet: read the files themselves")?;
        let changes = baseline.changes(workspace, &paths);
        return Ok(if changes.is_empty() { format!("no differences ({scope}): nothing has changed since this session began") } else { spill("diff", &changes, MAX_OUTPUT) });
    }
    // (only what is under the workspace: a workspace that is a folder of a larger repository saw that repository's every change, which is
    // not its work and may be what its owner never meant to show)
    let mut args = vec!["diff", "--no-ext-diff", "--no-color", "--relative"];
    let base;
    match scope {
        "staged" => args.push("--cached"),
        "unstaged" => {}
        _ => {
            base = workspace_diff_base(workspace)?;
            args.push(base.trim());
        }
    }
    args.push("--");
    args.extend(paths.iter().copied());
    let mut diff = git(workspace, &args).map_err(|error| error.lines().next().unwrap_or("git could not show this").to_string())?;
    // (a new file is a change to a workspace, whatever git diff leaves out: creating only new files must not read as nothing done)
    if scope == "workspace" {
        for change in untracked_changes(workspace, &paths) {
            diff.push_str(&format!("--- new file {} ---\n{}\n", change.path, change.diff));
        }
    }
    Ok(if diff.is_empty() { format!("no differences ({scope})") } else { spill("diff", &diff, MAX_OUTPUT) })
}

#[cfg(windows)]
pub(crate) fn no_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
}

#[cfg(not(windows))]
pub(crate) fn no_window(_command: &mut Command) {}

#[cfg(not(windows))]
fn shell(line: &str) -> Command {
    let mut command = Command::new("sh");
    command.args(["-c", line]);
    command
}

#[cfg(windows)]
pub(crate) fn kill_tree(pid: u32) {
    let mut command = Command::new("taskkill");
    command.args(["/T", "/F", "/PID", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null());
    no_window(&mut command);
    let _ = command.status();
}

#[cfg(not(windows))]
pub(crate) fn kill_tree(pid: u32) {
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
}

/// What is in a folder by size and time, for saying what a command changed. None when there is too much to look through without keeping the command waiting.
#[cfg(windows)]
fn file_states(root: &Path) -> Option<BTreeMap<String, (u64, Option<std::time::SystemTime>)>> {
    const LOOKING: Duration = Duration::from_millis(250);
    const MOST_FILES: usize = 20_000;
    let started = Instant::now();
    let mut states = BTreeMap::new();
    for entry in walker(root, true).build().flatten() {
        if states.len() > MOST_FILES || started.elapsed() > LOOKING {
            return None;
        }
        if entry.file_type().is_some_and(|kind| kind.is_file()) {
            let data = entry.metadata().ok();
            let name = entry.path().strip_prefix(root).unwrap_or(entry.path()).to_string_lossy().replace('\\', "/");
            states.insert(name, (data.as_ref().map_or(0, std::fs::Metadata::len), data.and_then(|data| data.modified().ok())));
        }
    }
    Some(states)
}

/// How a command's result begins to say what it changed in the workspace. The agent takes a result that has it for a write, as far as the calls that look at files are concerned.
pub(crate) const CHANGED_BY_COMMAND: &str = "files changed by this command:\n";

/// What a command did, whatever its exit code says: a last step that failed has often been taken for all of it.
#[cfg(windows)]
fn described_changes(before: &BTreeMap<String, (u64, Option<std::time::SystemTime>)>, after: &BTreeMap<String, (u64, Option<std::time::SystemTime>)>) -> String {
    const SHOWN: usize = 20;
    let mut lines: Vec<String> = Vec::new();
    for (name, state) in after {
        match before.get(name) {
            None => lines.push(format!("- created {name} ({} bytes)", state.0)),
            Some(known) if known != state => lines.push(format!("- changed {name} ({} bytes)", state.0)),
            _ => {}
        }
    }
    lines.extend(before.keys().filter(|name| !after.contains_key(*name)).map(|name| format!("- deleted {name}")));
    if lines.is_empty() {
        return String::new();
    }
    let more = lines.len().saturating_sub(SHOWN);
    lines.truncate(SHOWN);
    let tail = if more > 0 { format!("\n- and {more} more") } else { String::new() };
    format!("{CHANGED_BY_COMMAND}{}{tail}\n", lines.join("\n"))
}

/// A command with no network that failed at reaching it says so where it is read first, as what it prints is a long way to get to one line.
#[cfg(windows)]
fn no_network_note(network: bool, status: Option<i32>, out: &str, err: &str) -> Option<&'static str> {
    const SIGNS: [&str; 14] = ["getaddrinfo", "name or service not known", "could not resolve", "name resolution", "enotfound", "eai_again", "network is unreachable", "no such host", "forbidden by its access permissions", "10013", "11001", "名前解決", "リモート名を解決できません", "アクセス許可で禁じられた"];
    if network || status == Some(0) {
        return None;
    }
    let (out, err) = (out.to_lowercase(), err.to_lowercase());
    SIGNS.iter().any(|sign| out.contains(sign) || err.contains(sign)).then_some("note: this command failed because it ran with no network, and nothing about the site or the code is known from it. If it needs the network, make the same call with network=true, and the user is asked to approve it.\n")
}

/// Whether `dir` or a command like it in this line was refused, as either stream says: a line that sends its errors on with `2>&1` says it with its output.
#[cfg(windows)]
fn refused_for_the_drive_root(line: &str, out: &str, err: &str, system_words: &str) -> bool {
    needs_drive_root(line) && (says_access_denied(err, system_words) || says_access_denied(out, system_words))
}

/// Whether a part of this command line is one of cmd's that fail in the sandbox wherever their file is.
#[cfg(windows)]
fn needs_drive_root(line: &str) -> bool {
    line.split(['&', '|', '(', ')']).any(|part| {
        let mut words = part.split_whitespace();
        let first = words.next().unwrap_or("").trim_start_matches('@').to_ascii_lowercase();
        let first = first.split(['/', '>', '<']).next().unwrap_or("");
        matches!(first, "dir" | "del" | "erase" | "vol" | "tree" | "attrib" | "xcopy" | "robocopy") || (first == "where" && part.to_ascii_lowercase().contains("/r"))
    })
}

/// A line's commands: split where cmd would split them, and not inside double quotes or after its escape, where `&` and `|` are only letters.
#[cfg(windows)]
fn commands_of(line: &str) -> Vec<&str> {
    let (mut parts, mut start, mut quoted, mut escaped) = (Vec::new(), 0, false, false);
    for (at, c) in line.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '"' {
            quoted = !quoted;
        } else if c == '^' && !quoted {
            escaped = true;
        } else if !quoted && matches!(c, '&' | '|' | '(' | ')' | '\n' | '\r') {
            parts.push(&line[start..at]);
            start = at + c.len_utf8();
        }
    }
    parts.push(&line[start..]);
    parts
}

/// A command's first word, its quotes off, and what follows it.
#[cfg(windows)]
fn first_word(text: &str) -> (&str, &str) {
    let text = text.trim_start();
    if let Some(rest) = text.strip_prefix('"') {
        return rest.split_once('"').unwrap_or((rest, ""));
    }
    text.split_at(text.find([' ', '\t', '>', '<']).unwrap_or(text.len()))
}

/// A command's words, with their double quotes off. None where the line is more than plain words: what cmd makes of those is not for this to guess.
#[cfg(windows)]
fn plain_words(text: &str) -> Option<Vec<String>> {
    if text.contains(['%', '^', '!', '<', '`']) {
        return None;
    }
    let (mut words, mut word, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
    for c in text.chars() {
        if c == '"' {
            quoted = !quoted;
            any = true;
        } else if c.is_whitespace() && !quoted {
            if any {
                words.push(std::mem::take(&mut word));
                any = false;
            }
        } else {
            word.push(c);
            any = true;
        }
    }
    if quoted {
        return None;
    }
    if any {
        words.push(word);
    }
    // "2>nul" says nothing here; any other redirection is the line's own business.
    words.retain(|word| !word.eq_ignore_ascii_case("2>nul"));
    (!words.iter().any(|word| word.contains('>'))).then_some(words)
}

/// `plain_words`, with a `for`'s own variables (`%F`, `%~nxF`, of these letters) taken as part of a word: cmd puts the name in their place before the command runs.
#[cfg(windows)]
fn plain_words_with(text: &str, variables: &[char]) -> Option<Vec<String>> {
    let chars: Vec<char> = text.chars().collect();
    let (mut masked, mut kept, mut at) = (String::new(), Vec::<String>::new(), 0);
    while at < chars.len() {
        if chars[at] == '%' {
            let mut end = at + 1;
            let found = if chars.get(end) == Some(&'~') {
                end += 1;
                while chars.get(end).is_some_and(char::is_ascii_alphabetic) {
                    end += 1;
                }
                // (the variable is the last letter, the modifiers the ones before it)
                end > at + 2 && variables.contains(&chars[end - 1]) && chars[at + 2..end - 1].iter().all(|c| "fdpnxsatz".contains(c.to_ascii_lowercase()))
            } else {
                end += 1;
                chars.get(at + 1).is_some_and(|c| variables.contains(c))
            };
            if found {
                masked.push(char::from_u32(0xE000 + kept.len() as u32)?);
                kept.push(chars[at..end].iter().collect());
                at = end;
                continue;
            }
        }
        masked.push(chars[at]);
        at += 1;
    }
    let words = plain_words(&masked)?;
    Some(words.into_iter().map(|word| word.chars().map(|c| match (c as u32).checked_sub(0xE000).and_then(|index| kept.get(index as usize)) {
        Some(text) => text.clone(),
        None => c.to_string(),
    }).collect()).collect())
}

/// A line with parentheses (an `if`, a `for`, a group of commands) has a stand-in only for its `dir`s that cmd runs as commands of their own: a step of the line
/// outside every parenthesis (a `for`'s body is one: it runs to the end of the line), and the command a `for /f` reads (`in ('dir /b /s crates\*.rs')`). A `for`'s
/// variables are words there (`dir "%F"`), and the stand-in's own loop takes a letter none of the line's `for`s has. What is inside parentheses, and a `del` (which
/// runs in the sandbox as it is, saying less), stay as they were written. (Such a line was left whole, and a raw `dir` reads the drive's root, which a container
/// cannot: `dir /b *.html && for %f in (*.html) do @echo %~zf`, `for /f %f in ('dir /b /s crates\*.rs') do …` and `for %F in (a b) do … & dir "%F" | find "html"`
/// ended "Access is denied", 4 of the 64 lines with a `dir` among other commands in 194 sessions; the others had no parentheses and ran.)
#[cfg(windows)]
fn dir_stand_ins_around_parentheses(line: &str) -> String {
    static FOR_VARIABLE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = FOR_VARIABLE.get_or_init(|| regex::Regex::new(r"(?i)\bfor\b[^%]*?%%?([a-z])\s+in\s*\(").expect("the pattern is valid"));
    let variables: Vec<char> = pattern.captures_iter(line).filter_map(|found| found[1].chars().next()).collect();
    let Some(own) = ['d', 'e', 'g', 'h', 'j', 'k', 'q', 'v', 'w', 'y'].into_iter().find(|letter| !variables.iter().any(|v| v.eq_ignore_ascii_case(letter))) else {
        return line.to_string();
    };
    // (what a `dir` lists, as its stand-in has it)
    let dir_words = |step: &str| -> Option<Vec<String>> {
        let bare = step.trim_start().trim_start_matches('@');
        let word = bare.split_whitespace().next()?;
        let (name, glued) = word.split_once('/').map_or((word, None), |(name, rest)| (name, Some(rest)));
        if !name.eq_ignore_ascii_case("dir") {
            return None;
        }
        let mut words = plain_words_with(&bare[word.len()..], &variables)?;
        words.extend(glued.into_iter().flat_map(|rest| rest.split('/')).map(|switch| format!("/{switch}")));
        Some(words)
    };
    let dir = |step: &str, piped: bool| -> Option<String> {
        let words = dir_words(step)?;
        Some(format!("{}({})", &step[..step.len() - step.trim_start().len()], dir_stand_in_as(&words, piped, own)))
    };
    // (a `for`'s own `dir`s: the command a `for /f` reads, between `in ('` and `')`, and its body, after `) do `)
    let in_a_for = |step: &str, piped: bool| -> Option<String> {
        let lower = step.to_ascii_lowercase();
        let head = lower.trim_start();
        if !head.starts_with("for") {
            return None;
        }
        let mut text = step.to_string();
        if head.starts_with("for /f") || head.starts_with("for/f") {
            let open = lower.find("in ('").map(|at| at + "in ('".len());
            let close = open.and_then(|open| lower[open..].find("')").map(|at| open + at));
            if let (Some(open), Some(close)) = (open, close) {
                // (in the command a `for /f` reads, cmd takes a parenthesis, `&` or `|` for its own unless it has a `^`: the stand-in's `(*.txt)` ended the `in (`, and
                // the rest was looked for as a file)
                if let Some(words) = dir_words(&step[open..close]) {
                    let inner: String = dir_stand_in_as(&words, false, own).chars().flat_map(|c| ["^".chars().filter(|_| "()&|<>".contains(c)).collect::<String>(), c.to_string()]).collect();
                    text = format!("{}{inner}{}", &step[..open], &step[close..]);
                }
            }
        }
        // (the last `) do `: the command read above may have one of its own)
        if let Some(body) = text.to_ascii_lowercase().rfind(") do ").map(|at| at + ") do ".len()) {
            if let Some(stand_in) = dir(&text[body..], piped) {
                text = format!("{}{stand_in}", &text[..body]);
            }
        }
        (text != step).then_some(text)
    };
    let (mut out, mut start, mut quoted, mut depth, mut changed) = (String::new(), 0, false, 0usize, false);
    let mut step = |text: &str, piped: bool, out: &mut String| match dir(text, piped).or_else(|| in_a_for(text, piped)) {
        Some(stand_in) => {
            changed = true;
            out.push_str(&stand_in);
        }
        None => out.push_str(text),
    };
    for (at, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => match depth.checked_sub(1) {
                Some(less) => depth = less,
                None => return line.to_string(),
            },
            '&' | '|' if !quoted && depth == 0 => {
                let piped = c == '|' && !line[at + 1..].starts_with('|') && !line[..at].ends_with('|');
                step(&line[start..at], piped, &mut out);
                out.push(c);
                start = at + 1;
            }
            _ => {}
        }
    }
    if quoted || depth != 0 {
        return line.to_string();
    }
    step(&line[start..], false, &mut out);
    if changed { out } else { line.to_string() }
}

/// What `dir` is asked for, in what runs in the sandbox: it lists a folder, a file or a pattern as `dir` does, in fewer words. `piped`: cmd reads what comes before a `|` again, and an `if` with parentheses does not survive that.
#[cfg(windows)]
fn dir_stand_in(words: &[String], piped: bool) -> String {
    dir_stand_in_as(words, piped, 'd')
}

/// `dir_stand_in`, its own loop's variable `%v`: in a `for`'s body that is `%d` itself, a loop of `%d` in it would be the outer one's.
#[cfg(windows)]
fn dir_stand_in_as(words: &[String], piped: bool, v: char) -> String {
    let recursive = words.iter().any(|word| word.eq_ignore_ascii_case("/s"));
    let mut places: Vec<String> = words.iter().filter(|word| !word.starts_with('/')).map(|word| word.trim_end_matches('\\').to_string()).collect();
    if places.is_empty() {
        places.push(".".to_string());
    }
    let one = |place: &String| {
        if recursive {
            let (folder, mask) = if place.contains(['*', '?']) { place.rsplit_once('\\').unwrap_or((".", place.as_str())) } else { (place.as_str(), "*") };
            return format!("for /r \"{folder}\" %{v} in ({mask}) do @echo %{v}");
        }
        if piped {
            return format!("(for /d %{v} in (\"{place}\\*\") do @echo %~nx{v}\\) & (for %{v} in (\"{place}\\*\") do @echo %~nx{v}  %~z{v})");
        }
        format!("if exist \"{place}\\*\" ((for /d %{v} in (\"{place}\\*\") do @echo %~nx{v}\\) & (for %{v} in (\"{place}\\*\") do @echo %~nx{v}  %~z{v})) else if exist \"{place}\" (for %{v} in (\"{place}\") do @echo %~nx{v}  %~z{v}) else (echo File Not Found 1>&2)")
    };
    places.iter().map(one).collect::<Vec<_>>().join(" & ")
}

/// What `del` is asked for, in what runs in the sandbox. None for what asks more of it than deleting these files.
#[cfg(windows)]
fn del_stand_in(words: &[String]) -> Option<String> {
    if !words.iter().filter(|word| word.starts_with('/')).all(|word| word.eq_ignore_ascii_case("/q") || word.eq_ignore_ascii_case("/f")) {
        return None;
    }
    let files: Vec<&String> = words.iter().filter(|word| !word.starts_with('/')).collect();
    if files.is_empty() || files.iter().any(|file| file.contains('\'')) {
        return None;
    }
    // (`[IO.File]::Delete` says nothing of a file that is not there, and a pattern that matches nothing runs no delete: each says so, as `del` does, or a delete outside the
    // workspace, where the container sees no file, and one of a misspelt name, were told to the model as done. PowerShell started in the container says on its error output that
    // it could not set up the drives, each time, in the system's language: that line is none of the delete's, and was taken for a failed one; what the delete has to say goes to
    // the output instead.)
    let missing = |file: &str| format!("[Console]::Out.WriteLine('Could Not Find {file}'); exit 1");
    let delete = |file: &str| {
        format!(
            "powershell -NoProfile -NonInteractive -Command \"if (-not ([IO.File]::Exists('{file}') -or [IO.Directory]::Exists('{file}'))) {{ {} }} try {{ [IO.File]::Delete('{file}') }} catch {{ [Console]::Out.WriteLine($_.Exception.GetBaseException().Message); exit 1 }}\" 2>nul",
            missing(file)
        )
    };
    let matched = |file: &str| format!("if exist \"{file}\" (for %d in (\"{file}\") do @{}) else (powershell -NoProfile -NonInteractive -Command \"{}\" 2>nul)", delete("%d"), missing(file));
    Some(files.iter().map(|file| if file.contains(['*', '?']) { matched(file) } else { delete(file) }).collect::<Vec<_>>().join(" & "))
}

/// One step of a line, with `dir` or `del` in the forms that say plainly what they want replaced by what does that.
#[cfg(windows)]
fn step_stand_in(step: &str, piped: bool) -> Option<String> {
    let bare = step.trim_start().trim_start_matches('@');
    let word = bare.split_whitespace().next().unwrap_or("");
    let (name, glued) = word.split_once('/').map_or((word, None), |(name, rest)| (name, Some(rest)));
    let name = name.to_ascii_lowercase();
    if glued.is_none() {
        if let Some(rest) = with_backslashes(&name, &bare[word.len()..]) {
            return Some(format!("{}{word}{rest}", &step[..step.len() - bare.len()]));
        }
    }
    if !matches!(name.as_str(), "dir" | "del" | "erase") {
        return None;
    }
    let mut words = plain_words(&bare[word.len()..])?;
    words.extend(glued.into_iter().flat_map(|rest| rest.split('/')).map(|switch| format!("/{switch}")));
    let text = if name == "dir" { dir_stand_in(&words, piped) } else { del_stand_in(&words)? };
    Some(format!("{}({text})", &step[..step.len() - step.trim_start().len()]))
}

/// The words after a command's name, for `type`, `findstr`, `copy`, `move`, `mkdir` and `md`, with a path's `/` written `\` and `mkdir`'s `-p` left out; None where nothing changes.
/// cmd takes a `/` for the start of a switch: `findstr /n x gacha/three.module.js` said "Cannot open three.module.js", and `type`, `copy` and `mkdir` fail the same way. And
/// `mkdir -p gacha`, a habit from elsewhere, made a folder named `-p` in the person's workspace beside `gacha` (cmd's `mkdir` makes the folders between anyway). A real
/// turn lost rounds to both, and a model went on to `pause` and `echo noop` with them. Switches, addresses, what `findstr` looks for and lines with more than plain words are left.
#[cfg(windows)]
fn with_backslashes(name: &str, rest: &str) -> Option<String> {
    if !matches!(name, "type" | "findstr" | "copy" | "move" | "mkdir" | "md") || rest.contains(['%', '^', '!', '<', '>', '`']) {
        return None;
    }
    // (each word where it is, its quotes and all)
    let (mut words, mut began, mut quoted) = (Vec::new(), None, false);
    for (at, c) in rest.char_indices() {
        if c == '"' {
            quoted = !quoted;
        }
        if c.is_whitespace() && !quoted {
            if let Some(start) = began.take() {
                words.push(start..at);
            }
        } else if began.is_none() {
            began = Some(at);
        }
    }
    if quoted {
        return None;
    }
    words.extend(began.map(|start| start..rest.len()));
    let read = |at: &std::ops::Range<usize>| rest[at.clone()].replace('"', "");
    // (`findstr`'s first word that is not a switch is what it looks for, unless a switch gives that)
    let looked_for = (name == "findstr" && !words.iter().any(|at| read(at).get(..3).is_some_and(|start| start.eq_ignore_ascii_case("/c:") || start.eq_ignore_ascii_case("/g:"))))
        .then(|| words.iter().position(|at| !read(at).starts_with('/')))
        .flatten();
    let (mut out, mut last, mut changed) = (String::new(), 0, false);
    for (index, at) in words.iter().enumerate() {
        let word = read(at);
        if matches!(name, "mkdir" | "md") && matches!(word.as_str(), "-p" | "--parents") {
            (last, changed) = (at.end, true);
            continue;
        }
        out.push_str(&rest[last..at.start]);
        last = at.end;
        if !word.starts_with(['/', '-']) && word.contains('/') && !word.contains("://") && Some(index) != looked_for {
            out.push_str(&rest[at.clone()].replace('/', "\\"));
            changed = true;
        } else {
            out.push_str(&rest[at.clone()]);
        }
    }
    out.push_str(&rest[last..]);
    changed.then_some(out)
}

/// The line a command of the sandbox runs, from the line as written and where it runs: [`line_that_runs`], and its PowerShell put where it was started
/// ([`powershell_in_place`]). What the person is asked about and shown is this line, as it is what runs.
pub fn line_that_runs_at(line: &str, arguments: &Value, workspace: &Path) -> String {
    let cwd = resolve(workspace, arguments.get("cwd").and_then(Value::as_str).unwrap_or("."));
    in_sandbox_form(line, &cwd)
}

/// What the sandbox is given for a line run in `cwd`: its stand-ins, PowerShell put in the folder, a `git init` made whole and a venv made in two steps.
fn in_sandbox_form(line: &str, cwd: &Path) -> String {
    let users = users_folder();
    let made = powershell_in_place(&line_that_runs(line), cwd, users.as_deref());
    let made = git_init_whole(&made, cwd, users.as_deref(), crate::bundled::default_branch);
    venv_in_two_steps(&made, bundled_python().as_deref())
}

/// What a `git init` made whole has in it, by which a line already made so is known.
const GIT_HEAD_WRITTEN: &str = "HEAD echo ref: refs/heads/";

/// A line whose `git init` is made whole in a folder under the users' folder. A container cannot look at C:\Users, and git, which writes a ref by the
/// repository's full path and looks at each folder of it on the way, cannot lock HEAD there ("cannot lock ref 'HEAD'"): `git init` stops with no HEAD and no
/// objects folder, and nothing after it finds a repository. So where it failed so, the line writes HEAD itself, on the branch named or else the person's
/// own default, and runs `git init` again, which finds HEAD there and makes the rest. One relative folder and plain words only; a bare repository, one kept
/// elsewhere, a reftable one and every other form are left as they are.
fn git_init_whole(line: &str, cwd: &Path, users: Option<&Path>, default_branch: impl FnOnce() -> &'static str) -> String {
    if !under_users(cwd, users) || line.contains(GIT_HEAD_WRITTEN) {
        return line.to_string();
    }
    let pattern = regex::Regex::new(r"(?i)(^|[&|(]\s*)(git(?:\.exe)?\s+init)((?:\s+[\w.\-/\\=]+)*?)\s*($|&&|&|\|\||\))").expect("the pattern is valid");
    let mut default_branch = Some(default_branch);
    let mut default = None;
    pattern.replace_all(line, |found: &regex::Captures| {
        let words: Vec<&str> = found[3].split_whitespace().collect();
        let (mut folder, mut named, mut again) = (None, None, vec!["-q"]);
        let mut at = 0;
        while at < words.len() {
            let (word, lower) = (words[at], words[at].to_ascii_lowercase());
            if lower == "--bare" || lower.starts_with("--separate-git-dir") || lower.starts_with("--ref-format") {
                return found[0].to_string();
            } else if lower == "-b" || lower == "--initial-branch" {
                named = words.get(at + 1).copied();
                at += 1;
            } else if lower.starts_with("--initial-branch=") {
                named = Some(&word["--initial-branch=".len()..]);
            } else if lower == "--template" || lower == "--object-format" {
                again.extend(words.get(at..at + 2).unwrap_or(&[]));
                at += 1;
            } else if lower == "-q" || lower == "--quiet" {
            } else if word.starts_with('-') {
                again.push(word);
            } else if folder.replace(word).is_some() || word.contains(':') || word.starts_with(['\\', '/']) {
                return found[0].to_string();
            }
            at += 1;
        }
        let branch = match named {
            Some(name) => name,
            None => *default.get_or_insert_with(|| default_branch.take().map_or("master", |ask| ask())),
        };
        if branch.is_empty() || !branch.chars().all(|c| c.is_alphanumeric() || "-_./".contains(c)) {
            return found[0].to_string();
        }
        let git = folder.map_or(".git".to_string(), |folder| format!("{}\\.git", folder.replace('/', "\\").trim_end_matches('\\')));
        let there = folder.map_or(String::new(), |folder| format!("-C {folder} "));
        again.extend(folder);
        let (before, init, words, after) = (&found[1], &found[2], &found[3], &found[4]);
        let space = if after.is_empty() { "" } else { " " };
        // (cmd's echo ends HEAD with CR LF, which git takes but its fsck warns of: git writes HEAD again once there is a repository for it to write in. Where
        // HEAD was there, `git init` failed of something else, and `(call)` keeps it a failure for what comes after)
        format!(
            "{before}{init}{words} || (if not exist {git}\\HEAD (>{git}\\{GIT_HEAD_WRITTEN}{branch}&& git init {} && git {there}symbolic-ref HEAD refs/heads/{branch}) else (call)){space}{after}",
            again.join(" ")
        )
    }).to_string()
}

/// What is said of a `git init` made whole (`git_init_whole`) whose first try failed: that the line wrote HEAD and ran it again, and that the repository is
/// whole only where each git folder it wrote HEAD in has its objects folder now. (A `cd` earlier in the line puts a folder elsewhere than `cwd`: one not found
/// there is not said to be whole, nor to be broken.)
fn git_init_note(runs: &str, cwd: &Path) -> String {
    let pattern = regex::Regex::new(r"if not exist (\S+)\\HEAD \(").expect("the pattern is valid");
    let folders: Vec<PathBuf> = pattern.captures_iter(runs).map(|found| cwd.join(&found[1])).collect();
    let whole = !folders.is_empty() && folders.iter().all(|git| git.join("HEAD").is_file() && git.join("objects").is_dir());
    let then = if whole { "which made the rest: the repository is whole" } else { "and what that said is above" };
    format!("note: under the users' folder git cannot look at C:\\Users, so `git init` could not lock the new repository's HEAD and stopped (the error above). The line then wrote HEAD and ran `git init` again, {then}.\n")
}

/// Whether a folder is under the users' folder, which a container cannot look at.
fn under_users(cwd: &Path, users: Option<&Path>) -> bool {
    users.is_some_and(|users| {
        let key = |path: &Path| path.to_string_lossy().trim_start_matches("\\\\?\\").replace('/', "\\").trim_end_matches('\\').to_lowercase();
        key(cwd).starts_with(&format!("{}\\", key(users)))
    })
}

/// The bundle's Python folder, where it has one: an embeddable Python, with no venv. A bare `python` in a command is that one, as the bundle takes the
/// system's Python folders off PATH.
fn bundled_python() -> Option<PathBuf> {
    crate::bundled::root().filter(|root| crate::bundled::Present::of(root).python).map(|root| root.join("python"))
}

/// A line whose `python -m venv FOLDER` is made in two steps where a system's Python runs it: in the sandbox, venv's own last step, ensurepip run by the new
/// Python as a child of it, fails as it clears its temporary folder (WinError 5), though the same ensurepip run by itself goes through. So: the venv with no pip,
/// then its own Python's ensurepip. Which Python it is, is the line's: a bare `python` or `py` is the bundle's where there is one (`bundled`, its folder: the
/// bundle has a `py.cmd` of its own, ahead of the launcher on PATH), which has no venv and is left to say so (`venv_note`); one named by its path is a system's
/// unless it is in the bundle's folder. One folder and plain words only.
fn venv_in_two_steps(line: &str, bundled: Option<&Path>) -> String {
    let pattern = regex::Regex::new(
        r#"(?i)(^|[&|(]\s*)("[^"]*\\python(?:\.exe)?"|(?:[^\s"&|()]*\\)?python(?:\.exe)?|py(?:\.exe)?(?:\s+-[0-9][0-9.\-]*)?)\s+-m\s+venv\s+([\w.\-/\\: ]+?)\s*($|&&|&|\|\||\))"#,
    )
    .expect("the pattern is valid");
    let key = |path: &str| path.trim_matches('"').trim_start_matches("\\\\?\\").replace('/', "\\").trim_end_matches('\\').to_lowercase();
    pattern.replace_all(line, |found: &regex::Captures| {
        let (before, program, words, after) = (&found[1], &found[2], found[3].trim(), &found[4]);
        let path = key(program);
        let systems = if path.contains('\\') {
            bundled.is_none_or(|folder| !path.starts_with(&format!("{}\\", key(&folder.to_string_lossy()))))
        } else {
            // (a bare `python`, or `py`: the launcher with no bundle, and the bundle's own shim with one)
            bundled.is_none()
        };
        if !systems {
            return found[0].to_string();
        }
        let mut options: Vec<&str> = words.split_whitespace().filter(|word| word.starts_with('-')).collect();
        let folders: Vec<&str> = words.split_whitespace().filter(|word| !word.starts_with('-')).collect();
        if folders.len() != 1 || options.iter().any(|option| option.eq_ignore_ascii_case("--without-pip")) {
            return found[0].to_string();
        }
        let folder = folders[0].replace('/', "\\");
        options.push("--without-pip");
        let space = if after.is_empty() { "" } else { " " };
        format!("{before}{program} -m venv {} {folder} && {folder}\\Scripts\\python -m ensurepip --upgrade --default-pip{space}{after}", options.join(" "))
    }).to_string()
}

/// What is said of a venv the bundle's Python could not make: it has none, as an embeddable Python, and a project needs none here, as what pip installs goes
/// to the workspace's own folder for it, which this Python reads. A person's Python in their own folder is not one to turn to: with the bundle's on PATH in
/// its place, its folder is not opened to commands, and it cannot start in the sandbox (measured: "Access is denied", or no `encodings` module).
fn venv_note(out: &str, err: &str) -> Option<String> {
    (format!("{out}\n{err}").contains("No module named venv") && bundled_python().is_some()).then(|| {
        format!(
            "note: this Python is the app's own, an embeddable one with no venv, and a project needs none here: `python -m pip install` puts packages in the workspace's `{}`, which this Python and the scripts it runs read. A Python installed in the user's own folder cannot start in the sandbox.\n",
            crate::bundled::PY_DEPS
        )
    })
}

/// The folder the users' own folders are in (`C:\Users`), from this account's.
fn users_folder() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from).and_then(|home| home.parent().map(Path::to_path_buf))
}

/// The name of the PowerShell drive a command in the sandbox is put on.
const POWERSHELL_PLACE: &str = "DogmindWork";

/// A line whose PowerShell is to run in `cwd` though `cwd` is under the users' folder: there PowerShell, in the sandbox, starts at the drive's root (`C:\`), as it
/// makes sure of each folder on the way to the one it was started in, and a container may not look into the users' folder. Every relative path then went to
/// `C:\` and was refused, and `Set-Location` to the folder was refused the same way. On a drive of PowerShell's own whose root is the folder, it is there: the
/// command of `-Command` (or `-c`) is given that first, and `-File` with plain words after it becomes a command that runs the script by its full path and
/// ends with its exit code. Other forms, and a folder whose name cmd would read something into, are left as they are.
fn powershell_in_place(line: &str, cwd: &Path, users: Option<&Path>) -> String {
    let under = under_users(cwd, users);
    let folder = cwd.to_string_lossy().trim_start_matches("\\\\?\\").to_string();
    if !under || folder.contains(['%', '^', '&', '|', '<', '>', '"', '!', '\n', '\r']) {
        return line.to_string();
    }
    let prefix = format!("$null=New-PSDrive -Name {POWERSHELL_PLACE} -PSProvider FileSystem -Root '{}' -Scope Global; Set-Location {POWERSHELL_PLACE}:\\;", folder.replace('\'', "''"));
    let program = regex::Regex::new(r#"(?i)(?:^|[&|(]\s*|^\s*@)\s*(?:"[^"]*\\(?:powershell|pwsh)(?:\.exe)?"|(?:[^\s"&|()]*\\)?(?:powershell|pwsh)(?:\.exe)?)(?:\s|$)"#).expect("the pattern is valid");
    let mut out = String::new();
    let mut last = 0;
    for found in program.find_iter(line) {
        if found.start() < last {
            continue;
        }
        // (its options, then -Command or -File: a value after the options that take one)
        let rest = &line[found.end()..];
        let mut at = 0;
        let mut words = Vec::new();
        while at < rest.len() {
            let gap = rest[at..].len() - rest[at..].trim_start().len();
            let start = at + gap;
            if start >= rest.len() || rest[start..].starts_with(['&', '|', ')']) {
                break;
            }
            let end = rest[start..].find(char::is_whitespace).map_or(rest.len(), |length| start + length);
            words.push((start, end));
            at = end;
        }
        let word = |index: usize| words.get(index).map(|&(start, end)| rest[start..end].to_ascii_lowercase());
        let takes_value = ["-executionpolicy", "-ep", "-ex", "-windowstyle", "-w", "-inputformat", "-outputformat", "-version", "-psconsolefile", "-configurationname", "-workingdirectory", "-wd"];
        let mut index = 0;
        let mut change: Option<(usize, String)> = None;
        while let Some(current) = word(index) {
            if !current.starts_with('-') {
                break;
            }
            let is_command = current.len() >= 2 && "-command".starts_with(&current);
            let is_file = current == "-f" || (current.len() >= 3 && "-file".starts_with(&current));
            if is_command {
                // (after it: the drive and the place, then the command as it was)
                let insert = words.get(index + 1).map_or(rest.len(), |&(start, _)| start);
                change = Some((insert, format!("{prefix} ")));
                break;
            }
            if is_file {
                let Some(&(script_start, _)) = words.get(index + 1) else { break };
                let tail_end = words.last().map_or(rest.len(), |&(_, end)| end);
                let after_file = &rest[script_start..tail_end];
                let plain = regex::Regex::new(r"^[\w.\-/\\:=,+@ ]+$").expect("the pattern is valid");
                if !plain.is_match(after_file) {
                    break;
                }
                let (script, args) = after_file.split_once(' ').map_or((after_file, ""), |(script, args)| (script, args.trim()));
                let full = if Path::new(script).is_absolute() { PathBuf::from(script) } else { cwd.join(script) };
                let full = full.to_string_lossy().replace('\'', "''");
                let (file_start, _) = words[index];
                // (in double quotes, where cmd takes the call's `&` for PowerShell's and not its own; and with the script's exit code, as -File gives it: sourced with `.`
                // instead, Windows PowerShell ended with 1 for a script's `exit 3`)
                let command = format!("-Command \"{prefix} & '{full}'{}{}; exit $LASTEXITCODE\"", if args.is_empty() { "" } else { " " }, args);
                out.push_str(&line[last..found.end() + file_start]);
                out.push_str(&command);
                last = found.end() + tail_end;
                change = None;
                break;
            }
            index += if takes_value.contains(&current.as_str()) { 2 } else { 1 };
        }
        if let Some((insert, text)) = change {
            out.push_str(&line[last..found.end() + insert]);
            out.push_str(&text);
            last = found.end() + insert;
        }
    }
    out.push_str(&line[last..]);
    out
}

/// The line that runs for a line a model wrote: the same, but where `dir`, `del` or `| head` is replaced by what does that in the
/// sandbox. The gate weighs this one too, and a person who is asked is shown this one, with what was written beside it.
/// The lines of a command are joined into one first, as `execute_command` does before it puts the stand-ins in. That alone is the same commands in the same order, so a line
/// that only needed joining comes back as it was written.
pub fn line_that_runs(line: &str) -> String {
    #[cfg(windows)]
    {
        match on_one_line(line) {
            Ok(one) => {
                let runs = with_stand_ins(&one);
                if runs == one.as_ref() { line.to_string() } else { runs }
            }
            // (a line it refuses is not run at all)
            Err(_) => line.to_string(),
        }
    }
    #[cfg(not(windows))]
    {
        line.to_string()
    }
}

/// The line that runs: `dir` and `del` read the drive's root before anything else, which a container cannot, wherever their file is. A model reaches for them by habit however it is
/// told, and each refusal is a round of a slow model's time. A line with parentheses or more than one line of its own is left as it is, and so is a line this has made: running
/// it on its own answer changes nothing, so that what was shown is what runs.
#[cfg(windows)]
fn with_stand_ins(line: &str) -> String {
    if line.contains(['\n', '\r']) {
        return line.to_string();
    }
    let mut quoted = false;
    if line.chars().any(|c| {
        quoted ^= c == '"';
        !quoted && matches!(c, '(' | ')')
    }) {
        return dir_stand_ins_around_parentheses(line);
    }
    let (mut out, mut start, mut quoted, mut changed, mut after_a_pipe) = (String::new(), 0, false, false, false);
    let mut step = |text: &str, piped: bool, after_a_pipe: bool, out: &mut String| {
        // (`| head -60`, a model's habit from elsewhere, and a failed round each time. All of it is captured and cut to size anyway, and this passes it on as it is.)
        if after_a_pipe && only_keeps_some_lines(text) {
            changed = true;
            out.push_str(" findstr \"^\"");
            return;
        }
        match step_stand_in(text, piped) {
            Some(stand_in) => {
                changed = true;
                out.push_str(&stand_in);
            }
            None => out.push_str(text),
        }
    };
    for (at, c) in line.char_indices() {
        if c == '"' {
            quoted = !quoted;
        } else if !quoted && matches!(c, '(' | ')') {
            return line.to_string();
        } else if !quoted && matches!(c, '&' | '|') {
            let piped = c == '|' && !line[at + 1..].starts_with('|') && !line[..at].ends_with('|');
            step(&line[start..at], piped, after_a_pipe, &mut out);
            after_a_pipe = piped;
            out.push(c);
            start = at + 1;
        }
    }
    step(&line[start..], false, after_a_pipe, &mut out);
    if changed { out } else { line.to_string() }
}

/// `head`, `tail`, `head -5`, `tail -n 20`: nothing but which lines to keep.
#[cfg(windows)]
fn only_keeps_some_lines(step: &str) -> bool {
    let mut words = step.split_whitespace();
    if !matches!(words.next(), Some("head" | "tail")) {
        return false;
    }
    let rest: Vec<&str> = words.collect();
    let number = |word: &str| !word.is_empty() && word.trim_start_matches(['-', '+']).chars().all(|c| c.is_ascii_digit());
    match rest[..] {
        [] => true,
        [one] => number(one) && one.starts_with('-') || one.strip_prefix("-n").is_some_and(number),
        [switch, count] => switch == "-n" && number(count),
        _ => false,
    }
}

/// Whether the line has a cmd `set NAME=value` that is not quoted and ends in a space before a `&`, `&&`, `|` or `||`: cmd keeps the space in the value, so `set A=C:\x && cd %A%` looks for `C:\x ` (a real
/// model's `set CARGO_TARGET_DIR=%TEMP%\sd-isolation-target && cargo build` failed on a path ending in a space). `set` is the first word of the line, or of what follows `&`, `|` or `(`; the
/// switches (`set /a`, `set /p`) and the quoted form `set "A=b"` do not have it.
#[cfg(windows)]
fn set_value_ends_in_a_space(line: &str) -> bool {
    let (mut start, mut quoted, mut escaped) = (0, false, false);
    for (at, c) in line.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '"' {
            quoted = !quoted;
        } else if c == '^' && !quoted {
            escaped = true;
        } else if !quoted && matches!(c, '&' | '|' | '(' | ')' | '\n' | '\r') {
            if matches!(c, '&' | '|') && set_with_a_space_at_its_end(&line[start..at]) {
                return true;
            }
            start = at + c.len_utf8();
        }
    }
    false
}

/// A command (as `commands_of` cuts them, with its trailing space) that is `set NAME=value` with the value ending in a space.
#[cfg(windows)]
fn set_with_a_space_at_its_end(command: &str) -> bool {
    let command = command.trim_start().trim_start_matches('@').trim_start();
    if !command.get(..3).is_some_and(|word| word.eq_ignore_ascii_case("set")) || !command[3..].starts_with([' ', '\t']) {
        return false;
    }
    let rest = command[3..].trim_start();
    !rest.starts_with(['"', '/']) && rest.split_once('=').is_some_and(|(name, _)| !name.trim().is_empty()) && rest.ends_with([' ', '\t'])
}

/// The C++ build tool a command said it could not find, if it did: in the words of cmd, English or Japanese (`'cl' は、内部コマンドまたは外部コマンド...として認識されていません`).
fn build_tool_not_found(err: &str) -> Option<&'static str> {
    let not_found = err.contains("is not recognized") || err.contains("認識されていません");
    not_found.then(|| ["cl", "link", "nmake"].into_iter().find(|program| err.contains(&format!("'{program}'")))).flatten()
}

/// What a model is told when `cl`, `link` or `nmake` was not found: why the app did not give the build tools, and what the person can install. `known`: the app has found some.
fn build_tools_note(program: &str, why: Option<crate::msvc::Why>, known: bool) -> String {
    match (why, known) {
        (Some(why), _) => format!("note: `{program}` was not found because the build tools were not given to this command: {}. Tell the person that; building needs them.\n", why.words()),
        // (they are found: the command was not run with them for another reason, or the look had just ended)
        (None, true) => String::new(),
        (None, false) => format!("note: `{program}` was not found, and the search for Visual Studio's build tools had not come to an answer when this command began (it runs in the background). Running the command again may find them; if not, the person has to install the \"Desktop development with C++\" workload of Visual Studio or its Build Tools.\n"),
    }
}

/// What a command in the sandbox is told when it asks what runs or listens on the PC: it sees only the programs it started itself.
const PC_PROGRAMS_NOTE: &str = "note: in the sandbox a command sees only the programs it started itself. `tasklist` fails there (\"The user name or password is incorrect\"), `Get-Process` and `netstat` show nothing of the PC's other programs or the ports they listen on, and `Get-NetTCPConnection` is refused: what this printed says nothing of what runs or listens on the PC. Ask the user, or, if they asked for it, run it again with `outside_sandbox: true`.\n";

/// What it is told when it could not connect to an address of the PC's own.
const THIS_PC_SERVER_NOTE: &str = "note: a command in the sandbox cannot connect to a server of this PC's that runs outside the sandbox (127.0.0.1, localhost, ::1), with network access or without; one that a command in the sandbox started can be reached. So this does not say the server is down. An MCP server is used through Dogmind's own MCP client, which the user sets up with `/mcp`; for anything else, if the user asked for it, run the command again with `outside_sandbox: true`.\n";

/// Whether the line asks what runs or listens on the PC: a command of it that is a program listing processes or connections, or a cmdlet that does.
fn asks_what_the_pc_runs(line: &str, lower: &str) -> bool {
    commands_of(line).iter().any(|command| {
        let word = first_word(command).0.to_ascii_lowercase();
        matches!(word.trim_end_matches(".exe"), "tasklist" | "netstat" | "wmic")
    }) || ["get-process", "get-nettcpconnection", "get-netudpendpoint"].iter().any(|cmdlet| lower.contains(cmdlet))
        || (lower.contains("win32_process") && (lower.contains("get-ciminstance") || lower.contains("get-wmiobject")))
}

/// Whether the line names an address of the PC's own.
fn names_this_pc(lower: &str) -> bool {
    lower.contains("127.0.0.1") || lower.contains("localhost") || lower.contains("[::1]")
}

/// Whether what the command said is that a connection was not made: in the words of curl, PowerShell, Python, Node and Windows in Japanese.
fn says_no_connection(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    ["failed to connect", "could not connect", "connection refused", "actively refused", "connection timed out", "connect timeout", "unable to connect", "no connection could be made", "econnrefused", "etimedout"]
        .iter()
        .any(|words| lower.contains(words))
        || err.contains("接続できませんでした")
        || err.contains("接続を拒否")
}

/// Looks at a running job with nothing new, after the first, before a look waits for it (the look after the one that said not to look again), and how long it waits at most.
const QUIET_LOOKS_BEFORE_WAITING: u32 = 1;
const LOOK_WAITS: Duration = Duration::from_secs(60);

/// What says a file or folder could not be opened for want of the right, in the words of cargo, git, cmd, PowerShell and Windows in Japanese.
const DENIED_WORDS: [&str; 4] = ["Access is denied", "アクセスが拒否されました", "Permission denied", "is denied"];

/// The paths a line of a refusal names: what it quotes (`...`, '...', "...") and what comes before its first `: ` (git's and icacls' way), each taken
/// apart on `/` and `\` and with `.` and `..` worked out, so that cargo's `src\../../../assets` is the folder it means.
fn named_paths(line: &str) -> Vec<PathBuf> {
    let mut found: Vec<String> = Vec::new();
    for quote in ['`', '\'', '"'] {
        let parts: Vec<&str> = line.split(quote).collect();
        found.extend(parts.iter().skip(1).step_by(2).take(parts.len().saturating_sub(1) / 2).map(|part| part.to_string()));
    }
    if let Some((before, _)) = line.split_once(": ") {
        found.push(before.trim().trim_start_matches("error").trim_start_matches(':').trim().to_string());
    }
    found.into_iter().filter(|text| !text.is_empty() && (text.contains('\\') || text.contains('/') || text.contains('.'))).map(|text| {
        let mut path = PathBuf::new();
        for part in text.split(['/', '\\']) {
            match part {
                "" | "." => {}
                ".." => {
                    path.pop();
                }
                part if path.as_os_str().is_empty() && part.ends_with(':') => path.push(format!("{part}\\")),
                part => path.push(part),
            }
        }
        path
    }).collect()
}

/// A refusal of a path in the workspace under a folder another account owns (another program's sandbox account made it there): the person cannot change
/// that folder's access list, so the access the app gives commands never reached it, and nothing a command does will. Said, with the folder and its owner,
/// so that the model neither tries again nor takes the file for damaged. (A folder Codex's sandbox account made in a repository stopped every build there
/// with "access is denied", a real session of 2026-11-22.)
fn foreign_owner_note(err: &str, workspace: &Path) -> Option<String> {
    let root = crate::sandbox::key(workspace);
    for line in err.lines().filter(|line| DENIED_WORDS.iter().any(|word| line.contains(word))).take(20) {
        for named in named_paths(line) {
            let full = if named.is_absolute() { named } else { workspace.join(named) };
            let Ok(inside) = full.strip_prefix(workspace) else { continue };
            if !crate::sandbox::key(&full).starts_with(&root) {
                continue;
            }
            let mut at = workspace.to_path_buf();
            for part in inside.components() {
                at.push(part);
                if !at.exists() {
                    break;
                }
                if let Some(owner) = crate::sandbox::owned_by_another(&at) {
                    return Some(format!("note: `{}` belongs to another account, {owner} (another program's sandbox may have made it), not to the user. The user cannot change its access list, so the access this app gives commands never reached it, and the sandbox is refused what is in it. The files are not damaged, and no command can change this: tell the user. A copy the user makes is theirs: copying the folder, deleting the original and giving the copy its name puts it right (for files a repository tracks, deleting the folder and `git checkout -- <folder>` does too).\n", at.display()));
                }
            }
        }
    }
    None
}

/// One line more where a sandbox's answer reads as something else: a model believes it, and the next rounds go on that.
#[cfg(windows)]
fn sandbox_notes(line: &str, workspace: &Path, status: Option<i32>, err: &str) -> String {
    let mut notes = String::new();
    let lower = line.to_ascii_lowercase();
    // (whatever it came to: the program that is then run fails, or runs with a value that is not the one written, and the line itself shows nothing wrong)
    if set_value_ends_in_a_space(line) {
        notes.push_str("note: in cmd, the space before `&&` (or `&`, `||`, `|`) stays in the value of `set NAME=value && ...`. Write `set \"NAME=value\" && ...`.\n");
    }
    if crate::bundled::root().is_some() && lower.contains("pip") && lower.contains("install") {
        // (a repository need not see them)
        if workspace.join(crate::bundled::PY_DEPS).is_dir() {
            let _ = std::fs::write(workspace.join(crate::bundled::PY_DEPS).join(".gitignore"), "*\n");
        }
        notes.push_str("note: `pip install` puts packages in .pydeps in the workspace, and python finds them there. The bundled Python itself cannot be changed.\n");
    }
    // (a real model ended its own server so, `taskkill /F /IM python.exe /T`: every session's commands run in the one sandbox, and a
    // server another conversation started went too)
    if (lower.contains("taskkill") && lower.contains("/im")) || (lower.contains("stop-process") && lower.contains("-name")) {
        notes.push_str("note: a command started in the background is ended with command_stop and its id, with all it started. By name, this ends every such program the sandbox runs, those of the user's other conversations too.\n");
    }
    // (git says it and goes on, with nothing else wrong)
    if let Some(note) = foreign_owner_note(err, workspace) {
        notes.push_str(&note);
    }
    // (a model looking for the user's Unity editor and its MCP server asked tasklist and was told "The user name or password is incorrect", and netstat answered with
    // no connections at all, which reads as nothing listening: it does whatever it ends with)
    if asks_what_the_pc_runs(line, &lower) {
        notes.push_str(PC_PROGRAMS_NOTE);
    }
    if status == Some(0) {
        return notes;
    }
    // (the same model then tried the server's port on 127.0.0.1, which a command in the sandbox cannot reach)
    if names_this_pc(&lower) && says_no_connection(err) {
        notes.push_str(THIS_PC_SERVER_NOTE);
    }
    if err.contains("Can not combine '--user' and '--target'") {
        notes.push_str("note: leave out `--user`: `pip install` already keeps packages in the workspace.\n");
    }
    if crate::bundled::root().is_some() && (lower.contains("-m venv") || lower.contains("-m virtualenv")) {
        notes.push_str("note: this Python has no venv. No venv is needed: `pip install` already keeps packages in the workspace.\n");
    }
    if crate::bundled::root().is_some() && (lower.contains("program files\\nodejs") || lower.contains("program files\\git") || lower.contains("appdata\\local\\programs\\python")) {
        notes.push_str("note: run it by name. `python`, `node`, `npm` and `git` are on PATH; what is installed under that path is closed to the sandbox.\n");
    }
    // (it says this in the language of Windows, and ends with 1 for it and with 2 for anything else)
    if first_word(line).0.eq_ignore_ascii_case("where") && status == Some(1) {
        notes.push_str("note: `where` cannot see every folder from the sandbox, so this does not say the program is missing. Run the program itself to find out.\n");
    }
    if (lower.contains("powershell") || lower.contains("pwsh")) && powershell_lost_at_a_root(err, workspace) {
        notes.push_str("note: PowerShell starts at the root of a drive in the sandbox and cannot move from there, whatever `cwd` says: give it absolute paths, and `Start-Process` a `-WorkingDirectory`. `[IO.File]` methods do take paths relative to `cwd`.\n");
    }
    if lower.contains("git") && (err.contains("failed to stat") || err.contains("not a git repository") || (err.contains("error reading") && err.contains(".git"))) {
        // (the workspace may be a folder of a repository above it, which the sandbox cannot reach: then it is that, and not that the folder is no repository)
        if let Some(top) = repository_above(workspace) {
            notes.push_str(&format!("note: this folder is part of the git repository at `{}`, and the sandbox cannot reach its `.git`. {}\n", top.display(), git_above_words(&top)));
        } else if err.contains("failed to stat") {
            notes.push_str("note: git cannot look above the workspace from the sandbox, so this says only that the workspace is not a git repository. `git init` at the start of a line works.\n");
        }
    }
    // (cargo expands `members = ["crates/*"]` by looking at each folder from the drive's root, and a container may not look at C:\Users: the pattern then finds nothing and is read as
    // a name, "failed to read `…\crates\*\Cargo.toml`", and a model goes on trying other options and folders, a crate's own manifest too, which loads the same workspace)
    // (a command that is cargo itself, in such a workspace, runs with a restricted token where it can: this is cargo started through something else)
    if err.contains("failed to load manifest for workspace member") && err.contains('*') {
        notes.push_str("note: cargo could not expand this workspace's `members` pattern: it looks at each folder from the drive's root, and the sandbox may not look at C:\\Users on the way. \
                        A command that is cargo itself is run where it can: run cargo directly, not through a script or another program, and with nothing else on its line (no `&&`, `&`, `|`, or `>` to a file; it may end with `2>&1` or `> nul`). \
                        Do not change the workspace's Cargo.toml to get round this. \
                        If that cannot be done, say what could not be built or run, for the user to run themselves.\n");
    }
    // (cl, link and nmake are in no PATH of a person's unless they started from a Developer prompt; the app gives a command their folders when it finds them, and says here why it did not)
    if let Some(program) = build_tool_not_found(err) {
        notes.push_str(&build_tools_note(program, crate::msvc::why_not(), crate::msvc::known().is_some()));
    }
    let denied = says_access_denied(err, system_access_denied());
    let lower_err = err.to_ascii_lowercase();
    let in_git = lower.contains(".git\\config") || lower.contains(".git/config") || lower.contains(".git\\hooks") || lower.contains(".git/hooks");
    if lower_err.contains("config file") || (denied && in_git) || (in_git && lower_err.contains("permission denied")) {
        notes.push_str("note: the sandbox refuses changes to `.git\\config` and `.git\\hooks`, as what is written there would run outside it the next time git is used. Tell the user the exact command, for them to run.\n");
    }
    // (the line may name no path, as `python clean.py` does: then the refusal's own line says where)
    if denied && (names_outside(line, workspace) || err.lines().any(|said| says_access_denied(said, system_access_denied()) && names_outside(said, workspace))) {
        notes.push_str(OUTSIDE_READ_NOTE);
    }
    // (a model's habit from elsewhere: cmd says only that it does not know the program, in the language of Windows, or Git's own copy fails at its start and says nothing a model can use)
    let msys = err.contains("NtCreateDirectoryObject");
    let unknown = status == Some(9009) || err.contains("not recognized") || err.contains("認識されていません");
    let unix = unix_tools_in(line);
    if !unix.is_empty() && (msys || unknown) {
        let moves = unix.iter().any(|tool| matches!(tool.as_str(), "rm" | "cp" | "mv"));
        notes.push_str(&format!("note: Unix tools do not run in the sandbox. Read, list and search with `read_file`, `list_files`, `find_files` and `search_files`{}.\n", if moves { "; `del`, `copy` and `move` do what `rm`, `cp` and `mv` do" } else { "" }));
    }
    if msys {
        notes.push_str("note: programs from Git's `usr\\bin` (MSYS) cannot start in the sandbox, git hooks included.\n");
    }
    notes
}

/// What to say where Chrome stopped at once in the sandbox: its crash handler is refused the pipe it makes there, whatever switches it is given (with
/// `2>&1` what it said is in the output). A model that wanted a page's picture ran it outside the sandbox for it, which the user is asked about each time.
fn chrome_note(out: &str, err: &str) -> Option<&'static str> {
    [out, err].iter().any(|said| said.contains("crashpad_client_win.cc") && said.contains("CreateNamedPipe")).then_some(
        "note: Chrome cannot start in the sandbox: it stops at once when its crash handler is refused there, whatever its switches. To look at a page, use check_page, \
         which opens it in a browser in the sandbox and gives a picture of it, WebGL drawn too.\n",
    )
}

/// Whether a line names a path on a drive that is not under the workspace.
#[cfg(windows)]
/// A program's own words count too: Python's `'C:\\Users\\me\\a.txt'`, in quotes and with its backslashes doubled.
fn names_outside(line: &str, workspace: &Path) -> bool {
    let root = workspace.to_string_lossy().to_lowercase();
    line.split(|c: char| c.is_whitespace() || c == '"').any(|word| {
        let word = word.trim_matches(|c: char| matches!(c, '\'' | '(' | ')' | '[' | ']' | ',' | ';')).replace("\\\\", "\\").to_lowercase();
        word.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) && word.as_bytes().get(1) == Some(&b':') && !word.starts_with(&root)
    })
}

/// The Unix tools a line starts: the first word of each of its commands, when it is one of those a model reaches for from habit. By the line that runs, in which `| head` is no longer there.
#[cfg(windows)]
fn unix_tools_in(line: &str) -> Vec<String> {
    const TOOLS: [&str; 14] = ["ls", "cat", "wc", "head", "tail", "grep", "sed", "awk", "find", "touch", "rm", "cp", "mv", "which"];
    let runs = line_that_runs(line);
    commands_of(&runs).into_iter().filter_map(|command| {
        let first = first_word(command.trim_start().trim_start_matches('@')).0.rsplit(['\\', '/']).next().unwrap_or("").to_ascii_lowercase();
        let name = first.strip_suffix(".exe").unwrap_or(&first);
        TOOLS.contains(&name).then(|| name.to_string())
    }).collect()
}

/// That something was refused access: by the code in what the program said where it gives one, which reads the same on every Windows. What only has words, `cmd`'s own commands, says
/// what Windows calls it in its language: `system_words`, or what was seen in English and Japanese when that could not be read.
fn says_access_denied(err: &str, system_words: &str) -> bool {
    const CODES: [&str; 8] = ["[WinError 5]", "[Errno 13]", "(os error 5)", "0x80070005", "EACCES", "EPERM", "UnauthorizedAccessException", "PermissionDenied"];
    CODES.iter().any(|code| err.contains(code)) || (!system_words.is_empty() && err.contains(system_words)) || err.contains("Access is denied") || err.contains("アクセスが拒否されました")
}

/// ERROR_ACCESS_DENIED in the words of this Windows: "Zugriff verweigert" on a German one.
fn system_access_denied() -> &'static str {
    #[cfg(windows)]
    {
        static WORDS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        WORDS.get_or_init(|| {
            use windows_sys::Win32::System::Diagnostics::Debug::{FormatMessageW, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS};
            let mut buffer = [0u16; 256];
            // SAFETY: the buffer is as long as the call is told.
            let length = unsafe { FormatMessageW(FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS, std::ptr::null(), 5, 0, buffer.as_mut_ptr(), buffer.len() as u32, std::ptr::null()) };
            String::from_utf16_lossy(&buffer[..length as usize]).trim().trim_end_matches(['.', '。']).to_string()
        })
    }
    #[cfg(not(windows))]
    ""
}

/// PowerShell says this in the language of Windows, with the same names in its last lines, `(D:\\a:String)` and `ItemNotFoundException`, and on whichever drive it began.
/// `Start-Process` with no folder of its own cannot use that root either, and names no path: the drive it does not find is the provider's own name, in any language and either PowerShell.
pub(crate) fn powershell_lost_at_a_root(err: &str, workspace: &Path) -> bool {
    if err.contains("'Microsoft.PowerShell.Core\\FileSystem'") {
        return true;
    }
    let in_a_drive = |text: &str| {
        let bytes = text.as_bytes();
        bytes.len() > 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\'
    };
    let root = workspace.to_string_lossy().to_lowercase();
    let named = err.match_indices('(').map(|(at, _)| &err[at + 1..]).find_map(|rest| rest.split_once(":String)").map(|(path, _)| path)).filter(|path| in_a_drive(&format!("{path}\\")));
    if let Some(path) = named {
        // (the root itself refused, or a path under it that was not found: one of the workspace's own was asked for by its full name, and is only missing)
        return !path.to_lowercase().starts_with(&root) && (path.len() == 3 || err.contains("ItemNotFoundException") || err.contains("PathNotFound"));
    }
    err.split_once("Cannot find path '").is_some_and(|(_, rest)| in_a_drive(rest) && !rest.to_lowercase().starts_with(&root))
}

const RUNS_ITSELF: [&str; 4] = ["exe", "com", "cmd", "bat"];

/// Whether `start` would find a program under this name: by its path, or in the folder or on PATH, with an extension that runs or none. What it does not find it opens, or says it cannot find in a dialog of its own.
#[cfg(windows)]
fn is_program(word: &str, cwd: &Path) -> bool {
    let extension = Path::new(word).extension().map(|extension| extension.to_string_lossy().to_ascii_lowercase());
    let names: Vec<String> = match extension {
        Some(extension) if RUNS_ITSELF.contains(&extension.as_str()) => vec![word.to_string()],
        Some(_) => return false,
        None => RUNS_ITSELF.iter().map(|extension| format!("{word}.{extension}")).collect(),
    };
    // (what a command would find: the bundled tools first)
    let path = crate::bundled::path_for_programs().map_or_else(|| std::env::var_os("PATH").unwrap_or_default(), std::ffi::OsString::from);
    std::iter::once(cwd.to_path_buf()).chain(std::env::split_paths(&path)).any(|folder| names.iter().any(|name| folder.join(name).is_file()))
}

/// What this command would hand to whatever Windows has for that kind of file instead of running it. With nothing for it, that is a dialog on the person's screen asking how to open it,
/// which no command can answer and which they did not ask for.
///
/// Refused: a first word that is a file of a kind that opens (`OPENS`, whether or not it is there), or any file there that is not a program; the same after `call` and `cmd /c`;
/// `start` of anything that is not a program, an address or a folder too, and `start` of nothing.
/// Let through: a program named as its first word (`python x.py`, `tool.exe a.py`, `python3.12 x.py`, `run.bat`); a file only as an argument (`echo x.py`, `type a.txt`); `start` of a program
/// it finds, with its title and switches (`start "" /b python x.py`, `start /d sub /wait run.bat`).
/// Not seen: the same asked of another shell (`powershell ./x.py`), and what `if` and `for` go on to run.
#[cfg(windows)]
fn opens_by_association(command: &str, cwd: &Path) -> Option<String> {
    const OPENS: [&str; 22] = ["py", "pyw", "js", "mjs", "ts", "vbs", "wsf", "ps1", "sh", "bash", "html", "htm", "txt", "md", "json", "pdf", "png", "jpg", "jpeg", "gif", "svg", "log"];
    // As cmd reads it: its escape is not in the name, and a switch may follow a name with no space, as in `cmd/c x` and `start/b x`.
    let unescaped = command.trim().trim_start_matches('@').replace('^', "");
    let (first, rest) = first_word(&unescaped);
    let (first, joined) = match first.find('/').map(|at| first.split_at(at)) {
        Some((name, switch)) if matches!(name.to_ascii_lowercase().as_str(), "cmd" | "cmd.exe" | "start") => (name, format!("{switch} {rest}")),
        _ => (first, rest.to_string()),
    };
    let rest = joined.as_str();
    match first.to_ascii_lowercase().as_str() {
        "call" => return opens_by_association(rest, cwd),
        "cmd" | "cmd.exe" => {
            let lower = rest.to_ascii_lowercase();
            // (what it runs may follow with no space: `cmd /c"x.txt"`)
            let at = lower.find("/c").or_else(|| lower.find("/k"))?;
            return opened_on_the_desktop(rest[at + 2..].trim().trim_matches('"'), cwd);
        }
        "start" => {
            let mut rest = rest.trim_start();
            // (quoted first, it is the window's title, whatever it says)
            if rest.starts_with('"') {
                rest = first_word(rest).1.trim_start();
            }
            loop {
                let (word, after) = first_word(rest);
                if !word.starts_with('/') {
                    break;
                }
                rest = after.trim_start();
                if matches!(word.to_ascii_lowercase().as_str(), "/d" | "/node" | "/affinity" | "/machine") {
                    rest = first_word(rest).1.trim_start();
                }
            }
            let target = first_word(rest).0;
            return (!is_program(target, cwd) || target.is_empty()).then(|| format!("{first} {target}").trim_end().to_string());
        }
        _ => {}
    }
    let extension = Path::new(first).extension()?.to_string_lossy().to_ascii_lowercase();
    (!RUNS_ITSELF.contains(&extension.as_str()) && (OPENS.contains(&extension.as_str()) || cwd.join(first).is_file())).then(|| first.to_string())
}

/// The first thing in this line that would open on the person's screen. What a `for` goes through is a list of words and no command: `for %f in (app.log) do @echo %~zf` opens nothing.
#[cfg(windows)]
fn opened_on_the_desktop(line: &str, cwd: &Path) -> Option<String> {
    let commands = commands_of(line);
    let of_a_for = |at: usize| at > 0 && {
        let before = commands[at - 1].trim().trim_start_matches('@').to_ascii_lowercase();
        before.starts_with("for ") && before.ends_with(" in")
    };
    commands.iter().enumerate().filter(|(at, _)| !of_a_for(*at)).find_map(|(_, command)| opens_by_association(command, cwd))
}

/// A line that begins `cd <folder> && `, with `/d` or not, or `chdir` or `pushd`, where the folder is given by its full path and is `cwd` itself: that first part, and the rest.
/// None for anything else, a `cd` to another folder or by a relative path included (those are the command's own, and a relative one works in the sandbox).
#[cfg(windows)]
fn after_cd_to<'a>(line: &'a str, cwd: &Path) -> Option<(&'a str, &'a str)> {
    let (head, rest) = line.trim_start().split_once("&&")?;
    let (head, rest) = (head.trim(), rest.trim_start());
    let lower = head.to_ascii_lowercase();
    let after = ["cd ", "chdir ", "pushd "].iter().find(|word| lower.starts_with(*word)).map(|word| head[word.len()..].trim_start())?;
    let target = if after.get(..2).is_some_and(|switch| switch.eq_ignore_ascii_case("/d")) { after[2..].trim_start() } else { after };
    let target = target.strip_prefix('"').and_then(|inner| inner.strip_suffix('"')).unwrap_or(target);
    if rest.is_empty() || target.is_empty() || target.contains('"') || !Path::new(target).is_absolute() {
        return None;
    }
    let same = matches!((std::fs::canonicalize(target), std::fs::canonicalize(cwd)), (Ok(there), Ok(here)) if there == here);
    same.then_some((head, rest))
}

/// What a command refused outside the workspace is told, in the container, which reads there no more than it writes.
const OUTSIDE_READ_NOTE: &str = "note: in the sandbox a command cannot read or change anything outside the workspace and the folders the user has named under Settings > Permissions and safety, by any route. `read_file`, `list_files` and `search_files` can look there. If the user asked for this, run the same command again with `outside_sandbox: true`: the user is asked, and if they allow it, it runs as them.\n";
/// The same, for a command run with a restricted token, which reads where the user can.
const OUTSIDE_WRITE_NOTE: &str = "note: this command could read outside the workspace but not write there: in the sandbox it writes in the workspace alone. If the user asked for this, run the same command again with `outside_sandbox: true`: the user is asked, and if they allow it, it runs as them.\n";

/// Whether a line is one cargo command, under the user folder, in a workspace whose manifest (the one cargo would load from `cwd`) names its members by a pattern: cargo
/// expands it by looking at each folder from the drive's root, and in the container C:\Users is refused, so the pattern finds nothing. Such a command, once the person has
/// allowed it (the gate asks with `REASON_CARGO_OUTSIDE`, which no rule, "always" or mode answers), runs with a restricted token.
#[cfg(windows)]
pub(crate) fn cargo_reads_above(line: &str, cwd: &Path, root: &Path, home: Option<&Path>) -> bool {
    let Some(users) = home.and_then(Path::parent) else { return false };
    one_cargo_command(line) && root.starts_with(users) && cwd.starts_with(root) && members_by_pattern(cwd, root)
}

/// Whether the line is cargo and nothing else: its first word names cargo, and it has none of what lets cmd run more than one program or put something else in its words
/// (`& | ( ) < > ^ % ! ;`, a line break). A line of more, as `cargo build & del x`, would run all of it with what cargo is given, and is the container's. It may end by
/// putting cargo's errors with its output, or by letting either go (`CARGO_LINE_ENDS`): a model's `cargo test -p app --lib 2>&1` ran in the container for its `2>&1`,
/// failed on the workspace's `crates/*`, and the model rewrote the workspace's members in Cargo.toml to get round it.
#[cfg(windows)]
pub(crate) fn one_cargo_command(line: &str) -> bool {
    let line = without_harmless_ends(line.trim());
    if line.contains(['&', '|', '(', ')', '<', '>', '^', '%', '!', ';', '\n', '\r', '`']) {
        return false;
    }
    let word = match line.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next().unwrap_or(""),
        None => line.split_whitespace().next().unwrap_or(""),
    };
    let name = word.rsplit(['\\', '/']).next().unwrap_or(word);
    name.eq_ignore_ascii_case("cargo") || name.eq_ignore_ascii_case("cargo.exe")
}

/// What may end a line that is still cargo alone: its errors put with its output, or either let go. None of them runs a program, and NUL and the screen are all they write
/// to. (The longer ones first: `2> nul` ends with `> nul` too.)
const CARGO_LINE_ENDS: [&str; 8] = ["2> nul", "1> nul", "2>nul", "1>nul", "2>&1", "> nul", ">nul", ">&2"];

/// The line without the `CARGO_LINE_ENDS` it ends with, each a word of its own (`x2>&1` is not).
fn without_harmless_ends(mut line: &str) -> &str {
    loop {
        let lower = line.to_ascii_lowercase();
        let Some(end) = CARGO_LINE_ENDS.iter().find(|end| lower.ends_with(*end)) else { return line };
        let rest = &line[..line.len() - end.len()];
        if !rest.ends_with([' ', '\t']) {
            return line;
        }
        line = rest.trim_end();
    }
}

/// What cargo's config in the workspace or above it, up to the user folder, runs besides cargo and the compiler: a wrapper of rustc, a runner or a linker it names. They run with what
/// the command is given, so the person is told of them with it.
#[cfg(windows)]
pub(crate) fn cargo_config_programs(cwd: &Path, home: Option<&Path>) -> Vec<String> {
    let mut found = Vec::new();
    for folder in cwd.ancestors().take_while(|folder| home.is_none_or(|home| folder.starts_with(home))) {
        for name in ["config.toml", "config"] {
            let Ok(text) = std::fs::read_to_string(folder.join(".cargo").join(name)) else { continue };
            for line in text.lines().map(str::trim).filter(|line| !line.starts_with('#')) {
                let Some((key, value)) = line.split_once('=') else { continue };
                let key = key.trim().trim_matches('"').to_ascii_lowercase();
                if key.ends_with("wrapper") || key == "runner" || key == "linker" {
                    let value = value.trim().trim_matches(['"', '\'']).to_string();
                    if !value.is_empty() && !found.contains(&value) {
                        found.push(value);
                    }
                }
            }
        }
    }
    found
}

/// Whether the workspace manifest cargo would load from `cwd`, the nearest `Cargo.toml` with a `[workspace]` table from `cwd` up to `root`, names its members by a
/// pattern (`crates/*`).
#[cfg(windows)]
fn members_by_pattern(cwd: &Path, root: &Path) -> bool {
    for folder in cwd.ancestors().take_while(|folder| folder.starts_with(root)) {
        let Ok(text) = std::fs::read_to_string(folder.join("Cargo.toml")) else { continue };
        let (mut table, mut members, mut in_members, mut workspace) = ("", String::new(), false, false);
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') && !in_members {
                table = line;
                workspace |= line == "[workspace]";
            } else if table == "[workspace]" && (in_members || line.split(['=', ' ']).next() == Some("members")) {
                let value = if in_members { line } else { line.split_once('=').map_or("", |(_, value)| value) };
                members.push_str(value);
                in_members = !value.contains(']');
            }
        }
        if workspace {
            return members.contains(['*', '?']);
        }
    }
    false
}

/// What cmd is to be given. It ends a line at its first line break and says nothing: of two commands the second never runs, and `python -c "` with its code on the next lines runs
/// an empty program, with no output and exit code 0, which sent a model looking for what was wrong and then writing the same code into a file left behind in the workspace.
/// So lines become commands joined by `&`, as they would run in a batch file, and code on several lines given to `python -c` or `node -e` goes as one line that runs the same text.
/// None of that can be done for a line break in any other quotes, and that is refused, as only a part of what was asked would run.
#[cfg(windows)]
fn on_one_line(line: &str) -> Result<std::borrow::Cow<'_, str>, String> {
    let line = line.trim_matches(['\n', '\r', ' ']);
    if !line.contains(['\n', '\r']) {
        return Ok(line.into());
    }
    let mut out = String::with_capacity(line.len());
    let mut rest = after_comments(line);
    while let Some(c) = rest.chars().next() {
        match c {
            '"' => {
                let quoted = &rest[1..];
                if let Some(run) = runs_code(&out).filter(|_| argument_end(quoted).is_some_and(|end| quoted[..end].contains('\n'))) {
                    let end = argument_end(quoted).expect("just looked at");
                    out.push_str(&format!("\"{run}('{}')\"", escaped_for_a_string(&argument_text(&quoted[..end]))));
                    rest = &quoted[end + 1..];
                    continue;
                }
                let Some(end) = quoted.find('"') else { return Err(LINE_BREAK_IN_QUOTES.to_string()) };
                if quoted[..end].contains(['\n', '\r']) {
                    return Err(LINE_BREAK_IN_QUOTES.to_string());
                }
                out.push_str(&rest[..end + 2]);
                rest = &quoted[end + 1..];
            }
            // (its escape: at a line's end that line goes on, and otherwise what follows is only a letter, a quote as well)
            '^' => {
                let after = &rest[1..];
                if after.starts_with(['\n', '\r']) {
                    rest = after.trim_start_matches(['\n', '\r']);
                } else {
                    let next = after.chars().next().map_or(0, char::len_utf8);
                    out.push_str(&rest[..1 + next]);
                    rest = &after[next..];
                }
            }
            '\n' | '\r' => {
                rest = after_comments(rest.trim_start_matches(['\n', '\r', ' ', '\t']));
                if rest.is_empty() {
                    break;
                }
                let before = out.trim_end();
                // (a block's parentheses take no `&` beside them)
                let plain = before.ends_with(['(', '&', '|']) || rest.starts_with(')') || before.is_empty();
                out.push_str(if plain { " " } else { " & " });
            }
            _ => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    Ok(out.into())
}

/// What follows the comments that begin this. In a batch file a comment ends with its line; joined by `&`, a `rem` or a `::` would take all the lines after it with it, with nothing said and
/// exit code 0.
#[cfg(windows)]
fn after_comments(mut rest: &str) -> &str {
    loop {
        let start = rest.trim_start_matches([' ', '\t', '@']);
        let word: String = start.chars().take(4).collect::<String>().to_ascii_lowercase();
        let is_rem = word.starts_with("rem") && word[3..].chars().next().is_none_or(|next| next.is_whitespace());
        if !is_rem && !start.starts_with("::") {
            return rest;
        }
        let Some(end) = start.find(['\n', '\r']) else { return "" };
        rest = start[end..].trim_start_matches(['\n', '\r', ' ', '\t']);
    }
}

#[cfg(windows)]
const LINE_BREAK_IN_QUOTES: &str ="not run: cmd.exe ends a command at its first line break, and this one has a line break inside its quotes, so only a part of it would run. Nothing was started. Give it on one line: for a commit message several `-m`, for code `python -c` or `node -e` directly, or a file.";

/// What code on several lines goes through, if what has been written of this line ends in a flag that takes code to run.
#[cfg(windows)]
fn runs_code(written: &str) -> Option<&'static str> {
    let command = written.rsplit(['&', '|', '(']).next().unwrap_or(written).trim_start();
    let mut words = command.split_whitespace();
    let program = words.next()?.trim_matches('"').to_ascii_lowercase();
    let program = program.rsplit(['\\', '/']).next().unwrap_or(&program).trim_end_matches(".exe").to_string();
    let flag = words.next_back()?;
    if (program == "python" || program == "python3" || program == "py") && flag == "-c" {
        return Some("exec");
    }
    (program == "node" && matches!(flag, "-e" | "--eval" | "-p" | "--print")).then_some("eval")
}

/// Where an argument that opened with a double quote ends, as a program's own reading of its command line finds it: `\"` is a quote inside it.
#[cfg(windows)]
fn argument_end(text: &str) -> Option<usize> {
    let mut slashes = 0;
    for (at, c) in text.char_indices() {
        match c {
            '\\' => slashes += 1,
            '"' if slashes % 2 == 0 => return Some(at),
            _ => slashes = 0,
        }
        if c == '"' {
            slashes = 0;
        }
    }
    None
}

/// That argument as the program would get it.
#[cfg(windows)]
fn argument_text(raw: &str) -> String {
    let (mut text, mut slashes) = (String::with_capacity(raw.len()), 0);
    for c in raw.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                text.extend(std::iter::repeat_n('\\', slashes / 2));
                text.push('"');
                slashes = 0;
            }
            _ => {
                text.extend(std::iter::repeat_n('\\', slashes));
                text.push(c);
                slashes = 0;
            }
        }
    }
    text.extend(std::iter::repeat_n('\\', slashes));
    text
}

/// As the inside of a single-quoted string in Python and in JavaScript. With no double quote left, all of it is one quoted stretch to cmd, which then takes nothing in it for its own; nor a `%`, which it would expand even there.
#[cfg(windows)]
fn escaped_for_a_string(code: &str) -> String {
    let mut out = String::with_capacity(code.len() + 16);
    for c in code.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\x22"),
            '%' => out.push_str("\\x25"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

/// That one step of a line was refused says nothing of the steps before it, and the exit code is only the last one's.
#[cfg(windows)]
const DRIVE_ROOT_NOTE: &str = "note: the refusal of access and the exit code below come from `dir` or a command like it, and not from anything else in this line: what ran before it did run, and it is not about your file or your rights. {}\n";

/// A `git init` this can do first, and what is left of the line after it. The words it takes are only those that change nothing but the name of the first branch and how much is said.
#[cfg(windows)]
fn git_init_first(line: &str) -> Option<(Vec<String>, String)> {
    let line = line.trim();
    if line.contains(['\n', '\r']) {
        return None;
    }
    let (first, rest) = match line.find('&') {
        Some(at) => (&line[..at], line[at..].trim_start_matches('&').trim_start()),
        None => (line, ""),
    };
    if first.contains(['|', '>', '<', '"', '(', ')', '%', '^']) {
        return None;
    }
    let mut words = first.split_whitespace();
    if !(words.next()?.eq_ignore_ascii_case("git") && words.next()? == "init") {
        return None;
    }
    let mut kept = Vec::new();
    while let Some(word) = words.next() {
        match word {
            "." => {}
            "-q" | "--quiet" => kept.push(word.to_string()),
            "-b" | "--initial-branch" => kept.extend([word.to_string(), words.next()?.to_string()]),
            _ if word.starts_with("--initial-branch=") => kept.push(word.to_string()),
            _ => return None,
        }
    }
    Some((kept, rest.to_string()))
}

/// `live` has what it prints as it prints it.
#[cfg(windows)]
fn execute_command(arguments: &Value, workspace: &Path, cancel: &AtomicBool, live: Option<Arc<Live>>) -> Result<String, String> {
    let line = on_one_line(text_arg(arguments, "command")?)?;
    let line = line.as_ref();
    let cwd = resolve(workspace, arguments.get("cwd").and_then(Value::as_str).unwrap_or("."));
    let timeout = Duration::from_secs(arguments.get("timeout_seconds").and_then(Value::as_u64).unwrap_or(180).clamp(1, 3600));
    // Under the user folder `git init` cannot work in the sandbox: it looks at every folder on the way, and a container cannot be let look at C:\\Users. It makes only `.git` in a folder
    // the command may write in, with no hooks to bring, so the app does that much and the rest of the line runs as ever.
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    // First: all that follows here looks in that folder as the person, and it may be anywhere, a share on another machine included.
    crate::sandbox::writable_root(&cwd, &resolve(workspace, "."), home.as_deref()).map_err(|error| format!("{error}. The command was not started; changing its spelling cannot repair this"))?;
    // (else the system's own words, "the directory name is invalid", read as a misspelling: the folder a command would make cannot be the one it starts in)
    if !cwd.is_dir() {
        return Err(format!("not run: {} does not exist yet, and a command starts in its cwd. Nothing was started. Run it from a folder that is there (with no cwd: the workspace) \
                            and let the command make the folder, or make it first", cwd.display()));
    }
    // `cd <the folder it runs in> && …`, a habit from shells that start elsewhere: in the sandbox `cd` to a full path under the user folder is refused ("Access is denied"),
    // as cmd looks at every folder on the way, so that part is left out and said to be.
    let (line, cd_left_out) = match after_cd_to(line, &cwd) {
        Some((cd, rest)) => (rest, Some(cd)),
        None => (line, None),
    };
    let started_with_init = git_init_first(line);
    // What would be refused is refused before the app makes anything: a refusal says nothing was started, and `.git` would have been.
    let after_init: &str = started_with_init.as_ref().map_or(line, |(_, rest)| rest);
    if let Some(word) = opened_on_the_desktop(after_init, &cwd) {
        return Err(format!("not run: `{word}` would open on the user's screen and not run here. Nothing was started. Name the program that runs it, as in `python x.py` or `node x.js`; a page or a picture cannot be opened from a command."));
    }
    let mut said_by_init = String::new();
    let line: &str = if let Some((words, rest)) = &started_with_init {
        let mut command = Command::new(crate::bundled::git_program());
        command.args(["init", "--template="]).args(words).current_dir(&cwd).stdin(Stdio::null());
        no_window(&mut command);
        let output = command.output().map_err(|error| format!("git: {error}"))?;
        said_by_init = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        if !output.status.success() {
            return Ok(format!("stdout:\n\nstderr:\n{said_by_init}\nexit_code: {}", output.status.code().unwrap_or(1)));
        }
        if rest.is_empty() {
            // (and if the line goes on, what the rest of it does there is a command's like any other)
            crate::sandbox::note_made_by_the_app(&crate::sandbox::writable_root(&cwd, &resolve(workspace, "."), home.as_deref())?, &cwd);
            return Ok(format!("stdout:\n{said_by_init}\nstderr:\n\nexit_code: 0"));
        }
        rest
    } else {
        line
    };
    let network = crate::safety::wants_network(arguments);
    let started = Instant::now();
    let before = file_states(&cwd);
    // As the person, allowed by them: `dir` and `del` work there as they are, and the line runs as it was written and shown.
    let as_person = crate::safety::outside_sandbox(arguments);
    // (as line_that_runs_at makes it, which is what the person was shown: the folder here is the one after a leading `cd` was left out)
    let runs = if as_person { line.to_string() } else { in_sandbox_form(line, &cwd) };
    // One cargo command under the user folder, in a workspace that names its members by a pattern, which cargo expands looking at each folder from the drive's root: a
    // container may not look at C:\Users, so this one runs with a restricted token, reading as the person does and writing in the workspace alone (`run_live_restricted`).
    // Judged on the line as the model wrote it, which is what the gate judged and the person allowed (`REASON_CARGO_OUTSIDE`), and on what runs, which must be that line.
    let root = resolve(workspace, ".");
    let restricted = !as_person && cargo_reads_above(text_arg(arguments, "command")?, &cwd, &root, home.as_deref()) && cargo_reads_above(&runs, &cwd, &root, home.as_deref());
    // Spelled as `cwd` is, or a path with `..` in it would not be seen as holding it.
    let done = if as_person {
        crate::sandbox::run_live_as_person(&runs, &cwd, &root, timeout, cancel, live)
    } else if restricted {
        crate::sandbox::run_live_restricted(&runs, &cwd, &root, network, timeout, cancel, live)
    } else {
        crate::sandbox::run_live(&runs, &cwd, &root, network, timeout, cancel, live)
    };
    let done = done.map_err(|error| {
        // (the app's `git init` has run by then, and what is said is what is there)
        let started = if started_with_init.is_some() { "`git init` ran, and made .git there; the rest of the line was not started" } else { "The command was not started" };
        format!("{error}. {started}; changing its spelling cannot repair this")
    })?;
    let (out, err) = (crate::sandbox::decode(&done.stdout), crate::sandbox::decode(&done.stderr));
    let mut report = String::new();
    if as_person {
        // (a model that checked what it did with another command there asked the person again for a look: `dir` and `type` are let through, `if exist … (echo …)` is not)
        report.push_str("note: this ran outside the sandbox, as the user, who allowed it. To see what it did there, use `list_files` or `read_file`, which look anywhere with no one asked.\n");
    }
    if restricted {
        report.push_str(&format!("note: cargo ran outside the container, with a restricted token: in the container cargo cannot expand this workspace's `members` pattern under \
                                  the user folder. It could read what the user can and write only in the workspace{}.\n",
                                 if network { "" } else { ", and was pointed away from the network (CARGO_NET_OFFLINE and a proxy that is not there)" }));
    }
    // (said, and not done quietly: the model learns what its words became, and the person reads it in the result)
    if let Some(cd) = cd_left_out {
        report.push_str(&format!("note: `{cd}` was left out: the command runs in that folder already, and in the sandbox `cd` to a full path under the user folder is refused.\n"));
    }
    if runs != line {
        report.push_str(&format!("note: this ran as `{runs}`: in the sandbox `dir`, `del` and `| head` are replaced by what does the same there, cmd takes a `/` for a switch, so a path is written with `\\`, and PowerShell under the users' folder starts on a drive of its own whose root is the folder ({POWERSHELL_PLACE}:), as it cannot start in the folder itself there.\n"));
    }
    if !done.shown.is_empty() {
        report.push_str(&format!("note: it opened a window that nobody can see, as nothing from a command reaches the screen: {}. Run what prints instead of what shows.\n", done.shown.iter().map(|title| if title.is_empty() { "a dialog" } else { title.as_str() }).take(4).collect::<Vec<_>>().join(", ")));
    }
    let no_network = no_network_note(network, done.status, &out, &err);
    report.push_str(no_network.unwrap_or(""));
    report.push_str(&venv_note(&out, &err).unwrap_or_default());
    if runs.contains(GIT_HEAD_WRITTEN) && err.contains("cannot lock ref 'HEAD'") {
        report.push_str(&git_init_note(&runs, &cwd));
    }
    if let (Some(before), Some(after)) = (&before, file_states(&cwd)) {
        report.push_str(&described_changes(before, &after));
    }
    if done.status == Some(1) || done.status == Some(9009) {
        // "Not found" from a folder the sandbox cannot read says the wrong thing: the program is there. 9009 is cmd's code for it, in any language.
        let looks_absent = done.status == Some(9009) || err.contains("not recognized") || err.contains("認識されていません");
        if looks_absent && crate::bundled::root().is_none() {
            report.push_str(crate::bundled::NOT_THERE);
        }
        let closed = crate::sandbox::closed_tool_folders();
        if looks_absent && !closed.is_empty() {
            let names: Vec<String> = closed.iter().map(|folder| folder.display().to_string()).collect();
            report.push_str(&format!("note: the sandbox cannot read these folders on PATH, so what is in them looks absent: {}. They are not missing, and their full paths are closed too: do not try them. Tell the user that /tools opens them to commands, with one Windows prompt.\n", names.join(", ")));
        }
    }
    if refused_for_the_drive_root(line, &out, &err, system_access_denied()) {
        report.push_str(&DRIVE_ROOT_NOTE.replace("{}", crate::prompt::NO_DRIVE_ROOT));
    }
    if !done.disarmed.is_empty() {
        report.push_str(&format!("note: {} Git does not read what was moved, and the user can put back any of it that was theirs. A hook or such a setting has to come from the user.\n", done.disarmed.join(" ")));
    }
    if let Some(why) = &done.unguarded {
        report.push_str(&format!("note: after this command the app found what it cannot keep a command from planting something in, to run outside the sandbox: {why}. The next command will not start until the user has put that right.\n"));
    }
    if let Some(cap) = done.out_of_memory {
        report.push_str(&format!("note: this reached the {} GiB that commands may use on this machine, and was refused more. It needs to use less at a time.\n", cap >> 30));
    }
    // (with the cause said above, a traceback's middle tells nothing more, and it is read again with every later request)
    let mut notes = if as_person { String::new() } else { sandbox_notes(line, &resolve(workspace, "."), done.status, &err) };
    if let Some(note) = chrome_note(&out, &err).filter(|_| !as_person) {
        notes.push_str(note);
    }
    // (the stand-in for `del` finds no file outside the workspace, where the container sees none, and says it could not find it: that is the sandbox's refusal too)
    if !as_person && done.status != Some(0) && !notes.contains(OUTSIDE_READ_NOTE) && out.contains("Could Not Find") && names_outside(line, &resolve(workspace, ".")) {
        notes.push_str(OUTSIDE_READ_NOTE);
    }
    // (a restricted token reads anywhere the user can: what it is refused outside the workspace is a write)
    report.push_str(&if restricted { notes.replace(OUTSIDE_READ_NOTE, OUTSIDE_WRITE_NOTE) } else { notes });
    let most_err = if no_network.is_some() { MAX_COMMAND_ERR / 6 } else { MAX_COMMAND_ERR };
    let whole = (out.len() > MAX_COMMAND_OUT || err.len() > most_err).then(|| kept_whole(&out, &err)).flatten();
    report.push_str(&format!("stdout:\n{said_by_init}{}\nstderr:\n{}\n", cap_evenly(&out, MAX_COMMAND_OUT, whole.as_deref()), cap_evenly(&err, most_err, whole.as_deref())));
    let ended = match done.status {
        Some(code) => format!("exit_code: {code}"),
        None => format!("exit_code: none, {} after {} s", done.ended, started.elapsed().as_secs()),
    };
    // (first as well as last: how it ended is read before what it printed, as a model was taught to read a command's result; the last line stays what the app reads)
    report.insert_str(0, &format!("{ended}\n"));
    report.push_str(&ended);
    Ok(report)
}

#[cfg(not(windows))]
fn execute_command(arguments: &Value, workspace: &Path, cancel: &AtomicBool, live: Option<Arc<Live>>) -> Result<String, String> {
    let line = text_arg(arguments, "command")?;
    let cwd = resolve(workspace, arguments.get("cwd").and_then(Value::as_str).unwrap_or("."));
    let timeout = Duration::from_secs(arguments.get("timeout_seconds").and_then(Value::as_u64).unwrap_or(180).clamp(1, 3600));
    let mut command = shell(line);
    command.current_dir(&cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    no_window(&mut command);
    let mut child = command.spawn().map_err(|error| format!("{}: {error}", cwd.display()))?;
    let capture = |pipe: Option<Box<dyn Read + Send>>| {
        let live = live.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let mut block = [0u8; 64 * 1024];
                while let Ok(count @ 1..) = pipe.read(&mut block) {
                    bytes.extend_from_slice(&block[..count]);
                    if let Some(live) = &live {
                        live.push(&block[..count]);
                    }
                }
            }
            bytes
        })
    };
    let stdout = capture(child.stdout.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>));
    let stderr = capture(child.stderr.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>));
    let started = Instant::now();
    let (status, ended) = loop {
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(status) => break (Some(status), ""),
            None if cancel.load(Ordering::Relaxed) => break (None, "cancelled"),
            None if started.elapsed() > timeout => break (None, "timed out"),
            None => std::thread::sleep(Duration::from_millis(30)),
        }
    };
    if status.is_none() {
        kill_tree(child.id());
        let _ = child.kill();
        let _ = child.wait();
    }
    let read = |handle: std::thread::JoinHandle<Vec<u8>>| String::from_utf8_lossy(&handle.join().unwrap_or_default()).into_owned();
    let (out, err) = (read(stdout), read(stderr));
    let mut report = String::from("sandbox: none (the person approved or allowed this command)\n");
    // (all of it kept, as a sandboxed command's is: the part cut out is read back, not run again)
    let whole = (out.len() > MAX_OUTPUT || err.len() > MAX_OUTPUT / 2).then(|| kept_whole(&out, &err)).flatten();
    report.push_str(&format!("stdout:\n{}\nstderr:\n{}\n", cap_evenly(&out, MAX_OUTPUT, whole.as_deref()), cap_evenly(&err, MAX_OUTPUT / 2, whole.as_deref())));
    let ended = match status {
        Some(status) => format!("exit_code: {}", status.code().unwrap_or(-1)),
        None => format!("exit_code: none, {ended} after {} s", started.elapsed().as_secs()),
    };
    report.insert_str(0, &format!("{ended}\n"));
    report.push_str(&ended);
    Ok(report)
}

/// Current reports lead with the exit status. Older ones put it after stderr, before any file-change notes.
pub fn command_exit_status(output: &str) -> Option<&str> {
    let mut lines = output.lines();
    lines.next()?.strip_prefix("exit_code:")
        .or_else(|| lines.rev().find_map(|line| line.strip_prefix("exit_code:")))
        .map(str::trim)
}

/// One line saying what a call is about, for the transcript and the approval note.
pub fn summary(tool: &str, arguments: &Value) -> String {
    let text = |name: &str| arguments.get(name).and_then(Value::as_str).unwrap_or("");
    match tool {
        "read_file" | "list_files" | "create_file" => text("path").to_string(),
        "edit" => edit_path(arguments).unwrap_or("").to_string(),
        "check_page" => match crate::page_check::served_url(arguments) {
            Some(url) => url.split(['?', '#']).next().unwrap_or(url).to_string(),
            None => text("path").to_string(),
        },
        "search_files" => format!("{} in {}", text("pattern"), arguments.get("path").and_then(Value::as_str).unwrap_or(".")),
        "execute_command" => text("command").lines().next().unwrap_or("").to_string(),
        "apply_patch" => {
            let named = arguments.get("expected_files").and_then(Value::as_array).filter(|files| !files.is_empty()).or_else(|| arguments.get("edits").and_then(Value::as_array));
            let mut files: Vec<&str> = named.into_iter().flatten().filter_map(|item| item.as_str().or_else(|| item.get("path")?.as_str())).collect();
            let mut seen = std::collections::HashSet::new();
            files.retain(|file| seen.insert(*file));
            // (a saved call has the shape the model wrote it in)
            if files.is_empty() {
                return text("path").to_string();
            }
            files.join(", ")
        }
        "show_diff" => text("scope").to_string(),
        "present" => arguments.get("paths").and_then(Value::as_array).map_or(String::new(), |paths| paths.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")),
        "web_read" => text("url").to_string(),
        "find_files" => text("pattern").to_string(),
        "skill" => text("name").to_string(),
        "remember" => text("note").chars().take(80).collect(),
        "forget" => text("words").to_string(),
        "command_status" | "command_stop" => text("id").to_string(),
        "wake_at" => format!("{}: {}", arguments.get("delay_seconds").and_then(Value::as_u64).map_or(String::new(), |seconds| format!("{seconds} s")), text("note").chars().take(100).collect::<String>()),
        "explore" => text("task").lines().next().unwrap_or("").chars().take(100).collect(),
        "run_code" => text("description").lines().next().unwrap_or("").chars().take(100).collect(),
        _ if tool.starts_with("mcp__") => arguments.to_string().chars().take(120).collect(),
        "web_search" => arguments.get("queries").and_then(Value::as_array).map_or(String::new(), |queries| queries.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")),
        _ => String::new(),
    }
}

#[cfg(test)]
mod power_tests {
    use super::*;

    #[test]
    fn a_search_stops_within_one_file_and_keeps_matches_on_timeout() {
        let dir = crate::in_container::tempdir().unwrap();
        let file = dir.path().join("rows.txt");
        std::fs::write(&file, format!("needle\n{}needle\n", "row\n".repeat(1024))).unwrap();
        let args = json!({"path": "rows.txt", "pattern": "needle"});
        let search = |expired: Result<bool, String>| {
            let mut checks = 0;
            search_files_until(&args, dir.path(), &[], || {
                checks += 1;
                if checks >= 3 { expired.clone() } else { Ok(false) }
            })
        };
        let partial = search(Ok(true)).unwrap();
        assert!(partial.contains("rows.txt:1:needle") && partial.contains("the time ran out; 1 matches so far"), "{partial}");
        assert_eq!(search(Err("cancelled".into())), Err("cancelled".into()));
        let complete = search(Ok(false)).unwrap();
        assert!(complete.contains("rows.txt:1026:needle"));
    }

    #[test]
    fn stopping_a_file_walk_returns_cancellation_even_without_matches() {
        let dir = crate::in_container::tempdir().unwrap();
        std::fs::write(dir.path().join("found.txt"), "x").unwrap();
        let mut bench = Workbench::new(dir.path());
        for (tool, args, expected) in [
            ("list_files", json!({"recursive": true}), "found.txt"),
            ("find_files", json!({"pattern": "*.missing"}), "no file"),
        ] {
            let cancel = AtomicBool::new(true);
            assert_eq!(bench.run(tool, &args, dir.path(), &cancel), Err("cancelled".into()), "{tool}");
            cancel.store(false, Ordering::Relaxed);
            let output = bench.run(tool, &args, dir.path(), &cancel).unwrap();
            assert!(output.starts_with(expected), "{tool}: {output}");
        }
    }

    #[test]
    fn files_are_found_by_name_and_what_git_ignores_is_not() {
        let dir = crate::in_container::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "skip.rs\n").unwrap();
        for name in ["src/a.rs", "src/deep/b.rs", "src/c.txt", "skip.rs"] {
            std::fs::write(dir.path().join(name), "x").unwrap();
        }
        let cancel = AtomicBool::new(false);
        let found = find_files(&json!({"pattern": "**/*.rs"}), dir.path(), &cancel).unwrap();
        assert_eq!(found, "src/a.rs\nsrc/deep/b.rs");
        assert!(find_files(&json!({"pattern": "*.zig"}), dir.path(), &cancel).unwrap().starts_with("no file"));
        assert!(find_files(&json!({}), dir.path(), &cancel).is_err());
    }

    #[test]
    fn an_edit_that_misses_says_where_its_line_is() {
        assert!(nearest_line("a\n  fn  main( ) {\nb", "fn main( ) {\n  x").contains("line 2"));
        assert_eq!(nearest_line("a\nb", "nowhere"), "");
    }

    #[test]
    fn a_missing_patch_shows_a_short_unique_span_with_its_actual_indentation() {
        let hint = nearest_line("prefix\r\n    色 = 青;  \r\n\tfinish();\r\nunrelated", "色  =  青;\n  finish();");
        assert!(hint.ends_with("line 2 is:\n    色 = 青;  \n\tfinish();"), "{hint}");
        let ambiguous = nearest_line("  color\na\n  color\nb", "color\nz");
        assert!(ambiguous.contains("line 1, 3") && !ambiguous.contains('\n'));
        let long = format!("color\n{}", "長".repeat(300));
        assert!(!nearest_line(&long, "color\nz").contains('長'));
        assert!(!nearest_line("color\na\nb\nc\nd\ne", "color\nz\nz\nz\nz\nz").contains('\n'));
    }

    #[test]
    fn the_tools_of_a_session_depend_on_what_is_on() {
        let names = |value: Value| value.as_array().unwrap().iter().map(|tool| tool["function"]["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let plain = names(schemas_for(false, false, false, Vec::new(), false));
        assert!(plain.contains(&"find_files".to_string()) && !plain.contains(&"skill".to_string()) && !plain.contains(&"web_read".to_string()) && !plain.contains(&"remember".to_string()));
        let all = names(schemas_for(true, true, true, vec![json!({"type": "function", "function": {"name": "mcp__s__t"}})], false));
        assert!(all.contains(&"mcp__s__t".to_string()));
        for tool in ["skill", "web_read", "remember", "forget", "command_status"] {
            assert!(all.contains(&tool.to_string()), "{tool}");
        }
        let schemas = schemas_for(false, false, false, Vec::new(), false);
        let command = schemas.as_array().unwrap().iter().find(|tool| tool["function"]["name"] == "execute_command").unwrap();
        assert!(command["function"]["parameters"]["properties"].get("background").is_some());
    }

    /// A background job's output is reported once in a report of all of them, and after that in a line; asked by its id, again in full.
    /// A background job runs without the timeout the model gave, which is how long a call waits.
    #[test]
    fn an_ended_jobs_output_is_reported_once_among_all_and_its_timeout_is_not_its_life() {
        let mut bench = Workbench::default();
        let done: CommandResult = Arc::new(Mutex::new(Some(Ok("BUILD OUTPUT".to_string()))));
        bench.add_job(&json!({"command": "cargo build"}), Instant::now(), Arc::new(AtomicBool::new(false)), done, Arc::default()).unwrap();
        let first = bench.job_status(&json!({}), &AtomicBool::new(false)).unwrap();
        let second = bench.job_status(&json!({}), &AtomicBool::new(false)).unwrap();
        let by_id = bench.job_status(&json!({"id": "job1"}), &AtomicBool::new(false)).unwrap();
        assert!(first.contains("BUILD OUTPUT") && !second.contains("BUILD OUTPUT") && second.contains("as reported before") && by_id.contains("BUILD OUTPUT"), "{first}\n{second}\n{by_id}");
        let given = background_arguments(&json!({"command": "cargo build", "background": true, "timeout_seconds": 60}));
        assert!(given.get("timeout_seconds").is_none() && given["command"] == "cargo build");
    }

    /// A job still running shows what it has printed, and the next look only what came after; a look past what one shows says how much it left out.
    #[test]
    fn a_running_job_shows_what_it_printed_since_the_last_look() {
        let mut bench = Workbench::default();
        let live: Arc<Live> = Arc::default();
        bench.add_job(&json!({"command": "npm run dev"}), Instant::now(), Arc::new(AtomicBool::new(false)), Arc::default(), live.clone()).unwrap();
        assert!(bench.job_status(&json!({}), &AtomicBool::new(false)).unwrap().ends_with("it has printed nothing yet"));
        live.push("compiling\nlistening on http://localhost:5173\n".as_bytes());
        let first = bench.job_status(&json!({"id": "job1"}), &AtomicBool::new(false)).unwrap();
        assert!(first.contains("is running") && first.contains("listening on http://localhost:5173"), "{first}");
        assert!(bench.job_status(&json!({}), &AtomicBool::new(false)).unwrap().contains("it has printed nothing since the last look. Its end wakes you"), "a look again with nothing new is a wait");
        live.push("GET / 200\n".as_bytes());
        let next = bench.job_status(&json!({}), &AtomicBool::new(false)).unwrap();
        assert!(next.contains("since the last look:\nGET / 200") && !next.contains("listening"), "{next}");
        // (a look starting in the middle of a character starts after it)
        live.push("あ".as_bytes());
        live.push(&[b'x'; LIVE_SHOWN]);
        let (shown, _) = live.since(0, LIVE_SHOWN + 1);
        assert!(shown.starts_with('(') && !shown.contains('\u{FFFD}') && !shown.contains('あ') && shown.ends_with('x'), "{shown}");
    }

    /// A command's result says how it ended on its first line as well as its last.
    #[cfg(windows)]
    #[test]
    fn a_commands_result_says_how_it_ended_first_and_last() {
        let dir = crate::in_container::tempdir().unwrap();
        let (mut bench, cancel) = (Workbench::default(), AtomicBool::new(false));
        let said = bench.run("execute_command", &json!({"command": "echo hello& exit /b 3"}), dir.path(), &cancel).unwrap();
        assert!(said.starts_with("exit_code: 3\n") && said.ends_with("exit_code: 3") && said.contains("hello"), "{said}");
    }

    /// What a real command in the sandbox prints reaches a look while it still runs: a server's ready line before it ends, which it never does.
    #[cfg(windows)]
    #[test]
    fn what_a_command_in_the_sandbox_prints_is_seen_while_it_runs() {
        let dir = crate::in_container::tempdir().unwrap();
        let (mut bench, cancel) = (Workbench::default(), AtomicBool::new(false));
        // (spelled with a caret, so that the line itself does not say what it prints. It runs far longer than it is waited for: a whole run of the tests starts
        // so many sandboxed commands at once that one began to print only after 25 s, and the test said it had not, though it had nothing to do with the look)
        let line = "echo ready^-now& powershell -NoProfile -Command \"Start-Sleep -Seconds 300\"";
        bench.run("execute_command", &json!({"command": line, "background": true}), dir.path(), &cancel).unwrap();
        let started = Instant::now();
        let seen = loop {
            let said = bench.job_status(&json!({"id": "job1"}), &AtomicBool::new(false)).unwrap();
            if said.contains("ready-now") || started.elapsed() > Duration::from_secs(120) {
                break said;
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        bench.job_stop(&json!({"id": "job1"})).unwrap();
        // (looked at before it printed, a look after says what came since)
        assert!(seen.contains("is running") && seen.contains("printed since the last look:\nready-now"), "{seen}");
    }

    /// `wake_on` belongs to a command started in the background, and has to be a pattern: what is wrong with it is said before anything is started.
    #[test]
    fn wake_on_is_for_a_background_command_and_has_to_be_a_pattern() {
        let dir = crate::in_container::tempdir().unwrap();
        let (mut bench, cancel) = (Workbench::default(), AtomicBool::new(false));
        let waited = bench.run("execute_command", &json!({"command": "echo x", "wake_on": "x"}), dir.path(), &cancel).unwrap_err();
        assert!(waited.contains("background: true") && waited.contains("not started"), "{waited}");
        let bad = bench.run("execute_command", &json!({"command": "echo x", "background": true, "wake_on": "("}), dir.path(), &cancel).unwrap_err();
        assert!(bad.contains("regular expression") && bad.contains("not started"), "{bad}");
        assert!(bench.job_status(&json!({}), &AtomicBool::new(false)).unwrap().starts_with("no command"), "nothing was started");
        let schemas = schemas();
        let tool = |name: &str| schemas.as_array().unwrap().iter().find(|tool| tool["function"]["name"] == name).unwrap_or_else(|| panic!("no {name}")).clone();
        assert!(tool("execute_command")["function"]["parameters"]["properties"].get("wake_on").is_some());
        assert_eq!(tool("wake_at")["function"]["parameters"]["required"], json!(["delay_seconds", "note"]));
    }

    #[test]
    fn a_session_with_no_background_command_says_so() {
        let bench = Workbench::default();
        assert!(bench.job_status(&json!({}), &AtomicBool::new(false)).unwrap().starts_with("no command"));
        assert!(bench.job_stop(&json!({"id": "job1"})).is_err());
    }
}

#[cfg(test)]
mod change_tests {
    use super::*;

    fn run_git(dir: &Path, args: &[&str]) {
        let mut all = vec!["-c", "user.name=t", "-c", "user.email=t@example.com"];
        all.extend(args);
        git(dir, &all).unwrap();
    }

    #[test]
    fn what_a_repository_plants_in_its_own_config_does_not_run_when_the_app_asks_git_anything() {
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let dir = crate::in_container::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q"]);
        std::fs::write(dir.path().join(".gitattributes"), "*.txt filter=planted diff=planted\n").unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "first"]);
        // As a command in the sandbox could write them.
        let config = dir.path().join(".git/config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        for line in ["[core]\nfsmonitor = \"echo planted > marker1.txt\"", "[filter \"planted\"]\nclean = \"echo planted > marker2.txt; cat\"", "[diff \"planted\"]\ntextconv = \"echo planted > marker3.txt; cat\"", "[diff]\nexternal = \"echo planted > marker4.txt\""] {
            text.push_str(line);
            text.push('\n');
        }
        std::fs::write(&config, text).unwrap();
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        assert!(git_info(dir.path()).is_some());
        assert!(workspace_changes(dir.path()).unwrap().iter().any(|change| change.path == "a.txt"));
        let said = show_diff(&json!({"scope": "workspace"}), dir.path(), None).unwrap();
        assert!(said.contains("+two"), "{said}");
        for marker in ["marker1.txt", "marker2.txt", "marker3.txt", "marker4.txt"] {
            assert!(!dir.path().join(marker).exists(), "{marker} ran");
        }
    }

    #[test]
    fn a_file_changed_a_file_new_and_a_file_deleted_are_each_a_row() {
        let dir = crate::in_container::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q"]);
        std::fs::write(dir.path().join("kept.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.path().join("gone.txt"), "x\n").unwrap();
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "first"]);
        std::fs::write(dir.path().join("kept.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::remove_file(dir.path().join("gone.txt")).unwrap();
        std::fs::write(dir.path().join("新しい.txt"), "a\nb\n").unwrap();
        let changes = workspace_changes(dir.path()).unwrap();
        let rows: Vec<(&str, usize, usize)> = changes.iter().map(|change| (change.path.as_str(), change.added, change.removed)).collect();
        assert_eq!(rows, [("gone.txt", 0, 1), ("kept.txt", 2, 1), ("新しい.txt", 2, 0)]);
        assert!(changes[1].diff.contains("+TWO") && !changes[1].diff.starts_with("diff --git"));
    }

    #[test]
    fn a_repository_with_no_commit_shows_its_new_files() {
        let dir = crate::in_container::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q"]);
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        std::fs::write(dir.path().join("b.bin"), [0xFFu8, 0, 1]).unwrap();
        let changes = workspace_changes(dir.path()).unwrap();
        assert_eq!(changes.iter().map(|change| change.path.as_str()).collect::<Vec<_>>(), ["a.txt", "b.bin"]);
        assert_eq!(changes[0].added, 1);
        assert!(changes[1].diff.contains("not text"));
    }

    #[cfg(windows)]
    #[test]
    fn what_a_command_changed_is_said_whatever_its_exit_code() {
        let dir = crate::in_container::tempdir().unwrap();
        std::fs::write(dir.path().join("kept.txt"), "a").unwrap();
        std::fs::write(dir.path().join("gone.txt"), "a").unwrap();
        std::fs::write(dir.path().join("grows.txt"), "a").unwrap();
        let before = file_states(dir.path()).unwrap();
        assert_eq!(described_changes(&before, &before), "");
        std::fs::remove_file(dir.path().join("gone.txt")).unwrap();
        std::fs::write(dir.path().join("grows.txt"), "abc").unwrap();
        std::fs::write(dir.path().join("three.js"), "12345").unwrap();
        let said = described_changes(&before, &file_states(dir.path()).unwrap());
        assert!(said.contains("- created three.js (5 bytes)") && said.contains("- changed grows.txt (3 bytes)") && said.contains("- deleted gone.txt") && !said.contains("kept.txt"), "{said}");
    }

    #[cfg(windows)]
    #[test]
    fn a_failure_with_no_network_says_so_at_the_top_and_only_then() {
        let traceback = "Traceback (most recent call last):\nurllib.error.URLError: <urlopen error [Errno 11001] getaddrinfo failed>\n";
        assert!(no_network_note(false, Some(1), "", traceback).unwrap().contains("network=true"));
        assert!(no_network_note(false, Some(6), "curl: (6) Could not resolve host: unpkg.com", "").is_some());
        assert!(no_network_note(true, Some(1), "", traceback).is_none(), "it had the network");
        assert!(no_network_note(false, Some(0), "", traceback).is_none(), "it did not fail");
        assert!(no_network_note(false, Some(1), "", "TypeError: nothing to do with it").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn what_would_open_on_the_desktop_is_told_from_what_runs() {
        let dir = crate::in_container::tempdir().unwrap();
        std::fs::write(dir.path().join("data.xyz"), "x").unwrap();
        std::fs::write(dir.path().join("run.bat"), "@echo x").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let opens = |line: &str| opened_on_the_desktop(line, dir.path());
        for (line, word) in [
            ("x.py", "x.py"), ("scripts\\x.py --flag", "scripts\\x.py"), ("\"C:\\a b\\x.ps1\" -v", "C:\\a b\\x.ps1"), ("@a.sh>out.txt", "a.sh"), ("data.xyz", "data.xyz"),
            // Anywhere in a line.
            ("cargo build && test.py", "test.py"), ("echo a & y.py", "y.py"), ("type a | z.py", "z.py"), ("cargo build || w.py", "w.py"), ("(q.py)", "q.py"), ("echo a\nb.py", "b.py"),
            // Through what runs what it is given.
            ("call x.py", "x.py"), ("cmd /c x.py", "x.py"), ("cmd /d /s /c \"echo a & x.py\"", "x.py"), ("cmd.exe /k x.html", "x.html"),
            // What `start` would open and not run.
            ("start index.html", "start index.html"), ("start \"\" index.html", "start index.html"), ("start /b x.py", "start x.py"), ("start https://example.com", "start https://example.com"),
            ("start sub", "start sub"), ("start .", "start ."), ("start", "start"), ("start \"title\"", "start"), ("START no-such-program", "START no-such-program"), ("git status && start a.txt", "start a.txt"),
        ] {
            assert_eq!(opens(line).as_deref(), Some(word), "{line}");
        }
        for line in [
            // A program, with files as its arguments.
            "python x.py", "py -3 x.py", "node x.js", "tool.exe a.py", "C:\\Python314\\python.exe x.py", "python3.12 x.py", "run.bat", "cargo test && git status", "a.unknown",
            // A file only as an argument.
            "echo x.py", "type a.txt", "mkdir start", "echo start",
            // `&` and `|` that split nothing.
            "python -c \"print(1) & x.py\"", "echo \"a && b.py c\"", "echo a^&x.py",
            // A shell given a program.
            "cmd /c echo hi", "cmd /c \"python x.py\"", "call run.bat",
            // `start` of a program it finds.
            "start cmd /c echo hi", "start \"\" cmd", "start \"title\" /b cmd", "start /wait cmd", "start /d sub /wait cmd", "start \"\" run.bat", "start /b run.bat", "start run",
        ] {
            assert_eq!(opens(line), None, "{line}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_cd_by_full_path_to_the_folder_it_runs_in_is_left_out() {
        let dir = crate::in_container::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let here = dir.path().display().to_string();
        assert_eq!(after_cd_to(&format!("cd {here} && python x.py"), dir.path()), Some((format!("cd {here}").as_str(), "python x.py")));
        assert_eq!(after_cd_to(&format!("CD /D \"{here}\" &&  dir"), dir.path()), Some((format!("CD /D \"{here}\"").as_str(), "dir")));
        assert_eq!(after_cd_to(&format!("pushd {}\\ && echo a", here.replace('\\', "/")), dir.path()).map(|(_, rest)| rest), Some("echo a"));
        for kept in [format!("cd {here}\\sub && echo a"), "cd sub && echo a".to_string(), format!("cd {here} & echo a"), format!("cd {here} && "), format!("cd {here}"),
                     format!("echo {here} && cd {here}")] {
            assert_eq!(after_cd_to(&kept, dir.path()), None, "{kept}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn cargo_in_a_workspace_of_member_patterns_under_the_user_folder_runs_restricted() {
        assert!(one_cargo_command("cargo test -p a") && one_cargo_command("  cargo.exe build --release ") && one_cargo_command("\"C:\\Program Files\\x\\cargo.exe\" test -- --nocapture"));
        // (its errors put with its output, or either let go: nothing more runs, and nothing is written but to NUL)
        for line in ["cargo test -p dogmind-app --lib 2>&1", "cargo metadata --no-deps --format-version 1 > nul", "cargo build 2>nul", "cargo check 2>&1 >NUL", "cargo test 2> nul"] {
            assert!(one_cargo_command(line), "{line}");
        }
        for more in ["cargo test 2>&1 | findstr error", "cargo build x2>&1", "cargo build 2>&1 & del x", "cargo build >nul & echo x", "cargo build 2>errors.txt"] {
            assert!(!one_cargo_command(more), "{more}");
        }
        // (a line of more than cargo would run all of it with what cargo is given: never)
        for more in ["cargo build & echo x", "cargo build && del x", "cargo test | more", "echo x && cargo build", "(cargo fmt)", "cargo build > out.txt", "cargo build ^& x",
                     "cargo run %PATH%", "cargo run !x!", "cargo build; x", "cargo build\necho x", "python cargo.py", "echo cargo", "cargo-nextest run"] {
            assert!(!one_cargo_command(more), "{more}");
        }
        let dir = crate::in_container::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("crates/a/src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\n  \"crates/*\",\n]\nresolver = \"2\"\n\n[workspace.package]\nversion = \"0.1.0\"\n").unwrap();
        std::fs::write(root.join("crates/a/Cargo.toml"), "[package]\nname = \"a\"\nversion = \"0.1.0\"\n").unwrap();
        assert!(members_by_pattern(root, root) && members_by_pattern(&root.join("crates/a"), root), "from a member's folder too");
        // (the user folder stood for by the folder above the workspace, wherever the test's folder is)
        let home = root.parent().unwrap().join("someone");
        assert!(cargo_reads_above("cargo build", &root.join("crates/a"), root, Some(&home)));
        assert!(!cargo_reads_above("python x.py", root, root, Some(&home)));
        assert!(!cargo_reads_above("cargo build", root, root, Some(Path::new(r"Q:\home\someone"))), "not under the user folder");
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/a\"]\nexclude = [\"old/*\"]\n").unwrap();
        assert!(!members_by_pattern(root, root), "members named one by one");
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers=[\"crates/*\"]\n").unwrap();
        assert!(members_by_pattern(root, root), "written without spaces");
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"solo\"\n").unwrap();
        assert!(!members_by_pattern(root, root));
    }

    /// The case a review run met: `members = ["crates/*"]` under the user folder, which cargo cannot expand in the container.
    #[cfg(windows)]
    #[test]
    fn cargo_loads_a_workspace_of_member_patterns_with_a_restricted_token() {
        if !std::process::Command::new("cargo").arg("--version").output().is_ok_and(|output| output.status.success()) {
            return;
        }
        let dir = crate::in_container::tempdir().unwrap();
        let root = dir.path();
        for name in ["a", "b"] {
            std::fs::create_dir_all(root.join("crates").join(name).join("src")).unwrap();
            std::fs::write(root.join("crates").join(name).join("Cargo.toml"), format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")).unwrap();
            std::fs::write(root.join("crates").join(name).join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        }
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n").unwrap();
        let said = execute_command(&json!({"command": "cargo metadata --offline --no-deps --format-version 1", "cwd": "crates/b"}), root, &AtomicBool::new(false), None).unwrap();
        assert!(said.contains("exit_code: 0") && said.contains("\"name\":\"a\"") && said.contains("\"name\":\"b\""), "{said}");
        let under_users = std::env::var_os("USERPROFILE").map(PathBuf::from).and_then(|home| home.parent().map(Path::to_path_buf)).is_some_and(|users| root.starts_with(users));
        assert_eq!(said.contains("with a restricted token"), under_users, "{said}");
        // (the same with its errors put with its output, as a model writes it: a dogfood run's `2>&1` ran in the container, failed on `crates/*`, and the model rewrote
        // the workspace's members)
        let merged = execute_command(&json!({"command": "cargo metadata --offline --no-deps --format-version 1 2>&1", "cwd": "crates/b"}), root, &AtomicBool::new(false), None).unwrap();
        assert!(merged.contains("exit_code: 0") && merged.contains("\"name\":\"a\""), "{merged}");
        assert_eq!(merged.contains("with a restricted token"), under_users, "{merged}");
    }

    /// Run as the model wrote them, both failed: cmd refuses a `cd` by full path under the user folder ("Access is denied"), and a cwd the command would make is no place to start.
    #[cfg(windows)]
    #[test]
    fn a_command_that_cds_to_its_own_folder_runs_and_one_whose_cwd_is_not_there_is_told_so() {
        let dir = crate::in_container::tempdir().unwrap();
        let ran = execute_command(&json!({"command": format!("cd /d \"{}\" && echo ran-here", dir.path().display())}), dir.path(), &AtomicBool::new(false), None).unwrap();
        assert!(ran.contains("ran-here") && ran.contains("was left out") && ran.contains("exit_code: 0"), "{ran}");
        let said = execute_command(&json!({"command": "echo x", "cwd": "made-later"}), dir.path(), &AtomicBool::new(false), None).unwrap_err();
        assert!(said.contains("does not exist yet") && said.contains("Nothing was started"), "{said}");
    }

    #[cfg(windows)]
    #[test]
    fn a_line_that_would_open_a_dialog_is_refused_before_any_sandbox_is_made() {
        let dir = crate::in_container::tempdir().unwrap();
        let said = execute_command(&json!({"command": "x.py"}), dir.path(), &AtomicBool::new(false), None).unwrap_err();
        assert!(said.contains("Nothing was started") && said.contains("python x.py"), "{said}");
    }

    #[cfg(windows)]
    #[test]
    fn a_refused_dir_does_not_read_as_the_whole_line_failing() {
        assert!(DRIVE_ROOT_NOTE.contains("what ran before it did run"));
    }

    #[cfg(windows)]
    #[test]
    fn a_refused_del_is_known_by_what_either_stream_says() {
        // (a line whose `2>&1` sends cmd's refusal on with the output, as a model wrote it)
        assert!(refused_for_the_drive_root("del \"tests\\_tmp.rs\" 2>&1; cargo test 2>&1", "アクセスが拒否されました。\r\n", "", ""));
        assert!(refused_for_the_drive_root("dir", "", "Access is denied.\r\n", ""));
        assert!(!refused_for_the_drive_root("cargo test 2>&1", "Access is denied.\r\n", "", ""), "no dir or del in it");
        assert!(!refused_for_the_drive_root("del a.txt", "", "", ""), "not refused");
    }

    #[test]
    fn a_session_is_told_of_the_network_as_a_thing_to_ask_for() {
        let schemas = schemas_for(false, false, false, Vec::new(), false);
        let command = schemas.as_array().unwrap().iter().find(|tool| tool["function"]["name"] == "execute_command").unwrap();
        assert!(command["function"]["parameters"]["properties"].get("network").is_some());
        assert!(command["function"]["parameters"]["properties"]["network"]["description"].as_str().unwrap().contains("Asks the user"));
    }

    #[test]
    fn an_empty_folder_is_said_to_be() {
        let dir = crate::in_container::tempdir().unwrap();
        assert!(list_files(&json!({}), dir.path(), &AtomicBool::new(false)).unwrap().ends_with("is empty"));
    }

    #[cfg(windows)]
    #[test]
    fn the_commands_that_read_the_drive_root_are_told_apart() {
        for line in ["dir", "dir /b vendor", "dir/b", "@DEL a.txt", "mkdir x & dir x", "(erase a) && echo y", "where /r . *.txt", "type a | tree /f"] {
            assert!(needs_drive_root(line), "{line}");
        }
        for line in ["mkdir dir", "echo dir", "type del.txt", "where python", "python dir.py", "rmdir /s /q vendor"] {
            assert!(!needs_drive_root(line), "{line}");
        }
    }

    #[test]
    fn outside_a_repository_it_says_so() {
        let dir = crate::in_container::tempdir().unwrap();
        assert!(workspace_changes(dir.path()).is_err());
        assert_eq!(git_info(dir.path()), None);
    }

    #[test]
    fn workspace_diffs_before_the_first_commit_compare_against_an_empty_tree() {
        let dir = crate::in_container::tempdir().unwrap();
        let root = dir.path();
        let pkg = root.join("pkg");
        run_git(root, &["init", "-q"]);
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(root.join("outside.txt"), "outside\n").unwrap();
        std::fs::write(pkg.join("file.txt"), "staged\n").unwrap();
        run_git(root, &["add", "."]);
        let show = |args| show_diff(&args, &pkg, None).unwrap();
        assert!(show(json!({})).contains("+staged"));
        assert_eq!(workspace_changes(&pkg).unwrap()[0].path, "file.txt");

        std::fs::write(pkg.join("file.txt"), "working\n").unwrap();
        std::fs::write(pkg.join("new.txt"), "untracked\n").unwrap();
        let work = show(json!({}));
        assert!(work.contains("+working") && work.contains("+untracked"), "{work}");
        assert!(!work.contains("outside") && !work.contains("staged"), "{work}");
        let one = show(json!({"paths":["file.txt"]}));
        assert!(one.contains("+working") && !one.contains("new.txt"), "{one}");
        let changes = workspace_changes(&pkg).unwrap();
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().any(|change| change.path == "file.txt" && change.diff.contains("+working")));
        assert!(show(json!({"scope":"staged"})).contains("+staged"));
        assert!(show(json!({"scope":"unstaged"})).contains("-staged"));

        std::fs::remove_file(pkg.join("file.txt")).unwrap();
        assert!(show(json!({"paths":["file.txt"]})).starts_with("no differences"));
    }

    #[test]
    fn a_repository_says_where_it_stands() {
        let dir = crate::in_container::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "work"]);
        assert_eq!(git_info(dir.path()).unwrap().branch, "work");
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "the first one"]);
        std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "staged\n").unwrap();
        run_git(dir.path(), &["add", "b.txt"]);
        std::fs::write(dir.path().join("new.txt"), "n\n").unwrap();
        let info = git_info(dir.path()).unwrap();
        assert_eq!((info.branch.as_str(), info.staged, info.changed, info.untracked, info.upstream), ("work", 1, 1, 1, None));
        assert!(info.last_commit.unwrap().starts_with("the first one"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bench() -> (tempfile::TempDir, Workbench, AtomicBool) {
        (crate::in_container::tempdir().unwrap(), Workbench::default(), AtomicBool::new(false))
    }

    #[test]
    fn every_generated_schema_has_a_tool() {
        let schemas = schemas();
        let names: Vec<&str> = schemas.as_array().unwrap().iter().map(|tool| tool["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["apply_patch", "create_file", "execute_command", "list_files", "read_file", "search_files", "show_diff", "ask_user", "submit_plan", "set_goal", "goal_status", "finish_goal", "todo", "present", "find_files", "check_page", "command_status", "command_stop", "wake_at", "explore"]);
    }

    /// `present` names files of the workspace, each once, from its root with forward slashes; a file that is not there, or a path out of the workspace, is refused, and so is none.
    #[test]
    fn present_names_files_of_the_workspace_and_nothing_else() {
        let dir = crate::in_container::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("web")).unwrap();
        std::fs::write(dir.path().join("web/index.html"), "<p>").unwrap();
        std::fs::write(dir.path().join("notes.md"), "x").unwrap();
        let files = present(&json!({"paths": ["web/index.html", "./notes.md", "web\\index.html"]}), dir.path()).unwrap();
        assert_eq!(files, ["web/index.html", "notes.md"]);
        assert!(present(&json!({"paths": ["gone.txt"]}), dir.path()).unwrap_err().contains("not a file"));
        assert!(present(&json!({"paths": ["web"]}), dir.path()).is_err(), "a folder is not a file");
        assert!(present(&json!({"paths": ["../outside.txt"]}), dir.path()).unwrap_err().contains("not in the workspace"));
        assert!(present(&json!({"paths": []}), dir.path()).is_err());
        let (_, mut bench, cancel) = bench();
        let said = bench.run("present", &json!({"paths": ["notes.md"]}), dir.path(), &cancel).unwrap();
        assert_eq!(said, "The person sees a card with notes.md.");
    }

    #[test]
    fn show_diff_outside_git_answers_in_a_line_and_never_with_gits_usage() {
        let dir = crate::in_container::tempdir().unwrap();
        for scope in ["staged", "unstaged"] {
            let said = show_diff(&json!({"scope": scope}), dir.path(), None).unwrap_err();
            assert!(said.contains("use scope session") && !said.contains('\n') && !said.contains("usage"), "{said}");
        }
        for scope in ["session", "workspace"] {
            let said = show_diff(&json!({"scope": scope}), dir.path(), None).unwrap_err();
            assert!(!said.contains('\n') && !said.contains("usage"), "{said}");
        }
    }

    #[test]
    fn show_diff_in_a_repository_goes_on_as_it_did_and_says_a_bad_path_in_a_line() {
        let dir = crate::in_container::tempdir().unwrap();
        let run = |args: &[&str]| {
            let mut all = vec!["-c", "user.name=t", "-c", "user.email=t@example.com"];
            all.extend(args);
            git(dir.path(), &all).unwrap();
        };
        run(&["init", "-q"]);
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "first"]);
        let sessions = crate::in_container::tempdir().unwrap();
        let baseline = crate::baseline::Baseline::for_session(&sessions.path().join("s.jsonl"), dir.path());
        std::fs::write(dir.path().join("a.txt"), "b\n").unwrap();
        for scope in ["session", "workspace", "unstaged"] {
            assert!(show_diff(&json!({"scope": scope}), dir.path(), Some(&baseline)).unwrap().contains("+b"), "{scope}");
        }
        assert!(show_diff(&json!({"scope": "staged"}), dir.path(), Some(&baseline)).unwrap().starts_with("no differences"));
        std::fs::write(dir.path().join("only-new.txt"), "fresh
").unwrap();
        for scope in ["session", "workspace"] {
            let said = show_diff(&json!({"scope": scope}), dir.path(), Some(&baseline)).unwrap();
            assert!(said.contains("only-new.txt") && said.contains("+fresh"), "{scope}: {said}");
        }
        assert!(!show_diff(&json!({"scope": "workspace", "paths": ["a.txt"]}), dir.path(), Some(&baseline)).unwrap().contains("only-new"), "a filter holds for new files too");
        std::fs::remove_file(dir.path().join("only-new.txt")).unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "second"]);
        assert!(show_diff(&json!({"scope": "session"}), dir.path(), Some(&baseline)).unwrap().contains("+b"), "since the session began, not since the last commit");
        assert!(show_diff(&json!({"scope": "workspace"}), dir.path(), Some(&baseline)).unwrap().starts_with("no differences"));
    }

    /// A workspace that is a folder of a larger repository: its diff and its counts are of what is under it, and the rest of the repository's
    /// changes are not shown to the model as its work (a real run's first show_diff in an empty folder printed its parent's every change).
    #[test]
    fn a_workspace_in_a_larger_repository_sees_only_its_own_changes() {
        let dir = crate::in_container::tempdir().unwrap();
        let run = |args: &[&str]| {
            let mut all = vec!["-c", "user.name=t", "-c", "user.email=t@example.com"];
            all.extend(args);
            git(dir.path(), &all).unwrap();
        };
        run(&["init", "-q"]);
        std::fs::create_dir_all(dir.path().join("work")).unwrap();
        std::fs::write(dir.path().join("secret.txt"), "old\n").unwrap();
        std::fs::write(dir.path().join("work").join("page.txt"), "a\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "first"]);
        std::fs::write(dir.path().join("secret.txt"), "not for the model\n").unwrap();
        let work = dir.path().join("work");
        let said = show_diff(&json!({"scope": "workspace"}), &work, None).unwrap();
        assert!(said.starts_with("no differences") && !said.contains("not for the model"), "{said}");
        std::fs::write(work.join("page.txt"), "b\n").unwrap();
        let said = show_diff(&json!({"scope": "unstaged"}), &work, None).unwrap();
        assert!(said.contains("+b") && !said.contains("secret"), "{said}");
        let info = git_info(&work).unwrap();
        assert_eq!(info.changed, 1, "the folder's one change, not the repository's two");
    }

    #[test]
    fn the_schemas_promise_only_what_the_tools_do() {
        let text = schemas().to_string();
        assert!(text.contains("outside git as session") && !text.contains("captured workspace baseline"));
        for gone in ["PDF", "spreadsheet", "queries", "`del` not", "risk_level", "expected_files", "if_exists", "allow_empty_file"] {
            assert!(!text.contains(gone), "{gone}");
        }
        assert!(text.contains("see a picture") && text.contains("append=true"));
    }

    #[test]
    fn create_file_asks_for_parts_unless_the_provider_shows_a_call_as_it_is_written() {
        let description = |schemas: &Value| schemas.as_array().unwrap().iter().find(|tool| tool["function"]["name"] == "create_file").unwrap()["function"]["description"].as_str().unwrap().to_string();
        let mut schemas = schemas_for(true, true, true, Vec::new(), false);
        assert_eq!(description(&schemas), CREATE_FILE_IN_PARTS);
        assert!(description(&schemas).contains("Over about 300 lines"));
        let others: Vec<Value> = schemas.as_array().unwrap().iter().filter(|tool| tool["function"]["name"] != "create_file").cloned().collect();
        for_calls_shown_as_written(&mut schemas);
        assert_eq!(description(&schemas), CREATE_FILE_WHOLE);
        assert!(description(&schemas).contains("whole in one call") && description(&schemas).contains("append=true"));
        let after: Vec<Value> = schemas.as_array().unwrap().iter().filter(|tool| tool["function"]["name"] != "create_file").cloned().collect();
        assert_eq!(after, others, "nothing else changes");
    }

    /// A program's declaration of create_file says what create_file is as it is offered, after the provider's own version of it.
    #[test]
    fn run_code_declares_create_file_as_it_is_offered() {
        let declared = |schemas: &Value| {
            let run_code = schemas.as_array().unwrap().iter().find(|tool| tool["function"]["name"] == "run_code").unwrap()["function"]["description"].as_str().unwrap().to_string();
            run_code.lines().find(|line| line.starts_with("tools.create_file(")).unwrap().to_string()
        };
        let mut schemas = schemas_for(false, false, false, Vec::new(), true);
        let in_parts = declared(&schemas);
        for_calls_shown_as_written(&mut schemas);
        let whole = declared(&schemas);
        assert_ne!(in_parts, whole);
        assert!(whole.contains("whole in one call"), "{whole}");
        assert_eq!(schemas.as_array().unwrap().last().unwrap()["function"]["name"], "run_code", "still the last of them");
    }

    #[test]
    fn a_read_says_what_it_decoded_and_when_it_went_past_the_end() {
        let (dir, mut bench, cancel) = bench();
        let mut wide = vec![0xFF, 0xFE];
        wide.extend("日本語\n".encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(dir.path().join("wide.txt"), wide).unwrap();
        let out = bench.run("read_file", &json!({"path": "wide.txt"}), dir.path(), &cancel).unwrap();
        assert!(out.contains("encoding=utf-16") && out.contains("1|日本語"), "{out}");
        #[cfg(windows)]
        {
            std::fs::write(dir.path().join("sjis.txt"), [0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA]).unwrap();
            let out = bench.run("read_file", &json!({"path": "sjis.txt"}), dir.path(), &cancel).unwrap();
            assert!(out.contains("encoding=shift_jis") && out.contains("1|日本語"), "{out}");
        }
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let past = bench.run("read_file", &json!({"path": "a.txt", "start_line": 9}), dir.path(), &cancel).unwrap();
        assert!(past.contains("only 1 lines"), "{past}");
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        assert!(bench.run("read_file", &json!({"path": "empty.txt"}), dir.path(), &cancel).unwrap().contains("the file is empty"));
    }

    #[test]
    fn a_picture_read_with_no_way_to_show_it_says_so() {
        let (dir, mut bench, cancel) = bench();
        std::fs::write(dir.path().join("a.png"), crate::images::tests::encoded(8, 8, image::ImageFormat::Png)).unwrap();
        let said = bench.run("read_file", &json!({"path": "a.png"}), dir.path(), &cancel).unwrap_err();
        assert!(said.contains("is a picture") && !said.contains("binary"), "{said}");
    }

    #[test]
    fn a_walk_stays_out_of_what_no_one_wrote_and_a_look_still_shows_it() {
        let (dir, mut bench, cancel) = bench();
        std::fs::create_dir_all(dir.path().join("node_modules/x")).unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "/Library/\n").unwrap();
        std::fs::create_dir(dir.path().join("Library")).unwrap();
        std::fs::write(dir.path().join("node_modules/x/a.js"), "needle").unwrap();
        std::fs::write(dir.path().join("b.js"), "needle").unwrap();
        let deep = bench.run("list_files", &json!({"recursive": true}), dir.path(), &cancel).unwrap();
        assert!(!deep.contains("node_modules"), "{deep}");
        assert!(!deep.contains("Library/"), "{deep}");
        let found = bench.run("search_files", &json!({"pattern": "needle"}), dir.path(), &cancel).unwrap();
        assert!(found.contains("b.js") && !found.contains("a.js"), "{found}");
        let shallow = bench.run("list_files", &json!({}), dir.path(), &cancel).unwrap();
        assert!(shallow.contains("node_modules/") && shallow.contains("Library/") && !shallow.contains(".git/"), "{shallow}");
        let named = bench.run("search_files", &json!({"pattern": "needle", "path": "node_modules"}), dir.path(), &cancel).unwrap();
        assert!(named.contains("a.js"), "asked for by name: {named}");
    }

    #[test]
    fn a_search_does_not_print_what_a_read_would_ask_about() {
        let (dir, mut bench, cancel) = bench();
        std::fs::write(dir.path().join(".env"), "API_KEY=sekret").unwrap();
        std::fs::write(dir.path().join("a.txt"), "sekret").unwrap();
        let found = bench.run("search_files", &json!({"pattern": "sekret"}), dir.path(), &cancel).unwrap();
        assert!(found.contains("a.txt") && !found.contains("API_KEY"), "{found}");
        assert!(found.contains("1 files that may hold secrets"), "{found}");
    }

    #[test]
    fn a_read_numbers_its_lines_and_pages_on() {
        let (dir, mut bench, cancel) = bench();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let out = bench.run("read_file", &json!({"path": "a.txt", "start_line": 2, "limit": 1}), dir.path(), &cancel).unwrap();
        assert!(out.contains("2|two\n") && !out.contains("3|three") && out.contains("start_line=3"), "{out}");
        let tail = bench.run("read_file", &json!({"path": "a.txt", "start_line": -1}), dir.path(), &cancel).unwrap();
        assert!(tail.contains("3|three") && !tail.contains("2|two"), "{tail}");
        std::fs::write(dir.path().join("b.bin"), [0u8, 1, 2]).unwrap();
        assert!(bench.run("read_file", &json!({"path": "b.bin"}), dir.path(), &cancel).is_err());
    }

    /// `edit` is apply_patch with one edit: once, or every time old_string is there with replace_all.
    #[test]
    fn edit_is_a_patch_of_one_edit() {
        let (dir, mut bench, cancel) = bench();
        std::fs::write(dir.path().join("a.js"), "let x = 1;\nlet y = x;\nlet z = x;\n").unwrap();
        let once = edit_as_patch(&json!({"file_path": "a.js", "old_string": "let y", "new_string": "const y"}), dir.path());
        bench.run("apply_patch", &once, dir.path(), &cancel).unwrap();
        let all = edit_as_patch(&json!({"file_path": "a.js", "old_string": "= x;", "new_string": "= 2;", "replace_all": true}), dir.path());
        assert_eq!(all["edits"][0]["count"], 2);
        bench.run("apply_patch", &all, dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("a.js")).unwrap(), "let x = 1;\nconst y = 2;\nlet z = 2;\n");
    }

    #[test]
    fn edit_all_counts_in_the_files_own_encoding_and_line_endings() {
        let (dir, mut bench, cancel) = bench();
        let path = dir.path().join("a.txt");
        let before = "日本\r\n語\r\n日本\r\n語";
        let after = "完了\r\n済\r\n完了\r\n済";
        let encode = |text: &str, mark: &[u8]| -> Vec<u8> {
            match mark {
                [0xff, 0xfe] => mark.iter().copied().chain(text.encode_utf16().flat_map(u16::to_le_bytes)).collect(),
                [0xfe, 0xff] => mark.iter().copied().chain(text.encode_utf16().flat_map(u16::to_be_bytes)).collect(),
                [] => in_code_page_932(text).unwrap(),
                _ => [mark, text.as_bytes()].concat(),
            }
        };
        for mark in [&[0xff, 0xfe][..], &[0xfe, 0xff], &[], UTF8_MARK] {
            std::fs::write(&path, encode(before, mark)).unwrap();
            let patch = edit_as_patch(&json!({"file_path":"a.txt", "old_string":"日本\n語", "new_string":"完了\n済", "replace_all":true}), dir.path());
            bench.run("apply_patch", &patch, dir.path(), &cancel).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), encode(after, mark));
        }
    }

    /// Edits written as a string of JSON are taken as the list they are, and a patch to a file not read in the session goes through where its text is there.
    #[test]
    fn edits_written_as_a_string_are_the_list_and_no_read_is_asked_for() {
        let (dir, mut bench, cancel) = bench();
        std::fs::write(dir.path().join("page.html"), "<p>one</p>\n<p>two</p>\n").unwrap();
        let listed = serde_json::to_string(&json!([{"path": "page.html", "old_string": "<p>two</p>", "new_string": "<p>2</p>"}])).unwrap();
        bench.run("apply_patch", &json!({"edits": listed}), dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("page.html")).unwrap(), "<p>one</p>\n<p>2</p>\n");
    }

    #[test]
    fn a_patch_explains_why_its_edit_list_cannot_be_read() {
        let (dir, mut bench, cancel) = bench();
        for (arguments, reason) in [
            (json!({}), "edits is required"),
            (json!({"edits": []}), "edits is empty"),
            (json!({"edits": "[]"}), "edits is empty"),
            (json!({"edits": null}), "edits must be an array"),
            (json!({"edits": "{}"}), "edits must be an array"),
            (json!({"edits": "[{\"new_string\" string=\"true\""}), "expected `:` at line 1 column 16"),
        ] {
            let error = bench.run("apply_patch", &arguments, dir.path(), &cancel).unwrap_err();
            assert!(error.contains(reason), "{error}");
        }
    }

    /// An edit whose line differs by a word is shown the line it differs from, not a longer line that only holds the same letters.
    #[test]
    fn the_nearest_line_is_the_one_most_like_it() {
        let text = "for (toggle, on) in [(self.toggle_thinking, self.settings.show_thinking), (self.toggle_compact, self.settings.compact_tools), (self.toggle_safe, self.settings.safe_commands)] {\n\
                    let edit = CreateWindowExW(0, class.as_ptr(), std::ptr::null(), WS_POPUP | ES_MULTILINE, 0, 0, 700, 500, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null());\n";
        let old = "let edit = CreateWindowExW(0, class.as_ptr(), std::ptr::null(), WS_POPUP | ES_MULTILINE, 0, 0, 700, 500, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null_mut());";
        let said = nearest_line(text, old);
        assert!(said.starts_with(". The nearest is line 2,"), "{said}");
        assert_eq!(nearest_line(text, "nothing at all like either of them"), "");
    }

    #[test]
    fn a_patch_goes_by_what_is_there_and_is_all_or_nothing() {
        let (dir, mut bench, cancel) = bench();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "alpha\nbeta\n").unwrap();
        let edit = json!({"edits": [{"path": "a.txt", "old_string": "alpha", "new_string": "ALPHA"}], "expected_files": ["a.txt"]});
        // (what is there is what counts: no read is asked for, and a file changed since it was read takes an edit whose text is still there)
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
        let bad = json!({"edits": [{"path": "a.txt", "old_string": "alpha", "new_string": "ALPHA"}, {"path": "a.txt", "old_string": "nowhere", "new_string": "x"}], "expected_files": ["a.txt"]});
        assert!(bench.run("apply_patch", &bad, dir.path(), &cancel).unwrap_err().contains("nothing was changed"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\nbeta\ngamma\n");
        let out = bench.run("apply_patch", &edit, dir.path(), &cancel).unwrap();
        assert!(out.contains("+ALPHA"), "{out}");
        // Its own edit counts as seen.
        let again = json!({"edits": [{"old_string": "beta", "new_string": "BETA"}], "expected_files": ["a.txt"]});
        bench.run("apply_patch", &again, dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ALPHA\nBETA\ngamma\n");
    }

    /// A file that cannot take its new text leaves every file as it was, the ones already written put back, and no part-written file
    /// or leftover beside them: what the failure says is what the files are.
    #[test]
    fn a_patch_that_fails_on_its_second_file_puts_the_first_back() {
        let (dir, mut bench, cancel) = bench();
        let (first, second) = (dir.path().join("a.txt"), dir.path().join("b.txt"));
        std::fs::write(&first, "alpha\n").unwrap();
        std::fs::write(&second, "beta\n").unwrap();
        bench.run("read_file", &json!({"path": "a.txt"}), dir.path(), &cancel).unwrap();
        bench.run("read_file", &json!({"path": "b.txt"}), dir.path(), &cancel).unwrap();
        // (read only: its new text is written beside it, and cannot take its place)
        let mut locked = std::fs::metadata(&second).unwrap().permissions();
        locked.set_readonly(true);
        std::fs::set_permissions(&second, locked.clone()).unwrap();
        let edit = json!({"edits": [{"path": "a.txt", "old_string": "alpha", "new_string": "ALPHA"}, {"path": "b.txt", "old_string": "beta", "new_string": "BETA"}], "expected_files": ["a.txt", "b.txt"]});
        let said = bench.run("apply_patch", &edit, dir.path(), &cancel).unwrap_err();
        locked.set_readonly(false);
        std::fs::set_permissions(&second, locked).unwrap();
        assert!(said.contains("the files are as they were"), "{said}");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "alpha\n");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "beta\n");
        let left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).filter(|name| name.contains("dogmind-edit")).collect();
        assert!(left.is_empty(), "{left:?}");
        // And done again with nothing in the way, both change.
        bench.run("apply_patch", &edit, dir.path(), &cancel).unwrap();
        assert_eq!((std::fs::read_to_string(&first).unwrap(), std::fs::read_to_string(&second).unwrap()), ("ALPHA\n".to_string(), "BETA\n".to_string()));
    }

    /// A page or a script written with a syntax error says so in the write's result, at its line; a page not yet whole (no `</html>`) and a clean one say nothing.
    #[test]
    fn a_write_with_a_script_that_will_not_parse_says_so() {
        let (dir, mut bench, cancel) = bench();
        let page = "<!DOCTYPE html>\n<html><body>\n<script type=\"module\">\nconst a = 1;\nemit(a, 1 and 0);\n</script>\n</body></html>\n";
        let said = bench.run("create_file", &json!({"path": "page.html", "content": page}), dir.path(), &cancel).unwrap();
        assert!(said.contains("page.html has a syntax error") && said.contains("line 5: SyntaxError"), "{said}");
        let part = bench.run("create_file", &json!({"path": "part.html", "content": "<html><script>if (a) {\n"}), dir.path(), &cancel).unwrap();
        assert!(!part.contains("syntax error"), "not whole yet: {part}");
        let fixed = bench.run("apply_patch", &json!({"edits": [{"path": "page.html", "old_string": "1 and 0", "new_string": "1 && 0"}]}), dir.path(), &cancel).unwrap();
        assert!(!fixed.contains("syntax error"), "{fixed}");
        let broke = bench.run("apply_patch", &json!({"edits": [{"path": "page.html", "old_string": "const a = 1;", "new_string": "const a = (1;"}]}), dir.path(), &cancel).unwrap();
        assert!(broke.contains("line 4: SyntaxError"), "{broke}");
    }

    /// What is written to a file is written in its form: UTF-16 with its mark, appended to as UTF-16; CRLF lines; a UTF-8 mark kept;
    /// Shift_JIS, or nothing written when a character has no form there.
    /// A PowerShell script that comes to have more than ASCII is written with UTF-8's mark, which Windows PowerShell 5.1 needs to read it; other
    /// files, scripts of ASCII, and scripts that had more than ASCII without the mark (or in another encoding) are written as they were.
    #[test]
    fn a_powershell_script_with_more_than_ascii_is_written_with_the_mark() {
        let (dir, mut bench, cancel) = bench();
        let bytes = |name: &str| std::fs::read(dir.path().join(name)).unwrap();
        let marked = |text: &str| [UTF8_MARK, text.as_bytes()].concat();
        let read = |bench: &mut Workbench, name: &str| bench.run("read_file", &json!({"path": name}), dir.path(), &cancel).unwrap();
        // new: with Japanese, the mark; of ASCII, none; not a script, none
        let said = bench.run("create_file", &json!({"path": "say.ps1", "content": "Write-Output 'こんにちは'\n"}), dir.path(), &cancel).unwrap();
        assert_eq!(bytes("say.ps1"), marked("Write-Output 'こんにちは'\n"));
        assert!(said.contains("byte order mark"), "{said}");
        let said = bench.run("create_file", &json!({"path": "Tools/Mod.PSM1", "content": "# 日本語\n"}), dir.path(), &cancel).unwrap();
        assert_eq!(bytes("Tools/Mod.PSM1"), marked("# 日本語\n"), "{said}");
        let said = bench.run("create_file", &json!({"path": "plain.ps1", "content": "Write-Output 'hello'\n"}), dir.path(), &cancel).unwrap();
        assert_eq!(bytes("plain.ps1"), b"Write-Output 'hello'\n");
        assert!(!said.contains("byte order mark"), "{said}");
        bench.run("create_file", &json!({"path": "note.txt", "content": "こんにちは\n"}), dir.path(), &cancel).unwrap();
        assert_eq!(bytes("note.txt"), "こんにちは\n".as_bytes());
        // (a mark the model wrote itself is not doubled)
        bench.run("create_file", &json!({"path": "own.ps1", "content": "\u{feff}# 日本語\n"}), dir.path(), &cancel).unwrap();
        assert_eq!(bytes("own.ps1"), marked("# 日本語\n"));
        // read back, the mark is not shown, and an edit of the first line finds it
        let shown = read(&mut bench, "say.ps1");
        assert!(!shown.contains('\u{feff}') && shown.contains("Write-Output 'こんにちは'"), "{shown}");
        bench.run("apply_patch", &json!({"edits": [{"path": "say.ps1", "old_string": "Write-Output 'こんにちは'", "new_string": "Write-Output 'こんばんは'"}]}), dir.path(), &cancel).unwrap();
        assert_eq!(bytes("say.ps1"), marked("Write-Output 'こんばんは'\n"));
        // an ASCII script patched or added to with Japanese takes the mark
        let said = bench.run("apply_patch", &json!({"edits": [{"path": "plain.ps1", "old_string": "'hello'", "new_string": "'こんにちは'"}]}), dir.path(), &cancel).unwrap();
        assert_eq!(bytes("plain.ps1"), marked("Write-Output 'こんにちは'\n"));
        assert!(said.contains("plain.ps1 was with a UTF-8 byte order mark"), "{said}");
        std::fs::write(dir.path().join("log.ps1"), "Write-Output 'a'\r\n").unwrap();
        read(&mut bench, "log.ps1");
        bench.run("create_file", &json!({"path": "log.ps1", "content": "Write-Output 'あ'\n", "append": true}), dir.path(), &cancel).unwrap();
        assert_eq!(bytes("log.ps1"), marked("Write-Output 'a'\r\nWrite-Output 'あ'\r\n"));
        // a script that had Japanese without the mark (written for PowerShell 7), and one in Shift_JIS or UTF-16, keep their form
        std::fs::write(dir.path().join("seven.ps1"), "Write-Output 'あ'\n").unwrap();
        read(&mut bench, "seven.ps1");
        bench.run("apply_patch", &json!({"edits": [{"path": "seven.ps1", "old_string": "'あ'", "new_string": "'い'"}]}), dir.path(), &cancel).unwrap();
        assert_eq!(bytes("seven.ps1"), "Write-Output 'い'\n".as_bytes());
        read(&mut bench, "seven.ps1");
        bench.run("create_file", &json!({"path": "seven.ps1", "content": "Write-Output 'う'\n"}), dir.path(), &cancel).unwrap();
        assert_eq!(bytes("seven.ps1"), "Write-Output 'う'\n".as_bytes());
        // (「日本」 in Shift_JIS, then 「日本語」)
        std::fs::write(dir.path().join("sjis.ps1"), [b"# ".as_slice(), &[0x93, 0xFA, 0x96, 0x7B], b"\n"].concat()).unwrap();
        bench.run("apply_patch", &json!({"edits": [{"path": "sjis.ps1", "old_string": "日本", "new_string": "日本語"}]}), dir.path(), &cancel).unwrap();
        assert_eq!(bytes("sjis.ps1"), [b"# ".as_slice(), &[0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA], b"\n"].concat());
        let utf16 = |text: &str| [0xFF, 0xFE].into_iter().chain(text.encode_utf16().flat_map(u16::to_le_bytes)).collect::<Vec<u8>>();
        std::fs::write(dir.path().join("wide.ps1"), utf16("# a\r\n")).unwrap();
        bench.run("apply_patch", &json!({"edits": [{"path": "wide.ps1", "old_string": "# a", "new_string": "# あ"}]}), dir.path(), &cancel).unwrap();
        assert_eq!(bytes("wide.ps1"), utf16("# あ\r\n"));
    }

    #[test]
    fn a_file_is_written_in_its_own_encoding_and_line_ends() {
        let (dir, mut bench, cancel) = bench();
        let read = |bench: &mut Workbench, name: &str| bench.run("read_file", &json!({"path": name}), dir.path(), &cancel).unwrap();
        let utf16 = |text: &str| [0xFF, 0xFE].into_iter().chain(text.encode_utf16().flat_map(u16::to_le_bytes)).collect::<Vec<u8>>();
        std::fs::write(dir.path().join("w.txt"), utf16("一行目\r\n")).unwrap();
        read(&mut bench, "w.txt");
        bench.run("create_file", &json!({"path": "w.txt", "content": "追加\n", "append": true}), dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read(dir.path().join("w.txt")).unwrap(), utf16("一行目\r\n追加\r\n"));
        std::fs::write(dir.path().join("crlf.txt"), "a\r\nb\r\n").unwrap();
        read(&mut bench, "crlf.txt");
        bench.run("create_file", &json!({"path": "crlf.txt", "content": "x\ny\n"}), dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read(dir.path().join("crlf.txt")).unwrap(), b"x\r\ny\r\n");
        std::fs::write(dir.path().join("bom.ps1"), "\u{feff}Write-Host 'あ'\n").unwrap();
        read(&mut bench, "bom.ps1");
        bench.run("create_file", &json!({"path": "bom.ps1", "content": "Write-Host 'い'\n"}), dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("bom.ps1")).unwrap(), "\u{feff}Write-Host 'い'\n");
        std::fs::write(dir.path().join("sjis.txt"), [0x93, 0xFA, 0x96, 0x7B, 0x0A]).unwrap();
        read(&mut bench, "sjis.txt");
        let said = bench.run("create_file", &json!({"path": "sjis.txt", "content": "会员\n"}), dir.path(), &cancel).unwrap_err();
        assert!(said.contains("Shift_JIS") && said.contains('员') && said.contains("Nothing was written"), "{said}");
        assert_eq!(std::fs::read(dir.path().join("sjis.txt")).unwrap(), [0x93, 0xFA, 0x96, 0x7B, 0x0A]);
        bench.run("create_file", &json!({"path": "sjis.txt", "content": "本日\n"}), dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read(dir.path().join("sjis.txt")).unwrap(), [0x96, 0x7B, 0x93, 0xFA, 0x0A]);
    }

    #[test]
    fn a_patch_keeps_the_file_encoding_mark_and_mixed_line_ends() {
        let (dir, mut bench, cancel) = bench();
        let before = "日本\r\n二行目\r\n末尾\n";
        let after = "本日\r\n日本\r\n二行目\r\n末尾\n";
        let utf16 = |text: &str, big_endian: bool| {
            let mark = if big_endian { [0xFE, 0xFF] } else { [0xFF, 0xFE] };
            mark.into_iter().chain(text.encode_utf16().flat_map(|unit| {
                if big_endian { unit.to_be_bytes() } else { unit.to_le_bytes() }
            })).collect::<Vec<u8>>()
        };
        for (name, before, after) in [
            ("le.txt", utf16(before, false), utf16(after, false)),
            ("be.txt", utf16(before, true), utf16(after, true)),
            ("bom.txt", format!("\u{feff}{before}").into_bytes(), format!("\u{feff}{after}").into_bytes()),
            ("sjis.txt", shift_jis(before).unwrap(), shift_jis(after).unwrap()),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, before).unwrap();
            bench.run("read_file", &json!({"path": name}), dir.path(), &cancel).unwrap();
            let edit = json!({"edits": [{"path": name, "old_string": "日本", "new_string": "本日\n日本"}]});
            let said = bench.run("apply_patch", &edit, dir.path(), &cancel).unwrap();
            assert!(said.contains("+本日"), "{said}");
            assert_eq!(std::fs::read(&path).unwrap(), after, "{name}");
            let again = json!({"edits": [{"path": name, "old_string": "本日", "new_string": "日本"}]});
            bench.run("apply_patch", &again, dir.path(), &cancel).unwrap();
        }
    }

    #[test]
    fn a_patch_that_cannot_encode_one_file_leaves_all_files_unchanged() {
        let (dir, mut bench, cancel) = bench();
        let plain = dir.path().join("plain.txt");
        let legacy = dir.path().join("legacy.txt");
        let original = [0x93, 0xFA, 0x96, 0x7B, 0x0A];
        std::fs::write(&plain, "alpha\n").unwrap();
        std::fs::write(&legacy, original).unwrap();
        for name in ["plain.txt", "legacy.txt"] {
            bench.run("read_file", &json!({"path": name}), dir.path(), &cancel).unwrap();
        }
        let edit = json!({"edits": [
            {"path": "plain.txt", "old_string": "alpha", "new_string": "ALPHA"},
            {"path": "legacy.txt", "old_string": "日本", "new_string": "会员"},
        ]});
        let said = bench.run("apply_patch", &edit, dir.path(), &cancel).unwrap_err();
        assert!(said.contains("legacy.txt") && said.contains("Shift_JIS") && said.contains('员'), "{said}");
        assert_eq!(std::fs::read(&plain).unwrap(), b"alpha\n");
        assert_eq!(std::fs::read(&legacy).unwrap(), original);
    }

    #[test]
    fn a_patch_does_not_rewrite_malformed_utf16() {
        let (dir, mut bench, cancel) = bench();
        let path = dir.path().join("bad.txt");
        let original = [0xFF, 0xFE, b'a', 0, 0xFF];
        std::fs::write(&path, original).unwrap();
        bench.run("read_file", &json!({"path": "bad.txt"}), dir.path(), &cancel).unwrap();
        let edit = json!({"edits": [{"path": "bad.txt", "old_string": "a", "new_string": "b"}]});
        let said = bench.run("apply_patch", &edit, dir.path(), &cancel).unwrap_err();
        assert!(said.contains("nothing was changed"), "{said}");
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    /// A file shown only in part is not written whole over, as what was not shown would go; read on to its end, it is. Patched, a
    /// write that was cut off no longer goes on from it.
    #[test]
    fn a_file_seen_in_part_is_not_written_whole_over() {
        let (dir, mut bench, cancel) = bench();
        std::fs::write(dir.path().join("long.txt"), (1..=2600).map(|at| format!("line {at}\n")).collect::<String>()).unwrap();
        bench.run("read_file", &json!({"path": "long.txt"}), dir.path(), &cancel).unwrap();
        let write = json!({"path": "long.txt", "content": "short\n"});
        let said = bench.run("create_file", &write, dir.path(), &cancel).unwrap_err();
        assert!(said.contains("2600 lines") && said.contains("shown 2000") && said.contains("start_line=2001"), "{said}");
        bench.run("read_file", &json!({"path": "long.txt", "start_line": 2001}), dir.path(), &cancel).unwrap();
        bench.run("create_file", &write, dir.path(), &cancel).unwrap();
        // (an outline shows its head, and not its parts after a gap)
        let source: String = (0..150).map(|at| format!("fn part_{at}() {{\n{}}}\n", "    x();\n".repeat(14))).collect();
        std::fs::write(dir.path().join("big.rs"), &source).unwrap();
        assert!(bench.run("read_file", &json!({"path": "big.rs"}), dir.path(), &cancel).unwrap().contains("--- outline ---"));
        assert!(bench.run("create_file", &json!({"path": "big.rs", "content": "fn main() {}\n"}), dir.path(), &cancel).unwrap_err().contains("shown 30"));
        // A cut write's mark goes with a patch of its file.
        std::fs::write(dir.path().join("page.html"), "<p>a</p>\n").unwrap();
        bench.run("read_file", &json!({"path": "page.html"}), dir.path(), &cancel).unwrap();
        bench.note_cut(dir.path().join("page.html"));
        bench.run("apply_patch", &json!({"edits": [{"path": "page.html", "old_string": "a", "new_string": "b"}]}), dir.path(), &cancel).unwrap();
        bench.run("create_file", &json!({"path": "page.html", "content": "<p>c</p>\n"}), dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("page.html")).unwrap(), "<p>c</p>\n", "written whole, not added to");
    }

    /// A write cut off goes on from where it was cut; written again from its first line, or said to be whole, it takes the file's place.
    #[test]
    fn a_cut_write_written_again_from_the_top_is_not_there_twice() {
        let (dir, mut bench, cancel) = bench();
        let head = "<!DOCTYPE html>\n<html lang=\"ja\">\n<head><title>A page</title></head>\n";
        bench.run("create_file", &json!({"path": "page.html", "content": head}), dir.path(), &cancel).unwrap();
        bench.note_cut(dir.path().join("page.html"));
        bench.run("create_file", &json!({"path": "page.html", "content": "<body></body>\n"}), dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("page.html")).unwrap(), format!("{head}<body></body>\n"), "it goes on");
        bench.note_cut(dir.path().join("page.html"));
        let again = format!("{head}<body><p>all of it</p></body>\n</html>\n");
        bench.run("create_file", &json!({"path": "page.html", "content": again}), dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("page.html")).unwrap(), again, "from the top, it is the whole");
        bench.note_cut(dir.path().join("page.html"));
        bench.run("create_file", &json!({"path": "page.html", "content": "short\n", "append": false}), dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("page.html")).unwrap(), "short\n", "said to be whole");
    }

    /// An edit to a CRLF file whose new_string has CRLF already keeps one CR before each LF.
    #[test]
    fn an_edit_written_with_crlf_into_a_crlf_file_has_no_cr_more() {
        let (dir, mut bench, cancel) = bench();
        std::fs::write(dir.path().join("a.txt"), "one\r\ntwo\r\n").unwrap();
        bench.run("read_file", &json!({"path": "a.txt"}), dir.path(), &cancel).unwrap();
        bench.run("apply_patch", &json!({"edits": [{"path": "a.txt", "old_string": "one\ntwo", "new_string": "uno\r\ndos"}]}), dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"uno\r\ndos\r\n");
    }

    /// A file written again that cannot take its new text is as it was, not emptied, and nothing is left beside it.
    #[test]
    fn a_file_written_again_that_fails_is_as_it_was() {
        let (dir, mut bench, cancel) = bench();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "alpha\n").unwrap();
        bench.run("read_file", &json!({"path": "a.txt"}), dir.path(), &cancel).unwrap();
        let mut locked = std::fs::metadata(&file).unwrap().permissions();
        locked.set_readonly(true);
        std::fs::set_permissions(&file, locked.clone()).unwrap();
        let write = json!({"path": "a.txt", "content": "omega\n"});
        let said = bench.run("create_file", &write, dir.path(), &cancel).unwrap_err();
        locked.set_readonly(false);
        std::fs::set_permissions(&file, locked).unwrap();
        assert!(said.contains("the file is as it was"), "{said}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n");
        let left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).filter(|name| name.contains("dogmind-edit")).collect();
        assert!(left.is_empty(), "{left:?}");
        bench.run("create_file", &write, dir.path(), &cancel).unwrap();
        bench.run("create_file", &json!({"path": "new.txt", "content": "made\n"}), dir.path(), &cancel).unwrap();
        assert_eq!((std::fs::read_to_string(&file).unwrap(), std::fs::read_to_string(dir.path().join("new.txt")).unwrap()), ("omega\n".to_string(), "made\n".to_string()));
    }

    #[test]
    fn patch_paths_follow_the_case_rules_of_the_directory() {
        let (dir, mut bench, cancel) = bench();
        let original = dir.path().join("ReadMe.txt");
        let other = dir.path().join("README.TXT");
        std::fs::write(&original, "one\ntwo\n").unwrap();
        let same_entry = other.exists();
        if !same_entry {
            std::fs::write(&other, "separate\n").unwrap();
        }
        let expected = if same_entry { vec!["ReadMe.txt"] } else { vec!["ReadMe.txt", "README.TXT"] };
        let patch = json!({"expected_files": expected, "edits": [
            {"path": "ReadMe.txt", "old_string": "one", "new_string": "first"},
            {"path": "README.TXT", "old_string": if same_entry { "first" } else { "separate" }, "new_string": "final"}
        ]});
        let report = bench.run("apply_patch", &patch, dir.path(), &cancel).unwrap();
        if same_entry {
            assert_eq!(std::fs::read_to_string(original).unwrap(), "final\ntwo\n");
            assert_eq!(report.matches("--- applied_diff ").count(), 1);
        } else {
            assert_eq!(std::fs::read_to_string(original).unwrap(), "first\ntwo\n");
            assert_eq!(std::fs::read_to_string(other).unwrap(), "final\n");
        }
    }

    #[test]
    fn an_invalid_patch_count_leaves_every_file_unchanged() {
        let (dir, mut bench, cancel) = bench();
        let first = dir.path().join("a.txt");
        let second = dir.path().join("b.txt");
        std::fs::write(&first, b"first\r\n").unwrap();
        std::fs::write(&second, b"second\n").unwrap();
        for count in [json!("2"), json!(-1), json!(1.5), json!(null), json!(true)] {
            let patch = json!({"edits": [
                {"path": "a.txt", "old_string": "first", "new_string": "edited"},
                {"path": "b.txt", "old_string": "second", "new_string": "changed", "count": count}
            ]});
            let why = bench.run("apply_patch", &patch, dir.path(), &cancel).unwrap_err();
            assert!(why.contains("edit 2: count"), "{count}: {why}");
            assert_eq!(std::fs::read(&first).unwrap(), b"first\r\n");
            assert_eq!(std::fs::read(&second).unwrap(), b"second\n");
        }
    }

    #[test]
    fn an_ambiguous_patch_shows_bounded_match_locations_after_prior_edits() {
        let (dir, mut bench, cancel) = bench();
        let file = dir.path().join("a.txt");
        let before = "導入\r\n猫 猫\n猫\n猫\n猫\n猫\n";
        std::fs::write(&file, before).unwrap();
        let patch = json!({"edits":[
            {"path":"a.txt", "old_string":"導入", "new_string":"導入\n詳細"},
            {"path":"a.txt", "old_string":"猫", "new_string":"犬"}
        ]});
        let error = bench.run("apply_patch", &patch, dir.path(), &cancel).unwrap_err();
        assert!(error.contains("occurs 6 times") && error.contains("lines at this edit: 3, 3, 4, 5, 6, ..."), "{error}");
        assert_eq!(std::fs::read_to_string(file).unwrap(), before);
    }

    #[test]
    fn a_line_ending_fallback_does_not_hide_matches_in_a_mixed_file() {
        let (dir, mut bench, cancel) = bench();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "header\r\nx\ny\nx\ny\n").unwrap();
        let patch = json!({"edits":[{"path":"a.txt","old_string":"x\ny","new_string":"z"}]});
        let error = bench.run("apply_patch", &patch, dir.path(), &cancel).unwrap_err();
        assert!(error.contains("occurs 2 times") && error.contains("lines at this edit: 2, 4"), "{error}");
    }

    #[test]
    fn a_patch_written_with_lf_fits_a_crlf_file_and_counts_its_matches() {
        let (dir, mut bench, cancel) = bench();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x\r\ny\r\nx\r\ny\r\n").unwrap();
        bench.run("read_file", &json!({"path": "a.txt"}), dir.path(), &cancel).unwrap();
        let once = json!({"edits": [{"path": "a.txt", "old_string": "x\ny", "new_string": "z\nw"}], "expected_files": ["a.txt"]});
        let error = bench.run("apply_patch", &once, dir.path(), &cancel).unwrap_err();
        assert!(error.contains("occurs 2 times") && error.contains("lines at this edit: 1, 3"), "{error}");
        let twice = json!({"edits": [{"path": "a.txt", "old_string": "x\ny", "new_string": "z\nw", "count": 2}], "expected_files": ["a.txt"]});
        bench.run("apply_patch", &twice, dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "z\r\nw\r\nz\r\nw\r\n");
    }

    /// A file of LF lines whose last line ends with CRLF, as PowerShell's Set-Content leaves one: an edit of its LF part, written with LF, is
    /// found and keeps LF there; one of its CRLF part is found too.
    #[test]
    fn a_patch_fits_a_file_with_both_line_ends_where_it_is_written() {
        let (dir, mut bench, cancel) = bench();
        let file = dir.path().join("shop.py");
        std::fs::write(&file, "def f(code):\n    if code == 'A':\n        return 1\n    return 0\n\nlast = 1\r\nend = 2\r\n").unwrap();
        bench.run("read_file", &json!({"path": "shop.py"}), dir.path(), &cancel).unwrap();
        let lf = json!({"edits": [{"path": "shop.py", "old_string": "    if code == 'A':\n        return 1", "new_string": "    if code.upper() == 'A':\n        return 1"}]});
        bench.run("apply_patch", &lf, dir.path(), &cancel).unwrap();
        let crlf = json!({"edits": [{"path": "shop.py", "old_string": "last = 1\nend = 2", "new_string": "last = 3\nend = 4"}]});
        bench.run("apply_patch", &crlf, dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "def f(code):\n    if code.upper() == 'A':\n        return 1\n    return 0\n\nlast = 3\r\nend = 4\r\n");
    }

    /// Found by the app itself in use: one line replaced by three written with LF, in a file of CRLF lines, left the three with LF.
    #[test]
    fn one_line_replaced_by_three_takes_the_line_end_of_the_file() {
        let (dir, mut bench, cancel) = bench();
        let edit = json!({"edits": [{"path": "a.rs", "old_string": "const MAX_LINKS: usize = 20;", "new_string": "const MAX_LINKS: usize = 20;\nconst MAX_IMAGES: usize = 8;\nconst MAX_DEPTH: usize = 3;"}]});
        std::fs::write(dir.path().join("a.rs"), "use x;\r\nconst MAX_LINKS: usize = 20;\r\nfn main() {}\r\n").unwrap();
        bench.run("read_file", &json!({"path": "a.rs"}), dir.path(), &cancel).unwrap();
        bench.run("apply_patch", &edit, dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap(), "use x;\r\nconst MAX_LINKS: usize = 20;\r\nconst MAX_IMAGES: usize = 8;\r\nconst MAX_DEPTH: usize = 3;\r\nfn main() {}\r\n");
        // (in a file of LF lines, no CR at all)
        std::fs::write(dir.path().join("a.rs"), "use x;\nconst MAX_LINKS: usize = 20;\nfn main() {}\n").unwrap();
        bench.run("read_file", &json!({"path": "a.rs"}), dir.path(), &cancel).unwrap();
        bench.run("apply_patch", &edit, dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap(), "use x;\nconst MAX_LINKS: usize = 20;\nconst MAX_IMAGES: usize = 8;\nconst MAX_DEPTH: usize = 3;\nfn main() {}\n");
    }

    /// In a file with both line ends, the line it replaces says which one: a one-line edit of the CRLF last line gets CRLF, the LF lines are not touched,
    /// and a CR the new text already has is not doubled.
    #[test]
    fn a_line_replaced_by_more_takes_the_line_end_where_it_is() {
        let (dir, mut bench, cancel) = bench();
        let file = dir.path().join("shop.py");
        std::fs::write(&file, "def f(code):\n    return 0\n\nlast = 1\r\nend = 2\r\n").unwrap();
        bench.run("read_file", &json!({"path": "shop.py"}), dir.path(), &cancel).unwrap();
        let crlf = json!({"edits": [{"path": "shop.py", "old_string": "end = 2", "new_string": "end = 2\nmore = 3\r\nmost = 4"}]});
        bench.run("apply_patch", &crlf, dir.path(), &cancel).unwrap();
        let lf = json!({"edits": [{"path": "shop.py", "old_string": "    return 0", "new_string": "    if code:\n        return 1\n    return 0"}]});
        bench.run("apply_patch", &lf, dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "def f(code):\n    if code:\n        return 1\n    return 0\n\nlast = 1\r\nend = 2\r\nmore = 3\r\nmost = 4\r\n");
        // (a last line with no end of its own takes the one before it)
        std::fs::write(&file, "a\r\nb").unwrap();
        bench.run("read_file", &json!({"path": "shop.py"}), dir.path(), &cancel).unwrap();
        bench.run("apply_patch", &json!({"edits": [{"path": "shop.py", "old_string": "b", "new_string": "b\nc"}]}), dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\r\nb\r\nc");
    }

    #[test]
    fn a_file_is_written_over_only_as_this_session_knows_it() {
        let (dir, mut bench, cancel) = bench();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "theirs").unwrap();
        let write = json!({"path": "a.txt", "content": "mine"});
        let said = bench.run("create_file", &write, dir.path(), &cancel).unwrap_err();
        assert!(said.contains("read it first"), "{said}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs");
        bench.run("read_file", &json!({"path": "a.txt"}), dir.path(), &cancel).unwrap();
        std::fs::write(&file, "changed since").unwrap();
        assert!(bench.run("create_file", &write, dir.path(), &cancel).unwrap_err().contains("changed since it was read"));
        bench.run("read_file", &json!({"path": "a.txt"}), dir.path(), &cancel).unwrap();
        assert!(bench.run("create_file", &write, dir.path(), &cancel).unwrap().starts_with("replaced a.txt"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "mine");
    }

    /// A long page written whole is told to be opened with check_page before it changes, and what it shows changed with apply_patch. A short page, a file that
    /// is not a page, and a write that goes on from a cut one are not.
    #[test]
    fn a_long_page_written_whole_is_told_to_be_opened_first() {
        let (dir, mut bench, cancel) = bench();
        let long = "<p>line</p>\n".repeat(WRITTEN_WHOLE_LINES);
        let said = bench.run("create_file", &json!({"path": "page.html", "content": long}), dir.path(), &cancel).unwrap();
        assert!(said.ends_with(&not_opened_yet(WRITTEN_WHOLE_LINES)) && said.contains(&format!("writing all {WRITTEN_WHOLE_LINES} lines again")), "{said}");
        assert_eq!(said.lines().next(), Some(format!("created page.html: now {WRITTEN_WHOLE_LINES} lines, {} bytes", long.len()).as_str()), "the first line, which the window shows, is as before");
        let again = bench.run("create_file", &json!({"path": "page.html", "content": long}), dir.path(), &cancel).unwrap();
        assert!(again.starts_with("replaced page.html") && again.ends_with(&not_opened_yet(WRITTEN_WHOLE_LINES)), "{again}");
        let short = "<p>line</p>\n".repeat(WRITTEN_WHOLE_LINES - 1);
        assert!(!bench.run("create_file", &json!({"path": "short.html", "content": short}), dir.path(), &cancel).unwrap().contains("check_page"));
        for other in ["notes.md", "game.py", "app.js"] {
            assert!(!bench.run("create_file", &json!({"path": other, "content": long}), dir.path(), &cancel).unwrap().contains("check_page"), "{other}");
        }
        let added = bench.run("create_file", &json!({"path": "page.html", "content": long, "append": true}), dir.path(), &cancel).unwrap();
        assert!(added.starts_with("added to page.html") && !added.contains("check_page"), "{added}");
        assert!(is_a_page(Path::new(r"a\Index.HTML")) && is_a_page(Path::new("x.htm")) && !is_a_page(Path::new("image.svg")) && !is_a_page(Path::new("style.css")));
    }

    #[test]
    fn a_new_file_makes_its_folders_and_append_adds_to_what_is_there() {
        let (dir, mut bench, cancel) = bench();
        let said = bench.run("create_file", &json!({"path": "sub/a.txt", "content": "one\n"}), dir.path(), &cancel).unwrap();
        assert_eq!(said, "created sub/a.txt: now 1 lines, 4 bytes");
        let said = bench.run("create_file", &json!({"path": "sub/a.txt", "content": "two\n", "append": true}), dir.path(), &cancel).unwrap();
        assert_eq!(said, "added to sub/a.txt: now 2 lines, 8 bytes");
        assert_eq!(std::fs::read_to_string(dir.path().join("sub/a.txt")).unwrap(), "one\ntwo\n");
        // Written as this session left it, so an edit needs no read.
        bench.run("apply_patch", &json!({"edits": [{"path": "sub/a.txt", "old_string": "two", "new_string": "2"}]}), dir.path(), &cancel).unwrap();
        // Appending to what someone else's hands have been at is writing over what it has not seen.
        std::fs::write(dir.path().join("sub/a.txt"), "theirs").unwrap();
        assert!(bench.run("create_file", &json!({"path": "sub/a.txt", "content": "x", "append": true}), dir.path(), &cancel).is_err());
        assert_eq!(std::fs::read_to_string(dir.path().join("sub/a.txt")).unwrap(), "theirs");
    }

    #[test]
    fn after_a_cut_the_next_write_goes_on_once_and_then_writes_are_writes_again() {
        let (dir, mut bench, cancel) = bench();
        bench.run("create_file", &json!({"path": "a.txt", "content": "one\n"}), dir.path(), &cancel).unwrap();
        bench.note_cut(dir.path().join("a.txt"));
        bench.run("create_file", &json!({"path": "b.txt", "content": "other\n"}), dir.path(), &cancel).unwrap();
        bench.run("create_file", &json!({"path": "a.txt", "content": "two\n"}), dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "one\ntwo\n");
        bench.run("create_file", &json!({"path": "a.txt", "content": "whole\n"}), dir.path(), &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "whole\n");
    }

    #[test]
    fn a_read_is_bounded_by_lines_and_bytes_and_says_where_to_go_on() {
        let (dir, mut bench, cancel) = bench();
        let mut minified = String::from("first\n");
        minified.push_str(&"x".repeat(100_000));
        minified.push_str("\nlast\n");
        std::fs::write(dir.path().join("min.js"), &minified).unwrap();
        let out = bench.run("read_file", &json!({"path": "min.js", "limit": 2}), dir.path(), &cancel).unwrap();
        assert!(out.len() < MAX_LINE + 200, "{}", out.len());
        assert!(out.starts_with("1|first\n2|xxx") && out.contains("98000 more bytes on this line") && out.ends_with("--- 1 more lines; continue with start_line=3 ---\n"), "{}", &out[out.len() - 120..]);
        let long = "a line of an ordinary file with some words on it\n".repeat(5000);
        std::fs::write(dir.path().join("long.txt"), &long).unwrap();
        let out = bench.run("read_file", &json!({"path": "long.txt", "limit": 2000}), dir.path(), &cancel).unwrap();
        assert!(out.len() < MAX_READ + 200, "{}", out.len());
        let told: usize = out.rsplit_once("start_line=").unwrap().1.trim_end_matches(" ---\n").parse().unwrap();
        assert!(out.contains(&format!("\n{}|", told - 1)) && !out.contains(&format!("\n{told}|")), "goes on where it stopped");
        let next = bench.run("read_file", &json!({"path": "long.txt", "start_line": told, "limit": 1}), dir.path(), &cancel).unwrap();
        assert!(next.starts_with(&format!("{told}|")), "{next}");
        // A hash of no use to a model, and the encoding only when it is not what it would assume.
        assert!(!out.contains("sha256"));
    }

    #[test]
    fn an_edit_that_misses_by_a_character_is_shown_the_line_and_says_which_it_was() {
        let (dir, mut bench, cancel) = bench();
        std::fs::write(dir.path().join("a.txt"), "first line\n// 水平線の近くのもや\nlast line\n").unwrap();
        bench.run("read_file", &json!({"path": "a.txt"}), dir.path(), &cancel).unwrap();
        let edit = json!({"edits": [{"path": "a.txt", "old_string": "first line", "new_string": "one"}, {"path": "a.txt", "old_string": "// 水平线の近くのもや", "new_string": "x"}]});
        let said = bench.run("apply_patch", &edit, dir.path(), &cancel).unwrap_err();
        assert!(said.contains("edit 2 of 2 (`// 水平线"), "{said}");
        assert!(said.contains("line 2") && said.contains("水平線の近くのもや"), "{said}");
        let nothing_like = json!({"edits": [{"path": "a.txt", "old_string": "zzz qqq", "new_string": "x"}]});
        assert!(!bench.run("apply_patch", &nothing_like, dir.path(), &cancel).unwrap_err().contains("nearest"));
    }

    #[cfg(windows)]
    #[test]
    fn only_a_plain_git_init_at_the_start_of_a_line_is_done_by_the_app() {
        assert_eq!(git_init_first("git init"), Some((vec![], String::new())));
        assert_eq!(git_init_first("git init -q . && git add ."), Some((vec!["-q".to_string()], "git add .".to_string())));
        assert_eq!(git_init_first("git init -b main & dir"), Some((vec!["-b".to_string(), "main".to_string()], "dir".to_string())));
        for line in ["git init ../other", "git init --template=x", "git init --separate-git-dir=C:\\x", "git status", "echo x && git init", "git init > C:\\x", "git init\ndel x", "git init -c core.hooksPath=x"] {
            assert_eq!(git_init_first(line), None, "{line}");
        }
    }

    /// A turn that has read much is told so, and again at each as much more, and pointed at explore; a new turn counts from nothing.
    #[test]
    fn a_turn_that_reads_much_is_told_so_and_pointed_at_explore() {
        let (dir, mut bench, cancel) = bench();
        std::fs::write(dir.path().join("big.txt"), (0..400).map(|at| format!("line {at} {}\n", "x".repeat(70))).collect::<String>()).unwrap();
        let read = |bench: &mut Workbench, start: i64| bench.run("read_file", &json!({"path": "big.txt", "start_line": start, "limit": 400}), dir.path(), &cancel).unwrap();
        let notes = |bench: &mut Workbench| (1..=9).map(|start| read(bench, start)).filter(|out| out.contains("give the question to explore")).count();
        let told = notes(&mut bench);
        assert!(told >= 2 && told == bench.read_this_turn / READ_TOLD_PAST, "at each 96 KB: {told} for {} bytes", bench.read_this_turn);
        bench.new_turn();
        // (lines read again unchanged are not sent again, and count for nothing)
        std::fs::write(dir.path().join("big.txt"), (0..400).map(|at| format!("other {at} {}\n", "y".repeat(70))).collect::<String>()).unwrap();
        assert_eq!(notes(&mut bench), told, "and so again in the next");
    }

    #[test]
    fn overflowing_outlines_keep_only_the_first_top_level_headings() {
        let mut text = "  ## nested\n".repeat(OUTLINE_MOST - 1);
        text.push_str("# first\n");
        assert_eq!(outline(&text).len(), OUTLINE_MOST);
        text.push_str("  ## one more\n");
        assert_eq!(outline(&text), [(OUTLINE_MOST - 1, "# first")]);
        text.push_str(&"# next\n".repeat(OUTLINE_MOST));
        let parts = outline(&text);
        assert_eq!(parts.len(), OUTLINE_MOST);
        assert_eq!(parts.last(), Some(&(2 * OUTLINE_MOST - 1, "# next")));
    }

    #[test]
    fn the_outline_counts_numbered_pages_instead_of_source_bytes() {
        // Line numbers push this file over the second page's byte limit.
        let text = format!("# note\n{}", format!("{}\n", "x".repeat(32)).repeat(3700));
        assert_eq!(outline_read_count(&text, text.lines().count(), ""), "3 reads");
        // A single long line is abbreviated by a ranged read.
        let text = format!("# note\n{}\n", "語".repeat(MAX_READ));
        assert_eq!(outline_read_count(&text, 2, ""), "1 read");
        // Beyond the counting budget the wording promises only a lower bound.
        assert_eq!(outline_read_count(&"row\n".repeat(20_001), 20_001, ""), "at least 11 reads");
        let text = format!("{}\n", "x".repeat(MAX_LINE)).repeat(300);
        assert_eq!(outline_read_count(&text, 300, ""), "at least 9 reads");
    }

    #[test]
    fn a_file_that_only_overflows_after_numbering_still_shows_its_last_heading() {
        let (dir, mut bench, cancel) = bench();
        let text = format!("# Introduction\n{}# Appendix\n", "paragraph with thirty letters.\n".repeat(1983));
        assert!(text.len() < MAX_READ && text.lines().count() < READ_LINES);
        std::fs::write(dir.path().join("chapters.md"), &text).unwrap();
        let read = |bench: &mut Workbench, args| bench.run("read_file", &args, dir.path(), &cancel).unwrap();
        let outline = read(&mut bench, json!({"path":"chapters.md"}));
        assert!(outline.contains("--- outline ---") && outline.contains("1985|# Appendix"), "{}", outline.lines().last().unwrap());
        let first = read(&mut bench, json!({"path":"chapters.md", "start_line":1}));
        assert!(first.contains("continue with start_line=1852") && !first.contains("--- outline ---"));
        let last = read(&mut bench, json!({"path":"chapters.md", "start_line":1852}));
        assert!(last.ends_with("1985|# Appendix\n"));
    }

    /// A long file read with no place in it comes as its head and where its parts begin; asked for a place, it comes as lines, as before;
    /// a long file of no language the outline knows is read as lines too. One that fits in a read, a page of a thousand lines, comes whole.
    #[test]
    fn a_long_file_read_whole_comes_as_its_outline() {
        {
            let (dir, mut bench, cancel) = bench();
            let page: String = (0..1153).map(|at| format!("<div class=\"row-{at}\">{at}</div>\n")).collect();
            assert!(page.len() < MAX_READ, "{}", page.len());
            std::fs::write(dir.path().join("kuji.html"), &page).unwrap();
            let out = bench.run("read_file", &json!({"path": "kuji.html"}), dir.path(), &cancel).unwrap();
            assert!(out.starts_with("1|<div") && out.contains("1153|<div class=\"row-1152\">") && !out.contains("outline") && !out.contains("continue with"), "{}", &out[out.len() - 200..]);
        }
        let (dir, mut bench, cancel) = bench();
        let mut source = String::from("//! What this module is.\nuse std::path::Path;\n");
        for at in 0..130 {
            source.push_str(&format!("\n/// Part {at}.\npub fn part_{at}(x: u32) -> u32 {{\n"));
            for line in 0..14 {
                source.push_str(&format!("    let v{line} = x + {at};\n"));
            }
            source.push_str("    x\n}\n");
        }
        std::fs::write(dir.path().join("big.rs"), &source).unwrap();
        let out = bench.run("read_file", &json!({"path": "big.rs"}), dir.path(), &cancel).unwrap();
        assert!(out.starts_with("--- ") && out.contains("--- outline ---") && out.contains("|pub fn part_129(x: u32) -> u32 {") && out.contains("start_line and limit"), "{}", &out[..out.len().min(600)]);
        assert!(!out.contains("let v7 = x + 129;"), "the bodies are not in it");
        assert!(out.len() < 12000, "{}", out.len());
        // (and how to read all of it, where all of it is wanted: in reads of 2000 lines and 64 KB)
        assert!(out.lines().next().unwrap().contains("all numbered lines (2 reads), use read_file with start_line=1"), "{}", out.lines().next().unwrap());
        // A place asked for is read as it was.
        let part = bench.run("read_file", &json!({"path": "big.rs", "start_line": 100, "limit": 5}), dir.path(), &cancel).unwrap();
        assert!(part.starts_with("100|") && !part.contains("outline"), "{part}");
        // Lines of no language: read as lines.
        std::fs::write(dir.path().join("big.log"), (0..2500).map(|at| format!("entry {at}\n")).collect::<String>()).unwrap();
        let log = bench.run("read_file", &json!({"path": "big.log"}), dir.path(), &cancel).unwrap();
        assert!(log.starts_with("1|entry 0") && log.contains("continue with start_line="), "{}", &log[..200]);
    }

    /// A line whose rest would be refused is refused before the app's `git init`: it says nothing was started, and nothing was.
    #[cfg(windows)]
    #[test]
    fn a_refused_line_makes_no_repository_first() {
        let dir = crate::in_container::tempdir().unwrap();
        std::fs::write(dir.path().join("app.log"), "x").unwrap();
        let said = execute_command(&json!({"command": "git init && app.log"}), dir.path(), &AtomicBool::new(false), None).unwrap_err();
        assert!(said.contains("Nothing was started"), "{said}");
        assert!(!dir.path().join(".git").exists(), "the repository was made before the line was refused");
    }

    /// As cmd.exe runs what comes of it, with no sandbox: what it prints, on this machine's python and node if it has them.
    #[cfg(windows)]
    fn through_cmd(line: &str) -> String {
        use std::os::windows::process::CommandExt;
        const NO_WINDOW: u32 = 0x0800_0000;
        let line = on_one_line(line).unwrap();
        let output = Command::new("cmd").raw_arg(format!("/d /s /c \"chcp 65001 >nul & {line}\"")).creation_flags(NO_WINDOW).stdin(Stdio::null()).output().unwrap();
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
    }

    #[cfg(windows)]
    fn has_program(name: &str) -> bool {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).any(|folder| folder.join(format!("{name}.exe")).is_file())
    }

    /// As it came from a real model in two of seven real jobs, each time an empty result, a wasted round and a script left in the workspace.
    #[cfg(windows)]
    #[test]
    fn code_on_several_lines_runs_as_it_was_written() {
        assert_eq!(through_cmd("echo one\necho two"), "one \ntwo\n", "both lines, where cmd alone runs the first");
        assert_eq!(through_cmd("(echo a\necho b\n)"), "a \nb \n");
        assert_eq!(through_cmd("echo a ^\nb"), "a b\n");
        if !has_program("python") {
            return;
        }
        let code = "python -I -c \"\nimport collections\nc = collections.Counter('aab')\nfor k, v in c.items():\n    print(k, v, '%d%%' % 5, \\\"q'\\\")\nprint('a\\tb & c | d > e')\n\"";
        assert_eq!(through_cmd(code), "a 2 5% q'\nb 1 5% q'\na\tb & c | d > e\n");
        assert_eq!(through_cmd("python -I -c \"\nimport sys\nprint(sys.argv[1:])\n\" x y"), "['x', 'y']\n", "what follows it stays its arguments");
        assert_eq!(through_cmd("echo before && python -I -c \"\nprint(1)\n\" && echo after"), "before \n1\nafter\n");
    }

    #[cfg(windows)]
    #[test]
    fn a_comment_ends_with_its_line_when_the_lines_are_joined() {
        assert_eq!(on_one_line("rem first we build\npython x.py\nrem then we test\npython y.py").unwrap(), "python x.py & python y.py");
        assert_eq!(on_one_line(":: note\r\npython x.py").unwrap(), "python x.py");
        assert_eq!(on_one_line("@REM note\nREM other\necho a").unwrap(), "echo a");
        assert_eq!(on_one_line("echo a\nrem only a note").unwrap(), "echo a");
        assert_eq!(on_one_line("remove.exe a\nremark b").unwrap(), "remove.exe a & remark b", "a word that only begins so");
        assert_eq!(through_cmd("rem note\necho ran"), "ran\n", "where cmd alone said nothing and ended with 0");
    }

    #[cfg(windows)]
    #[test]
    fn what_opens_on_the_screen_is_seen_as_cmd_reads_it() {
        let dir = crate::in_container::tempdir().unwrap();
        let opens = |line: &str| opened_on_the_desktop(line, dir.path());
        assert_eq!(opens("x.t^xt").as_deref(), Some("x.txt"));
        assert_eq!(opens("^x.py").as_deref(), Some("x.py"));
        assert_eq!(opens("cmd/c x.txt").as_deref(), Some("x.txt"));
        assert_eq!(opens("cmd /c\"x.txt\"").as_deref(), Some("x.txt"));
        assert_eq!(opens("cmd.exe/d/c x.py").as_deref(), Some("x.py"));
        assert_eq!(opens("c^md /c x.txt").as_deref(), Some("x.txt"));
        assert_eq!(opens("start/b x.html").as_deref(), Some("start x.html"));
        assert_eq!(opens("s^tart x.html").as_deref(), Some("start x.html"));
        assert_eq!(opens("cmd/c echo hi"), None);
        assert_eq!(opens("start/b cmd"), None);
    }

    #[test]
    fn a_file_too_large_to_hold_is_not_read_to_say_so() {
        let dir = crate::in_container::tempdir().unwrap();
        let large = dir.path().join("model.gguf");
        std::fs::File::create(&large).unwrap().set_len(MOST_READ_WHOLE + 1).unwrap();
        let said = read_file(&json!({"path": "model.gguf"}), dir.path()).err().unwrap();
        assert!(said.contains("too large"), "{said}");
    }

    #[test]
    fn a_search_leaves_out_what_else_holds_keys_as_it_does_what_reads_as_keys() {
        let dir = crate::in_container::tempdir().unwrap();
        let (app, keys) = (dir.path().join("DogmindApp"), dir.path().join("keys"));
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&keys).unwrap();
        std::fs::write(app.join("sessions.jsonl"), "needle in a session\n").unwrap();
        std::fs::write(keys.join("openai.txt"), "needle in a key\n").unwrap();
        std::fs::write(dir.path().join("a.txt"), "needle in a file\n").unwrap();
        let secrets = [app.clone(), keys.join("openai.txt")];
        let cancel = AtomicBool::new(false);
        let said = search_files(&json!({"pattern": "needle"}), dir.path(), &secrets, &cancel).unwrap();
        assert!(said.contains("needle in a file") && !said.contains("in a session") && !said.contains("in a key") && said.contains("2 files"), "{said}");
        // The person has answered for that folder when it is what is searched.
        assert!(search_files(&json!({"pattern": "needle", "path": app.to_str().unwrap()}), dir.path(), &secrets, &cancel).unwrap().contains("in a session"));
    }

    #[test]
    fn a_repositorys_worktree_config_and_its_submodules_start_no_program_either() {
        let dir = crate::in_container::tempdir().unwrap();
        let run = |args: &[&str]| { git(dir.path(), args).unwrap(); };
        run(&["init", "-q"]);
        run(&["config", "extensions.worktreeConfig", "true"]);
        run(&["config", "--worktree", "filter.planted.clean", "evil"]);
        run(&["config", "filter.other.smudge", "evil"]);
        let settings = git_without_programs(dir.path());
        assert!(settings.contains(&"filter.planted.clean=".to_string()), "{settings:?}");
        assert!(settings.contains(&"filter.other.smudge=".to_string()), "{settings:?}");
        assert!(settings.contains(&"diff.ignoreSubmodules=all".to_string()), "{settings:?}");
    }

    #[cfg(windows)]
    #[test]
    fn a_command_for_a_place_it_cannot_run_in_is_refused_before_anything_looks_there() {
        let dir = crate::in_container::tempdir().unwrap();
        let said = execute_command(&json!({"command": "x.py", "cwd": "\\\\127.0.0.1\\nothing-here"}), dir.path(), &AtomicBool::new(false), None).unwrap_err();
        assert!(said.contains("outside the workspace") && said.contains("was not started"), "{said}");
    }

    #[cfg(windows)]
    #[test]
    fn what_cannot_go_on_one_line_is_refused_and_what_is_one_line_is_left_alone() {
        let refused = on_one_line("git commit -m \"first\nsecond\"").unwrap_err();
        assert!(refused.starts_with("not run") && refused.contains("several `-m`"), "{refused}");
        assert!(matches!(on_one_line("python -c \"print(1)\"\n"), Ok(std::borrow::Cow::Borrowed("python -c \"print(1)\""))));
        assert_eq!(on_one_line("python -c \"print(1)\"\nnode -e \"\nconsole.log(1)\n\"").unwrap(), "python -c \"print(1)\" & node -e \"eval('\\nconsole.log(1)\\n')\"");
        assert_eq!(runs_code("cd a && C:\\py\\python.exe -X utf8 -c "), Some("exec"));
        assert_eq!(runs_code("echo -c "), None);
    }

    #[cfg(windows)]
    #[test]
    fn what_a_for_goes_through_opens_nothing_and_what_it_runs_is_as_before() {
        let dir = crate::in_container::tempdir().unwrap();
        std::fs::write(dir.path().join("app.log"), "x").unwrap();
        assert_eq!(opened_on_the_desktop("for %f in (app.log) do @echo %~zf bytes", dir.path()), None);
        assert_eq!(opened_on_the_desktop("@for %f in (*.log) do echo %f", dir.path()), None);
        assert_eq!(opened_on_the_desktop("(app.log)", dir.path()).as_deref(), Some("app.log"));
        assert_eq!(opened_on_the_desktop("echo a & app.log", dir.path()).as_deref(), Some("app.log"));
    }

    #[test]
    fn a_listing_says_how_large_a_large_file_is() {
        let (dir, mut bench, cancel) = bench();
        std::fs::write(dir.path().join("small.log"), "x").unwrap();
        std::fs::write(dir.path().join("app.log"), vec![b'x'; 1_677_722]).unwrap();
        std::fs::write(dir.path().join("agent.rs"), vec![b'x'; 248 * 1024 + 7]).unwrap();
        std::fs::write(dir.path().join("lib.rs"), vec![b'x'; 31 * 1024]).unwrap();
        let said = bench.run("list_files", &json!({"path": "."}), dir.path(), &cancel).unwrap();
        assert_eq!(said, "agent.rs (248 KB)\napp.log (1.6 MB)\nlib.rs\nsmall.log");
    }

    #[test]
    fn a_slip_in_an_answer_is_told_from_what_was_read_and_from_chinese_on_purpose() {
        assert_eq!(slips_in_answer("会员割引を適用します。\n| 会员数 | 约 76 GB |", ""), "员约");
        assert_eq!(slips_in_answer("会員割引を適用します。", ""), "", "good Japanese");
        assert_eq!(slips_in_answer("頰と剝がすと塡補", ""), "", "Japanese outside Shift_JIS");
        assert_eq!(slips_in_answer("ファイル名は 会员.txt です", "会员.txt\n"), "", "it was given that");
        assert_eq!(slips_in_answer("中国語では「会员」と書きます。", ""), "", "it says it is Chinese");
        assert_eq!(slips_in_answer("这是一个测试我们需要检查系统。\nこちらが訳です。", ""), "", "a whole line of Chinese is one asked for");
    }

    #[test]
    fn chinese_in_a_japanese_conversation_is_pointed_out_where_it_is_written() {
        assert_eq!(not_japanese("Gerstner 波を叠加したデモ。海の波、重ね合わせ、髙、ｱ。青透过"), ['叠', '过']);
        let (dir, mut bench, cancel) = bench();
        let plain = bench.run("create_file", &json!({"path": "a.txt", "content": "波を叠加した"}), dir.path(), &cancel).unwrap();
        assert!(!plain.contains("note:"), "not in another language's conversation: {plain}");
        bench.in_japanese = true;
        let said = bench.run("create_file", &json!({"path": "b.txt", "content": "波を叠加した"}), dir.path(), &cancel).unwrap();
        assert!(said.contains("note: 叠 in this text is Chinese"), "{said}");
        let fine = bench.run("create_file", &json!({"path": "c.txt", "content": "波を重ねた"}), dir.path(), &cancel).unwrap();
        assert!(!fine.contains("note:"), "{fine}");
        let edit = json!({"edits": [{"path": "c.txt", "old_string": "重ねた", "new_string": "透过"}]});
        let said = bench.run("apply_patch", &edit, dir.path(), &cancel).unwrap();
        assert!(said.contains("note: 透过 in this text is Chinese") || said.contains("note: 过 in this text is Chinese"), "{said}");
    }

    #[test]
    fn a_page_check_summary_names_the_target_the_browser_uses() {
        let url = "http://127.0.0.1:8765/index.html";
        assert_eq!(summary("check_page", &json!({"url": url})), url);
        assert_eq!(summary("check_page", &json!({"url": format!(" {url}?q=one#part "), "path": "other.html"})), url);
        assert_eq!(summary("check_page", &json!({"url": "  ", "path": "index.html"})), "index.html");
        assert_eq!(summary("check_page", &json!({"path": "index.html"})), "index.html");
    }

    #[test]
    fn a_patch_needs_no_list_of_files_and_says_where_it_landed_in_few_words() {
        let (dir, mut bench, cancel) = bench();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        bench.run("read_file", &json!({"path": "a.txt"}), dir.path(), &cancel).unwrap();
        let edit = json!({"edits": [{"path": "a.txt", "old_string": "one", "new_string": "two"}]});
        let said = bench.run("apply_patch", &edit, dir.path(), &cancel).unwrap();
        assert!(said.starts_with("--- applied_diff a.txt ---"), "{said}");
        assert_eq!(summary("apply_patch", &edit), "a.txt");
        assert_eq!(summary("apply_patch", &json!({"expected_files": ["b.txt"], "edits": [{"path": "b.txt"}]})), "b.txt", "a session from before");
        assert_eq!(summary("apply_patch", &json!({"edits": [{"path": "a"}, {"path": "b"}, {"path": "a"}]})), "a, b");
    }

    #[test]
    fn the_range_recommended_by_an_outline_is_shown_before_it_is_marked_read() {
        let (dir, mut bench, cancel) = bench();
        let text = format!("# section\n{}", "body\n".repeat(READ_LINES));
        std::fs::write(dir.path().join("large.md"), text).unwrap();
        let mut read = |args| bench.run("read_file", &args, dir.path(), &cancel).unwrap();
        let outline = json!({"path": "large.md"});
        let first = json!({"path": "large.md", "start_line": 1});
        let short = json!({"path": "large.md", "limit": 500});
        assert!(read(outline.clone()).contains("--- outline ---"));
        assert!(read(first.clone()).contains("2000|body\n"));
        let part = read(short.clone());
        assert!(part.contains("500|body\n") && !part.contains("501|body\n"));
        assert_eq!(read(json!({"path": "large.md", "limit": READ_LINES})), UNCHANGED_READ);
        for args in [outline, first.clone(), short] {
            assert_eq!(read(args), UNCHANGED_READ);
        }
        // An edit beyond the first page starts a new version even if that page reads the same.
        let updated = format!("# section\n{}new last line\n", "body\n".repeat(READ_LINES));
        std::fs::write(dir.path().join("large.md"), updated).unwrap();
        assert!(read(first).contains("2000|body\n"));
    }

    #[test]
    fn the_same_lines_of_an_unchanged_file_are_not_sent_twice() {
        let (dir, mut bench, cancel) = bench();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let read = |bench: &mut Workbench, arguments: Value| bench.run("read_file", &arguments, dir.path(), &cancel).unwrap();
        assert!(read(&mut bench, json!({"path": "a.txt"})).contains("1|one"));
        assert_eq!(read(&mut bench, json!({"path": "a.txt"})), UNCHANGED_READ);
        assert!(read(&mut bench, json!({"path": "a.txt", "start_line": 2})).contains("2|two"), "other lines are other lines");
        bench.run("apply_patch", &json!({"edits": [{"path": "a.txt", "old_string": "one", "new_string": "ONE"}]}), dir.path(), &cancel).unwrap();
        assert!(read(&mut bench, json!({"path": "a.txt"})).contains("1|ONE"), "changed");
        bench.forget_reads();
        assert!(read(&mut bench, json!({"path": "a.txt"})).contains("1|ONE"), "gone from the conversation");
        // An edit still has what it needs after an answer of no lines.
        assert_eq!(read(&mut bench, json!({"path": "a.txt"})), UNCHANGED_READ);
        bench.run("apply_patch", &json!({"edits": [{"path": "a.txt", "old_string": "two", "new_string": "TWO"}]}), dir.path(), &cancel).unwrap();
    }

    #[test]
    fn a_file_is_shown_as_the_agent_reads_it_and_a_binary_is_not_shown() {
        let utf16 = utf16_with_its_mark();
        assert_eq!(text_of_file(&utf16), Some(("日本語 ok\r\n".to_string(), "utf-16")), "what PowerShell's > writes has NULs all through it");
        assert_eq!(text_of_file("plain 見出し".as_bytes()), Some(("plain 見出し".to_string(), "utf-8")));
        assert_eq!(text_of_file(&[0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA]), Some(("日本語".to_string(), "shift_jis")));
        assert_eq!(text_of_file(b"MZ\0\0 binary"), None);
        assert_eq!(text_of_file(b""), Some((String::new(), "utf-8")));
    }

    /// "日本語 ok\r\n" as UTF-16 little-endian with its mark, which is what a Japanese PowerShell's `>` writes.
    fn utf16_with_its_mark() -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "日本語 ok\r\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    /// What a command has printed so far is read as the final output is: a double-byte character of the console's code page that has come in part is left for the next look and shown
    /// whole then, and what came before it is not garbled.
    #[cfg(windows)]
    #[test]
    fn what_a_running_command_has_printed_is_read_as_what_it_printed_when_it_ends() {
        use windows_sys::Win32::Globalization::GetOEMCP;
        // (the console's code page on a Japanese Windows: this is what is looked at, and on another it would be another)
        if unsafe { GetOEMCP() } != 932 {
            return;
        }
        // "日本語" and then "です" in Shift_JIS, the first chunk cut in the middle of the third character
        let all: Vec<u8> = vec![0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA, 0x82, 0xC5, 0x82, 0xB7];
        let live = Live::default();
        live.push(&all[..5]);
        let (first, next) = live.since(0, LIVE_SHOWN);
        assert_eq!(first, "日本", "the character that has come in part waits");
        assert_eq!(next, 4);
        live.push(&all[5..]);
        let (rest, after) = live.since(next, LIVE_SHOWN);
        assert_eq!(rest, "語です", "and is whole in the next look");
        assert_eq!(after, all.len() as u64);
        // (all at once, it reads as the final output does)
        let once = Live::default();
        once.push(&all);
        assert_eq!(once.since(0, LIVE_SHOWN).0, crate::sandbox::decode(&all));
        // (UTF-8 as asked for, cut in the middle of a character, waits the same way)
        let utf8 = "ありがとう".as_bytes();
        let split = Live::default();
        split.push(&utf8[..4]);
        let (first, next) = split.since(0, LIVE_SHOWN);
        assert_eq!((first.as_str(), next), ("あ", 3));
        split.push(&utf8[4..]);
        assert_eq!(split.since(next, LIVE_SHOWN).0, "りがとう");
        // (a look that nothing new has come to is empty, and does not move)
        assert_eq!(split.since(utf8.len() as u64, LIVE_SHOWN), (String::new(), utf8.len() as u64));
    }

    /// Plain text and UTF-8 are read as they were, whatever the code page.
    #[test]
    fn what_a_running_command_has_printed_in_plain_text_is_unchanged() {
        let live = Live::default();
        live.push(b"line one\r\nline two\r\n");
        assert_eq!(live.since(0, LIVE_SHOWN), ("line one\r\nline two\r\n".to_string(), 20));
        live.push("日本語\n".as_bytes());
        assert_eq!(live.since(20, LIVE_SHOWN).0, "日本語\n");
    }

    /// Two outputs kept in the same millisecond are two files, each with its own words.
    #[test]
    fn two_outputs_kept_in_one_millisecond_take_two_files() {
        let dir = crate::in_container::tempdir().unwrap();
        let first = keep_whole_in(dir.path(), "command", "20261105-120000000", "first output").unwrap();
        let second = keep_whole_in(dir.path(), "command", "20261105-120000000", "second output").unwrap();
        let third = keep_whole_in(dir.path(), "command", "20261105-120000000", "third output").unwrap();
        assert!(first != second && second != third && first != third);
        assert_eq!((std::fs::read_to_string(&first).unwrap(), std::fs::read_to_string(&second).unwrap(), std::fs::read_to_string(&third).unwrap()), ("first output".to_string(), "second output".to_string(), "third output".to_string()));
        assert!(second.file_name().unwrap().to_string_lossy().ends_with("-1.txt"), "{second:?}");
        // (on threads of their own, as commands end)
        let place = dir.path().to_path_buf();
        let kept: Vec<PathBuf> = (0..8).map(|at| { let place = place.clone(); std::thread::spawn(move || keep_whole_in(&place, "search", "same", &format!("output {at}")).unwrap()) }).collect::<Vec<_>>().into_iter().map(|thread| thread.join().unwrap()).collect();
        let mut names: Vec<_> = kept.iter().collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 8, "{kept:?}");
        let mut written: Vec<String> = kept.iter().map(|file| std::fs::read_to_string(file).unwrap()).collect();
        written.sort();
        assert_eq!(written, (0..8).map(|at| format!("output {at}")).collect::<Vec<_>>());
    }

    /// A job that has ended is said to have, with what it came to; one still at work is being ended. A command that went on in the background after it was waited for began when it began.
    #[test]
    fn command_stop_of_a_job_that_has_ended_says_so_and_a_job_counts_from_its_real_start() {
        let mut bench = Workbench::default();
        let (done, running): (CommandResult, CommandResult) = (Arc::new(Mutex::new(Some(Ok("stdout:\nfinished\n\nexit_code: 0".to_string())))), Arc::new(Mutex::new(None)));
        let arguments = json!({"command": "make"});
        let ended = bench.add_job(&arguments, Instant::now(), Arc::new(AtomicBool::new(false)), done, Arc::new(Live::default())).unwrap();
        let said = bench.job_stop(&json!({"id": ended})).unwrap();
        assert!(said.contains("had already ended") && said.contains("nothing was stopped") && said.contains("finished") && !said.contains("is being ended"), "{said}");
        assert!(!bench.jobs.0[0].cancel.load(Ordering::Relaxed), "nothing was told to stop");
        // (a job that is running, which began a while before it was left in the background)
        let began = Instant::now().checked_sub(Duration::from_secs(42)).unwrap();
        let at_work = bench.add_job(&json!({"command": "build"}), began, Arc::new(AtomicBool::new(false)), running, Arc::new(Live::default())).unwrap();
        let status = bench.job_status(&json!({"id": at_work}), &AtomicBool::new(false)).unwrap();
        let seconds: u64 = status.split("for ").nth(1).and_then(|rest| rest.split(" s").next()).and_then(|number| number.parse().ok()).unwrap();
        assert!((42..50).contains(&seconds), "counted from its real start, not from when it was left: {status}");
        assert_eq!(bench.job_stop(&json!({"id": at_work})).unwrap(), format!("{at_work} is being ended; it will not wake you"));
        assert!(bench.jobs.0[1].cancel.load(Ordering::Relaxed));
        assert_eq!(bench.job_stop(&json!({"id": "job9"})).unwrap_err(), "there is no job9");
    }

    #[test]
    fn a_command_past_its_time_is_left_at_work_and_a_stop_ends_it() {
        let (stop, cancel) = (AtomicBool::new(false), AtomicBool::new(false));
        let result: CommandResult = Arc::new(Mutex::new(None));
        assert!(wait_for_command(&result, &stop, &cancel, Duration::from_millis(30)).is_none());
        assert!(!stop.load(Ordering::Relaxed), "it goes on");
        *result.lock().unwrap() = Some(Ok("done".to_string()));
        assert_eq!(wait_for_command(&result, &stop, &cancel, Duration::from_secs(5)).unwrap().unwrap(), "done");
        // The person stops the turn: it ends, however long there was to wait, and its answer is waited for.
        cancel.store(true, Ordering::Relaxed);
        let (late, setter) = (result.clone(), std::thread::spawn({
            let result = result.clone();
            move || {
                std::thread::sleep(Duration::from_millis(60));
                *result.lock().unwrap() = Some(Ok("cut short".to_string()));
            }
        }));
        assert_eq!(wait_for_command(&late, &stop, &cancel, Duration::from_millis(1)).unwrap().unwrap(), "cut short");
        assert!(stop.load(Ordering::Relaxed));
        setter.join().unwrap();
    }

    #[test]
    fn what_is_cut_from_a_command_keeps_its_end() {
        let text = format!("start\n{}\nthe test failed at the end", "x".repeat(50_000));
        let kept = cap_evenly(&text, 1000, None);
        assert!(kept.len() < 1200 && kept.starts_with("start") && kept.ends_with("the test failed at the end") && kept.contains("truncated"));
        assert_eq!(cap_evenly("short", 1000, None), "short");
    }

    /// What is cut from a long output is kept whole, and the report says where: the model reads the rest there, not by running it again.
    #[test]
    fn a_long_outputs_whole_is_kept_where_the_report_says() {
        let out = format!("first line\n{}\nlast line", "middle\n".repeat(20_000));
        let file = kept_whole(&out, "a warning").unwrap();
        let kept = std::fs::read_to_string(&file).unwrap();
        assert!(kept.contains("first line") && kept.contains("last line") && kept.contains("stderr:\na warning"), "{}", &kept[..80]);
        let report = cap_evenly(&out, 1000, Some(&file));
        assert!(report.contains(&format!("all of it is in {}", file.display())) && report.contains("read_file"), "{report}");
        let _ = std::fs::remove_file(file);
    }

    /// A long result that was cut with no copy (a diff, a background command's report, an MCP tool's or a helper's answer) is kept whole and
    /// the cut names where: the part between head and tail can be read back. A short one is as it was.
    #[test]
    fn a_long_result_is_kept_whole_and_the_cut_says_where() {
        assert_eq!(spill("diff", "short", 1000), "short");
        let text: String = (0..2000).map(|line| format!("line {line}\n")).collect();
        let shown = spill("diff", &text, 1000);
        assert!(shown.len() < 1400, "{}", shown.len());
        assert!(shown.starts_with("line 0\n") && shown.ends_with("line 1999\n"));
        let named = shown.split("all of it is in ").nth(1).and_then(|rest| rest.split(':').next()).map(str::to_string);
        let mut file = named.expect("the cut names the file");
        // (a Windows path's drive letter is followed by a colon too)
        if file.len() == 1 {
            file = shown.split("all of it is in ").nth(1).unwrap().split(": read").next().unwrap().to_string();
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
        assert!(std::path::Path::new(&file).file_name().unwrap().to_string_lossy().starts_with("diff-"));
    }

    #[cfg(windows)]
    #[test]
    fn a_pipe_to_head_or_tail_passes_the_output_on_and_nothing_else_is_touched() {
        assert_eq!(with_stand_ins("set -e & python -m pytest -q 2>&1 | head -60"), "set -e & python -m pytest -q 2>&1 | findstr \"^\"");
        for line in ["x | head", "x | tail -5", "x | tail -n 20", "x | head -n5"] {
            assert_eq!(with_stand_ins(line), "x | findstr \"^\"", "{line}");
        }
        for same in ["head -5 a.txt", "x | head -c 5", "x | tail -f", "x || head -5", "echo head -5", "x | headline -5", "x | head -5 a.txt"] {
            assert_eq!(with_stand_ins(same), same, "{same}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn dir_and_del_in_their_plain_forms_get_a_stand_in_and_nothing_else_is_touched() {
        for same in ["echo dir", "mkdir a", "python x.py", "dir > out.txt", "dir %TEMP%", "if exist x (del x)", "for %f in (*) do del %f", "del /s a.txt", "del /p a.txt", "del", "echo a & echo dir", "findstr \"dir | del\" a.txt", "dir\necho x",
                     "if exist x (dir /b x) else (echo none)", "(dir /b) & echo x", "for %f in (*) do @echo %f", "echo (a & dir", "for %f in (*.js) do dir %TEMP%"] {
            assert_eq!(with_stand_ins(same), same, "{same}");
        }
        let dir = with_stand_ins("dir vendor");
        assert!(dir.starts_with("(if exist \"vendor\\*\"") && dir.contains("for /d %d in (\"vendor\\*\")") && !dir.contains("dir vendor"), "{dir}");
        assert_eq!(with_stand_ins("dir"), with_stand_ins("dir ."));
        assert_eq!(with_stand_ins("dir /b vendor 2>nul"), with_stand_ins("dir vendor"));
        assert_eq!(with_stand_ins("dir/b vendor"), with_stand_ins("dir vendor"));
        assert_eq!(with_stand_ins("DIR \"my folder\\\""), with_stand_ins("dir \"my folder\""));
        assert!(with_stand_ins("dir /s *.js").contains("for /r \".\" %d in (*.js) do @echo %d"));
        assert!(with_stand_ins("dir /s vendor").contains("for /r \"vendor\" %d in (*) do @echo %d"));
        let line = with_stand_ins("mkdir vendor 2>nul & curl -o vendor/a.js https://x.example/a.js & dir vendor");
        assert!(line.starts_with("mkdir vendor 2>nul & curl -o vendor/a.js https://x.example/a.js & (if exist"), "{line}");
        let del = with_stand_ins("del /q a.txt \"b c.txt\"");
        assert!(del.starts_with("(powershell -NoProfile -NonInteractive -Command ") && del.matches("powershell").count() == 2 && !del.contains("del "), "{del}");
        for file in ["a.txt", "b c.txt"] {
            assert!(del.contains(&format!("[IO.File]::Delete('{file}')")) && del.contains(&format!("Could Not Find {file}")), "{file}: {del}");
        }
        assert!(with_stand_ins("erase *.log").contains("for %d in (\"*.log\") do @powershell"));
        let piped = with_stand_ins("dir /b vendor | findstr three");
        assert!(piped.starts_with("((for /d %d in (\"vendor\\*\")") && !piped.contains("if exist") && piped.ends_with("| findstr three"), "{piped}");
        assert!(with_stand_ins("dir vendor || echo none").contains("if exist"), "a second chance is not a pipe");
        assert_eq!(with_stand_ins("del it's.txt"), "del it's.txt");
    }

    /// In a line with parentheses, a `dir` cmd runs as a command of its own gets its stand-in: one before or after a `for`, one in a `for`'s body (with the `for`'s
    /// variable as its word), and the command a `for /f` reads; the stand-in's loop takes a letter of its own.
    #[cfg(windows)]
    #[test]
    fn a_dir_beside_or_in_a_for_gets_its_stand_in_too() {
        let first = with_stand_ins("dir /b *.html && for %f in (*.html) do @echo %~zf");
        assert!(first.starts_with("(if exist \"*.html\\*\"") && first.ends_with("&& for %f in (*.html) do @echo %~zf") && !first.contains("dir /b"), "{first}");
        let body = with_stand_ins("for %F in (a.html b.html) do @echo === %F & dir \"%F\" | find \"html\"");
        assert!(body.starts_with("for %F in (a.html b.html) do @echo === %F & ((for /d %d in (\"%F\\*\")") && body.ends_with("| find \"html\"") && !body.contains("dir \""), "{body}");
        let read = with_stand_ins("for /f %f in ('dir /b /s crates\\*.rs') do @echo %~zf %f");
        assert_eq!(read, "for /f %f in ('for /r \"crates\" %d in ^(*.rs^) do @echo %d') do @echo %~zf %f");
        // (the line's own loop is `%d`: the stand-in's is another letter, and the line's `%d` stays the line's)
        let own = with_stand_ins("for %d in (a b) do dir %d");
        assert!(own.contains("for /d %e in (\"%d\\*\")") && !own.contains("for /d %d"), "{own}");
        // (with a `del` beside it, the `del` is left: it runs in the sandbox as it is)
        let both = with_stand_ins("for %f in (*.tmp) do del %f & dir /b");
        assert!(both.starts_with("for %f in (*.tmp) do del %f & (if exist"), "{both}");
    }

    /// A path written with `/` is given to cmd's commands with `\`, and `mkdir -p` makes no folder named `-p`; what `findstr` looks for, switches and addresses stay as written.
    #[cfg(windows)]
    #[test]
    fn a_path_with_slashes_is_given_to_cmd_with_backslashes() {
        for (written, runs) in [
            ("findstr /n /c:\"three.core\" gacha/three.module.js", "findstr /n /c:\"three.core\" gacha\\three.module.js"),
            ("findstr from gacha/a.js gacha/b.js", "findstr from gacha\\a.js gacha\\b.js"),
            ("type \"docs/a b.md\"", "type \"docs\\a b.md\""),
            ("mkdir -p gacha/assets && copy a.txt gacha/assets/", "mkdir gacha\\assets && copy a.txt gacha\\assets\\"),
            ("mkdir -p gacha", "mkdir gacha"),
            ("cd gacha && findstr /i three x/y.js | findstr import", "cd gacha && findstr /i three x\\y.js | findstr import"),
            ("  @move a/b.txt c/", "  @move a\\b.txt c\\"),
        ] {
            assert_eq!(with_stand_ins(written), runs, "{written}");
            assert_eq!(with_stand_ins(runs), runs, "{runs}: once is all");
        }
        for same in [
            "findstr src/main.rs Cargo.toml", "findstr /s /i \"a/b\"", "curl -sL -o gacha/x.js https://cdn.example/x.js", "copy /y a.txt b.txt", "type %TEMP%/x.txt",
            "type a/b.txt > c.txt", "copy https://x.example/a b", "mkdir a", "python tools/x.py", "md \"unclosed/x",
        ] {
            assert_eq!(with_stand_ins(same), same, "{same}");
        }
    }

    /// PowerShell in a folder under the users' folder is put on a drive whose root is the folder: its command first goes there, and a script of `-File` with plain
    /// words is run by its full path with its exit code; elsewhere, and in other forms, the line is as it was.
    #[test]
    fn powershell_under_the_users_folder_is_put_where_it_was_started() {
        let (users, cwd) = (Path::new("C:\\Users"), Path::new("C:\\Users\\me\\src\\app"));
        let goes = "$null=New-PSDrive -Name DogmindWork -PSProvider FileSystem -Root 'C:\\Users\\me\\src\\app' -Scope Global; Set-Location DogmindWork:\\;";
        assert_eq!(powershell_in_place("powershell -NoProfile -Command \"Set-Content a.txt x\"", cwd, Some(users)), format!("powershell -NoProfile -Command {goes} \"Set-Content a.txt x\""));
        assert_eq!(powershell_in_place("echo hi & pwsh -nop -ExecutionPolicy Bypass -c Get-ChildItem", cwd, Some(users)), format!("echo hi & pwsh -nop -ExecutionPolicy Bypass -c {goes} Get-ChildItem"));
        assert_eq!(powershell_in_place("powershell -NoProfile -File .\\build.ps1 -Target release", cwd, Some(users)),
            format!("powershell -NoProfile -Command \"{goes} & 'C:\\Users\\me\\src\\app\\.\\build.ps1' -Target release; exit $LASTEXITCODE\""));
        assert_eq!(powershell_in_place("\"C:\\Program Files\\PowerShell\\7\\pwsh.exe\" -Command \"ls\"", cwd, Some(users)), format!("\"C:\\Program Files\\PowerShell\\7\\pwsh.exe\" -Command {goes} \"ls\""));
        for same in [
            "powershell -NoProfile -Command \"ls\"",
        ] {
            assert_eq!(powershell_in_place(same, Path::new("C:\\work\\app"), Some(users)), same, "not under the users' folder");
        }
        for same in ["powershell -File .\\a.ps1 \"x y\"", "powershell -EncodedCommand AAAA", "echo powershell -Command x", "python make.py", "powershell Get-Date"] {
            assert_eq!(powershell_in_place(same, cwd, Some(users)), same, "{same}");
        }
        assert_eq!(powershell_in_place("powershell -Command ls", Path::new("C:\\Users\\me\\100%\\x"), Some(users)), "powershell -Command ls", "a name cmd would read a variable into");
    }

    /// A `git init` under the users' folder is made whole when git could not write HEAD: on the branch it names, else the person's default, asked once and only
    /// for a line that has one; a line made so, and other forms and places, are as they were.
    #[test]
    fn a_git_init_under_the_users_folder_is_made_whole() {
        let users = Path::new(r"C:\Users");
        let (under, outside) = (Path::new(r"C:\Users\someone\Documents\app"), Path::new(r"C:\work\app"));
        let asked = std::cell::Cell::new(0);
        let ask = || {
            asked.set(asked.get() + 1);
            "main"
        };
        let whole = |line: &str| git_init_whole(line, under, Some(users), || "main");
        let made = |git: &str, branch: &str, again: &str, there: &str| {
            format!(r"|| (if not exist {git}\HEAD (>{git}\HEAD echo ref: refs/heads/{branch}&& git init {again} && git {there}symbolic-ref HEAD refs/heads/{branch}) else (call))")
        };
        assert_eq!(whole("git init"), format!("git init {}", made(".git", "main", "-q", "")));
        assert_eq!(whole("git init && git add -A && git commit -m first"), format!("git init {} && git add -A && git commit -m first", made(".git", "main", "-q", "")));
        assert_eq!(whole("mkdir web & git init -b trunk --template=t web/site & cd web"),
            format!("mkdir web & git init -b trunk --template=t web/site {} & cd web", made(r"web\site\.git", "trunk", "-q --template=t web/site", "-C web/site ")));
        assert_eq!(whole("git init --quiet --initial-branch=dev"), format!("git init --quiet --initial-branch=dev {}", made(".git", "dev", "-q", "")));
        let once = whole("git init");
        assert_eq!(whole(&once), once, "made once");
        for same in ["git init --bare", "git init a b", r"git init C:\elsewhere", "git init --separate-git-dir=x", "git init --ref-format=reftable", "git init | more",
            "git init -b \"a b\"", "git initx", "git status", "echo git init"] {
            assert_eq!(whole(same), same, "{same}");
        }
        assert_eq!(git_init_whole("git init", outside, Some(users), ask), "git init", "outside the users' folder");
        assert_eq!(git_init_whole("git status && python x.py", under, Some(users), ask), "git status && python x.py");
        assert_eq!(git_init_whole("git init -b dev", under, Some(users), ask).contains("refs/heads/dev"), true);
        assert_eq!(asked.get(), 0, "the default is asked only when a line needs it");
        git_init_whole("git init a & git init b", under, Some(users), ask);
        assert_eq!(asked.get(), 1, "and once for a line");
    }

    /// A venv of a system's Python is made in two steps, with no pip and then its own ensurepip. Which Python is the line's: with the bundle, a bare `python`
    /// is its own and is left as it is, one named by its path is a system's unless it is in the bundle's folder, and the launcher's are the system's. Two
    /// folders, options with no pip already, or anything else, are as they were.
    #[test]
    fn a_venv_of_the_systems_python_is_made_in_two_steps() {
        let two = |program: &str, folder: &str| format!(r"{program} -m venv --without-pip {folder} && {folder}\Scripts\python -m ensurepip --upgrade --default-pip");
        assert_eq!(venv_in_two_steps("python -m venv .venv", None), two("python", ".venv"));
        assert_eq!(venv_in_two_steps("python -m venv --clear env/dev && env\\dev\\Scripts\\pip install -r requirements.txt", None),
            "python -m venv --clear --without-pip env\\dev && env\\dev\\Scripts\\python -m ensurepip --upgrade --default-pip && env\\dev\\Scripts\\pip install -r requirements.txt");
        assert_eq!(venv_in_two_steps(r"python -m venv env\dev", None), two("python", r"env\dev"), "a folder with backslashes");
        let bundle = Some(Path::new(r"C:\Apps\Dogmind\tools\python"));
        for (line, program) in [
            (r"C:\Python314\python.exe -m venv .venv", r"C:\Python314\python.exe"),
            (r#""C:\Program Files\Python314\python.exe" -m venv .venv"#, r#""C:\Program Files\Python314\python.exe""#),
        ] {
            assert_eq!(venv_in_two_steps(line, bundle), two(program, ".venv"), "a system's Python, with the bundle there");
        }
        for same in ["python -m venv .venv", "py -m venv .venv", "py -3.14 -m venv .venv", r"C:\Apps\Dogmind\tools\python\python.exe -m venv .venv", r#""c:/apps/dogmind/tools/python/python.exe" -m venv .venv"#] {
            assert_eq!(venv_in_two_steps(same, bundle), same, "the bundle's Python (its py is a shim of its own)");
        }
        for (line, program) in [("py -3.14 -m venv .venv", "py -3.14"), ("py -m venv .venv", "py")] {
            assert_eq!(venv_in_two_steps(line, None), two(program, ".venv"), "the launcher, with no bundle");
        }
        for same in ["python -m venv a b", "python -m venv --without-pip .venv", "python -m venvx .venv", "python script.py -m venv", "pyx -m venv .venv"] {
            assert_eq!(venv_in_two_steps(same, None), same, "{same}");
        }
    }

    /// The note of a `git init` made whole says it is whole only where the git folder it wrote HEAD in has its objects now.
    #[test]
    fn a_git_init_made_whole_is_said_whole_only_when_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let runs = git_init_whole("git init", Path::new(r"C:\Users\someone\app"), Some(Path::new(r"C:\Users")), || "main");
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(git_init_note(&runs, dir.path()).contains("and what that said is above"), "no objects folder yet");
        std::fs::create_dir_all(dir.path().join(".git").join("objects")).unwrap();
        assert!(git_init_note(&runs, dir.path()).contains("the repository is whole"));
    }

    /// What a person was shown must be what runs: a line this has made is not changed by being gone over again.
    #[cfg(windows)]
    #[test]
    fn a_line_with_its_stand_ins_in_is_left_as_it_is() {
        for line in ["dir", "dir /s *.js", "del /q a.txt \"b c.txt\"", "erase *.log", "x | head -60"] {
            let once = with_stand_ins(line);
            assert_ne!(once, line, "{line}: something was put in");
            assert_eq!(with_stand_ins(&once), once, "{line}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_line_that_runs_is_the_line_the_sandbox_is_given() {
        for same in ["echo hi", "python x.py", "if exist x (del x)", "dir > out.txt", "echo a\necho b", "python -c \"print(1)\nprint(2)\""] {
            assert_eq!(line_that_runs(same), same, "joining the lines alone is not told of: {same}");
        }
        assert_eq!(line_that_runs("del a.txt"), with_stand_ins("del a.txt"));
        assert!(line_that_runs("del a.txt").starts_with("(powershell -NoProfile -NonInteractive -Command "));
        assert_eq!(line_that_runs("x | head -60"), "x | findstr \"^\"");
        assert_eq!(line_that_runs("dir /s *.js"), with_stand_ins("dir /s *.js"));
        // Lines are joined before the stand-ins go in, so that a `del` on the second one is replaced too.
        let two = line_that_runs("echo a\ndel a.txt");
        assert_eq!(two, format!("echo a & {}", with_stand_ins("del a.txt")), "{two}");
        // A line that is refused is not run, so there is nothing else for it to be.
        assert_eq!(line_that_runs("echo \"a\nb\"\ndel x"), "echo \"a\nb\"\ndel x");
    }

    /// A repository with `pkg` and `other` folders, each with a file committed, and a change to each: the workspace is `pkg`.
    fn repository_with_a_folder() -> tempfile::TempDir {
        let dir = crate::in_container::tempdir().unwrap();
        let run = |args: &[&str]| {
            let mut all = vec!["-c", "user.name=t", "-c", "user.email=t@example.com"];
            all.extend(args);
            git(dir.path(), &all).unwrap();
        };
        run(&["init", "-q"]);
        for folder in ["pkg", "other"] {
            std::fs::create_dir_all(dir.path().join(folder)).unwrap();
            std::fs::write(dir.path().join(folder).join("a.txt"), "a\n").unwrap();
        }
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "first"]);
        std::fs::write(dir.path().join("pkg").join("a.txt"), "b\n").unwrap();
        std::fs::write(dir.path().join("other").join("a.txt"), "OTHER-CHANGE\n").unwrap();
        dir
    }

    #[test]
    fn the_repository_above_a_folder_is_told_from_one_that_is_the_folder() {
        let dir = repository_with_a_folder();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        let top = repository_above(&dir.path().join("pkg")).expect("pkg is a folder of the repository");
        assert_eq!(std::fs::canonicalize(&top).unwrap(), real);
        assert!(!top.display().to_string().starts_with("\\\\?"), "{}", top.display());
        assert_eq!(repository_above(dir.path()), None, "the top itself");
        let alone = crate::in_container::tempdir().unwrap();
        assert_eq!(repository_above(alone.path()), None, "no repository");
        assert_eq!(repository_above(&alone.path().join("not-there")), None);
    }

    /// In a folder of a larger repository, the scopes that run git for the app work and show this folder's changes and not the repository's other ones.
    #[test]
    fn show_diff_in_a_folder_of_a_repository_shows_that_folders_changes() {
        let dir = repository_with_a_folder();
        let pkg = dir.path().join("pkg");
        for scope in ["workspace", "unstaged"] {
            let said = show_diff(&json!({"scope": scope}), &pkg, None).unwrap();
            assert!(said.contains("+b") && !said.contains("OTHER-CHANGE"), "{scope}: {said}");
        }
        git(&pkg, &["add", "a.txt"]).unwrap();
        let staged = show_diff(&json!({"scope": "staged"}), &pkg, None).unwrap();
        assert!(staged.contains("+b") && !staged.contains("OTHER-CHANGE"), "{staged}");
        assert!(show_diff(&json!({"scope": "unstaged"}), &pkg, None).unwrap().starts_with("no differences"));
    }

    /// cargo in the sandbox, a workspace of `members = ["crates/*"]` under the user folder, as it was said in a run.
    #[cfg(windows)]
    #[test]
    fn cargo_that_cannot_expand_a_members_pattern_is_told_why() {
        let dir = crate::in_container::tempdir().unwrap();
        let err = "error: failed to load manifest for workspace member `C:\\Users\\a\\work\\crates/*`\nreferenced by workspace at `C:\\Users\\a\\work\\Cargo.toml`\n\nCaused by:\n  \
                   failed to read `C:\\Users\\a\\work\\crates\\*\\Cargo.toml`\n\nCaused by:\n  ファイル名、ディレクトリ名、またはボリューム ラベルの構文が間違っています。 (os error 123)\n";
        let note = sandbox_notes("cargo test -p app", dir.path(), Some(101), err);
        assert!(note.contains("`members` pattern") && note.contains("run cargo directly") && note.contains("for the user to run themselves"), "{note}");
        assert!(note.contains("nothing else on its line") && note.contains("Do not change the workspace's Cargo.toml"), "{note}");
        // a member named as it is, which is missing, is the project's own
        let named = "error: failed to load manifest for workspace member `C:\\Users\\a\\work\\crates\\gone`\n";
        assert!(!sandbox_notes("cargo build", dir.path(), Some(101), named).contains("`members` pattern"));
    }

    #[cfg(windows)]
    #[test]
    fn git_failing_in_a_folder_of_a_repository_says_whose_repository_it_is() {
        let dir = repository_with_a_folder();
        let pkg = dir.path().join("pkg");
        let top = repository_above(&pkg).unwrap();
        for err in ["fatal: error reading 'C:/Projects/DogmindApp/.git'", "fatal: not a git repository (or any of the parent directories): .git", "fatal: failed to stat '        C:/Users': Permission denied"] {
            let note = sandbox_notes("git status --short", &pkg, Some(128), err);
            assert!(note.contains(&format!("part of the git repository at `{}`", top.display())) && note.contains("cannot reach its `.git`") && note.contains("`show_diff`") && note.contains("command to run themselves"), "{err}: {note}");
            assert!(!note.contains("not a git repository. `git init`"), "{err}: {note}");
        }
        // not a command of git's, and a folder that is no repository's: as before
        assert!(!sandbox_notes("dir", &pkg, Some(1), "fatal: not a git repository").contains("part of the git repository"));
        let alone = crate::in_container::tempdir().unwrap();
        assert!(sandbox_notes("git status", alone.path(), Some(128), "fatal: failed to stat 'C:/Users': Permission denied").contains("not a git repository. `git init`"));
        assert_eq!(sandbox_notes("git status", alone.path(), Some(128), "fatal: not a git repository (or any of the parent directories): .git"), "");
    }

    /// A command that did not find `cl`, `link` or `nmake` is told why the build tools were not given, for each reason, and what the person can install.
    #[cfg(windows)]
    #[test]
    fn a_missing_compiler_is_told_with_the_reason_the_build_tools_were_not_given() {
        use crate::msvc::Why;
        let root = Path::new("C:\\work");
        let english = "'cl' is not recognized as an internal or external command,\noperable program or batch file.";
        let japanese = "'link' \u{306f}\u{3001}\u{5185}\u{90e8}\u{30b3}\u{30de}\u{30f3}\u{30c9}\u{307e}\u{305f}\u{306f}\u{5916}\u{90e8}\u{30b3}\u{30de}\u{30f3}\u{30c9}\u{3001}\u{64cd}\u{4f5c}\u{53ef}\u{80fd}\u{306a}\u{30d7}\u{30ed}\u{30b0}\u{30e9}\u{30e0}\u{307e}\u{305f}\u{306f}\u{30d0}\u{30c3}\u{30c1} \u{30d5}\u{30a1}\u{30a4}\u{30eb}\u{3068}\u{3057}\u{3066}\u{8a8d}\u{8b58}\u{3055}\u{308c}\u{3066}\u{3044}\u{307e}\u{305b}\u{3093}\u{3002}";
        for (why, wanted) in [(Why::NotX64, "not an x64 one"), (Why::NoInstaller, "installer was not found"), (Why::NoCppTools, "without the C++ tools"), (Why::NoToolset, "not all there"), (Why::NoSdk, "no complete Windows SDK")] {
            crate::msvc::set_test_why(Some(Some(why)));
            for (line, err, program) in [("cl main.c", english, "cl"), ("link /?", japanese, "link"), ("nmake", "'nmake' is not recognized as an internal or external command", "nmake")] {
                let note = sandbox_notes(line, root, Some(1), err);
                assert!(note.contains(&format!("`{program}` was not found because the build tools were not given")) && note.contains(wanted), "{why:?} {program}: {note}");
                if why != Why::NotX64 {
                    assert!(note.contains("Desktop development with C++"), "{why:?}: {note}");
                }
            }
        }
        // no answer yet, and not found: the look may not have ended; nothing for another word not found, or for a command that was fine
        crate::msvc::set_test_why(Some(None));
        // (whether the look has found some by now is the process's, and not for a test to count on: the words are told by the function)
        assert!(build_tools_note("cl", None, false).contains("had not come to an answer") && build_tools_note("cl", None, true).is_empty());
        assert!(!sandbox_notes("foo", root, Some(1), "'foo' is not recognized as an internal or external command").contains("build tools"));
        assert!(!sandbox_notes("cl main.c", root, Some(0), "").contains("build tools"));
        crate::msvc::set_test_why(None);
    }

    /// A command that asks what runs or listens on the PC is told that the sandbox sees only what it started itself, whatever it ended with (netstat's empty list ends
    /// with 0); one that could not connect to an address of the PC's own is told why, and that this does not say the server is down. Nothing for other lines.
    #[cfg(windows)]
    #[test]
    fn what_the_sandbox_cannot_see_of_the_pc_is_said() {
        let root = Path::new("C:\\work");
        for (line, status, err) in [
            ("tasklist /FI \"IMAGENAME eq Unity.exe\"", Some(1), "ERROR: The user name or password is incorrect."),
            ("netstat -ano | findstr LISTENING", Some(1), ""),
            ("netstat -ano", Some(0), ""),
            ("powershell -NoProfile -Command \"Get-Process -Name Unity\"", Some(1), "Get-Process : Cannot find a process with the name \"Unity\"."),
            ("powershell -Command \"Get-NetTCPConnection -State Listen\"", Some(1), "Access denied"),
            ("powershell -Command \"Get-CimInstance Win32_Process | Select Name\"", Some(1), ""),
        ] {
            assert!(sandbox_notes(line, root, status, err).contains("sees only the programs it started itself"), "{line}");
        }
        for (line, err) in [
            ("curl.exe -sS -m 4 http://127.0.0.1:27182/mcp", "curl: (7) Failed to connect to 127.0.0.1:27182 after 0 ms: Could not connect to server"),
            ("curl -s http://localhost:8080/", "curl: (28) Connection timed out after 3011 milliseconds"),
            ("powershell -Command \"Invoke-WebRequest http://localhost:3000\"", "Invoke-WebRequest : Unable to connect to the remote server"),
            ("python -c \"import socket; socket.create_connection(('127.0.0.1', 27182))\"", "ConnectionRefusedError: [WinError 10061] No connection could be made because the target machine actively refused it"),
        ] {
            let note = sandbox_notes(line, root, Some(1), err);
            assert!(note.contains("cannot connect to a server of this PC's") && note.contains("`/mcp`") && note.contains("`outside_sandbox: true`"), "{line}: {note}");
        }
        // (a server elsewhere, a port on the PC that answered, and lines that only mention a process)
        assert!(!sandbox_notes("curl -s https://example.com/", root, Some(7), "curl: (7) Failed to connect to example.com port 443").contains("this PC's"));
        assert!(!sandbox_notes("curl -s http://127.0.0.1:8000/", root, Some(0), "").contains("this PC's"));
        assert!(!sandbox_notes("curl -s http://127.0.0.1:8000/", root, Some(22), "curl: (22) The requested URL returned error: 404").contains("this PC's"));
        for line in ["taskkill /PID 1234", "type tasklist.txt.bak", "cargo build -p netstatus"] {
            assert!(!sandbox_notes(line, root, Some(1), "").contains("sees only the programs"), "{line}");
        }
    }

    /// In the real sandbox: `tasklist` there says a password is wrong, and what the model is given says why.
    #[cfg(windows)]
    #[test]
    fn a_process_list_asked_in_the_sandbox_comes_with_why_it_is_not_one() {
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let dir = crate::in_container::tempdir().unwrap();
        let said = execute_command(&json!({"command": "tasklist"}), dir.path(), &AtomicBool::new(false), None).unwrap();
        assert!(said.contains("sees only the programs it started itself"), "{said}");
    }

    #[cfg(windows)]
    #[test]
    fn a_sandboxs_answer_that_reads_as_something_else_is_explained() {
        let root = Path::new("C:\\work");
        // A program that names no path, refused one outside the workspace, in its own words: the note says how to ask for it; refused one inside, it does not.
        let python = |path: &str| sandbox_notes("python clean.py", root, Some(1), &format!("Traceback (most recent call last):\n  File \"C:\\work\\clean.py\", line 2\nPermissionError: [WinError 5] Access is denied: '{path}'"));
        assert!(python("C:\\\\Users\\\\me\\\\old.txt").contains("`outside_sandbox: true`"));
        assert!(!python("C:\\\\work\\\\out\\\\a.txt").contains("`outside_sandbox: true`"));
        assert!(sandbox_notes("where python", root, Some(1), "INFO: Could not find files for the given pattern(s).").contains("does not say the program is missing"));
        assert!(sandbox_notes("powershell -NoProfile -Command \"Get-Item a\"", root, Some(1), "Get-Item : Cannot find path 'C:\\a' because it does not exist.").contains("absolute paths"));
        assert!(sandbox_notes("powershell -NoProfile -Command \"Get-ChildItem\"", root, Some(1), "PermissionDenied: (C:\\:String) [Get-ChildItem]").contains("absolute paths"));
        assert!(sandbox_notes("pwsh -c \"Start-Process x\"", root, Some(1), "Start-Process: Das Laufwerk wurde nicht gefunden. Ein Laufwerk mit dem Namen 'Microsoft.PowerShell.Core\\FileSystem' ist nicht vorhanden.").contains("-WorkingDirectory"));
        assert!(sandbox_notes("git status", root, Some(128), "fatal: failed to stat '        C:/Users': Permission denied").contains("not a git repository"));
        for (line, err) in [("git config user.name x", "error: could not lock config file .git/config: Permission denied"), ("git remote add o u", "error: could not lock config file .git/config: Permission denied"), ("echo x> .git\\hooks\\pre-commit", "Access is denied."), ("del .git\\config", "アクセスが拒否されました。")] {
            assert!(sandbox_notes(line, Path::new("C:\\work"), Some(1), err).contains("Tell the user the exact command"), "{line}");
        }
        assert!(!sandbox_notes("git status", Path::new("C:\\work"), Some(1), "fatal: bad").contains("Tell the user"));
        // (by name, whether it worked or not, a server is ended for every conversation; by its PID or id it is not said)
        assert!(sandbox_notes("taskkill /F /IM python.exe /T", root, Some(0), "").contains("command_stop"));
        assert!(sandbox_notes("powershell -Command \"Stop-Process -Name node\"", root, Some(1), "").contains("command_stop"));
        assert!(!sandbox_notes("taskkill /PID 4242 /F", root, Some(0), "").contains("command_stop"));
        let refused = sandbox_notes("type C:\\elsewhere\\a.txt", root, Some(1), "Access is denied.");
        assert!(refused.contains("cannot read or change anything outside the workspace") && refused.contains("`outside_sandbox: true`"), "{refused}");
        assert!(sandbox_notes("type C:\\elsewhere\\a.txt", root, Some(1), "アクセスが拒否されました。").contains("`read_file`"));
        // On another drive, and in another language, the same.
        assert!(sandbox_notes("powershell -c \"Get-Item a\"", Path::new("E:\\work"), Some(1), "Get-Item : Der Pfad \"D:\\a\" kann nicht gefunden werden.\n    + CategoryInfo : ObjectNotFound: (D:\\a:String) [Get-Item], ItemNotFoundException\n    + FullyQualifiedErrorId : PathNotFound,Microsoft.PowerShell.Commands.GetItemCommand").contains("absolute paths"));
        assert!(sandbox_notes("powershell -c \"Get-ChildItem\"", Path::new("E:\\work"), Some(1), "Get-ChildItem : Der Zugriff wurde verweigert\n  + CategoryInfo : PermissionDenied: (D:\\:String) [Get-ChildItem], UnauthorizedAccessException").contains("absolute paths"));
        assert!(!sandbox_notes("powershell -c \"Get-Item E:\\work\\a\"", Path::new("E:\\work"), Some(1), "    + CategoryInfo : ObjectNotFound: (E:\\work\\a:String) [Get-Item], ItemNotFoundException").contains("absolute paths"), "the workspace's own file is only missing");
        assert!(sandbox_notes("where python", root, Some(1), "INFORMATION: Es wurden keine Dateien mit dem angegebenen Muster gefunden.").contains("Run the program itself"));
        assert!(!sandbox_notes("where /?", root, Some(2), "FEHLER").contains("Run the program itself"), "something else went wrong");
        for (err, by) in [("PermissionError: [WinError 5] Zugriff verweigert: 'x'", "Python"), ("PermissionError: [Errno 13] Permission denied: 'x'", "Python"), ("Error: EPERM: operation not permitted, open 'x'", "Node"), ("Error: EACCES: permission denied, open 'x'", "Node"), ("Zugriff verweigert (os error 5)", "Rust"), ("Ausnahme von HRESULT: 0x80070005", "COM"), ("+ FullyQualifiedErrorId : UnauthorizedAccess, System.UnauthorizedAccessException", "PowerShell")] {
            assert!(says_access_denied(err, ""), "{by}: {err}");
            assert!(sandbox_notes("type D:\\elsewhere\\a.txt", root, Some(1), err).contains("cannot read or change anything outside the workspace"), "{by}");
        }
        // What only has words, in the words of a German Windows.
        assert!(says_access_denied("Zugriff verweigert\r\n", "Zugriff verweigert"));
        assert!(!says_access_denied("Zugriff verweigert\r\n", ""), "as this test's Windows would not know them");
        assert!(!says_access_denied("Datei nicht gefunden", "Zugriff verweigert"));
        assert!(!says_access_denied("exit 5", "Zugriff verweigert"), "a code of a program's own says nothing");
        let here = system_access_denied();
        assert!(!here.is_empty() && !here.ends_with('.') && says_access_denied(&format!("{here}.\r\n"), here), "{here}");
        for (line, status, err) in [("type C:\\work\\a.txt", Some(1), "Access is denied."), ("where python", Some(0), ""), ("python x.py", Some(1), "boom"), ("powershell -c x", Some(1), "boom")] {
            assert_eq!(sandbox_notes(line, root, status, err), "", "{line}");
        }
    }

    /// A real model's `set CARGO_TARGET_DIR=%TEMP%\sd-isolation-target && cargo build`: cmd kept the space before `&&` in the value, and cargo failed on a path that ended in a space.
    #[cfg(windows)]
    #[test]
    fn a_set_whose_value_ends_in_a_space_before_the_ampersands_is_told_to_quote_it() {
        let root = Path::new("C:\\work");
        let joined = on_one_line("set A=b\ncargo build").unwrap();
        assert_eq!(joined, "set A=b & cargo build", "two lines are joined with the space");
        for line in ["set A=b && x", "set CARGO_TARGET_DIR=%TEMP%\\sd-isolation-target && cargo build", "SET A=b &&x", "set A=b & x", "set A=b || x", "set A=b | x", "@set A=b  && x", "cd src && set A=b && x", "echo y & set A=b && x", "(set A=b && x)", "set A=\"b c\" && x", "set\tA=b && x", &*joined] {
            // (whatever the program that follows made of it: it may well have run, with a value that is not the one written)
            for (status, err) in [(Some(0), ""), (Some(1), "boom"), (Some(9009), ""), (None, "")] {
                let notes = sandbox_notes(line, root, status, err);
                assert!(notes.contains("the space before `&&`") && notes.contains("`set \"NAME=value\" && ...`"), "{line} {status:?}: {notes}");
                assert!(notes.lines().all(|note| note.starts_with("note: ")) && notes.lines().count() == 1, "{line}: {notes}");
            }
        }
        for line in [
            "set \"A=b\" && x", "set A=b&& x", "set \"A=b\"&& x", "set \"A=b & c\" && x", "set A=b", "set A=b ", "x && y", "cargo build",
            "echo set A=b && x", "reset A=b && x", "rem set A=b && x", "set /a n=1 && x", "set /p n=ask && x", "echo \"set A=b \" && x", "set && x", "set  && x", "xset A=b && x",
        ] {
            for (status, err) in [(Some(0), ""), (Some(1), "boom")] {
                assert_eq!(sandbox_notes(line, root, status, err), "", "{line} {status:?}");
            }
        }
    }

    /// Chrome stopped by its crash handler in the sandbox, as it said it on its error stream or, with `2>&1`, in its output: the model is told of check_page. Nothing else is.
    /// A refusal names its path as cargo, git, icacls and PowerShell do, and cargo's `..` are worked out.
    #[test]
    fn the_path_a_refusal_names_is_read_as_each_program_writes_it() {
        let cargo = r"error: couldn't read `crates\dogmind-agent\src\../../../assets/skills/modern-web-guidance/SKILL.md`: アクセスが拒否されました。 (os error 5)";
        assert!(named_paths(cargo).contains(&PathBuf::from(r"assets\skills\modern-web-guidance\SKILL.md")), "{:?}", named_paths(cargo));
        assert_eq!(named_paths("assets/skills/modern-web-guidance/SOURCE: Permission denied"), [PathBuf::from(r"assets\skills\modern-web-guidance\SOURCE")]);
        assert_eq!(named_paths(r"assets\skills: Access is denied."), [PathBuf::from(r"assets\skills")]);
        assert!(named_paths(r"Set-Content : Access to the path 'C:\w\a.txt' is denied.").contains(&PathBuf::from(r"C:\w\a.txt")));
    }

    /// A refused path under a folder another account owns is said so, with that folder and its owner; one under the user's own folders is not. (Windows'
    /// own folders stand in for one another account owns: nothing here can make such a folder without being an administrator.)
    #[test]
    fn a_refusal_under_a_folder_another_account_owns_names_it() {
        let windows = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()));
        let note = foreign_owner_note(r"System32\config\SAM: Access is denied.", &windows).expect("System32 is TrustedInstaller's");
        assert!(note.contains(&windows.join("System32").display().to_string()) && note.contains("TrustedInstaller"), "{note}");
        let mine = crate::in_container::tempdir().unwrap();
        std::fs::create_dir_all(mine.path().join("a").join("b")).unwrap();
        assert_eq!(foreign_owner_note("a/b/c.txt: Permission denied", mine.path()), None);
        assert_eq!(foreign_owner_note("System32: no such thing", &windows), None, "only a refusal");
    }

    #[test]
    fn chrome_that_cannot_start_in_the_sandbox_points_to_check_page() {
        let fatal = "[1121/211815.239:FATAL:third_party\\crashpad\\crashpad\\client\\crashpad_client_win.cc:331] Check failed: . CreateNamedPipe: アクセスが拒否されました。 (0x5)";
        assert!(chrome_note("", fatal).is_some_and(|note| note.contains("check_page")));
        assert!(chrome_note(fatal, "").is_some());
        assert!(chrome_note("crashpad_client_win.cc is a file", "").is_none());
        assert!(chrome_note("", "").is_none());
    }

    /// A Unix habit that fails says what to use instead, whether cmd does not know the program (in English or in Japanese) or Git's own copy of it fails at its start.
    #[cfg(windows)]
    #[test]
    fn a_unix_tool_that_fails_is_told_not_to_run_here_and_what_does_the_same() {
        let root = Path::new("C:\\work");
        let english = "'wc' is not recognized as an internal or external command,\r\noperable program or batch file.\r\n";
        let japanese = "'wc' は、内部コマンドまたは外部コマンド、\r\n操作可能なプログラムまたはバッチ ファイルとして認識されていません。\r\n";
        let msys = "      0 [main] cat (4021) C:\\Program Files\\Git\\usr\\bin\\cat.exe: *** fatal error - NtCreateDirectoryObject(&dir, 0xC0000022) failed\r\n";
        for (line, status, err) in [("wc -l a.txt", Some(1), english), ("wc -l a.txt", Some(9009), japanese), ("dir | wc -l", Some(1), english), ("cd src & type a.txt && grep x a.txt", Some(1), english), ("\"C:\\Git\\usr\\bin\\ls.exe\" -l", Some(1), msys), ("cat a.txt", Some(127), msys), ("@echo off & head a.txt", Some(1), japanese)] {
            let notes = sandbox_notes(line, root, status, err);
            assert!(notes.contains("Unix tools do not run in the sandbox") && notes.contains("`read_file`, `list_files`, `find_files` and `search_files`"), "{line}: {notes}");
            assert_eq!(notes.contains("usr\\bin"), err == msys, "{line}: the MSYS note is for the MSYS failure: {notes}");
            assert!(notes.lines().all(|note| note.starts_with("note: ")) && notes.lines().count() <= 2, "{line}: {notes}");
        }
        for line in ["rm a.txt", "cp a b & mv b c", "dir | mv x y"] {
            assert!(sandbox_notes(line, root, Some(1), english).contains("`del`, `copy` and `move` do what `rm`, `cp` and `mv` do"), "{line}");
        }
        assert!(!sandbox_notes("grep x a.txt", root, Some(1), english).contains("`del`, `copy`"), "only where one of those was tried");
        // The MSYS failure of any program is said, and the Unix tools are not named when none was tried.
        let hook = sandbox_notes("git commit -m x", root, Some(1), msys);
        assert!(hook.contains("programs from Git's `usr\\bin` (MSYS) cannot start in the sandbox, git hooks included") && !hook.contains("Unix tools"), "{hook}");
        // Not for a command that went well, nor a failure that is something else, nor a program that is not one of these.
        assert_eq!(sandbox_notes("wc -l a.txt", root, Some(0), ""), "");
        assert_eq!(sandbox_notes("echo x | wc -l", root, Some(0), ""), "");
        assert_eq!(sandbox_notes("wc -l a.txt", root, Some(0), msys), "", "it went well");
        assert_eq!(sandbox_notes("ls", root, Some(1), "boom"), "");
        assert_eq!(sandbox_notes("grep x a.txt", root, Some(1), "FINDSTR: Cannot open a.txt"), "");
        assert_eq!(sandbox_notes("frobnicate x", root, Some(1), "'frobnicate' is not recognized as an internal or external command,"), "");
        assert_eq!(sandbox_notes("echo wc -l & frobnicate", root, Some(1), english), "", "a word of an argument is not a program");
        assert_eq!(sandbox_notes("foo | head -5", root, Some(1), english), "", "`| head` is replaced, and does not run");
    }

    #[test]
    fn a_skill_named_as_the_list_itself_is_read_and_otherwise_the_list_comes() {
        let (dir, mut bench, cancel) = bench();
        let skill = |name: &str| crate::skills::Skill { name: name.into(), description: "Does a thing".into(), folder: dir.path().to_path_buf(), scope: "user", for_the_model: true, for_the_person: true, ..Default::default() };
        bench.skills = vec![skill("one"), skill("two")];
        let said = bench.run("skill", &json!({"name": "list"}), dir.path(), &cancel).unwrap();
        assert!(said.contains("- one: Does a thing") && said.contains("- two:"), "{said}");
    }

    /// The project's `pdf` that only the person may start does not make the model's `skill("pdf")` say there is none: the person's own that the model may start is read.
    #[test]
    fn the_skill_tool_reads_the_first_of_a_name_that_the_model_may_start() {
        let (dir, mut bench, cancel) = bench();
        let (home, work) = (dir.path().join("me"), dir.path().join("work"));
        let write = |folder: PathBuf, text: &str| {
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join("SKILL.md"), text).unwrap();
        };
        std::fs::create_dir_all(work.join(".git")).unwrap();
        write(work.join(".claude").join("skills").join("pdf"), "---\nname: pdf\ndescription: d\ndisable-model-invocation: true\n---\nthe project's, for the person");
        write(home.join(".claude").join("skills").join("pdf"), "---\nname: pdf\ndescription: d\n---\nthe home's, for the model");
        write(work.join(".claude").join("skills").join("manual"), "---\nname: manual\ndescription: d\ndisable-model-invocation: true\n---\nonly the person");
        bench.skills = crate::skills::discover(&work, Some(&home));
        let said = bench.run("skill", &json!({"name": "pdf"}), &work, &cancel).unwrap();
        assert!(said.contains("the home's, for the model") && !said.contains("for the person"), "{said}");
        let none = bench.run("skill", &json!({"name": "manual"}), &work, &cancel).unwrap_err();
        assert!(none.contains("no skill is called manual"), "{none}");
    }

    #[test]
    fn a_truncated_listing_counts_files_separately_from_directories() {
        let dir = crate::in_container::tempdir().unwrap();
        for i in 0..LIST_LIMIT {
            std::fs::create_dir(dir.path().join(i.to_string())).unwrap();
        }
        std::fs::write(dir.path().join("one.txt"), b"abc").unwrap();
        let out = list_files(&json!({"recursive": true}), dir.path(), &AtomicBool::new(false)).unwrap();
        assert!(out.contains(&format!("of {} entries; counted 1 file (3 bytes)", LIST_LIMIT + 1)), "{}", out.lines().last().unwrap());
    }

    #[test]
    fn a_named_file_that_cannot_be_searched_is_not_reported_as_no_matches() {
        let dir = crate::in_container::tempdir().unwrap();
        let search = |path: &str| search_files(&json!({"path": path, "pattern": "absent"}), dir.path(), &[], &AtomicBool::new(false));
        assert!(search("missing.txt").unwrap_err().contains("missing.txt"));
        std::fs::File::create(dir.path().join("large.txt")).unwrap().set_len(SEARCH_FILE_LIMIT + 1).unwrap();
        assert!(search("large.txt").unwrap_err().contains("16 MiB"));
        std::fs::write(dir.path().join("binary"), b"absent\0binary").unwrap();
        assert!(search("binary").unwrap_err().contains("binary"));
        std::fs::write(dir.path().join("plain.txt"), "ordinary text").unwrap();
        assert_eq!(search("plain.txt").unwrap(), "no matches\n");
        assert_eq!(search(".").unwrap(), "no matches\n");
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let _locked = std::fs::OpenOptions::new().read(true).share_mode(0).open(dir.path().join("plain.txt")).unwrap();
            assert!(search("plain.txt").unwrap_err().contains("plain.txt"));
        }
    }

    #[test]
    fn search_reads_utf16_and_utf8_bom_files_and_ignores_binaries() {
        let dir = crate::in_container::tempdir().unwrap();
        let line = "日本語\r\nsecond";
        let le = [0xFF, 0xFE].into_iter().chain(line.encode_utf16().flat_map(u16::to_le_bytes)).collect::<Vec<_>>();
        let be = [0xFE, 0xFF].into_iter().chain(line.encode_utf16().flat_map(u16::to_be_bytes)).collect::<Vec<_>>();
        std::fs::write(dir.path().join("le.txt"), le).unwrap();
        std::fs::write(dir.path().join("be.txt"), be).unwrap();
        std::fs::write(dir.path().join("bom.txt"), format!("\u{feff}{line}")).unwrap();
        std::fs::write(dir.path().join("binary"), "日本語\n\0binary").unwrap();
        #[cfg(windows)]
        std::fs::write(dir.path().join("cp932.txt"), [0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA]).unwrap();
        let found = search_files(&json!({"pattern": "^日本語$"}), dir.path(), &[], &AtomicBool::new(false)).unwrap();
        let mut lines: Vec<_> = found.lines().collect();
        lines.sort_unstable();
        #[cfg(windows)]
        assert_eq!(lines, ["be.txt:1:日本語", "bom.txt:1:日本語", "cp932.txt:1:日本語", "le.txt:1:日本語"]);
        #[cfg(not(windows))]
        assert_eq!(lines, ["be.txt:1:日本語", "bom.txt:1:日本語", "le.txt:1:日本語"]);
    }

    #[test]
    fn a_search_shows_the_match_even_near_the_end_of_a_long_line() {
        let (dir, mut bench, cancel) = bench();
        let needle = "検索🐕".repeat(95);
        let contents = format!("見出し\n{}{}末尾\n", "前".repeat(400), needle);
        std::fs::write(dir.path().join("long.txt"), contents).unwrap();
        let result = bench.run("search_files", &json!({"pattern": needle}), dir.path(), &cancel).unwrap();
        assert!(result.starts_with("long.txt:2:…"));
        assert!(result.contains(&needle));
        assert!(result.contains("末尾"));
    }

    #[test]
    fn a_search_uses_smart_case_and_takes_bad_regex_as_text() {
        let (dir, mut bench, cancel) = bench();
        std::fs::write(dir.path().join("a.rs"), "Hello world\nfn(x\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "hello\n").unwrap();
        let all = bench.run("search_files", &json!({"pattern": "hello"}), dir.path(), &cancel).unwrap();
        assert!(all.contains("a.rs:1:") && all.contains("b.txt:1:"), "{all}");
        let exact = bench.run("search_files", &json!({"pattern": "Hello"}), dir.path(), &cancel).unwrap();
        assert!(exact.contains("a.rs:1:") && !exact.contains("b.txt"), "{exact}");
        let glob = bench.run("search_files", &json!({"pattern": "hello", "file_glob": "*.txt"}), dir.path(), &cancel).unwrap();
        assert!(!glob.contains("a.rs") && glob.contains("b.txt"), "{glob}");
        let bad = bench.run("search_files", &json!({"pattern": "fn(x"}), dir.path(), &cancel).unwrap();
        assert!(bad.contains("searched as literal") && bad.contains("a.rs:2:"), "{bad}");
    }

    #[test]
    fn a_search_glob_filters_the_files_left_by_gitignore() {
        let (dir, mut bench, cancel) = bench();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "generated.json\ncache/\n").unwrap();
        for folder in ["cache", "notes"] {
            std::fs::create_dir(root.join(folder)).unwrap();
        }
        for path in ["config.json", "generated.json", "cache/copy.json", "notes/item.json", "notes/item.txt"] {
            std::fs::write(root.join(path), "match").unwrap();
        }
        let mut search = |path, glob| bench.run("search_files", &json!({"path":path, "pattern":"match", "file_glob":glob}), root, &cancel).unwrap();
        let json = search(".", "*.json");
        assert!(json.contains("config.json") && json.contains("notes/item.json"), "{json}");
        assert!(!json.contains("generated.json") && !json.contains("copy.json") && !json.contains("item.txt"), "{json}");
        let remaining = search(".", "!notes/");
        assert!(remaining.contains("config.json") && !remaining.contains("notes/") && !remaining.contains("generated.json"), "{remaining}");
        assert!(search("generated.json", "*.json").contains(":1:match"));
    }

    #[test]
    fn a_command_reports_its_output_and_exit_code() {
        let dir = crate::in_container::folder_for_commands();
        let (mut bench, cancel) = (Workbench::default(), AtomicBool::new(false));
        let out = bench.run("execute_command", &json!({"command": "echo hello"}), dir.path(), &cancel).unwrap();
        assert!(out.contains("hello") && out.contains("exit_code: 0"), "{out}");
        let failed = bench.run("execute_command", &json!({"command": "exit 3"}), dir.path(), &cancel).unwrap();
        assert!(failed.contains("exit_code: 3"), "{failed}");
    }

    /// In the real sandbox, in a temp folder: a command that makes a junction to another repository of the workspace is told in its own result what the app cannot keep a command from, and the
    /// next one is not started. Nothing of the other repository is changed.
    #[test]
    fn a_link_a_command_made_to_a_repository_is_said_in_its_result_and_the_next_command_is_not_started() {
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let dir = crate::in_container::tempdir().unwrap();
        let mine = dir.path().join("mine\\.git");
        std::fs::create_dir_all(mine.join("hooks")).unwrap();
        std::fs::write(mine.join("hooks\\pre-commit"), "the person's\n").unwrap();
        std::fs::write(mine.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(mine.join("config"), "[alias]\n\tx = !echo\n").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let (mut bench, cancel) = (Workbench::default(), AtomicBool::new(false));
        let first = bench.run("execute_command", &json!({"command": "echo first"}), dir.path(), &cancel).unwrap();
        assert!(first.contains("exit_code: 0") && !first.contains("cannot keep a command"), "{first}");
        let made = bench.run("execute_command", &json!({"command": "mklink /J sub\\.git mine\\.git"}), dir.path(), &cancel).unwrap();
        let link = dir.path().join("sub\\.git");
        if !std::fs::symlink_metadata(&link).is_ok_and(|what| what.file_type().is_symlink()) {
            eprintln!("the sandbox of this machine cannot make a junction to a repository it may write in: {made}");
            return;
        }
        let next = bench.run("execute_command", &json!({"command": "echo second> second.txt"}), dir.path(), &cancel);
        // (the link first, by itself, and not through)
        std::fs::remove_dir(&link).unwrap();
        assert!(made.contains("note: after this command the app found what it cannot keep a command from") && made.contains("is a link") && made.contains("until the user has put that right"), "{made}");
        assert_eq!(std::fs::read_to_string(mine.join("hooks\\pre-commit")).unwrap(), "the person's\n");
        assert_eq!(std::fs::read_to_string(mine.join("config")).unwrap(), "[alias]\n\tx = !echo\n");
        let next = next.unwrap_err();
        assert!(next.contains("is a link") && next.contains("The command was not started") && !dir.path().join("second.txt").exists(), "{next}");
    }

    #[test]
    fn a_long_output_keeps_its_head_and_tail() {
        let text = format!("start{}end", "あ".repeat(10000));
        let capped = cap(&text, 1000);
        assert!(capped.starts_with("start") && capped.ends_with("end") && capped.contains("truncated"));
    }

    #[test]
    fn a_todo_merges_by_id() {
        let (_dir, mut bench, _cancel) = bench();
        bench.todo(&json!({"todos": [{"id": "a", "text": "first"}, {"id": "b", "text": "second"}]})).unwrap();
        let out = bench.todo(&json!({"todos": [{"id": "a", "status": "done"}]})).unwrap();
        assert_eq!(out, "[done] a: first\n[pending] b: second");
    }

    #[test]
    fn a_checklist_is_what_its_calls_leave_when_they_are_replayed() {
        let mut todos = Vec::new();
        apply_todo(&mut todos, &json!({"todos": [{"id": "a", "text": "first"}, {"id": "b", "text": "second"}]})).unwrap();
        apply_todo(&mut todos, &json!({"todos": [{"id": "b", "status": "done"}]})).unwrap();
        let shown = |todos: &[Value]| todos.iter().map(|item| (item["id"].as_str().unwrap().to_string(), item["status"].as_str().unwrap().to_string())).collect::<Vec<_>>();
        assert_eq!(shown(&todos), [("a".to_string(), "doing".to_string()), ("b".to_string(), "done".to_string())], "the first of a new list is the one being done");
        // A call that fails part of the way leaves the items before the failing one in.
        assert_eq!(apply_todo(&mut todos, &json!({"todos": [{"id": "c"}, {"text": "no id"}]})), Err("each todo needs an id".to_string()));
        assert_eq!(todos.len(), 3);
        assert!(apply_todo(&mut todos, &json!({"merge": false})).is_err() && todos.len() == 3, "a call with no list changes none");
        apply_todo(&mut todos, &json!({"todos": [{"id": "z"}], "merge": false})).unwrap();
        assert_eq!(shown(&todos), [("z".to_string(), "doing".to_string())]);
    }
}
