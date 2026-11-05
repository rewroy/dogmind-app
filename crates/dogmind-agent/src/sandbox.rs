//! Where `execute_command` runs on Windows: its own AppContainer. The workspace is writable, the per-user toolchains on PATH
//! are readable, and nothing else outside what every app may read; no network unless this command was approved for it. A Job
//! Object ends all a command started. There is no fallback to an unrestricted child: what cannot be contained does not run.
//!
//! Grants are ACEs for this profile's SID, as AppContainers have no other way: they are written on the folders and persist.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::Authorization::*;
use windows_sys::Win32::Security::Isolation::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, CreateDesktopW, CreateWindowStationW, EnumDesktopWindows, GetProcessWindowStation, GetUserObjectInformationW, SetProcessWindowStation, HDESK, UOI_NAME};
use windows_sys::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, SE_GROUP_ENABLED};
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetWindowTextW};

const PROFILE: &str = "DogmindApp.Exec";
pub(crate) const READ_EXECUTE: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
/// Going through a folder, and nothing of what is in it.
pub(crate) const TRAVERSE: u32 = FILE_GENERIC_EXECUTE;
pub(crate) const READ_WRITE_EXECUTE: u32 = READ_EXECUTE | FILE_GENERIC_WRITE | DELETE;
/// `internetClient` and `privateNetworkClientServer`, for a command approved for the network.
const NETWORK_CAPABILITIES: [&str; 2] = ["S-1-15-3-1", "S-1-15-3-3"];
/// What a command inherits, as the agent's own environment may hold keys. TEMP and TMP are the profile's. The last three are a Developer prompt's, for a build to find the
/// compiler's libraries and headers: a person who started the app from one has them, and a command is given them as they are.
const VARIABLES: [&str; 27] = [
    "SystemRoot", "windir", "SystemDrive", "ComSpec", "OS", "PATH", "PATHEXT", "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE", "ProgramData",
    "ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "CommonProgramFiles", "USERPROFILE", "USERNAME", "HOMEDRIVE", "HOMEPATH", "APPDATA", "LOCALAPPDATA",
    "LANG", "CARGO_HOME", "RUSTUP_HOME", "PUBLIC", "LIB", "INCLUDE", "LIBPATH",
];
/// Per-user toolchain homes that PATH alone does not reach; each is granted when it exists.
const TOOLCHAIN_HOMES: [&str; 8] = [".cargo", ".rustup", ".local\\bin", "AppData\\Local\\Programs\\Python", "AppData\\Local\\Python", "AppData\\Roaming\\uv", "AppData\\Roaming\\npm", "AppData\\Roaming\\nvm"];
const DEVELOPER_TOOLS: [&str; 24] = ["node", "npm", "python", "py", "git", "go", "cargo", "rustc", "dotnet", "java", "javac", "ruby", "php", "perl", "cmake", "ninja", "make", "gcc", "clang", "deno", "bun", "pnpm", "yarn", "uv"];
/// In a folder as wide as the home or a drive, what a command may not be given to write in, however it asks.
const NOT_A_PROJECT: [&str; 18] = ["appdata", "desktop", "documents", "downloads", "pictures", "music", "videos", "onedrive", "windows", "program files", "program files (x86)", "programdata", "users", "$recycle.bin", "system volume information", "contacts", "favorites", "searches"];

#[derive(Default)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: Option<i32>,
    /// Why it has no status.
    pub ended: &'static str,
    /// The windows it had open on its own desktop, which nobody can see.
    pub shown: Vec<String>,
    /// What was taken out of a repository the command made, as it would have run outside the sandbox.
    pub disarmed: Vec<String>,
    /// The memory it may use, when it failed having used all of it.
    pub out_of_memory: Option<u64>,
    /// What the app could not keep a command from planting something in, once this one had ended (a link it does not go through, or a list it could not write). What it says still holds at
    /// the next command's start, which is refused for it: the person has to put it right.
    pub unguarded: Option<String>,
}

/// So that nothing a command does can raise a box on the person's screen: not "the application was unable to start", which the system itself shows, on the desktop in use, whatever desktop
/// the program is on, and not the crash dialog. Children have this from their parent, so it is set before any is started, and it stays set. It is said at the app's start too.
pub fn no_error_boxes() {
    use windows_sys::Win32::System::Diagnostics::Debug::{SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX};
    // SAFETY: it takes flags and returns the old ones.
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
    }
}

/// What a command may do to the person's session, whatever it is: nothing of their clipboard, their settings or their windows.
const UI_LIMITS: u32 = JOB_OBJECT_UILIMIT_DESKTOP | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS | JOB_OBJECT_UILIMIT_EXITWINDOWS | JOB_OBJECT_UILIMIT_GLOBALATOMS | JOB_OBJECT_UILIMIT_HANDLES | JOB_OBJECT_UILIMIT_READCLIPBOARD | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS | JOB_OBJECT_UILIMIT_WRITECLIPBOARD;
/// A dialog with no one to answer it is the end of a command: it would only wait out its time, at the cost of minutes of a slow model's.
const MOST_DIALOG: Duration = Duration::from_secs(10);
/// Read, draw and enumerate; never switch to it, which would take the person's screen away.
const ON_ITS_DESKTOP: u32 = 0x0001 | 0x0002 | 0x0004 | 0x0008 | 0x0040 | 0x0080 | READ_CONTROL;

/// A desktop of its own for one command, in a window station that is never shown. Whatever the command opens goes there.
struct Desktop {
    handle: HDESK,
    /// As `lpDesktop` wants it, to the terminating nul.
    name: Vec<u16>,
}

impl Drop for Desktop {
    fn drop(&mut self) {
        // SAFETY: the handle is this value's.
        unsafe { CloseDesktop(self.handle) };
    }
}

/// Lets the container use a window station or desktop: these rights, and a label it may write under, as a container runs at low integrity.
unsafe fn let_container_use(object: HANDLE, sid: PSID, rights: u32) -> Result<(), String> {
    let (mut old, mut descriptor): (*mut ACL, PSECURITY_DESCRIPTOR) = (std::ptr::null_mut(), std::ptr::null_mut());
    if GetSecurityInfo(object, SE_WINDOW_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), &mut old, std::ptr::null_mut(), &mut descriptor) != 0 {
        return Err(last_error("a desktop's access list"));
    }
    let mut access: EXPLICIT_ACCESS_W = std::mem::zeroed();
    access.grfAccessPermissions = rights;
    access.grfAccessMode = GRANT_ACCESS;
    access.grfInheritance = NO_INHERITANCE;
    access.Trustee.TrusteeForm = TRUSTEE_IS_SID;
    access.Trustee.TrusteeType = TRUSTEE_IS_WELL_KNOWN_GROUP;
    access.Trustee.ptstrName = sid as *mut u16;
    let mut new: *mut ACL = std::ptr::null_mut();
    let merged = SetEntriesInAclW(1, &access, old, &mut new);
    let written = if merged == 0 { SetSecurityInfo(object, SE_WINDOW_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), new, std::ptr::null()) } else { merged };
    LocalFree(new as HLOCAL);
    LocalFree(descriptor as HLOCAL);
    if written != 0 {
        return Err(format!("a desktop's access list could not be written (error {written})"));
    }
    let label = wide("S:(ML;;NW;;;LW)");
    let mut low: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    if ConvertStringSecurityDescriptorToSecurityDescriptorW(label.as_ptr(), 1, &mut low, std::ptr::null_mut()) == 0 {
        return Err(last_error("a desktop's label"));
    }
    let (mut present, mut sacl, mut defaulted): (BOOL, *mut ACL, BOOL) = (0, std::ptr::null_mut(), 0);
    let labelled = GetSecurityDescriptorSacl(low, &mut present, &mut sacl, &mut defaulted) != 0 && SetSecurityInfo(object, SE_WINDOW_OBJECT, LABEL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), sacl) == 0;
    LocalFree(low as HLOCAL);
    if labelled { Ok(()) } else { Err("a desktop's label could not be written".to_string()) }
}

/// One window station for all commands, for as long as the app runs. One made by a program is not interactive: nothing in it can reach the screen, and the system does not show
/// its hard errors, which it would show on the desktop in use from any desktop of the person's own station, for a program that has put its error mode back to the default.
static STATION: Mutex<usize> = Mutex::new(0);
/// Everything but ending the session and reading the screen.
const ON_ITS_STATION: u32 = 0x0001 | 0x0002 | 0x0004 | 0x0008 | 0x0010 | 0x0020 | 0x0200 | READ_CONTROL;

impl Desktop {
    unsafe fn new(sid: PSID) -> Result<Desktop, String> {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let name = format!("DogmindExec-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let own = wide(&name);
        let mut station = STATION.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *station == 0 {
            // (with no name, as only an administrator can choose one: the system gives it one; WINSTA_ALL_ACCESS)
            let made = CreateWindowStationW(std::ptr::null(), 0, 0x37F | READ_CONTROL | WRITE_DAC | WRITE_OWNER, std::ptr::null());
            if made.is_null() {
                return Err(last_error("a window station of its own"));
            }
            let_container_use(made as HANDLE, sid, ON_ITS_STATION)?;
            *station = made as usize;
        }
        // A desktop is made in the process's station. The app's own threads have theirs already, and it is back in a moment.
        let usual = GetProcessWindowStation();
        if SetProcessWindowStation(*station as _) == 0 {
            return Err(last_error("its window station"));
        }
        let handle = CreateDesktopW(own.as_ptr(), std::ptr::null(), std::ptr::null(), 0, 0x1000_0000, std::ptr::null());
        let failed = handle.is_null().then(|| last_error("a desktop of its own"));
        SetProcessWindowStation(usual);
        drop(station);
        if let Some(failed) = failed {
            return Err(failed);
        }
        let mut station_name = [0u16; 128];
        let mut needed = 0u32;
        if GetUserObjectInformationW(*STATION.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) as HANDLE, UOI_NAME, station_name.as_mut_ptr().cast(), 256, &mut needed) == 0 {
            CloseDesktop(handle);
            return Err(last_error("its window station's name"));
        }
        let length = station_name.iter().position(|unit| *unit == 0).unwrap_or(0);
        let desktop = Desktop { handle, name: wide(&format!("{}\\{name}", String::from_utf16_lossy(&station_name[..length]))) };
        let_container_use(handle as HANDLE, sid, ON_ITS_DESKTOP)?;
        Ok(desktop)
    }

    /// Its windows: their class and title.
    unsafe fn windows(&self) -> Vec<(String, String)> {
        unsafe extern "system" fn each(window: windows_sys::Win32::Foundation::HWND, found: LPARAM) -> BOOL {
            let found = &mut *(found as *mut Vec<(String, String)>);
            let (mut class, mut title) = ([0u16; 128], [0u16; 256]);
            let class_length = GetClassNameW(window, class.as_mut_ptr(), class.len() as i32).max(0) as usize;
            let title_length = GetWindowTextW(window, title.as_mut_ptr(), title.len() as i32).max(0) as usize;
            let (class, title) = (String::from_utf16_lossy(&class[..class_length]), String::from_utf16_lossy(&title[..title_length]));
            // (nothing is ever "visible" in a station with no screen. These are a program's own unseen helpers, there for any that uses windows at all)
            const HELPERS: [&str; 8] = ["IME", "MSCTFIME", "GDI+", "Chrome_", "Base_", "tooltips", "CicMarshal", "OleMainThread"];
            if !HELPERS.iter().any(|helper| class.starts_with(helper)) {
                found.push((class, title));
            }
            1
        }
        let mut found: Vec<(String, String)> = Vec::new();
        // (a desktop's windows can be listed only from its own station)
        let station = STATION.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let usual = GetProcessWindowStation();
        if SetProcessWindowStation(*station as _) != 0 {
            EnumDesktopWindows(self.handle, Some(each), std::ptr::addr_of_mut!(found) as LPARAM);
            SetProcessWindowStation(usual);
        }
        found
    }
}

/// A command's output: UTF-8, as asked for, or for what ignored that (cmd.exe's own messages) the system's code page.
pub fn decode(bytes: &[u8]) -> String {
    use windows_sys::Win32::Globalization::{GetOEMCP, MultiByteToWideChar};
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    // SAFETY: the buffers are as long as the calls are told.
    unsafe {
        let code_page = GetOEMCP();
        let length = MultiByteToWideChar(code_page, 0, bytes.as_ptr(), bytes.len() as i32, std::ptr::null_mut(), 0);
        if length <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        let mut units = vec![0u16; length as usize];
        MultiByteToWideChar(code_page, 0, bytes.as_ptr(), bytes.len() as i32, units.as_mut_ptr(), length);
        String::from_utf16_lossy(&units)
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

pub(crate) fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let plain: Vec<u16> = path.as_os_str().encode_wide().collect();
    // (past this the ordinary spelling is refused, and what is deep in a tree is as much a part of it as what is not)
    let prefix: &[u16] = if plain.len() >= 248 && path.is_absolute() && !path.as_os_str().to_string_lossy().starts_with("\\\\") { &[0x5c, 0x5c, 0x3f, 0x5c] } else { &[] };
    prefix.iter().copied().chain(plain).chain(std::iter::once(0)).collect()
}

fn last_error(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

pub(crate) fn key(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// Whether `inner` is `outer` or lies under it.
pub(crate) fn under(inner: &str, outer: &str) -> bool {
    inner == outer || inner.starts_with(&format!("{outer}\\"))
}

/// What the drive of a folder is, as far as a sandbox and a large file care.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// `NTFS`, `exFAT`, `FAT32`..., or empty for a network folder, whose answer may take long and changes nothing.
    pub file_system: String,
    /// Whether it keeps access lists at all. Without them nothing on it can be closed to a container, and an AppContainer's grants have nothing to be written on.
    pub keeps_permissions: bool,
    /// On another machine. A container has no network, so it cannot reach this at all.
    pub remote: bool,
}

/// The drive of this folder, or of the nearest folder above it that exists.
pub fn volume_of(path: &Path) -> Option<Volume> {
    let existing = path.ancestors().find(|folder| folder.is_dir())?;
    if existing.to_string_lossy().starts_with("\\\\") && !existing.to_string_lossy().starts_with("\\\\?\\") {
        return Some(Volume { file_system: String::new(), keeps_permissions: true, remote: true });
    }
    let mut root = [0u16; 300];
    // SAFETY: the name ends in a zero, and the buffers are as long as the calls are told.
    unsafe {
        if GetVolumePathNameW(wide_path(existing).as_ptr(), root.as_mut_ptr(), root.len() as u32) == 0 {
            return None;
        }
        if GetDriveTypeW(root.as_ptr()) == 4 { // DRIVE_REMOTE
            return Some(Volume { file_system: String::new(), keeps_permissions: true, remote: true });
        }
        let (mut flags, mut name) = (0u32, [0u16; 64]);
        if GetVolumeInformationW(root.as_ptr(), std::ptr::null_mut(), 0, std::ptr::null_mut(), std::ptr::null_mut(), &mut flags, name.as_mut_ptr(), name.len() as u32) == 0 {
            return None;
        }
        let length = name.iter().position(|&unit| unit == 0).unwrap_or(name.len());
        Some(Volume { file_system: String::from_utf16_lossy(&name[..length]), keeps_permissions: flags & 8 != 0, remote: false }) // FILE_PERSISTENT_ACLS
    }
}

/// Whether this process could write in a folder and change who may read it, asked by opening it for those and changing nothing. (`None` where it cannot be opened for looking at all.)
pub fn rights_on(folder: &Path) -> Option<(bool, bool)> {
    let open = |access: u32| {
        // SAFETY: the name ends in a zero; the handle is closed here.
        unsafe {
            let handle = CreateFileW(wide_path(folder).as_ptr(), access, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, std::ptr::null_mut());
            if handle == INVALID_HANDLE_VALUE {
                return false;
            }
            CloseHandle(handle);
            true
        }
    };
    open(FILE_READ_ATTRIBUTES).then(|| (open(FILE_ADD_FILE), open(WRITE_DAC)))
}

/// Why a command cannot be contained on this drive, whatever the folder: it would not reach it, or nothing there could keep it in.
fn drive_refusal(volume: &Volume, folder: &Path) -> Option<String> {
    if volume.remote {
        return Some(format!("{} is on a network drive, and a command has no network to reach it by: it runs only in a folder on a drive of this PC", folder.display()));
    }
    (!volume.keeps_permissions).then(|| format!("{} is on a drive with no file permissions ({}), so nothing there could keep a command inside its folder: it runs only in a folder on an NTFS drive", folder.display(), volume.file_system))
}

/// Where a path really leads, through links, junctions and short names. An ACE goes on what a path leads to, so that is what has to be judged: `C:\Documents and Settings\me` and
/// `C:\Users\me\Local Settings` are there on every Windows, and read as nothing in particular.
fn real(path: &Path) -> PathBuf {
    crate::safety::real_path(path).unwrap_or_else(|| path.to_path_buf())
}

/// The folders a command writing in would be writing over this app itself: what it runs from, and what it remembers, with the lists of what it has given.
fn own_folders() -> Vec<PathBuf> {
    let program = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.to_path_buf()));
    program.into_iter().chain(crate::workspace_grants::data_folder()).collect()
}

/// Why this folder cannot be the writable one: a command could then write over everything the person has.
pub fn refusal(workspace: &Path, home: Option<&Path>) -> Option<String> {
    refusal_with(workspace, home, &own_folders())
}

/// How a workspace is closed to commands, which decides what a model is told to do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Closed {
    /// Too wide to give away (a whole drive, the whole user folder): a project folder inside it can still be run in.
    Wide,
    /// A place keys are kept, and so is anything inside it.
    Keys,
    /// This app's own files, or a folder among them: what a command could write there runs outside the sandbox, and a folder made inside it is as closed.
    Own,
}

fn refusal_with(workspace: &Path, home: Option<&Path>, own: &[PathBuf]) -> Option<String> {
    closed_with(workspace, home, own).map(|(reason, _)| reason)
}

/// What a model is told, after why the workspace is closed: what can be done about it, and what only looks like it. A folder is made only when the person asked for something new; to look at
/// files nothing is made, since `read_file`, `list_files` and `search_files` are not refused.
fn advice(reason: &str, kind: Closed) -> String {
    match kind {
        Closed::Wide => format!("{reason}. Set cwd to a project folder that is already inside it: that folder alone can then be written in. To look at files, `read_file`, `list_files` and `search_files` are not refused. Make a new folder with create_file only when the user asked for something new to be made"),
        Closed::Keys => format!("{reason}, and no folder inside it is one to run in either. `read_file`, `list_files` and `search_files` can look there"),
        Closed::Own => format!("{reason}. A folder made inside it does not help, as a command there cannot read the rest of it: `read_file`, `list_files` and `search_files` can look at its files"),
    }
}

fn closed_with(workspace: &Path, home: Option<&Path>, own: &[PathBuf]) -> Option<(String, Closed)> {
    let really = real(workspace);
    for folder in [workspace, really.as_path()] {
        let place = key(folder);
        if folder.parent().is_none() || place.ends_with(':') {
            return Some((format!("{} is a whole drive, so a command runs only in a project folder inside it", workspace.display()), Closed::Wide));
        }
        if crate::safety::keys_folder(folder) {
            return Some((format!("{} is a place keys are kept, so a command does not run in it", workspace.display()), Closed::Keys));
        }
        // (a folder of the app's own stays closed whatever: what a command could write over in it, or rename away from under it with its
        // parent's leave, runs the next time with none of the sandbox. Writing a Deny on it would not hold against that.)
        if let Some(at) = own.iter().position(|folder| {
            let own = key(&real(folder));
            under(&own, &place) || under(&place, &own)
        }) {
            // (the first is what the app runs from: a copy run from elsewhere leaves this folder free; the rest are what it keeps)
            let way = if at == 0 { ". To work on a folder that holds a build of this app, run a copy of the app from another folder, as the one Dogmind's installer puts in %LOCALAPPDATA%\\Programs\\Dogmind" } else { "" };
            return Some((format!("{} holds this app's own files ({}), or lies among them, and what a command wrote there would run outside its sandbox: a command does not run in it{way}", workspace.display(), own[at].display()), Closed::Own));
        }
        if let Some(home) = home {
            if under(&key(home), &place) || under(&key(&real(home)), &place) {
                return Some((format!("{} holds your whole user folder, so a command runs only in a project folder inside it", workspace.display()), Closed::Wide));
            }
        }
    }
    None
}

