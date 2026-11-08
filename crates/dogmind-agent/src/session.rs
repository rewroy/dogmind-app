//! A conversation on disk: one JSON line a message, in the app's own folder. Dogmind's `sessions.db` is sealed and is not
//! touched. A snapshot line replaces all before it, so a resumed session sends the same prefix it last sent.
//!
//! When the person has chosen to have conversations encrypted (`conversation_key`), a new session begins with a plain line that says
//! so, gives the file's own random id and the fingerprint of the key, and holds nothing of the conversation. Every line after it is
//! one message of AES-256-GCM: a fresh 12-byte nonce and the ciphertext, in base64, with the file's id and the line's number as
//! additional data, so that a line cannot be moved to another file or to another place in its own, and one changed by a byte does
//! not open. A line that does not open is damage: it is counted and told, and never dropped without a word. Writing stays one line
//! appended at a time, as it is for a plain file. A plain session still opens as it is, whatever is chosen.
//!
//! Everything that reads a session file goes through `entries_of`, and everything that writes one through `Session::write_json`
//! (the name of a session, which a window gives it while an agent may be writing, through `rename`, under the same count of lines).

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::conversation_key::{self, b64d, b64d_bytes, b64e, from_hex, hex, random_bytes, Cipher, Policy};
use crate::message::Message;
use crate::prompt::typed_text;

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Line {
    Meta { workspace: PathBuf, created: String },
    Message { message: Message },
    Snapshot { messages: Vec<Message> },
    Title { title: String },
    /// Web content has entered this conversation: for as long as the session lives, whatever a compaction leaves of it.
    WebSeen,
    /// The tools this conversation is offered, the last of these lines counting: they are part of its prefix, and what is installed may differ when it is reopened.
    Tools { tools: serde_json::Value },
    /// What the session works toward, as the person set it (/goal), the last of these lines counting; none: cleared.
    Goal { goal: Option<String> },
}

/// The plain first line of an encrypted session.
#[derive(Serialize, Deserialize)]
struct Header {
    kind: String,
    version: u32,
    /// The file's own random id, in hex: it is part of every line's and every picture's additional data.
    id: String,
    /// The fingerprint of the key the lines are under.
    key: String,
}

/// What starts a file beside a session that is sealed whole (a picture, an index): no picture format and no JSON starts so.
pub(crate) const SEALED_MARK: &[u8; 8] = b"DMENC\x01\0\0";
const ENCRYPTED: &str = "encrypted";
const FORMAT: u32 = 1;
const ID_BYTES: usize = 16;
/// What a header may take of a file's start.
const HEADER_MOST: u64 = 4096;

/// What an encrypted file is sealed with: the key, and the file's id.
#[derive(Clone)]
pub(crate) struct Sealed {
    cipher: Arc<Cipher>,
    id: [u8; ID_BYTES],
}

impl Sealed {
    pub(crate) fn new(cipher: &Arc<Cipher>) -> Result<Sealed, String> {
        Ok(Sealed { cipher: cipher.clone(), id: random_bytes::<ID_BYTES>().map_err(|error| error.to_string())? })
    }

    pub(crate) fn header(&self) -> String {
        serde_json::to_string(&Header { kind: ENCRYPTED.into(), version: FORMAT, id: hex(&self.id), key: self.cipher.fingerprint().to_string() }).expect("a plain header serializes")
    }

    fn aad(&self, number: u64) -> Vec<u8> {
        let mut aad = b"dogmind session line 1\0".to_vec();
        aad.extend_from_slice(&self.id);
        aad.extend_from_slice(&number.to_be_bytes());
        aad
    }

    /// A line as it is written: its place in the file is in what it is sealed with.
    pub(crate) fn line(&self, number: u64, json: &str) -> String {
        b64e(&self.cipher.seal(json.as_bytes(), &self.aad(number)))
    }

    fn open_line(&self, number: u64, text: &[u8]) -> Option<String> {
        let blob = b64d_bytes(text)?;
        String::from_utf8(self.cipher.open_owned(blob, &self.aad(number))?).ok()
    }

    fn file_aad(&self, kind: &str, name: &str) -> Vec<u8> {
        let mut aad = b"dogmind session file 1\0".to_vec();
        aad.extend_from_slice(kind.as_bytes());
        aad.push(0);
        aad.extend_from_slice(&self.id);
        aad.extend_from_slice(name.as_bytes());
        aad
    }

    /// A file beside the session, whole: its picture, its index of checkpoints. Sealed with the session's id and the file's name.
    pub(crate) fn seal_file(&self, kind: &str, name: &str, plain: &[u8]) -> Vec<u8> {
        self.cipher.seal(plain, &self.file_aad(kind, name))
    }

    pub(crate) fn open_file(&self, kind: &str, name: &str, sealed: &[u8]) -> Option<Vec<u8>> {
        self.cipher.open(sealed, &self.file_aad(kind, name))
    }

    /// As `seal_file`, with the mark that says it is sealed in front.
    pub(crate) fn seal_marked(&self, kind: &str, name: &str, plain: &[u8]) -> Vec<u8> {
        let mut out = SEALED_MARK.to_vec();
        out.extend(self.seal_file(kind, name, plain));
        out
    }

    /// What `seal_marked` made, opened; none if it has no mark or does not open.
    pub(crate) fn open_marked(&self, kind: &str, name: &str, bytes: &[u8]) -> Option<Vec<u8>> {
        self.open_file(kind, name, bytes.strip_prefix(SEALED_MARK)?)
    }
}

/// Why a file cannot be opened, as an error that can be told from the system's.
#[derive(Debug)]
struct Unreadable(&'static str);

impl std::fmt::Display for Unreadable {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(self.0)
    }
}

impl std::error::Error for Unreadable {}

/// Whether an error is that of an encrypted file whose key is not here.
pub fn is_unreadable(error: &std::io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<Unreadable>())
}

/// The key is not unlocked, or it is another's.
fn no_key() -> std::io::Error {
    std::io::Error::other(Unreadable(if conversation_key::any_unlocked() { conversation_key::OTHER_KEY } else { conversation_key::LOCKED }))
}

fn damaged_header() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "the first line of this encrypted conversation is damaged")
}

