//! Skills: a folder with a `SKILL.md`, YAML frontmatter and then instructions, as Claude Code and Codex read them, so that a
//! skill written for either works here unchanged. The model sees each one's name and description and reads the rest when it
//! needs it; the person can also ask for one by name, `/name what to do`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

const MAX_DEPTH: usize = 6;
const MAX_SKILLS: usize = 200;
const MAX_BODY: usize = 48 * 1024;
/// What the list may take of the prompt: it is there for every request of the session.
const MAX_LIST: usize = 1400;
/// (a local model reads the list in every new session: 32 skills from other tools were 1,000 tokens of a 7,700-token prefix, and 200 tokens a second is what it reads at)
const MAX_DESCRIPTION: usize = 60;
/// Under a project and under the home alike. Claude's and the shared form come first, then Codex's, then Dogmind's own.
const FOLDERS: [&str; 4] = [".claude/skills", ".agents/skills", ".codex/skills", ".dogmind/skills"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub folder: PathBuf,
    /// `project` or `user`.
    pub scope: &'static str,
    /// `disable-model-invocation: true`: only the person starts it.
    pub for_the_model: bool,
    /// `user-invocable: false`: only the model starts it.
    pub for_the_person: bool,
}

/// The frontmatter's top-level words: plain, quoted, or a block (`>` or `|`). Nothing else of YAML is needed here, and anything
/// else in it is left alone rather than refused, as the other readers of these files have fields this one does not know.
pub fn frontmatter(text: &str) -> (Vec<(String, String)>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else { return (Vec::new(), text) };
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut at = 0;
    let mut block: Option<(String, bool, Vec<String>)> = None;
    let flush = |block: &mut Option<(String, bool, Vec<String>)>, fields: &mut Vec<(String, String)>| {
        if let Some((name, folded, lines)) = block.take() {
            let joined = lines.iter().map(|line| line.trim()).collect::<Vec<_>>().join(if folded { " " } else { "\n" });
            fields.push((name, joined.trim().to_string()));
        }
    };
    for line in rest.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\r', '\n']);
        at += line.len();
        if bare.trim_end() == "---" {
            flush(&mut block, &mut fields);
            return (fields, rest[at..].trim_start_matches(['\r', '\n']));
        }
        if block.is_some() && (bare.starts_with([' ', '\t']) || bare.trim().is_empty()) {
            if let Some((_, _, lines)) = &mut block {
                lines.push(bare.to_string());
            }
            continue;
        }
        flush(&mut block, &mut fields);
        if bare.starts_with([' ', '\t', '#', '-']) {
            continue;
        }
        let Some((name, value)) = bare.split_once(':') else { continue };
        let value = value.trim();
        if value.starts_with('>') || value.starts_with('|') {
            block = Some((name.trim().to_string(), value.starts_with('>'), Vec::new()));
            continue;
        }
        let unquoted = if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
            value[1..value.len() - 1].replace("\\\"", "\"").replace("\\n", "\n").replace("\\\\", "\\")
        } else if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
            value[1..value.len() - 1].replace("''", "'")
        } else {
            value.to_string()
        };
        fields.push((name.trim().to_string(), unquoted));
    }
    // A frontmatter that never ends is not one.
    (Vec::new(), text)
}

fn read_skill(file: &Path, scope: &'static str) -> Option<Skill> {
    let text = std::fs::read_to_string(file).ok()?;
    let (fields, _) = frontmatter(&text);
    let field = |name: &str| fields.iter().find(|(known, _)| known == name).map(|(_, value)| value.as_str());
    let description = field("description").filter(|description| !description.trim().is_empty())?.trim().to_string();
    let folder = file.parent()?.to_path_buf();
    let name = field("name").map(str::trim).filter(|name| !name.is_empty()).map(str::to_string)
        .or_else(|| folder.file_name().map(|name| name.to_string_lossy().into_owned()))?;
    let yes = |name: &str, default: bool| field(name).map_or(default, |value| value.trim().eq_ignore_ascii_case("true"));
    Some(Skill { name, description, folder, scope, for_the_model: !yes("disable-model-invocation", false), for_the_person: yes("user-invocable", true) })
}

fn walk(folder: &Path, depth: usize, scope: &'static str, found: &mut Vec<Skill>) {
    if depth > MAX_DEPTH || found.len() >= MAX_SKILLS {
        return;
    }
    let file = folder.join("SKILL.md");
    if file.is_file() {
        found.extend(read_skill(&file, scope));
        return;
    }
    let mut folders: Vec<PathBuf> = std::fs::read_dir(folder).into_iter().flatten().flatten().map(|entry| entry.path()).filter(|path| path.is_dir()).collect();
    folders.sort();
    for inner in folders {
        walk(&inner, depth + 1, scope, found);
    }
}

