//! What this app asks of the Windows it runs on, and what it may find different from the machine it was built on.

use std::path::{Path, PathBuf};

use crate::lang::tf;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A debug build told to with `DOGMIND_TEST_KEYS` keeps out of the real user's registry, known folders and AppContainer profiles: walking the screens with a fake data
/// folder must see what the fake says. A release build cannot be told this.
pub fn kept_from_the_real_machine() -> bool {
    cfg!(debug_assertions) && std::env::var_os("DOGMIND_TEST_KEYS").is_some()
}

/// The build number of this Windows, as it really is: `GetVersionEx` lies to a program not manifested for it.
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

/// Windows 11 began at this build, and it is all this app is made for.
pub fn is_supported(build: u32) -> bool {
    build >= 22000
}

/// A variable as the registry has it now, as a program started before it was set does not see it: expanded, the person's own over the machine's.
fn registry_variable(name: &str) -> Vec<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ};
    let (user, system) = (wide("Environment"), wide("SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment"));
    let value = wide(name);
    let mut found = Vec::new();
    for (root, key) in [(HKEY_CURRENT_USER, &user), (HKEY_LOCAL_MACHINE, &system)] {
        let mut bytes = 0u32;
        // SAFETY: the names end in zeros, and the buffer is as long as the system says it must be.
        unsafe {
            if RegGetValueW(root, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ, std::ptr::null_mut(), std::ptr::null_mut(), &mut bytes) != 0 {
                continue;
            }
            let mut buffer = vec![0u16; bytes as usize / 2 + 1];
            if RegGetValueW(root, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ, std::ptr::null_mut(), buffer.as_mut_ptr().cast(), &mut bytes) == 0 {
                let length = buffer.iter().position(|&unit| unit == 0).unwrap_or(buffer.len());
                found.push(String::from_utf16_lossy(&buffer[..length]));
            }
        }
    }
    found
}

/// A variable of the person's that was set after this program started: closing the window does not end it, so "start it again" is not what a person will do.
pub fn fresh_variable(name: &str) -> Option<String> {
    if let Ok(value) = std::env::var(name) {
        return Some(value);
    }
    if kept_from_the_real_machine() {
        return None;
    }
    registry_variable(name).into_iter().next().filter(|value| !value.is_empty())
}

/// What the registry's PATH has that this process's has not, on the end of it: something installed since this program started is found.
fn with_new_folders(path: &str, registry: &[String]) -> String {
    let mut all = path.to_string();
    let mut known: Vec<String> = path.split(';').map(|folder| folder.trim_end_matches('\\').to_lowercase()).collect();
    for folder in registry.iter().flat_map(|variable| variable.split(';')).filter(|folder| !folder.trim().is_empty()) {
        let key = folder.trim_end_matches('\\').to_lowercase();
        if !known.contains(&key) {
            known.push(key);
            all.push(';');
            all.push_str(folder);
        }
    }
    all
}

pub fn refresh_path() {
    if kept_from_the_real_machine() {
        return;
    }
    let path = std::env::var("PATH").unwrap_or_default();
    let fresh = with_new_folders(&path, &registry_variable("Path"));
    if fresh != path {
        std::env::set_var("PATH", fresh);
    }
}

/// The free bytes of the drive `path` is on or would be on.
pub fn free_bytes(path: &Path) -> Option<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let existing = path.ancestors().find(|folder| folder.is_dir())?;
    let name: Vec<u16> = existing.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
    let mut free = 0u64;
    // SAFETY: the name ends in a zero, and the out-parameter lives through the call.
    (unsafe { GetDiskFreeSpaceExW(name.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) } != 0).then_some(free)
}

/// What is known of a folder before anything is put in it. For the words below, which are the whole of what follows from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderFacts {
    pub file_system: String,
    pub keeps_permissions: bool,
    pub remote: bool,
    /// Whether what is there, or the folder above where it would be made, can take a new file. Not known counts as yes: the folder may only be slow to answer.
    pub can_write: bool,
    /// Whether its permissions can be changed: what a sandbox needs of a folder. A folder of another account's, or an administrator's, cannot.
    pub can_change_permissions: bool,
    /// The folder as it is called, for the words.
    pub shown: String,
}