/// The folder a command may write in. Normally the workspace; a workspace too wide to give away still has project folders in
/// it, and one of those is given when the command says it runs there, and nothing more. What a person keeps in the usual
/// places stays closed, and whatever the folder, the command has been through the approvals like any other. It is the folder itself that is given, wherever the path to it goes through.
pub fn writable_root(cwd: &Path, workspace: &Path, home: Option<&Path>) -> Result<PathBuf, String> {
    let (cwd, workspace, home) = (real(cwd), real(workspace), home.map(real));
    let (cwd, workspace, home) = (cwd.as_path(), workspace.as_path(), home.as_deref());
    let Some((reason, kind)) = closed_with(workspace, home, &own_folders()) else {
        if !under(&key(cwd), &key(workspace)) {
            return Err(format!("{} is outside the workspace: a command runs inside it. `read_file`, `list_files` and `search_files` can look there; running something there takes the user opening it as the workspace", cwd.display()));
        }
        return Ok(workspace.to_path_buf());
    };
    let advice = advice(&reason, kind);
    let (place, wide) = (key(cwd), key(workspace));
    if place == wide || !under(&place, &wide) {
        return Err(advice);
    }
    if refusal(cwd, home).is_some() {
        return Err(advice);
    }
    let first = place[wide.len()..].trim_start_matches('\\');
    let first = first.split('\\').next().unwrap_or("");
    if first.starts_with('.') || NOT_A_PROJECT.contains(&first) || crate::safety::keys_folder(cwd) {
        return Err(format!("{} is not a project folder. {advice}", cwd.display()));
    }
    // Directly under one of the usual places would not be caught above when the workspace is itself one level higher.
    if !cwd.is_dir() {
        return Err(format!("{} does not exist yet. Set cwd to a folder that exists; make it with create_file only when the user asked for something new to be made", cwd.display()));
    }
    Ok(cwd.to_path_buf())
}

/// The per-user folders to make readable: PATH's under the home, and the toolchain homes. Never the home itself.
pub fn toolchain_folders(path_variable: &str, home: &Path) -> Vec<PathBuf> {
    const TOO_WIDE: [&str; 3] = ["appdata", "appdata\\local", "appdata\\roaming"];
    let home_key = key(&real(home));
    let mut folders: Vec<PathBuf> = std::env::split_paths(path_variable)
        .filter(|folder| {
            // (as it would be opened: an entry that ends in `\.` is the home itself)
            if folder.components().any(|part| part == std::path::Component::ParentDir) {
                return false;
            }
            let entry = key(&real(&folder.components().collect::<PathBuf>()));
            let wide = entry.strip_prefix(&format!("{home_key}\\")).is_some_and(|inside| TOO_WIDE.contains(&inside));
            entry != home_key && under(&entry, &home_key) && !wide && !crate::safety::keys_folder(folder) && !entry.contains("\\.ssh")
        })
        .collect();
    folders.extend(TOOLCHAIN_HOMES.iter().map(|name| home.join(name)));
    let mut seen = HashSet::new();
    folders.retain(|folder| seen.insert(key(folder)));
    folders
}

/// PATH's folders outside the home that nothing lets a container read. Most of Program Files is open to every container; an
/// installer can close its own folder, and only an administrator can open it again.
pub fn closed_tool_folders() -> Vec<PathBuf> {
    let Some(path) = std::env::var_os("PATH") else { return Vec::new() };
    let home = std::env::var_os("USERPROFILE").map_or(String::new(), |home| key(Path::new(&home)));
    let system = std::env::var_os("SystemRoot").map_or(String::new(), |root| key(Path::new(&root)));
    let bundled = crate::bundled::root().is_some();
    // SAFETY: the SID lives as long as the process, and each call below gets the sizes it is given.
    unsafe {
        let Ok(sid) = profile_sid() else { return Vec::new() };
        let mut seen = HashSet::new();
        std::env::split_paths(&path)
            .filter(|folder| folder.is_dir() && seen.insert(key(folder)))
            .filter(|folder| !under(&key(folder), &home) && !under(&key(folder), &system))
            // What a project's commands would want; an unrelated program's folder stays as it is.
            .filter(|folder| DEVELOPER_TOOLS.iter().any(|tool| ["exe", "cmd"].iter().any(|ext| folder.join(format!("{tool}.{ext}")).is_file())))
            // (a person's own Node or Python is not what a command runs when these are beside the app)
            .filter(|folder| !bundled || !crate::bundled::NAMES.iter().any(|name| folder.join(name).is_file()))
            .filter(|folder| !open_to_container(folder, sid))
            .collect()
    }
}

/// Whether every container can read this already, as under Program Files, where a person who is not an administrator could not change that.
pub fn open_to_every_container(folder: &Path) -> bool {
    // SAFETY: the system's own well-known SID, freed by what makes it.
    unsafe {
        let text = wide("S-1-15-2-1");
        let mut every: PSID = std::ptr::null_mut();
        if ConvertStringSidToSidW(text.as_ptr(), &mut every) == 0 {
            return false;
        }
        let found = open_to_container(folder, every);
        LocalFree(every as HLOCAL);
        found
    }
}

/// Passing through the folders above, and nothing of what is in them.
pub(crate) unsafe fn grant_way_to(folder: &Path, sid: PSID) {
    for ancestor in folder.ancestors().skip(1).filter(|above| above.parent().is_some()) {
        let _ = grant(ancestor, sid, TRAVERSE, false);
    }
}

/// Whether the folder's own list lets in every container, or this one.
unsafe fn open_to_container(folder: &Path, sid: PSID) -> bool {
    let wanted = wide_path(folder);
    let (mut list, mut descriptor): (*mut ACL, PSECURITY_DESCRIPTOR) = (std::ptr::null_mut(), std::ptr::null_mut());
    if GetNamedSecurityInfoW(wanted.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), &mut list, std::ptr::null_mut(), &mut descriptor) != 0 {
        return true;
    }
    let everyone_text = wide("S-1-15-2-1");
    let mut everyone: PSID = std::ptr::null_mut();
    ConvertStringSidToSidW(everyone_text.as_ptr(), &mut everyone);
    let mut found = false;
    let mut information: ACL_SIZE_INFORMATION = std::mem::zeroed();
    if !list.is_null() && GetAclInformation(list, std::ptr::addr_of_mut!(information).cast(), std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32, AclSizeInformation) != 0 {
        for at in 0..information.AceCount {
            let mut entry: *mut std::ffi::c_void = std::ptr::null_mut();
            if GetAce(list, at, &mut entry) == 0 || entry.is_null() {
                continue;
            }
            let ace = entry as *const ACCESS_ALLOWED_ACE;
            if u32::from((*ace).Header.AceType) != ACCESS_ALLOWED_ACE_TYPE {
                continue;
            }
            let owner = std::ptr::addr_of!((*ace).SidStart) as PSID;
            if EqualSid(owner, sid) != 0 || (!everyone.is_null() && EqualSid(owner, everyone) != 0) {
                found = true;
                break;
            }
        }
    }
    LocalFree(everyone as HLOCAL);
    LocalFree(descriptor as HLOCAL);
    found
}

/// What opens those folders, to be run as an administrator: read and execute for this container, nothing else.
pub fn open_folders_command(folders: &[PathBuf]) -> Option<String> {
    // SAFETY: the text is the system's to free, and is copied before that.
    unsafe {
        let sid = profile_sid().ok()?;
        let mut text: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut text) == 0 {
            return None;
        }
        let length = (0..).take_while(|&at| *text.add(at) != 0).count();
        let name = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
        LocalFree(text as HLOCAL);
        let parts: Vec<String> = folders.iter().map(|folder| format!("icacls \"{}\" /grant *{name}:(OI)(CI)(RX)", folder.display())).collect();
        (!parts.is_empty()).then(|| parts.join(" & "))
    }
}

struct Sid(PSID);

// The SID is read-only memory for the life of the process.
unsafe impl Send for Sid {}
unsafe impl Sync for Sid {}

static PROFILE_SID: Mutex<Option<Sid>> = Mutex::new(None);
static GRANTED: Mutex<Option<HashSet<(String, u32, bool)>>> = Mutex::new(None);

pub(crate) unsafe fn profile_sid() -> Result<PSID, String> {
    let mut slot = PROFILE_SID.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(sid) = &*slot {
        return Ok(sid.0);
    }
    let (name, display, description) = (wide(PROFILE), wide("Dogmind commands"), wide("Where Dogmind runs the commands you approve"));
    let mut sid: PSID = std::ptr::null_mut();
    let created = CreateAppContainerProfile(name.as_ptr(), display.as_ptr(), description.as_ptr(), std::ptr::null(), 0, &mut sid);
    if created < 0 && DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) < 0 {
        return Err(format!("the AppContainer profile could not be made (0x{created:08x})"));
    }
    *slot = Some(Sid(sid));
    Ok(sid)
}

pub(crate) unsafe fn has_grant(path: &[u16], sid: PSID, mask: u32, inherit: bool) -> bool {
    let (mut dacl, mut descriptor): (*mut ACL, PSECURITY_DESCRIPTOR) = (std::ptr::null_mut(), std::ptr::null_mut());
    if GetNamedSecurityInfoW(path.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), &mut dacl, std::ptr::null_mut(), &mut descriptor) != 0 {
        return false;
    }
    let mut found = dacl.is_null();
    if !dacl.is_null() {
        for at in 0..u32::from((*dacl).AceCount) {
            let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
            if GetAce(dacl, at, &mut ace) == 0 {
                continue;
            }
            let entry = &*(ace as *const ACCESS_ALLOWED_ACE);
            if u32::from(entry.Header.AceType) != ACCESS_ALLOWED_ACE_TYPE {
                continue;
            }
            let inherits = u32::from(entry.Header.AceFlags) & (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) == (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE);
            let ace_sid = std::ptr::addr_of!(entry.SidStart) as PSID;
            if EqualSid(ace_sid, sid) != 0 && entry.Mask & mask == mask && (inherits || !inherit) {
                found = true;
                break;
            }
        }
    }
    LocalFree(descriptor as HLOCAL);
    found
}

unsafe fn write_ace(path: &Path, name: &[u16], sid: PSID, mask: u32, inherit: bool, mode: ACCESS_MODE) -> Result<(), String> {
    let (mut old, mut descriptor): (*mut ACL, PSECURITY_DESCRIPTOR) = (std::ptr::null_mut(), std::ptr::null_mut());
    let read = GetNamedSecurityInfoW(name.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), &mut old, std::ptr::null_mut(), &mut descriptor);
    if read != 0 {
        return Err(format!("{}: its access list cannot be read (error {read})", path.display()));
    }
    let mut access: EXPLICIT_ACCESS_W = std::mem::zeroed();
    access.grfAccessPermissions = mask;
    access.grfAccessMode = mode;
    access.grfInheritance = if inherit { SUB_CONTAINERS_AND_OBJECTS_INHERIT } else { NO_INHERITANCE };
    access.Trustee.TrusteeForm = TRUSTEE_IS_SID;
    access.Trustee.TrusteeType = TRUSTEE_IS_WELL_KNOWN_GROUP;
    access.Trustee.ptstrName = sid as *mut u16;
    let mut new: *mut ACL = std::ptr::null_mut();
    let merged = SetEntriesInAclW(1, &access, old, &mut new);
    let written = if merged != 0 {
        merged
    } else {
        SetNamedSecurityInfoW(name.as_ptr() as *mut u16, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), new, std::ptr::null())
    };
    if !new.is_null() {
        LocalFree(new as HLOCAL);
    }
    LocalFree(descriptor as HLOCAL);
    if written != 0 {
        return Err(format!("{}: access could not be set (error {written})", path.display()));
    }
    Ok(())
}

/// An access list that can be changed one exact ACE at a time, leaving all the others as they are. `SetEntriesInAcl` cannot: it merges what it is given with what is there for that
/// SID, and to revoke it takes away everything that SID has.
pub(crate) struct Dacl {
    aces: Vec<Vec<u8>>,
    revision: u32,
    control: u16,
}

impl Dacl {
    /// What is on the object itself. Not by `GetNamedSecurityInfo`, which for a list that says nothing of auto-inheritance, as `Documents` and much else have, works out what it
    /// should say by comparing with the parent: change the parent, and everything under it reads as keeping to itself with all its entries its own, while nothing on it has changed.
    pub(crate) unsafe fn read(path: &Path) -> Result<Dacl, String> {
        let memory = raw_descriptor(path)?;
        let descriptor = memory.as_ptr() as PSECURITY_DESCRIPTOR;
        let (mut control, mut version) = (0u16, 0u32);
        GetSecurityDescriptorControl(descriptor, &mut control, &mut version);
        let (mut present, mut dacl, mut defaulted): (BOOL, *mut ACL, BOOL) = (0, std::ptr::null_mut(), 0);
        GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted);
        let mut aces = Vec::new();
        let mut revision = ACL_REVISION as u32;
        if present != 0 && !dacl.is_null() {
            revision = u32::from((*dacl).AclRevision);
            for at in 0..u32::from((*dacl).AceCount) {
                let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
                if GetAce(dacl, at, &mut ace) != 0 {
                    let size = usize::from((*(ace as *const ACE_HEADER)).AceSize);
                    aces.push(std::slice::from_raw_parts(ace as *const u8, size).to_vec());
                }
            }
        }
        Ok(Dacl { aces, revision, control })
    }

    /// How an entry that came from the folder above reads on this object. On a list that takes part in auto-inheritance it is marked so. On one that says nothing of that it carries
    /// no mark: the kernel leaves it off what it hands down from such a folder, and what shows it is only `GetNamedSecurityInfo` working it out.
    pub(crate) fn flags_from_above(&self, folder: bool) -> u8 {
        let marked = if self.control & SE_DACL_AUTO_INHERITED != 0 { INHERITED_ACE as u8 } else { 0 };
        marked | if folder { (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8 } else { 0 }
    }

    /// Whether it has this from the folder above, in whichever form it came: what was made after the list changed may carry the mark while the list itself does not.
    pub(crate) unsafe fn from_above(&self, sid: PSID, mask: u32, folder: bool) -> Option<u8> {
        let plain = self.flags_from_above(folder) & !(INHERITED_ACE as u8);
        let marked = plain | INHERITED_ACE as u8;
        if self.has_exact(sid, mask, marked) {
            Some(marked)
        } else if self.control & SE_DACL_AUTO_INHERITED == 0 && self.has_exact(sid, mask, plain) {
            Some(plain)
        } else {
            None
        }
    }

    pub(crate) fn protected(&self) -> bool {
        self.control & SE_DACL_PROTECTED != 0
    }

    /// Whether this one allows `sid` exactly this, with exactly these flags, and was not inherited.
    unsafe fn is_exact(ace: &[u8], sid: PSID, mask: u32, flags: u8) -> bool {
        Dacl::is_allowed_for(ace, sid) && ace[1] == flags && u32::from_ne_bytes([ace[4], ace[5], ace[6], ace[7]]) == mask
    }

    unsafe fn is_allowed_for(ace: &[u8], sid: PSID) -> bool {
        u32::from(ace[0]) == ACCESS_ALLOWED_ACE_TYPE && EqualSid(ace[8..].as_ptr() as PSID, sid) != 0
    }

    pub(crate) unsafe fn has_exact(&self, sid: PSID, mask: u32, flags: u8) -> bool {
        self.aces.iter().any(|ace| Dacl::is_exact(ace, sid, mask, flags))
    }

    unsafe fn allowed_ace(sid: PSID, mask: u32, flags: u8) -> Vec<u8> {
        let length = GetLengthSid(sid) as usize;
        let size = 8 + length;
        let mut ace = vec![0u8; size];
        ace[0] = ACCESS_ALLOWED_ACE_TYPE as u8;
        ace[1] = flags;
        ace[2..4].copy_from_slice(&(size as u16).to_ne_bytes());
        ace[4..8].copy_from_slice(&mask.to_ne_bytes());
        std::ptr::copy_nonoverlapping(sid as *const u8, ace[8..].as_mut_ptr(), length);
        ace
    }

    /// With one more, where the canonical order has it: after this object's own denials.
    pub(crate) unsafe fn with_exact(&self, sid: PSID, mask: u32, flags: u8) -> Dacl {
        let place = self.aces.iter().position(|ace| u32::from(ace[0]) != ACCESS_DENIED_ACE_TYPE || u32::from(ace[1]) & INHERITED_ACE != 0).unwrap_or(self.aces.len());
        let mut aces = self.aces.clone();
        aces.insert(place, Dacl::allowed_ace(sid, mask, flags));
        Dacl { aces, revision: self.revision, control: self.control }
    }

    /// With one more that reads as inherited, where a folder's own would have put it: after what this object has of itself, and after inherited denials.
    pub(crate) unsafe fn with_inherited(&self, sid: PSID, mask: u32, flags: u8) -> Dacl {
        let place = self.aces.iter().position(|ace| u32::from(ace[1]) & INHERITED_ACE != 0 && u32::from(ace[0]) != ACCESS_DENIED_ACE_TYPE).unwrap_or(self.aces.len());
        let mut aces = self.aces.clone();
        aces.insert(place, Dacl::allowed_ace(sid, mask, flags));
        Dacl { aces, revision: self.revision, control: self.control }
    }

    pub(crate) unsafe fn without_exact(&self, sid: PSID, mask: u32, flags: u8) -> Dacl {
        Dacl { aces: self.aces.iter().filter(|ace| !Dacl::is_exact(ace, sid, mask, flags)).cloned().collect(), revision: self.revision, control: self.control }
    }

    /// Keeping its own list from now on, with what it inherited written into it, and nothing at all for this SID.
    pub(crate) unsafe fn as_explicit_and_protected(&self, sid: PSID) -> Dacl {
        let aces = self.aces.iter().filter(|ace| !Dacl::is_allowed_for(ace, sid)).map(|ace| {
            let mut ace = ace.clone();
            ace[1] &= !(INHERITED_ACE as u8);
            ace
        }).collect();
        Dacl { aces, revision: self.revision, control: self.control | SE_DACL_PROTECTED }
    }

    /// As an ACL for a call that takes one. In `u32`s: an ACL must be aligned.
    pub(crate) unsafe fn to_acl(&self) -> Vec<u32> {
        let size = std::mem::size_of::<ACL>() + self.aces.iter().map(Vec::len).sum::<usize>();
        let mut memory = vec![0u32; size.div_ceil(4)];
        let acl = memory.as_mut_ptr() as *mut ACL;
        InitializeAcl(acl, (memory.len() * 4) as u32, self.revision);
        for ace in &self.aces {
            AddAce(acl, self.revision, u32::MAX, ace.as_ptr().cast(), ace.len() as u32);
        }
        memory
    }

    /// On this object alone. What is under a folder keeps what it inherited from before: a walk through a tree is what takes the time.
    pub(crate) unsafe fn write_alone(&self, path: &Path) -> Result<(), String> {
        let mut acl = self.to_acl();
        let mut alone: SECURITY_DESCRIPTOR = std::mem::zeroed();
        let place = std::ptr::addr_of_mut!(alone) as PSECURITY_DESCRIPTOR;
        let kept = SE_DACL_PROTECTED | SE_DACL_AUTO_INHERITED;
        // (asked for as well as said, or the kernel leaves off the flag that says this list takes part in inheritance; one without it stays without)
        let flags = (self.control & kept) | if self.control & SE_DACL_AUTO_INHERITED != 0 { SE_DACL_AUTO_INHERIT_REQ } else { 0 };
        if InitializeSecurityDescriptor(place, 1) == 0 || SetSecurityDescriptorDacl(place, 1, acl.as_mut_ptr().cast(), 0) == 0 || SetSecurityDescriptorControl(place, kept | SE_DACL_AUTO_INHERIT_REQ, flags) == 0 {
            return Err(last_error(&path.display().to_string()));
        }
        set_on_handle(path, place, self.protected())
    }
}

/// A security descriptor on this object alone, by a handle: `SetFileSecurity` drops the bit that makes a list keep to itself, and whatever walks a tree to do it takes minutes in some folders.
unsafe fn set_on_handle(path: &Path, descriptor: PSECURITY_DESCRIPTOR, protected: bool) -> Result<(), String> {
    let name = wide_path(path);
    let handle = Handle(CreateFileW(name.as_ptr(), WRITE_DAC | READ_CONTROL, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, std::ptr::null_mut()));
    if handle.0 == INVALID_HANDLE_VALUE {
        return Err(last_error(&path.display().to_string()));
    }
    let keeps = if protected { PROTECTED_DACL_SECURITY_INFORMATION } else { UNPROTECTED_DACL_SECURITY_INFORMATION };
    if SetKernelObjectSecurity(handle.0, DACL_SECURITY_INFORMATION | keeps, descriptor) == 0 {
        return Err(last_error(&path.display().to_string()));
    }
    Ok(())
}