/// What the first line of a file says: plain, or encrypted under a key that is here.
fn sealed_from(first: &[u8]) -> std::io::Result<Option<Sealed>> {
    let text = String::from_utf8_lossy(first);
    let text = text.trim();
    if !text.starts_with("{\"kind\":\"encrypted\"") {
        return Ok(None);
    }
    let header: Header = serde_json::from_str(text).map_err(|_| damaged_header())?;
    if header.version != FORMAT {
        return Err(std::io::Error::other(Unreadable(conversation_key::NEWER_FORMAT)));
    }
    let id: [u8; ID_BYTES] = from_hex(&header.id).and_then(|bytes| bytes.try_into().ok()).ok_or_else(damaged_header)?;
    let cipher = conversation_key::find(&header.key).ok_or_else(no_key)?;
    Ok(Some(Sealed { cipher, id }))
}

/// What a session is sealed with: none for a plain one, and for one that is not there yet.
pub(crate) fn sealed_of(path: &Path) -> std::io::Result<Option<Sealed>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut first = Vec::new();
    BufReader::new(file.take(HEADER_MOST)).read_until(b'\n', &mut first)?;
    sealed_from(&first)
}

/// Whether a session file is an encrypted one, whether or not its key is here: by its first line.
pub fn is_encrypted(path: &Path) -> bool {
    let mut first = Vec::new();
    File::open(path).and_then(|file| BufReader::new(file.take(HEADER_MOST)).read_until(b'\n', &mut first)).is_ok() && String::from_utf8_lossy(&first).trim().starts_with("{\"kind\":\"encrypted\"")
}

pub struct Session {
    pub path: PathBuf,
    file: File,
    sealed: Option<Sealed>,
    generation: u64,
}

// What changed about the files of sessions under every `Session` (a conversion of all of them): an agent that holds a file open finds it again by its name.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Every conversion of the session files calls this when it has put new files in the places of old ones.
pub fn files_replaced() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    COUNTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
}

// How many lines each encrypted file has, by its name, for the number the next line is sealed under. Held while the line is written: a window naming a session and the session's own agent write one line at a time, each under its own number.
static COUNTS: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());

fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").to_lowercase()
}

fn with_count<R>(path: &Path, init: impl FnOnce() -> u64, write: impl FnOnce(u64) -> std::io::Result<R>) -> std::io::Result<R> {
    let key = path_key(path);
    let mut counts = COUNTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let at = match counts.iter().position(|(known, _)| *known == key) {
        Some(at) => at,
        None => {
            counts.push((key, init()));
            counts.len() - 1
        }
    };
    let done = write(counts[at].1)?;
    counts[at].1 += 1;
    Ok(done)
}

fn set_count(path: &Path, lines: u64) {
    let key = path_key(path);
    let mut counts = COUNTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match counts.iter_mut().find(|(known, _)| *known == key) {
        Some(entry) => entry.1 = lines,
        None => counts.push((key, lines)),
    }
}

/// How many lines a file has, a cut one at its end included.
fn lines_in(path: &Path) -> u64 {
    File::open(path).map_or(0, |file| BufReader::new(file).split(b'\n').map_while(Result::ok).count() as u64)
}

const HELPER_MARK: &str = ".helper-";

/// The conversation of the `number`th `explore` helper of a session, beside it: `<session>.helper-<number>.jsonl`. It is written as a session is, by the same writer, so what the
/// conversation key does for a session it does for this; it goes with the session when that is deleted (`remove_helpers`) and is carried by a branch of it (`fork`).
pub fn helper_file_of(parent: &Path, number: u32) -> PathBuf {
    parent.with_extension(format!("helper-{number}.jsonl"))
}

/// Which helper of its session a file is; none for any other file.
fn helper_number(path: &Path) -> Option<u32> {
    let stem = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
    let (_, number) = stem.rsplit_once(HELPER_MARK)?;
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}

/// Whether a file of the sessions' folder is a helper's conversation and not a session of its own.
pub fn is_helper(path: &Path) -> bool {
    helper_number(path).is_some()
}

/// The helpers' files of a session, by number.
pub fn helpers_of(parent: &Path) -> Vec<(u32, PathBuf)> {
    let (Some(folder), Some(stem)) = (parent.parent(), parent.file_stem().and_then(|stem| stem.to_str())) else { return Vec::new() };
    let prefix = format!("{stem}{HELPER_MARK}");
    let mut found: Vec<(u32, PathBuf)> = fs::read_dir(folder).into_iter().flatten().flatten().map(|entry| entry.path())
        .filter(|path| path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with(&prefix)))
        .filter_map(|path| helper_number(&path).map(|number| (number, path))).collect();
    found.sort();
    found
}

/// The helper file the `number`th helper of `parent` has, if it is there.
pub fn helper_path(parent: &Path, number: u32) -> Option<PathBuf> {
    Some(helper_file_of(parent, number)).filter(|path| path.is_file())
}

/// With a session that is deleted: its helpers' conversations go.
pub fn remove_helpers(parent: &Path) {
    for (_, path) in helpers_of(parent) {
        let _ = fs::remove_file(path);
    }
}

/// A workspace's sessions, the newest first, and the workspaces with the most recent session first.
pub fn by_workspace(sessions: Vec<Summary>) -> Vec<(PathBuf, Vec<Summary>)> {
    let key = |path: &Path| path.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let mut groups: Vec<(String, PathBuf, Vec<Summary>)> = Vec::new();
    for summary in sessions {
        let name = key(&summary.workspace);
        match groups.iter_mut().find(|group| group.0 == name) {
            Some(group) => group.2.push(summary),
            None => groups.push((name, summary.workspace.clone(), vec![summary])),
        }
    }
    groups.into_iter().map(|(_, workspace, sessions)| (workspace, sessions)).collect()
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub path: PathBuf,
    pub title: String,
    pub workspace: PathBuf,
    pub created: String,
    pub messages: usize,
    /// Why it cannot be opened (an encrypted session whose key is not here): then `title` says so.
    pub unreadable: Option<String>,
}

/// What a session's file holds, line by line: what the line says, or that it could not be read.
pub(crate) enum Entry {
    Text(String),
    Damaged,
}

/// The lines of a session file with their place in it (from 0), a plain file's as they are and an encrypted file's opened, whatever a crash cut: a line cut in the middle of a character
/// is one line, and those after it are read too. A line that does not open is `Damaged`, and is never left out of the count.
pub(crate) struct Entries {
    lines: std::io::Split<BufReader<File>>,
    sealed: Option<Sealed>,
    held: Option<Vec<u8>>,
    next: u64,
}

