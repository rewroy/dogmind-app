//! Dogmind's installer: one executable with the package on its end, no administrator and nothing to install it with.
//!
//!     dogmind-setup --pack FOLDER OUT.exe [--version V] [--level 0-9]    makes DogmindSetup.exe from this program and FOLDER (scripts/package.py does)
//!     dogmind-setup --verify FILE.exe                                    unpacks all of it to nowhere, and says if every file is sound
//!     DogmindSetup.exe [--dir FOLDER]                                    installs, or updates what is there. It starts with FOLDER, else with the install Installed apps names if that is still
//!                                                                        there, else with %LOCALAPPDATA%\Programs\Dogmind; a first install can be moved to another folder on its first page
//!     DogmindSetup.exe --update --dir FOLDER [--wait-pid PID]...         what the app runs for an update it has downloaded: no question is asked. It waits for those
//!                                                                        programs to end, updates the install in FOLDER (which must hold one), and starts the app again
//!     dogmind-uninstall.exe --uninstall                                  what Installed apps runs: clears the way, and opens the app's own page for removing it
//!
//! `--test` is for trying it out: no real Start menu entry or registry key is written, what would be goes in files in the folder, `--dir` must be named, and only what runs from that
//! folder is looked at. With it `--auto` presses what a person would press, and the exit code says how it ended.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod install;
mod pack;
mod place;
mod system;
mod window;

use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub folder: PathBuf,
    pub test: bool,
    pub auto: bool,
    pub wait_pids: Vec<u32>,
    pub app_name: String,
    /// What a test asks to go wrong, or to go slowly enough to be seen.
    pub fail_after: Option<usize>,
    pub slowly: Duration,
    /// What a test has the folder picker answer, so that the page after a choice can be seen without anyone choosing: the picker is not opened.
    pub pick: Option<PathBuf>,
}

/// What the command line says.
#[derive(Clone, Debug, Default, PartialEq)]
struct Arguments {
    pack: Option<(PathBuf, PathBuf)>,
    verify: Option<PathBuf>,
    version: Option<String>,
    level: Option<u32>,
    dir: Option<PathBuf>,
    test: bool,
    auto: bool,
    uninstall: bool,
    after_remove: bool,
    finish_removal: bool,
    /// The app runs this for an update it has fetched: the install in `dir` is updated without a first page, and the app is started again after it.
    update: bool,
    wait_pids: Vec<u32>,
}

fn parse(mut words: impl Iterator<Item = std::ffi::OsString>) -> Result<Arguments, String> {
    let mut found = Arguments::default();
    while let Some(word) = words.next() {
        let word_text = word.to_string_lossy().into_owned();
        let mut value = |name: &str| words.next().ok_or_else(|| format!("{name} needs a value"));
        match word_text.as_str() {
            "--pack" => {
                let (folder, out) = (value("--pack")?, value("--pack")?);
                found.pack = Some((folder.into(), out.into()));
            }
            "--verify" => found.verify = Some(value("--verify")?.into()),
            "--version" => found.version = Some(value("--version")?.to_string_lossy().into_owned()),
            "--level" => found.level = Some(value("--level")?.to_string_lossy().parse().map_err(|_| "--level is a number from 0 to 9".to_string())?),
            "--dir" => found.dir = Some(value("--dir")?.into()),
            "--wait-pid" => found.wait_pids.push(value("--wait-pid")?.to_string_lossy().parse().map_err(|_| "--wait-pid is a number".to_string())?),
            "--test" => found.test = true,
            "--auto" => found.auto = true,
            "--uninstall" => found.uninstall = true,
            "--after-remove" => found.after_remove = true,
            "--finish-removal" => found.finish_removal = true,
            "--update" => found.update = true,
            other => return Err(format!("{other}: not something this understands")),
        }
    }
    Ok(found)
}

/// The test switch's checks: it cannot fall back on the real folder, or on another program's registry.
fn check(found: &Arguments) -> Result<(), String> {
    if found.test && found.dir.is_none() && !found.after_remove && !found.uninstall {
        return Err("--test needs --dir: it must not go in the real folder".to_string());
    }
    if found.auto && !found.test {
        return Err("--auto is for --test".to_string());
    }
    if found.update {
        if found.dir.is_none() {
            return Err("--update needs --dir: the folder of the install to update".to_string());
        }
        if found.pack.is_some() || found.verify.is_some() || found.uninstall || found.after_remove || found.finish_removal {
            return Err("--update goes with --dir, --wait-pid, --test and --auto, and with nothing else".to_string());
        }
    }
    Ok(())
}

