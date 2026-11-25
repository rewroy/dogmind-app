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
/// The runtime of MSYS (Git's `usr\bin`, MSYS2) and of Cygwin: a folder with one of these in it holds programs that cannot start in the sandbox.
const UNIX_RUNTIMES: [&str; 2] = ["msys-2.0.dll", "cygwin1.dll"];
/// Written when everything in `tools\` can be read by the sandbox: a stop half way goes on from the start next time.
/// What the name of the mark that the tools were opened to a container's commands begins with: then `-` and a hash of that container's SID.
pub const OPENED: &str = ".open-to-sandbox";
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

/// `path` without the folders that hold the MSYS or Cygwin runtime (its DLL right in them), for a command that runs in the sandbox: nothing in such a folder can start in a container
/// (it ends with 0xC0000142), and one that comes before System32 puts its `find` and `sort` in place of Windows' own. An empty entry stays, and so does a folder that is not there.
/// Not for a program the app starts outside the sandbox, where these work (see `path_for_programs`).
pub fn without_unix_runtimes(path: &str) -> String {
    let all: Vec<PathBuf> = std::env::split_paths(path).collect();
    let kept: Vec<PathBuf> = all.iter().filter(|folder| folder.as_os_str().is_empty() || !UNIX_RUNTIMES.iter().any(|runtime| folder.join(runtime).is_file())).cloned().collect();
    if kept.len() == all.len() {
        return path.to_string();
    }
    joined(kept, path)
}

/// PATH for a command: what is here first, without anything of a person's that holds the same programs.
pub fn path_with(root: &Path, inherited: &str) -> String {
    joined(folders_with(root, inherited, None), inherited)
}

/// Same for a program the app starts itself, such as an MCP server run with `npx`. MSYS and Cygwin folders stay on it: outside the sandbox their programs start.
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

/// What git, run here and not in the sandbox, has for `key` in `scope` (`--global`, the person's own config, or `--system`).
fn config_of(scope: &str, key: &str) -> Option<String> {
    let mut command = std::process::Command::new(git_program());
    command.args(["config", scope, key]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command.output().ok().map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string()).filter(|said| !said.is_empty())
}

/// The branch a new repository starts on, as git would take it: the person's own config, then the system's, then git's own. A name with anything in it
/// that a command line would read something into is not taken.
pub fn default_branch() -> &'static str {
    static BRANCH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BRANCH.get_or_init(|| {
        config_of("--global", "init.defaultBranch")
            .or_else(|| config_of("--system", "init.defaultBranch"))
            .filter(|name| name.chars().all(|c| c.is_alphanumeric() || "-_./".contains(c)))
            .unwrap_or_else(|| "master".to_string())
    })
}

/// A global config for commands, with who commits and the branch a new repository starts on, as the person's own has them. On a machine with no name set,
/// this app's: `git commit` otherwise fails, and nothing a command can do mends it.
fn git_identity(temp: &str) -> String {
    static WHO: std::sync::OnceLock<(String, String, Option<String>)> = std::sync::OnceLock::new();
    let (name, address, branch) = WHO.get_or_init(|| {
        (
            config_of("--global", "user.name").unwrap_or_else(|| "Dogmind".to_string()),
            config_of("--global", "user.email").unwrap_or_else(|| "dogmind@localhost".to_string()),
            config_of("--global", "init.defaultBranch"),
        )
    });
    let quoted = |text: &str| text.replace('\\', "\\\\").replace('"', "\\\"");
    let file = format!("{temp}\\gitconfig");
    let mut text = format!("[user]\n\tname = \"{}\"\n\temail = \"{}\"\n", quoted(name), quoted(address));
    if let Some(branch) = branch {
        text.push_str(&format!("[init]\n\tdefaultBranch = \"{}\"\n", quoted(branch)));
    }
    let _ = std::fs::write(&file, text);
    file
}

static DONE: Mutex<Option<HashSet<(PathBuf, String)>>> = Mutex::new(None);

/// So that the sandbox can read and run the folder. The first time through it writes to each thing in it; a second caller waits for that here. The app starts this in the
/// background, so that a person's first command rarely waits. A folder every container can read already (Program Files) needs nothing, and cannot be written to anyway.
pub fn open_to_sandbox(root: &Path) -> Result<(), String> {
    // SAFETY: the SID lives as long as the process.
    unsafe { open_to(root, crate::sandbox::profile_sid()?) }
}

