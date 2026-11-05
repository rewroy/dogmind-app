//! Putting a package in a folder, and taking it out again. What a person keeps is never in the way: settings and sessions are in another folder altogether, and in this one only what
//! the last install put there is ever taken away.
//!
//! Every file is unpacked beside its place, under a name of its own, and checked by its hash. Only when all of them are there do they take their places, so that a failure before that
//! leaves the program as it was. A file made there has what its folder gives new files, which matters: the sandbox's entries on `tools\` are on each thing in it and inherited by what comes.
//! Running it again after any failure goes on: what is in place, or waiting beside its place, with the right hash, is not unpacked again.
//!
//! To find that a file in place has the right hash, all of an install would be read each time, and it is all but a few files that are found so. So each install writes down, in its manifest, the
//! hash of each file of the package with the size and time the file had when it was written down; an update (the app's own, `--update`, which runs often and has to be quick) takes a file that
//! still has them for the same file, and does not read it. Anything else is read and hashed. A setup that a person runs reads every file whose size is right, and so is the way to repair an
//! install: a size and a time can be put back by anyone who can write the folder, and a file that was changed that way is found by its bytes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::pack::{hash_file, hex, unhex, Entry, Package, KEPT};

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
    /// A placing failed, and these files (sorted, each once) could not be put back as they were: the folder holds files of both versions, or lacks one. The program is not to be started from it
    /// until the setup has been run again, which finishes it. `not_put_back_words` says it in a line.
    NotPutBack(Vec<String>),
    TooLittleRoom { needed: u64, free: u64 },
}

/// What a finished install was.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Installed {
    pub unpacked: usize,
    pub already_there: usize,
    pub removed: usize,
    /// The files that were read whole to be hashed, which the install's own record of a file spares an update.
    pub hashed: usize,
}

fn beside(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(WAITING);
    target.with_file_name(name)
}

/// What an install wrote down of a file it placed or found in place: the hash its bytes have, and the size and last-write time the file had after that. A file that still has that size
/// and that time is the file that was written down, and the next install does not read it again to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub sha256: [u8; 32],
    pub size: u64,
    /// As Windows keeps it, in steps of 100 ns (`Metadata::modified` is made from the same count).
    pub modified: u64,
}

/// A file's size and last-write time, as they are on the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified: u64,
}

impl Stamp {
    fn of(data: &std::fs::Metadata) -> Stamp {
        use std::os::windows::fs::MetadataExt;
        Stamp { size: data.len(), modified: data.last_write_time() }
    }
}

/// What the last install of a folder did, and how long each of its parts took, in milliseconds: written into the manifest so that an update can be read afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LastRun {
    pub counts: Installed,
    pub checking: u64,
    pub unpacking: u64,
    pub placing: u64,
    pub finishing: u64,
}

/// What a folder holds of an install. The manifest is lines of a word and what it says; a reader goes by the words it knows and passes over the rest, so that a line added later (`sum`,
/// `took` and `count` were) is nothing to a program that is older than it:
///
///     version 0.1.5
///     test                                                           (only when it was made with the test switch)
///     file PATH                                                      each file the install put there, and the uninstaller
///     sum SHA256 SIZE MODIFIED PATH                                  what was written down of a file of the package (see `Record`)
///     took checking=MS unpacking=MS placing=MS finishing=MS          the last run's time in each stage
///     count unpacked=N already_there=N hashed=N removed=N            and what it did
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    pub version: String,
    /// Made with the test switch: nothing real was registered for it, and nothing real is taken away for it.
    pub test: bool,
    pub files: Vec<String>,
    /// By path, as the package spells it. An install made before these were written has none, and is read whole once.
    pub records: Vec<(String, Record)>,
    pub last_run: Option<LastRun>,
}

/// `HASH SIZE MODIFIED PATH` of a `sum` line. The path is the rest of the line, spaces and all.
fn read_record(rest: &str) -> Option<(String, Record)> {
    let mut parts = rest.splitn(4, ' ');
    let sha256 = unhex(parts.next()?)?;
    let size = parts.next()?.parse().ok()?;
    let modified = parts.next()?.parse().ok()?;
    let path = parts.next().filter(|path| crate::pack::is_safe(path))?;
    Some((path.to_string(), Record { sha256, size, modified }))
}

/// The `NAME=NUMBER` words of a `took` or `count` line.
fn numbers(rest: &str) -> impl Iterator<Item = (&str, u64)> {
    rest.split(' ').filter_map(|word| {
        let (name, number) = word.split_once('=')?;
        Some((name, number.parse().ok()?))
    })
}

