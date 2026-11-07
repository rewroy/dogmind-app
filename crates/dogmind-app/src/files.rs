//! The Files view of the panel on the right: the session's workspace as a tree of folders and files.
//!
//! This is the model, and what reads the disk for it. A folder is read when it is opened, on another thread, and only once; what a folder shows is capped, with a row that says how
//! many more there are, so that a folder of fifty thousand files costs what a folder of three hundred does. The tree changes only through what is told to it (a read that came,
//! files the session changed, folders the watcher saw change), so it is all testable without a window; `ui/files_panel.rs` draws it and takes its keys and pointer.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How many of a folder's entries show at first, and how many more each press on its "more" row adds.
pub const SHOWN_AT_FIRST: usize = 300;
pub const SHOWN_MORE: usize = 300;
/// A folder is read as far as this many entries: past them nothing is wanted of it here.
pub const MOST_READ: usize = 200_000;
/// The folders that are the program's own mess and not the person's work: shown greyed, and their changes are not watched.
pub const NOISE: [&str; 4] = [".git", "node_modules", "target", "__pycache__"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Folder,
    File,
}

/// What a read of a folder finds in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    /// A name that begins with a dot, or a file Windows has marked hidden or as the system's.
    pub hidden: bool,
}

impl Entry {
    /// Shown greyed: hidden, or one of the usual noise.
    #[cfg(test)]
    pub fn is_dim(&self) -> bool {
        self.hidden || is_noise(&self.name, self.kind)
    }
}

/// One of the folders that fill a tree with what nobody looks for there.
pub fn is_noise(name: &str, kind: Kind) -> bool {
    kind == Kind::Folder && NOISE.iter().any(|noise| noise.eq_ignore_ascii_case(name))
}

/// Whether a path under the workspace is inside noise: no change there is told of.
pub fn inside_noise(relative: &str) -> bool {
    relative.split(['/', '\\']).any(|part| NOISE.iter().any(|noise| noise.eq_ignore_ascii_case(part)))
}

/// Names in the order a person reads them: letters without regard to case, and a run of digits by its value, so that `part2` comes before `part10`. Equal in that, by the letters as written.
fn natural(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut left, mut right) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (left.peek().copied(), right.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let (mut one, mut other) = (String::new(), String::new());
                while let Some(digit) = left.next_if(char::is_ascii_digit) {
                    one.push(digit);
                }
                while let Some(digit) = right.next_if(char::is_ascii_digit) {
                    other.push(digit);
                }
                let (one, other) = (one.trim_start_matches('0'), other.trim_start_matches('0'));
                match one.len().cmp(&other.len()).then_with(|| one.cmp(other)) {
                    Ordering::Equal => {}
                    unequal => return unequal,
                }
            }
            (Some(x), Some(y)) => {
                left.next();
                right.next();
                let (x, y) = (x.to_lowercase().next().unwrap_or(x), y.to_lowercase().next().unwrap_or(y));
                match x.cmp(&y) {
                    Ordering::Equal => {}
                    unequal => return unequal,
                }
            }
        }
    }
}

/// Folders first, then files; each in the order a person reads names.
pub fn sort(entries: &mut [Entry]) {
    entries.sort_by(|a, b| (a.kind == Kind::File).cmp(&(b.kind == Kind::File)).then_with(|| natural(&a.name, &b.name)));
}

/// A folder's entries, sorted, or why it could not be read. Only an entry that cannot be told about is left out: a name that is not text is shown as far as it goes.
pub fn read_folder(path: &Path) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    for found in std::fs::read_dir(path).map_err(|error| error.to_string())?.take(MOST_READ) {
        let Ok(found) = found else { continue };
        // (on Windows the listing carries each entry's kind and attributes: nothing is asked of the disk again for each of fifty thousand)
        let Ok(kind) = found.file_type() else { continue };
        let name = found.file_name().to_string_lossy().into_owned();
        let attributes_hide = {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const HIDDEN_OR_SYSTEM: u32 = 0x2 | 0x4;
                found.metadata().is_ok_and(|metadata| metadata.file_attributes() & HIDDEN_OR_SYSTEM != 0)
            }
            #[cfg(not(windows))]
            false
        };
        let hidden = name.starts_with('.') || attributes_hide;
        // (a link to a folder is a folder to look in; the others are files)
        let is_folder = kind.is_dir() || (kind.is_symlink() && std::fs::metadata(found.path()).is_ok_and(|target| target.is_dir()));
        entries.push(Entry { name, kind: if is_folder { Kind::Folder } else { Kind::File }, hidden });
    }
    sort(&mut entries);
    Ok(entries)
}

/// How a changed file is told apart from the others whatever case or slashes the path was written in.
pub fn key_of(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches("./").trim_matches('/').to_lowercase()
}

/// The folder a path is in: `a/b` of `a/b/c`, and nothing of `c`.
pub fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// What a row says of the session's changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mark {
    #[default]
    None,
    /// A file the session changed: the lines it added and removed.
    Changed(usize, usize),
    /// A folder with such a file somewhere in it.
    Inside,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKind {
    Folder { open: bool },
    File,
    /// The folder is being read.
    Reading,
    /// The folder has nothing in it.
    Empty,
    /// The folder could not be read.
    Problem(String),
    /// This many more entries than show: the row of the folder `path`.
    More(usize),
}

