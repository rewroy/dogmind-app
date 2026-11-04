//! `DogmindSetup.exe --update`, run for real on a scratch folder: the way the app runs it, with the test switch and `--auto` so that nothing real is written and no one has to press anything.
//! The install and the setups are made here with the installer's own `--pack`. The "app" is a copy of `cmd.exe`, which, started with no console to read, stays running: that is how a test
//! sees that the app was started, and it is ended by its process number, from the scratch folder it ran in.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE};

const SETUP: &str = env!("CARGO_BIN_EXE_dogmind-setup");
/// Neither the person's screen nor a console of any.
const NO_WINDOW: u32 = 0x0800_0000;
const APP: &str = "test-app.exe";

fn command_interpreter() -> Vec<u8> {
    std::fs::read(std::env::var_os("ComSpec").expect("ComSpec")).expect("cmd.exe")
}

/// The app of a version: bytes after the end of an executable change its hash and not what it does.
fn version_app(version: &str) -> Vec<u8> {
    let mut app = command_interpreter();
    app.extend_from_slice(format!("version {version}").as_bytes());
    app
}

/// The files of a version: the app, and a file that says which version it is.
fn version_tree(folder: &Path, version: &str) {
    std::fs::create_dir_all(folder.join("data")).unwrap();
    std::fs::write(folder.join(APP), version_app(version)).unwrap();
    std::fs::write(folder.join("data").join("version.txt"), version).unwrap();
}

/// A setup of that version, packed by the installer.
fn pack(scene: &Path, version: &str) -> PathBuf {
    let (source, out) = (scene.join(format!("in-{version}")), scene.join(format!("DogmindSetup-{version}.exe")));
    version_tree(&source, version);
    let made = Command::new(SETUP).arg("--pack").arg(&source).arg(&out).args(["--version", version, "--level", "1"]).status().unwrap();
    assert!(made.success());
    out
}

fn setup(program: &Path) -> Command {
    let mut command = Command::new(program);
    command.env("DOGMIND_SETUP_APP_NAME", APP).env("DOGMIND_SETUP_LANG", "en").env_remove("DOGMIND_SETUP_FAIL_AFTER").env_remove("DOGMIND_SETUP_SLOW_MS");
    command
}

/// The numbers of the processes whose program is a file of this folder.
fn running_from(folder: &Path) -> Vec<u32> {
    let wanted = format!("{}\\", folder.to_string_lossy().trim_end_matches('\\').to_lowercase());
    let mut found = Vec::new();
    // SAFETY: the snapshot is this function's, and each buffer is as long as the call is told.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        assert!(snapshot != INVALID_HANDLE_VALUE);
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID);
            if !process.is_null() {
                let mut name = vec![0u16; 32768];
                let mut length = name.len() as u32;
                if QueryFullProcessImageNameW(process, 0, name.as_mut_ptr(), &mut length) != 0 && String::from_utf16_lossy(&name[..length as usize]).to_lowercase().starts_with(&wanted) {
                    found.push(entry.th32ProcessID);
                }
                CloseHandle(process);
            }
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    found
}

/// Ends what runs from the scratch folder, by number, and waits until it has gone. Only what this test made there: nothing else runs from a folder that was made for it.
fn end_what_runs_from(folder: &Path) {
    for pid in running_from(folder) {
        // SAFETY: the handle is this loop's.
        unsafe {
            let process = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid);
            if !process.is_null() {
                TerminateProcess(process, 1);
                WaitForSingleObject(process, 5000);
                CloseHandle(process);
            }
        }
    }
}

/// The scratch folder, and what it holds is ended with the test, whichever way it ends.
struct Scene {
    dir: tempfile::TempDir,
}