pub(crate) fn entries_of(path: &Path) -> std::io::Result<Entries> {
    // (a line of a session is tens of kilobytes: read in blocks that hold a few)
    let mut lines = BufReader::with_capacity(1 << 16, File::open(path)?).split(b'\n');
    let first = lines.next().and_then(Result::ok);
    let sealed = match &first {
        Some(bytes) => sealed_from(bytes)?,
        None => None,
    };
    let held = if sealed.is_some() { None } else { first };
    Ok(Entries { lines, sealed, held, next: 1 })
}

impl Iterator for Entries {
    type Item = (u64, Entry);

    fn next(&mut self) -> Option<(u64, Entry)> {
        let (number, bytes) = match self.held.take() {
            Some(bytes) => (0, bytes),
            None => {
                let bytes = self.lines.next()?.ok()?;
                self.next += 1;
                (self.next - 1, bytes)
            }
        };
        Some((number, match &self.sealed {
            None => Entry::Text(String::from_utf8_lossy(&bytes).trim_end_matches('\r').to_string()),
            Some(sealed) => sealed.open_line(number, &bytes).map_or(Entry::Damaged, Entry::Text),
        }))
    }
}

/// A session file's lines, each as far as it reads: a line a crash cut in the middle of a character is one line, and those after it are
/// read too. (As text, reading stopped at it, and what came after (a name, the mark of web content) went unseen.) A line of an encrypted file that does not open is not here.
fn lines_of(path: &Path) -> std::io::Result<impl Iterator<Item = String>> {
    Ok(entries_of(path)?.filter_map(|(_, entry)| match entry {
        Entry::Text(text) => Some(text),
        Entry::Damaged => None,
    }))
}

/// What a session file comes to when it is read whole.
struct Loaded {
    workspace: PathBuf,
    messages: Vec<Message>,
    /// The lines that did not open, by their number in the file from 1 (an encrypted file's only).
    damaged: Vec<usize>,
    /// How many lines the file has, a cut one at its end included.
    lines: u64,
}

