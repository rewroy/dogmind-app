//! What the person has said may always run in a workspace. The file is in the app's own folder, never in the workspace,
//! where the model can write. A rule only answers what would ask: what is denied stays denied, and high risk, secrets,
//! the network and paths outside the workspace ask every time, whatever a rule says.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::safety::{command_words, Gate, Risk};

/// Programs whose second word can be any code at all: a rule for them would allow anything.
const INTERPRETERS: &[&str] = &["python", "python3", "py", "node", "deno", "bun", "ruby", "perl", "php", "lua", "cmd", "powershell", "pwsh", "bash", "sh", "wsl", "npx", "uvx", "cscript", "wscript", "mshta", "rundll32", "regsvr32"];
const EDIT: &str = "edit";
const COMMAND: &str = "execute_command";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub workspace: PathBuf,
    pub tool: String,
    /// The words a command starts with; empty for edits.
    #[serde(default)]
    pub prefix: String,
    /// The whole command, not its start.
    #[serde(default)]
    pub exact: bool,
}

impl Rule {
    /// As the person reads it.
    pub fn label(&self) -> String {
        match (self.tool.as_str(), self.exact) {
            (EDIT, _) => "Edits to files in this folder".to_string(),
            (_, true) => self.prefix.clone(),
            _ => format!("{} ...", self.prefix),
        }
    }
}

// One writer at a time: every session's agent and the window share the file.
static FILE: Mutex<()> = Mutex::new(());

fn same_workspace(a: &Path, b: &Path) -> bool {
    let key = |path: &Path| path.to_string_lossy().replace('\\', "/").trim_end_matches('/').to_lowercase();
    key(a) == key(b)
}

fn family(tool: &str) -> Option<&'static str> {
    match tool {
        "create_file" | "apply_patch" => Some(EDIT),
        COMMAND => Some(COMMAND),
        _ => None,
    }
}

/// A word as a rule keeps it: one that holds a space is quoted again, so that `del "a  b.txt"` is not `del "a b.txt"`, another file. The words of `"python" -c x` and `python -c x` are the same.
fn written(word: &str) -> String {
    if word.is_empty() || word.contains([' ', '\t']) { format!("\"{word}\"") } else { word.to_string() }
}

fn joined(words: &[String]) -> String {
    words.iter().map(|word| written(word)).collect::<Vec<_>>().join(" ")
}

/// The program a command's first word names: its folder taken off, in lower case, and without the extension that cmd would add.
fn program_of(first: &str) -> String {
    let name = first.rsplit(['\\', '/']).next().unwrap_or(first).to_lowercase();
    [".exe", ".cmd", ".bat", ".com"].iter().find_map(|extension| name.strip_suffix(extension)).unwrap_or(&name).to_string()
}

/// A second line, or cmd's escape, makes a command more than its first words say.
fn plain(gate: &Gate, command: &str) -> bool {
    !gate.is_compound(command) && !command.contains(['\n', '\r', '^'])
}

/// The rule to offer for a call, or none: then only this exact call can be allowed, for this session.
pub fn proposed(gate: &Gate, workspace: &Path, tool: &str, arguments: &Value, risk: Risk) -> Option<Rule> {
    let family = family(tool)?;
    if family == EDIT {
        return Some(Rule { workspace: workspace.to_path_buf(), tool: EDIT.to_string(), prefix: String::new(), exact: false });
    }
    let command = arguments.get("command")?.as_str()?;
    if !plain(gate, command) {
        return None;
    }
    let all = command_words(command);
    let mut parts = all.iter();
    let first = parts.next().filter(|first| !first.is_empty())?;
    // What deletes, moves or stops things, and what runs code it was handed, is allowed to the letter or not at all.
    let exact = risk >= Risk::Medium || INTERPRETERS.contains(&program_of(first).as_str());
    let prefix = if exact {
        joined(&all)
    } else {
        match parts.next().filter(|second| !second.starts_with(['-', '/'])) {
            Some(second) => joined(&[first.clone(), second.clone()]),
            None => joined(std::slice::from_ref(first)),
        }
    };
    Some(Rule { workspace: workspace.to_path_buf(), tool: COMMAND.to_string(), prefix, exact })
}