/// Where something recorded by its identity is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Located {
    At(PathBuf),
    /// Its drive is there, and it is not on it: deleted, or moved to another drive, which is a copy.
    Gone,
    /// The drive it was on is not here to ask.
    DriveAway,
}

unsafe fn id_of_handle(handle: HANDLE) -> Option<FILE_ID_INFO> {
    let mut info: FILE_ID_INFO = std::mem::zeroed();
    (GetFileInformationByHandleEx(handle, FileIdInfo, std::ptr::addr_of_mut!(info).cast(), std::mem::size_of::<FILE_ID_INFO>() as u32) != 0).then_some(info)
}

unsafe fn open_for_id(path: &Path) -> Handle {
    let name = wide_path(path);
    Handle(CreateFileW(name.as_ptr(), FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, std::ptr::null_mut()))
}

/// What a file or folder is, whatever it is called and wherever on its drive it goes: its drive's serial number and its own number there.
pub(crate) fn file_id(path: &Path) -> Option<String> {
    // SAFETY: the handle is this function's, and `info` is as long as the call is told.
    unsafe {
        let handle = open_for_id(path);
        if handle.0 == INVALID_HANDLE_VALUE {
            return None;
        }
        let info = id_of_handle(handle.0)?;
        let number: String = info.FileId.Identifier.iter().map(|byte| format!("{byte:02x}")).collect();
        Some(format!("{:016x}:{number}", info.VolumeSerialNumber))
    }
}

/// Where what had this identity is now, looking first where it was. `was_at` names the drive.
pub(crate) fn locate(id: &str, was_at: &Path) -> Located {
    if file_id(was_at).as_deref() == Some(id) {
        return Located::At(was_at.to_path_buf());
    }
    let (Some((serial, number)), Some(root)) = (id.split_once(':'), was_at.ancestors().last()) else { return Located::Gone };
    // SAFETY: the handles are this function's, the descriptor outlives the call, and the buffer is as long as the call is told.
    unsafe {
        let drive = open_for_id(root);
        if drive.0 == INVALID_HANDLE_VALUE {
            return Located::DriveAway;
        }
        let Some(info) = id_of_handle(drive.0) else { return Located::DriveAway };
        if format!("{:016x}", info.VolumeSerialNumber) != serial {
            return Located::DriveAway;
        }
        let mut wanted: FILE_ID_128 = std::mem::zeroed();
        for (at, slot) in wanted.Identifier.iter_mut().enumerate() {
            let Some(byte) = number.get(at * 2..at * 2 + 2).and_then(|text| u8::from_str_radix(text, 16).ok()) else { return Located::Gone };
            *slot = byte;
        }
        let descriptor = FILE_ID_DESCRIPTOR { dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32, Type: ExtendedFileIdType, Anonymous: FILE_ID_DESCRIPTOR_0 { ExtendedFileId: wanted } };
        let found = Handle(OpenFileById(drive.0, &descriptor, FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), FILE_FLAG_BACKUP_SEMANTICS));
        if found.0 == INVALID_HANDLE_VALUE {
            return Located::Gone;
        }
        let mut text = vec![0u16; 4096];
        let length = GetFinalPathNameByHandleW(found.0, text.as_mut_ptr(), text.len() as u32, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS) as usize;
        if length == 0 || length >= text.len() {
            return Located::Gone;
        }
        let path = String::from_utf16_lossy(&text[..length]);
        Located::At(PathBuf::from(path.strip_prefix("\\\\?\\").unwrap_or(&path)))
    }
}

/// The security descriptor on the object itself, in `u32`s as it must be aligned.
unsafe fn raw_descriptor(path: &Path) -> Result<Vec<u32>, String> {
    let name = wide_path(path);
    let mut size = 0u32;
    GetFileSecurityW(name.as_ptr(), DACL_SECURITY_INFORMATION, std::ptr::null_mut(), 0, &mut size);
    if size == 0 {
        return Err(last_error(&path.display().to_string()));
    }
    let mut memory = vec![0u32; (size as usize).div_ceil(4)];
    if GetFileSecurityW(name.as_ptr(), DACL_SECURITY_INFORMATION, memory.as_mut_ptr().cast(), size, &mut size) == 0 {
        return Err(last_error(&path.display().to_string()));
    }
    Ok(memory)
}

/// One exact ACE more on this object alone.
pub(crate) unsafe fn add_exact_ace(path: &Path, sid: PSID, mask: u32, flags: u8) -> Result<(), String> {
    Dacl::read(path)?.with_exact(sid, mask, flags).write_alone(path)
}

/// A path's access list in SDDL, as it is on the object at this moment.
pub(crate) fn dacl_text(path: &Path) -> Result<String, String> {
    // SAFETY: what each call is given lives until it returns, and the system's memory is freed once copied.
    unsafe {
        let memory = raw_descriptor(path)?;
        let mut text: *mut u16 = std::ptr::null_mut();
        if ConvertSecurityDescriptorToStringSecurityDescriptorW(memory.as_ptr() as PSECURITY_DESCRIPTOR, SDDL_REVISION_1, DACL_SECURITY_INFORMATION, &mut text, std::ptr::null_mut()) == 0 {
            return Err(last_error("its access list"));
        }
        let length = (0..).take_while(|&at| *text.add(at) != 0).count();
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
        LocalFree(text as HLOCAL);
        Ok(out)
    }
}

/// Puts back on this object alone an access list as [`dacl_text`] gave it.
pub(crate) fn restore_dacl_text(path: &Path, text: &str) -> Result<(), String> {
    // SAFETY: what each call is given lives until it returns, and the system's memory is freed once used.
    unsafe {
        let sddl = wide(text);
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, std::ptr::null_mut()) == 0 {
            return Err(last_error("its access list"));
        }
        let (mut control, mut version) = (0u16, 0u32);
        GetSecurityDescriptorControl(descriptor, &mut control, &mut version);
        // (as in `write_alone`)
        if control & SE_DACL_AUTO_INHERITED != 0 {
            SetSecurityDescriptorControl(descriptor, SE_DACL_AUTO_INHERIT_REQ, SE_DACL_AUTO_INHERIT_REQ);
        }
        let done = set_on_handle(path, descriptor, control & SE_DACL_PROTECTED != 0);
        LocalFree(descriptor as HLOCAL);
        done
    }
}

/// The container's SID in its text form.
#[cfg(test)]
pub(crate) fn profile_sid_text() -> String {
    // SAFETY: the text is the system's to free, and is copied before that.
    unsafe {
        let sid = profile_sid().unwrap();
        let mut text: *mut u16 = std::ptr::null_mut();
        ConvertSidToStringSidW(sid, &mut text);
        let length = (0..).take_while(|&at| *text.add(at) != 0).count();
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
        LocalFree(text as HLOCAL);
        out
    }
}

/// One of the lists of what was given, whole or not at all, before what it says is done. Flushed first: after a power cut a rename can stand on a file with nothing in it, and a list with
/// nothing in it is folders left open with no one knowing.
pub(crate) fn write_list(file: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let partial = file.with_extension("partial");
    let written = File::create(&partial).and_then(|mut out| {
        std::io::Write::write_all(&mut out, text.as_bytes())?;
        out.sync_all()
    });
    written.map_err(|error| format!("{}: {error}", partial.display()))?;
    std::fs::rename(&partial, file).map_err(|error| format!("{}: {error}", file.display()))
}

/// A list that cannot be read is kept beside where it was: the next thing written there would otherwise be all that is left of what it said.
pub(crate) fn set_aside(file: &Path) -> PathBuf {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_millis());
    let kept = file.with_extension(format!("damaged-{stamp}"));
    let _ = std::fs::rename(file, &kept);
    kept
}

/// That list next time, by looking: something may have taken one back.
pub(crate) fn forget_grants() {
    *GRANTED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

/// This object's list, with exactly this for the container and nothing else for it, keeping its own list from now on. Windows does not honour a deny ACE for a container's SID: what
/// keeps a container out is not being let in. `walk`: what is under it follows.
unsafe fn limit_to(root: &Path, path: &Path, sid: PSID, keep: &[(u32, u8)], walk: bool) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let dacl = Dacl::read(path)?;
    let has = dacl.aces.iter().filter(|ace| Dacl::is_allowed_for(ace, sid)).count();
    if dacl.protected() && has == keep.len() && keep.iter().all(|(mask, flags)| dacl.has_exact(sid, *mask, *flags)) {
        return Ok(());
    }
    // (what is already written down of one that keeps its own list stays: that is its list from before this)
    crate::workspace_grants::before_guarding(root, path, dacl_text(path)?, dacl.protected())?;
    let mut limited = dacl.as_explicit_and_protected(sid);
    for (mask, flags) in keep {
        limited = limited.with_exact(sid, *mask, *flags);
    }
    if !walk {
        return limited.write_alone(path);
    }
    let (name, mut acl) = (wide_path(path), limited.to_acl());
    let written = SetNamedSecurityInfoW(name.as_ptr() as *mut u16, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), acl.as_mut_ptr().cast(), std::ptr::null());
    if written != 0 {
        return Err(format!("{}: access could not be set (error {written})", path.display()));
    }
    Ok(())
}

/// Whether this is itself a link or a junction, looked at and not gone through: `is_symlink` is true of a junction on Windows too.
fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|what| what.file_type().is_symlink())
}

/// What is said of a link that was met where the app writes or deletes as the person. A command in the sandbox can make a junction to anything it may write, such as another repository's `.git`
/// in the same workspace or in another one, and what the app does there when the command ends would be done to that repository.
fn link_refusal(link: &Path) -> String {
    format!("{} is a link, which the app does not go through, so it cannot be kept from what a command would plant there to run outside its sandbox, and nothing was changed through it. No command runs in this workspace until the link is replaced with the folder itself", link.display())
}

/// The first of these that is a link, as a refusal.
fn refuse_links(paths: &[&Path]) -> Result<(), String> {
    paths.iter().find(|path| is_link(path)).map_or(Ok(()), |link| Err(link_refusal(link)))
}

/// What a command must not change in a repository: a hook, or a line of config, runs outside the sandbox the next time a person or this app uses git there. Looked at before each
/// command, as the person's own git writes `config` anew, inheriting. Commits, branches and the rest go on as they do. The folder itself cannot be deleted or renamed by the
/// container either, or a command could put another in its place. What a command makes of a repository it has just created is not seen until the next command starts.
pub(crate) unsafe fn guard_repository(root: &Path, git: &Path, sid: PSID) -> Result<(), String> {
    let Ok(what) = std::fs::symlink_metadata(git) else { return Ok(()) };
    // A link is not gone through: an ACE would land on whatever it leads to, the person's own `config` or `hooks` anywhere, and that would be opened to commands.
    refuse_links(&[git, &git.join("config"), &git.join("hooks")])?;
    // A worktree's or a submodule's: one line, which says where git's folder is. A command that rewrites it has a repository of its own making, hooks and all.
    if !what.is_dir() {
        return limit_to(root, git, sid, &[(READ_EXECUTE, 0)], false);
    }
    // In a repository's own `.git` this has no business: only a worktree's folder under `worktrees` has one. It tells git to read its config and hooks from wherever it says, past all of this.
    // Taken away before each command: the folder has to stay open for git's own new files, so nothing can stop it being made.
    let elsewhere = git.join("commondir");
    if std::fs::symlink_metadata(&elsewhere).is_ok() && std::fs::remove_file(&elsewhere).is_err() {
        return Err(format!("{} would send git elsewhere for its config, and could not be taken away: no command is run until it has been", elsewhere.display()));
    }
    let everything = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;
    limit_to(root, git, sid, &[(READ_WRITE_EXECUTE & !DELETE, 0), (READ_WRITE_EXECUTE, everything | INHERIT_ONLY_ACE as u8)], false)?;
    limit_to(root, &git.join("config"), sid, &[(READ_EXECUTE, 0)], false)?;
    limit_to(root, &git.join("hooks"), sid, &[(READ_EXECUTE, everything)], true)
}

/// How far under the workspace a repository is looked for. A hook in one deeper or in what is skipped runs only for a person who goes there and uses git.
const REPOSITORY_DEPTH: usize = 8;
const MOST_FOLDERS_LOOKED_AT: usize = 4000;
const MOST_REPOSITORIES: usize = 64;
const NO_REPOSITORIES_HERE: [&str; 6] = ["node_modules", "target", ".venv", "venv", "__pycache__", crate::bundled::PY_DEPS];

/// What git would use in each repository under a folder: a `.git`, or a folder that is a bare repository, which git takes as one when it is run there. The nearest first, with what a look of this
/// size finds. It is done before and after every command.
fn repositories_under(root: &Path) -> Vec<PathBuf> {
    let (mut found, mut level, mut looked) = (Vec::new(), vec![root.to_path_buf()], 0usize);
    for _ in 0..=REPOSITORY_DEPTH {
        let mut next = Vec::new();
        for folder in &level {
            looked += 1;
            if looked > MOST_FOLDERS_LOOKED_AT || found.len() >= MOST_REPOSITORIES {
                return found;
            }
            let (mut inner, mut bare_marks) = (Vec::new(), 0);
            for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                // (an entry's own type, which does not follow a link: a junction is no folder here, and is never gone down into. A `.git` that is a link is found all the same, for the
                // guard to refuse it and for the disarming not to go through it)
                let folder_there = entry.file_type().is_ok_and(|kind| kind.is_dir());
                bare_marks += usize::from(if folder_there { matches!(name.as_str(), "objects" | "refs") } else { name == "head" });
                if name == ".git" {
                    found.push(entry.path());
                } else if entry.file_type().is_ok_and(|kind| kind.is_dir()) && !NO_REPOSITORIES_HERE.iter().any(|skipped| name.eq_ignore_ascii_case(skipped)) {
                    inner.push(entry.path());
                }
            }
            if bare_marks == 3 {
                found.push(folder.clone());
            } else {
                next.append(&mut inner);
            }
        }
        level = next;
    }
    found
}

/// The repositories known of in each workspace: those there at the first command, and those made since and looked at.
static KNOWN_REPOSITORIES: Mutex<Option<HashMap<String, HashSet<String>>>> = Mutex::new(None);

/// What the app's own commands have done in each workspace: how many are at work (a background job is one for as long as it runs), and whether any has run since the last look at the
/// repositories there. A repository that first shows up when none has is the person's: they made it themselves, as they do in their own terminal while a chat is open.
static COMMANDS: Mutex<Option<HashMap<String, Commands>>> = Mutex::new(None);

#[derive(Default)]
struct Commands {
    running: usize,
    ran_since_look: bool,
}

fn commands_of_workspaces() -> std::sync::MutexGuard<'static, Option<HashMap<String, Commands>>> {
    COMMANDS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A command of the app at work in a workspace: from after it was made ready to when what it started has ended and what it made has been looked at.
struct AtWork {
    place: String,
}

impl AtWork {
    fn start(root: &Path) -> AtWork {
        let place = key(root);
        let mut all = commands_of_workspaces();
        let commands = all.get_or_insert_with(HashMap::new).entry(place.clone()).or_default();
        commands.running += 1;
        commands.ran_since_look = true;
        AtWork { place }
    }
}

impl Drop for AtWork {
    fn drop(&mut self) {
        if let Some(commands) = commands_of_workspaces().as_mut().and_then(|all| all.get_mut(&self.place)) {
            commands.running = commands.running.saturating_sub(1);
        }
    }
}

/// Whether a command of the app was at work in this workspace, or ran, since the last look: what shows up in it now may be its making. Taken as a look at it: it is asked once for each.
fn commands_since_the_last_look(root: &Path) -> bool {
    let mut all = commands_of_workspaces();
    let commands = all.get_or_insert_with(HashMap::new).entry(key(root)).or_default();
    let at_work = commands.running > 0 || commands.ran_since_look;
    commands.ran_since_look = false;
    at_work
}

/// Which look at the repositories of a workspace this is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Look {
    /// As a command is made ready. A new repository here is a command's only if one was at work since the last look (a background job, or another conversation's command); otherwise the
    /// person made it, and it is guarded like every other, and nothing is taken out of it.
    BeforeACommand,
    /// As a command has ended and nothing it started is left: what is new was made while it ran, by it or by something at work beside it.
    AfterACommand,
}

/// Of these, those not known of until now, which are from then on. On a first look at a workspace nothing is new: what is there is the person's, hooks and all.
fn newly_made(root: &Path, found: &[PathBuf]) -> Vec<PathBuf> {
    let mut known = KNOWN_REPOSITORIES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let known = known.get_or_insert_with(HashMap::new);
    let first_look = !known.contains_key(&key(root));
    let here = known.entry(key(root)).or_default();
    found.iter().filter(|git| here.insert(key(git)) && !first_look).cloned().collect()
}

/// The app itself has just made this one, and what it writes there after this has been asked about.
pub fn note_made_by_the_app(root: &Path, folder: &Path) {
    let mut known = KNOWN_REPOSITORIES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // (in a workspace not looked at yet there is nothing to say: the first look takes all that is there as it is)
    if let Some(here) = known.get_or_insert_with(HashMap::new).get_mut(&key(root)) {
        here.insert(key(&folder.join(".git")));
    }
}

/// What of a new repository's config may stay: what git itself writes, and who commits. Anything else in a config can name a program to run (`core.fsmonitor`, `core.sshCommand`, an alias, a filter,
/// a diff driver, `include.path`...), and a list of those would have to know them all.
fn config_line_stays(section: &str, line: &str) -> bool {
    let name = line.split('=').next().unwrap_or("").trim().to_ascii_lowercase();
    let section = section.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    match section.as_str() {
        // (`worktree` is what a submodule's says, and runs nothing)
        "core" => ["repositoryformatversion", "filemode", "bare", "logallrefupdates", "symlinks", "ignorecase", "precomposeunicode", "autocrlf", "eol", "safecrlf", "longpaths", "quotepath", "worktree"].contains(&name.as_str()),
        "user" => true,
        // (not `worktreeConfig`, which makes a file that a command can still write into a second config)
        "extensions" => ["objectformat", "refstorage"].contains(&name.as_str()),
        "remote" => ["url", "pushurl", "fetch", "prune", "tagopt"].contains(&name.as_str()),
        "branch" => ["remote", "merge", "rebase"].contains(&name.as_str()),
        "init" => name == "defaultbranch",
        "submodule" => ["url", "active"].contains(&name.as_str()),
        _ => false,
    }
}

/// A config's text with only what may stay, and the names of what was taken out. A section's name stays, or what is said under it would land in another.
fn config_without_programs(text: &str) -> (String, Vec<String>) {
    let (mut kept, mut gone, mut section, mut going_on, mut dropping) = (String::new(), Vec::new(), String::new(), false, false);
    for line in text.split_inclusive('\n') {
        let bare = line.trim();
        let continues = bare.ends_with('\\');
        if going_on {
            if !dropping {
                kept.push_str(line);
            }
            going_on = continues;
            continue;
        }
        if bare.starts_with('[') && !bare.contains(']') {
            // (a section's name cut off is nothing git would read: the whole of what follows is not to be trusted)
            gone.push(bare.to_string());
            break;
        }
        if bare.starts_with('[') {
            let (name, rest) = bare[1..].split_once(']').unwrap_or((&bare[1..], ""));
            section = name.to_string();
            kept.push_str(&format!("[{name}]\n"));
            // (git takes a setting on the section's own line too)
            let rest = rest.trim();
            if !rest.is_empty() && !rest.starts_with(['#', ';']) {
                if config_line_stays(&section, rest) {
                    kept.push_str(&format!("\t{rest}\n"));
                } else {
                    gone.push(format!("{}.{}", section.split_whitespace().next().unwrap_or(""), rest.split('=').next().unwrap_or("").trim()));
                }
            }
            going_on = false;
            dropping = false;
            continue;
        }
        if bare.is_empty() || bare.starts_with(['#', ';']) {
            kept.push_str(line);
            continue;
        }
        going_on = continues;
        dropping = !config_line_stays(&section, bare);
        if dropping {
            gone.push(format!("{}.{}", section.split_whitespace().next().unwrap_or(""), bare.split('=').next().unwrap_or("").trim()));
        } else {
            kept.push_str(line);
        }
    }
    (kept, gone)
}