fn folder_facts(folder: &Path) -> FolderFacts {
    let shown = folder.display().to_string();
    let Some(volume) = dogmind_agent::sandbox::volume_of(folder) else { return FolderFacts { keeps_permissions: true, can_write: true, can_change_permissions: true, shown, ..FolderFacts::default() } };
    let existing = folder.ancestors().find(|place| place.is_dir());
    // (a folder made here is the person's own, whoever owns the one above it)
    let (can_write, can_change_permissions) = match existing.filter(|_| !volume.remote).and_then(dogmind_agent::sandbox::rights_on) {
        Some((write, change)) => (write, change || existing != Some(folder)),
        None => (true, true),
    };
    FolderFacts { file_system: volume.file_system, keeps_permissions: volume.keeps_permissions, remote: volume.remote, can_write, can_change_permissions, shown }
}

/// Why the model cannot go in this folder, in one line, before anything is downloaded. What the engine does there is read: its worker runs with no network, so it cannot open a file on
/// another machine; FAT32 holds no file of 4 GB or more; exFAT has no access lists to grant on, and none in the way, so it works; the worker's access is granted by the engine on the
/// folder, which takes the right to change it, unless every container can read it already.
pub fn model_folder_refusal(facts: &FolderFacts, open_to_containers: bool) -> Option<String> {
    if facts.remote {
        return Some(tf("{0} is on a network drive, and the engine has no network to read a model by. Choose a folder on a drive of this PC.", &[&facts.shown]));
    }
    if facts.file_system.to_uppercase().starts_with("FAT") {
        return Some(tf("{0} is on a FAT32 drive, which cannot hold a file of 4 GB or more, and a model's files are far larger. Choose a folder on an NTFS or exFAT drive.", &[&facts.shown]));
    }
    if !facts.can_write {
        return Some(tf("You cannot write in {0}. Choose a folder of your own.", &[&facts.shown]));
    }
    (facts.keeps_permissions && !facts.can_change_permissions && !open_to_containers).then(|| tf("{0} belongs to another account, so the engine could not be let read a model there. Choose a folder of your own.", &[&facts.shown]))
}

pub fn model_folder_problem(folder: &Path) -> Option<String> {
    if kept_from_the_real_machine() && std::env::var_os("DOGMIND_TEST_FOLDER_FACTS").is_some() {
        return model_folder_refusal(&test_facts(folder), false);
    }
    let facts = folder_facts(folder);
    // (asked only of a folder that exists: it is what the worker's grant would have to pass)
    let open = facts.can_change_permissions || !folder.is_dir() || dogmind_agent::sandbox::open_to_every_container(folder);
    model_folder_refusal(&facts, open)
}

/// A walk through the screens has no FAT32 drive or network share at hand: `DOGMIND_TEST_FOLDER_FACTS` is what a debug build told to keep from the machine takes them to be.
fn test_facts(folder: &Path) -> FolderFacts {
    let said = std::env::var("DOGMIND_TEST_FOLDER_FACTS").unwrap_or_default();
    let has = |word: &str| said.split(',').any(|part| part == word);
    FolderFacts {
        file_system: said.split(',').find(|part| part.starts_with("FAT") || part.eq_ignore_ascii_case("exFAT") || *part == "NTFS").unwrap_or("NTFS").to_string(),
        keeps_permissions: !said.split(',').any(|part| part.to_uppercase().contains("FAT")),
        remote: has("remote"),
        can_write: !has("readonly"),
        can_change_permissions: !has("other"),
        shown: folder.display().to_string(),
    }
}

/// What a person should know of a workspace they have just chosen, in a line: it still reads and edits there, whatever this says. A cloud folder is only a warning.
pub fn workspace_words(facts: &FolderFacts, cloud: bool) -> Option<String> {
    if facts.remote {
        return Some(tf("{0} is on a network drive. Commands cannot run there, as the sandbox has no network; files can still be read and edited.", &[&facts.shown]));
    }
    if !facts.keeps_permissions {
        return Some(tf("{0} is on a drive with no file permissions ({1}). Commands cannot be contained there, so they do not run; files can still be read and edited.", &[&facts.shown, &facts.file_system]));
    }
    if !facts.can_change_permissions {
        return Some(tf("{0} belongs to another account. Commands need its permissions changed, so they may not run; a folder of your own is safer.", &[&facts.shown]));
    }
    cloud.then(|| tf("{0} is kept by OneDrive or another cloud, with files that may be online only. A command in the sandbox cannot bring those down and sees them as missing, and every build gets synced: a folder outside it works better.", &[&facts.shown]))
}

