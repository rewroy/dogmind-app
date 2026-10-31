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
        let path = folder.join(format!("{stamp}.jsonl"));
        let file = OpenOptions::new().create(true).append(true).open(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let mut session = Session { path, file };
        session.write(&Line::Meta { workspace: workspace.to_path_buf(), created: now.to_string() })?;
        Ok(session)
    }

    /// The session's messages, and it ready to append to. A tool call left without its result (the app was closed
    /// during the call) gets one, as the server refuses a conversation without.
    pub fn open(path: &Path) -> Result<(Session, PathBuf, Vec<Message>), String> {
        let reader = BufReader::new(File::open(path).map_err(|error| format!("{}: {error}", path.display()))?);
        let (mut workspace, mut messages) = (PathBuf::new(), Vec::new());
        for line in reader.lines() {
            let line = line.map_err(|error| error.to_string())?;
            match serde_json::from_str(&line) {
                Ok(Line::Meta { workspace: place, .. }) => workspace = place,
                Ok(Line::Message { message }) => messages.push(message),
                Ok(Line::Snapshot { messages: all }) => messages = all,
                Ok(Line::Title { .. } | Line::WebSeen) => {}
                Err(_) => {} // a line cut short by a crash
            }
        }
        complete_tool_calls(&mut messages);
        let file = OpenOptions::new().append(true).open(path).map_err(|error| format!("{}: {error}", path.display()))?;
        Ok((Session { path: path.to_path_buf(), file }, workspace, messages))
    }

    fn write(&mut self, line: &Line) -> Result<(), String> {
        let text = serde_json::to_string(line).map_err(|error| error.to_string())?;
        writeln!(self.file, "{text}").map_err(|error| format!("{}: {error}", self.path.display()))
    }

    pub fn append(&mut self, message: &Message) -> Result<(), String> {
        self.write(&Line::Message { message: message.clone() })
    }

    pub fn mark_web_seen(&mut self) -> Result<(), String> {
        self.write(&Line::WebSeen)
    }

    pub fn snapshot(&mut self, messages: &[Message]) -> Result<(), String> {
        self.write(&Line::Snapshot { messages: messages.to_vec() })
    }
}

pub fn web_seen(path: &Path) -> bool {
    let marked = serde_json::to_string(&Line::WebSeen).expect("a plain line serializes");
    File::open(path).map(BufReader::new).is_ok_and(|reader| reader.lines().map_while(Result::ok).any(|line| line == marked))
}

/// A session's messages as they are on disk, for a reader beside the agent that writes them: a call still running has
/// no result yet, and none is made up.
pub fn read(path: &Path) -> Result<Vec<Message>, String> {
    let reader = BufReader::new(File::open(path).map_err(|error| format!("{}: {error}", path.display()))?);
    let mut messages = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
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
    let mut file = OpenOptions::new().append(true).open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    writeln!(file, "{text}").map_err(|error| format!("{}: {error}", path.display()))
}

/// Stub results for calls that have none, right after their assistant message.
pub fn complete_tool_calls(messages: &mut Vec<Message>) {
    let mut at = 0;
    while at < messages.len() {
        let calls: Vec<String> = messages[at].tool_calls.iter().flatten().map(|call| call.id.clone()).collect();
        at += 1;
        let mut answered = Vec::new();
        while at < messages.len() && messages[at].role == "tool" {
            answered.extend(messages[at].tool_call_id.clone());
            at += 1;
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
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl")).collect();
    paths.sort();
    paths.reverse();
    paths.into_iter().filter_map(|path| {
        let (mut workspace, mut created, mut title, mut messages, mut named) = (PathBuf::new(), String::new(), String::new(), 0, None);
        for line in BufReader::new(File::open(&path).ok()?).lines().map_while(Result::ok) {
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
        first.append(&Message::text("user", crate::prompt::user_message("ハッシュテーブルの話", Path::new("Active"), "now"))).unwrap();
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
            session.append(&Message::text("user", crate::prompt::user_message(text, Path::new("w"), "now"))).unwrap();
        }
        Session::create(dir.path(), Path::new("w"), "now", "3").unwrap();
        let titles: Vec<String> = list(dir.path()).into_iter().map(|summary| summary.title).collect();
        assert_eq!(titles, ["second", "first"]);
    }
}
