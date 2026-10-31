//! The tools, each behaving as its Dogmind namesake says it does in the schema the model reads.

use std::collections::HashMap;
#[cfg(not(windows))]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regex::RegexBuilder;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::prompt::TOOLS_JSON;
use crate::safety::resolve;

const MAX_OUTPUT: usize = 16384;
const LIST_LIMIT: usize = 500;
const MATCH_LIMIT: usize = 1000;
const SEARCH_FILE_LIMIT: u64 = 16 << 20;

pub fn schemas() -> Value {
    serde_json::from_str(TOOLS_JSON).expect("the generated schemas are JSON")
}

/// Independent reads run together; everything else runs alone and in order.
/// With the web tools, when they are on.
pub fn schemas_for(web: bool) -> Value {
    let mut all = schemas();
    if let (true, Some(list), Some(more)) = (web, all.as_array_mut(), crate::web::schemas().as_array().cloned()) {
        list.extend(more);
    }
    all
}

pub fn is_read(tool: &str) -> bool {
    matches!(tool, "read_file" | "list_files" | "search_files" | "show_diff")
}

#[derive(Default)]
pub struct Workbench {
    /// The hash of each file as this session last saw it: an edit needs a read of what is there now.
    seen: HashMap<PathBuf, String>,
    pub todos: Vec<Value>,
    /// HEAD when the session began, for `show_diff`'s session scope.
    pub baseline: Option<String>,
    pub web: crate::web::Web,
}

fn sha16(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

/// Head and tail, with what is left out said between them.
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

fn text_arg<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    arguments.get(name).and_then(Value::as_str).ok_or_else(|| format!("{name} is required"))
}

impl Workbench {
    pub fn new(workspace: &Path) -> Workbench {
        let baseline = git(workspace, &["rev-parse", "HEAD"]).ok().map(|head| head.trim().to_string());
        Workbench { baseline, ..Workbench::default() }
    }

    /// Runs a read that may share the workbench with others: what it saw comes back to record afterwards.
    pub fn run_read(tool: &str, arguments: &Value, workspace: &Path, baseline: Option<&str>, cancel: &AtomicBool) -> (Result<String, String>, Option<(PathBuf, String)>) {
        match tool {
            "read_file" => match read_file(arguments, workspace) {
                Ok((text, seen)) => (Ok(text), Some(seen)),
                Err(error) => (Err(error), None),
            },
            "list_files" => (list_files(arguments, workspace), None),
            "search_files" => (search_files(arguments, workspace, cancel), None),
            "show_diff" => (show_diff(arguments, workspace, baseline), None),
            other => (Err(format!("{other} is not a read")), None),
        }
    }

    pub fn run(&mut self, tool: &str, arguments: &Value, workspace: &Path, cancel: &AtomicBool) -> Result<String, String> {
        match tool {
            "apply_patch" => self.apply_patch(arguments, workspace),
            "create_file" => self.create_file(arguments, workspace),
            "execute_command" => execute_command(arguments, workspace, cancel),
            "todo" => self.todo(arguments),
            "web_search" | "web_read" if !self.web.settings.enabled => Err("the web tools are turned off in Settings".to_string()),
            "web_search" => self.web.search(arguments, cancel),
            "web_read" => self.web.read(arguments, cancel),
            _ => {
                let (result, seen) = Workbench::run_read(tool, arguments, workspace, self.baseline.as_deref(), cancel);
                if let Some((path, hash)) = seen {
                    self.seen.insert(path, hash);
                }
                result
            }
        }
    }

    pub fn record_seen(&mut self, path: PathBuf, hash: String) {
        self.seen.insert(path, hash);
    }

    /// Files went back: a patch must read them again first.
    pub fn forget_seen(&mut self) {
        self.seen.clear();
    }

