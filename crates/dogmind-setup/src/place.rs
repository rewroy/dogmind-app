//! Where the program goes: which folder a person's choice comes to, whether this person can put files in it, and where an install that is there already is found. No window and no
//! registry here: the words for a refusal are the window's, and what is registered is read by `system` and handed in.

use std::path::{Path, PathBuf};

use crate::install;

/// What a folder is called inside one the person chose, when that one has other things in it.
const SUBFOLDER: &str = "Dogmind";

/// Why a folder cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// It is, or is inside, the folder of the app's settings and sessions, which a removal empties.
    SettingsFolder,
    /// This person cannot make a file there, as it is with a folder only an administrator can write in. The system's words for it.
    NotWritable(String),
}

/// The folders that hold what a person keeps: settings, sessions and the engine's log. An install is never put in them, or in anything in them.
pub fn settings_folders() -> Vec<PathBuf> {
    std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty()).map(|local| PathBuf::from(local).join("DogmindApp")).into_iter().collect()
}

/// A folder is where a person's choice lands, unless that holds other things and no install of this: then it is a `Dogmind` folder inside it, as most installers do. What is empty, what is
/// not there yet and what has this program's own install in it is used as it is, and so is no drive's root, which is never an install's folder.
pub fn choose(picked: &Path) -> PathBuf {
    let as_it_is = picked.parent().is_some()
        && (install::read_manifest(picked).is_some()
            || match std::fs::read_dir(picked) {
                Ok(mut entries) => entries.next().is_none(),
                Err(error) => error.kind() == std::io::ErrorKind::NotFound,
            });
    if as_it_is { picked.to_path_buf() } else { picked.join(SUBFOLDER) }
}

/// A path as its parts, lower case, for telling whether one is inside another: made absolute, the names of what is there as the system has them (not a short name, not a link), and no
/// `\\?\` in front.
fn parts(path: &Path) -> Vec<String> {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let real = path.ancestors().find_map(|up| std::fs::canonicalize(up).ok().map(|real| (up.to_path_buf(), real)));
    let whole = match real {
        Some((up, real)) => real.join(path.strip_prefix(&up).unwrap_or(Path::new(""))),
        None => path,
    };
    let text = whole.to_string_lossy().replace('/', "\\");
    let text = match (text.strip_prefix("\\\\?\\UNC\\"), text.strip_prefix("\\\\?\\")) {
        (Some(rest), _) => format!("\\\\{rest}"),
        (None, Some(rest)) => rest.to_string(),
        (None, None) => text,
    };
    text.to_lowercase().split('\\').filter(|part| !part.is_empty()).map(str::to_string).collect()
}

/// Whether `folder` is `within` or in it, at any depth.
pub fn is_inside(folder: &Path, within: &Path) -> bool {
    parts(folder).starts_with(&parts(within))
}

/// Whether this person can put files in the folder, as an install would: by making one there and taking it away. If the folder is not there yet it is made, by this, under the nearest
/// folder that is, and so what is checked is the place where it would be made. Nothing is left behind.
pub fn writable(folder: &Path) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    let nearest = folder.ancestors().find(|up| up.exists()).ok_or_else(|| Error::new(ErrorKind::NotFound, format!("{}: there is no such place", folder.display())))?;
    if !nearest.is_dir() {
        return Err(Error::new(ErrorKind::AlreadyExists, format!("{}: is a file, not a folder", nearest.display())));
    }
    let probe = format!(".dogmind-setup-probe-{}", std::process::id());
    // (a folder that is not there is made as the install makes it: a new one in the nearest, and a file in that)
    let place = if folder.is_dir() { folder.to_path_buf() } else { nearest.join(&probe) };
    let made_place = place != folder;
    if made_place {
        std::fs::create_dir(&place)?;
    }
    let file = place.join(&probe);
    let result = std::fs::OpenOptions::new().write(true).create_new(true).open(&file).and_then(|mut out| std::io::Write::write_all(&mut out, b"x"));
    let _ = std::fs::remove_file(&file);
    if made_place {
        let _ = std::fs::remove_dir(&place);
    }
    result
}

/// The system's words for an error, without the `(os error 5)` Rust puts after them, or their full stop: they go in the middle of a sentence of ours.
fn words_of(error: &std::io::Error) -> String {
    let text = error.to_string();
    let text = text.rfind(" (os error ").map_or(text.as_str(), |at| &text[..at]);
    text.trim_end_matches(['.', '。', ' ']).to_string()
}