fn load(path: &Path) -> Result<Loaded, String> {
    let entries = entries_of(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let (mut workspace, mut messages, mut damaged, mut lines) = (PathBuf::new(), Vec::new(), Vec::new(), 0);
    for (number, entry) in entries {
        lines = number + 1;
        match entry {
            Entry::Damaged => damaged.push(number as usize + 1),
            Entry::Text(line) => match serde_json::from_str(&line) {
                Ok(Line::Meta { workspace: place, .. }) => workspace = place,
                Ok(Line::Message { message }) => messages.push(message),
                Ok(Line::Snapshot { messages: all }) => messages = all,
                Ok(Line::Title { .. } | Line::WebSeen | Line::Tools { .. } | Line::Goal { .. }) => {}
                Err(_) => {} // a line cut short by a crash, or of a kind that is not known
            },
        }
    }
    Ok(Loaded { workspace, messages, damaged, lines })
}

/// What is said when lines of a conversation could not be read: followed by their numbers.
pub const DAMAGE_NOTICE: &str = "some lines of this conversation could not be read and were left out: lines ";

/// The lines that could not be read, as a person is told of them.
pub fn damage_words(damaged: &[usize]) -> String {
    let shown: Vec<String> = damaged.iter().take(12).map(usize::to_string).collect();
    format!("{DAMAGE_NOTICE}{}{}", shown.join(", "), if damaged.len() > shown.len() { format!(" (+{})", damaged.len() - shown.len()) } else { String::new() })
}

/// A session that is open: where it is, what it has said, and the lines that did not open.
pub struct Opened {
    pub session: Session,
    pub workspace: PathBuf,
    pub messages: Vec<Message>,
    pub damaged: Vec<usize>,
}

impl Session {
    pub fn create(folder: &Path, workspace: &Path, now: &str, stamp: &str) -> Result<Session, String> {
        Session::create_with(folder, workspace, now, stamp, None)
    }

    /// A new session, encrypted when `policy` says so; while it says that nothing is to be written (the key is not unlocked), none.
    pub fn create_under(policy: &Policy, folder: &Path, workspace: &Path, now: &str, stamp: &str) -> Result<Session, String> {
        let writer = policy.writer()?;
        Session::create_with(folder, workspace, now, stamp, writer.as_ref())
    }

    /// A new session, under this key if there is one.
    pub fn create_with(folder: &Path, workspace: &Path, now: &str, stamp: &str, writer: Option<&Arc<Cipher>>) -> Result<Session, String> {
        fs::create_dir_all(folder).map_err(|error| format!("{}: {error}", folder.display()))?;
        // (two chats begun in one second must not share a file: their lines would make one conversation of the two)
        let mut path = folder.join(format!("{stamp}.jsonl"));
        let mut again = 1;
        let file = loop {
            match OpenOptions::new().create_new(true).append(true).open(&path) {
                Ok(file) => break file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    again += 1;
                    path = folder.join(format!("{stamp}-{again}.jsonl"));
                }
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
        };
        Session::begin(path, file, workspace, now, writer)
    }

    /// A file just made, with what every session starts with: for an encrypted one its plain first line, then its first line of meta.
    fn begin(path: PathBuf, file: File, workspace: &Path, now: &str, writer: Option<&Arc<Cipher>>) -> Result<Session, String> {
        let sealed = writer.map(Sealed::new).transpose()?;
        let mut session = Session { path, file, sealed, generation: GENERATION.load(Ordering::SeqCst) };
        if let Some(sealed) = &session.sealed {
            // (a line of its own, plain, and in one write like the others; the lines that follow are numbered from 1)
            let header = format!("{}\n", sealed.header());
            session.file.write_all(header.as_bytes()).map_err(|error| format!("{}: {error}", session.path.display()))?;
            set_count(&session.path, 1);
        }
        session.write(&Line::Meta { workspace: workspace.to_path_buf(), created: now.to_string() })?;
        Ok(session)
    }

    /// The conversation of an `explore` helper, as a session of its own beside `parent` (`helper_file_of`), under the next number that is free; encrypted when `policy` says so, as a
    /// session is. The number is returned with it.
    pub fn create_helper(policy: &Policy, parent: &Path, workspace: &Path, now: &str) -> Result<(Session, u32), String> {
        let writer = policy.writer()?;
        let mut number = helpers_of(parent).last().map_or(1, |(last, _)| last + 1);
        loop {
            let path = helper_file_of(parent, number);
            match OpenOptions::new().create_new(true).append(true).open(&path) {
                Ok(file) => return Ok((Session::begin(path, file, workspace, now, writer.as_ref())?, number)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => number += 1,
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
        }
    }

    /// The session's messages, and it ready to append to. A tool call left without its result (the app was closed
    /// during the call) gets one, as the server refuses a conversation without.
    pub fn open(path: &Path) -> Result<(Session, PathBuf, Vec<Message>), String> {
        let opened = Session::open_checked(path)?;
        Ok((opened.session, opened.workspace, opened.messages))
    }

    /// As `open`, and with the lines of an encrypted session that did not open: it opens without them, and says so.
    pub fn open_checked(path: &Path) -> Result<Opened, String> {
        // (whatever a crash cut in two, a character too, and what is written next starts a line of its own)
        let loaded = load(path)?;
        let mut messages = loaded.messages;
        complete_tool_calls(&mut messages);
        let sealed = sealed_of(path).map_err(|error| format!("{}: {error}", path.display()))?;
        let mut file = OpenOptions::new().append(true).open(path).map_err(|error| format!("{}: {error}", path.display()))?;
        if !ends_a_line(path) {
            file.write_all(b"\n").map_err(|error| format!("{}: {error}", path.display()))?;
        }
        if sealed.is_some() {
            set_count(path, loaded.lines);
        }
        let session = Session { path: path.to_path_buf(), file, sealed, generation: GENERATION.load(Ordering::SeqCst) };
        Ok(Opened { session, workspace: loaded.workspace, messages, damaged: loaded.damaged })
    }

    /// The file is another one by now (a conversion of every session put a new file in its place): the same name, opened again.
    fn reopen(&mut self) -> Result<(), String> {
        let place = |error: std::io::Error| format!("{}: {error}", self.path.display());
        self.generation = GENERATION.load(Ordering::SeqCst);
        self.file = OpenOptions::new().append(true).open(&self.path).map_err(place)?;
        self.sealed = sealed_of(&self.path).map_err(place)?;
        Ok(())
    }

    /// One line, in one write: a crash then cuts at most this line.
    fn write_json(&mut self, json: &str) -> Result<(), String> {
        if GENERATION.load(Ordering::SeqCst) != self.generation {
            self.reopen()?;
        }
        let written = match &self.sealed {
            None => {
                let mut text = json.to_string();
                text.push('\n');
                self.file.write_all(text.as_bytes())
            }
            Some(sealed) => {
                let file = &mut self.file;
                with_count(&self.path, || lines_in(&self.path), |number| {
                    let mut text = sealed.line(number, json);
                    text.push('\n');
                    file.write_all(text.as_bytes())
                })
            }
        };
        written.map_err(|error| format!("{}: {error}", self.path.display()))
    }

    fn write(&mut self, line: &Line) -> Result<(), String> {
        let json = serde_json::to_string(line).map_err(|error| error.to_string())?;
        self.write_json(&json)
    }

    pub fn append(&mut self, message: &Message) -> Result<(), String> {
        self.write(&Line::Message { message: message.clone() })
    }

    pub fn mark_web_seen(&mut self) -> Result<(), String> {
        self.write(&Line::WebSeen)
    }

    pub fn set_goal(&mut self, goal: Option<&str>) -> Result<(), String> {
        self.write(&Line::Goal { goal: goal.map(str::to_string) })
    }

    pub fn set_tools(&mut self, tools: &serde_json::Value) -> Result<(), String> {
        self.write(&Line::Tools { tools: tools.clone() })
    }

    pub fn snapshot(&mut self, messages: &[Message]) -> Result<(), String> {
        self.write(&Line::Snapshot { messages: messages.to_vec() })
    }
}

/// Whether the file ends a line (or is empty): what is written after a cut line starts one of its own.
fn ends_a_line(path: &Path) -> bool {
    std::fs::File::open(path).and_then(|mut file| {
        use std::io::{Seek, SeekFrom};
        if file.seek(SeekFrom::End(0))? == 0 {
            return Ok(true);
        }
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last)?;
        Ok(last[0] == b'\n')
    }).unwrap_or(true)
}

pub fn web_seen(path: &Path) -> bool {
    let marked = serde_json::to_string(&Line::WebSeen).expect("a plain line serializes");
    lines_of(path).is_ok_and(|mut lines| lines.any(|line| line == marked))
}

/// What a session works toward, as the person last set it; none when it was never set or was cleared.
pub fn goal_of(path: &Path) -> Option<String> {
    lines_of(path).ok()?.filter(|line| line.starts_with("{\"kind\":\"goal\"")).filter_map(|line| match serde_json::from_str(&line) {
        Ok(Line::Goal { goal }) => Some(goal),
        _ => None,
    }).last().flatten()
}

/// The tools a session was last offered; none in a file from before they were kept.
pub fn tools_of(path: &Path) -> Option<serde_json::Value> {
    lines_of(path).ok()?.filter(|line| line.starts_with("{\"kind\":\"tools\"")).filter_map(|line| match serde_json::from_str(&line) {
        Ok(Line::Tools { tools }) => Some(tools),
        _ => None,
    }).last()
}

/// What can be said in a line of a list of tools, so that a person can tell which a session was given: a reopened one keeps its own, whatever this build offers.
pub fn describe_tools(tools: &serde_json::Value) -> String {
    use sha2::Digest;
    let version: String = sha2::Sha256::digest(tools.to_string().as_bytes()).iter().take(6).map(|byte| format!("{byte:02x}")).collect();
    let list = tools.as_array().map_or(&[][..], Vec::as_slice);
    let command = list.iter().find(|tool| tool["function"]["name"] == "execute_command");
    let network = command.is_some_and(|tool| tool["function"]["parameters"]["properties"].get("network").is_some());
    format!("{version}, {} tools, network parameter: {}", list.len(), if network { "yes" } else { "no" })
}

/// A session's messages as they are on disk, for a reader beside the agent that writes them: a call still running has
/// no result yet, and none is made up.
pub fn read(path: &Path) -> Result<Vec<Message>, String> {
    Ok(load(path)?.messages)
}

/// As `read`, and with the lines of an encrypted session that did not open, by their number in the file from 1.
pub fn read_checked(path: &Path) -> Result<(Vec<Message>, Vec<usize>), String> {
    let loaded = load(path)?;
    Ok((loaded.messages, loaded.damaged))
}

/// A session that has what was searched for, with the words around its first place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub path: PathBuf,
    pub title: String,
    pub snippet: String,
}

const SNIPPET_BEFORE: usize = 24;
const SNIPPET_AFTER: usize = 60;

fn snippet(text: &str, at: usize) -> String {
    let start: usize = text[..at].char_indices().rev().nth(SNIPPET_BEFORE - 1).map_or(0, |(position, _)| position);
    let shown: String = text[start..].chars().take(SNIPPET_BEFORE + SNIPPET_AFTER).map(|c| if c.is_whitespace() { ' ' } else { c }).collect();
    if start > 0 { format!("…{}", shown.trim_start()) } else { shown.trim_start().to_string() }
}

/// Where a word is, by the original text's bytes, whatever its case.
fn place_of(text: &str, word: &[char]) -> Option<usize> {
    text.char_indices().map(|(at, _)| at).find(|at| {
        let mut rest = text[*at..].chars().flat_map(char::to_lowercase);
        word.iter().all(|wanted| rest.next() == Some(*wanted))
    })
}

/// The sessions with every word of `query` in their name or in what was said (not in the tools' output, and not in the
/// system prompt, which every session has), the newest first.
pub fn search(folder: &Path, query: &str) -> Vec<Hit> {
    let words: Vec<Vec<char>> = query.split_whitespace().map(|word| word.chars().flat_map(char::to_lowercase).collect()).collect();
    if words.is_empty() {
        return Vec::new();
    }
    list(folder).into_iter().filter_map(|summary| {
        let mut said = vec![summary.title.clone()];
        for message in read(&summary.path).ok()? {
            match (message.role.as_str(), message.content) {
                ("user", Some(text)) if crate::prompt::is_nudge(&text) => {}
                ("user", Some(text)) => said.push(typed_text(&text).to_string()),
                ("assistant", Some(text)) => said.push(text),
                _ => {}
            }
        }
        let mut shown = None;
        for word in &words {
            let (at, text) = said.iter().find_map(|text| place_of(text, word).map(|at| (at, text)))?;
            shown.get_or_insert_with(|| snippet(text, at));
        }
        Some(Hit { path: summary.path, title: summary.title, snippet: shown.unwrap_or_default() })
    }).collect()
}

/// A name for a session, as the person chose it.
pub fn rename(path: &Path, title: &str) -> Result<(), String> {
    let text = serde_json::to_string(&Line::Title { title: title.to_string() }).map_err(|error| error.to_string())?;
    let place = |error: std::io::Error| format!("{}: {error}", path.display());
    let sealed = sealed_of(path).map_err(place)?;
    // (after a line a crash cut short, on a line of its own: run on into it, the name was part of a line nothing reads)
    let ends_a_line = ends_a_line(path);
    let mut file = OpenOptions::new().append(true).open(path).map_err(place)?;
    match sealed {
        None => writeln!(file, "{}{text}", if ends_a_line { "" } else { "\n" }).map_err(place),
        Some(sealed) => with_count(path, || lines_in(path), |number| {
            // (a cut line at the end counts as the line it is: the line break after it ends it)
            let mut line = if ends_a_line { String::new() } else { "\n".to_string() };
            line.push_str(&sealed.line(number, &text));
            line.push('\n');
            file.write_all(line.as_bytes())
        }).map_err(place),
    }
}

/// A new session beside this one, with its conversation through its `through`th message (from 0): to go on from there another way, as
/// DeepSeek Harness's fork does. It has the workspace, the tools this one was offered, its mark of web content, and the pictures of the
/// messages it keeps; `title`, when given, is its name. This one is only read. What changed in the workspace since is not undone: a branch
/// of the conversation, not of the files. A branch of an encrypted session is encrypted, under the same key, with an id of its own; one of a conversation with lines that did not open is
/// refused, as it would leave them out without a word.
pub fn fork(path: &Path, through: usize, title: Option<&str>) -> Result<PathBuf, String> {
    // (as `open` reads it: a line a crash cut in the middle of a character is one line lost, not the whole session refused)
    let Loaded { workspace, mut messages, damaged, .. } = load(path)?;
    if !damaged.is_empty() {
        return Err(format!("{}: {}", path.display(), damage_words(&damaged)));
    }
    if through >= messages.len() {
        return Err(format!("{}: it has {} messages, not {}", path.display(), messages.len(), through + 1));
    }
    messages.truncate(through + 1);
    complete_tool_calls(&mut messages);
    let folder = path.parent().ok_or_else(|| format!("{}: no folder", path.display()))?;
    let now = chrono::Local::now();
    let writer = sealed_of(path).map_err(|error| format!("{}: {error}", path.display()))?.map(|sealed| sealed.cipher);
    let mut copy = Session::create_with(folder, &workspace, &now.format("%Y-%m-%d %H:%M:%S %:z").to_string(), &now.format("%Y%m%d-%H%M%S").to_string(), writer.as_ref())?;
    if let Some(tools) = tools_of(path) {
        copy.set_tools(&tools)?;
    }
    if web_seen(path) {
        copy.mark_web_seen()?;
    }
    for message in &messages {
        copy.append(message)?;
    }
    if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
        copy.write(&Line::Title { title: title.to_string() })?;
    }
    // The helpers the kept messages came from go with them, under the branch's own name and the numbers they had (the messages say them): copied as they are, line for line, so that
    // an encrypted one needs no key to be copied and a line that did not open is not lost in the copy.
    for number in messages.iter().filter_map(|message| message.helper.as_ref().map(|helper| helper.number)) {
        if let Some(from) = helper_path(path, number) {
            fs::copy(&from, helper_file_of(&copy.path, number)).map_err(|error| format!("{}: {error}", from.display()))?;
        }
    }
    let from = crate::images::folder_of(path);
    for image in messages.iter().flat_map(|message| &message.images) {
        // (one gone from the original is gone from the branch too: it says so where it would show)
        if let Ok(bytes) = crate::images::read_stored(&from.join(&image.file)) {
            crate::images::store(&copy.path, &image.file, &bytes)?;
        }
    }
    Ok(copy.path)
}

