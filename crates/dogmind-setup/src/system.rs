//! What the installer asks of Windows beside the files: who is running from the folder, asking the app to close, the Start menu and Add or remove programs. It holds no call that ends
//! another program: a copy that does not close stays open and the person is told.

use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{CloseHandle, HWND, INVALID_HANDLE_VALUE, LPARAM};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};
use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetClassNameW, GetWindowThreadProcessId, PostMessageW, RegisterWindowMessageW};

use crate::install::Shell;

pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// The folder a person's programs go in without an administrator.
pub fn default_folder() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty()).map(|local| PathBuf::from(local).join("Programs").join("Dogmind"))
}

pub fn windows_build() -> Option<u32> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
    // SAFETY: ntdll is in every process, the function has this signature, and the structure says its own size.
    unsafe {
        let library = GetModuleHandleW(wide("ntdll.dll").as_ptr());
        let found = GetProcAddress(library, c"RtlGetVersion".as_ptr().cast())?;
        let get: unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32 = std::mem::transmute(found);
        let mut version: OSVERSIONINFOW = std::mem::zeroed();
        version.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
        (get(&mut version) == 0).then_some(version.dwBuildNumber)
    }
}

/// The free bytes of the drive this folder is on or would be on.
pub fn free_bytes(path: &Path) -> Option<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let existing = path.ancestors().find(|folder| folder.is_dir())?;
    let mut free = 0u64;
    // SAFETY: the name ends in a zero, and the out-parameter lives through the call.
    (unsafe { GetDiskFreeSpaceExW(wide_path(existing).as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) } != 0).then_some(free)
}

/// A running program: what it is, and what can be said of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    pub pid: u32,
    pub name: String,
}

fn all_processes() -> Vec<(u32, PathBuf)> {
    let mut found = Vec::new();
    // SAFETY: the snapshot is this function's, and each buffer is as long as the call is told.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return found;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID);
            if !process.is_null() {
                let mut name = vec![0u16; 32768];
                let mut length = name.len() as u32;
                if QueryFullProcessImageNameW(process, 0, name.as_mut_ptr(), &mut length) != 0 {
                    found.push((entry.th32ProcessID, PathBuf::from(String::from_utf16_lossy(&name[..length as usize]))));
                }
                CloseHandle(process);
            }
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    found
}

/// Whether this program file lies in this folder.
fn is_in(file: &Path, folder: &Path) -> bool {
    let (file, folder) = (file.to_string_lossy().to_lowercase(), folder.to_string_lossy().trim_end_matches('\\').to_lowercase());
    file.starts_with(&format!("{folder}\\"))
}

/// What is running from this folder, but this program. `any_copy`: a copy of the app from another folder too, which holds the same settings. Not in a test, which must leave the person's own alone.
pub fn running_from(folder: &Path, any_copy: bool) -> Vec<Running> {
    let own = std::process::id();
    let windows = if any_copy { app_windows() } else { Vec::new() };
    let mut found: Vec<Running> = all_processes()
        .into_iter()
        .filter(|(pid, file)| *pid != own && (is_in(file, folder) || windows.iter().any(|(owner, _)| owner == pid)))
        .map(|(pid, file)| Running { pid, name: file.file_name().map_or(String::new(), |name| name.to_string_lossy().into_owned()) })
        .collect();
    found.sort_by_key(|running| running.pid);
    found
}

/// The windows and their owners that an enumeration gives, of this kind.
fn windows_where(keep: &dyn Fn(HWND, u32) -> bool) -> Vec<(u32, HWND)> {
    struct Walk<'a> {
        keep: &'a dyn Fn(HWND, u32) -> bool,
        found: Vec<(u32, HWND)>,
    }
    unsafe extern "system" fn visit(window: HWND, data: LPARAM) -> i32 {
        let walk = &mut *(data as *mut Walk);
        let mut owner = 0;
        GetWindowThreadProcessId(window, &mut owner);
        if (walk.keep)(window, owner) {
            walk.found.push((owner, window));
        }
        1
    }
    let mut walk = Walk { keep, found: Vec::new() };
    // SAFETY: `walk` lives through the call, which calls back on this thread.
    unsafe { EnumWindows(Some(visit), std::ptr::addr_of_mut!(walk) as LPARAM) };
    walk.found
}

fn app_windows() -> Vec<(u32, HWND)> {
    windows_where(&|window, _| {
        let mut name = [0u16; 32];
        // SAFETY: the buffer is as long as the call is told.
        let length = unsafe { GetClassNameW(window, name.as_mut_ptr(), name.len() as i32) };
        String::from_utf16_lossy(&name[..length.max(0) as usize]) == "DogmindApp"
    })
}

