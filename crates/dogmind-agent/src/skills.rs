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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
    /// The servers whose tools its text names (`mcp__<server>__...`): as Dogmind names a tool, the first `__` after `mcp__` ends the server's part.
    pub mcp_servers: Vec<String>,
    /// Which of [`OTHER_APP`] it carries: it needs something of another app that this one has no counterpart for.
    pub other_app: Option<&'static str>,
}

/// What a skill that is written for another app's own features has in it. Calibrated on the fourteen skills a Claude account synced to a machine: the six general ones (documents, sheets,
/// slides, drawing, writing skills) carry none, and the eight that cannot work here carry at least one, or name tools of a server this app has not. Each is looked for where it is meant: in the
/// description, which is where a skill says what it needs before it is read, or in the whole text. `(in the description, the words, what they mean)`.
const OTHER_APP: [(bool, &str, &str); 4] = [
    // (the host's connector, which a call is made through)
    (true, "connector call", "a connector of another app"),
    (true, "-connector", "a connector of another app"),
    // (a skill that says it works by spawning agents of its own, and is for use only where there is a tool to spawn them)
    (true, "subagent", "subagents that it spawns"),
    // (the host's own scheduler: a morning brief that is set up as a recurring task)
    (false, "scheduled task", "another app's scheduled tasks"),
];

/// What a skill needs that is not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    /// The tools of a server this app has not configured, by the server's part of their names.
    Tools(String),
    /// Something of another app, in these words.
    Something(&'static str),
}

/// The servers a text names tools of: `mcp__<server>__<tool>`, where the server's part ends at the first `__` (a server called `Claude_Browser` has one single `_`). `mcp__` with no name after
/// it (a skill saying how such names are written) is no server.
fn servers_named(text: &str) -> Vec<String> {
    let mut servers: Vec<String> = Vec::new();
    for (at, _) in text.match_indices("mcp__") {
        let rest = &text[at + 5..];
        let run: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
        let server = run.split("__").next().unwrap_or("").trim_matches('_');
        if !server.is_empty() && !servers.iter().any(|known| known.eq_ignore_ascii_case(server)) {
            servers.push(server.to_string());
        }
    }
    servers
}

impl Skill {
    /// What it needs that this app does not have, if anything: tools of a server that is not configured (`servers`: the part of each configured server's tools' names), or a feature of the app it was
    /// written for. A skill that is turned on by the person anyway is theirs to run; this is for what is off until they say.
    pub fn needs_what_is_not_here(&self, servers: &[String]) -> Option<Missing> {
        if let Some(server) = self.mcp_servers.iter().find(|server| !servers.iter().any(|known| known.eq_ignore_ascii_case(server))) {
            return Some(Missing::Tools(server.clone()));
        }
        self.other_app.map(|what| Missing::Something(OTHER_APP.iter().find(|(_, _, meaning)| *meaning == what).map_or(what, |(_, _, meaning)| meaning)))
    }
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
    let (lower, lower_description) = (text.to_lowercase(), description.to_lowercase());
    let other_app = OTHER_APP.iter().find(|(in_description, words, _)| if *in_description { lower_description.contains(words) } else { lower.contains(words) }).map(|(_, _, meaning)| *meaning);
    Some(Skill { name, description, folder, scope, for_the_model: !yes("disable-model-invocation", false), for_the_person: yes("user-invocable", true), mcp_servers: servers_named(&text), other_app })
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
    // (not a hidden folder: `.system` is a tool's own, as Codex's are, written for its tools and its agents. Codex's review-agent, for one,
    // says to read every line itself and hand nothing on, and Dogmind's model, given it for a review, read 880 KB into one turn.)
    let mut folders: Vec<PathBuf> = std::fs::read_dir(folder).into_iter().flatten().flatten().map(|entry| entry.path()).filter(|path| path.is_dir() && !path.file_name().is_some_and(|name| name.to_string_lossy().starts_with('.'))).collect();
    folders.sort();
    for inner in folders {
        walk(&inner, depth + 1, scope, found);
    }
}