impl Scene {
    fn new() -> Scene {
        Scene { dir: tempfile::tempdir().unwrap() }
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn install_folder(&self) -> PathBuf {
        self.path().join("Programs").join("Dogmind")
    }
    /// A first install of this version, as the test switch makes one.
    fn installed(&self, version: &str) -> PathBuf {
        let folder = self.install_folder();
        let status = setup(&pack(self.path(), version)).args(["--test", "--auto", "--dir"]).arg(&folder).status().unwrap();
        assert_eq!(status.code(), Some(0));
        assert!(running_from(&folder).is_empty(), "an install does not start the app");
        folder
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        end_what_runs_from(self.dir.path());
    }
}

/// What runs from the folder, once the installer has had a moment to start it.
fn wait_for_the_app(folder: &Path) -> Vec<u32> {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let found = running_from(folder);
        if !found.is_empty() || Instant::now() > until {
            return found;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn version_of(folder: &Path) -> String {
    std::fs::read_to_string(folder.join("data").join("version.txt")).unwrap()
}

fn manifest_version(folder: &Path) -> String {
    let text = std::fs::read_to_string(folder.join("dogmind-install.txt")).unwrap();
    text.lines().find_map(|line| line.strip_prefix("version ")).unwrap().to_string()
}

/// A program that runs for a few seconds and is not the app's: what `--wait-pid` is given.
fn short_lived(scene: &Path) -> Child {
    let folder = scene.join("elsewhere");
    std::fs::create_dir_all(&folder).unwrap();
    let copy = folder.join("sleeper.exe");
    std::fs::write(&copy, command_interpreter()).unwrap();
    use std::os::windows::process::CommandExt;
    Command::new(copy).args(["/c", "ping -n 4 127.0.0.1 >nul"]).creation_flags(NO_WINDOW).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap()
}

#[test]
fn an_update_waits_for_the_app_replaces_its_files_and_starts_it_again() {
    let scene = Scene::new();
    let folder = scene.installed("1.0.0");
    assert_eq!((version_of(&folder), manifest_version(&folder)), ("1.0.0".to_string(), "1.0.0".to_string()));
    let second = pack(scene.path(), "2.0.0");
    let mut old_app = short_lived(scene.path());
    let began = Instant::now();
    let status = setup(&second).args(["--update", "--test", "--auto", "--dir"]).arg(&folder).args(["--wait-pid", &old_app.id().to_string()]).status().unwrap();
    assert_eq!(status.code(), Some(0));
    assert!(old_app.try_wait().unwrap().is_some(), "it waited for the program it was told of, and did not go on over it");
    assert!(began.elapsed() >= Duration::from_secs(1), "{:?}", began.elapsed());
    assert_eq!((version_of(&folder), manifest_version(&folder)), ("2.0.0".to_string(), "2.0.0".to_string()), "the files are the new version's");
    // the app is started again, from the folder, with no word of asking
    let started = wait_for_the_app(&folder);
    assert_eq!(started.len(), 1, "the app is running from the folder: {started:?}");
    end_what_runs_from(&folder);
    assert!(running_from(&folder).is_empty());
}

#[test]
fn an_update_that_cannot_finish_leaves_the_old_version_and_starts_it_as_it_was() {
    let scene = Scene::new();
    let folder = scene.installed("1.0.0");
    let second = pack(scene.path(), "2.0.0");
    // (a failure asked for by the test, on the first file it unpacks)
    let status = setup(&second).env("DOGMIND_SETUP_FAIL_AFTER", "0").args(["--update", "--test", "--auto", "--dir"]).arg(&folder).status().unwrap();
    assert_eq!(status.code(), Some(1), "it says so, and ends with a failure");
    assert_eq!((version_of(&folder), manifest_version(&folder)), ("1.0.0".to_string(), "1.0.0".to_string()));
    // what a person does on that page is start the app as it was: the old version, from the folder
    assert_eq!(wait_for_the_app(&folder).len(), 1, "the app is started as it was");
    assert_eq!(std::fs::read(folder.join(APP)).unwrap(), version_app("1.0.0"));
}

#[test]
fn a_folder_with_no_install_is_refused_and_left_as_it_is() {
    let scene = Scene::new();
    let empty = scene.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let second = pack(scene.path(), "2.0.0");
    let status = setup(&second).args(["--update", "--test", "--auto", "--dir"]).arg(&empty).status().unwrap();
    assert_eq!(status.code(), Some(1));
    assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0, "nothing was put there");
    // a folder that does not exist is no install either, and is not made
    let missing = scene.path().join("missing");
    let status = setup(&second).args(["--update", "--test", "--auto", "--dir"]).arg(&missing).status().unwrap();
    assert_eq!(status.code(), Some(1));
    assert!(!missing.exists());
}

#[test]
fn an_update_without_its_folder_is_refused_before_anything_starts() {
    let scene = Scene::new();
    let second = pack(scene.path(), "2.0.0");
    let status = setup(&second).args(["--update", "--test", "--auto"]).status().unwrap();
    assert_eq!(status.code(), Some(1));
}