pub fn read_manifest(folder: &Path) -> Option<Manifest> {
    let text = std::fs::read_to_string(folder.join(MANIFEST)).ok()?;
    let mut manifest = Manifest::default();
    for line in text.lines() {
        match line.split_once(' ') {
            Some(("version", version)) => manifest.version = version.to_string(),
            Some(("file", file)) if crate::pack::is_safe(file) => manifest.files.push(file.to_string()),
            Some(("sum", rest)) => manifest.records.extend(read_record(rest)),
            Some(("took", rest)) => {
                let run = manifest.last_run.get_or_insert_with(LastRun::default);
                for (name, milliseconds) in numbers(rest) {
                    match name {
                        "checking" => run.checking = milliseconds,
                        "unpacking" => run.unpacking = milliseconds,
                        "placing" => run.placing = milliseconds,
                        "finishing" => run.finishing = milliseconds,
                        _ => {}
                    }
                }
            }
            Some(("count", rest)) => {
                let run = manifest.last_run.get_or_insert_with(LastRun::default);
                for (name, count) in numbers(rest) {
                    match name {
                        "unpacked" => run.counts.unpacked = count as usize,
                        "already_there" => run.counts.already_there = count as usize,
                        "hashed" => run.counts.hashed = count as usize,
                        "removed" => run.counts.removed = count as usize,
                        _ => {}
                    }
                }
            }
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
    for (path, record) in &manifest.records {
        text.push_str(&format!("sum {} {} {} {path}\n", hex(&record.sha256), record.size, record.modified));
    }
    if let Some(run) = &manifest.last_run {
        text.push_str(&format!("took checking={} unpacking={} placing={} finishing={}\n", run.checking, run.unpacking, run.placing, run.finishing));
        let counts = &run.counts;
        text.push_str(&format!("count unpacked={} already_there={} hashed={} removed={}\n", counts.unpacked, counts.already_there, counts.hashed, counts.removed));
    }
    let partial = folder.join(format!("{MANIFEST}{WAITING}"));
    // (on the disk before its name changes: after a power cut a rename can stand on nothing, and then no file here is known to be this program's)
    let mut out = std::fs::File::create(&partial)?;
    std::io::Write::write_all(&mut out, text.as_bytes())?;
    out.sync_all()?;
    drop(out);
    replace(&partial, &folder.join(MANIFEST))
}

/// `from` renamed to `to`, over what is there. A virus scanner or the search index may have a new file open for a moment, and so for the last two things an install writes, which come after
/// every file is in place (the manifest, and the uninstaller in `write_stub`): a lock that passes in a moment is no reason to say that an install whose files are all new did not finish.
pub fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
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

/// How long an install that no one watches (the app's own update, a test's `--auto`) waits for another install of its folder to end, before it fails.
pub const WAIT_FOR_ANOTHER: Duration = Duration::from_secs(10 * 60);

/// What an install or a removal says when another one of the folder is still at work and it has waited as long as it may.
const ANOTHER_IS_RUNNING: &str = "Another install of this folder is still running, so nothing has been changed.";

/// The name of the mutex of a folder: the hash of its full path in lower case, as Windows takes two spellings of a path for one folder. Nothing of the folder is looked at, so that a first
/// install, whose folder is not made yet, has one.
fn lock_name(folder: &Path) -> String {
    use sha2::{Digest, Sha256};
    let full = std::path::absolute(folder).unwrap_or_else(|_| folder.to_path_buf());
    // (the components leave out a slash at the end and a `.`)
    let text = full.components().collect::<PathBuf>().to_string_lossy().to_lowercase();
    let text = match text.strip_prefix("\\\\?\\unc\\") {
        Some(rest) => format!("\\\\{rest}"),
        None => text.strip_prefix("\\\\?\\").map_or(text.clone(), str::to_string),
    };
    format!("Local\\DogmindSetup-{}", hex(&Sha256::digest(text.as_bytes()).into()))
}

/// The mutex of a folder, held by the one install, update or removal that works on it. A mutex belongs to the thread that took it, and is let go by that thread (or by the system, when the
/// process ends): it is taken and dropped on the thread that does the work.
pub struct FolderLock(windows_sys::Win32::Foundation::HANDLE);

impl Drop for FolderLock {
    fn drop(&mut self) {
        // SAFETY: the handle is the mutex this took, which this thread owns.
        unsafe {
            windows_sys::Win32::System::Threading::ReleaseMutex(self.0);
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// The mutex of `folder`, taken. When another install, update or removal of the folder has it, `held(true)` says so once, and this waits for it: as long as it takes, or, with `at_most`, that
/// long and then fails with a line. `held(false)` says the wait is over. `stop` ends the wait (`Failure::Stopped`), and nothing has been written by then.
pub fn lock_folder(folder: &Path, at_most: Option<Duration>, stop: &AtomicBool, held: &mut dyn FnMut(bool)) -> Result<FolderLock, Failure> {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
    let name = crate::system::wide(&lock_name(folder));
    // SAFETY: the name ends in a zero; the mutex has the default security and is not owned at first.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(Failure::Said(format!("{}: the folder's lock could not be made: {}", folder.display(), std::io::Error::last_os_error())));
    }
    let (began, mut said) = (Instant::now(), false);
    loop {
        // (the first look does not wait; after it, the wait ends the moment the mutex is let go, and is cut short to hear a stop)
        // SAFETY: the handle is the mutex made above.
        let got = unsafe { WaitForSingleObject(handle, if said { 200 } else { 0 }) };
        // (a mutex that was abandoned was held by a program that ended with it: this has it all the same)
        if got == WAIT_OBJECT_0 || got == WAIT_ABANDONED {
            if said {
                held(false);
            }
            return Ok(FolderLock(handle));
        }
        let failure = if got != WAIT_TIMEOUT {
            Some(Failure::Said(format!("{}: the folder's lock could not be waited for: {}", folder.display(), std::io::Error::last_os_error())))
        } else if stop.load(Ordering::Relaxed) {
            Some(Failure::Stopped)
        } else if at_most.is_some_and(|most| began.elapsed() >= most) {
            Some(Failure::Said(ANOTHER_IS_RUNNING.to_string()))
        } else {
            None
        };
        if let Some(failure) = failure {
            if said {
                held(false);
            }
            // SAFETY: the handle is the mutex made above, which this does not own.
            unsafe { CloseHandle(handle) };
            return Err(failure);
        }
        if !said {
            said = true;
            held(true);
        }
    }
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
    /// This is the app's own update (`--update`): the install's record of a file, its hash with the size and time it had, then stands for reading the file while the file still has that size and
    /// time. Otherwise (a setup that a person runs, which is how an install is repaired) every file of the size an entry says is hashed, and the records are written down all the same.
    pub update: bool,
    /// A test's failure, after this many files have been unpacked.
    pub fail_after: Option<usize>,
    /// A test's slowness, for a capture of it at work, after each file.
    pub slowly: Duration,
    /// How long to wait for another install, update or removal of the folder to end (see `install_alone`): as long as it takes, or this long and then fail.
    pub wait: Option<Duration>,
}

/// Whether a file holds what an entry says, and the size and time it had when that was found out. The time is kept only for a file that was the same all through the look: one that
/// changed while it was read is the entry's by its hash, and there is nothing to write down of it.
enum Holds {
    No,
    Yes(Option<Stamp>),
}

/// Whether a file with this size and time is `entry`'s by `record`, which says so without the file being read: the record has the entry's hash, and the size and time the file has now.
fn is_by_record(stamp: Stamp, entry: &Entry, record: Option<&Record>) -> bool {
    stamp.size == entry.size && record.is_some_and(|record| record.sha256 == entry.sha256 && record.size == stamp.size && record.modified == stamp.modified)
}

/// Whether the file at `path` is `entry`'s. `record`: what the last install wrote down of this very file, when this run goes by that, which stands for reading it while the file still has the
/// size and time it was written down with. Otherwise it is read whole and hashed, and counted in `hashed`. `announce` is told of a file that is looked at.
fn holds(path: &Path, entry: &Entry, record: Option<&Record>, hashed: &mut usize, announce: &mut dyn FnMut()) -> Holds {
    let Ok(before) = std::fs::metadata(path) else { return Holds::No };
    if before.len() != entry.size {
        return Holds::No;
    }
    announce();
    let stamp = Stamp::of(&before);
    if is_by_record(stamp, entry, record) {
        return Holds::Yes(Some(stamp));
    }
    *hashed += 1;
    if !hash_file(path, &mut |_| {}).is_ok_and(|hash| hash == entry.sha256) {
        return Holds::No;
    }
    Holds::Yes(std::fs::metadata(path).ok().map(|after| Stamp::of(&after)).filter(|after| *after == stamp))
}

/// The package in the folder, from nothing or over an earlier install, going on from where another run stopped.
pub fn install(package: &mut Package, job: &Install, on: &mut dyn FnMut(Progress)) -> Result<Installed, Failure> {
    let folder = job.folder;
    let entries = package.entries.clone();
    // A folder of the install that has been made a link leads out of it: a file written there, moved aside there and deleted there would be another place's. Nothing is written before this is known.
    if let Some(link) = entries.iter().find_map(|entry| link_on_the_way(folder, &entry.path)) {
        return Err(Failure::Said(format!("{}: this folder of the install is a link to somewhere else, so nothing has been changed. Install in another folder, or take the link away.", link.display())));
    }
    std::fs::create_dir_all(folder).map_err(|error| say(folder, &error))?;
    let total = package.total_bytes().max(1);
    // (how long each stage took, for the manifest)
    let (mut checking, mut unpacking, mut placing) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let looked = Instant::now();
    let mut hashed = 0usize;
    let previous = read_manifest(folder);
    let ours: HashSet<String> = previous.iter().flat_map(|manifest| manifest.files.iter()).map(|file| file.to_lowercase()).collect();
    // What the last install wrote down of its files, by lower case path.
    let records: HashMap<String, &Record> = previous.iter().flat_map(|manifest| manifest.records.iter()).map(|(path, record)| (path.to_lowercase(), record)).collect();
    // What will not fit is said before anything is written, and no file is read to know. A file the package leaves out (KEPT) needs no room; nor does one that waits beside its place at the entry's
    // size (an earlier run unpacked it, and its room is taken); nor one in place that the install's record of it says is the entry's, by hash, size and time, which an update takes without reading
    // it. Any other counts whole, one in place at the entry's size too: if it is not the entry's it is unpacked beside its place in full.
    let needed: u64 = entries
        .iter()
        .filter(|entry| {
            entry.method != KEPT
                && std::fs::metadata(beside(&folder.join(&entry.path))).map_or(true, |data| data.len() != entry.size)
                && !std::fs::metadata(folder.join(&entry.path)).is_ok_and(|data| is_by_record(Stamp::of(&data), entry, records.get(&entry.path.to_lowercase()).copied()))
        })
        .map(|entry| entry.size)
        .sum();
    // What is taken for a file that has the size and time it was written down with: in an update, and no other run (see `Install::update`).
    let trusted = |path: &str| if job.update { records.get(&path.to_lowercase()).copied() } else { None };
    // The files this package leaves out (KEPT) are in the folder already, as the app saw them when it put the package together, or nothing is changed and the person is told what to do: such a
    // setup can be no more than an update of the install it was made for, which only the app makes and only `--update` runs, so these go by their records whoever asks.
    let mut found: HashMap<usize, Option<Stamp>> = HashMap::new();
    for (at, entry) in entries.iter().enumerate().filter(|(_, entry)| entry.method == KEPT) {
        let mut announce = || on(Progress { stage: Stage::Checking, fraction: 0.0, file: entry.path.clone() });
        match holds(&folder.join(&entry.path), entry, records.get(&entry.path.to_lowercase()).copied(), &mut hashed, &mut announce) {
            Holds::Yes(stamp) => {
                found.insert(at, stamp);
            }
            Holds::No => {
                return Err(Failure::Said(format!(
                    "{}: this update leaves out the files that were in this folder when it was fetched, and this one is not as it was, so nothing has been changed. Download the full setup from the release page, and run that.",
                    folder.join(&entry.path).display()
                )));
            }
        }
    }
    if let Some(free) = job.free {
        if needed + ROOM_TO_SPARE > free {
            return Err(Failure::TooLittleRoom { needed: needed + ROOM_TO_SPARE, free });
        }
    }
    // What is in the way and is no install's. The folder can be any the person names, and what this is about to do is put its own file in that place and, one day, take it away.
    for entry in &entries {
        let target = folder.join(&entry.path);
        if !ours.contains(&entry.path.to_lowercase()) && target.exists() {
            hashed += 1;
            if !hash_file(&target, &mut |_| {}).is_ok_and(|hash| hash == entry.sha256) {
                return Err(Failure::Said(format!("{}: there is a file of that name in this folder already, which no install put there, so nothing has been changed. Move it, or install in another folder.", target.display())));
            }
        }
    }
    checking += looked.elapsed();
    let mut done = 0u64;
    let mut outcome = Installed::default();
    let mut unpacked_now = 0;
    // (`found`, from above, has the files that were found in place, by their place in the package, with the size and time they had when they were looked at.)
    // Unpack, or find it there.
    let mut to_place: Vec<&Entry> = Vec::new();
    for (at, entry) in entries.iter().enumerate() {
        if job.stop.load(Ordering::Relaxed) {
            return Err(Failure::Stopped);
        }
        let target = folder.join(&entry.path);
        let waiting = beside(&target);
        let parts = done as f64 / total as f64;
        if entry.method == KEPT {
            // (found above; a file waiting for it is of a run that was for another package)
            let _ = std::fs::remove_file(&waiting);
            outcome.already_there += 1;
            done += entry.size;
            on(Progress { stage: Stage::Unpacking, fraction: done as f64 / total as f64, file: entry.path.clone() });
            continue;
        }
        let looked = Instant::now();
        let mut announce = || on(Progress { stage: Stage::Checking, fraction: parts, file: entry.path.clone() });
        let in_waiting = matches!(holds(&waiting, entry, None, &mut hashed, &mut announce), Holds::Yes(_));
        let in_place = if in_waiting { Holds::No } else { holds(&target, entry, trusted(&entry.path), &mut hashed, &mut announce) };
        checking += looked.elapsed();
        if in_waiting {
            to_place.push(entry);
            outcome.unpacked += 1;
        } else if let Holds::Yes(stamp) = in_place {
            let _ = std::fs::remove_file(&waiting);
            outcome.already_there += 1;
            found.insert(at, stamp);
        } else {
            if job.fail_after.is_some_and(|after| unpacked_now >= after) {
                return Err(Failure::Said(format!("{}: a failure asked for by a test", entry.path)));
            }
            let began = Instant::now();
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
            unpacking += began.elapsed();
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
    // What is about to be here is written down first, under the version that is here: from a failure from now on, a removal takes what came of this too. (What was written down of the files
    // that are there now is true of them until they are replaced, and a file that is replaced has another time.)
    let mut going_on: Vec<String> = previous.iter().flat_map(|manifest| manifest.files.iter().cloned()).collect();
    going_on.extend(entries.iter().filter(|entry| !ours.contains(&entry.path.to_lowercase())).map(|entry| entry.path.clone()));
    let version_here = previous.as_ref().map_or_else(|| package.version.clone(), |manifest| manifest.version.clone());
    write_manifest(folder, &Manifest {
        version: version_here,
        test: previous.as_ref().map_or(job.test, |manifest| manifest.test),
        files: going_on,
        records: previous.iter().flat_map(|manifest| manifest.records.iter().cloned()).collect(),
        last_run: previous.as_ref().and_then(|manifest| manifest.last_run.clone()),
    })
    .map_err(|error| say(&folder.join(MANIFEST), &error))?;
    let placing_began = Instant::now();
    let moved = place_all(folder, &to_place, on)?;
    // (one a program still runs from cannot go: it stays, and goes with the next install)
    for (_, aside) in moved {
        let _ = std::fs::remove_file(aside);
    }
    placing += placing_began.elapsed();
    let finishing_began = Instant::now();
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
    // What is written down of each file is its size and time as the disk has them now, after the last write of it: a file placed by this run has the time it was unpacked with, which the
    // rename keeps. A file that was found in place is written down only if it still has what it had when it was looked at.
    let mut written_down = Vec::with_capacity(entries.len());
    for (at, entry) in entries.iter().enumerate() {
        let Ok(data) = std::fs::metadata(folder.join(&entry.path)) else { continue };
        let now = Stamp::of(&data);
        if now.size != entry.size || found.get(&at).is_some_and(|seen| *seen != Some(now)) {
            continue;
        }
        written_down.push((entry.path.clone(), Record { sha256: entry.sha256, size: now.size, modified: now.modified }));
    }
    outcome.hashed = hashed;
    let millis = |time: Duration| time.as_millis() as u64;
    let last_run = LastRun { counts: outcome.clone(), checking: millis(checking), unpacking: millis(unpacking), placing: millis(placing), finishing: millis(finishing_began.elapsed()) };
    write_manifest(folder, &Manifest { version: package.version.clone(), test: job.test, files, records: written_down, last_run: Some(last_run) }).map_err(|error| say(&folder.join(MANIFEST), &error))?;
    job.shell.add_entries(folder, &package.version, package.total_bytes() / 1024).map_err(Failure::Said)?;
    Ok(outcome)
}

/// `install`, when no other install, update or removal of the folder is at work: with the folder's mutex taken, for the whole of it. `held` is told when it has to wait for another (`true`) and when
/// that is over (`false`). A stop that comes while it waits changes nothing; one that comes after takes away what was left waiting beside the places, while the mutex is still held, as another
/// install of the folder could be unpacking there as soon as it is let go.
pub fn install_alone(package: &mut Package, job: &Install, held: &mut dyn FnMut(bool), on: &mut dyn FnMut(Progress)) -> Result<Installed, Failure> {
    let _alone = lock_folder(job.folder, job.wait, job.stop, held)?;
    let result = install(package, job, on);
    if result == Err(Failure::Stopped) {
        clean_up_waiting(package, job.folder);
    }
    result
}

/// Every file waiting beside its place put in it, or none: half of two versions would run as neither. First what is there now is moved
/// aside, each under a name of its own (Windows lets a program's file be renamed while it runs, and it runs on from there); then each new
/// one comes into its place. Anything that fails on the way puts back what was moved, and what came goes back to waiting beside its
/// place: the program is as it was, and running again goes on. If something cannot be put back either (a virus scanner or a library that
/// is loaded has the file), that is the failure that is returned, with the files it is of: the folder is then not as it was, and what is
/// left is still where running again finds it, a new file by its hash in its place or waiting beside it, and an old one under its own name.
/// What was moved aside, by where it stood, for the caller to take away.
fn place_all(folder: &Path, to_place: &[&Entry], on: &mut dyn FnMut(Progress)) -> Result<Vec<(PathBuf, PathBuf)>, Failure> {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs());
    let steps = (to_place.len() * 2).max(1) as f64;
    let (mut moved, mut placed): (Vec<(PathBuf, PathBuf)>, Vec<PathBuf>) = (Vec::new(), Vec::new());
    let failed = |moved: &[(PathBuf, PathBuf)], placed: &[PathBuf], file: &str, target: &Path, error: std::io::Error| {
        // What stays as this left it, by its place: an old file that is not back, or a new one that is still where it came and has no old one to go over it.
        let mut stuck: Vec<&Path> = Vec::new();
        let mut still_there: Vec<&Path> = Vec::new();
        for done in placed.iter().rev() {
            if put_back(Back::WhatCame, folder, done, done, &beside(done)).is_err() {
                still_there.push(done);
            }
        }
        for (target, aside) in moved.iter().rev() {
            if put_back(Back::WhatWas, folder, target, aside, target).is_err() {
                stuck.push(target);
            }
        }
        // (a new file that stays where it came is put over by the old one when that goes back, and then the folder is as it was)
        stuck.extend(still_there.into_iter().filter(|done| !moved.iter().any(|(target, _)| target == done)));
        if !stuck.is_empty() {
            return not_put_back(folder, &stuck);
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

/// Which way a file goes when a failed placing is undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Back {
    /// What came into its place goes to waiting beside it again.
    WhatCame,
    /// What was moved aside goes back into its place.
    WhatWas,
}

/// One step of undoing a failed placing: `from` renamed to `to`, for the file that stands at `target`.
fn put_back(back: Back, folder: &Path, target: &Path, from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    let refused = tests::REFUSE_PUTTING_BACK.with(|refused| refused.borrow().iter().any(|(file, which)| *which == back && folder.join(file) == target));
    #[cfg(not(test))]
    let refused = {
        let _ = (back, folder, target);
        false
    };
    if refused {
        return Err(std::io::Error::from_raw_os_error(32));
    }
    replace(from, to)
}

/// How many files the failure names before it says how many more there are.
const NAMED: usize = 3;

/// The files a line names of these, and how many more there are.
pub fn named(names: &[String]) -> (&[String], usize) {
    let shown = names.len().min(NAMED);
    (&names[..shown], names.len() - shown)
}

/// The failure of a placing that could not be undone, for the files that are not as they were. That running the setup again finishes it is true of any such state: the new files are in their
/// places or waiting beside them, which the next run finds by their hashes, and a file that is not in its place is put there by the next run.
fn not_put_back(folder: &Path, stuck: &[&Path]) -> Failure {
    let mut names: Vec<String> = stuck.iter().map(|path| path.strip_prefix(folder).unwrap_or(path).display().to_string()).collect();
    names.sort();
    names.dedup();
    Failure::NotPutBack(names)
}

/// What is said of `Failure::NotPutBack`, in one line for the person: which files are not as they were (three at most, then how many more), and that running the setup again finishes it.
pub fn not_put_back_words(names: &[String]) -> String {
    let (shown, more) = named(names);
    let mut shown = shown.join(", ");
    if more > 0 {
        shown.push_str(&format!(" and {more} more"));
    }
    let was = if names.len() == 1 { "it was" } else { "they were" };
    format!("This did not finish: {shown} could not be put back as {was}, so the folder is not as it was. Running the setup again finishes it.")
}

/// The name a file is moved aside under: `NAME.previous-TIME.EXT`, as the app's own new builds do, and one no file has yet.
fn aside_of(target: &Path, stamp: u64) -> PathBuf {
    let stem = target.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let extension = target.extension().map(|extension| format!(".{}", extension.to_string_lossy())).unwrap_or_default();
    (stamp..).map(|time| target.with_file_name(format!("{stem}.previous-{time}{extension}"))).find(|path| std::fs::symlink_metadata(path).is_err()).unwrap_or_default()
}

/// The first folder on the way to this file from the folder that is a link (a symbolic link, or a junction, which Rust takes for one). What it leads to is not in the folder, whatever the path
/// says, and is not this program's to write in or take.
fn link_on_the_way(folder: &Path, file: &str) -> Option<PathBuf> {
    let mut place = folder.to_path_buf();
    let mut parts = file.split('\\').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return None;
        }
        place.push(part);
        if std::fs::symlink_metadata(&place).is_ok_and(|what| what.file_type().is_symlink()) {
            return Some(place);
        }
    }
    None
}

/// Whether the way to this file from the folder goes through a link.
fn through_a_link(folder: &Path, file: &str) -> bool {
    link_on_the_way(folder, file).is_some()
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

/// `remove`, when no other install, update or removal of the folder is at work (see `install_alone`). A removal has no stop: `wait` is as long as it waits, or none for as long as it takes.
pub fn remove_alone(folder: &Path, shell: &dyn Shell, wait: Option<Duration>, held: &mut dyn FnMut(bool), on: &mut dyn FnMut(f64)) -> Result<Removed, String> {
    let _alone = match lock_folder(folder, wait, &AtomicBool::new(false), held) {
        Ok(lock) => lock,
        Err(Failure::Said(words)) => return Err(words),
        Err(_) => return Err("The removal was stopped.".to_string()),
    };
    remove(folder, shell, on)
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
        /// The put-backs of a failed placing that fail, as if something had the file open: the file, and which of its two put-backs.
        pub static REFUSE_PUTTING_BACK: RefCell<Vec<(String, Back)>> = const { RefCell::new(Vec::new()) };
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
        /// The app's own update (`--update`): the install's records stand for reading its files.
        fn run(&self, package: &mut Package, free: Option<u64>, fail_after: Option<usize>) -> Result<Installed, Failure> {
            self.run_as(true, package, free, fail_after)
        }
        /// A setup that a person runs: every file of the size an entry says is read.
        fn run_by_hand(&self, package: &mut Package) -> Result<Installed, Failure> {
            self.run_as(false, package, None, None)
        }
        fn run_as(&self, update: bool, package: &mut Package, free: Option<u64>, fail_after: Option<usize>) -> Result<Installed, Failure> {
            let folder = self.folder();
            let job = Install { folder: &folder, shell: &self.shell, free, stop: &self.stop, test: true, write_uninstaller: &|path| std::fs::write(path, "uninstaller"), update, fail_after, slowly: Duration::ZERO, wait: None };
            install(package, &job, &mut |_| {})
        }
    }

    #[test]
    fn a_first_install_puts_everything_in_place_and_registers_once() {
        let scene = Scene::new();
        let mut package = scene.package("1.0.0", &[("dogmind-app.exe", b"app"), ("tools\\python\\python.exe", b"py"), ("\u{65e5}\u{672c}\\x", b"j")]);
        let done = scene.run(&mut package, None, None).unwrap();
        assert_eq!(done, Installed { unpacked: 3, already_there: 0, removed: 0, hashed: 0 });
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
        assert_eq!(done, Installed { unpacked: 2, already_there: 2, removed: 1, hashed: 1 });
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
        assert_eq!(done, Installed { unpacked: 3, already_there: 0, removed: 0, hashed: 3 });
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
        // what was written down is still true of each file: a rename keeps its time, and so what was moved aside and put back has the one it had
        let records = read_manifest(&folder).unwrap().records;
        assert_eq!(records.len(), 3);
        for (path, record) in &records {
            assert_eq!(Stamp::of(&std::fs::metadata(folder.join(path)).unwrap()), Stamp { size: record.size, modified: record.modified }, "{path}");
        }
        let done = scene.run(&mut second, None, None).unwrap();
        assert_eq!(done.unpacked, 4, "all were waiting: {done:?}");
        assert_eq!(std::fs::read(folder.join("tools\\d.py")).unwrap(), b"new d");
        assert_eq!(read_manifest(&folder).unwrap().version, "2.0.0");
        assert!(moved_aside(&folder).is_empty() && walk(&folder).iter().all(|name| !name.ends_with(WAITING)), "{:?}", walk(&folder));
    }

    /// A file whose coming into its place fails, and one that cannot be put back after that: the failure says which, and running the setup again, with nothing refused, ends with every file
    /// of the new version, by its hash. Both ways a put-back can fail: the new file stays in its place with the old one beside it, or the old one is not back and the new one waits.
    #[test]
    fn a_failed_placing_that_cannot_put_a_file_back_says_which_and_running_again_finishes() {
        use sha2::{Digest, Sha256};
        let hash_of = |bytes: &[u8]| -> [u8; 32] { Sha256::digest(bytes).into() };
        let new: [(&str, &[u8]); 4] = [("a.exe", b"two a"), ("b.dll", b"two b"), ("tools\\c.py", b"two c"), ("tools\\d.py", b"new d")];
        // (b.dll is held in its place, which neither takes the new file out of it nor puts the old one over it; then only the old one's way back is held)
        for (refused, new_one_stays) in [(vec![("b.dll", Back::WhatCame), ("b.dll", Back::WhatWas)], true), (vec![("b.dll", Back::WhatWas)], false)] {
            let scene = Scene::new();
            let mut first = scene.package("1.0.0", &[("a.exe", b"one a"), ("b.dll", b"one b"), ("tools\\c.py", b"one c")]);
            scene.run(&mut first, None, None).unwrap();
            let folder = scene.folder();
            let mut second = scene.package("2.0.0", &new);
            REFUSE_PLACING.with(|refusing| *refusing.borrow_mut() = Some("tools\\c.py".into()));
            REFUSE_PUTTING_BACK.with(|refusing| *refusing.borrow_mut() = refused.iter().map(|(file, back)| (file.to_string(), *back)).collect());
            let failed = scene.run(&mut second, None, None).unwrap_err();
            REFUSE_PLACING.with(|refusing| *refusing.borrow_mut() = None);
            REFUSE_PUTTING_BACK.with(|refusing| refusing.borrow_mut().clear());
            assert_eq!(failed, Failure::NotPutBack(vec!["b.dll".to_string()]), "{new_one_stays}");
            assert_eq!(not_put_back_words(&["b.dll".to_string()]), "This did not finish: b.dll could not be put back as it was, so the folder is not as it was. Running the setup again finishes it.");
            // what could be put back is as it was; the file that could not is where it is said to be
            assert_eq!(std::fs::read(folder.join("a.exe")).unwrap(), b"one a");
            assert_eq!(std::fs::read(folder.join("tools\\c.py")).unwrap(), b"one c");
            assert_eq!(folder.join("b.dll").exists(), new_one_stays);
            assert_eq!(moved_aside(&folder).len(), 1, "the old b.dll is where it was moved to: {:?}", walk(&folder));
            assert_eq!(beside(&folder.join("b.dll")).exists(), !new_one_stays, "{:?}", walk(&folder));
            assert_eq!(read_manifest(&folder).unwrap().version, "1.0.0", "not done until it is all done");
            // (the app's update in the one state, and a setup run by hand in the other: it finishes either way)
            let done = if new_one_stays { scene.run(&mut second, None, None) } else { scene.run_by_hand(&mut second) }.unwrap();
            assert_eq!(done.unpacked + done.already_there, 4, "{done:?}");
            for (file, bytes) in new {
                assert_eq!(hash_file(&folder.join(file), &mut |_| {}).unwrap(), hash_of(bytes), "{file}");
            }
            assert_eq!(read_manifest(&folder).unwrap().version, "2.0.0");
            assert!(moved_aside(&folder).is_empty() && walk(&folder).iter().all(|name| !name.ends_with(WAITING)), "{:?}", walk(&folder));
            // and what it wrote down is of what is there: the next run reads nothing
            let mut third = scene.package("2.0.1", &new);
            assert_eq!(scene.run(&mut third, None, None).unwrap(), Installed { unpacked: 0, already_there: 4, removed: 0, hashed: 0 });
        }
    }

    /// A new file that has no old one to go over it, and cannot be put back to waiting, stays in its place: that is named too. A file that was put back by the old one going over it is not.
    #[test]
    fn a_new_file_that_stays_in_its_place_is_named_and_one_the_old_file_went_over_is_not() {
        let scene = Scene::new();
        let mut first = scene.package("1.0.0", &[("a.exe", b"one a"), ("b.dll", b"one b")]);
        scene.run(&mut first, None, None).unwrap();
        let folder = scene.folder();
        // a.exe: the new file will not go back to waiting, but the old one goes over it, and the folder is as it was
        let mut second = scene.package("2.0.0", &[("a.exe", b"two a"), ("b.dll", b"two b"), ("z.dll", b"new z")]);
        REFUSE_PLACING.with(|refusing| *refusing.borrow_mut() = Some("z.dll".into()));
        REFUSE_PUTTING_BACK.with(|refusing| *refusing.borrow_mut() = vec![("a.exe".to_string(), Back::WhatCame)]);
        let failed = scene.run(&mut second, None, None).unwrap_err();
        assert_eq!(failed, Failure::InUse("z.dll".to_string()), "nothing is left that the person has to know of");
        assert_eq!(std::fs::read(folder.join("a.exe")).unwrap(), b"one a");
        assert!(moved_aside(&folder).is_empty(), "{:?}", walk(&folder));
        // m.dll is new, and has no old file to go over it
        let mut third = scene.package("3.0.0", &[("a.exe", b"two a"), ("b.dll", b"two b"), ("m.dll", b"new m"), ("z.dll", b"new z")]);
        REFUSE_PUTTING_BACK.with(|refusing| *refusing.borrow_mut() = vec![("m.dll".to_string(), Back::WhatCame)]);
        let failed = scene.run(&mut third, None, None).unwrap_err();
        REFUSE_PLACING.with(|refusing| *refusing.borrow_mut() = None);
        REFUSE_PUTTING_BACK.with(|refusing| refusing.borrow_mut().clear());
        assert_eq!(failed, Failure::NotPutBack(vec!["m.dll".to_string()]));
        assert_eq!(std::fs::read(folder.join("m.dll")).unwrap(), b"new m");
        let done = scene.run(&mut third, None, None).unwrap();
        assert_eq!(done.unpacked + done.already_there, 4, "{done:?}");
        assert_eq!(read_manifest(&folder).unwrap().version, "3.0.0");
        assert_eq!(std::fs::read(folder.join("z.dll")).unwrap(), b"new z");
        assert_eq!(std::fs::read(folder.join("a.exe")).unwrap(), b"two a");
    }

    #[test]
    fn the_files_not_put_back_are_named_a_few_at_a_time_and_counted_after() {
        let folder = Path::new("C:\\x");
        let paths: Vec<PathBuf> = ["e.dll", "tools\\d.py", "a.exe", "c.dll", "b.dll"].iter().map(|name| folder.join(name)).collect();
        let stuck: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let words = |stuck: &[&Path]| match not_put_back(folder, stuck) {
            Failure::NotPutBack(names) => not_put_back_words(&names),
            other => panic!("{other:?}"),
        };
        assert_eq!(words(&stuck), "This did not finish: a.exe, b.dll, c.dll and 2 more could not be put back as they were, so the folder is not as it was. Running the setup again finishes it.");
        assert_eq!(words(&stuck[..3]), "This did not finish: a.exe, e.dll, tools\\d.py could not be put back as they were, so the folder is not as it was. Running the setup again finishes it.");
        assert!(words(&stuck[2..3]).contains(": a.exe could not be put back as it was,"));
        assert!(words(&[stuck[0], stuck[0]]).contains(": e.dll could not be put back as it was,"), "a file named twice is named once");
        // what carries the names has all of them, and a line says three
        assert_eq!(not_put_back(folder, &stuck), Failure::NotPutBack(["a.exe", "b.dll", "c.dll", "e.dll", "tools\\d.py"].map(String::from).to_vec()));
        let all = ["a", "b", "c", "d"].map(String::from);
        assert_eq!(named(&all), (&all[..3], 1));
        assert_eq!(named(&all[..2]), (&all[..2], 0));
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

    /// Holds `path` open for `milliseconds` on another thread, as a virus scanner or the search index does a new file: for reading, with no sharing for deleting or replacing it.
    fn held_for(path: &Path, milliseconds: u64) -> std::thread::JoinHandle<()> {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        let held = std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(path).unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(milliseconds));
            drop(held);
        })
    }

    #[test]
    fn the_manifests_own_rename_is_tried_again_while_something_has_it_open_for_a_moment() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = |version: &str| Manifest { version: version.to_string(), test: true, files: vec!["a.exe".to_string()], ..Manifest::default() };
        write_manifest(dir.path(), &manifest("1.0.0")).unwrap();
        // (what a rename made once meets: the target is open, and nothing lets it be replaced)
        std::fs::write(dir.path().join("other"), "x").unwrap();
        let release = held_for(&dir.path().join(MANIFEST), 100);
        let error = std::fs::rename(dir.path().join("other"), dir.path().join(MANIFEST)).unwrap_err();
        assert!(matches!(error.raw_os_error(), Some(5 | 32)), "{error:?}");
        release.join().unwrap();
        // the manifest's own, with the lock gone after 400 ms
        let release = held_for(&dir.path().join(MANIFEST), 400);
        let began = std::time::Instant::now();
        write_manifest(dir.path(), &manifest("2.0.0")).unwrap();
        assert!(began.elapsed() >= Duration::from_millis(250), "it did wait for the lock to go: {:?}", began.elapsed());
        release.join().unwrap();
        assert_eq!(read_manifest(dir.path()).unwrap().version, "2.0.0");
        assert!(!dir.path().join(format!("{MANIFEST}{WAITING}")).exists(), "nothing is left waiting");
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

    /// `read_manifest` as 0.1.4 had it: what an older setup or uninstaller reads a newer manifest with.
    fn read_manifest_as_0_1_4(text: &str) -> Manifest {
        let mut manifest = Manifest::default();
        for line in text.lines() {
            match line.split_once(' ') {
                Some(("version", version)) => manifest.version = version.to_string(),
                Some(("file", file)) if crate::pack::is_safe(file) => manifest.files.push(file.to_string()),
                _ if line == "test" => manifest.test = true,
                _ => {}
            }
        }
        manifest
    }

    fn stamp_on_disk(path: &Path) -> Stamp {
        Stamp::of(&std::fs::metadata(path).unwrap())
    }

    #[test]
    fn the_manifest_holds_a_record_of_each_file_and_the_last_runs_numbers_and_an_older_reader_passes_over_them() {
        let scene = Scene::new();
        let mut package = scene.package("1.0.0", &[("dogmind-app.exe", b"app"), ("tools\\py thon\\python.exe", b"py"), ("\u{65e5}\u{672c}\\x", b"j")]);
        let done = scene.run(&mut package, None, None).unwrap();
        let folder = scene.folder();
        let manifest = read_manifest(&folder).unwrap();
        // one for each file of the package, and none for the uninstaller, which no package holds
        let mut written: Vec<&str> = manifest.records.iter().map(|(path, _)| path.as_str()).collect();
        written.sort();
        assert_eq!(written, ["dogmind-app.exe", "tools\\py thon\\python.exe", "\u{65e5}\u{672c}\\x"]);
        assert!(manifest.files.contains(&UNINSTALLER.to_string()));
        for (path, record) in &manifest.records {
            let place = folder.join(path);
            assert_eq!(record.sha256, hash_file(&place, &mut |_| {}).unwrap(), "{path}");
            assert_eq!(Stamp { size: record.size, modified: record.modified }, stamp_on_disk(&place), "read back from the disk after it was placed: {path}");
        }
        let run = manifest.last_run.clone().unwrap();
        assert_eq!(run.counts, done);
        let text = std::fs::read_to_string(folder.join(MANIFEST)).unwrap();
        assert!(text.starts_with("version 1.0.0\ntest\nfile "), "{text}");
        assert_eq!(text.lines().filter(|line| line.starts_with("sum ")).count(), 3);
        assert!(text.contains("\nsum ") && text.contains(" tools\\py thon\\python.exe\n"), "a path is the rest of its line, spaces and all");
        assert!(text.contains("\ntook checking=") && text.contains(" finishing=") && text.contains("\ncount unpacked=3 already_there=0 hashed=0 removed=0\n"), "{text}");
        // an older setup or uninstaller reads what it knows of it, and the same as before
        let old = read_manifest_as_0_1_4(&text);
        assert_eq!((old.version.as_str(), old.test, &old.files), ("1.0.0", true, &manifest.files));
        // and a second run replaces the numbers of the first, and does not add to them
        let mut again = scene.package("1.0.1", &[("dogmind-app.exe", b"app"), ("tools\\py thon\\python.exe", b"py"), ("\u{65e5}\u{672c}\\x", b"j")]);
        scene.run(&mut again, None, None).unwrap();
        let text = std::fs::read_to_string(folder.join(MANIFEST)).unwrap();
        assert_eq!((text.matches("\ntook ").count(), text.matches("\ncount ").count(), text.matches("\nsum ").count()), (1, 1, 3));
        assert!(text.contains("\ncount unpacked=0 already_there=3 hashed=0 removed=0\n"), "{text}");
    }

    #[test]
    fn lines_a_reader_does_not_understand_are_passed_over_and_a_record_that_is_not_in_order_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let text = format!(
            "version 2\nfile a.exe\nsum {hash} 5 123 a.exe\nsum {hash} 5 123\nsum nothing 5 123 b.exe\nsum {hash} five 123 c.exe\nsum {hash} 5 123 ..\\d.exe\nsum {hash} 5 123 e f\\g h.exe\ntook checking=7 other=1 bad\ncount hashed=3 unpacked=x\nsomething else\n"
        );
        std::fs::write(dir.path().join(MANIFEST), text).unwrap();
        let manifest = read_manifest(dir.path()).unwrap();
        let paths: Vec<&str> = manifest.records.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(paths, ["a.exe", "e f\\g h.exe"]);
        assert_eq!(manifest.records[0].1, Record { sha256: unhex(hash).unwrap(), size: 5, modified: 123 });
        let run = manifest.last_run.unwrap();
        assert_eq!((run.checking, run.counts.hashed, run.counts.unpacked), (7, 3, 0));
    }

    #[test]
    fn an_update_reads_only_the_files_it_has_no_record_of_or_whose_record_no_longer_fits() {
        let scene = Scene::new();
        let big = noise(300_000);
        let mut first = scene.package("1.0.0", &[("a.exe", b"one a"), ("b.dll", &big), ("c.dll", b"one c")]);
        scene.run(&mut first, None, None).unwrap();
        // a.exe is of the same size as it was and another file: it is read, and found other. b.dll and c.dll are not read at all.
        let mut second = scene.package("2.0.0", &[("a.exe", b"two a"), ("b.dll", &big), ("c.dll", b"one c")]);
        let done = scene.run(&mut second, None, None).unwrap();
        assert_eq!(done, Installed { unpacked: 1, already_there: 2, removed: 0, hashed: 1 });
        let mut third = scene.package("3.0.0", &[("a.exe", b"two a"), ("b.dll", &big), ("c.dll", b"one c")]);
        let done = scene.run(&mut third, None, None).unwrap();
        assert_eq!(done, Installed { unpacked: 0, already_there: 3, removed: 0, hashed: 0 }, "all three have a record that fits");
        let run = read_manifest(&scene.folder()).unwrap().last_run.unwrap();
        assert_eq!(run.counts, done);
    }

    /// An install of 0.1.4 has `file` lines and nothing else: it is updated as it always was, every file of the size it should be read once, and written down for the next update.
    #[test]
    fn an_install_with_file_lines_only_is_updated_and_has_its_records_after() {
        let scene = Scene::new();
        let mut first = scene.package("1.0.0", &[("a.exe", b"one a"), ("b.dll", b"same b"), ("gone.py", b"old")]);
        scene.run(&mut first, None, None).unwrap();
        let folder = scene.folder();
        std::fs::write(folder.join(MANIFEST), format!("version 1.0.0\ntest\nfile a.exe\nfile b.dll\nfile gone.py\nfile {UNINSTALLER}\n")).unwrap();
        let before = read_manifest(&folder).unwrap();
        assert!(before.records.is_empty() && before.last_run.is_none() && before.files.len() == 4);
        let mut second = scene.package("2.0.0", &[("a.exe", b"two a"), ("b.dll", b"same b"), ("new.py", b"new")]);
        let done = scene.run(&mut second, None, None).unwrap();
        assert_eq!(done, Installed { unpacked: 2, already_there: 1, removed: 1, hashed: 2 }, "a.exe and b.dll are read; new.py is not there to be");
        assert_eq!(std::fs::read(folder.join("a.exe")).unwrap(), b"two a");
        assert!(!folder.join("gone.py").exists());
        let after = read_manifest(&folder).unwrap();
        assert_eq!((after.version.as_str(), after.records.len()), ("2.0.0", 3));
        let mut third = scene.package("3.0.0", &[("a.exe", b"two a"), ("b.dll", b"same b"), ("new.py", b"new")]);
        assert_eq!(scene.run(&mut third, None, None).unwrap(), Installed { unpacked: 0, already_there: 3, removed: 0, hashed: 0 });
    }

    #[test]
    fn a_file_with_another_time_than_the_one_written_down_is_read_again_and_written_down_anew() {
        let scene = Scene::new();
        let files: [(&str, &[u8]); 2] = [("a.exe", b"one a"), ("b.dll", b"one b")];
        let mut first = scene.package("1.0.0", &files);
        scene.run(&mut first, None, None).unwrap();
        let folder = scene.folder();
        let set_time = |name: &str, ahead: u64| {
            let file = std::fs::OpenOptions::new().write(true).open(folder.join(name)).unwrap();
            file.set_modified(std::time::SystemTime::now() + Duration::from_secs(ahead)).unwrap();
        };
        // touched: the same bytes, another time
        set_time("b.dll", 3600);
        let mut second = scene.package("1.0.1", &files);
        assert_eq!(scene.run(&mut second, None, None).unwrap(), Installed { unpacked: 0, already_there: 2, removed: 0, hashed: 1 });
        let records = read_manifest(&folder).unwrap().records;
        let of = |name: &str| records.iter().find(|(path, _)| path == name).unwrap().1.clone();
        assert_eq!(Stamp { size: 5, modified: of("b.dll").modified }, stamp_on_disk(&folder.join("b.dll")), "it is written down as it is now");
        let mut third = scene.package("1.0.2", &files);
        assert_eq!(scene.run(&mut third, None, None).unwrap(), Installed { unpacked: 0, already_there: 2, removed: 0, hashed: 0 });
        // changed by someone: other bytes of the same size, and another time. It is read, found not to be the package's, and replaced.
        std::fs::write(folder.join("a.exe"), b"edit!").unwrap();
        set_time("a.exe", 7200);
        let mut fourth = scene.package("1.0.3", &files);
        assert_eq!(scene.run(&mut fourth, None, None).unwrap(), Installed { unpacked: 1, already_there: 1, removed: 0, hashed: 1 });
        assert_eq!(std::fs::read(folder.join("a.exe")).unwrap(), b"one a");
        let mut fifth = scene.package("1.0.4", &files);
        assert_eq!(scene.run(&mut fifth, None, None).unwrap().hashed, 0);
    }

    /// The update goes by the install's own records, as it must to be quick, and a size and a time can be put back by whoever can write the folder: a file changed that way stays through an
    /// update. The setup a person runs is how an install is repaired, and reads every file of the size it should be.
    #[test]
    fn a_file_changed_with_its_size_and_time_put_back_stays_through_an_update_and_a_setup_run_by_hand_replaces_it() {
        let scene = Scene::new();
        let files: [(&str, &[u8]); 3] = [("a.exe", b"one a"), ("b.dll", b"one b"), ("c.dll", b"one c")];
        let mut first = scene.package("1.0.0", &files);
        scene.run(&mut first, None, None).unwrap();
        let folder = scene.folder();
        let written = stamp_on_disk(&folder.join("a.exe"));
        // other bytes of the same size, and the last-write time it had
        let time = std::fs::metadata(folder.join("a.exe")).unwrap().modified().unwrap();
        std::fs::write(folder.join("a.exe"), b"one X").unwrap();
        std::fs::OpenOptions::new().write(true).open(folder.join("a.exe")).unwrap().set_modified(time).unwrap();
        assert_eq!(stamp_on_disk(&folder.join("a.exe")), written, "its size and its time say nothing of the change");
        // an update goes by what was written down: nothing is read, and the file stays
        let mut second = scene.package("1.0.1", &files);
        assert_eq!(scene.run(&mut second, None, None).unwrap(), Installed { unpacked: 0, already_there: 3, removed: 0, hashed: 0 });
        assert_eq!(std::fs::read(folder.join("a.exe")).unwrap(), b"one X");
        // the same package, run by hand, reads each file of that size, finds this one other, and puts the right one there
        let mut third = scene.package("1.0.2", &files);
        assert_eq!(scene.run_by_hand(&mut third).unwrap(), Installed { unpacked: 1, already_there: 2, removed: 0, hashed: 3 });
        assert_eq!(std::fs::read(folder.join("a.exe")).unwrap(), b"one a");
        // the records are written down after it as they are after any run, for the update that comes next
        assert_eq!(read_manifest(&folder).unwrap().records.len(), 3);
        let mut fourth = scene.package("1.0.3", &files);
        assert_eq!(scene.run(&mut fourth, None, None).unwrap(), Installed { unpacked: 0, already_there: 3, removed: 0, hashed: 0 });
        // and a run by hand over files that are all as they should be reads every one of them, and changes none
        let mut fifth = scene.package("1.0.4", &files);
        assert_eq!(scene.run_by_hand(&mut fifth).unwrap(), Installed { unpacked: 0, already_there: 3, removed: 0, hashed: 3 });
        let manifest = read_manifest(&folder).unwrap();
        assert_eq!(manifest.version, "1.0.4");
        for (path, record) in &manifest.records {
            assert_eq!(Stamp { size: record.size, modified: record.modified }, stamp_on_disk(&folder.join(path)), "{path}");
        }
    }

    impl Scene {
        /// The package of these files with the files named left out of it (KEPT), as the app puts one together.
        fn package_without(&self, version: &str, files: &[(&str, &[u8])], kept: &[&str]) -> Package {
            drop(self.package(version, files));
            let part = self.dir.path().join(format!("setup-{version}-part.exe"));
            crate::pack::tests::leave_out(&self.dir.path().join(format!("setup-{version}.exe")), &part, kept);
            Package::open(&part).unwrap()
        }
    }

    #[test]
    fn a_package_that_leaves_files_out_goes_over_the_install_it_was_made_for_and_reads_none_of_them() {
        let scene = Scene::new();
        let big = noise(200_000);
        let mut first = scene.package("1.0.0", &[("dogmind-app.exe", b"app one"), ("big.dll", &big), ("same.txt", b"unchanged"), ("tools\\keep.py", b"keep")]);
        scene.run(&mut first, None, None).unwrap();
        let folder = scene.folder();
        let files: [(&str, &[u8]); 5] = [("dogmind-app.exe", b"app two!"), ("big.dll", &big), ("same.txt", b"unchanged"), ("tools\\keep.py", b"keep"), ("new.txt", b"new")];
        let mut second = scene.package_without("2.0.0", &files, &["big.dll", "same.txt", "tools\\keep.py"]);
        // (room for what is in the package and no more: what it leaves out needs none)
        let done = scene.run(&mut second, Some(ROOM_TO_SPARE + 100), None).unwrap();
        assert_eq!(done, Installed { unpacked: 2, already_there: 3, removed: 0, hashed: 0 }, "the files left out count as in place, and none of them is read");
        for (name, bytes) in files {
            assert_eq!(std::fs::read(folder.join(name)).unwrap(), bytes, "{name}");
        }
        let manifest = read_manifest(&folder).unwrap();
        assert_eq!(manifest.version, "2.0.0");
        for (name, _) in files {
            assert!(manifest.files.contains(&name.to_string()), "{name}");
        }
        assert_eq!(manifest.records.len(), 5, "the files that were left out are written down as the others are");
        for (path, record) in &manifest.records {
            assert_eq!(Stamp { size: record.size, modified: record.modified }, stamp_on_disk(&folder.join(path)), "{path}");
        }
        // what is left is what a whole setup of that version leaves, and one finds everything in place at once
        let mut whole = scene.package("2.0.1", &files);
        assert_eq!(scene.run(&mut whole, None, None).unwrap(), Installed { unpacked: 0, already_there: 5, removed: 0, hashed: 0 });
    }

    /// Only the app makes a package that leaves files out, and only `--update` runs it: the files it leaves out go by their records, whoever it is run by.
    #[test]
    fn a_package_that_leaves_files_out_goes_by_the_records_when_it_is_run_by_hand_too() {
        let scene = Scene::new();
        let big = noise(50_000);
        let mut first = scene.package("1.0.0", &[("dogmind-app.exe", b"app one"), ("big.dll", &big), ("same.txt", b"unchanged")]);
        scene.run(&mut first, None, None).unwrap();
        let files: [(&str, &[u8]); 3] = [("dogmind-app.exe", b"app two!"), ("big.dll", &big), ("same.txt", b"unchanged")];
        let mut second = scene.package_without("2.0.0", &files, &["big.dll", "same.txt"]);
        let done = scene.run_by_hand(&mut second).unwrap();
        assert_eq!(done, Installed { unpacked: 1, already_there: 2, removed: 0, hashed: 0 });
        assert_eq!(std::fs::read(scene.folder().join("dogmind-app.exe")).unwrap(), b"app two!");
    }

    #[test]
    fn a_file_left_out_is_found_by_its_hash_when_the_install_has_no_record_of_it() {
        let scene = Scene::new();
        let big = noise(50_000);
        let mut first = scene.package("1.0.0", &[("dogmind-app.exe", b"app one"), ("big.dll", &big), ("same.txt", b"unchanged")]);
        scene.run(&mut first, None, None).unwrap();
        let folder = scene.folder();
        // an install of 0.1.4: `file` lines, and no record of any file
        std::fs::write(folder.join(MANIFEST), format!("version 1.0.0\ntest\nfile dogmind-app.exe\nfile big.dll\nfile same.txt\nfile {UNINSTALLER}\n")).unwrap();
        let files: [(&str, &[u8]); 3] = [("dogmind-app.exe", b"app two!"), ("big.dll", &big), ("same.txt", b"unchanged")];
        let mut second = scene.package_without("2.0.0", &files, &["big.dll", "same.txt"]);
        let done = scene.run(&mut second, None, None).unwrap();
        assert_eq!(done, Installed { unpacked: 1, already_there: 2, removed: 0, hashed: 2 });
        assert_eq!(read_manifest(&folder).unwrap().records.len(), 3, "and now it has them");
    }

    #[test]
    fn a_file_left_out_that_is_not_as_it_was_stops_it_before_anything_changes_and_says_what_to_do() {
        let scene = Scene::new();
        let big = noise(50_000);
        let mut first = scene.package("1.0.0", &[("dogmind-app.exe", b"app one"), ("big.dll", &big), ("same.txt", b"unchanged")]);
        scene.run(&mut first, None, None).unwrap();
        let folder = scene.folder();
        let files: [(&str, &[u8]); 3] = [("dogmind-app.exe", b"app two!"), ("big.dll", &big), ("same.txt", b"unchanged")];
        let mut second = scene.package_without("2.0.0", &files, &["big.dll", "same.txt"]);
        let said = |failed: Failure| match failed {
            Failure::Said(words) => words,
            other => panic!("{other:?}"),
        };
        let manifest_before = std::fs::read(folder.join(MANIFEST)).unwrap();
        // gone
        std::fs::remove_file(folder.join("same.txt")).unwrap();
        let before = walk(&folder);
        let words = said(scene.run(&mut second, None, None).unwrap_err());
        assert!(words.contains("same.txt") && words.contains("full setup") && words.contains("release page") && words.contains("nothing has been changed"), "{words}");
        assert_eq!(walk(&folder), before, "not a file made, not one moved");
        assert_eq!(std::fs::read(folder.join("dogmind-app.exe")).unwrap(), b"app one");
        assert_eq!(std::fs::read(folder.join(MANIFEST)).unwrap(), manifest_before);
        // there, with other bytes of the same size, and another time
        std::fs::write(folder.join("same.txt"), b"unchangeX").unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(folder.join("same.txt")).unwrap();
        file.set_modified(std::time::SystemTime::now() + Duration::from_secs(3600)).unwrap();
        drop(file);
        let before = walk(&folder);
        assert!(said(scene.run(&mut second, None, None).unwrap_err()).contains("full setup"));
        assert_eq!(walk(&folder), before);
        // with no install at all there is nothing for it to leave out
        let fresh = Scene::new();
        let mut alone = fresh.package_without("2.0.0", &files, &["big.dll", "same.txt"]);
        assert!(said(fresh.run(&mut alone, None, None).unwrap_err()).contains("full setup"));
        assert!(walk(&fresh.folder()).is_empty() && read_manifest(&fresh.folder()).is_none());
        // the file as it was, put back, is read, found to be the one, and the update goes through
        std::fs::write(folder.join("same.txt"), b"unchanged").unwrap();
        let done = scene.run(&mut second, None, None).unwrap();
        assert_eq!(done, Installed { unpacked: 1, already_there: 2, removed: 0, hashed: 1 });
        assert_eq!(std::fs::read(folder.join("dogmind-app.exe")).unwrap(), b"app two!");
    }

    /// Another install of `folder`, as far as the mutex goes: it holds it on a thread of its own until the sender is used, and is holding it when this returns.
    fn holding(folder: &Path) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let (release, waiting) = std::sync::mpsc::channel::<()>();
        let (taken, ready) = std::sync::mpsc::channel::<()>();
        let folder = folder.to_path_buf();
        let thread = std::thread::spawn(move || {
            let lock = lock_folder(&folder, None, &AtomicBool::new(false), &mut |_| {}).unwrap();
            taken.send(()).unwrap();
            let _ = waiting.recv();
            drop(lock);
        });
        ready.recv().unwrap();
        (release, thread)
    }

    /// Lets go of what `holding` holds after `milliseconds`, on a thread of its own; `released` is set just before.
    fn let_go_after(release: std::sync::mpsc::Sender<()>, milliseconds: u64, released: std::sync::Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(milliseconds));
            released.store(true, Ordering::Relaxed);
            release.send(()).unwrap();
        })
    }

    impl Scene {
        fn job<'a>(&'a self, folder: &'a Path, stop: &'a AtomicBool, wait: Option<Duration>) -> Install<'a> {
            Install { folder, shell: &self.shell, free: None, stop, test: true, write_uninstaller: &|path| std::fs::write(path, "uninstaller"), update: true, fail_after: None, slowly: Duration::ZERO, wait }
        }
    }

    #[test]
    fn the_mutex_of_a_folder_is_named_by_its_path_in_lower_case_and_two_folders_do_not_share_one() {
        let name = lock_name(Path::new("C:\\Users\\Someone\\Programs\\Dogmind"));
        assert!(name.starts_with("Local\\DogmindSetup-") && name.len() == "Local\\DogmindSetup-".len() + 64, "{name}");
        assert_eq!(name, lock_name(Path::new("c:\\users\\someone\\PROGRAMS\\dogmind")));
        assert_eq!(name, lock_name(Path::new("C:\\Users\\Someone\\Programs\\Dogmind\\")));
        assert_eq!(name, lock_name(Path::new("\\\\?\\C:\\Users\\Someone\\Programs\\Dogmind")));
        assert_ne!(name, lock_name(Path::new("C:\\Users\\Someone\\Programs\\Dogmind2")));
        // two folders are held at once, and a folder is held by one at a time
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let stop = AtomicBool::new(false);
        let (release, other) = holding(a.path());
        let _second = lock_folder(b.path(), None, &stop, &mut |_| {}).unwrap();
        let upper = PathBuf::from(a.path().to_string_lossy().to_uppercase());
        let failed = lock_folder(&upper, Some(Duration::from_millis(100)), &stop, &mut |_| {}).err();
        assert_eq!(failed, Some(Failure::Said(ANOTHER_IS_RUNNING.to_string())), "the same folder, spelled another way, is held");
        release.send(()).unwrap();
        other.join().unwrap();
    }

    #[test]
    fn a_second_install_of_a_folder_waits_while_the_first_holds_it_and_goes_on_when_it_is_let_go() {
        let scene = Scene::new();
        let mut package = scene.package("1.0.0", &[("a", b"1"), ("tools\\b", b"2")]);
        let folder = scene.folder();
        let (release, other) = holding(&folder);
        let released = std::sync::Arc::new(AtomicBool::new(false));
        let letting = let_go_after(release, 400, released.clone());
        let (mut told, mut early) = (Vec::new(), false);
        let began = Instant::now();
        let stop = AtomicBool::new(false);
        let done = install_alone(&mut package, &scene.job(&folder, &stop, None), &mut |held| told.push(held), &mut |_| early |= !released.load(Ordering::Relaxed)).unwrap();
        assert!(!early, "nothing was done before the first was let go");
        assert!(began.elapsed() >= Duration::from_millis(350), "{:?}", began.elapsed());
        assert_eq!(told, [true, false], "it said that it waited, once, and when that was over");
        assert_eq!(done.unpacked, 2);
        assert_eq!(read_manifest(&folder).unwrap().version, "1.0.0");
        letting.join().unwrap();
        other.join().unwrap();
        // with nothing holding the folder it does not wait, nor say so
        told.clear();
        let again = install_alone(&mut package, &scene.job(&folder, &stop, None), &mut |held| told.push(held), &mut |_| {}).unwrap();
        assert!(told.is_empty() && again.unpacked == 0);
    }

    #[test]
    fn an_install_that_may_wait_only_so_long_fails_with_one_line_and_has_changed_nothing() {
        let scene = Scene::new();
        let mut package = scene.package("1.0.0", &[("a", b"1")]);
        let folder = scene.folder();
        let (release, other) = holding(&folder);
        let (mut told, began, stop) = (Vec::new(), Instant::now(), AtomicBool::new(false));
        let failed = install_alone(&mut package, &scene.job(&folder, &stop, Some(Duration::from_millis(300))), &mut |held| told.push(held), &mut |_| {}).unwrap_err();
        assert_eq!(failed, Failure::Said("Another install of this folder is still running, so nothing has been changed.".to_string()));
        assert!(began.elapsed() >= Duration::from_millis(300) && began.elapsed() < Duration::from_secs(10), "{:?}", began.elapsed());
        assert_eq!(told, [true, false]);
        assert!(!folder.exists(), "not a folder made, not a file written");
        assert_eq!(scene.shell.said.borrow().len(), 0);
        // and a removal, the same
        let failed = remove_alone(&folder, &scene.shell, Some(Duration::from_millis(200)), &mut |_| {}, &mut |_| {}).unwrap_err();
        assert_eq!(failed, "Another install of this folder is still running, so nothing has been changed.");
        release.send(()).unwrap();
        other.join().unwrap();
        assert!(install_alone(&mut package, &scene.job(&folder, &stop, Some(Duration::from_millis(300))), &mut |_| {}, &mut |_| {}).is_ok(), "let go, it is taken");
    }

    #[test]
    fn a_stop_while_it_waits_for_another_install_ends_it_and_leaves_what_the_other_has_there() {
        let scene = Scene::new();
        let mut package = scene.package("1.0.0", &[("a", b"1")]);
        let folder = scene.folder();
        // (what the other install has unpacked and not yet placed)
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(beside(&folder.join("a")), b"1").unwrap();
        let (release, other) = holding(&folder);
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let pressing = stop.clone();
        let presser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            pressing.store(true, Ordering::Relaxed);
        });
        let began = Instant::now();
        let mut told = Vec::new();
        let failed = install_alone(&mut package, &scene.job(&folder, &stop, None), &mut |held| told.push(held), &mut |_| {}).unwrap_err();
        assert_eq!(failed, Failure::Stopped);
        assert!(began.elapsed() < Duration::from_secs(5), "{:?}", began.elapsed());
        assert_eq!(told, [true, false]);
        assert!(beside(&folder.join("a")).exists() && folder.exists(), "what the other has there is not taken away");
        presser.join().unwrap();
        release.send(()).unwrap();
        other.join().unwrap();
        // a stop that comes after the mutex is taken takes away what this left waiting, as before
        let stop = AtomicBool::new(true);
        std::fs::remove_dir_all(&folder).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(beside(&folder.join("a")), b"1").unwrap();
        assert_eq!(install_alone(&mut package, &scene.job(&folder, &stop, None), &mut |_| {}, &mut |_| {}).unwrap_err(), Failure::Stopped);
        assert!(!folder.exists(), "nothing of it is left");
    }

    #[test]
    fn a_removal_waits_while_another_install_of_the_folder_holds_it_and_goes_on_when_it_is_let_go() {
        let scene = Scene::new();
        let mut package = scene.package("1.0.0", &[("a.exe", b"1")]);
        scene.run(&mut package, None, None).unwrap();
        let folder = scene.folder();
        let (release, other) = holding(&folder);
        let released = std::sync::Arc::new(AtomicBool::new(false));
        let letting = let_go_after(release, 300, released.clone());
        let (mut told, mut early) = (Vec::new(), false);
        remove_alone(&folder, &scene.shell, None, &mut |held| told.push(held), &mut |_| early |= !released.load(Ordering::Relaxed)).unwrap();
        assert!(!early && told == [true, false]);
        assert!(read_manifest(&folder).is_none());
        letting.join().unwrap();
        other.join().unwrap();
    }

    /// A junction at `link` to `target`, as `mklink /J` makes it.
    fn make_junction(link: &Path, target: &Path) {
        use std::os::windows::process::CommandExt;
        let made = std::process::Command::new("cmd").arg("/c").arg("mklink").arg("/J").arg(link).arg(target).creation_flags(0x0800_0000).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().unwrap();
        assert!(made.success());
    }

    #[test]
    fn an_update_whose_folder_has_been_made_a_link_stops_before_anything_changes_and_names_it() {
        let scene = Scene::new();
        let mut first = scene.package("1.0.0", &[("a.exe", b"1"), ("tools\\x.py", b"2"), ("tools\\deep\\y.py", b"3")]);
        scene.run(&mut first, None, None).unwrap();
        let folder = scene.folder();
        // the folder `tools` is now a junction to a folder outside the install, which holds files of the same names
        let elsewhere = scene.dir.path().join("elsewhere");
        tree(&elsewhere, &[("x.py", b"the person's x"), ("deep\\y.py", b"the person's y"), ("z.py", b"the person's z")]);
        std::fs::remove_dir_all(folder.join("tools")).unwrap();
        make_junction(&folder.join("tools"), &elsewhere);
        let mut second = scene.package("2.0.0", &[("a.exe", b"11"), ("tools\\x.py", b"22"), ("tools\\deep\\y.py", b"33")]);
        let (inside, outside) = (walk(&folder), walk(&elsewhere));
        let manifest_before = std::fs::read(folder.join(MANIFEST)).unwrap();
        for by_hand in [false, true] {
            let failed = if by_hand { scene.run_by_hand(&mut second) } else { scene.run(&mut second, None, None) };
            let Err(Failure::Said(words)) = failed else { panic!("{failed:?}") };
            assert!(words.contains(&folder.join("tools").display().to_string()) && words.contains("nothing has been changed"), "{words}");
            assert!(!words.contains('\n'), "one line: {words}");
            assert_eq!(walk(&folder), inside, "not a file made or moved in the install");
            assert_eq!(walk(&elsewhere), outside, "not a file made, moved aside or deleted outside it");
            assert_eq!(std::fs::read(elsewhere.join("x.py")).unwrap(), b"the person's x");
            assert_eq!(std::fs::read(elsewhere.join("deep\\y.py")).unwrap(), b"the person's y");
            assert_eq!(std::fs::read(folder.join("a.exe")).unwrap(), b"1");
            assert_eq!(std::fs::read(folder.join(MANIFEST)).unwrap(), manifest_before);
        }
        assert_eq!(scene.shell.said.borrow().len(), 1, "nothing was registered again");
        // and with the link taken away, the same update goes through
        std::fs::remove_dir(folder.join("tools")).unwrap();
        assert!(scene.run(&mut second, None, None).is_ok());
        assert_eq!(std::fs::read(folder.join("tools\\x.py")).unwrap(), b"22");
        assert_eq!(std::fs::read(elsewhere.join("x.py")).unwrap(), b"the person's x");
    }

    #[test]
    fn a_changed_file_of_the_size_it_had_is_counted_in_the_room_an_update_asks_for() {
        let scene = Scene::new();
        let (a, b) = (noise(200_000), noise(150_000));
        let mut first = scene.package("1.0.0", &[("a", &a), ("b", &b)]);
        scene.run(&mut first, None, None).unwrap();
        // the new `a` is another file of the same size, and `b` is the same: the install's record of `b` says so, and it is not read to know
        let mut changed = a.clone();
        changed[1000] ^= 1;
        let mut second = scene.package("2.0.0", &[("a", &changed), ("b", &b)]);
        let folder = scene.folder();
        let before = walk(&folder);
        let size = a.len() as u64;
        let failed = scene.run(&mut second, Some(ROOM_TO_SPARE + size - 1), None).unwrap_err();
        assert_eq!(failed, Failure::TooLittleRoom { needed: ROOM_TO_SPARE + size, free: ROOM_TO_SPARE + size - 1 }, "the bytes of the changed file are asked for, and not those of `b`");
        assert_eq!(walk(&folder), before, "nothing was written");
        let done = scene.run(&mut second, Some(ROOM_TO_SPARE + size), None).unwrap();
        assert_eq!((done.unpacked, done.already_there), (1, 1));
        assert_eq!(std::fs::read(folder.join("a")).unwrap(), changed);
        // a file whose time is not the one written down is counted too, as it is read to find out
        let file = std::fs::OpenOptions::new().write(true).open(folder.join("b")).unwrap();
        file.set_modified(std::time::SystemTime::now() + Duration::from_secs(3600)).unwrap();
        drop(file);
        let mut third = scene.package("3.0.0", &[("a", &changed), ("b", &b)]);
        let failed = scene.run(&mut third, Some(ROOM_TO_SPARE + b.len() as u64 - 1), None).unwrap_err();
        assert_eq!(failed, Failure::TooLittleRoom { needed: ROOM_TO_SPARE + b.len() as u64, free: ROOM_TO_SPARE + b.len() as u64 - 1 });
        assert!(scene.run(&mut third, Some(ROOM_TO_SPARE + b.len() as u64), None).is_ok());
    }
}