/// Stub results for calls that have none, right after their assistant message; and no result for a call that is not there (its line
/// was what a crash cut), which a server refuses as surely.
pub fn complete_tool_calls(messages: &mut Vec<Message>) {
    let mut at = 0;
    while at < messages.len() {
        let calls: Vec<String> = messages[at].tool_calls.iter().flatten().map(|call| call.id.clone()).collect();
        if messages[at].role != "tool" {
            at += 1;
        }
        let mut answered = Vec::new();
        while at < messages.len() && messages[at].role == "tool" {
            match messages[at].tool_call_id.clone().filter(|id| calls.contains(id) && !answered.contains(id)) {
                Some(id) => {
                    answered.push(id);
                    at += 1;
                }
                None => {
                    messages.remove(at);
                }
            }
        }
        for id in calls.iter().filter(|id| !answered.contains(id)) {
            messages.insert(at, Message::tool_result(id, "interrupted: this call did not finish"));
            at += 1;
        }
    }
}

/// The sessions in a folder, the newest first. An encrypted one whose key is not here is listed too, saying so: it is not left out of the list as if it were gone.
pub fn list(folder: &Path) -> Vec<Summary> {
    let mut paths: Vec<PathBuf> = fs::read_dir(folder).into_iter().flatten().flatten().map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl") && !crate::trajectory::is_trajectory(path) && !is_helper(path)).collect();
    paths.sort();
    paths.reverse();
    paths.into_iter().filter_map(|path| {
        let (mut workspace, mut created, mut title, mut messages, mut named) = (PathBuf::new(), String::new(), String::new(), 0, None);
        let entries = match entries_of(&path) {
            Ok(entries) => entries,
            Err(error) if is_unreadable(&error) => {
                return Some(Summary { title: error.to_string(), path, workspace, created, messages: 0, unreadable: Some(error.to_string()) });
            }
            Err(_) => return None,
        };
        for line in entries.filter_map(|(_, entry)| match entry {
            Entry::Text(text) => Some(text),
            Entry::Damaged => None,
        }) {
            // Once it has its title, a message is only counted: its text may be megabytes.
            if line.starts_with("{\"kind\":\"title\"") {
                if let Ok(Line::Title { title }) = serde_json::from_str(&line) {
                    named = Some(title);
                }
                continue;
            }
            if !title.is_empty() {
                messages += usize::from(line.starts_with("{\"kind\":\"message\""));
                continue;
            }
            match serde_json::from_str(&line) {
                Ok(Line::Meta { workspace: place, created: at }) => (workspace, created) = (place, at),
                Ok(Line::Message { message }) => {
                    messages += 1;
                    if title.is_empty() && message.role == "user" {
                        title = message.content.as_deref().map(|text| typed_text(text).lines().next().unwrap_or("").chars().take(70).collect()).unwrap_or_default();
                    }
                }
                _ => {}
            }
        }
        (messages > 0).then_some(Summary { path, title: named.unwrap_or(title), workspace, created, messages, unreadable: None })
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{FunctionCall, ToolCall};

    fn call(id: &str) -> ToolCall {
        ToolCall { id: id.into(), kind: "function".into(), function: FunctionCall { name: "read_file".into(), arguments: "{}".into() } }
    }

    #[test]
    fn a_branch_has_the_conversation_through_its_message_and_its_pictures_and_the_original_is_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("C:/w"), "now", "20261103-1").unwrap();
        session.set_tools(&serde_json::json!([{"function": {"name": "read_file"}}])).unwrap();
        session.append(&Message::text("system", "s")).unwrap();
        let mut asked = Message::text("user", "look at this");
        asked.images.push(crate::message::ImageRef { file: "a.png".into(), media_type: "image/png".into(), width: 1, height: 1 });
        session.append(&asked).unwrap();
        session.append(&Message::text("assistant", "first answer")).unwrap();
        session.append(&Message::text("user", "then this")).unwrap();
        session.append(&Message::text("assistant", "second answer")).unwrap();
        let path = session.path.clone();
        drop(session);
        fs::create_dir_all(crate::images::folder_of(&path)).unwrap();
        fs::write(crate::images::folder_of(&path).join("a.png"), b"png").unwrap();
        let before = fs::read(&path).unwrap();
        let branch = fork(&path, 2, Some("a title (branch)")).unwrap();
        assert_ne!(branch, path);
        assert_eq!(fs::read(&path).unwrap(), before, "the original is only read");
        let (_, workspace, messages) = Session::open(&branch).unwrap();
        assert_eq!(workspace, Path::new("C:/w"));
        assert_eq!(messages.iter().map(|m| m.content.as_deref().unwrap()).collect::<Vec<_>>(), ["s", "look at this", "first answer"]);
        assert_eq!(fs::read(crate::images::folder_of(&branch).join("a.png")).unwrap(), b"png");
        assert!(tools_of(&branch).is_some());
        let listed = list(dir.path());
        assert!(listed.iter().any(|summary| summary.path == branch && summary.title == "a title (branch)"), "{listed:?}");
        assert!(fork(&path, 9, None).is_err());
    }

    #[test]
    fn a_session_comes_back_as_it_was_after_its_last_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("C:/w"), "now", "20261031-1").unwrap();
        session.append(&Message::text("system", "s")).unwrap();
        session.append(&Message::text("user", "old")).unwrap();
        session.snapshot(&[Message::text("system", "s"), Message::text("assistant", "sum")]).unwrap();
        session.append(&Message::text("user", "new")).unwrap();
        let path = session.path.clone();
        drop(session);
        let (mut again, workspace, messages) = Session::open(&path).unwrap();
        assert_eq!(workspace, Path::new("C:/w"));
        assert_eq!(messages.iter().map(|m| m.content.as_deref().unwrap()).collect::<Vec<_>>(), ["s", "sum", "new"]);
        again.append(&Message::text("assistant", "more")).unwrap();
        assert_eq!(Session::open(&path).unwrap().2.len(), 4);
    }

    /// A name given after a line a crash cut short is on a line of its own, and read; a session with such a line can still be branched.
    #[test]
    fn a_cut_line_takes_neither_the_name_after_it_nor_a_branch() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("C:/w"), "now", "20261104-1").unwrap();
        session.append(&Message::text("user", "hello")).unwrap();
        session.append(&Message::text("assistant", "hi")).unwrap();
        let path = session.path.clone();
        drop(session);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"kind\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":\"\xE3\x81").unwrap();
        drop(file);
        rename(&path, "named after the cut").unwrap();
        assert_eq!(list(dir.path())[0].title, "named after the cut");
        let copy = fork(&path, 1, None).unwrap();
        assert_eq!(Session::open(&copy).unwrap().2.len(), 2);
    }

    /// A call without its result gets one; a result whose call a crash cut out of the file, or one given twice, goes.
    #[test]
    fn every_call_has_one_result_and_every_result_its_call() {
        let call = |id: &str| crate::message::ToolCall { id: id.into(), kind: "function".into(), function: crate::message::FunctionCall { name: "read_file".into(), arguments: "{}".into() } };
        let asking = |ids: &[&str]| Message { role: "assistant".into(), tool_calls: Some(ids.iter().map(|id| call(id)).collect()), ..Message::default() };
        let mut messages = vec![
            Message::tool_result("lost", "its call is gone"),
            Message::text("user", "go"),
            Message::tool_result("cut", "its call was the line a crash cut"),
            asking(&["a", "b"]),
            Message::tool_result("a", "one"),
            Message::tool_result("a", "one again"),
            Message::text("user", "next"),
            asking(&["c"]),
        ];
        complete_tool_calls(&mut messages);
        let said: Vec<(String, Option<String>, Option<String>)> = messages.iter().map(|m| (m.role.clone(), m.tool_call_id.clone(), m.content.clone())).collect();
        let tool = |id: &str, text: &str| ("tool".to_string(), Some(id.to_string()), Some(text.to_string()));
        assert_eq!(said, [
            ("user".to_string(), None, Some("go".to_string())),
            ("assistant".to_string(), None, None),
            tool("a", "one"),
            tool("b", "interrupted: this call did not finish"),
            ("user".to_string(), None, Some("next".to_string())),
            ("assistant".to_string(), None, None),
            tool("c", "interrupted: this call did not finish"),
        ]);
    }

    #[test]
    fn a_list_of_tools_is_told_from_another_in_a_line() {
        let with = serde_json::json!([{"function": {"name": "execute_command", "parameters": {"properties": {"network": {}}}}}]);
        let without = serde_json::json!([{"function": {"name": "execute_command", "parameters": {"properties": {}}}}]);
        assert!(describe_tools(&with).ends_with("1 tools, network parameter: yes"), "{}", describe_tools(&with));
        assert!(describe_tools(&without).ends_with("network parameter: no"));
        assert_ne!(describe_tools(&with)[..12], describe_tools(&without)[..12]);
    }

    #[test]
    fn two_sessions_begun_in_one_second_have_a_file_each() {
        let dir = tempfile::tempdir().unwrap();
        let first = Session::create(dir.path(), Path::new("w"), "now", "same").unwrap();
        let second = Session::create(dir.path(), Path::new("w"), "now", "same").unwrap();
        assert_ne!(first.path, second.path);
    }

    #[test]
    fn the_tools_come_back_to_the_byte_and_the_last_ones_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("w"), "now", "1").unwrap();
        assert_eq!(tools_of(&session.path), None);
        let tools = serde_json::json!([{"zebra": 1, "apple": {"é": "日本語\n"}}]);
        session.set_tools(&serde_json::json!([])).unwrap();
        session.append(&Message::text("user", "a")).unwrap();
        session.set_tools(&tools).unwrap();
        session.snapshot(&[Message::text("user", "b")]).unwrap();
        assert_eq!(tools_of(&session.path).unwrap().to_string(), tools.to_string());
        let (_, _, messages) = Session::open(&session.path).unwrap();
        assert_eq!(messages.len(), 1, "a line of tools is no message");
    }

    #[test]
    fn a_call_cut_off_gets_a_result_and_a_cut_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("w"), "now", "a").unwrap();
        session.append(&Message { role: "assistant".into(), tool_calls: Some(vec![call("1"), call("2")]), ..Message::default() }).unwrap();
        session.append(&Message::tool_result("1", "done")).unwrap();
        let path = session.path.clone();
        drop(session);
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("{\"kind\":\"mess");
        fs::write(&path, text).unwrap();
        let messages = Session::open(&path).unwrap().2;
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("2"));
    }

    #[test]
    fn sessions_group_by_workspace_with_the_most_recent_first() {
        let summary = |workspace: &str, title: &str| Summary { path: PathBuf::new(), title: title.into(), workspace: workspace.into(), created: String::new(), messages: 1, unreadable: None };
        let groups = by_workspace(vec![summary("C:\\b", "newest"), summary("C:\\a", "middle"), summary("c:/B\\", "oldest")]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, Path::new("C:\\b"));
        assert_eq!(groups[0].1.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), ["newest", "oldest"]);
        assert_eq!(groups[1].1[0].title, "middle");
    }

    #[test]
    fn a_reader_beside_the_writer_sees_a_running_call_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("w"), "now", "1").unwrap();
        session.append(&Message { role: "assistant".into(), tool_calls: Some(vec![call("1")]), ..Message::default() }).unwrap();
        assert_eq!(read(&session.path).unwrap().len(), 1);
        session.append(&Message::tool_result("1", "done")).unwrap();
        assert_eq!(read(&session.path).unwrap().len(), 2);
    }

    #[test]
    fn a_search_finds_what_was_said_and_not_what_every_session_has() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = Session::create(dir.path(), Path::new("w"), "now", "1").unwrap();
        first.append(&Message::text("system", "the system prompt mentions Banana")).unwrap();
        first.append(&Message::text("user", crate::prompt::user_message("ハッシュテーブルの話", "now"))).unwrap();
        first.append(&Message::text("assistant", "A HashMap keeps Cherry in a bucket")).unwrap();
        first.append(&Message::tool_result("1", "tool output with Plum")).unwrap();
        let mut second = Session::create(dir.path(), Path::new("w"), "now", "2").unwrap();
        second.append(&Message::text("user", "plain words")).unwrap();
        second.append(&Message::text("assistant", "CHERRY again")).unwrap();
        let titles = |query: &str| search(dir.path(), query).into_iter().map(|hit| hit.title).collect::<Vec<_>>();
        assert_eq!(titles("cherry"), ["plain words", "ハッシュテーブルの話"]);
        assert_eq!(titles("テーブル"), ["ハッシュテーブルの話"]);
        assert_eq!(titles("cherry bucket"), ["ハッシュテーブルの話"]);
        assert!(titles("cherry nothing").is_empty());
        assert!(titles("banana").is_empty(), "the system prompt");
        assert!(titles("plum").is_empty(), "a tool's output");
        assert!(titles("active").is_empty(), "the wrapper of a message");
        assert!(titles("   ").is_empty());
        let hit = search(dir.path(), "BUCKET").remove(0);
        assert!(hit.snippet.contains("bucket"), "{}", hit.snippet);
    }

    #[test]
    fn a_snippet_stays_on_character_boundaries_and_on_one_line() {
        let long = format!("{}\n見つけた言葉{}", "前の文章".repeat(30), "後の文章".repeat(30));
        let at = long.find("見つけた").unwrap();
        let shown = snippet(&long, at);
        assert!(shown.starts_with('…') && shown.contains("見つけた") && !shown.contains('\n'), "{shown}");
        assert_eq!(snippet("短い", 0), "短い");
    }

    #[test]
    fn a_new_name_replaces_the_first_words_and_leaves_the_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("w"), "now", "1").unwrap();
        session.append(&Message::text("user", "first words")).unwrap();
        let path = session.path.clone();
        rename(&path, "my name").unwrap();
        session.append(&Message::text("assistant", "a")).unwrap();
        let summary = list(dir.path()).remove(0);
        assert_eq!((summary.title.as_str(), summary.messages), ("my name", 2));
        assert_eq!(Session::open(&path).unwrap().2.len(), 2);
    }

    #[test]
    fn the_list_counts_every_message_after_the_title() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("w"), "now", "1").unwrap();
        session.append(&Message::text("system", "s")).unwrap();
        session.append(&Message::text("user", "title")).unwrap();
        session.append(&Message::text("assistant", "a")).unwrap();
        session.snapshot(&[Message::text("system", "s")]).unwrap();
        session.append(&Message::text("user", "b")).unwrap();
        assert_eq!(list(dir.path())[0].messages, 4);
    }

    #[test]
    fn the_list_is_newest_first_and_titled_by_what_was_typed() {
        let dir = tempfile::tempdir().unwrap();
        for (stamp, text) in [("1", "first"), ("2", "second")] {
            let mut session = Session::create(dir.path(), Path::new("w"), "now", stamp).unwrap();
            session.append(&Message::text("user", crate::prompt::user_message(text, "now"))).unwrap();
        }
        Session::create(dir.path(), Path::new("w"), "now", "3").unwrap();
        let titles: Vec<String> = list(dir.path()).into_iter().map(|summary| summary.title).collect();
        assert_eq!(titles, ["second", "first"]);
    }
}

