//! Folders the person has named for commands to read. The container that runs commands can read only what its SID is on, so each named folder gets one ACE for it,
//! read and execute, which its files and folders inherit, and the folders on the way to it get traverse alone, for themselves. Network and writes are as they were.
//!
//! These are lasting changes to the person's own files, so all of it is written down first, in the app's own folder, where neither a command nor a model's tool can write: what is
//! undone is what the list says was done, exact ACE by exact ACE, whenever that is, after a crash too. What keeps files out is taking the container's ACE away, as Windows does
//! not honour a deny ACE for a container's SID. Files that read as secrets are held out when the folder is added, and only those: the person is told so.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Security::*;

pub use crate::sandbox::Located;
use crate::sandbox::{self, Dacl, READ_EXECUTE, TRAVERSE};

pub const NOT_A_FOLDER: &str = "That is not a folder.";
pub const TOO_WIDE: &str = "That is too wide to open to commands: name a project or data folder inside it.";
pub const HOLDS_KEYS: &str = "That is a place keys are kept, so it cannot be opened to commands.";
pub const OVERLAPS: &str = "That is inside a folder already named, or holds one: remove that one first.";
pub const NOT_ALLOWED: &str = "Windows would not let its access be changed.";
pub const DRIVE_AWAY: &str = "The drive it was on is not here, so what it was given cannot be taken back yet. It stays on the list, and that is tried again each time the app starts.";
pub const LIST_DAMAGED: &str = "The list of folders opened to commands could not be read, so what it named may still be open to them. It is kept here, for anyone who wants to look.";
/// Every whole message above, for the test that each has its Japanese.
pub const WORDS: [&str; 7] = [NOT_A_FOLDER, TOO_WIDE, HOLDS_KEYS, OVERLAPS, NOT_ALLOWED, DRIVE_AWAY, LIST_DAMAGED];

const FILE_NAME: &str = "read-folders.json";
const FLAGS_INHERITED_BY_ALL: u8 = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;
/// How many things are looked at or changed before the person hears of it.
const TOLD_EVERY: u64 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    /// Being added, or its adding did not finish: on the next start it is undone.
    Adding,
    Open,
    /// Being taken back, or that did not finish: on the next start it goes on.
    Removing,
}

/// A file held out of what the folder gives, and its access list as it was.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Held {
    path: PathBuf,
    sddl: String,
    /// The list this app gave it, as text: when the folder goes, what is put back is put back over that and over nothing else. A file whose list is not that now has been changed by someone, or
    /// is a file made anew in its place, and is left as it is. (An entry from before this was kept has none, and is taken to be as it was if it still keeps its own list.)
    #[serde(default)]
    set: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Folder {
    pub path: PathBuf,
    pub state: State,
    /// Things Windows would not let be changed: commands cannot read those.
    #[serde(default)]
    pub not_changed: u64,
    /// The ACE on the folder is ours: there was none like it before.
    #[serde(default)]
    root_ace: bool,
    #[serde(default)]
    held: Vec<Held>,
    /// What it is whatever it comes to be called, so that a folder renamed or moved on its drive is still found: it takes its ACE with it.
    #[serde(default)]
    id: Option<String>,
}

impl Folder {
    /// Where it is now.
    pub fn whereabouts(&self) -> Located {
        match &self.id {
            Some(id) => sandbox::locate(id, &self.path),
            None if self.path.exists() => Located::At(self.path.clone()),
            None => Located::Gone,
        }
    }

    /// How many files and folders that read as secrets were held out when it was added.
    pub fn held_out(&self) -> usize {
        self.held.len()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Journal {
    #[serde(default)]
    folders: Vec<Folder>,
    /// The folders on the way that got a traverse ACE from this list, and so lose it when no named folder needs it.
    #[serde(default)]
    traverse_added: Vec<PathBuf>,
}

/// Where these are now, for a page: asked once and not waited for long, as a drive on the network can take half a minute to say it is not there.
pub fn whereabouts_soon(folders: &[Folder]) -> Vec<Located> {
    let (tell, told) = std::sync::mpsc::channel();
    let asked = folders.to_vec();
    std::thread::spawn(move || {
        let _ = tell.send(asked.iter().map(Folder::whereabouts).collect::<Vec<_>>());
    });
    told.recv_timeout(std::time::Duration::from_millis(400)).unwrap_or_else(|_| folders.iter().map(|folder| Located::At(folder.path.clone())).collect())
}

/// What taking a folder back came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removed {
    /// All that was given is gone, from here.
    From(PathBuf),
    /// It is not on its drive any more: nothing there to take back, and what became of it cannot be known from here.
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Added {
    Open { objects: u64, held_out: usize, not_changed: u64 },
    /// Stopped midway, and all of it undone.
    Stopped,
}

// One change to a person's files at a time, whoever asks.
static WORK: Mutex<()> = Mutex::new(());
/// Lists found to be unreadable, set aside, and not yet said.
static DAMAGED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Where the list is, in the app's own folder.
pub fn file_in(folder: &Path) -> PathBuf {
    folder.join(FILE_NAME)
}

fn read(file: &Path) -> Journal {
    let Ok(text) = std::fs::read_to_string(file) else { return Journal::default() };
    serde_json::from_str(&text).unwrap_or_else(|_| {
        // (it is said at the next start, by `recover`, and what it says is kept: the next thing written would otherwise be all that is left of it)
        let kept = sandbox::set_aside(file);
        DAMAGED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(kept);
        Journal::default()
    })
}

/// In one piece, before what it says is done.
fn write(file: &Path, journal: &Journal) -> Result<(), String> {
    sandbox::write_list(file, &serde_json::to_string_pretty(journal).map_err(|error| error.to_string())?)
}

pub fn list(file: &Path) -> Vec<Folder> {
    read(file).folders
}

/// What commands can read as well as the workspace, for the model to be told.
pub fn open_folders(file: &Path) -> Vec<PathBuf> {
    read(file).folders.into_iter().filter(|folder| folder.state == State::Open).filter_map(|folder| match folder.whereabouts() {
        Located::At(path) => Some(path),
        _ => None,
    }).collect()
}

fn entry<'a>(journal: &'a mut Journal, folder: &Path) -> Option<&'a mut Folder> {
    let wanted = sandbox::key(folder);
    journal.folders.iter_mut().find(|known| sandbox::key(&known.path) == wanted)
}

fn is_secret(path: &Path, secrets: &[PathBuf]) -> bool {
    crate::safety::sensitive(path) || secrets.iter().any(|secret| crate::safety::inside(secret, path))
}

