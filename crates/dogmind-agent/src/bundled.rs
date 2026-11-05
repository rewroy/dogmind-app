//! What commands run with, beside the exe in `tools\`: Python with its modules, Node.js and Git, so that a person installs nothing and a command does the same on every machine.
//!
//! It comes first on a command's PATH, and what a person has installed of the same is left off it. A command can read and run it and cannot change it. The folder is the only thing
//! given to the sandbox for this: under Program Files every container can read already, and elsewhere one read-and-run entry goes on each thing in it, which happens once.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Below `tools\`, what goes on PATH, in order.
const ON_PATH: [&str; 3] = ["python", "node", "git\\cmd"];
/// Programs that come from here. A folder of a person's that holds one of these is left off a command's PATH, so that `python3`, `pip` or `npm` from there cannot be what runs.
pub const NAMES: [&str; 3] = ["python.exe", "node.exe", "git.exe"];
/// Written when everything in `tools\` can be read by the sandbox: a stop half way goes on from the start next time.
const OPENED: &str = ".open-to-sandbox";
/// What the workspace gets from `pip install`, which cannot write in the bundled folder.
pub const PY_DEPS: &str = ".pydeps";
/// What the model is told is there, of each of the three. It reaches the prompt and has to stay the same from one session to the next.
const OFFERED_PYTHON: &str = "python (numpy, pandas, matplotlib, requests, bs4, lxml, PIL, yaml, openpyxl, docx, pptx, pypdf, markdown, jinja2, pytest), pip";
const OFFERED_NODE: &str = "node, npm, npx";
const OFFERED_GIT: &str = "git";
/// What node needs of its folder besides itself: `NODE_OPTIONS` requires it, and without it every `node` fails at its start.
const NODE_FIX: &str = "sandbox_fix.cjs";

/// What of the bundle is there, each of the three on its own. A virus scanner may quarantine one file, or a folder be left half updated, and the bundle is then there in part: what is
/// promised, set and taken off PATH is of what is there, and not of all of it for any one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Present {
    pub python: bool,
    pub node: bool,
    pub git: bool,
}

impl Present {
    pub fn of(root: &Path) -> Present {
        Present {
            python: root.join("python").join("python.exe").is_file(),
            node: root.join("node").join("node.exe").is_file(),
            git: root.join("git").join("cmd").join("git.exe").is_file(),
        }
    }

    /// Whether a program of `NAMES` comes from here.
    fn has(&self, program: &str) -> bool {
        match program {
            "python.exe" => self.python,
            "node.exe" => self.node,
            "git.exe" => self.git,
            _ => false,
        }
    }

    /// Whether a tool the prompt names is this bundle's.
    pub fn covers(&self, tool: &str) -> bool {
        match tool {
            "python" | "pip" => self.python,
            "node" | "npm" | "npx" => self.node,
            "git" => self.git,
            _ => false,
        }
    }

    /// What the model is told is there: the same words, in the same order, for each that is. All of it is the words there have always been.
    pub fn offered(&self) -> String {
        [(self.python, OFFERED_PYTHON), (self.node, OFFERED_NODE), (self.git, OFFERED_GIT)].iter().filter(|(there, _)| *there).map(|(_, words)| *words).collect::<Vec<_>>().join(", ")
    }

    /// What is not there.
    pub fn missing(&self) -> Vec<&'static str> {
        [(self.python, "python"), (self.node, "node"), (self.git, "git")].iter().filter(|(there, _)| !*there).map(|(_, name)| *name).collect()
    }
}
pub const NOT_THERE: &str = "note: this build has no tools folder beside the app, so commands have only what this machine has installed.\n";

#[cfg(test)]
thread_local! {
    static TEST_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_test_root(root: Option<&Path>) {
    TEST_ROOT.with(|known| *known.borrow_mut() = root.map(Path::to_path_buf));
}

/// The folder: beside the exe, or for a development build with none there, where `DOGMIND_TOOLS` says.
pub fn root() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(root) = TEST_ROOT.with(|known| known.borrow().clone()) {
        return Some(root);
    }
    let beside = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join("tools")));
    if let Some(found) = beside.filter(|folder| folder.join("python").is_dir() || folder.join("node").is_dir() || folder.join("git").is_dir()) {
        return Some(found);
    }
    #[cfg(debug_assertions)]
    if let Some(named) = std::env::var_os("DOGMIND_TOOLS").map(PathBuf::from).filter(|folder| folder.is_dir()) {
        return Some(named);
    }
    None
}

