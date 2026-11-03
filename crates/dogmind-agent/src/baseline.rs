//! Where a session began, for `show_diff`, kept beside the session so that a commit or a reopening does not move it.
//!
//! In a repository: HEAD, and copies of what differed from it then (what was modified, and what was new). Git has the rest, so the cost is that of asking it, and what was
//! already changed before the session is not taken for this session's work. Outside one, or before the first commit: a list of the folder's files, with a copy of each small
//! text file. What is too large or too much is listed or left out, and the answer says so. Neither copies a whole folder, and what `.gitignore` skips is skipped.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::tools::{cap, file_text, git, git_raw, untracked_names, walker, FileChange};

/// What a first message may wait for, in a folder with no repository.
const LOOKING: Duration = Duration::from_secs(3);
const MOST_FILES: usize = 20_000;
const MOST_COPIED: u64 = 256 * 1024;
const MOST_KEPT: u64 = 32 << 20;
/// What is read to compare it with what was there.
const MOST_READ: u64 = 64 << 20;
const MOST_SHOWN_OF_A_FILE: usize = 6000;
/// Files shown in full in an answer: past these, a line each.
const MOST_SHOWN: usize = 100;
const MANIFEST: &str = "manifest.json";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Entry {
    size: u64,
    modified: Option<u128>,
    /// The copy's name in the folder; none when it was too large, not text, or may hold secrets.
    copy: Option<String>,
    /// A tracked file that had been deleted when the session began.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    gone: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Baseline {
    /// HEAD when the session began; none outside a repository, or in one with no commit yet.
    pub head: Option<String>,
    /// In a repository, what differed from `head` then; elsewhere, the folder.
    files: BTreeMap<String, Entry>,
    /// Whether every file was looked at.
    whole: bool,
    #[serde(skip)]
    session: PathBuf,
}

pub fn folder_of(session: &Path) -> PathBuf {
    session.with_extension("baseline")
}

fn may_hold_secrets(name: &str) -> bool {
    crate::safety::sensitive(Path::new(&format!("/{name}")))
}

fn binary(bytes: &[u8]) -> bool {
    !(bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF])) && bytes.iter().take(8192).any(|&byte| byte == 0)
}

fn modified(data: &std::fs::Metadata) -> Option<u128> {
    data.modified().ok()?.duration_since(UNIX_EPOCH).ok().map(|since| since.as_nanos())
}

/// The names of the files that differ from a commit, by what asks nothing of the repository: this takes no lock, as the person's own git may be at work.
fn differing(workspace: &Path, head: &str) -> Vec<String> {
    git(workspace, &["--no-optional-locks", "-c", "core.quotepath=off", "diff", "--no-renames", "--name-only", "--relative", head])
        .map_or(Vec::new(), |names| names.lines().map(str::to_string).collect())
}

/// One file as it is now, with a copy of it if it is small text and within what may be kept.
fn entry_of(folder: &Path, name: &str, path: &Path, data: &std::fs::Metadata, kept: &mut u64) -> Entry {
    let mut entry = Entry { size: data.len(), modified: modified(data), ..Entry::default() };
    if entry.size <= MOST_COPIED && *kept + entry.size <= MOST_KEPT && !may_hold_secrets(name) {
        if let Ok(bytes) = std::fs::read(path) {
            if !binary(&bytes) && std::fs::create_dir_all(folder).is_ok() {
                let copy: String = Sha256::digest(&bytes).iter().take(16).map(|byte| format!("{byte:02x}")).collect();
                if folder.join(&copy).exists() || std::fs::write(folder.join(&copy), &bytes).is_ok() {
                    *kept += entry.size;
                    entry.copy = Some(copy);
                }
            }
        }
    }
    entry
}

/// What was there before, as far as it is known.
enum Before {
    Absent,
    Bytes(Vec<u8>),
    /// It was there, of this size, and no copy was kept.
    Unknown(u64),
}