/// A row of the tree as it is drawn, and what it stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// From the workspace, with `/` between the parts; a row of the kinds that stand for no file or folder of its own has its folder's.
    pub path: String,
    pub name: String,
    pub depth: usize,
    pub kind: RowKind,
    pub dim: bool,
    pub mark: Mark,
}

impl Row {
    pub fn is_folder(&self) -> bool {
        matches!(self.kind, RowKind::Folder { .. })
    }

    pub fn is_file(&self) -> bool {
        self.kind == RowKind::File
    }
}

#[derive(Debug)]
enum State {
    Unread,
    Reading,
    Read(Vec<Node>),
    Failed(String),
}

#[derive(Debug)]
struct Node {
    name: String,
    kind: Kind,
    hidden: bool,
    state: State,
    open: bool,
    shown: usize,
}

impl Node {
    fn new(name: &str, kind: Kind, hidden: bool) -> Node {
        Node { name: name.to_string(), kind, hidden, state: State::Unread, open: false, shown: SHOWN_AT_FIRST }
    }
}

/// What opening or closing a folder asks of whoever holds the tree.
#[derive(Debug, PartialEq, Eq)]
pub enum Toggled {
    Nothing,
    /// The folder is to be read: this is its place, as rows have it.
    Read(String),
}

/// The workspace as a tree, lazily: only what was opened has been read.
#[derive(Debug)]
pub struct Tree {
    root: Node,
    /// The files the session changed, by `key_of`, with the lines each added and removed.
    changed: HashMap<String, (usize, usize)>,
    /// Every folder that has such a file in it, at any depth, by `key_of`.
    holding: HashSet<String>,
    /// Counts what was done to the tree, so that whoever shows it can tell that nothing was, without asking for its rows.
    version: u64,
}

impl Tree {
    pub fn new() -> Tree {
        let mut root = Node::new("", Kind::Folder, false);
        root.open = true;
        Tree { root, changed: HashMap::new(), holding: HashSet::new(), version: 0 }
    }

    /// Changes with every change to the tree: the same number is the same rows.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Whether the workspace's own folder has still to be read, which is the first thing a tree wants.
    pub fn root_is_unread(&self) -> bool {
        matches!(self.root.state, State::Unread)
    }

    /// The root is being read now: nothing asks for it again meanwhile.
    pub fn root_reading(&mut self) {
        self.version += 1;
        if self.root_is_unread() {
            self.root.state = State::Reading;
        }
    }

    #[cfg(test)]
    fn node(&self, path: &str) -> Option<&Node> {
        let mut at = &self.root;
        for part in path.split('/').filter(|part| !part.is_empty()) {
            let State::Read(children) = &at.state else { return None };
            at = &children[child_named(children, part)?];
        }
        Some(at)
    }

    fn node_mut(&mut self, path: &str) -> Option<&mut Node> {
        let mut at = &mut self.root;
        for part in path.split('/').filter(|part| !part.is_empty()) {
            let State::Read(children) = &mut at.state else { return None };
            let found = child_named(children, part)?;
            at = &mut children[found];
        }
        Some(at)
    }

    /// Opens a closed folder and closes an open one. One not read yet is to be.
    pub fn toggle(&mut self, path: &str) -> Toggled {
        self.version += 1;
        let Some(node) = self.node_mut(path) else { return Toggled::Nothing };
        if node.kind != Kind::Folder {
            return Toggled::Nothing;
        }
        node.open = !node.open;
        if node.open && matches!(node.state, State::Unread | State::Failed(_)) {
            node.state = State::Reading;
            return Toggled::Read(path.to_string());
        }
        Toggled::Nothing
    }

    /// A folder's read has come. What was in it that still is keeps what the person did to it (open, how much of it shows); the rest is new.
    pub fn read_came(&mut self, path: &str, result: Result<Vec<Entry>, String>) {
        self.version += 1;
        let Some(node) = self.node_mut(path) else { return };
        match result {
            Err(why) => node.state = State::Failed(why),
            Ok(mut entries) => {
                // (sorted already where it was read: a run that is in order costs nothing to sort)
                sort(&mut entries);
                let mut before: HashMap<(String, bool), Node> = match std::mem::replace(&mut node.state, State::Unread) {
                    State::Read(children) => children.into_iter().map(|child| ((child.name.clone(), child.kind == Kind::Folder), child)).collect(),
                    _ => HashMap::new(),
                };
                let children = entries
                    .iter()
                    .map(|entry| match before.remove(&(entry.name.clone(), entry.kind == Kind::Folder)) {
                        Some(mut kept) => {
                            kept.hidden = entry.hidden;
                            kept
                        }
                        None => Node::new(&entry.name, entry.kind, entry.hidden),
                    })
                    .collect();
                node.state = State::Read(children);
            }
        }
    }

    /// Shows the next page of a folder's entries.
    pub fn more(&mut self, path: &str) {
        self.version += 1;
        if let Some(node) = self.node_mut(path) {
            node.shown = node.shown.saturating_add(SHOWN_MORE);
        }
    }

