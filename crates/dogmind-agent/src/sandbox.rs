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

fn refusal_with(workspace: &Path, home: Option<&Path>, own: &[PathBuf]) -> Option<String> {
    let really = real(workspace);
    for folder in [workspace, really.as_path()] {
        let place = key(folder);
        if folder.parent().is_none() || place.ends_with(':') {
            return Some(format!("{} is a whole drive, so a command runs only in a project folder inside it", workspace.display()));
        }
        if crate::safety::keys_folder(folder) {
            return Some(format!("{} is a place keys are kept, so a command does not run in it", workspace.display()));
        }
        // (a folder of the app's own stays closed whatever: what a command could write over in it, or rename away from under it with its
        // parent's leave, runs the next time with none of the sandbox. Writing a Deny on it would not hold against that.)
        if let Some(at) = own.iter().position(|folder| {
            let own = key(&real(folder));
            under(&own, &place) || under(&place, &own)
        }) {
            // (the first is what the app runs from: a copy run from elsewhere leaves this folder free; the rest are what it keeps)
            let way = if at == 0 { ". To work on a folder that holds a build of this app, run a copy of the app from another folder, as the one Dogmind's installer puts in %LOCALAPPDATA%\\Programs\\Dogmind" } else { "" };
            return Some(format!("{} holds this app's own files ({}), or lies among them, and what a command wrote there would run outside its sandbox: a command runs only in a folder of its own{way}", workspace.display(), own[at].display()));
        }
        if let Some(home) = home {
            if under(&key(home), &place) || under(&key(&real(home)), &place) {
                return Some(format!("{} holds your whole user folder, so a command runs only in a project folder inside it", workspace.display()));
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
    let Some(reason) = refusal(workspace, home) else {
        if !under(&key(cwd), &key(workspace)) {
            return Err(format!("{} is outside the workspace: a command runs inside it. `read_file`, `list_files` and `search_files` can look there; running something there takes the user opening it as the workspace", cwd.display()));
        }
        return Ok(workspace.to_path_buf());
    };
    let advice = format!("{reason}. Set cwd to that project's folder, creating it first with create_file if there is none; that folder alone can then be written in");
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
        return Err(format!("{} does not exist yet: create it first with create_file", cwd.display()));
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

/// What a command must not change in a repository: a hook, or a line of config, runs outside the sandbox the next time a person or this app uses git there. Looked at before each
/// command, as the person's own git writes `config` anew, inheriting. Commits, branches and the rest go on as they do. The folder itself cannot be deleted or renamed by the
/// container either, or a command could put another in its place. What a command makes of a repository it has just created is not seen until the next command starts.
pub(crate) unsafe fn guard_repository(root: &Path, git: &Path, sid: PSID) -> Result<(), String> {
    let Ok(what) = std::fs::symlink_metadata(git) else { return Ok(()) };
    // A link is not gone through: an ACE would land on whatever it leads to, the person's own `config` or `hooks` anywhere, and that would be opened to commands.
    let is_link = |path: &Path| std::fs::symlink_metadata(path).is_ok_and(|what| what.file_type().is_symlink());
    if let Some(link) = [git.to_path_buf(), git.join("config"), git.join("hooks")].iter().find(|path| is_link(path)) {
        return Err(format!("{} is a link, so it cannot be kept from what a command would plant there to run outside its sandbox: the command was not run. Replace the link with the folder itself", link.display()));
    }
    // A worktree's or a submodule's: one line, which says where git's folder is. A command that rewrites it has a repository of its own making, hooks and all.
    if !what.is_dir() {
        return limit_to(root, git, sid, &[(READ_EXECUTE, 0)], false);
    }
    // In a repository's own `.git` this has no business: only a worktree's folder under `worktrees` has one. It tells git to read its config and hooks from wherever it says, past all of this.
    // Taken away before each command: the folder has to stay open for git's own new files, so nothing can stop it being made.
    let elsewhere = git.join("commondir");
    if elsewhere.exists() && std::fs::remove_file(&elsewhere).is_err() {
        return Err(format!("{} would send git elsewhere for its config, and could not be taken away: the command was not run", elsewhere.display()));
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

/// What is in the place a `.git` file points to, if that is in the workspace: outside it a command could not have written anything.
fn git_folder_of(git_file: &Path, root: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(git_file).ok()?;
    let named = text.trim().strip_prefix("gitdir:")?.trim();
    let target = crate::safety::resolve(git_file.parent()?, named);
    (crate::safety::inside(root, &target) && target.is_dir()).then_some(target)
}

/// Takes out of a repository a command made what would run as the person the next time git is used there: its hooks, and what its config names. Guarding it first would keep later commands from
/// doing the same and leave what this one already did. `known`: repositories that were there before it, which are the person's and are not touched, whatever a worktree says of them.
fn disarm_repository(git: &Path, root: &Path, known: &HashSet<String>) -> Vec<String> {
    let mut said = Vec::new();
    let disarm_folder = |folder: &Path, said: &mut Vec<String>| {
        let hooks = folder.join("hooks");
        if std::fs::symlink_metadata(&hooks).is_ok_and(|what| what.file_type().is_symlink()) {
            if std::fs::remove_dir(&hooks).is_ok() || std::fs::remove_file(&hooks).is_ok() {
                said.push("a link in place of its hooks".to_string());
            }
        } else {
            for entry in std::fs::read_dir(&hooks).into_iter().flatten().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with(".sample") {
                    continue;
                }
                let gone = if entry.file_type().is_ok_and(|kind| kind.is_dir()) { std::fs::remove_dir_all(entry.path()) } else { std::fs::remove_file(entry.path()) };
                if gone.is_ok() {
                    said.push(format!("hooks/{name}"));
                }
            }
        }
        let config = folder.join("config");
        // (git does not ask that it be UTF-8)
        if let Ok(bytes) = std::fs::read(&config) {
            let (kept, gone) = config_without_programs(&String::from_utf8_lossy(&bytes));
            if !gone.is_empty() && std::fs::write(&config, kept).is_ok() {
                said.extend(gone);
            }
        }
    };
    if git.is_dir() {
        disarm_folder(git, &mut said);
        return said;
    }
    // A worktree's or a submodule's: the folder it points to, and for a worktree, the repository that says it is part of.
    let Some(folder) = git_folder_of(git, root) else { return said };
    disarm_folder(&folder, &mut said);
    if let Some(common) = std::fs::read_to_string(folder.join("commondir")).ok().map(|text| crate::safety::resolve(&folder, text.trim())) {
        if crate::safety::inside(root, &common) && common.is_dir() && !known.contains(&key(&common)) {
            disarm_folder(&common, &mut said);
        }
    }
    said
}

/// Before a command, and after it for what that one made: every repository in reach, each new one disarmed and all of them guarded. Says what was taken out.
unsafe fn look_after_repositories(root: &Path, sid: PSID) -> Result<Vec<String>, String> {
    let found = repositories_under(root);
    let new = newly_made(root, &found);
    let mut said = Vec::new();
    if !new.is_empty() {
        let known: HashSet<String> = found.iter().filter(|git| !new.contains(git)).map(|git| key(git)).collect();
        for git in &new {
            let taken = disarm_repository(git, root, &known);
            if !taken.is_empty() {
                let shown = git.parent().and_then(|folder| folder.strip_prefix(root).ok()).map_or(String::new(), |place| place.display().to_string());
                said.push(format!("{} in {}", taken.join(", "), if shown.is_empty() { "." } else { &shown }));
            }
        }
    }
    for git in &found {
        guard_repository(root, git, sid)?;
        // (a submodule's folder is in the outer `.git`, which is open to git's own new files; a worktree's has neither of these)
        if git.is_file() {
            if let Some(folder) = git_folder_of(git, root).filter(|folder| !folder.join("commondir").exists()) {
                let everything = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;
                limit_to(root, &folder.join("config"), sid, &[(READ_EXECUTE, 0)], false)?;
                limit_to(root, &folder.join("hooks"), sid, &[(READ_EXECUTE, everything)], true)?;
            }
        }
    }
    Ok(said)
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
    *disarmed = look_after_repositories(workspace, sid)?;
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

fn environment(temp: Option<&Path>, workspace: &Path) -> Vec<u16> {
    let block: Vec<(String, String)> = VARIABLES.iter().filter_map(|name| std::env::var(name).ok().map(|value| (name.to_string(), value))).collect();
    let temp = temp.map(|path| path.display().to_string()).or_else(|| std::env::var("TEMP").ok()).unwrap_or_default();
    let block = environment_block(block, crate::bundled::root().as_deref(), temp, workspace, crate::msvc::found);
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
    run_limited(line, cwd, workspace, network, timeout, cancel, UI_LIMITS, Some(job_memory_cap()), live)
}

/// A browser under those limits falls over in most runs, whichever of them is set (0xC0000005, 12 to 19 runs in 20, against none in 20 with none set). Its own window station
/// keeps it from the person's clipboard and session in any case.
pub fn run_browser(line: &str, cwd: &Path, workspace: &Path, timeout: Duration, cancel: &AtomicBool) -> Result<Output, String> {
    run_limited(line, cwd, workspace, false, timeout, cancel, 0, None, None)
}

#[allow(clippy::too_many_arguments)]
fn run_limited(line: &str, cwd: &Path, workspace: &Path, network: bool, timeout: Duration, cancel: &AtomicBool, ui_limits: u32, memory_cap: Option<u64>, live: Option<Arc<crate::tools::Live>>) -> Result<Output, String> {
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
        let (out_read, out_write) = pipe()?;
        let (err_read, err_write) = pipe()?;
        let nul = wide("NUL");
        let inheritable = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: std::ptr::null_mut(), bInheritHandle: 1 };
        let input = Handle(CreateFileW(nul.as_ptr(), FILE_GENERIC_READ, FILE_SHARE_READ, &inheritable, OPEN_EXISTING, 0, std::ptr::null_mut()));

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
        let block = environment(profile_folder(sid).as_deref(), &root);
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
        // What it made, with nothing of its own left running to go on making it. (Not being able to guard this does not take back what has already run: the next command is what is refused.)
        if let Ok(taken) = look_after_repositories(&root, sid) {
            disarmed.extend(taken);
        }
        Ok(Output { stdout: stdout.join().unwrap_or_default(), stderr: stderr.join().unwrap_or_default(), status, ended, shown, disarmed, out_of_memory })
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
        assert!(refused.contains("Set cwd"), "{refused}");
        for place in [".ssh", ".aws", "AppData\\Local", "Documents", "Desktop", "Downloads\\x"] {
            std::fs::create_dir_all(home.join(place)).unwrap();
            assert!(writable_root(&home.join(place), &home, Some(&home)).is_err(), "{place}");
        }
        assert!(writable_root(&home.join("not-made-yet"), &home, Some(&home)).unwrap_err().contains("does not exist"));
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

    #[test]
    fn what_holds_this_app_or_keys_is_no_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let (home, tools, data) = (dir.path().join("me"), dir.path().join("Tools"), dir.path().join("Tools\\Dogmind"));
        std::fs::create_dir_all(&data).unwrap();
        let own = [data.clone()];
        assert!(refusal_with(&tools, Some(&home), &own).unwrap().contains("this app's own"), "it holds it");
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
        let done = run_limited(greedy, &place, &place, false, Duration::from_secs(120), &AtomicBool::new(false), UI_LIMITS, Some(cap), None).unwrap();
        eprintln!("status {:?}, out_of_memory {:?}, out {} err {}", done.status, done.out_of_memory, decode(&done.stdout).trim(), decode(&done.stderr));
        assert!(decode(&done.stdout).contains("refused") && done.status == Some(7), "{}", decode(&done.stdout));
        // Bit by bit, as a leak goes: it is said to have been stopped.
        let leak = "powershell -NoProfile -Command \"try { while ($true) { $null = [Runtime.InteropServices.Marshal]::AllocHGlobal(5000000) } } catch { 'refused'; exit 7 }\"";
        let leaked = run_limited(leak, &place, &place, false, Duration::from_secs(120), &AtomicBool::new(false), UI_LIMITS, Some(cap), None).unwrap();
        // (PowerShell catches the refusal and ends with 7, or now and then falls over in the middle of it with 0xC0000005: stopped all the
        // same, and said to have been)
        assert!(matches!(leaked.status, Some(7) | Some(-1073741819)) && leaked.out_of_memory == Some(cap), "{:?} {:?}", leaked.status, leaked.out_of_memory);
        let fine = run_limited("echo fine", &place, &place, false, Duration::from_secs(60), &AtomicBool::new(false), UI_LIMITS, Some(cap), None).unwrap();
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
        let done = run_limited(&line, &place, &place, false, Duration::from_secs(120), &AtomicBool::new(false), UI_LIMITS, Some(1200u64 << 20), None).unwrap();
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