/// With this system's separators, as the model reads these paths and repeats them.
fn under(folder: &Path, place: &str) -> PathBuf {
    place.split('/').fold(folder.to_path_buf(), |path, part| path.join(part))
}

/// A project's skills, from the workspace up to its root, and then the person's. The first of a name wins: the project's over
/// the person's, and the nearer folder's over the farther.
pub fn discover(workspace: &Path, home: Option<&Path>) -> Vec<Skill> {
    let mut found = Vec::new();
    let root = workspace.ancestors().find(|folder| folder.join(".git").exists()).unwrap_or(workspace);
    for folder in workspace.ancestors().take_while(|folder| folder.starts_with(root)) {
        // The home is the person's, not a project, when it is the workspace.
        if Some(folder) == home {
            continue;
        }
        for place in FOLDERS {
            walk(&under(folder, place), 0, "project", &mut found);
        }
    }
    if let Some(home) = home {
        for place in FOLDERS {
            walk(&under(home, place), 0, "user", &mut found);
        }
    }
    let mut names = HashSet::new();
    found.retain(|skill| names.insert(skill.name.to_lowercase()));
    found
}

/// Without the ones the person turned off, by name in any case: the same name in a project and in the home is one skill to them.
pub fn without(mut skills: Vec<Skill>, off: &[String]) -> Vec<Skill> {
    skills.retain(|skill| !off.iter().any(|name| name.eq_ignore_ascii_case(&skill.name)));
    skills
}

/// Whether a skill says by name that it is for tools this session has not got, as Claude's for its browser and desktop do ("whose tools are named mcp__..."). Offered, one that begins "Read this skill
/// before the first step that uses..." drew a real model in three real jobs, and one read all 6 KB of it to no use. A person can still ask for it by name.
fn for_tools_not_here(skill: &Skill, tools: &[String]) -> bool {
    let named: Vec<&str> = skill.description.match_indices("mcp__").map(|(at, _)| skill.description[at + 5..].split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-')).next().unwrap_or("").trim_end_matches('_')).collect();
    !named.is_empty() && !named.iter().any(|server| tools.iter().any(|tool| tool.strip_prefix("mcp__").is_some_and(|rest| rest.starts_with(server))))
}

/// What the model is told of them, every request: enough to know when one applies. `tools`: what this session has, by name.
pub fn prompt_section(skills: &[Skill], tools: &[String]) -> String {
    let (mut list, mut more) = (String::new(), 0);
    for skill in skills.iter().filter(|skill| skill.for_the_model && !for_tools_not_here(skill, tools)) {
        let about: String = skill.description.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_DESCRIPTION).collect();
        let line = format!("- {}: {about}\n", skill.name);
        if list.len() + line.len() > MAX_LIST {
            more += 1;
            continue;
        }
        list.push_str(&line);
    }
    if list.is_empty() {
        return String::new();
    }
    let more = if more > 0 { format!("And {more} more: `skill` with the name `list` lists them all.\n") } else { String::new() };
    format!("## Skills\nInstructions for particular kinds of task. Most messages need none, nor does anything you already know how to do with the tools here, and one says nothing about this conversation: a file it mentions has not been given to you. When one is needed, call `skill` before you start and follow it, using what tools you have to the same end.\n{list}{more}\n")
}

/// All of them, for what the prompt had no room for.
pub fn listing(skills: &[Skill]) -> String {
    skills.iter().filter(|skill| skill.for_the_model).map(|skill| format!("- {}: {}\n", skill.name, skill.description.split_whitespace().collect::<Vec<_>>().join(" "))).collect()
}

/// A skill's instructions, as the model reads them: where it is, as what it names is relative to that.
pub fn instructions(skill: &Skill) -> Result<String, String> {
    let file = skill.folder.join("SKILL.md");
    let text = std::fs::read_to_string(&file).map_err(|error| format!("{}: {error}", file.display()))?;
    let (_, body) = frontmatter(&text);
    let body: String = if body.len() > MAX_BODY {
        let mut cut = MAX_BODY;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n--- truncated: read {} for the rest ---", &body[..cut], file.display())
    } else {
        body.to_string()
    };
    let files: Vec<String> = std::fs::read_dir(&skill.folder).into_iter().flatten().flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned()).filter(|name| name != "SKILL.md").collect();
    let beside = if files.is_empty() { String::new() } else { format!(" Beside it: {}.", files.join(", ")) };
    Ok(format!("Skill `{}`. Its folder is {}, which paths in it are relative to; read_file and commands can read there.{beside}\n\n{body}", skill.name, skill.folder.display()))
}

