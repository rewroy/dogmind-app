//! What the app writes or deletes as the person in a workspace, where a command may turn a folder on the way into a junction to anywhere (one with a
//! restricted token can: Windows does not count that token a sandbox's, and holds its mount points to nothing), and the app's write would land
//! there, as the person: in the Startup folder, to run at the next sign-in. So the folders from the workspace's root down to the file's are each held
//! open while it is written, which keeps them from being renamed, deleted or put in another's place, and each is looked at by its handle: no link or
//! junction, and right under the one before it, as the file system has it. A folder held while it is empty can still be made a mount point (one
//! with something in it cannot), so what the app makes in one, a folder on the way or the file it writes beside its target, is looked at by its own
//! handle as it is made: made anywhere else, it is deleted at once, and nothing is written.

use std::path::{Component, Path, PathBuf};

/// What is said where a place is in the workspace where it really is, but not as it is written.
pub(crate) const NOT_AS_WRITTEN: &str = "it is in the workspace where it really is, but not as it is written (a link on the way, or the workspace named in two ways), so nothing was written there: write it by its path in the workspace";

/// What is said where it cannot be told whether a place is in the workspace.
pub(crate) const CANNOT_TELL: &str = "where it really is, and so whether it is in the workspace, cannot be told, so nothing was written there";

/// What is said where a folder on the way is not as it should be.
pub(crate) const ON_THE_WAY: &str = "a folder on the way to it is a link or a junction, or is not where it was (a command can make one), so nothing was written there";

/// The folders from a workspace's root to the one a file is written in, held open (see the module) for as long as this lasts.
pub(crate) struct Way {
    #[cfg(windows)]
    _held: Vec<std::os::windows::io::OwnedHandle>,
    /// The folder written in, as the file system has it.
    #[cfg_attr(not(windows), allow(dead_code))]
    real: PathBuf,
}

/// The names of `folder`'s parts below `root`, as written: `None` where it is not below it, or a part is `..`.
fn below(root: &Path, folder: &Path) -> Option<Vec<std::ffi::OsString>> {
    fn plain(path: &Path) -> Vec<Component<'_>> {
        path.components().filter(|part| *part != Component::CurDir).collect()
    }
    let (root, folder) = (plain(root), plain(folder));
    let lower = |part: &Component| part.as_os_str().to_string_lossy().replace('/', "\\").to_lowercase();
    if folder.len() < root.len() || root.iter().zip(&folder).any(|(a, b)| lower(a) != lower(b)) {
        return None;
    }
    folder[root.len()..].iter().map(|part| match part {
        Component::Normal(name) => Some(name.to_os_string()),
        _ => None,
    }).collect()
}

/// `folder` held from `root` down, its missing folders made when `make`. `Ok(None)` where it is not below `root` as written (a place outside the
/// workspace that the person allowed, which no command there can write): it is made, if asked, and written as before.
///
/// What is not below the root as written is looked at with the long names of what is there (a short name, as `PROJEC~1`, written out, and nothing gone
/// through). What is still not, but is in the workspace where both really are (a link on the way, or a real path among named ones), is refused: it
/// would otherwise be written with nothing held.
pub(crate) fn hold(root: &Path, folder: &Path, make: bool) -> Result<Option<Way>, String> {
    let Some(parts) = below(root, folder).or_else(|| below(&long_names(root), &long_names(folder))) else {
        // (written with nothing held only where it is outside the workspace where both really are: what cannot be told is not written)
        match (crate::safety::real_path(root), crate::safety::real_path(folder)) {
            (Some(root), Some(real)) if !crate::safety::inside(&root, &real) => {}
            (Some(_), Some(_)) => return Err(format!("{}: {NOT_AS_WRITTEN}", folder.display())),
            _ => return Err(format!("{}: {CANNOT_TELL}", folder.display())),
        }
        if make {
            std::fs::create_dir_all(folder).map_err(|error| format!("{}: {error}", folder.display()))?;
        }
        return Ok(None);
    };
    #[cfg(windows)]
    {
        let (first, mut real) = windows::open_where(root, None, false)?;
        let mut held = vec![first];
        let mut named = root.to_path_buf();
        for part in parts {
            named.push(&part);
            let made = make && match std::fs::create_dir(&named) {
                Ok(()) => true,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
                Err(error) => return Err(format!("{}: {error}", named.display())),
            };
            let (handle, now) = windows::open_where(&named, Some(&real), made)?;
            held.push(handle);
            real = now;
        }
        Ok(Some(Way { _held: held, real }))
    }
    #[cfg(not(windows))]
    {
        let _ = parts;
        if make {
            std::fs::create_dir_all(folder).map_err(|error| format!("{}: {error}", folder.display()))?;
        }
        Ok(None)
    }
}

