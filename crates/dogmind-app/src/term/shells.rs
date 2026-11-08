//! Which shells are on this PC and how each is started: PowerShell 7, Windows PowerShell, the Command Prompt, and Git Bash where a full Git for Windows has one (the MinGit this app ships has
//! none). Looked for in the folders a `Places` names, so that a test gives it made-up ones; `Places::from_env` is this PC's own.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shell {
    /// What the setting remembers it by.
    pub id: &'static str,
    /// What the menu and the tab say.
    pub name: &'static str,
    pub program: PathBuf,
    pub arguments: &'static [&'static str],
    /// What it is started with besides what the person's own environment has.
    pub environment: &'static [(&'static str, &'static str)],
}

/// The shells, by `Shell::id`, in the order the menu has them.
#[cfg(test)]
pub const IDS: [&str; 4] = ["pwsh", "powershell", "cmd", "git-bash"];

#[derive(Clone, Debug, Default)]
pub struct Places {
    /// The folders of `PATH`, in order.
    pub path: Vec<PathBuf>,
    /// Where programs are installed: `ProgramW6432`, `ProgramFiles` and `ProgramFiles(x86)`.
    pub program_files: Vec<PathBuf>,
    /// Windows's own folder.
    pub system_root: Option<PathBuf>,
    pub local_app_data: Option<PathBuf>,
}

impl Places {
    /// This PC's own.
    pub fn from_env() -> Places {
        let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from);
        let mut program_files: Vec<PathBuf> = Vec::new();
        for name in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(folder) = var(name) {
                if !program_files.contains(&folder) {
                    program_files.push(folder);
                }
            }
        }
        Places { path: std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect()).unwrap_or_default(), program_files, system_root: var("SystemRoot"), local_app_data: var("LOCALAPPDATA") }
    }
}

/// A file is there. (A program that the Store lets by its name alone is a link that `is_file` cannot follow.)
fn is_there(path: &Path) -> bool {
    path.is_file() || std::fs::symlink_metadata(path).is_ok_and(|found| !found.is_dir())
}

fn on_path(places: &Places, name: &str) -> Option<PathBuf> {
    places.path.iter().map(|folder| folder.join(name)).find(|program| is_there(program))
}

/// The installed shells, in the menu's order.
pub fn discover(places: &Places) -> Vec<Shell> {
    let mut found = Vec::new();
    let system = places.system_root.clone().unwrap_or_else(|| PathBuf::from("C:\\Windows"));
    let first = |candidates: Vec<PathBuf>| candidates.into_iter().find(|program| is_there(program));
    // PowerShell 7: where its installer puts it, then wherever PATH has it (a Store or a winget copy).
    let seven: Vec<PathBuf> = places.program_files.iter().map(|folder| folder.join("PowerShell").join("7").join("pwsh.exe")).collect();
    if let Some(program) = first(seven).or_else(|| on_path(places, "pwsh.exe")) {
        found.push(Shell { id: "pwsh", name: "PowerShell 7", program, arguments: &["-NoLogo"], environment: &[] });
    }
    if let Some(program) = first(vec![system.join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe")]) {
        found.push(Shell { id: "powershell", name: "Windows PowerShell", program, arguments: &["-NoLogo"], environment: &[] });
    }
    if let Some(program) = first(vec![system.join("System32").join("cmd.exe")]) {
        found.push(Shell { id: "cmd", name: "Command Prompt", program, arguments: &[], environment: &[] });
    }
    if let Some(program) = git_bash(places) {
        // (a login shell, as Git's own does; and it stays in the folder it is started in, where it would go home)
        found.push(Shell { id: "git-bash", name: "Git Bash", program, arguments: &["--login", "-i"], environment: &[("CHERE_INVOKING", "1")] });
    }
    found
}

/// A full Git for Windows has `bin\bash.exe` beside `cmd\git.exe`. MinGit has the second and not the first, and an MSYS2 has the first and not the second.
fn git_bash(places: &Places) -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = places.program_files.iter().map(|folder| folder.join("Git")).collect();
    roots.extend(places.local_app_data.iter().map(|folder| folder.join("Programs").join("Git")));
    // (and wherever git itself is on PATH: its folder is `cmd`, or `bin`, or `mingw64\bin`, under the root)
    for folder in &places.path {
        if is_there(&folder.join("git.exe")) {
            roots.extend(folder.ancestors().skip(1).take(3).map(Path::to_path_buf));
        }
    }
    roots.into_iter().find(|root| is_there(&root.join("cmd").join("git.exe")) && is_there(&root.join("bin").join("bash.exe"))).map(|root| root.join("bin").join("bash.exe"))
}

/// What is run, as `CreateProcessW` is given it: the program in quotes, and its arguments.
pub fn command_line(shell: &Shell) -> String {
    let mut line = format!("\"{}\"", shell.program.display());
    for argument in shell.arguments {
        line.push(' ');
        line.push_str(argument);
    }
    line
}