/// Git for the app's own calls, which run outside the sandbox and take the same care with what a repository's config can start.
pub fn git_program() -> PathBuf {
    root().map(|root| root.join("git").join("cmd").join("git.exe")).filter(|git| git.is_file()).unwrap_or_else(|| PathBuf::from("git"))
}

/// Whether a folder holds a program that comes from here: one the bundle has, and not one it has not (a person's own Python stays on PATH when the bundle lost its own).
fn holds_a_tool(folder: &Path, present: &Present) -> bool {
    NAMES.iter().any(|name| present.has(name) && folder.join(name).is_file())
}

/// Whether the bundle takes the place of what is in this folder of a person's: it holds a program the bundle has.
pub fn replaces(folder: &Path) -> bool {
    root().is_some_and(|root| holds_a_tool(folder, &Present::of(&root)))
}

/// The folders on a command's PATH: those of the bundle that are there (a tool's folder with the tool missing is not), then `extra`, then what the machine has, without a person's folder
/// that holds a program the bundle really has.
fn folders_with(root: &Path, inherited: &str, extra: Option<PathBuf>) -> Vec<PathBuf> {
    let present = Present::of(root);
    let own = ON_PATH.iter().zip([present.python, present.node, present.git]).filter(|(_, there)| *there).map(|(folder, _)| root.join(folder)).filter(|folder| folder.is_dir());
    let rest = std::env::split_paths(inherited).filter(|folder| !holds_a_tool(folder, &present));
    own.chain(extra).chain(rest).collect()
}

fn joined(folders: Vec<PathBuf>, otherwise: &str) -> String {
    std::env::join_paths(folders).map_or_else(|_| otherwise.to_string(), |joined| joined.to_string_lossy().into_owned())
}

/// PATH for a command: what is here first, without anything of a person's that holds the same programs.
pub fn path_with(root: &Path, inherited: &str) -> String {
    joined(folders_with(root, inherited, None), inherited)
}

/// Same for a program the app starts itself, such as an MCP server run with `npx`.
pub fn path_for_programs() -> Option<String> {
    let root = root()?;
    Some(path_with(&root, &std::env::var("PATH").unwrap_or_default()))
}

/// What a command's environment needs for these: everything they would otherwise write beside themselves or in the person's profile goes where a command can write.
pub fn environment(root: &Path, inherited_path: &str, temp: &str, workspace: &Path) -> Vec<(String, String)> {
    let deps = workspace.join(PY_DEPS).display().to_string();
    // (where `npm install -g` puts its commands, right in the folder on Windows: on the PATH, and in the sandbox's own folder, which lasts from one command to the next)
    let global = format!("{temp}\\npm-global");
    let mut block = vec![("PATH".to_string(), joined(folders_with(root, inherited_path, Some(PathBuf::from(&global))), inherited_path))];
    let present = Present::of(root);
    let fix = root.join("node").join(NODE_FIX);
    for (name, value) in [
        ("PYTHONUTF8", "1".to_string()),
        // (its looking for a repository would otherwise stop at a folder a container cannot look at, and say something else than that there is none)
        ("GIT_DISCOVERY_ACROSS_FILESYSTEM", "1".to_string()),
        // (no prompt can be answered from here, and the person's own config is closed: their name and address come in this one, which a repository's own overrides)
        ("GIT_TERMINAL_PROMPT", "0".to_string()),
        ("GIT_CONFIG_GLOBAL", git_identity(temp)),
        ("PYTHONDONTWRITEBYTECODE", "1".to_string()),
        // (so that a test run leaves no `.pytest_cache` in the person's folder: the workspace gets only what was asked for)
        ("PYTEST_ADDOPTS", format!("-o \"cache_dir={}/pytest-cache\"", temp.replace('\\', "/"))),
        ("PYTHONNOUSERSITE", "1".to_string()),
        ("DOGMIND_PYDEPS", deps.clone()),
        ("PIP_TARGET", deps),
        ("PIP_CACHE_DIR", format!("{temp}\\pip-cache")),
        ("PIP_DISABLE_PIP_VERSION_CHECK", "1".to_string()),
        ("PIP_NO_WARN_SCRIPT_LOCATION", "1".to_string()),
        ("MPLBACKEND", "Agg".to_string()),
        ("MPLCONFIGDIR", format!("{temp}\\matplotlib")),
        ("npm_config_cache", format!("{temp}\\npm-cache")),
        ("npm_config_prefix", global.clone()),
        ("npm_config_userconfig", format!("{temp}\\npmrc")),
        ("npm_config_update_notifier", "false".to_string()),
        ("npm_config_fund", "false".to_string()),
    ] {
        block.push((name.to_string(), value));
    }
    // Only where the folder is: a PYTHONHOME that names nothing stops every python at its start, a person's own among them (it is on PATH when the bundle has none).
    if present.python {
        block.push(("PYTHONHOME".to_string(), root.join("python").display().to_string()));
    }
    // (without the first two, node looks at the root of the drive to load even this, and a container may not; and if the file is not there, every `node` fails at its start)
    if present.node && fix.is_file() {
        block.push(("NODE_OPTIONS".to_string(), format!("--preserve-symlinks --preserve-symlinks-main --require \"{}\"", fix.display().to_string().replace('\\', "/"))));
    }
    block
}