    fn create_file(&mut self, arguments: &Value, workspace: &Path) -> Result<String, String> {
        let path = resolve(workspace, text_arg(arguments, "path")?);
        let content = text_arg(arguments, "content")?;
        if path.exists() {
            return Err(format!("{} exists: create_file cannot overwrite; use apply_patch", path.display()));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        std::fs::write(&path, content).map_err(|error| format!("{}: {error}", path.display()))?;
        self.seen.insert(path.clone(), sha16(content.as_bytes()));
        Ok(format!("created {} ({} bytes)", path.display(), content.len()))
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
                    let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}: nothing was changed", path.display()))?;
                    let hash = sha16(&bytes);
                    match self.seen.get(&path) {
                        None => return Err(format!("{given} has not been read in this session: read it first; nothing was changed")),
                        Some(known) if *known != hash => return Err(format!("{given} changed since it was read: read it again; nothing was changed")),
                        Some(_) => {}
                    }
                    let text = String::from_utf8(bytes).map_err(|_| format!("{given} is not UTF-8 text: nothing was changed"))?;
                    files.push((path, text.clone(), text));
                    files.len() - 1
                }
            };
            let text = &mut files[slot].2;
            // A file with CRLF lines takes an edit written with LF.
            let crlf = text.contains("\r\n") && !old.contains("\r\n");
            let (old, new) = if crlf { (old.replace('\n', "\r\n"), new.replace('\n', "\r\n")) } else { (old.to_string(), new.to_string()) };
            let found = text.matches(&old).count();
            if found != count {
                return Err(format!("edit {}: old_string occurs {found} times in {given}, not {count}: nothing was changed. Read the exact text; do not include line numbers", number + 1));
            }
            let changed = text.replace(&old, &new);
            if changed.is_empty() && !edit.get("allow_empty_file").and_then(Value::as_bool).unwrap_or(false) {
                return Err(format!("edit {} would leave {given} empty: nothing was changed", number + 1));
            }
            *text = changed;
        }
        let mut report = String::new();
        for (written, (path, before, after)) in files.iter().enumerate() {
            if let Err(error) = std::fs::write(path, after) {
                for (undo, original, _) in &files[..written] {
                    let _ = std::fs::write(undo, original);
                }
                return Err(format!("{}: {error}: the files are as they were", path.display()));
            }
            report.push_str(&format!("--- applied_diff {} ---\n{}\n", path.display(), similar::TextDiff::from_lines(before, after).unified_diff().context_radius(2)));
        }
        for (path, _, after) in files {
            self.seen.insert(path, sha16(after.as_bytes()));
        }
        Ok(cap(&report, MAX_OUTPUT))
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

fn read_file(arguments: &Value, workspace: &Path) -> Result<(String, (PathBuf, String)), String> {
    let path = resolve(workspace, text_arg(arguments, "path")?);
    let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    if bytes.iter().take(8192).any(|&byte| byte == 0) {
        return Err(format!("{} is binary: unsupported binaries are refused", path.display()));
    }
    let hash = sha16(&bytes);
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let limit = arguments.get("limit").and_then(Value::as_i64).unwrap_or(500).clamp(1, 2000) as usize;
    let start = arguments.get("start_line").and_then(Value::as_i64).unwrap_or(1);
    let first = if start < 0 { lines.len().saturating_sub(start.unsigned_abs() as usize) } else { (start.max(1) as usize) - 1 };
    let mut out = format!("--- sha256={hash} encoding=utf-8 ---\n");
    for (at, line) in lines.iter().enumerate().skip(first).take(limit) {
        out.push_str(&format!("{}|{line}\n", at + 1));
    }
    if first + limit < lines.len() {
        out.push_str(&format!("--- {} more lines; continue with start_line={} ---\n", lines.len() - first - limit, first + limit + 1));
    }
    Ok((cap(&out, MAX_OUTPUT * 4), (path, hash)))
}

fn walker(root: &Path) -> ignore::WalkBuilder {
    let mut walk = ignore::WalkBuilder::new(root);
    walk.hidden(false).filter_entry(|entry| entry.file_name() != ".git");
    walk
}