/// `path` with the long name of each part that is there (as a folder lists it), and the rest as written: nothing is gone through, and a link keeps its own name.
fn long_names(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let (mut there, mut rest) = (path.to_path_buf(), Vec::new());
        loop {
            if let Some(mut long) = windows::long_path(&there) {
                for part in rest.iter().rev() {
                    long.push(part);
                }
                return long;
            }
            match (there.file_name().map(std::ffi::OsStr::to_os_string), there.parent().map(Path::to_path_buf)) {
                (Some(name), Some(parent)) => {
                    rest.push(name);
                    there = parent;
                }
                _ => return path.to_path_buf(),
            }
        }
    }
    #[cfg(not(windows))]
    path.to_path_buf()
}

/// A new file at `new`, in the folder `way` holds, filled by `fill` and on the disk: where it lands there, as a handle on it says, and anywhere else
/// deleted at once with nothing in it. One left at that name by an end that came before it is taken away first, if it is a plain file there.
pub(crate) fn write_new(way: Option<&Way>, new: &Path, fill: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>) -> std::io::Result<()> {
    #[cfg(windows)]
    let mut file = match windows::create_new(new) {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            windows::take_away_left(way, new)?;
            windows::create_new(new)?
        }
        made => made?,
    };
    #[cfg(not(windows))]
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(new)?;
    #[cfg(windows)]
    if let Some(way) = way {
        if !windows::placed(&file, &way.real) {
            let _ = windows::delete(&file);
            return Err(std::io::Error::other(format!("{}: {ON_THE_WAY}", new.display())));
        }
    }
    #[cfg(not(windows))]
    let _ = way;
    let filled = fill(&mut file).and_then(|()| file.sync_all());
    if filled.is_err() {
        // (what was written of it goes: a full disk, or a copy that would not read)
        #[cfg(windows)]
        let _ = windows::delete(&file);
        #[cfg(not(windows))]
        let _ = std::fs::remove_file(new);
    }
    filled
}

/// `path`, a file right in the folder `way` holds, deleted by a handle on it, which says that it is a plain file there as the file system has it: by its name
/// alone, the folder could have been made a mount point since it was held (an empty one can be), and a file of the same name wherever it led would go.
pub(crate) fn remove(way: Option<&Way>, path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    if let Some(way) = way {
        let file = windows::open_to_delete(path)?;
        if !windows::plain_file(&file) || !windows::placed(&file, &way.real) {
            return Err(std::io::Error::other(format!("{}: {ON_THE_WAY}", path.display())));
        }
        return windows::delete(&file);
    }
    #[cfg(not(windows))]
    let _ = way;
    std::fs::remove_file(path)
}

/// A file of the app's in the folder `way` holds, there for as long as this lasts and deleted after: a folder with something in it cannot be made a
/// mount point, and one held while it is empty can. For what a program the app starts makes in it (`git init`), which the app cannot look at as it is made.
pub(crate) struct Kept {
    #[cfg(windows)]
    file: Option<std::fs::File>,
    /// Its name, which no one can know before it is made: a thing put at a name known before would have kept the app from making it.
    name: Option<String>,
}

impl Kept {
    /// Whether this is the name of the app's file, met in the folder it keeps.
    pub(crate) fn is(&self, name: &str) -> bool {
        self.name.as_deref() == Some(name)
    }
}

/// What the name of a [`Kept`] file begins with: one met in a folder held is the app's own.
pub(crate) const KEPT_MARK: &str = ".dogmind-held-";

