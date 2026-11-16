//! A drive letter that names a workspace while one command runs: what cargo needs in the sandbox to expand a workspace's
//! `members = ["crates/*"]` when the workspace is under the user folder.
//!
//! cargo expands such a pattern from the drive's root, looking at each folder on the way (the `glob` crate), and a container may not
//! look at `C:\Users` or `C:\`: the pattern finds nothing, and cargo reads it as a folder's name ("failed to read `…\crates\*\Cargo.toml`").
//! From `Z:\`, which names the workspace itself, there is no folder above to look at. The letter is the logon session's, as `subst`
//! makes one (`DefineDosDeviceW`): no administrator and nothing kept on the machine, and Explorer is not told of it. It goes when the
//! command ends ([`Letter`]'s drop), and one that an app which ended at once left behind goes at the next one's start (a list beside
//! the app's other files).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows_sys::Win32::Storage::FileSystem::{DefineDosDeviceW, GetLogicalDrives, QueryDosDeviceW, DDD_EXACT_MATCH_ON_REMOVE, DDD_NO_BROADCAST_SYSTEM, DDD_REMOVE_DEFINITION};

/// One choice of a letter at a time in this process: two commands at once each get their own.
static CHOOSING: Mutex<()> = Mutex::new(());

/// A letter that names a folder until this is dropped.
pub struct Letter {
    letter: char,
    folder: String,
}

impl Letter {
    pub fn letter(&self) -> char {
        self.letter
    }
}

impl Drop for Letter {
    fn drop(&mut self) {
        let _one = CHOOSING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        remove(self.letter, &self.folder);
        forget(self.letter);
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// What the letter names now, its newest definition first; none when it names nothing.
fn targets(letter: char) -> Vec<String> {
    let name = wide(&format!("{letter}:"));
    let mut buffer = vec![0u16; 8192];
    // SAFETY: a name that ends in 0 and a buffer of the length given.
    let length = unsafe { QueryDosDeviceW(name.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    buffer[..length.min(buffer.len())].split(|&unit| unit == 0).filter(|part| !part.is_empty()).map(String::from_utf16_lossy).collect()
}

fn remove(letter: char, folder: &str) {
    let (name, target) = (wide(&format!("{letter}:")), wide(folder));
    // SAFETY: both end in 0; only the definition that names this folder goes, and not one another program made on the same letter.
    unsafe { DefineDosDeviceW(DDD_REMOVE_DEFINITION | DDD_EXACT_MATCH_ON_REMOVE | DDD_NO_BROADCAST_SYSTEM, name.as_ptr(), target.as_ptr()) };
}

/// A letter for `folder`, from Z down, that no drive and no other definition has; none if there is none to have.
pub fn name(folder: &Path) -> Option<Letter> {
    let folder = folder.to_str()?.trim_end_matches('\\').to_string();
    if !Path::new(&folder).is_absolute() || folder.starts_with(r"\\") {
        return None;
    }
    let _one = CHOOSING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    take_back_left_ones();
    let named = format!("\\??\\{folder}");
    // SAFETY: no arguments.
    let drives = unsafe { GetLogicalDrives() };
    for letter in ('D'..='Z').rev() {
        if drives & (1 << (letter as u32 - 'A' as u32)) != 0 || !targets(letter).is_empty() {
            continue;
        }
        let (name, target) = (wide(&format!("{letter}:")), wide(&folder));
        // SAFETY: both end in 0.
        if unsafe { DefineDosDeviceW(DDD_NO_BROADCAST_SYSTEM, name.as_ptr(), target.as_ptr()) } == 0 {
            continue;
        }
        let now = targets(letter);
        if now.len() == 1 && now[0].eq_ignore_ascii_case(&named) {
            note(letter, &folder);
            return Some(Letter { letter, folder });
        }
        // (another program took the letter at the same moment: ours goes, and the next is tried)
        remove(letter, &folder);
    }
    None
}

/// The list of the letters this app has named, and by which process, for a later start to take back what an app that ended at once left.
fn list() -> Option<PathBuf> {
    crate::workspace_grants::data_folder().map(|folder| folder.join("drive-letters.txt"))
}

fn note(letter: char, folder: &str) {
    let Some(file) = list() else { return };
    let mut text = std::fs::read_to_string(&file).unwrap_or_default();
    text.push_str(&format!("{letter}\t{}\t{folder}\n", std::process::id()));
    let _ = std::fs::write(&file, text);
}

fn forget(letter: char) {
    let Some(file) = list() else { return };
    let Ok(text) = std::fs::read_to_string(&file) else { return };
    let mine = std::process::id().to_string();
    let kept: String = text.lines().filter(|line| {
        let mut parts = line.split('\t');
        !(parts.next() == Some(&letter.to_string()) && parts.next() == Some(mine.as_str()))
    }).map(|line| format!("{line}\n")).collect();
    let _ = if kept.is_empty() { std::fs::remove_file(&file) } else { std::fs::write(&file, kept) };
}

fn take_back_left_ones() {
    let Some(file) = list() else { return };
    let Ok(text) = std::fs::read_to_string(&file) else { return };
    let mut kept = String::new();
    for line in text.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(letter), Some(pid), Some(folder)) = (parts.next().and_then(|letter| letter.chars().next()), parts.next().and_then(|pid| pid.parse::<u32>().ok()), parts.next()) else { continue };
        if alive(pid) {
            kept.push_str(&format!("{line}\n"));
        } else if targets(letter).iter().any(|target| target.eq_ignore_ascii_case(&format!("\\??\\{folder}"))) {
            remove(letter, folder);
        }
    }
    let _ = if kept.is_empty() { std::fs::remove_file(&file) } else { std::fs::write(&file, kept) };
}

fn alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: a handle this opens and closes.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let mut code = 0u32;
        let running = GetExitCodeProcess(process, &mut code) != 0 && code == STILL_ACTIVE as u32;
        CloseHandle(process);
        running
    }
}

