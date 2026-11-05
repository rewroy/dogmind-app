//! The tools, each behaving as its Dogmind namesake says it does in the schema the model reads.

use std::collections::{BTreeMap, HashMap};
#[cfg(not(windows))]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
/// What one `read_file` brings back, whatever its `limit`: a local model pays for each token of it as it goes in, and it stays in the context.
const MAX_READ: usize = 32 * 1024;
/// A line of a minified file, as read: 65 KB of one once came back from a request for two lines.
const MAX_LINE: usize = 2000;
/// What a turn reads of files before it is told how much that is, and of explore: 96 KB, some 30,000 tokens.
const READ_TOLD_PAST: usize = 96 * 1024;
/// A file longer than this, read with no place in it, is shown as where its parts begin: a review that read every file from the top
/// had a context of 197K tokens for 6,600 lines, most of them of no use to it.
const OUTLINE_PAST: usize = 800;
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
pub fn schemas() -> Value {
    let function = |name: &str, description: &str, properties: Value, required: &[&str]| json!({"type": "function", "function": {"name": name, "description": description, "parameters": {"type": "object", "properties": properties, "required": required}}});
    json!([
        function("apply_patch", "Edit existing files by exact text replacement. All edits land together or none do. Read a file before editing it. old_string must occur exactly once (or `count` times) and has no line numbers. After a failure the file is unchanged, so correct old_string instead of reading again.", json!({
            "edits": {"type": "array", "items": {"type": "object", "properties": {"path": {"type": "string"}, "old_string": {"type": "string"}, "new_string": {"type": "string"}, "count": {"type": "integer"}}, "required": ["path", "old_string", "new_string"]}}
        }), &["edits"]),
        function("create_file", "Write a file, new or one you have read. Over about 300 lines, write the first part, then add each further part with append=true: what is on disk survives a stop, and a long call is minutes with nothing to show.", json!({
            "path": {"type": "string"},
            "content": {"type": "string"},
            "append": {"type": "boolean", "description": "Add to the end of the file."}
        }), &["path", "content"]),
        function("execute_command", "Run a program, script, test, build, package manager or git, through cmd.exe in the sandbox the Environment section describes. Output is captured whole, so no redirect to a log and no `| head`. Not for reading files or listing folders.", json!({
            "command": {"type": "string"},
            "cwd": {"type": "string", "description": "Where to run, instead of `cd ... &&`. Default: the workspace."},
            "timeout_seconds": {"type": "integer", "description": "How long to wait for it, default 180. Past that it goes on in the background."},
            "purpose": {"type": "string", "description": "A few words on why, for the user when they are asked."},
            "network": {"type": "boolean", "description": "Needs the network, and is not curl, a package install or remote git, which get it anyway. Asks the user."},
            "background": {"type": "boolean", "description": "Start it and go on, for what takes minutes. Comes back with an id for command_status."}
        }), &["command"]),
        function("list_files", "List a folder. {} is the workspace.", json!({
            "path": {"type": "string"},
            "recursive": {"type": "boolean", "description": "Skips node_modules, .venv and the like."}
        }), &[]),
        function("read_file", "Read a text file as numbered lines, or see a picture where this model can be shown one.", json!({
            "path": {"type": "string"},
            "start_line": {"type": "integer", "description": "1-based. -N starts N lines from the end."},
            "limit": {"type": "integer", "description": "Lines. Default 500, max 2000."}
        }), &["path"]),
        function("search_files", "Search file contents by regex, ignoring case unless the pattern has capitals. Skips what .gitignore skips.", json!({
            "pattern": {"type": "string"},
            "literal": {"type": "boolean", "description": "The pattern is plain text."},
            "path": {"type": "string"},
            "file_glob": {"type": "string"}
        }), &["pattern"]),
        function("show_diff", "Show file changes.", json!({
            "scope": {"type": "string", "enum": ["session", "workspace", "staged", "unstaged"], "description": "session: since this session began. workspace: against HEAD, or outside git as session. staged, unstaged: git's index."},
            "paths": {"type": "array", "items": {"type": "string"}}
        }), &["scope"]),
        function("todo", "A checklist, only for work with several separate deliverables. Send only what changed.", json!({
            "todos": {"type": "array", "items": {"type": "object", "properties": {"id": {"type": "string"}, "text": {"type": "string"}, "status": {"type": "string", "enum": ["pending", "doing", "done", "blocked"]}}, "required": ["id"]}},
            "merge": {"type": "boolean", "description": "false: replace the whole list."}
        }), &["todos"]),
        function("find_files", "Find files by name with a glob such as `**/*.rs`. Skips what .gitignore skips.", json!({
            "pattern": {"type": "string"},
            "path": {"type": "string"}
        }), &["pattern"]),
        function("check_page", "Open a page in a headless browser with no network: what its scripts printed and threw, what it shows, pictures.", json!({
            "path": {"type": "string"},
            "url": {"type": "string", "description": "Instead of path: http://localhost:PORT/ of a server you started"},
            "views": {"type": "array", "items": {"type": "string"}, "description": "desktop (default), mobile, or WxH"},
            "wait_ms": {"type": "integer"},
            "steps": {"type": "string", "description": "JS run after load: await click/fill/press/check/select(sel, ...), wait(ms); text/count/visible(sel); console.log results. A reload (await reload()) starts the next part in the new load; variables do not carry over."}
        }), &[]),
        function("command_status", "A background command's state, and what it printed since the last look.", json!({"id": {"type": "string"}}), &[]),
        function("command_stop", "End a background command.", json!({"id": {"type": "string"}}), &["id"]),
        function("explore", "A look across many files by a helper with its own context: only its answer comes back. It only reads.", json!({"task": {"type": "string", "description": "All it needs: it saw none of this."}}), &["task"])
    ])
}

/// All the tools of a session: those every session has, and those that depend on what is on.
pub fn schemas_for(web: bool, skills: bool, memory: bool, mcp: Vec<Value>) -> Value {
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
    all
}

/// Independent reads run together; everything else runs alone and in order.
pub fn is_read(tool: &str) -> bool {
    matches!(tool, "read_file" | "list_files" | "search_files" | "show_diff" | "find_files")
}

struct Job {
    id: String,
    command: String,
    started: Instant,
    cancel: Arc<AtomicBool>,
    result: Arc<Mutex<Option<Result<String, String>>>>,
    /// It was started to run in the session's scratch folder, and not in the workspace.
    in_scratch: bool,
    /// Its output has been in a report of all of them: the next such report says so in a line.
    told: AtomicBool,
    /// What it has printed while it runs, and how far a look at it has got.
    live: Arc<Live>,
    looked: AtomicU64,
}

/// What a command has printed so far, its two streams as they came, for a look while it runs. A dev server never ends, and what it said
/// (where it listens, the error it stopped on) was out of reach until it did. Only the end is kept.
#[derive(Default)]
pub struct Live(Mutex<(std::collections::VecDeque<u8>, u64)>);

/// What is kept of a running command's output for a look, and what one look shows of what is new.
const LIVE_KEPT: usize = 64 * 1024;
const LIVE_SHOWN: usize = 4 * 1024;

impl Live {
    pub fn push(&self, bytes: &[u8]) {
        let mut kept = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        kept.0.extend(bytes);
        kept.1 += bytes.len() as u64;
        let over = kept.0.len().saturating_sub(LIVE_KEPT);
        kept.0.drain(..over);
    }

    /// What came after its first `from` bytes, at most the last `most` of it, and where the next look starts.
    fn since(&self, from: u64, most: usize) -> (String, u64) {
        let kept = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (buffer, total) = (&kept.0, kept.1);
        let new = total.saturating_sub(from);
        let shown = (new.min(buffer.len() as u64) as usize).min(most);
        let mut bytes: Vec<u8> = buffer.iter().skip(buffer.len() - shown).copied().collect();
        // (a look may start in the middle of a character)
        let cut = bytes.iter().take_while(|byte| (0x80..0xC0).contains(*byte)).count();
        bytes.drain(..cut);
        let mut text = String::new();
        if new > shown as u64 {
            text.push_str(&format!("({} bytes before this are not shown)\n", new - shown as u64));
        }
        text.push_str(&String::from_utf8_lossy(&bytes));
        (text, total)
    }
}

/// Commands going on in the background. They do not outlive the session that started them.
#[derive(Default)]
struct Jobs(Vec<Job>);