fn matches(rule: &Rule, gate: &Gate, workspace: &Path, tool: &str, arguments: &Value) -> bool {
    if !same_workspace(&rule.workspace, workspace) || family(tool) != Some(rule.tool.as_str()) {
        return false;
    }
    if rule.tool == EDIT {
        return true;
    }
    let Some(command) = arguments.get("command").and_then(Value::as_str) else { return false };
    if !plain(gate, command) {
        return false;
    }
    let all = joined(&command_words(command));
    all == rule.prefix || (!rule.exact && all.starts_with(&format!("{} ", rule.prefix)))
}

fn read(path: &Path) -> Vec<Rule> {
    std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

/// The rules, to change them. A file there that does not read as rules is moved aside first, to `NAME.unreadable-TIME.json`: taken for
/// none, the next rule written took the place of all of them.
fn read_to_change(path: &Path) -> Vec<Rule> {
    let Ok(bytes) = std::fs::read(path) else { return Vec::new() };
    match serde_json::from_slice(&bytes) {
        Ok(rules) => rules,
        Err(_) => {
            let stem = path.file_stem().map_or(String::new(), |stem| stem.to_string_lossy().into_owned());
            let _ = std::fs::rename(path, path.with_file_name(format!("{stem}.unreadable-{}.json", chrono::Local::now().format("%Y%m%d-%H%M%S"))));
            Vec::new()
        }
    }
}

fn write(path: &Path, rules: &[Rule]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(rules).map_err(|error| error.to_string())?;
    let partial = path.with_extension("partial");
    // (on the disk before it takes the place: after a power cut, a rename that got there without its bytes leaves the file empty)
    std::fs::File::create(&partial).and_then(|mut out| {
        std::io::Write::write_all(&mut out, text.as_bytes())?;
        out.sync_all()
    }).map_err(|error| format!("{}: {error}", partial.display()))?;
    std::fs::rename(&partial, path).map_err(|error| format!("{}: {error}", path.display()))
}

/// Read each time, so that a rule removed in the window stops at once in every session.
pub fn allows(path: &Path, gate: &Gate, workspace: &Path, tool: &str, arguments: &Value) -> bool {
    let _one = FILE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    read(path).iter().any(|rule| matches(rule, gate, workspace, tool, arguments))
}

pub fn list(path: &Path) -> Vec<Rule> {
    let _one = FILE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    read(path)
}

pub fn add(path: &Path, rule: Rule) -> Result<(), String> {
    let _one = FILE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut rules = read_to_change(path);
    if !rules.contains(&rule) {
        rules.push(rule);
        write(path, &rules)?;
    }
    Ok(())
}

pub fn remove(path: &Path, rule: &Rule) -> Result<(), String> {
    let _one = FILE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut rules = read_to_change(path);
    rules.retain(|known| known != rule);
    write(path, &rules)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn command(text: &str) -> Value {
        json!({ "command": text })
    }

    /// A rules file that does not read as rules is moved aside before the next rule is written, not written over with that one alone.
    #[test]
    fn rules_that_do_not_read_are_kept_aside_and_not_written_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        std::fs::write(&path, b"[{\"workspace\": \"C:\\\\w\", \"tool\"\0\0\0\0").unwrap();
        let rule = Rule { workspace: PathBuf::from("C:\\w"), tool: COMMAND.to_string(), prefix: "npm test".into(), exact: false };
        add(&path, rule.clone()).unwrap();
        assert_eq!(list(&path), [rule]);
        let aside: Vec<String> = std::fs::read_dir(dir.path()).unwrap().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).filter(|name| name.starts_with("permissions.unreadable-")).collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert!(std::fs::read(dir.path().join(&aside[0])).unwrap().starts_with(b"[{\"workspace\""), "what was there is kept");
    }

    /// A quoted program is the program, a quoted path with spaces in it is one word, and spaces inside quotes are not taken for gaps.
    #[test]
    fn what_is_quoted_is_read_as_cmd_reads_it() {
        let gate = Gate::new();
        let root = Path::new("C:\\w");
        for (text, prefix) in [
            ("\"python\" -c \"import os\"", "python -c \"import os\""),
            ("\"C:\\Program Files\\Python312\\python.exe\" -c x", "\"C:\\Program Files\\Python312\\python.exe\" -c x"),
            ("\"PYTHON.EXE\" x.py", "PYTHON.EXE x.py"),
            ("C:\\tools\\node.cmd x.js", "C:\\tools\\node.cmd x.js"),
        ] {
            let rule = proposed(&gate, root, COMMAND, &command(text), Risk::Low).unwrap();
            assert!(rule.exact, "{text}");
            assert_eq!(rule.prefix, prefix, "{text}");
            assert!(matches(&rule, &gate, root, COMMAND, &command(text)), "{text}");
            assert!(!matches(&rule, &gate, root, COMMAND, &command(&format!("{text} more"))), "{text}: to the letter");
        }
        let python = proposed(&gate, root, COMMAND, &command("\"python\" -c x"), Risk::Low).unwrap();
        assert!(!matches(&python, &gate, root, COMMAND, &command("\"python\" -c \"import shutil\"")));
        // A program in quotes is the program for a rule that is not to the letter as well.
        let cargo = proposed(&gate, root, COMMAND, &command("\"cargo\" build"), Risk::Low).unwrap();
        assert_eq!((cargo.prefix.as_str(), cargo.exact), ("cargo build", false));
        assert!(matches(&cargo, &gate, root, COMMAND, &command("cargo build --release")));
        assert!(!matches(&cargo, &gate, root, COMMAND, &command("cargo buildx")));
        // A rule saved in the form this read quotes in before asks again, and allows no more than it did.
        let old = Rule { workspace: root.to_path_buf(), tool: COMMAND.to_string(), prefix: "\"python\"".into(), exact: false };
        for text in ["python -c x", "\"python\" -c x"] {
            assert!(!matches(&old, &gate, root, COMMAND, &command(text)), "{text}");
        }
    }

    #[test]
    fn spaces_inside_quotes_are_kept_and_two_files_are_two_rules() {
        let gate = Gate::new();
        let root = Path::new("C:\\w");
        let rule = proposed(&gate, root, COMMAND, &command("del \"a  b.txt\""), Risk::Medium).unwrap();
        assert_eq!(rule.prefix, "del \"a  b.txt\"");
        assert!(matches(&rule, &gate, root, COMMAND, &command("del   \"a  b.txt\"")), "the gaps between words are not words");
        assert!(!matches(&rule, &gate, root, COMMAND, &command("del \"a b.txt\"")), "another file");
        assert!(!matches(&rule, &gate, root, COMMAND, &command("del a b.txt")), "two files");
        let two = proposed(&gate, root, COMMAND, &command("del a b.txt"), Risk::Medium).unwrap();
        assert!(!matches(&two, &gate, root, COMMAND, &command("del \"a b.txt\"")));
        assert_eq!(proposed(&gate, root, COMMAND, &command("del \"\""), Risk::Medium).unwrap().prefix, "del \"\"");
        assert!(proposed(&gate, root, COMMAND, &command("\"\" x"), Risk::Low).is_none(), "no program, no rule");
    }

    #[test]
    fn a_rule_is_the_first_words_of_an_ordinary_command() {
        let gate = Gate::new();
        let root = Path::new("C:\\w");
        let rule = proposed(&gate, root, COMMAND, &command("cargo  build --release"), Risk::Low).unwrap();
        assert_eq!((rule.prefix.as_str(), rule.exact), ("cargo build", false));
        assert_eq!(proposed(&gate, root, COMMAND, &command("make -j4"), Risk::Low).unwrap().prefix, "make");
    }

    #[test]
    fn what_deletes_or_runs_code_is_allowed_only_to_the_letter() {
        let gate = Gate::new();
        let root = Path::new("C:\\w");
        for text in ["del build.log", "rm -rf target"] {
            let rule = proposed(&gate, root, COMMAND, &command(text), Risk::Medium).unwrap();
            assert!(rule.exact, "{text}");
            assert_eq!(rule.prefix, text);
        }
        for text in ["python -c print(1)", "python.exe tool.py", "C:\\bin\\node.exe x.js", "wsl ls"] {
            let rule = proposed(&gate, root, COMMAND, &command(text), Risk::Low).unwrap();
            assert!(rule.exact, "{text}");
        }
    }

    #[test]
    fn a_compound_command_gets_no_rule_and_no_rule_reaches_it() {
        let gate = Gate::new();
        let root = Path::new("C:\\w");
        for text in ["cargo build && del x", "cargo build > out", "cargo build\ndel x", "cargo build ^\n& del x", "cargo build | sh", "cargo build `x`", "cargo build $(x)"] {
            assert!(proposed(&gate, root, COMMAND, &command(text), Risk::Low).is_none(), "{text}");
        }
        let rule = proposed(&gate, root, COMMAND, &command("cargo build"), Risk::Low).unwrap();
        assert!(matches(&rule, &gate, root, COMMAND, &command("cargo build --release")));
        for text in ["cargo build && del x", "cargo build\ndel x", "cargo buildx", "cargo"] {
            assert!(!matches(&rule, &gate, root, COMMAND, &command(text)), "{text}");
        }
    }

    #[test]
    fn a_rule_stays_in_its_workspace_and_its_tool() {
        let gate = Gate::new();
        let edits = proposed(&gate, Path::new("C:\\w"), "create_file", &json!({"path": "a"}), Risk::Low).unwrap();
        assert!(matches(&edits, &gate, Path::new("c:/W/"), "apply_patch", &json!({})));
        assert!(!matches(&edits, &gate, Path::new("C:\\other"), "apply_patch", &json!({})));
        assert!(!matches(&edits, &gate, Path::new("C:\\w"), COMMAND, &command("make")));
        assert!(proposed(&gate, Path::new("C:\\w"), "read_file", &json!({}), Risk::Medium).is_none());
    }

    #[test]
    fn an_exact_rule_is_not_a_prefix() {
        let gate = Gate::new();
        let root = Path::new("C:\\w");
        let rule = proposed(&gate, root, COMMAND, &command("del build.log"), Risk::Medium).unwrap();
        assert!(matches(&rule, &gate, root, COMMAND, &command("del  build.log")));
        assert!(!matches(&rule, &gate, root, COMMAND, &command("del build.log other.txt")));
    }

    #[test]
    fn rules_outlive_the_program_and_go_when_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep").join("permissions.json");
        let gate = Gate::new();
        let root = Path::new("C:\\w");
        assert!(!allows(&path, &gate, root, COMMAND, &command("make")));
        let rule = proposed(&gate, root, COMMAND, &command("make"), Risk::Low).unwrap();
        add(&path, rule.clone()).unwrap();
        add(&path, rule.clone()).unwrap();
        assert_eq!(list(&path).len(), 1);
        assert!(allows(&path, &gate, root, COMMAND, &command("make all")));
        remove(&path, &rule).unwrap();
        assert!(!allows(&path, &gate, root, COMMAND, &command("make all")));
    }

    #[test]
    fn a_damaged_file_allows_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(!allows(&path, &Gate::new(), Path::new("C:\\w"), COMMAND, &command("make")));
    }
}