    /// Folders the disk says changed, by their places: the ones to be read again. A folder that is open is read now; one that is closed, and was read, is left to be read when it is opened.
    /// `everything`: the watcher lost count, and all that was read is read again.
    pub fn changed_on_disk(&mut self, folders: &[String], everything: bool) -> Vec<String> {
        self.version += 1;
        let mut again = Vec::new();
        fn walk(node: &mut Node, path: &str, wanted: &dyn Fn(&str) -> bool, again: &mut Vec<String>) {
            let State::Read(children) = &mut node.state else { return };
            if wanted(path) {
                if node.open || path.is_empty() {
                    again.push(path.to_string());
                } else {
                    node.state = State::Unread;
                    return;
                }
            }
            for child in children.iter_mut().filter(|child| child.kind == Kind::Folder) {
                let below = if path.is_empty() { child.name.clone() } else { format!("{path}/{}", child.name) };
                walk(child, &below, wanted, again);
            }
        }
        let touched: HashSet<String> = folders.iter().map(|folder| key_of(folder)).collect();
        walk(&mut self.root, "", &|path: &str| everything || touched.contains(&key_of(path)), &mut again);
        again
    }

    /// The files the session changed, with the lines each added and removed (a file changed in more than one answer, in all of them).
    pub fn set_changes<'a>(&mut self, files: impl IntoIterator<Item = (&'a str, usize, usize)>) {
        self.version += 1;
        self.changed.clear();
        self.holding.clear();
        for (path, added, removed) in files {
            let key = key_of(path);
            if key.is_empty() {
                continue;
            }
            let total = self.changed.entry(key.clone()).or_insert((0, 0));
            total.0 += added;
            total.1 += removed;
            let mut folder = parent_of(&key);
            loop {
                self.holding.insert(folder.to_string());
                if folder.is_empty() {
                    break;
                }
                folder = parent_of(folder);
            }
        }
    }

    pub fn mark_of(&self, path: &str, kind: Kind) -> Mark {
        let key = key_of(path);
        match kind {
            Kind::File => self.changed.get(&key).map_or(Mark::None, |(added, removed)| Mark::Changed(*added, *removed)),
            Kind::Folder if self.holding.contains(&key) => Mark::Inside,
            Kind::Folder => Mark::None,
        }
    }

    /// The rows now: the workspace's entries, and under each folder that is open what was read of it, down to the depth that is open.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        self.add_rows(&self.root, "", 0, &mut rows);
        rows
    }

    fn add_rows(&self, folder: &Node, path: &str, depth: usize, rows: &mut Vec<Row>) {
        match &folder.state {
            State::Unread | State::Reading => {
                if depth > 0 || folder.open {
                    rows.push(Row { path: path.to_string(), name: String::new(), depth, kind: RowKind::Reading, dim: true, mark: Mark::None });
                }
            }
            State::Failed(why) => rows.push(Row { path: path.to_string(), name: String::new(), depth, kind: RowKind::Problem(why.clone()), dim: true, mark: Mark::None }),
            State::Read(children) if children.is_empty() && depth > 0 => rows.push(Row { path: path.to_string(), name: String::new(), depth, kind: RowKind::Empty, dim: true, mark: Mark::None }),
            State::Read(children) => {
                for child in children.iter().take(folder.shown) {
                    let below = if path.is_empty() { child.name.clone() } else { format!("{path}/{}", child.name) };
                    let dim = child.hidden || is_noise(&child.name, child.kind);
                    let mark = self.mark_of(&below, child.kind);
                    let kind = if child.kind == Kind::Folder { RowKind::Folder { open: child.open } } else { RowKind::File };
                    rows.push(Row { path: below.clone(), name: child.name.clone(), depth, kind, dim, mark });
                    if child.kind == Kind::Folder && child.open {
                        self.add_rows(child, &below, depth + 1, rows);
                    }
                }
                if children.len() > folder.shown {
                    rows.push(Row { path: path.to_string(), name: String::new(), depth, kind: RowKind::More(children.len() - folder.shown), dim: true, mark: Mark::None });
                }
            }
        }
    }

    /// Whether this path is a folder that is open.
    #[cfg(test)]
    pub fn is_open(&self, path: &str) -> bool {
        self.node(path).is_some_and(|node| node.kind == Kind::Folder && node.open)
    }
}

/// The child with this name: as written, and failing that in any case (the paths of the agent are not always in the case the disk has).
fn child_named(children: &[Node], name: &str) -> Option<usize> {
    children.iter().position(|child| child.name == name).or_else(|| children.iter().position(|child| child.name.eq_ignore_ascii_case(name)))
}

impl Default for Tree {
    fn default() -> Tree {
        Tree::new()
    }
}

/// The row that is the parent folder of row `at`: the nearest one above it that is less deep.
pub fn parent_row(rows: &[Row], at: usize) -> Option<usize> {
    let depth = rows.get(at)?.depth;
    if depth == 0 {
        return None;
    }
    (0..at).rev().find(|&above| rows[above].depth < depth && rows[above].is_folder())
}

/// Collects what the watcher sees until it has been quiet for a moment, so that a build that writes a thousand files is one change and not a thousand. Pure: the clock is given to it.
pub struct Debounce {
    quiet: Duration,
    longest: Duration,
    folders: BTreeSet<String>,
    everything: bool,
    first: Option<Instant>,
    last: Option<Instant>,
}

/// What a quiet moment hands over: the folders that had changes in them, or that all of them may have.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changed {
    pub everything: bool,
    pub folders: Vec<String>,
}

impl Debounce {
    pub fn new(quiet: Duration, longest: Duration) -> Debounce {
        Debounce { quiet, longest, folders: BTreeSet::new(), everything: false, first: None, last: None }
    }

