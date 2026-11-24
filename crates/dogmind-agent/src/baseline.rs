//! Where a session began, for `show_diff`, kept beside the session so that a commit or a reopening does not move it.
//!
//! In a repository: HEAD, and copies of what differed from it then (what was modified, and what was new). Git has the rest, so the cost is that of asking it, and what was
//! already changed before the session is not taken for this session's work. Outside one, or before the first commit: a list of the folder's files, with a copy of each small
//! text file. What is too large or too much is listed or left out, and the answer says so. Neither copies a whole folder, and what `.gitignore` skips is skipped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::tools::{cap, cap_ends, decode_file_text, file_text, git, git_raw, untracked_names, walker, FileChange};

/// What a first message may wait for, in a folder with no repository.
const LOOKING: Duration = Duration::from_secs(3);
const MOST_FILES: usize = 20_000;
const MOST_COPIED: u64 = 256 * 1024;
const MOST_KEPT: u64 = 32 << 20;
/// What is read to compare it with what was there.
pub(crate) const MOST_READ: u64 = 64 << 20;
const MOST_SHOWN_OF_A_FILE: usize = 6000;
/// Files shown in full in an answer: past these, a line each.
const MOST_SHOWN: usize = 100;
const MANIFEST: &str = "manifest.json";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Entry {
    size: u64,
    modified: Option<u128>,
    /// The copy's name in the folder; none when it was too large, not text, or may hold secrets.
    copy: Option<String>,
    /// A tracked file that had been deleted when the session began.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    gone: bool,
}

/// A manifest of an older or a newer build may lack a field, or have one more: what it has is read, and what it lacks is empty, and it is no reason to take the starting point anew.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Baseline {
    /// HEAD when the session began; none outside a repository, or in one with no commit yet.
    pub head: Option<String>,
    /// In a repository, what differed from `head` then; elsewhere, the folder.
    files: BTreeMap<String, Entry>,
    /// Whether every file was looked at.
    whole: bool,
    /// When the starting point was taken anew because the record of the first could not be read: what changed before then is not known, and `changes` says so.
    #[serde(skip_serializing_if = "Option::is_none")]
    moved: Option<String>,
    #[serde(skip)]
    session: PathBuf,
}

/// A file's whole new content, written beside it and then in its place: a crash in the middle leaves the old file or none, and never a cut one that the next look takes for whole.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let partial = path.with_extension(format!("partial-{}", std::process::id()));
    let done = std::fs::File::create(&partial)
        .and_then(|mut out| {
            std::io::Write::write_all(&mut out, bytes)?;
            out.sync_all()
        })
        .and_then(|()| std::fs::rename(&partial, path));
    if done.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    done
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

/// Git candidates relative to the saved commit. Only a complete listing can establish that a new path has no old blob.
fn git_candidates(workspace: &Path, head: &str) -> (BTreeMap<String, bool>, bool) {
    let tracked = git(workspace, &["--no-optional-locks", "diff", "--no-renames", "--name-status", "-z", "--relative", head]);
    let untracked = untracked_names(workspace, &[]);
    let complete = tracked.is_ok() && untracked.is_ok();
    let mut names = BTreeMap::new();
    if let Ok(output) = tracked {
        let mut fields = output.split_terminator('\0');
        while let (Some(status), Some(name)) = (fields.next(), fields.next()) {
            names.insert(name.to_string(), status != "A");
        }
    }
    for name in untracked.unwrap_or_default() {
        // A staged deletion can also appear as untracked if the file was recreated.
        names.entry(name).or_insert(false);
    }
    (names, complete)
}

/// One file as it is now, with a copy of it if it is small text and within what may be kept.
fn entry_of(folder: &Path, name: &str, path: &Path, data: &std::fs::Metadata, kept: &mut u64) -> Entry {
    let mut entry = Entry { size: data.len(), modified: modified(data), ..Entry::default() };
    if entry.size <= MOST_COPIED && *kept + entry.size <= MOST_KEPT && !may_hold_secrets(name) {
        if let Ok(bytes) = std::fs::read(path) {
            if !binary(&bytes) && std::fs::create_dir_all(folder).is_ok() {
                let copy: String = Sha256::digest(&bytes).iter().take(16).map(|byte| format!("{byte:02x}")).collect();
                // (named by what it holds, so one that is there is the same, unless a crash cut it: it is used only at the size of the file it is a copy of)
                let there = std::fs::metadata(folder.join(&copy)).is_ok_and(|data| data.len() == entry.size && data.len() == bytes.len() as u64);
                if there || write_atomically(&folder.join(&copy), &bytes).is_ok() {
                    *kept += entry.size;
                    entry.copy = Some(copy);
                }
            }
        }
    }
    entry
}