fn list_files(arguments: &Value, workspace: &Path) -> Result<String, String> {
    let root = resolve(workspace, arguments.get("path").and_then(Value::as_str).unwrap_or("."));
    if !root.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    let recursive = arguments.get("recursive").and_then(Value::as_bool).unwrap_or(false);
    let mut walk = walker(&root);
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
        files += 1;
        bytes += entry.metadata().map_or(0, |data| data.len());
        if names.len() < LIST_LIMIT {
            names.push(if folder { format!("{name}/") } else { name });
        }
        if files >= 50_000 {
            break;
        }
    }
    names.sort();
    let mut out = names.join("\n");
    if files > LIST_LIMIT {
        out.push_str(&format!("\n--- truncated: showing first {LIST_LIMIT} of {files} files ({bytes} bytes); use search_files for targeted lookup ---"));
    }
    Ok(out)
}

fn search_files(arguments: &Value, workspace: &Path, cancel: &AtomicBool) -> Result<String, String> {
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
    let mut walk = walker(&root);
    if let Some(glob) = arguments.get("file_glob").and_then(Value::as_str) {
        let mut overrides = ignore::overrides::OverrideBuilder::new(&root);
        overrides.add(glob).map_err(|error| format!("file_glob: {error}"))?;
        walk.overrides(overrides.build().map_err(|error| format!("file_glob: {error}"))?);
    }
    let mut out = note.to_string();
    let (mut matches, mut stopped) = (0usize, "");
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
    if !stopped.is_empty() {
        out.push_str(&format!("--- stopped: {stopped}; {matches} matches so far, narrow the path or pattern ---\n"));
    }
    Ok(cap(&out, MAX_OUTPUT))
}