    /// Something changed at `path` (from the workspace), or, with None, more than could be kept count of.
    pub fn add(&mut self, now: Instant, path: Option<&str>) {
        match path {
            Some(path) => {
                self.folders.insert(parent_of(&path.replace('\\', "/")).to_string());
            }
            None => self.everything = true,
        }
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    /// How long to wait for more before what is held is due; None: nothing is held, and only an event is waited for.
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        let (first, last) = (self.first?, self.last?);
        let due = (last + self.quiet).min(first + self.longest);
        Some(due.saturating_duration_since(now))
    }

    /// What is held, when it is due: nothing came for the quiet time, or the first change has waited as long as it may.
    pub fn due(&mut self, now: Instant) -> Option<Changed> {
        if self.wait(now)? > Duration::ZERO {
            return None;
        }
        self.first = None;
        self.last = None;
        Some(Changed { everything: std::mem::take(&mut self.everything), folders: std::mem::take(&mut self.folders).into_iter().collect() })
    }
}

#[cfg(windows)]
pub use watch::Watcher;

/// Changes under a folder, from the system's own notice of them: the thread sleeps until there is one, and then until it has been quiet. Nothing is polled.
#[cfg(windows)]
mod watch {
    use super::{inside_noise, Changed, Debounce};
    use std::path::Path;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects, INFINITE};
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    /// Waits this long with nothing new before it hands over what it saw, and no longer than `LONGEST` after the first.
    pub const QUIET: Duration = Duration::from_millis(400);
    pub const LONGEST: Duration = Duration::from_millis(2500);

    /// A handle that is a thread's own to wait on, and is closed with it.
    struct Event(HANDLE);

    // SAFETY: an event handle is only waited on and set; either is fine from any thread.
    unsafe impl Send for Event {}
    unsafe impl Sync for Event {}

    impl Drop for Event {
        fn drop(&mut self) {
            // SAFETY: the handle is this value's own.
            unsafe { CloseHandle(self.0) };
        }
    }

    pub struct Watcher {
        stop: std::sync::Arc<Event>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Watcher {
        /// Watches `root` and everything under it; `changed` is called on the watcher's thread, a moment after the changes stop. None: it could not be watched (the folder is gone, or no right to it).
        pub fn start(root: &Path, changed: impl Fn(Changed) + Send + 'static) -> Option<Watcher> {
            let wide: Vec<u16> = root.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: the name ends in a zero; the handle is closed by the thread that is given it.
            let directory = unsafe { CreateFileW(wide.as_ptr(), FILE_LIST_DIRECTORY, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED, std::ptr::null_mut()) };
            if directory == INVALID_HANDLE_VALUE {
                return None;
            }
            // SAFETY: plain events; no name, no security.
            let (stop, io) = unsafe { (CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()), CreateEventW(std::ptr::null(), 1, 0, std::ptr::null())) };
            if stop.is_null() || io.is_null() {
                // SAFETY: what was made is closed.
                unsafe {
                    CloseHandle(directory);
                    if !stop.is_null() {
                        CloseHandle(stop);
                    }
                    if !io.is_null() {
                        CloseHandle(io);
                    }
                }
                return None;
            }
            let stop = std::sync::Arc::new(Event(stop));
            let (for_thread, directory, io) = (stop.clone(), directory as usize, Event(io));
            let thread = std::thread::Builder::new().name("files-watch".into()).spawn(move || run(directory as HANDLE, io, &for_thread, changed)).ok()?;
            Some(Watcher { stop, thread: Some(thread) })
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            // SAFETY: the event is alive as long as this value is.
            unsafe { SetEvent(self.stop.0) };
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn run(directory: HANDLE, io: Event, stop: &Event, changed: impl Fn(Changed)) {
        // (aligned for FILE_NOTIFY_INFORMATION: it holds 32-bit fields)
        #[repr(C, align(8))]
        struct Buffer([u8; 64 * 1024]);
        let mut buffer = Box::new(Buffer([0; 64 * 1024]));
        // SAFETY: zero is an overlapped structure that has no event of its own yet.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = io.0;
        let mut debounce = Debounce::new(QUIET, LONGEST);
        // (what the tree shows is names, and whether they are hidden: a file written to again changes none of it, and is not told of)
        let filter = FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_ATTRIBUTES;
        let mut asking = false;
        loop {
            if !asking {
                // SAFETY: the buffer and the structure live until the request is done, which is before this function ends (it is waited for below).
                let asked = unsafe { ReadDirectoryChangesW(directory, buffer.0.as_mut_ptr().cast(), buffer.0.len() as u32, 1, filter, std::ptr::null_mut(), &mut overlapped, None) };
                if asked == 0 {
                    break;
                }
                asking = true;
            }
            let wait = debounce.wait(Instant::now()).map_or(INFINITE, |left| left.as_millis().min(u128::from(u32::MAX - 1)) as u32);
            let handles = [io.0, stop.0];
            // SAFETY: two live handles.
            let woke = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, wait) };
            if woke == WAIT_TIMEOUT {
                if let Some(due) = debounce.due(Instant::now()) {
                    changed(due);
                }
            } else if woke == WAIT_OBJECT_0 {
                let mut bytes = 0u32;
                // SAFETY: the request is finished (the event was set).
                let done = unsafe { GetOverlappedResult(directory, &overlapped, &mut bytes, 0) };
                asking = false;
                if done == 0 {
                    break;
                }
                let now = Instant::now();
                if bytes == 0 {
                    // (more happened than the buffer held: what changed is not known)
                    debounce.add(now, None);
                } else {
                    let mut at = 0usize;
                    loop {
                        // SAFETY: the system wrote entries, each at the offset the one before gave, within what it said it wrote.
                        let entry = unsafe { &*(buffer.0.as_ptr().add(at) as *const FILE_NOTIFY_INFORMATION) };
                        let units = (entry.FileNameLength / 2) as usize;
                        // SAFETY: the name is that many UTF-16 units from the field.
                        let name = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(entry.FileName.as_ptr(), units) });
                        let path = name.replace('\\', "/");
                        // (a file renamed is two notices; each is a change in its folder)
                        if !inside_noise(&path) {
                            debounce.add(now, Some(&path));
                        }
                        if entry.NextEntryOffset == 0 {
                            break;
                        }
                        at += entry.NextEntryOffset as usize;
                    }
                }
                // SAFETY: the event is this thread's.
                unsafe { windows_sys::Win32::System::Threading::ResetEvent(io.0) };
            } else {
                // the stop event, or a failure
                break;
            }
        }
        if asking {
            // The request has to be over before the buffer goes.
            // SAFETY: the same structure and handle the request was made with.
            unsafe {
                CancelIoEx(directory, &overlapped);
                let mut bytes = 0u32;
                GetOverlappedResult(directory, &overlapped, &mut bytes, 1);
            }
        }
        // SAFETY: the handle was given to this thread to close.
        unsafe { CloseHandle(directory) };
    }
}