/// Why this folder is not to be opened, in words for the person.
fn refusal(folder: &Path, secrets: &[PathBuf], journal: &Journal) -> Option<&'static str> {
    if !folder.is_dir() {
        return Some(NOT_A_FOLDER);
    }
    if is_secret(folder, secrets) {
        return Some(HOLDS_KEYS);
    }
    let place = sandbox::key(folder);
    let home = std::env::var_os("USERPROFILE").map(|home| sandbox::key(Path::new(&home)));
    let system = std::env::var_os("SystemRoot").map(|root| sandbox::key(Path::new(&root)));
    if folder.parent().is_none() || place.ends_with(':') || home.is_some_and(|home| sandbox::under(&home, &place)) || system.is_some_and(|system| sandbox::under(&place, &system) || sandbox::under(&system, &place)) {
        return Some(TOO_WIDE);
    }
    let now_at = |known: &Folder| if let Located::At(path) = known.whereabouts() { path } else { known.path.clone() };
    journal.folders.iter().map(|known| sandbox::key(&now_at(known))).any(|known| sandbox::under(&known, &place) || sandbox::under(&place, &known)).then_some(OVERLAPS)
}

/// The folders on the way to one, up to and not including the drive.
fn ways_to(folder: &Path) -> Vec<PathBuf> {
    folder.ancestors().skip(1).filter(|above| above.parent().is_some()).map(Path::to_path_buf).collect()
}

/// Read and run for the sandbox on all of this folder, for what ships beside the app. Returns how many things it could not change.
pub(crate) unsafe fn open_for_reading(root: &Path, sid: PSID) -> Result<u64, String> {
    let dacl = Dacl::read(root).map_err(|_| NOT_ALLOWED.to_string())?;
    if !dacl.has_exact(sid, READ_EXECUTE, FLAGS_INHERITED_BY_ALL) {
        dacl.with_exact(sid, READ_EXECUTE, FLAGS_INHERITED_BY_ALL).write_alone(root).map_err(|_| NOT_ALLOWED.to_string())?;
    }
    let mut no_word = |_| {};
    let mut walk = Walk { on: &mut no_word, cancel: None, objects: 0, not_changed: 0 };
    through_tree(root, sid, true, &mut walk);
    Ok(walk.not_changed)
}

/// What goes through a tree, and what it can tell and be stopped by.
struct Walk<'a> {
    on: &'a mut dyn FnMut(u64),
    cancel: Option<&'a AtomicBool>,
    objects: u64,
    not_changed: u64,
}

impl Walk<'_> {
    fn stopped(&self) -> bool {
        self.cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed))
    }
}

/// The one ACE that the folder's own would put on each thing under it, added or taken away, and nothing else about them changed. By hand: `TreeSetNamedSecurityInfo` strips the
/// protection from whatever it goes through, and so opens what the person had closed. What keeps its own list is passed by with all under it, and links are not gone through.
unsafe fn through_tree(root: &Path, sid: PSID, adding: bool, walk: &mut Walk) {
    let mut waiting = vec![root.to_path_buf()];
    while let Some(folder) = waiting.pop() {
        for item in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
            if walk.stopped() {
                return;
            }
            let Ok(kind) = item.file_type() else { continue };
            if kind.is_symlink() {
                continue;
            }
            let path = item.path();
            walk.objects += 1;
            if walk.objects % TOLD_EVERY == 0 {
                (walk.on)(walk.objects);
            }
            let Ok(dacl) = Dacl::read(&path) else {
                // (what cannot be looked at has had nothing given it: only then is it something to say)
                walk.not_changed += u64::from(adding && path.exists());
                continue;
            };
            if dacl.protected() {
                continue;
            }
            let has = dacl.from_above(sid, READ_EXECUTE, kind.is_dir());
            let changed = match (adding, has) {
                (true, None) => dacl.with_inherited(sid, READ_EXECUTE, dacl.flags_from_above(kind.is_dir())).write_alone(&path),
                (false, Some(flags)) => dacl.without_exact(sid, READ_EXECUTE, flags).write_alone(&path),
                _ => Ok(()),
            };
            if changed.is_err() && path.exists() {
                walk.not_changed += 1;
            }
            if kind.is_dir() {
                waiting.push(path);
            }
        }
    }
}

/// What in a tree reads as a secret, the first of each branch: what is under it goes with it, as it inherits from there. Links are not followed.
fn secrets_in(root: &Path, secrets: &[PathBuf], on: &mut dyn FnMut(u64), cancel: &AtomicBool) -> Vec<PathBuf> {
    let (mut found, mut waiting, mut looked) = (Vec::new(), vec![root.to_path_buf()], 0u64);
    while let Some(folder) = waiting.pop() {
        for item in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
            looked += 1;
            if looked % TOLD_EVERY == 0 {
                on(looked);
            }
            if cancel.load(Ordering::Relaxed) {
                return found;
            }
            let path = item.path();
            let Ok(kind) = item.file_type() else { continue };
            if kind.is_symlink() {
                continue;
            }
            if is_secret(&path, secrets) {
                found.push(path);
            } else if kind.is_dir() {
                waiting.push(path);
            }
        }
    }
    found
}

/// Opens a folder to commands. `on` hears how many things have been gone through; `cancel` stops it midway and undoes what it did.
pub fn add(file: &Path, folder: &Path, secrets: &[PathBuf], on: &mut dyn FnMut(u64), cancel: &AtomicBool) -> Result<Added, String> {
    let _one = WORK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut journal = read(file);
    // The folder itself, wherever the path to it goes through. An ACE lands on what a link leads to, and a link or a short name to the home would otherwise read as nothing in particular.
    let really = crate::safety::real_path(folder).unwrap_or_else(|| folder.to_path_buf());
    let folder = really.as_path();
    if let Some(why) = refusal(folder, secrets, &journal) {
        return Err(why.to_string());
    }
    journal.folders.push(Folder { path: folder.to_path_buf(), state: State::Adding, not_changed: 0, root_ace: false, held: Vec::new(), id: sandbox::file_id(folder) });
    write(file, &journal)?;
    // SAFETY: what each call below is given lives until it returns; `walk` until the tree call that takes it does.
    let outcome = unsafe { open(file, &mut journal, folder, secrets, on, cancel) };
    match outcome {
        Ok(Some(added)) => Ok(added),
        Ok(None) => {
            // SAFETY: as above.
            unsafe { take_back(file, &mut journal, folder, on) }?;
            Ok(Added::Stopped)
        }
        Err(error) => {
            // SAFETY: as above.
            unsafe { take_back(file, &mut journal, folder, on) }?;
            Err(error)
        }
    }
}