impl Baseline {
    /// A session's, as it was first taken; taken now if it has none.
    pub fn for_session(session: &Path, workspace: &Path) -> Baseline {
        let folder = folder_of(session);
        let kept = std::fs::read_to_string(folder.join(MANIFEST)).ok().and_then(|text| serde_json::from_str::<Baseline>(&text).ok());
        let mut baseline = kept.unwrap_or_else(|| {
            let taken = Baseline::take(&folder, workspace);
            if std::fs::create_dir_all(&folder).is_ok() {
                if let Ok(text) = serde_json::to_string(&taken) {
                    let _ = std::fs::write(folder.join(MANIFEST), text);
                }
            }
            taken
        });
        baseline.session = session.to_path_buf();
        baseline
    }

    fn take(folder: &Path, workspace: &Path) -> Baseline {
        let head = git(workspace, &["rev-parse", "--verify", "-q", "HEAD"]).ok().map(|head| head.trim().to_string()).filter(|head| !head.is_empty());
        let started = Instant::now();
        let (mut files, mut kept, mut whole) = (BTreeMap::new(), 0u64, true);
        match &head {
            Some(head) => {
                let names: BTreeSet<String> = differing(workspace, head).into_iter().chain(untracked_names(workspace, &[])).collect();
                for name in names {
                    if files.len() >= MOST_FILES || started.elapsed() > LOOKING {
                        whole = false;
                        break;
                    }
                    let path = workspace.join(&name);
                    let entry = match std::fs::metadata(&path) {
                        Ok(data) => entry_of(folder, &name, &path, &data, &mut kept),
                        Err(_) => Entry { gone: true, ..Entry::default() },
                    };
                    files.insert(name, entry);
                }
            }
            None => {
                for entry in walker(workspace, true).build().flatten() {
                    if files.len() >= MOST_FILES || started.elapsed() > LOOKING {
                        whole = false;
                        break;
                    }
                    if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                        continue;
                    }
                    let Ok(data) = entry.metadata() else { continue };
                    let name = entry.path().strip_prefix(workspace).unwrap_or(entry.path()).to_string_lossy().replace('\\', "/");
                    let kept_entry = entry_of(folder, &name, entry.path(), &data, &mut kept);
                    files.insert(name, kept_entry);
                }
            }
        }
        Baseline { head, files, whole, session: PathBuf::new() }
    }

    /// The files that may differ from the beginning, and whether that is all of them.
    fn candidates(&self, workspace: &Path) -> (BTreeSet<String>, bool) {
        let mut names: BTreeSet<String> = self.files.keys().cloned().collect();
        if let Some(head) = &self.head {
            names.extend(differing(workspace, head));
            names.extend(untracked_names(workspace, &[]));
            return (names, self.whole);
        }
        let started = Instant::now();
        let mut seen = 0;
        for entry in walker(workspace, true).build().flatten() {
            if seen >= MOST_FILES || started.elapsed() > LOOKING {
                return (names, false);
            }
            if entry.file_type().is_some_and(|kind| kind.is_file()) {
                seen += 1;
                names.insert(entry.path().strip_prefix(workspace).unwrap_or(entry.path()).to_string_lossy().replace('\\', "/"));
            }
        }
        (names, self.whole)
    }

    /// What has changed under the workspace since the session began: a line for each file, then what can be shown of them. Empty when nothing has.
    pub fn changes(&self, workspace: &Path, paths: &[&str]) -> String {
        let wanted = |name: &str| paths.is_empty() || paths.iter().any(|path| {
            let path = path.replace('\\', "/");
            let path = path.trim_start_matches("./").trim_end_matches('/');
            path == "." || name == path || name.starts_with(&format!("{path}/"))
        });
        let (names, whole) = self.candidates(workspace);
        let (mut lines, mut shown): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
        // (of what the agent itself edited, the session has a first copy whatever its size)
        let edited = std::cell::OnceCell::new();
        let edited_diff = |name: &str| -> Option<String> {
            let changes = edited.get_or_init(|| crate::checkpoint::Checkpoints::open(&self.session).session_changes(workspace));
            changes.iter().find(|change| change.path.replace('\\', "/") == name).map(|change| change.diff.clone())
        };
        for name in names.iter().filter(|name| wanted(name)) {
            let path = workspace.join(name);
            let now = std::fs::metadata(&path).ok();
            let entry = self.files.get(name);
            match (entry, &now) {
                (Some(entry), None) if entry.gone => continue,
                (Some(entry), Some(data)) if !entry.gone && entry.size == data.len() && entry.modified == modified(data) => continue,
                (None, None) if self.head.is_none() => continue,
                _ => {}
            }
            // (nothing is shown of it, and to read it is to hold it: one larger than memory would end the app, with the person's conversation in it)
            if let Some(size) = now.as_ref().map(std::fs::Metadata::len).filter(|size| *size > MOST_READ) {
                lines.push(format!("{} {name} ({size} bytes): too large to compare or show", if entry.is_some_and(|entry| !entry.gone) { "changed" } else { "created" }));
                continue;
            }
            let before = match entry {
                Some(entry) if entry.gone => Before::Absent,
                Some(entry) => match entry.copy.as_ref().and_then(|copy| std::fs::read(folder_of(&self.session).join(copy)).ok()) {
                    Some(bytes) => Before::Bytes(bytes),
                    None => Before::Unknown(entry.size),
                },
                None => match &self.head {
                    Some(head) => git_raw(workspace, &["--no-optional-locks", "show", &format!("{head}:./{name}")]).map_or(Before::Absent, Before::Bytes),
                    None => Before::Absent,
                },
            };
            let after = now.as_ref().and_then(|_| std::fs::read(&path).ok());
            if let (Before::Bytes(before), Some(after)) = (&before, &after) {
                // (touched and the same is no change)
                if before == after {
                    continue;
                }
            }
            if matches!(before, Before::Absent) && after.is_none() {
                continue;
            }
            let size = after.as_ref().map_or(0, Vec::len);
            let word = match (&before, &after) {
                (Before::Absent, _) => format!("created {name} ({size} bytes)"),
                (_, None) => format!("deleted {name}"),
                (Before::Bytes(before), Some(_)) => format!("changed {name} ({} to {size} bytes)", before.len()),
                (Before::Unknown(known), Some(_)) => format!("changed {name} ({known} to {size} bytes)"),
            };
            if may_hold_secrets(name) {
                lines.push(format!("{word}: it may hold secrets, so it is not shown"));
                continue;
            }
            let text = |bytes: &[u8]| file_text(bytes).0;
            let is_binary = after.as_deref().is_some_and(binary) || matches!(&before, Before::Bytes(bytes) if binary(bytes));
            let diff = match (&before, &after) {
                _ if is_binary => None,
                (Before::Unknown(_), _) => edited_diff(name),
                // (a file too large to have kept could have been created large as well: an answer has its limit)
                (Before::Absent, Some(after)) if after.len() as u64 > MOST_COPIED => edited_diff(name),
                (Before::Absent, after) => Some(FileChange::between(name, None, after.as_deref().map(text).as_deref()).diff),
                (Before::Bytes(before), after) => Some(FileChange::between(name, Some(&text(before)), after.as_deref().map(text).as_deref()).diff),
            };
            match diff {
                Some(diff) => {
                    lines.push(word);
                    if shown.len() < MOST_SHOWN {
                        shown.push(format!("--- {name} ---\n{}", cap(&diff, MOST_SHOWN_OF_A_FILE)));
                    }
                }
                None if matches!(before, Before::Unknown(_)) => lines.push(format!("{word}: no copy was kept of what it held, as it was too large or not text")),
                None => lines.push(word),
            }
        }
        let partly = if whole { "" } else { "--- too much to look through completely, so this may not be everything ---\n" };
        if lines.is_empty() {
            return partly.to_string();
        }
        format!("{partly}{}\n\n{}", lines.join("\n"), shown.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
        let (work, sessions) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let session = sessions.path().join("s.jsonl");
        (work, sessions, session)
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let mut all = vec!["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "core.autocrlf=false"];
        all.extend(args);
        git(dir, &all).unwrap();
    }

    /// A repository with one commit of `a.txt`.
    fn repository(dir: &Path) {
        run_git(dir, &["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "committed\n").unwrap();
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-q", "-m", "first"]);
    }

    #[test]
    fn a_file_too_large_to_hold_is_named_and_not_read() {
        let (work, _keep, session) = fixture();
        let baseline = Baseline::for_session(&session, work.path());
        std::fs::File::create(work.path().join("data.bin")).unwrap().set_len(MOST_READ + 1).unwrap();
        let said = baseline.changes(work.path(), &[]);
        assert!(said.contains("created data.bin") && said.contains("too large"), "{said}");
    }

    #[test]
    fn a_folder_with_no_repository_shows_what_changed_since_the_session_began() {
        let (work, _keep, session) = fixture();
        std::fs::write(work.path().join("kept.txt"), "same\n").unwrap();
        std::fs::write(work.path().join("edited.txt"), "one\ntwo\n").unwrap();
        std::fs::write(work.path().join("gone.txt"), "goodbye\n").unwrap();
        let baseline = Baseline::for_session(&session, work.path());
        assert!(baseline.head.is_none());
        assert_eq!(baseline.changes(work.path(), &[]), "", "nothing yet");
        std::fs::write(work.path().join("edited.txt"), "one\nTWO\n").unwrap();
        std::fs::remove_file(work.path().join("gone.txt")).unwrap();
        std::fs::write(work.path().join("new.txt"), "hello\n").unwrap();
        let said = baseline.changes(work.path(), &[]);
        assert!(said.contains("changed edited.txt") && said.contains("-two") && said.contains("+TWO"), "{said}");
        assert!(said.contains("deleted gone.txt") && said.contains("-goodbye"), "{said}");
        assert!(said.contains("created new.txt") && said.contains("+hello"), "{said}");
        assert!(!said.contains("kept.txt"), "{said}");
        let one = baseline.changes(work.path(), &["edited.txt"]);
        assert!(one.contains("edited.txt") && !one.contains("new.txt") && !one.contains("gone.txt"), "{one}");
    }

    #[test]
    fn a_repository_before_its_first_commit_compares_with_the_folder() {
        let (work, _keep, session) = fixture();
        run_git(work.path(), &["init", "-q"]);
        std::fs::write(work.path().join("a.txt"), "first\n").unwrap();
        let baseline = Baseline::for_session(&session, work.path());
        assert!(baseline.head.is_none());
        assert_eq!(baseline.changes(work.path(), &[]), "");
        std::fs::write(work.path().join("a.txt"), "second\n").unwrap();
        std::fs::write(work.path().join("b.txt"), "new\n").unwrap();
        let said = baseline.changes(work.path(), &[]);
        assert!(said.contains("+second") && said.contains("-first") && said.contains("created b.txt"), "{said}");
    }

    #[test]
    fn what_was_uncommitted_before_the_session_is_not_this_sessions_work() {
        let (work, _keep, session) = fixture();
        repository(work.path());
        std::fs::write(work.path().join("b.txt"), "tracked\n").unwrap();
        std::fs::write(work.path().join("c.txt"), "tracked too\n").unwrap();
        run_git(work.path(), &["add", "."]);
        run_git(work.path(), &["commit", "-q", "-m", "more"]);
        // All of this before the session: modified, deleted, and new.
        std::fs::write(work.path().join("a.txt"), "someone's own work\n").unwrap();
        std::fs::remove_file(work.path().join("b.txt")).unwrap();
        std::fs::write(work.path().join("mine.txt"), "untracked\n").unwrap();
        let baseline = Baseline::for_session(&session, work.path());
        assert_eq!(baseline.changes(work.path(), &[]), "", "nothing of it is the session's");
        // What the session does to the same file is against what it found.
        std::fs::write(work.path().join("a.txt"), "someone's own work\nand the session's\n").unwrap();
        let said = baseline.changes(work.path(), &[]);
        assert!(said.contains("+and the session's") && !said.contains("committed") && !said.contains("-someone's"), "{said}");
        assert!(!said.contains("b.txt") && !said.contains("mine.txt"), "{said}");
        std::fs::write(work.path().join("b.txt"), "back\n").unwrap();
        std::fs::remove_file(work.path().join("mine.txt")).unwrap();
        let said = baseline.changes(work.path(), &[]);
        assert!(said.contains("created b.txt") && said.contains("deleted mine.txt") && said.contains("-untracked"), "{said}");
    }

    #[test]
    fn a_session_that_only_creates_files_has_something_to_show() {
        let (work, _keep, session) = fixture();
        repository(work.path());
        std::fs::write(work.path().join(".gitignore"), "skipped.txt\n").unwrap();
        run_git(work.path(), &["add", "."]);
        run_git(work.path(), &["commit", "-q", "-m", "ignore"]);
        let baseline = Baseline::for_session(&session, work.path());
        std::fs::write(work.path().join("page.html"), "<p>new</p>\n").unwrap();
        std::fs::write(work.path().join("skipped.txt"), "not this\n").unwrap();
        let said = baseline.changes(work.path(), &[]);
        assert!(said.contains("created page.html") && said.contains("+<p>new</p>") && !said.contains("skipped.txt"), "{said}");
    }

    #[test]
    fn a_commit_and_a_reopening_during_a_session_do_not_move_where_it_began() {
        let (work, _keep, session) = fixture();
        repository(work.path());
        let first = Baseline::for_session(&session, work.path());
        assert!(first.head.is_some() && first.files.is_empty(), "in a clean repository nothing is copied");
        std::fs::write(work.path().join("a.txt"), "committed\nthe session's\n").unwrap();
        run_git(work.path(), &["add", "."]);
        run_git(work.path(), &["commit", "-q", "-m", "second"]);
        let again = Baseline::for_session(&session, work.path());
        assert_eq!(again.head, first.head);
        let said = again.changes(work.path(), &[]);
        assert!(said.contains("+the session's"), "the commit is part of the session's work: {said}");
    }

    #[test]
    fn a_reopened_session_compares_with_where_it_began_and_not_with_now() {
        let (work, _keep, session) = fixture();
        std::fs::write(work.path().join("a.txt"), "first\n").unwrap();
        Baseline::for_session(&session, work.path());
        std::fs::write(work.path().join("a.txt"), "second\n").unwrap();
        let said = Baseline::for_session(&session, work.path()).changes(work.path(), &[]);
        assert!(said.contains("-first") && said.contains("+second"), "{said}");
    }

    #[test]
    fn a_large_file_is_listed_and_not_copied_and_a_touch_is_no_change() {
        let (work, _keep, session) = fixture();
        std::fs::write(work.path().join("big.txt"), "x".repeat(MOST_COPIED as usize + 1)).unwrap();
        std::fs::write(work.path().join("small.txt"), "same").unwrap();
        let baseline = Baseline::for_session(&session, work.path());
        assert_eq!(std::fs::read_dir(folder_of(&session)).unwrap().count(), 2, "the small one's copy and the list");
        std::fs::write(work.path().join("big.txt"), "y".repeat(MOST_COPIED as usize + 5)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(work.path().join("small.txt"), "same").unwrap();
        let said = baseline.changes(work.path(), &[]);
        assert!(said.contains("changed big.txt") && said.contains("no copy was kept"), "{said}");
        assert!(!said.contains("small.txt"), "{said}");
    }

    #[test]
    fn what_may_hold_secrets_is_neither_copied_nor_shown() {
        let (work, _keep, session) = fixture();
        std::fs::write(work.path().join(".env"), "KEY=before-secret").unwrap();
        let baseline = Baseline::for_session(&session, work.path());
        assert_eq!(std::fs::read_dir(folder_of(&session)).unwrap().count(), 1, "the list alone");
        std::fs::write(work.path().join(".env"), "KEY=after-secret-longer").unwrap();
        let said = baseline.changes(work.path(), &[]);
        assert!(said.contains("changed .env") && !said.contains("secret-longer") && !said.contains("before-secret"), "{said}");
        std::fs::write(work.path().join(".env.local"), "KEY=created-secret").unwrap();
        let said = baseline.changes(work.path(), &[]);
        assert!(said.contains("created .env.local") && !said.contains("created-secret"), "{said}");
    }
}