/// Where a path that a `.git` says git's folder is leads, if that is a folder in the workspace: `Ok(None)` for a place that is not in it (a command could not have written anything there), or
/// not a folder. A path that is spelled as in the workspace and leads out through a link is refused, and so is one whose end cannot be found: a link in the workspace is still a link out of it, and
/// what is done there is done as the person.
fn folder_in_the_workspace(spelled: &Path, root: &Path) -> Result<Option<PathBuf>, String> {
    if !crate::safety::inside(root, spelled) {
        return Ok(None);
    }
    let real_root = crate::safety::real_path(root).unwrap_or_else(|| root.to_path_buf());
    match crate::safety::real_path(spelled) {
        Some(real) if crate::safety::inside(&real_root, &real) => Ok(real.is_dir().then_some(real)),
        _ => Err(format!("{} is in the workspace by its spelling, and leads out of it through a link, which the app does not go through, so it cannot be kept from what a command would plant there to run outside its sandbox, and nothing was changed through it. No command runs in this workspace until the link is replaced with the folder itself", spelled.display())),
    }
}

/// What is in the place a `.git` file points to, where that is in the workspace: outside it a command could not have written anything. Where it really is, which is where the app then writes.
fn git_folder_of(git_file: &Path, root: &Path) -> Result<Option<PathBuf>, String> {
    // (a `.git` that is a link was refused before this: what is read here is a file of the workspace's own)
    let Ok(text) = std::fs::read_to_string(git_file) else { return Ok(None) };
    let (Some(named), Some(beside)) = (text.trim().strip_prefix("gitdir:").map(str::trim), git_file.parent()) else { return Ok(None) };
    folder_in_the_workspace(&crate::safety::resolve(beside, named), root)
}

/// The repository that a worktree's folder says it is part of, where it is in the workspace. Read only from a file that is one: a link there is not read through either.
fn common_folder_of(folder: &Path, root: &Path) -> Result<Option<PathBuf>, String> {
    let said = folder.join("commondir");
    refuse_links(&[&said])?;
    let Ok(text) = std::fs::read_to_string(&said) else { return Ok(None) };
    folder_in_the_workspace(&crate::safety::resolve(folder, text.trim()), root)
}

//// What was taken out of one repository that a command made, and where it was put.
#[derive(Default)]
struct Taken {
    /// What it was, as a person reads it: `hooks/pre-commit`, `core.fsmonitor`.
    names: Vec<String>,
    /// The folders, each inside the git folder it was taken from (where git reads nothing), that hold it.
    kept_in: Vec<PathBuf>,
}

impl Taken {
    /// What is said of it, for the person and for the model: where it was made, and where to find what was moved.
    fn said_of(&self, place: &str) -> String {
        let names = self.names.join(", ");
        if self.kept_in.is_empty() {
            return format!("In {place}, made by a command, this was removed, as it would have run programs outside the sandbox: {names}.");
        }
        let folders = self.kept_in.iter().map(|folder| folder.display().to_string()).collect::<Vec<_>>().join(", ");
        format!("In {place}, made by a command, hooks and config lines that would run programs outside the sandbox were moved to {folders}: {names}.")
    }
}

/// What cannot be removed by itself, as a link is: the link, and never what it leads to.
fn remove_link(path: &Path) -> bool {
    std::fs::remove_dir(path).or_else(|_| std::fs::remove_file(path)).is_ok()
}

/// A place for what is taken out of a git folder, inside it: git reads nothing there, so none of it runs. One for each time, and never one that is already there.
fn make_room_in(git_folder: &Path) -> Option<PathBuf> {
    let time = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    for number in 1..=1000 {
        let folder = git_folder.join(if number == 1 { format!("dogmind-disarmed-{time}") } else { format!("dogmind-disarmed-{time}-{number}") });
        match std::fs::create_dir(&folder) {
            Ok(()) => return Some(folder),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// Every link under a folder taken away, each by itself and never through: what a command made is moved with the folder it is in, and not to be left as a way to somewhere else.
fn remove_links_inside(folder: &Path) {
    for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
        let kind = entry.file_type().ok();
        if kind.is_some_and(|kind| kind.is_symlink()) {
            remove_link(&entry.path());
        } else if kind.is_some_and(|kind| kind.is_dir()) {
            remove_links_inside(&entry.path());
        }
    }
}

/// Takes `from` to `to`, a file or a folder of one volume as a git folder's is. Where a move cannot be done a file is copied and then deleted.
fn move_to(from: &Path, to: &Path) -> bool {
    if std::fs::rename(from, to).is_ok() {
        return true;
    }
    from.is_file() && std::fs::copy(from, to).is_ok() && std::fs::remove_file(from).is_ok()
}

/// Writes down where a link that is taken away led, beside what else is put aside, so that nothing the person pointed it at is lost track of.
fn write_down_link(room: Option<&Path>, link: &Path, name: &str) {
    use std::io::Write;
    if let (Some(room), Ok(target)) = (room, std::fs::read_link(link)) {
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(room.join("links.txt")) {
            let _ = writeln!(file, "{name} -> {}", target.display());
        }
    }
}

/// What is taken out of one git folder, put aside inside it as it is: its hooks other than the samples, and its config as it was, before it is written again without what names a program.
/// A link where a hook or the config should be is taken away, and where it led is written down beside the rest.
fn disarm_folder(folder: &Path, taken: &mut Taken) {
    // (made when first needed: a folder with nothing taken out of it gets none)
    let mut room: Option<Option<PathBuf>> = None;
    let mut room_for = |taken: &mut Taken| -> Option<PathBuf> {
        room.get_or_insert_with(|| {
            let made = make_room_in(folder);
            if let Some(made) = &made {
                taken.kept_in.push(made.clone());
            }
            made
        })
        .clone()
    };
    let hooks = folder.join("hooks");
    if is_link(&hooks) {
        write_down_link(room_for(taken).as_deref(), &hooks, "hooks");
        if remove_link(&hooks) {
            taken.names.push("a link in place of its hooks".to_string());
        }
    } else {
        for entry in std::fs::read_dir(&hooks).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".sample") {
                continue;
            }
            // (an entry's own type, which is a link's own and not what it leads to; the links in a folder are taken away before it is moved, and not gone through)
            let (path, kind) = (entry.path(), entry.file_type().ok());
            if kind.is_some_and(|kind| kind.is_symlink()) {
                write_down_link(room_for(taken).as_deref(), &path, &format!("hooks/{name}"));
                if remove_link(&path) {
                    taken.names.push(format!("hooks/{name} (a link)"));
                }
                continue;
            }
            let is_folder = kind.is_some_and(|kind| kind.is_dir());
            if is_folder {
                remove_links_inside(&path);
            }
            let moved = room_for(taken).map(|room| room.join("hooks")).is_some_and(|aside| std::fs::create_dir_all(&aside).is_ok() && move_to(&path, &aside.join(&name)));
            if moved {
                taken.names.push(format!("hooks/{name}"));
            } else if (if is_folder { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) }).is_ok() {
                // (it could not be put aside, and a hook that runs is not left in its place)
                taken.names.push(format!("hooks/{name} (removed: it could not be put aside)"));
            }
        }
    }
    let config = folder.join("config");
    if is_link(&config) {
        write_down_link(room_for(taken).as_deref(), &config, "config");
        if remove_link(&config) {
            taken.names.push("a link in place of its config".to_string());
        }
        return;
    }
    // (git does not ask that it be UTF-8)
    let Ok(bytes) = std::fs::read(&config) else { return };
    let (kept, gone) = config_without_programs(&String::from_utf8_lossy(&bytes));
    if gone.is_empty() {
        return;
    }
    // The config as it was, byte for byte, before it is written again.
    let saved = room_for(taken).is_some_and(|room| std::fs::write(room.join("config"), &bytes).is_ok());
    if std::fs::write(&config, kept).is_ok() {
        if saved {
            taken.names.extend(gone);
        } else {
            taken.names.extend(gone.into_iter().map(|name| format!("{name} (removed: the config could not be put aside)")));
        }
    }
}

/// Takes out of a repository a command made what would run as the person the next time git is used there: its hooks, and what its config names. Guarding it first would keep later commands from
/// doing the same and leave what this one already did. `known`: repositories that were there before it, which are the person's and are not touched, whatever a worktree says of them.
///
/// Nothing is destroyed: what is taken out is put aside in a folder of its own inside the git folder (`dogmind-disarmed-<time>`, with the hooks and the config as they were), where git does not
/// read it, and `taken` names the folder.
///
/// This is done as the person, and nothing here goes through a link: a command can make one to what it may write, and a repository of the person's can be that. What a link is met at is refused as
/// a whole, and not touched; what is said is in `taken` as it is done, as one place can be refused after another was taken care of. Whatever the order of this and the guarding, neither follows one.
fn disarm_repository(git: &Path, root: &Path, known: &HashSet<String>, taken: &mut Taken) -> Result<(), String> {
    refuse_links(&[git])?;
    if git.is_dir() {
        disarm_folder(git, taken);
        return Ok(());
    }
    // A worktree's or a submodule's: the folder it points to, and for a worktree, the repository that says it is part of.
    let Some(folder) = git_folder_of(git, root)? else { return Ok(()) };
    let common = common_folder_of(&folder, root);
    disarm_folder(&folder, taken);
    if let Some(common) = common?.filter(|common| !known.contains(&key(common))) {
        disarm_folder(&common, taken);
    }
    Ok(())
}

/// One repository's `.git`, guarded, and what its file leads to if it is one.
unsafe fn guard_with_what_it_leads_to(root: &Path, git: &Path, sid: PSID) -> Result<(), String> {
    guard_repository(root, git, sid)?;
    // (a submodule's folder is in the outer `.git`, which is open to git's own new files; a worktree's has neither of these)
    if git.is_file() {
        if let Some(folder) = git_folder_of(git, root)? {
            if std::fs::symlink_metadata(folder.join("commondir")).is_ok() {
                // A worktree's: what it says is part of it is the main repository's, which is guarded where it is in the workspace, and is not to lead out through a link.
                common_folder_of(&folder, root)?;
            } else {
                refuse_links(&[&folder, &folder.join("config"), &folder.join("hooks")])?;
                let everything = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;
                limit_to(root, &folder.join("config"), sid, &[(READ_EXECUTE, 0)], false)?;
                limit_to(root, &folder.join("hooks"), sid, &[(READ_EXECUTE, everything)], true)?;
            }
        }
    }
    Ok(())
}

/// What looking after the repositories of a workspace came to.
struct Looked {
    /// What was taken out of the repositories a command made, as it would have run outside the sandbox, and where it was put: a sentence for each repository.
    said: Vec<String>,
    /// What cannot be kept from commands, the first of it: a link met where the app writes as the person, or a list that could not be written. It stays so until a person puts it right, and it is
    /// looked for again before every command, which is not run while it is there.
    refused: Option<String>,
}

//// Before a command, and after it for what that one made: every repository in reach, each new one that a command could have made disarmed, and all of them guarded. Says what was taken out,
/// and what stands in the way of guarding. A refusal at one repository is not the end of the looking: the others are still taken care of, and what a command made there is not left for no one to see.
///
/// A repository that is new at a look is a command's if this is the look after one, or if one of the workspace's was at work, or ran, since the last look: a background job, or a command of
/// another conversation. When nothing of the app's was, the person made it, as in their own terminal while a chat is open on the workspace: it is guarded as every repository is, so that
/// no command can change its hooks or its config, and nothing is taken out of it.
unsafe fn look_after_repositories(root: &Path, sid: PSID, when: Look) -> Looked {
    let found = repositories_under(root);
    let new = newly_made(root, &found);
    let by_a_command = commands_since_the_last_look(root) || when == Look::AfterACommand;
    let mut looked = Looked { said: Vec::new(), refused: None };
    if by_a_command && !new.is_empty() {
        let known: HashSet<String> = found.iter().filter(|git| !new.contains(git)).map(|git| key(git)).collect();
        for git in &new {
            let mut taken = Taken::default();
            let result = disarm_repository(git, root, &known, &mut taken);
            if !taken.names.is_empty() {
                let shown = git.parent().and_then(|folder| folder.strip_prefix(root).ok()).map_or(String::new(), |place| place.display().to_string());
                looked.said.push(taken.said_of(&if shown.is_empty() { "the workspace folder itself".to_string() } else { shown }));
            }
            if let Err(refusal) = result {
                looked.refused.get_or_insert(refusal);
            }
        }
    }
    for git in &found {
        if let Err(refusal) = guard_with_what_it_leads_to(root, git, sid) {
            looked.refused.get_or_insert(refusal);
        }
    }
    looked
}

/// What the commands of one run may commit between them: what the machine could give without its running out of memory for everything else, as it once did, and less than all of its RAM, past which
/// everything slows to a crawl. Some is always allowed: a command cannot be blamed for a machine that was already short.
fn job_memory_cap() -> u64 {
    const LEAST: u64 = 4 << 30;
    const LEFT_FOR_THE_REST: u64 = 4 << 30;
    let mut status: windows_sys::Win32::System::SystemInformation::MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of_val(&status) as u32;
    // SAFETY: the struct lives through the call, with its size set.
    if unsafe { windows_sys::Win32::System::SystemInformation::GlobalMemoryStatusEx(&mut status) } == 0 {
        return u64::MAX;
    }
    memory_cap_from(status.ullAvailPageFile, status.ullTotalPhys, LEAST, LEFT_FOR_THE_REST)
}

fn memory_cap_from(commit_available: u64, ram: u64, least: u64, left_for_the_rest: u64) -> u64 {
    commit_available.saturating_sub(left_for_the_rest).min(ram / 4 * 3).max(least)
}

/// One ACE for the profile, once: propagating it through a tree is what costs, and it persists.
pub(crate) unsafe fn grant(path: &Path, sid: PSID, mask: u32, inherit: bool) -> Result<(), String> {
    let memo = (key(path), mask, inherit);
    {
        let mut granted = GRANTED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if granted.get_or_insert_with(HashSet::new).contains(&memo) {
            return Ok(());
        }
    }
    let name = wide_path(path);
    if !has_grant(&name, sid, mask, inherit) {
        if inherit {
            write_ace(path, &name, sid, mask, true, GRANT_ACCESS)?;
        } else {
            // For this folder alone, and nothing else about its list changed: SetNamedSecurityInfo walks everything under a folder whatever the ACE says, and on the way to a workspace that
            // can be the whole of a person's AppData, and SetFileSecurity takes off the bit that makes a list keep to itself, from a folder the person had closed.
            add_exact_ace(path, sid, mask, 0)?;
        }
    }
    GRANTED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get_or_insert_with(HashSet::new).insert(memo);
    Ok(())
}

/// Folders commands may read and run from besides the toolchains: a skill's scripts, once it is in use.
static ALSO_READABLE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

pub fn also_readable(folder: &Path) {
    let mut folders = ALSO_READABLE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !folders.iter().any(|known| key(known) == key(folder)) {
        folders.push(folder.to_path_buf());
    }
}

unsafe fn prepare(workspace: &Path, sid: PSID, disarmed: &mut Vec<String>) -> Result<(), String> {
    grant(workspace, sid, READ_WRITE_EXECUTE, true)?;
    // Each repository in the workspace. If one cannot be guarded the command does not run, as with any other containment.
    let looked = look_after_repositories(workspace, sid, Look::BeforeACommand);
    *disarmed = looked.said;
    if let Some(refusal) = looked.refused {
        // (what was taken out of a repository on the way is said too: nothing the app does as the person is done quietly)
        return Err(if disarmed.is_empty() { refusal } else { format!("{refusal}. The app also did this, in a repository made since the last command: {}", disarmed.join(" ")) });
    }
    for folder in ALSO_READABLE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter() {
        let _ = grant(folder, sid, READ_EXECUTE, true);
        for ancestor in folder.ancestors().skip(1).filter(|above| above.parent().is_some()) {
            let _ = grant(ancestor, sid, FILE_GENERIC_EXECUTE, false);
        }
    }
    // The folders above: their attributes only, and only for shells that look at where they are. An OS-owned one may refuse, which costs little.
    for ancestor in workspace.ancestors().skip(1).filter(|folder| folder.parent().is_some()) {
        let _ = grant(ancestor, sid, FILE_GENERIC_EXECUTE, false);
    }
    // (never a test's: these are the real home's folders, with real lists)
    #[cfg(not(test))]
    if let (Some(path), Some(home)) = (std::env::var_os("PATH"), std::env::var_os("USERPROFILE")) {
        open_toolchains(&path.to_string_lossy(), Path::new(&home), crate::bundled::root().is_some(), sid);
    }
    Ok(())
}

pub(crate) unsafe fn open_toolchains(path_variable: &str, home: &Path, bundled: bool, sid: PSID) {
    for folder in toolchain_folders(path_variable, home) {
        // (a person's own Python or Node is not what runs, so nothing is opened for it)
        let replaced = bundled && (crate::bundled::NAMES.iter().any(|name| folder.join(name).is_file()) || ["python", "npm", "nvm"].iter().any(|word| key(&folder).contains(word)));
        if folder.exists() && !replaced {
            let _ = grant(&folder, sid, READ_EXECUTE, true);
            hold_out_secrets(&folder, sid);
        }
    }
}

/// What reads as a secret directly in a toolchain's folder is not given with it: `.cargo\credentials.toml` holds a person's token for crates.io, and is written anew by `cargo login`, so
/// this is looked at each time. It goes on keeping its own list with what it inherited written into it, and nothing for the container: there is no taking this back, and nothing to take.
unsafe fn hold_out_secrets(folder: &Path, sid: PSID) {
    for item in std::fs::read_dir(folder).into_iter().flatten().flatten() {
        let path = item.path();
        if item.file_type().is_ok_and(|kind| !kind.is_symlink()) && crate::safety::sensitive(&Path::new("/").join(item.file_name())) {
            if let Ok(dacl) = Dacl::read(&path) {
                if dacl.aces.iter().any(|ace| Dacl::is_allowed_for(ace, sid)) {
                    let _ = dacl.as_explicit_and_protected(sid).write_alone(&path);
                }
            }
        }
    }
}

/// The sandbox's own temp folder: what a command leaves there, the app can read.
pub(crate) fn temp_folder() -> Option<PathBuf> {
    // SAFETY: the SID lives as long as the process.
    unsafe { profile_folder(profile_sid().ok()?) }
}

unsafe fn profile_folder(sid: PSID) -> Option<PathBuf> {
    let mut text: *mut u16 = std::ptr::null_mut();
    if ConvertSidToStringSidW(sid, &mut text) == 0 {
        return None;
    }
    let mut folder: *mut u16 = std::ptr::null_mut();
    let found = GetAppContainerFolderPath(text, &mut folder);
    LocalFree(text as HLOCAL);
    if found < 0 || folder.is_null() {
        return None;
    }
    let length = (0..).take_while(|&at| *folder.add(at) != 0).count();
    let path = PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(folder, length)));
    windows_sys::Win32::System::Com::CoTaskMemFree(folder as _);
    Some(path)
}

/// A command's environment, as the block CreateProcess takes. `build_tools` is asked for the compiler's folders only when the app has no `LIB` of its own: the command's start waits for it, so it
/// is given the time the command has and the person's Stop (see `crate::msvc::found_before`).
fn environment(temp: Option<&Path>, workspace: &Path, build_tools: impl FnOnce() -> Option<crate::msvc::BuildTools>) -> Vec<u16> {
    let block: Vec<(String, String)> = VARIABLES.iter().filter_map(|name| std::env::var(name).ok().map(|value| (name.to_string(), value))).collect();
    let temp = temp.map(|path| path.display().to_string()).or_else(|| std::env::var("TEMP").ok()).unwrap_or_default();
    let block = environment_block(block, crate::bundled::root().as_deref(), temp, workspace, build_tools);
    let mut out = Vec::new();
    for (name, value) in block {
        out.extend(format!("{name}={value}").encode_utf16());
        out.push(0);
    }
    out.push(0);
    out
}

/// A command's environment from what it inherits. The PATH comes out as the bundled tools, then what the machine has (without a person's own Python, Node or Git), then the folders of
/// Visual Studio's compiler when `build_tools` finds them, and last the bundled Git's Unix tools: nothing that is earlier on it can be shadowed by a later one.
fn environment_block(mut block: Vec<(String, String)>, bundled: Option<&Path>, temp: String, workspace: &Path, build_tools: impl FnOnce() -> Option<crate::msvc::BuildTools>) -> Vec<(String, String)> {
    crate::msvc::add_to(&mut block, build_tools);
    if let Some(root) = bundled {
        let inherited = block.iter().find(|(name, _)| name.eq_ignore_ascii_case("PATH")).map(|(_, path)| path.clone()).unwrap_or_default();
        let added = crate::bundled::environment(root, &inherited, &temp, workspace);
        block.retain(|(name, _)| !added.iter().any(|(new, _)| new.eq_ignore_ascii_case(name)));
        block.extend(added);
    }
    // Where git looks for its per-user files: the person's profile is closed to a command, and it would warn on every run.
    block.push(("XDG_CONFIG_HOME".to_string(), temp.clone()));
    block.push(("TEMP".to_string(), temp.clone()));
    block.push(("TMP".to_string(), temp));
    block.sort_by_key(|(name, _)| name.to_uppercase());
    block
}

