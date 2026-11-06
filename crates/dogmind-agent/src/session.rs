//! A conversation on disk: one JSON line a message, in the app's own folder. Dogmind's `sessions.db` is sealed and is not
//! touched. A snapshot line replaces all before it, so a resumed session sends the same prefix it last sent.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::message::Message;
use crate::prompt::typed_text;

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Line {
    Meta { workspace: PathBuf, created: String },
    Message { message: Message },
    Snapshot { messages: Vec<Message> },
    Title { title: String },
    /// Web content has entered this conversation: for as long as the session lives, whatever a compaction leaves of it.
    WebSeen,
    /// The tools this conversation is offered, the last of these lines counting: they are part of its prefix, and what is installed may differ when it is reopened.
    Tools { tools: serde_json::Value },
    /// What the session works toward, as the person set it (/goal), the last of these lines counting; none: cleared.
    Goal { goal: Option<String> },
}

pub struct Session {
    pub path: PathBuf,
    file: File,
}

/// A workspace's sessions, the newest first, and the workspaces with the most recent session first.
pub fn by_workspace(sessions: Vec<Summary>) -> Vec<(PathBuf, Vec<Summary>)> {
    let key = |path: &Path| path.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let mut groups: Vec<(String, PathBuf, Vec<Summary>)> = Vec::new();
    for summary in sessions {
        let name = key(&summary.workspace);
        match groups.iter_mut().find(|group| group.0 == name) {
            Some(group) => group.2.push(summary),
            None => groups.push((name, summary.workspace.clone(), vec![summary])),
        }
    }
    groups.into_iter().map(|(_, workspace, sessions)| (workspace, sessions)).collect()
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub path: PathBuf,
    pub title: String,
    pub workspace: PathBuf,
    pub created: String,
    pub messages: usize,
}

impl Session {
    pub fn create(folder: &Path, workspace: &Path, now: &str, stamp: &str) -> Result<Session, String> {
        fs::create_dir_all(folder).map_err(|error| format!("{}: {error}", folder.display()))?;
        // (two chats begun in one second must not share a file: their lines would make one conversation of the two)
        let mut path = folder.join(format!("{stamp}.jsonl"));
        let mut again = 1;
        let file = loop {
            match OpenOptions::new().create_new(true).append(true).open(&path) {
                Ok(file) => break file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    again += 1;
                    path = folder.join(format!("{stamp}-{again}.jsonl"));
                }
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
        };
        let mut session = Session { path, file };
        session.write(&Line::Meta { workspace: workspace.to_path_buf(), created: now.to_string() })?;
        Ok(session)
    }

    /// The session's messages, and it ready to append to. A tool call left without its result (the app was closed
    /// during the call) gets one, as the server refuses a conversation without.
    pub fn open(path: &Path) -> Result<(Session, PathBuf, Vec<Message>), String> {
        // (whatever a crash cut in two, a character too, and what is written next starts a line of its own)
        let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
        let text = String::from_utf8_lossy(&bytes);
        let (mut workspace, mut messages) = (PathBuf::new(), Vec::new());
        for line in text.lines() {
            match serde_json::from_str(line) {
                Ok(Line::Meta { workspace: place, .. }) => workspace = place,
                Ok(Line::Message { message }) => messages.push(message),
                Ok(Line::Snapshot { messages: all }) => messages = all,
                Ok(Line::Title { .. } | Line::WebSeen | Line::Tools { .. } | Line::Goal { .. }) => {}
                Err(_) => {} // a line cut short by a crash
            }
        }
        complete_tool_calls(&mut messages);
        let mut file = OpenOptions::new().append(true).open(path).map_err(|error| format!("{}: {error}", path.display()))?;
        if bytes.last().is_some_and(|last| *last != b'\n') {
            file.write_all(b"\n").map_err(|error| format!("{}: {error}", path.display()))?;
        }
        Ok((Session { path: path.to_path_buf(), file }, workspace, messages))
    }

    fn write(&mut self, line: &Line) -> Result<(), String> {
        let mut text = serde_json::to_string(line).map_err(|error| error.to_string())?;
        // (in one write: a crash then cuts at most this line)
        text.push('\n');
        self.file.write_all(text.as_bytes()).map_err(|error| format!("{}: {error}", self.path.display()))
    }

