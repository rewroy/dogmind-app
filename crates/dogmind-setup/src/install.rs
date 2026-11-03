//! Putting a package in a folder, and taking it out again. What a person keeps is never in the way: settings and sessions are in another folder altogether, and in this one only what
//! the last install put there is ever taken away.
//!
//! Every file is unpacked beside its place, under a name of its own, and checked by its hash. Only when all of them are there do they take their places, so that a failure before that
//! leaves the program as it was. A file made there has what its folder gives new files, which matters: the sandbox's entries on `tools\` are on each thing in it and inherited by what comes.
//! Running it again after any failure goes on: what is in place, or waiting beside its place, with the right hash, is not unpacked again.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::pack::{hash_file, Entry, Package};

/// What says what an install put in a folder, and of which version.
pub const MANIFEST: &str = "dogmind-install.txt";
pub const UNINSTALLER: &str = "dogmind-uninstall.exe";
const WAITING: &str = ".dogmind-new";
/// A little more than the files, for the drive's own bookkeeping.
const ROOM_TO_SPARE: u64 = 64 << 20;

/// What is said of a Start menu entry and an Add or remove programs entry, by whichever does them.
pub trait Shell {
    fn add_entries(&self, folder: &Path, version: &str, kilobytes: u64) -> Result<(), String>;
    fn remove_entries(&self) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    Checking,
    Unpacking,
    Placing,
    Finishing,
}

/// How far along, for the window.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub stage: Stage,
    pub fraction: f64,
    pub file: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// The person stopped it.
    Stopped,
    /// In one line, for the person: what was done stays, and running it again goes on.
    Said(String),
    /// A file could not be replaced, as something has it open.
    InUse(String),
    TooLittleRoom { needed: u64, free: u64 },
}

/// What a finished install was.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Installed {
    pub unpacked: usize,
    pub already_there: usize,
    pub removed: usize,
}

fn beside(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(WAITING);
    target.with_file_name(name)
}

/// What a folder holds of an install.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    pub version: String,
    /// Made with the test switch: nothing real was registered for it, and nothing real is taken away for it.
    pub test: bool,
    pub files: Vec<String>,
}

pub fn read_manifest(folder: &Path) -> Option<Manifest> {
    let text = std::fs::read_to_string(folder.join(MANIFEST)).ok()?;
    let mut manifest = Manifest::default();
    for line in text.lines() {
        match line.split_once(' ') {
            Some(("version", version)) => manifest.version = version.to_string(),
            Some(("file", file)) if crate::pack::is_safe(file) => manifest.files.push(file.to_string()),
            _ if line == "test" => manifest.test = true,
            _ => {}
        }
    }
    Some(manifest)
}

fn write_manifest(folder: &Path, manifest: &Manifest) -> std::io::Result<()> {
    let mut text = format!("version {}\n", manifest.version);
    if manifest.test {
        text.push_str("test\n");
    }
    for file in &manifest.files {
        text.push_str(&format!("file {file}\n"));
    }
    let partial = folder.join(format!("{MANIFEST}{WAITING}"));
    // (on the disk before its name changes: after a power cut a rename can stand on nothing, and then no file here is known to be this program's)
    let mut out = std::fs::File::create(&partial)?;
    std::io::Write::write_all(&mut out, text.as_bytes())?;
    out.sync_all()?;
    drop(out);
    std::fs::rename(&partial, folder.join(MANIFEST))
}

/// A virus scanner or the search index may have a new file open for a moment.
fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut tries = 0;
    loop {
        match std::fs::rename(from, to) {
            Err(error) if tries < 5 && matches!(error.raw_os_error(), Some(5 | 32)) => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(300));
            }
            result => return result,
        }
    }
}

fn say(what: &Path, error: &std::io::Error) -> Failure {
    Failure::Said(format!("{}: {error}", what.display()))
}

/// Everything an install asks of a machine, for tests to hold back.
pub struct Install<'a> {
    pub folder: &'a Path,
    pub shell: &'a dyn Shell,
    /// The free bytes of the drive the folder is on, when known.
    pub free: Option<u64>,
    pub stop: &'a AtomicBool,
    pub test: bool,
    /// Writes the program that takes it all away, as `dogmind-uninstall.exe`.
    pub write_uninstaller: &'a dyn Fn(&Path) -> std::io::Result<()>,
    /// A test's failure, after this many files have been unpacked.
    pub fail_after: Option<usize>,
    /// A test's slowness, for a capture of it at work, after each file.
    pub slowly: Duration,
}