/// The folder `way` holds kept from being made a mount point while this lasts (see [`Kept`]): `None` where nothing is held.
pub(crate) fn kept_full(way: Option<&Way>) -> std::io::Result<Kept> {
    #[cfg(windows)]
    {
        let Some(way) = way else { return Ok(Kept { file: None, name: None }) };
        let random = crate::conversation_key::random_bytes::<8>().map_err(|_| std::io::Error::other("no random name could be made for the app's file"))?;
        let name = format!("{KEPT_MARK}{}", crate::conversation_key::hex(&random));
        let mark = way.real.join(&name);
        let file = match windows::create_new(&mark) {
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                windows::take_away_left(Some(way), &mark)?;
                windows::create_new(&mark)?
            }
            made => made?,
        };
        if !windows::placed(&file, &way.real) {
            let _ = windows::delete(&file);
            return Err(std::io::Error::other(format!("{}: {ON_THE_WAY}", mark.display())));
        }
        Ok(Kept { file: Some(file), name: Some(name) })
    }
    #[cfg(not(windows))]
    {
        let _ = way;
        Ok(Kept { name: None })
    }
}

impl Drop for Kept {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(file) = self.file.take() {
            let _ = windows::delete(&file);
        }
    }
}

/// `new`, written beside `path` in a folder held, in its place: over the file there by Windows's ReplaceFile (which keeps what it was but its
/// words), or as a new one. A link at the name is not written through.
pub(crate) fn into_place(new: &Path, path: &Path, exists: bool) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|found| found.file_type().is_symlink()) {
        return Err(std::io::Error::other(format!("{} is a link, which is not written through", path.display())));
    }
    if exists { crate::tools::take_the_place(new, path) } else { std::fs::rename(new, path) }
}