/// What an update that was started wrongly says on a window: nobody reads a command line the app started this with.
fn refuse_the_update(found: &Arguments, words: &str) -> i32 {
    let options = Options { folder: found.dir.clone().unwrap_or_default(), test: found.test, ..Options::default() };
    let japanese = window::japanese(&options);
    let detail = match (found.dir.is_none(), japanese) {
        (true, true) => "更新するインストール先のフォルダが指定されていません。".to_string(),
        (true, false) => "No folder was named for the install to update.".to_string(),
        (false, true) => "この更新は、使えない引数で起動されました。".to_string(),
        (false, false) => format!("This update was started with arguments that cannot go together ({words})."),
    };
    window::run(options, window::Start::Refuse((if japanese { "Dogmind を更新できません" } else { "Dogmind cannot be updated" }).to_string(), detail));
    1
}

/// Windows's own: the entries, or under the test switch, or for an install made with it, what stands in for them.
pub fn shell_for(options: &Options, folder: &Path) -> Box<dyn install::Shell + Send> {
    if options.test || install::read_manifest(folder).is_some_and(|manifest| manifest.test) {
        Box::new(system::TestShell { folder: folder.to_path_buf() })
    } else {
        Box::new(system::RealShell { folder: folder.to_path_buf() })
    }
}

/// This program, without a package on its end, at `path`.
pub fn write_stub(from: &Path, path: &Path) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut source = std::fs::File::open(from)?;
    let length = pack::overlay_start(&mut source)?;
    source.seek(SeekFrom::Start(0))?;
    let partial = path.with_extension("dogmind-new");
    let mut out = std::fs::File::create(&partial)?;
    std::io::copy(&mut source.take(length), &mut out)?;
    drop(out);
    std::fs::rename(partial, path)
}

/// In a window program a word to the command line goes to where the caller put its error stream, when it did.
fn fail(words: &str) -> i32 {
    eprintln!("{words}");
    1
}

fn do_pack(folder: &Path, out: &Path, version: &str, level: u32) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return fail(&error.to_string()),
    };
    match pack::pack(&exe, folder, out, version, level.min(9), &mut |_| {}) {
        Ok(size) => {
            let length = std::fs::metadata(out).map_or(0, |data| data.len());
            println!("{}: {} files, {:.0} MB in them, {:.0} MB packed, {:.0} MB in all", out.display(), size.files, size.bytes as f64 / 1e6, size.stored as f64 / 1e6, length as f64 / 1e6);
            0
        }
        Err(error) => fail(&format!("{}: {error}", out.display())),
    }
}

fn do_verify(file: &Path) -> i32 {
    let mut package = match pack::Package::open(file) {
        Ok(package) => package,
        Err(error) => return fail(&format!("{}: {error}", file.display())),
    };
    for entry in package.entries.clone() {
        if let Err(error) = package.extract(&entry, &mut std::io::sink(), &mut |_| {}) {
            return fail(&error.to_string());
        }
    }
    println!("{}: version {}, {} files, all as they were packed", file.display(), package.version, package.entries.len());
    0
}

/// After the app's own page: this program can't take away the folder it runs from, so a copy elsewhere does, once this and the app have ended.
fn hand_over_to_a_copy(found: &Arguments, folder: &Path, test: bool) -> i32 {
    let Ok(exe) = std::env::current_exe() else { return 1 };
    let copy = std::env::temp_dir().join(format!("dogmind-remove-{}.exe", std::process::id()));
    if write_stub(&exe, &copy).is_err() {
        return 1;
    }
    let mut command = std::process::Command::new(&copy);
    command.arg("--finish-removal").arg("--dir").arg(folder).arg("--wait-pid").arg(std::process::id().to_string());
    for pid in &found.wait_pids {
        command.arg("--wait-pid").arg(pid.to_string());
    }
    if test {
        command.arg("--test");
    }
    i32::from(command.spawn().is_err())
}

