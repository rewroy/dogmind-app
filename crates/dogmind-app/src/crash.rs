//! What is left when the app goes wrong where no one can see it: a window program has no error stream. A panic or a fault leaves a text file in the data folder, the next start says so,
//! and a report gathers what a stranger can send: words, never a key or the content of a conversation.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::lang::tf;
use crate::settings::Settings;

/// Where reports go: the public repository's issues, the only address for them the app itself knows, kept here alone. With none, a
/// report's words would only say where the file is, for the person to send however they like.
pub const WHERE_REPORTS_GO: Option<&str> = Some("https://github.com/rewroy/dogmind-app/issues");

const FOLDER: &str = "crashes";
const MOST_LOG_LINES: usize = 200;

static CRASHES: OnceLock<PathBuf> = OnceLock::new();

/// What the person is told once a report is written.
pub fn report_words(file: &str) -> String {
    report_words_with(file, WHERE_REPORTS_GO)
}

fn report_words_with(file: &str, address: Option<&str>) -> String {
    match address {
        Some(address) => tf("Written to {0}. Nothing is sent by itself: read it, and attach it at {1} with what you were doing.", &[&file, &address]),
        None => tf("Written to {0}. Nothing is sent by itself: read it, and send it with what you were doing to whoever you had this app from, in any way you like.", &[&file]),
    }
}

fn version_line() -> String {
    format!("Dogmind {} ({}), Windows build {}", env!("CARGO_PKG_VERSION"), env!("DOGMIND_APP_BUILD"), crate::machine::windows_build().map_or("unknown".to_string(), |build| build.to_string()))
}

fn seconds_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

fn write_crash(words: &str) {
    let Some(folder) = CRASHES.get() else { return };
    let _ = std::fs::create_dir_all(folder);
    let _ = std::fs::write(folder.join(format!("crash-{}.txt", seconds_now())), format!("{}\n{words}\n", version_line()));
}

/// From here on what goes wrong is written down.
pub fn install(data: &Path) {
    let _ = CRASHES.set(data.join(FOLDER));
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        // (the message and where: what was being worked on is not in it, unless the code put it in its own panic's words)
        write_crash(&format!("panic on thread {}\n{info}\n\n{}", thread.name().unwrap_or("unnamed"), std::backtrace::Backtrace::force_capture()));
    }));
    use windows_sys::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter;
    // SAFETY: the function lives as long as the process.
    unsafe { SetUnhandledExceptionFilter(Some(on_fault)) };
}

/// A fault no Rust code saw: an address, the module it was in, and the stack if there is room to take it.
unsafe extern "system" fn on_fault(pointers: *const windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS) -> i32 {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT};
    const STACK_OVERFLOW: u32 = 0xC000_00FD;
    let record = if pointers.is_null() { std::ptr::null() } else { (*pointers).ExceptionRecord };
    if !record.is_null() {
        let (code, address) = ((*record).ExceptionCode as u32, (*record).ExceptionAddress);
        let mut module = std::ptr::null_mut();
        let mut name = [0u16; 260];
        let mut length = 0;
        if GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, address as *const u16, &mut module) != 0 {
            length = GetModuleFileNameW(module, name.as_mut_ptr(), 260) as usize;
        }
        let file = String::from_utf16_lossy(&name[..length]);
        let file = file.rsplit('\\').next().unwrap_or("");
        let within = (address as usize).wrapping_sub(module as usize);
        let stack = if code == STACK_OVERFLOW { "(the stack overflowed: none to show)".to_string() } else { std::backtrace::Backtrace::force_capture().to_string() };
        write_crash(&format!("fault {code:#010x} at {file}+{within:#x}\n\n{stack}"));
    }
    0 // EXCEPTION_CONTINUE_SEARCH: Windows ends it as it would have
}

/// The crash the person has not been told of, the latest; told once.
pub fn untold(data: &Path) -> Option<PathBuf> {
    let folder = data.join(FOLDER);
    let mut found: Vec<PathBuf> = std::fs::read_dir(&folder).ok()?.flatten().map(|entry| entry.path()).filter(|path| path.extension().is_some_and(|extension| extension == "txt")).collect();
    found.sort();
    let latest = found.last()?.clone();
    let mark = folder.join("told");
    if std::fs::read_to_string(&mark).ok().as_deref() == latest.file_name().and_then(|name| name.to_str()) {
        return None;
    }
    let _ = std::fs::write(mark, latest.file_name()?.to_str()?);
    Some(latest)
}

/// The person's name is not what a report is for.
fn without_the_person(text: &str, home: Option<&str>, user: Option<&str>) -> String {
    let mut text = text.to_string();
    if let Some(home) = home.filter(|home| !home.is_empty()) {
        text = text.replace(home, "%USERPROFILE%");
    }
    if let Some(user) = user.filter(|user| user.len() >= 3) {
        text = text.replace(user, "%USERNAME%");
    }
    text
}