#[cfg(windows)]
mod windows {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileDispositionInfo, GetFileInformationByHandle, GetFinalPathNameByHandleW, SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, DELETE,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, SYNCHRONIZE,
    };

    use super::ON_THE_WAY;

    /// A folder held: listed, its reading and writing shared (a file is put in its place by a rename, which opens its folder to write) and not its
    /// deleting, so it cannot be renamed, deleted or replaced while this lasts.
    /// It is no link or junction, and is right under `under` (where that is said), as its handle has it. One the app has just made (`made`) that is not
    /// so is deleted.
    pub(super) fn open_where(named: &Path, under: Option<&Path>, made: bool) -> Result<(OwnedHandle, PathBuf), String> {
        let name: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(named.as_os_str()).chain([0]).collect();
        let access = FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE | if made { DELETE } else { 0 };
        // SAFETY: the name ends with a zero and lives through the call; the handle is owned from here.
        let handle = unsafe {
            CreateFileW(name.as_ptr(), access, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, std::ptr::null_mut())
        };
        if handle == INVALID_HANDLE_VALUE {
            let error = std::io::Error::last_os_error();
            return Err(if error.raw_os_error() == Some(32) {
                format!("{}: another program has it open to change it, so nothing was written there: try again", named.display())
            } else {
                format!("{}: {error}", named.display())
            });
        }
        // SAFETY: a handle that was just opened, and is no one else's.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle as _) };
        let folder = attributes(&handle).is_some_and(|found| found & FILE_ATTRIBUTE_REPARSE_POINT == 0 && found & FILE_ATTRIBUTE_DIRECTORY != 0);
        let real = final_path(&handle);
        let placed = match (&real, under) {
            (Some(real), Some(under)) => real.parent().is_some_and(|parent| same(parent, under)),
            (Some(_), None) => true,
            (None, _) => false,
        };
        match real {
            Some(real) if folder && placed => Ok((handle, real)),
            _ => {
                if made {
                    let _ = delete(&handle);
                }
                Err(format!("{}: {ON_THE_WAY}", named.display()))
            }
        }
    }

    /// A path as the file system lists each part of it, short names written out.
    pub(super) fn long_path(path: &Path) -> Option<PathBuf> {
        let name: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(path.as_os_str()).chain([0]).collect();
        let mut text = vec![0u16; 1024];
        loop {
            // SAFETY: the name ends with a zero; the buffer lives through the call, which is told its length.
            let length = unsafe { windows_sys::Win32::Storage::FileSystem::GetLongPathNameW(name.as_ptr(), text.as_mut_ptr(), text.len() as u32) } as usize;
            if length == 0 {
                return None;
            }
            if length < text.len() {
                return Some(PathBuf::from(String::from_utf16_lossy(&text[..length])));
            }
            text.resize(length + 1, 0);
        }
    }

    /// A new file, written to alone, which this handle may delete; a link at the name is not opened through.
    pub(super) fn create_new(new: &Path) -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new().write(true).create_new(true).access_mode(GENERIC_WRITE | DELETE).share_mode(0).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(new)
    }

    /// What an end that came before left at `new`, taken away where it is a plain file in the folder held.
    pub(super) fn take_away_left(way: Option<&super::Way>, new: &Path) -> std::io::Result<()> {
        let left = open_to_delete(new)?;
        if !plain_file(&left) || way.is_some_and(|way| !placed(&left, &way.real)) {
            return Err(std::io::Error::other(format!("{}: {ON_THE_WAY}", new.display())));
        }
        delete(&left)
    }

    /// A file opened to be deleted by its handle: a link at the name is not opened through.
    pub(super) fn open_to_delete(path: &Path) -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new().access_mode(DELETE | FILE_READ_ATTRIBUTES).share_mode(7).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path)
    }

    /// Whether what a handle is of is a plain file: no folder, link or junction.
    pub(super) fn plain_file(handle: &impl AsRawHandle) -> bool {
        attributes(handle).is_some_and(|found| found & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) == 0)
    }

    /// Whether what a handle is of lies right in `folder`, as the file system has it.
    pub(super) fn placed(handle: &impl AsRawHandle, folder: &Path) -> bool {
        final_path(handle).is_some_and(|real| real.parent().is_some_and(|parent| same(parent, folder)))
    }

    /// What a handle is of, deleted as its handle closes (an empty folder, or a file).
    pub(super) fn delete(handle: &impl AsRawHandle) -> std::io::Result<()> {
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: 1 };
        // SAFETY: the struct lives through the call, which is told its size.
        let done = unsafe {
            SetFileInformationByHandle(handle.as_raw_handle() as HANDLE, FileDispositionInfo, (&disposition as *const FILE_DISPOSITION_INFO).cast(), std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32)
        };
        if done == 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
    }

    fn attributes(handle: &impl AsRawHandle) -> Option<u32> {
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the struct lives through the call.
        (unsafe { GetFileInformationByHandle(handle.as_raw_handle() as HANDLE, &mut info) } != 0).then_some(info.dwFileAttributes)
    }

    fn final_path(handle: &impl AsRawHandle) -> Option<PathBuf> {
        let mut text = vec![0u16; 1024];
        loop {
            // SAFETY: the buffer lives through the call, which is told its length.
            let length = unsafe { GetFinalPathNameByHandleW(handle.as_raw_handle() as HANDLE, text.as_mut_ptr(), text.len() as u32, 0) } as usize;
            if length == 0 {
                return None;
            }
            if length < text.len() {
                return Some(PathBuf::from(String::from_utf16_lossy(&text[..length])));
            }
            text.resize(length + 1, 0);
        }
    }

    fn same(a: &Path, b: &Path) -> bool {
        let spelled = |path: &Path| crate::sandbox::key(path).trim_start_matches("\\\\?\\").to_string();
        spelled(a) == spelled(b)
    }
}