/// `None`: it was stopped.
unsafe fn open(file: &Path, journal: &mut Journal, folder: &Path, secrets: &[PathBuf], on: &mut dyn FnMut(u64), cancel: &AtomicBool) -> Result<Option<Added>, String> {
    let sid = sandbox::profile_sid()?;
    for above in ways_to(folder) {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let name = sandbox::wide_path(&above);
        if sandbox::has_grant(&name, sid, TRAVERSE, false) {
            continue;
        }
        journal.traverse_added.push(above.clone());
        write(file, journal)?;
        if sandbox::add_exact_ace(&above, sid, TRAVERSE, 0).is_err() {
            // (the system's own folders will not take one, and bypassing traverse checks gets a command through them anyway)
            journal.traverse_added.pop();
            write(file, journal)?;
        }
    }
    for path in secrets_in(folder, secrets, on, cancel) {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let Ok(dacl) = Dacl::read(&path) else { continue };
        // (one that keeps its own list takes nothing from the folder)
        if dacl.protected() {
            continue;
        }
        let Ok(sddl) = sandbox::dacl_text(&path) else { continue };
        let held = Held { path: path.clone(), sddl, set: None };
        entry(journal, folder).ok_or(NOT_ALLOWED)?.held.push(held);
        write(file, journal)?;
        dacl.as_explicit_and_protected(sid).write_alone(&path).map_err(|_| NOT_ALLOWED.to_string())?;
        // (written down with the rest, below: a crash before then leaves it without, which is put back only while it keeps its own list)
        if let (Ok(now), Some(last)) = (sandbox::dacl_text(&path), entry(journal, folder).and_then(|found| found.held.last_mut())) {
            last.set = Some(now);
        }
    }
    let dacl = Dacl::read(folder).map_err(|_| NOT_ALLOWED.to_string())?;
    let already = dacl.has_exact(sid, READ_EXECUTE, FLAGS_INHERITED_BY_ALL);
    entry(journal, folder).ok_or(NOT_ALLOWED)?.root_ace = !already;
    write(file, journal)?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let mut walk = Walk { on, cancel: Some(cancel), objects: 0, not_changed: 0 };
    if !already {
        // The folder first, so that what is made while this goes on has it too.
        dacl.with_exact(sid, READ_EXECUTE, FLAGS_INHERITED_BY_ALL).write_alone(folder).map_err(|_| NOT_ALLOWED.to_string())?;
        through_tree(folder, sid, true, &mut walk);
        if walk.stopped() {
            return Ok(None);
        }
    }
    let (objects, not_changed) = (walk.objects, walk.not_changed);
    let found = entry(journal, folder).ok_or(NOT_ALLOWED)?;
    found.state = State::Open;
    found.not_changed = not_changed;
    let held_out = found.held.len();
    write(file, journal)?;
    Ok(Some(Added::Open { objects, held_out, not_changed }))
}

/// Takes back what the list says was given for this folder.
pub fn remove(file: &Path, folder: &Path, on: &mut dyn FnMut(u64)) -> Result<Removed, String> {
    let _one = WORK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut journal = read(file);
    // SAFETY: what each call below is given lives until it returns.
    unsafe { take_back(file, &mut journal, folder, on) }
}

unsafe fn take_back(file: &Path, journal: &mut Journal, folder: &Path, on: &mut dyn FnMut(u64)) -> Result<Removed, String> {
    let sid = sandbox::profile_sid()?;
    let Some(found) = entry(journal, folder) else { return Ok(Removed::NotFound) };
    found.state = State::Removing;
    let (root_ace, held) = (found.root_ace, found.held.clone());
    let whereabouts = found.whereabouts();
    write(file, journal)?;
    // A folder renamed or moved on its drive took its ACE with it, and its path now may be another folder altogether, which is left alone.
    let now_at = match &whereabouts {
        Located::At(path) => Some(path.clone()),
        Located::Gone => None,
        // (it stays on the list as being taken back: what it was given may still be on it)
        Located::DriveAway => return Err(DRIVE_AWAY.to_string()),
    };
    if let (true, Some(place)) = (root_ace, &now_at) {
        let dacl = Dacl::read(place).map_err(|_| NOT_ALLOWED.to_string())?;
        if dacl.has_exact(sid, READ_EXECUTE, FLAGS_INHERITED_BY_ALL) {
            dacl.without_exact(sid, READ_EXECUTE, FLAGS_INHERITED_BY_ALL).write_alone(place).map_err(|_| NOT_ALLOWED.to_string())?;
        }
        // Whether or not it was there: a stop or a crash may have left the folder's gone and those under it not.
        let mut walk = Walk { on, cancel: None, objects: 0, not_changed: 0 };
        through_tree(place, sid, false, &mut walk);
        // (it stays on the list, and the next start tries again: the person is told, and it is not let go of as if done)
        if walk.not_changed > 0 {
            return Err(NOT_ALLOWED.to_string());
        }
    }
    for item in &held {
        // Under the folder, wherever that has gone.
        let path = match (&now_at, item.path.strip_prefix(folder)) {
            (Some(place), Ok(inside)) => place.join(inside),
            _ => item.path.clone(),
        };
        // Only a file that still has the list the app gave it: one whose list was changed since, or that was made anew in its place, is not written over with the old file's.
        let as_set = match &item.set {
            Some(set) => sandbox::dacl_text(&path).is_ok_and(|now| now == *set),
            None => Dacl::read(&path).is_ok_and(|dacl| dacl.protected()),
        };
        if path.exists() && as_set {
            sandbox::restore_dacl_text(&path, &item.sddl).map_err(|_| NOT_ALLOWED.to_string())?;
        }
    }
    let wanted = sandbox::key(folder);
    journal.folders.retain(|known| sandbox::key(&known.path) != wanted);
    let needed: Vec<String> = journal.folders.iter().flat_map(|known| ways_to(&known.path)).map(|above| sandbox::key(&above)).collect();
    let (going, staying): (Vec<PathBuf>, Vec<PathBuf>) = std::mem::take(&mut journal.traverse_added).into_iter().partition(|above| ways_to(folder).iter().any(|way| sandbox::key(way) == sandbox::key(above)) && !needed.contains(&sandbox::key(above)));
    journal.traverse_added = staying;
    for above in &going {
        if above.exists() {
            let dacl = Dacl::read(above).map_err(|_| NOT_ALLOWED.to_string())?;
            if dacl.has_exact(sid, TRAVERSE, 0) {
                dacl.without_exact(sid, TRAVERSE, 0).write_alone(above).map_err(|_| NOT_ALLOWED.to_string())?;
            }
        }
    }
    // (a workspace may have been relying on one of those: the next command puts it back)
    sandbox::forget_grants();
    write(file, journal)?;
    Ok(now_at.map_or(Removed::NotFound, Removed::From))
}

