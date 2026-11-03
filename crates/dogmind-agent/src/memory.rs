//! What the agent keeps from one session to the next: short notes the model chooses to remember, one file for the person and one
//! for each project, in the app's own folder. A new session reads them. They are pointers, not proof, and the model is told so.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const MAX_NOTE: usize = 600;
const MAX_NOTES: usize = 200;
/// What the notes may take of the prompt.
const MAX_SECTION: usize = 8 * 1024;

fn project_key(workspace: &Path) -> String {
    let name = workspace.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    Sha256::digest(name.as_bytes()).iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

fn file_of(folder: &Path, workspace: &Path, scope: &str) -> Result<PathBuf, String> {
    match scope {
        "user" => Ok(folder.join("user.md")),
        "project" | "" => Ok(folder.join(format!("project-{}.md", project_key(workspace)))),
        other => Err(format!("{other}: scope is project or user")),
    }
}

fn notes_in(file: &Path) -> Vec<String> {
    std::fs::read_to_string(file).unwrap_or_default().lines().filter_map(|line| line.strip_prefix("- ")).map(str::to_string).collect()
}

fn write(file: &Path, notes: &[String]) -> Result<(), String> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let text: String = notes.iter().map(|note| format!("- {note}\n")).collect();
    let partial = file.with_extension("partial");
    std::fs::write(&partial, text).map_err(|error| format!("{}: {error}", partial.display()))?;
    std::fs::rename(&partial, file).map_err(|error| format!("{}: {error}", file.display()))
}

pub fn remember(folder: &Path, workspace: &Path, arguments: &Value) -> Result<String, String> {
    let note = arguments.get("note").and_then(Value::as_str).ok_or("note is required")?.split_whitespace().collect::<Vec<_>>().join(" ");
    if note.is_empty() {
        return Err("note is required".into());
    }
    if note.chars().count() > MAX_NOTE {
        return Err(format!("a note is at most {MAX_NOTE} characters: keep what a later session could not find out by looking"));
    }
    let scope = arguments.get("scope").and_then(Value::as_str).unwrap_or("project");
    let file = file_of(folder, workspace, scope)?;
    let mut notes = notes_in(&file);
    if notes.contains(&note) {
        return Ok("already remembered".to_string());
    }
    notes.push(note);
    let dropped = notes.len().saturating_sub(MAX_NOTES);
    notes.drain(..dropped);
    write(&file, &notes)?;
    Ok(format!("remembered, for {} sessions", if scope == "user" { "all" } else { "this project's" }))
}

pub fn forget(folder: &Path, workspace: &Path, arguments: &Value) -> Result<String, String> {
    let words = arguments.get("words").and_then(Value::as_str).ok_or("words is required")?.to_lowercase();
    if words.trim().is_empty() {
        return Err("words is required".into());
    }
    let mut gone = 0;
    for scope in ["project", "user"] {
        let file = file_of(folder, workspace, scope)?;
        let mut notes = notes_in(&file);
        let before = notes.len();
        notes.retain(|note| !note.to_lowercase().contains(&words));
        if notes.len() != before {
            gone += before - notes.len();
            write(&file, &notes)?;
        }
    }
    Ok(format!("{gone} notes forgotten"))
}

/// For the prompt of a new session, the newest kept if they do not all fit.
pub fn prompt_section(folder: &Path, workspace: &Path) -> String {
    let mut lines = Vec::new();
    let mut size = 0;
    for (scope, title) in [("project", "About this project"), ("user", "About the user")] {
        let Ok(file) = file_of(folder, workspace, scope) else { continue };
        let notes = notes_in(&file);
        if notes.is_empty() {
            continue;
        }
        lines.push(format!("{title}:"));
        for note in notes.iter().rev() {
            size += note.len() + 3;
            if size > MAX_SECTION {
                break;
            }
            lines.push(format!("- {note}"));
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    format!("## Memory\nNotes from earlier sessions: pointers, not proof, and they may be out of date. Check one against what is there now before relying on it.\n{}\n\n", lines.join("\n"))
}

pub fn schemas() -> Value {
    json!([
        {"type": "function", "function": {"name": "remember", "description": "Keep a note for later sessions: what they would want and could not find by looking, such as the user's preferences and decisions. Not what the code or git says, nor this task's progress.", "parameters": {"type": "object", "properties": {
            "note": {"type": "string", "description": "One fact, standing alone."},
            "scope": {"type": "string", "enum": ["project", "user"], "description": "Default project: this workspace's sessions. user: every session."}}, "required": ["note"]}}},
        {"type": "function", "function": {"name": "forget", "description": "Drop the notes holding these words, when one turns out wrong.", "parameters": {"type": "object", "properties": {
            "words": {"type": "string"}}, "required": ["words"]}}}
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_reaches_later_sessions_of_its_scope() {
        let dir = tempfile::tempdir().unwrap();
        let (one, two) = (Path::new("C:/w/one"), Path::new("C:/w/two"));
        remember(dir.path(), one, &json!({"note": "builds with  make\nnot cargo"})).unwrap();
        remember(dir.path(), one, &json!({"note": "prefers short answers", "scope": "user"})).unwrap();
        let here = prompt_section(dir.path(), one);
        assert!(here.contains("- builds with make not cargo") && here.contains("- prefers short answers") && here.contains("pointers, not proof"), "{here}");
        let there = prompt_section(dir.path(), two);
        assert!(!there.contains("builds with") && there.contains("prefers short answers"), "{there}");
        assert!(prompt_section(dir.path(), Path::new("c:\\W\\ONE\\")).contains("builds with"), "the same folder, however it is written");
    }

    #[test]
    fn notes_are_kept_small_and_can_go() {
        let dir = tempfile::tempdir().unwrap();
        let root = Path::new("w");
        assert!(remember(dir.path(), root, &json!({"note": "x".repeat(601)})).is_err());
        assert!(remember(dir.path(), root, &json!({"note": "  "})).is_err());
        assert!(remember(dir.path(), root, &json!({"note": "a", "scope": "world"})).is_err());
        remember(dir.path(), root, &json!({"note": "Keep this"})).unwrap();
        assert_eq!(remember(dir.path(), root, &json!({"note": "Keep this"})).unwrap(), "already remembered");
        remember(dir.path(), root, &json!({"note": "Drop this"})).unwrap();
        assert_eq!(forget(dir.path(), root, &json!({"words": "drop"})).unwrap(), "1 notes forgotten");
        let section = prompt_section(dir.path(), root);
        assert!(section.contains("Keep this") && !section.contains("Drop this"));
    }

    #[test]
    fn with_no_notes_yet_the_prompt_says_nothing_and_the_tool_says_how_to_use_it() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(prompt_section(dir.path(), Path::new("w")), "", "a section of no notes is read by every session for nothing");
        assert!(schemas().to_string().contains("could not find by looking"));
    }
}