/// Asks these to close, by a message only the app knows and answers: it closes as its tray's Quit does, unless it is working, and then says so itself. Nothing else hears it, and nothing is ended.
pub fn ask_to_close(pids: &[u32]) {
    // SAFETY: the name ends in a zero, and a message to a window that has gone is simply lost.
    unsafe {
        let message = RegisterWindowMessageW(wide("DogmindApp.PleaseQuit").as_ptr());
        for (_, window) in windows_where(&|_, owner| pids.contains(&owner)) {
            PostMessageW(window, message, 0, 0);
        }
    }
}

/// Waits for these processes to have ended, for as long as `milliseconds` for each.
pub fn wait_for_end(pids: &[u32], milliseconds: u32) {
    for pid in pids {
        // SAFETY: the handle is this loop's.
        unsafe {
            let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, *pid);
            if !process.is_null() {
                WaitForSingleObject(process, milliseconds);
                CloseHandle(process);
            }
        }
    }
}

/// Starts a program of the person's. Under the test switch it stays off their screen, whatever it is: a window is born far off it and not activated, and a console program gets no console.
pub fn start_program(program: &Path, arguments: &str, test: bool) -> std::io::Result<()> {
    use windows_sys::Win32::System::Threading::{CreateProcessW, CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTF_USEPOSITION, STARTF_USESHOWWINDOW, STARTUPINFOW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE;
    if !test {
        let mut command = std::process::Command::new(program);
        if !arguments.is_empty() {
            command.arg(arguments);
        }
        return command.spawn().map(|_| ());
    }
    let mut line = wide(&format!("\"{}\" {arguments}", program.display()));
    // SAFETY: the structures are as long as they say, and the command line can be written in, as the call wants.
    unsafe {
        let mut start: STARTUPINFOW = std::mem::zeroed();
        start.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        start.dwFlags = STARTF_USEPOSITION | STARTF_USESHOWWINDOW;
        start.dwX = (-20000i32) as u32;
        start.wShowWindow = SW_SHOWNOACTIVATE as u16;
        let mut started: PROCESS_INFORMATION = std::mem::zeroed();
        if CreateProcessW(wide_path(program).as_ptr(), line.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 0, CREATE_NO_WINDOW, std::ptr::null(), std::ptr::null(), &start, &mut started) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        CloseHandle(started.hThread);
        CloseHandle(started.hProcess);
    }
    Ok(())
}

/// What Add or remove programs shows, and what it runs.
pub fn uninstall_values(folder: &Path, version: &str, kilobytes: u64) -> (Vec<(&'static str, String)>, Vec<(&'static str, u32)>) {
    let shown = folder.display();
    (
        vec![
            ("DisplayName", "Dogmind App".to_string()),
            ("DisplayVersion", version.to_string()),
            ("Publisher", "rewroy".to_string()),
            ("InstallLocation", shown.to_string()),
            ("DisplayIcon", format!("{shown}\\dogmind-app.exe,0")),
            ("UninstallString", format!("\"{shown}\\{}\" --uninstall", crate::install::UNINSTALLER)),
        ],
        vec![("NoModify", 1), ("NoRepair", 1), ("EstimatedSize", u32::try_from(kilobytes).unwrap_or(u32::MAX))],
    )
}

const UNINSTALL_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\DogmindApp";
const SHORTCUT: &str = "Dogmind App.lnk";

/// A shortcut to a program in its own folder.
pub fn write_shortcut(place: &Path, target: &Path, description: &str) -> Result<(), String> {
    use windows::core::{Interface, HSTRING, PCWSTR};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    // SAFETY: COM on this thread; the strings live through each call.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(|error| format!("a shortcut: {error}"))?;
        let (file, folder, words) = (HSTRING::from(target.as_os_str()), HSTRING::from(target.parent().unwrap_or(target).as_os_str()), HSTRING::from(description));
        link.SetPath(PCWSTR(file.as_ptr())).map_err(|error| error.to_string())?;
        link.SetWorkingDirectory(PCWSTR(folder.as_ptr())).map_err(|error| error.to_string())?;
        link.SetDescription(PCWSTR(words.as_ptr())).map_err(|error| error.to_string())?;
        let persist: IPersistFile = link.cast().map_err(|error| error.to_string())?;
        persist.Save(PCWSTR(HSTRING::from(place.as_os_str()).as_ptr()), true).map_err(|error| format!("{}: {error}", place.display()))
    }
}

/// The real thing: for this user, and no administrator's. There is one entry, whatever the folder: `folder` is the install this speaks for, which may not be the one registered.
pub struct RealShell {
    pub folder: PathBuf,
}