/// `$ARGUMENTS` where a skill asks for it, and otherwise after its text.
pub fn with_arguments(instructions: &str, arguments: &str) -> String {
    if instructions.contains("$ARGUMENTS") {
        return instructions.replace("$ARGUMENTS", arguments);
    }
    if arguments.is_empty() {
        return instructions.to_string();
    }
    format!("{instructions}\n\nARGUMENTS: {arguments}")
}

pub fn schema() -> Value {
    json!({"type": "function", "function": {"name": "skill", "description": "Read a skill's instructions, by its name in the Skills list.", "parameters": {"type": "object", "properties": {
        "name": {"type": "string"}}, "required": ["name"]}}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(folder: &Path, text: &str) {
        std::fs::create_dir_all(folder).unwrap();
        std::fs::write(folder.join("SKILL.md"), text).unwrap();
    }

    #[test]
    fn the_frontmatter_of_either_tool_is_read() {
        let claude = "---\nname: pdf\ndescription: \"Reads \\\"PDF\\\" files: all of them\"\nlicense: Proprietary. LICENSE.txt has complete terms\nallowed-tools: Read, Write\n---\n\n# PDF\n\nBody here.\n";
        let (fields, body) = frontmatter(claude);
        assert_eq!(fields[0], ("name".to_string(), "pdf".to_string()));
        assert_eq!(fields[1].1, "Reads \"PDF\" files: all of them");
        assert_eq!(body, "# PDF\n\nBody here.\n");
        let codex = "---\r\nname: review\r\ndescription: >\r\n  Reviews code,\r\n  carefully.\r\nmetadata:\r\n  short-description: Review\r\n---\r\nBody";
        let (fields, body) = frontmatter(codex);
        assert_eq!(fields.iter().find(|(name, _)| name == "description").unwrap().1, "Reviews code, carefully.");
        assert!(!fields.iter().any(|(name, _)| name == "short-description"), "nested, not top-level");
        assert_eq!(body, "Body");
        let (fields, body) = frontmatter("---\nname: x\ndescription: |\n  one\n  two\n---\nb");
        assert_eq!(fields[1].1, "one\ntwo");
        assert_eq!(body, "b");
        assert_eq!(frontmatter("no frontmatter").1, "no frontmatter");
        assert!(frontmatter("---\nname: never closed\n").0.is_empty());
    }

    #[test]
    fn skills_are_found_where_each_tool_keeps_them() {
        let dir = tempfile::tempdir().unwrap();
        let (home, work) = (dir.path().join("me"), dir.path().join("work"));
        std::fs::create_dir_all(work.join(".git")).unwrap();
        write(&home.join(".claude/skills/one"), "---\ndescription: from Claude\n---\nb");
        write(&home.join(".claude/skills/synced/abc/deep"), "---\nname: deep\ndescription: nested\n---\nb");
        write(&home.join(".codex/skills/.system/two"), "---\nname: two\ndescription: from Codex\n---\nb");
        write(&home.join(".agents/skills/three"), "---\nname: three\ndescription: shared\n---\nb");
        write(&home.join(".claude/skills/nameless"), "---\nname: nothing-said\n---\nb");
        write(&work.join(".claude/skills/one"), "---\nname: one\ndescription: the project's own\n---\nb");
        let skills = discover(&work, Some(&home));
        let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
        for name in ["one", "deep", "two", "three"] {
            assert!(names.contains(&name), "{names:?}");
        }
        assert!(!names.contains(&"nothing-said"), "no description, nothing to choose it by");
        let one = skills.iter().find(|skill| skill.name == "one").unwrap();
        assert_eq!((one.description.as_str(), one.scope), ("the project's own", "project"));
        assert_eq!(names.iter().filter(|name| **name == "one").count(), 1);
    }

    #[test]
    fn the_home_as_workspace_is_still_the_persons() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("me");
        write(&home.join(".claude/skills/one"), "---\ndescription: mine\n---\nb");
        assert_eq!(discover(&home, Some(&home))[0].scope, "user");
    }

    #[test]
    fn who_may_start_a_skill_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".claude/skills/manual"), "---\nname: manual\ndescription: d\ndisable-model-invocation: true\n---\nb");
        write(&dir.path().join(".claude/skills/hidden"), "---\nname: hidden\ndescription: d\nuser-invocable: false\n---\nb");
        let skills = discover(dir.path(), None);
        let section = prompt_section(&skills, &[]);
        assert!(!section.contains("manual") && section.contains("hidden"), "{section}");
        assert!(skills.iter().find(|skill| skill.name == "manual").unwrap().for_the_person);
        assert!(!skills.iter().find(|skill| skill.name == "hidden").unwrap().for_the_person);
    }

    #[test]
    fn instructions_say_where_they_are_and_take_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join(".claude/skills/pdf");
        write(&folder, "---\nname: pdf\ndescription: d\n---\nRun scripts/x.py on $ARGUMENTS.");
        std::fs::write(folder.join("reference.md"), "r").unwrap();
        let skill = discover(dir.path(), None).remove(0);
        let text = instructions(&skill).unwrap();
        assert!(text.contains(&under(dir.path(), ".claude/skills").join("pdf").display().to_string()) && text.contains("reference.md") && !text.contains("description: d"), "{text}");
        assert!(with_arguments(&text, "a.pdf").ends_with("Run scripts/x.py on a.pdf."));
        assert_eq!(with_arguments("plain", "a"), "plain\n\nARGUMENTS: a");
        assert_eq!(with_arguments("plain", ""), "plain");
    }

    #[test]
    fn the_list_stays_within_what_a_prompt_can_carry() {
        let skills: Vec<Skill> = (0..150).map(|at| Skill { name: format!("s{at}"), description: "word ".repeat(200), folder: PathBuf::new(), scope: "user", for_the_model: true, for_the_person: true }).collect();
        assert!(prompt_section(&skills, &[]).len() < MAX_LIST + 1000);
        assert_eq!(prompt_section(&[], &[]), "");
    }

    #[test]
    fn a_skill_for_tools_this_session_has_not_got_is_not_offered() {
        let skill = |name: &str, description: &str| Skill { name: name.into(), description: description.into(), folder: PathBuf::new(), scope: "user", for_the_model: true, for_the_person: true };
        let skills = [
            skill("built-in-browser", "Read this skill before the first step that uses the pane, whose tools are named mcp__Claude_Browser__* in the app and mcp__remote-devices__Claude_Browser__* when linked."),
            skill("chrome-browser", "Before Claude in Chrome, whose tools are named mcp__claude-in-chrome__*."),
            skill("xlsx", "Use this skill any time a spreadsheet file is the input."),
        ];
        let section = prompt_section(&skills, &[]);
        assert!(section.contains("xlsx") && !section.contains("built-in-browser") && !section.contains("chrome-browser"), "{section}");
        let with_chrome = prompt_section(&skills, &["mcp__claude-in-chrome__navigate".to_string()]);
        assert!(with_chrome.contains("chrome-browser") && !with_chrome.contains("built-in-browser"), "{with_chrome}");
        assert!(listing(&skills).contains("chrome-browser"), "named, or listed, it is still there");
    }

    #[test]
    fn a_skill_turned_off_is_gone_whatever_its_case() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".claude/skills/pdf"), "---\nname: PDF\ndescription: d\n---\nb");
        write(&dir.path().join(".claude/skills/xlsx"), "---\nname: xlsx\ndescription: d\n---\nb");
        let left = without(discover(dir.path(), None), &["pdf".to_string()]);
        assert_eq!(left.iter().map(|skill| skill.name.as_str()).collect::<Vec<_>>(), ["xlsx"]);
        assert_eq!(without(discover(dir.path(), None), &[]).len(), 2);
    }

    #[test]
    fn the_list_says_it_is_no_word_about_this_conversation() {
        let skill = Skill { name: "imagegen".into(), description: "Edits images".into(), folder: PathBuf::new(), scope: "user", for_the_model: true, for_the_person: true };
        assert!(prompt_section(&[skill], &[]).contains("says nothing about this conversation"));
    }

    #[test]
    fn a_long_list_stays_short_in_the_prompt_and_says_how_to_read_it_all() {
        let skill = |at: usize| Skill { name: format!("skill-{at}"), description: "Does something at great length, ".repeat(10), folder: PathBuf::new(), scope: "user", for_the_model: true, for_the_person: true };
        let all: Vec<Skill> = (0..40).map(skill).collect();
        let section = prompt_section(&all, &[]);
        assert!(section.len() < MAX_LIST + 500, "{}", section.len());
        assert!(section.contains("`list`") && section.contains("more:"), "{section}");
        assert!(listing(&all).contains("skill-39") && !section.contains("skill-39"));
        assert!(!prompt_section(&all[..2], &[]).contains("`list`"), "nothing left out, nothing to say");
    }
}