/// A global config for commands, with only who commits. On a machine with no name set, this app's: `git commit` otherwise fails, and nothing a command can do mends it.
fn git_identity(temp: &str) -> String {
    static WHO: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();
    let (name, address) = WHO.get_or_init(|| {
        let ask = |key: &str, otherwise: &str| {
            let mut command = std::process::Command::new(git_program());
            command.args(["config", "--global", key]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            command.output().ok().map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string()).filter(|said| !said.is_empty()).unwrap_or_else(|| otherwise.to_string())
        };
        (ask("user.name", "Dogmind"), ask("user.email", "dogmind@localhost"))
    });
    let quoted = |text: &str| text.replace('\\', "\\\\").replace('"', "\\\"");
    let file = format!("{temp}\\gitconfig");
    let _ = std::fs::write(&file, format!("[user]\n\tname = \"{}\"\n\temail = \"{}\"\n", quoted(name), quoted(address)));
    file
}

static DONE: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

/// So that the sandbox can read and run the folder. The first time through it writes to each thing in it; a second caller waits for that here. The app starts this in the
/// background, so that a person's first command rarely waits. A folder every container can read already (Program Files) needs nothing, and cannot be written to anyway.
pub fn open_to_sandbox(root: &Path) -> Result<(), String> {
    let mut done = DONE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if done.get_or_insert_with(HashSet::new).contains(root) {
        return Ok(());
    }
    // SAFETY: the SID lives as long as the process.
    unsafe {
        let sid = crate::sandbox::profile_sid()?;
        crate::sandbox::grant_way_to(root, sid);
        if !crate::sandbox::open_to_every_container(root) && !root.join(OPENED).exists() {
            let not_changed = crate::read_folders::open_for_reading(root, sid)?;
            if not_changed > 0 {
                return Err(format!("{not_changed} things in {} could not be opened to commands", root.display()));
            }
            std::fs::write(root.join(OPENED), "").map_err(|error| format!("{}: {error}", root.display()))?;
        }
    }
    done.get_or_insert_with(HashSet::new).insert(root.to_path_buf());
    Ok(())
}