    pub fn append(&mut self, message: &Message) -> Result<(), String> {
        self.write(&Line::Message { message: message.clone() })
    }

    pub fn mark_web_seen(&mut self) -> Result<(), String> {
        self.write(&Line::WebSeen)
    }

    pub fn set_goal(&mut self, goal: Option<&str>) -> Result<(), String> {
        self.write(&Line::Goal { goal: goal.map(str::to_string) })
    }

    pub fn set_tools(&mut self, tools: &serde_json::Value) -> Result<(), String> {
        self.write(&Line::Tools { tools: tools.clone() })
    }

    pub fn snapshot(&mut self, messages: &[Message]) -> Result<(), String> {
        self.write(&Line::Snapshot { messages: messages.to_vec() })
    }
}

/// A session file's lines, each as far as it reads: a line a crash cut in the middle of a character is one line, and those after it are
/// read too. (As text, reading stopped at it, and what came after (a name, the mark of web content) went unseen.)
fn lines_of(path: &Path) -> std::io::Result<impl Iterator<Item = String>> {
    let reader = BufReader::new(File::open(path)?);
    Ok(reader.split(b'\n').map_while(Result::ok).map(|bytes| String::from_utf8_lossy(&bytes).trim_end_matches('\r').to_string()))
}

pub fn web_seen(path: &Path) -> bool {
    let marked = serde_json::to_string(&Line::WebSeen).expect("a plain line serializes");
    lines_of(path).is_ok_and(|mut lines| lines.any(|line| line == marked))
}

/// What a session works toward, as the person last set it; none when it was never set or was cleared.
pub fn goal_of(path: &Path) -> Option<String> {
    lines_of(path).ok()?.filter(|line| line.starts_with("{\"kind\":\"goal\"")).filter_map(|line| match serde_json::from_str(&line) {
        Ok(Line::Goal { goal }) => Some(goal),
        _ => None,
    }).last().flatten()
}

/// The tools a session was last offered; none in a file from before they were kept.
pub fn tools_of(path: &Path) -> Option<serde_json::Value> {
    lines_of(path).ok()?.filter(|line| line.starts_with("{\"kind\":\"tools\"")).filter_map(|line| match serde_json::from_str(&line) {
        Ok(Line::Tools { tools }) => Some(tools),
        _ => None,
    }).last()
}

/// What can be said in a line of a list of tools, so that a person can tell which a session was given: a reopened one keeps its own, whatever this build offers.
pub fn describe_tools(tools: &serde_json::Value) -> String {
    use sha2::Digest;
    let version: String = sha2::Sha256::digest(tools.to_string().as_bytes()).iter().take(6).map(|byte| format!("{byte:02x}")).collect();
    let list = tools.as_array().map_or(&[][..], Vec::as_slice);
    let command = list.iter().find(|tool| tool["function"]["name"] == "execute_command");
    let network = command.is_some_and(|tool| tool["function"]["parameters"]["properties"].get("network").is_some());
    format!("{version}, {} tools, network parameter: {}", list.len(), if network { "yes" } else { "no" })
}

/// A session's messages as they are on disk, for a reader beside the agent that writes them: a call still running has
/// no result yet, and none is made up.
pub fn read(path: &Path) -> Result<Vec<Message>, String> {
    let mut messages = Vec::new();
    for line in lines_of(path).map_err(|error| format!("{}: {error}", path.display()))? {
        match serde_json::from_str(&line) {
            Ok(Line::Message { message }) => messages.push(message),
            Ok(Line::Snapshot { messages: all }) => messages = all,
            _ => {}
        }
    }
    Ok(messages)
}

/// A session that has what was searched for, with the words around its first place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub path: PathBuf,
    pub title: String,
    pub snippet: String,
}

const SNIPPET_BEFORE: usize = 24;
const SNIPPET_AFTER: usize = 60;

