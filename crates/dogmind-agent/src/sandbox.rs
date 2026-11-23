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
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, CreateDesktopW, CreateWindowStationW, EnumDesktopWindows, GetProcessWindowStation, GetUserObjectInformationW, SetProcessWindowStation, HDESK, UOI_NAME};
use windows_sys::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, SE_GROUP_ENABLED};
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetWindowTextW};

/// The container commands run in. The tests have one of their own: what their runs leave (the grants on its temp folder, what their commands leave in it) was in the
/// container of the app the person uses on the same PC, which is not a place for tests; 667 grants their restricted commands had left there were inherited by all in it.
/// A development build is given another by `DOGMIND_SANDBOX_PROFILE`, for a run of the app that is a check (a dogfood run, a measurement) and not the person's own: their
/// container is no place for checks either.
fn profile_name() -> &'static str {
    #[cfg(test)]
    return "DogmindApp.Exec.Tests";
    #[cfg(not(test))]
    {
        #[cfg(debug_assertions)]
        {
            static NAMED: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
            // (a name Windows takes for a container: 64 characters at most, of letters, digits, `.`, `-` and `_`)
            let named = NAMED.get_or_init(|| {
                std::env::var("DOGMIND_SANDBOX_PROFILE").ok().filter(|name| !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
            });
            if let Some(name) = named {
                return name;
            }
        }
        "DogmindApp.Exec"
    }
}
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

/// Lets the container use a desktop of the app's own: these rights, and a label it may write under, as a container runs at low integrity. Never a window station: see `STATION`.
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
///
/// It is not the app's own. Only an administrator can name a station, and the one the system gives for no name is the same for every program of this logon
/// (`Service-0x0-<logon id>$`): the sandbox of every Chromium browser runs its processes there. So nothing is ever written on it. An ACE for this container on it was the end
/// of each of those that loads user32: win32k refused them, and Chrome and Edge played no sound and no protected video until the person signed out. Measured on Windows 11
/// 26300 with Chrome 157, on a station nothing else used: the same list written back, a label, or an ACE for an ordinary group changed nothing, and an ACE for one
/// AppContainer broke it, by either call that writes one.
static STATION: Mutex<usize> = Mutex::new(0);

/// The station, opened: made if nothing has made it yet, with the list of the person's own station, as a browser makes it. That list allows all application packages what a
/// program needs to be in a station, which is all a container gets here. (No name: see `STATION`. Everything but ending the session and reading the screen.)
unsafe fn open_station() -> Result<usize, String> {
    let (mut list, mut descriptor): (*mut ACL, PSECURITY_DESCRIPTOR) = (std::ptr::null_mut(), std::ptr::null_mut());
    let read = GetSecurityInfo(GetProcessWindowStation() as HANDLE, SE_WINDOW_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), &mut list, std::ptr::null_mut(), &mut descriptor);
    if read != 0 {
        return Err(format!("the access list of the person's window station could not be read (error {read})"));
    }
    let attributes = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: descriptor, bInheritHandle: 0 };
    let made = CreateWindowStationW(std::ptr::null(), 0, 0x37F | READ_CONTROL, &attributes);
    let failed = made.is_null().then(|| last_error("a window station for commands"));
    LocalFree(descriptor as HLOCAL);
    match failed {
        Some(failed) => Err(failed),
        None => Ok(made as usize),
    }
}

impl Desktop {
    unsafe fn new(sid: PSID) -> Result<Desktop, String> {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let name = format!("DogmindExec-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let own = wide(&name);
        let mut station = STATION.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *station == 0 {
            *station = open_station()?;
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
            .filter(|folder| !crate::bundled::replaces(folder))
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

/// A SID made from its text, freed with it.
pub(crate) struct OwnedSid(pub(crate) PSID);

impl OwnedSid {
    pub(crate) unsafe fn from_text(text: &str) -> Result<OwnedSid, String> {
        let mut sid: PSID = std::ptr::null_mut();
        if ConvertStringSidToSidW(wide(text).as_ptr(), &mut sid) == 0 {
            return Err(last_error("a workspace's SID"));
        }
        Ok(OwnedSid(sid))
    }
}

impl Drop for OwnedSid {
    fn drop(&mut self) {
        // SAFETY: a SID that ConvertStringSidToSidW allocated for this value.
        unsafe { LocalFree(self.0 as HLOCAL) };
    }
}

/// The write grant of a workspace, given ahead of its first command: when the person opens it, on a thread of the app's, as Windows takes a while over a large folder (it
/// rewrites the list of everything in it, 0.1 to 0.15 ms a thing). A command waits for it to be done (`workspace_grants::before_grant`), and finds it given. A folder that has
/// it already, as one kept on the person's list does from the first time, takes a look at its list and no more.
pub fn ready_workspace(workspace: &Path) -> Result<(), String> {
    if in_the_sandbox() {
        return Ok(());
    }
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let root = writable_root(workspace, workspace, home.as_deref())?;
    // SAFETY: the capability lives as long as the process, and the call below is given nothing else.
    unsafe {
        let capability = workspace_capability(&root)?;
        let _one = crate::workspace_grants::before_grant(&root, capability)?;
        grant(&root, capability, READ_WRITE_EXECUTE, true)
    }
}

/// An earlier grant to `old` on this folder moved to `new`: one out and the other in, in one write of the folder's list that Windows hands down to all under it, as when a
/// grant is given. One walk of the tree, where taking one back and giving the other would be two.
pub(crate) unsafe fn move_grant(root: &Path, old: PSID, new: PSID) -> Result<(), String> {
    let name = wide_path(root);
    let (mut list, mut descriptor): (*mut ACL, PSECURITY_DESCRIPTOR) = (std::ptr::null_mut(), std::ptr::null_mut());
    let read = GetNamedSecurityInfoW(name.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), &mut list, std::ptr::null_mut(), &mut descriptor);
    if read != 0 {
        return Err(format!("{}: its access list cannot be read (error {read})", root.display()));
    }
    let mut access: [EXPLICIT_ACCESS_W; 2] = std::mem::zeroed();
    for (entry, (sid, mode)) in access.iter_mut().zip([(old, REVOKE_ACCESS), (new, GRANT_ACCESS)]) {
        entry.grfAccessPermissions = READ_WRITE_EXECUTE;
        entry.grfAccessMode = mode;
        entry.grfInheritance = SUB_CONTAINERS_AND_OBJECTS_INHERIT;
        entry.Trustee.TrusteeForm = TRUSTEE_IS_SID;
        entry.Trustee.TrusteeType = TRUSTEE_IS_WELL_KNOWN_GROUP;
        entry.Trustee.ptstrName = sid as *mut u16;
    }
    let mut changed: *mut ACL = std::ptr::null_mut();
    let merged = SetEntriesInAclW(2, access.as_ptr(), list, &mut changed);
    let written = if merged != 0 { merged } else { SetNamedSecurityInfoW(name.as_ptr() as *mut u16, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), changed, std::ptr::null()) };
    if !changed.is_null() {
        LocalFree(changed as HLOCAL);
    }
    LocalFree(descriptor as HLOCAL);
    if written != 0 {
        return Err(format!("{}: the grant could not be moved (error {written})", root.display()));
    }
    Ok(())
}

/// What a command run in the container in this workspace is given besides the container, and what the workspace's write grant is to: a capability of the workspace's own,
/// made from the app's name and the folder's key. Every command runs in the one container, whose SID has what all of them read (the toolchains, the bundled tools, the app's
/// records, the folders the person opened, the way through the folders above); the write grant on a workspace is to its capability, which no command of another
/// workspace has, so a grant can stay on a workspace for as long as the person keeps it on their list (`workspace_grants`) and another workspace's commands can neither write
/// nor read there. A container's grant was taken back whenever a workspace went out of use, and given again with its first command next time: Windows rewrites the list of
/// everything under a folder for each, 0.1 to 0.15 ms a thing, and a first command waited 49.6 s for a workspace of 329,000.
pub(crate) unsafe fn workspace_capability(root: &Path) -> Result<PSID, String> {
    static MADE: Mutex<Option<HashMap<String, Sid>>> = Mutex::new(None);
    let place = key(&real(root));
    let mut made = MADE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let made = made.get_or_insert_with(HashMap::new);
    if let Some(sid) = made.get(&place) {
        return Ok(sid.0);
    }
    let name = wide(&capability_name(&place));
    let (mut groups, mut group_count, mut capabilities, mut capability_count): (*mut PSID, u32, *mut PSID, u32) = (std::ptr::null_mut(), 0, std::ptr::null_mut(), 0);
    if DeriveCapabilitySidsFromName(name.as_ptr(), &mut groups, &mut group_count, &mut capabilities, &mut capability_count) == 0 || capability_count == 0 {
        return Err(last_error("a workspace's capability"));
    }
    // (the group SIDs that come with it are a service's, and of no use here: they go, and the capability is kept for the process)
    for at in 0..group_count as usize {
        LocalFree(*groups.add(at) as HLOCAL);
    }
    LocalFree(groups as HLOCAL);
    let capability = *capabilities;
    for at in 1..capability_count as usize {
        LocalFree(*capabilities.add(at) as HLOCAL);
    }
    LocalFree(capabilities as HLOCAL);
    made.insert(place, Sid(capability));
    Ok(capability)
}

/// The name a workspace's capability is made from: the container's, and a hash of the folder's key that stays the same from one version of the app to the next.
fn capability_name(place: &str) -> String {
    format!("{}.Workspace.{:016x}", profile_name(), fnv(place))
}

/// FNV-1a, 64 bits: a short name made from a long one, the same each time.
fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3))
}

/// The temp folder of a workspace's commands that run with a restricted token: a folder of the sandbox's own temp folder, by the workspace's SID. That SID is granted
/// this folder alone. Granted the sandbox's temp folder, the list of everything in it was written anew, 47 to 50 s with 20,000 things there, while every other command
/// waited on the grants' lock; and each workspace's SID stayed on it, inherited by all that was made there after, 674 of them on this PC.
fn restricted_temp(sandbox_temp: &Path, writer: &str) -> PathBuf {
    sandbox_temp.join(format!("w-{:016x}", fnv(writer)))
}

/// The SID a workspace's commands that run with a restricted token write as ([`restricted_token`]): a random one in the form of a domain account's, made once for
/// the workspace and kept beside the app's other files. One workspace's commands cannot write in another's.
fn workspace_sid_text(root: &Path) -> String {
    static MADE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);
    let place = key(&real(root));
    let file = crate::workspace_grants::data_folder().map(|folder| folder.join("workspace-sids.txt"));
    let mut made = MADE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let made = made.get_or_insert_with(|| {
        let text = file.as_ref().and_then(|file| std::fs::read_to_string(file).ok()).unwrap_or_default();
        text.lines().filter_map(|line| line.split_once('\t')).map(|(sid, place)| (place.to_string(), sid.to_string())).collect()
    });
    if let Some(sid) = made.get(&place) {
        return sid.clone();
    }
    let sid = random_sid_text();
    made.insert(place.clone(), sid.clone());
    if let Some(file) = file {
        use std::io::Write;
        if let Ok(mut out) = std::fs::OpenOptions::new().create(true).append(true).open(&file) {
            let _ = writeln!(out, "{sid}\t{place}");
        }
    }
    sid
}

fn random_sid_text() -> String {
    use std::hash::{BuildHasher, Hasher};
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_nanos());
    let part = |n: u64| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(n);
        hasher.write_u128(now);
        hasher.write_u32(std::process::id());
        hasher.finish() as u32
    };
    format!("S-1-5-21-{}-{}-{}-{}", part(1), part(2), part(3), part(4).max(1000))
}

/// A primary token for a command that reads as the person does and writes only where `writer` may, a restricted token (CreateRestrictedToken):
/// the person's token with no privilege but going through folders (`DISABLE_MAX_PRIVILEGE`, `LUA_TOKEN`), and `WRITE_RESTRICTED`, so that the restricting SIDs
/// (`writer`, the logon session's and Everyone) are looked at for writes alone. What it makes with no list of its own can be opened by its logon session only.
unsafe fn restricted_token(writer: PSID) -> Result<Handle, String> {
    use windows_sys::Win32::System::SystemServices::SE_GROUP_LOGON_ID;
    let mut own: HANDLE = std::ptr::null_mut();
    let rights = TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT | TOKEN_ADJUST_SESSIONID | TOKEN_ADJUST_PRIVILEGES;
    if OpenProcessToken(GetCurrentProcess(), rights, &mut own) == 0 {
        return Err(last_error("the app's token"));
    }
    let own = Handle(own);
    let mut needed = 0u32;
    GetTokenInformation(own.0, TokenGroups, std::ptr::null_mut(), 0, &mut needed);
    // (words, for the alignment of what is read out of it)
    let mut groups = vec![0usize; needed as usize / std::mem::size_of::<usize>() + 1];
    if GetTokenInformation(own.0, TokenGroups, groups.as_mut_ptr().cast(), needed, &mut needed) == 0 {
        return Err(last_error("the app's token's groups"));
    }
    let list = &*(groups.as_ptr() as *const TOKEN_GROUPS);
    let entries = std::slice::from_raw_parts(list.Groups.as_ptr(), list.GroupCount as usize);
    let logon_flag = SE_GROUP_LOGON_ID as u32;
    let logon = entries.iter().find(|group| group.Attributes & logon_flag == logon_flag).map(|group| group.Sid).ok_or("the app's token has no logon SID")?;
    let mut world = [0usize; 12];
    let mut size = std::mem::size_of_val(&world) as u32;
    if CreateWellKnownSid(WinWorldSid, std::ptr::null_mut(), world.as_mut_ptr().cast(), &mut size) == 0 {
        return Err(last_error("the SID of Everyone"));
    }
    let restricting = [writer, logon, world.as_mut_ptr().cast()].map(|sid| SID_AND_ATTRIBUTES { Sid: sid, Attributes: 0 });
    let mut made: HANDLE = std::ptr::null_mut();
    if CreateRestrictedToken(own.0, DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED, 0, std::ptr::null(), 0, std::ptr::null(), restricting.len() as u32, restricting.as_ptr(), &mut made) == 0 {
        return Err(last_error("a restricted token"));
    }
    let made = Handle(made);
    // What it makes with no list of its own: its logon session's, and the owner's implicit right to change the list taken away (OWNER RIGHTS: read it only).
    let owner_rights = OwnedSid::from_text("S-1-3-4")?;
    let entries = [(logon, GENERIC_ALL), (owner_rights.0, READ_CONTROL)].map(|(sid, rights)| {
        let mut access: EXPLICIT_ACCESS_W = std::mem::zeroed();
        access.grfAccessPermissions = rights;
        access.grfAccessMode = GRANT_ACCESS;
        access.Trustee.TrusteeForm = TRUSTEE_IS_SID;
        access.Trustee.TrusteeType = TRUSTEE_IS_UNKNOWN;
        access.Trustee.ptstrName = sid as *mut u16;
        access
    });
    let mut default: *mut ACL = std::ptr::null_mut();
    let merged = SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), std::ptr::null(), &mut default);
    if merged != 0 {
        return Err(format!("a restricted token's default list could not be made (error {merged})"));
    }
    let info = TOKEN_DEFAULT_DACL { DefaultDacl: default };
    let set = SetTokenInformation(made.0, TokenDefaultDacl, std::ptr::addr_of!(info).cast(), std::mem::size_of_val(&info) as u32);
    LocalFree(default as HLOCAL);
    if set == 0 {
        return Err(last_error("a restricted token's default list"));
    }
    let mut luid: LUID = std::mem::zeroed();
    if LookupPrivilegeValueW(std::ptr::null(), wide("SeChangeNotifyPrivilege").as_ptr(), &mut luid) != 0 {
        let privileges = TOKEN_PRIVILEGES { PrivilegeCount: 1, Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }] };
        AdjustTokenPrivileges(made.0, 0, &privileges, 0, std::ptr::null_mut(), std::ptr::null_mut());
    }
    drop(groups);
    Ok(made)
}

/// What a command run with a restricted token needs before it starts: write for its SID in the workspace and in the temp folder its environment names (its own,
/// `restricted_temp`, made here), and the workspace's repositories guarded for that SID as for the container's (no new config or hooks). Nothing to read is opened: it
/// reads as the person does.
unsafe fn prepare_restricted(workspace: &Path, writer: PSID, temp: Option<&Path>, disarmed: &mut Vec<String>, times: &mut Timings) -> Result<(), String> {
    let began = Instant::now();
    grant(workspace, writer, READ_WRITE_EXECUTE, true)?;
    if let Some(temp) = temp {
        std::fs::create_dir_all(temp).map_err(|error| format!("{}: the temp folder of the command could not be made: {error}", temp.display()))?;
        grant(temp, writer, READ_WRITE_EXECUTE, true)?;
    }
    times.grant += began.elapsed();
    let began = Instant::now();
    let looked = look_after_repositories(workspace, writer, Look::BeforeACommand);
    times.repos += began.elapsed();
    *disarmed = looked.said;
    if let Some(refusal) = looked.refused {
        return Err(if disarmed.is_empty() { refusal } else { format!("{refusal}. The app also did this, in a repository made since the last command: {}", disarmed.join(" ")) });
    }
    Ok(())
}

static PROFILE_SID: Mutex<Option<Sid>> = Mutex::new(None);
static GRANTED: Mutex<Option<HashSet<(String, u32, bool)>>> = Mutex::new(None);

/// Whether this process runs in the sandbox itself: a project's own tests that run commands, run by a command of the app's, or the app built and run so,
/// in a container or with the restricted token cargo is run with (`run_live_restricted`). Neither can make a window station, a desktop or a container of its
/// own, nor write a grant on anything: every command that was run so failed before it started ("a window station for commands: access is denied", "access
/// could not be set (error 5)"). A command is then run as its caller is, in its container or with its token, with what that one was given: it can do no more
/// than its caller, which is what the sandbox is for, and the caller's own command is what was granted and looked after. (Only the container was asked,
/// and the app's own tests, run by a session as cargo is, failed 97 times for what they could not do there, 94 of the agent's and 3 of the app's.)
pub fn in_the_sandbox() -> bool {
    in_a_container() || with_a_restricted_token()
}

