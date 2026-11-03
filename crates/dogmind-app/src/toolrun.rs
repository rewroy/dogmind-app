//! Tool calls in a row, with nothing said between them, as one line of the transcript: the call under way while they run, what they came to once they
//! have ended, and every call again for whoever opens it. A call that failed, or that the person had to allow, stands on a line of its own.

use std::path::Path;
use std::time::Instant;

use crate::lang::{t, tf};
use crate::ui::tool_name;

/// One call of a run.
pub struct RunEntry {
    pub id: String,
    pub tool: String,
    /// As the agent says it: the whole path.
    pub summary: String,
    pub elapsed: Option<f64>,
    pub done: bool,
    /// What it has to show when the run is opened.
    pub output: Vec<String>,
    /// The model is still writing this call: it has not begun to run.
    pub writing: Option<Writing>,
}

/// A call as far as the model has written it.
pub struct Writing {
    pub index: usize,
    /// Its arguments so far, which are JSON up to a point.
    pub raw: String,
    /// How long they were when a path was last looked for in them.
    looked_at: usize,
}

impl RunEntry {
    pub fn running(id: String, tool: String, summary: String) -> RunEntry {
        RunEntry { id, tool, summary, elapsed: None, done: false, output: Vec::new(), writing: None }
    }

    pub fn being_written(index: usize, tool: String) -> RunEntry {
        RunEntry { writing: Some(Writing { index, raw: String::new(), looked_at: 0 }), ..RunEntry::running(String::new(), tool, String::new()) }
    }

    /// More of what the model is writing. What it has written of a path or a command is read as soon as it is whole.
    pub fn more_written(&mut self, piece: &str) {
        // (a call that writes its file before saying where is read no further: this runs a few times a second)
        const LOOKED_AT: usize = 64 * 1024;
        let Some(writing) = &mut self.writing else { return };
        writing.raw.push_str(piece);
        // (each time while it may be the first thing written, then as it doubles)
        let time_to_look = writing.raw.len() <= 4096 || writing.raw.len() >= writing.looked_at * 2;
        if self.summary.is_empty() && writing.raw.len() <= LOOKED_AT && time_to_look {
            writing.looked_at = writing.raw.len();
            let key = match self.tool.as_str() {
                "create_file" | "read_file" | "list_files" | "apply_patch" => "path",
                "execute_command" => "command",
                _ => return,
            };
            if let Some(value) = partial_value(&writing.raw, key) {
                self.summary = value.lines().next().unwrap_or("").chars().take(160).collect();
            }
        }
    }