/// When a session starts: what a first command would otherwise wait for, on a thread of low priority.
pub fn open_in_the_background() {
    if let Some(root) = root() {
        std::thread::spawn(move || {
            crate::workspace_grants::in_the_background();
            let _ = open_to_sandbox(&root);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    fn inherited(folders: &[&Path]) -> String {
        std::env::join_paths(folders).unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn the_bundled_tools_come_first_and_a_persons_own_are_left_off() {
        let dir = tempfile::tempdir().unwrap();
        let root = whole_bundle(dir.path());
        let (own_node, other) = (dir.path().join("Program Files\\nodejs"), dir.path().join("other"));
        std::fs::create_dir_all(&own_node).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(own_node.join("node.exe"), "").unwrap();
        let path = path_with(&root, &inherited(&[&own_node, &other]));
        let found: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(found, [root.join("python"), root.join("node"), root.join("git\\cmd"), other]);
    }

    /// A bundle with these programs in it (empty files will do: only that they are there is looked at), and `sandbox_fix.cjs` beside node's.
    fn bundle_with(dir: &Path, programs: &[&str]) -> PathBuf {
        let root = dir.join("tools");
        for program in programs {
            let (folder, file) = match *program {
                "python" => ("python", "python.exe"),
                "node" => ("node", "node.exe"),
                _ => ("git\\cmd", "git.exe"),
            };
            std::fs::create_dir_all(root.join(folder)).unwrap();
            std::fs::write(root.join(folder).join(file), "").unwrap();
            if *program == "node" {
                std::fs::write(root.join(folder).join(NODE_FIX), "").unwrap();
            }
        }
        root
    }

    fn whole_bundle(dir: &Path) -> PathBuf {
        bundle_with(dir, &["python", "node", "git"])
    }

    #[test]
    fn what_they_would_write_goes_where_a_command_can_write() {
        let dir = tempfile::tempdir().unwrap();
        let root = whole_bundle(dir.path());
        let block = environment(&root, "C:\\x", "C:\\ac\\Temp", Path::new("C:\\work"));
        let value = |name: &str| block.iter().find(|(key, _)| key == name).unwrap().1.clone();
        assert_eq!(value("PIP_TARGET"), "C:\\work\\.pydeps");
        assert_eq!(value("DOGMIND_PYDEPS"), "C:\\work\\.pydeps");
        assert_eq!(value("npm_config_cache"), "C:\\ac\\Temp\\npm-cache");
        // (`npm install -g` puts its commands right in the prefix on Windows: it is in the sandbox's own folder, which lasts, and on the PATH, after the bundled tools)
        assert_eq!(value("npm_config_prefix"), "C:\\ac\\Temp\\npm-global");
        let path: Vec<PathBuf> = std::env::split_paths(&value("PATH")).collect();
        assert_eq!(path, [root.join("python"), root.join("node"), root.join("git\\cmd"), PathBuf::from("C:\\ac\\Temp\\npm-global"), PathBuf::from("C:\\x")]);
        assert_eq!(value("PYTHONHOME"), root.join("python").display().to_string());
        assert!(value("NODE_OPTIONS").contains("sandbox_fix.cjs"));
        assert_eq!(value("PYTEST_ADDOPTS"), "-o \"cache_dir=C:/ac/Temp/pytest-cache\"");
    }

    /// A bundle with a program missing is promised, set and taken off PATH only as far as what is there.
    #[test]
    fn a_bundle_with_only_git_offers_only_git_and_leaves_a_persons_python_and_node_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = bundle_with(dir.path(), &["git"]);
        let (python, node) = (dir.path().join("Python312"), dir.path().join("nodejs"));
        for (folder, program) in [(&python, "python.exe"), (&node, "node.exe")] {
            std::fs::create_dir_all(folder).unwrap();
            std::fs::write(folder.join(program), "").unwrap();
        }
        let present = Present::of(&root);
        assert_eq!(present, Present { python: false, node: false, git: true });
        assert_eq!(present.offered(), "git");
        assert_eq!(present.missing(), ["python", "node"]);
        assert!(!present.covers("python") && !present.covers("npm") && present.covers("git"));
        // PATH: its git, and the person's own Python and Node, which are what runs
        let path: Vec<PathBuf> = std::env::split_paths(&path_with(&root, &inherited(&[&python, &node]))).collect();
        assert_eq!(path, [root.join("git\\cmd"), python.clone(), node.clone()]);
        let block = environment(&root, &inherited(&[&python, &node]), "C:\\ac\\Temp", Path::new("C:\\work"));
        let names: Vec<&str> = block.iter().map(|(name, _)| name.as_str()).collect();
        assert!(!names.contains(&"PYTHONHOME") && !names.contains(&"NODE_OPTIONS"), "{names:?}");
        // all of it is the words there have always been, in their order
        let whole = Present { python: true, node: true, git: true };
        assert_eq!(whole.offered(), "python (numpy, pandas, matplotlib, requests, bs4, lxml, PIL, yaml, openpyxl, docx, pptx, pypdf, markdown, jinja2, pytest), pip, node, npm, npx, git");
        assert!(whole.missing().is_empty());
        // node without the file its options require gets no options
        let loose = bundle_with(&dir.path().join("loose"), &["node"]);
        std::fs::remove_file(loose.join("node").join(NODE_FIX)).unwrap();
        let block = environment(&loose, "", "C:\\ac\\Temp", Path::new("C:\\work"));
        assert!(!block.iter().any(|(name, _)| name == "NODE_OPTIONS") && !block.iter().any(|(name, _)| name == "PYTHONHOME"));
        // a tool's folder with the tool gone is not put on PATH
        std::fs::remove_file(loose.join("node").join("node.exe")).unwrap();
        assert!(folders_with(&loose, "", None).iter().all(|folder| folder.as_os_str().is_empty()), "only the empty entry that an empty PATH is");
    }

    fn real_root() -> PathBuf {
        PathBuf::from(std::env::var_os("DOGMIND_TOOLS").expect("DOGMIND_TOOLS names a folder made with: python -I scripts/package.py --tools-only FOLDER"))
    }

    /// As the model's call runs, with the tool's own steps.
    fn command(line: &str, work: &Path, network: bool) -> (Option<i32>, String) {
        if network {
            let done = crate::sandbox::run(line, work, work, network, Duration::from_secs(120), &AtomicBool::new(false)).unwrap();
            return (done.status, format!("{}{}", crate::sandbox::decode(&done.stdout), crate::sandbox::decode(&done.stderr)));
        }
        let said = crate::tools::Workbench::new(work).run("execute_command", &serde_json::json!({"command": line}), work, &AtomicBool::new(false)).unwrap();
        let code = said.rsplit("exit_code: ").next().and_then(|code| code.trim().parse().ok());
        (code, said)
    }

    /// In the real sandbox, with the real thing, in the layout that ships. Each thing a clean machine has nothing else for.
    #[test]
    #[ignore = "needs DOGMIND_TOOLS: python -I scripts/package.py --tools-only FOLDER"]
    fn the_real_tools_work_in_the_sandbox_and_change_nothing_outside_it() {
        let root = real_root();
        set_test_root(Some(&root));
        let holder = tempfile::tempdir().unwrap();
        let (work, outside) = (holder.path().join("work"), holder.path().join("outside"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(work.join("data.txt"), "IN-THE-WORKSPACE").unwrap();
        let ok = |line: &str, shows: &str| {
            let started = std::time::Instant::now();
            let (status, said) = command(line, &work, false);
            println!("{} ms: {line}\n{}", started.elapsed().as_millis(), said.chars().take(300).collect::<String>());
            assert_eq!(status, Some(0), "{line}: {said}");
            assert!(said.contains(shows), "{line}: {said}");
        };
        ok("python -c \"import numpy, pandas, requests, PIL, lxml, bs4, yaml, openpyxl, docx, pptx, pypdf, markdown, jinja2, pytest; print('ALL-IMPORTED', numpy.__version__)\"", "ALL-IMPORTED");
        ok("python -c \"import matplotlib.pyplot as p; p.plot([1,2]); p.savefig('plot.png'); print('PLOTTED')\"", "PLOTTED");
        assert!(work.join("plot.png").is_file());
        ok("python -c \"import sys; print(sys.prefix)\"", "tools");
        std::fs::write(work.join("helper.py"), "VALUE = 'BESIDE-THE-SCRIPT'\n").unwrap();
        std::fs::write(work.join("main.py"), "import helper\nprint(helper.VALUE)\n").unwrap();
        ok("python main.py", "BESIDE-THE-SCRIPT");
        ok("py -3 main.py", "BESIDE-THE-SCRIPT");
        ok("python3 main.py", "BESIDE-THE-SCRIPT");
        ok("python -c \"print('日本語')\"", "日本語");
        std::fs::write(work.join("test_a.py"), "import helper\ndef test_it():\n    assert helper.VALUE\n").unwrap();
        ok("pytest -q test_a.py", "1 passed");
        ok("pip --version", "pip");
        ok("node -e \"console.log('FROM-NODE', process.version)\"", "FROM-NODE");
        std::fs::write(work.join("good.mjs"), "import { readFileSync } from 'node:fs';\nconsole.log(readFileSync('data.txt', 'utf8'));\n").unwrap();
        ok("node --check good.mjs && echo CHECKED", "CHECKED");
        ok("node good.mjs", "IN-THE-WORKSPACE");
        ok("node -e \"const fs=require('fs'); fs.realpathSync('.'); fs.lstatSync('C:\\\\'); fs.lstatSync('C:/Users'); fs.promises.lstat('C:\\\\Users').then(()=>console.log('LOOKED'))\"", "LOOKED");
        ok("python -c \"import tempfile, os; d=tempfile.mkdtemp(); f=os.path.join(d,'x'); open(f,'w').close(); os.remove(f); os.rmdir(d); print('TEMP-OK')\"", "TEMP-OK");
        ok("npm --version", ".");
        ok("npx --version", ".");
        ok("npm init -y", "package.json");
        ok("git --version", "git version");
        assert!(command("git status", &work, false).1.contains("not a git repository"), "and not that a folder could not be looked at");
        ok("git init -q && git status", "No commits yet");
        ok("git add data.txt && git commit -q -m first && git log --oneline", "first");
        let (status, said) = command("git config core.hooksPath x", &work, false);
        assert_ne!(status, Some(0), "the app's protection of what would run outside stays: {said}");
        // Nothing outside the workspace, whichever of them tries.
        let far = outside.join("x.txt");
        for line in [format!("python -c \"open(r'{}','w')\"", far.display()), format!("node -e \"require('fs').writeFileSync(String.raw`{}`,'x')\"", far.display()), format!("python -c \"open(r'{}','w')\"", root.join("python").join("planted.txt").display())] {
            let (status, said) = command(&line, &work, false);
            assert_ne!(status, Some(0), "{line}: {said}");
        }
        assert!(!far.exists());
        assert!(!root.join("python").join("planted.txt").exists());
        // Not the network, unless approved.
        let (status, said) = command("python -c \"import socket; socket.create_connection(('1.1.1.1', 443), 3)\"", &work, false);
        assert_ne!(status, Some(0), "{said}");
        set_test_root(None);
    }

    /// In the real sandbox, with the real thing: what `npm install -g` installs runs by its name, and nothing is left in the workspace.
    #[test]
    #[ignore = "needs DOGMIND_TOOLS: python -I scripts/package.py --tools-only FOLDER"]
    fn a_command_installed_with_npm_g_runs_by_its_name() {
        let root = real_root();
        set_test_root(Some(&root));
        let holder = tempfile::tempdir().unwrap();
        let (work, package) = (holder.path().join("work"), holder.path().join("work").join("hello-pkg"));
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("package.json"), r#"{"name":"hello-pkg","version":"1.0.0","bin":{"hellopkg":"cli.js"}}"#).unwrap();
        std::fs::write(package.join("cli.js"), "#!/usr/bin/env node\nconsole.log('HELLO-FROM-A-GLOBAL-PACKAGE');\n").unwrap();
        let (status, said) = command("npm pack ./hello-pkg && npm install -g --offline ./hello-pkg-1.0.0.tgz", &work, false);
        println!("{status:?}: {}", said.chars().take(600).collect::<String>());
        assert_eq!(status, Some(0), "{said}");
        let (status, said) = command("hellopkg", &work, false);
        println!("{status:?}: {}", said.chars().take(600).collect::<String>());
        assert!(status == Some(0) && said.contains("HELLO-FROM-A-GLOBAL-PACKAGE"), "{status:?}: {said}");
        assert!(!work.join(".npm-global").exists(), "nothing of it in the person's folder");
        set_test_root(None);
    }

    /// As on a machine with none of these installed: nothing but the system on PATH.
    #[test]
    #[ignore = "needs DOGMIND_TOOLS, and sets PATH for the process: run alone"]
    fn on_a_machine_with_nothing_installed_they_are_still_there() {
        let root = real_root();
        set_test_root(Some(&root));
        let system = std::env::var("SystemRoot").unwrap();
        let saved = std::env::var_os("PATH");
        // SAFETY: this test runs alone, as its reason says.
        unsafe { std::env::set_var("PATH", format!("{system}\\System32;{system}")) };
        let holder = tempfile::tempdir().unwrap();
        let work = holder.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        for (line, shows) in [
            ("python -c \"import numpy, pandas, requests, PIL, lxml; print('ALL-IMPORTED')\"", "ALL-IMPORTED"),
            ("node -e \"console.log('FROM-NODE')\"", "FROM-NODE"),
            ("npm --version", "."),
            ("git init -q && git status", "No commits yet"),
            ("curl --version", "curl"),
            ("tar --version", "tar"),
            ("python -c \"import sys; print(sys.executable)\"", "tools"),
            ("node -p process.execPath", "tools"),
            ("git --exec-path", "tools"),
        ] {
            let (status, said) = command(line, &work, false);
            println!("{line}: {status:?}: {}", said.chars().take(200).collect::<String>());
            assert_eq!(status, Some(0), "{line}: {said}");
            assert!(said.contains(shows), "{line}: {said}");
        }
        // SAFETY: as above.
        unsafe { std::env::set_var("PATH", saved.unwrap()) };
        set_test_root(None);
    }
}