pub fn workspace_notice(folder: &Path) -> Option<String> {
    if kept_from_the_real_machine() && std::env::var_os("DOGMIND_TEST_FOLDER_FACTS").is_some() {
        let said = std::env::var("DOGMIND_TEST_FOLDER_FACTS").unwrap_or_default();
        return workspace_words(&test_facts(folder), said.split(',').any(|part| part == "cloud"));
    }
    let facts = folder_facts(folder);
    // (not on a share, whose every look can take long)
    let cloud = !facts.remote && (in_a_cloud_root(folder, &cloud_roots()) || holds_placeholders(folder));
    workspace_words(&facts, cloud)
}

/// Where OneDrive says it keeps this person's files.
fn cloud_roots() -> Vec<PathBuf> {
    ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"].iter().filter_map(|name| std::env::var_os(name)).filter(|value| !value.is_empty()).map(PathBuf::from).collect()
}

fn in_a_cloud_root(folder: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| folder.starts_with(root))
}

/// These attributes are how a cloud provider's file says its content may not be here: RECALL_ON_OPEN, PINNED, UNPINNED, RECALL_ON_DATA_ACCESS, OFFLINE.
fn is_placeholder(attributes: u32) -> bool {
    attributes & (0x0004_0000 | 0x0008_0000 | 0x0010_0000 | 0x0040_0000 | 0x0000_1000) != 0
}

/// The folder or the first things in it, without walking its tree: enough to tell, on a folder of any size.
fn holds_placeholders(folder: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const LOOKED_AT: usize = 256;
    if std::fs::metadata(folder).is_ok_and(|data| is_placeholder(data.file_attributes())) {
        return true;
    }
    std::fs::read_dir(folder).into_iter().flatten().flatten().take(LOOKED_AT).any(|entry| entry.metadata().is_ok_and(|data| is_placeholder(data.file_attributes())))
}

/// Where the person's Documents really are: moved by OneDrive or a policy, they are not under the home folder.
fn known_documents() -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Documents, SHGetKnownFolderPath};
    if kept_from_the_real_machine() {
        return None;
    }
    let mut text = std::ptr::null_mut();
    // SAFETY: the system gives a string that ends in a zero, for the caller to free.
    unsafe {
        if SHGetKnownFolderPath(&FOLDERID_Documents, 0, std::ptr::null_mut(), &mut text) != 0 || text.is_null() {
            return None;
        }
        let length = (0..).take_while(|&at| *text.add(at) != 0).count();
        let path = PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(text, length)));
        CoTaskMemFree(text.cast());
        Some(path)
    }
}

/// Where a first session can begin: the person's Documents. Not ones that a cloud syncs, which would carry every build and `node_modules` with them and hold files back on
/// demand from the sandbox: the home folder then, and not a `Documents` that Explorer no longer shows.
fn first_folder_in(home: &Path, documents: Option<&Path>, cloud: Option<&Path>) -> PathBuf {
    let Some(documents) = documents else { return home.join("Documents").join("Dogmind") };
    let in_the_cloud = documents.to_string_lossy().starts_with("\\\\") || cloud.is_some_and(|cloud| documents.starts_with(cloud)) || documents.components().any(|part| part.as_os_str().to_string_lossy().to_lowercase().starts_with("onedrive"));
    if in_the_cloud {
        home.join("Dogmind")
    } else {
        documents.join("Dogmind")
    }
}

pub fn first_folder(data: &Path) -> PathBuf {
    match std::env::var_os("USERPROFILE") {
        Some(home) => first_folder_in(Path::new(&home), known_documents().as_deref(), std::env::var_os("OneDrive").as_deref().map(Path::new)),
        None => data.join("workspace"),
    }
}

/// The AppContainer profile this app made. The ACEs that name it stay on what they were put on, and name no one once the profile has gone.
/// Not Straydog's worker: the engine runs as a server of its own too, with that same profile, and it is Straydog's to take.
pub fn delete_profiles() {
    use windows_sys::Win32::Security::Isolation::DeleteAppContainerProfile;
    if kept_from_the_real_machine() {
        return;
    }
    for name in ["DogmindApp.Exec"] {
        // SAFETY: the name ends in a zero.
        unsafe { DeleteAppContainerProfile(wide(name).as_ptr()) };
    }
}

/// Whether Windows counts the connection by the amount: set as metered by the person, or one it knows is limited, over its limit or roaming. The app's own downloads of any size wait
/// for the person on such a connection. What Windows cannot say, or a PC with no connection to say it of, is not metered: the request itself says that it cannot be made.
/// It is asked on the window's thread, which has COM as an apartment for good: a thread of its own that began one, for this alone, would end it with the thread, and the classes this
/// asks for are kept for the whole process in the apartment of the first that asked.
pub fn connection_is_metered() -> bool {
    use windows::Networking::Connectivity::NetworkInformation;
    let cost = NetworkInformation::GetInternetConnectionProfile().ok().and_then(|profile| profile.GetConnectionCost().ok());
    cost.and_then(|cost| Some(counts_by_the_amount(cost.NetworkCostType().ok()?, cost.Roaming().unwrap_or(false), cost.OverDataLimit().unwrap_or(false), cost.ApproachingDataLimit().unwrap_or(false)))).unwrap_or(false)
}

