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

fn folder_of(session: &Path) -> PathBuf {
    session.with_extension("checkpoints")
}

/// With a session that is deleted.
pub fn remove_for(session: &Path) -> Result<(), String> {
    let folder = folder_of(session);
    match std::fs::remove_dir_all(&folder) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {error}", folder.display())),
        _ => Ok(()),
    }
}

impl Checkpoints {
    /// A session's, as far as it got. A damaged index means nothing can be undone, not that the session cannot go on.
    pub fn open(session: &Path) -> Checkpoints {
        let folder = folder_of(session);
        let index = std::fs::read_to_string(folder.join(INDEX)).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
        Checkpoints { folder, index }
    }

    fn save(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.folder).map_err(|error| format!("{}: {error}", self.folder.display()))?;
        let text = serde_json::to_string(&self.index).map_err(|error| error.to_string())?;
        let partial = self.folder.join("index.partial");
        std::fs::write(&partial, text).map_err(|error| format!("{}: {error}", partial.display()))?;
        std::fs::rename(&partial, self.folder.join(INDEX)).map_err(|error| format!("{}: {error}", self.folder.display()))
    }

    pub fn begin(&mut self, title: &str, at: &str, message: Option<usize>) -> Result<(), String> {
        let id = self.index.turns.last().map_or(1, |turn| turn.id + 1);
        self.index.turns.push(Turn { id, title: title.to_string(), at: at.to_string(), message, files: Vec::new() });
        self.save()
    }

    fn copy_of(&mut self, path: &Path) -> Result<Saved, String> {
        if !path.exists() {
            return Ok(Saved { path: path.to_path_buf(), copy: None });
        }
        std::fs::create_dir_all(&self.folder).map_err(|error| format!("{}: {error}", self.folder.display()))?;
        let name = format!("{}.bin", self.index.next_copy);
        self.index.next_copy += 1;
        // A copy, not a read: the file may be larger than this machine's memory.
        std::fs::copy(path, self.folder.join(&name)).map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(Saved { path: path.to_path_buf(), copy: Some(name) })
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

    /// What the agent changed in this session, where git cannot say: each file against its first copy.
    pub fn session_changes(&self, workspace: &Path) -> Vec<FileChange> {
        let read = |path: &Path| std::fs::read(path).ok().map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
        self.plan(0).into_iter().filter_map(|saved| {
            let before = saved.copy.as_ref().and_then(|copy| read(&self.folder.join(copy)));
            let after = read(&saved.path);
            let name = saved.path.strip_prefix(workspace).unwrap_or(&saved.path).display().to_string();
            (before != after).then(|| FileChange::between(&name, before.as_deref(), after.as_deref()))
        }).collect()
    }

    /// Back to a turn's beginning. What is there now is saved first: a rewind can itself be rewound.
    pub fn restore(&mut self, id: u64, at: &str) -> Restored {
        let plan = self.plan(id);
        let mut restored = Restored::default();
        if plan.is_empty() {
            return restored;
        }
        let title = self.turn(id).map_or(String::new(), |turn| turn.title.clone());
        let mut keep = Vec::new();
        for saved in &plan {
            match self.copy_of(&saved.path) {
                Ok(current) => keep.push(current),
                Err(error) => {
                    // Without a copy of what is there, it is not overwritten.
                    restored.failed.push(error);
                }
            }
        }
        let kept: HashSet<PathBuf> = keep.iter().map(|saved| saved.path.clone()).collect();
        let next = self.index.turns.last().map_or(1, |turn| turn.id + 1);
        self.index.turns.push(Turn { id: next, title: format!("Before the rewind to: {title}"), at: at.to_string(), message: None, files: keep });
        if let Err(error) = self.save() {
            restored.failed.push(error);
            return restored;
        }
        for saved in plan.iter().filter(|saved| kept.contains(&saved.path)) {
            match &saved.copy {
                Some(copy) => {
                    if let Some(parent) = saved.path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match std::fs::copy(self.folder.join(copy), &saved.path) {
                        Ok(_) => restored.written.push(saved.path.clone()),
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