/// A copy in the temporary folder, once nothing more needs it: it cannot take itself away while it runs.
fn remove_this_copy_afterwards() {
    use std::os::windows::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else { return };
    let line = format!("/c ping -n 3 127.0.0.1 >nul & del /f /q \"{}\"", exe.display());
    let _ = std::process::Command::new("cmd.exe").raw_arg(line).creation_flags(0x0800_0000).spawn(); // CREATE_NO_WINDOW
}

fn number_from(name: &str, test: bool) -> Option<u64> {
    if test { std::env::var(name).ok().and_then(|value| value.parse().ok()) } else { None }
}

fn main() {
    let found = match parse(std::env::args_os().skip(1)) {
        Ok(found) => found,
        Err(words) => std::process::exit(fail(&words)),
    };
    if let Err(words) = check(&found) {
        // (an update is started by the app, which reads no command line: it is said on a window; a test run says it where its caller reads)
        std::process::exit(if found.update && !found.test { refuse_the_update(&found, &words) } else { fail(&words) });
    }
    let version = found.version.clone().unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    if let Some((folder, out)) = &found.pack {
        std::process::exit(do_pack(folder, out, &version, found.level.unwrap_or(9)));
    }
    if let Some(file) = &found.verify {
        std::process::exit(do_verify(file));
    }
    let exe_folder = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.to_path_buf()));
    let removing = found.uninstall || found.after_remove || found.finish_removal;
    let folder = found.dir.clone().or_else(|| if removing { exe_folder } else { place::existing_install(system::registered_folder().as_deref(), system::default_folder()) });
    let Some(folder) = folder else { std::process::exit(fail("no folder to work in: name one with --dir")) };
    let test = found.test || (removing && install::read_manifest(&folder).is_some_and(|manifest| manifest.test));
    if found.after_remove {
        std::process::exit(hand_over_to_a_copy(&found, &folder, test));
    }
    let options = Options {
        folder: folder.clone(),
        test,
        auto: found.auto,
        wait_pids: found.wait_pids.clone(),
        app_name: if test { std::env::var("DOGMIND_SETUP_APP_NAME").unwrap_or_else(|_| "dogmind-app.exe".to_string()) } else { "dogmind-app.exe".to_string() },
        fail_after: number_from("DOGMIND_SETUP_FAIL_AFTER", test).map(|count| count as usize),
        slowly: Duration::from_millis(number_from("DOGMIND_SETUP_SLOW_MS", test).unwrap_or(0)),
        pick: if test { std::env::var_os("DOGMIND_SETUP_PICK").map(PathBuf::from) } else { None },
    };
    let start = if found.finish_removal {
        window::Start::FinishRemoval
    } else if found.uninstall {
        window::Start::Uninstall
    } else {
        start_of_install(&options, found.update)
    };
    let code = window::run(options, start);
    if found.finish_removal {
        remove_this_copy_afterwards();
    }
    std::process::exit(code);
}