/// The package in the folder, from nothing or over an earlier install, going on from where another run stopped.
pub fn install(package: &mut Package, job: &Install, on: &mut dyn FnMut(Progress)) -> Result<Installed, Failure> {
    let folder = job.folder;
    std::fs::create_dir_all(folder).map_err(|error| say(folder, &error))?;
    let entries = package.entries.clone();
    let total = package.total_bytes().max(1);
    // What will not fit is said before anything is written. What is there already at this size will most likely stay as it is.
    let needed: u64 = entries.iter().filter(|entry| std::fs::metadata(folder.join(&entry.path)).map_or(true, |data| data.len() != entry.size) && std::fs::metadata(beside(&folder.join(&entry.path))).map_or(true, |data| data.len() != entry.size)).map(|entry| entry.size).sum();
    if let Some(free) = job.free {
        if needed + ROOM_TO_SPARE > free {
            return Err(Failure::TooLittleRoom { needed: needed + ROOM_TO_SPARE, free });
        }
    }
    // What is in the way and is no install's. The folder can be any the person names, and what this is about to do is put its own file in that place and, one day, take it away.
    let previous = read_manifest(folder);
    let ours: HashSet<String> = previous.iter().flat_map(|manifest| manifest.files.iter()).map(|file| file.to_lowercase()).collect();
    for entry in &entries {
        let target = folder.join(&entry.path);
        if !ours.contains(&entry.path.to_lowercase()) && target.exists() && !hash_file(&target, &mut |_| {}).is_ok_and(|hash| hash == entry.sha256) {
            return Err(Failure::Said(format!("{}: there is a file of that name in this folder already, which no install put there, so nothing has been changed. Move it, or install in another folder.", target.display())));
        }
    }
    let mut done = 0u64;
    let mut outcome = Installed::default();
    let mut unpacked_now = 0;
    // Unpack, or find it there.
    let mut to_place: Vec<&Entry> = Vec::new();
    for entry in &entries {
        if job.stop.load(Ordering::Relaxed) {
            return Err(Failure::Stopped);
        }
        let target = folder.join(&entry.path);
        let waiting = beside(&target);
        let parts = done as f64 / total as f64;
        let same = |path: &Path, on: &mut dyn FnMut(Progress)| -> bool {
            if std::fs::metadata(path).map_or(true, |data| data.len() != entry.size) {
                return false;
            }
            on(Progress { stage: Stage::Checking, fraction: parts, file: entry.path.clone() });
            hash_file(path, &mut |_| {}).is_ok_and(|hash| hash == entry.sha256)
        };
        if same(&waiting, on) {
            to_place.push(entry);
            outcome.unpacked += 1;
        } else if same(&target, on) {
            let _ = std::fs::remove_file(&waiting);
            outcome.already_there += 1;
        } else {
            if job.fail_after.is_some_and(|after| unpacked_now >= after) {
                return Err(Failure::Said(format!("{}: a failure asked for by a test", entry.path)));
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|error| say(parent, &error))?;
            }
            let file = std::fs::File::create(&waiting).map_err(|error| say(&waiting, &error))?;
            let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
            let mut so_far = done;
            let stop = job.stop;
            let result = package.extract(entry, &mut out, &mut |count| {
                so_far += count;
                on(Progress { stage: Stage::Unpacking, fraction: so_far as f64 / total as f64, file: entry.path.clone() });
            });
            let flushed = result.and_then(|()| std::io::Write::flush(&mut out));
            drop(out);
            if let Err(error) = flushed {
                let _ = std::fs::remove_file(&waiting);
                return Err(say(&target, &error));
            }
            if stop.load(Ordering::Relaxed) {
                return Err(Failure::Stopped);
            }
            to_place.push(entry);
            outcome.unpacked += 1;
            unpacked_now += 1;
            if !job.slowly.is_zero() {
                std::thread::sleep(job.slowly);
            }
        }
        done += entry.size;
        on(Progress { stage: Stage::Unpacking, fraction: done as f64 / total as f64, file: entry.path.clone() });
    }
    // All are here and sound. From now on nothing is stopped half way by the person: what is left would be half of two versions.
    // What is about to be here is written down first, under the version that is here: from a failure from now on, a removal takes what came of this too.
    let mut going_on: Vec<String> = previous.iter().flat_map(|manifest| manifest.files.iter().cloned()).collect();
    going_on.extend(entries.iter().filter(|entry| !ours.contains(&entry.path.to_lowercase())).map(|entry| entry.path.clone()));
    let version_here = previous.as_ref().map_or_else(|| package.version.clone(), |manifest| manifest.version.clone());
    write_manifest(folder, &Manifest { version: version_here, test: previous.as_ref().map_or(job.test, |manifest| manifest.test), files: going_on }).map_err(|error| say(&folder.join(MANIFEST), &error))?;
    let moved = place_all(folder, &to_place, on)?;
    // (one a program still runs from cannot go: it stays, and goes with the next install)
    for (_, aside) in moved {
        let _ = std::fs::remove_file(aside);
    }
    on(Progress { stage: Stage::Finishing, fraction: 1.0, file: String::new() });
    // What the last version had and this one has not: nothing else in the folder is this program's to take.
    let mut new_files: HashSet<String> = entries.iter().map(|entry| entry.path.to_lowercase()).collect();
    new_files.insert(UNINSTALLER.to_lowercase());
    if let Some(previous) = &previous {
        for file in previous.files.iter().filter(|file| !new_files.contains(&file.to_lowercase()) && !through_a_link(folder, file)) {
            if std::fs::remove_file(folder.join(file)).is_ok() {
                outcome.removed += 1;
            }
        }
        remove_empty_folders(folder, previous.files.iter().map(String::as_str));
    }
    // (copies of what this version has, or the last one had: the last one's own may have been moved aside before it went)
    let ours: HashSet<String> = new_files.iter().cloned().chain(previous.iter().flat_map(|manifest| manifest.files.iter().map(|file| file.to_lowercase()))).collect();
    remove_old_copies(folder, &ours);
    (job.write_uninstaller)(&folder.join(UNINSTALLER)).map_err(|error| say(&folder.join(UNINSTALLER), &error))?;
    let mut files: Vec<String> = entries.iter().map(|entry| entry.path.clone()).collect();
    files.push(UNINSTALLER.to_string());
    write_manifest(folder, &Manifest { version: package.version.clone(), test: job.test, files }).map_err(|error| say(&folder.join(MANIFEST), &error))?;
    job.shell.add_entries(folder, &package.version, package.total_bytes() / 1024).map_err(Failure::Said)?;
    Ok(outcome)
}