/// A Git blob often has LF where its Windows checkout has CRLF. Show that difference
/// once so the unified diff still locates the edited lines within the report limit.
fn text_diff(name: &str, before: &str, after: &str) -> String {
    fn uniform_crlf(text: &str) -> Option<bool> {
        let mut ends = text.match_indices('\n').map(|(at, _)| text[..at].ends_with('\r'));
        let first = ends.next()?;
        ends.all(|crlf| crlf == first).then_some(first)
    }

    let diff = |old: &str, new: &str| FileChange::between(name, Some(old), Some(new)).diff;
    if before.contains("\r\n") != after.contains("\r\n") {
        match (uniform_crlf(before), uniform_crlf(after)) {
            (Some(false), Some(true)) => return format!("line endings: LF to CRLF\n{}", diff(before, &after.replace("\r\n", "\n"))),
            (Some(true), Some(false)) => return format!("line endings: CRLF to LF\n{}", diff(&before.replace("\r\n", "\n"), after)),
            _ => {}
        }
    }
    diff(before, after)
}

/// A deletion has one hunk and one '-' per original line. Its middle needs only a byte count.
fn deleted_preview(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return cap(&similar::TextDiff::from_lines(text, "").unified_diff().to_string(), limit);
    }
    let missing_newline = !text.ends_with(['\r', '\n']);
    let lone_cr = text.match_indices('\r').filter(|(at, _)| text.as_bytes().get(at + 1) != Some(&b'\n')).count();
    let lines = text.matches('\n').count() + lone_cr + usize::from(missing_newline);
    let header = if lines == 1 { "@@ -1 +0,0 @@\n".to_string() } else { format!("@@ -1,{lines} +0,0 @@\n") };
    let hint = if missing_newline { "\n\\ No newline at end of file\n" } else { "" };
    let total = header.len() + text.len() + lines + hint.len();

    // Even if a slice cuts a line, its artificial edge is outside the part kept by cap_ends.
    let mark = |out: &mut String, slice: &str| {
        for line in similar::DiffableStr::tokenize_lines(slice) {
            out.push('-');
            out.push_str(line);
        }
    };
    let mut first = header;
    mark(&mut first, &text[..text.ceil_char_boundary(limit * 3 / 4)]);
    let mut last = String::new();
    mark(&mut last, &text[text.floor_char_boundary(text.len() - limit / 4)..]);
    last.push_str(hint);
    cap_ends(&first, &last, total, limit)
}

/// What was there before, as far as it is known.
enum Before {
    Absent,
    Bytes(Vec<u8>),
    /// It was there, of this size, and no copy was kept.
    Unknown(u64),
}

struct Blob {
    id: String,
    size: u64,
}

impl Blob {
    fn from_tree_header(header: &str) -> Result<Option<Blob>, String> {
        let mut fields = header.split_whitespace().skip(1);
        if fields.next() != Some("blob") {
            return Ok(None);
        }
        let id = fields.next().ok_or("missing blob ID in the starting tree")?;
        let size = fields.next().and_then(|field| field.parse().ok()).ok_or("invalid blob size in the starting tree")?;
        Ok(Some(Blob { id: id.to_string(), size }))
    }

    fn read(&self, workspace: &Path) -> Result<Before, String> {
        if self.size > MOST_READ {
            return Ok(Before::Unknown(self.size));
        }
        git_raw(workspace, &["--no-optional-locks", "cat-file", "blob", &self.id]).map(Before::Bytes)
    }
}