    pub fn ended(tool: String, summary: String, output: Vec<String>) -> RunEntry {
        RunEntry { id: String::new(), tool, summary, elapsed: None, done: true, output, writing: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Done,
    Bad,
    Warn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Head(Tone),
    Entry,
    Output,
    Added,
    Removed,
    Hunk,
}

/// A run and where it is in the transcript.
pub struct ToolRun {
    pub entries: Vec<RunEntry>,
    pub expanded: bool,
    pub open: bool,
    pub began: Option<Instant>,
    /// When its last call ended.
    pub ended: Option<Instant>,
    pub took: Option<f64>,
    pub tone: Tone,
    /// What went wrong, after the call's name.
    pub note: String,
    /// Its lines in the transcript, from the first character to past the last line's end; below zero when it is not written.
    pub first: i32,
    pub last: i32,
    /// Where its first line's words end.
    pub head_end: i32,
    /// That line as it was last written, and when.
    pub written: String,
    pub written_at: Option<Instant>,
    /// How fast the model is writing, while it writes a call.
    pub live_rate: Option<f64>,
}

impl ToolRun {
    pub fn new() -> ToolRun {
        ToolRun { entries: Vec::new(), expanded: false, open: false, began: None, ended: None, took: None, tone: Tone::Done, note: String::new(), first: -1, last: -1, head_end: -1, written: String::new(), written_at: None, live_rate: None }
    }

    /// Whether there is more of it to see.
    pub fn expandable(&self) -> bool {
        if self.open {
            return self.entries.iter().any(|entry| entry.writing.is_some());
        }
        self.tone == Tone::Done && (self.entries.len() > 1 || self.entries.iter().any(|entry| !entry.output.is_empty()))
    }

    /// The call the row is about while the run goes on: the one the model is writing, or else the first still running, or else the last.
    fn current(&self) -> Option<&RunEntry> {
        self.entries.iter().rev().find(|entry| entry.writing.is_some()).or_else(|| self.entries.iter().find(|entry| !entry.done)).or(self.entries.last())
    }

    /// What the first line says. `units`: the room, in half-width characters. `since`: seconds from its first call.
    pub fn head(&self, workspace: &Path, units: usize, since: f64) -> String {
        const SPINNER: [char; 4] = ['◐', '◓', '◑', '◒'];
        let line = if self.open {
            let Some(entry) = self.current() else { return String::new() };
            let mark = if entry.done { '✓' } else { SPINNER[(since * 4.0) as usize % SPINNER.len()] };
            let mut after = String::new();
            if let Some(writing) = &entry.writing {
                after.push_str("  ·  ");
                after.push_str(&size_text(writing.raw.len()));
                if let Some(rate) = self.live_rate {
                    after.push_str(&format!("  ·  {rate:.1} tok/s"));
                }
            }
            if self.entries.len() > 1 {
                after.push_str("  ·  ");
                after.push_str(&tf("{0} so far", &[&self.entries.len()]));
            }
            if since >= 1.0 {
                after.push_str("  ·  ");
                after.push_str(&tf("{0} s", &[&(since as u64)]));
            }
            format!("{mark}  {}{after}", being_done(entry, workspace, units.saturating_sub(width(&after) + 3)))
        } else if self.tone != Tone::Done {
            let entry = self.entries.first();
            let what = entry.map_or(String::new(), |entry| named(entry, workspace, (units / 2).max(20)));
            let mark = '✗';
            let reason = bare_reason(&without_workspace(&self.note, workspace), &what);
            // (one row like its neighbours: the pointer on it has the whole of a long reason)
            if reason.is_empty() { format!("{mark}  {what}") } else { clip(&format!("{mark}  {what}  —  {reason}"), units) }
        } else if let [entry] = self.entries.as_slice() {
            let after = time_text(entry.elapsed.or(self.took));
            format!("✓  {}{after}", named(entry, workspace, units.saturating_sub(width(&after) + 3)))
        } else {
            format!("✓  {}{}", tally(&self.entries), time_text(self.took))
        };
        one_line(&line)
    }

    /// All its lines as they stand now.
    pub fn lines(&self, workspace: &Path, units: usize, since: f64) -> Vec<(String, LineKind)> {
        let mut lines = vec![(self.head(workspace, units, since), LineKind::Head(self.tone))];
        if self.open {
            if let Some(writing) = self.current().and_then(|entry| entry.writing.as_ref()).filter(|_| self.expanded) {
                lines.extend(written_tail(&writing.raw).into_iter().map(|line| (one_line(&line), LineKind::Output)));
            }
        } else if self.expanded && self.expandable() {
            let several = self.entries.len() > 1;
            for entry in &self.entries {
                if several {
                    // (the part everything here has in common goes without saying, and what is elsewhere says where)
                    let words = without_workspace(&full_summary(&entry.tool, &entry.summary, workspace), workspace);
                    let words = if words == workspace.display().to_string() { String::new() } else { words };
                    let name = t(tool_name(&entry.tool));
                    let line = if words.is_empty() { format!("{name}{}", time_text(entry.elapsed)) } else { format!("{name}  {words}{}", time_text(entry.elapsed)) };
                    lines.push((one_line(&line), LineKind::Entry));
                }
                let diff = entry.tool == "apply_patch";
                lines.extend(entry.output.iter().map(|line| (one_line(line), output_kind(line, diff))));
            }
        }
        lines
    }

    /// What the pointer on its first line says at the foot of the window.
    pub fn hover(&self, workspace: &Path) -> String {
        if self.open && self.expandable() {
            return t(if self.expanded { "Click to hide what it is writing" } else { "Click to see what it is writing" }).to_string();
        }
        if self.expandable() && self.entries.len() > 1 {
            return t(if self.expanded { "Click to hide these calls" } else { "Click to show these calls" }).to_string();
        }
        if !self.open && self.tone != Tone::Done && !self.note.is_empty() {
            return without_workspace(&self.note, workspace);
        }
        let shown = if self.open { self.current() } else { self.entries.first() };
        shown.map_or(String::new(), |entry| full_summary(&entry.tool, &entry.summary, workspace))
    }
}

fn output_kind(line: &str, diff: bool) -> LineKind {
    match line.chars().next() {
        Some('+') if diff => LineKind::Added,
        Some('-') if diff => LineKind::Removed,
        Some('@') if diff => LineKind::Hunk,
        _ => LineKind::Output,
    }
}

/// What a call is doing, in words. A file being written says so with its name, before it can have run.
pub fn being_done(entry: &RunEntry, workspace: &Path, room: usize) -> String {
    if entry.writing.is_none() || !matches!(entry.tool.as_str(), "create_file" | "apply_patch") {
        return named(entry, workspace, room);
    }
    if entry.summary.is_empty() {
        return t("Writing a file").to_string();
    }
    let words = t("Writing {0}");
    tf("Writing {0}", &[&clip(&short_summary(&entry.tool, &entry.summary, workspace), room.saturating_sub(width(words)).max(12))])
}

/// A call being written, for the line above the input when each call has no row of its own going on.
pub fn writing_line(entry: &RunEntry, workspace: &Path) -> String {
    let size = entry.writing.as_ref().map_or(0, |writing| writing.raw.len());
    format!("{}  ·  {}", being_done(entry, workspace, 60), size_text(size))
}

pub fn size_text(bytes: usize) -> String {
    match bytes {
        0..=1023 => format!("{bytes} B"),
        1024..=1048575 => format!("{} KB", bytes / 1024),
        _ => format!("{:.1} MB", bytes as f64 / 1048576.0),
    }
}

/// The value of this key in JSON that is still being written, wherever it stands, once its string has ended. Strings are stepped over whole, so that what a file's
/// content says about a path is not taken for one.
pub fn partial_value(raw: &str, key: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let string_end = |from: usize| -> Option<usize> {
        let mut at = from + 1;
        while at < bytes.len() {
            match bytes[at] {
                b'\\' => at += 2,
                b'"' => return Some(at),
                _ => at += 1,
            }
        }
        None
    };
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'"' {
            at += 1;
            continue;
        }
        let end = string_end(at)?;
        let mut next = end + 1;
        while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
            next += 1;
        }
        if bytes.get(next) == Some(&b':') && raw[at + 1..end] == *key {
            next += 1;
            while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
                next += 1;
            }
            if bytes.get(next) != Some(&b'"') {
                return None;
            }
            let value_end = string_end(next)?;
            return serde_json::from_str(&raw[next..=value_end]).ok();
        }
        at = end + 1;
    }
    None
}

