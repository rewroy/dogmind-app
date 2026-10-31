//! Where `execute_command` runs on Windows: its own AppContainer. The workspace is writable, the per-user toolchains on PATH
//! are readable, and nothing else outside what every app may read; no network unless this command was approved for it. A Job
//! Object ends all a command started. There is no fallback to an unrestricted child: what cannot be contained does not run.
//!
//! Grants are ACEs for this profile's SID, as AppContainers have no other way: they are written on the folders and persist.

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::Authorization::*;
use windows_sys::Win32::Security::Isolation::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, SE_GROUP_ENABLED};
use windows_sys::Win32::System::Threading::*;

const PROFILE: &str = "DogmindApp.Exec";
const READ_EXECUTE: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
const READ_WRITE_EXECUTE: u32 = READ_EXECUTE | FILE_GENERIC_WRITE | DELETE;
/// `internetClient` and `privateNetworkClientServer`, for a command approved for the network.
const NETWORK_CAPABILITIES: [&str; 2] = ["S-1-15-3-1", "S-1-15-3-3"];
/// What a command inherits, as the agent's own environment may hold keys. TEMP and TMP are the profile's.
const VARIABLES: [&str; 24] = [
    "SystemRoot", "windir", "SystemDrive", "ComSpec", "OS", "PATH", "PATHEXT", "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE", "ProgramData",
    "ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "CommonProgramFiles", "USERPROFILE", "USERNAME", "HOMEDRIVE", "HOMEPATH", "APPDATA", "LOCALAPPDATA",
    "LANG", "CARGO_HOME", "RUSTUP_HOME", "PUBLIC",
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

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn last_error(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

fn key(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// Whether `inner` is `outer` or lies under it.
fn under(inner: &str, outer: &str) -> bool {
    inner == outer || inner.starts_with(&format!("{outer}\\"))
}

/// Why this folder cannot be the writable one: a command could then write over everything the person has.
pub fn refusal(workspace: &Path, home: Option<&Path>) -> Option<String> {
    let place = key(workspace);
    if workspace.parent().is_none() || place.ends_with(':') {
        return Some(format!("{} is a whole drive, so a command runs only in a project folder inside it", workspace.display()));
    }
    let home = home.map(key)?;
    under(&home, &place).then(|| format!("{} holds your whole user folder, so a command runs only in a project folder inside it", workspace.display()))
}

/// The folder a command may write in. Normally the workspace; a workspace too wide to give away still has project folders in
/// it, and one of those is given when the command says it runs there, and nothing more. What a person keeps in the usual
/// places stays closed, and whatever the folder, the command has been through the approvals like any other.
pub fn writable_root(cwd: &Path, workspace: &Path, home: Option<&Path>) -> Result<PathBuf, String> {
    let Some(reason) = refusal(workspace, home) else {
        if !under(&key(cwd), &key(workspace)) {
            return Err(format!("{} is outside the workspace: a command runs inside it", cwd.display()));
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
    if first.starts_with('.') || NOT_A_PROJECT.contains(&first) {
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
    let home_key = key(home);
    let mut folders: Vec<PathBuf> = std::env::split_paths(path_variable)
        .filter(|folder| {
            let entry = key(folder);
            entry != home_key && under(&entry, &home_key) && !entry.contains("\\.ssh")
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
            .filter(|folder| !open_to_container(folder, sid))
            .collect()
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

unsafe fn profile_sid() -> Result<PSID, String> {
    let mut slot = PROFILE_SID.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(sid) = &*slot {
        return Ok(sid.0);
    }
    let (name, display, description) = (wide(PROFILE), wide("Dogmind App commands"), wide("Where Dogmind App runs the commands you approve"));
    let mut sid: PSID = std::ptr::null_mut();
    let created = CreateAppContainerProfile(name.as_ptr(), display.as_ptr(), description.as_ptr(), std::ptr::null(), 0, &mut sid);
    if created < 0 && DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) < 0 {
        return Err(format!("the AppContainer profile could not be made (0x{created:08x})"));
    }
    *slot = Some(Sid(sid));
    Ok(sid)
}

unsafe fn has_grant(path: &[u16], sid: PSID, mask: u32, inherit: bool) -> bool {
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

/// One ACE for the profile, once: propagating it through a tree is what costs, and it persists.
unsafe fn grant(path: &Path, sid: PSID, mask: u32, inherit: bool) -> Result<(), String> {
    let memo = (key(path), mask, inherit);
    {
        let mut granted = GRANTED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if granted.get_or_insert_with(HashSet::new).contains(&memo) {
            return Ok(());
        }
    }
    let name = wide_path(path);
    if !has_grant(&name, sid, mask, inherit) {
        let (mut old, mut descriptor): (*mut ACL, PSECURITY_DESCRIPTOR) = (std::ptr::null_mut(), std::ptr::null_mut());
        let read = GetNamedSecurityInfoW(name.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), &mut old, std::ptr::null_mut(), &mut descriptor);
        if read != 0 {
            return Err(format!("{}: its access list cannot be read (error {read})", path.display()));
        }
        let mut access: EXPLICIT_ACCESS_W = std::mem::zeroed();
        access.grfAccessPermissions = mask;
        access.grfAccessMode = GRANT_ACCESS;
        access.grfInheritance = if inherit { SUB_CONTAINERS_AND_OBJECTS_INHERIT } else { NO_INHERITANCE };
        access.Trustee.TrusteeForm = TRUSTEE_IS_SID;
        access.Trustee.TrusteeType = TRUSTEE_IS_WELL_KNOWN_GROUP;
        access.Trustee.ptstrName = sid as *mut u16;
        let mut new: *mut ACL = std::ptr::null_mut();
        let merged = SetEntriesInAclW(1, &access, old, &mut new);
        let written = if merged == 0 {
            SetNamedSecurityInfoW(name.as_ptr() as *mut u16, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), new, std::ptr::null())
        } else {
            merged
        };
        if !new.is_null() {
            LocalFree(new as HLOCAL);
        }
        LocalFree(descriptor as HLOCAL);
        if written != 0 {
            return Err(format!("{}: access could not be granted (error {written})", path.display()));
        }
    }
    GRANTED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get_or_insert_with(HashSet::new).insert(memo);
    Ok(())
}

unsafe fn prepare(workspace: &Path, sid: PSID) -> Result<(), String> {
    grant(workspace, sid, READ_WRITE_EXECUTE, true)?;
    // The folders above: their attributes only, and only for shells that look at where they are. An OS-owned one may refuse, which costs little.
    for ancestor in workspace.ancestors().skip(1).filter(|folder| folder.parent().is_some()) {
        let _ = grant(ancestor, sid, FILE_GENERIC_EXECUTE, false);
    }
    if let (Some(path), Some(home)) = (std::env::var_os("PATH"), std::env::var_os("USERPROFILE")) {
        for folder in toolchain_folders(&path.to_string_lossy(), Path::new(&home)) {
            if folder.exists() {
                let _ = grant(&folder, sid, READ_EXECUTE, true);
            }
        }
    }
    Ok(())
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

fn environment(temp: Option<&Path>) -> Vec<u16> {
    let mut block: Vec<(String, String)> = VARIABLES.iter().filter_map(|name| std::env::var(name).ok().map(|value| (name.to_string(), value))).collect();
    let temp = temp.map(|path| path.display().to_string()).or_else(|| std::env::var("TEMP").ok()).unwrap_or_default();
    // Where git looks for its per-user files: the person's profile is closed to a command, and it would warn on every run.
    block.push(("XDG_CONFIG_HOME".to_string(), temp.clone()));
    block.push(("TEMP".to_string(), temp.clone()));
    block.push(("TMP".to_string(), temp));
    block.sort_by_key(|(name, _)| name.to_uppercase());
    let mut out = Vec::new();
    for (name, value) in block {
        out.extend(format!("{name}={value}").encode_utf16());
        out.push(0);
    }
    out.push(0);
    out
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
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let root = writable_root(cwd, workspace, home.as_deref())?;
    // SAFETY: every pointer below lives until the call that takes it returns, and the attribute list's buffers until the process is started.
    unsafe {
        let sid = profile_sid()?;
        prepare(&root, sid)?;
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
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(job.0, JobObjectExtendedLimitInformation, std::ptr::addr_of!(limits).cast(), std::mem::size_of_val(&limits) as u32);

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
        startup.StartupInfo.hStdInput = input.0;
        startup.StartupInfo.hStdOutput = out_write.0;
        startup.StartupInfo.hStdError = err_write.0;
        startup.lpAttributeList = list;
        let shell = std::env::var("ComSpec").unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".to_string());
        // The code page first, so that a tool's output arrives as UTF-8 on a Japanese Windows.
        let mut command_line = wide(&format!("\"{shell}\" /d /s /c \"chcp 65001 >nul & {line}\""));
        let block = environment(profile_folder(sid).as_deref());
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
            std::thread::spawn(move || {
                // SAFETY: ownership of the read end came here.
                let mut file = File::from_raw_handle(raw as _);
                let mut bytes = Vec::new();
                let _ = file.read_to_end(&mut bytes);
                bytes
            })
        };
        let (stdout, stderr) = (capture(out_read), capture(err_read));

        let began = Instant::now();
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
        };
        let mut code = 0u32;
        let status = if ended.is_empty() && GetExitCodeProcess(process_handle.0, &mut code) != 0 { Some(code as i32) } else { None };
        // Whatever it left running goes with it, and that closes the pipes.
        TerminateJobObject(job.0, 1);
        drop(job);
        Ok(Output { stdout: stdout.join().unwrap_or_default(), stderr: stderr.join().unwrap_or_default(), status, ended })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_folder_outside_the_workspace_does_not_run() {
        let folder = work_folder("cwd");
        assert!(run("echo x", Path::new("C:\\Windows"), &folder, false, Duration::from_secs(5), &AtomicBool::new(false)).is_err());
    }
}