/// Every file waiting beside its place put in it, or none: half of two versions would run as neither. First what is there now is moved
/// aside, each under a name of its own (Windows lets a program's file be renamed while it runs, and it runs on from there); then each new
/// one comes into its place. Anything that fails on the way puts back what was moved, and what came goes back to waiting beside its
/// place: the program is as it was, and running again goes on. What was moved aside, by where it stood, for the caller to take away.
fn place_all(folder: &Path, to_place: &[&Entry], on: &mut dyn FnMut(Progress)) -> Result<Vec<(PathBuf, PathBuf)>, Failure> {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs());
    let steps = (to_place.len() * 2).max(1) as f64;
    let (mut moved, mut placed): (Vec<(PathBuf, PathBuf)>, Vec<PathBuf>) = (Vec::new(), Vec::new());
    let failed = |moved: &[(PathBuf, PathBuf)], placed: &[PathBuf], file: &str, target: &Path, error: std::io::Error| {
        for done in placed.iter().rev() {
            let _ = replace(done, &beside(done));
        }
        for (target, aside) in moved.iter().rev() {
            let _ = replace(aside, target);
        }
        if matches!(error.raw_os_error(), Some(5 | 32)) { Failure::InUse(file.to_string()) } else { say(target, &error) }
    };
    for (at, entry) in to_place.iter().enumerate() {
        let target = folder.join(&entry.path);
        on(Progress { stage: Stage::Placing, fraction: at as f64 / steps, file: entry.path.clone() });
        if std::fs::symlink_metadata(&target).is_err() {
            continue;
        }
        let aside = aside_of(&target, stamp);
        if let Err(error) = replace(&target, &aside) {
            return Err(failed(&moved, &placed, &entry.path, &target, error));
        }
        moved.push((target, aside));
    }
    for (at, entry) in to_place.iter().enumerate() {
        let target = folder.join(&entry.path);
        on(Progress { stage: Stage::Placing, fraction: (to_place.len() + at) as f64 / steps, file: entry.path.clone() });
        #[cfg(test)]
        let refused = tests::REFUSE_PLACING.with(|refused| refused.borrow().as_deref() == Some(entry.path.as_str()));
        #[cfg(not(test))]
        let refused = false;
        let result = if refused { Err(std::io::Error::from_raw_os_error(32)) } else { replace(&beside(&target), &target) };
        if let Err(error) = result {
            return Err(failed(&moved, &placed, &entry.path, &target, error));
        }
        placed.push(target);
    }
    Ok(moved)
}

/// The name a file is moved aside under: `NAME.previous-TIME.EXT`, as the app's own new builds do, and one no file has yet.
fn aside_of(target: &Path, stamp: u64) -> PathBuf {
    let stem = target.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let extension = target.extension().map(|extension| format!(".{}", extension.to_string_lossy())).unwrap_or_default();
    (stamp..).map(|time| target.with_file_name(format!("{stem}.previous-{time}{extension}"))).find(|path| std::fs::symlink_metadata(path).is_err()).unwrap_or_default()
}

/// Whether the way to this file from the folder goes through a link. What it leads to is not in the folder, whatever the path says, and is not this program's to take.
fn through_a_link(folder: &Path, file: &str) -> bool {
    let mut place = folder.to_path_buf();
    let mut parts = file.split('\\').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return false;
        }
        place.push(part);
        if std::fs::symlink_metadata(&place).is_ok_and(|what| what.file_type().is_symlink()) {
            return true;
        }
    }
    false
}

/// The file a copy moved aside stood for: `NAME.previous-TIME.EXT` stood for `NAME.EXT`, TIME being digits. None for any other name.
fn stood_for(name: &str) -> Option<String> {
    let (stem, rest) = name.split_once(".previous-")?;
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    let extension = &rest[digits..];
    (!stem.is_empty() && digits > 0 && (extension.is_empty() || extension.starts_with('.'))).then(|| format!("{stem}{extension}"))
}