#[cfg(test)]
mod notice_tests {
    use super::*;
    use crate::message::{Wake, WakeCause};

    fn notice() -> Message {
        let wake = Wake { cause: WakeCause::CommandEnded, id: "job1".into(), what: "cargo build".into(), exit_code: Some(0), ended: None, seconds: 91 };
        Message { notice: Some(vec![wake]), ..Message::text("user", format!("{}Notice from the harness: job1 ended.", crate::prompt::DRAFT_NOTE)) }
    }

    /// A session with a notice in it: it comes back as one, whether it is a line of its own or in a snapshot, it is not the person's words to the list, the search or the model's request.
    #[test]
    fn a_notice_is_kept_as_one_and_is_not_taken_for_the_persons_words() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("w"), "now", "1").unwrap();
        session.append(&Message::text("user", crate::prompt::user_message("build it", "now"))).unwrap();
        session.append(&Message::text("assistant", "started")).unwrap();
        session.append(&notice()).unwrap();
        session.append(&Message::text("assistant", "it passed")).unwrap();
        let path = session.path.clone();
        for messages in [read(&path).unwrap(), Session::open(&path).unwrap().2] {
            assert_eq!(messages.len(), 4);
            assert_eq!(messages[2].notice.as_ref().map(|wakes| wakes[0].id.as_str()), Some("job1"));
            assert!(messages[0].notice.is_none() && messages[3].notice.is_none());
        }
        session.snapshot(&read(&path).unwrap()).unwrap();
        assert!(read(&path).unwrap()[2].notice.is_some(), "a compaction's snapshot keeps it");
        assert_eq!(list(dir.path())[0].title, "build it");
        assert!(search(dir.path(), "harness").is_empty(), "the app's word is not what was said");
        let body = crate::llm::request_body("m", &[notice()], None, None);
        assert!(body["messages"][0].get("notice").is_none() && body["messages"][0]["content"].as_str().unwrap().contains("job1 ended"), "{body}");
    }

    /// What a window from before notices does with such a session: the message is read as the app's word and not the person's, as the one the app's own checks and cut-off answers are;
    /// and a line of a kind it does not know is skipped, as a line a crash cut is, with the rest read.
    #[test]
    fn a_session_with_a_notice_opens_in_a_reader_from_before_them() {
        // (the message as that reader has it: the fields it knows, and no more)
        #[derive(Deserialize)]
        struct Before {
            role: String,
            content: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum LineBefore {
            Message { message: Before },
        }
        let line = serde_json::to_string(&Line::Message { message: notice() }).unwrap();
        let LineBefore::Message { message } = serde_json::from_str(&line).expect("the old reader reads a message that has a notice field");
        assert_eq!(message.role, "user");
        assert!(crate::prompt::is_nudge(message.content.as_deref().unwrap()), "it shows as the app's word, never as the person's bubble");

        // a line of a kind no reader has (what a later app may write): it is skipped by `read`, `open`, `list` and `fork`, and what is around it is read
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), Path::new("w"), "now", "1").unwrap();
        session.append(&Message::text("user", "first")).unwrap();
        let path = session.path.clone();
        drop(session);
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("{\"kind\":\"wake\",\"what\":\"a line of a kind that is not known\"}\n");
        fs::write(&path, text).unwrap();
        let mut again = Session::open(&path).unwrap().0;
        again.append(&Message::text("assistant", "after it")).unwrap();
        assert_eq!(read(&path).unwrap().len(), 2);
        assert_eq!(Session::open(&path).unwrap().2.len(), 2);
        assert_eq!(list(dir.path())[0].messages, 2);
        assert_eq!(Session::open(&fork(&path, 1, None).unwrap()).unwrap().2.len(), 2);
    }
}

#[cfg(test)]
mod crash_tests {
    use super::*;

    #[test]
    fn a_file_cut_by_a_crash_opens_and_goes_on_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cut.jsonl");
        let whole = serde_json::to_string(&Line::Message { message: Message::text("user", "kept") }).unwrap();
        // A line cut in the middle of a character, with no newline after it.
        let mut bytes = format!("{whole}\n{{\"kind\":\"message\",\"message\":{{\"content\":\"").into_bytes();
        bytes.extend_from_slice(&[0xE3, 0x81]);
        std::fs::write(&path, bytes).unwrap();
        let (mut session, _, messages) = Session::open(&path).unwrap();
        assert_eq!(messages.len(), 1);
        session.append(&Message::text("user", "after")).unwrap();
        drop(session);
        let (_, _, messages) = Session::open(&path).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].content.as_deref(), Some("after"));
    }
}

#[cfg(test)]
mod sealed_tests;