/// Whether taking away a folder's install takes the entries too: not when they name another install, which is still there and still in use. None: nothing says.
fn entries_are_of(registered: Option<&str>, folder: &Path) -> bool {
    let plain = |text: &str| text.trim_end_matches('\\').to_lowercase();
    registered.is_none_or(|registered| plain(registered) == plain(&folder.to_string_lossy()))
}

/// Where Add or remove programs says the install is.
fn registered_folder() -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let mut text = vec![0u16; 2048];
    let mut size = (text.len() * 2) as u32;
    // SAFETY: the names end in zeros, and the buffer is as long as the call is told.
    let read = unsafe { RegGetValueW(HKEY_CURRENT_USER, wide(UNINSTALL_KEY).as_ptr(), wide("InstallLocation").as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), text.as_mut_ptr().cast(), &mut size) };
    (read == 0).then(|| {
        let length = text.iter().position(|&unit| unit == 0).unwrap_or(text.len());
        String::from_utf16_lossy(&text[..length])
    })
}

fn start_menu() -> Result<PathBuf, String> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Programs, SHGetKnownFolderPath};
    let mut text = std::ptr::null_mut();
    // SAFETY: the system gives a string that ends in a zero, for the caller to free.
    unsafe {
        if SHGetKnownFolderPath(&FOLDERID_Programs, 0, std::ptr::null_mut(), &mut text) != 0 || text.is_null() {
            return Err("the Start menu's folder is not known".to_string());
        }
        let length = (0..).take_while(|&at| *text.add(at) != 0).count();
        let path = PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(text, length)));
        CoTaskMemFree(text.cast());
        Ok(path)
    }
}

impl Shell for RealShell {
    fn add_entries(&self, folder: &Path, version: &str, kilobytes: u64) -> Result<(), String> {
        use windows_sys::Win32::System::Registry::{RegCloseKey, RegCreateKeyW, RegSetValueExW, HKEY_CURRENT_USER, REG_DWORD, REG_SZ};
        write_shortcut(&start_menu()?.join(SHORTCUT), &folder.join("dogmind-app.exe"), "Dogmind App")?;
        let (texts, numbers) = uninstall_values(folder, version, kilobytes);
        // SAFETY: the names end in zeros, and each value is as long as the call is told.
        unsafe {
            let mut key = std::ptr::null_mut();
            let opened = RegCreateKeyW(HKEY_CURRENT_USER, wide(UNINSTALL_KEY).as_ptr(), &mut key);
            if opened != 0 {
                return Err(format!("Add or remove programs: error {opened}"));
            }
            for (name, value) in texts {
                let units = wide(&value);
                RegSetValueExW(key, wide(name).as_ptr(), 0, REG_SZ, units.as_ptr().cast(), (units.len() * 2) as u32);
            }
            for (name, value) in numbers {
                RegSetValueExW(key, wide(name).as_ptr(), 0, REG_DWORD, std::ptr::addr_of!(value).cast(), 4);
            }
            RegCloseKey(key);
        }
        Ok(())
    }

    fn remove_entries(&self) -> Result<(), String> {
        use windows_sys::Win32::System::Registry::{RegDeleteKeyW, HKEY_CURRENT_USER};
        if !entries_are_of(registered_folder().as_deref(), &self.folder) {
            return Ok(());
        }
        let _ = std::fs::remove_file(start_menu()?.join(SHORTCUT));
        // SAFETY: the name ends in a zero.
        unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, wide(UNINSTALL_KEY).as_ptr()) };
        Ok(())
    }
}

/// The test switch: what would be registered goes in a file in the scratch folder, and the shortcut in a folder beside it. Neither the Start menu nor the registry is touched.
pub struct TestShell {
    pub folder: PathBuf,
}

pub const TEST_RECORD: &str = ".setup-test-shell.txt";
pub const TEST_START_MENU: &str = ".setup-test-start-menu";

impl Shell for TestShell {
    fn add_entries(&self, folder: &Path, version: &str, kilobytes: u64) -> Result<(), String> {
        let menu = self.folder.join(TEST_START_MENU);
        std::fs::create_dir_all(&menu).map_err(|error| error.to_string())?;
        write_shortcut(&menu.join(SHORTCUT), &folder.join("dogmind-app.exe"), "Dogmind App")?;
        let (texts, numbers) = uninstall_values(folder, version, kilobytes);
        let mut record = format!("HKCU\\{UNINSTALL_KEY}\n");
        for (name, value) in texts {
            record.push_str(&format!("{name} = {value}\n"));
        }
        for (name, value) in numbers {
            record.push_str(&format!("{name} = {value}\n"));
        }
        std::fs::write(self.folder.join(TEST_RECORD), record).map_err(|error| error.to_string())
    }