/// What was left beside the app's files when they were replaced while it ran from them (`NAME.previous-TIME.EXT`), in its folder and in fonts:
/// a copy of one of `ours` (the install's files, as the manifest names them, in lower case), and nothing else. A person's file of such a name stays.
fn remove_old_copies(folder: &Path, ours: &HashSet<String>) {
    for (place, within) in [(folder.to_path_buf(), ""), (folder.join("fonts"), "fonts\\")] {
        for entry in std::fs::read_dir(&place).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if stood_for(&name).is_some_and(|original| ours.contains(&format!("{within}{original}").to_lowercase())) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// What a stopped install has left beside its places, taken away: a person who says stop does not expect a gigabyte to stay. The folder goes too if that leaves it empty.
pub fn clean_up_waiting(package: &Package, folder: &Path) {
    for entry in &package.entries {
        let _ = std::fs::remove_file(beside(&folder.join(&entry.path)));
    }
    remove_empty_folders(folder, package.entries.iter().map(|entry| entry.path.as_str()));
    let _ = std::fs::remove_dir(folder);
}

/// The folders these files were in and any above them, deepest first, those with nothing left in them.
fn remove_empty_folders<'a>(folder: &Path, files: impl Iterator<Item = &'a str>) {
    let mut folders: Vec<PathBuf> = Vec::new();
    let mut seen = HashSet::new();
    for file in files {
        let mut place = Path::new(file).parent();
        while let Some(part) = place.filter(|part| !part.as_os_str().is_empty()) {
            if seen.insert(part.to_path_buf()) {
                folders.push(folder.join(part));
            }
            place = part.parent();
        }
    }
    folders.sort_by_key(|place| std::cmp::Reverse(place.components().count()));
    for place in folders {
        let _ = std::fs::remove_dir(place);
    }
}

/// What the folder still holds that this program did not put there, after it took away what it did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Removed {
    pub left: Vec<String>,
}