/// The first thing the window shows: the install's page, or, for `--update`, the work itself, which has no page before it and must find an install to work on.
fn start_of_install(options: &Options, updating: bool) -> window::Start {
    let japanese = window::japanese(options);
    let words = |english: &str, in_japanese: &str| (if japanese { in_japanese } else { english }).to_string();
    if system::windows_build().is_some_and(|build| build < 22000) {
        return window::Start::Refuse(words("Dogmind needs Windows 11", "Dogmind には Windows 11 が必要です"), words("This version of Windows is older.", "この Windows はそれより前のバージョンです。"));
    }
    let previous = install::read_manifest(&options.folder).map(|manifest| manifest.version);
    if updating && previous.is_none() {
        let folder = options.folder.display();
        return window::Start::Refuse(
            words("Dogmind cannot be updated", "Dogmind を更新できません"),
            if japanese { format!("{folder} には Dogmind のインストールがないため、更新するものがありません。") } else { format!("{folder} holds no install of Dogmind, so there is nothing to update.") },
        );
    }
    let package = std::env::current_exe().and_then(|exe| pack::Package::open(&exe));
    match (package, previous) {
        (Ok(package), Some(previous)) if updating => window::Start::Update { previous, version: package.version },
        (Ok(package), previous) => window::Start::Install { previous, version: package.version },
        (Err(error), _) => window::Start::Refuse(words("This file cannot be used", "このファイルは使えません"), if japanese { format!("{error}。もう一度ダウンロードしてください。") } else { format!("{error}. Download it again.") }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Result<Arguments, String> {
        parse(words.iter().map(std::ffi::OsString::from))
    }

    #[test]
    fn the_command_line_is_read_and_checked() {
        let found = args(&["--test", "--dir", "C:\\x y", "--auto"]).unwrap();
        assert!(found.test && found.auto);
        assert_eq!(found.dir, Some(PathBuf::from("C:\\x y")));
        assert!(check(&found).is_ok());
        assert_eq!(args(&["--pack", "a", "b", "--level", "3", "--version", "9"]).unwrap().pack, Some((PathBuf::from("a"), PathBuf::from("b"))));
        assert!(args(&["--pack", "a"]).is_err());
        assert!(args(&["--whatever"]).is_err());
        assert_eq!(args(&["--wait-pid", "5", "--wait-pid", "6"]).unwrap().wait_pids, [5, 6]);
    }

    #[test]
    fn an_update_names_its_folder_and_is_for_nothing_else() {
        assert!(args(&["--update"]).unwrap().update);
        assert!(check(&args(&["--update"]).unwrap()).unwrap_err().contains("--dir"), "an update with no folder has nothing to update");
        assert!(check(&args(&["--update", "--wait-pid", "5"]).unwrap()).is_err());
        let update = args(&["--update", "--dir", "C:\\x y", "--wait-pid", "5", "--wait-pid", "6"]).unwrap();
        assert!(check(&update).is_ok());
        assert_eq!((update.wait_pids.clone(), update.dir.clone()), (vec![5, 6], Some(PathBuf::from("C:\\x y"))));
        // under the test switch as it is for an install: a scratch folder, and the person's screen left alone
        assert!(check(&args(&["--update", "--test", "--dir", "x", "--auto"]).unwrap()).is_ok());
        assert!(check(&args(&["--update", "--test"]).unwrap()).is_err());
        for other in [&["--uninstall"][..], &["--after-remove"], &["--finish-removal"], &["--verify", "a.exe"], &["--pack", "a", "b"]] {
            let mut words = vec!["--update", "--dir", "x"];
            words.extend_from_slice(other);
            assert!(check(&args(&words).unwrap()).unwrap_err().contains("--update"), "{other:?}");
        }
    }

    #[test]
    fn the_test_switch_never_falls_on_the_real_folder() {
        assert!(check(&args(&["--test"]).unwrap()).unwrap_err().contains("--dir"));
        assert!(check(&args(&["--auto", "--dir", "x"]).unwrap()).is_err(), "pressing by itself is only for a test");
        let dir = tempfile::tempdir().unwrap();
        assert!(install::read_manifest(dir.path()).is_none());
        // An install made with it stands for its own removal too, whoever starts that.
        std::fs::write(dir.path().join(install::MANIFEST), "version 1\ntest\nfile a\n").unwrap();
        assert!(install::read_manifest(dir.path()).unwrap().test);
    }

    #[test]
    fn the_uninstaller_is_this_program_without_a_package() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("in");
        pack::tests::tree(&folder, &[("a", b"1")]);
        let setup = dir.path().join("setup.exe");
        pack::pack(&pack::tests::own_exe(), &folder, &setup, "1", 6, &mut |_| {}).unwrap();
        let stub = dir.path().join("stub.exe");
        write_stub(&setup, &stub).unwrap();
        assert!(pack::Package::open(&stub).is_err(), "no package on it");
        assert_eq!(std::fs::metadata(&stub).unwrap().len(), pack::overlay_start(&mut std::fs::File::open(&stub).unwrap()).unwrap());
    }

    #[test]
    fn a_large_file_is_streamed_and_not_held() {
        // 64 MB through a package with memory no larger than a block: `pack` and `extract` have no buffer that grows with what passes, and this says only that all of it comes back.
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("in");
        std::fs::create_dir_all(&folder).unwrap();
        let sample = pack::tests::noise(1 << 16);
        {
            use std::io::Write;
            let mut out = std::fs::File::create(folder.join("large")).unwrap();
            for at in 0..1024u32 {
                out.write_all(&sample).unwrap();
                out.write_all(&at.to_le_bytes()).unwrap();
            }
        }
        let setup = dir.path().join("setup.exe");
        pack::pack(&pack::tests::own_exe(), &folder, &setup, "1", 1, &mut |_| {}).unwrap();
        assert_eq!(do_verify(&setup), 0);
    }
}