/// Whether this process's token is a restricted one (`restricted_token`): the person's, with SIDs every access must also pass. It reads what the person can,
/// and writes in its workspace and its own temp folder alone.
pub fn with_a_restricted_token() -> bool {
    static IN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *IN.get_or_init(|| {
        // SAFETY: the token is this process's own, opened for asking and closed here.
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let restricted = IsTokenRestricted(token) != 0;
            CloseHandle(token);
            restricted
        }
    })
}

/// Whether this process runs in an AppContainer itself (`in_the_sandbox`).
fn in_a_container() -> bool {
    static IN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *IN.get_or_init(|| {
        // SAFETY: the token is this process's own, opened for asking and closed here; the answer is a 32-bit value.
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let (mut value, mut size) = (0u32, 0u32);
            let asked = GetTokenInformation(token, TokenIsAppContainer, std::ptr::addr_of_mut!(value).cast(), std::mem::size_of::<u32>() as u32, &mut size) != 0;
            CloseHandle(token);
            asked && value != 0
        }
    })
}

pub(crate) unsafe fn profile_sid() -> Result<PSID, String> {
    let mut slot = PROFILE_SID.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(sid) = &*slot {
        return Ok(sid.0);
    }
    let (name, display, description) = (wide(profile_name()), wide("Dogmind commands"), wide("Where Dogmind runs the commands you approve"));
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
    // SAFETY: the SID lives as long as the process.
    unsafe { sid_text(profile_sid().unwrap()) }
}

/// The account that owns `path`, by its name (or its SID where it has none), when that is not the person this app runs as: then they cannot change the
/// path's access list, and the access the app gives commands does not reach it. None where it is theirs, or where that cannot be told.
pub(crate) fn owned_by_another(path: &Path) -> Option<String> {
    // SAFETY: the descriptor is the system's, freed here; the token and its buffer live through the calls.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let token = Handle(token);
        let mut needed = 0u32;
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        let mut user = vec![0u64; (needed as usize).div_ceil(8).max(1)];
        if GetTokenInformation(token.0, TokenUser, user.as_mut_ptr().cast(), needed, &mut needed) == 0 {
            return None;
        }
        let person = (*(user.as_ptr() as *const TOKEN_USER)).User.Sid;
        let name = wide_path(path);
        let (mut owner, mut descriptor): (PSID, PSECURITY_DESCRIPTOR) = (std::ptr::null_mut(), std::ptr::null_mut());
        if GetNamedSecurityInfoW(name.as_ptr(), SE_FILE_OBJECT, OWNER_SECURITY_INFORMATION, &mut owner, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), &mut descriptor) != 0 {
            return None;
        }
        let another = (!owner.is_null() && EqualSid(owner, person) == 0).then(|| account_name(owner).unwrap_or_else(|| sid_text(owner)));
        LocalFree(descriptor as HLOCAL);
        another
    }
}

/// An account's name as Windows gives it, with its domain or machine: `MACHINE\CodexSandboxOffline`.
unsafe fn account_name(sid: PSID) -> Option<String> {
    let (mut name, mut domain) = ([0u16; 256], [0u16; 256]);
    let (mut name_length, mut domain_length, mut kind) = (name.len() as u32, domain.len() as u32, 0);
    if LookupAccountSidW(std::ptr::null(), sid, name.as_mut_ptr(), &mut name_length, domain.as_mut_ptr(), &mut domain_length, &mut kind) == 0 {
        return None;
    }
    let (name, domain) = (String::from_utf16_lossy(&name[..name_length as usize]), String::from_utf16_lossy(&domain[..domain_length as usize]));
    Some(if domain.is_empty() { name } else { format!("{domain}\\{name}") })
}

/// A SID as its text (S-1-...); empty if the system cannot say it.
pub(crate) unsafe fn sid_text(sid: PSID) -> String {
    let mut text: *mut u16 = std::ptr::null_mut();
    if ConvertSidToStringSidW(sid, &mut text) == 0 || text.is_null() {
        return String::new();
    }
    let length = (0..).take_while(|&at| *text.add(at) != 0).count();
    let out = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
    LocalFree(text as HLOCAL);
    out
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

/// Whether what is under an object follows its list when `limit_to` writes it: never, always, or the first time the SID is put on it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Walk {
    No,
    Yes,
    WhenNew,
}

/// This object's list, with exactly this for the container and nothing else for it, keeping its own list from now on. Windows does not honour a deny ACE for a container's SID: what
/// keeps a container out is not being let in.
unsafe fn limit_to(root: &Path, path: &Path, sid: PSID, keep: &[(u32, u8)], walk: Walk) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let dacl = Dacl::read(path)?;
    let has = dacl.aces.iter().filter(|ace| Dacl::is_allowed_for(ace, sid)).count();
    let walk = walk == Walk::Yes || (walk == Walk::WhenNew && has == 0);
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
        return limit_to(root, git, sid, &[(READ_EXECUTE, 0)], Walk::No);
    }
    // In a repository's own `.git` this has no business: only a worktree's folder under `worktrees` has one. It tells git to read its config and hooks from wherever it says, past all of this.
    // Taken away before each command: the folder has to stay open for git's own new files, so nothing can stop it being made.
    let elsewhere = git.join("commondir");
    if std::fs::symlink_metadata(&elsewhere).is_ok() && std::fs::remove_file(&elsewhere).is_err() {
        return Err(format!("{} would send git elsewhere for its config, and could not be taken away: no command is run until it has been", elsewhere.display()));
    }
    let everything = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;
    // (handed down to what is in it the first time it is guarded for this SID: a workspace inside another has a capability of its own, and what the folder held had only the
    // other's, given by that one's grant before the folder kept its own list; the inner workspace's grant does not reach into a list kept to itself, and git there found nothing)
    limit_to(root, git, sid, &[(READ_WRITE_EXECUTE & !DELETE, 0), (READ_WRITE_EXECUTE, everything | INHERIT_ONLY_ACE as u8)], Walk::WhenNew)?;
    limit_to(root, &git.join("config"), sid, &[(READ_EXECUTE, 0)], Walk::No)?;
    limit_to(root, &git.join("hooks"), sid, &[(READ_EXECUTE, everything)], Walk::Yes)
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
    // The config as it was, byte for byte, on the disk before it is written again; and the new one whole, as a power cut leaves the old or the new, and never zeros.
    let saved = room_for(taken).is_some_and(|room| {
        File::create(room.join("config")).and_then(|mut out| std::io::Write::write_all(&mut out, &bytes).and_then(|()| out.sync_all())).is_ok()
    });
    if write_list(&config, &kept).is_ok() {
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
                limit_to(root, &folder.join("config"), sid, &[(READ_EXECUTE, 0)], Walk::No)?;
                limit_to(root, &folder.join("hooks"), sid, &[(READ_EXECUTE, everything)], Walk::Yes)?;
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

/// One ACE for the profile, once: propagating it through a tree is what costs, and it persists. (Remembered by the SID too: a restricted command's workspace SID granted a
/// folder is not the container's grant of it, which a later command of the container would otherwise skip, and start in a folder it may not open.)
pub(crate) unsafe fn grant(path: &Path, sid: PSID, mask: u32, inherit: bool) -> Result<(), String> {
    let memo = (format!("{}|{}", sid_text(sid), key(path)), mask, inherit);
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

/// What of the app's folder stays closed to commands, whatever of it is opened ([`records_readable`]): the settings, which may hold a key, and what was set aside of them,
/// the folders the person has granted and opened, the engine's own settings, and the person's notes, instructions and attachments. Nothing opens these; a test reads each
/// from a command once the records are open, and is refused.
pub const HELD_CLOSED: [&str; 10] = ["settings.json", "settings.json.broken", "workspace-grants.json", "read-folders.json", "engine.toml", "engine.toml.old", "instructions.md", "memory", "attachments", "updates"];

/// Dogmind's own records that commands may read, besides the workspace: its conversations, each with its trajectory, and the engine's log.
static RECORDS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Opens these records of the app's to each command from now on, to read and not to write: a folder with what is in it and what comes to be, a file alone, and the folders above
/// them only to pass through, the app's own folder among them, which is not listed. A session asked why its replies had slowed went outside the sandbox for each of 29 reads of
/// them, each a card for the person in real use: in the sandbox `%LOCALAPPDATA%` is the container's own folder, and the app's was closed to commands.
pub fn records_readable(records: &[PathBuf]) {
    let mut known = RECORDS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    for record in records {
        if !known.iter().any(|kept| key(kept) == key(record)) {
            known.push(record.clone());
        }
    }
}

/// Where the time of a command went apart from the program's own: what the app does before the process starts (`prepare`, in the parts of it that take time), the program from its start to its
/// end, and the look after it. A command's one duration says none of this, and a data job's seven Python commands took 30% of it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timings {
    /// From the call to just before the process is started: all of the below, and the work of the folder, the lists and the bundled tools.
    pub prepare: Duration,
    /// The grants on the workspace, on what a skill may read, and on the folders above.
    pub grant: Duration,
    /// The look through the workspace for repositories to guard.
    pub repos: Duration,
    /// The toolchains' folders, and what in them is held out.
    pub toolchains: Duration,
    /// The environment block, which may wait for the build tools.
    pub env: Duration,
    /// The process, from its start to its end.
    pub run: Duration,
    /// The look through the workspace for repositories after it.
    pub after: Duration,
    /// The walk after it for the programs it made, which are given the person's level (`programs_to_medium`): its time, the entries it looked at, and the programs changed.
    pub relabel: Duration,
    pub relabel_walked: usize,
    pub relabel_changed: usize,
    /// Whether what it made was seen as it made it (`ChangeWatch`), and how many changes that was; when not, the walk was of the whole workspace.
    pub relabel_watched: bool,
    pub relabel_seen: usize,
}

/// What a command in the container makes is labelled low, as the container itself is. A program it made (a build's .exe, .dll or .node) then runs at low integrity whoever
/// starts it: the person's own build after the model's, in the same target folder, ran cargo's build scripts low, and they were refused their own output folder ("Access is
/// denied", every time until cargo clean); and an app the model built and the person starts could not write its own settings. Low is for the commands of the sandbox, and
/// says nothing of what the person runs. So after each command the programs it made or changed are given the person's level, those labelled low. Which they are, the
/// changes Windows told of while it ran say (`ChangeWatch`); where they cannot say all of it, the workspace is walked for programs made or changed since the command began.
/// Links and junctions are not gone through (what they lead to is not the workspace's), only what the listing of a folder says is looked at, and nothing is opened but what
/// is changed. The walk has a twentieth of the command's time, or 200 ms, whichever is more; where it stops, the next command's walk goes on from there, and then walks the
/// whole again for what that command made, unless that one was watched.
pub(crate) struct Relabeled {
    pub walked: usize,
    pub changed: usize,
    pub took: Duration,
}

/// A walk not finished: the oldest start of the commands it is for, the folders left, and whether the whole is to be walked again after them.
struct ProgramWalk {
    since: std::time::SystemTime,
    folders: Vec<PathBuf>,
    again: bool,
}

/// The walks left over, by workspace.
static WALKS: Mutex<Vec<(PathBuf, ProgramWalk)>> = Mutex::new(Vec::new());

/// What runs, by its name's ending: what a build makes that is started, or loaded by what is.
const PROGRAMS: [&str; 3] = ["exe", "dll", "node"];

/// The least a walk is given, and the share of the command's own time it has beyond that.
const LEAST_WALK: Duration = Duration::from_millis(200);
const WALK_SHARE: u32 = 20;

/// After a command in `root` that began at `began` and took `took`, with what was seen of what it made, if all of it was (`Relabeled`).
pub(crate) fn programs_to_medium(root: &Path, began: std::time::SystemTime, took: Duration, seen: Option<Seen>) -> Relabeled {
    walk_for_programs(root, began, (took / WALK_SHARE).max(LEAST_WALK), seen)
}

/// As `programs_to_medium`, with the time it has. A folder is walked each time, however little that is.
fn walk_for_programs(root: &Path, began: std::time::SystemTime, budget: Duration, seen: Option<Seen>) -> Relabeled {
    let started = Instant::now();
    // (a little before it began: a file's times are kept to two seconds on some drives)
    let began = began.checked_sub(Duration::from_secs(2)).unwrap_or(began);
    let mut walks = WALKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let left = walks.iter().position(|(at, _)| at == root).map(|at| walks.remove(at).1);
    let mut walk = match (left, seen.is_some()) {
        // (what was seen is all this command made: a walk left over goes on as it was)
        (Some(mut left), true) => {
            left.since = left.since.min(began);
            left
        }
        (Some(mut left), false) => {
            left.since = left.since.min(began);
            left.again = true;
            left
        }
        (None, true) => ProgramWalk { since: began, folders: Vec::new(), again: false },
        (None, false) => ProgramWalk { since: began, folders: vec![root.to_path_buf()], again: false },
    };
    let person = PersonSid::now();
    let (mut walked, mut changed, mut folders) = (0, 0, 0);
    if let Some(seen) = seen {
        // (what came in whole, with what it held: it is walked as the workspace would be)
        walk.folders.extend(seen.folders_to_walk(root));
        for path in seen.programs {
            walked += 1;
            // (a link, as in the walk, is passed over; and so is what is in a repository's own folder)
            let Ok(data) = std::fs::symlink_metadata(&path) else { continue };
            if !data.is_file() || path.strip_prefix(root).is_ok_and(|inside| inside.components().any(|part| part.as_os_str() == ".git")) {
                continue;
            }
            // SAFETY: the SID lives as long as `person`.
            if let Some(person) = &person {
                if unsafe { to_medium(&path, person.sid()) }.unwrap_or(false) {
                    changed += 1;
                }
            }
        }
    }
    loop {
        if folders > 0 && started.elapsed() > budget {
            walks.push((root.to_path_buf(), walk));
            break;
        }
        folders += 1;
        let Some(folder) = walk.folders.pop() else {
            if walk.again {
                walk.again = false;
                walk.folders.push(root.to_path_buf());
                continue;
            }
            break;
        };
        let Ok(entries) = std::fs::read_dir(&folder) else { continue };
        for entry in entries.flatten() {
            walked += 1;
            let Ok(kind) = entry.file_type() else { continue };
            // (a link or a junction, to a file or a folder: what it leads to is not the workspace's)
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if entry.file_name() != ".git" {
                    walk.folders.push(entry.path());
                }
                continue;
            }
            let path = entry.path();
            if !is_program(&path) {
                continue;
            }
            // (made, or written again: a program copied in keeps the time it was written, and has the time it was made here)
            let Ok(data) = entry.metadata() else { continue };
            if ![data.created(), data.modified()].into_iter().flatten().any(|time| time >= walk.since) {
                continue;
            }
            // SAFETY: the SID lives as long as `person`.
            if let Some(person) = &person {
                if unsafe { to_medium(&path, person.sid()) }.unwrap_or(false) {
                    changed += 1;
                }
            }
        }
    }
    Relabeled { walked, changed, took: started.elapsed() }
}

/// What runs, by its name.
fn is_program(path: &Path) -> bool {
    path.extension().and_then(|ending| ending.to_str()).is_some_and(|ending| PROGRAMS.iter().any(|program| ending.eq_ignore_ascii_case(program)))
}

/// The changes in a workspace while a command runs, as Windows tells of them (ReadDirectoryChangesExW, for the whole tree, with each one's attributes): the programs the
/// command made are then known without a walk, and a command that made nothing costs nothing after it, however large the workspace (a walk of one of 330,000 files had
/// up to 200 ms after each command). What a folder that came in whole held (one moved in from outside the workspace) was not told of, and that folder is walked. Where
/// the watch cannot say all of it (what Windows keeps between two reads ran over, the drive does not tell of changes so, a read failed), it says nothing, and the
/// workspace is walked as before.
struct ChangeWatch {
    stop: Handle,
    thread: Option<std::thread::JoinHandle<Option<Seen>>>,
}

/// What a `ChangeWatch` saw: the programs made, written or renamed to, where they are now; the folders that came in (made, or moved in), with whether anything was seen made
/// in them; and how many changes it read.
#[derive(Default)]
pub(crate) struct Seen {
    programs: HashSet<PathBuf>,
    came: HashMap<PathBuf, bool>,
    renamed_from: Option<PathBuf>,
    changes: usize,
}

/// The changes a watch is told of: names made, taken away and renamed, of files and folders, and what is written.
const WATCHED: u32 = FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_CREATION;

/// What a read of the changes takes at most, which is also what Windows keeps of them between two reads.
const WATCH_BUFFER: usize = 256 << 10;

impl ChangeWatch {
    /// Watching `root` from now, before the command starts; nothing where it cannot be.
    fn start(root: &Path) -> Option<ChangeWatch> {
        // SAFETY: the folder's handle is given to the thread, which closes it; the stop's is closed after the thread has ended (`Drop`).
        unsafe {
            let folder = CreateFileW(wide_path(root).as_ptr(), FILE_LIST_DIRECTORY, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED, std::ptr::null_mut());
            if folder == INVALID_HANDLE_VALUE {
                return None;
            }
            let stop = Handle(CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()));
            if stop.0.is_null() {
                CloseHandle(folder);
                return None;
            }
            let (told, ready) = std::sync::mpsc::channel();
            let (root, folder_at, stop_at) = (root.to_path_buf(), folder as usize, stop.0 as usize);
            let spawned = std::thread::Builder::new().name("change-watch".into()).spawn(move || {
                let folder = Handle(folder_at as HANDLE);
                watch(&root, folder.0, stop_at as HANDLE, &told)
            });
            let Ok(thread) = spawned else {
                CloseHandle(folder);
                return None;
            };
            let watch = ChangeWatch { stop, thread: Some(thread) };
            // (from the first read on, Windows keeps what changes: the command starts after it)
            ready.recv().unwrap_or(false).then_some(watch)
        }
    }

    /// What it saw, with the command and all it started ended; nothing where it could not see all of it.
    fn finish(mut self) -> Option<Seen> {
        // SAFETY: the event is this value's.
        unsafe { SetEvent(self.stop.0) };
        self.thread.take()?.join().ok().flatten()
    }
}