/// Before a command: a file held out of a named folder that has been written anew since is held out again. Many editors save by writing a new file and renaming it over the old one (JetBrains'
/// safe write, vim with some settings, many tools that write a config), and the new one inherits the folder's ACE for the container: `.env` and the like would be readable by commands, though
/// the person was told they are held out. It is the held list that is looked at, a few files and not the tree. What was not held is not looked for: the person is told that what is made
/// later can be read, and a walk through the tree before every command would take minutes in a large one. If an adding or a removing is going on it does its own, and this does not wait for it.
pub fn hold_out_again(file: &Path) {
    // (in a test, the work of the others going on at the same time is waited for, so that what a test asks of this is what it gets)
    #[cfg(test)]
    let one = WORK.lock().map_err(|_| ());
    #[cfg(not(test))]
    let one = WORK.try_lock().map_err(|_| ());
    let Ok(_one) = one else { return };
    let mut journal = read(file);
    if journal.folders.iter().all(|folder| folder.held.is_empty()) {
        return;
    }
    // SAFETY: the SID lives as long as the process; what each call below is given lives until it returns.
    let Ok(sid) = (unsafe { sandbox::profile_sid() }) else { return };
    let mut changed = false;
    for folder in journal.folders.iter_mut().filter(|folder| folder.state == State::Open) {
        for item in &mut folder.held {
            // SAFETY: as above.
            unsafe {
                let Ok(dacl) = Dacl::read(&item.path) else { continue };
                // (one that keeps its own list takes nothing from the folder)
                if dacl.protected() || dacl.as_explicit_and_protected(sid).write_alone(&item.path).is_err() {
                    continue;
                }
            }
            item.set = sandbox::dacl_text(&item.path).ok();
            changed = true;
        }
    }
    if changed {
        let _ = write(file, &journal);
    }
}