/// The last lines of what is being written, for whoever opens the row: only the end is looked at, however long it has grown.
pub fn written_tail(raw: &str) -> Vec<String> {
    const LOOKED_AT: usize = 4096;
    const LINES: usize = 24;
    let mut from = raw.len().saturating_sub(LOOKED_AT);
    while !raw.is_char_boundary(from) {
        from += 1;
    }
    let mut part = &raw[from..];
    // (from its beginning, the file's words and not what says which file)
    if from == 0 {
        if let Some(found) = ["\"content\"", "\"new_string\"", "\"command\""].iter().find_map(|key| part.find(key).map(|at| at + key.len())) {
            let rest = &part[found..];
            if let Some(quote) = rest.find('"') {
                part = &rest[quote + 1..];
            }
        }
    }
    let text = unescape_partial(part);
    let lines: Vec<&str> = text.split('\n').collect();
    // (what the cut left of a line is not a line)
    let lines = if from > 0 && lines.len() > 1 { &lines[1..] } else { &lines[..] };
    lines[lines.len().saturating_sub(LINES)..].iter().map(|line| line.trim_end_matches('\r').chars().take(200).collect()).collect()
}

/// A JSON string's insides, wherever it was cut.
fn unescape_partial(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push_str("    "),
            Some('r') => {}
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(c);
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

fn named(entry: &RunEntry, workspace: &Path, room: usize) -> String {
    let name = t(tool_name(&entry.tool));
    let short = short_summary(&entry.tool, &entry.summary, workspace);
    if short.is_empty() {
        return name.to_string();
    }
    format!("{name}  {}", clip(&short, room.saturating_sub(width(name) + 2).max(12)))
}

/// (no time at all says more than `0.0s` on every line)
fn time_text(seconds: Option<f64>) -> String {
    // (what took no time worth waiting for says nothing about it)
    seconds.filter(|seconds| *seconds >= 5.0).map_or(String::new(), |seconds| format!("  ·  {seconds:.1}s"))
}

/// An error says where it happened with the whole path: the part everything here has in common goes.
fn without_workspace(words: &str, workspace: &Path) -> String {
    let root = workspace.display().to_string();
    let root = root.trim_end_matches(['/', '\\']);
    if root.is_empty() {
        return words.to_string();
    }
    words.replace(&format!("{root}\\"), "").replace(&format!("{root}/"), "")
}

/// Why a call failed, without what the row says already: an error begins with the path it is about, and ends with the system's number for it.
/// `what`: what the row says of the call. A path it does not name stays.
fn bare_reason(words: &str, what: &str) -> String {
    let words = crate::lang::notice(words);
    match words.split_once(": ") {
        Some((before, after)) if !after.is_empty() && what.contains(before.rsplit(['/', '\\']).next().unwrap_or(before)) => after.to_string(),
        _ => words,
    }
}

/// How many of each tool, in the order they first came.
pub fn tally(entries: &[RunEntry]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for entry in entries {
        match counts.iter_mut().find(|(tool, _)| *tool == entry.tool) {
            Some((_, count)) => *count += 1,
            None => counts.push((&entry.tool, 1)),
        }
    }
    let words: Vec<String> = counts.iter().map(|(tool, count)| {
        let name = t(tool_name(tool));
        if *count == 1 { name.to_string() } else { format!("{name} {count}") }
    }).collect();
    words.join(t(", "))
}

/// A row is a line, whatever a model has put in its arguments.
fn one_line(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// A character's room in half-widths.
fn char_width(c: char) -> usize {
    if u32::from(c) >= 0x1100 { 2 } else { 1 }
}

fn width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// As much of it as fits in this room.
pub fn clip(text: &str, units: usize) -> String {
    if width(text) <= units {
        return text.to_string();
    }
    let mut kept = String::new();
    let mut used = 1;
    for c in text.chars() {
        used += char_width(c);
        if used > units {
            break;
        }
        kept.push(c);
    }
    kept.push('…');
    kept
}

fn is_absolute(path: &str) -> bool {
    path.starts_with('/') || path.as_bytes().get(1) == Some(&b':')
}

/// A path as it is worth reading: from the workspace if it is in it, and its name if it is not.
pub fn short_path(path: &str, workspace: &Path) -> String {
    let clean = path.trim().replace('\\', "/");
    if clean.is_empty() || clean == "." {
        return clean;
    }
    if !is_absolute(&clean) {
        return clean.strip_prefix("./").unwrap_or(&clean).to_string();
    }
    let root = workspace.display().to_string().replace('\\', "/");
    let root = root.strip_prefix("//?/").unwrap_or(&root).trim_end_matches('/');
    let clean = clean.strip_prefix("//?/").unwrap_or(&clean);
    // (Windows does not tell the case of a path's letters apart)
    if let Some(head) = clean.get(..root.len()).filter(|head| !root.is_empty() && head.eq_ignore_ascii_case(root)) {
        let rest = &clean[head.len()..];
        if rest.is_empty() {
            return ".".to_string();
        }
        if let Some(rest) = rest.strip_prefix('/') {
            return rest.to_string();
        }
    }
    let name = clean.trim_end_matches('/');
    name.rsplit('/').next().unwrap_or(name).to_string()
}

/// A path whole.
fn full_path(path: &str, workspace: &Path) -> String {
    let path = path.trim();
    if path.is_empty() {
        return String::new();
    }
    // (a model writes its paths with either slash, and this is the name Windows would give it)
    if is_absolute(path) {
        return path.replace('/', "\\");
    }
    let joined = if path == "." { workspace.to_path_buf() } else { workspace.join(path.strip_prefix("./").unwrap_or(path)) };
    joined.display().to_string().replace('/', "\\")
}

/// A call's summary with each path that is in it put through this.
fn with_paths(tool: &str, summary: &str, change: impl Fn(&str) -> String) -> String {
    match tool {
        "read_file" | "list_files" | "create_file" => change(summary),
        "apply_patch" => summary.split(", ").map(&change).collect::<Vec<_>>().join(", "),
        "search_files" => match summary.rsplit_once(" in ") {
            Some((pattern, path)) => format!("{pattern} in {}", change(path)),
            None => summary.to_string(),
        },
        _ => summary.to_string(),
    }
}

pub fn short_summary(tool: &str, summary: &str, workspace: &Path) -> String {
    let short = with_paths(tool, summary, |path| short_path(path, workspace));
    // (the folder it is working in goes without saying)
    if short == "." {
        return String::new();
    }
    short.strip_suffix(" in .").map_or_else(|| short.clone(), str::to_string)
}

pub fn full_summary(tool: &str, summary: &str, workspace: &Path) -> String {
    with_paths(tool, summary, |path| full_path(path, workspace))
}

/// How a command ended, from what the agent reports: none if it did not run to its end at zero. (A command that fails is no failed call: the model reads
/// its output and goes on. The person is who has to see it.)
pub fn command_failed(output: &str) -> Option<String> {
    let (_, rest) = output.rsplit_once("exit_code:")?;
    let value = rest.lines().next().unwrap_or("").trim();
    match value {
        "0" => None,
        _ => match value.strip_prefix("none,") {
            Some(how) => Some(how.trim().to_string()),
            None => Some(tf("exit code {0}", &[&value])),
        },
    }
}

/// The last lines of what a command wrote, and how many there were above them.
pub fn command_lines(output: &str) -> (usize, Vec<String>) {
    let body = output.split_once("stdout:\n").map_or("", |(_, rest)| rest);
    let (out, rest) = body.split_once("\nstderr:\n").unwrap_or((body, ""));
    let err = rest.rsplit_once("\nexit_code:").map_or(rest, |(text, _)| text);
    let lines: Vec<&str> = out.lines().chain(err.lines()).filter(|line| !line.trim().is_empty()).collect();
    let from = lines.len().saturating_sub(6);
    (from, lines[from..].iter().map(|line| line.chars().take(200).collect()).collect())
}

/// What a diff changed, without its file names.
pub fn diff_lines(text: &str) -> Vec<&str> {
    text.lines().filter(|line| !line.starts_with("--- ") && !line.starts_with("+++ ")).collect()
}

/// What a call that went well keeps for when its run is opened.
pub fn kept_output(tool: &str, output: &str) -> Vec<String> {
    const MOST_DIFF_LINES: usize = 40;
    match tool {
        "execute_command" => {
            let (above, lines) = command_lines(output);
            let mut kept = Vec::new();
            if above > 0 {
                kept.push(tf("… {0} lines above", &[&above]));
            }
            kept.extend(lines);
            kept
        }
        "apply_patch" => {
            let lines = diff_lines(output);
            let mut kept: Vec<String> = lines.iter().take(MOST_DIFF_LINES).map(|line| line.to_string()).collect();
            if lines.len() > MOST_DIFF_LINES {
                kept.push(tf("… {0} more lines", &[&(lines.len() - MOST_DIFF_LINES)]));
            }
            kept
        }
        _ => Vec::new(),
    }
}

/// How a call told from a session's file went: its words if badly. `None`: it went well.
pub fn told_failure(tool: &str, result: &str) -> Option<(Tone, String)> {
    if let Some(words) = result.strip_prefix("error: ") {
        if words.starts_with("the user declined") {
            return Some((Tone::Warn, t("Rejected").to_string()));
        }
        return Some((Tone::Bad, words.lines().next().unwrap_or("").chars().take(240).collect()));
    }
    if tool == "execute_command" {
        return command_failed(result).map(|how| (Tone::Bad, how));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn work() -> PathBuf {
        PathBuf::from(r"C:\Projects\App")
    }

    fn call(tool: &str, summary: &str, seconds: f64) -> RunEntry {
        let mut entry = RunEntry::ended(tool.to_string(), summary.to_string(), Vec::new());
        entry.elapsed = Some(seconds);
        entry
    }

    #[test]
    fn a_path_shortens_to_the_workspace_or_to_its_name() {
        let w = work();
        assert_eq!(short_path(r"C:\Projects\App\src\tools.rs", &w), "src/tools.rs");
        assert_eq!(short_path(r"c:\projects\app\src\tools.rs", &w), "src/tools.rs", "the case of a Windows path");
        assert_eq!(short_path("C:/Projects/App/README.md", &w), "README.md");
        assert_eq!(short_path(r"C:\Projects\App", &w), ".");
        assert_eq!(short_path(r"C:\Projects\Application\x.rs", &w), "x.rs", "a folder whose name only begins the same");
        assert_eq!(short_path(r"D:\elsewhere\notes.txt", &w), "notes.txt");
        assert_eq!(short_path("./src/lib.rs", &w), "src/lib.rs");
        assert_eq!(short_path(r"src\lib.rs", &w), "src/lib.rs");
        assert_eq!(short_path("/etc/hosts", &w), "hosts");
        assert_eq!(short_path(r"\\?\C:\Projects\App\a.rs", &PathBuf::from(r"\\?\C:\Projects\App")), "a.rs");
    }

    #[test]
    fn the_paths_in_a_summary_are_found_by_its_tool() {
        let w = work();
        assert_eq!(short_summary("search_files", r"fn main in C:\Projects\App\src", &w), "fn main in src");
        assert_eq!(short_summary("apply_patch", r"C:\Projects\App\a.rs, C:\Projects\App\b\c.rs", &w), "a.rs, b/c.rs");
        assert_eq!(short_summary("execute_command", r"type C:\Projects\App\a.rs", &w), r"type C:\Projects\App\a.rs", "a command is as it was given");
        assert_eq!(full_summary("read_file", "src/a.rs", &w), r"C:\Projects\App\src\a.rs");
        assert_eq!(full_summary("read_file", "C:/elsewhere/a.rs", &w), r"C:\elsewhere\a.rs");
        assert_eq!(full_summary("read_file", r"D:\b.rs", &w), r"D:\b.rs");
        assert_eq!(full_summary("search_files", "x in .", &w), format!("x in {}", w.display()));
    }

    #[test]
    fn what_is_too_long_for_its_row_is_cut_by_its_width() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("abcdefghij", 5), "abcd…");
        assert_eq!(clip("日本語のパス", 7), "日本語…");
    }

    #[test]
    fn a_command_has_failed_unless_it_ended_at_zero() {
        assert_eq!(command_failed("stdout:\nok\nstderr:\n\nexit_code: 0"), None);
        assert!(command_failed("stdout:\n\nstderr:\nbad\n\nexit_code: 3").is_some_and(|how| how.contains('3')));
        assert_eq!(command_failed("stdout:\n\nstderr:\n\nexit_code: none, timed out after 30 s").as_deref(), Some("timed out after 30 s"));
        assert_eq!(command_failed("stdout:\nexit_code: 9 was printed\nstderr:\n\nexit_code: 0"), None, "the report's own last line counts");
        assert_eq!(command_failed("started in the background"), None);
    }

    #[test]
    fn a_command_keeps_its_last_lines() {
        let many: Vec<String> = (0..10).map(|at| format!("line {at}")).collect();
        let (above, lines) = command_lines(&format!("stdout:\n{}\nstderr:\nwarned\n\nexit_code: 1", many.join("\n")));
        assert_eq!(above, 5);
        assert_eq!(lines.last().map(String::as_str), Some("warned"));
        assert_eq!(lines.len(), 6);
    }

    #[test]
    fn a_closed_run_counts_its_calls_in_the_order_they_came() {
        let mut run = ToolRun::new();
        run.took = Some(31.5);
        run.entries = vec![call("read_file", "a.rs", 0.1), call("search_files", "x in .", 0.2), call("read_file", "b.rs", 0.1)];
        let words = (t(tool_name("read_file")).to_string(), t(tool_name("search_files")).to_string());
        let head = run.head(&work(), 80, 40.0);
        let (read, search) = (head.find(&words.0).unwrap(), head.find(&words.1).unwrap());
        assert!(read < search, "{head}");
        assert!(head.starts_with('✓') && head.ends_with("31.5s") && head.contains('2'), "{head}");
        assert!(run.expandable());
        run.expanded = true;
        let lines = run.lines(&work(), 80, 40.0);
        assert_eq!(lines.len(), 4);
        assert!(lines[1].0.contains("a.rs") && !lines[1].0.contains(r"C:\Projects\App"), "opened, from this folder: {}", lines[1].0);
        run.entries[0].summary = r"D:\elsewhere\a.rs".to_string();
        assert!(run.lines(&work(), 80, 40.0)[1].0.contains(r"D:\elsewhere\a.rs"), "and what is elsewhere says where");
    }

    #[test]
    fn a_failure_says_why_and_not_where_again() {
        let what = "Read  src/missing.rs";
        assert_eq!(bare_reason(r"src\missing.rs: not found (os error 2)", what), "not found");
        assert_eq!(bare_reason("src/missing.rs: not found", what), "not found");
        assert_eq!(bare_reason("src/other.rs: not found", what), "src/other.rs: not found", "not what the row names");
        assert_eq!(bare_reason("denied: by a rule", what), "denied: by a rule");
        assert_eq!(bare_reason("", what), "");
    }

    #[test]
    fn a_single_call_needs_opening_only_for_what_it_wrote() {
        let mut run = ToolRun::new();
        run.entries = vec![call("read_file", r"C:\Projects\App\a.rs", 0.1)];
        assert!(!run.expandable());
        let head = run.head(&work(), 80, 1.0);
        assert!(head.contains("a.rs") && !head.contains("Projects") && !head.contains("0.1s"), "what took no time says nothing about it: {head}");
        assert!(run.hover(&work()).contains("Projects"), "the whole path on the pointer");
        run.entries[0].tool = "execute_command".to_string();
        run.entries[0].output = vec!["built".to_string()];
        assert!(run.expandable());
        run.expanded = true;
        assert_eq!(run.lines(&work(), 80, 1.0).len(), 2);
    }

    #[test]
    fn an_open_run_shows_the_call_under_way_in_one_line() {
        let mut run = ToolRun::new();
        run.open = true;
        run.entries = vec![call("read_file", "a.rs", 0.1), RunEntry::running("2".into(), "read_file".into(), r"C:\Projects\App\src\b.rs".into())];
        let head = run.head(&work(), 80, 4.2);
        assert!(head.contains("src/b.rs") && !head.contains("a.rs") && head.contains('4') && head.contains('2'), "{head}");
        assert!(!head.starts_with('✓'), "{head}");
        assert_eq!(run.lines(&work(), 80, 4.2).len(), 1);
        assert!(!run.expandable(), "while it runs");
        run.entries[1].done = true;
        assert!(run.head(&work(), 80, 4.2).starts_with('✓'), "between two calls");
        // What a model put in a path stays in one row.
        run.entries[1].summary = "two\nlines".to_string();
        assert!(!run.head(&work(), 80, 4.2).contains('\n'));
    }

    #[test]
    fn a_long_call_leaves_its_time_in_the_row() {
        let mut run = ToolRun::new();
        run.open = true;
        run.entries = vec![RunEntry::running("1".into(), "read_file".into(), format!("{}/end.rs", "very_long_folder_name/".repeat(20)))];
        let head = run.head(&work(), 60, 9.0);
        assert!(width(&head) <= 62, "the ring and the dots are wide ones: {head}");
        assert!(head.contains('9'), "{head}");
    }

    #[test]
    fn a_path_is_read_from_arguments_still_on_their_way() {
        assert_eq!(partial_value(r#"{"pa"#, "path"), None);
        assert_eq!(partial_value(r#"{"path": "in"#, "path"), None, "not until it has ended");
        assert_eq!(partial_value(r#"{"path": "src\\index.html", "content": "<ht"#, "path").as_deref(), Some(r"src\index.html"));
        assert_eq!(partial_value(r#"{"content": "say \"path\": \"no.txt\"", "path": "yes.txt""#, "path").as_deref(), Some("yes.txt"), "what the content says is not the key");
        assert_eq!(partial_value(r#"{"edits": [{"path": "a.rs", "old_string": "x"#, "path").as_deref(), Some("a.rs"));
        assert_eq!(partial_value(r#"{"content": "never ends"#, "path"), None);
        let mut entry = RunEntry::being_written(0, "create_file".into());
        entry.more_written(r#"{"path": "index"#);
        assert_eq!(entry.summary, "");
        entry.more_written(r#".html", "content": "<html>"#);
        assert_eq!(entry.summary, "index.html");
        assert!(being_done(&entry, &work(), 60).contains("index.html"));
    }

    #[test]
    fn the_row_says_what_is_being_written_and_how_much() {
        let mut run = ToolRun::new();
        run.open = true;
        run.live_rate = Some(22.6);
        run.entries = vec![call("read_file", "a.rs", 0.1), RunEntry::being_written(0, "create_file".into())];
        run.entries[1].more_written(&format!(r#"{{"path": "C:\\Projects\\App\\web\\index.html", "content": "{}"#, "x".repeat(18 * 1024)));
        let head = run.head(&work(), 90, 829.0);
        assert!(head.contains("web/index.html") && head.contains("18 KB") && head.contains("22.6 tok/s") && head.contains("829"), "{head}");
        assert!(run.expandable(), "what it writes can be looked at");
        assert_eq!(run.lines(&work(), 90, 829.0).len(), 1, "closed, one line");
        run.expanded = true;
        let lines = run.lines(&work(), 90, 829.0);
        assert!(lines.len() > 1 && lines.len() <= 25);
        assert_eq!(size_text(1023), "1023 B");
        assert_eq!(size_text(2048), "2 KB");
        assert_eq!(size_text(1572864), "1.5 MB");
    }

    #[test]
    fn what_is_being_written_reads_from_its_end_however_it_was_cut() {
        assert_eq!(written_tail(r#"{"path":"a","content":"one\ntwo\n  thr"#), ["one", "two", "  thr"]);
        assert_eq!(written_tail(r#"{"path":"a","content":"cut in an escape\"#), ["cut in an escape"]);
        let long = format!(r#"{{"content":"{}"#, "行\n".repeat(10_000));
        let tail = written_tail(&long);
        assert!(tail.len() <= 24 && tail.iter().rev().nth(1).is_some_and(|line| line == "行"), "{tail:?}");
    }

    #[test]
    fn a_failure_stands_with_its_words() {
        assert_eq!(told_failure("read_file", "error: no such file\nmore"), Some((Tone::Bad, "no such file".to_string())));
        assert_eq!(told_failure("read_file", "error: the user declined this call").map(|(tone, _)| tone), Some(Tone::Warn));
        assert_eq!(told_failure("read_file", "fn main"), None);
        assert_eq!(told_failure("execute_command", "stdout:\n\nstderr:\n\nexit_code: 0"), None);
        assert!(told_failure("execute_command", "stdout:\n\nstderr:\n\nexit_code: 2").is_some());
        let mut run = ToolRun::new();
        run.tone = Tone::Bad;
        run.note = "no such file".to_string();
        run.entries = vec![call("read_file", "gone.rs", 0.1)];
        let head = run.head(&work(), 80, 1.0);
        assert!(head.starts_with('✗') && head.contains("gone.rs") && head.ends_with("no such file"), "{head}");
        assert!(!run.expandable());
        run.note = format!("{}\\src\\gone.rs: not found", work().display());
        let head = run.head(&work(), 80, 1.0);
        assert!(head.ends_with("not found") && head.contains("gone.rs") && !head.contains(&work().display().to_string()), "its place once, without what all of them share: {head}");
    }

    #[test]
    fn no_time_is_better_than_none_of_it() {
        let mut run = ToolRun::new();
        run.entries = vec![call("read_file", "a.rs", 0.01)];
        assert!(!run.head(&work(), 80, 1.0).contains('s') || run.head(&work(), 80, 1.0).ends_with("a.rs"), "{}", run.head(&work(), 80, 1.0));
    }

    #[test]
    fn what_a_call_keeps_for_opening_is_what_the_person_would_read() {
        assert!(kept_output("read_file", "the whole file").is_empty());
        assert_eq!(kept_output("execute_command", "stdout:\nbuilt\nstderr:\n\nexit_code: 0"), vec!["built"]);
        let diff = "--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n";
        assert_eq!(kept_output("apply_patch", diff), vec!["@@ -1 +1 @@", "-old", "+new"]);
    }
}