fn last_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

/// What a stranger can send when something goes wrong, in a file they can read first: what ran, on what, and what the engine and any crash said. No key, no address of a
/// server, no conversation. `about_this_pc`: what Straydog says of the GPU and the memory.
pub fn write_report(data: &Path, settings: &Settings, about_this_pc: &str) -> Result<PathBuf, String> {
    let mut text = format!("{}\n", version_line());
    let exe = std::env::current_exe().map_or(String::new(), |exe| exe.display().to_string());
    text.push_str(&format!("Program: {exe}\nThis PC: {about_this_pc}\nLanguage of Windows: {}\n", if crate::lang::system_is_japanese() { "Japanese" } else { "not Japanese" }));
    text.push_str(&format!("Reaching a model by: {}\nEngine's port: {}\n", settings.active_provider().map_or("none", |provider| provider.kind.label()), settings.port));
    if let Some(folder) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf)) {
        text.push_str("\nBeside the program:\n");
        for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
            let size = entry.metadata().map_or(0, |data| data.len());
            text.push_str(&format!("  {}  {size}\n", entry.file_name().to_string_lossy()));
        }
    }
    let mut crashes: Vec<PathBuf> = std::fs::read_dir(data.join(FOLDER)).into_iter().flatten().flatten().map(|entry| entry.path()).filter(|path| path.extension().is_some_and(|extension| extension == "txt")).collect();
    crashes.sort();
    for crash in crashes.iter().rev().take(3).rev() {
        text.push_str(&format!("\n--- {} ---\n{}\n", crash.file_name().map_or(String::new(), |name| name.to_string_lossy().into_owned()), std::fs::read_to_string(crash).unwrap_or_default()));
    }
    let log = std::fs::read(crate::engine::log_path(data)).map_or(String::new(), |bytes| String::from_utf8_lossy(&bytes).into_owned());
    text.push_str(&format!("\n--- the last of the engine's log ---\n{}\n", last_lines(&log, MOST_LOG_LINES)));
    let text = without_the_person(&text, std::env::var("USERPROFILE").ok().as_deref(), std::env::var("USERNAME").ok().as_deref());
    let path = data.join("report.txt");
    std::fs::create_dir_all(data).and_then(|()| std::fs::write(&path, text)).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_crash_is_told_once() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(untold(dir.path()), None);
        let folder = dir.path().join(FOLDER);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("crash-1.txt"), "a").unwrap();
        std::fs::write(folder.join("crash-2.txt"), "b").unwrap();
        assert_eq!(untold(dir.path()), Some(folder.join("crash-2.txt")));
        assert_eq!(untold(dir.path()), None);
        std::fs::write(folder.join("crash-3.txt"), "c").unwrap();
        assert_eq!(untold(dir.path()), Some(folder.join("crash-3.txt")));
    }

    #[test]
    fn a_report_says_where_the_file_is_and_where_to_send_it() {
        let words = report_words("C:\\data\\report.txt");
        assert!(words.contains("C:\\data\\report.txt") && words.contains("https://github.com/rewroy/dogmind-app/issues"), "{words}");
        let without = report_words_with("f.txt", None);
        assert!(without.contains("f.txt") && without.contains("any way you like") && !without.contains("http"), "{without}");
    }

    #[test]
    fn a_report_has_what_helps_and_not_the_person() {
        let dir = tempfile::tempdir().unwrap();
        let log: String = (0..300).map(|at| format!("line {at}\n")).collect();
        std::fs::write(crate::engine::log_path(dir.path()), log).unwrap();
        let folder = dir.path().join(FOLDER);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("crash-1.txt"), "fault here").unwrap();
        let mut settings = Settings::default();
        settings.providers[2].api_key = "credential:Claude".into();
        settings.providers.push(crate::settings::ProviderConfig { name: "Home".into(), kind: crate::settings::Kind::Server, base_url: "http://192.168.7.7:1/v1".into(), model: String::new(), api_key: String::new() });
        settings.active = "Home".into();
        let text = std::fs::read_to_string(write_report(dir.path(), &settings, "RTX (6 GB)").unwrap()).unwrap();
        assert!(text.contains("RTX (6 GB)") && text.contains("fault here") && text.contains("line 299") && !text.contains("line 99\n"), "{text}");
        assert!(!text.contains("192.168.7.7") && !text.contains("credential"), "{text}");
        assert_eq!(without_the_person("C:\\Users\\me\\x and me", Some("C:\\Users\\me"), Some("me")), "%USERPROFILE%\\x and me", "too short a name is not replaced in the words");
        assert_eq!(without_the_person("C:\\Users\\みやけ\\x みやけ", Some("C:\\Users\\みやけ"), Some("みやけ")), "%USERPROFILE%\\x %USERNAME%");
    }
}