impl Drop for Jobs {
    fn drop(&mut self) {
        for job in &self.0 {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }
}

type CommandResult = Arc<Mutex<Option<Result<String, String>>>>;

/// What a command started in the background runs with: not its timeout, which is how long a call waits for it, and a background one
/// does not. As the model gave it, it was how long the job had before it was ended, and a long build was stopped in the middle without a word.
fn background_arguments(arguments: &Value) -> Value {
    let mut given = arguments.clone();
    if let Some(fields) = given.as_object_mut() {
        fields.remove("timeout_seconds");
    }
    given
}

/// A command on a thread of its own, with what ends it and where its result will be. It has minutes to itself unless the call says otherwise.
fn spawn_command(arguments: &Value, workspace: &Path) -> (Arc<AtomicBool>, CommandResult, Arc<Live>) {
    let mut given = arguments.clone();
    if given.get("timeout_seconds").is_none() {
        given["timeout_seconds"] = json!(3600);
    }
    let (cancel, result, live): (_, CommandResult, _) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(None)), Arc::new(Live::default()));
    let (stop, done, place, printed) = (cancel.clone(), result.clone(), workspace.to_path_buf(), live.clone());
    std::thread::spawn(move || {
        let outcome = execute_command(&given, &place, &stop, Some(printed));
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
    /// The hash of each file as this session last saw it: an edit needs a read of what is there now.
    seen: HashMap<PathBuf, String>,
    pub todos: Vec<Value>,
    /// Where the session began, for `show_diff`. None until it has a file to keep that beside.
    pub baseline: Option<Arc<crate::baseline::Baseline>>,
    pub web: crate::web::Web,
    pub skills: Vec<crate::skills::Skill>,
    /// The folder of the notes kept between sessions.
    pub memory: Option<PathBuf>,
    pub mcp: Option<Arc<crate::mcp::Hub>>,
    jobs: Jobs,
    /// A file whose `create_file` the app cut off to get it on disk: the next write to it goes on from there, whether or not the model says append.
    cut_file: Option<PathBuf>,
    /// What has been read of each file and what it was then, while that is still in the conversation.
    reads: HashMap<(PathBuf, i64, i64), String>,
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
fn write_all_or_none(files: &[(PathBuf, String, String)]) -> Result<(), String> {
    let beside: Vec<PathBuf> = files.iter().map(|(path, _, _)| written_beside(path)).collect();
    let forget = |from: usize| {
        for new in &beside[from..] {
            let _ = std::fs::remove_file(new);
        }
    };
    for (at, (path, _, after)) in files.iter().enumerate() {
        let written = std::fs::File::create(&beside[at]).and_then(|mut out| {
            std::io::Write::write_all(&mut out, after.as_bytes())?;
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

/// A long result, whole, in a file of its own kind (`command`, `search`) in the temporary folder.
fn keep_whole_as(kind: &str, whole: &str) -> Option<PathBuf> {
    const KEPT: Duration = Duration::from_secs(7 * 24 * 3600);
    let folder = std::env::temp_dir().join("dogmind-output");
    std::fs::create_dir_all(&folder).ok()?;
    for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
        if entry.metadata().ok().and_then(|meta| meta.modified().ok()).and_then(|at| at.elapsed().ok()).is_some_and(|age| age > KEPT) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let file = folder.join(format!("{kind}-{}.txt", chrono::Local::now().format("%Y%m%d-%H%M%S%3f")));
    std::fs::write(&file, whole).ok()?;
    Some(file)
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
            "list_files" => (list_files(arguments, workspace), None),
            "find_files" => (find_files(arguments, workspace), None),
            "search_files" => (search_files(arguments, workspace, secrets, cancel), None),
            "show_diff" => (show_diff(arguments, workspace, baseline), None),
            other => (Err(format!("{other} is not a read")), None),
        }
    }

    pub fn run(&mut self, tool: &str, arguments: &Value, workspace: &Path, cancel: &AtomicBool) -> Result<String, String> {
        match tool {
            "apply_patch" => self.apply_patch(arguments, workspace),
            "create_file" => self.create_file(arguments, workspace),
            "execute_command" => {
                // A command whose folder is the scratch folder runs there as it would in a workspace: that folder is the only one it may write in.
                let place = self.scratch_for(arguments).unwrap_or_else(|| workspace.to_path_buf());
                if arguments.get("background").and_then(Value::as_bool) == Some(true) {
                    self.start_job(arguments, &place)
                } else {
                    self.run_command(arguments, &place, cancel)
                }
            }
            #[cfg(windows)]
            "check_page" => crate::page_check::check_page(arguments, workspace, cancel),
            "command_status" => self.job_status(arguments),
            "command_stop" => self.job_stop(arguments),
            "todo" => self.todo(arguments),
            "skill" => self.read_skill(arguments),
            _ if tool.starts_with("mcp__") => {
                let hub = self.mcp.as_ref().ok_or("no MCP server is running")?;
                // What a server says may come from anywhere: it is data, as a page is.
                hub.call(tool, arguments, cancel).map(|text| crate::web::envelope(tool, &format!("{text}\n")))
            }
            "remember" | "forget" => {
                let folder = self.memory.as_deref().ok_or("notes are not kept here")?;
                if tool == "remember" { crate::memory::remember(folder, workspace, arguments) } else { crate::memory::forget(folder, workspace, arguments) }
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
        #[cfg(windows)]
        crate::sandbox::also_readable(&skill.folder);
        crate::skills::instructions(skill)
    }

    fn start_job(&mut self, arguments: &Value, workspace: &Path) -> Result<String, String> {
        let (cancel, result, live) = spawn_command(&background_arguments(arguments), workspace);
        let id = self.add_job(arguments, cancel, result, live)?;
        Ok(format!("started as {id}. Go on with other work, and use command_status for it; do not wait in a loop."))
    }

    fn add_job(&mut self, arguments: &Value, cancel: Arc<AtomicBool>, result: CommandResult, live: Arc<Live>) -> Result<String, String> {
        let id = format!("job{}", self.jobs.0.len() + 1);
        let command = text_arg(arguments, "command")?.to_string();
        let in_scratch = self.scratch_for(arguments).is_some();
        self.jobs.0.push(Job { id: id.clone(), command, started: Instant::now(), cancel, result, in_scratch, told: AtomicBool::new(false), live, looked: AtomicU64::new(0) });
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
        let (stop, result, live) = spawn_command(&given, workspace);
        if let Some(outcome) = wait_for_command(&result, &stop, cancel, wait) {
            return outcome;
        }
        let id = self.add_job(arguments, stop, result, live)?;
        Ok(format!("still running after {} s, so it goes on in the background as {id}. Go on with other work and use command_status for it: do not run it again, and do not wait in a loop.", wait.as_secs()))
    }

    fn job_status(&self, arguments: &Value) -> Result<String, String> {
        let wanted = arguments.get("id").and_then(Value::as_str);
        // (asked for all of them, an ended one whose output was in such a report already is a line: all of them again each time pushed the
        // middle ones out of the report, and the model was not told)
        let report = |job: &Job| match &*job.result.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
            Some(_) if wanted.is_none() && job.told.load(Ordering::Relaxed) => format!("{} has ended, as reported before (command_status with its id says it again): {}", job.id, job.command),
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
                let first = job.looked.swap(next, Ordering::Relaxed) == 0;
                let said = match (printed.is_empty(), first) {
                    (true, true) => "it has printed nothing yet".to_string(),
                    (true, false) => "it has printed nothing since the last look".to_string(),
                    (false, true) => format!("what it has printed:\n{printed}"),
                    (false, false) => format!("what it has printed since the last look:\n{printed}"),
                };
                format!("{} is running, for {} s: {}\n{said}", job.id, job.started.elapsed().as_secs(), job.command)
            }
        };
        let lines: Vec<String> = self.jobs.0.iter().filter(|job| wanted.is_none_or(|id| job.id == id)).map(report).collect();
        if lines.is_empty() {
            return Ok(wanted.map_or("no command has been started in the background".to_string(), |id| format!("there is no {id}")));
        }
        Ok(cap(&lines.join("\n\n"), MAX_OUTPUT))
    }

    fn job_stop(&self, arguments: &Value) -> Result<String, String> {
        let id = text_arg(arguments, "id")?;
        let job = self.jobs.0.iter().find(|job| job.id == id).ok_or_else(|| format!("there is no {id}"))?;
        job.cancel.store(true, Ordering::Relaxed);
        Ok(format!("{id} is being ended"))
    }

    /// A read has come back. The same lines of the same file as they were are already in the conversation, and are not sent again to be paid for twice.
    pub fn after_read(&mut self, arguments: &Value, path: PathBuf, hash: String, result: Result<String, String>) -> Result<String, String> {
        if self.seen.insert(path.clone(), hash.clone()).is_some_and(|before| before != hash) {
            self.covered.remove(&path);
        }
        if let Ok(text) = &result {
            // (the lines shown in a row from where what was seen ends: an outline's head counts, and its parts after a gap do not)
            let mut covered = self.covered.get(&path).copied().unwrap_or(0);
            for number in text.lines().filter_map(|line| line.split_once('|')?.0.parse::<usize>().ok()) {
                if number > covered + 1 {
                    break;
                }
                covered = covered.max(number);
            }
            self.covered.insert(path.clone(), covered);
        }
        let number = |name: &str, default: i64| arguments.get(name).and_then(Value::as_i64).unwrap_or(default);
        let key = (path, number("start_line", 1), number("limit", 500));
        let same = self.reads.get(&key) == Some(&hash);
        self.reads.insert(key, hash);
        if same && result.is_ok() {
            return Ok(UNCHANGED_READ.to_string());
        }
        // At each so much more in a turn: what it read stays in every later request, and a look further can go to a helper. (Once was
        // not enough: a real model told at 96 KB went on to 390.)
        let result = result.map(|text| {
            self.read_this_turn += text.len();
            if self.read_this_turn > READ_TOLD_PAST * (self.read_told + 1) {
                self.read_told = self.read_this_turn / READ_TOLD_PAST;
                format!("{text}--- this turn has read {} KB of files into the conversation, where it stays for every later request. To look further, give the question to explore: only its answer joins ---\n", self.read_this_turn / 1024)
            } else {
                text
            }
        });
        result
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

    /// Files went back: a patch must read them again first.
    pub fn forget_seen(&mut self) {
        self.seen.clear();
        self.reads.clear();
        self.covered.clear();
        self.cut_file = None;
    }

    /// This file's write was cut off and is to go on.
    pub fn note_cut(&mut self, path: PathBuf) {
        self.cut_file = Some(path);
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
        let written = if exists {
            let form = Form::of(&before);
            let new = form.bytes(content, !append).map_err(|why| format!("{given}: {why}. Nothing was written"))?;
            if append { [before, new].concat() } else { new }
        } else {
            content.as_bytes().to_vec()
        };
        written_whole(&path, &written, exists).map_err(|error| format!("{given}: {error}: {}", if exists { "the file is as it was" } else { "it was not made" }))?;
        let lines = file_text(&written).0.lines().count();
        self.seen.insert(path.clone(), sha16(&written));
        if self.cut_file.as_ref() == Some(&path) {
            self.cut_file = None;
        }
        self.covered.insert(path, seen_in_part.unwrap_or(lines));
        let word = if !exists { "created" } else if append { "added to" } else { "replaced" };
        let said = format!("{word} {given}: now {lines} lines, {} bytes", written.len());
        Ok(if self.in_japanese { said + &not_japanese_note(content) } else { said })
    }

    /// Every edit is validated against the text before anything is written; then the files change together, or, when a
    /// write fails, those already written go back.
    fn apply_patch(&mut self, arguments: &Value, workspace: &Path) -> Result<String, String> {
        let edits = arguments.get("edits").and_then(Value::as_array).filter(|edits| !edits.is_empty())
            .ok_or("edits is required: a list of {path, old_string, new_string}")?;
        let expected: Vec<&str> = arguments.get("expected_files").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        let mut files: Vec<(PathBuf, String, String)> = Vec::new();
        for (number, edit) in edits.iter().enumerate() {
            let given = edit.get("path").and_then(Value::as_str).or(if expected.len() == 1 { expected.first().copied() } else { None })
                .ok_or_else(|| format!("edit {}: path is required", number + 1))?;
            let path = resolve(workspace, given);
            if !expected.is_empty() && !expected.iter().any(|name| resolve(workspace, name) == path) {
                return Err(format!("{given} is not in expected_files: nothing was changed"));
            }
            let old = text_arg(edit, "old_string").map_err(|error| format!("edit {}: {error}", number + 1))?;
            let new = text_arg(edit, "new_string").map_err(|error| format!("edit {}: {error}", number + 1))?;
            let count = edit.get("count").and_then(Value::as_u64).unwrap_or(1) as usize;
            if old.is_empty() {
                return Err(format!("edit {}: old_string is empty", number + 1));
            }
            let slot = match files.iter().position(|file| file.0 == path) {
                Some(slot) => slot,
                None => {
                    if !path.exists() {
                        return Err(format!("{}: it does not exist: nothing was changed", path.display()));
                    }
                    // (first, as in `create_file`)
                    let Some(known) = self.seen.get(&path) else {
                        return Err(format!("{given} has not been read in this session: read it first; nothing was changed"));
                    };
                    let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}: nothing was changed", path.display()))?;
                    if *known != sha16(&bytes) {
                        return Err(format!("{given} changed since it was read: read it again; nothing was changed"));
                    }
                    let text = String::from_utf8(bytes).map_err(|_| format!("{given} is not UTF-8 text: nothing was changed"))?;
                    files.push((path, text.clone(), text));
                    files.len() - 1
                }
            };
            let text = &mut files[slot].2;
            // A file with CRLF lines takes an edit written with LF: as written first, and else with CRLF lines, whichever is there as often as
            // asked. (A file with both, as an editor that ended only its last line with CRLF leaves, took every edit as CRLF, and none of more
            // than one line of its LF part could be found: a real model gave up and wrote the whole file again.)
            let as_written = (old.to_string(), new.to_string());
            // (a new_string written with CRLF already keeps them, not one CR more)
            let with_crlf = (old.contains('\n') && !old.contains("\r\n") && text.contains("\r\n")).then(|| (old.replace('\n', "\r\n"), new.replace("\r\n", "\n").replace('\n', "\r\n")));
            let (old, new) = match with_crlf {
                Some(crlf) if text.matches(as_written.0.as_str()).count() != count => crlf,
                _ => as_written,
            };
            let found = text.matches(&old).count();
            if found != count {
                // What it took to find the place is what a retry needs: the same line, spaced differently, is the usual cause.
                let hint = if found == 0 { nearest_line(text, &old) } else { String::new() };
                // (edit 1 is the first: a model takes it either way, and goes looking in the wrong one)
                let start: String = old.trim().chars().take(40).collect();
                return Err(format!("edit {} of {} (`{start}`): old_string occurs {found} times in {given}, not {count}: nothing was changed. Read the exact text; do not include line numbers{hint}", number + 1, edits.len()));
            }
            let changed = replace_with_the_line_end_there(text, &old, &new);
            if changed.is_empty() && !edit.get("allow_empty_file").and_then(Value::as_bool).unwrap_or(false) {
                return Err(format!("edit {} would leave {given} empty: nothing was changed", number + 1));
            }
            *text = changed;
        }
        let mut report = String::new();
        write_all_or_none(&files)?;
        for (path, before, after) in &files {
            // (the model wrote this change itself: enough to see that it landed where it meant, and a local model pays for every token of it, again in each later request's prefix)
            let diff = similar::TextDiff::from_lines(before, after).unified_diff().context_radius(2).to_string();
            let name = path.strip_prefix(workspace).unwrap_or(path).display().to_string();
            report.push_str(&format!("--- applied_diff {name} ---\n{}\n", cap(&diff, MAX_APPLIED_DIFF)));
        }
        for (path, before, after) in files {
            // (seen whole before, it is seen whole after: the model wrote what changed)
            if self.covered.get(&path).is_some_and(|covered| *covered >= before.lines().count()) {
                self.covered.insert(path.clone(), after.lines().count());
            }
            // A write cut off and going on is not, once its file is patched: the next create_file of it is the model's own, whole or not as it says.
            if self.cut_file.as_ref() == Some(&path) {
                self.cut_file = None;
            }
            self.seen.insert(path, sha16(after.as_bytes()));
        }
        let mut said = cap(&report, MAX_OUTPUT);
        if self.in_japanese {
            for edit in edits {
                said.push_str(&not_japanese_note(edit.get("new_string").and_then(Value::as_str).unwrap_or("")));
            }
        }
        Ok(said)
    }

    fn todo(&mut self, arguments: &Value) -> Result<String, String> {
        let items = arguments.get("todos").and_then(Value::as_array).ok_or("todos is required")?;
        if arguments.get("merge").and_then(Value::as_bool) == Some(false) {
            self.todos.clear();
        }
        for item in items {
            let id = item.get("id").and_then(Value::as_str).ok_or("each todo needs an id")?;
            match self.todos.iter_mut().find(|known| known.get("id").and_then(Value::as_str) == Some(id)) {
                Some(known) => {
                    for (name, value) in item.as_object().into_iter().flatten() {
                        known[name] = value.clone();
                    }
                }
                None => {
                    let mut fresh = item.clone();
                    if fresh.get("status").is_none() {
                        fresh["status"] = json!(if self.todos.is_empty() { "doing" } else { "pending" });
                    }
                    self.todos.push(fresh);
                }
            }
        }
        Ok(self.todos.iter().map(|item| format!("[{}] {}: {}", item["status"].as_str().unwrap_or("pending"), item["id"].as_str().unwrap_or(""), item["text"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n"))
    }
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
        let mark: &'static [u8] = [&[0xEF, 0xBB, 0xBF][..], &[0xFF, 0xFE], &[0xFE, 0xFF]].into_iter().find(|mark| bytes.starts_with(mark)).unwrap_or(&[]);
        // (as most of its lines end: an editor may have ended only its last one otherwise)
        let ends = text.matches('\n').count();
        Form { encoding, mark, crlf: ends > 0 && text.matches("\r\n").count() * 2 > ends }
    }

    /// `text` as this file has it; with its mark when it is the whole of the file.
    fn bytes(&self, text: &str, whole: bool) -> Result<Vec<u8>, String> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let text = if self.crlf { text.replace("\r\n", "\n").replace('\n', "\r\n") } else { text.to_string() };
        let mut out = if whole { self.mark.to_vec() } else { Vec::new() };
        match self.encoding {
            "utf-8" => out.extend_from_slice(text.as_bytes()),
            "utf-16" if self.mark == [0xFE, 0xFF] => out.extend(text.encode_utf16().flat_map(u16::to_be_bytes)),
            "utf-16" => out.extend(text.encode_utf16().flat_map(u16::to_le_bytes)),
            "shift_jis" => out.extend(shift_jis(&text)?),
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
    let wide = |pair: fn([u8; 2]) -> u16| String::from_utf16_lossy(&bytes[2..].chunks_exact(2).map(|two| pair([two[0], two[1]])).collect::<Vec<u16>>());
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return (wide(u16::from_le_bytes), "utf-16");
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return (wide(u16::from_be_bytes), "utf-16");
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return (text.to_string(), "utf-8");
    }
    match crate::web::from_code_page(bytes, 932) {
        Some(text) => (text, "shift_jis"),
        None => (String::from_utf8_lossy(bytes).into_owned(), "unknown"),
    }
}

/// A file's bytes as text to be shown, as the agent reads files (`file_text`): UTF-16 with its mark, else UTF-8, else code page 932, with the name of what it was. None for a binary, which is
/// a NUL in the first 8 KB of what is not UTF-16.
pub fn text_of_file(bytes: &[u8]) -> Option<(String, &'static str)> {
    let utf16 = bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]);
    if !utf16 && bytes.iter().take(8192).any(|&byte| byte == 0) {
        return None;
    }
    Some(file_text(bytes))
}

fn read_file(arguments: &Value, workspace: &Path) -> Result<(String, (PathBuf, String)), String> {
    let path = resolve(workspace, text_arg(arguments, "path")?);
    if crate::images::is_picture(&path) {
        return Err(format!("{} is a picture, and it cannot be read here: this model cannot be shown one by a tool. Say so, and ask the user to attach it to a message.", path.display()));
    }
    // (read whole to number its lines: one larger than memory would end the app, with all that is going on in it)
    let size = std::fs::metadata(&path).map_err(|error| format!("{}: {error}", path.display()))?.len();
    if size > MOST_READ_WHOLE {
        return Err(format!("{} is too large to read here, at {} MB: use search_files for what is in it, or a command that prints a part of it", path.display(), size / 1_048_576));
    }
    let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let Some((text, encoding)) = text_of_file(&bytes) else {
        return Err(format!("{} is binary: unsupported binaries are refused", path.display()));
    };
    let hash = sha16(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    // A large file asked for with no place in it: where its parts begin, to read the one wanted, and not its first 500 lines, which stay
    // in the context for the rest of the conversation whether they were the part or not.
    let placed = arguments.get("start_line").is_some() || arguments.get("limit").is_some();
    if !placed && lines.len() > OUTLINE_PAST {
        let parts = outline(&lines);
        if !parts.is_empty() {
            return Ok((outlined(&lines, &parts, size), (path, hash)));
        }
    }
    let limit = arguments.get("limit").and_then(Value::as_i64).unwrap_or(500).clamp(1, 2000) as usize;
    let start = arguments.get("start_line").and_then(Value::as_i64).unwrap_or(1);
    let first = if start < 0 { lines.len().saturating_sub(start.unsigned_abs() as usize) } else { (start.max(1) as usize) - 1 };
    // (the guard on edits keeps its own hash: the model has no use for one)
    let mut out = if encoding == "utf-8" { String::new() } else { format!("--- encoding={encoding} ---\n") };
    if first >= lines.len() {
        // (nothing below this line could mean a file that ends here or a read that went past it)
        out.push_str(&if lines.is_empty() { "--- the file is empty ---\n".to_string() } else { format!("--- the file has only {} lines ---\n", lines.len()) });
    }
    let mut next = first;
    for (at, line) in lines.iter().enumerate().skip(first).take(limit) {
        let shown = if line.len() > MAX_LINE {
            let mut cut = MAX_LINE;
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}... ({} more bytes on this line)", &line[..cut], line.len() - cut)
        } else {
            (*line).to_string()
        };
        if out.len() + shown.len() > MAX_READ && next > first {
            break;
        }
        out.push_str(&format!("{}|{shown}\n", at + 1));
        next = at + 1;
    }
    if next > first && next < lines.len() {
        out.push_str(&format!("--- {} more lines; continue with start_line={} ---\n", lines.len() - next, next + 1));
    }
    Ok((out, (path, hash)))
}

/// Where a file's parts begin, by line: functions, types, classes, sections, in the languages a project is mostly written in. Empty
/// for a file of none of them, which is then read as any other.
fn outline(lines: &[&str]) -> Vec<(usize, usize)> {
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
    let mut parts: Vec<(usize, usize)> = lines
        .iter()
        .enumerate()
        .filter_map(|(at, line)| {
            let trimmed = line.trim_start();
            let indent = line.len() - trimmed.len();
            (indent <= 8 && starts.is_match(trimmed)).then_some((at, indent))
        })
        .collect();
    if parts.len() > OUTLINE_MOST {
        parts.retain(|(_, indent)| *indent == 0);
        parts.truncate(OUTLINE_MOST);
    }
    parts
}

/// A large file, as its head and its outline: each part's first line with its number, to read with `start_line` and `limit`.
fn outlined(lines: &[&str], parts: &[(usize, usize)], size: u64) -> String {
    let shown = |line: &str| -> String { if line.len() > 160 { format!("{}…", &line[..line.floor_char_boundary(160)]) } else { line.to_string() } };
    let mut out = format!("--- {} lines, {} KB: more than to read whole. Its first lines, then where its parts begin ---\n", lines.len(), size / 1024);
    for (at, line) in lines.iter().enumerate().take(OUTLINE_HEAD) {
        out.push_str(&format!("{}|{}\n", at + 1, shown(line)));
    }
    out.push_str("--- outline ---\n");
    for (at, _) in parts.iter().filter(|(at, _)| *at >= OUTLINE_HEAD) {
        out.push_str(&format!("{}|{}\n", at + 1, shown(lines[*at])));
    }
    out.push_str("--- read the part you need: read_file with start_line and limit, at a line above, or one search_files finds ---\n");
    out
}

/// What a walk through a project does not go into, as `.gitignore` does not always say so: outside a repository it is not read at all.
const HEAVY: [&str; 6] = ["node_modules", ".venv", "__pycache__", ".mypy_cache", ".pytest_cache", ".tox"];

/// `deep`: going down through folders. A folder named as the place to look is looked in, and one level shows what is there.
pub(crate) fn walker(root: &Path, deep: bool) -> ignore::WalkBuilder {
    let mut walk = ignore::WalkBuilder::new(root);
    walk.hidden(false).filter_entry(move |entry| {
        let name = entry.file_name();
        name != ".git" && !(deep && entry.depth() > 0 && HEAVY.iter().any(|heavy| name == *heavy))
    });
    walk
}

/// Where the first line of an edit that did not match is, if its words are there with other spacing.
fn nearest_line(text: &str, old: &str) -> String {
    let squeeze = |line: &str| line.split_whitespace().collect::<Vec<_>>().join(" ");
    let Some(first) = old.lines().map(squeeze).find(|line| !line.is_empty()) else { return String::new() };
    let places: Vec<String> = text.lines().enumerate().filter(|(_, line)| squeeze(line) == first).map(|(at, _)| (at + 1).to_string()).take(5).collect();
    if !places.is_empty() {
        return format!(". Its first line is at line {}, spaced differently or followed by other lines: read there", places.join(", "));
    }
    // One character different, as a Chinese form for a Japanese one, is what it cannot see in what it read: the line itself, to copy from, with no read for it.
    let wanted: Vec<char> = first.chars().collect();
    let shared = |line: &str| line.chars().filter(|letter| !letter.is_whitespace() && wanted.contains(letter)).count();
    let best = text.lines().enumerate().map(|(at, line)| (shared(line), at, line)).max_by_key(|(count, at, _)| (*count, std::cmp::Reverse(*at)));
    match best {
        Some((count, at, line)) if count * 10 >= wanted.iter().filter(|letter| !letter.is_whitespace()).count() * 6 => format!(". The nearest is line {}, character for character: {}", at + 1, line.trim().chars().take(160).collect::<String>()),
        _ => String::new(),
    }
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

fn find_files(arguments: &Value, workspace: &Path) -> Result<String, String> {
    let pattern = text_arg(arguments, "pattern")?;
    let root = resolve(workspace, arguments.get("path").and_then(Value::as_str).unwrap_or("."));
    if !root.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    let mut globs = ignore::overrides::OverrideBuilder::new(&root);
    globs.add(pattern).map_err(|error| format!("{pattern}: {error}"))?;
    // Matched after the walk: given to the walk as an override, a glob would bring back what .gitignore leaves out.
    let globs = globs.build().map_err(|error| format!("{pattern}: {error}"))?;
    let mut names: Vec<String> = walker(&root, true).build().flatten()
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter(|entry| globs.matched(entry.path(), false).is_whitelist())
        .map(|entry| entry.path().strip_prefix(&root).unwrap_or(entry.path()).to_string_lossy().replace('\\', "/"))
        .take(LIST_LIMIT + 1).collect();
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

fn list_files(arguments: &Value, workspace: &Path) -> Result<String, String> {
    let root = resolve(workspace, arguments.get("path").and_then(Value::as_str).unwrap_or("."));
    if !root.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    let recursive = arguments.get("recursive").and_then(Value::as_bool).unwrap_or(false);
    let mut walk = walker(&root, recursive);
    if !recursive {
        walk.max_depth(Some(1));
    }
    let mut names = Vec::new();
    let (mut files, mut bytes) = (0usize, 0u64);
    for entry in walk.build().flatten() {
        if entry.depth() == 0 {
            continue;
        }
        let name = entry.path().strip_prefix(&root).unwrap_or(entry.path()).to_string_lossy().replace('\\', "/");
        let folder = entry.file_type().is_some_and(|kind| kind.is_dir());
        let size = entry.metadata().map_or(0, |data| data.len());
        files += 1;
        bytes += size;
        if names.len() < LIST_LIMIT {
            // (a large one says so here: a model about to read a log first asks how long it is, in a round of its own)
            names.push(match (folder, size >= BIG_FILE) {
                (true, _) => format!("{name}/"),
                (false, true) if size < 1 << 20 => format!("{name} ({} KB)", size / 1024),
                (false, true) => format!("{name} ({:.1} MB)", size as f64 / 1_048_576.0),
                (false, false) => name,
            });
        }
        if files >= 50_000 {
            break;
        }
    }
    names.sort();
    // (no words at all would read as a tool that did not answer)
    if names.is_empty() {
        return Ok(format!("{} is empty", root.display()));
    }
    let mut out = names.join("\n");
    if files > LIST_LIMIT {
        out.push_str(&format!("\n--- truncated: showing first {LIST_LIMIT} of {files} files ({bytes} bytes); use search_files for targeted lookup ---"));
    }
    Ok(out)
}

fn search_files(arguments: &Value, workspace: &Path, secrets_besides: &[PathBuf], cancel: &AtomicBool) -> Result<String, String> {
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
    let deadline = Instant::now() + Duration::from_secs(arguments.get("timeout_seconds").and_then(Value::as_u64).unwrap_or(30).clamp(1, 300));
    let mut walk = walker(&root, true);
    if let Some(glob) = arguments.get("file_glob").and_then(Value::as_str) {
        let mut overrides = ignore::overrides::OverrideBuilder::new(&root);
        overrides.add(glob).map_err(|error| format!("file_glob: {error}"))?;
        walk.overrides(overrides.build().map_err(|error| format!("file_glob: {error}"))?);
    }
    let mut out = note.to_string();
    let (mut matches, mut stopped, mut secrets) = (0usize, "", 0usize);
    for entry in walk.build().flatten() {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        if Instant::now() > deadline {
            stopped = "the time ran out";
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) || entry.metadata().map_or(0, |data| data.len()) > SEARCH_FILE_LIMIT {
            continue;
        }
        // `read_file` asks a person before it opens one of these, and a pattern of `.` would otherwise print it without asking. Searching a place that is itself one has been asked about.
        // The same goes for what else holds keys here, of which a wider folder holds some: `%LOCALAPPDATA%` holds this app's sessions.
        let among_the_others = secrets_besides.iter().any(|secret| crate::safety::inside(secret, entry.path()) && !crate::safety::inside(secret, &root));
        if (crate::safety::sensitive(entry.path()) && !crate::safety::sensitive(&root)) || among_the_others {
            secrets += 1;
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else { continue };
        if bytes.iter().take(8192).any(|&byte| byte == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let name = entry.path().strip_prefix(workspace).unwrap_or(entry.path()).to_string_lossy().replace('\\', "/");
        for (number, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                let shown: String = line.chars().take(300).collect();
                out.push_str(&format!("{name}:{}:{shown}\n", number + 1));
                matches += 1;
                if matches >= MATCH_LIMIT {
                    stopped = "the match limit was reached";
                    break;
                }
            }
        }
        if matches >= MATCH_LIMIT {
            break;
        }
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

/// What git says differs from the last commit under the workspace, new files included. An error: this is no repository.
pub fn workspace_changes(workspace: &Path) -> Result<Vec<FileChange>, String> {
    git(workspace, &["rev-parse", "--is-inside-work-tree"])?;
    let has_commit = git(workspace, &["rev-parse", "--verify", "-q", "HEAD"]).is_ok();
    let mut args = vec!["-c", "core.quotepath=off", "diff", "--no-ext-diff", "--no-color", "--relative"];
    if has_commit {
        args.push("HEAD");
    }
    let text = git(workspace, &args)?;
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
        return Ok(if changes.is_empty() { format!("no differences ({scope}): nothing has changed since this session began") } else { cap(&changes, MAX_OUTPUT) });
    }
    // (only what is under the workspace: a workspace that is a folder of a larger repository saw that repository's every change, which is
    // not its work and may be what its owner never meant to show)
    let mut args = vec!["diff", "--no-ext-diff", "--no-color", "--relative"];
    match scope {
        "staged" => args.push("--cached"),
        "unstaged" => {}
        // A repository with no commit yet has no HEAD to compare with.
        _ if git(workspace, &["rev-parse", "--verify", "-q", "HEAD"]).is_ok() => args.push("HEAD"),
        _ => {}
    }
    if !paths.is_empty() {
        args.push("--");
        args.extend(paths.iter().copied());
    }
    let mut diff = git(workspace, &args).map_err(|error| error.lines().next().unwrap_or("git could not show this").to_string())?;
    // (a new file is a change to a workspace, whatever git diff leaves out: creating only new files must not read as nothing done)
    if scope == "workspace" {
        for change in untracked_changes(workspace, &paths) {
            diff.push_str(&format!("--- new file {} ---\n{}\n", change.path, change.diff));
        }
    }
    Ok(if diff.is_empty() { format!("no differences ({scope})") } else { cap(&diff, MAX_OUTPUT) })
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

/// What `dir` is asked for, in what runs in the sandbox: it lists a folder, a file or a pattern as `dir` does, in fewer words. `piped`: cmd reads what comes before a `|` again, and an `if` with parentheses does not survive that.
#[cfg(windows)]
fn dir_stand_in(words: &[String], piped: bool) -> String {
    let recursive = words.iter().any(|word| word.eq_ignore_ascii_case("/s"));
    let mut places: Vec<String> = words.iter().filter(|word| !word.starts_with('/')).map(|word| word.trim_end_matches('\\').to_string()).collect();
    if places.is_empty() {
        places.push(".".to_string());
    }
    let one = |place: &String| {
        if recursive {
            let (folder, mask) = if place.contains(['*', '?']) { place.rsplit_once('\\').unwrap_or((".", place.as_str())) } else { (place.as_str(), "*") };
            return format!("for /r \"{folder}\" %d in ({mask}) do @echo %d");
        }
        if piped {
            return format!("(for /d %d in (\"{place}\\*\") do @echo %~nxd\\) & (for %d in (\"{place}\\*\") do @echo %~nxd  %~zd)");
        }
        format!("if exist \"{place}\\*\" ((for /d %d in (\"{place}\\*\") do @echo %~nxd\\) & (for %d in (\"{place}\\*\") do @echo %~nxd  %~zd)) else if exist \"{place}\" (for %d in (\"{place}\") do @echo %~nxd  %~zd) else (echo File Not Found 1>&2)")
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
    let delete = |file: &str| format!("powershell -NoProfile -NonInteractive -Command \"[IO.File]::Delete('{file}')\"");
    Some(files.iter().map(|file| if file.contains(['*', '?']) { format!("for %d in (\"{file}\") do @{}", delete("%d")) } else { delete(file) }).collect::<Vec<_>>().join(" & "))
}

/// One step of a line, with `dir` or `del` in the forms that say plainly what they want replaced by what does that.
#[cfg(windows)]
fn step_stand_in(step: &str, piped: bool) -> Option<String> {
    let bare = step.trim_start().trim_start_matches('@');
    let word = bare.split_whitespace().next().unwrap_or("");
    let (name, glued) = word.split_once('/').map_or((word, None), |(name, rest)| (name, Some(rest)));
    let name = name.to_ascii_lowercase();
    if !matches!(name.as_str(), "dir" | "del" | "erase") {
        return None;
    }
    let mut words = plain_words(&bare[word.len()..])?;
    words.extend(glued.into_iter().flat_map(|rest| rest.split('/')).map(|switch| format!("/{switch}")));
    let text = if name == "dir" { dir_stand_in(&words, piped) } else { del_stand_in(&words)? };
    Some(format!("{}({text})", &step[..step.len() - step.trim_start().len()]))
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
    if status == Some(0) {
        return notes;
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
    if lower.contains("git") && err.contains("failed to stat") {
        notes.push_str("note: git cannot look above the workspace from the sandbox, so this says only that the workspace is not a git repository. `git init` at the start of a line works.\n");
    }
    let denied = says_access_denied(err, system_access_denied());
    let lower_err = err.to_ascii_lowercase();
    let in_git = lower.contains(".git\\config") || lower.contains(".git/config") || lower.contains(".git\\hooks") || lower.contains(".git/hooks");
    if lower_err.contains("config file") || (denied && in_git) || (in_git && lower_err.contains("permission denied")) {
        notes.push_str("note: the sandbox refuses changes to `.git\\config` and `.git\\hooks`, as what is written there would run outside it the next time git is used. Tell the user the exact command, for them to run.\n");
    }
    let root = workspace.to_string_lossy().to_lowercase();
    let outside = line.split(|c: char| c.is_whitespace() || c == '"').any(|word| word.as_bytes().get(1) == Some(&b':') && !word.to_lowercase().starts_with(&root));
    if denied && outside {
        notes.push_str("note: a command cannot read outside the workspace and the folders the user has named under Settings > Permissions and safety, by any route. `read_file`, `list_files` and `search_files` can look there; for a command to read it, ask the user to name its folder there.\n");
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
    let runs = line_that_runs(line);
    // Spelled as `cwd` is, or a path with `..` in it would not be seen as holding it.
    let done = crate::sandbox::run_live(&runs, &cwd, &resolve(workspace, "."), network, timeout, cancel, live).map_err(|error| {
        // (the app's `git init` has run by then, and what is said is what is there)
        let started = if started_with_init.is_some() { "`git init` ran, and made .git there; the rest of the line was not started" } else { "The command was not started" };
        format!("{error}. {started}; changing its spelling cannot repair this")
    })?;
    let (out, err) = (crate::sandbox::decode(&done.stdout), crate::sandbox::decode(&done.stderr));
    let mut report = String::new();
    // (said, and not done quietly: the model learns what its words became, and the person reads it in the result)
    if runs != line {
        report.push_str(&format!("note: this ran as `{runs}`: in the sandbox `dir`, `del` and `| head` are replaced by what does the same there.\n"));
    }
    if !done.shown.is_empty() {
        report.push_str(&format!("note: it opened a window that nobody can see, as nothing from a command reaches the screen: {}. Run what prints instead of what shows.\n", done.shown.iter().map(|title| if title.is_empty() { "a dialog" } else { title.as_str() }).take(4).collect::<Vec<_>>().join(", ")));
    }
    let no_network = no_network_note(network, done.status, &out, &err);
    report.push_str(no_network.unwrap_or(""));
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
    if needs_drive_root(line) && says_access_denied(&err, system_access_denied()) {
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
    report.push_str(&sandbox_notes(line, &resolve(workspace, "."), done.status, &err));
    let most_err = if no_network.is_some() { MAX_COMMAND_ERR / 6 } else { MAX_COMMAND_ERR };
    let whole = (out.len() > MAX_COMMAND_OUT || err.len() > most_err).then(|| kept_whole(&out, &err)).flatten();
    report.push_str(&format!("stdout:\n{said_by_init}{}\nstderr:\n{}\n", cap_evenly(&out, MAX_COMMAND_OUT, whole.as_deref()), cap_evenly(&err, most_err, whole.as_deref())));
    match done.status {
        Some(code) => report.push_str(&format!("exit_code: {code}")),
        None => report.push_str(&format!("exit_code: none, {} after {} s", done.ended, started.elapsed().as_secs())),
    }
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
    report.push_str(&format!("stdout:\n{}\nstderr:\n{}\n", cap(&out, MAX_OUTPUT), cap(&err, MAX_OUTPUT / 2)));
    match status {
        Some(status) => report.push_str(&format!("exit_code: {}", status.code().unwrap_or(-1))),
        None => report.push_str(&format!("exit_code: none, {ended} after {} s", started.elapsed().as_secs())),
    }
    Ok(report)
}

/// One line saying what a call is about, for the transcript and the approval note.
pub fn summary(tool: &str, arguments: &Value) -> String {
    let text = |name: &str| arguments.get(name).and_then(Value::as_str).unwrap_or("");
    match tool {
        "read_file" | "list_files" | "create_file" | "check_page" => text("path").to_string(),
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
        "web_read" => text("url").to_string(),
        "find_files" => text("pattern").to_string(),
        "skill" => text("name").to_string(),
        "remember" => text("note").chars().take(80).collect(),
        "forget" => text("words").to_string(),
        "command_status" | "command_stop" => text("id").to_string(),
        "explore" => text("task").lines().next().unwrap_or("").chars().take(100).collect(),
        _ if tool.starts_with("mcp__") => arguments.to_string().chars().take(120).collect(),
        "web_search" => arguments.get("queries").and_then(Value::as_array).map_or(String::new(), |queries| queries.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")),
        _ => String::new(),
    }
}

#[cfg(test)]
mod power_tests {
    use super::*;

    #[test]
    fn files_are_found_by_name_and_what_git_ignores_is_not() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "skip.rs\n").unwrap();
        for name in ["src/a.rs", "src/deep/b.rs", "src/c.txt", "skip.rs"] {
            std::fs::write(dir.path().join(name), "x").unwrap();
        }
        let found = find_files(&json!({"pattern": "**/*.rs"}), dir.path()).unwrap();
        assert_eq!(found, "src/a.rs\nsrc/deep/b.rs");
        assert!(find_files(&json!({"pattern": "*.zig"}), dir.path()).unwrap().starts_with("no file"));
        assert!(find_files(&json!({}), dir.path()).is_err());
    }

    #[test]
    fn an_edit_that_misses_says_where_its_line_is() {
        assert!(nearest_line("a\n  fn  main( ) {\nb", "fn main( ) {\n  x").contains("line 2"));
        assert_eq!(nearest_line("a\nb", "nowhere"), "");
    }

    #[test]
    fn the_tools_of_a_session_depend_on_what_is_on() {
        let names = |value: Value| value.as_array().unwrap().iter().map(|tool| tool["function"]["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let plain = names(schemas_for(false, false, false, Vec::new()));
        assert!(plain.contains(&"find_files".to_string()) && !plain.contains(&"skill".to_string()) && !plain.contains(&"web_read".to_string()) && !plain.contains(&"remember".to_string()));
        let all = names(schemas_for(true, true, true, vec![json!({"type": "function", "function": {"name": "mcp__s__t"}})]));
        assert!(all.contains(&"mcp__s__t".to_string()));
        for tool in ["skill", "web_read", "remember", "forget", "command_status"] {
            assert!(all.contains(&tool.to_string()), "{tool}");
        }
        let schemas = schemas_for(false, false, false, Vec::new());
        let command = schemas.as_array().unwrap().iter().find(|tool| tool["function"]["name"] == "execute_command").unwrap();
        assert!(command["function"]["parameters"]["properties"].get("background").is_some());
    }

    /// A background job's output is reported once in a report of all of them, and after that in a line; asked by its id, again in full.
    /// A background job runs without the timeout the model gave, which is how long a call waits.
    #[test]
    fn an_ended_jobs_output_is_reported_once_among_all_and_its_timeout_is_not_its_life() {
        let mut bench = Workbench::default();
        let done: CommandResult = Arc::new(Mutex::new(Some(Ok("BUILD OUTPUT".to_string()))));
        bench.add_job(&json!({"command": "cargo build"}), Arc::new(AtomicBool::new(false)), done, Arc::default()).unwrap();
        let first = bench.job_status(&json!({})).unwrap();
        let second = bench.job_status(&json!({})).unwrap();
        let by_id = bench.job_status(&json!({"id": "job1"})).unwrap();
        assert!(first.contains("BUILD OUTPUT") && !second.contains("BUILD OUTPUT") && second.contains("as reported before") && by_id.contains("BUILD OUTPUT"), "{first}\n{second}\n{by_id}");
        let given = background_arguments(&json!({"command": "cargo build", "background": true, "timeout_seconds": 60}));
        assert!(given.get("timeout_seconds").is_none() && given["command"] == "cargo build");
    }

    /// A job still running shows what it has printed, and the next look only what came after; a look past what one shows says how much it left out.
    #[test]
    fn a_running_job_shows_what_it_printed_since_the_last_look() {
        let mut bench = Workbench::default();
        let live: Arc<Live> = Arc::default();
        bench.add_job(&json!({"command": "npm run dev"}), Arc::new(AtomicBool::new(false)), Arc::default(), live.clone()).unwrap();
        assert!(bench.job_status(&json!({})).unwrap().ends_with("it has printed nothing yet"));
        live.push("compiling\nlistening on http://localhost:5173\n".as_bytes());
        let first = bench.job_status(&json!({"id": "job1"})).unwrap();
        assert!(first.contains("is running") && first.contains("listening on http://localhost:5173"), "{first}");
        assert!(bench.job_status(&json!({})).unwrap().ends_with("it has printed nothing since the last look"));
        live.push("GET / 200\n".as_bytes());
        let next = bench.job_status(&json!({})).unwrap();
        assert!(next.contains("since the last look:\nGET / 200") && !next.contains("listening"), "{next}");
        // (a look starting in the middle of a character starts after it)
        live.push("あ".as_bytes());
        live.push(&[b'x'; LIVE_SHOWN]);
        let (shown, _) = live.since(0, LIVE_SHOWN + 1);
        assert!(shown.starts_with('(') && !shown.contains('\u{FFFD}') && !shown.contains('あ') && shown.ends_with('x'), "{shown}");
    }

    /// What a real command in the sandbox prints reaches a look while it still runs: a server's ready line before it ends, which it never does.
    #[cfg(windows)]
    #[test]
    fn what_a_command_in_the_sandbox_prints_is_seen_while_it_runs() {
        let dir = tempfile::tempdir().unwrap();
        let (mut bench, cancel) = (Workbench::default(), AtomicBool::new(false));
        // (spelled with a caret, so that the line itself does not say what it prints)
        let line = "echo ready^-now& powershell -NoProfile -Command \"Start-Sleep -Seconds 30\"";
        bench.run("execute_command", &json!({"command": line, "background": true}), dir.path(), &cancel).unwrap();
        let started = Instant::now();
        let seen = loop {
            let said = bench.job_status(&json!({"id": "job1"})).unwrap();
            if said.contains("ready-now") || started.elapsed() > Duration::from_secs(25) {
                break said;
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        bench.job_stop(&json!({"id": "job1"})).unwrap();
        assert!(seen.contains("is running") && seen.contains("printed:\nready-now"), "{seen}");
    }

    #[test]
    fn a_session_with_no_background_command_says_so() {
        let bench = Workbench::default();
        assert!(bench.job_status(&json!({})).unwrap().starts_with("no command"));
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
        let dir = tempfile::tempdir().unwrap();
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
        let dir = tempfile::tempdir().unwrap();
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
        let dir = tempfile::tempdir().unwrap();
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
        let dir = tempfile::tempdir().unwrap();
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
        let dir = tempfile::tempdir().unwrap();
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
    fn a_line_that_would_open_a_dialog_is_refused_before_any_sandbox_is_made() {
        let dir = tempfile::tempdir().unwrap();
        let said = execute_command(&json!({"command": "x.py"}), dir.path(), &AtomicBool::new(false), None).unwrap_err();
        assert!(said.contains("Nothing was started") && said.contains("python x.py"), "{said}");
    }

    #[cfg(windows)]
    #[test]
    fn a_refused_dir_does_not_read_as_the_whole_line_failing() {
        assert!(DRIVE_ROOT_NOTE.contains("what ran before it did run"));
    }

    #[test]
    fn a_session_is_told_of_the_network_as_a_thing_to_ask_for() {
        let schemas = schemas_for(false, false, false, Vec::new());
        let command = schemas.as_array().unwrap().iter().find(|tool| tool["function"]["name"] == "execute_command").unwrap();
        assert!(command["function"]["parameters"]["properties"].get("network").is_some());
        assert!(command["function"]["parameters"]["properties"]["network"]["description"].as_str().unwrap().contains("Asks the user"));
    }

    #[test]
    fn an_empty_folder_is_said_to_be() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_files(&json!({}), dir.path()).unwrap().ends_with("is empty"));
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
        let dir = tempfile::tempdir().unwrap();
        assert!(workspace_changes(dir.path()).is_err());
        assert_eq!(git_info(dir.path()), None);
    }

    #[test]
    fn a_repository_says_where_it_stands() {
        let dir = tempfile::tempdir().unwrap();
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
        (tempfile::tempdir().unwrap(), Workbench::default(), AtomicBool::new(false))
    }

    #[test]
    fn every_generated_schema_has_a_tool() {
        let schemas = schemas();
        let names: Vec<&str> = schemas.as_array().unwrap().iter().map(|tool| tool["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["apply_patch", "create_file", "execute_command", "list_files", "read_file", "search_files", "show_diff", "todo", "find_files", "check_page", "command_status", "command_stop", "explore"]);
    }

    #[test]
    fn show_diff_outside_git_answers_in_a_line_and_never_with_gits_usage() {
        let dir = tempfile::tempdir().unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let mut all = vec!["-c", "user.name=t", "-c", "user.email=t@example.com"];
            all.extend(args);
            git(dir.path(), &all).unwrap();
        };
        run(&["init", "-q"]);
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "first"]);
        let sessions = tempfile::tempdir().unwrap();
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
        let dir = tempfile::tempdir().unwrap();
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
        std::fs::write(dir.path().join("node_modules/x/a.js"), "needle").unwrap();
        std::fs::write(dir.path().join("b.js"), "needle").unwrap();
        let deep = bench.run("list_files", &json!({"recursive": true}), dir.path(), &cancel).unwrap();
        assert!(!deep.contains("node_modules"), "{deep}");
        let found = bench.run("search_files", &json!({"pattern": "needle"}), dir.path(), &cancel).unwrap();
        assert!(found.contains("b.js") && !found.contains("a.js"), "{found}");
        assert!(bench.run("list_files", &json!({}), dir.path(), &cancel).unwrap().contains("node_modules/"), "one level shows it");
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

    #[test]
    fn a_patch_needs_a_read_of_what_is_there_and_is_all_or_nothing() {
        let (dir, mut bench, cancel) = bench();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "alpha\nbeta\n").unwrap();
        let edit = json!({"edits": [{"path": "a.txt", "old_string": "alpha", "new_string": "ALPHA"}], "expected_files": ["a.txt"]});
        assert!(bench.run("apply_patch", &edit, dir.path(), &cancel).unwrap_err().contains("not been read"));
        bench.run("read_file", &json!({"path": "a.txt"}), dir.path(), &cancel).unwrap();
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
        assert!(bench.run("apply_patch", &edit, dir.path(), &cancel).unwrap_err().contains("changed since"));
        bench.run("read_file", &json!({"path": "a.txt"}), dir.path(), &cancel).unwrap();
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

    /// What is written to a file is written in its form: UTF-16 with its mark, appended to as UTF-16; CRLF lines; a UTF-8 mark kept;
    /// Shift_JIS, or nothing written when a character has no form there.
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

    /// A file shown only in part is not written whole over, as what was not shown would go; read on to its end, it is. Patched, a
    /// write that was cut off no longer goes on from it.
    #[test]
    fn a_file_seen_in_part_is_not_written_whole_over() {
        let (dir, mut bench, cancel) = bench();
        std::fs::write(dir.path().join("long.txt"), (1..=600).map(|at| format!("line {at}\n")).collect::<String>()).unwrap();
        bench.run("read_file", &json!({"path": "long.txt"}), dir.path(), &cancel).unwrap();
        let write = json!({"path": "long.txt", "content": "short\n"});
        let said = bench.run("create_file", &write, dir.path(), &cancel).unwrap_err();
        assert!(said.contains("600 lines") && said.contains("shown 500") && said.contains("start_line=501"), "{said}");
        bench.run("read_file", &json!({"path": "long.txt", "start_line": 501}), dir.path(), &cancel).unwrap();
        bench.run("create_file", &write, dir.path(), &cancel).unwrap();
        // (an outline shows its head, and not its parts after a gap)
        let source: String = (0..60).map(|at| format!("fn part_{at}() {{\n{}}}\n", "    x();\n".repeat(14))).collect();
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
    fn a_patch_written_with_lf_fits_a_crlf_file_and_counts_its_matches() {
        let (dir, mut bench, cancel) = bench();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x\r\ny\r\nx\r\ny\r\n").unwrap();
        bench.run("read_file", &json!({"path": "a.txt"}), dir.path(), &cancel).unwrap();
        let once = json!({"edits": [{"path": "a.txt", "old_string": "x\ny", "new_string": "z\nw"}], "expected_files": ["a.txt"]});
        assert!(bench.run("apply_patch", &once, dir.path(), &cancel).unwrap_err().contains("occurs 2 times"));
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

    /// A long file read with no place in it comes as its head and where its parts begin; asked for a place, it comes as lines, as before;
    /// a long file of no language the outline knows is read as lines too.
    #[test]
    fn a_long_file_read_whole_comes_as_its_outline() {
        let (dir, mut bench, cancel) = bench();
        let mut source = String::from("//! What this module is.\nuse std::path::Path;\n");
        for at in 0..60 {
            source.push_str(&format!("\n/// Part {at}.\npub fn part_{at}(x: u32) -> u32 {{\n"));
            for line in 0..14 {
                source.push_str(&format!("    let v{line} = x + {at};\n"));
            }
            source.push_str("    x\n}\n");
        }
        std::fs::write(dir.path().join("big.rs"), &source).unwrap();
        let out = bench.run("read_file", &json!({"path": "big.rs"}), dir.path(), &cancel).unwrap();
        assert!(out.starts_with("--- ") && out.contains("--- outline ---") && out.contains("|pub fn part_59(x: u32) -> u32 {") && out.contains("start_line and limit"), "{}", &out[..out.len().min(600)]);
        assert!(!out.contains("let v7 = x + 59;"), "the bodies are not in it");
        assert!(out.len() < 8000, "{}", out.len());
        // A place asked for is read as it was.
        let part = bench.run("read_file", &json!({"path": "big.rs", "start_line": 100, "limit": 5}), dir.path(), &cancel).unwrap();
        assert!(part.starts_with("100|") && !part.contains("outline"), "{part}");
        // Lines of no language: read as lines.
        std::fs::write(dir.path().join("big.log"), (0..1000).map(|at| format!("entry {at}\n")).collect::<String>()).unwrap();
        let log = bench.run("read_file", &json!({"path": "big.log"}), dir.path(), &cancel).unwrap();
        assert!(log.starts_with("1|entry 0") && log.contains("continue with start_line="), "{}", &log[..200]);
    }

    /// A line whose rest would be refused is refused before the app's `git init`: it says nothing was started, and nothing was.
    #[cfg(windows)]
    #[test]
    fn a_refused_line_makes_no_repository_first() {
        let dir = tempfile::tempdir().unwrap();
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
        let dir = tempfile::tempdir().unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let large = dir.path().join("model.gguf");
        std::fs::File::create(&large).unwrap().set_len(MOST_READ_WHOLE + 1).unwrap();
        let said = read_file(&json!({"path": "model.gguf"}), dir.path()).err().unwrap();
        assert!(said.contains("too large"), "{said}");
    }

    #[test]
    fn a_search_leaves_out_what_else_holds_keys_as_it_does_what_reads_as_keys() {
        let dir = tempfile::tempdir().unwrap();
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
        let dir = tempfile::tempdir().unwrap();
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
        let dir = tempfile::tempdir().unwrap();
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
        let dir = tempfile::tempdir().unwrap();
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
        for same in ["echo dir", "mkdir a", "python x.py", "dir > out.txt", "dir %TEMP%", "if exist x (del x)", "for %f in (*) do del %f", "del /s a.txt", "del /p a.txt", "del", "echo a & echo dir", "findstr \"dir | del\" a.txt", "dir\necho x"] {
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
        assert_eq!(del, "(powershell -NoProfile -NonInteractive -Command \"[IO.File]::Delete('a.txt')\" & powershell -NoProfile -NonInteractive -Command \"[IO.File]::Delete('b c.txt')\")");
        assert!(with_stand_ins("erase *.log").contains("for %d in (\"*.log\") do @powershell"));
        let piped = with_stand_ins("dir /b vendor | findstr three");
        assert!(piped.starts_with("((for /d %d in (\"vendor\\*\")") && !piped.contains("if exist") && piped.ends_with("| findstr three"), "{piped}");
        assert!(with_stand_ins("dir vendor || echo none").contains("if exist"), "a second chance is not a pipe");
        assert_eq!(with_stand_ins("del it's.txt"), "del it's.txt");
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
        assert_eq!(line_that_runs("del a.txt"), "(powershell -NoProfile -NonInteractive -Command \"[IO.File]::Delete('a.txt')\")");
        assert_eq!(line_that_runs("x | head -60"), "x | findstr \"^\"");
        assert_eq!(line_that_runs("dir /s *.js"), with_stand_ins("dir /s *.js"));
        // Lines are joined before the stand-ins go in, so that a `del` on the second one is replaced too.
        let two = line_that_runs("echo a\ndel a.txt");
        assert_eq!(two, "echo a & (powershell -NoProfile -NonInteractive -Command \"[IO.File]::Delete('a.txt')\")", "{two}");
        // A line that is refused is not run, so there is nothing else for it to be.
        assert_eq!(line_that_runs("echo \"a\nb\"\ndel x"), "echo \"a\nb\"\ndel x");
    }

    #[cfg(windows)]
    #[test]
    fn a_sandboxs_answer_that_reads_as_something_else_is_explained() {
        let root = Path::new("C:\\work");
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
        assert!(sandbox_notes("type C:\\elsewhere\\a.txt", root, Some(1), "Access is denied.").contains("cannot read outside the workspace"));
        assert!(sandbox_notes("type C:\\elsewhere\\a.txt", root, Some(1), "アクセスが拒否されました。").contains("`read_file`"));
        // On another drive, and in another language, the same.
        assert!(sandbox_notes("powershell -c \"Get-Item a\"", Path::new("E:\\work"), Some(1), "Get-Item : Der Pfad \"D:\\a\" kann nicht gefunden werden.\n    + CategoryInfo : ObjectNotFound: (D:\\a:String) [Get-Item], ItemNotFoundException\n    + FullyQualifiedErrorId : PathNotFound,Microsoft.PowerShell.Commands.GetItemCommand").contains("absolute paths"));
        assert!(sandbox_notes("powershell -c \"Get-ChildItem\"", Path::new("E:\\work"), Some(1), "Get-ChildItem : Der Zugriff wurde verweigert\n  + CategoryInfo : PermissionDenied: (D:\\:String) [Get-ChildItem], UnauthorizedAccessException").contains("absolute paths"));
        assert!(!sandbox_notes("powershell -c \"Get-Item E:\\work\\a\"", Path::new("E:\\work"), Some(1), "    + CategoryInfo : ObjectNotFound: (E:\\work\\a:String) [Get-Item], ItemNotFoundException").contains("absolute paths"), "the workspace's own file is only missing");
        assert!(sandbox_notes("where python", root, Some(1), "INFORMATION: Es wurden keine Dateien mit dem angegebenen Muster gefunden.").contains("Run the program itself"));
        assert!(!sandbox_notes("where /?", root, Some(2), "FEHLER").contains("Run the program itself"), "something else went wrong");
        for (err, by) in [("PermissionError: [WinError 5] Zugriff verweigert: 'x'", "Python"), ("PermissionError: [Errno 13] Permission denied: 'x'", "Python"), ("Error: EPERM: operation not permitted, open 'x'", "Node"), ("Error: EACCES: permission denied, open 'x'", "Node"), ("Zugriff verweigert (os error 5)", "Rust"), ("Ausnahme von HRESULT: 0x80070005", "COM"), ("+ FullyQualifiedErrorId : UnauthorizedAccess, System.UnauthorizedAccessException", "PowerShell")] {
            assert!(says_access_denied(err, ""), "{by}: {err}");
            assert!(sandbox_notes("type D:\\elsewhere\\a.txt", root, Some(1), err).contains("cannot read outside the workspace"), "{by}");
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
    fn a_command_reports_its_output_and_exit_code() {
        // Under the target folder: on Windows this writes real ACEs, and they should not go on the temp folder.
        let dir = tempfile::tempdir_in(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target")).unwrap();
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
        let dir = tempfile::tempdir().unwrap();
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
}