/// What a crash left half done: an adding is undone and a removing finished. For when the app starts, off the window's thread. Says what could not be.
pub fn recover(file: &Path, on: &mut dyn FnMut(u64)) -> Vec<String> {
    let _one = WORK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut journal = read(file);
    let unfinished: Vec<PathBuf> = journal.folders.iter().filter(|folder| folder.state != State::Open).map(|folder| folder.path.clone()).collect();
    // (this list's notes only: another list's stay for its own recover, which took them all and dropped the rest before, and in a run of
    // tests side by side one test's recover took another's note before that test could say it)
    let mut failures: Vec<String> = {
        let mut damaged = DAMAGED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (mine, others): (Vec<PathBuf>, Vec<PathBuf>) = std::mem::take(&mut *damaged).into_iter().partition(|kept| kept.parent() == file.parent());
        *damaged = others;
        mine.into_iter().map(|kept| format!("{}: {LIST_DAMAGED}", kept.display())).collect()
    };
    for folder in unfinished {
        // SAFETY: what each call below is given lives until it returns.
        if let Err(error) = unsafe { take_back(file, &mut journal, &folder, on) } {
            failures.push(format!("{}: {error}", folder.display()));
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// All in a temp folder, which goes with the test: nothing but those ACEs is ever changed.
    struct Stand {
        holder: tempfile::TempDir,
        work: PathBuf,
        named: PathBuf,
        outside: PathBuf,
        journal: PathBuf,
    }

    fn real(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).map_or(path.to_path_buf(), |full| PathBuf::from(full.to_string_lossy().trim_start_matches("\\\\?\\")))
    }

    fn stand() -> Stand {
        let holder = tempfile::tempdir().unwrap();
        let base = real(holder.path());
        // Each under a folder of its own: running a command in the workspace puts traverse on the workspace's way, and the way to the named folder must be this module's alone to prove.
        let (work, named, outside) = (base.join("w").join("work"), base.join("n").join("named"), base.join("o").join("outside"));
        for folder in [work.clone(), named.join("sub"), named.join(".ssh"), named.join("deep\\more"), outside.clone()] {
            std::fs::create_dir_all(folder).unwrap();
        }
        for (file, text) in [(named.join("a.txt"), "IN-A"), (named.join("sub\\b.txt"), "IN-B"), (named.join("deep\\more\\c.txt"), "IN-C"), (named.join(".ssh\\id_rsa"), "PLANTED-KEY"), (named.join(".env"), "PLANTED-ENV"), (outside.join("o.txt"), "OUT-O")] {
            std::fs::write(file, text).unwrap();
        }
        let journal = base.join("journal").join(FILE_NAME);
        Stand { holder, work, named, outside, journal }
    }

    fn command(stand: &Stand, line: &str) -> (Option<i32>, String) {
        let done = sandbox::run(line, &stand.work, &stand.work, false, Duration::from_secs(60), &AtomicBool::new(false)).unwrap();
        (done.status, format!("{}{}", sandbox::decode(&done.stdout), sandbox::decode(&done.stderr)))
    }

    fn reads(stand: &Stand, path: &Path, what: &str) -> bool {
        let (status, said) = command(stand, &format!("type \"{}\"", path.display()));
        status == Some(0) && said.contains(what)
    }

    /// Every access list under these, for comparing with what it was.
    fn lists(roots: &[&Path]) -> Vec<(PathBuf, String)> {
        let mut all = Vec::new();
        let mut waiting: Vec<PathBuf> = roots.iter().map(|root| root.to_path_buf()).collect();
        while let Some(path) = waiting.pop() {
            all.push((path.clone(), plain_text(&path)));
            if path.is_dir() {
                waiting.extend(std::fs::read_dir(&path).unwrap().flatten().map(|item| item.path()));
            }
        }
        all.sort();
        all
    }

    /// An access list, to the byte.
    fn plain_text(path: &Path) -> String {
        sandbox::dacl_text(path).unwrap()
    }

    fn no_word(_: u64) {}

    #[test]
    fn a_named_folder_can_be_read_and_nothing_else_changes_and_removing_it_puts_all_back() {
        let stand = stand();
        let parent = stand.named.parent().unwrap().to_path_buf();
        // The workspace is made ready by this first command, and nothing has been named.
        assert_eq!(command(&stand, "echo here> mine.txt").0, Some(0));
        let before = lists(&[&stand.named, &stand.outside]);
        let above_before = plain_text(&parent);
        let (a, c) = (stand.named.join("a.txt"), stand.named.join("deep\\more\\c.txt"));
        assert!(!reads(&stand, &a, "IN-A"), "not before it is named");
        let added = add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        assert!(matches!(added, Added::Open { held_out: 2, not_changed: 0, .. }), "{added:?}");
        assert_eq!(open_folders(&stand.journal), [stand.named.clone()]);
        // Reads inside, at any depth.
        assert!(reads(&stand, &a, "IN-A") && reads(&stand, &stand.named.join("sub\\b.txt"), "IN-B") && reads(&stand, &c, "IN-C"));
        assert!(command(&stand, &format!("for %f in (\"{}\\*\") do @echo %~nxf", stand.named.display())).1.contains("a.txt"), "and its folder can be listed");
        // Not outside it, and not the folder above, which it can only pass through.
        assert!(!reads(&stand, &stand.outside.join("o.txt"), "OUT-O"));
        let above = command(&stand, &format!("for %f in (\"{}\\*\") do @echo seen", parent.display()));
        assert!(!above.1.contains("seen"), "{above:?}");
        // What read as a secret when it was named.
        assert!(!reads(&stand, &stand.named.join(".ssh\\id_rsa"), "PLANTED-KEY"), "what was there when it was named");
        assert!(!reads(&stand, &stand.named.join(".env"), "PLANTED-ENV"));
        // Files made since inherit it, and that is all that is promised of secrets: one made since can be read, as the person is told.
        let (later, later_folder, later_key) = (stand.named.join("later.txt"), stand.named.join("later"), stand.named.join(".pem-later.pem"));
        std::fs::write(&later, "IN-LATER").unwrap();
        std::fs::create_dir(&later_folder).unwrap();
        std::fs::write(later_folder.join("d.txt"), "IN-D").unwrap();
        std::fs::write(&later_key, "LATER-KEY").unwrap();
        assert!(reads(&stand, &later, "IN-LATER") && reads(&stand, &later_folder.join("d.txt"), "IN-D"));
        assert!(reads(&stand, &later_key, "LATER-KEY"), "the plain limit of what was promised");
        // No writing in it, however it is tried.
        for line in [format!("echo x>> \"{}\"", a.display()), format!("echo x> \"{}\\new.txt\"", stand.named.display()), format!("powershell -NoProfile -Command \"[IO.File]::Delete('{}')\"", a.display()), format!("ren \"{}\" gone.txt", a.display()), format!("mkdir \"{}\\newfolder\"", stand.named.display()), format!("rmdir /s /q \"{}\\sub\"", stand.named.display())] {
            command(&stand, &line);
            assert_eq!(std::fs::read_to_string(&a).unwrap(), "IN-A", "{line}");
            assert!(!stand.named.join("new.txt").exists() && !stand.named.join("newfolder").exists() && stand.named.join("sub\\b.txt").is_file(), "{line}");
        }
        // The workspace still takes writes, and the network stays off.
        assert_eq!(command(&stand, "echo here> again.txt").0, Some(0));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let network = command(&stand, &format!("powershell -NoProfile -Command \"try {{ (New-Object Net.Sockets.TcpClient).Connect('127.0.0.1', {port}); 'CONNECTED' }} catch {{ 'refused' }}\""));
        assert!(!network.1.contains("CONNECTED"), "{network:?}");
        // Removing it takes back what was given, those made since included.
        remove(&stand.journal, &stand.named, &mut no_word).unwrap();
        assert!(list(&stand.journal).is_empty() && open_folders(&stand.journal).is_empty());
        for path in [&a, &c, &later, &later_folder.join("d.txt"), &later_key] {
            assert!(!reads(&stand, path, "IN-") && !reads(&stand, path, "LATER-KEY"), "{}", path.display());
        }
        for path in [&later, &later_folder.join("d.txt"), &later_key] {
            assert!(!sandbox::dacl_text(path).unwrap().contains(&sandbox::profile_sid_text()), "{}", path.display());
        }
        std::fs::remove_file(&later).unwrap();
        std::fs::remove_file(&later_key).unwrap();
        std::fs::remove_dir_all(&later_folder).unwrap();
        assert_eq!(lists(&[&stand.named, &stand.outside]), before, "to the byte");
        assert_eq!(plain_text(&parent), above_before, "the way to it too");
        drop(stand.holder);
    }

    /// What the person changed in a held file's list since the folder was named survives the folder going.
    #[test]
    fn a_held_file_whose_list_was_changed_since_keeps_the_change_when_the_folder_goes() {
        let stand = stand();
        let before = lists(&[&stand.named]);
        let added = add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        assert!(matches!(added, Added::Open { held_out: 2, .. }), "{added:?}");
        let (env, key) = (stand.named.join(".env"), stand.named.join(".ssh\\id_rsa"));
        let held = plain_text(&env);
        assert!(held.starts_with("D:P") && !held.contains(&sandbox::profile_sid_text()), "{held}");
        // The person gives the container's own SID a plain ACE on it (any change will do for a test), and leaves the other alone.
        unsafe { Dacl::read(&env).unwrap().with_exact(sandbox::profile_sid().unwrap(), READ_EXECUTE, 0).write_alone(&env).unwrap() };
        let changed = plain_text(&env);
        assert_ne!(changed, held);
        remove(&stand.journal, &stand.named, &mut no_word).unwrap();
        assert_eq!(plain_text(&env), changed, "their change is not written over with the list from before");
        // The one nobody touched is put back as it was.
        let key_before = before.iter().find(|(path, _)| *path == key).unwrap().1.clone();
        assert_eq!(plain_text(&key), key_before);
        // (and what the test changed goes with its folder)
        unsafe { Dacl::read(&env).unwrap().without_exact(sandbox::profile_sid().unwrap(), READ_EXECUTE, 0).write_alone(&env).unwrap() };
    }

    /// A held file saved anew by an editor (written new, renamed over the old) inherits the folder's ACE for the container; it is held out again before the next command runs.
    #[test]
    fn a_held_file_saved_anew_by_an_editor_is_held_out_again_before_the_next_command() {
        let stand = stand();
        // (the commands of this test find the app's folder where the list is)
        crate::workspace_grants::use_test_folder(stand.journal.parent().unwrap());
        add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        let (env, key) = (stand.named.join(".env"), stand.named.join(".ssh\\id_rsa"));
        let id = sandbox::profile_sid_text();
        assert!(!reads(&stand, &env, "PLANTED-ENV"));
        // An editor's safe write.
        let new = stand.named.join(".env.new");
        std::fs::write(&new, "REPLACED-ENV").unwrap();
        std::fs::rename(&new, &env).unwrap();
        // (confirmed: the new file has what the folder gives the container, and keeps no list of its own)
        let after = plain_text(&env);
        assert!(after.contains(&id) && !after.starts_with("D:P"), "{after}");
        unsafe { assert!(!Dacl::read(&env).unwrap().protected()) };
        // The next command holds it out again, and then runs.
        assert!(!reads(&stand, &env, "REPLACED-ENV"), "it is held out again before the command runs");
        let held = plain_text(&env);
        assert!(held.starts_with("D:P") && !held.contains(&id), "{held}");
        assert!(!reads(&stand, &key, "PLANTED-KEY"));
        // What was not held is as the person was told: a file made since is readable.
        std::fs::write(stand.named.join("later.txt"), "IN-LATER").unwrap();
        assert!(reads(&stand, &stand.named.join("later.txt"), "IN-LATER"));
        // Taken back, the folder goes and the new file is not given the old one's list: it is as any file of the folder is.
        remove(&stand.journal, &stand.named, &mut no_word).unwrap();
        assert!(!reads(&stand, &env, "REPLACED-ENV") && !reads(&stand, &stand.named.join("later.txt"), "IN-LATER"));
        assert!(!plain_text(&env).contains(&id), "nothing of the container is left on it");
    }

    /// What looking at the held list before a command costs, with one held file and with twenty (nothing to hold out again: the common case).
    #[test]
    fn looking_at_the_held_list_costs_little_before_a_command() {
        for held in [1usize, 20] {
            let stand = stand();
            for at in 0..held {
                std::fs::write(stand.named.join(format!(".env.{at}")), "PLANTED").unwrap();
            }
            add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
            let count = list(&stand.journal)[0].held.len();
            let started = std::time::Instant::now();
            for _ in 0..20 {
                hold_out_again(&stand.journal);
            }
            let each = started.elapsed() / 20;
            println!("HELD {count} files: {each:?} a look");
            assert!(count >= held && each < Duration::from_millis(250), "{count} held: {each:?}");
            remove(&stand.journal, &stand.named, &mut no_word).unwrap();
        }
    }

    #[test]
    fn stopping_midway_undoes_it_all() {
        let stand = stand();
        for at in 0..400 {
            std::fs::write(stand.named.join(format!("many{at}.txt")), "x").unwrap();
        }
        let before = lists(&[&stand.named]);
        let above_before = plain_text(stand.named.parent().unwrap());
        let cancel = AtomicBool::new(false);
        let mut told = 0;
        let added = add(&stand.journal, &stand.named, &[], &mut |count| {
            told = count;
            cancel.store(true, Ordering::Relaxed);
        }, &cancel).unwrap();
        assert_eq!(added, Added::Stopped);
        assert!(told > 0, "and it had said how far it had come");
        assert!(list(&stand.journal).is_empty());
        assert_eq!(lists(&[&stand.named]), before);
        assert_eq!(plain_text(stand.named.parent().unwrap()), above_before);
        assert!(!reads(&stand, &stand.named.join("a.txt"), "IN-A"));
    }

    #[test]
    fn after_a_crash_the_list_is_enough_to_undo_or_finish() {
        for left in [State::Adding, State::Removing] {
            let stand = stand();
            let before = lists(&[&stand.named]);
            let above_before = plain_text(stand.named.parent().unwrap());
            add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
            std::fs::write(stand.named.join("after.txt"), "IN-AFTER").unwrap();
            // As the file stands when the app goes down in the middle.
            let mut journal = read(&stand.journal);
            journal.folders[0].state = left;
            write(&stand.journal, &journal).unwrap();
            assert!(open_folders(&stand.journal).is_empty(), "the model is not told of what is not finished");
            assert!(recover(&stand.journal, &mut no_word).is_empty());
            assert!(list(&stand.journal).is_empty());
            assert!(!reads(&stand, &stand.named.join("a.txt"), "IN-A") && !reads(&stand, &stand.named.join("after.txt"), "IN-AFTER"), "{left:?}");
            std::fs::remove_file(stand.named.join("after.txt")).unwrap();
            assert_eq!(lists(&[&stand.named]), before, "{left:?}");
            assert_eq!(plain_text(stand.named.parent().unwrap()), above_before, "{left:?}");
        }
        // Nothing unfinished: nothing is touched.
        let stand = stand();
        add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        assert!(recover(&stand.journal, &mut no_word).is_empty());
        assert_eq!(open_folders(&stand.journal).len(), 1);
        remove(&stand.journal, &stand.named, &mut no_word).unwrap();
    }

    #[test]
    fn what_was_there_before_stays_when_a_folder_goes() {
        // The folder is on the way to another's workspace, and has a traverse ACE of its own from that: it keeps it.
        let stand = stand();
        let sid = unsafe { sandbox::profile_sid().unwrap() };
        unsafe { sandbox::add_exact_ace(&stand.named, sid, TRAVERSE, 0).unwrap() };
        let (before, above) = (plain_text(&stand.named), stand.named.parent().unwrap().to_path_buf());
        unsafe { sandbox::add_exact_ace(&above, sid, TRAVERSE, 0).unwrap() };
        let above_before = plain_text(&above);
        add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        assert!(reads(&stand, &stand.named.join("a.txt"), "IN-A"));
        remove(&stand.journal, &stand.named, &mut no_word).unwrap();
        assert_eq!(plain_text(&stand.named), before);
        assert_eq!(plain_text(&above), above_before, "not ours to take");
    }

    #[test]
    fn what_keeps_its_own_list_is_left_with_it() {
        let stand = stand();
        let own = stand.named.join("own");
        std::fs::create_dir(&own).unwrap();
        std::fs::write(own.join("f.txt"), "IN-OWN").unwrap();
        let sid = unsafe { sandbox::profile_sid().unwrap() };
        unsafe { Dacl::read(&own).unwrap().as_explicit_and_protected(sid).write_alone(&own).unwrap() };
        assert!(sandbox::dacl_text(&own).unwrap().starts_with("D:P"), "{}", sandbox::dacl_text(&own).unwrap());
        let before = sandbox::dacl_text(&own).unwrap();
        let added = add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        assert!(matches!(added, Added::Open { held_out: 2, .. }), "and it is not counted as held out by this: {added:?}");
        assert_eq!(sandbox::dacl_text(&own).unwrap(), before, "the person's own choice stands");
        assert!(!reads(&stand, &own.join("f.txt"), "IN-OWN") && reads(&stand, &stand.named.join("a.txt"), "IN-A"));
        remove(&stand.journal, &stand.named, &mut no_word).unwrap();
        assert_eq!(sandbox::dacl_text(&own).unwrap(), before);
    }

    #[test]
    fn what_is_too_long_for_an_ordinary_path_is_given_and_taken_back_too() {
        let stand = stand();
        let mut deep = stand.named.clone();
        while deep.as_os_str().len() < 300 {
            deep = deep.join("a-rather-long-folder-name");
        }
        std::fs::create_dir_all(&deep).unwrap();
        let file = deep.join("f.txt");
        std::fs::write(&file, "x").unwrap();
        let sid = sandbox::profile_sid_text();
        assert!(!sandbox::dacl_text(&file).unwrap().contains(&sid));
        let added = add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        assert!(matches!(added, Added::Open { not_changed: 0, .. }), "{added:?}");
        assert!(sandbox::dacl_text(&file).unwrap().contains(&sid));
        remove(&stand.journal, &stand.named, &mut no_word).unwrap();
        assert!(!sandbox::dacl_text(&file).unwrap().contains(&sid));
    }

    /// As `Documents` and much else are: lists with no word of auto-inheritance on them. `GetNamedSecurityInfo` reports everything under one as keeping to itself once the folder above has
    /// changed, though nothing on them has, and a walk that believed it left them all out.
    #[test]
    fn a_tree_whose_lists_say_nothing_of_inheritance_is_opened_and_put_back_to_the_byte() {
        let stand = stand();
        let text = sandbox::dacl_text(&stand.named).unwrap();
        sandbox::restore_dacl_text(&stand.named, &text.replacen("D:AI", "D:", 1)).unwrap();
        assert!(sandbox::dacl_text(&stand.named).unwrap().starts_with("D:("), "the stand-in has the shape it is for");
        // Everything in it made anew, so that it inherits from a list of that kind.
        let plain = stand.named.join("plain");
        std::fs::create_dir_all(plain.join("deep")).unwrap();
        std::fs::write(plain.join("f.txt"), "IN-F").unwrap();
        std::fs::write(plain.join("deep").join("g.txt"), "IN-G").unwrap();
        std::fs::write(plain.join(".env"), "PLANTED-ENV").unwrap();
        assert!(plain_text(&plain.join("f.txt")).starts_with("D:(A;;"), "as the kernel hands it down from such a folder: {}", plain_text(&plain.join("f.txt")));
        let before = lists(&[&stand.named]);
        let added = add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        assert!(matches!(added, Added::Open { not_changed: 0, .. }), "{added:?}");
        for path in [plain.join("f.txt"), plain.join("deep"), plain.join("deep").join("g.txt")] {
            let list = plain_text(&path);
            assert!(!list.starts_with("D:P") && list.contains(&sandbox::profile_sid_text()), "{}: {list}", path.display());
        }
        assert!(reads(&stand, &plain.join("f.txt"), "IN-F") && reads(&stand, &plain.join("deep").join("g.txt"), "IN-G"));
        assert!(!reads(&stand, &plain.join(".env"), "PLANTED-ENV"));
        // What is made now takes the grant the kernel's way, and goes with it all the same.
        let later = plain.join("later.txt");
        std::fs::write(&later, "IN-LATER").unwrap();
        assert!(reads(&stand, &later, "IN-LATER"));
        remove(&stand.journal, &stand.named, &mut no_word).unwrap();
        assert!(!reads(&stand, &later, "IN-LATER"), "{}", plain_text(&later));
        assert!(!plain_text(&later).contains(&sandbox::profile_sid_text()), "{}", plain_text(&later));
        std::fs::remove_file(&later).unwrap();
        assert_eq!(lists(&[&stand.named]), before, "to the byte");
    }

    fn renamed_is_found_and_taken_back(to: &Path, stand: &Stand, before: &[(PathBuf, String)]) {
        let old = stand.named.clone();
        std::fs::rename(&old, to).unwrap();
        // Another folder takes its name, as it may, and it has nothing to do with this.
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("other.txt"), "OTHER").unwrap();
        let other = lists(&[&old]);
        let shown = list(&stand.journal);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].whereabouts(), Located::At(real(to)), "and the page can say where it is");
        assert_eq!(open_folders(&stand.journal), [real(to)], "and the model is told where it is");
        assert!(reads(stand, &to.join("a.txt"), "IN-A"), "the grant went with it");
        // Asked for by the name the list has.
        assert_eq!(remove(&stand.journal, &old, &mut no_word).unwrap(), Removed::From(real(to)));
        assert!(list(&stand.journal).is_empty());
        assert!(!reads(stand, &to.join("a.txt"), "IN-A"), "nothing left behind");
        assert_eq!(lists(&[&old]), other, "what took its name was not touched");
        std::fs::remove_dir_all(&old).unwrap();
        std::fs::rename(to, &old).unwrap();
        assert_eq!(lists(&[&old]), before, "to the byte");
    }

    #[test]
    fn a_named_folder_renamed_or_moved_on_its_drive_is_found_and_leaves_no_grant() {
        let stand = stand();
        let before = lists(&[&stand.named]);
        add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        let renamed = stand.named.with_file_name("renamed");
        renamed_is_found_and_taken_back(&renamed, &stand, &before);
        add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        let moved = stand.outside.join("deep-down");
        renamed_is_found_and_taken_back(&moved, &stand, &before);
        // Held files too, wherever they have gone with it.
        assert!(!sandbox::dacl_text(&stand.named.join(".env")).unwrap().starts_with("D:P"));
    }

    #[test]
    fn a_folder_no_longer_on_its_drive_is_said_to_be_not_found() {
        let stand = stand();
        add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        std::fs::remove_dir_all(&stand.named).unwrap();
        assert_eq!(list(&stand.journal)[0].whereabouts(), Located::Gone);
        assert!(open_folders(&stand.journal).is_empty());
        assert_eq!(remove(&stand.journal, &stand.named, &mut no_word).unwrap(), Removed::NotFound);
        assert!(list(&stand.journal).is_empty());
    }

    #[test]
    fn a_drive_that_is_not_here_keeps_the_folder_on_the_list_until_it_is() {
        let stand = stand();
        let before = lists(&[&stand.named]);
        add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        // As if its drive were another: the serial number is what says which it is.
        let mut journal = read(&stand.journal);
        let real_id = journal.folders[0].id.clone().unwrap();
        let (_, number) = real_id.split_once(':').unwrap();
        journal.folders[0].id = Some(format!("{:016x}:{number}", 0x1234u64));
        write(&stand.journal, &journal).unwrap();
        // (not where it was, as far as this identity goes, so it looks for the drive)
        assert_eq!(list(&stand.journal)[0].whereabouts(), Located::DriveAway);
        assert_eq!(remove(&stand.journal, &stand.named, &mut no_word).unwrap_err(), DRIVE_AWAY);
        let left = list(&stand.journal);
        assert_eq!((left.len(), left[0].state), (1, State::Removing), "not let go of as if done");
        assert_eq!(recover(&stand.journal, &mut no_word).len(), 1, "and said again at a start");
        assert!(reads(&stand, &stand.named.join("a.txt"), "IN-A"), "it still has it, which is why it stays");
        // The drive back.
        let mut journal = read(&stand.journal);
        journal.folders[0].id = Some(real_id);
        write(&stand.journal, &journal).unwrap();
        assert!(recover(&stand.journal, &mut no_word).is_empty());
        assert!(list(&stand.journal).is_empty());
        assert_eq!(lists(&[&stand.named]), before);
    }

    /// The plain limit, which the person is told of: moving keeps a file's list, and nothing says where it went.
    /// Commands reach it only by a way through the folders above it, as the workspace's own folders are.
    #[test]
    fn a_file_moved_out_while_named_keeps_its_entry_and_can_be_read_only_where_commands_can_reach() {
        let stand = stand();
        add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        let (far, near) = (stand.outside.join("moved-out.txt"), stand.work.join("moved-in.txt"));
        std::fs::rename(stand.named.join("a.txt"), &far).unwrap();
        std::fs::rename(stand.named.join("sub\\b.txt"), &near).unwrap();
        remove(&stand.journal, &stand.named, &mut no_word).unwrap();
        assert!(sandbox::dacl_text(&far).unwrap().contains(&sandbox::profile_sid_text()), "if this ever stops being so, the words on the page can go");
        assert!(!reads(&stand, &far, "IN-A"), "no way to it");
        assert!(reads(&stand, &near, "IN-B"));
    }

    #[test]
    fn a_link_inside_is_not_gone_through() {
        let stand = stand();
        let mut link = std::process::Command::new("cmd");
        link.arg("/c").arg("mklink").arg("/J").arg(stand.named.join("through")).arg(&stand.outside).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        crate::tools::no_window(&mut link);
        assert!(link.status().unwrap().success());
        let before = plain_text(&stand.outside.join("o.txt"));
        add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        assert_eq!(plain_text(&stand.outside.join("o.txt")), before);
        assert!(!reads(&stand, &stand.named.join("through\\o.txt"), "OUT-O"));
        remove(&stand.journal, &stand.named, &mut no_word).unwrap();
        std::fs::remove_dir(stand.named.join("through")).unwrap();
    }

    #[test]
    fn what_is_too_wide_or_holds_keys_or_overlaps_is_refused_and_nothing_is_written() {
        let stand = stand();
        let attempt = |folder: &Path, secrets: &[PathBuf]| add(&stand.journal, folder, secrets, &mut no_word, &AtomicBool::new(false)).unwrap_err();
        assert_eq!(attempt(&stand.named.join("a.txt"), &[]), NOT_A_FOLDER);
        assert_eq!(attempt(&stand.named.join("nothing-here"), &[]), NOT_A_FOLDER);
        assert_eq!(attempt(&stand.named.join(".ssh"), &[]), HOLDS_KEYS);
        assert_eq!(attempt(&stand.named, std::slice::from_ref(&stand.named)), HOLDS_KEYS, "the app's own folder is on that list");
        let home = PathBuf::from(std::env::var_os("USERPROFILE").unwrap());
        let drive: PathBuf = home.ancestors().last().unwrap().to_path_buf();
        for wide in [home.clone(), home.parent().unwrap().to_path_buf(), drive, PathBuf::from(std::env::var_os("SystemRoot").unwrap())] {
            assert_eq!(attempt(&wide, &[]), TOO_WIDE, "{}", wide.display());
        }
        assert!(list(&stand.journal).is_empty());
        add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        assert_eq!(attempt(&stand.named.join("sub"), &[]), OVERLAPS);
        assert_eq!(attempt(stand.named.parent().unwrap(), &[]), OVERLAPS);
        assert_eq!(attempt(&stand.named, &[]), OVERLAPS);
        assert_eq!(list(&stand.journal).len(), 1);
        remove(&stand.journal, &stand.named, &mut no_word).unwrap();
        // Removing what is not named is nothing.
        remove(&stand.journal, &stand.outside, &mut no_word).unwrap();
    }

    #[test]
    fn a_folder_named_through_a_link_is_the_folder_it_leads_to() {
        let stand = stand();
        let link = stand.named.parent().unwrap().join("the-link");
        let mut made = std::process::Command::new("cmd");
        made.arg("/c").arg("mklink").arg("/J").arg(&link).arg(&stand.named).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        crate::tools::no_window(&mut made);
        assert!(made.status().unwrap().success());
        let before = lists(&[&stand.named]);
        let link_before = plain_text(&link);
        add(&stand.journal, &link, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        assert_eq!(list(&stand.journal)[0].path, stand.named, "what is written down is the folder");
        assert!(plain_text(&stand.named).contains(&sandbox::profile_sid_text()), "the ACE is on it, for what is made there to inherit");
        remove(&stand.journal, &stand.named, &mut no_word).unwrap();
        assert_eq!(lists(&[&stand.named]), before, "to the byte");
        assert_eq!(plain_text(&link), link_before);
        std::fs::remove_dir(&link).unwrap();
    }

    #[test]
    fn a_list_that_cannot_be_read_is_kept_and_said_and_is_not_written_over() {
        let stand = stand();
        std::fs::create_dir_all(stand.journal.parent().unwrap()).unwrap();
        std::fs::write(&stand.journal, "").unwrap();
        add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        let kept: Vec<PathBuf> = std::fs::read_dir(stand.journal.parent().unwrap()).unwrap().flatten().map(|item| item.path()).filter(|path| path.to_string_lossy().contains("damaged")).collect();
        assert_eq!(kept.len(), 1, "set aside before anything is written");
        let said = recover(&stand.journal, &mut no_word);
        assert_eq!(said.len(), 1);
        assert!(said[0].contains(LIST_DAMAGED) && said[0].contains(&*kept[0].to_string_lossy()), "{said:?}");
        assert!(recover(&stand.journal, &mut no_word).is_empty(), "said once");
        remove(&stand.journal, &stand.named, &mut no_word).unwrap();
    }

    #[test]
    fn a_held_file_that_has_gone_and_a_folder_that_has_gone_do_not_stop_the_rest() {
        let stand = stand();
        add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        std::fs::remove_file(stand.named.join(".env")).unwrap();
        remove(&stand.journal, &stand.named, &mut no_word).unwrap();
        assert!(list(&stand.journal).is_empty());
        let stand = self::stand();
        add(&stand.journal, &stand.named, &[], &mut no_word, &AtomicBool::new(false)).unwrap();
        std::fs::remove_dir_all(&stand.named).unwrap();
        remove(&stand.journal, &stand.named, &mut no_word).unwrap();
        assert!(list(&stand.journal).is_empty());
    }
}