/// The install in this folder taken out of it: what its manifest says, and only that, with what the program itself makes there. Whatever else a person put there stays, and is said.
pub fn remove(folder: &Path, shell: &dyn Shell, on: &mut dyn FnMut(f64)) -> Result<Removed, String> {
    let Some(manifest) = read_manifest(folder) else { return Err(format!("{} has no install in it", folder.display())) };
    let total = manifest.files.len().max(1);
    let mut problems = Vec::new();
    for (at, file) in manifest.files.iter().enumerate() {
        on(at as f64 / total as f64);
        if through_a_link(folder, file) {
            continue;
        }
        let path = folder.join(file);
        let _ = std::fs::remove_file(beside(&path));
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => problems.push(format!("{}: {error}", path.display())),
        }
    }
    // (made there by the app and its tools, and of no use without them)
    for (root, name) in [("tools", ".open-to-sandbox")] {
        let _ = std::fs::remove_file(folder.join(root).join(name));
    }
    remove_old_copies(folder, &manifest.files.iter().map(|file| file.to_lowercase()).collect());
    remove_empty_folders(folder, manifest.files.iter().map(String::as_str));
    if problems.is_empty() {
        // (the entries first: with the manifest gone, nothing could take them away again, and they would stand for a program that is not there)
        shell.remove_entries()?;
        let _ = std::fs::remove_file(folder.join(MANIFEST));
    } else {
        // (a file still in use: the entries stay, so that it can be done again)
        return Err(problems.join("\n"));
    }
    let left: Vec<String> = std::fs::read_dir(folder).into_iter().flatten().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
    if left.is_empty() {
        let _ = std::fs::remove_dir(folder);
    }
    on(1.0);
    Ok(Removed { left })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::tests::{noise, own_exe, tree};
    use crate::pack::pack;
    use std::cell::RefCell;

    thread_local! {
        /// The file whose coming into its place fails, as if something had opened its place in the moment between: for this thread's install.
        pub static REFUSE_PLACING: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    /// The names in a folder that something was moved aside under.
    fn moved_aside(folder: &Path) -> Vec<String> {
        walk(folder).into_iter().filter(|name| name.contains(".previous-")).collect()
    }

    #[derive(Default)]
    struct FakeShell {
        said: RefCell<Vec<String>>,
        /// Its entries cannot be taken away (a key the registry will not give up).
        stuck: std::cell::Cell<bool>,
    }

    impl Shell for FakeShell {
        fn add_entries(&self, folder: &Path, version: &str, kilobytes: u64) -> Result<(), String> {
            self.said.borrow_mut().push(format!("add {} {version} {kilobytes}", folder.display()));
            Ok(())
        }
        fn remove_entries(&self) -> Result<(), String> {
            if self.stuck.get() {
                return Err("the entry could not be removed".into());
            }
            self.said.borrow_mut().push("remove".to_string());
            Ok(())
        }
    }

    /// Entries that cannot be taken away leave the manifest, so that the removal can be done again and take them then.
    #[test]
    fn a_removal_whose_entries_stay_keeps_what_can_remove_them_again() {
        let scene = Scene::new();
        let mut package = scene.package("1.0.0", &[("dogmind-app.exe", b"app")]);
        scene.run(&mut package, None, None).unwrap();
        let folder = scene.folder();
        scene.shell.stuck.set(true);
        assert!(remove(&folder, &scene.shell, &mut |_| {}).is_err());
        assert!(read_manifest(&folder).is_some(), "the manifest stays with the entries");
        scene.shell.stuck.set(false);
        remove(&folder, &scene.shell, &mut |_| {}).unwrap();
        assert!(read_manifest(&folder).is_none() && scene.shell.said.borrow().last().unwrap() == "remove");
    }

    struct Scene {
        dir: tempfile::TempDir,
        shell: FakeShell,
        stop: AtomicBool,
    }

    impl Scene {
        fn new() -> Scene {
            Scene { dir: tempfile::tempdir().unwrap(), shell: FakeShell::default(), stop: AtomicBool::new(false) }
        }
        fn folder(&self) -> PathBuf {
            self.dir.path().join("Programs").join("Dogmind")
        }
        fn package(&self, version: &str, files: &[(&str, &[u8])]) -> Package {
            let source = self.dir.path().join(format!("in-{version}"));
            tree(&source, files);
            let out = self.dir.path().join(format!("setup-{version}.exe"));
            pack(&own_exe(), &source, &out, version, 6, &mut |_| {}).unwrap();
            Package::open(&out).unwrap()
        }
        fn run(&self, package: &mut Package, free: Option<u64>, fail_after: Option<usize>) -> Result<Installed, Failure> {
            let folder = self.folder();
            let job = Install { folder: &folder, shell: &self.shell, free, stop: &self.stop, test: true, write_uninstaller: &|path| std::fs::write(path, "uninstaller"), fail_after, slowly: Duration::ZERO };
            install(package, &job, &mut |_| {})
        }
    }

    #[test]
    fn a_first_install_puts_everything_in_place_and_registers_once() {
        let scene = Scene::new();
        let mut package = scene.package("1.0.0", &[("dogmind-app.exe", b"app"), ("tools\\python\\python.exe", b"py"), ("\u{65e5}\u{672c}\\x", b"j")]);
        let done = scene.run(&mut package, None, None).unwrap();
        assert_eq!(done, Installed { unpacked: 3, already_there: 0, removed: 0 });
        let folder = scene.folder();
        assert_eq!(std::fs::read(folder.join("tools\\python\\python.exe")).unwrap(), b"py");
        assert_eq!(std::fs::read(folder.join(UNINSTALLER)).unwrap(), b"uninstaller");
        let manifest = read_manifest(&folder).unwrap();
        assert_eq!(manifest.version, "1.0.0");
        assert!(manifest.test && manifest.files.contains(&UNINSTALLER.to_string()));
        assert_eq!(scene.shell.said.borrow().len(), 1);
        assert!(scene.shell.said.borrow()[0].starts_with("add ") && scene.shell.said.borrow()[0].contains(" 1.0.0 "));
        assert!(walk(&folder).iter().all(|name| !name.ends_with(WAITING)), "nothing left waiting");
    }

    fn walk(folder: &Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut waiting = vec![folder.to_path_buf()];
        while let Some(current) = waiting.pop() {
            for entry in std::fs::read_dir(&current).into_iter().flatten().flatten() {
                if entry.path().is_dir() {
                    waiting.push(entry.path());
                } else {
                    found.push(entry.path().strip_prefix(folder).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        found.sort();
        found
    }

    #[test]
    fn an_update_replaces_what_changed_keeps_what_is_a_persons_and_takes_what_went() {
        let scene = Scene::new();
        let mut first = scene.package("1.0.0", &[("dogmind-app.exe", b"app one"), ("same.dll", b"unchanged"), ("tools\\old\\gone.py", b"old"), ("tools\\keep.py", b"keep")]);
        scene.run(&mut first, None, None).unwrap();
        let folder = scene.folder();
        // What the machine and the person have added since.
        tree(&folder, &[("tools\\.open-to-sandbox", b""), ("notes.txt", b"mine"), ("notes.previous-1.txt", b"mine too"), ("dogmind-app.previous-1.exe", b"old copy"), ("same.previous-163000.dll", b"old library")]);
        // The person's data is elsewhere: it is what the data folder holds, and no part of this comes near it.
        let data = scene.dir.path().join("DogmindApp");
        tree(&data, &[("settings.json", b"{}"), ("sessions\\one.jsonl", b"s"), ("models\\m.gguf", b"m")]);

        let mut second = scene.package("2.0.0", &[("dogmind-app.exe", b"app two"), ("same.dll", b"unchanged"), ("tools\\keep.py", b"keep"), ("tools\\new\\added.py", b"new")]);
        let done = scene.run(&mut second, None, None).unwrap();
        assert_eq!(done, Installed { unpacked: 2, already_there: 2, removed: 1 });
        assert_eq!(std::fs::read(folder.join("dogmind-app.exe")).unwrap(), b"app two");
        assert_eq!(std::fs::read(folder.join("tools\\new\\added.py")).unwrap(), b"new");
        assert!(!folder.join("tools\\old").exists(), "what is gone from the package goes, with its folder");
        assert_eq!(std::fs::read(folder.join("notes.txt")).unwrap(), b"mine", "a person's file stays");
        assert!(folder.join("tools\\.open-to-sandbox").exists(), "and what says the folder is open: its entries are still on it, and new files inherit them");
        assert!(!folder.join("dogmind-app.previous-1.exe").exists() && !folder.join("same.previous-163000.dll").exists(), "a copy of the program's own file, moved aside, goes");
        assert_eq!(std::fs::read(folder.join("notes.previous-1.txt")).unwrap(), b"mine too", "a person's file of that kind of name stays: it is no copy of the program's");
        assert_eq!(read_manifest(&folder).unwrap().version, "2.0.0");
        assert_eq!(walk(&data), ["models\\m.gguf", "sessions\\one.jsonl", "settings.json"]);
    }

    #[test]
    fn a_failure_leaves_the_program_as_it_was_and_running_again_goes_on() {
        let scene = Scene::new();
        let mut first = scene.package("1.0.0", &[("a.exe", b"one a"), ("b.dll", b"one b"), ("c.dll", b"one c")]);
        scene.run(&mut first, None, None).unwrap();
        let folder = scene.folder();
        let mut second = scene.package("2.0.0", &[("a.exe", b"two a"), ("b.dll", b"two b"), ("c.dll", b"two c")]);
        let failed = scene.run(&mut second, None, Some(2)).unwrap_err();
        assert!(matches!(failed, Failure::Said(ref words) if words.contains("asked for by a test")), "{failed:?}");
        assert_eq!(std::fs::read(folder.join("a.exe")).unwrap(), b"one a", "nothing has moved");
        assert_eq!(read_manifest(&folder).unwrap().version, "1.0.0");
        assert!(beside(&folder.join("a.exe")).exists() && beside(&folder.join("b.dll")).exists() && !beside(&folder.join("c.dll")).exists());
        // A half-written file from a power cut: its hash says so, and it is done again.
        std::fs::write(beside(&folder.join("b.dll")), "two").unwrap();
        let done = scene.run(&mut second, None, None).unwrap();
        assert_eq!(done, Installed { unpacked: 3, already_there: 0, removed: 0 });
        assert_eq!(std::fs::read(folder.join("b.dll")).unwrap(), b"two b");
        assert_eq!(read_manifest(&folder).unwrap().version, "2.0.0");
        assert!(walk(&folder).iter().all(|name| !name.ends_with(WAITING)));
    }

    #[test]
    fn a_file_in_use_is_named_and_trying_again_finishes() {
        let scene = Scene::new();
        let mut first = scene.package("1.0.0", &[("a.exe", b"one a"), ("b.dll", b"one b")]);
        scene.run(&mut first, None, None).unwrap();
        let folder = scene.folder();
        let mut second = scene.package("2.0.0", &[("a.exe", b"two a"), ("b.dll", b"two b")]);
        // Open with no sharing: as a running program's image, for this.
        use std::os::windows::fs::OpenOptionsExt;
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(folder.join("b.dll")).unwrap();
        let failed = scene.run(&mut second, None, None).unwrap_err();
        assert_eq!(failed, Failure::InUse("b.dll".to_string()));
        assert_eq!(read_manifest(&folder).unwrap().version, "1.0.0", "not done until it is all done");
        assert_eq!(std::fs::read(folder.join("a.exe")).unwrap(), b"one a", "the one before it is put back: no half of two versions");
        assert!(moved_aside(&folder).is_empty(), "{:?}", walk(&folder));
        drop(held);
        let done = scene.run(&mut second, None, None).unwrap();
        assert_eq!((done.unpacked, done.already_there), (2, 0), "both were waiting, and are not unpacked again: {done:?}");
        assert_eq!(std::fs::read(folder.join("a.exe")).unwrap(), b"two a");
        assert_eq!(std::fs::read(folder.join("b.dll")).unwrap(), b"two b");
        assert_eq!(read_manifest(&folder).unwrap().version, "2.0.0");
        assert!(moved_aside(&folder).is_empty() && walk(&folder).iter().all(|name| !name.ends_with(WAITING)), "{:?}", walk(&folder));
    }

    /// A file that fails to come into its place, after others have come into theirs: all of them go back to waiting, and what was there
    /// is put back, every file as it was.
    #[test]
    fn a_failure_while_placing_puts_every_file_back_as_it_was() {
        let scene = Scene::new();
        let mut first = scene.package("1.0.0", &[("a.exe", b"one a"), ("b.dll", b"one b"), ("tools\\c.py", b"one c")]);
        scene.run(&mut first, None, None).unwrap();
        let folder = scene.folder();
        let mut second = scene.package("2.0.0", &[("a.exe", b"two a"), ("b.dll", b"two b"), ("tools\\c.py", b"two c"), ("tools\\d.py", b"new d")]);
        REFUSE_PLACING.with(|refused| *refused.borrow_mut() = Some("tools\\c.py".into()));
        let failed = scene.run(&mut second, None, None).unwrap_err();
        REFUSE_PLACING.with(|refused| *refused.borrow_mut() = None);
        assert_eq!(failed, Failure::InUse("tools\\c.py".to_string()));
        for (file, was) in [("a.exe", "one a"), ("b.dll", "one b"), ("tools\\c.py", "one c")] {
            assert_eq!(std::fs::read_to_string(folder.join(file)).unwrap(), was, "{file}");
        }
        assert!(!folder.join("tools\\d.py").exists(), "a new file is not there either");
        assert!(moved_aside(&folder).is_empty(), "{:?}", walk(&folder));
        assert_eq!(read_manifest(&folder).unwrap().version, "1.0.0");
        let done = scene.run(&mut second, None, None).unwrap();
        assert_eq!(done.unpacked, 4, "all were waiting: {done:?}");
        assert_eq!(std::fs::read(folder.join("tools\\d.py")).unwrap(), b"new d");
        assert_eq!(read_manifest(&folder).unwrap().version, "2.0.0");
        assert!(moved_aside(&folder).is_empty() && walk(&folder).iter().all(|name| !name.ends_with(WAITING)), "{:?}", walk(&folder));
    }

    #[test]
    fn a_file_is_moved_aside_under_a_name_no_file_has() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dogmind-app.exe");
        assert_eq!(aside_of(&target, 7).file_name().unwrap(), "dogmind-app.previous-7.exe");
        std::fs::write(dir.path().join("dogmind-app.previous-7.exe"), b"left by a run that was still going").unwrap();
        assert_eq!(aside_of(&target, 7).file_name().unwrap(), "dogmind-app.previous-8.exe");
        assert_eq!(aside_of(&dir.path().join("LICENSE"), 3).file_name().unwrap(), "LICENSE.previous-3");
        assert_eq!(stood_for("LICENSE.previous-3").as_deref(), Some("LICENSE"), "and it is known again for what it stood for");
    }

    #[test]
    fn a_stop_ends_it_and_what_it_left_goes_with_it() {
        let scene = Scene::new();
        let mut package = scene.package("1.0.0", &[("a", b"1"), ("deep\\b", b"2")]);
        scene.stop.store(true, Ordering::Relaxed);
        assert_eq!(scene.run(&mut package, None, None).unwrap_err(), Failure::Stopped);
        scene.stop.store(false, Ordering::Relaxed);
        let folder = scene.folder();
        let failed = scene.run(&mut package, None, Some(1));
        assert!(failed.is_err() && beside(&folder.join("a")).exists());
        clean_up_waiting(&package, &folder);
        assert!(!folder.exists(), "nothing of it is left");
    }

    #[test]
    fn too_little_room_is_said_before_anything_is_written() {
        let scene = Scene::new();
        let bytes = noise(200_000);
        let mut package = scene.package("1.0.0", &[("a", &bytes)]);
        let failed = scene.run(&mut package, Some(1000), None).unwrap_err();
        assert!(matches!(failed, Failure::TooLittleRoom { free: 1000, .. }), "{failed:?}");
        assert!(walk(&scene.folder()).is_empty());
        assert!(scene.run(&mut package, Some(u64::MAX / 2), None).is_ok());
    }

    #[test]
    fn taking_it_away_takes_what_it_put_there_and_says_what_stays() {
        let scene = Scene::new();
        let mut package = scene.package("1.0.0", &[("dogmind-app.exe", b"app"), ("tools\\python\\python.exe", b"py")]);
        scene.run(&mut package, None, None).unwrap();
        let folder = scene.folder();
        tree(&folder, &[("tools\\.open-to-sandbox", b""), ("notes.txt", b"mine")]);
        let mut seen = Vec::new();
        let removed = remove(&folder, &scene.shell, &mut |at| seen.push(at)).unwrap();
        assert_eq!(removed.left, ["notes.txt"], "and it is said");
        assert_eq!(walk(&folder), ["notes.txt"]);
        assert!(scene.shell.said.borrow().iter().any(|line| line == "remove"));
        assert_eq!(seen.last(), Some(&1.0));

        // With nothing of a person's in it, the folder goes too.
        let scene = Scene::new();
        let mut package = scene.package("1.0.0", &[("a", b"1")]);
        scene.run(&mut package, None, None).unwrap();
        assert!(remove(&scene.folder(), &scene.shell, &mut |_| {}).unwrap().left.is_empty());
        assert!(!scene.folder().exists());
        assert!(remove(scene.dir.path(), &scene.shell, &mut |_| {}).is_err(), "a folder it did not install in is not touched");
    }

    #[test]
    fn a_file_in_the_way_that_no_install_put_there_stops_it_before_anything_changes() {
        let scene = Scene::new();
        let folder = scene.folder();
        tree(&folder, &[("READ-ME-FIRST.txt", b"my own notes")]);
        let mut package = scene.package("1.0.0", &[("dogmind-app.exe", b"app"), ("READ-ME-FIRST.txt", b"from the package")]);
        let failed = scene.run(&mut package, None, None).unwrap_err();
        assert!(matches!(failed, Failure::Said(ref words) if words.contains("READ-ME-FIRST.txt") && words.contains("no install put there")), "{failed:?}");
        assert_eq!(std::fs::read(folder.join("READ-ME-FIRST.txt")).unwrap(), b"my own notes");
        assert_eq!(walk(&folder), ["READ-ME-FIRST.txt"], "nothing else was put there");
        assert!(read_manifest(&folder).is_none(), "and it is not written down as the install's, to go with it");
        // The same bytes are no one's loss. And a file a later version adds is in the way the same as any.
        std::fs::write(folder.join("READ-ME-FIRST.txt"), b"from the package").unwrap();
        scene.run(&mut package, None, None).unwrap();
        tree(&folder, &[("mine.txt", b"mine")]);
        let mut second = scene.package("2.0.0", &[("dogmind-app.exe", b"app"), ("READ-ME-FIRST.txt", b"from the package"), ("mine.txt", b"new in this version")]);
        assert!(scene.run(&mut second, None, None).is_err());
        assert_eq!(std::fs::read(folder.join("mine.txt")).unwrap(), b"mine");
        // What an install did put there it replaces, changed or not.
        std::fs::write(folder.join("READ-ME-FIRST.txt"), b"edited since").unwrap();
        let mut third = scene.package("3.0.0", &[("dogmind-app.exe", b"app"), ("READ-ME-FIRST.txt", b"from the third")]);
        scene.run(&mut third, None, None).unwrap();
        assert_eq!(std::fs::read(folder.join("READ-ME-FIRST.txt")).unwrap(), b"from the third");
    }

    #[test]
    fn what_a_failed_update_put_there_goes_with_a_removal() {
        let scene = Scene::new();
        let mut first = scene.package("1.0.0", &[("a.exe", b"one a"), ("z.dll", b"one z")]);
        scene.run(&mut first, None, None).unwrap();
        let folder = scene.folder();
        let mut second = scene.package("2.0.0", &[("a.exe", b"two a"), ("m.dll", b"new"), ("z.dll", b"two z")]);
        use std::os::windows::fs::OpenOptionsExt;
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(folder.join("z.dll")).unwrap();
        assert_eq!(scene.run(&mut second, None, None).unwrap_err(), Failure::InUse("z.dll".to_string()));
        drop(held);
        assert!(!folder.join("m.dll").exists() && beside(&folder.join("m.dll")).is_file(), "the new file waits beside its place");
        assert_eq!(read_manifest(&folder).unwrap().version, "1.0.0");
        remove(&folder, &scene.shell, &mut |_| {}).unwrap();
        assert!(!folder.exists(), "{:?}", walk(&folder));
    }

    #[test]
    fn a_removal_does_not_go_through_a_link_to_what_is_not_the_programs() {
        let scene = Scene::new();
        let mut package = scene.package("1.0.0", &[("a.exe", b"1"), ("tools\\x.py", b"2")]);
        scene.run(&mut package, None, None).unwrap();
        let folder = scene.folder();
        let elsewhere = scene.dir.path().join("elsewhere");
        tree(&elsewhere, &[("x.py", b"the person's")]);
        std::fs::remove_dir_all(folder.join("tools")).unwrap();
        use std::os::windows::process::CommandExt;
        let made = std::process::Command::new("cmd").arg("/c").arg("mklink").arg("/J").arg(folder.join("tools")).arg(&elsewhere).creation_flags(0x0800_0000).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().unwrap();
        assert!(made.success());
        remove(&folder, &scene.shell, &mut |_| {}).unwrap();
        assert_eq!(std::fs::read(elsewhere.join("x.py")).unwrap(), b"the person's");
    }

    #[test]
    fn a_file_still_open_keeps_the_entries_for_trying_again() {
        let scene = Scene::new();
        let mut package = scene.package("1.0.0", &[("a.exe", b"1"), ("b", b"2")]);
        scene.run(&mut package, None, None).unwrap();
        let folder = scene.folder();
        use std::os::windows::fs::OpenOptionsExt;
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(folder.join("a.exe")).unwrap();
        assert!(remove(&folder, &scene.shell, &mut |_| {}).unwrap_err().contains("a.exe"));
        assert!(!scene.shell.said.borrow().iter().any(|line| line == "remove"));
        assert!(folder.join(MANIFEST).exists());
        drop(held);
        remove(&folder, &scene.shell, &mut |_| {}).unwrap();
        assert!(!folder.exists());
    }
}