/// Ask for sizes together, keeping literal path arguments comfortably within the Windows command-line limit.
fn starting_blobs(workspace: &Path, head: &str, mut paths: &[&str]) -> BTreeMap<String, Result<Option<Blob>, String>> {
    let mut entries = BTreeMap::new();
    while !paths.is_empty() {
        let mut bytes = 0;
        let count = paths.iter().take_while(|path| {
            bytes += path.len() + 3;
            bytes <= 12 * 1024
        }).count().max(1);
        let (batch, rest) = paths.split_at(count);
        paths = rest;
        let mut args = vec!["--no-optional-locks", "--literal-pathspecs", "ls-tree", "-l", "-z", head, "--"];
        args.extend_from_slice(batch);
        match git(workspace, &args) {
            Ok(output) => {
                for entry in output.split_terminator('\0') {
                    if let Some((header, name)) = entry.split_once('\t') {
                        entries.insert(name.to_string(), Blob::from_tree_header(header));
                    }
                }
            }
            Err(error) => {
                for name in batch {
                    entries.insert((*name).to_string(), Err(error.clone()));
                }
            }
        }
    }
    entries
}

impl Baseline {
    /// A session's, as it was first taken; taken now if it has none.
    pub fn for_session(session: &Path, workspace: &Path) -> Baseline {
        let folder = folder_of(session);
        let text = std::fs::read_to_string(folder.join(MANIFEST));
        let kept = text.as_ref().ok().and_then(|text| serde_json::from_str::<Baseline>(text).ok());
        // A record that is there and cannot be read is not written over: it is kept aside, and the starting point, taken now, says that it moved.
        let damaged = kept.is_none() && folder.join(MANIFEST).exists();
        if damaged {
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let _ = std::fs::rename(folder.join(MANIFEST), folder.join(format!("manifest.damaged-{stamp}")));
        }
        let mut baseline = kept.unwrap_or_else(|| {
            let mut taken = Baseline::take(&folder, workspace);
            if damaged {
                taken.moved = Some(chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string());
            }
            if std::fs::create_dir_all(&folder).is_ok() {
                if let Ok(text) = serde_json::to_string(&taken) {
                    let _ = write_atomically(&folder.join(MANIFEST), text.as_bytes());
                }
            }
            taken
        });
        baseline.session = session.to_path_buf();
        baseline
    }

    fn take(folder: &Path, workspace: &Path) -> Baseline {
        Baseline::take_within(folder, workspace, LOOKING)
    }