/// For tests: what a command with a restricted token can do over and over to a folder of a workspace while the app writes there.
#[cfg(all(test, windows))]
pub(crate) mod swapping {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

/// `folder`, empty, made a mount point to `target`, as a command can make one of a folder it may write.
pub(crate) fn mount_point(folder: &Path, target: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    let w = |text: &std::ffi::OsStr| -> Vec<u16> { text.encode_wide().collect() };
    let target = target.as_os_str();
    let substitute = w(std::ffi::OsStr::new(&format!("\\??\\{}", target.to_string_lossy())));
    let print = w(target);
    let mut names = substitute.clone();
    names.push(0);
    names.extend(&print);
    names.push(0);
    let header = [0u16, (substitute.len() * 2) as u16, ((substitute.len() + 1) * 2) as u16, (print.len() * 2) as u16];
    let mut buffer = 0xA000_0003u32.to_le_bytes().to_vec();
    buffer.extend(((header.len() * 2 + names.len() * 2) as u16).to_le_bytes());
    buffer.extend(0u16.to_le_bytes());
    for value in header.iter().chain(names.iter()) {
        buffer.extend(value.to_le_bytes());
    }
    let name: Vec<u16> = folder.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: the name and the buffer live through the calls; the handle is closed here.
    unsafe {
        let handle = CreateFileW(name.as_ptr(), GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, std::ptr::null_mut());
        if handle == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut returned = 0u32;
        let done = windows_sys::Win32::System::IO::DeviceIoControl(handle, 0x0009_00A4, buffer.as_ptr().cast(), buffer.len() as u32, std::ptr::null_mut(), 0, &mut returned, std::ptr::null_mut());
        CloseHandle(handle);
        done != 0
    }
}

    /// A loop that sets `folder` aside, puts a junction to `target` in its place, takes it away and puts the folder back, over and over, until it
    /// is stopped; `back` is run each time the folder is back. A folder the app holds cannot be set aside: it is tried again.
    pub(crate) struct Swapping {
        stop: Arc<AtomicBool>,
        swaps: Arc<AtomicUsize>,
        thread: Option<std::thread::JoinHandle<()>>,
        folder: PathBuf,
    }

