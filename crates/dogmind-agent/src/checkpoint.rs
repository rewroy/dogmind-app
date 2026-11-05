//! What the agent's edits replaced, turn by turn, so that they can be undone. A file's history is one line: the first
//! copy saved after a moment is what the file held at that moment. What a command changed is not here, and neither is
//! what the person changed: a rewind says so, and first saves what it is about to overwrite, as a turn of its own.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::tools::FileChange;

const INDEX: &str = "index.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Saved {
    pub path: PathBuf,
    /// The copy's name in the folder; none when the file did not exist.
    pub copy: Option<String>,
    /// Where the path led when it was saved, through links and junctions. A rewind is the app's write, outside the sandbox: a folder a
    /// command has since made a junction to somewhere else would take what is put back there, so a path that leads elsewhere now is left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub real: Option<PathBuf>,
}

impl Saved {
    fn of(path: &Path, copy: Option<String>) -> Saved {
        Saved { path: path.to_path_buf(), copy, real: crate::safety::real_path(path) }
    }

    /// Whether the path leads somewhere else than when it was saved. One saved before that was kept has no "then" to compare with: a link or a junction on its way now is taken to lead
    /// elsewhere, as nothing says it did not.
    fn leads_elsewhere(&self) -> bool {
        let key = |path: &Path| path.to_string_lossy().replace('/', "\\").to_lowercase();
        match &self.real {
            Some(then) => crate::safety::real_path(&self.path).is_none_or(|now| key(&now) != key(then)),
            None => has_a_link_on_its_way(&self.path),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub id: u64,
    pub title: String,
    pub at: String,
    /// Where the conversation stood when the turn began; none once that place is gone (compacted, or rewound past).
    pub message: Option<usize>,
    pub files: Vec<Saved>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Restored {
    pub written: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    pub failed: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Index {
    next_copy: u64,
    turns: Vec<Turn>,
}

pub struct Checkpoints {
    folder: PathBuf,
    index: Index,
}

/// Whether the path, or any folder on the way to it, is a link or a junction (`is_symlink` is true of a junction on Windows too), looked at and not gone through.
fn has_a_link_on_its_way(path: &Path) -> bool {
    path.ancestors().any(|place| std::fs::symlink_metadata(place).is_ok_and(|what| what.file_type().is_symlink()))
}

fn folder_of(session: &Path) -> PathBuf {
    session.with_extension("checkpoints")
}

/// A kept copy in a file's place: copied beside it whole, then put in its place, as an edit is. Copied straight over it, a rewind that
/// failed on the way (a full disk, the app ended) left the file cut or empty.
fn put_back(copy: &Path, path: &Path) -> std::io::Result<()> {
    let beside = crate::tools::written_beside(path);
    let done = std::fs::copy(copy, &beside).and_then(|_| if path.exists() { crate::tools::take_the_place(&beside, path) } else { std::fs::rename(&beside, path) });
    if done.is_err() {
        let _ = std::fs::remove_file(&beside);
    }
    done
}

/// With a session that is deleted.
pub fn remove_for(session: &Path) -> Result<(), String> {
    // (its pictures too: nothing else reads them)
    let _ = std::fs::remove_dir_all(crate::images::folder_of(session));
    let _ = std::fs::remove_dir_all(crate::baseline::folder_of(session));
    crate::trajectory::remove_for(session);
    let folder = folder_of(session);
    match std::fs::remove_dir_all(&folder) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {error}", folder.display())),
        _ => Ok(()),
    }
}

/// One more than the largest `N` of the `N.bin` copies in a folder: where the numbering goes on when the index that held it is not there to say.
fn after_the_last_copy(folder: &Path) -> u64 {
    std::fs::read_dir(folder).into_iter().flatten().flatten()
        .filter_map(|entry| entry.file_name().to_str()?.strip_suffix(".bin")?.parse::<u64>().ok())
        .max().map_or(0, |last| last + 1)
}

/// Whether two files hold the same bytes: their sizes first, then block by block, so that two files that differ early are told apart by reading no more than that, and no file is read whole.
fn same_bytes(a: &Path, b: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    let (mut one, mut other) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    if one.metadata()?.len() != other.metadata()?.len() {
        return Ok(false);
    }
    let (mut first, mut second) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let count = one.read(&mut first)?;
        if count == 0 {
            return Ok(true);
        }
        other.read_exact(&mut second[..count])?;
        if first[..count] != second[..count] {
            return Ok(false);
        }
    }
}

/// `from` as a new file at `to`, streamed, and never over one that is there: a copy that was kept is the only one there is of what it held.
fn copy_new(from: &Path, to: &Path) -> std::io::Result<u64> {
    let mut source = std::fs::File::open(from)?;
    let mut copy = std::fs::OpenOptions::new().write(true).create_new(true).open(to)?;
    let written = std::io::copy(&mut source, &mut copy);
    if written.is_err() {
        drop(copy);
        let _ = std::fs::remove_file(to);
    }
    written
}

impl Checkpoints {
    /// A session's, as far as it got. A damaged index means nothing can be undone, not that the session cannot go on: it is put aside under another name, so that the copies it
    /// named can still be found, and none of them is written over by what comes next.
    pub fn open(session: &Path) -> Checkpoints {
        let folder = folder_of(session);
        let text = std::fs::read_to_string(folder.join(INDEX));
        let parsed = text.as_ref().ok().and_then(|text| serde_json::from_str::<Index>(text).ok());
        if parsed.is_none() && folder.join(INDEX).exists() {
            let aside = folder.join(format!("index.damaged-{}.json", chrono::Local::now().format("%Y%m%d-%H%M%S")));
            let _ = std::fs::rename(folder.join(INDEX), aside);
        }
        let mut index = parsed.unwrap_or_default();
        // (what the index said it had come to may be less than what is there: the numbering goes on past every copy in the folder)
        index.next_copy = index.next_copy.max(after_the_last_copy(&folder));
        Checkpoints { folder, index }
    }

    fn save(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.folder).map_err(|error| format!("{}: {error}", self.folder.display()))?;
        let text = serde_json::to_string(&self.index).map_err(|error| error.to_string())?;
        let partial = self.folder.join("index.partial");
        // (on the disk before it takes the place: after a power cut, a rename that got there without its bytes leaves no turns to go back to)
        std::fs::File::create(&partial).and_then(|mut out| {
            std::io::Write::write_all(&mut out, text.as_bytes())?;
            out.sync_all()
        }).map_err(|error| format!("{}: {error}", partial.display()))?;
        std::fs::rename(&partial, self.folder.join(INDEX)).map_err(|error| format!("{}: {error}", self.folder.display()))
    }

    pub fn begin(&mut self, title: &str, at: &str, message: Option<usize>) -> Result<(), String> {
        let id = self.index.turns.last().map_or(1, |turn| turn.id + 1);
        self.index.turns.push(Turn { id, title: title.to_string(), at: at.to_string(), message, files: Vec::new() });
        self.save()
    }

    fn copy_of(&mut self, path: &Path) -> Result<Saved, String> {
        if !path.exists() {
            return Ok(Saved::of(path, None));
        }
        std::fs::create_dir_all(&self.folder).map_err(|error| format!("{}: {error}", self.folder.display()))?;
        // A copy, not a read: the file may be larger than this machine's memory. Never over one that is there: the next name that is free.
        loop {
            let name = format!("{}.bin", self.index.next_copy);
            self.index.next_copy += 1;
            match copy_new(path, &self.folder.join(&name)) {
                Ok(_) => return Ok(Saved::of(path, Some(name))),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
        }
    }

    /// Before an edit: what the files hold now, unless this turn already has them. The first copy is the one that counts.
    pub fn before_edit(&mut self, paths: &[PathBuf]) -> Result<(), String> {
        for path in paths {
            let known = self.index.turns.last().is_some_and(|turn| turn.files.iter().any(|saved| saved.path == *path));
            if known || self.index.turns.is_empty() {
                continue;
            }
            let saved = self.copy_of(path)?;
            self.index.turns.last_mut().expect("there is a turn").files.push(saved);
            self.save()?;
        }
        Ok(())
    }

    pub fn turns(&self) -> &[Turn] {
        &self.index.turns
    }

    pub fn turn(&self, id: u64) -> Option<&Turn> {
        self.index.turns.iter().find(|turn| turn.id == id)
    }

    /// What going back to a turn's beginning would change: each file with what it held then.
    fn plan(&self, id: u64) -> Vec<Saved> {
        let mut seen = HashSet::new();
        let mut plan = Vec::new();
        for turn in self.index.turns.iter().filter(|turn| turn.id >= id) {
            for saved in &turn.files {
                if seen.insert(saved.path.clone()) {
                    plan.push(saved.clone());
                }
            }
        }
        plan
    }

    pub fn files_after(&self, id: u64) -> Vec<PathBuf> {
        self.plan(id).into_iter().map(|saved| saved.path).collect()
    }

    /// What the agent changed in this session, where git cannot say: each file against its first copy. Whether a file changed is told by its bytes, sizes first and then block by block, with
    /// no whole-file read; what is shown of the change is the text as the agent reads files (UTF-16, UTF-8, Shift_JIS). A file over what is read for a comparison (as `baseline` has it) or a binary
    /// one is said to have changed, with no diff.
    pub fn session_changes(&self, workspace: &Path) -> Vec<FileChange> {
        self.session_changes_within(workspace, crate::baseline::MOST_READ)
    }

    fn session_changes_within(&self, workspace: &Path, most_read: u64) -> Vec<FileChange> {
        self.plan(0).into_iter().filter_map(|saved| {
            let before = saved.copy.as_ref().map(|copy| self.folder.join(copy));
            let now = std::fs::metadata(&saved.path).ok().filter(std::fs::Metadata::is_file).map(|data| data.len());
            let changed = match (&before, now) {
                (None, None) => false,
                (None, Some(_)) | (Some(_), None) => true,
                // (a copy that cannot be read to compare is not one that proves it the same)
                (Some(before), Some(_)) => !same_bytes(before, &saved.path).unwrap_or(false),
            };
            if !changed {
                return None;
            }
            let name = saved.path.strip_prefix(workspace).unwrap_or(&saved.path).display().to_string();
            let before_size = before.as_ref().and_then(|copy| std::fs::metadata(copy).ok()).map(|data| data.len());
            if before_size.is_some_and(|size| size > most_read) || now.is_some_and(|size| size > most_read) {
                return Some(FileChange::without_diff(&name));
            }
            let read = |path: &Path| std::fs::read(path).ok();
            let (before_bytes, after_bytes) = (before.as_ref().and_then(|copy| read(copy)), now.and_then(|_| read(&saved.path)));
            // (a file that is there and cannot be read now is a change whose words are not known)
            if (before.is_some() && before_bytes.is_none()) || (now.is_some() && after_bytes.is_none()) {
                return Some(FileChange::without_diff(&name));
            }
            let text = |bytes: &Option<Vec<u8>>| -> Result<Option<String>, ()> {
                match bytes {
                    None => Ok(None),
                    Some(bytes) => crate::tools::text_of_file(bytes).map(|(text, _)| Some(text)).ok_or(()),
                }
            };
            Some(match (text(&before_bytes), text(&after_bytes)) {
                (Ok(before), Ok(after)) => FileChange::between(&name, before.as_deref(), after.as_deref()),
                _ => FileChange::without_diff(&name),
            })
        }).collect()
    }

    /// Back to a turn's beginning. What is there now is saved first: a rewind can itself be rewound.
    pub fn restore(&mut self, id: u64, at: &str) -> Restored {
        self.restore_with(id, at, &mut || {})
    }

    /// Whether a file is still as the rewind saw it when it kept a copy of what it held: absent if it was, else the same bytes as that copy.
    fn as_kept(&self, seen: &Saved) -> bool {
        match &seen.copy {
            None => std::fs::symlink_metadata(&seen.path).is_err(),
            Some(copy) => same_bytes(&self.folder.join(copy), &seen.path).unwrap_or(false),
        }
    }

    /// `between` is called when the copies are kept and before any file is written: a test's way to make the person, or a command, write in between.
    fn restore_with(&mut self, id: u64, at: &str, between: &mut dyn FnMut()) -> Restored {
        let plan = self.plan(id);
        let mut restored = Restored::default();
        if plan.is_empty() {
            return restored;
        }
        let title = self.turn(id).map_or(String::new(), |turn| turn.title.clone());
        let mut keep = Vec::new();
        for saved in &plan {
            if saved.leads_elsewhere() {
                restored.failed.push(Checkpoints::leads_elsewhere_words(saved));
                continue;
            }
            match self.copy_of(&saved.path) {
                Ok(current) => keep.push(current),
                Err(error) => {
                    // Without a copy of what is there, it is not overwritten.
                    restored.failed.push(error);
                }
            }
        }
        let kept: HashSet<PathBuf> = keep.iter().map(|saved| saved.path.clone()).collect();
        let keep_seen = keep.clone();
        let next = self.index.turns.last().map_or(1, |turn| turn.id + 1);
        self.index.turns.push(Turn { id: next, title: format!("Before the rewind to: {title}"), at: at.to_string(), message: None, files: keep });
        if let Err(error) = self.save() {
            restored.failed.push(error);
            return restored;
        }
        between();
        for saved in plan.iter().filter(|saved| kept.contains(&saved.path)) {
            // Nothing is written over what came after the copy was kept, and nothing is taken away that was not there to take: it is left, and said.
            let seen = keep_seen.iter().find(|seen| seen.path == saved.path).expect("every file that is written was kept");
            if saved.leads_elsewhere() {
                restored.failed.push(Checkpoints::leads_elsewhere_words(saved));
                continue;
            }
            if !self.as_kept(seen) {
                restored.failed.push(format!("{}: it changed while the rewind was under way, so it was left as it is", saved.path.display()));
                continue;
            }
            match &saved.copy {
                Some(copy) => {
                    if let Some(parent) = saved.path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match put_back(&self.folder.join(copy), &saved.path) {
                        Ok(()) => restored.written.push(saved.path.clone()),
                        Err(error) => restored.failed.push(format!("{}: {error}", saved.path.display())),
                    }
                }
                None => match std::fs::remove_file(&saved.path) {
                    Ok(()) => restored.removed.push(saved.path.clone()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => restored.failed.push(format!("{}: {error}", saved.path.display())),
                },
            }
        }
        restored
    }

    fn leads_elsewhere_words(saved: &Saved) -> String {
        if saved.real.is_some() {
            format!("{}: a link on its way leads somewhere else than when it was saved, so nothing was put there", saved.path.display())
        } else {
            format!("{}: a link or junction is on its way, and this record, from before they were looked at, does not say where the path led when it was saved, so nothing was put there", saved.path.display())
        }
    }

    /// The conversation before `from` is all that remains: later turns can go back in their files only.
    pub fn conversation_cut(&mut self, from: usize) {
        for turn in &mut self.index.turns {
            if turn.message.is_some_and(|message| message >= from) {
                turn.message = None;
            }
        }
        let _ = self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        file: PathBuf,
        other: PathBuf,
        marks: Checkpoints,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        let other = dir.path().join("b.txt");
        let marks = Checkpoints::open(&dir.path().join("session.jsonl"));
        Fixture { _dir: dir, file, other, marks }
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    /// A folder made a junction to somewhere else after its files were saved takes nothing a rewind would have put back, and loses
    /// nothing it would have taken away; the rest of the rewind goes on.
    #[cfg(windows)]
    #[test]
    fn a_rewind_puts_nothing_where_a_junction_now_leads() {
        let dir = tempfile::tempdir().unwrap();
        let (work, elsewhere) = (dir.path().join("work"), dir.path().join("elsewhere"));
        std::fs::create_dir_all(work.join("sub")).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let (planted, made, plain) = (work.join("sub").join("run.bat"), work.join("sub").join("new.txt"), work.join("a.txt"));
        std::fs::write(&planted, "payload").unwrap();
        std::fs::write(&plain, "before").unwrap();
        let mut marks = Checkpoints::open(&dir.path().join("session.jsonl"));
        marks.begin("one", "t", Some(1)).unwrap();
        marks.before_edit(&[planted.clone(), made.clone(), plain.clone()]).unwrap();
        std::fs::write(&plain, "after").unwrap();
        // (what a command in the sandbox can do: the folder goes, and a junction to elsewhere takes its name)
        std::fs::remove_dir_all(work.join("sub")).unwrap();
        let status = std::process::Command::new("cmd").arg("/c").arg("mklink").arg("/J").arg(work.join("sub")).arg(&elsewhere).stdout(std::process::Stdio::null()).status().unwrap();
        assert!(status.success());
        std::fs::write(elsewhere.join("new.txt"), "theirs").unwrap();
        let restored = marks.restore(1, "t");
        assert!(!elsewhere.join("run.bat").exists(), "nothing written through the junction");
        assert_eq!(read(&elsewhere.join("new.txt")), "theirs", "nothing removed through it");
        assert_eq!(read(&plain), "before", "the rest is rewound");
        assert_eq!(restored.failed.len(), 2, "{:?}", restored.failed);
        std::fs::remove_dir(work.join("sub")).unwrap();
    }

    /// The index is gone bad (a cut, a bad disk): it is kept aside and not written over, and none of the copies it named is overwritten by the next one.
    #[test]
    fn a_damaged_index_keeps_the_copies_it_named_and_is_put_aside() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session.jsonl");
        let (a, b) = (dir.path().join("a.txt"), dir.path().join("b.txt"));
        std::fs::write(&a, "first").unwrap();
        std::fs::write(&b, "second").unwrap();
        let mut marks = Checkpoints::open(&session);
        marks.begin("one", "t", Some(1)).unwrap();
        marks.before_edit(&[a.clone(), b.clone()]).unwrap();
        let folder = folder_of(&session);
        assert_eq!((read(&folder.join("0.bin")), read(&folder.join("1.bin"))), ("first".to_string(), "second".to_string()));
        // (what a cut leaves)
        std::fs::write(folder.join(INDEX), "{\"next_copy\": 2, \"tur").unwrap();
        let mut again = Checkpoints::open(&session);
        assert!(again.turns().is_empty(), "nothing to go back to, and the session goes on");
        let aside: Vec<String> = std::fs::read_dir(&folder).unwrap().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).filter(|name| name.starts_with("index.damaged-")).collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert!(!folder.join(INDEX).exists() || serde_json::from_str::<serde_json::Value>(&read(&folder.join(INDEX))).is_ok());
        let c = dir.path().join("c.txt");
        std::fs::write(&c, "third").unwrap();
        again.begin("two", "t", Some(2)).unwrap();
        again.before_edit(&[c.clone()]).unwrap();
        assert_eq!((read(&folder.join("0.bin")), read(&folder.join("1.bin"))), ("first".to_string(), "second".to_string()), "the copies of the history before it are still there");
        assert_eq!(read(&folder.join("2.bin")), "third", "and the new one has a name of its own");
        assert_eq!(read(&folder.join(&aside[0])), "{\"next_copy\": 2, \"tur", "the damaged index is kept as it was");
    }

    /// A copy never goes over a file that is there, whatever the numbering says.
    #[test]
    fn a_copy_takes_the_next_name_that_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session.jsonl");
        let folder = folder_of(&session);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("0.bin"), "old zero").unwrap();
        std::fs::write(folder.join("1.bin"), "old one").unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "now").unwrap();
        // (an index that says nothing of them)
        let mut marks = Checkpoints { folder: folder.clone(), index: Index::default() };
        marks.begin("one", "t", Some(1)).unwrap();
        marks.before_edit(&[file]).unwrap();
        assert_eq!((read(&folder.join("0.bin")), read(&folder.join("1.bin")), read(&folder.join("2.bin"))), ("old zero".to_string(), "old one".to_string(), "now".to_string()));
    }

    /// Two Shift_JIS files that differ, and that the lossy text of both is the same for (every byte of both is a bad UTF-8 byte), are told apart, and shown as they read.
    #[test]
    fn two_shift_jis_versions_that_differ_are_told_apart() {
        let mut f = fixture();
        // ("日語" and "旅上": four bytes each, none of them a character of UTF-8)
        std::fs::write(&f.file, [0x93, 0xFA, 0x8C, 0xEA]).unwrap();
        f.marks.begin("one", "t", Some(1)).unwrap();
        f.marks.before_edit(&[f.file.clone()]).unwrap();
        std::fs::write(&f.file, [0x97, 0xB7, 0x8F, 0xE3]).unwrap();
        let changes = f.marks.session_changes(f.file.parent().unwrap());
        assert_eq!(changes.len(), 1, "a changed file is not called unchanged");
        assert!(changes[0].diff.contains("-日語") && changes[0].diff.contains("+旅上"), "{}", changes[0].diff);
        // (the same bytes written again are no change)
        std::fs::write(&f.file, [0x93, 0xFA, 0x8C, 0xEA]).unwrap();
        assert!(f.marks.session_changes(f.file.parent().unwrap()).is_empty());
    }

    /// A file larger than what is read for a comparison is reported changed with no diff, found changed by its bytes and not by reading it; a binary one too.
    #[test]
    fn a_large_file_and_a_binary_one_are_reported_changed_with_no_diff() {
        let mut f = fixture();
        let big = f.file.clone();
        std::fs::write(&big, "line\n".repeat(20_000)).unwrap();
        std::fs::write(&f.other, [b'M', b'Z', 0, 0, 1, 2, 3]).unwrap();
        f.marks.begin("one", "t", Some(1)).unwrap();
        f.marks.before_edit(&[big.clone(), f.other.clone()]).unwrap();
        std::fs::write(&big, format!("{}last\n", "line\n".repeat(19_999))).unwrap();
        std::fs::write(&f.other, [b'M', b'Z', 0, 0, 1, 2, 4]).unwrap();
        let root = big.parent().unwrap();
        let small_cap = f.marks.session_changes_within(root, 1024);
        assert_eq!(small_cap.len(), 2, "both are changed");
        assert!(small_cap.iter().all(|change| change.diff.is_empty() && change.added == 0), "{small_cap:?}");
        let whole = f.marks.session_changes(root);
        let (text, binary) = (whole.iter().find(|change| change.path == "a.txt").unwrap(), whole.iter().find(|change| change.path == "b.txt").unwrap());
        assert!(text.diff.contains("+last") && text.added == 1, "within the cap it is shown");
        assert!(binary.diff.is_empty(), "a binary is not");
    }

    /// Two files are told apart by their size, or by the first block that differs, and the same bytes are the same.
    #[test]
    fn two_files_are_compared_by_size_and_then_by_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        let body = vec![7u8; 200_000];
        std::fs::write(&a, &body).unwrap();
        std::fs::write(&b, &body).unwrap();
        assert!(same_bytes(&a, &b).unwrap());
        let mut later = body.clone();
        later[150_000] = 8;
        std::fs::write(&b, &later).unwrap();
        assert!(!same_bytes(&a, &b).unwrap(), "a difference in a later block");
        std::fs::write(&b, &body[..199_999]).unwrap();
        assert!(!same_bytes(&a, &b).unwrap(), "a different size");
        assert!(same_bytes(&a, &dir.path().join("none")).is_err());
    }

    /// What is written between the rewind's two looks is not written over, nor taken away: the file is left as the person made it, and the rewind says so. The rest goes on.
    #[test]
    fn a_file_changed_between_the_rewinds_passes_is_left_and_said() {
        let mut f = fixture();
        let created = f.file.with_file_name("created.txt");
        std::fs::write(&f.file, "before").unwrap();
        std::fs::write(&f.other, "other before").unwrap();
        f.marks.begin("one", "t", Some(1)).unwrap();
        // (a.txt is edited, b.txt too, and created.txt did not exist when the turn began and is not made by it)
        f.marks.before_edit(&[f.file.clone(), f.other.clone(), created.clone()]).unwrap();
        std::fs::write(&f.file, "the agent's").unwrap();
        std::fs::write(&f.other, "the agent's too").unwrap();
        let (file, created_now) = (f.file.clone(), created.clone());
        let restored = f.marks.restore_with(1, "t", &mut || {
            std::fs::write(&file, "the person's own, a moment ago").unwrap();
            std::fs::write(&created_now, "theirs").unwrap();
        });
        assert_eq!(read(&f.file), "the person's own, a moment ago", "not written over");
        assert_eq!(read(&created), "theirs", "not removed, as it was not there when the copies were kept");
        assert_eq!(read(&f.other), "other before", "the rest of the rewind goes on");
        assert_eq!(restored.failed.len(), 2, "{:?}", restored.failed);
        assert!(restored.failed.iter().all(|why| why.contains("changed while the rewind was under way")), "{:?}", restored.failed);
        assert_eq!(restored.written, [f.other.clone()]);
        // (and what the rewind left is what a later rewind can bring back: the copies were kept before the person's own were written)
        assert!(restored.removed.is_empty());
    }

    /// A record from before the link guard has no place to compare with: a junction on its way now is not written through.
    #[cfg(windows)]
    #[test]
    fn an_older_record_is_not_written_through_a_junction_that_is_there_now() {
        let dir = tempfile::tempdir().unwrap();
        let (work, elsewhere) = (dir.path().join("work"), dir.path().join("elsewhere"));
        std::fs::create_dir_all(work.join("sub")).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let (planted, plain) = (work.join("sub").join("run.bat"), work.join("a.txt"));
        std::fs::write(&planted, "payload").unwrap();
        std::fs::write(&plain, "before").unwrap();
        let mut marks = Checkpoints::open(&dir.path().join("session.jsonl"));
        marks.begin("one", "t", Some(1)).unwrap();
        marks.before_edit(&[planted.clone(), plain.clone()]).unwrap();
        std::fs::write(&plain, "after").unwrap();
        // (a record as it was written before `real` was kept)
        for turn in &mut marks.index.turns {
            for saved in &mut turn.files {
                saved.real = None;
            }
        }
        std::fs::remove_dir_all(work.join("sub")).unwrap();
        let status = std::process::Command::new("cmd").arg("/c").arg("mklink").arg("/J").arg(work.join("sub")).arg(&elsewhere).stdout(std::process::Stdio::null()).status().unwrap();
        assert!(status.success());
        let restored = marks.restore(1, "t");
        assert!(!elsewhere.join("run.bat").exists(), "nothing written through the junction");
        assert_eq!(read(&plain), "before", "the rest is rewound");
        assert_eq!(restored.failed.len(), 1, "{:?}", restored.failed);
        assert!(restored.failed[0].contains("this record"), "{:?}", restored.failed);
        std::fs::remove_dir(work.join("sub")).unwrap();
    }

    /// A path with no link on its way is no concern of the guard's, whether its record says where it led or not.
    #[test]
    fn a_plain_path_has_no_link_on_its_way() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a").join("b")).unwrap();
        assert!(!has_a_link_on_its_way(&dir.path().join("a").join("b").join("c.txt")));
        let record = Saved { path: dir.path().join("a").join("b").join("c.txt"), copy: None, real: None };
        assert!(!record.leads_elsewhere());
    }

    /// A file that cannot take what a rewind puts back is as it was, with nothing left beside it, and the rewind says so.
    #[test]
    fn a_file_a_rewind_cannot_write_is_as_it_was() {
        let mut f = fixture();
        std::fs::write(&f.file, "before").unwrap();
        f.marks.begin("one", "t", Some(1)).unwrap();
        f.marks.before_edit(&[f.file.clone()]).unwrap();
        std::fs::write(&f.file, "after").unwrap();
        let mut locked = std::fs::metadata(&f.file).unwrap().permissions();
        locked.set_readonly(true);
        std::fs::set_permissions(&f.file, locked.clone()).unwrap();
        let restored = f.marks.restore(1, "t");
        locked.set_readonly(false);
        std::fs::set_permissions(&f.file, locked).unwrap();
        assert_eq!((read(&f.file), restored.failed.len()), ("after".to_string(), 1), "{:?}", restored.failed);
        let left: Vec<String> = std::fs::read_dir(f.file.parent().unwrap()).unwrap().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).filter(|name| name.contains("dogmind-edit")).collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn a_rewind_brings_back_the_beginning_of_a_turn() {
        let mut f = fixture();
        std::fs::write(&f.file, "zero").unwrap();
        f.marks.begin("first", "t", Some(1)).unwrap();
        f.marks.before_edit(&[f.file.clone()]).unwrap();
        std::fs::write(&f.file, "one").unwrap();
        f.marks.before_edit(&[f.file.clone()]).unwrap();
        std::fs::write(&f.file, "one and more").unwrap();
        f.marks.begin("second", "t", Some(3)).unwrap();
        f.marks.before_edit(&[f.file.clone()]).unwrap();
        std::fs::write(&f.file, "two").unwrap();
        let restored = f.marks.restore(2, "t");
        assert_eq!(read(&f.file), "one and more");
        assert_eq!(restored.written, [f.file.clone()]);
        f.marks.restore(1, "t");
        assert_eq!(read(&f.file), "zero");
    }

    #[test]
    fn a_file_the_turn_created_goes_away() {
        let mut f = fixture();
        f.marks.begin("first", "t", Some(1)).unwrap();
        f.marks.before_edit(&[f.file.clone()]).unwrap();
        std::fs::write(&f.file, "new").unwrap();
        let restored = f.marks.restore(1, "t");
        assert!(!f.file.exists());
        assert_eq!(restored.removed, [f.file.clone()]);
    }

    #[test]
    fn a_rewind_can_itself_be_rewound() {
        let mut f = fixture();
        std::fs::write(&f.file, "before").unwrap();
        f.marks.begin("first", "t", Some(1)).unwrap();
        f.marks.before_edit(&[f.file.clone()]).unwrap();
        std::fs::write(&f.file, "after").unwrap();
        f.marks.restore(1, "t");
        assert_eq!(read(&f.file), "before");
        let last = f.marks.turns().last().unwrap().clone();
        assert!(last.title.starts_with("Before the rewind"));
        assert_eq!(last.message, None);
        f.marks.restore(last.id, "t");
        assert_eq!(read(&f.file), "after");
    }

    #[test]
    fn the_state_at_any_turn_survives_a_rewind_in_between() {
        let mut f = fixture();
        std::fs::write(&f.file, "a0").unwrap();
        std::fs::write(&f.other, "b0").unwrap();
        f.marks.begin("one", "t", Some(1)).unwrap();
        f.marks.before_edit(&[f.file.clone()]).unwrap();
        std::fs::write(&f.file, "a1").unwrap();
        f.marks.begin("two", "t", Some(2)).unwrap();
        f.marks.before_edit(&[f.other.clone()]).unwrap();
        std::fs::write(&f.other, "b1").unwrap();
        f.marks.restore(2, "t");
        assert_eq!((read(&f.file), read(&f.other)), ("a1".to_string(), "b0".to_string()));
        f.marks.begin("three", "t", Some(2)).unwrap();
        f.marks.before_edit(&[f.file.clone()]).unwrap();
        std::fs::write(&f.file, "a3").unwrap();
        f.marks.restore(1, "t");
        assert_eq!((read(&f.file), read(&f.other)), ("a0".to_string(), "b0".to_string()));
    }

    #[test]
    fn they_come_back_with_the_session_and_go_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session.jsonl");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let mut marks = Checkpoints::open(&session);
        marks.begin("first", "t", Some(1)).unwrap();
        marks.before_edit(&[file.clone()]).unwrap();
        std::fs::write(&file, "y").unwrap();
        let mut again = Checkpoints::open(&session);
        assert_eq!(again.turns().len(), 1);
        assert_eq!(again.files_after(1), [file.clone()]);
        again.restore(1, "t");
        assert_eq!(read(&file), "x");
        remove_for(&session).unwrap();
        assert!(Checkpoints::open(&session).turns().is_empty());
        remove_for(&session).unwrap();
    }

    #[test]
    fn the_session_changes_are_against_the_first_copy() {
        let mut f = fixture();
        std::fs::write(&f.file, "one\n").unwrap();
        std::fs::write(&f.other, "same\n").unwrap();
        f.marks.begin("first", "t", Some(1)).unwrap();
        f.marks.before_edit(&[f.file.clone(), f.other.clone()]).unwrap();
        std::fs::write(&f.file, "two\n").unwrap();
        f.marks.begin("second", "t", Some(2)).unwrap();
        f.marks.before_edit(&[f.file.clone()]).unwrap();
        std::fs::write(&f.file, "three\n").unwrap();
        let root = f.file.parent().unwrap().to_path_buf();
        let changes = f.marks.session_changes(&root);
        assert_eq!(changes.len(), 1);
        assert_eq!((changes[0].path.as_str(), changes[0].added, changes[0].removed), ("a.txt", 1, 1));
        assert!(changes[0].diff.contains("-one") && changes[0].diff.contains("+three"));
    }

    #[test]
    fn a_cut_conversation_leaves_only_the_files() {
        let mut f = fixture();
        f.marks.begin("one", "t", Some(1)).unwrap();
        f.marks.begin("two", "t", Some(5)).unwrap();
        f.marks.conversation_cut(3);
        assert_eq!(f.marks.turn(1).unwrap().message, Some(1));
        assert_eq!(f.marks.turn(2).unwrap().message, None);
        f.marks.conversation_cut(0);
        assert_eq!(f.marks.turn(1).unwrap().message, None);
    }

    #[test]
    fn a_turn_that_edits_nothing_has_nothing_to_restore() {
        let mut f = fixture();
        f.marks.begin("one", "t", Some(1)).unwrap();
        assert_eq!(f.marks.restore(1, "t"), Restored::default());
        assert_eq!(f.marks.turns().len(), 1);
    }
}