/// Fixed (a limit of data) and variable (paid by the amount) are metered, and so is anything that is roaming or at its limit.
fn counts_by_the_amount(kind: windows::Networking::Connectivity::NetworkCostType, roaming: bool, over_limit: bool, near_limit: bool) -> bool {
    use windows::Networking::Connectivity::NetworkCostType;
    matches!(kind, NetworkCostType::Fixed | NetworkCostType::Variable) || roaming || over_limit || near_limit
}

/// The day a build was made, as `2026-11-04`, from what `build.rs` gives: its text ends in the seconds since 1970.
pub fn build_day(build: &str) -> Option<String> {
    let seconds = build.rsplit(' ').next().and_then(|last| last.parse::<i64>().ok())?;
    // Days to a date in the proleptic Gregorian calendar, counted from 0000-03-01.
    let z = seconds.div_euclid(86400) + 719468;
    let era = z.div_euclid(146097);
    let day_of_era = z.rem_euclid(146097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    Some(format!("{year}-{month:02}-{day:02}"))
}

/// The build as `/status` says it: which commit, and the day. What `build.rs` gives ends in the seconds since 1970.
pub fn readable_build(build: &str) -> String {
    let commit = build.split_whitespace().next().unwrap_or("").trim_end_matches(',');
    match build_day(build) {
        Some(day) => format!("{commit} ({day})"),
        None => commit.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_is_told_by_its_commit_and_the_day() {
        assert_eq!(build_day("b41c461656, built at 1793796827").as_deref(), Some("2026-11-04"));
        assert_eq!(build_day("abc built at 0").as_deref(), Some("1970-01-01"));
        assert_eq!(build_day("abc"), None);
        assert_eq!(readable_build("056bbaaa1a with changes, built at 1793616313"), "056bbaaa1a (2026-11-02)");
        assert_eq!(readable_build("abc"), "abc");
    }

    #[test]
    fn a_connection_that_is_paid_by_the_amount_or_at_its_limit_is_metered() {
        use windows::Networking::Connectivity::NetworkCostType;
        assert!(counts_by_the_amount(NetworkCostType::Fixed, false, false, false));
        assert!(counts_by_the_amount(NetworkCostType::Variable, false, false, false));
        assert!(counts_by_the_amount(NetworkCostType::Unrestricted, true, false, false), "roaming");
        assert!(counts_by_the_amount(NetworkCostType::Unrestricted, false, true, false), "over its limit");
        assert!(counts_by_the_amount(NetworkCostType::Unknown, false, false, true), "near its limit");
        assert!(!counts_by_the_amount(NetworkCostType::Unrestricted, false, false, false));
        assert!(!counts_by_the_amount(NetworkCostType::Unknown, false, false, false), "what Windows cannot say is not metered");
    }

    #[test]
    fn this_windows_is_read_and_the_baseline_is_eleven() {
        assert!(windows_build().is_some_and(|build| build > 10000));
        assert!(is_supported(22000) && is_supported(26300));
        assert!(!is_supported(19045));
    }

    #[test]
    fn a_folder_installed_since_is_added_once_at_the_end() {
        let registry = ["C:\\new\\;C:\\Windows".to_string(), "c:\\NEW;D:\\other".to_string()];
        assert_eq!(with_new_folders("C:\\Windows;C:\\a", &registry), "C:\\Windows;C:\\a;C:\\new\\;D:\\other");
        assert_eq!(with_new_folders("C:\\a", &[]), "C:\\a");
    }

    #[test]
    fn a_first_folder_keeps_out_of_the_cloud() {
        let home = Path::new("C:\\Users\\me");
        assert_eq!(first_folder_in(home, Some(Path::new("C:\\Users\\me\\Documents")), None), Path::new("C:\\Users\\me\\Documents\\Dogmind"));
        assert_eq!(first_folder_in(home, Some(Path::new("D:\\Docs")), None), Path::new("D:\\Docs\\Dogmind"));
        assert_eq!(first_folder_in(home, Some(Path::new("C:\\Users\\me\\OneDrive\\Documents")), None), Path::new("C:\\Users\\me\\Dogmind"));
        assert_eq!(first_folder_in(home, Some(Path::new("C:\\Users\\me\\OneDrive - Acme\\ドキュメント")), None), Path::new("C:\\Users\\me\\Dogmind"));
        assert_eq!(first_folder_in(home, Some(Path::new("E:\\Sync\\Docs")), Some(Path::new("E:\\Sync"))), Path::new("C:\\Users\\me\\Dogmind"));
        assert_eq!(first_folder_in(home, Some(Path::new("\\\\server\\me\\Docs")), None), Path::new("C:\\Users\\me\\Dogmind"), "a network share runs no sandbox");
        assert_eq!(first_folder_in(home, None, None), Path::new("C:\\Users\\me\\Documents\\Dogmind"));
    }

    fn facts(file_system: &str) -> FolderFacts {
        FolderFacts { file_system: file_system.into(), keeps_permissions: file_system == "NTFS", can_write: true, can_change_permissions: true, shown: "E:\\m".into(), ..FolderFacts::default() }
    }

    #[test]
    fn a_model_is_refused_where_it_cannot_work_and_only_there() {
        assert_eq!(model_folder_refusal(&facts("NTFS"), false), None);
        assert_eq!(model_folder_refusal(&facts("exFAT"), false), None, "no 4 GB limit, and no access lists in the worker's way");
        for name in ["FAT32", "FAT", "fat32"] {
            assert!(model_folder_refusal(&facts(name), false).unwrap().contains("4 GB"), "{name}");
        }
        assert!(model_folder_refusal(&FolderFacts { remote: true, ..facts("NTFS") }, false).unwrap().contains("network"));
        assert!(model_folder_refusal(&FolderFacts { can_write: false, ..facts("NTFS") }, false).unwrap().contains("cannot write"));
        let others = FolderFacts { can_change_permissions: false, ..facts("NTFS") };
        assert!(model_folder_refusal(&others, false).unwrap().contains("another account"));
        assert_eq!(model_folder_refusal(&others, true), None, "every container can read it already: nothing need be granted");
    }

    #[test]
    fn a_workspace_says_what_will_not_work_there_and_goes_on() {
        assert_eq!(workspace_words(&facts("NTFS"), false), None);
        assert!(workspace_words(&FolderFacts { remote: true, ..facts("NTFS") }, false).unwrap().contains("network"));
        let exfat = workspace_words(&facts("exFAT"), false).unwrap();
        assert!(exfat.contains("exFAT") && exfat.contains("still be read and edited"), "{exfat}");
        assert!(workspace_words(&FolderFacts { can_change_permissions: false, ..facts("NTFS") }, false).unwrap().contains("another account"));
        assert!(workspace_words(&facts("NTFS"), true).unwrap().contains("OneDrive"));
        assert!(workspace_words(&facts("exFAT"), true).unwrap().contains("no file permissions"), "what stops commands comes first");
    }

    #[test]
    fn a_cloud_folder_is_told_by_its_files_or_its_place() {
        assert!(in_a_cloud_root(Path::new("C:\\Users\\me\\OneDrive\\work"), &[PathBuf::from("C:\\Users\\me\\OneDrive")]));
        assert!(!in_a_cloud_root(Path::new("C:\\Users\\me\\OneDriveOld\\work"), &[PathBuf::from("C:\\Users\\me\\OneDrive")]));
        for online_only in [0x0040_0000, 0x0004_0000, 0x0010_0000, 0x0008_0000] {
            assert!(is_placeholder(online_only | 0x20));
        }
        assert!(!is_placeholder(0x20) && !is_placeholder(0x10));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        assert!(!holds_placeholders(dir.path()));
    }

    #[test]
    fn this_machines_folders_read_as_they_are() {
        let dir = tempfile::tempdir().unwrap();
        let found = folder_facts(&dir.path().join("not").join("yet"));
        assert!(!found.remote && found.can_write && found.can_change_permissions && !found.file_system.is_empty(), "{found:?}");
        assert_eq!(model_folder_refusal(&found, false), None);
        // The system's own folder is another's to change, here as on any PC.
        let windows = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        let theirs = folder_facts(&windows);
        assert!(!theirs.can_write && !theirs.can_change_permissions, "{theirs:?}");
        assert!(model_folder_refusal(&theirs, false).is_some());
    }

    #[test]
    fn free_space_is_read_from_the_nearest_folder_that_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_bytes(&dir.path().join("not").join("yet")).is_some_and(|free| free > 0));
    }
}