impl Drop for ChangeWatch {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            // SAFETY: the event is this value's, and is closed after the thread has ended.
            unsafe { SetEvent(self.stop.0) };
            let _ = thread.join();
        }
    }
}

/// The thread of a `ChangeWatch`: it reads until the stop, then what Windows kept for it to then, and says what it saw, or nothing where it could not see all of it. That
/// the first read was asked is told on `told`.
unsafe fn watch(root: &Path, folder: HANDLE, stop: HANDLE, told: &std::sync::mpsc::Sender<bool>) -> Option<Seen> {
    let mut seen = Seen::default();
    let mut buffer = vec![0u64; WATCH_BUFFER / 8];
    let event = Handle(CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()));
    let (mut first, mut stopping) = (true, false);
    loop {
        let mut overlapped: OVERLAPPED = std::mem::zeroed();
        overlapped.hEvent = event.0;
        let asked = !event.0.is_null() && ReadDirectoryChangesExW(folder, buffer.as_mut_ptr().cast(), WATCH_BUFFER as u32, 1, WATCHED, std::ptr::null_mut(), &mut overlapped, None, ReadDirectoryNotifyExtendedInformation) != 0;
        if first {
            first = false;
            let _ = told.send(asked);
        }
        if !asked {
            return None;
        }
        if !stopping {
            let both = [event.0, stop];
            stopping = WaitForMultipleObjects(2, both.as_ptr(), 0, INFINITE) != WAIT_OBJECT_0;
        }
        let mut got = 0u32;
        // (after the stop, what was kept since the last read comes at once; a read with nothing to give waits, and is called off)
        if GetOverlappedResult(folder, &overlapped, &mut got, 0) == 0 {
            if GetLastError() != ERROR_IO_INCOMPLETE {
                return None;
            }
            CancelIoEx(folder, &overlapped);
            if GetOverlappedResult(folder, &overlapped, &mut got, 1) == 0 {
                return (GetLastError() == ERROR_OPERATION_ABORTED).then_some(seen);
            }
        }
        // (none: the changes ran over what is kept of them, and all of it was let go)
        if got == 0 {
            return None;
        }
        seen.read(root, &buffer, got as usize);
    }
}

impl Seen {
    /// The changes of one read, `got` bytes of `buffer`.
    unsafe fn read(&mut self, root: &Path, buffer: &[u64], got: usize) {
        use std::os::windows::ffi::OsStringExt;
        let bytes = buffer.as_ptr().cast::<u8>();
        let name_at = std::mem::offset_of!(FILE_NOTIFY_EXTENDED_INFORMATION, FileName);
        let mut at = 0;
        while at + name_at <= got {
            let entry = bytes.add(at).cast::<FILE_NOTIFY_EXTENDED_INFORMATION>();
            let next = std::ptr::addr_of!((*entry).NextEntryOffset).read_unaligned() as usize;
            let action = std::ptr::addr_of!((*entry).Action).read_unaligned();
            let attributes = std::ptr::addr_of!((*entry).FileAttributes).read_unaligned();
            let length = std::ptr::addr_of!((*entry).FileNameLength).read_unaligned() as usize / 2;
            if at + name_at + length * 2 > got {
                break;
            }
            let mut name = vec![0u16; length];
            std::ptr::copy_nonoverlapping(bytes.add(at + name_at), name.as_mut_ptr().cast::<u8>(), length * 2);
            self.changes += 1;
            self.note(action, attributes, root.join(std::ffi::OsString::from_wide(&name)));
            if next == 0 {
                break;
            }
            at += next;
        }
    }

    /// One change.
    fn note(&mut self, action: u32, attributes: u32, path: PathBuf) {
        let folder = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        // (a link to a folder leads out of the workspace, and is not walked)
        let link = attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        // (something made in a folder that came in: what is in that one is seen as it is made)
        if let Some(inside) = path.parent().and_then(|parent| self.came.get_mut(parent)) {
            *inside = true;
        }
        match action {
            FILE_ACTION_RENAMED_OLD_NAME => {
                self.renamed_from = Some(path);
                return;
            }
            // (a folder renamed: what was seen in it is where it is now; what it held from before, its own commands were seen to)
            FILE_ACTION_RENAMED_NEW_NAME if folder => match self.renamed_from.take() {
                Some(from) => self.moved(&from, &path),
                None if !link => {
                    self.came.insert(path, false);
                }
                None => {}
            },
            FILE_ACTION_ADDED if folder && !link => {
                self.came.insert(path, false);
            }
            FILE_ACTION_ADDED | FILE_ACTION_MODIFIED | FILE_ACTION_RENAMED_NEW_NAME if !folder && is_program(&path) => {
                self.programs.insert(path);
            }
            _ => {}
        }
        self.renamed_from = None;
    }

    /// What was seen in `from`, renamed to `to`.
    fn moved(&mut self, from: &Path, to: &Path) {
        let now = |path: PathBuf| match path.strip_prefix(from) {
            Ok(rest) if rest.as_os_str().is_empty() => to.to_path_buf(),
            Ok(rest) => to.join(rest),
            Err(_) => path,
        };
        self.programs = std::mem::take(&mut self.programs).into_iter().map(now).collect();
        self.came = std::mem::take(&mut self.came).into_iter().map(|(path, inside)| (now(path), inside)).collect();
    }

    /// The folders that came in with nothing seen made in them, which hold what came with them: one in another is in that one's walk. Not what is now a link, nor a
    /// repository's own folder.
    fn folders_to_walk(&self, root: &Path) -> Vec<PathBuf> {
        let mut unseen: Vec<&PathBuf> = self.came.iter().filter(|(_, inside)| !**inside).map(|(path, _)| path).collect();
        // (in the order of their parts, a folder's own come right after it)
        unseen.sort();
        let mut walked: Vec<PathBuf> = Vec::new();
        for path in unseen {
            if walked.last().is_some_and(|above| path.starts_with(above)) || path.strip_prefix(root).is_ok_and(|inside| inside.components().any(|part| part.as_os_str() == ".git")) {
                continue;
            }
            if std::fs::symlink_metadata(path).is_ok_and(|data| data.is_dir()) {
                walked.push(path.clone());
            }
        }
        walked
    }
}

/// What an earlier version's restricted token's commands left on the sandbox's temp folder (`profile_folder`): a grant to each workspace's SID, on the folder itself and
/// inherited by all in it. 667 were on this PC's, three more with each whole run of the tests, and each grant there carried all of them down to everything in it again.
/// Taken off at the app's start, off its thread, in one write of the folder's list, which Windows carries down; nothing puts one there again (`restricted_temp`), and a
/// restricted command meanwhile does not wait, in a folder of its own. How many there were, and how long it took; nothing when there were none.
pub fn clear_old_restricted_grants() -> Option<(usize, Duration)> {
    if in_the_sandbox() {
        return None;
    }
    // SAFETY: the SIDs live through the calls.
    let cleared = unsafe {
        let temp = profile_folder(profile_sid().ok()?)?;
        let person = PersonSid::now()?;
        clear_restricted_grants(&temp, person.sid()).ok().filter(|(count, _)| *count > 0)
    };
    // (a grant written on a folder of it while the list was carried down is looked at again, not taken as given)
    if cleared.is_some() {
        forget_grants();
    }
    cleared
}

/// As `clear_old_restricted_grants`, on `folder`: an allowing grant to all in it, of read, write and run, that is the folder's own, to a SID of the form a workspace's has
/// (a domain account's), and not the person's.
unsafe fn clear_restricted_grants(folder: &Path, person: PSID) -> Result<(usize, Duration), String> {
    let began = Instant::now();
    let list = Dacl::read(folder)?;
    let old = |ace: &Vec<u8>| {
        let sid = ace[8..].as_ptr() as PSID;
        u32::from(ace[0]) == ACCESS_ALLOWED_ACE_TYPE
            && ace[1] == (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8
            && u32::from_ne_bytes([ace[4], ace[5], ace[6], ace[7]]) == READ_WRITE_EXECUTE
            && EqualSid(sid, person) == 0
            && sid_text(sid).starts_with("S-1-5-21-")
    };
    let count = list.aces.iter().filter(|ace| old(ace)).count();
    if count == 0 {
        return Ok((0, began.elapsed()));
    }
    let kept = Dacl { aces: list.aces.iter().filter(|ace| !old(ace)).cloned().collect(), revision: list.revision, control: list.control };
    let acl = kept.to_acl();
    let name = wide_path(folder);
    match SetNamedSecurityInfoW(name.as_ptr() as *mut u16, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), acl.as_ptr().cast(), std::ptr::null()) {
        0 => Ok((count, began.elapsed())),
        error => Err(format!("{}: its list could not be written (error {error})", folder.display())),
    }
}

/// The person this app runs as: their SID, in the buffer the token gave it in.
struct PersonSid(Vec<u64>);

impl PersonSid {
    fn now() -> Option<PersonSid> {
        // SAFETY: the token is this function's, and the buffer is as long as the call is told.
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let token = Handle(token);
            let mut needed = 0u32;
            GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            let mut user = vec![0u64; (needed as usize).div_ceil(8).max(1)];
            (GetTokenInformation(token.0, TokenUser, user.as_mut_ptr().cast(), needed, &mut needed) != 0).then_some(PersonSid(user))
        }
    }

    fn sid(&self) -> PSID {
        // SAFETY: the buffer holds a TOKEN_USER, whose SID is in it.
        unsafe { (*(self.0.as_ptr() as *const TOKEN_USER)).User.Sid }
    }
}

/// The label of the low integrity level, as an ACE of a SACL says it.
const LABEL_ACE: u8 = 0x11;
const LOW_RID: u32 = 0x1000;

/// Whether this file is labelled low.
unsafe fn labelled_low(name: &[u16]) -> Result<bool, String> {
    let (mut sacl, mut descriptor): (*mut ACL, PSECURITY_DESCRIPTOR) = (std::ptr::null_mut(), std::ptr::null_mut());
    let read = GetNamedSecurityInfoW(name.as_ptr(), SE_FILE_OBJECT, LABEL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), &mut sacl, &mut descriptor);
    if read != 0 {
        return Err(format!("its label cannot be read (error {read})"));
    }
    let mut low = false;
    if !sacl.is_null() {
        for at in 0..u32::from((*sacl).AceCount) {
            let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
            if GetAce(sacl, at, &mut ace) == 0 || (*(ace as *const ACE_HEADER)).AceType != LABEL_ACE {
                continue;
            }
            let label = std::ptr::addr_of!((*(ace as *const SYSTEM_MANDATORY_LABEL_ACE)).SidStart) as PSID;
            let count = *GetSidSubAuthorityCount(label);
            low = count > 0 && *GetSidSubAuthority(label, u32::from(count) - 1) == LOW_RID;
        }
    }
    LocalFree(descriptor as HLOCAL);
    Ok(low)
}

/// The person's level for a program labelled low, as `labelled_low` says: true when it was changed. A label is changed with WRITE_OWNER, which the lists of a workspace do
/// not give the person (as its owner they have READ_CONTROL and WRITE_DAC, and no more): it is given them on this file for the change, and the list is then as it was.
unsafe fn to_medium(path: &Path, person: PSID) -> Result<bool, String> {
    let name = wide_path(path);
    if !labelled_low(&name)? {
        return Ok(false);
    }
    let before = Dacl::read(path)?;
    before.with_exact(person, WRITE_OWNER, 0).write_alone(path)?;
    let label = wide("S:(ML;;NW;;;ME)");
    let mut medium: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let labelled = if ConvertStringSecurityDescriptorToSecurityDescriptorW(label.as_ptr(), 1, &mut medium, std::ptr::null_mut()) == 0 {
        Err(last_error("the person's label"))
    } else {
        let (mut present, mut sacl, mut defaulted): (BOOL, *mut ACL, BOOL) = (0, std::ptr::null_mut(), 0);
        let set = if GetSecurityDescriptorSacl(medium, &mut present, &mut sacl, &mut defaulted) == 0 {
            Err(last_error("the person's label"))
        } else {
            match SetNamedSecurityInfoW(name.as_ptr() as *mut u16, SE_FILE_OBJECT, LABEL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), sacl) {
                0 => Ok(()),
                error => Err(format!("{}: its label could not be set (error {error})", path.display())),
            }
        };
        LocalFree(medium as HLOCAL);
        set
    };
    // (the list as it was, whatever came of the label)
    let restored = before.write_alone(path);
    labelled.and(restored).map(|()| true)
}

/// The timings of the commands run since `forget_timings`, for the trajectory's line of the call.
static TIMINGS: Mutex<Vec<Timings>> = Mutex::new(Vec::new());

pub fn forget_timings() {
    TIMINGS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
}

