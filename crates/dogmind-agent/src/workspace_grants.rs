//! The write grant a workspace has for commands, for as long as something uses it. A command needs the container's ACE on its workspace, and that ACE lasts until it is taken
//! away: without this, commands could go on writing in every folder ever worked in. So what is granted is written down first, what uses a folder is counted, and when the count
//! comes to nothing the grant goes, on a thread of its own, as a tree can take minutes. What a chat has open, or a command running or in the background, counts.
//!
//! Only what the list says, or what can be proved, is taken back: the SID is this app's alone, so its exact write ACE on a folder this app's own sessions name can only be this
//! app's. What it cannot prove it leaves. The traverse ACEs on the way to a workspace, which show nothing of what is in a folder, stay.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Security::*;

use crate::sandbox::{self, Dacl, READ_EXECUTE, READ_WRITE_EXECUTE};

const FILE_NAME: &str = "workspace-grants.json";
const TO_ALL: u8 = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;

/// An access list as it was before a repository was guarded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Guarded {
    path: PathBuf,
    sddl: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Grant {
    root: PathBuf,
    #[serde(default)]
    guarded: Vec<Guarded>,
    /// What it is whatever it comes to be called: a folder renamed or moved on its drive takes its ACE with it.
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Journal {
    #[serde(default)]
    grants: Vec<Grant>,
}

/// The folders in use, by key, and how many things use each.
static USERS: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);
/// One change to a tree at a time, and no command being made ready during one.
static WORK: Mutex<()> = Mutex::new(());
static FILE: Mutex<Option<PathBuf>> = Mutex::new(None);
/// The app is ending: what is in use stays granted for when it comes back, rather than be walked now and again then.
static CLOSING: Mutex<bool> = Mutex::new(false);

#[cfg(test)]
thread_local! {
    /// A test's own list, so that tests going on together do not see each other's.
    static TEST_FILE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn users() -> MutexGuard<'static, Option<HashMap<String, u32>>> {
    USERS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Where the list is kept: in the app's own folder, where no command or tool of a model can write.
pub fn set_folder(folder: &Path) {
    *FILE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(folder.join(FILE_NAME));
}

/// From here on nothing is taken back: the app is ending.
pub fn app_is_closing() {
    *CLOSING.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
}

fn the_file() -> Option<PathBuf> {
    #[cfg(test)]
    {
        return TEST_FILE.with(|file| file.borrow().clone());
    }
    #[cfg(not(test))]
    FILE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

/// The app's folder, where it keeps this list and the rest of what it remembers.
pub(crate) fn data_folder() -> Option<PathBuf> {
    the_file()?.parent().map(Path::to_path_buf)
}

fn read(file: &Path) -> Journal {
    let Ok(text) = std::fs::read_to_string(file) else { return Journal::default() };
    serde_json::from_str(&text).unwrap_or_else(|_| {
        // (what a session names is found by its ACE at the next start: this is only for anyone who wants to look)
        sandbox::set_aside(file);
        Journal::default()
    })
}

fn write(file: &Path, journal: &Journal) -> Result<(), String> {
    sandbox::write_list(file, &serde_json::to_string_pretty(journal).map_err(|error| error.to_string())?)
}

/// What the list says is still given and could not be taken back, for when the app is leaving this PC and the list goes too.
pub fn still_granted(folder: &Path) -> Vec<PathBuf> {
    read(&folder.join(FILE_NAME)).grants.into_iter().map(|grant| grant.root).collect()
}

/// Something that uses a folder, for as long as this lasts: a chat open on it, or a command running in it.
pub struct Use {
    place: String,
    path: PathBuf,
    file: Option<PathBuf>,
}

impl Use {
    pub fn new(path: &Path) -> Use {
        let place = sandbox::key(path);
        *users().get_or_insert_with(HashMap::new).entry(place.clone()).or_insert(0) += 1;
        Use { place, path: path.to_path_buf(), file: the_file() }
    }

    /// What this leaves behind is not taken back, as in a test's own temp folder or a command's process ending with the app.
    #[cfg(test)]
    fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }
}

impl Drop for Use {
    fn drop(&mut self) {
        {
            let mut all = users();
            if let Some(count) = all.as_mut().and_then(|all| all.get_mut(&self.place)) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    all.as_mut().map(|all| all.remove(&self.place));
                }
            }
        }
        if *CLOSING.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
            return;
        }
        let Some(file) = self.file.take() else { return };
        // (a folder that was only ever passed through has nothing on the list to go, and no thread is started for it)
        let path = std::mem::take(&mut self.path);
        if !is_in_use(&path) && recorded_at(&file, &path) {
            std::thread::spawn(move || {
                in_the_background();
                let _ = release(&file, &path);
            });
        }
    }
}