/// The shell to open when none is named: the one remembered, if this PC still has it, and otherwise the first it has.
pub fn chosen<'a>(shells: &'a [Shell], remembered: &str) -> Option<&'a Shell> {
    shells.iter().find(|shell| shell.id == remembered).or_else(|| shells.first())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, path: &str) -> PathBuf {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"").unwrap();
        file
    }

    fn places(root: &Path) -> Places {
        Places { path: Vec::new(), program_files: vec![root.join("Program Files"), root.join("Program Files (x86)")], system_root: Some(root.join("Windows")), local_app_data: Some(root.join("Local")) }
    }

    fn ids(shells: &[Shell]) -> Vec<&str> {
        shells.iter().map(|shell| shell.id).collect()
    }

    #[test]
    fn a_pc_with_nothing_of_the_four_has_none_and_one_with_windows_has_the_two_it_ships() {
        let root = tempfile::tempdir().unwrap();
        assert!(discover(&places(root.path())).is_empty());
        touch(root.path(), "Windows/System32/cmd.exe");
        touch(root.path(), "Windows/System32/WindowsPowerShell/v1.0/powershell.exe");
        let shells = discover(&places(root.path()));
        assert_eq!(ids(&shells), ["powershell", "cmd"]);
        assert_eq!(shells[1].program, root.path().join("Windows/System32/cmd.exe"));
        assert_eq!(command_line(&shells[0]), format!("\"{}\" -NoLogo", shells[0].program.display()));
        assert_eq!(command_line(&shells[1]), format!("\"{}\"", shells[1].program.display()));
    }

    #[test]
    fn powershell_7_is_found_where_it_installs_and_on_path_and_comes_first() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), "Windows/System32/cmd.exe");
        let installed = touch(root.path(), "Program Files/PowerShell/7/pwsh.exe");
        let shells = discover(&places(root.path()));
        assert_eq!(ids(&shells), ["pwsh", "cmd"]);
        assert_eq!(shells[0].program, installed);
        // (one from the Store or winget is on PATH and nowhere else)
        std::fs::remove_file(&installed).unwrap();
        let elsewhere = touch(root.path(), "Apps/pwsh.exe");
        let mut here = places(root.path());
        assert_eq!(ids(&discover(&here)), ["cmd"]);
        here.path = vec![root.path().join("Nothing"), root.path().join("Apps")];
        let shells = discover(&here);
        assert_eq!((ids(&shells), &shells[0].program), (vec!["pwsh", "cmd"], &elsewhere));
    }

    #[test]
    fn git_bash_is_there_with_a_full_git_for_windows_and_not_with_mingit_or_an_msys2() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), "Windows/System32/cmd.exe");
        // MinGit, as this app ships it: git on PATH from its cmd folder, and no bash.
        touch(root.path(), "tools/git/cmd/git.exe");
        touch(root.path(), "tools/git/usr/bin/sh.exe");
        let mut here = places(root.path());
        here.path = vec![root.path().join("tools/git/cmd")];
        assert_eq!(ids(&discover(&here)), ["cmd"], "MinGit has no bash");
        // An MSYS2: a bash, and no cmd\git.exe beside it.
        touch(root.path(), "msys64/usr/bin/bash.exe");
        touch(root.path(), "msys64/usr/bin/git.exe");
        here.path = vec![root.path().join("msys64/usr/bin")];
        assert_eq!(ids(&discover(&here)), ["cmd"], "an MSYS2 is not Git for Windows");
        // Git for Windows, in Program Files.
        touch(root.path(), "Program Files/Git/cmd/git.exe");
        let bash = touch(root.path(), "Program Files/Git/bin/bash.exe");
        let shells = discover(&places(root.path()));
        assert_eq!(ids(&shells), ["cmd", "git-bash"]);
        assert_eq!(shells[1].program, bash);
        assert_eq!(shells[1].arguments, ["--login", "-i"]);
        assert!(shells[1].environment.contains(&("CHERE_INVOKING", "1")), "it stays in the folder it is started in");
        // (and one the person put elsewhere, found by where git is on PATH, whichever of its folders that is)
        for git_folder in ["cmd", "bin", "mingw64/bin"] {
            let other = tempfile::tempdir().unwrap();
            touch(other.path(), "Windows/System32/cmd.exe");
            touch(other.path(), "Tools/Git/cmd/git.exe");
            touch(other.path(), "Tools/Git/bin/bash.exe");
            touch(other.path(), &format!("Tools/Git/{git_folder}/git.exe"));
            let mut here = places(other.path());
            here.path = vec![other.path().join("Tools/Git").join(git_folder)];
            assert_eq!(ids(&discover(&here)), ["cmd", "git-bash"], "git on PATH from {git_folder}");
        }
        // (or in the folder Git's per-user installer uses)
        let local = tempfile::tempdir().unwrap();
        touch(local.path(), "Local/Programs/Git/cmd/git.exe");
        touch(local.path(), "Local/Programs/Git/bin/bash.exe");
        assert_eq!(ids(&discover(&places(local.path()))), ["git-bash"]);
    }

    #[test]
    fn the_shell_last_chosen_is_opened_when_this_pc_still_has_it_and_otherwise_the_first() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), "Windows/System32/cmd.exe");
        touch(root.path(), "Windows/System32/WindowsPowerShell/v1.0/powershell.exe");
        let shells = discover(&places(root.path()));
        assert_eq!(chosen(&shells, "cmd").map(|shell| shell.id), Some("cmd"));
        assert_eq!(chosen(&shells, "git-bash").map(|shell| shell.id), Some("powershell"), "gone since");
        assert_eq!(chosen(&shells, "").map(|shell| shell.id), Some("powershell"));
        assert_eq!(chosen(&[], "cmd"), None);
    }

    #[test]
    fn this_pcs_own_places_find_at_least_the_command_prompt() {
        let shells = discover(&Places::from_env());
        assert!(shells.iter().any(|shell| shell.id == "cmd"), "{shells:?}");
        assert!(shells.iter().all(|shell| IDS.contains(&shell.id)));
    }
}