pub fn take_timings() -> Vec<Timings> {
    std::mem::take(&mut *TIMINGS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
}

/// `sid` is the container's, `writer` the workspace's capability (`workspace_capability`).
unsafe fn prepare(workspace: &Path, sid: PSID, writer: PSID, disarmed: &mut Vec<String>, times: &mut Timings) -> Result<(), String> {
    let began = Instant::now();
    grant(workspace, writer, READ_WRITE_EXECUTE, true)?;
    times.grant += began.elapsed();
    // Each repository in the workspace. If one cannot be guarded the command does not run, as with any other containment.
    let began = Instant::now();
    let looked = look_after_repositories(workspace, writer, Look::BeforeACommand);
    times.repos += began.elapsed();
    *disarmed = looked.said;
    if let Some(refusal) = looked.refused {
        // (what was taken out of a repository on the way is said too: nothing the app does as the person is done quietly)
        return Err(if disarmed.is_empty() { refusal } else { format!("{refusal}. The app also did this, in a repository made since the last command: {}", disarmed.join(" ")) });
    }
    let began = Instant::now();
    for folder in ALSO_READABLE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter() {
        let _ = grant(folder, sid, READ_EXECUTE, true);
        for ancestor in folder.ancestors().skip(1).filter(|above| above.parent().is_some()) {
            let _ = grant(ancestor, sid, FILE_GENERIC_EXECUTE, false);
        }
    }
    // (Dogmind's records, to read alone: `FILE_GENERIC_READ`, which a folder's things inherit, and the folders above them to pass through. A file's folder is listed as well, and
    // nothing in it read: cmd's `type` and `findstr` look a file up in its folder's list first, and were refused the engine's log, though it could be read.)
    for record in RECORDS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter().filter(|record| record.exists()) {
        let folder = record.is_dir();
        let _ = grant(record, sid, FILE_GENERIC_READ, folder);
        if let Some(parent) = record.parent().filter(|_| !folder) {
            let _ = grant(parent, sid, READ_EXECUTE, false);
        }
        grant_way_to(record, sid);
    }
    // The folders above: their attributes only, and only for shells that look at where they are. An OS-owned one may refuse, which costs little.
    for ancestor in workspace.ancestors().skip(1).filter(|folder| folder.parent().is_some()) {
        let _ = grant(ancestor, sid, FILE_GENERIC_EXECUTE, false);
    }
    times.grant += began.elapsed();
    // (never a test's: these are the real home's folders, with real lists)
    #[cfg(not(test))]
    if let (Some(path), Some(home)) = (std::env::var_os("PATH"), std::env::var_os("USERPROFILE")) {
        let began = Instant::now();
        open_toolchains(&path.to_string_lossy(), Path::new(&home), crate::bundled::root().map(|root| crate::bundled::Present::of(&root)), sid);
        times.toolchains += began.elapsed();
    }
    Ok(())
}

pub(crate) unsafe fn open_toolchains(path_variable: &str, home: &Path, bundled: Option<crate::bundled::Present>, sid: PSID) {
    for folder in toolchain_folders(path_variable, home) {
        // (a person's own Python or Node is not what runs when the bundle has its own, so nothing is opened for it; where the bundle lacks one, theirs is)
        let replaced = bundled.is_some_and(|present| {
            let folder_key = key(&folder);
            crate::bundled::NAMES.iter().any(|name| present.covers(name.trim_end_matches(".exe")) && folder.join(name).is_file())
                || (present.python && folder_key.contains("python"))
                || (present.node && (folder_key.contains("npm") || folder_key.contains("nvm")))
        });
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

/// The sandbox's own temp folder: what a command leaves there, the app can read. With a restricted token, the profile's folder is not this process's to write
/// in, and its own temp folder is what it has (`restricted_temp`), as a command it starts does.
pub(crate) fn temp_folder() -> Option<PathBuf> {
    if with_a_restricted_token() {
        return std::env::var_os("TEMP").map(PathBuf::from);
    }
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

/// What points a command that runs with a restricted token away from the network when it was not let reach it: a proxy that is not there for what
/// honours one, and the offline switches of cargo, npm and pip. Nothing enforces it.
const OFFLINE: [(&str, &str); 10] = [
    ("HTTP_PROXY", "http://127.0.0.1:9"), ("HTTPS_PROXY", "http://127.0.0.1:9"), ("ALL_PROXY", "http://127.0.0.1:9"), ("NO_PROXY", "localhost,127.0.0.1,::1"),
    ("GIT_HTTP_PROXY", "http://127.0.0.1:9"), ("GIT_HTTPS_PROXY", "http://127.0.0.1:9"), ("CARGO_NET_OFFLINE", "true"), ("NPM_CONFIG_OFFLINE", "true"),
    ("PIP_NO_INDEX", "1"), ("PIP_DISABLE_PIP_VERSION_CHECK", "1"),
];

/// Where a command's Python finds `PYTHON_SHIM` as its sitecustomize: a folder of the sandbox's own temp folder, put first on PYTHONPATH.
const PYTHON_SHIM_FOLDER: &str = "dogmind-python";
/// The sitecustomize a command's Python starts with, whichever Python it is (a person's own has none of the bundled one's): see its own words.
const PYTHON_SHIM: &str = r#"# Dogmind's, for a command in its sandbox. On Windows, Python 3.13 and later make a folder of mode 0o700 (tempfile.mkdtemp, TemporaryDirectory,
# pytest's tmp_path, pip) with a list of only the user, SYSTEM and the administrators on it, which shuts a process under an AppContainer out of
# the folder it has just made: a test that uses one fails with Permission denied, and the folder cannot be removed. It is made with its parent's
# list instead, as Python made it before 3.13. The sitecustomize that comes after this one on the path (a bundled Python's, or a project's) runs too.
import os
import sys

if os.name == "nt" and not getattr(os.mkdir, "_dogmind_open", False):
    _dogmind_mkdir = os.mkdir

    def _dogmind_open_mkdir(path, mode=0o777, *, dir_fd=None):
        return _dogmind_mkdir(path, 0o777 if mode == 0o700 else mode, dir_fd=dir_fd)

    _dogmind_open_mkdir._dogmind_open = True
    os.mkdir = _dogmind_open_mkdir


def _dogmind_next_sitecustomize():
    import importlib.machinery
    import importlib.util

    here = os.path.normcase(os.path.dirname(os.path.abspath(__file__)))
    rest = [entry for entry in sys.path if os.path.normcase(os.path.abspath(entry or os.curdir)) != here]
    spec = importlib.machinery.PathFinder.find_spec("sitecustomize", rest)
    if spec is not None and spec.loader is not None:
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)


_dogmind_next_sitecustomize()
"#;

/// `PYTHON_SHIM` in the sandbox's temp folder, written where it is not there as it is. A command can read it there, and nothing a command does to it outlasts the next.
fn put_python_shim(temp: &str) {
    if temp.is_empty() {
        return;
    }
    let file = Path::new(temp).join(PYTHON_SHIM_FOLDER).join("sitecustomize.py");
    if std::fs::read_to_string(&file).ok().as_deref() != Some(PYTHON_SHIM) {
        let _ = std::fs::create_dir_all(file.parent().expect("a folder"));
        let _ = std::fs::write(&file, PYTHON_SHIM);
    }
}

/// A command's environment, as the block CreateProcess takes. `build_tools` is asked for the compiler's folders only when the app has no `LIB` of its own: the command's start waits for it, so it
/// is given the time the command has and the person's Stop (see `crate::msvc::found_before`).
fn environment(temp: Option<&Path>, workspace: &Path, offline: bool, as_person: bool, build_tools: impl FnOnce() -> Option<crate::msvc::BuildTools>) -> Vec<u16> {
    let block: Vec<(String, String)> = VARIABLES.iter().filter_map(|name| std::env::var(name).ok().map(|value| (name.to_string(), value))).collect();
    let temp = temp.map(|path| path.display().to_string()).or_else(|| std::env::var("TEMP").ok()).unwrap_or_default();
    if !as_person {
        put_python_shim(&temp);
    }
    let mut block = environment_block(block, crate::bundled::root().as_deref(), temp, workspace, build_tools);
    // (as the person, git reads their own settings and Python starts as it does for them: what stands in for those in the sandbox is not for them)
    if as_person {
        block.retain(|(name, _)| !["XDG_CONFIG_HOME", "PYTHONPATH", "GIT_CONFIG_GLOBAL", POWERSHELL_POLICY.0, NO_ZONE_CHECKS.0].iter().any(|kept_out| name.eq_ignore_ascii_case(kept_out)));
        their_git_first(&mut block, &std::env::var("PATH").unwrap_or_default(), crate::bundled::root().as_deref());
        // (their credential helper may not ask anything on a desktop nobody sees: what it has stored is used, and without it the push fails at once and says so)
        block.push(("GCM_INTERACTIVE".to_string(), "never".to_string()));
        block.sort_by_key(|(name, _)| name.to_uppercase());
    }
    if offline {
        block.retain(|(name, _)| !OFFLINE.iter().any(|(set, _)| set.eq_ignore_ascii_case(name)));
        block.extend(OFFLINE.iter().map(|(name, value)| (name.to_string(), value.to_string())));
        block.sort_by_key(|(name, _)| name.to_uppercase());
    }
    let mut out = Vec::new();
    for (name, value) in block {
        out.extend(format!("{name}={value}").encode_utf16());
        out.push(0);
    }
    out.push(0);
    out
}

/// As the person, their own Git comes first on PATH, ahead of the bundled one: with it come its settings, and Git for Windows keeps its credential helper
/// (`credential.helper=manager`) in its own system config, which the bundled Git does not read. Without it `git push` as the person had no credentials and
/// could only fail ("could not read Username for 'https://github.com': terminal prompts disabled", a real session of 2026-11-22). Nothing changes where they
/// have no Git of their own.
fn their_git_first(block: &mut [(String, String)], inherited: &str, bundled: Option<&Path>) {
    let theirs = std::env::split_paths(inherited).find(|folder| folder.join("git.exe").is_file() && !bundled.is_some_and(|root| key(folder).starts_with(&key(root))));
    let (Some(theirs), Some((_, path))) = (theirs, block.iter_mut().find(|(name, _)| name.eq_ignore_ascii_case("PATH"))) else { return };
    let rest: Vec<PathBuf> = std::env::split_paths(path.as_str()).filter(|folder| key(folder) != key(&theirs)).collect();
    if let Ok(joined) = std::env::join_paths(std::iter::once(theirs).chain(rest)) {
        *path = joined.to_string_lossy().into_owned();
    }
}

/// A command's environment from what it inherits. The PATH comes out as the bundled tools and the folder `npm install -g` fills, then what the machine has (without a person's own Python,
/// Node or Git, and without a folder of MSYS or Cygwin programs, which cannot start here and would stand in for Windows' own `find` and `sort`), and last the folders of Visual Studio's
/// compiler when `build_tools` finds them: nothing that is earlier on it can be shadowed by a later one.
fn environment_block(mut block: Vec<(String, String)>, bundled: Option<&Path>, temp: String, workspace: &Path, build_tools: impl FnOnce() -> Option<crate::msvc::BuildTools>) -> Vec<(String, String)> {
    crate::msvc::add_to(&mut block, build_tools);
    // (before anything is made of it, with a bundle or without: `path_of` leaves the same ones off)
    if let Some((_, path)) = block.iter_mut().find(|(name, _)| name.eq_ignore_ascii_case("PATH")) {
        *path = crate::bundled::without_unix_runtimes(path);
    }
    if let Some(root) = bundled {
        let inherited = block.iter().find(|(name, _)| name.eq_ignore_ascii_case("PATH")).map(|(_, path)| path.clone()).unwrap_or_default();
        let added = crate::bundled::environment(root, &inherited, &temp, workspace);
        block.retain(|(name, _)| !added.iter().any(|(new, _)| new.eq_ignore_ascii_case(name)));
        block.extend(added);
    }
    // Where git looks for its per-user files: the person's profile is closed to a command, and it would warn on every run.
    block.push(("XDG_CONFIG_HOME".to_string(), temp.clone()));
    // (its sitecustomize first, for any Python: a folder of mode 0o700 made under the container is one the container cannot open, see PYTHON_SHIM)
    if !temp.is_empty() {
        block.push(("PYTHONPATH".to_string(), format!("{temp}\\{PYTHON_SHIM_FOLDER}")));
    }
    block.push(("TEMP".to_string(), temp.clone()));
    block.push(("TMP".to_string(), temp));
    // (the CLI of the skill that comes with the app sends usage reports to its makers otherwise, and it runs with no one asked: see `safety::the_guidance_cli`. Other programs that read it send none either)
    block.retain(|(name, _)| !name.eq_ignore_ascii_case("DISABLE_TELEMETRY"));
    block.push(("DISABLE_TELEMETRY".to_string(), "1".to_string()));
    // (in a container PowerShell's check of a script against the execution policy fails whatever the policy, "AuthorizationManager check failed", and no `.ps1`
    // ran with -File: a project's own scripts, a test's screenshot script among them. The policy keeps no one out of anything, the sandbox does: for this
    // process and what it starts, it is not asked)
    block.retain(|(name, _)| !name.eq_ignore_ascii_case(POWERSHELL_POLICY.0));
    block.push((POWERSHELL_POLICY.0.to_string(), POWERSHELL_POLICY.1.to_string()));
    // (a program a command starts the way a person's shell does, Start-Process among them, was checked for where it came from, and the question about it, "Open
    // File - Security Warning", was put on the command's desktop, where nobody sees it: the command waited until it was stopped, a session's screenshot script
    // among them. Its own question keeps no one out of anything here: what it starts runs in the same sandbox)
    block.retain(|(name, _)| !name.eq_ignore_ascii_case(NO_ZONE_CHECKS.0));
    block.push((NO_ZONE_CHECKS.0.to_string(), NO_ZONE_CHECKS.1.to_string()));
    block.sort_by_key(|(name, _)| name.to_uppercase());
    block
}

/// The execution policy a command's PowerShell takes, for itself and what it starts: see `environment_block`.
const POWERSHELL_POLICY: (&str, &str) = ("PSExecutionPolicyPreference", "Bypass");
/// No question of where a program came from, when a command starts one through the shell: see `environment_block`.
const NO_ZONE_CHECKS: (&str, &str) = ("SEE_MASK_NOZONECHECKS", "1");

/// The folders on the PATH a command starts with, in order, made as `environment_block` makes it: the bundled tools, the machine's without a person's own Python, Node or Git and without MSYS
/// or Cygwin folders, and then Visual Studio's. Only the PATH is made: `environment_block` goes on to ask git for a name and to write a config file, which a question about a name in a command
/// must not do.
fn path_of(mut block: Vec<(String, String)>, bundled: Option<&Path>, build_tools: impl FnOnce() -> Option<crate::msvc::BuildTools>) -> Vec<PathBuf> {
    crate::msvc::add_to(&mut block, build_tools);
    let inherited = block.iter().find(|(name, _)| name.eq_ignore_ascii_case("PATH")).map(|(_, path)| path.clone()).unwrap_or_default();
    let inherited = crate::bundled::without_unix_runtimes(&inherited);
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
    run_limited(line, cwd, workspace, network, timeout, cancel, UI_LIMITS, Some(job_memory_cap()), false, live, Token::Container)
}

/// As `run_live`, with a restricted token in place of the container ([`restricted_token`]): it reads what the person can, and writes only in the workspace and its temp
/// folder. For cargo in a workspace whose `members` pattern it expands from the drive's root, looking at C:\Users on the way, which a container may not.
pub fn run_live_restricted(line: &str, cwd: &Path, workspace: &Path, network: bool, timeout: Duration, cancel: &AtomicBool, live: Option<Arc<crate::tools::Live>>) -> Result<Output, String> {
    run_limited(line, cwd, workspace, network, timeout, cancel, UI_LIMITS, Some(job_memory_cap()), false, live, Token::Restricted)
}

/// As `run_live`, as the person, outside the sandbox: what they asked to have done outside the workspace, once they have allowed it (the gate asks with `REASON_OUTSIDE_SANDBOX`,
/// which no rule, "always" or mode answers): what the person asked for there could not be done at all in the sandbox. It reads and writes what the person can and has the network. It is still a job that ends
/// with the app, on a desktop nobody sees, under the same time and memory limits.
pub fn run_live_as_person(line: &str, cwd: &Path, workspace: &Path, timeout: Duration, cancel: &AtomicBool, live: Option<Arc<crate::tools::Live>>) -> Result<Output, String> {
    run_limited(line, cwd, workspace, true, timeout, cancel, UI_LIMITS, Some(job_memory_cap()), false, live, Token::Person)
}

/// What a command runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    /// In the AppContainer: it reads and writes the workspace, and what the person has opened to commands.
    Container,
    /// With a restricted token: it reads what the person can, and writes in the workspace alone.
    Restricted,
    /// As the person, with nothing taken away.
    Person,
}

/// A browser under those limits falls over in most runs, whichever of them is set (0xC0000005, 12 to 19 runs in 20, against none in 20 with none set). Its own window station
/// keeps it from the person's clipboard and session in any case.
///
/// Its input is a closed pipe, not the NUL device. With the NUL device Edge's crash handler (`--type=crashpad-handler`, which the browser starts) began with a standard input that
/// was not a handle of its own process (its output and error were), and ended in the loader with STATUS_INVALID_HANDLE (0xC0000008) before it ran; with a pipe it runs, to the end
/// of the browser's. With no handler to answer, the browser's other processes waited for it in `WaitNamedPipe` (crashpad's registration), and in about one run in three the page was
/// never opened: no end and no output until the time was up.
pub fn run_browser(line: &str, cwd: &Path, workspace: &Path, timeout: Duration, cancel: &AtomicBool) -> Result<Output, String> {
    run_limited(line, cwd, workspace, false, timeout, cancel, 0, None, true, None, Token::Container)
}

#[allow(clippy::too_many_arguments)]
fn run_limited(line: &str, cwd: &Path, workspace: &Path, network: bool, timeout: Duration, cancel: &AtomicBool, ui_limits: u32, memory_cap: Option<u64>, closed_stdin: bool, live: Option<Arc<crate::tools::Live>>, token: Token) -> Result<Output, String> {
    let asked_at = Instant::now();
    // (what it makes from here is what is looked for after it: see `programs_to_medium`)
    let began_at = std::time::SystemTime::now();
    // (the build tools are looked for while the folder is made ready below, and the environment then usually finds them ready)
    crate::msvc::look_ahead();
    no_error_boxes();
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let root = writable_root(cwd, workspace, home.as_deref())?;
    if let Some(reason) = volume_of(&root).and_then(|volume| drive_refusal(&volume, &root)) {
        return Err(reason);
    }
    // (in the sandbox already, nothing is granted, made or looked after, and the command is started as this process is: see `in_the_sandbox`. It then has
    // this process's own token, a container's or a restricted one, and can reach no more than this process can: no grant is written and no folder's list
    // changed for it, which that token could not do anyway.)
    let nested = in_the_sandbox();
    // It counts as in use from before it is granted to when all it started has ended.
    let _in_use = (!nested).then(|| crate::workspace_grants::Use::new(&root));
    // SAFETY: every pointer below lives until the call that takes it returns, and the attribute list's buffers until the process is started.
    unsafe {
        let sid = profile_sid()?;
        // (a restricted token's command writes as its workspace's own SID, and is given what it writes and what is guarded as that SID: see `restricted_token`)
        let writer_text = (token == Token::Restricted && !nested).then(|| workspace_sid_text(&root));
        let writer = match &writer_text {
            Some(text) => Some(OwnedSid::from_text(text)?),
            None => None,
        };
        // (a command in the container writes as its workspace's capability, and is given that besides the container: see `workspace_capability`)
        let capability = if token == Token::Container && !nested { Some(workspace_capability(&root)?) } else { None };
        // (what the workspace's grant and its guards are to; the desktop and the rest are the container's, or the restricted token's own SID)
        let grantee = writer.as_ref().map(|writer| writer.0).or(capability).unwrap_or(sid);
        let runner = writer.as_ref().map_or(sid, |writer| writer.0);
        let temp = if nested { std::env::var_os("TEMP").map(PathBuf::from) } else { profile_folder(sid) };
        // (a restricted token's command has its workspace's own, in the sandbox's: see `restricted_temp`)
        let temp = match (&writer_text, temp) {
            (Some(writer), Some(temp)) => Some(restricted_temp(&temp, writer)),
            (_, temp) => temp,
        };
        let mut disarmed = Vec::new();
        let mut times = Timings::default();
        // (as the person, nothing is to be granted: it can do what they can)
        if token != Token::Person && !nested {
            let _one = crate::workspace_grants::before_grant(&root, grantee)?;
            if let Some(writer) = &writer {
                prepare_restricted(&root, writer.0, temp.as_deref(), &mut disarmed, &mut times)?;
            } else {
                prepare(&root, sid, grantee, &mut disarmed, &mut times)?;
                // (a file held out of a folder commands can read, saved anew since by an editor, inherited what the folder gives: it is held out again before this command can read it)
                if let Some(folder) = crate::workspace_grants::data_folder() {
                    crate::read_folders::hold_out_again(&crate::read_folders::file_in(&folder));
                }
                if let Some(tools) = crate::bundled::root() {
                    crate::bundled::open_to_sandbox(&tools)?;
                }
            }
        }
        // (from here to the look after it, a repository that shows up in the workspace may be what it makes)
        let _at_work = AtWork::start(&root);
        // (what it makes, as it makes it: see `programs_to_medium`)
        let watch = if token == Token::Container && !nested { ChangeWatch::start(&root) } else { None };
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
        // (in a container already, the caller's job has the screen's limits, and a job of its own with them cannot be nested in it: error 50)
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS { UIRestrictionsClass: if nested { 0 } else { ui_limits } };
        if (!nested || ui_limits == 0) && SetInformationJobObject(job.0, JobObjectBasicUIRestrictions, std::ptr::addr_of!(ui).cast(), std::mem::size_of_val(&ui) as u32) == 0 {
            return Err(last_error("the limits on what it can do to the screen"));
        }
        let desktop = if nested { None } else { Some(Desktop::new(runner)?) };
        // (what runs in a container of the app's own is given that container and its capabilities; in a container already, it is in that one)
        let contained = token == Token::Container && !nested;

        let mut capability_sids: Vec<PSID> = Vec::new();
        if network && contained {
            for text in NETWORK_CAPABILITIES {
                let mut capability: PSID = std::ptr::null_mut();
                let text = wide(text);
                if ConvertStringSidToSidW(text.as_ptr(), &mut capability) == 0 {
                    return Err(last_error("a network capability"));
                }
                capability_sids.push(capability);
            }
        }
        let mut capabilities: Vec<SID_AND_ATTRIBUTES> = capability_sids.iter().chain(capability.iter().filter(|_| contained)).map(|&capability| SID_AND_ATTRIBUTES { Sid: capability, Attributes: SE_GROUP_ENABLED as u32 }).collect();
        let security = SECURITY_CAPABILITIES { AppContainerSid: sid, Capabilities: if capabilities.is_empty() { std::ptr::null_mut() } else { capabilities.as_mut_ptr() }, CapabilityCount: capabilities.len() as u32, Reserved: 0 };
        let handles = [input.0, out_write.0, err_write.0];
        let jobs = [job.0];

        // (a restricted token, or the person's own, is not a container's: no capabilities to give it)
        // (in a container already, a job given at the start is not supported, error 50: the process starts suspended and is put in it then)
        let count = if contained { 3 } else if nested { 1 } else { 2 };
        let mut size = 0usize;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut size);
        let mut list_memory = vec![0u8; size];
        let list = list_memory.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if InitializeProcThreadAttributeList(list, count, 0, &mut size) == 0 {
            return Err(last_error("the attribute list"));
        }
        let attributes_ok = (!contained || UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize, std::ptr::addr_of!(security).cast(), std::mem::size_of_val(&security), std::ptr::null_mut(), std::ptr::null()) != 0)
            && UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize, handles.as_ptr().cast(), std::mem::size_of_val(&handles), std::ptr::null_mut(), std::ptr::null()) != 0
            && (nested || UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_JOB_LIST as usize, jobs.as_ptr().cast(), std::mem::size_of_val(&jobs), std::ptr::null_mut(), std::ptr::null()) != 0);
        if !attributes_ok {
            DeleteProcThreadAttributeList(list);
            return Err(last_error("the process attributes"));
        }

        let mut startup: STARTUPINFOEXW = std::mem::zeroed();
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        let mut desktop_name = desktop.as_ref().map(|desktop| desktop.name.clone());
        if let Some(name) = &mut desktop_name {
            startup.StartupInfo.lpDesktop = name.as_mut_ptr();
        }
        startup.StartupInfo.hStdInput = input.0;
        startup.StartupInfo.hStdOutput = out_write.0;
        startup.StartupInfo.hStdError = err_write.0;
        startup.lpAttributeList = list;
        let shell = std::env::var("ComSpec").unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".to_string());
        // The code page first, so that a tool's output arrives as UTF-8 on a Japanese Windows.
        let mut command_line = wide(&format!("\"{shell}\" /d /s /c \"chcp 65001 >nul & {line}\""));
        // (the look for the build tools may still be going on: the command's start waits for it no longer than the command has time, and not past Stop)
        let env_began = Instant::now();
        // (with a restricted token nothing stops the network: a command not let reach it is pointed away from it)
        let block = environment(temp.as_deref().filter(|_| token != Token::Person), &root, writer.is_some() && !network, token == Token::Person, || crate::msvc::found_before(asked_at.checked_add(timeout).unwrap_or(asked_at + Duration::from_secs(3600)), cancel));
        times.env = env_began.elapsed();
        let place = wide_path(cwd);
        let mut process: PROCESS_INFORMATION = std::mem::zeroed();
        times.prepare = asked_at.elapsed();
        let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | if nested { CREATE_SUSPENDED } else { 0 };
        let failure = match &writer {
            Some(writer) => match restricted_token(writer.0) {
                Ok(token) => {
                    let started = CreateProcessAsUserW(token.0, std::ptr::null(), command_line.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 1, flags, block.as_ptr().cast(), place.as_ptr(), &startup.StartupInfo, &mut process);
                    (started == 0).then(|| last_error("the command did not start with its restricted token"))
                }
                Err(error) => Some(error),
            },
            None => {
                let started = CreateProcessW(std::ptr::null(), command_line.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 1, flags, block.as_ptr().cast(), place.as_ptr(), &startup.StartupInfo, &mut process);
                (started == 0).then(|| last_error("the command did not start in its AppContainer"))
            }
        };
        DeleteProcThreadAttributeList(list);
        for capability in capability_sids {
            LocalFree(capability as HLOCAL);
        }
        if let Some(failure) = failure {
            return Err(failure);
        }
        let process_handle = Handle(process.hProcess);
        let thread = Handle(process.hThread);
        if nested {
            if AssignProcessToJobObject(job.0, process_handle.0) == 0 {
                let failed = last_error("the Job Object of a command run in the container it is in");
                TerminateProcess(process_handle.0, 1);
                return Err(failed);
            }
            ResumeThread(thread.0);
        }
        drop(thread);
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
                last_seen = desktop.as_ref().map_or_else(Vec::new, |desktop| desktop.windows());
                if !last_seen.iter().any(|(class, _)| class == "#32770") {
                    dialog_since = None;
                } else if dialog_since.get_or_insert(looked).elapsed() > MOST_DIALOG {
                    break "stopped, as it was waiting on a dialog nobody can see";
                }
            }
        };
        times.run = began.elapsed();
        let seen = desktop.as_ref().map_or_else(Vec::new, |desktop| desktop.windows());
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
        let after_began = Instant::now();
        let looked = if nested { Looked { said: Vec::new(), refused: None } } else { look_after_repositories(&root, grantee, Look::AfterACommand) };
        times.after = after_began.elapsed();
        // (what it made that runs: from the container it is low, and so would be what anyone starts of it; a restricted token and the person's own run at the person's level)
        if token == Token::Container && !nested {
            let seen = watch.and_then(ChangeWatch::finish);
            (times.relabel_watched, times.relabel_seen) = (seen.is_some(), seen.as_ref().map_or(0, |seen| seen.changes));
            let relabeled = programs_to_medium(&root, began_at, times.run, seen);
            (times.relabel, times.relabel_walked, times.relabel_changed) = (relabeled.took, relabeled.walked, relabeled.changed);
        }
        TIMINGS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(times);
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

    /// The restricted token's command: it reads as the person does (a folder of the person's that no grant names), writes in its workspace, and cannot write in another
    /// folder of the person's, nor in another workspace's; a command not let reach the network is pointed away from it.
    #[test]
    fn a_restricted_command_reads_as_the_person_and_writes_in_its_workspace_alone() {
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let (work, elsewhere) = (crate::in_container::tempdir().unwrap(), crate::in_container::tempdir().unwrap());
        std::fs::write(elsewhere.path().join("theirs.txt"), "theirs").unwrap();
        let (cancel, time) = (AtomicBool::new(false), Duration::from_secs(60));
        let run = |line: &str, network: bool| run_live_restricted(line, work.path(), work.path(), network, time, &cancel, None).unwrap();
        let read = run(&format!("type \"{}\"", elsewhere.path().join("theirs.txt").display()), false);
        assert_eq!(read.status, Some(0), "{}", String::from_utf8_lossy(&read.stderr));
        assert!(String::from_utf8_lossy(&read.stdout).contains("theirs"));
        let written = run("echo ours> ours.txt", false);
        assert_eq!(written.status, Some(0), "{}", String::from_utf8_lossy(&written.stderr));
        assert!(work.path().join("ours.txt").is_file());
        let outside = run(&format!("echo no> \"{}\"", elsewhere.path().join("planted.txt").display()), false);
        assert_ne!(outside.status, Some(0));
        assert!(!elsewhere.path().join("planted.txt").exists(), "it wrote outside its workspace");
        let offline = run("set CARGO_NET_OFFLINE & set HTTPS_PROXY", false);
        let said = String::from_utf8_lossy(&offline.stdout);
        assert!(said.contains("CARGO_NET_OFFLINE=true") && said.contains("HTTPS_PROXY=http://127.0.0.1:9"), "{said}");
        let online = run("set CARGO_NET_OFFLINE", true);
        assert!(!String::from_utf8_lossy(&online.stdout).contains("CARGO_NET_OFFLINE=true"), "let reach the network, it is not pointed away");
        // another workspace's restricted command cannot write here either
        let other = crate::in_container::tempdir().unwrap();
        let across = run_live_restricted(&format!("echo no> \"{}\"", work.path().join("from-other.txt").display()), other.path(), other.path(), false, time, &cancel, None).unwrap();
        assert_ne!(across.status, Some(0));
        assert!(!work.path().join("from-other.txt").exists(), "one workspace's command wrote in another's");
        // the container's command after them still has its own grant of the workspace (its grant was once taken for the restricted SID's, and skipped)
        let after = run_live("echo in-the-container> after.txt", work.path(), work.path(), false, time, &cancel, None).unwrap();
        assert_eq!(after.status, Some(0), "{}", String::from_utf8_lossy(&after.stderr));
        assert!(work.path().join("after.txt").is_file());
    }

    /// The other half of `the_sandbox_is_told_by_the_token_alone_and_a_run_in_it_starts_as_its_caller`, run by it as a copy of this program: it says whether it
    /// is in the sandbox; with `nested`, it also runs a command in a folder of its own and one that writes in the folder `outside.txt` names, and says what
    /// each wrote and whether a folder's list changed.
    #[test]
    #[ignore = "run by the_sandbox_is_told_by_the_token_alone_and_a_run_in_it_starts_as_its_caller"]
    fn a_sandbox_probe() {
        let mode = std::env::args().last().unwrap_or_default();
        if mode != "ask" && mode != "nested" {
            return;
        }
        println!("PROBE in the sandbox: {}", in_the_sandbox());
        if mode == "nested" {
            let here = std::env::current_dir().unwrap();
            let inner = here.join("inner");
            std::fs::create_dir_all(&inner).unwrap();
            let outside = PathBuf::from(std::fs::read_to_string(here.join("outside.txt")).unwrap().trim());
            let lists = || [&here, &inner, &outside].map(|folder| dacl_text(folder).unwrap());
            let before = lists();
            let (cancel, time) = (AtomicBool::new(false), Duration::from_secs(60));
            let made = run("echo inside> made.txt", &inner, &inner, false, time, &cancel).unwrap();
            let planted = run(&format!("echo no> \"{}\"", outside.join("planted.txt").display()), &inner, &inner, false, time, &cancel).unwrap();
            println!("PROBE wrote inside: {}", made.status == Some(0) && inner.join("made.txt").is_file());
            println!("PROBE wrote outside: {}", planted.status == Some(0) || outside.join("planted.txt").exists());
            println!("PROBE lists as they were: {}", lists() == before);
        }
    }

    /// Whether a process is in the sandbox is its token's alone (`in_the_sandbox`): given the variables a command in the sandbox has, a program is where its
    /// token puts it, and one given the restricted token is in it. A command it runs then starts as it does, with its token (`run_limited`'s `nested`): it
    /// writes in its own folder and not where its caller could not, and no folder's list is changed for it: it can do no more than its caller could.
    #[test]
    fn the_sandbox_is_told_by_the_token_alone_and_a_run_in_it_starts_as_its_caller() {
        let (work, elsewhere) = (crate::in_container::tempdir().unwrap(), crate::in_container::tempdir().unwrap());
        // (in a folder of its own: a folder that holds the program a command is run from is no place for its commands)
        let probe = work.path().join("bin").join("probe.exe");
        std::fs::create_dir_all(probe.parent().unwrap()).unwrap();
        std::fs::copy(std::env::current_exe().unwrap(), &probe).unwrap();
        std::fs::write(work.path().join("outside.txt"), elsewhere.path().display().to_string()).unwrap();
        let args = ["sandbox::tests::a_sandbox_probe", "--exact", "--ignored", "--nocapture", "--test-threads=1"];
        // (the variables alone: a profile's name, temp folders in a container's own, and what a sandbox might be told by)
        let container_temp = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default().join("Packages").join("dogmindapp.exec.probe").join("AC").join("Temp");
        let mut asked = std::process::Command::new(&probe);
        asked.args(args).arg("ask").current_dir(work.path()).env("DOGMIND_SANDBOX_PROFILE", "DogmindApp.Exec.Probe").env("TEMP", &container_temp).env("TMP", &container_temp)
            .env("DOGMIND_IN_SANDBOX", "1").env("CARGO_NET_OFFLINE", "true").env("HTTPS_PROXY", "http://127.0.0.1:9");
        crate::tools::no_window(&mut asked);
        let said = asked.output().unwrap();
        let said = String::from_utf8_lossy(&said.stdout);
        assert!(said.contains(&format!("PROBE in the sandbox: {}", in_the_sandbox())), "{said}");
        // (from a process in the sandbox already, a restricted token is not made: the run below would be its own token's)
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let line = format!("\"{}\" {} nested", probe.display(), args.join(" "));
        let done = run_live_restricted(&line, work.path(), work.path(), false, Duration::from_secs(120), &AtomicBool::new(false), None).unwrap();
        let said = format!("{}{}", decode(&done.stdout), decode(&done.stderr));
        for line in ["PROBE in the sandbox: true", "PROBE wrote inside: true", "PROBE wrote outside: false", "PROBE lists as they were: true"] {
            assert!(said.contains(line), "{line}: {said}");
        }
        assert!(!elsewhere.path().join("planted.txt").exists(), "a command started in the sandbox wrote where its caller could not");
    }

    /// The tests' commands run in a container of their own: nothing a run of them leaves is in the one of the app the person uses on the same PC.
    #[test]
    fn the_tests_commands_run_in_a_container_of_their_own() {
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        // SAFETY: the profile's SID lives as long as the process.
        let folder = unsafe { profile_folder(profile_sid().unwrap()) }.unwrap();
        assert!(folder.to_string_lossy().to_lowercase().contains(r"\dogmindapp.exec.tests\"), "{}", folder.display());
    }

    /// A restricted token's command writes in a temp folder of its workspace's own, in the sandbox's, and its SID is granted that folder alone: nothing of it is on the
    /// sandbox's temp folder, whose list every grant there wrote anew on all in it.
    #[test]
    fn a_restricted_commands_temp_folder_is_its_workspaces_own() {
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let work = crate::in_container::tempdir().unwrap();
        let (cancel, time) = (AtomicBool::new(false), Duration::from_secs(60));
        let done = run_live_restricted("echo kept> \"%TEMP%\\kept.txt\" && echo %TEMP%", work.path(), work.path(), false, time, &cancel, None).unwrap();
        assert_eq!(done.status, Some(0), "{}", String::from_utf8_lossy(&done.stderr));
        let named = PathBuf::from(String::from_utf8_lossy(&done.stdout).trim());
        // SAFETY: the profile's SID lives as long as the process.
        let sandbox_temp = unsafe { profile_folder(profile_sid().unwrap()) }.unwrap();
        let writer = workspace_sid_text(&real(work.path()));
        assert_eq!(key(&named), key(&restricted_temp(&sandbox_temp, &writer)), "its TEMP is its workspace's own");
        assert_eq!(std::fs::read_to_string(named.join("kept.txt")).unwrap().trim(), "kept");
        // SAFETY: read only; the SID lives through the look.
        let granted = unsafe {
            let writer = OwnedSid::from_text(&writer).unwrap();
            Dacl::read(&sandbox_temp).unwrap().aces.iter().any(|ace| Dacl::is_allowed_for(ace, writer.0))
        };
        assert!(!granted, "the sandbox's temp folder is not granted the workspace's SID");
    }

    /// What an earlier version's restricted commands left on the sandbox's temp folder, a grant to each workspace's SID, is taken off it and all in it, in one write;
    /// what else it has stays: the person's own, one of another kind, a capability's.
    #[test]
    fn the_grants_an_earlier_version_left_on_the_sandboxs_temp_folder_are_taken_off() {
        if crate::in_container::cannot_run(crate::in_container::FOLDERS) {
            return;
        }
        let folder = outside_folder("old-restricted-grants");
        std::fs::create_dir_all(folder.join("below")).unwrap();
        std::fs::write(folder.join("below").join("made.txt"), "x").unwrap();
        // SAFETY: what each call is given lives through it.
        unsafe {
            let person = PersonSid::now().unwrap();
            let (old, other, capability) = (OwnedSid::from_text(&random_sid_text()).unwrap(), OwnedSid::from_text(&random_sid_text()).unwrap(), OwnedSid::from_text("S-1-15-3-1024-1-2-3-4-5-6-7-8").unwrap());
            grant(&folder, old.0, READ_WRITE_EXECUTE, true).unwrap();
            grant(&folder, other.0, READ_EXECUTE, true).unwrap();
            grant(&folder, capability.0, READ_WRITE_EXECUTE, true).unwrap();
            grant(&folder, person.sid(), READ_WRITE_EXECUTE, true).unwrap();
            let below = folder.join("below").join("made.txt");
            assert!(Dacl::read(&below).unwrap().aces.iter().any(|ace| Dacl::is_allowed_for(ace, old.0)), "inherited below, before");
            let (count, _) = clear_restricted_grants(&folder, person.sid()).unwrap();
            assert_eq!(count, 1);
            for path in [folder.clone(), below] {
                let list = Dacl::read(&path).unwrap();
                assert!(!list.aces.iter().any(|ace| Dacl::is_allowed_for(ace, old.0)), "{} still has it", path.display());
                for kept in [other.0, capability.0, person.sid()] {
                    assert!(list.aces.iter().any(|ace| Dacl::is_allowed_for(ace, kept)), "{} lost {}", path.display(), sid_text(kept));
                }
            }
            assert_eq!(clear_restricted_grants(&folder, person.sid()).unwrap().0, 0, "nothing the second time");
        }
    }

    #[test]
    fn a_command_gets_a_desktop_in_a_station_of_its_own_and_no_error_boxes() {
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
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
    fn the_station_is_shared_with_browsers_and_is_left_as_it_was_found() {
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        // Its access list and label, as text.
        unsafe fn security_of(station: usize) -> String {
            let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let asked = DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;
            assert_eq!(GetSecurityInfo(station as HANDLE, SE_WINDOW_OBJECT, asked, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), &mut descriptor), 0);
            let mut text: *mut u16 = std::ptr::null_mut();
            assert_ne!(ConvertSecurityDescriptorToStringSecurityDescriptorW(descriptor, SDDL_REVISION_1, asked, &mut text, std::ptr::null_mut()), 0);
            let length = (0..).take_while(|&at| *text.add(at) != 0).count();
            let said = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
            LocalFree(text as HLOCAL);
            LocalFree(descriptor as HLOCAL);
            said
        }
        // SAFETY: the handles are the app's, and the SID lives as long as the process.
        unsafe {
            let station = {
                let mut station = STATION.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                if *station == 0 {
                    *station = open_station().unwrap();
                }
                *station
            };
            let before = security_of(station);
            assert!(before.contains(";;;AC)"), "all application packages may be in it, which is how a container is: {before}");
            let desktop = Desktop::new(profile_sid().unwrap()).unwrap();
            assert_eq!(security_of(station), before, "a browser's sandbox is in this station too, and an ACE for one container on it ends its audio and its protected video");
            drop(desktop);
        }
    }

    #[test]
    fn a_drive_that_cannot_contain_a_command_says_so_first() {
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let folder = Path::new("E:\\work");
        let plain = |file_system: &str, keeps_permissions: bool, remote: bool| Volume { file_system: file_system.into(), keeps_permissions, remote };
        assert_eq!(drive_refusal(&plain("NTFS", true, false), folder), None);
        assert!(drive_refusal(&plain("exFAT", false, false), folder).unwrap().contains("exFAT"));
        assert!(drive_refusal(&plain("FAT32", false, false), folder).unwrap().contains("no file permissions"));
        assert!(drive_refusal(&plain("", true, true), folder).unwrap().contains("network"));
        let dir = crate::in_container::tempdir().unwrap();
        let here = volume_of(&dir.path().join("not").join("yet")).unwrap();
        assert!(!here.remote && here.keeps_permissions && !here.file_system.is_empty(), "{here:?}");
        let rights = rights_on(dir.path()).unwrap();
        assert_eq!(rights, (true, true), "one's own temporary folder");
        assert_eq!(rights_on(&dir.path().join("not")), None);
    }

    #[test]
    fn a_workspace_too_wide_still_has_project_folders_to_run_in() {
        let dir = crate::in_container::tempdir().unwrap();
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
        let dir = crate::in_container::tempdir().unwrap();
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

    /// A label, as SDDL gives it, on a file of the person's (whose list gives them all, WRITE_OWNER too).
    fn label(path: &Path, sddl: &str) {
        // SAFETY: the descriptor is freed here, and the name lives through the call.
        unsafe {
            let (text, name) = (wide(sddl), wide_path(path));
            let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            assert_ne!(ConvertStringSecurityDescriptorToSecurityDescriptorW(text.as_ptr(), 1, &mut descriptor, std::ptr::null_mut()), 0);
            let (mut present, mut sacl, mut defaulted): (BOOL, *mut ACL, BOOL) = (0, std::ptr::null_mut(), 0);
            GetSecurityDescriptorSacl(descriptor, &mut present, &mut sacl, &mut defaulted);
            assert_eq!(SetNamedSecurityInfoW(name.as_ptr() as *mut u16, SE_FILE_OBJECT, LABEL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), sacl), 0, "{}", path.display());
            LocalFree(descriptor as HLOCAL);
        }
    }

    fn low(path: &Path) -> bool {
        // SAFETY: the name lives through the call.
        unsafe { labelled_low(&wide_path(path)).unwrap() }
    }

    /// Written an hour ago, as far as its times say: made before the command it is looked at after.
    fn from_before(path: &Path) {
        use std::os::windows::fs::FileTimesExt;
        let then = std::time::SystemTime::now() - Duration::from_secs(3600);
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(then).set_accessed(then).set_created(then)).unwrap();
    }

    /// After a command, what it made that runs is at the person's level: its programs, and not what was there before it, nor what is not a program, nor what a link in the
    /// workspace leads to outside it. The file's list is as it was.
    #[test]
    fn what_a_command_made_that_runs_is_given_the_persons_level_after_it() {
        if crate::in_container::cannot_run(crate::in_container::FOLDERS) {
            return;
        }
        let (work, outside) = (outside_folder("relabel-work"), outside_folder("relabel-outside"));
        std::fs::create_dir_all(work.join("target").join("debug").join("build")).unwrap();
        let made = [work.join("target").join("debug").join("app.exe"), work.join("target").join("debug").join("lib.DLL"), work.join("target").join("debug").join("build").join("addon.node")];
        let (old, words, away) = (work.join("old.exe"), work.join("notes.txt"), outside.join("away.exe"));
        for path in made.iter().chain([&old, &words, &away]) {
            std::fs::write(path, b"MZ").unwrap();
            label(path, "S:(ML;;NW;;;LW)");
        }
        from_before(&old);
        let linked = std::process::Command::new("cmd").args(["/d", "/c", "mklink", "/J"]).arg(work.join("linked")).arg(&outside).output().unwrap();
        assert!(linked.status.success(), "{}", String::from_utf8_lossy(&linked.stderr));
        // SAFETY: read only.
        let list = |path: &Path| unsafe { Dacl::read(path).unwrap().aces };
        let lists: Vec<_> = made.iter().map(|path| list(path)).collect();
        let began = std::time::SystemTime::now() - Duration::from_secs(60);
        let done = programs_to_medium(&work, began, Duration::from_secs(1), None);
        assert_eq!(done.changed, 3, "{} looked at", done.walked);
        for (path, before) in made.iter().zip(lists) {
            assert!(!low(path), "{}", path.display());
            assert_eq!(list(path), before, "the list of {} is as it was", path.display());
        }
        assert!(low(&old) && low(&words) && low(&away), "what was there before, what is not a program, and what is outside are as they were");
        // (and a second walk changes nothing)
        assert_eq!(programs_to_medium(&work, began, Duration::from_secs(1), None).changed, 0);
        let _ = std::fs::remove_dir(work.join("linked"));
    }

    /// What is made while a watch runs is what it says: a program made, one in a folder made, one renamed to a program's name, one in a folder renamed after it was made
    /// there (where it is now), a folder moved in whole from outside (walked, with what it held), and what is not a program, or is in a repository's own folder, left out.
    /// Those labelled low are then given the person's level, and a walk of the whole after it finds nothing left.
    #[test]
    fn what_a_watch_sees_made_is_given_the_persons_level_and_a_walk_finds_nothing_more() {
        if crate::in_container::cannot_run(crate::in_container::FOLDERS) {
            return;
        }
        let (work, outside) = (outside_folder("watch-work"), outside_folder("watch-outside"));
        std::fs::create_dir_all(work.join(".git")).unwrap();
        let began = std::time::SystemTime::now();
        let watch = ChangeWatch::start(&work).expect("a watch on a folder of the temp drive");
        let program = |path: &Path| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"MZ").unwrap();
            label(path, "S:(ML;;NW;;;LW)");
        };
        program(&work.join("made.exe"));
        program(&work.join("deps").join("made.dll"));
        program(&work.join("named.tmp"));
        std::fs::rename(work.join("named.tmp"), work.join("named.node")).unwrap();
        program(&work.join("working").join("in.exe"));
        std::fs::rename(work.join("working"), work.join("final")).unwrap();
        program(&outside.join("whole").join("deeper").join("came.exe"));
        std::fs::rename(outside.join("whole"), work.join("whole")).unwrap();
        program(&work.join("notes.txt"));
        program(&work.join(".git").join("kept.exe"));
        let seen = watch.finish().expect("all of it seen");
        let mut programs: Vec<String> = seen.programs.iter().map(|path| path.strip_prefix(&work).unwrap().display().to_string()).collect();
        programs.sort();
        assert_eq!(programs, [".git\\kept.exe", "deps\\made.dll", "final\\in.exe", "made.exe", "named.node"]);
        assert_eq!(seen.folders_to_walk(&work), [work.join("whole")], "the folder moved in whole is walked; those made here are not");
        let done = programs_to_medium(&work, began, Duration::from_secs(1), Some(seen));
        assert_eq!(done.changed, 5, "{} looked at", done.walked);
        for path in [work.join("made.exe"), work.join("deps").join("made.dll"), work.join("named.node"), work.join("final").join("in.exe"), work.join("whole").join("deeper").join("came.exe")] {
            assert!(!low(&path), "{}", path.display());
        }
        assert!(low(&work.join("notes.txt")) && low(&work.join(".git").join("kept.exe")));
        assert_eq!(walk_for_programs(&work, began, Duration::from_secs(60), None).changed, 0, "a walk of the whole finds nothing left");
    }

    /// A command in the container that made nothing has nothing walked after it, and what one made is given the person's level, from what was seen of it.
    #[test]
    fn a_command_is_watched_and_what_it_made_that_runs_is_given_the_persons_level() {
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let work = work_folder("watched-command");
        let system = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        let made = command(&format!("mkdir out & copy /y \"{system}\\System32\\whoami.exe\" out\\who.exe >NUL & echo x> words.txt"), &work, false);
        assert_eq!(made.status, Some(0), "{}", decode(&made.stderr));
        assert!(!low(&work.join("out").join("who.exe")), "the program it made is at the person's level");
        assert!(low(&work.join("words.txt")), "what is not a program is as it was made");
        assert_eq!(walk_for_programs(&work, std::time::SystemTime::now() - Duration::from_secs(600), Duration::from_secs(60), None).changed, 0);
    }

    /// A walk out of time goes on after the next command from where it stopped, and then walks the whole again for what that one made.
    #[test]
    fn a_walk_out_of_time_goes_on_after_the_next_command() {
        if crate::in_container::cannot_run(crate::in_container::FOLDERS) {
            return;
        }
        let work = outside_folder("relabel-resume");
        let programs: Vec<PathBuf> = (0..6).map(|at| work.join(format!("d{at}")).join("x.exe")).collect();
        for path in &programs {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"MZ").unwrap();
            label(path, "S:(ML;;NW;;;LW)");
        }
        let began = std::time::SystemTime::now() - Duration::from_secs(60);
        // (no time at all: one folder each time)
        let first = walk_for_programs(&work, began, Duration::ZERO, None);
        assert_eq!(first.changed, 0, "the top folder alone, which holds none");
        assert!(WALKS.lock().unwrap().iter().any(|(at, _)| at == &work), "what is left is kept");
        let mut changed = 0;
        for _ in 0..40 {
            changed += walk_for_programs(&work, began, Duration::ZERO, None).changed;
            if !WALKS.lock().unwrap().iter().any(|(at, _)| at == &work) {
                break;
            }
        }
        assert_eq!(changed, programs.len());
        assert!(programs.iter().all(|path| !low(path)));
    }

    /// A folder no workspace holds, for what a command must not reach. The target folder is not one: it is in the workspace of this source, and once Dogmind has
    /// worked there, its container may change all of that workspace (the grant on it stays, and the target folder inherits it), so that whatever a test put there as
    /// being outside was reached by every command, in a run of the tests outside the sandbox as well.
    fn outside_folder(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join("dogmind-sandbox-tests").join(name);
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::canonicalize(&folder).map_or(folder, |full| PathBuf::from(full.to_string_lossy().trim_start_matches("\\\\?\\")))
    }

    fn command(line: &str, folder: &Path, network: bool) -> Output {
        run(line, folder, folder, network, Duration::from_secs(60), &AtomicBool::new(false)).unwrap()
    }

    /// A workspace's capability is a SID anyone can make from its name, which is the app's and a hash of the folder's key. A command of another workspace that makes it and
    /// claims it, by the attribute CreateProcess takes or by a token NtCreateLowBoxToken makes from its own, does not reach that workspace: if either did, a workspace's
    /// grant could not stay. This program, copied into B and run by B's command, is the one that tries (`capability_probe`), and says what each way came to.
    #[test]
    fn a_command_cannot_claim_another_workspaces_capability() {
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let (a, b) = (work_folder("claimed-a"), work_folder("claiming-b"));
        std::fs::write(a.join("a.txt"), "SECRET-OF-A").unwrap();
        // A's grant is given, and stays; B's own commands cannot reach A.
        assert_eq!(command("echo x> mine.txt", &a, false).status, Some(0));
        let plain = command(&format!("type \"{}\\a.txt\" & echo x> \"{}\\plain.txt\"", a.display(), a.display()), &b, false);
        assert!(!decode(&plain.stdout).contains("SECRET-OF-A") && !a.join("plain.txt").exists(), "B cannot reach A at all, before anything is claimed");
        std::fs::copy(std::env::current_exe().unwrap(), b.join("probe.exe")).unwrap();
        std::fs::write(b.join("probe-args.txt"), format!("{}\n{}\n{}", a.display(), profile_name(), capability_name(&key(&real(&a))))).unwrap();
        let done = command("probe.exe --exact sandbox::tests::capability_probe --ignored --nocapture --test-threads 1", &b, false);
        let said = format!("{}{}", decode(&done.stdout), decode(&done.stderr));
        println!("{said}");
        assert!(said.contains("PROBE-DONE"), "the probe ran to its end: {said}");
        assert!(!said.contains("SECRET-OF-A"), "A's file was read: {said}");
        assert!(!a.join("probe-1.txt").exists() && !a.join("probe-2.txt").exists() && !a.join("probe-3.txt").exists(), "a file was written in A: {said}");
    }

    /// The trying half of the test above: run in B's command by a copy of this program, told by a file there what to try. Nothing when that file is not there.
    #[test]
    #[ignore]
    fn capability_probe() {
        let Ok(args) = std::fs::read_to_string("probe-args.txt") else { return };
        let mut lines = args.lines();
        let (a, profile, name) = (PathBuf::from(lines.next().unwrap()), lines.next().unwrap().to_string(), lines.next().unwrap().to_string());
        // SAFETY: what each call is given lives through it.
        unsafe {
            let mut container: PSID = std::ptr::null_mut();
            assert!(DeriveAppContainerSidFromAppContainerName(wide(&profile).as_ptr(), &mut container) >= 0);
            let (mut groups, mut group_count, mut capabilities, mut capability_count): (*mut PSID, u32, *mut PSID, u32) = (std::ptr::null_mut(), 0, std::ptr::null_mut(), 0);
            assert_ne!(DeriveCapabilitySidsFromName(wide(&name).as_ptr(), &mut groups, &mut group_count, &mut capabilities, &mut capability_count), 0);
            let claimed = *capabilities;
            println!("claiming {} in {}", sid_text(claimed), sid_text(container));
            // 1. A child started with A's capability, by the attribute.
            let mut list_size = 0usize;
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut list_size);
            let mut list_memory = vec![0u8; list_size];
            let list = list_memory.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
            assert_ne!(InitializeProcThreadAttributeList(list, 1, 0, &mut list_size), 0);
            let mut wanted = [SID_AND_ATTRIBUTES { Sid: claimed, Attributes: SE_GROUP_ENABLED as u32 }];
            let security = SECURITY_CAPABILITIES { AppContainerSid: container, Capabilities: wanted.as_mut_ptr(), CapabilityCount: 1, Reserved: 0 };
            assert_ne!(UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize, std::ptr::addr_of!(security).cast(), std::mem::size_of_val(&security), std::ptr::null_mut(), std::ptr::null()), 0);
            let mut startup: STARTUPINFOEXW = std::mem::zeroed();
            startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdOutput = std::os::windows::io::AsRawHandle::as_raw_handle(&std::io::stdout()) as HANDLE;
            startup.StartupInfo.hStdError = startup.StartupInfo.hStdOutput;
            startup.lpAttributeList = list;
            let mut line = wide(&format!("cmd.exe /d /c \"type \"{}\\a.txt\" & echo x> \"{}\\probe-1.txt\"\"", a.display(), a.display()));
            let mut info: PROCESS_INFORMATION = std::mem::zeroed();
            if CreateProcessW(std::ptr::null(), line.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 1, EXTENDED_STARTUPINFO_PRESENT, std::ptr::null(), std::ptr::null(), std::ptr::addr_of!(startup.StartupInfo), &mut info) == 0 {
                println!("way 1, CreateProcess with the attribute: refused, error {}", GetLastError());
            } else {
                WaitForSingleObject(info.hProcess, 20_000);
                let mut code = 0u32;
                GetExitCodeProcess(info.hProcess, &mut code);
                println!("way 1, CreateProcess with the attribute: started, exit {code}");
                CloseHandle(info.hThread);
                CloseHandle(info.hProcess);
            }
            DeleteProcThreadAttributeList(list);
            // 2. A token made from its own, with A's capability.
            #[link(name = "ntdll", kind = "raw-dylib")]
            extern "system" {
                fn NtCreateLowBoxToken(token: *mut HANDLE, existing: HANDLE, access: u32, attributes: *const core::ffi::c_void, package: PSID, capability_count: u32, capabilities: *const SID_AND_ATTRIBUTES, handle_count: u32, handles: *const HANDLE) -> i32;
            }
            let create = NtCreateLowBoxToken;
            let mut own: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_IMPERSONATE | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT, &mut own) == 0 {
                println!("way 2: its own token could not be opened, error {}", GetLastError());
            } else {
                let mut made: HANDLE = std::ptr::null_mut();
                let status = create(&mut made, own, TOKEN_ALL_ACCESS, std::ptr::null(), container, 1, wanted.as_ptr(), 0, std::ptr::null());
                if status < 0 {
                    println!("way 2, NtCreateLowBoxToken: refused, status 0x{:08x}", status as u32);
                } else {
                    // As the token on this thread: A's file read, and one written there.
                    if ImpersonateLoggedOnUser(made) == 0 {
                        println!("way 2, NtCreateLowBoxToken: made, and could not be taken on, error {}", GetLastError());
                    } else {
                        let read = std::fs::read_to_string(a.join("a.txt"));
                        let wrote = std::fs::write(a.join("probe-2.txt"), "x");
                        RevertToSelf();
                        println!("way 2, NtCreateLowBoxToken: made and taken on: read {:?}, write {:?}", read, wrote.map_err(|error| error.to_string()));
                    }
                    // And a child started with it.
                    let mut line = wide(&format!("cmd.exe /d /c \"type \"{}\\a.txt\" & echo x> \"{}\\probe-3.txt\"\"", a.display(), a.display()));
                    let mut plain: STARTUPINFOW = std::mem::zeroed();
                    plain.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
                    plain.dwFlags = STARTF_USESTDHANDLES;
                    plain.hStdOutput = startup.StartupInfo.hStdOutput;
                    plain.hStdError = startup.StartupInfo.hStdOutput;
                    let mut info: PROCESS_INFORMATION = std::mem::zeroed();
                    if CreateProcessAsUserW(made, std::ptr::null(), line.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 1, 0, std::ptr::null(), std::ptr::null(), &plain, &mut info) == 0 {
                        println!("way 2, a child with it: refused, error {}", GetLastError());
                    } else {
                        WaitForSingleObject(info.hProcess, 20_000);
                        let mut code = 0u32;
                        GetExitCodeProcess(info.hProcess, &mut code);
                        println!("way 2, a child with it: started, exit {code}");
                        CloseHandle(info.hThread);
                        CloseHandle(info.hProcess);
                    }
                    CloseHandle(made);
                }
                CloseHandle(own);
            }
        }
        println!("PROBE-DONE");
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
        let dir = crate::in_container::tempdir().unwrap();
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
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let dir = crate::in_container::tempdir().unwrap();
        let home = PathBuf::from(std::env::var_os("USERPROFILE").unwrap());
        let link = dir.path().join("to-home");
        junction(&link, &home);
        assert!(refusal(&link, Some(&home)).is_some());
        std::fs::remove_dir(&link).unwrap();
    }

    /// Each way a workspace is closed has the advice that is true for it: a wide one has project folders to run in, and a folder made in what holds keys or this app's own files is as closed.
    #[test]
    fn the_advice_for_a_closed_workspace_is_the_one_that_is_true_for_it() {
        let dir = crate::in_container::tempdir().unwrap();
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
        let dir = crate::in_container::tempdir().unwrap();
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
        if crate::in_container::cannot_run_with_a_restricted_token(crate::in_container::KEPT_BY_A_CONTAINER) {
            return;
        }
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
        if crate::in_container::cannot_run_with_a_restricted_token(crate::in_container::KEPT_BY_A_CONTAINER) {
            return;
        }
        let folder = work_folder("network");
        let script = "powershell -NoProfile -Command \"try { (New-Object Net.Sockets.TcpClient).Connect('1.1.1.1', 443); 'connected' } catch { 'refused' }\"";
        let closed = command(script, &folder, false);
        assert!(!String::from_utf8_lossy(&closed.stdout).contains("connected"), "{}", String::from_utf8_lossy(&closed.stdout));
    }

    /// Dogmind's records, once open, are read by a command, in a folder of them and beneath it, and are not written; and nothing else of the app's folder is read,
    /// what is held closed (`HELD_CLOSED`) above all.
    #[test]
    fn a_command_reads_the_apps_records_and_nothing_else_of_its_folder() {
        // (in a container, a command the test runs stays in the test's own container, which no grant of the app's opens or closes)
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let (work, app) = (work_folder("records-work"), outside_folder("records-app"));
        let sessions = app.join("sessions");
        std::fs::create_dir_all(sessions.join("a.checkpoints")).unwrap();
        std::fs::write(sessions.join("a.trajectory.jsonl"), "RECORD-ONE\n").unwrap();
        std::fs::write(sessions.join("a.checkpoints").join("1.json"), "RECORD-TWO\n").unwrap();
        std::fs::write(app.join("engine.log"), "RECORD-LOG\n").unwrap();
        let held = |name: &str| if name.contains('.') { app.join(name) } else { app.join(name).join("x.txt") };
        for name in HELD_CLOSED {
            std::fs::create_dir_all(held(name).parent().unwrap()).unwrap();
            std::fs::write(held(name), format!("HELD-{name}\n")).unwrap();
        }
        records_readable(&[sessions.clone(), app.join("engine.log")]);
        let read = |path: &Path| command(&format!("type \"{}\"", path.display()), &work, false);
        for (path, said) in [(sessions.join("a.trajectory.jsonl"), "RECORD-ONE"), (sessions.join("a.checkpoints").join("1.json"), "RECORD-TWO"), (app.join("engine.log"), "RECORD-LOG")] {
            let out = read(&path);
            assert!(decode(&out.stdout).contains(said), "{}: {:?} {}", path.display(), out.status, decode(&out.stderr));
        }
        // (a session written after they were opened is read as well: it inherits what the folder gives)
        std::fs::write(sessions.join("b.jsonl"), "RECORD-NEW\n").unwrap();
        assert!(decode(&read(&sessions.join("b.jsonl")).stdout).contains("RECORD-NEW"));
        for name in HELD_CLOSED {
            let out = read(&held(name));
            assert!(!decode(&out.stdout).contains("HELD-") && out.status != Some(0), "{name} was read: {:?} {}", out.status, decode(&out.stdout));
        }
        let written = command(&format!("echo more>> \"{}\"", sessions.join("a.trajectory.jsonl").display()), &work, false);
        assert_ne!(written.status, Some(0), "{}", decode(&written.stderr));
        assert_eq!(std::fs::read_to_string(sessions.join("a.trajectory.jsonl")).unwrap(), "RECORD-ONE\n", "a record is not written");
        let made = command(&format!("echo x> \"{}\"", sessions.join("c.jsonl").display()), &work, false);
        assert!(made.status != Some(0) && !sessions.join("c.jsonl").exists(), "nothing is made among them");
    }

    #[test]
    fn a_timeout_ends_all_a_command_started() {
        let folder = work_folder("timeout");
        let began = Instant::now();
        let done = run("for /l %i in (0,0,1) do @rem", &folder, &folder, false, Duration::from_secs(1), &AtomicBool::new(false)).unwrap();
        assert_eq!((done.status, done.ended), (None, "timed out"));
        // (a command that is not ended goes on for ever: what is measured is that it ends, and not how soon the sandbox starts one while a thousand other tests run,
        // which went past 10 s in every whole run here)
        assert!(began.elapsed() < Duration::from_secs(30), "{:?}", began.elapsed());
    }

    #[test]
    fn what_the_prompt_offers_for_dir_and_del_works() {
        let folder = work_folder("no-root");
        assert_eq!(command("mkdir sub & echo a> sub\\a.txt & echo a> b.txt", &folder, false).status, Some(0));
        // That these still fail is why the prompt offers the others: they read the drive's root, which a container cannot.
        let refused = command("dir", &folder, false);
        println!("dir: {:?} {:?}", refused.status, decode(&refused.stderr));
        // What runs instead: `dir` and `del` as a model writes them, with what the app puts in their place, and a plain `rmdir /s /q`. (Not
        // PowerShell's own listing: it starts at the drive's root in the sandbox, and lists the workspace only where it may read the folders above it.)
        let listed = command(&crate::tools::line_that_runs("dir /s"), &folder, false);
        let names = decode(&listed.stdout);
        assert!(names.contains("b.txt") && names.contains("a.txt"), "{names} {}", decode(&listed.stderr));
        let gone = command(&crate::tools::line_that_runs("del b.txt"), &folder, false);
        assert_eq!(gone.status, Some(0), "{}", decode(&gone.stderr));
        assert!(!folder.join("b.txt").exists());
        let tree = command("rmdir /s /q sub", &folder, false);
        assert_eq!(tree.status, Some(0), "{}", decode(&tree.stderr));
        assert!(!folder.join("sub").exists());
    }

    /// What the person asked to have done outside the workspace, and allowed: as them it is done, where the container is refused.
    #[test]
    fn a_command_run_as_the_person_changes_what_is_outside_the_workspace_and_one_in_the_container_cannot() {
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let folder = work_folder("as-person");
        let outside = outside_folder("as-person-outside");
        std::fs::write(outside.join("old.txt"), "a").unwrap();
        let line = format!("del /q \"{}\"", outside.join("old.txt").display());
        let time = Duration::from_secs(60);
        let contained = run_live(&line, &folder, &folder, false, time, &AtomicBool::new(false), None).unwrap();
        assert!(outside.join("old.txt").exists(), "the container cannot: {:?} {}", contained.status, decode(&contained.stderr));
        let done = run_live_as_person(&line, &folder, &folder, time, &AtomicBool::new(false), None).unwrap();
        assert_eq!(done.status, Some(0), "{}", decode(&done.stderr));
        assert!(!outside.join("old.txt").exists());
        // (and it starts in the workspace, with the person's own git settings and Python, not the sandbox's)
        let said = run_live_as_person("cd & set XDG_CONFIG_HOME & set PYTHONPATH", &folder, &folder, time, &AtomicBool::new(false), None).unwrap();
        let out = decode(&said.stdout);
        assert!(out.to_lowercase().contains(&folder.display().to_string().to_lowercase()) && !out.contains("XDG_CONFIG_HOME=") && !out.contains("PYTHONPATH="), "{out}");
    }

    /// A `del` of what is not there says so, as `del` does: a name the container does not see (outside the workspace, or misspelt) was a delete with nothing said and exit 0.
    #[test]
    fn a_del_of_what_is_not_there_says_so_and_one_of_what_is_deletes_it() {
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let folder = work_folder("del-missing");
        let outside = outside_folder("del-outside");
        std::fs::write(outside.join("kept.txt"), "a").unwrap();
        assert_eq!(command("echo a> a.log & echo b> b.log & echo c> c.txt", &folder, false).status, Some(0));
        let run = |line: &str| command(&crate::tools::line_that_runs(line), &folder, false);
        let missing = run("del /q nothere.txt");
        assert_eq!(missing.status, Some(1));
        assert!(decode(&missing.stdout).contains("Could Not Find nothere.txt"), "{}", decode(&missing.stdout));
        let none = run("del *.tmp");
        assert_ne!(none.status, Some(0));
        assert!(decode(&none.stdout).contains("Could Not Find *.tmp"), "{}", decode(&none.stdout));
        let elsewhere = format!("{}\\*", outside.display());
        let refused = run(&format!("del /q {elsewhere}"));
        assert_ne!(refused.status, Some(0), "{}", decode(&refused.stderr));
        assert!(decode(&refused.stdout).contains("Could Not Find"), "{}", decode(&refused.stdout));
        assert!(outside.join("kept.txt").exists());
        let logs = run("del /q *.log");
        assert_eq!(logs.status, Some(0), "{}", decode(&logs.stderr));
        assert!(!folder.join("a.log").exists() && !folder.join("b.log").exists() && folder.join("c.txt").exists());
        // (nothing on the error output of a delete that went as it should: PowerShell's word of the drives it could not set up is not the delete's)
        assert_eq!((decode(&logs.stdout), decode(&logs.stderr)), (String::new(), String::new()));
        let one = run("del c.txt");
        assert_eq!(one.status, Some(0), "{}", decode(&one.stderr));
        assert!(!folder.join("c.txt").exists());
        // One that cannot be deleted says why, as `del` says "Access is denied" of a read-only file.
        let kept = folder.join("ro.txt");
        std::fs::write(&kept, "a").unwrap();
        let mut permissions = std::fs::metadata(&kept).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&kept, permissions.clone()).unwrap();
        let denied = run("del ro.txt");
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&kept, permissions).unwrap();
        assert_eq!(denied.status, Some(1));
        assert!(decode(&denied.stdout).contains("ro.txt") && kept.exists(), "{}", decode(&denied.stdout));
    }

    /// In a temp folder, which goes with the test. The one sandbox test written to run on its own: `--skip sandbox::` leaves it out with the others.
    #[test]
    fn a_command_cannot_plant_what_would_run_outside_the_sandbox_and_git_still_works() {
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let holder = crate::in_container::tempdir().unwrap();
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
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let holder = crate::in_container::tempdir().unwrap();
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
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let holder = crate::in_container::tempdir().unwrap();
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
        let holder = crate::in_container::tempdir().unwrap();
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
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let holder = crate::in_container::tempdir().unwrap();
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
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let holder = crate::in_container::tempdir().unwrap();
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
        let holder = crate::in_container::tempdir().unwrap();
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
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let holder = crate::in_container::tempdir().unwrap();
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
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let holder = crate::in_container::tempdir().unwrap();
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
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let holder = crate::in_container::tempdir().unwrap();
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
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let holder = crate::in_container::tempdir().unwrap();
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
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let holder = crate::in_container::tempdir().unwrap();
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
        if crate::in_container::cannot_run(crate::in_container::SANDBOX) {
            return;
        }
        let holder = crate::in_container::tempdir().unwrap();
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
        let holder = crate::in_container::tempdir().unwrap();
        let place = holder.path().join("work");
        std::fs::create_dir_all(&place).unwrap();
        let cap = 300u64 << 20;
        let greedy = "powershell -NoProfile -Command \"try { $a = [Runtime.InteropServices.Marshal]::AllocHGlobal(800000000); 'allocated' } catch { 'refused'; exit 7 }\"";
        let done = run_limited(greedy, &place, &place, false, Duration::from_secs(120), &AtomicBool::new(false), UI_LIMITS, Some(cap), false, None, Token::Container).unwrap();
        eprintln!("status {:?}, out_of_memory {:?}, out {} err {}", done.status, done.out_of_memory, decode(&done.stdout).trim(), decode(&done.stderr));
        assert!(decode(&done.stdout).contains("refused") && done.status == Some(7), "{}", decode(&done.stdout));
        // Bit by bit, as a leak goes: it is said to have been stopped.
        let leak = "powershell -NoProfile -Command \"try { while ($true) { $null = [Runtime.InteropServices.Marshal]::AllocHGlobal(5000000) } } catch { 'refused'; exit 7 }\"";
        let leaked = run_limited(leak, &place, &place, false, Duration::from_secs(120), &AtomicBool::new(false), UI_LIMITS, Some(cap), false, None, Token::Container).unwrap();
        // (What a process does at the end of its memory is its own. PowerShell catches the refusal and ends with 7, or falls over in the middle of it, which it does in about
        // one run in three, and not with one code: 0xC0000005, 0xC00000FD when the stack cannot grow by a page, 0xC0000409, or the runtime's own 0xE0434352 or 0x80131523, as
        // what it has left to run its handler with is not the same twice. Which of them is no business of the sandbox. It is stopped all the same: it ended by itself, and not
        // well, and it is said to have used all it was allowed.)
        assert!(leaked.ended.is_empty() && leaked.status.is_some_and(|status| status != 0) && leaked.out_of_memory == Some(cap), "{:?} {:?} {:?}", leaked.status, leaked.ended, leaked.out_of_memory);
        let fine = run_limited("echo fine", &place, &place, false, Duration::from_secs(60), &AtomicBool::new(false), UI_LIMITS, Some(cap), false, None, Token::Container).unwrap();
        assert!(decode(&fine.stdout).contains("fine") && fine.out_of_memory.is_none());
        drop(holder);
    }

    /// As the person, their own Git is the first on PATH, and the bundled one after it: theirs has their credential helper. A PATH with no Git of theirs stays
    /// as it was, and the bundled folder is never taken for theirs.
    /// A command's PowerShell runs a script with -File: in a container the execution policy's check fails whatever the policy. As the person, their own policy holds.
    #[test]
    fn a_commands_powershell_runs_scripts_and_the_persons_keeps_their_policy() {
        let place = crate::in_container::tempdir().unwrap();
        let block = environment_block(Vec::new(), None, place.path().display().to_string(), place.path(), || None);
        assert!(block.iter().any(|(name, value)| name == POWERSHELL_POLICY.0 && value == "Bypass"), "{block:?}");
        assert!(block.iter().any(|(name, value)| name == NO_ZONE_CHECKS.0 && value == "1"), "no question nobody can answer: {block:?}");
        let theirs = String::from_utf16_lossy(&environment(Some(place.path()), place.path(), false, true, || None)).to_ascii_lowercase();
        assert!(!theirs.contains("psexecutionpolicypreference") && !theirs.contains("see_mask_nozonechecks"), "{theirs}");
    }

    #[test]
    fn as_the_person_their_own_git_comes_first() {
        let holder = crate::in_container::tempdir().unwrap();
        let (bundle, theirs, other) = (holder.path().join("tools"), holder.path().join("Git").join("cmd"), holder.path().join("bin"));
        for folder in [bundle.join("git").join("cmd"), theirs.clone(), other.clone()] {
            std::fs::create_dir_all(&folder).unwrap();
        }
        std::fs::write(bundle.join("git").join("cmd").join("git.exe"), "").unwrap();
        std::fs::write(theirs.join("git.exe"), "").unwrap();
        let path = |folders: &[&Path]| std::env::join_paths(folders).unwrap().to_string_lossy().into_owned();
        let mut block = vec![("PATH".to_string(), path(&[&bundle.join("git").join("cmd"), &other]))];
        their_git_first(&mut block, &path(&[&other, &theirs]), Some(&bundle));
        assert_eq!(block[0].1, path(&[&theirs, &bundle.join("git").join("cmd"), &other]));
        // (no Git of theirs: as it was; and the bundle's own folder on the inherited PATH is not theirs)
        let mut block = vec![("PATH".to_string(), path(&[&bundle.join("git").join("cmd"), &other]))];
        their_git_first(&mut block, &path(&[&other, &bundle.join("git").join("cmd")]), Some(&bundle));
        assert_eq!(block[0].1, path(&[&bundle.join("git").join("cmd"), &other]));
    }

    #[test]
    fn several_processes_share_one_memory_cap() {
        let holder = crate::in_container::tempdir().unwrap();
        let place = holder.path().join("work");
        std::fs::create_dir_all(&place).unwrap();
        // Each alone fits under the cap; four together do not.
        let one = "try { $a = [Runtime.InteropServices.Marshal]::AllocHGlobal(500000000); Write-Output held; Start-Sleep 8 } catch { Write-Output refused }";
        let line = format!("powershell -NoProfile -Command \"$p = 1..4 | ForEach-Object {{ $i = New-Object Diagnostics.ProcessStartInfo 'powershell.exe', '-NoProfile -Command {}'; $i.UseShellExecute = $false; [Diagnostics.Process]::Start($i) }}; $p | ForEach-Object {{ $_.WaitForExit() }}\"", one);
        let done = run_limited(&line, &place, &place, false, Duration::from_secs(120), &AtomicBool::new(false), UI_LIMITS, Some(1200u64 << 20), false, None, Token::Container).unwrap();
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
        let holder = crate::in_container::tempdir().unwrap();
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

    /// Git for Windows can put its `usr\bin` on a person's PATH ahead of System32. Its `find` is MSYS's and cannot start in the container (0xC0000142), so a command that means Windows'
    /// `find` must still get that one. Needs Git for Windows installed in its usual place; without it the test says so and passes.
    #[test]
    #[ignore = "sets PATH for the process: run alone"]
    fn windows_find_runs_with_gits_usr_bin_first_on_the_inherited_path() {
        let usr_bin = Path::new("C:\\Program Files\\Git\\usr\\bin");
        if !usr_bin.join("msys-2.0.dll").is_file() || !usr_bin.join("find.exe").is_file() {
            println!("skipped: {} has no msys-2.0.dll and find.exe on this machine", usr_bin.display());
            return;
        }
        let folder = work_folder("msys-find");
        let saved = std::env::var_os("PATH").unwrap_or_default();
        let first = std::env::join_paths(std::iter::once(usr_bin.to_path_buf()).chain(std::env::split_paths(&saved))).unwrap();
        // SAFETY: this test runs alone, as its reason says.
        unsafe { std::env::set_var("PATH", &first) };
        let done = command("echo a:b| find /c \":\"", &folder, false);
        // SAFETY: as above.
        unsafe { std::env::set_var("PATH", &saved) };
        let said = decode(&done.stdout);
        println!("status {:?}: {}{}", done.status, said.trim(), decode(&done.stderr));
        assert_eq!(done.status, Some(0), "{said} {}", decode(&done.stderr));
        assert_eq!(said.trim(), "1", "{}", decode(&done.stderr));
    }

    /// A Python test's temporary folder is one the command can use and take away, in the sandbox's temp folder and in the workspace alike: a
    /// suite's task failed with Permission denied in both, and left folders in the workspace that nothing could remove (Python 3.14, the
    /// person's own, with no sitecustomize of the bundle's). Where there is no python to run, there is nothing to see.
    #[test]
    fn a_python_temporary_folder_is_one_the_command_can_use_and_remove() {
        let folder = work_folder("python-temp");
        let script = "import os, shutil, tempfile; made = []\nfor where in (None, os.getcwd()):\n    with tempfile.TemporaryDirectory(dir=where) as d:\n        open(os.path.join(d, 'a.txt'), 'w').write('x')\n        made.append(os.path.basename(d))\nd = tempfile.mkdtemp(dir=os.getcwd()); open(os.path.join(d, 'b.txt'), 'w').write('y'); shutil.rmtree(d)\nprint('ok', len(made))";
        std::fs::write(folder.join("probe.py"), script).unwrap();
        let done = command("python probe.py", &folder, false);
        let (said, errors) = (decode(&done.stdout), decode(&done.stderr));
        // (in the tests' own container, a person's Python is not opened to commands: only the bundle's is, where `DOGMIND_TOOLS` names one)
        if errors.contains("is not recognized") || errors.contains("認識されていません") || errors.contains("was not found") {
            eprintln!("no python here: {errors}");
            return;
        }
        assert_eq!((done.status, said.trim()), (Some(0), "ok 2"), "{errors}");
        let left: Vec<String> = std::fs::read_dir(&folder).unwrap().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).filter(|name| name != "probe.py").collect();
        assert!(left.is_empty(), "left in the workspace: {left:?}");
    }

    /// A command's parts are timed: before the process, the process, and after it.
    #[test]
    fn a_command_has_its_own_parts_timed() {
        let folder = work_folder("timings");
        forget_timings();
        let done = run("echo hi", &folder, &folder, false, Duration::from_secs(60), &AtomicBool::new(false)).unwrap();
        assert_eq!(done.status, Some(0));
        // (the list is the process's: a test run beside this one may have put its own commands in it)
        let times = take_timings();
        assert!(!times.is_empty());
        for one in times {
            assert!(one.run > Duration::ZERO && one.prepare > Duration::ZERO, "{one:?}");
            // (the parts of the preparation are within it)
            assert!(one.grant + one.repos + one.toolchains + one.env <= one.prepare + Duration::from_millis(5), "{one:?}");
        }
    }

    /// What the app's own work costs before and after a command, on three workspaces: a few files, a copy of this repository's sources, and a tree of 3,000 folders six deep with a
    /// repository in it. `echo hi` and a Python line, ten each. Prints the median and the worst of each part. `DOGMIND_TOOLS` names the bundled tools (for the Python), `DOGMIND_MEASURE_SOURCES`
    /// the folder whose crates, assets and scripts are copied.
    #[test]
    #[ignore = "a measurement: needs DOGMIND_TOOLS and DOGMIND_MEASURE_SOURCES, and prints"]
    fn what_a_command_costs_before_and_after_on_three_workspaces() {
        let tools = PathBuf::from(std::env::var_os("DOGMIND_TOOLS").expect("DOGMIND_TOOLS"));
        crate::bundled::set_test_root(Some(&tools));
        let sources = PathBuf::from(std::env::var_os("DOGMIND_MEASURE_SOURCES").expect("DOGMIND_MEASURE_SOURCES"));
        let holder = crate::in_container::tempdir().unwrap();
        let (small, copy, deep) = (holder.path().join("small"), holder.path().join("sources"), holder.path().join("deep"));
        std::fs::create_dir_all(&small).unwrap();
        for at in 0..5 {
            std::fs::write(small.join(format!("f{at}.txt")), "x").unwrap();
        }
        fn copy_tree(from: &Path, to: &Path) {
            std::fs::create_dir_all(to).unwrap();
            for item in std::fs::read_dir(from).unwrap().flatten() {
                let name = item.file_name().to_string_lossy().to_string();
                if name.starts_with("target") || name == "dist" || name == ".git" || name == "node_modules" {
                    continue;
                }
                let kind = item.file_type().unwrap();
                if kind.is_dir() {
                    copy_tree(&item.path(), &to.join(&name));
                } else if kind.is_file() {
                    let _ = std::fs::copy(item.path(), to.join(&name));
                }
            }
        }
        for part in ["crates", "assets", "scripts"] {
            if sources.join(part).is_dir() {
                copy_tree(&sources.join(part), &copy.join(part));
            }
        }
        // 3,000 folders, six levels (7 x 7 x 7 x 3 x 3 x 3 is 9,261: this is the first 3,000 of them), a file in each, and a repository
        let mut made = 0;
        'all: for a in 0..7 {
            for b in 0..7 {
                for c in 0..7 {
                    for d in 0..3 {
                        for e in 0..3 {
                            for f in 0..3 {
                                let folder = deep.join(format!("a{a}")).join(format!("b{b}")).join(format!("c{c}")).join(format!("d{d}")).join(format!("e{e}")).join(format!("f{f}"));
                                std::fs::create_dir_all(&folder).unwrap();
                                std::fs::write(folder.join("x.js"), "1").unwrap();
                                made += 1;
                                if made >= 3000 {
                                    break 'all;
                                }
                            }
                        }
                    }
                }
            }
        }
        let nested = deep.join("a0").join("b0").join("nested-repo");
        std::fs::create_dir_all(&nested).unwrap();
        crate::tools::git(&nested, &["init", "-q"]).unwrap();
        std::fs::write(nested.join("a.txt"), "a").unwrap();
        let count = |folder: &Path| walkdir_count(folder);
        fn walkdir_count(folder: &Path) -> usize {
            std::fs::read_dir(folder).map_or(0, |items| items.flatten().map(|item| 1 + if item.file_type().is_ok_and(|kind| kind.is_dir()) { walkdir_count(&item.path()) } else { 0 }).sum())
        }
        println!("MEASURE sizes: small {} items, sources {} items, deep {} items", count(&small), count(&copy), count(&deep));
        let median = |mut values: Vec<f64>| {
            values.sort_by(|a, b| a.partial_cmp(b).unwrap());
            values[values.len() / 2]
        };
        let ms = |time: Duration| time.as_secs_f64() * 1000.0;
        for (name, folder) in [("small", &small), ("sources", &copy), ("deep", &deep)] {
            for line in ["echo hi", "python -c \"print(1)\""] {
                let mut all: Vec<Timings> = Vec::new();
                let mut totals = Vec::new();
                for _ in 0..11 {
                    forget_timings();
                    let began = Instant::now();
                    let done = run(line, folder, folder, false, Duration::from_secs(120), &AtomicBool::new(false)).unwrap();
                    totals.push(ms(began.elapsed()));
                    assert_eq!(done.status, Some(0), "{line}: {}", decode(&done.stderr));
                    all.extend(take_timings());
                }
                // (the first of the eleven grants the folder, and is not the cost of a command in a folder that has it: it is told apart)
                let first = all[0];
                let rest = &all[1..];
                let part = |pick: fn(&Timings) -> Duration| (median(rest.iter().map(|times| ms(pick(times))).collect()), rest.iter().map(|times| ms(pick(times))).fold(0.0, f64::max));
                let (prepare, grant, repos, toolchains, env, run_ms, after) = (part(|t| t.prepare), part(|t| t.grant), part(|t| t.repos), part(|t| t.toolchains), part(|t| t.env), part(|t| t.run), part(|t| t.after));
                println!("MEASURE {name:8} {line:24} first: prepare {:.0} (grant {:.0}, repos {:.0})  |  median/worst of 10: prepare {:.0}/{:.0} (grants {:.0}/{:.0}, repos {:.0}/{:.0}, toolchains {:.0}/{:.0}, env {:.0}/{:.0}) run {:.0}/{:.0} after {:.0}/{:.0}  total median {:.0}",
                    ms(first.prepare), ms(first.grant), ms(first.repos), prepare.0, prepare.1, grant.0, grant.1, repos.0, repos.1, toolchains.0, toolchains.1, env.0, env.1, run_ms.0, run_ms.1, after.0, after.1, median(totals));
            }
        }
        crate::bundled::set_test_root(None);
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
        let dir = crate::in_container::tempdir().unwrap();
        let (root, work) = (dir.path().join("tools"), dir.path().join("work"));
        for (folder, program) in [("python", "python.exe"), ("node", "node.exe"), ("git\\cmd", "git.exe")] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
            std::fs::write(root.join(folder).join(program), "").unwrap();
        }
        let (own_node, other) = (dir.path().join("Program Files\\nodejs"), dir.path().join("other"));
        std::fs::create_dir_all(&own_node).unwrap();
        std::fs::write(own_node.join("node.exe"), "").unwrap();
        let inherited = std::env::join_paths([&own_node, &other]).unwrap().to_string_lossy().into_owned();
        let start = || vec![("PATH".to_string(), inherited.clone()), ("SystemRoot".to_string(), "C:\\Windows".to_string())];
        let temp = dir.path().display().to_string();
        let made = environment_block(start(), Some(&root), temp.clone(), &work, || Some(build_tools()));
        // (and where `npm install -g` puts its commands, in the sandbox's folder, after the bundled tools)
        let global = PathBuf::from(&temp).join("npm-global");
        assert_eq!(path_folders(&made), [root.join("python"), root.join("node"), root.join("git\\cmd"), global.clone(), other.clone(), PathBuf::from("C:\\vs\\bin"), PathBuf::from("C:\\kits\\bin")]);
        // The PATH alone, for a question about a name, is the same one (without the folder of the commands that were installed, which no question about a name of the project's own is about).
        let without_global: Vec<PathBuf> = path_folders(&made).into_iter().filter(|folder| *folder != global).collect();
        assert_eq!(path_of(start(), Some(&root), || Some(build_tools())), without_global);
        assert_eq!(path_of(start(), None, || Some(build_tools())), path_folders(&environment_block(start(), None, temp.clone(), &work, || Some(build_tools()))));
        assert_eq!(path_of(start(), Some(&root), || None), path_folders(&environment_block(start(), Some(&root), temp.clone(), &work, || None)).into_iter().filter(|folder| *folder != global).collect::<Vec<_>>());
        let value = |name: &str| made.iter().find(|(known, _)| known == name).map(|(_, value)| value.as_str());
        assert_eq!(value("LIB"), Some("C:\\vs\\lib\\x64;C:\\kits\\um\\x64"));
        assert_eq!(value("INCLUDE"), Some("C:\\vs\\include;C:\\kits\\ucrt"));
        // Without the bundled tools: the machine's PATH, and then the same.
        let plain = environment_block(start(), None, temp.clone(), &work, || Some(build_tools()));
        assert_eq!(path_folders(&plain), [own_node.clone(), other.clone(), PathBuf::from("C:\\vs\\bin"), PathBuf::from("C:\\kits\\bin")]);
        // On a machine with none found, PATH is what it was.
        let none = environment_block(start(), Some(&root), temp, &work, || None);
        assert_eq!(path_folders(&none), [root.join("python"), root.join("node"), root.join("git\\cmd"), global, other]);
        assert!(none.iter().all(|(name, _)| name != "LIB" && name != "INCLUDE"));
    }

    #[test]
    fn a_folder_of_msys_or_cygwin_programs_is_left_off_a_commands_path_and_only_there() {
        let dir = crate::in_container::tempdir().unwrap();
        let (root, work) = (dir.path().join("tools"), dir.path().join("work"));
        for (folder, program) in [("python", "python.exe"), ("node", "node.exe"), ("git\\cmd", "git.exe")] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
            std::fs::write(root.join(folder).join(program), "").unwrap();
        }
        let folder = |name: &str, files: &[&str]| {
            let folder = dir.path().join(name);
            std::fs::create_dir_all(&folder).unwrap();
            files.iter().for_each(|file| std::fs::write(folder.join(file), "").unwrap());
            folder
        };
        let (unix, cyg, plain) = (folder("unix", &["msys-2.0.dll", "find.exe"]), folder("cyg", &["cygwin1.dll"]), folder("plain", &["find.exe"]));
        let inherited = std::env::join_paths([&unix, &cyg, &plain]).unwrap().to_string_lossy().into_owned();
        let start = || vec![("PATH".to_string(), inherited.clone()), ("SystemRoot".to_string(), "C:\\Windows".to_string())];
        let temp = dir.path().display().to_string();
        let global = PathBuf::from(&temp).join("npm-global");
        let bundled = [root.join("python"), root.join("node"), root.join("git\\cmd")];
        // Without a bundle: the machine's PATH, minus the two.
        assert_eq!(path_folders(&environment_block(start(), None, temp.clone(), &work, || None)), [plain.clone()]);
        assert_eq!(path_of(start(), None, || None), [plain.clone()]);
        // With one: the same, after its own, and `path_of` says what `environment_block` gives.
        let made = path_folders(&environment_block(start(), Some(&root), temp.clone(), &work, || None));
        assert_eq!(made, [bundled[0].clone(), bundled[1].clone(), bundled[2].clone(), global.clone(), plain.clone()]);
        assert_eq!(path_of(start(), Some(&root), || None), made.iter().filter(|folder| **folder != global).cloned().collect::<Vec<_>>());
        // (and Visual Studio's still come last)
        let with_build = path_of(start(), Some(&root), || Some(build_tools()));
        assert_eq!(with_build, [bundled[0].clone(), bundled[1].clone(), bundled[2].clone(), plain.clone(), PathBuf::from("C:\\vs\\bin"), PathBuf::from("C:\\kits\\bin")]);
        // A program the app starts outside the sandbox keeps them: `path_for_programs` is `path_with` of the PATH it has.
        let outside: Vec<PathBuf> = std::env::split_paths(&crate::bundled::path_with(&root, &inherited)).collect();
        assert_eq!(outside, [bundled[0].clone(), bundled[1].clone(), bundled[2].clone(), unix, cyg, plain]);
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
        let dir = crate::in_container::tempdir().unwrap();
        let start = vec![("PATH".to_string(), "C:\\a".to_string()), ("LIB".to_string(), "C:\\dev\\lib".to_string()), ("INCLUDE".to_string(), "C:\\dev\\include".to_string()), ("LIBPATH".to_string(), "C:\\dev\\libpath".to_string())];
        let made = environment_block(start, None, dir.path().display().to_string(), dir.path(), || panic!("an app with a LIB of its own looks for none"));
        let value = |name: &str| made.iter().find(|(known, _)| known == name).map(|(_, value)| value.as_str());
        assert_eq!((value("PATH"), value("LIB"), value("INCLUDE"), value("LIBPATH")), (Some("C:\\a"), Some("C:\\dev\\lib"), Some("C:\\dev\\include"), Some("C:\\dev\\libpath")));
    }

    /// The CLI of the skill that comes with the app runs with no one asked, so what it would report of its use is off in every command, with the bundled tools or without, whatever was inherited.
    #[test]
    fn every_command_has_usage_reports_off() {
        let dir = crate::in_container::tempdir().unwrap();
        let root = dir.path().join("tools");
        for bundled in [None, Some(root.as_path())] {
            let start = vec![("PATH".to_string(), "C:\\a".to_string()), ("disable_telemetry".to_string(), "0".to_string())];
            let made = environment_block(start, bundled, dir.path().display().to_string(), dir.path(), || None);
            let set: Vec<&str> = made.iter().filter(|(name, _)| name.eq_ignore_ascii_case("DISABLE_TELEMETRY")).map(|(_, value)| value.as_str()).collect();
            assert_eq!(set, ["1"], "{bundled:?}");
        }
    }
}