/// The folders on the PATH a command starts with, in order, made as `environment_block` makes it: the bundled tools, the machine's without a person's own Python, Node or Git, and then Visual
/// Studio's. Only the PATH is made: `environment_block` goes on to ask git for a name and to write a config file, which a question about a name in a command must not do.
fn path_of(mut block: Vec<(String, String)>, bundled: Option<&Path>, build_tools: impl FnOnce() -> Option<crate::msvc::BuildTools>) -> Vec<PathBuf> {
    crate::msvc::add_to(&mut block, build_tools);
    let inherited = block.iter().find(|(name, _)| name.eq_ignore_ascii_case("PATH")).map(|(_, path)| path.clone()).unwrap_or_default();
    let path = bundled.map_or(inherited.clone(), |root| crate::bundled::path_with(root, &inherited));
    std::env::split_paths(&path).collect()
}

/// Where a command's programs are found: its PATH, as this machine gives it to the sandbox. The gate asks, to tell a file in the folder that takes the place of an installed program from
/// one that is the project's own. It asks while a command waits to be weighed, with nothing yet on the screen for the person to Stop, so it does not wait for Visual Studio's folders: the
/// ones that are found by then are in it, and the look for them begins in the background if it has not.
pub(crate) fn command_path() -> Vec<PathBuf> {
    let block: Vec<(String, String)> = ["PATH", "LIB", "INCLUDE"].into_iter().filter_map(|name| std::env::var(name).ok().map(|value| (name.to_string(), value))).collect();
    path_of(block, crate::bundled::root().as_deref(), crate::msvc::known)
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: the handle is this value's.
            unsafe { CloseHandle(self.0) };
        }
    }
}

unsafe fn pipe() -> Result<(Handle, Handle), String> {
    let attributes = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: std::ptr::null_mut(), bInheritHandle: 1 };
    let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    if CreatePipe(&mut read, &mut write, &attributes, 0) == 0 {
        return Err(last_error("a pipe"));
    }
    SetHandleInformation(read, HANDLE_FLAG_INHERIT, 0);
    Ok((Handle(read), Handle(write)))
}

/// Runs `line` in the container and waits, ending it on `timeout` or on `cancel`.
pub fn run(line: &str, cwd: &Path, workspace: &Path, network: bool, timeout: Duration, cancel: &AtomicBool) -> Result<Output, String> {
    run_live(line, cwd, workspace, network, timeout, cancel, None)
}

/// As `run`, with what it prints in `live` as it prints it.
pub fn run_live(line: &str, cwd: &Path, workspace: &Path, network: bool, timeout: Duration, cancel: &AtomicBool, live: Option<Arc<crate::tools::Live>>) -> Result<Output, String> {
    run_limited(line, cwd, workspace, network, timeout, cancel, UI_LIMITS, Some(job_memory_cap()), false, live)
}

/// A browser under those limits falls over in most runs, whichever of them is set (0xC0000005, 12 to 19 runs in 20, against none in 20 with none set). Its own window station
/// keeps it from the person's clipboard and session in any case.
///
/// Its input is a closed pipe, not the NUL device. With the NUL device Edge's crash handler (`--type=crashpad-handler`, which the browser starts) began with a standard input that
/// was not a handle of its own process (its output and error were), and ended in the loader with STATUS_INVALID_HANDLE (0xC0000008) before it ran; with a pipe it runs, to the end
/// of the browser's. With no handler to answer, the browser's other processes waited for it in `WaitNamedPipe` (crashpad's registration), and in about one run in three the page was
/// never opened: no end and no output until the time was up.
pub fn run_browser(line: &str, cwd: &Path, workspace: &Path, timeout: Duration, cancel: &AtomicBool) -> Result<Output, String> {
    run_limited(line, cwd, workspace, false, timeout, cancel, 0, None, true, None)
}

#[allow(clippy::too_many_arguments)]
fn run_limited(line: &str, cwd: &Path, workspace: &Path, network: bool, timeout: Duration, cancel: &AtomicBool, ui_limits: u32, memory_cap: Option<u64>, closed_stdin: bool, live: Option<Arc<crate::tools::Live>>) -> Result<Output, String> {
    let asked_at = Instant::now();
    // (the build tools are looked for while the folder is made ready below, and the environment then usually finds them ready)
    crate::msvc::look_ahead();
    no_error_boxes();
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let root = writable_root(cwd, workspace, home.as_deref())?;
    if let Some(reason) = volume_of(&root).and_then(|volume| drive_refusal(&volume, &root)) {
        return Err(reason);
    }
    // It counts as in use from before it is granted to when all it started has ended.
    let _in_use = crate::workspace_grants::Use::new(&root);
    // SAFETY: every pointer below lives until the call that takes it returns, and the attribute list's buffers until the process is started.
    unsafe {
        let sid = profile_sid()?;
        let mut disarmed = Vec::new();
        {
            let _one = crate::workspace_grants::before_grant(&root)?;
            prepare(&root, sid, &mut disarmed)?;
            if let Some(tools) = crate::bundled::root() {
                crate::bundled::open_to_sandbox(&tools)?;
            }
        }
        // (from here to the look after it, a repository that shows up in the workspace may be what it makes)
        let _at_work = AtWork::start(&root);
        let (out_read, out_write) = pipe()?;
        let (err_read, err_write) = pipe()?;
        let nul = wide("NUL");
        let inheritable = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: std::ptr::null_mut(), bInheritHandle: 1 };
        let input = if closed_stdin {
            // The end of a pipe nothing writes to: reading it ends at once, as the NUL device does.
            let (read_end, write_end) = pipe()?;
            drop(write_end);
            if SetHandleInformation(read_end.0, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
                return Err(last_error("a pipe for its input"));
            }
            read_end
        } else {
            Handle(CreateFileW(nul.as_ptr(), FILE_GENERIC_READ, FILE_SHARE_READ, &inheritable, OPEN_EXISTING, 0, std::ptr::null_mut()))
        };

        let job = Handle(CreateJobObjectW(std::ptr::null(), std::ptr::null()));
        if job.0.is_null() {
            return Err(last_error("a Job Object"));
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        // (the second: no process in it gets the crash dialog, whatever error mode it has given itself; it ends there, with the exception as its code)
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        if let Some(cap) = memory_cap.filter(|cap| *cap < usize::MAX as u64) {
            // (both: with the job's alone a process went to 869 MB under a limit of 300 MB, on Windows 11 26300. With both, a process is refused at the limit.)
            limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            limits.JobMemoryLimit = cap as usize;
            limits.ProcessMemoryLimit = cap as usize;
        }
        // (without it, what a command started outlives the app when that goes down)
        if SetInformationJobObject(job.0, JobObjectExtendedLimitInformation, std::ptr::addr_of!(limits).cast(), std::mem::size_of_val(&limits) as u32) == 0 {
            return Err(last_error("the limits that end what it starts"));
        }
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS { UIRestrictionsClass: ui_limits };
        if SetInformationJobObject(job.0, JobObjectBasicUIRestrictions, std::ptr::addr_of!(ui).cast(), std::mem::size_of_val(&ui) as u32) == 0 {
            return Err(last_error("the limits on what it can do to the screen"));
        }
        let desktop = Desktop::new(sid)?;

        let mut capability_sids: Vec<PSID> = Vec::new();
        if network {
            for text in NETWORK_CAPABILITIES {
                let mut capability: PSID = std::ptr::null_mut();
                let text = wide(text);
                if ConvertStringSidToSidW(text.as_ptr(), &mut capability) == 0 {
                    return Err(last_error("a network capability"));
                }
                capability_sids.push(capability);
            }
        }
        let mut capabilities: Vec<SID_AND_ATTRIBUTES> = capability_sids.iter().map(|&capability| SID_AND_ATTRIBUTES { Sid: capability, Attributes: SE_GROUP_ENABLED as u32 }).collect();
        let security = SECURITY_CAPABILITIES { AppContainerSid: sid, Capabilities: if capabilities.is_empty() { std::ptr::null_mut() } else { capabilities.as_mut_ptr() }, CapabilityCount: capabilities.len() as u32, Reserved: 0 };
        let handles = [input.0, out_write.0, err_write.0];
        let jobs = [job.0];

        let mut size = 0usize;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 3, 0, &mut size);
        let mut list_memory = vec![0u8; size];
        let list = list_memory.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if InitializeProcThreadAttributeList(list, 3, 0, &mut size) == 0 {
            return Err(last_error("the attribute list"));
        }
        let attributes_ok = UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize, std::ptr::addr_of!(security).cast(), std::mem::size_of_val(&security), std::ptr::null_mut(), std::ptr::null()) != 0
            && UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize, handles.as_ptr().cast(), std::mem::size_of_val(&handles), std::ptr::null_mut(), std::ptr::null()) != 0
            && UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_JOB_LIST as usize, jobs.as_ptr().cast(), std::mem::size_of_val(&jobs), std::ptr::null_mut(), std::ptr::null()) != 0;
        if !attributes_ok {
            DeleteProcThreadAttributeList(list);
            return Err(last_error("the process attributes"));
        }

        let mut startup: STARTUPINFOEXW = std::mem::zeroed();
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        let mut desktop_name = desktop.name.clone();
        startup.StartupInfo.lpDesktop = desktop_name.as_mut_ptr();
        startup.StartupInfo.hStdInput = input.0;
        startup.StartupInfo.hStdOutput = out_write.0;
        startup.StartupInfo.hStdError = err_write.0;
        startup.lpAttributeList = list;
        let shell = std::env::var("ComSpec").unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".to_string());
        // The code page first, so that a tool's output arrives as UTF-8 on a Japanese Windows.
        let mut command_line = wide(&format!("\"{shell}\" /d /s /c \"chcp 65001 >nul & {line}\""));
        // (the look for the build tools may still be going on: the command's start waits for it no longer than the command has time, and not past Stop)
        let block = environment(profile_folder(sid).as_deref(), &root, || crate::msvc::found_before(asked_at.checked_add(timeout).unwrap_or(asked_at + Duration::from_secs(3600)), cancel));
        let place = wide_path(cwd);
        let mut process: PROCESS_INFORMATION = std::mem::zeroed();
        let started = CreateProcessW(
            std::ptr::null(), command_line.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 1,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            block.as_ptr().cast(), place.as_ptr(), &startup.StartupInfo, &mut process,
        );
        let failure = (started == 0).then(|| last_error("the command did not start in its AppContainer"));
        DeleteProcThreadAttributeList(list);
        for capability in capability_sids {
            LocalFree(capability as HLOCAL);
        }
        if let Some(failure) = failure {
            return Err(failure);
        }
        let process_handle = Handle(process.hProcess);
        drop(Handle(process.hThread));
        drop(out_write);
        drop(err_write);
        drop(input);

        let capture = |pipe: Handle| {
            let raw = pipe.0;
            std::mem::forget(pipe);
            let raw = raw as usize;
            let live = live.clone();
            std::thread::spawn(move || {
                // SAFETY: ownership of the read end came here.
                let mut file = File::from_raw_handle(raw as _);
                kept_of(&mut file, KEPT_HEAD, KEPT_TAIL, live.as_deref())
            })
        };
        let (stdout, stderr) = (capture(out_read), capture(err_read));

        let began = Instant::now();
        let (mut looked, mut dialog_since, mut last_seen): (Instant, Option<Instant>, Vec<(String, String)>) = (began, None, Vec::new());
        let ended = loop {
            if WaitForSingleObject(process_handle.0, 30) == WAIT_OBJECT_0 {
                break "";
            }
            if cancel.load(Ordering::Relaxed) {
                break "cancelled";
            }
            if began.elapsed() > timeout {
                break "timed out";
            }
            if looked.elapsed() > Duration::from_secs(1) {
                looked = Instant::now();
                last_seen = desktop.windows();
                if !last_seen.iter().any(|(class, _)| class == "#32770") {
                    dialog_since = None;
                } else if dialog_since.get_or_insert(looked).elapsed() > MOST_DIALOG {
                    break "stopped, as it was waiting on a dialog nobody can see";
                }
            }
        };
        let seen = desktop.windows();
        let shown = (if seen.is_empty() { last_seen } else { seen }).into_iter().filter(|(class, title)| class == "#32770" || !title.is_empty()).map(|(_, title)| title).collect();
        let mut code = 0u32;
        let status = if ended.is_empty() && GetExitCodeProcess(process_handle.0, &mut code) != 0 { Some(code as i32) } else { None };
        // How much it used, while there is a job to ask. An allocation refused near the limit is what shows: what it used may stop a little short of it.
        let mut used: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        let asked = QueryInformationJobObject(job.0, JobObjectExtendedLimitInformation, std::ptr::addr_of_mut!(used).cast(), std::mem::size_of_val(&used) as u32, std::ptr::null_mut()) != 0;
        let out_of_memory = memory_cap.filter(|cap| asked && status != Some(0) && used.PeakJobMemoryUsed as u64 >= cap / 10 * 9);
        // Whatever it left running goes with it, and that closes the pipes.
        TerminateJobObject(job.0, 1);
        drop(job);
        // What it made, with nothing of its own left running to go on making it. (Not being able to guard this does not take back what has already run: the next command is what is refused, for what is
        // found again before it, and it is said now in what this one returns.)
        let looked = look_after_repositories(&root, sid, Look::AfterACommand);
        disarmed.extend(looked.said);
        Ok(Output { stdout: stdout.join().unwrap_or_default(), stderr: stderr.join().unwrap_or_default(), status, ended, shown, disarmed, out_of_memory, unguarded: looked.refused })
    }
}

/// What is kept of a command's output, at most: its start and its end. A command that printed without end (a loop, a log of gigabytes)
/// was read whole into the app's memory, all of it, before any of it was cut.
const KEPT_HEAD: usize = 4 << 20;
const KEPT_TAIL: usize = 4 << 20;