    fn remove_entries(&self) -> Result<(), String> {
        let _ = std::fs::remove_dir_all(self.folder.join(TEST_START_MENU));
        let _ = std::fs::remove_file(self.folder.join(TEST_RECORD));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_or_remove_programs_is_told_what_runs_the_removal() {
        let (texts, numbers) = uninstall_values(Path::new("C:\\Users\\me\\AppData\\Local\\Programs\\Dogmind"), "1.2.3", 1_000_000);
        let text = |name: &str| texts.iter().find(|(key, _)| *key == name).unwrap().1.clone();
        assert_eq!(text("DisplayName"), "Dogmind App");
        assert_eq!(text("DisplayVersion"), "1.2.3");
        assert_eq!(text("UninstallString"), "\"C:\\Users\\me\\AppData\\Local\\Programs\\Dogmind\\dogmind-uninstall.exe\" --uninstall", "in quotes: the path may have spaces");
        assert!(text("DisplayIcon").ends_with("dogmind-app.exe,0"));
        assert!(numbers.contains(&("NoModify", 1)) && numbers.contains(&("NoRepair", 1)) && numbers.contains(&("EstimatedSize", 1_000_000)));
        assert!(UNINSTALL_KEY.starts_with("Software\\"), "under this user, as what needs no administrator");
    }

    #[test]
    fn removing_a_second_install_leaves_the_entries_of_the_one_they_name() {
        let installed = Path::new("C:\\Users\\me\\AppData\\Local\\Programs\\Dogmind");
        assert!(entries_are_of(Some("c:\\users\\ME\\AppData\\Local\\Programs\\Dogmind\\"), installed));
        assert!(entries_are_of(None, installed), "nothing says otherwise");
        assert!(!entries_are_of(Some("C:\\Users\\me\\AppData\\Local\\Programs\\Dogmind"), Path::new("D:\\Other")));
    }

    #[test]
    fn a_shortcut_is_made_and_leads_to_the_program() {
        use windows::core::{Interface, HSTRING, PCWSTR};
        use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ};
        use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
        let dir = tempfile::tempdir().unwrap();
        let (place, target) = (dir.path().join("x.lnk"), dir.path().join("\u{65e5}\u{672c} y").join("dogmind-app.exe"));
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "").unwrap();
        write_shortcut(&place, &target, "Dogmind App").unwrap();
        // SAFETY: COM on this thread, begun above; the buffer is as long as it is said to be.
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            link.cast::<IPersistFile>().unwrap().Load(PCWSTR(HSTRING::from(place.as_os_str()).as_ptr()), STGM_READ).unwrap();
            let mut found = [0u16; 1024];
            link.GetPath(&mut found, std::ptr::null_mut(), 0).unwrap();
            let length = found.iter().position(|&unit| unit == 0).unwrap();
            assert_eq!(PathBuf::from(String::from_utf16_lossy(&found[..length])), target);
        }
    }

    #[test]
    fn the_test_switch_stays_in_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        let shell = TestShell { folder: dir.path().to_path_buf() };
        shell.add_entries(dir.path(), "1.0.0", 5).unwrap();
        let record = std::fs::read_to_string(dir.path().join(TEST_RECORD)).unwrap();
        assert!(record.contains("DisplayName = Dogmind App") && record.contains("--uninstall"), "{record}");
        assert!(dir.path().join(TEST_START_MENU).join(SHORTCUT).is_file());
        shell.remove_entries().unwrap();
        assert!(!dir.path().join(TEST_RECORD).exists() && !dir.path().join(TEST_START_MENU).exists());
    }

    #[test]
    fn what_runs_from_a_folder_is_found_and_asked_and_never_ended() {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("sleeper.exe");
        std::fs::copy(std::env::var_os("ComSpec").unwrap(), &copy).unwrap();
        use std::os::windows::process::CommandExt;
        // (no console of its own: a test puts nothing on the person's screen)
        let mut child = std::process::Command::new(&copy).args(["/c", "ping -n 10 127.0.0.1 >nul"]).creation_flags(0x0800_0000).stdout(std::process::Stdio::null()).spawn().unwrap();
        let found = running_from(dir.path(), false);
        assert!(found.iter().any(|running| running.pid == child.id() && running.name == "sleeper.exe"), "{found:?}");
        assert!(running_from(&dir.path().join("other"), false).is_empty(), "nothing else's");
        assert!(!is_in(Path::new("C:\\a\\bc\\x.exe"), Path::new("C:\\a\\b")));
        assert!(is_in(Path::new("C:\\A\\B\\x.exe"), Path::new("c:\\a\\b\\")));
        // It has no window to hear the question: it goes on, and is still there.
        ask_to_close(&[child.id()]);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(child.try_wait().unwrap().is_none());
        let _ = child.kill();
        let _ = child.wait();
    }
}