/// `root` opened to the commands of the container `sid`, once. The mark of it is that container's own (`OPENED`, `-`, a hash of its SID): a folder opened to
/// one container is not so to another, as a copy of the tools or a development build's profile, and one mark for all had the other's commands find nothing
/// there to run. A mark of no container's, from before, stands for none.
unsafe fn open_to(root: &Path, sid: windows_sys::Win32::Security::PSID) -> Result<(), String> {
    let text = crate::sandbox::sid_text(sid);
    let memo = (root.to_path_buf(), text.clone());
    let mut done = DONE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if done.get_or_insert_with(HashSet::new).contains(&memo) {
        return Ok(());
    }
    crate::sandbox::grant_way_to(root, sid);
    let mark = root.join(format!("{OPENED}-{:016x}", crate::sandbox::fnv(&text)));
    if !crate::sandbox::open_to_every_container(root) && !mark.exists() {
        let not_changed = crate::read_folders::open_for_reading(root, sid)?;
        if not_changed > 0 {
            return Err(format!("{not_changed} things in {} could not be opened to commands", root.display()));
        }
        std::fs::write(&mark, "").map_err(|error| format!("{}: {error}", root.display()))?;
    }
    done.get_or_insert_with(HashSet::new).insert(memo);
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
        let dir = crate::in_container::tempdir().unwrap();
        let root = whole_bundle(dir.path());
        let (own_node, other) = (dir.path().join("Program Files\\nodejs"), dir.path().join("other"));
        std::fs::create_dir_all(&own_node).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(own_node.join("node.exe"), "").unwrap();
        let path = path_with(&root, &inherited(&[&own_node, &other]));
        let found: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(found, [root.join("python"), root.join("node"), root.join("git\\cmd"), other]);
    }

    #[test]
    fn only_a_folder_that_holds_the_runtime_itself_is_left_off() {
        let dir = crate::in_container::tempdir().unwrap();
        let (unix, cyg, deeper, plain) = (dir.path().join("unix"), dir.path().join("cyg"), dir.path().join("deeper"), dir.path().join("plain"));
        for (folder, file) in [(&unix, "msys-2.0.dll"), (&cyg, "cygwin1.dll"), (&deeper.join("bin"), "msys-2.0.dll"), (&plain, "find.exe")] {
            std::fs::create_dir_all(folder).unwrap();
            std::fs::write(folder.join(file), "").unwrap();
        }
        let missing = dir.path().join("not-there");
        // (the runtime must be directly in the folder; an empty entry and a missing folder are as they were)
        let path = format!("{};;{};{};{};{}", unix.display(), missing.display(), cyg.display(), deeper.display(), plain.display());
        assert_eq!(without_unix_runtimes(&path), format!(";{};{};{}", missing.display(), deeper.display(), plain.display()));
        assert_eq!(without_unix_runtimes(""), "");
        let untouched = format!("{};;{}", missing.display(), plain.display());
        assert_eq!(without_unix_runtimes(&untouched), untouched);
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
        let dir = crate::in_container::tempdir().unwrap();
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
        let dir = crate::in_container::tempdir().unwrap();
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

    /// The tools opened to one container are opened to another all the same (a copy of them, a development build's profile), and to the one only once: its
    /// mark says so at the next start. An old mark of no container's stands for none.
    #[test]
    fn the_tools_are_opened_to_each_container_once() {
        // (it writes the folder's list, which a command, in a container or with a restricted token, may not)
        if crate::in_container::cannot_run(crate::in_container::FOLDERS) {
            return;
        }
        let named = |name: &str| -> windows_sys::Win32::Security::PSID {
            let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
            let mut sid = std::ptr::null_mut();
            // SAFETY: the name ends with a zero; the SID is left for the process's end.
            assert!(unsafe { windows_sys::Win32::Security::Isolation::DeriveAppContainerSidFromAppContainerName(wide.as_ptr(), &mut sid) } >= 0);
            sid
        };
        let root = crate::in_container::tempdir().unwrap();
        std::fs::write(root.path().join("tool.exe"), "x").unwrap();
        std::fs::write(root.path().join(OPENED), "").unwrap();
        let (one, other) = (named(&format!("DogmindApp.Exec.Tests.Opened.One.P{}", std::process::id())), named(&format!("DogmindApp.Exec.Tests.Opened.Other.P{}", std::process::id())));
        // SAFETY: the SIDs live to the process's end.
        let (one_text, other_text) = unsafe { (crate::sandbox::sid_text(one), crate::sandbox::sid_text(other)) };
        let list = |path: &Path| crate::sandbox::dacl_text(path).unwrap();
        // SAFETY: as above.
        unsafe { open_to(root.path(), one) }.unwrap();
        assert!(list(&root.path().join("tool.exe")).contains(&one_text), "opened to the one, an old mark notwithstanding");
        // (the one's entry taken off the folder by hand, and this process's memo forgotten, as at the next start: the mark says it is open to the one)
        let taken_off = std::process::Command::new("icacls").arg(root.path()).arg("/remove:g").arg(format!("*{one_text}")).output().unwrap();
        assert!(taken_off.status.success() && !list(root.path()).contains(&one_text));
        DONE.lock().unwrap().take();
        // SAFETY: as above.
        unsafe { open_to(root.path(), one) }.unwrap();
        assert!(!list(root.path()).contains(&one_text), "not opened to the one again");
        // SAFETY: as above.
        unsafe { open_to(root.path(), other) }.unwrap();
        assert!(list(root.path()).contains(&other_text) && list(&root.path().join("tool.exe")).contains(&other_text), "opened to the other");
        let marks = std::fs::read_dir(root.path()).unwrap().flatten().filter(|entry| entry.file_name().to_string_lossy().starts_with(&format!("{OPENED}-"))).count();
        assert_eq!(marks, 2, "a mark for each");
    }

    /// In the real sandbox, with the real thing, in the layout that ships. Each thing a clean machine has nothing else for.
    #[test]
    #[ignore = "needs DOGMIND_TOOLS: python -I scripts/package.py --tools-only FOLDER"]
    fn the_real_tools_work_in_the_sandbox_and_change_nothing_outside_it() {
        let root = real_root();
        set_test_root(Some(&root));
        let holder = crate::in_container::tempdir().unwrap();
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
        let holder = crate::in_container::tempdir().unwrap();
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
        let holder = crate::in_container::tempdir().unwrap();
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