fn snippet(text: &str, at: usize) -> String {
    let start: usize = text[..at].char_indices().rev().nth(SNIPPET_BEFORE - 1).map_or(0, |(position, _)| position);
    let shown: String = text[start..].chars().take(SNIPPET_BEFORE + SNIPPET_AFTER).map(|c| if c.is_whitespace() { ' ' } else { c }).collect();
    if start > 0 { format!("…{}", shown.trim_start()) } else { shown.trim_start().to_string() }
}

/// Where a word is, by the original text's bytes, whatever its case.
fn place_of(text: &str, word: &[char]) -> Option<usize> {
    text.char_indices().map(|(at, _)| at).find(|at| {
        let mut rest = text[*at..].chars().flat_map(char::to_lowercase);
        word.iter().all(|wanted| rest.next() == Some(*wanted))
    })
}

/// The sessions with every word of `query` in their name or in what was said (not in the tools' output, and not in the
/// system prompt, which every session has), the newest first.
pub fn search(folder: &Path, query: &str) -> Vec<Hit> {
    let words: Vec<Vec<char>> = query.split_whitespace().map(|word| word.chars().flat_map(char::to_lowercase).collect()).collect();
    if words.is_empty() {
        return Vec::new();
    }
    list(folder).into_iter().filter_map(|summary| {
        let mut said = vec![summary.title.clone()];
        for message in read(&summary.path).ok()? {
            match (message.role.as_str(), message.content) {
                ("user", Some(text)) if crate::prompt::is_nudge(&text) => {}
                ("user", Some(text)) => said.push(typed_text(&text).to_string()),
                ("assistant", Some(text)) => said.push(text),
                _ => {}
            }
        }
        let mut shown = None;
        for word in &words {
            let (at, text) = said.iter().find_map(|text| place_of(text, word).map(|at| (at, text)))?;
            shown.get_or_insert_with(|| snippet(text, at));
        }
        Some(Hit { path: summary.path, title: summary.title, snippet: shown.unwrap_or_default() })
    }).collect()
}

/// A name for a session, as the person chose it.
pub fn rename(path: &Path, title: &str) -> Result<(), String> {
    let text = serde_json::to_string(&Line::Title { title: title.to_string() }).map_err(|error| error.to_string())?;
    // (after a line a crash cut short, on a line of its own: run on into it, the name was part of a line nothing reads)
    let ends_a_line = std::fs::File::open(path).and_then(|mut file| {
        use std::io::{Read, Seek, SeekFrom};
        if file.seek(SeekFrom::End(0))? == 0 {
            return Ok(true);
        }
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last)?;
        Ok(last[0] == b'\n')
    }).unwrap_or(true);
    let mut file = OpenOptions::new().append(true).open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    writeln!(file, "{}{text}", if ends_a_line { "" } else { "\n" }).map_err(|error| format!("{}: {error}", path.display()))
}

/// A new session beside this one, with its conversation through its `through`th message (from 0): to go on from there another way, as
/// DeepSeek Harness's fork does. It has the workspace, the tools this one was offered, its mark of web content, and the pictures of the
/// messages it keeps; `title`, when given, is its name. This one is only read. What changed in the workspace since is not undone: a branch
/// of the conversation, not of the files.
pub fn fork(path: &Path, through: usize, title: Option<&str>) -> Result<PathBuf, String> {
    // (as `open` reads it: a line a crash cut in the middle of a character is one line lost, not the whole session refused)
    let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let (mut workspace, mut messages) = (PathBuf::new(), Vec::new());
    for line in text.lines() {
        match serde_json::from_str(line) {
            Ok(Line::Meta { workspace: place, .. }) => workspace = place,
            Ok(Line::Message { message }) => messages.push(message),
            Ok(Line::Snapshot { messages: all }) => messages = all,
            _ => {}
        }
    }
    if through >= messages.len() {
        return Err(format!("{}: it has {} messages, not {}", path.display(), messages.len(), through + 1));
    }
    messages.truncate(through + 1);
    complete_tool_calls(&mut messages);
    let folder = path.parent().ok_or_else(|| format!("{}: no folder", path.display()))?;
    let now = chrono::Local::now();
    let mut copy = Session::create(folder, &workspace, &now.format("%Y-%m-%d %H:%M:%S %:z").to_string(), &now.format("%Y%m%d-%H%M%S").to_string())?;
    if let Some(tools) = tools_of(path) {
        copy.set_tools(&tools)?;
    }
    if web_seen(path) {
        copy.mark_web_seen()?;
    }
    for message in &messages {
        copy.append(message)?;
    }
    if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
        copy.write(&Line::Title { title: title.to_string() })?;
    }
    let (from, to) = (crate::images::folder_of(path), crate::images::folder_of(&copy.path));
    for image in messages.iter().flat_map(|message| &message.images) {
        // (one gone from the original is gone from the branch too: it says so where it would show)
        if from.join(&image.file).is_file() {
            fs::create_dir_all(&to).map_err(|error| format!("{}: {error}", to.display()))?;
            fs::copy(from.join(&image.file), to.join(&image.file)).map_err(|error| format!("{}: {error}", to.display()))?;
        }
    }
    Ok(copy.path)
}