/// Whether an install may go in this folder: not in the settings' folders, and where this person can write.
pub fn check(folder: &Path, settings: &[PathBuf]) -> Result<(), Refusal> {
    if settings.iter().any(|kept| is_inside(folder, kept)) {
        return Err(Refusal::SettingsFolder);
    }
    writable(folder).map_err(|error| Refusal::NotWritable(words_of(&error)))
}

/// Where an update goes: the install that is registered, when its folder still holds one, or else the default folder, which is where a first one goes too.
pub fn existing_install(registered: Option<&str>, default: Option<PathBuf>) -> Option<PathBuf> {
    registered
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(PathBuf::from)
        .filter(|folder| folder.is_absolute() && install::read_manifest(folder).is_some())
        .or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_install(folder: &Path) {
        std::fs::create_dir_all(folder).unwrap();
        std::fs::write(folder.join(install::MANIFEST), "version 1\nfile a\n").unwrap();
    }

    #[test]
    fn what_holds_other_things_gets_a_subfolder_and_the_rest_is_used_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let empty = root.join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert_eq!(choose(&empty), empty, "nothing in it");
        assert_eq!(choose(&root.join("not there yet")), root.join("not there yet"), "a new folder is made as named");
        let full = root.join("full");
        std::fs::create_dir_all(full.join("some folder")).unwrap();
        std::fs::write(full.join("a.txt"), "a").unwrap();
        assert_eq!(choose(&full), full.join("Dogmind"), "other things in it");
        let only_a_folder = root.join("only a folder");
        std::fs::create_dir_all(only_a_folder.join("x")).unwrap();
        assert_eq!(choose(&only_a_folder), only_a_folder.join("Dogmind"), "a folder is something too");
        // An install of this program in it: it is that one that is updated, other things or not.
        let ours = root.join("ours");
        with_install(&ours);
        std::fs::write(ours.join("other.txt"), "x").unwrap();
        assert_eq!(choose(&ours), ours);
        // The subfolder is where an earlier choice may have put an install: choosing the same again is not another level deeper.
        assert_eq!(choose(&full.join("Dogmind")), full.join("Dogmind"), "not there yet");
        with_install(&full.join("Dogmind"));
        assert_eq!(choose(&full.join("Dogmind")), full.join("Dogmind"));
        // A drive's root is not an install's folder, however empty it is.
        assert_eq!(choose(Path::new("D:\\")), Path::new("D:\\").join("Dogmind"));
    }

    #[test]
    fn the_folder_of_settings_and_sessions_is_refused_whatever_is_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("Local");
        let kept = local.join("DogmindApp");
        let settings = [kept.clone()];
        // (not there at all: a person who never ran the app has none, and it is refused all the same)
        for refused in [kept.clone(), kept.join("Dogmind"), kept.join("sessions").join("x").join("y"), local.join("dogmindapp").join("X"), local.join("tmp").join("..").join("DogmindApp").join("z")] {
            assert_eq!(check(&refused, &settings), Err(Refusal::SettingsFolder), "{}", refused.display());
        }
        std::fs::create_dir_all(kept.join("sessions")).unwrap();
        for refused in [kept.clone(), kept.join("Dogmind"), kept.join("sessions"), PathBuf::from(format!("{}\\", kept.display()))] {
            assert_eq!(check(&refused, &settings), Err(Refusal::SettingsFolder), "{}", refused.display());
        }
        for fine in [local.clone(), local.join("Dogmind"), local.join("DogmindApp2"), local.join("DogmindApp 2").join("x"), local.join("Programs").join("Dogmind"), dir.path().join("DogmindApp")] {
            assert_eq!(check(&fine, &settings), Ok(()), "{}", fine.display());
        }
        assert!(!is_inside(&local, &kept) && is_inside(&kept, &local), "the folder above it is not in it");
        assert_eq!(check(&dir.path().join("x"), &[]), Ok(()), "no settings folder is known");
    }

    #[test]
    fn a_folder_is_writable_when_a_file_can_be_made_in_it_and_nothing_stays() {
        let dir = tempfile::tempdir().unwrap();
        let count = || std::fs::read_dir(dir.path()).unwrap().count();
        assert!(writable(dir.path()).is_ok());
        assert_eq!(count(), 0, "the probe file is taken away");
        // Not there yet: the place where it would be made is what is tried, and it is not made.
        assert!(writable(&dir.path().join("a").join("b").join("c")).is_ok());
        assert_eq!(count(), 0, "neither the probe nor a folder of the name is left");
        let existing = dir.path().join("there");
        std::fs::create_dir(&existing).unwrap();
        assert!(writable(&existing).is_ok());
        assert_eq!(std::fs::read_dir(&existing).unwrap().count(), 0);
        // A file in the way, at the folder or above it: the install could not make the folder.
        let file = dir.path().join("a file");
        std::fs::write(&file, "x").unwrap();
        assert!(writable(&file).unwrap_err().to_string().contains("a file"));
        assert!(writable(&file.join("below")).is_err());
        if !Path::new("Q:\\").exists() {
            assert!(writable(Path::new("Q:\\no such drive\\Dogmind")).is_err());
        }
        assert_eq!(check(&file, &[]).unwrap_err(), Refusal::NotWritable(words_of(&writable(&file).unwrap_err())));
    }

    #[test]
    fn the_systems_words_go_in_a_sentence_without_their_code_or_full_stop() {
        let denied = words_of(&std::io::Error::from_raw_os_error(5));
        assert!(!denied.is_empty() && !denied.contains("os error") && !denied.ends_with(['.', '。']), "{denied}");
        assert_eq!(words_of(&std::io::Error::other("it broke.")), "it broke");
        assert_eq!(words_of(&std::io::Error::other("アクセスが拒否されました。")), "アクセスが拒否されました");
    }

    #[test]
    fn the_real_settings_folder_is_refused_without_anything_made_in_it() {
        let kept = settings_folders();
        let Some(first) = kept.first() else { return };
        assert!(first.ends_with("DogmindApp"));
        // (refused before any file is tried, so this makes nothing in the person's own folder, there or not)
        for refused in [first.clone(), first.join("Dogmind"), first.join("sessions").join("x")] {
            assert_eq!(check(&refused, &kept), Err(Refusal::SettingsFolder), "{}", refused.display());
        }
    }

    /// Whether this process runs with an administrator's rights, in which case a folder only an administrator writes in can be written.
    fn elevated() -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        // SAFETY: the token is this function's, and the structure is as long as the call is told.
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut size = 0u32;
            let read = GetTokenInformation(token, TokenElevation, std::ptr::addr_of_mut!(elevation).cast(), std::mem::size_of::<TOKEN_ELEVATION>() as u32, &mut size);
            CloseHandle(token);
            read != 0 && elevation.TokenIsElevated != 0
        }
    }

    #[test]
    fn program_files_is_refused_to_whoever_is_not_an_administrator() {
        let Some(program_files) = std::env::var_os("ProgramFiles").map(PathBuf::from).filter(|folder| folder.is_dir()) else { return };
        if elevated() {
            return;
        }
        for folder in [program_files.clone(), program_files.join("Dogmind"), choose(&program_files)] {
            match check(&folder, &[]) {
                Err(Refusal::NotWritable(why)) => assert!(!why.is_empty(), "{}", folder.display()),
                other => panic!("{}: {other:?}", folder.display()),
            }
        }
        assert!(!program_files.join("Dogmind").exists() || install::read_manifest(&program_files.join("Dogmind")).is_some(), "the check made nothing there");
    }

    #[test]
    fn an_update_goes_where_the_install_is_registered_if_it_is_still_there() {
        let dir = tempfile::tempdir().unwrap();
        let (registered, default) = (dir.path().join("D drive").join("Dogmind"), dir.path().join("Programs").join("Dogmind"));
        let text = registered.to_string_lossy().into_owned();
        // Registered, but gone or never an install of this: the default.
        assert_eq!(existing_install(Some(&text), Some(default.clone())), Some(default.clone()));
        std::fs::create_dir_all(&registered).unwrap();
        assert_eq!(existing_install(Some(&text), Some(default.clone())), Some(default.clone()), "a folder, but no install in it");
        with_install(&registered);
        assert_eq!(existing_install(Some(&text), Some(default.clone())), Some(registered.clone()));
        assert_eq!(existing_install(Some(&format!("  {text}\\ ")), Some(default.clone())), Some(PathBuf::from(format!("{text}\\"))), "as the registry may have it");
        // Nothing registered, or nothing that means anything: the default, as before.
        for nothing in [None, Some(""), Some("  "), Some("relative\\folder")] {
            assert_eq!(existing_install(nothing, Some(default.clone())), Some(default.clone()), "{nothing:?}");
        }
        assert_eq!(existing_install(None, None), None);
        assert_eq!(existing_install(Some(&text), None), Some(registered), "the registered one needs no default");
    }
}