/// The paths of a folder for a read, made from the tree's place for it.
pub fn disk_path(root: &Path, relative: &str) -> PathBuf {
    relative.split('/').filter(|part| !part.is_empty()).fold(root.to_path_buf(), |path, part| path.join(part))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: Kind, hidden: bool) -> Entry {
        Entry { name: name.to_string(), kind, hidden }
    }

    fn file(name: &str) -> Entry {
        entry(name, Kind::File, name.starts_with('.'))
    }

    fn folder(name: &str) -> Entry {
        entry(name, Kind::Folder, name.starts_with('.'))
    }

    fn names(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|row| format!("{}{}", "  ".repeat(row.depth), if row.name.is_empty() { format!("<{:?}>", row.kind) } else { row.name.clone() })).collect()
    }

    #[test]
    fn folders_come_first_and_names_in_the_order_a_person_reads_them() {
        let mut entries = vec![file("b.txt"), folder("src"), file("A.txt"), file("part10.txt"), folder("Docs"), file("part2.txt"), file(".env"), folder(".git"), file("part02b.txt")];
        sort(&mut entries);
        let order: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(order, [".git", "Docs", "src", ".env", "A.txt", "b.txt", "part2.txt", "part02b.txt", "part10.txt"]);
    }

    #[test]
    fn natural_order_is_total_so_that_two_names_never_swap_places() {
        let mut one = vec![file("a1"), file("A1"), file("a01")];
        let mut other = vec![file("A1"), file("a01"), file("a1")];
        sort(&mut one);
        sort(&mut other);
        assert_eq!(one, other, "the same order whichever way they came");
    }

    #[test]
    fn the_usual_noise_is_greyed_and_so_are_hidden_files_and_nothing_else() {
        for name in [".git", "node_modules", "target", "__pycache__", "Target", "NODE_MODULES"] {
            assert!(folder(name).is_dim() && is_noise(name, Kind::Folder), "{name}");
        }
        assert!(!is_noise("target", Kind::File), "a file named target is a file");
        assert!(!is_noise("src", Kind::Folder) && !folder("src").is_dim());
        assert!(file(".env").is_dim() && entry("desktop.ini", Kind::File, true).is_dim());
        assert!(inside_noise("node_modules/left-pad/index.js") && inside_noise("a/target/debug/x.exe") && inside_noise(".git\\HEAD"));
        assert!(!inside_noise("src/targets/a.rs") && !inside_noise("targeted.txt"));
    }

    #[test]
    fn a_tree_starts_with_the_root_unread_and_reads_each_folder_when_it_is_first_opened() {
        let mut tree = Tree::new();
        assert!(tree.root_is_unread());
        tree.root_reading();
        assert!(!tree.root_is_unread());
        assert_eq!(names(&tree.rows()), ["<Reading>"], "the root is being read");
        tree.read_came("", Ok(vec![folder("src"), folder("docs"), file("README.md")]));
        assert_eq!(names(&tree.rows()), ["docs", "src", "README.md"].map(String::from), "folders first");
        // opening a folder asks for its read once: a second opening, after it was closed, does not
        assert_eq!(tree.toggle("src"), Toggled::Read("src".into()));
        assert_eq!(names(&tree.rows()), ["docs", "src", "  <Reading>", "README.md"].map(String::from));
        tree.read_came("src", Ok(vec![file("main.rs"), folder("ui")]));
        assert_eq!(names(&tree.rows()), ["docs", "src", "  ui", "  main.rs", "README.md"].map(String::from));
        assert_eq!(tree.toggle("src"), Toggled::Nothing, "closing asks for nothing");
        assert_eq!(names(&tree.rows()), ["docs", "src", "README.md"].map(String::from));
        assert_eq!(tree.toggle("src"), Toggled::Nothing, "it was read, and is not read again");
        assert!(tree.is_open("src"));
        // a file is not opened
        assert_eq!(tree.toggle("README.md"), Toggled::Nothing);
        assert_eq!(tree.toggle("no/such"), Toggled::Nothing);
    }

    #[test]
    fn an_empty_folder_and_one_that_cannot_be_read_say_so_and_the_second_is_tried_again() {
        let mut tree = Tree::new();
        tree.read_came("", Ok(vec![folder("empty"), folder("locked")]));
        assert_eq!(tree.toggle("empty"), Toggled::Read("empty".into()));
        tree.read_came("empty", Ok(vec![]));
        assert_eq!(tree.toggle("locked"), Toggled::Read("locked".into()));
        tree.read_came("locked", Err("Access is denied.".into()));
        let rows = tree.rows();
        assert_eq!(rows[1].kind, RowKind::Empty);
        assert_eq!(rows[3].kind, RowKind::Problem("Access is denied.".into()));
        // closed and opened again, a folder that failed is read again
        assert_eq!(tree.toggle("locked"), Toggled::Nothing);
        assert_eq!(tree.toggle("locked"), Toggled::Read("locked".into()));
    }

    #[test]
    fn what_a_folder_shows_is_capped_with_a_row_that_says_how_many_more_and_each_press_adds_a_page() {
        let mut tree = Tree::new();
        // (fifty thousand files in one folder)
        let mut many: Vec<Entry> = (0..50_000).map(|at| file(&format!("file{at:05}.txt"))).collect();
        many.push(folder("sub"));
        let began = Instant::now();
        sort(&mut many);
        tree.read_came("", Ok(many));
        let rows = tree.rows();
        assert!(began.elapsed() < Duration::from_secs(2), "sorting and showing fifty thousand took {:?}", began.elapsed());
        assert_eq!(rows.len(), SHOWN_AT_FIRST + 1, "the capped rows and the row that says there are more");
        assert_eq!(rows.last().unwrap().kind, RowKind::More(50_001 - SHOWN_AT_FIRST));
        assert_eq!(rows[0].name, "sub", "the folder is first of them");
        tree.more("");
        let rows = tree.rows();
        assert_eq!(rows.len(), SHOWN_AT_FIRST + SHOWN_MORE + 1);
        assert_eq!(rows.last().unwrap().kind, RowKind::More(50_001 - SHOWN_AT_FIRST - SHOWN_MORE));
        for _ in 0..200 {
            tree.more("");
        }
        let rows = tree.rows();
        assert_eq!(rows.len(), 50_001, "all of them, and no row of more");
        assert!(rows.iter().all(|row| !matches!(row.kind, RowKind::More(_))));
    }

    #[test]
    fn a_read_again_keeps_what_the_person_opened_and_shows_what_came_and_went() {
        let mut tree = Tree::new();
        tree.read_came("", Ok(vec![folder("a"), folder("b"), file("x.txt")]));
        tree.toggle("a");
        tree.read_came("a", Ok(vec![folder("deep"), file("one.txt")]));
        tree.toggle("a/deep");
        tree.read_came("a/deep", Ok(vec![file("leaf.txt")]));
        assert_eq!(names(&tree.rows()), ["a", "  deep", "    leaf.txt", "  one.txt", "b", "x.txt"].map(String::from));
        // a file came in the root, one went from a, and b has gone
        tree.read_came("", Ok(vec![folder("a"), file("new.txt"), file("x.txt")]));
        tree.read_came("a", Ok(vec![folder("deep"), file("two.txt")]));
        assert_eq!(names(&tree.rows()), ["a", "  deep", "    leaf.txt", "  two.txt", "new.txt", "x.txt"].map(String::from), "a and deep are still open; b is gone");
        // a read for a folder that is no longer there changes nothing
        tree.read_came("b", Ok(vec![file("late.txt")]));
        assert_eq!(tree.rows().len(), 6);
    }

    #[test]
    fn what_the_disk_says_changed_is_read_again_where_it_is_open_and_when_it_is_opened_where_it_is_not() {
        let mut tree = Tree::new();
        tree.read_came("", Ok(vec![folder("a"), folder("b"), folder("c")]));
        for place in ["a", "b", "c"] {
            tree.toggle(place);
            tree.read_came(place, Ok(vec![file("f.txt")]));
        }
        tree.toggle("b");
        assert!(!tree.is_open("b"));
        // a changed, b (closed) changed, the root changed
        let again = tree.changed_on_disk(&["a".into(), "b".into(), String::new()], false);
        assert_eq!(again, ["", "a"], "the root and the open folder are read now");
        assert_eq!(tree.toggle("b"), Toggled::Read("b".into()), "the closed one is read when it is opened");
        // nothing changed in c, and a folder that is not in the tree is ignored
        assert!(tree.changed_on_disk(&["c/zzz".into()], false).is_empty());
        // the watcher lost count: all that is open is read again
        let mut everything = tree.changed_on_disk(&[], true);
        everything.sort();
        assert_eq!(everything, ["", "a", "c"]);
    }

    #[test]
    fn the_files_the_session_changed_are_marked_with_their_lines_and_the_folders_that_hold_them_too() {
        let mut tree = Tree::new();
        tree.read_came("", Ok(vec![folder("src"), file("README.md")]));
        tree.toggle("src");
        tree.read_came("src", Ok(vec![folder("ui"), file("main.rs"), file("lib.rs")]));
        tree.toggle("src/ui");
        tree.read_came("src/ui", Ok(vec![file("panel.rs")]));
        // (the paths as the agent writes them: any case, either slash; a file changed in two answers adds up)
        tree.set_changes([("SRC\\ui\\panel.rs", 10, 2), ("src/main.rs", 3, 0), ("src/main.rs", 4, 1), ("./README.md", 1, 1)]);
        let rows = tree.rows();
        let find = |path: &str| rows.iter().find(|row| row.path == path && !row.name.is_empty()).unwrap();
        assert_eq!(find("src/ui/panel.rs").mark, Mark::Changed(10, 2));
        assert_eq!(find("src/main.rs").mark, Mark::Changed(7, 1));
        assert_eq!(find("src/lib.rs").mark, Mark::None);
        assert_eq!(find("README.md").mark, Mark::Changed(1, 1));
        assert_eq!(find("src").mark, Mark::Inside);
        assert_eq!(find("src/ui").mark, Mark::Inside, "at any depth");
        // a folder that is closed shows that something under it changed, though it has not been read
        tree.read_came("", Ok(vec![folder("src"), folder("other"), file("README.md")]));
        tree.set_changes([("other/deep/x.txt", 1, 0)]);
        assert_eq!(tree.rows().iter().find(|row| row.path == "other").unwrap().mark, Mark::Inside);
        // and the marks go when the changes do
        tree.set_changes(std::iter::empty());
        assert!(tree.rows().iter().all(|row| row.mark == Mark::None));
    }

    #[test]
    fn a_hidden_file_and_noise_are_dim_in_the_rows() {
        let mut tree = Tree::new();
        tree.read_came("", Ok(vec![folder(".git"), folder("node_modules"), folder("src"), file(".env"), file("main.rs")]));
        let dim: Vec<(String, bool)> = tree.rows().into_iter().map(|row| (row.name, row.dim)).collect();
        assert_eq!(dim, [(".git", true), ("node_modules", true), ("src", false), (".env", true), ("main.rs", false)].map(|(name, dim)| (name.to_string(), dim)));
    }

    #[test]
    fn the_tree_counts_what_is_done_to_it_so_that_what_shows_it_can_tell_that_nothing_was() {
        let mut tree = Tree::new();
        let mut seen = tree.version();
        let mut changed = |tree: &Tree, what: &str| {
            let now = tree.version();
            assert_ne!(now, seen, "{what}");
            seen = now;
        };
        tree.root_reading();
        changed(&tree, "the root is being read");
        tree.read_came("", Ok(vec![folder("a")]));
        changed(&tree, "a read came");
        tree.toggle("a");
        changed(&tree, "a folder was opened");
        tree.more("");
        changed(&tree, "more was asked for");
        tree.set_changes([("a/x.txt", 1, 0)]);
        changed(&tree, "files were marked");
        tree.changed_on_disk(&[String::new()], false);
        changed(&tree, "the disk said something changed");
        // asking for the rows is not a change
        let _ = tree.rows();
        assert_eq!(tree.version(), seen);
    }

    #[test]
    fn a_row_finds_the_folder_it_is_in() {
        let mut tree = Tree::new();
        tree.read_came("", Ok(vec![folder("a"), file("z.txt")]));
        tree.toggle("a");
        tree.read_came("a", Ok(vec![folder("b"), file("one.txt")]));
        tree.toggle("a/b");
        tree.read_came("a/b", Ok(vec![file("deep.txt")]));
        let rows = tree.rows();
        let at = |path: &str| rows.iter().position(|row| row.path == path).unwrap();
        assert_eq!(parent_row(&rows, at("a/b/deep.txt")), Some(at("a/b")));
        assert_eq!(parent_row(&rows, at("a/one.txt")), Some(at("a")));
        assert_eq!(parent_row(&rows, at("a")), None, "a top row has none");
        assert_eq!(parent_row(&rows, 99), None);
    }

    #[test]
    fn paths_split_into_their_folder_and_name_whatever_slash_they_have() {
        assert_eq!(parent_of("a/b/c.txt"), "a/b");
        assert_eq!(parent_of("c.txt"), "");
        assert_eq!(key_of(".\\Src\\Main.RS"), "src/main.rs");
        assert_eq!(key_of("/a/b/"), "a/b");
        assert_eq!(disk_path(Path::new("C:\\work"), "a/b"), Path::new("C:\\work\\a\\b"));
        assert_eq!(disk_path(Path::new("C:\\work"), ""), Path::new("C:\\work"));
    }

    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn changes_wait_until_it_is_quiet_and_then_come_once_with_each_folder_once() {
        let start = base();
        let ms = |n: u64| start + Duration::from_millis(n);
        let mut debounce = Debounce::new(Duration::from_millis(400), Duration::from_millis(2500));
        assert_eq!(debounce.wait(start), None, "nothing held: only an event is waited for");
        assert_eq!(debounce.due(start), None);
        debounce.add(ms(0), Some("src/a.rs"));
        debounce.add(ms(100), Some("src/b.rs"));
        debounce.add(ms(150), Some("docs\\c.md"));
        debounce.add(ms(200), Some("top.txt"));
        assert_eq!(debounce.wait(ms(200)), Some(Duration::from_millis(400)), "400 after the last");
        assert_eq!(debounce.due(ms(500)), None, "not yet quiet");
        assert_eq!(debounce.wait(ms(500)), Some(Duration::from_millis(100)));
        let due = debounce.due(ms(600)).unwrap();
        assert_eq!(due, Changed { everything: false, folders: vec!["".into(), "docs".into(), "src".into()] });
        assert_eq!(debounce.wait(ms(600)), None, "and nothing is held after");
        assert_eq!(debounce.due(ms(5000)), None);
    }

    #[test]
    fn a_build_that_never_stops_writing_is_handed_over_after_the_longest_wait() {
        let start = base();
        let ms = |n: u64| start + Duration::from_millis(n);
        let mut debounce = Debounce::new(Duration::from_millis(400), Duration::from_millis(2500));
        for at in (0..4000).step_by(100) {
            debounce.add(ms(at), Some(&format!("out/f{at}.o")));
            let due = debounce.due(ms(at));
            if at < 2500 {
                assert_eq!(due, None, "at {at}");
            } else {
                assert_eq!(due, Some(Changed { everything: false, folders: vec!["out".into()] }), "at {at}");
                return;
            }
        }
        panic!("it never came");
    }

    #[test]
    fn a_buffer_that_overflowed_says_that_anything_may_have_changed() {
        let start = base();
        let mut debounce = Debounce::new(Duration::from_millis(400), Duration::from_millis(2500));
        debounce.add(start, None);
        debounce.add(start + Duration::from_millis(10), Some("a/b.txt"));
        let due = debounce.due(start + Duration::from_secs(1)).unwrap();
        assert!(due.everything && due.folders == ["a"]);
        // and the next one is as any
        debounce.add(start + Duration::from_secs(2), Some("x.txt"));
        assert!(!debounce.due(start + Duration::from_secs(3)).unwrap().everything);
    }

    #[test]
    fn a_folder_is_read_with_its_kinds_hidden_names_and_order() {
        let place = tempfile::tempdir().unwrap();
        std::fs::create_dir(place.path().join("src")).unwrap();
        std::fs::create_dir(place.path().join(".git")).unwrap();
        std::fs::write(place.path().join("b.txt"), "b").unwrap();
        std::fs::write(place.path().join("A.txt"), "a").unwrap();
        std::fs::write(place.path().join(".env"), "k").unwrap();
        let entries = read_folder(place.path()).unwrap();
        let shown: Vec<(&str, Kind, bool)> = entries.iter().map(|entry| (entry.name.as_str(), entry.kind, entry.hidden)).collect();
        assert_eq!(shown, [(".git", Kind::Folder, true), ("src", Kind::Folder, false), (".env", Kind::File, true), ("A.txt", Kind::File, false), ("b.txt", Kind::File, false)]);
        assert!(read_folder(&place.path().join("not-there")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn a_file_marked_hidden_by_windows_is_hidden_with_no_dot_in_its_name() {
        let place = tempfile::tempdir().unwrap();
        let hidden = place.path().join("quiet.txt");
        std::fs::write(&hidden, "x").unwrap();
        let wide: Vec<u16> = hidden.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: a path that ends in a zero, of a file that is here.
        assert!(unsafe { windows_sys::Win32::Storage::FileSystem::SetFileAttributesW(wide.as_ptr(), 0x2) } != 0);
        let entries = read_folder(place.path()).unwrap();
        assert_eq!(entries, [entry("quiet.txt", Kind::File, true)]);
    }

    #[cfg(windows)]
    #[test]
    fn the_watcher_tells_of_a_change_once_it_is_quiet_and_not_of_noise_and_not_before() {
        let place = tempfile::tempdir().unwrap();
        std::fs::create_dir(place.path().join("src")).unwrap();
        std::fs::create_dir(place.path().join("target")).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let watcher = Watcher::start(place.path(), move |changed| {
            let _ = sender.send((Instant::now(), changed));
        })
        .expect("a folder that is there can be watched");
        // nothing happens: nothing is said
        assert!(receiver.recv_timeout(Duration::from_millis(900)).is_err(), "no change, no word");
        // noise is not told of
        std::fs::write(place.path().join("target").join("out.o"), "x").unwrap();
        assert!(receiver.recv_timeout(Duration::from_millis(1200)).is_err(), "what is written in a build folder is not told of");
        // a burst of writes is one word, after the burst
        let burst = Instant::now();
        for at in 0..30 {
            std::fs::write(place.path().join("src").join(format!("f{at}.rs")), "x").unwrap();
        }
        let (told_at, changed) = receiver.recv_timeout(Duration::from_secs(5)).expect("the burst is told of");
        assert!(told_at.duration_since(burst) >= Duration::from_millis(300), "not before it was quiet: {:?}", told_at.duration_since(burst));
        assert!(changed.folders.contains(&"src".to_string()) && !changed.everything, "{changed:?}");
        assert!(receiver.recv_timeout(Duration::from_millis(1200)).is_err(), "and told once");
        // a file made at the top is a change of the workspace's own folder
        std::fs::write(place.path().join("top.txt"), "x").unwrap();
        let (_, changed) = receiver.recv_timeout(Duration::from_secs(5)).expect("a change at the top is told of");
        assert!(changed.folders.contains(&String::new()), "{changed:?}");
        let stopping = Instant::now();
        drop(watcher);
        assert!(stopping.elapsed() < Duration::from_secs(2), "it stops when it is dropped");
        // and a folder that is not there cannot be watched
        assert!(Watcher::start(&place.path().join("gone"), |_| {}).is_none());
    }
}