/// The workspace's files, as `@` offers them: what git ignores is left out.
pub fn workspace_files(workspace: &Path, limit: usize) -> Vec<String> {
    let mut names: Vec<String> = walker(workspace).build().flatten()
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
    let status = git(workspace, &["-c", "core.quotepath=off", "status", "--porcelain=v2", "--branch"]).ok()?;
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
    let others = git(workspace, &["-c", "core.quotepath=off", "ls-files", "--others", "--exclude-standard"])?;
    for name in others.lines().take(MAX_UNTRACKED_SHOWN) {
        let path = workspace.join(name);
        let small = std::fs::metadata(&path).is_ok_and(|meta| meta.is_file() && meta.len() <= MAX_UNTRACKED_BYTES);
        let content = if small { std::fs::read(&path).ok().and_then(|bytes| String::from_utf8(bytes).ok()) } else { None };
        changes.push(match content {
            Some(content) => FileChange::between(name, None, Some(&content)),
            None => FileChange { path: name.to_string(), added: 0, removed: 0, diff: "A new file: too large, or not text.\n".to_string() },
        });
    }
    Ok(changes)
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

fn git(workspace: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    command.args(args).current_dir(workspace).stdin(Stdio::null());
    no_window(&mut command);
    let output = command.output().map_err(|error| format!("git: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn show_diff(arguments: &Value, workspace: &Path, baseline: Option<&str>) -> Result<String, String> {
    let scope = arguments.get("scope").and_then(Value::as_str).unwrap_or("workspace");
    let mut args = vec!["diff", "--no-ext-diff", "--no-color"];
    match scope {
        "staged" => args.push("--cached"),
        "unstaged" => {}
        "session" => args.push(baseline.ok_or("this session began outside a git repository: it has no baseline")?),
        // A repository with no commit yet has no HEAD to compare with.
        _ if git(workspace, &["rev-parse", "--verify", "-q", "HEAD"]).is_ok() => args.push("HEAD"),
        _ => {}
    }
    let paths: Vec<&str> = arguments.get("paths").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    if !paths.is_empty() {
        args.push("--");
        args.extend(paths);
    }
    let diff = git(workspace, &args)?;
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

#[cfg(windows)]
fn execute_command(arguments: &Value, workspace: &Path, cancel: &AtomicBool) -> Result<String, String> {
    let line = text_arg(arguments, "command")?;
    let cwd = resolve(workspace, arguments.get("cwd").and_then(Value::as_str).unwrap_or("."));
    let timeout = Duration::from_secs(arguments.get("timeout_seconds").and_then(Value::as_u64).unwrap_or(180).clamp(1, 3600));
    let network = crate::safety::uses_network(line);
    let started = Instant::now();
    // Spelled as `cwd` is, or a path with `..` in it would not be seen as holding it.
    let done = crate::sandbox::run(line, &cwd, &resolve(workspace, "."), network, timeout, cancel).map_err(|error| format!("{error}. The command was not started; changing its spelling cannot repair this"))?;
    let (out, err) = (crate::sandbox::decode(&done.stdout), crate::sandbox::decode(&done.stderr));
    let mut report = format!("sandbox: appcontainer (filesystem/process isolation active)\nnetwork: {}\n", if network { "approved for this command" } else { "off" });
    if done.status == Some(1) || done.status == Some(9009) {
        // "Not found" from a folder the sandbox cannot read says the wrong thing: the program is there.
        let looks_absent = err.contains("not recognized") || err.contains("認識されていません");
        let closed = crate::sandbox::closed_tool_folders();
        if looks_absent && !closed.is_empty() {
            let names: Vec<String> = closed.iter().map(|folder| folder.display().to_string()).collect();
            report.push_str(&format!("note: the sandbox cannot read these folders on PATH, so what is in them looks absent: {}. They are not missing. Tell the user that /tools opens them to commands, with one Windows prompt.\n", names.join(", ")));
        }
    }
    report.push_str(&format!("stdout:\n{}\nstderr:\n{}\n", cap(&out, MAX_OUTPUT), cap(&err, MAX_OUTPUT / 2)));
    match done.status {
        Some(code) => report.push_str(&format!("exit_code: {code}")),
        None => report.push_str(&format!("exit_code: none, {} after {} s", done.ended, started.elapsed().as_secs())),
    }
    Ok(report)
}

#[cfg(not(windows))]
fn execute_command(arguments: &Value, workspace: &Path, cancel: &AtomicBool) -> Result<String, String> {
    let line = text_arg(arguments, "command")?;
    let cwd = resolve(workspace, arguments.get("cwd").and_then(Value::as_str).unwrap_or("."));
    let timeout = Duration::from_secs(arguments.get("timeout_seconds").and_then(Value::as_u64).unwrap_or(180).clamp(1, 3600));
    let mut command = shell(line);
    command.current_dir(&cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    no_window(&mut command);
    let mut child = command.spawn().map_err(|error| format!("{}: {error}", cwd.display()))?;
    let capture = |pipe: Option<Box<dyn Read + Send>>| std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    });
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
        "read_file" | "list_files" | "create_file" => text("path").to_string(),
        "search_files" => format!("{} in {}", text("pattern"), arguments.get("path").and_then(Value::as_str).unwrap_or(".")),
        "execute_command" => text("command").lines().next().unwrap_or("").to_string(),
        "apply_patch" => arguments.get("expected_files").and_then(Value::as_array).map_or(String::new(), |files| files.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")),
        "show_diff" => text("scope").to_string(),
        "web_read" => text("url").to_string(),
        "web_search" => arguments.get("queries").and_then(Value::as_array).map_or(String::new(), |queries| queries.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")),
        _ => String::new(),
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
        assert_eq!(names, ["apply_patch", "create_file", "execute_command", "list_files", "read_file", "search_files", "show_diff", "todo"]);
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

    #[test]
    fn a_new_file_never_overwrites() {
        let (dir, mut bench, cancel) = bench();
        let create = json!({"path": "sub/a.txt", "content": "hi"});
        bench.run("create_file", &create, dir.path(), &cancel).unwrap();
        assert!(bench.run("create_file", &create, dir.path(), &cancel).is_err());
        assert_eq!(std::fs::read_to_string(dir.path().join("sub/a.txt")).unwrap(), "hi");
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