/// What a take-back came to.
enum Taken {
    /// All of it.
    Whole,
    /// Something began to use the folder while it went on: it stops, and the command that wants it makes it whole again.
    InUse,
    /// Some things could not be read or written, and are as they were: the container's ACE may still be on them.
    Partly,
}

/// What a release came to: whether the folders it was asked about are all clear of the grant, and the roots whose take-back left something behind.
struct Released {
    done: bool,
    failed: Vec<PathBuf>,
}

/// What the person is told of a folder whose take-back left something behind (see `sweep`).
pub const NOT_TAKEN_BACK: &str = "Some of what commands were allowed to write here could not be taken back, as Windows would not let it be changed. It stays on the list, and that is tried again each time the app starts.";
/// Every whole message here, for the test that each has its Japanese.
pub const WORDS: [&str; 1] = [NOT_TAKEN_BACK];

/// What goes through a whole tree gives way to everything else on the machine, the disk included.
pub(crate) fn in_the_background() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};
    // SAFETY: on this thread alone, for as long as it lasts.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
}

/// Whether anything uses this folder, or one inside it, or one it is inside: a command's folder can be a project in a chat's wider workspace, and either holds the other's files.
pub(crate) fn is_in_use(path: &Path) -> bool {
    let place = sandbox::key(path);
    users().as_ref().is_some_and(|all| all.keys().any(|known| sandbox::under(known, &place) || sandbox::under(&place, known)))
}

fn recorded_at(file: &Path, path: &Path) -> bool {
    let place = sandbox::key(path);
    read(file).grants.iter().any(|grant| {
        let root = sandbox::key(&grant.root);
        sandbox::under(&root, &place) || sandbox::under(&place, &root)
    })
}

/// A command is about to run with its folder writable: on the list before anything is granted, and on it too if an earlier version of the app gave it. Then what changes it waits for this.
/// What cannot be written down is not given: nothing could take it back.
pub(crate) fn before_grant(root: &Path) -> Result<MutexGuard<'static, ()>, String> {
    let one = WORK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(file) = the_file() {
        let mut journal = read(&file);
        let place = sandbox::key(root);
        if !journal.grants.iter().any(|grant| sandbox::key(&grant.root) == place) {
            journal.grants.push(Grant { root: root.to_path_buf(), guarded: Vec::new(), id: sandbox::file_id(root) });
            write(&file, &journal)?;
        }
    }
    Ok(one)
}

/// An access list about to be changed to guard a repository, with what it was, so that it can be put back. `protected`: it keeps its own list now, which is what this leaves it doing, so that
/// what is written down of it already is its list from before; if nothing is, a person had closed it themselves. One that does not keep its own has been written anew by a person's own git.
pub(crate) fn before_guarding(root: &Path, path: &Path, sddl: String, protected: bool) -> Result<(), String> {
    let Some(file) = the_file() else { return Ok(()) };
    let mut journal = read(&file);
    let place = sandbox::key(root);
    let Some(grant) = journal.grants.iter_mut().find(|grant| sandbox::key(&grant.root) == place) else { return Ok(()) };
    if protected && grant.guarded.iter().any(|known| known.path == path) {
        return Ok(());
    }
    grant.guarded.retain(|known| known.path != path);
    grant.guarded.push(Guarded { path: path.to_path_buf(), sddl });
    write(&file, &journal)
}