    fn take_within(folder: &Path, workspace: &Path, looking: Duration) -> Baseline {
        let head = git(workspace, &["rev-parse", "--verify", "-q", "HEAD"]).ok().map(|head| head.trim().to_string()).filter(|head| !head.is_empty());
        let (mut files, mut kept, mut whole) = (BTreeMap::new(), 0u64, true);
        match &head {
            Some(head) => {
                let (names, complete) = git_candidates(workspace, head);
                whole = complete;
                // The time is for the copies. What git took to name the files is no reason to keep none of them: on a busy machine it
                // was all of it, and what was changed before the session was taken for the session's work. Past the time each is still
                // kept by its size and time, which costs nothing to ask, and only its copy is left out.
                let started = Instant::now();
                for name in names.into_keys() {
                    if files.len() >= MOST_FILES {
                        whole = false;
                        break;
                    }
                    let path = workspace.join(&name);
                    let entry = match std::fs::metadata(&path) {
                        Ok(data) if started.elapsed() <= looking => entry_of(folder, &name, &path, &data, &mut kept),
                        Ok(data) => Entry { size: data.len(), modified: modified(&data), ..Entry::default() },
                        Err(_) => Entry { gone: true, ..Entry::default() },
                    };
                    files.insert(name, entry);
                }
            }
            None if std::fs::read_dir(workspace).is_err() => whole = false,
            None => {
                let started = Instant::now();
                for entry in walker(workspace, true).build() {
                    if files.len() >= MOST_FILES || started.elapsed() > looking {
                        whole = false;
                        break;
                    }
                    let Ok(entry) = entry else {
                        whole = false;
                        continue;
                    };
                    if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                        continue;
                    }
                    let Ok(data) = entry.metadata() else {
                        whole = false;
                        continue;
                    };
                    let name = entry.path().strip_prefix(workspace).unwrap_or(entry.path()).to_string_lossy().replace('\\', "/");
                    let kept_entry = entry_of(folder, &name, entry.path(), &data, &mut kept);
                    files.insert(name, kept_entry);
                }
            }
        }
        Baseline { head, files, whole, moved: None, session: PathBuf::new() }
    }

    /// Each candidate and whether it existed at the start, followed by whether the list is complete.
    fn candidates(&self, workspace: &Path) -> (BTreeMap<String, bool>, bool) {
        let saved = self.files.iter().map(|(name, entry)| (name.clone(), !entry.gone));
        if let Some(head) = &self.head {
            let (mut names, complete) = git_candidates(workspace, head);
            // The saved worktree takes precedence over the commit, including pre-session deletions.
            names.extend(saved);
            return (names, self.whole && complete);
        }
        let mut names: BTreeMap<_, _> = saved.collect();
        let started = Instant::now();
        let mut seen = 0;
        let mut whole = self.whole;
        for entry in walker(workspace, true).build() {
            if seen >= MOST_FILES || started.elapsed() > LOOKING {
                return (names, false);
            }
            let Ok(entry) = entry else {
                whole = false;
                continue;
            };
            if entry.file_type().is_some_and(|kind| kind.is_file()) {
                seen += 1;
                names.entry(entry.path().strip_prefix(workspace).unwrap_or(entry.path()).to_string_lossy().replace('\\', "/")).or_insert(false);
            }
        }
        (names, whole)
    }

    /// What has changed under the workspace since the session began: a line for each file, then what can be shown of them. Empty when nothing has.
    pub fn changes(&self, workspace: &Path, paths: &[&str]) -> String {
        if let Err(error) = std::fs::read_dir(workspace) {
            return format!("could not list the workspace for this session's differences: {error}");
        }
        let filters: Vec<PathBuf> = paths.iter().filter(|path| !path.is_empty()).map(|path| {
            let normalized = path.replace('\\', "/");
            let path = Path::new(&normalized);
            path.strip_prefix(".").unwrap_or(path).to_path_buf()
        }).collect();
        let wanted = |name: &str| paths.is_empty() || filters.iter().any(|dir_or_file| Path::new(name).starts_with(dir_or_file));
        let (names, whole) = self.candidates(workspace);
        let (mut lines, mut shown): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
        // (of what the agent itself edited, the session has a first copy whatever its size)
        let edited = std::cell::OnceCell::new();
        let edited_diff = |name: &str| -> Option<String> {
            let changes = edited.get_or_init(|| crate::checkpoint::Checkpoints::open(&self.session).session_changes(workspace));
            // (a change with no diff is one of a binary or of a file too large to read: nothing is shown of it, as of any other)
            changes.iter().find(|change| change.path.replace('\\', "/") == name).map(|change| change.diff.clone()).filter(|diff| !diff.is_empty())
        };
        let blobs = std::cell::OnceCell::new();
        let committed_file = |head: &str, name: &str, existed: bool| -> Result<Before, String> {
            if !existed && whole {
                return Ok(Before::Absent);
            }
            let blobs = blobs.get_or_init(|| {
                let paths: Vec<_> = names.iter().filter(|(path, existed)| (**existed || !whole) && !self.files.contains_key(*path) && wanted(path))
                    .map(|(path, _)| path.as_str()).collect();
                starting_blobs(workspace, head, &paths)
            });
            match blobs.get(name) {
                Some(Ok(Some(blob))) => blob.read(workspace),
                Some(Err(error)) => Err(error.clone()),
                None if existed => Err("missing tree entry for the starting version".to_string()),
                _ => Ok(Before::Absent),
            }
        };
        for (name, &existed) in names.iter().filter(|(name, _)| wanted(name)) {
            let path = workspace.join(name);
            let missing = |error: &std::io::Error| matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory);
            let now = match std::fs::metadata(&path) {
                Ok(data) if data.is_dir() => None, // A directory can replace a file that existed at the start.
                Ok(data) => Some(data),
                Err(error) if missing(&error) => None,
                Err(error) => {
                    lines.push(format!("could not compare {name}: {error}"));
                    continue;
                }
            };
            let entry = self.files.get(name);
            let same_metadata = matches!((entry, &now), (Some(entry), Some(data))
                if !entry.gone && entry.size == data.len() && entry.modified == modified(data));
            match (entry, &now) {
                (Some(entry), None) if entry.gone => continue,
                // A copy can preserve the timestamp and size of a different file.
                // Compare bytes whenever the original contents are available.
                (Some(entry), _) if same_metadata && entry.copy.is_none() => continue,
                (None, None) if self.head.is_none() => continue,
                (None, Some(data)) if self.head.is_none() && !self.whole => {
                    lines.push(format!("present {name} ({} bytes): its starting contents are unknown because the initial file list was incomplete", data.len()));
                    continue;
                }
                _ => {}
            }
            // (nothing is shown of it, and to read it is to hold it: one larger than memory would end the app, with the person's conversation in it)
            if let Some(size) = now.as_ref().map(std::fs::Metadata::len).filter(|size| *size > MOST_READ) {
                lines.push(format!("{} {name} ({size} bytes): too large to compare or show", if existed { "changed" } else if whole { "created" } else { "present" }));
                continue;
            }
            let after = match now.map(|_| std::fs::read(&path)).transpose() {
                Ok(bytes) => bytes,
                Err(error) if missing(&error) => None, // It disappeared after the metadata check.
                Err(error) => {
                    lines.push(format!("could not compare {name}: {error}"));
                    continue;
                }
            };
            let before = match entry {
                Some(entry) if entry.gone => Before::Absent,
                Some(entry) => match entry.copy.as_ref().and_then(|copy| std::fs::read(folder_of(&self.session).join(copy)).ok()) {
                    Some(bytes) => Before::Bytes(bytes),
                    None if same_metadata && after.is_some() => continue,
                    None => Before::Unknown(entry.size),
                },
                None => match &self.head {
                    Some(head) => match committed_file(head, name, existed) {
                        Ok(before) => before,
                        Err(error) => {
                            lines.push(format!("could not compare {name} with the starting commit: {error}"));
                            continue;
                        }
                    },
                    None => Before::Absent,
                },
            };
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
                (Before::Unknown(known), None) if *known > MOST_READ => format!("deleted {name} ({known} bytes)"),
                (_, None) => format!("deleted {name}"),
                (Before::Bytes(before), Some(_)) => format!("changed {name} ({} to {size} bytes)", before.len()),
                (Before::Unknown(known), Some(_)) => format!("changed {name} ({known} to {size} bytes)"),
            };
            if may_hold_secrets(name) {
                lines.push(format!("{word}: it may hold secrets, so it is not shown"));
                continue;
            }
            if matches!(before, Before::Unknown(known) if known > MOST_READ) {
                lines.push(format!("{word}: previous content too large to compare or show"));
                continue;
            }
            // Once the previews are full, only an unknown starting file may need a note
            // about its missing copy. All other changes already have their summary.
            if shown.len() == MOST_SHOWN && !matches!(before, Before::Unknown(_)) {
                lines.push(word);
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
                (Before::Bytes(before), Some(after)) => Some(text_diff(name, &text(before), &text(after))),
                (Before::Bytes(before), None) => {
                    let (text, _) = decode_file_text(before);
                    let preview = deleted_preview(&text, MOST_SHOWN_OF_A_FILE);
                    shown.push(format!("--- {name} ---\n{preview}"));
                    lines.push(word);
                    continue;
                }
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
        let partly = if whole { "" } else { "--- could not list all files, so this may not be everything ---\n" };
        // (said every time: what changed before the starting point moved is not in this, and a model that reads "nothing changed" would take it for the whole session)
        let moved = self.moved.as_ref().map_or(String::new(), |time| format!("note: the record of where this session began could not be read, so it began again at {time}: what the session changed before then is not shown here, and the files themselves are the way to see it.\n"));
        if lines.is_empty() {
            return format!("{partly}{moved}");
        }
        format!("{partly}{moved}{}\n\n{}", lines.join("\n"), shown.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capped_deletion_matches_the_complete_unified_diff_at_utf8_and_line_boundaries() {
        for text in ["", "終", "行\r\n次\r最終\n", "a\r\r\n🌿x", "\n\n", "長い改行なし"] {
            for copies in [1, 60] {
                let text = text.repeat(copies);
                let full = FileChange::between("a", Some(&text), None).diff;
                for limit in 0..=64 {
                    assert_eq!(deleted_preview(&text, limit), cap(&full, limit), "limit={limit}, text={text:?}");
                }
            }
        }
    }

    #[test]
    fn a_checkout_with_other_line_endings_still_shows_the_edited_lines() {
        let before = "first\nold\nlast\n";
        let after = "first\r\nnew\r\nlast\r\n";
        let report = text_diff("a", before, after);
        assert!(report.starts_with("line endings: LF to CRLF\n"));
        assert!(report.contains("-old\n+new\n") && !report.contains("-first"));
        assert_eq!(text_diff("a", "x\r\n", "x\n"), "line endings: CRLF to LF\n");
        assert!(text_diff("a", "a\nb\n", "a\r\nb").contains("No newline at end of file"));
        let mixed = "first\r\nnew\nlast\r\n";
        assert_eq!(text_diff("a", before, mixed), FileChange::between("a", Some(before), Some(mixed)).diff);
    }

    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
        let (work, sessions) = (crate::in_container::tempdir().unwrap(), crate::in_container::tempdir().unwrap());
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

    /// A manifest cut by a crash is not written over: it is kept aside, the starting point is taken now, and what `show_diff` says tells the model it moved.
    #[test]
    fn a_manifest_that_cannot_be_read_is_kept_aside_and_the_start_is_said_to_have_moved() {
        let (work, _keep, session) = fixture();
        std::fs::write(work.path().join("a.txt"), "first\n").unwrap();
        Baseline::for_session(&session, work.path());
        let folder = folder_of(&session);
        std::fs::write(work.path().join("a.txt"), "second\n").unwrap();
        let whole = std::fs::read_to_string(folder.join(MANIFEST)).unwrap();
        std::fs::write(folder.join(MANIFEST), &whole[..whole.len() / 2]).unwrap();
        let baseline = Baseline::for_session(&session, work.path());
        let aside: Vec<String> = std::fs::read_dir(&folder).unwrap().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).filter(|name| name.starts_with("manifest.damaged-")).collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert_eq!(std::fs::read_to_string(folder.join(&aside[0])).unwrap(), whole[..whole.len() / 2], "kept as it was");
        let said = baseline.changes(work.path(), &[]);
        assert!(said.contains("began again at") && !said.contains("+second"), "the work before the move is not shown, and it says so: {said}");
        std::fs::write(work.path().join("a.txt"), "third\n").unwrap();
        let said = baseline.changes(work.path(), &[]);
        assert!(said.contains("began again at") && said.contains("-second") && said.contains("+third"), "{said}");
        // (and it is said again by the next look at the session, which reads the new record)
        let again = Baseline::for_session(&session, work.path()).changes(work.path(), &[]);
        assert!(again.contains("began again at"), "{again}");
        // (a manifest that was never there is no loss)
        let (other, _keep2, other_session) = fixture();
        assert_eq!(Baseline::for_session(&other_session, other.path()).changes(other.path(), &[]), "");
    }

    /// A manifest of another build, with a field fewer or more, is read, and is no reason to begin again.
    #[test]
    fn a_manifest_with_other_fields_is_read_as_far_as_it_goes() {
        let (work, _keep, session) = fixture();
        std::fs::write(work.path().join("a.txt"), "first\n").unwrap();
        let first = Baseline::for_session(&session, work.path());
        let folder = folder_of(&session);
        let mut value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(folder.join(MANIFEST)).unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("whole");
        value["from_a_later_build"] = serde_json::json!(true);
        value["files"]["a.txt"].as_object_mut().unwrap().remove("modified");
        std::fs::write(folder.join(MANIFEST), value.to_string()).unwrap();
        std::fs::write(work.path().join("a.txt"), "second\n").unwrap();
        let said = Baseline::for_session(&session, work.path()).changes(work.path(), &[]);
        assert!(said.contains("-first") && said.contains("+second") && !said.contains("began again"), "{said}");
        assert_eq!(first.files.len(), 1);
    }

    /// A copy that a crash cut is not the "before": one of the wrong size is written again.
    #[test]
    fn a_copy_of_the_wrong_size_is_not_used() {
        let (work, _keep, session) = fixture();
        let folder = folder_of(&session);
        std::fs::create_dir_all(&folder).unwrap();
        let file = work.path().join("a.txt");
        std::fs::write(&file, "the whole file\n").unwrap();
        let bytes = std::fs::read(&file).unwrap();
        let name: String = Sha256::digest(&bytes).iter().take(16).map(|byte| format!("{byte:02x}")).collect();
        std::fs::write(folder.join(&name), "the who").unwrap();
        let data = std::fs::metadata(&file).unwrap();
        let entry = entry_of(&folder, "a.txt", &file, &data, &mut 0);
        assert_eq!(entry.copy.as_deref(), Some(name.as_str()));
        assert_eq!(std::fs::read(folder.join(&name)).unwrap(), bytes, "the cut copy was written again");
        // (and one that is whole is the one used, and left as it is)
        let before = std::fs::metadata(folder.join(&name)).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(30));
        entry_of(&folder, "a.txt", &file, &data, &mut 0);
        assert_eq!(std::fs::metadata(folder.join(&name)).unwrap().modified().unwrap(), before);
        assert!(std::fs::read_dir(&folder).unwrap().flatten().all(|entry| !entry.file_name().to_string_lossy().contains("partial")), "nothing left beside");
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
    fn a_recreated_untracked_path_still_compares_with_its_blob_if_git_cannot_list_changes() {
        let (work, _keep, session) = fixture();
        repository(work.path());
        let baseline = Baseline::for_session(&session, work.path());
        run_git(work.path(), &["rm", "--cached", "-q", "--", "a.txt"]);
        std::fs::write(work.path().join("a.txt"), "recreated\n").unwrap();
        std::fs::write(work.path().join("fresh.txt"), "new\n").unwrap();
        let complete = baseline.changes(work.path(), &[]);
        assert!(complete.contains("changed a.txt") && complete.contains("-committed") && complete.contains("+recreated"), "{complete}");
        assert!(complete.contains("created fresh.txt"), "{complete}");
        // The tracked diff fails, but Git still lists a.txt among the untracked paths and can read its original blob.
        run_git(work.path(), &["config", "diff.orderFile", "missing-order-file"]);
        let partial = baseline.changes(work.path(), &[]);
        let (notice, changes) = partial.split_once('\n').unwrap();
        assert!(notice.contains("could not list all files"), "{partial}");
        assert_eq!(changes, complete);

        let mut unavailable = baseline;
        unavailable.head = Some("0000000000000000000000000000000000000000".to_string());
        let report = unavailable.changes(work.path(), &["a.txt"]);
        assert!(report.contains("could not compare a.txt with the starting commit") && !report.contains("created a.txt"), "{report}");
    }

    #[test]
    fn an_oversized_committed_blob_is_reported_without_reading_its_contents() {
        let (work, _keep, session) = fixture();
        repository(work.path());
        let blob = work.path().join("data.bin");
        let size = MOST_READ + 1;
        std::fs::File::create(&blob).unwrap().set_len(size).unwrap();
        run_git(work.path(), &["add", "data.bin"]);
        run_git(work.path(), &["commit", "-qm", "binary asset"]);
        let baseline = Baseline::for_session(&session, work.path());
        std::fs::remove_file(&blob).unwrap();
        let report = baseline.changes(work.path(), &[]);
        assert!(report.contains(&format!("deleted data.bin ({size} bytes)")) && report.contains("too large"), "{report}");
        std::fs::write(&blob, b"tiny").unwrap();
        let report = baseline.changes(work.path(), &[]);
        assert!(report.contains(&format!("changed data.bin ({size} to 4 bytes)")) && report.contains("too large"), "{report}");
        assert!(!report.contains("@@"), "{report}");
    }

    #[test]
    fn oversized_session_files_keep_their_status_from_the_starting_commit_and_worktree() {
        let (repo, _keep, session) = fixture();
        let work = repo.path().join("pkg");
        std::fs::create_dir(&work).unwrap();
        run_git(repo.path(), &["init", "-q"]);
        for name in ["tracked.bin", "missing.bin"] {
            std::fs::write(work.join(name), "committed").unwrap();
        }
        run_git(repo.path(), &["add", "."]);
        run_git(repo.path(), &["commit", "-qm", "start"]);
        std::fs::remove_file(work.join("missing.bin")).unwrap();
        std::fs::write(work.join("already-new.bin"), "untracked at the start").unwrap();
        let baseline = Baseline::for_session(&session, &work);
        std::fs::write(work.join("committed-later.bin"), "after the start").unwrap();
        run_git(&work, &["add", "committed-later.bin"]);
        run_git(&work, &["commit", "-qm", "later"]);
        for name in ["tracked.bin", "missing.bin", "already-new.bin", "committed-later.bin", "new.bin"] {
            std::fs::File::create(work.join(name)).unwrap().set_len(MOST_READ + 1).unwrap();
        }
        let said = baseline.changes(&work, &[]);
        for label in ["changed tracked.bin", "created missing.bin", "changed already-new.bin", "created committed-later.bin", "created new.bin"] {
            assert!(said.contains(&format!("{label} ({} bytes): too large to compare or show", MOST_READ + 1)), "{said}");
        }
        assert_eq!(said.trim().lines().count(), 5, "{said}");
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

    /// With no time for copies at all, as on a machine too busy for git to answer in time: what was changed before the session is
    /// still known for what it was, and is not the session's.
    #[test]
    fn what_was_changed_before_is_not_the_sessions_even_with_no_time_to_copy_it() {
        let (work, _keep, session) = fixture();
        repository(work.path());
        std::fs::write(work.path().join("a.txt"), "someone's own work\n").unwrap();
        std::fs::write(work.path().join("mine.txt"), "untracked\n").unwrap();
        let mut baseline = Baseline::take_within(&folder_of(&session), work.path(), Duration::ZERO);
        baseline.session = session.clone();
        assert_eq!(baseline.changes(work.path(), &[]), "", "nothing of it is the session's, and nothing is missing");
        std::fs::write(work.path().join("a.txt"), "someone's own work\nand the session's\n").unwrap();
        let said = baseline.changes(work.path(), &[]);
        assert!(said.contains("changed a.txt") && !said.contains("mine.txt"), "{said}");
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
    fn a_full_preview_list_still_explains_which_file_had_no_starting_copy() {
        let (work, _keep, session) = fixture();
        let unknown = work.path().join("z-unknown.txt");
        std::fs::write(&unknown, vec![b'x'; MOST_COPIED as usize + 1]).unwrap();
        let baseline = Baseline::for_session(&session, work.path());
        for i in 0..=MOST_SHOWN {
            std::fs::write(work.path().join(format!("a-{i:03}.txt")), "new").unwrap();
        }
        std::fs::write(unknown, "changed").unwrap();
        let report = baseline.changes(work.path(), &[]);
        let last_visible = format!("a-{:03}.txt", MOST_SHOWN - 1);
        let first_hidden = format!("a-{MOST_SHOWN:03}.txt");
        assert!(report.contains(&format!("--- {last_visible} ---")));
        assert!(report.contains(&format!("created {first_hidden} (3 bytes)")));
        assert!(!report.contains(&format!("--- {first_hidden} ---")));
        assert!(report.lines().any(|line| line.starts_with("changed z-unknown.txt") && line.contains("no copy was kept")));
    }

    #[test]
    fn a_missing_workspace_is_neither_an_empty_start_nor_a_mass_deletion() {
        let (parent, _keep, session) = fixture();
        let work = parent.path().join("work");
        let baseline = Baseline::for_session(&session, &work);
        assert!(!baseline.whole);
        assert!(baseline.changes(&work, &[]).contains("could not list the workspace"));

        std::fs::create_dir(&work).unwrap();
        let file = work.join("notes.txt");
        std::fs::write(&file, "original").unwrap();
        let reopened = Baseline::for_session(&session, &work);
        let report = reopened.changes(&work, &[]);
        assert!(report.contains("present notes.txt (8 bytes)") && !report.contains("created notes.txt"), "{report}");

        let known = Baseline::for_session(&session.with_file_name("known.jsonl"), &work);
        let offline = parent.path().join("offline");
        std::fs::rename(&work, &offline).unwrap();
        let report = known.changes(&work, &[]);
        assert!(report.contains("could not list the workspace") && !report.contains("deleted notes.txt"), "{report}");
        std::fs::rename(offline, &work).unwrap();
        assert!(known.changes(&work, &[]).is_empty());
        std::fs::remove_file(file).unwrap();
        assert!(known.changes(&work, &[]).contains("deleted notes.txt"));
    }

    #[test]
    fn replacing_bytes_and_preserving_the_timestamp_still_shows_the_edit() {
        let (work, _keep, session) = fixture();
        let path = work.path().join("notes.txt");
        std::fs::write(&path, "old line\n").unwrap();
        let original_time = std::fs::metadata(&path).unwrap().modified().unwrap();
        Baseline::for_session(&session, work.path());

        std::fs::write(&path, "new line\n").unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(original_time).unwrap();
        let reopened = Baseline::for_session(&session, work.path());
        let report = reopened.changes(work.path(), &[]);
        assert!(report.contains("-old line") && report.contains("+new line"), "{report}");

        std::fs::write(&path, "old line\n").unwrap();
        assert!(reopened.changes(work.path(), &[]).is_empty());
        let copy = reopened.files["notes.txt"].copy.as_ref().unwrap();
        std::fs::remove_file(folder_of(&session).join(copy)).unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(original_time).unwrap();
        assert!(reopened.changes(work.path(), &[]).is_empty());
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