/// A command's output as the workspace's paths say it: the letter's root becomes the folder's, as it is written plainly, with `/`, and inside JSON.
pub fn in_folder_terms(text: &str, letter: char, folder: &Path) -> String {
    let plain = folder.display().to_string().trim_end_matches('\\').to_string();
    let escaped = plain.replace('\\', "\\\\");
    let forward = plain.replace('\\', "/");
    let mut out = String::with_capacity(text.len());
    let (upper, lower) = (letter.to_ascii_uppercase(), letter.to_ascii_lowercase());
    let mut rest = text;
    while let Some(at) = rest.find([upper, lower]) {
        let (before, here) = rest.split_at(at);
        out.push_str(before);
        let after = &here[letter.len_utf8()..];
        // (only where the letter begins a path: not inside a word, as in `XYZ:\`)
        let starts_a_path = !before.chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric());
        let replaced = if !starts_a_path {
            None
        } else if let Some(tail) = after.strip_prefix(":\\\\") {
            Some((format!("{escaped}\\\\"), tail))
        } else if let Some(tail) = after.strip_prefix(":\\") {
            Some((format!("{plain}\\"), tail))
        } else if let Some(tail) = after.strip_prefix(":/") {
            Some((format!("{forward}/"), tail))
        } else {
            None
        };
        match replaced {
            Some((path, tail)) => {
                out.push_str(&path);
                rest = tail;
            }
            None => {
                out.push(here.chars().next().expect("found there"));
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_letter_names_the_folder_while_it_is_held_and_not_after() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("here.txt"), "x").unwrap();
        let held = name(dir.path()).expect("a free letter");
        let root = format!("{}:\\", held.letter());
        assert!(Path::new(&root).join("here.txt").is_file(), "{root}");
        assert_eq!(targets(held.letter()).len(), 1);
        let letter = held.letter();
        drop(held);
        assert!(targets(letter).is_empty() && !Path::new(&format!("{letter}:\\here.txt")).exists(), "{letter}: is still there");
        // two at once have two letters
        let (first, second) = (name(dir.path()).unwrap(), name(dir.path()).unwrap());
        assert_ne!(first.letter(), second.letter());
    }

    #[test]
    fn output_is_said_in_the_folders_paths() {
        let folder = Path::new(r"C:\Users\a\work");
        let json = r#"{"src_path":"Z:\\crates\\a\\src\\lib.rs","id":"path+file:///Z:/crates/a#0.1.0"}"#;
        assert_eq!(in_folder_terms(json, 'Z', folder), r#"{"src_path":"C:\\Users\\a\\work\\crates\\a\\src\\lib.rs","id":"path+file:///C:/Users/a/work/crates/a#0.1.0"}"#);
        assert_eq!(in_folder_terms("error[E0425]\n  --> Z:\\crates\\a\\src\\lib.rs:3:5\nZ:\\\n", 'Z', folder), "error[E0425]\n  --> C:\\Users\\a\\work\\crates\\a\\src\\lib.rs:3:5\nC:\\Users\\a\\work\\\n");
        // a letter that is not a path's beginning is left as it is
        assert_eq!(in_folder_terms("XYZ:\\ and Zebra: z:/x", 'Z', folder), "XYZ:\\ and Zebra: C:/Users/a/work/x");
    }
}
