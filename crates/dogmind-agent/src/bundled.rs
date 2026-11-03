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
/// What the model is told is there. It reaches the prompt and has to stay the same from one session to the next.
pub const OFFERED: &str = "python (numpy, pandas, matplotlib, requests, bs4, lxml, PIL, yaml, openpyxl, docx, pptx, pypdf, markdown, jinja2, pytest), pip, node, npm, npx, git";
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

fn holds_a_tool(folder: &Path) -> bool {
    NAMES.iter().any(|name| folder.join(name).is_file())
}

/// PATH for a command: what is here first, without anything of a person's that holds the same programs.
pub fn path_with(root: &Path, inherited: &str) -> String {
    let own = ON_PATH.iter().map(|folder| root.join(folder)).filter(|folder| folder.is_dir());
    let rest = std::env::split_paths(inherited).filter(|folder| !holds_a_tool(folder));
    std::env::join_paths(own.chain(rest)).map_or_else(|_| inherited.to_string(), |joined| joined.to_string_lossy().into_owned())
}

/// Same for a program the app starts itself, such as an MCP server run with `npx`.
pub fn path_for_programs() -> Option<String> {
    let root = root()?;
    Some(path_with(&root, &std::env::var("PATH").unwrap_or_default()))
}

/// What a command's environment needs for these: everything they would otherwise write beside themselves or in the person's profile goes where a command can write.
pub fn environment(root: &Path, inherited_path: &str, temp: &str, workspace: &Path) -> Vec<(String, String)> {
    let deps = workspace.join(PY_DEPS).display().to_string();
    let mut block = vec![("PATH".to_string(), path_with(root, inherited_path))];
    for (name, value) in [
        ("PYTHONHOME", root.join("python").display().to_string()),
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
        // (without the first two, node looks at the root of the drive to load even this, and a container may not)
        ("NODE_OPTIONS", format!("--preserve-symlinks --preserve-symlinks-main --require \"{}\"", root.join("node").join("sandbox_fix.cjs").display().to_string().replace('\\', "/"))),
        ("MPLBACKEND", "Agg".to_string()),
        ("MPLCONFIGDIR", format!("{temp}\\matplotlib")),
        ("npm_config_cache", format!("{temp}\\npm-cache")),
        ("npm_config_prefix", workspace.join(".npm-global").display().to_string()),
        ("npm_config_userconfig", format!("{temp}\\npmrc")),
        ("npm_config_update_notifier", "false".to_string()),
        ("npm_config_fund", "false".to_string()),
    ] {
        block.push((name.to_string(), value));
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
        let root = dir.path().join("tools");
        for folder in ["python", "node", "git\\cmd"] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        let (own_node, other) = (dir.path().join("Program Files\\nodejs"), dir.path().join("other"));
        std::fs::create_dir_all(&own_node).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(own_node.join("node.exe"), "").unwrap();
        let path = path_with(&root, &inherited(&[&own_node, &other]));
        let found: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(found, [root.join("python"), root.join("node"), root.join("git\\cmd"), other]);
    }

    #[test]
    fn what_they_would_write_goes_where_a_command_can_write() {
        let block = environment(Path::new("C:\\app\\tools"), "C:\\x", "C:\\ac\\Temp", Path::new("C:\\work"));
        let value = |name: &str| block.iter().find(|(key, _)| key == name).unwrap().1.clone();
        assert_eq!(value("PIP_TARGET"), "C:\\work\\.pydeps");
        assert_eq!(value("DOGMIND_PYDEPS"), "C:\\work\\.pydeps");
        assert_eq!(value("npm_config_cache"), "C:\\ac\\Temp\\npm-cache");
        assert_eq!(value("npm_config_prefix"), "C:\\work\\.npm-global");
        assert_eq!(value("PYTHONHOME"), "C:\\app\\tools\\python");
        assert_eq!(value("PYTEST_ADDOPTS"), "-o \"cache_dir=C:/ac/Temp/pytest-cache\"");
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