    impl Swapping {
        pub(crate) fn start(folder: &Path, target: &Path, back: impl Fn(&Path) + Send + 'static) -> Swapping {
            let (stop, swaps) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
            let thread = {
                let (stop, swaps, folder, target) = (stop.clone(), swaps.clone(), folder.to_path_buf(), target.to_path_buf());
                let aside = folder.with_file_name(format!("{}-aside", folder.file_name().unwrap().to_string_lossy()));
                std::thread::spawn(move || {
                    let mut set_aside = 0;
                    while !stop.load(Ordering::Relaxed) {
                        if std::fs::rename(&folder, &aside).is_err() {
                            std::thread::yield_now();
                            continue;
                        }
                        if std::fs::create_dir(&folder).is_ok() {
                            if mount_point(&folder, &target) {
                                swaps.fetch_add(1, Ordering::Relaxed);
                            }
                            std::thread::yield_now();
                            let _ = std::fs::remove_dir(&folder);
                        }
                        // (back, whatever the app made at the name while it was away being moved out of the way)
                        while std::fs::rename(&aside, &folder).is_err() && !stop.load(Ordering::Relaxed) {
                            set_aside += 1;
                            let _ = std::fs::rename(&folder, folder.with_file_name(format!("made-{set_aside}")));
                            std::thread::yield_now();
                        }
                        back(&folder);
                        // (a while as it was, for the app to come to it so too)
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                })
            };
            Swapping { stop, swaps, thread: Some(thread), folder: folder.to_path_buf() }
        }

        /// The loop ended, a junction it left taken away: how many times it put one in place.
        pub(crate) fn stop(mut self) -> usize {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
            if std::fs::symlink_metadata(&self.folder).is_ok_and(|found| found.file_type().is_symlink()) {
                std::fs::remove_dir(&self.folder).unwrap();
            }
            self.swaps.load(Ordering::Relaxed)
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn a_way_is_its_parts_below_the_root_as_written() {
        let parts = |root: &str, folder: &str| below(Path::new(root), Path::new(folder)).map(|parts| parts.iter().map(|part| part.to_string_lossy().into_owned()).collect::<Vec<_>>());
        assert_eq!(parts("C:\\w", "c:\\W\\a\\b"), Some(vec!["a".to_string(), "b".to_string()]));
        assert_eq!(parts("C:\\w\\.", "C:\\w"), Some(vec![]));
        assert_eq!(parts("C:\\w", "C:\\w\\a\\..\\..\\x"), None, "a part that is `..`");
        assert_eq!(parts("C:\\w", "C:\\other\\a"), None);
        assert_eq!(parts("C:\\w", "C:\\wx\\a"), None);
    }

    /// Folders are made where they are missing, a file is written in the last, and a folder on the way that is a junction is refused.
    #[test]
    fn a_way_is_held_and_made_and_one_through_a_junction_is_refused() {
        let (dir, outside) = (crate::in_container::tempdir().unwrap(), crate::in_container::tempdir().unwrap());
        let root = dir.path();
        let way = hold(root, &root.join("a").join("b"), true).unwrap().unwrap();
        write_new(Some(&way), &root.join("a").join("b").join("x.txt"), |out| std::io::Write::write_all(out, b"x")).unwrap();
        drop(way);
        assert_eq!(std::fs::read_to_string(root.join("a").join("b").join("x.txt")).unwrap(), "x");
        let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(root.join("j")).arg(outside.path()).output().unwrap();
        assert!(made.status.success());
        let refused = hold(root, &root.join("j").join("c"), true).err().unwrap_or_default();
        assert!(refused.contains(ON_THE_WAY), "{refused}");
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0, "nothing made outside");
        std::fs::remove_dir(root.join("j")).unwrap();
        assert!(hold(root, &outside.path().join("x"), false).unwrap().is_none(), "outside the root: not held, written as before");
    }

    /// A path written with a short name (as `PROJEC~1`) is held as the folder it names: nothing is written through a junction on its way, and a plain one is
    /// held. One that is in the workspace where it really is, but under another name for the root (a real path among named ones), is not written at all.
    #[test]
    fn a_short_name_or_another_name_for_the_root_is_not_written_unheld() {
        let (dir, outside) = (crate::in_container::tempdir().unwrap(), crate::in_container::tempdir().unwrap());
        let root = dir.path();
        let long = root.join("project-with-a-long-name");
        std::fs::create_dir_all(long.join("plain")).unwrap();
        let junction = |link: &Path, target: &Path| {
            let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(link).arg(target).output().unwrap();
            assert!(made.status.success());
        };
        junction(&long.join("sub"), outside.path());
        let short = {
            let name: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(long.as_os_str()).chain([0]).collect();
            let mut text = vec![0u16; 1024];
            // SAFETY: the buffers live through the call, which is told the length of the one it writes.
            let length = unsafe { windows_sys::Win32::Storage::FileSystem::GetShortPathNameW(name.as_ptr(), text.as_mut_ptr(), text.len() as u32) } as usize;
            PathBuf::from(String::from_utf16_lossy(&text[..length]))
        };
        assert_ne!(short.file_name(), long.file_name(), "a short name is there: {}", short.display());
        let refused = hold(root, &short.join("sub").join("deeper"), true).err().unwrap_or_default();
        assert!(refused.contains(ON_THE_WAY), "{refused}");
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0, "nothing made outside");
        assert!(hold(root, &short.join("plain"), false).unwrap().is_some(), "held as the folder it names");
        // (the root named through a junction of the person's, the folder written by where it really is)
        let (real_root, named_root) = (root.join("real-root"), root.join("named-root"));
        std::fs::create_dir_all(real_root.join("a")).unwrap();
        junction(&named_root, &real_root);
        let refused = hold(&named_root, &real_root.join("a"), false).err().unwrap_or_default();
        assert!(refused.contains(NOT_AS_WRITTEN), "{refused}");
        std::fs::remove_dir(&named_root).unwrap();
        std::fs::remove_dir(long.join("sub")).unwrap();
    }