/// Stub results for calls that have none, right after their assistant message; and no result for a call that is not there (its line
/// was what a crash cut), which a server refuses as surely.
pub fn complete_tool_calls(messages: &mut Vec<Message>) {
    let mut at = 0;
    while at < messages.len() {
        let calls: Vec<String> = messages[at].tool_calls.iter().flatten().map(|call| call.id.clone()).collect();
        if messages[at].role != "tool" {
            at += 1;
        }
        let mut answered = Vec::new();
        while at < messages.len() && messages[at].role == "tool" {
            match messages[at].tool_call_id.clone().filter(|id| calls.contains(id) && !answered.contains(id)) {
                Some(id) => {
                    answered.push(id);
                    at += 1;
                }
                None => {
                    messages.remove(at);
                }
            }
        }
        for id in calls.iter().filter(|id| !answered.contains(id)) {
            messages.insert(at, Message::tool_result(id, "interrupted: this call did not finish"));
            at += 1;
        }
    }
}

/// The sessions in a folder, the newest first.
pub fn list(folder: &Path) -> Vec<Summary> {
    let mut paths: Vec<PathBuf> = fs::read_dir(folder).into_iter().flatten().flatten().map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl") && !crate::trajectory::is_trajectory(path)).collect();
    paths.sort();
    paths.reverse();
    paths.into_iter().filter_map(|path| {
        let (mut workspace, mut created, mut title, mut messages, mut named) = (PathBuf::new(), String::new(), String::new(), 0, None);
        for line in lines_of(&path).ok()? {
            // Once it has its title, a message is only counted: its text may be megabytes.
            if line.starts_with("{\"kind\":\"title\"") {
                if let Ok(Line::Title { title }) = serde_json::from_str(&line) {
                    named = Some(title);
                }
                continue;
            }
            if !title.is_empty() {
                messages += usize::from(line.starts_with("{\"kind\":\"message\""));
                continue;
            }
            match serde_json::from_str(&line) {
                Ok(Line::Meta { workspace: place, created: at }) => (workspace, created) = (place, at),
                Ok(Line::Message { message }) => {
                    messages += 1;
                    if title.is_empty() && message.role == "user" {
                        title = message.content.as_deref().map(|text| typed_text(text).lines().next().unwrap_or("").chars().take(70).collect()).unwrap_or_default();
                    }
                }
                _ => {}
            }
        }
        (messages > 0).then_some(Summary { path, title: named.unwrap_or(title), workspace, created, messages })
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{FunctionCall, ToolCall};

    fn call(id: &str) -> ToolCall {
        ToolCall { id: id.into(), kind: "function".into(), function: FunctionCall { name: "read_file".into(), arguments: "{}".into() } }
    }

    #[test]
    fn a_branch_has_the_conversation_through_its_message_and_its_pictures_and_the_original_is_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("C:/w"), "now", "20261103-1").unwrap();
        session.set_tools(&serde_json::json!([{"function": {"name": "read_file"}}])).unwrap();
        session.append(&Message::text("system", "s")).unwrap();
        let mut asked = Message::text("user", "look at this");
        asked.images.push(crate::message::ImageRef { file: "a.png".into(), media_type: "image/png".into(), width: 1, height: 1 });
        session.append(&asked).unwrap();
        session.append(&Message::text("assistant", "first answer")).unwrap();
        session.append(&Message::text("user", "then this")).unwrap();
        session.append(&Message::text("assistant", "second answer")).unwrap();
        let path = session.path.clone();
        drop(session);
        fs::create_dir_all(crate::images::folder_of(&path)).unwrap();
        fs::write(crate::images::folder_of(&path).join("a.png"), b"png").unwrap();
        let before = fs::read(&path).unwrap();
        let branch = fork(&path, 2, Some("a title (branch)")).unwrap();
        assert_ne!(branch, path);
        assert_eq!(fs::read(&path).unwrap(), before, "the original is only read");
        let (_, workspace, messages) = Session::open(&branch).unwrap();
        assert_eq!(workspace, Path::new("C:/w"));
        assert_eq!(messages.iter().map(|m| m.content.as_deref().unwrap()).collect::<Vec<_>>(), ["s", "look at this", "first answer"]);
        assert_eq!(fs::read(crate::images::folder_of(&branch).join("a.png")).unwrap(), b"png");
        assert!(tools_of(&branch).is_some());
        let listed = list(dir.path());
        assert!(listed.iter().any(|summary| summary.path == branch && summary.title == "a title (branch)"), "{listed:?}");
        assert!(fork(&path, 9, None).is_err());
    }

    #[test]
    fn a_session_comes_back_as_it_was_after_its_last_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("C:/w"), "now", "20261031-1").unwrap();
        session.append(&Message::text("system", "s")).unwrap();
        session.append(&Message::text("user", "old")).unwrap();
        session.snapshot(&[Message::text("system", "s"), Message::text("assistant", "sum")]).unwrap();
        session.append(&Message::text("user", "new")).unwrap();
        let path = session.path.clone();
        drop(session);
        let (mut again, workspace, messages) = Session::open(&path).unwrap();
        assert_eq!(workspace, Path::new("C:/w"));
        assert_eq!(messages.iter().map(|m| m.content.as_deref().unwrap()).collect::<Vec<_>>(), ["s", "sum", "new"]);
        again.append(&Message::text("assistant", "more")).unwrap();
        assert_eq!(Session::open(&path).unwrap().2.len(), 4);
    }

    /// A name given after a line a crash cut short is on a line of its own, and read; a session with such a line can still be branched.
    #[test]
    fn a_cut_line_takes_neither_the_name_after_it_nor_a_branch() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("C:/w"), "now", "20261104-1").unwrap();
        session.append(&Message::text("user", "hello")).unwrap();
        session.append(&Message::text("assistant", "hi")).unwrap();
        let path = session.path.clone();
        drop(session);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"kind\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":\"\xE3\x81").unwrap();
        drop(file);
        rename(&path, "named after the cut").unwrap();
        assert_eq!(list(dir.path())[0].title, "named after the cut");
        let copy = fork(&path, 1, None).unwrap();
        assert_eq!(Session::open(&copy).unwrap().2.len(), 2);
    }

    /// A call without its result gets one; a result whose call a crash cut out of the file, or one given twice, goes.
    #[test]
    fn every_call_has_one_result_and_every_result_its_call() {
        let call = |id: &str| crate::message::ToolCall { id: id.into(), kind: "function".into(), function: crate::message::FunctionCall { name: "read_file".into(), arguments: "{}".into() } };
        let asking = |ids: &[&str]| Message { role: "assistant".into(), tool_calls: Some(ids.iter().map(|id| call(id)).collect()), ..Message::default() };
        let mut messages = vec![
            Message::tool_result("lost", "its call is gone"),
            Message::text("user", "go"),
            Message::tool_result("cut", "its call was the line a crash cut"),
            asking(&["a", "b"]),
            Message::tool_result("a", "one"),
            Message::tool_result("a", "one again"),
            Message::text("user", "next"),
            asking(&["c"]),
        ];
        complete_tool_calls(&mut messages);
        let said: Vec<(String, Option<String>, Option<String>)> = messages.iter().map(|m| (m.role.clone(), m.tool_call_id.clone(), m.content.clone())).collect();
        let tool = |id: &str, text: &str| ("tool".to_string(), Some(id.to_string()), Some(text.to_string()));
        assert_eq!(said, [
            ("user".to_string(), None, Some("go".to_string())),
            ("assistant".to_string(), None, None),
            tool("a", "one"),
            tool("b", "interrupted: this call did not finish"),
            ("user".to_string(), None, Some("next".to_string())),
            ("assistant".to_string(), None, None),
            tool("c", "interrupted: this call did not finish"),
        ]);
    }

    #[test]
    fn a_list_of_tools_is_told_from_another_in_a_line() {
        let with = serde_json::json!([{"function": {"name": "execute_command", "parameters": {"properties": {"network": {}}}}}]);
        let without = serde_json::json!([{"function": {"name": "execute_command", "parameters": {"properties": {}}}}]);
        assert!(describe_tools(&with).ends_with("1 tools, network parameter: yes"), "{}", describe_tools(&with));
        assert!(describe_tools(&without).ends_with("network parameter: no"));
        assert_ne!(describe_tools(&with)[..12], describe_tools(&without)[..12]);
    }

    #[test]
    fn two_sessions_begun_in_one_second_have_a_file_each() {
        let dir = tempfile::tempdir().unwrap();
        let first = Session::create(dir.path(), Path::new("w"), "now", "same").unwrap();
        let second = Session::create(dir.path(), Path::new("w"), "now", "same").unwrap();
        assert_ne!(first.path, second.path);
    }

    #[test]
    fn the_tools_come_back_to_the_byte_and_the_last_ones_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("w"), "now", "1").unwrap();
        assert_eq!(tools_of(&session.path), None);
        let tools = serde_json::json!([{"zebra": 1, "apple": {"é": "日本語\n"}}]);
        session.set_tools(&serde_json::json!([])).unwrap();
        session.append(&Message::text("user", "a")).unwrap();
        session.set_tools(&tools).unwrap();
        session.snapshot(&[Message::text("user", "b")]).unwrap();
        assert_eq!(tools_of(&session.path).unwrap().to_string(), tools.to_string());
        let (_, _, messages) = Session::open(&session.path).unwrap();
        assert_eq!(messages.len(), 1, "a line of tools is no message");
    }

    #[test]
    fn a_call_cut_off_gets_a_result_and_a_cut_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("w"), "now", "a").unwrap();
        session.append(&Message { role: "assistant".into(), tool_calls: Some(vec![call("1"), call("2")]), ..Message::default() }).unwrap();
        session.append(&Message::tool_result("1", "done")).unwrap();
        let path = session.path.clone();
        drop(session);
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("{\"kind\":\"mess");
        fs::write(&path, text).unwrap();
        let messages = Session::open(&path).unwrap().2;
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("2"));
    }

    #[test]
    fn sessions_group_by_workspace_with_the_most_recent_first() {
        let summary = |workspace: &str, title: &str| Summary { path: PathBuf::new(), title: title.into(), workspace: workspace.into(), created: String::new(), messages: 1 };
        let groups = by_workspace(vec![summary("C:\\b", "newest"), summary("C:\\a", "middle"), summary("c:/B\\", "oldest")]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, Path::new("C:\\b"));
        assert_eq!(groups[0].1.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), ["newest", "oldest"]);
        assert_eq!(groups[1].1[0].title, "middle");
    }

    #[test]
    fn a_reader_beside_the_writer_sees_a_running_call_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("w"), "now", "1").unwrap();
        session.append(&Message { role: "assistant".into(), tool_calls: Some(vec![call("1")]), ..Message::default() }).unwrap();
        assert_eq!(read(&session.path).unwrap().len(), 1);
        session.append(&Message::tool_result("1", "done")).unwrap();
        assert_eq!(read(&session.path).unwrap().len(), 2);
    }

    #[test]
    fn a_search_finds_what_was_said_and_not_what_every_session_has() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = Session::create(dir.path(), Path::new("w"), "now", "1").unwrap();
        first.append(&Message::text("system", "the system prompt mentions Banana")).unwrap();
        first.append(&Message::text("user", crate::prompt::user_message("ハッシュテーブルの話", "now"))).unwrap();
        first.append(&Message::text("assistant", "A HashMap keeps Cherry in a bucket")).unwrap();
        first.append(&Message::tool_result("1", "tool output with Plum")).unwrap();
        let mut second = Session::create(dir.path(), Path::new("w"), "now", "2").unwrap();
        second.append(&Message::text("user", "plain words")).unwrap();
        second.append(&Message::text("assistant", "CHERRY again")).unwrap();
        let titles = |query: &str| search(dir.path(), query).into_iter().map(|hit| hit.title).collect::<Vec<_>>();
        assert_eq!(titles("cherry"), ["plain words", "ハッシュテーブルの話"]);
        assert_eq!(titles("テーブル"), ["ハッシュテーブルの話"]);
        assert_eq!(titles("cherry bucket"), ["ハッシュテーブルの話"]);
        assert!(titles("cherry nothing").is_empty());
        assert!(titles("banana").is_empty(), "the system prompt");
        assert!(titles("plum").is_empty(), "a tool's output");
        assert!(titles("active").is_empty(), "the wrapper of a message");
        assert!(titles("   ").is_empty());
        let hit = search(dir.path(), "BUCKET").remove(0);
        assert!(hit.snippet.contains("bucket"), "{}", hit.snippet);
    }

    #[test]
    fn a_snippet_stays_on_character_boundaries_and_on_one_line() {
        let long = format!("{}\n見つけた言葉{}", "前の文章".repeat(30), "後の文章".repeat(30));
        let at = long.find("見つけた").unwrap();
        let shown = snippet(&long, at);
        assert!(shown.starts_with('…') && shown.contains("見つけた") && !shown.contains('\n'), "{shown}");
        assert_eq!(snippet("短い", 0), "短い");
    }

    #[test]
    fn a_new_name_replaces_the_first_words_and_leaves_the_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("w"), "now", "1").unwrap();
        session.append(&Message::text("user", "first words")).unwrap();
        let path = session.path.clone();
        rename(&path, "my name").unwrap();
        session.append(&Message::text("assistant", "a")).unwrap();
        let summary = list(dir.path()).remove(0);
        assert_eq!((summary.title.as_str(), summary.messages), ("my name", 2));
        assert_eq!(Session::open(&path).unwrap().2.len(), 2);
    }

    #[test]
    fn the_list_counts_every_message_after_the_title() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("w"), "now", "1").unwrap();
        session.append(&Message::text("system", "s")).unwrap();
        session.append(&Message::text("user", "title")).unwrap();
        session.append(&Message::text("assistant", "a")).unwrap();
        session.snapshot(&[Message::text("system", "s")]).unwrap();
        session.append(&Message::text("user", "b")).unwrap();
        assert_eq!(list(dir.path())[0].messages, 4);
    }

    #[test]
    fn the_list_is_newest_first_and_titled_by_what_was_typed() {
        let dir = tempfile::tempdir().unwrap();
        for (stamp, text) in [("1", "first"), ("2", "second")] {
            let mut session = Session::create(dir.path(), Path::new("w"), "now", stamp).unwrap();
            session.append(&Message::text("user", crate::prompt::user_message(text, "now"))).unwrap();
        }
        Session::create(dir.path(), Path::new("w"), "now", "3").unwrap();
        let titles: Vec<String> = list(dir.path()).into_iter().map(|summary| summary.title).collect();
        assert_eq!(titles, ["second", "first"]);
    }
}

#[cfg(test)]
mod crash_tests {
    use super::*;

    #[test]
    fn a_file_cut_by_a_crash_opens_and_goes_on_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cut.jsonl");
        let whole = serde_json::to_string(&Line::Message { message: Message::text("user", "kept") }).unwrap();
        // A line cut in the middle of a character, with no newline after it.
        let mut bytes = format!("{whole}\n{{\"kind\":\"message\",\"message\":{{\"content\":\"").into_bytes();
        bytes.extend_from_slice(&[0xE3, 0x81]);
        std::fs::write(&path, bytes).unwrap();
        let (mut session, _, messages) = Session::open(&path).unwrap();
        assert_eq!(messages.len(), 1);
        session.append(&Message::text("user", "after")).unwrap();
        drop(session);
        let (_, _, messages) = Session::open(&path).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].content.as_deref(), Some("after"));
    }
}