/// All a stream gives, as its first `head` bytes and its last `tail`, with what was between them said and not kept. `live` has it all as it comes.
fn kept_of(stream: &mut impl Read, head: usize, tail: usize, live: Option<&crate::tools::Live>) -> Vec<u8> {
    let (mut start, mut end, mut dropped) = (Vec::new(), std::collections::VecDeque::<u8>::new(), 0u64);
    let mut block = [0u8; 64 * 1024];
    loop {
        let count = match stream.read(&mut block) {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        if let Some(live) = live {
            live.push(&block[..count]);
        }
        let mut chunk = &block[..count];
        if start.len() < head {
            let taken = (head - start.len()).min(chunk.len());
            start.extend_from_slice(&chunk[..taken]);
            chunk = &chunk[taken..];
        }
        end.extend(chunk);
        if end.len() > tail {
            let over = end.len() - tail;
            end.drain(..over);
            dropped += over as u64;
        }
    }
    if dropped > 0 {
        start.extend_from_slice(format!("\n--- {dropped} bytes in the middle of this output were not kept ---\n").as_bytes());
    }
    start.extend(end);
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An output past what is kept has its start and its end, and says what went between them.
    #[test]
    fn an_endless_output_keeps_its_start_and_end_and_says_what_went() {
        let text: Vec<u8> = b"BEGIN".iter().copied().chain(std::iter::repeat_n(b'x', 1000)).chain(b"END".iter().copied()).collect();
        let kept = kept_of(&mut std::io::Cursor::new(text.clone()), 100, 50, None);
        let said = String::from_utf8(kept).unwrap();
        assert!(said.starts_with("BEGIN") && said.ends_with("END") && said.contains("858 bytes in the middle of this output were not kept"), "{said}");
        assert_eq!(kept_of(&mut std::io::Cursor::new(b"short".to_vec()), 100, 50, None), b"short");
    }

    #[test]
    fn a_command_gets_a_desktop_in_a_station_of_its_own_and_no_error_boxes() {
        no_error_boxes();
        // SAFETY: it reads a flag of this process.
        let mode = unsafe { windows_sys::Win32::System::Diagnostics::Debug::GetErrorMode() };
        assert_eq!(mode & 0x8003, 0x8003, "what a child has from its parent: {mode:#x}");
        // SAFETY: the SID lives as long as the process.
        let desktop = unsafe { Desktop::new(profile_sid().unwrap()) }.unwrap();
        let name = String::from_utf16_lossy(&desktop.name);
        assert!(!name.to_ascii_lowercase().starts_with("winsta0"), "not the station of the person's screen: {name}");
        assert!(name.contains('\\') && name.contains("DogmindExec-"), "{name}");
        // SAFETY: as above.
        assert!(unsafe { desktop.windows() }.is_empty());
    }

    #[test]
    fn a_drive_that_cannot_contain_a_command_says_so_first() {
        let folder = Path::new("E:\\work");
        let plain = |file_system: &str, keeps_permissions: bool, remote: bool| Volume { file_system: file_system.into(), keeps_permissions, remote };
        assert_eq!(drive_refusal(&plain("NTFS", true, false), folder), None);
        assert!(drive_refusal(&plain("exFAT", false, false), folder).unwrap().contains("exFAT"));
        assert!(drive_refusal(&plain("FAT32", false, false), folder).unwrap().contains("no file permissions"));
        assert!(drive_refusal(&plain("", true, true), folder).unwrap().contains("network"));
        let dir = tempfile::tempdir().unwrap();
        let here = volume_of(&dir.path().join("not").join("yet")).unwrap();
        assert!(!here.remote && here.keeps_permissions && !here.file_system.is_empty(), "{here:?}");
        let rights = rights_on(dir.path()).unwrap();
        assert_eq!(rights, (true, true), "one's own temporary folder");
        assert_eq!(rights_on(&dir.path().join("not")), None);
    }

    #[test]
    fn a_workspace_too_wide_still_has_project_folders_to_run_in() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("me");
        let project = home.join("iris-study");
        std::fs::create_dir_all(project.join("src")).unwrap();
        let root = writable_root(&project.join("src"), &home, Some(&home)).unwrap();
        assert_eq!(root, project.join("src"));
        assert_eq!(writable_root(&project, &home, Some(&home)).unwrap(), project);
        let refused = writable_root(&home, &home, Some(&home)).unwrap_err();
        assert!(refused.contains("whole user folder") && refused.contains("Set cwd to a project folder that is already inside it"), "{refused}");
        assert!(refused.contains("`read_file`, `list_files` and `search_files` are not refused") && refused.contains("only when the user asked for something new"), "{refused}");
        assert!(!refused.contains("creating it first"), "a folder is not made only to have somewhere to run: {refused}");
        for place in [".ssh", ".aws", "AppData\\Local", "Documents", "Desktop", "Downloads\\x"] {
            std::fs::create_dir_all(home.join(place)).unwrap();
            assert!(writable_root(&home.join(place), &home, Some(&home)).is_err(), "{place}");
        }
        let missing = writable_root(&home.join("not-made-yet"), &home, Some(&home)).unwrap_err();
        assert!(missing.contains("does not exist") && missing.contains("only when the user asked for something new"), "{missing}");
        assert!(writable_root(dir.path(), &home, Some(&home)).is_err(), "outside it");
    }

    #[test]
    fn an_ordinary_workspace_is_the_root_whatever_the_folder_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let (home, work) = (dir.path().join("me"), dir.path().join("work"));
        std::fs::create_dir_all(work.join("sub")).unwrap();
        assert_eq!(writable_root(&work.join("sub"), &work, Some(&home)).unwrap(), work);
        assert!(writable_root(&home, &work, Some(&home)).is_err());
    }

    /// Under this crate's target folder, whose ACL nothing else depends on: the tests write real ACEs.
    fn work_folder(name: &str) -> PathBuf {
        let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/sandbox-tests").join(name);
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::canonicalize(&folder).map_or(folder, |full| PathBuf::from(full.to_string_lossy().trim_start_matches("\\\\?\\")))
    }

    fn command(line: &str, folder: &Path, network: bool) -> Output {
        run(line, folder, folder, network, Duration::from_secs(60), &AtomicBool::new(false)).unwrap()
    }

    /// The other half of `a_browsers_input_is_a_pipe_that_its_children_are_handed`, run in the container as a copy of this program: `parent` starts `child` with its own input handed
    /// on, as Edge starts its crash handler, and `child` says what kind of handle its input is.
    #[test]
    #[ignore = "run in the container by a_browsers_input_is_a_pipe_that_its_children_are_handed"]
    fn an_input_probe() {
        use std::os::windows::io::AsRawHandle;
        match std::env::args().last().unwrap_or_default().as_str() {
            "parent" => {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap());
                child.args(["sandbox::tests::an_input_probe", "--exact", "--ignored", "--nocapture", "--test-threads=1", "child"]).stdin(std::process::Stdio::inherit()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
                let said = child.output().unwrap();
                let out = String::from_utf8_lossy(&said.stdout).into_owned();
                println!("PROBE-PARENT {}", out.find("PROBE-CHILD").map_or_else(|| format!("PROBE-CHILD no answer, {:?}: {out} {}", said.status, String::from_utf8_lossy(&said.stderr)), |at| out[at..].lines().next().unwrap_or("").to_string()));
            }
            "child" => {
                // SAFETY: it asks the system what a handle is.
                let kind = unsafe { GetFileType(std::io::stdin().as_raw_handle() as HANDLE) };
                println!("PROBE-CHILD input is {}", match kind { 1 => "a file", 2 => "a device", 3 => "a pipe", _ => "no handle" });
            }
            _ => {}
        }
    }

    /// What a browser runs with as its input is a pipe, and a child of its that is handed it has a pipe too: with the NUL device in its place, the crash handler Edge starts as such a
    /// child ended before it ran (see `run_browser`). What a command is given is not looked at here.
    #[test]
    fn a_browsers_input_is_a_pipe_that_its_children_are_handed() {
        let folder = work_folder("browser-input");
        let probe = folder.join("probe.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &probe).unwrap();
        let line = format!("\"{}\" sandbox::tests::an_input_probe --exact --ignored --nocapture --test-threads=1 parent", probe.display());
        let done = run_browser(&line, &folder, &folder, Duration::from_secs(60), &AtomicBool::new(false)).unwrap();
        let said = decode(&done.stdout);
        assert!(said.contains("PROBE-PARENT PROBE-CHILD input is a pipe"), "{said} {}", decode(&done.stderr));
    }

    /// Nothing is written: this only asks about a link to the real home, and what is made goes with the temp folder.
    fn junction(link: &Path, target: &Path) {
        let mut made = std::process::Command::new("cmd");
        made.arg("/c").arg("mklink").arg("/J").arg(link).arg(target).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        crate::tools::no_window(&mut made);
        assert!(made.status().unwrap().success());
    }

    #[test]
    fn a_link_is_judged_by_where_it_leads() {
        let dir = tempfile::tempdir().unwrap();
        let (home, projects) = (dir.path().join("me"), dir.path().join("projects"));
        std::fs::create_dir_all(home.join("AppData\\Local")).unwrap();
        std::fs::create_dir_all(home.join("work\\sub")).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        // A workspace that is a link to the home: its ACE would go on all of it.
        let to_home = projects.join("innocent");
        junction(&to_home, &home);
        let refused = refusal(&to_home, Some(&home)).unwrap();
        assert!(refused.contains("whole user folder"), "{refused}");
        // In a home taken as the workspace, the links every Windows has in it. `Local Settings` is AppData\Local.
        let local_settings = home.join("Local Settings");
        junction(&local_settings, &home.join("AppData\\Local"));
        assert!(writable_root(&local_settings, &home, Some(&home)).is_err(), "as AppData\\Local itself");
        // A link inside an ordinary workspace to what is outside it.
        let out = projects.join("out");
        junction(&out, &home.join("work"));
        assert!(writable_root(&out, &projects, Some(&home)).is_err(), "it leads outside");
        // What is given is the folder, wherever the path to it went through.
        let link_to_work = dir.path().join("link-to-work");
        junction(&link_to_work, &home.join("work"));
        assert_eq!(writable_root(&link_to_work.join("sub"), &link_to_work, Some(&home)).unwrap(), std::fs::canonicalize(home.join("work")).map(|full| PathBuf::from(full.to_string_lossy().trim_start_matches("\\\\?\\"))).unwrap());
    }

    /// The real home, which only a test that asks can reach. It writes nothing: this is what an ACE on it would have been for.
    #[test]
    fn a_link_to_the_real_home_or_a_short_name_for_it_is_the_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = PathBuf::from(std::env::var_os("USERPROFILE").unwrap());
        let link = dir.path().join("to-home");
        junction(&link, &home);
        assert!(refusal(&link, Some(&home)).is_some());
        std::fs::remove_dir(&link).unwrap();
    }

    /// Each way a workspace is closed has the advice that is true for it: a wide one has project folders to run in, and a folder made in what holds keys or this app's own files is as closed.
    #[test]
    fn the_advice_for_a_closed_workspace_is_the_one_that_is_true_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("me");
        let ssh = home.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        let keys = writable_root(&ssh, &ssh, Some(&home)).unwrap_err();
        assert!(keys.contains("a place keys are kept") && keys.contains("no folder inside it is one to run in") && keys.contains("can look there"), "{keys}");
        assert!(!keys.contains("Set cwd") && !keys.contains("create_file"), "{keys}");
        let app = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        let own = writable_root(&app, &app, Some(&home)).unwrap_err();
        assert!(own.contains("holds this app's own files") && own.contains("a command does not run in it") && own.contains("run a copy of the app from another folder"), "{own}");
        assert!(own.contains("A folder made inside it does not help, as a command there cannot read the rest of it"), "{own}");
        assert!(!own.contains("Set cwd") && !own.contains("create_file") && !own.contains("folder of its own"), "{own}");
        // a folder inside it is as closed, and the person's notice (the reason alone) says nothing of making one
        let inside = writable_root(&app.join("sub"), &app, Some(&home)).unwrap_err();
        assert!(inside.contains("does not help"), "{inside}");
        assert!(!refusal(&app, Some(&home)).unwrap().contains("create_file"));
    }

    #[test]
    fn what_holds_this_app_or_keys_is_no_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let (home, tools, data) = (dir.path().join("me"), dir.path().join("Tools"), dir.path().join("Tools\\Dogmind"));
        std::fs::create_dir_all(&data).unwrap();
        let own = [data.clone()];
        assert!(refusal_with(&tools, Some(&home), &own).unwrap().contains("this app's own"), "it holds it");
        assert!(!refusal_with(&tools, Some(&home), &own).unwrap().contains("folder of its own"), "it does not point at one");
        assert!(refusal_with(&data, Some(&home), &own).is_some());
        assert!(refusal_with(&data.join("tools"), Some(&home), &own).is_some(), "it lies among them");
        assert_eq!(refusal_with(&dir.path().join("Tools2"), Some(&home), &own), None);
        assert!(refusal_with(Path::new("C:\\Users\\me\\.ssh"), None, &[]).is_some());
        assert!(refusal_with(Path::new("C:\\Users\\me\\.aws"), None, &[]).is_some());
        assert_eq!(refusal_with(Path::new("C:\\Projects\\credentials-api"), None, &[]), None, "only named like one");
    }

    #[test]
    fn only_a_project_folder_can_be_the_workspace() {
        let home = Path::new("C:\\Users\\me");
        assert!(refusal(Path::new("C:\\"), Some(home)).is_some());
        assert!(refusal(Path::new("D:\\"), Some(home)).is_some());
        assert!(refusal(home, Some(home)).is_some());
        assert!(refusal(Path::new("C:\\Users"), Some(home)).is_some());
        assert!(refusal(Path::new("C:\\Users\\ME\\"), Some(home)).is_some());
        assert!(refusal(Path::new("C:\\Users\\me\\src\\app"), Some(home)).is_none());
        assert!(refusal(Path::new("C:\\Projects\\app"), Some(home)).is_none());
        assert!(refusal(Path::new("C:\\Users\\meow"), Some(home)).is_none());
    }

    #[test]
    fn the_toolchains_are_granted_but_not_the_home_or_what_is_outside_it() {
        let folders = toolchain_folders("C:\\Windows;C:\\Users\\me;C:\\Users\\me\\.cargo\\bin;C:\\Users\\me\\.ssh;C:\\Program Files\\Git\\cmd", Path::new("C:\\Users\\me"));
        assert!(folders.contains(&PathBuf::from("C:\\Users\\me\\.cargo\\bin")));
        assert!(folders.contains(&PathBuf::from("C:\\Users\\me\\.rustup")));
        for gone in ["C:\\Windows", "C:\\Users\\me", "C:\\Users\\me\\.ssh", "C:\\Program Files\\Git\\cmd"] {
            assert!(!folders.contains(&PathBuf::from(gone)), "{gone}");
        }
        // However PATH spells it, the home and what holds all of a person's programs' data are not toolchains, and nor is a place keys are kept.
        let odd = toolchain_folders("C:\\Users\\me\\.;C:\\Users\\me\\x\\..;C:\\Users\\me\\AppData\\Local;C:\\Users\\me\\AppData;C:\\Users\\me\\.aws;C:\\Users\\me\\bin", Path::new("C:\\Users\\me"));
        assert!(odd.contains(&PathBuf::from("C:\\Users\\me\\bin")));
        assert!(!odd.iter().any(|folder| { let shown = folder.display().to_string(); shown.ends_with("\\.") || shown.ends_with("..") || shown.ends_with("AppData") || shown.ends_with("AppData\\Local") || shown.ends_with(".aws") }), "{odd:?}");
    }

    #[test]
    fn a_command_reports_and_writes_in_the_workspace_and_nowhere_else() {
        let folder = work_folder("contained");
        let done = command("echo hello> made.txt & type made.txt", &folder, false);
        assert_eq!(done.status, Some(0), "{}", String::from_utf8_lossy(&done.stderr));
        assert!(String::from_utf8_lossy(&done.stdout).contains("hello"));
        assert!(folder.join("made.txt").is_file());
        let home = std::env::var("USERPROFILE").unwrap();
        let name = "dogmind-app-sandbox-test.txt";
        let outside = command(&format!("echo x> \"{home}\\{name}\""), &folder, false);
        assert_ne!(outside.status, Some(0));
        assert!(!Path::new(&home).join(name).exists(), "it wrote outside the workspace");
    }

    #[test]
    fn a_command_cannot_read_the_users_files_or_see_keys() {
        let folder = work_folder("secrets");
        std::env::set_var("DOGMIND_APP_TEST_SECRET", "sekret");
        let seen = command("echo [%DOGMIND_APP_TEST_SECRET%]", &folder, false);
        assert!(!String::from_utf8_lossy(&seen.stdout).contains("sekret"));
        let home = std::env::var("USERPROFILE").unwrap();
        let listing = command(&format!("dir \"{home}\\Documents\""), &folder, false);
        assert_ne!(listing.status, Some(0), "{}", String::from_utf8_lossy(&listing.stdout));
    }

    #[test]
    fn the_network_needs_its_capability() {
        let folder = work_folder("network");
        let script = "powershell -NoProfile -Command \"try { (New-Object Net.Sockets.TcpClient).Connect('1.1.1.1', 443); 'connected' } catch { 'refused' }\"";
        let closed = command(script, &folder, false);
        assert!(!String::from_utf8_lossy(&closed.stdout).contains("connected"), "{}", String::from_utf8_lossy(&closed.stdout));
    }

    #[test]
    fn a_timeout_ends_all_a_command_started() {
        let folder = work_folder("timeout");
        let began = Instant::now();
        let done = run("for /l %i in (0,0,1) do @rem", &folder, &folder, false, Duration::from_secs(1), &AtomicBool::new(false)).unwrap();
        assert_eq!((done.status, done.ended), (None, "timed out"));
        assert!(began.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn what_the_prompt_offers_for_dir_and_del_works() {
        let folder = work_folder("no-root");
        assert_eq!(command("mkdir sub & echo a> sub\\a.txt & echo a> b.txt", &folder, false).status, Some(0));
        // That these still fail is why the prompt offers the others: they read the drive's root, which a container cannot.
        let refused = command("dir", &folder, false);
        println!("dir: {:?} {:?}", refused.status, decode(&refused.stderr));
        let listed = command("powershell -NoProfile -Command \"(Get-ChildItem -Recurse -Name) -join ','\"", &folder, false);
        let names = decode(&listed.stdout);
        assert!(names.contains("b.txt") && names.contains("a.txt"), "{names} {}", decode(&listed.stderr));
        let gone = command("powershell -NoProfile -Command \"[IO.File]::Delete('b.txt')\"", &folder, false);
        assert_eq!(gone.status, Some(0), "{}", decode(&gone.stderr));
        assert!(!folder.join("b.txt").exists());
        let tree = command("powershell -NoProfile -Command \"[IO.Directory]::Delete('sub', $true)\"", &folder, false);
        assert_eq!(tree.status, Some(0), "{}", decode(&tree.stderr));
        assert!(!folder.join("sub").exists());
    }

    /// In a temp folder, which goes with the test. The one sandbox test written to run on its own: `--skip sandbox::` leaves it out with the others.
    #[test]
    fn a_command_cannot_plant_what_would_run_outside_the_sandbox_and_git_still_works() {
        let holder = tempfile::tempdir().unwrap();
        let (place, kept) = (holder.path().join("repo"), holder.path().to_path_buf());
        std::fs::create_dir_all(&place).unwrap();
        let host = |args: &[&str]| crate::tools::git(&place, args);
        host(&["init", "-q"]).unwrap();
        std::fs::write(place.join("a.txt"), "x\n").unwrap();
        std::fs::write(place.join(".git/hooks/sample"), "x\n").unwrap();
        let config = std::fs::read_to_string(place.join(".git/config")).unwrap();
        let sandboxed = |line: &str| command(line, &place, false);
        // First run: the workspace is granted and the repository guarded.
        assert_eq!(sandboxed("echo ok> .git\\allowed.txt").status, Some(0), "the rest of .git is as it was");
        for line in ["echo evil>> .git\\config", "del .git\\config", "ren .git\\config config.old", "echo evil> .git\\hooks\\pre-commit", "echo evil>> .git\\hooks\\sample", "del .git\\hooks\\sample", "ren .git\\hooks\\sample post-index-change", "ren .git\\hooks hooks.old", "mkdir .git\\hooks\\sub", "ren .git git.old"] {
            sandboxed(line);
            assert_eq!(std::fs::read_to_string(place.join(".git/config")).unwrap(), config, "{line}");
            assert_eq!(std::fs::read_to_string(place.join(".git/hooks/sample")).unwrap(), "x\n", "{line}");
            assert!(!place.join(".git/hooks/pre-commit").exists() && !place.join(".git/hooks/post-index-change").exists() && !place.join(".git/hooks/sub").exists(), "{line}");
            assert!(place.join(".git/HEAD").is_file() && place.join(".git/hooks").is_dir(), "{line}");
        }
        // Work goes on, in the sandbox and outside it.
        let added = sandboxed("git add a.txt");
        assert_eq!(added.status, Some(0), "{}", decode(&added.stderr));
        let committed = sandboxed("git -c user.name=t -c user.email=t@example.com commit -q -m first");
        assert_eq!(committed.status, Some(0), "{}", decode(&committed.stderr));
        let branched = sandboxed("git branch other");
        assert_eq!(branched.status, Some(0), "{}", decode(&branched.stderr));
        let refused = sandboxed("git config user.name x");
        assert_ne!(refused.status, Some(0));
        host(&["config", "user.name", "the person"]).expect("the person's own git is not refused");
        // That wrote it anew, with no ACE: the next command guards it again.
        sandboxed("echo evil>> .git\\config");
        assert!(!std::fs::read_to_string(place.join(".git/config")).unwrap().contains("evil"));
        // What is in it can go, as any file of the workspace can. What would run cannot, and nothing can stand in its place.
        sandboxed("rmdir /s /q .git");
        assert!(place.join(".git/config").is_file() && place.join(".git/hooks/sample").is_file());
        drop(holder);
        assert!(!kept.exists(), "nothing left behind");
    }

    /// In a temp folder too, and run on its own as the one above is.
    #[test]
    fn what_a_command_plants_in_a_repository_it_made_is_gone_when_it_ends_at_any_depth() {
        let holder = tempfile::tempdir().unwrap();
        let place = holder.path().join("work");
        std::fs::create_dir_all(place.join("a/b/c")).unwrap();
        let run_here = |line: &str| command(line, &place, false);
        assert_eq!(run_here("echo first> first.txt").status, Some(0), "what is there at the first command is the person's");
        // Made and planted in one line, three levels down.
        // (by hand: under a user folder, where this temp folder is, `git init` itself cannot work in a container, and the app does it)
        let planted = run_here("cd a\\b\\c && mkdir .git\\hooks .git\\objects .git\\refs && echo ref: refs/heads/master> .git\\HEAD && (echo [core]& echo repositoryformatversion = 0& echo fsmonitor = evil.exe& echo [alias]& echo x = !evil.exe)> .git\\config && echo evil> .git\\hooks\\pre-commit");
        assert_eq!(planted.status, Some(0), "{}", decode(&planted.stderr));
        let git = place.join("a/b/c/.git");
        assert!(!git.join("hooks/pre-commit").exists(), "gone as soon as the command has ended");
        let config = std::fs::read_to_string(git.join("config")).unwrap();
        assert!(!config.contains("evil") && config.contains("repositoryformatversion"), "{config}");
        assert!(planted.disarmed.join(" ").contains("hooks/pre-commit") && planted.disarmed.join(" ").contains("core.fsmonitor"), "{:?}", planted.disarmed);
        // Nothing is destroyed: the hook and the config as the command left them are in a folder of git's own, which the note names.
        let aside = disarmed_folders(&git);
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert!(std::fs::read(aside[0].join("hooks\\pre-commit")).unwrap().starts_with(b"evil"), "the hook");
        assert!(String::from_utf8_lossy(&std::fs::read(aside[0].join("config")).unwrap()).contains("fsmonitor = evil.exe"), "the config as it was");
        let said = planted.disarmed.join(" ");
        assert!(said.contains(aside[0].file_name().unwrap().to_str().unwrap()) && said.contains("In a\\b\\c, made by a command"), "{said}");
        assert!(git.join("hooks").is_dir(), "the folder itself is git's");
        // From then on it is guarded as any other. Git still works there.
        let cwd = place.join("a/b/c");
        let later = command("echo evil> .git\\hooks\\post-commit", &cwd, false);
        assert_ne!(later.status, Some(0));
        assert!(!git.join("hooks/post-commit").exists());
        let added = command("git status --short", &cwd, false);
        assert_eq!(added.status, Some(0), "{}", decode(&added.stderr));
        drop(holder);
    }

    /// In the real sandbox, in temp folders. A command can make a junction to what it may write: another repository's `.git` is open to it as a folder, and so is another workspace. It cannot
    /// make one to what it may not (outside the workspaces, or a guarded `hooks`), nor a symbolic link, which takes a privilege. What the app does when the command ends, as the person, is
    /// not to be done through such a junction.
    #[test]
    fn a_junction_a_command_makes_to_a_repository_of_the_person_is_not_gone_through_when_it_ends() {
        let holder = tempfile::tempdir().unwrap();
        let place = holder.path().join("work");
        let mine = place.join("mine");
        std::fs::create_dir_all(mine.join(".git\\hooks")).unwrap();
        std::fs::write(mine.join(".git\\hooks\\pre-commit"), "the person's\n").unwrap();
        std::fs::write(mine.join(".git\\HEAD"), "ref: refs/heads/main\n").unwrap();
        let config = "[core]\n\trepositoryformatversion = 0\n[alias]\n\tx = !echo\n[credential]\n\thelper = store\n";
        std::fs::write(mine.join(".git\\config"), config).unwrap();
        std::fs::create_dir_all(place.join("sub")).unwrap();
        assert_eq!(command("echo first> first.txt", &place, false).status, Some(0), "what is there at the first command is the person's");
        // (a symbolic link takes a privilege that the container does not have, and a junction to a place outside takes a right to write there, which it does not have either)
        let (outside, symbolic) = (holder.path().join("standin"), command(&format!("mklink /D sub\\sl \"{}\"", holder.path().join("standin").display()), &place, false));
        eprintln!("mklink /D: status {:?}, {}{}", symbolic.status, decode(&symbolic.stdout).trim(), decode(&symbolic.stderr).trim());
        std::fs::create_dir_all(outside.join(".git")).unwrap();
        let abroad = command(&format!("mklink /J sub\\jn \"{}\"", outside.join(".git").display()), &place, false);
        eprintln!("mklink /J to outside: status {:?}, {}{}", abroad.status, decode(&abroad.stdout).trim(), decode(&abroad.stderr).trim());
        assert!(!place.join("sub\\sl").exists() && !place.join("sub\\jn").exists(), "no link out of the workspace");
        let made = command("mklink /J sub\\.git mine\\.git", &place, false);
        let link = place.join("sub\\.git");
        // (taken away first, by itself and not through, whatever happens after)
        let _links = Links(vec![link.clone()]);
        eprintln!("mklink /J to a repository in the workspace: status {:?}, {}{}", made.status, decode(&made.stdout).trim(), decode(&made.stderr).trim());
        if !std::fs::symlink_metadata(&link).is_ok_and(|what| what.file_type().is_symlink()) {
            eprintln!("the sandbox of this machine cannot make a junction to a repository it may write in: nothing more to see here, and the tests with junctions made by hand are what is left");
            return;
        }
        assert_eq!(std::fs::read_to_string(mine.join(".git\\hooks\\pre-commit")).unwrap(), "the person's\n", "the person's hook");
        assert_eq!(std::fs::read_to_string(mine.join(".git\\config")).unwrap(), config, "the person's config");
        // And from then on no command is run, until the link is replaced: that is what the app says, and what it does.
        assert!(made.unguarded.as_deref().is_some_and(|said| said.contains("is a link")), "{:?}", made.unguarded);
        let refused = run("echo again> again.txt", &place, &place, false, Duration::from_secs(60), &AtomicBool::new(false)).err().expect("not run");
        assert!(refused.contains("is a link") && !place.join("again.txt").exists(), "{refused}");
        std::fs::remove_dir(&link).unwrap();
        assert_eq!(command("echo again> again.txt", &place, false).status, Some(0));
        assert!(place.join("again.txt").is_file());
        assert_eq!(std::fs::read_to_string(mine.join(".git\\config")).unwrap(), config);
    }

    /// A stand-in for a repository of the person's, outside any workspace: a hook of theirs, and a config with an alias that names a program. `.git` of it.
    fn stand_in_repository(folder: &Path) -> PathBuf {
        let git = folder.join(".git");
        std::fs::create_dir_all(git.join("hooks")).unwrap();
        std::fs::write(git.join("hooks\\pre-commit"), "the person's hook\n").unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git.join("config"), "[core]\n\trepositoryformatversion = 0\n[alias]\n\tx = !echo\n[credential]\n\thelper = store\n").unwrap();
        git
    }

    /// Everything under a folder: each name, what is in each file, and each access list. Of a stand-in, which nothing may change, nor open to the container.
    fn all_of(folder: &Path) -> Vec<String> {
        let (mut seen, mut left) = (Vec::new(), vec![folder.to_path_buf()]);
        while let Some(path) = left.pop() {
            let kind = std::fs::symlink_metadata(&path).unwrap();
            assert!(!kind.file_type().is_symlink(), "a stand-in has no links: {}", path.display());
            let list = dacl_text(&path).unwrap_or_default();
            if kind.is_dir() {
                left.extend(std::fs::read_dir(&path).unwrap().flatten().map(|entry| entry.path()));
                seen.push(format!("{} {list}", path.display()));
            } else {
                seen.push(format!("{} {list} {:?}", path.display(), std::fs::read_to_string(&path).unwrap()));
            }
        }
        seen.sort();
        seen
    }

    /// The links a test made, taken away first, each by itself with `rmdir` and never through: before the folders they are in, which go with the temp folder.
    struct Links(Vec<PathBuf>);

    impl Links {
        fn junction(&mut self, link: &Path, target: &Path) {
            junction(link, target);
            self.0.push(link.to_path_buf());
        }

        /// A symbolic link, if this account may make one: it takes a privilege, or Developer Mode.
        fn symbolic(&mut self, link: &Path, target: &Path) -> bool {
            let made = if target.is_dir() { std::os::windows::fs::symlink_dir(target, link) } else { std::os::windows::fs::symlink_file(target, link) };
            if let Err(error) = &made {
                eprintln!("a symbolic link cannot be made by this account ({error}): that case is skipped");
            }
            if made.is_ok() {
                self.0.push(link.to_path_buf());
            }
            made.is_ok()
        }
    }

    impl Drop for Links {
        fn drop(&mut self) {
            for link in &self.0 {
                let _ = std::fs::remove_dir(link).or_else(|_| std::fs::remove_file(link));
            }
        }
    }

    /// The look after a command: what is new in the workspace is its making.
    fn look(place: &Path) -> Looked {
        // SAFETY: the profile's SID lives as long as the process.
        unsafe { look_after_repositories(place, profile_sid().unwrap(), Look::AfterACommand) }
    }

    /// The look as a command is made ready, when what was made since the last look may be the person's.
    fn look_between_commands(place: &Path) -> Looked {
        // SAFETY: the profile's SID lives as long as the process.
        unsafe { look_after_repositories(place, profile_sid().unwrap(), Look::BeforeACommand) }
    }

    fn is_refused_for_a_link(looked: &Looked) -> bool {
        looked.refused.as_deref().is_some_and(|said| said.contains("link") && said.contains("nothing was changed through it"))
    }

    /// In temp folders: a workspace, and outside it a stand-in for a repository of the person's. A `.git` that a command made as a link or a junction to that one is not gone through: not by the
    /// disarming of a repository a command made (which is where the app would delete hooks and cut a config as the person), and not by the guarding.
    #[test]
    fn a_git_that_is_a_link_to_a_repository_is_refused_and_it_is_not_changed() {
        let holder = tempfile::tempdir().unwrap();
        let mut links = Links(Vec::new());
        let (place, standin) = (holder.path().join("work"), holder.path().join("standin"));
        let git = stand_in_repository(&standin);
        for folder in ["sub", "sub2", "sub3"] {
            std::fs::create_dir_all(place.join(folder)).unwrap();
        }
        let first = look(&place);
        assert!(first.said.is_empty() && first.refused.is_none(), "what is there at the first look is the person's: {:?}", first.refused);
        let before = all_of(&git);
        // As a command would make it: a repository that was not there at the last look, and a junction.
        links.junction(&place.join("sub\\.git"), &git);
        let known = HashSet::new();
        let mut taken = Taken::default();
        let by_disarming = disarm_repository(&place.join("sub\\.git"), &place, &known, &mut taken).unwrap_err();
        assert!(by_disarming.contains("is a link") && taken.names.is_empty(), "{by_disarming} {:?}", taken.names);
        assert_eq!(all_of(&git), before, "disarming went through it");
        let by_guarding = unsafe { guard_repository(&place, &place.join("sub\\.git"), profile_sid().unwrap()) }.unwrap_err();
        assert!(by_guarding.contains("is a link"), "{by_guarding}");
        assert_eq!(all_of(&git), before, "guarding went through it");
        let looked = look(&place);
        assert!(is_refused_for_a_link(&looked) && looked.said.is_empty(), "{:?} {:?}", looked.refused, looked.said);
        assert_eq!(all_of(&git), before, "the hook and the config of the stand-in are as they were");
        // The next command's start finds it again, and is refused for it, until the link is replaced: by what is the person's own.
        assert!(is_refused_for_a_link(&look(&place)));
        std::fs::remove_dir(place.join("sub\\.git")).unwrap();
        let replaced = look(&place);
        assert!(replaced.refused.is_none() && replaced.said.is_empty(), "{:?}", replaced.refused);
        // A symbolic link, where this account can make one.
        if links.symbolic(&place.join("sub2\\.git"), &git) {
            let looked = look(&place);
            assert!(is_refused_for_a_link(&looked) && looked.said.is_empty(), "{:?}", looked.refused);
            assert_eq!(all_of(&git), before, "a symbolic link was gone through");
            assert!(is_refused_for_a_link(&look(&place)));
            std::fs::remove_file(place.join("sub2\\.git")).or_else(|_| std::fs::remove_dir(place.join("sub2\\.git"))).unwrap();
        }
        // Its config as a symbolic link to the stand-in's, in a `.git` that is a folder of its own.
        std::fs::create_dir_all(place.join("sub3\\.git\\hooks")).unwrap();
        std::fs::write(place.join("sub3\\.git\\HEAD"), "ref: refs/heads/main\n").unwrap();
        if links.symbolic(&place.join("sub3\\.git\\config"), &git.join("config")) {
            let looked = look(&place);
            assert_eq!(all_of(&git), before, "a symbolic link in place of its config was gone through");
            assert!(looked.said.iter().any(|said| said.contains("a link in place of its config")), "{:?} {:?}", looked.said, looked.refused);
            assert!(std::fs::symlink_metadata(place.join("sub3\\.git\\config")).is_err(), "the link itself is gone");
        }
        drop(links);
        assert!(git.join("config").is_file(), "taking the links away left what they led to");
    }

    /// The same where it is a `.git` file that says where git's folder is, and that is spelled as in the workspace, with a junction in it that leads out.
    #[test]
    fn a_gitdir_that_leads_out_through_a_junction_is_refused_and_not_changed() {
        let holder = tempfile::tempdir().unwrap();
        let mut links = Links(Vec::new());
        let (place, standin) = (holder.path().join("work"), holder.path().join("standin"));
        let git = stand_in_repository(&standin);
        for folder in ["a", "b", "c"] {
            std::fs::create_dir_all(place.join(folder)).unwrap();
        }
        assert!(look(&place).refused.is_none());
        let before = all_of(&git);
        // Named by a relative path, and by an absolute one.
        for (folder, said) in [("a", "gitdir: jn\n".to_string()), ("b", format!("gitdir: {}\n", place.join("b\\jn").display()))] {
            links.junction(&place.join(folder).join("jn"), &git);
            std::fs::write(place.join(folder).join(".git"), said).unwrap();
            let (mut taken, known) = (Taken::default(), HashSet::new());
            let by_disarming = disarm_repository(&place.join(folder).join(".git"), &place, &known, &mut taken).unwrap_err();
            assert!(by_disarming.contains("link") && taken.names.is_empty(), "{folder}: {by_disarming} {:?}", taken.names);
            assert_eq!(all_of(&git), before, "{folder}: disarming went through it");
            let looked = look(&place);
            assert!(is_refused_for_a_link(&looked), "{folder}: {:?}", looked.refused);
            assert_eq!(all_of(&git), before, "{folder}: the hook and the config of the stand-in are as they were");
            assert!(is_refused_for_a_link(&look(&place)), "{folder}: and the next command is refused for it too");
            std::fs::remove_dir(place.join(folder).join("jn")).unwrap();
            std::fs::remove_file(place.join(folder).join(".git")).unwrap();
            assert!(look(&place).refused.is_none(), "{folder}: replaced");
        }
        // A worktree's folder is in the workspace, and what it says is the repository it is part of is a junction that leads out.
        std::fs::create_dir_all(place.join("c\\wt\\hooks")).unwrap();
        std::fs::write(place.join("c\\.git"), "gitdir: wt\n").unwrap();
        std::fs::write(place.join("c\\wt\\commondir"), "link\n").unwrap();
        std::fs::write(place.join("c\\wt\\hooks\\pre-commit"), "planted\n").unwrap();
        links.junction(&place.join("c\\wt\\link"), &git);
        let looked = look(&place);
        assert!(is_refused_for_a_link(&looked), "{:?}", looked.refused);
        assert_eq!(all_of(&git), before, "what its commondir says was gone through");
        assert!(!place.join("c\\wt\\hooks\\pre-commit").exists(), "what is in the workspace is taken care of all the same");
        assert!(is_refused_for_a_link(&look(&place)), "and the next command is refused");
        drop(links);
        assert_eq!(all_of(&git), before);
    }

    /// A junction where a repository's `hooks` is, or in it, leads nowhere that the app goes: what is removed is the junction.
    #[test]
    fn a_junction_in_a_repositorys_hooks_is_removed_and_what_it_leads_to_is_not_changed() {
        let holder = tempfile::tempdir().unwrap();
        let mut links = Links(Vec::new());
        let (place, standin) = (holder.path().join("work"), holder.path().join("standin"));
        let git = stand_in_repository(&standin);
        for folder in ["a", "b"] {
            std::fs::create_dir_all(place.join(folder)).unwrap();
        }
        assert!(look(&place).refused.is_none());
        let before = all_of(&git);
        // `hooks` itself
        std::fs::create_dir_all(place.join("a\\.git")).unwrap();
        std::fs::write(place.join("a\\.git\\config"), "[core]\n\tfsmonitor = evil\n").unwrap();
        links.junction(&place.join("a\\.git\\hooks"), &git.join("hooks"));
        let looked = look(&place);
        assert_eq!(all_of(&git), before, "its hooks were gone through");
        assert!(looked.said.iter().any(|said| said.contains("a link in place of its hooks")) && looked.refused.is_none(), "{:?} {:?}", looked.said, looked.refused);
        assert!(std::fs::symlink_metadata(place.join("a\\.git\\hooks")).is_err());
        assert!(!std::fs::read_to_string(place.join("a\\.git\\config")).unwrap().contains("evil"), "and the rest is taken care of");
        // an entry of `hooks`, and one in a folder in it
        std::fs::create_dir_all(place.join("b\\.git\\hooks\\folder")).unwrap();
        links.junction(&place.join("b\\.git\\hooks\\pre-push"), &git.join("hooks"));
        links.junction(&place.join("b\\.git\\hooks\\folder\\inner"), &git.join("hooks"));
        std::fs::write(place.join("b\\.git\\hooks\\pre-commit"), "planted\n").unwrap();
        let looked = look(&place);
        assert_eq!(all_of(&git), before, "an entry of its hooks was gone through");
        assert!(looked.said.join(" ").contains("hooks/pre-commit") && looked.refused.is_none(), "{:?} {:?}", looked.said, looked.refused);
        assert!(!place.join("b\\.git\\hooks\\pre-commit").exists() && !place.join("b\\.git\\hooks\\folder").exists(), "what a command put in its hooks is gone");
        assert!(std::fs::symlink_metadata(place.join("b\\.git\\hooks\\pre-push")).is_err(), "the junction itself is gone");
        drop(links);
        assert_eq!(all_of(&git), before);
    }

    /// What is looked for is what is there, and a junction is no folder to look in: the stand-in's `.git` is never found by way of one, and a `.git` that is one is found, to be refused.
    #[test]
    fn a_junction_is_not_looked_into_and_a_git_that_is_one_is_found() {
        let holder = tempfile::tempdir().unwrap();
        let mut links = Links(Vec::new());
        let (place, standin) = (holder.path().join("work"), holder.path().join("standin"));
        let git = stand_in_repository(&standin);
        std::fs::create_dir_all(place.join("real\\.git")).unwrap();
        std::fs::create_dir_all(place.join("sub")).unwrap();
        links.junction(&place.join("sub\\.git"), &git);
        links.junction(&place.join("away"), &standin);
        let found = repositories_under(&place);
        assert!(found.contains(&place.join("real\\.git")) && found.contains(&place.join("sub\\.git")), "{found:?}");
        assert!(found.iter().all(|git| !git.starts_with(place.join("away")) && !git.starts_with(&standin)), "a junction was looked into: {found:?}");
        assert_eq!(found.len(), 2, "{found:?}");
        drop(links);
    }

    /// What is not a link goes on as it did: a `.git` file in the workspace reaches a folder in it, a repository that was there before is not touched whatever a worktree says, and a place outside
    /// the workspace is not looked at at all.
    #[test]
    fn a_gitdir_in_the_workspace_is_disarmed_a_known_repository_is_not_and_one_outside_is_left() {
        let holder = tempfile::tempdir().unwrap();
        let (place, standin) = (holder.path().join("work"), holder.path().join("standin"));
        let git = stand_in_repository(&standin);
        let mine = place.join("mine\\.git");
        std::fs::create_dir_all(mine.join("hooks")).unwrap();
        std::fs::write(mine.join("hooks\\pre-commit"), "the person's\n").unwrap();
        std::fs::write(mine.join("config"), "[alias]\n\tx = !echo\n").unwrap();
        for folder in ["wt", "far"] {
            std::fs::create_dir_all(place.join(folder)).unwrap();
        }
        assert!(look(&place).refused.is_none(), "mine is there at the first look");
        let before = all_of(&git);
        // A worktree of the person's repository: its own folder is a command's, and the repository it is part of is not.
        std::fs::create_dir_all(mine.join("worktrees\\w\\hooks")).unwrap();
        std::fs::write(mine.join("worktrees\\w\\hooks\\pre-commit"), "planted\n").unwrap();
        std::fs::write(mine.join("worktrees\\w\\commondir"), "../..\n").unwrap();
        std::fs::write(place.join("wt\\.git"), "gitdir: ../mine/.git/worktrees/w\n").unwrap();
        // Another, which says it is somewhere that is not in the workspace.
        std::fs::write(place.join("far\\.git"), format!("gitdir: {}\n", git.display())).unwrap();
        let looked = look(&place);
        assert!(looked.refused.is_none(), "{:?}", looked.refused);
        assert!(looked.said.join(" ").contains("hooks/pre-commit"), "{:?}", looked.said);
        assert!(!mine.join("worktrees\\w\\hooks\\pre-commit").exists(), "the worktree's own folder is taken care of");
        assert_eq!(std::fs::read_to_string(mine.join("hooks\\pre-commit")).unwrap(), "the person's\n");
        assert!(std::fs::read_to_string(mine.join("config")).unwrap().contains("alias"), "a repository that was there is the person's");
        assert_eq!(all_of(&git), before, "a place outside the workspace is left as it is");
    }

    #[test]
    fn what_a_person_had_is_not_touched_and_a_config_keeps_what_git_writes() {
        let holder = tempfile::tempdir().unwrap();
        let place = holder.path().join("work");
        std::fs::create_dir_all(place.join("deep/er")).unwrap();
        let own = |args: &[&str], folder: &Path| crate::tools::git(folder, args).unwrap();
        own(&["init", "-q"], &place.join("deep/er"));
        std::fs::write(place.join("deep/er/.git/hooks/pre-commit"), "the person's\n").unwrap();
        let config = place.join("deep/er/.git/config");
        std::fs::write(&config, format!("{}[core]\n\tsshCommand = theirs\n", std::fs::read_to_string(&config).unwrap())).unwrap();
        let before = std::fs::read_to_string(&config).unwrap();
        assert_eq!(command("echo x> x.txt", &place, false).disarmed, Vec::<String>::new());
        assert_eq!(command("echo x> y.txt", &place, false).disarmed, Vec::<String>::new());
        assert!(place.join("deep/er/.git/hooks/pre-commit").is_file() && std::fs::read_to_string(&config).unwrap() == before, "theirs");
        // And guarded though it is deep.
        command("echo evil> deep\\er\\.git\\hooks\\post-commit", &place, false);
        assert!(!place.join("deep/er/.git/hooks/post-commit").exists());
        drop(holder);
        let (kept, gone) = config_without_programs("[core]\n\trepositoryformatversion = 0\n\tfsmonitor = evil\n\tbare = false\n[remote \"origin\"]\n\turl = https://x/y\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n\tuploadpack = evil\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n[alias] x = !evil\n[filter \"f\"]\n\tclean = evil\\\n more evil\n\trequired\n[include]\n\tpath = evil\n[user]\n\tname = a\n");
        for stays in ["repositoryformatversion = 0", "bare = false", "url = https://x/y", "refs/remotes/origin", "remote = origin", "merge = refs/heads/main", "name = a"] {
            assert!(kept.contains(stays), "{stays}: {kept}");
        }
        assert!(!kept.contains("evil"), "{kept}");
        for name in ["core.fsmonitor", "remote.uploadpack", "alias.x", "filter.clean", "include.path"] {
            assert!(gone.iter().any(|said| said == name), "{name}: {gone:?}");
        }
    }

    /// The aside folders of a git folder: what a disarming puts what it takes out into.
    fn disarmed_folders(git: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(git).into_iter().flatten().flatten().map(|entry| entry.path()).filter(|path| path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("dogmind-disarmed-"))).collect()
    }

    /// A repository as a person has one that they cloned: a hook as husky leaves, and a config with a credential helper, an alias, `core.hooksPath` and what Git LFS needs.
    /// `.git` of it, with the hook's bytes and the config's.
    fn the_persons_own_repository(folder: &Path) -> (PathBuf, Vec<u8>, Vec<u8>) {
        let git = folder.join(".git");
        for inside in ["hooks", "objects", "refs"] {
            std::fs::create_dir_all(git.join(inside)).unwrap();
        }
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let hook = b"#!/bin/sh\n. \"$(dirname \"$0\")/_/husky.sh\"\nnpx lint-staged\n".to_vec();
        std::fs::write(git.join("hooks\\pre-commit"), &hook).unwrap();
        let config = b"[core]\n\trepositoryformatversion = 0\n\thooksPath = .husky\n[credential]\n\thelper = manager\n[alias]\n\tco = checkout\n[filter \"lfs\"]\n\tclean = git-lfs clean -- %f\n\tsmudge = git-lfs smudge -- %f\n\tprocess = git-lfs filter-process\n\trequired = true\n[lfs]\n\tstorage = lfs-store\n".to_vec();
        std::fs::write(git.join("config"), &config).unwrap();
        (git, hook, config)
    }

    /// In real commands, in a temp folder: a repository that the person makes between two commands, with no job at work, is theirs. The next command takes nothing out of it and says nothing of
    /// it, and it is guarded as any other: no command can write its hooks or its config.
    #[test]
    fn a_repository_the_person_makes_between_two_commands_is_theirs_and_is_guarded() {
        let holder = tempfile::tempdir().unwrap();
        let place = holder.path().join("work");
        std::fs::create_dir_all(&place).unwrap();
        assert_eq!(command("echo first> first.txt", &place, false).status, Some(0));
        let (git, hook, config) = the_persons_own_repository(&place.join("clone"));
        let next = command("echo second> second.txt", &place, false);
        assert_eq!(next.status, Some(0), "{}", decode(&next.stderr));
        assert!(next.disarmed.is_empty() && next.unguarded.is_none(), "{:?} {:?}", next.disarmed, next.unguarded);
        assert_eq!(std::fs::read(git.join("hooks\\pre-commit")).unwrap(), hook, "their hook, byte for byte");
        assert_eq!(std::fs::read(git.join("config")).unwrap(), config, "their config, byte for byte");
        assert!(disarmed_folders(&git).is_empty(), "nothing was put aside: nothing was taken");
        // Guarded all the same: a command cannot write its hooks, or its config.
        for line in ["echo evil> clone\\.git\\hooks\\post-commit", "echo evil>> clone\\.git\\hooks\\pre-commit", "echo evil>> clone\\.git\\config", "del clone\\.git\\config", "ren clone\\.git\\hooks hooks.old"] {
            let done = command(line, &place, false);
            assert!(done.disarmed.is_empty(), "{line}: {:?}", done.disarmed);
            assert_eq!(std::fs::read(git.join("hooks\\pre-commit")).unwrap(), hook, "{line}");
            assert_eq!(std::fs::read(git.join("config")).unwrap(), config, "{line}");
            assert!(!git.join("hooks\\post-commit").exists() && git.join("hooks").is_dir(), "{line}");
        }
        assert!(disarmed_folders(&git).is_empty());
        drop(holder);
    }

    /// In real commands: a repository that is made while a background job of the workspace is at work may be the job's, and is disarmed as one a command made. What is taken out is put aside,
    /// and the command's result names where.
    #[test]
    fn a_repository_made_while_a_background_job_runs_is_disarmed_and_what_is_taken_is_kept() {
        let holder = tempfile::tempdir().unwrap();
        let place = holder.path().join("work");
        std::fs::create_dir_all(&place).unwrap();
        assert_eq!(command("echo first> first.txt", &place, false).status, Some(0));
        let stop = Arc::new(AtomicBool::new(false));
        let job = {
            let (place, stop) = (place.clone(), stop.clone());
            std::thread::spawn(move || run("powershell -NoProfile -Command \"Start-Sleep -Seconds 60\"", &place, &place, false, Duration::from_secs(120), &stop))
        };
        let root = writable_root(&place, &place, std::env::var_os("USERPROFILE").map(PathBuf::from).as_deref()).unwrap();
        let waited = Instant::now();
        while !commands_of_workspaces().as_ref().and_then(|all| all.get(&key(&root))).is_some_and(|commands| commands.running > 0) {
            assert!(waited.elapsed() < Duration::from_secs(60), "the job did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        let (git, hook, config) = the_persons_own_repository(&place.join("made"));
        let beside = command("echo second> second.txt", &place, false);
        stop.store(true, Ordering::Relaxed);
        let ended = job.join().unwrap().unwrap();
        assert_eq!(ended.ended, "cancelled");
        assert_eq!(beside.status, Some(0), "{}", decode(&beside.stderr));
        let said = beside.disarmed.join(" ");
        assert!(said.contains("made") && said.contains("hooks/pre-commit") && said.contains("hooksPath"), "{said}");
        assert!(!git.join("hooks\\pre-commit").exists(), "taken out of its place");
        let aside = disarmed_folders(&git);
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert!(said.contains(aside[0].file_name().unwrap().to_str().unwrap()), "the note names the folder: {said}");
        assert_eq!(std::fs::read(aside[0].join("hooks\\pre-commit")).unwrap(), hook);
        assert_eq!(std::fs::read(aside[0].join("config")).unwrap(), config, "the config as it was");
        assert!(!std::fs::read_to_string(git.join("config")).unwrap().contains("hooksPath"));
        drop(holder);
    }

    /// Without a command: what a look takes as a command's and what it takes as the person's, by what has been at work since the last look.
    #[test]
    fn a_new_repository_is_a_commands_only_if_one_was_at_work_or_ran_since_the_last_look() {
        let holder = tempfile::tempdir().unwrap();
        let place = holder.path().join("work");
        std::fs::create_dir_all(&place).unwrap();
        assert!(look_between_commands(&place).said.is_empty(), "the first look takes what is there");
        // The person's, between two looks with nothing at work: not touched, and guarded.
        let (theirs, hook, config) = the_persons_own_repository(&place.join("theirs"));
        let looked = look_between_commands(&place);
        assert!(looked.said.is_empty() && looked.refused.is_none(), "{:?} {:?}", looked.said, looked.refused);
        assert_eq!(std::fs::read(theirs.join("hooks\\pre-commit")).unwrap(), hook);
        assert_eq!(std::fs::read(theirs.join("config")).unwrap(), config);
        assert!(disarmed_folders(&theirs).is_empty());
        // And it is theirs from then on, whatever is at work later.
        let job = AtWork::start(&place);
        assert!(look_between_commands(&place).said.is_empty() && std::fs::read(theirs.join("config")).unwrap() == config);
        // A job at work: what appears is taken as its making.
        let (others, other_hook, _) = the_persons_own_repository(&place.join("others"));
        let looked = look_between_commands(&place);
        assert!(looked.said.len() == 1 && looked.said[0].contains("others") && looked.said[0].contains("hooks/pre-commit"), "{:?}", looked.said);
        assert!(!others.join("hooks\\pre-commit").exists());
        let aside = disarmed_folders(&others);
        assert_eq!(aside.len(), 1);
        assert_eq!(std::fs::read(aside[0].join("hooks\\pre-commit")).unwrap(), other_hook);
        drop(job);
        // The look after a command takes what is new as the command's, whatever was or was not at work.
        let (after, _, _) = the_persons_own_repository(&place.join("after"));
        let looked = look(&place);
        assert_eq!(looked.said.len(), 1, "{:?}", looked.said);
        assert!(!after.join("hooks\\pre-commit").exists() && disarmed_folders(&after).len() == 1);
        // A command ran since the last look and no look came after it (it was cut short): the next look takes what is new as its making.
        drop(AtWork::start(&place));
        let (late, _, _) = the_persons_own_repository(&place.join("late"));
        assert_eq!(look_between_commands(&place).said.len(), 1);
        assert!(!late.join("hooks\\pre-commit").exists());
        // With none at work and none run, the person's again.
        let (mine, _, config) = the_persons_own_repository(&place.join("mine"));
        assert!(look_between_commands(&place).said.is_empty());
        assert_eq!(std::fs::read(mine.join("config")).unwrap(), config);
        drop(holder);
    }

    /// A worktree's folder and its repository's config are put aside too, each inside the git folder it was taken from; a link that stood in place of a hook leaves where it led in a note.
    #[test]
    fn what_is_taken_from_a_repository_is_put_aside_in_its_own_git_folder() {
        let holder = tempfile::tempdir().unwrap();
        let place = holder.path().join("work");
        std::fs::create_dir_all(&place).unwrap();
        assert!(look(&place).said.is_empty());
        let mut links = Links(Vec::new());
        let (git, _, config) = the_persons_own_repository(&place.join("a"));
        std::fs::write(git.join("hooks\\pre-commit.sample"), "a sample\n").unwrap();
        std::fs::create_dir_all(git.join("hooks\\folder")).unwrap();
        std::fs::write(git.join("hooks\\folder\\inner"), "in a folder\n").unwrap();
        let standin = stand_in_repository(&holder.path().join("standin"));
        let before = all_of(&standin);
        links.junction(&git.join("hooks\\pre-push"), &standin.join("hooks"));
        let looked = look(&place);
        assert!(looked.refused.is_none(), "{:?}", looked.refused);
        let aside = disarmed_folders(&git);
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert_eq!(std::fs::read(aside[0].join("config")).unwrap(), config);
        assert_eq!(std::fs::read(aside[0].join("hooks\\folder\\inner")).unwrap(), b"in a folder\n");
        assert!(git.join("hooks\\pre-commit.sample").is_file(), "a sample stays");
        assert!(std::fs::symlink_metadata(git.join("hooks\\pre-push")).is_err(), "the link is gone");
        assert!(std::fs::read_to_string(aside[0].join("links.txt")).unwrap().contains("hooks/pre-push"), "where it led is written down");
        assert!(!aside[0].join("hooks\\pre-push").exists());
        assert_eq!(all_of(&standin), before, "what the link led to is as it was");
        let said = looked.said.join(" ");
        assert!(said.contains("were moved to") && said.contains(aside[0].file_name().unwrap().to_str().unwrap()), "{said}");
        assert!(said.contains("credential") || said.contains("alias.co"), "{said}");
        drop(links);
        drop(holder);
    }

    #[test]
    fn what_a_command_may_use_leaves_the_machine_room_and_a_command_some() {
        const G: u64 = 1 << 30;
        assert_eq!(memory_cap_from(60 * G, 32 * G, 4 * G, 4 * G), 24 * G, "three quarters of its RAM");
        assert_eq!(memory_cap_from(14 * G, 32 * G, 4 * G, 4 * G), 10 * G, "what can be given while the rest goes on");
        assert_eq!(memory_cap_from(2 * G, 32 * G, 4 * G, 4 * G), 4 * G, "some, however short the machine");
    }

    /// In a temp folder, with a small limit of its own: nothing here comes near the machine's.
    #[test]
    fn a_command_past_its_memory_is_refused_it_and_said_to_have_been() {
        let holder = tempfile::tempdir().unwrap();
        let place = holder.path().join("work");
        std::fs::create_dir_all(&place).unwrap();
        let cap = 300u64 << 20;
        let greedy = "powershell -NoProfile -Command \"try { $a = [Runtime.InteropServices.Marshal]::AllocHGlobal(800000000); 'allocated' } catch { 'refused'; exit 7 }\"";
        let done = run_limited(greedy, &place, &place, false, Duration::from_secs(120), &AtomicBool::new(false), UI_LIMITS, Some(cap), false, None).unwrap();
        eprintln!("status {:?}, out_of_memory {:?}, out {} err {}", done.status, done.out_of_memory, decode(&done.stdout).trim(), decode(&done.stderr));
        assert!(decode(&done.stdout).contains("refused") && done.status == Some(7), "{}", decode(&done.stdout));
        // Bit by bit, as a leak goes: it is said to have been stopped.
        let leak = "powershell -NoProfile -Command \"try { while ($true) { $null = [Runtime.InteropServices.Marshal]::AllocHGlobal(5000000) } } catch { 'refused'; exit 7 }\"";
        let leaked = run_limited(leak, &place, &place, false, Duration::from_secs(120), &AtomicBool::new(false), UI_LIMITS, Some(cap), false, None).unwrap();
        // (What a process does at the end of its memory is its own. PowerShell catches the refusal and ends with 7, or falls over in the middle of it, which it does in about
        // one run in three, and not with one code: 0xC0000005, 0xC00000FD when the stack cannot grow by a page, 0xC0000409, or the runtime's own 0xE0434352 or 0x80131523, as
        // what it has left to run its handler with is not the same twice. Which of them is no business of the sandbox. It is stopped all the same: it ended by itself, and not
        // well, and it is said to have used all it was allowed.)
        assert!(leaked.ended.is_empty() && leaked.status.is_some_and(|status| status != 0) && leaked.out_of_memory == Some(cap), "{:?} {:?} {:?}", leaked.status, leaked.ended, leaked.out_of_memory);
        let fine = run_limited("echo fine", &place, &place, false, Duration::from_secs(60), &AtomicBool::new(false), UI_LIMITS, Some(cap), false, None).unwrap();
        assert!(decode(&fine.stdout).contains("fine") && fine.out_of_memory.is_none());
        drop(holder);
    }

    #[test]
    fn several_processes_share_one_memory_cap() {
        let holder = tempfile::tempdir().unwrap();
        let place = holder.path().join("work");
        std::fs::create_dir_all(&place).unwrap();
        // Each alone fits under the cap; four together do not.
        let one = "try { $a = [Runtime.InteropServices.Marshal]::AllocHGlobal(500000000); Write-Output held; Start-Sleep 8 } catch { Write-Output refused }";
        let line = format!("powershell -NoProfile -Command \"$p = 1..4 | ForEach-Object {{ $i = New-Object Diagnostics.ProcessStartInfo 'powershell.exe', '-NoProfile -Command {}'; $i.UseShellExecute = $false; [Diagnostics.Process]::Start($i) }}; $p | ForEach-Object {{ $_.WaitForExit() }}\"", one);
        let done = run_limited(&line, &place, &place, false, Duration::from_secs(120), &AtomicBool::new(false), UI_LIMITS, Some(1200u64 << 20), false, None).unwrap();
        let said = decode(&done.stdout);
        eprintln!("status {:?} out {} err {}", done.status, said.trim(), decode(&done.stderr).trim());
        assert!(said.contains("held"), "one fits: {said}");
        assert!(said.matches("held").count() < 4, "four of 500 MB under a cap of 1200 MB: {said}");
        drop(holder);
    }

    /// A workspace in the person's profile: PowerShell cannot stand in it, for the container may not read `C:\Users` itself, which no grant of ours can change.
    /// What the prompt and the notes tell the model to do instead works from there: absolute paths, and `Start-Process` given its folder.
    #[test]
    fn powershell_works_from_a_profile_by_absolute_paths() {
        let holder = tempfile::tempdir().unwrap();
        let folder = holder.path().join("work");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("a.txt"), "IN-A").unwrap();
        let full = folder.display().to_string();
        let done = command(&format!("powershell -NoProfile -Command \"Get-Content -LiteralPath '{full}\\a.txt'; Start-Process cmd.exe -ArgumentList '/c','echo started' -WorkingDirectory '{full}' -NoNewWindow -Wait\""), &folder, false);
        let said = decode(&done.stdout);
        assert!(said.contains("IN-A") && said.contains("started"), "{said} {}", decode(&done.stderr));
        // ... and what it says without, which the notes know
        let lost = command("powershell -NoProfile -Command \"Start-Process cmd.exe -ArgumentList '/c','echo started' -NoNewWindow -Wait\"", &folder, false);
        if !decode(&lost.stdout).contains("started") {
            assert!(crate::tools::powershell_lost_at_a_root(&decode(&lost.stderr), &folder), "{}", decode(&lost.stderr));
        }
        drop(holder);
    }

    #[test]
    fn a_folder_outside_the_workspace_does_not_run() {
        let folder = work_folder("cwd");
        assert!(run("echo x", Path::new("C:\\Windows"), &folder, false, Duration::from_secs(5), &AtomicBool::new(false)).is_err());
    }

    fn path_folders(block: &[(String, String)]) -> Vec<PathBuf> {
        std::env::split_paths(&block.iter().find(|(name, _)| name == "PATH").unwrap().1).collect()
    }

    fn build_tools() -> crate::msvc::BuildTools {
        let folders = |names: &[&str]| names.iter().map(PathBuf::from).collect::<Vec<_>>();
        crate::msvc::BuildTools { lib: folders(&["C:\\vs\\lib\\x64", "C:\\kits\\um\\x64"]), include: folders(&["C:\\vs\\include", "C:\\kits\\ucrt"]), path: folders(&["C:\\vs\\bin", "C:\\kits\\bin"]) }
    }

    #[test]
    fn a_build_gets_its_libraries_and_its_tools_come_after_everything_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let (root, work) = (dir.path().join("tools"), dir.path().join("work"));
        for folder in ["python", "node", "git\\cmd"] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        let (own_node, other) = (dir.path().join("Program Files\\nodejs"), dir.path().join("other"));
        std::fs::create_dir_all(&own_node).unwrap();
        std::fs::write(own_node.join("node.exe"), "").unwrap();
        let inherited = std::env::join_paths([&own_node, &other]).unwrap().to_string_lossy().into_owned();
        let start = || vec![("PATH".to_string(), inherited.clone()), ("SystemRoot".to_string(), "C:\\Windows".to_string())];
        let temp = dir.path().display().to_string();
        let made = environment_block(start(), Some(&root), temp.clone(), &work, || Some(build_tools()));
        assert_eq!(path_folders(&made), [root.join("python"), root.join("node"), root.join("git\\cmd"), other.clone(), PathBuf::from("C:\\vs\\bin"), PathBuf::from("C:\\kits\\bin")]);
        // The PATH alone, for a question about a name, is the same one.
        assert_eq!(path_of(start(), Some(&root), || Some(build_tools())), path_folders(&made));
        assert_eq!(path_of(start(), None, || Some(build_tools())), path_folders(&environment_block(start(), None, temp.clone(), &work, || Some(build_tools()))));
        assert_eq!(path_of(start(), Some(&root), || None), path_folders(&environment_block(start(), Some(&root), temp.clone(), &work, || None)));
        let value = |name: &str| made.iter().find(|(known, _)| known == name).map(|(_, value)| value.as_str());
        assert_eq!(value("LIB"), Some("C:\\vs\\lib\\x64;C:\\kits\\um\\x64"));
        assert_eq!(value("INCLUDE"), Some("C:\\vs\\include;C:\\kits\\ucrt"));
        // Without the bundled tools: the machine's PATH, and then the same.
        let plain = environment_block(start(), None, temp.clone(), &work, || Some(build_tools()));
        assert_eq!(path_folders(&plain), [own_node.clone(), other.clone(), PathBuf::from("C:\\vs\\bin"), PathBuf::from("C:\\kits\\bin")]);
        // On a machine with none found, PATH is what it was.
        let none = environment_block(start(), Some(&root), temp, &work, || None);
        assert_eq!(path_folders(&none), [root.join("python"), root.join("node"), root.join("git\\cmd"), other]);
        assert!(none.iter().all(|(name, _)| name != "LIB" && name != "INCLUDE"));
    }

    /// In the real sandbox, on a machine with Visual Studio's C++ tools: `cl` and `link` are found by PATH, and the headers and libraries by INCLUDE and LIB, with nothing set by hand.
    #[test]
    #[ignore = "needs Visual Studio with the C++ tools for x64, and the Windows SDK; the app must not have been started from a Developer prompt"]
    fn a_c_program_builds_in_the_sandbox_without_a_developer_prompt() {
        assert!(std::env::var_os("LIB").is_none(), "run this from a plain prompt");
        let folder = work_folder("msvc");
        std::fs::write(folder.join("hello.c"), "#include <stdio.h>\n#include <windows.h>\nint main(void) { puts(\"BUILT-IN-THE-SANDBOX\"); return 0; }\n").unwrap();
        let done = command("cl /nologo hello.c kernel32.lib user32.lib && hello.exe", &folder, false);
        let said = format!("{}{}", decode(&done.stdout), decode(&done.stderr));
        assert!(done.status == Some(0) && said.contains("BUILT-IN-THE-SANDBOX"), "{said}");
        let rc = command("rc /? | findstr /i \"Resource\"", &folder, false);
        assert_eq!(rc.status, Some(0), "rc.exe is on PATH too: {}", decode(&rc.stderr));
    }

    #[test]
    fn a_developer_prompts_variables_are_passed_on_and_nothing_is_looked_for() {
        for name in ["LIB", "INCLUDE", "LIBPATH"] {
            assert!(VARIABLES.contains(&name), "{name} is what the app was started with");
        }
        let dir = tempfile::tempdir().unwrap();
        let start = vec![("PATH".to_string(), "C:\\a".to_string()), ("LIB".to_string(), "C:\\dev\\lib".to_string()), ("INCLUDE".to_string(), "C:\\dev\\include".to_string()), ("LIBPATH".to_string(), "C:\\dev\\libpath".to_string())];
        let made = environment_block(start, None, dir.path().display().to_string(), dir.path(), || panic!("an app with a LIB of its own looks for none"));
        let value = |name: &str| made.iter().find(|(known, _)| known == name).map(|(_, value)| value.as_str());
        assert_eq!((value("PATH"), value("LIB"), value("INCLUDE"), value("LIBPATH")), (Some("C:\\a"), Some("C:\\dev\\lib"), Some("C:\\dev\\include"), Some("C:\\dev\\libpath")));
    }
}