    /// A file removed in a folder held, where the folder has been emptied and made a mount point to a folder outside since it was held (as an empty one can
    /// be): the file of the same name there is not deleted, and the removal says so. A plain one there is deleted.
    #[test]
    fn a_file_is_removed_only_right_in_the_folder_held() {
        let (dir, outside) = (crate::in_container::tempdir().unwrap(), crate::in_container::tempdir().unwrap());
        let root = dir.path();
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("x.txt"), "mine").unwrap();
        let way = hold(root, &sub, false).unwrap().unwrap();
        remove(Some(&way), &sub.join("x.txt")).unwrap();
        assert!(!sub.join("x.txt").exists(), "a plain one is deleted");
        std::fs::write(outside.path().join("x.txt"), "theirs").unwrap();
        assert!(swapping::mount_point(&sub, outside.path()), "the held folder, empty, made a mount point");
        let refused = remove(Some(&way), &sub.join("x.txt")).err().map(|error| error.to_string()).unwrap_or_default();
        assert!(refused.contains(ON_THE_WAY), "{refused}");
        assert_eq!(std::fs::read_to_string(outside.path().join("x.txt")).unwrap(), "theirs");
        drop(way);
        std::fs::remove_dir(&sub).unwrap();
    }

    /// While a loop swaps a folder for a junction to one outside, where the person has a file of the same name, a file of the workspace's there is written
    /// and removed many times: the person's file stays.
    #[test]
    fn while_a_folder_is_swapped_for_a_junction_nothing_outside_is_removed() {
        let (dir, outside) = (crate::in_container::tempdir().unwrap(), crate::in_container::tempdir().unwrap());
        let root = dir.path().to_path_buf();
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(outside.path().join("x.txt"), "theirs").unwrap();
        let swapping = swapping::Swapping::start(&sub, outside.path(), |_| {});
        let mut removed = 0;
        for _ in 0..60 {
            let file = sub.join("x.txt");
            let _ = crate::tools::written_whole(&root, &file, b"mine", false);
            if let Ok(way) = hold(&root, &sub, false) {
                removed += usize::from(remove(way.as_ref(), &file).is_ok());
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let swaps = swapping.stop();
        assert_eq!(std::fs::read_to_string(outside.path().join("x.txt")).unwrap(), "theirs");
        assert!(swaps > 0 && removed > 0, "{swaps} swaps, {removed} removed");
    }

    /// A loop in the background puts a junction to a folder outside in the place of a folder of the workspace, and the folder back, over and over, as a
    /// command with a restricted token can. Meanwhile create_file's, apply_patch's and a rewind's writers write in that folder, many times: nothing is
    /// ever made outside, and what is written lands in the workspace or is refused.
    #[test]
    fn while_a_folder_is_swapped_for_a_junction_nothing_is_written_outside() {
        let (dir, outside, kept) = (crate::in_container::tempdir().unwrap(), crate::in_container::tempdir().unwrap(), crate::in_container::tempdir().unwrap());
        let root = dir.path().to_path_buf();
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("patched.txt"), "0").unwrap();
        let copy = kept.path().join("copy.txt");
        std::fs::write(&copy, "kept").unwrap();
        // (the junction the loop makes is one: made of an empty folder, and gone again)
        let trial = root.join("trial");
        std::fs::create_dir(&trial).unwrap();
        assert!(swapping::mount_point(&trial, outside.path()), "a mount point is made: {}", std::io::Error::last_os_error());
        std::fs::remove_dir(&trial).unwrap();
        let swapping = swapping::Swapping::start(&sub, outside.path(), |_| {});
        let mut written = 0;
        for at in 0..60 {
            let new = sub.join(format!("new-{at}.txt"));
            written += usize::from(crate::tools::written_whole(&root, &new, b"new", false).is_ok());
            written += usize::from(crate::tools::written_whole(&root, &sub.join("patched.txt"), b"whole", true).is_ok());
            let patched = sub.join("patched.txt");
            written += usize::from(crate::tools::write_all_or_none(&root, &[(&patched, b"whole", b"patched".to_vec())]).is_ok());
            written += usize::from(crate::checkpoint::put_back(&root, &copy, &sub.join(format!("back-{at}.txt"))).is_ok());
            // (time for the loop to swap them, between the writes)
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let swaps = swapping.stop();
        let made: Vec<_> = std::fs::read_dir(outside.path()).unwrap().flatten().map(|entry| entry.file_name()).collect();
        assert!(made.is_empty(), "made outside: {made:?}");
        assert!(swaps > 0, "the junction was put in place");
        assert!(written > 0, "some writes landed");
        eprintln!("{swaps} swaps, {written} of 240 writes landed");
    }
}