/// One inherited ACE off everything under a folder, with these masks. What keeps its own list is passed by, and a workspace of its own inside it, and links are not gone through.
/// `None` if something began to use the folder meanwhile: then it stops, and the command that wants it makes it whole again. Otherwise how many things could not be read or written: the ACE may
/// still be on them, and that is not something to write down as done. (What is gone by the time it is looked at is not one.)
unsafe fn strip_tree(root: &Path, watch: &Path, sid: PSID, masks: &[u32]) -> Option<u32> {
    let gone = |path: &Path| std::fs::symlink_metadata(path).is_err();
    let mut waiting = vec![root.to_path_buf()];
    let (mut looked, mut failed) = (0u32, 0u32);
    while let Some(folder) = waiting.pop() {
        let items = match std::fs::read_dir(&folder) {
            Ok(items) => items,
            Err(_) => {
                failed += u32::from(!gone(&folder));
                continue;
            }
        };
        for item in items.flatten() {
            looked += 1;
            if looked % 64 == 0 && is_in_use(watch) {
                return None;
            }
            let Ok(kind) = item.file_type() else {
                failed += u32::from(!gone(&item.path()));
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = item.path();
            let Ok(mut dacl) = Dacl::read(&path) else {
                // (its list cannot be read, so it is not known to be clear of the ACE; what is under it is still looked at)
                failed += u32::from(!gone(&path));
                if kind.is_dir() {
                    waiting.push(path);
                }
                continue;
            };
            if dacl.protected() || (kind.is_dir() && dacl.has_exact(sid, READ_WRITE_EXECUTE, TO_ALL)) {
                continue;
            }
            let mut changed = false;
            for mask in masks {
                if let Some(flags) = dacl.from_above(sid, *mask, kind.is_dir()) {
                    dacl = dacl.without_exact(sid, *mask, flags);
                    changed = true;
                }
            }
            if changed && dacl.write_alone(&path).is_err() {
                failed += u32::from(!gone(&path));
            }
            if kind.is_dir() {
                waiting.push(path);
            }
        }
    }
    Some(failed)
}

/// Takes back all that the list says of this folder, if nothing uses it by the time this can begin. `done` if it did. A grant whose take-back left something behind stays on the list, and its
/// root is in `failed`: the next start tries again.
fn release(file: &Path, path: &Path) -> Released {
    let _one = WORK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut failed = Vec::new();
    // SAFETY: what each call below is given lives until it returns.
    unsafe {
        let Ok(sid) = sandbox::profile_sid() else { return Released { done: false, failed } };
        let mut journal = read(file);
        let place = sandbox::key(path);
        let roots: Vec<Grant> = journal.grants.iter().filter(|grant| {
            let root = sandbox::key(&grant.root);
            sandbox::under(&root, &place) || sandbox::under(&place, &root)
        }).cloned().collect();
        let mut all_done = true;
        for grant in roots {
            if is_in_use(&grant.root) {
                all_done = false;
                continue;
            }
            let whereabouts = match &grant.id {
                Some(id) => sandbox::locate(id, &grant.root),
                None if grant.root.exists() => sandbox::Located::At(grant.root.clone()),
                None => sandbox::Located::Gone,
            };
            match whereabouts {
                sandbox::Located::At(place) => {
                    // Where it has gone, its guarded paths with it; if something uses it there, it stays.
                    let moved = Grant { guarded: grant.guarded.iter().map(|item| Guarded { path: item.path.strip_prefix(&grant.root).map_or_else(|_| item.path.clone(), |inside| place.join(inside)), sddl: item.sddl.clone() }).collect(), root: place, id: grant.id.clone() };
                    if is_in_use(&moved.root) {
                        all_done = false;
                        continue;
                    }
                    match take_back(&moved, sid) {
                        Taken::Whole => {}
                        Taken::InUse => {
                            all_done = false;
                            continue;
                        }
                        // (some of it is still on the list's folder: it is not let go of as if done)
                        Taken::Partly => {
                            all_done = false;
                            failed.push(grant.root.clone());
                            continue;
                        }
                    }
                }
                sandbox::Located::Gone => {}
                // (it stays on the list: what it was given may still be on it)
                sandbox::Located::DriveAway => {
                    all_done = false;
                    continue;
                }
            }
            let root = sandbox::key(&grant.root);
            journal.grants.retain(|known| sandbox::key(&known.root) != root);
            let _ = write(file, &journal);
        }
        sandbox::forget_grants();
        Released { done: all_done, failed }
    }
}

unsafe fn take_back(grant: &Grant, sid: PSID) -> Taken {
    // What was made to keep its own list goes back to what it was, as long as it still does: if a person's git has written it anew, it inherits and is dealt with below.
    for item in &grant.guarded {
        // (one that could not be put back stays on the list, as does the rest of this grant, for the next start)
        if Dacl::read(&item.path).is_ok_and(|dacl| dacl.protected()) && sandbox::restore_dacl_text(&item.path, &item.sddl).is_err() {
            return Taken::Partly;
        }
    }
    // (a root whose list cannot be read is not known to be clear of the ACE)
    let Ok(dacl) = Dacl::read(&grant.root) else { return Taken::Partly };
    if dacl.has_exact(sid, READ_WRITE_EXECUTE, TO_ALL) && dacl.without_exact(sid, READ_WRITE_EXECUTE, TO_ALL).write_alone(&grant.root).is_err() {
        return Taken::Partly;
    }
    let Some(mut failed) = strip_tree(&grant.root, &grant.root, sid, &[READ_WRITE_EXECUTE]) else { return Taken::InUse };
    // Under a guarded folder what inherited was made read-only, and with what it inherited from put back that is left over.
    for item in &grant.guarded {
        if item.path.is_dir() {
            let Some(more) = strip_tree(&item.path, &grant.root, sid, &[READ_EXECUTE]) else { return Taken::InUse };
            failed += more;
        }
    }
    if failed == 0 {
        Taken::Whole
    } else {
        Taken::Partly
    }
}

/// When the app starts, off the window's thread: what earlier runs left. All that the list has, and what an earlier version left on a folder this app's sessions name, if its exact
/// write ACE is there to prove it. A folder in use is left. Says the folders whose take-back left something behind: they stay on the list, and the person is told ([`NOT_TAKEN_BACK`]).
pub fn sweep(folder: &Path, named_by_sessions: &[PathBuf]) -> Vec<PathBuf> {
    in_the_background();
    let file = folder.join(FILE_NAME);
    sweep_file(&file, named_by_sessions).1
}

/// How many were let go of, and which are left with something behind.
fn sweep_file(file: &Path, named_by_sessions: &[PathBuf]) -> (usize, Vec<PathBuf>) {
    // SAFETY: the SID lives as long as the process.
    let Ok(sid) = (unsafe { sandbox::profile_sid() }) else { return (0, Vec::new()) };
    {
        let _one = WORK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut journal = read(file);
        let before = journal.grants.len();
        for path in named_by_sessions {
            let place = sandbox::key(path);
            if journal.grants.iter().any(|grant| sandbox::key(&grant.root) == place) || !path.is_dir() {
                continue;
            }
            // SAFETY: as above.
            if unsafe { Dacl::read(path).is_ok_and(|dacl| dacl.has_exact(sid, READ_WRITE_EXECUTE, TO_ALL)) } {
                journal.grants.push(Grant { root: path.clone(), guarded: Vec::new(), id: sandbox::file_id(path) });
            }
        }
        if journal.grants.len() != before {
            let _ = write(file, &journal);
        }
    }
    let roots: Vec<PathBuf> = read(file).grants.into_iter().map(|grant| grant.root).collect();
    let (mut released, mut failed) = (0, Vec::new());
    for root in roots.iter().filter(|root| !is_in_use(root)) {
        let outcome = release(file, root);
        released += usize::from(outcome.done);
        failed.extend(outcome.failed);
    }
    failed.sort();
    failed.dedup();
    (released, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    /// In a temp folder, which goes with the test.
    struct Stand {
        _holder: tempfile::TempDir,
        base: PathBuf,
        file: PathBuf,
    }

    fn stand() -> Stand {
        let holder = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(holder.path()).map_or(holder.path().to_path_buf(), |full| PathBuf::from(full.to_string_lossy().trim_start_matches("\\\\?\\")));
        let file = base.join("journal").join(FILE_NAME);
        TEST_FILE.with(|known| *known.borrow_mut() = Some(file.clone()));
        Stand { _holder: holder, base, file }
    }

    fn folder(stand: &Stand, name: &str) -> PathBuf {
        let path = stand.base.join(name);
        std::fs::create_dir_all(path.join("sub")).unwrap();
        std::fs::write(path.join("a.txt"), "A").unwrap();
        std::fs::write(path.join("sub").join("b.txt"), "B").unwrap();
        path
    }

    fn command(place: &Path, line: &str) -> (Option<i32>, String) {
        let done = sandbox::run(line, place, place, false, Duration::from_secs(60), &AtomicBool::new(false)).unwrap();
        (done.status, format!("{}{}", sandbox::decode(&done.stdout), sandbox::decode(&done.stderr)))
    }

    fn lists(root: &Path) -> Vec<(PathBuf, String)> {
        let (mut all, mut waiting) = (Vec::new(), vec![root.to_path_buf()]);
        while let Some(path) = waiting.pop() {
            all.push((path.clone(), sandbox::dacl_text(&path).unwrap()));
            if path.is_dir() {
                waiting.extend(std::fs::read_dir(&path).unwrap().flatten().map(|item| item.path()));
            }
        }
        all.sort();
        all
    }

    fn holds_grant(path: &Path) -> bool {
        sandbox::dacl_text(path).unwrap().contains(&sandbox::profile_sid_text())
    }

    fn wait_for(what: &str, done: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(Instant::now() < until, "waited for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn moving_on_takes_back_the_write_grant_exactly_and_the_other_workspace_is_untouched() {
        let stand = stand();
        let (first, second) = (folder(&stand, "first"), folder(&stand, "second"));
        let before = lists(&first);
        let chat = Use::new(&first);
        assert!(chat.file().is_some());
        assert_eq!(command(&first, "echo made> made.txt & mkdir madefolder & echo x> madefolder\\c.txt").0, Some(0));
        assert!(read(&stand.file).grants.iter().any(|grant| grant.root == first), "written down");
        assert!(holds_grant(&first) && holds_grant(&first.join("a.txt")) && holds_grant(&first.join("made.txt")));
        // The session moves to the other folder.
        let moved = Use::new(&second);
        drop(chat);
        wait_for("the grant to go", || !holds_grant(&first));
        wait_for("what is under it", || !holds_grant(&first.join("sub").join("b.txt")) && !holds_grant(&first.join("made.txt")) && !holds_grant(&first.join("madefolder").join("c.txt")));
        wait_for("the list", || read(&stand.file).grants.iter().all(|grant| grant.root != first));
        // From the other workspace it can be neither written nor read, and that one works.
        assert_eq!(command(&second, "echo here> mine.txt").0, Some(0));
        command(&second, &format!("echo x> \"{}\\from-second.txt\"", first.display()));
        assert!(!first.join("from-second.txt").exists());
        assert!(!command(&second, &format!("type \"{}\\a.txt\"", first.display())).1.contains('A'));
        // Back as it was, to the byte, what the command made aside.
        std::fs::remove_file(first.join("made.txt")).unwrap();
        std::fs::remove_dir_all(first.join("madefolder")).unwrap();
        assert_eq!(lists(&first), before);
        assert!(holds_grant(&second));
        drop(moved);
        wait_for("the second to go too", || !holds_grant(&second));
    }

    #[test]
    fn two_chats_in_one_folder_keep_it_until_the_last_has_gone() {
        let stand = stand();
        let place = folder(&stand, "shared");
        let (one, two) = (Use::new(&place), Use::new(&place));
        assert_eq!(command(&place, "echo x> first.txt").0, Some(0));
        drop(one);
        std::thread::sleep(Duration::from_millis(200));
        assert!(holds_grant(&place), "another chat is on it");
        assert_eq!(command(&place, "echo x> second.txt").0, Some(0));
        drop(two);
        wait_for("the grant to go", || !holds_grant(&place));
    }

    #[test]
    fn a_grant_is_not_taken_back_during_a_command_only_after() {
        let stand = stand();
        let place = folder(&stand, "busy");
        let chat = Use::new(&place);
        assert_eq!(command(&place, "echo x> ready.txt").0, Some(0));
        let (running, file) = (place.clone(), stand.file.clone());
        let command_thread = std::thread::spawn(move || {
            TEST_FILE.with(|known| *known.borrow_mut() = Some(file));
            command(&running, "powershell -NoProfile -Command \"Start-Sleep -Seconds 3\" & echo after> after.txt").0
        });
        // Until it has begun.
        std::thread::sleep(Duration::from_millis(800));
        drop(chat);
        std::thread::sleep(Duration::from_millis(600));
        assert!(holds_grant(&place), "a command is running in it");
        assert_eq!(command_thread.join().unwrap(), Some(0));
        assert_eq!(std::fs::read_to_string(place.join("after.txt")).unwrap().trim(), "after", "it could still write at its end");
        wait_for("the grant to go after it", || !holds_grant(&place));
    }

    /// As `Documents` is: a list that says nothing of auto-inheritance, where what is handed down carries no mark.
    #[test]
    fn a_workspace_whose_lists_say_nothing_of_inheritance_loses_the_grant_too() {
        let stand = stand();
        let place = folder(&stand, "plain-shape");
        let text = sandbox::dacl_text(&place).unwrap();
        sandbox::restore_dacl_text(&place, &text.replacen("D:AI", "D:", 1)).unwrap();
        let chat = Use::new(&place);
        assert_eq!(command(&place, "echo x> made.txt & mkdir madefolder & echo y> madefolder\\c.txt").0, Some(0));
        assert!(holds_grant(&place) && holds_grant(&place.join("made.txt")));
        drop(chat);
        wait_for("the list", || read(&stand.file).grants.is_empty());
        let sid = sandbox::profile_sid_text();
        for (path, list) in lists(&place) {
            assert!(!list.contains(&sid), "{}: {list}", path.display());
        }
    }

    #[test]
    fn a_workspace_renamed_after_its_session_is_found_and_leaves_no_grant() {
        let stand = stand();
        let place = folder(&stand, "before-rename");
        let chat = Use::new(&place);
        assert_eq!(command(&place, "echo x> made.txt").0, Some(0));
        let renamed = stand.base.join("after-rename");
        std::fs::rename(&place, &renamed).unwrap();
        assert!(holds_grant(&renamed));
        // Another folder takes its name, and it has nothing to do with this.
        std::fs::create_dir_all(&place).unwrap();
        let other = lists(&place);
        drop(chat);
        wait_for("the list", || read(&stand.file).grants.is_empty());
        assert_eq!(lists(&place), other);
        let sid = sandbox::profile_sid_text();
        for (path, list) in lists(&renamed) {
            assert!(!list.contains(&sid), "{}: {list}", path.display());
        }
    }

    #[test]
    fn a_folder_inside_or_around_a_used_one_is_in_use() {
        let stand = stand();
        let wide = folder(&stand, "wide");
        let project = wide.join("sub");
        let chat = Use::new(&wide);
        assert!(is_in_use(&project) && is_in_use(&wide));
        drop(chat);
        let inner = Use::new(&project);
        assert!(is_in_use(&wide));
        drop(inner);
        assert!(!is_in_use(&wide));
    }

    #[test]
    fn a_guarded_repository_is_put_back_as_it_was() {
        let stand = stand();
        let place = folder(&stand, "repo");
        crate::tools::git(&place, &["init", "-q"]).unwrap();
        std::fs::write(place.join(".git").join("hooks").join("sample"), "x\n").unwrap();
        let before = lists(&place);
        let chat = Use::new(&place);
        assert_eq!(command(&place, "echo x> made.txt").0, Some(0));
        assert!(sandbox::dacl_text(&place.join(".git").join("config")).unwrap().starts_with("D:P"), "guarded");
        drop(chat);
        wait_for("the list", || read(&stand.file).grants.is_empty());
        std::fs::remove_file(place.join("made.txt")).unwrap();
        // (what git itself wrote meanwhile has no bearing: only the lists of what was there)
        let after = lists(&place);
        for (path, list) in &before {
            let now = after.iter().find(|(known, _)| known == path).map(|(_, list)| list.as_str());
            assert_eq!(now, Some(list.as_str()), "{}", path.display());
        }
        assert!(after.iter().all(|(_, list)| !list.contains(&sandbox::profile_sid_text())));
    }

    #[test]
    fn what_can_be_proved_is_cleaned_at_the_start_and_what_cannot_is_left() {
        let stand = stand();
        let sid = unsafe { sandbox::profile_sid().unwrap() };
        let (old, other_shape, not_named, in_use) = (folder(&stand, "old"), folder(&stand, "other-shape"), folder(&stand, "not-named"), folder(&stand, "in-use"));
        // As an earlier version left it: the exact ACE on the folder and inherited by what is in it.
        let grant = |path: &Path| unsafe {
            Dacl::read(path).unwrap().with_exact(sid, READ_WRITE_EXECUTE, TO_ALL).write_alone(path).unwrap();
            for (child, flags) in [(path.join("a.txt"), INHERITED_ACE as u8), (path.join("sub"), INHERITED_ACE as u8 | TO_ALL), (path.join("sub").join("b.txt"), INHERITED_ACE as u8)] {
                Dacl::read(&child).unwrap().with_inherited(sid, READ_WRITE_EXECUTE, flags).write_alone(&child).unwrap();
            }
        };
        for path in [&old, &not_named, &in_use] {
            grant(path);
        }
        unsafe { Dacl::read(&other_shape).unwrap().with_exact(sid, READ_EXECUTE, TO_ALL).write_alone(&other_shape).unwrap() };
        let plain = folder(&stand, "plain");
        let (other_before, plain_before, not_named_before) = (lists(&other_shape), lists(&plain), lists(&not_named));
        let held = Use::new(&in_use);
        let gone = stand.base.join("gone-now");
        let swept = sweep_file(&stand.file, &[old.clone(), other_shape.clone(), in_use.clone(), plain.clone(), gone]).0;
        assert_eq!(swept, 1);
        assert!(!holds_grant(&old) && !holds_grant(&old.join("a.txt")) && !holds_grant(&old.join("sub").join("b.txt")), "proved, and nothing uses it");
        assert_eq!(lists(&other_shape), other_before, "not the shape of a write grant: not provably one of these");
        assert_eq!(lists(&plain), plain_before);
        assert_eq!(lists(&not_named), not_named_before, "this app's sessions do not name it, and drives are not searched");
        assert!(holds_grant(&in_use) && holds_grant(&in_use.join("a.txt")), "something is using it");
        // It stays on the list, and goes when that has ended.
        assert!(read(&stand.file).grants.iter().any(|grant| grant.root == in_use));
        drop(held);
        wait_for("the one in use to go", || !holds_grant(&in_use));
        unsafe { Dacl::read(&other_shape).unwrap().without_exact(sid, READ_EXECUTE, TO_ALL).write_alone(&other_shape).unwrap() };
    }

    #[test]
    fn with_no_list_kept_nothing_is_recorded_or_taken_back() {
        let stand = stand();
        TEST_FILE.with(|known| *known.borrow_mut() = None);
        let place = folder(&stand, "unlisted");
        let chat = Use::new(&place);
        assert!(chat.file().is_none());
        drop(before_grant(&place).unwrap());
        drop(chat);
        assert!(!stand.file.exists());
    }

    /// What follows writes real ACEs only on what is in the stand's temp folder: the sandbox's preparing is not called, as that goes on to the folders above it.
    #[test]
    fn a_list_that_cannot_be_read_is_kept_and_what_cannot_be_written_down_is_not_given() {
        let stand = stand();
        let place = folder(&stand, "work");
        std::fs::create_dir_all(stand.file.parent().unwrap()).unwrap();
        std::fs::write(&stand.file, "{ not json").unwrap();
        drop(before_grant(&place).unwrap());
        let kept: Vec<String> = std::fs::read_dir(stand.file.parent().unwrap()).unwrap().flatten().filter(|item| item.file_name().to_string_lossy().contains("damaged")).map(|item| std::fs::read_to_string(item.path()).unwrap()).collect();
        assert_eq!(kept, ["{ not json"], "what it said is still there to look at");
        assert_eq!(read(&stand.file).grants.len(), 1);
        // Where nothing can be written: its folder is a file.
        let blocked = stand.base.join("blocked");
        std::fs::write(&blocked, "").unwrap();
        TEST_FILE.with(|known| *known.borrow_mut() = Some(blocked.join(FILE_NAME)));
        assert!(before_grant(&place).is_err());
    }

    #[test]
    fn a_repository_the_person_had_closed_themselves_is_guarded_and_loses_all_it_was_given() {
        let stand = stand();
        let place = folder(&stand, "repo");
        let git = place.join(".git");
        std::fs::create_dir_all(git.join("hooks")).unwrap();
        std::fs::write(git.join("config"), "x\n").unwrap();
        let sid = unsafe { sandbox::profile_sid().unwrap() };
        unsafe { Dacl::read(&git).unwrap().as_explicit_and_protected(sid).write_alone(&git).unwrap() };
        let before = sandbox::dacl_text(&git).unwrap();
        assert!(before.starts_with("D:P"), "{before}");
        drop(before_grant(&place).unwrap());
        unsafe { sandbox::guard_repository(&place, &git, sid).unwrap() };
        assert!(holds_grant(&git), "guarded");
        // Guarded again, as before each command: the list it had before stays what is written down.
        unsafe { sandbox::guard_repository(&place, &git, sid).unwrap() };
        assert!(release(&stand.file, &place).done);
        assert_eq!(sandbox::dacl_text(&git).unwrap(), before, "and nothing of the container's stays on it");
    }

    #[test]
    fn a_link_is_not_gone_through_to_guard_a_repository() {
        let stand = stand();
        let place = folder(&stand, "repo");
        let elsewhere = folder(&stand, "elsewhere");
        std::fs::create_dir_all(elsewhere.join("hooks")).unwrap();
        std::fs::write(elsewhere.join("config"), "mine\n").unwrap();
        std::fs::write(elsewhere.join("hooks").join("h"), "mine\n").unwrap();
        let before = lists(&elsewhere);
        let mut link = std::process::Command::new("cmd");
        link.arg("/c").arg("mklink").arg("/J").arg(place.join(".git")).arg(&elsewhere).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        crate::tools::no_window(&mut link);
        assert!(link.status().unwrap().success());
        drop(before_grant(&place).unwrap());
        let sid = unsafe { sandbox::profile_sid().unwrap() };
        let refused = unsafe { sandbox::guard_repository(&place, &place.join(".git"), sid) }.unwrap_err();
        assert!(refused.contains("is a link"), "{refused}");
        assert_eq!(lists(&elsewhere), before, "what it leads to has not been changed, or opened");
    }

    #[test]
    fn a_planted_commondir_does_not_send_git_elsewhere_for_its_config() {
        let stand = stand();
        let place = folder(&stand, "repo");
        let git = place.join(".git");
        std::fs::create_dir_all(git.join("hooks")).unwrap();
        std::fs::create_dir_all(git.join("worktrees").join("w")).unwrap();
        std::fs::write(git.join("config"), "x\n").unwrap();
        std::fs::write(git.join("commondir"), "..\\evil\n").unwrap();
        std::fs::write(git.join("worktrees").join("w").join("commondir"), "../..\n").unwrap();
        let sid = unsafe { sandbox::profile_sid().unwrap() };
        drop(before_grant(&place).unwrap());
        unsafe { sandbox::guard_repository(&place, &git, sid).unwrap() };
        assert!(!git.join("commondir").exists());
        assert!(git.join("worktrees").join("w").join("commondir").exists(), "a real worktree's");
        assert!(release(&stand.file, &place).done);
    }

    #[test]
    fn the_one_line_of_a_worktrees_git_cannot_be_rewritten_and_is_put_back() {
        let stand = stand();
        let place = folder(&stand, "worktree");
        let git = place.join(".git");
        std::fs::write(&git, "gitdir: C:\\elsewhere\\.git\\worktrees\\w\n").unwrap();
        let before = sandbox::dacl_text(&git).unwrap();
        drop(before_grant(&place).unwrap());
        let sid = unsafe { sandbox::profile_sid().unwrap() };
        unsafe { sandbox::guard_repository(&place, &git, sid).unwrap() };
        let guarded = sandbox::dacl_text(&git).unwrap();
        assert!(guarded.starts_with("D:P") && guarded.contains(&sandbox::profile_sid_text()), "{guarded}");
        assert!(release(&stand.file, &place).done);
        assert_eq!(sandbox::dacl_text(&git).unwrap(), before);
    }

    #[test]
    fn a_folder_on_the_way_keeps_its_list_to_itself_when_it_is_passed_through() {
        let stand = stand();
        let place = folder(&stand, "closed");
        let sid = unsafe { sandbox::profile_sid().unwrap() };
        unsafe { Dacl::read(&place).unwrap().as_explicit_and_protected(sid).write_alone(&place).unwrap() };
        assert!(sandbox::dacl_text(&place).unwrap().starts_with("D:P"));
        unsafe { sandbox::grant(&place, sid, sandbox::TRAVERSE, false).unwrap() };
        let after = sandbox::dacl_text(&place).unwrap();
        assert!(after.starts_with("D:P") && after.contains(&sandbox::profile_sid_text()), "the person closed it, and it stays so: {after}");
    }

    #[test]
    fn what_reads_as_a_secret_in_a_toolchains_folder_is_not_given_with_it() {
        let stand = stand();
        let home = stand.base.join("me");
        let cargo = home.join(".cargo");
        std::fs::create_dir_all(cargo.join("bin")).unwrap();
        std::fs::write(cargo.join("credentials.toml"), "token = \"secret\"").unwrap();
        std::fs::write(cargo.join("config.toml"), "").unwrap();
        let sid = unsafe { sandbox::profile_sid().unwrap() };
        unsafe { sandbox::open_toolchains("", &home, false, sid) };
        let id = sandbox::profile_sid_text();
        assert!(sandbox::dacl_text(&cargo).unwrap().contains(&id) && sandbox::dacl_text(&cargo.join("config.toml")).unwrap().contains(&id), "the toolchain");
        let held = sandbox::dacl_text(&cargo.join("credentials.toml")).unwrap();
        assert!(!held.contains(&id) && held.starts_with("D:P"), "{held}");
        // `cargo login` writes it anew, inheriting: it goes again.
        std::fs::remove_file(cargo.join("credentials.toml")).unwrap();
        std::fs::write(cargo.join("credentials.toml"), "token = \"secret\"").unwrap();
        assert!(sandbox::dacl_text(&cargo.join("credentials.toml")).unwrap().contains(&id));
        sandbox::forget_grants();
        unsafe { sandbox::open_toolchains("", &home, false, sid) };
        assert!(!sandbox::dacl_text(&cargo.join("credentials.toml")).unwrap().contains(&id));
    }

    /// An item under the grant that refuses to have its list changed leaves the grant on the list, and the take-back is done when it can be.
    #[test]
    fn a_take_back_that_could_not_change_everything_stays_on_the_list_and_is_tried_again() {
        let stand = stand();
        let first = folder(&stand, "first");
        let chat = Use::new(&first);
        assert_eq!(command(&first, "echo made> made.txt").0, Some(0));
        let a = first.join("a.txt");
        let text = sandbox::dacl_text(&a).unwrap();
        assert!(text.contains(&sandbox::profile_sid_text()), "it was granted: {text}");
        // The stand-in: nobody may write its list (Everyone is denied WRITE_DAC, and an OWNER RIGHTS entry takes from its owner the right the owner has without one), though it can be read. It
        // is in a temp folder, which can still be emptied: the folder lets its owner delete what is in it.
        let at = text.find('(').unwrap();
        sandbox::restore_dacl_text(&a, &format!("{}(D;;WD;;;WD)(A;;RC;;;OW){}", &text[..at], &text[at..])).unwrap();
        assert!(unsafe { Dacl::read(&a).unwrap().write_alone(&a) }.is_err(), "its list cannot be written now");
        drop(chat);
        let outcome = release(&stand.file, &first);
        assert!(!outcome.done && outcome.failed.len() == 1 && sandbox::key(&outcome.failed[0]) == sandbox::key(&first), "{:?}", outcome.failed);
        assert!(read(&stand.file).grants.iter().any(|grant| sandbox::key(&grant.root) == sandbox::key(&first)), "it stays on the list");
        assert!(holds_grant(&a), "and the ACE is still where it could not be taken off");
        // (the person is told through what the start of the app does with it)
        assert_eq!(sweep_file(&stand.file, &[]).1.len(), 1);
        // Once what refused is gone, the next try takes back the rest and lets the grant go.
        std::fs::remove_file(&a).unwrap();
        let outcome = release(&stand.file, &first);
        assert!(outcome.done && outcome.failed.is_empty(), "{:?}", outcome.failed);
        assert!(read(&stand.file).grants.iter().all(|grant| sandbox::key(&grant.root) != sandbox::key(&first)));
        assert!(!holds_grant(&first) && !holds_grant(&first.join("made.txt")) && !holds_grant(&first.join("sub").join("b.txt")));
    }
}