/// With this system's separators, as the model reads these paths and repeats them.
fn under(folder: &Path, place: &str) -> PathBuf {
    place.split('/').fold(folder.to_path_buf(), |path, part| path.join(part))
}

/// A project's skills, from the workspace up to its root, and then the person's, all of them, in the order that decides between two of a name: the project's before the person's, and the nearer
/// folder's before the farther. Which of two of a name is used depends on who starts it (see `the_model_may_start` and `one_each`): a project's `pdf` that only the person may start must not hide
/// the person's own that the model may.
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
    found
}

/// The first skill of each name that the model may start, in the order they were found: what its list and its `skill` tool have.
pub fn the_model_may_start(skills: &[Skill]) -> Vec<&Skill> {
    let mut names = HashSet::new();
    skills.iter().filter(|skill| skill.for_the_model && names.insert(skill.name.to_lowercase())).collect()
}

/// One skill of each name, for what the person sees of them: the first that the person may start as `/name`, or of a name that they may not start, the first. A switch to each is by its name.
pub fn one_each(skills: Vec<Skill>) -> Vec<Skill> {
    let mut chosen: Vec<Skill> = Vec::new();
    for skill in skills {
        match chosen.iter_mut().find(|known| known.name.eq_ignore_ascii_case(&skill.name)) {
            Some(known) if !known.for_the_person && skill.for_the_person => *known = skill,
            Some(_) => {}
            None => chosen.push(skill),
        }
    }
    chosen
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
    for skill in the_model_may_start(skills).into_iter().filter(|skill| !for_tools_not_here(skill, tools)) {
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
    the_model_may_start(skills).into_iter().map(|skill| format!("- {}: {}\n", skill.name, skill.description.split_whitespace().collect::<Vec<_>>().join(" "))).collect()
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

    fn found_as(text: &str) -> Skill {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a"), text);
        read_skill(&dir.path().join("a").join("SKILL.md"), "user").unwrap()
    }

    /// Skills written for another app's own tools are told from the general ones, by what they name and by what they say they need: stand-ins written for this test, one of each kind.
    #[test]
    fn a_skill_for_another_apps_features_is_told_from_a_general_one() {
        let general = found_as("---\nname: pdf\ndescription: Read, merge and split PDF files with the bundled Python.\n---\nUse pypdf. Write to out.pdf.\n");
        assert_eq!((general.mcp_servers.len(), general.other_app), (0, None));
        assert_eq!(general.needs_what_is_not_here(&[]), None);
        // a general skill that mentions the other app only in passing, or says how such names are written, is general
        let passing = found_as("---\nname: docx\ndescription: Makes Word files. If a page or doc skill or connector is offered, use that instead.\n---\nTools are named `mcp__<server>__<tool>`. A task may use an artifact. Subagents are optional here.\n");
        assert_eq!(passing.needs_what_is_not_here(&[]), None, "{passing:?}");
        // names the tools of a server this app has not
        let browser = found_as("---\nname: browser\ndescription: Drives a browser through mcp__Claude_Browser__tabs and mcp__remote-devices__x.\n---\nCall mcp__Claude_Browser__navigate.\n");
        assert_eq!(browser.mcp_servers, ["Claude_Browser", "remote-devices"]);
        assert_eq!(browser.needs_what_is_not_here(&["srv".to_string()]), Some(Missing::Tools("Claude_Browser".into())));
        // (a server this app has configured is no lack: the part of the tools' names is the server's)
        let wanted = vec!["Claude_Browser".to_string(), "remote-devices".to_string()];
        assert_eq!(browser.needs_what_is_not_here(&wanted), None);
        // in the body only
        let memory = found_as("---\nname: memory\ndescription: Imports notes.\n---\nUse mcp__memory__write to keep them.\n");
        assert_eq!(memory.needs_what_is_not_here(&[]), Some(Missing::Tools("memory".into())));
        // markers: a connector call and a spawned subagent in the description, a scheduled task in the text
        let connector = found_as("---\nname: sheets\ndescription: Read this before the first Drive or Sheets connector call.\n---\nText.\n");
        assert_eq!(connector.needs_what_is_not_here(&[]), Some(Missing::Something("a connector of another app")));
        let docs = found_as("---\nname: docs\ndescription: Making one - if no docs-connector instructions are in context, call its guide.\n---\nText.\n");
        assert!(docs.other_app.is_some());
        let research = found_as("---\nname: research\ndescription: This skill coordinates research subagents, so use it only with a tool for spawning them.\n---\nText.\n");
        assert_eq!(research.needs_what_is_not_here(&[]), Some(Missing::Something("subagents that it spawns")));
        let morning = found_as("---\nname: morning\ndescription: Render a brief.\n---\nWhen asked to set this up as a Scheduled Task, infer the time.\n");
        assert_eq!(morning.needs_what_is_not_here(&[]), Some(Missing::Something("another app's scheduled tasks")));
        // (a body that says subagents is not a skill that needs them: only what it says of itself first counts)
        let creator = found_as("---\nname: creator\ndescription: Create and test skills.\n---\nIf you can spawn subagents, run the checks in them; else do it yourself.\n");
        assert_eq!(creator.needs_what_is_not_here(&[]), None);
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
        write(&home.join(".codex/skills/two"), "---\nname: two\ndescription: from Codex\n---\nb");
        write(&home.join(".codex/skills/.system/review-agent"), "---\nname: review-agent\ndescription: Codex's own\n---\nb");
        write(&home.join(".agents/skills/three"), "---\nname: three\ndescription: shared\n---\nb");
        write(&home.join(".claude/skills/nameless"), "---\nname: nothing-said\n---\nb");
        write(&work.join(".claude/skills/one"), "---\nname: one\ndescription: the project's own\n---\nb");
        let skills = discover(&work, Some(&home));
        let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
        for name in ["one", "deep", "two", "three"] {
            assert!(names.contains(&name), "{names:?}");
        }
        assert!(!names.contains(&"nothing-said"), "no description, nothing to choose it by");
        assert!(!names.contains(&"review-agent"), "a tool's own, in a hidden folder, is not offered");
        let one = skills.iter().find(|skill| skill.name == "one").unwrap();
        assert_eq!((one.description.as_str(), one.scope), ("the project's own", "project"));
        let used = the_model_may_start(&skills);
        assert_eq!(used.iter().filter(|skill| skill.name == "one").count(), 1);
        assert_eq!(used.iter().find(|skill| skill.name == "one").unwrap().scope, "project", "the project's over the person's");
        assert_eq!(one_each(skills).iter().filter(|skill| skill.name == "one").count(), 1);
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

    /// Two skills of a name: the one a project has and the model may not start does not hide the person's own that it may, and the one the person may not start does not hide the other.
    #[test]
    fn two_skills_of_one_name_are_chosen_between_by_who_starts_the_one_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let (home, work) = (dir.path().join("me"), dir.path().join("work"));
        std::fs::create_dir_all(work.join(".git")).unwrap();
        write(&work.join(".claude/skills/pdf"), "---\nname: pdf\ndescription: from the project, for the person\ndisable-model-invocation: true\n---\nb");
        write(&home.join(".claude/skills/pdf"), "---\nname: pdf\ndescription: from the home, for the model\n---\nb");
        write(&work.join(".claude/skills/csv"), "---\nname: csv\ndescription: from the project, for the model\nuser-invocable: false\n---\nb");
        write(&home.join(".claude/skills/csv"), "---\nname: csv\ndescription: from the home, for the person\n---\nb");
        write(&work.join(".claude/skills/both"), "---\nname: both\ndescription: from the project\n---\nb");
        write(&home.join(".claude/skills/both"), "---\nname: both\ndescription: from the home\n---\nb");
        let skills = discover(&work, Some(&home));
        let scope_of = |chosen: &[&Skill], name: &str| chosen.iter().find(|skill| skill.name == name).unwrap().scope;
        let model = the_model_may_start(&skills);
        assert_eq!(model.len(), 3, "{model:?}");
        assert_eq!((scope_of(&model, "pdf"), scope_of(&model, "csv"), scope_of(&model, "both")), ("user", "project", "project"));
        let section = prompt_section(&skills, &[]);
        assert!(section.contains("- pdf: from the home, for the model") && section.contains("- csv: from the project, for the model") && section.contains("- both: from the project"), "{section}");
        assert!(!section.contains("from the project, for the person") && !section.contains("from the home, for the person") && !section.contains("from the home\n"), "{section}");
        let listed = listing(&skills);
        assert_eq!(listed.lines().count(), 3, "{listed}");
        let person = one_each(skills);
        let scope_of = |name: &str| person.iter().find(|skill| skill.name == name).unwrap().scope;
        assert_eq!(person.len(), 3);
        assert_eq!((scope_of("pdf"), scope_of("csv"), scope_of("both")), ("project", "user", "project"));
        assert!(person.iter().all(|skill| skill.for_the_person));
    }

    #[test]
    fn a_name_that_the_person_may_not_start_is_still_listed_once_for_its_switch() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".claude/skills/hidden"), "---\nname: Hidden\ndescription: d\nuser-invocable: false\n---\nb");
        let one = one_each(discover(dir.path(), None));
        assert_eq!(one.len(), 1);
        assert!(!one[0].for_the_person);
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
        let skills: Vec<Skill> = (0..150).map(|at| Skill { name: format!("s{at}"), description: "word ".repeat(200), folder: PathBuf::new(), scope: "user", for_the_model: true, for_the_person: true, ..Skill::default() }).collect();
        assert!(prompt_section(&skills, &[]).len() < MAX_LIST + 1000);
        assert_eq!(prompt_section(&[], &[]), "");
    }

    #[test]
    fn a_skill_for_tools_this_session_has_not_got_is_not_offered() {
        let skill = |name: &str, description: &str| Skill { name: name.into(), description: description.into(), folder: PathBuf::new(), scope: "user", for_the_model: true, for_the_person: true, ..Skill::default() };
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
        // (both of a name in the project and in the home go: it is one skill to them)
        let home = dir.path().join("me");
        write(&home.join(".claude/skills/pdf"), "---\nname: pdf\ndescription: d\n---\nb");
        assert!(without(discover(dir.path(), Some(&home)), &["pdf".to_string()]).iter().all(|skill| !skill.name.eq_ignore_ascii_case("pdf")));
        assert_eq!(without(discover(dir.path(), None), &[]).len(), 2);
    }

    #[test]
    fn the_list_says_it_is_no_word_about_this_conversation() {
        let skill = Skill { name: "imagegen".into(), description: "Edits images".into(), folder: PathBuf::new(), scope: "user", for_the_model: true, for_the_person: true, ..Skill::default() };
        assert!(prompt_section(&[skill], &[]).contains("says nothing about this conversation"));
    }

    #[test]
    fn a_long_list_stays_short_in_the_prompt_and_says_how_to_read_it_all() {
        let skill = |at: usize| Skill { name: format!("skill-{at}"), description: "Does something at great length, ".repeat(10), folder: PathBuf::new(), scope: "user", for_the_model: true, for_the_person: true, ..Skill::default() };
        let all: Vec<Skill> = (0..40).map(skill).collect();
        let section = prompt_section(&all, &[]);
        assert!(section.len() < MAX_LIST + 500, "{}", section.len());
        assert!(section.contains("`list`") && section.contains("more:"), "{section}");
        assert!(listing(&all).contains("skill-39") && !section.contains("skill-39"));
        assert!(!prompt_section(&all[..2], &[]).contains("`list`"), "nothing left out, nothing to say");
    }
}
