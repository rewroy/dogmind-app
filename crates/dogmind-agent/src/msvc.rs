//! What a Developer prompt for x64 gives a build, for commands that cannot find Visual Studio themselves. Rust finds MSVC through the Visual Studio setup configuration (COM and
//! `C:\ProgramData`), which a container cannot reach, and `cl` and `link` are on no PATH of a person's unless they started from a Developer prompt. The files are readable, so only the
//! variables are missing: found outside the sandbox, in the background, and given to a command as `LIB`, `INCLUDE` and the tool folders at the end of its PATH. What is found, and that
//! there is none to find, stay for the process; a look that ran out of time does not.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// `vswhere` is a small program that reads a few files; when it takes longer than this, something is wrong and a build goes on without.
const VSWHERE_LIMIT: Duration = Duration::from_secs(10);
/// A look that ran out of time (a virus scan or Windows Update has `vswhere` waiting) is made again on a later call, but not sooner than this after it ended.
const LOOK_AGAIN_AFTER: Duration = Duration::from_secs(60);
/// How often a wait for a look asks whether the command it is for has been stopped.
const ASK_STOP_EVERY: Duration = Duration::from_millis(25);
/// The Visual Studio component that has `cl.exe`, `link.exe` and the libraries for x64.
const CPP_TOOLS: &str = "Microsoft.VisualStudio.Component.VC.Tools.x86.x64";

/// Folders of a Visual Studio C++ toolset and a Windows SDK, as a Developer prompt for x64 would use them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildTools {
    /// For `LIB`: the toolset's, then the SDK's.
    pub lib: Vec<PathBuf>,
    /// For `INCLUDE`: the toolset's, then the SDK's.
    pub include: Vec<PathBuf>,
    /// The toolset's `bin` (`cl.exe`, `link.exe`), then the SDK's (`rc.exe`): for the end of PATH.
    pub path: Vec<PathBuf>,
}

/// Only x64 hosts: on arm64 the folders would be another set, and no build is better than one that links the wrong thing.
fn is_x64(architecture: &str) -> bool {
    architecture.eq_ignore_ascii_case("AMD64")
}

/// A version as numbers, for ordering: `10.0.9600.0` comes before `10.0.22621.0`, as it does not as text. A name that is not one is no version.
fn version_numbers(name: &str) -> Option<Vec<u64>> {
    name.split('.').map(|part| part.parse().ok()).collect()
}

/// The folders of one Visual Studio installation and one Windows SDK root, when all of them are there. Takes the toolset the installation names as its default, and the newest SDK
/// that has everything: a half-installed newer one is passed over, and what is taken is all of one version.
pub fn layout(architecture: &str, install: &Path, kits: &Path) -> Option<BuildTools> {
    if !is_x64(architecture) {
        return None;
    }
    let named = std::fs::read_to_string(install.join("VC\\Auxiliary\\Build\\Microsoft.VCToolsVersion.default.txt")).ok()?;
    let version = named.trim();
    // (a name from a file, joined to a path: a version, not a way out of the folder)
    if version_numbers(version).is_none() {
        return None;
    }
    let toolset = install.join("VC\\Tools\\MSVC").join(version);
    let (bin, lib, include) = (toolset.join("bin\\Hostx64\\x64"), toolset.join("lib\\x64"), toolset.join("include"));
    if ![&bin, &lib, &include].iter().all(|folder| folder.is_dir()) {
        return None;
    }
    let mut versions: Vec<(Vec<u64>, String)> = std::fs::read_dir(kits.join("Lib")).ok()?.flatten().filter_map(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        Some((version_numbers(&name)?, name))
    }).collect();
    versions.sort_by(|a, b| b.cmp(a));
    versions.into_iter().find_map(|(_, sdk)| {
        let (um_lib, ucrt_lib) = (kits.join("Lib").join(&sdk).join("um\\x64"), kits.join("Lib").join(&sdk).join("ucrt\\x64"));
        let includes: Vec<PathBuf> = ["ucrt", "um", "shared", "winrt"].iter().map(|part| kits.join("Include").join(&sdk).join(part)).collect();
        let sdk_bin = kits.join("bin").join(&sdk).join("x64");
        let complete = um_lib.join("kernel32.lib").is_file() && ucrt_lib.is_dir() && sdk_bin.is_dir() && includes.iter().all(|folder| folder.is_dir());
        complete.then(|| BuildTools {
            lib: vec![lib.clone(), um_lib, ucrt_lib],
            include: [vec![include.clone()], includes].concat(),
            path: vec![bin.clone(), sdk_bin],
        })
    })
}

/// A list of folders as a variable holds it. None when one cannot be written in one (a quote), which no folder of an install has.
fn joined(folders: &[PathBuf]) -> Option<String> {
    std::env::join_paths(folders).ok().map(|joined| joined.to_string_lossy().into_owned())
}

/// Puts what `find` returns in a command's environment: `LIB` and `INCLUDE`, and the tool folders after everything else on PATH, so that nothing a person has is shadowed and the
/// tools are found when nothing else has them. Nothing when the environment already has a `LIB`, which is a person's Developer prompt; `find` is not asked then.
pub fn add_to(block: &mut Vec<(String, String)>, find: impl FnOnce() -> Option<BuildTools>) {
    let value_of = |block: &[(String, String)], name: &str| block.iter().find(|(known, _)| known.eq_ignore_ascii_case(name)).map(|(_, value)| value.clone());
    if value_of(block, "LIB").is_some_and(|lib| !lib.is_empty()) {
        return;
    }
    let Some(tools) = find() else { return };
    let (Some(lib), Some(include), Some(tail)) = (joined(&tools.lib), joined(&tools.include), joined(&tools.path)) else { return };
    // (an INCLUDE without a LIB is the person's own for something else: theirs stays, after these)
    let include = match value_of(block, "INCLUDE").filter(|own| !own.is_empty()) {
        Some(own) => format!("{include};{own}"),
        None => include,
    };
    let path = match value_of(block, "PATH").filter(|own| !own.is_empty()) {
        Some(own) if own.ends_with(';') => format!("{own}{tail}"),
        Some(own) => format!("{own};{tail}"),
        None => tail,
    };
    block.retain(|(name, _)| !["LIB", "INCLUDE", "PATH"].iter().any(|set| name.eq_ignore_ascii_case(set)));
    block.extend([("LIB".to_string(), lib), ("INCLUDE".to_string(), include), ("PATH".to_string(), path)]);
}

/// What a look came to: an answer that stands, or none yet (the program it asks ran out of time, or could not be run).
enum Looked<T> {
    Settled(T),
    Unsettled,
}

/// What is known of the build tools, and whether a look for them is going on.
struct Remembered {
    /// What a look settled, for the process: the tools, or `None` for a machine that has none (no Visual Studio with the C++ tools, no `vswhere`, not x64). Not known yet: `None` out here.
    settled: Option<Option<BuildTools>>,
    /// A look is going on, on a thread of its own.
    looking: bool,
    /// When the last look ended without settling anything.
    unsettled_at: Option<Instant>,
}

/// The build tools of this machine, found by a look that runs in the background. No one waits on it unless they say so: the gate, which weighs a command before the person is shown it,
/// takes what is known, and a command's environment waits for a look that is going on for as long as the command has time.
struct Finder {
    remembered: Mutex<Remembered>,
    /// Told when a look ends.
    ended: Condvar,
    /// How long after a look that settled nothing the next one may begin.
    again_after: Duration,
}

impl Finder {
    const fn new(again_after: Duration) -> Finder {
        Finder { remembered: Mutex::new(Remembered { settled: None, looking: false, unsettled_at: None }), ended: Condvar::new(), again_after }
    }

    fn lock(&self) -> MutexGuard<'_, Remembered> {
        self.remembered.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Begins a look on a thread of its own, when one is wanted: not when the answer is known, a look is going on, or the last one ended without settling less than `again_after` ago.
    fn begin(&'static self, remembered: &mut Remembered, look: impl FnOnce() -> Looked<Option<BuildTools>> + Send + 'static) {
        let may_try_again = remembered.unsettled_at.is_none_or(|at| at.elapsed() >= self.again_after);
        if remembered.settled.is_some() || remembered.looking || !may_try_again {
            return;
        }
        let spawned = std::thread::Builder::new().name("build tools".to_string()).spawn(move || {
            // (a look that panics settles nothing, and does not leave the others waiting for it)
            let looked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(look)).unwrap_or(Looked::Unsettled);
            let mut remembered = self.lock();
            remembered.looking = false;
            match looked {
                Looked::Settled(tools) => remembered.settled = Some(tools),
                Looked::Unsettled => remembered.unsettled_at = Some(Instant::now()),
            }
            drop(remembered);
            self.ended.notify_all();
        });
        // (the thread takes the lock for its end, which this call holds until it returns)
        remembered.looking = spawned.is_ok();
        if spawned.is_err() {
            remembered.unsettled_at = Some(Instant::now());
        }
    }

    fn look_ahead(&'static self, look: impl FnOnce() -> Looked<Option<BuildTools>> + Send + 'static) {
        self.begin(&mut self.lock(), look);
    }

    /// What is known now, and never a wait: the tools when a look has found them, and otherwise none. A look that is wanted begins.
    fn known(&'static self, look: impl FnOnce() -> Looked<Option<BuildTools>> + Send + 'static) -> Option<BuildTools> {
        let mut remembered = self.lock();
        self.begin(&mut remembered, look);
        remembered.settled.clone().flatten()
    }

    /// The tools, waiting for a look that is going on (or begins here) until `deadline`, and not past the moment `stop` is set. None: the machine has none, or the look did not settle in that time.
    fn found_before(&'static self, look: impl FnOnce() -> Looked<Option<BuildTools>> + Send + 'static, deadline: Instant, stop: &AtomicBool) -> Option<BuildTools> {
        let mut remembered = self.lock();
        self.begin(&mut remembered, look);
        loop {
            if let Some(settled) = &remembered.settled {
                return settled.clone();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if !remembered.looking || left.is_zero() || stop.load(Ordering::Relaxed) {
                return None;
            }
            remembered = self.ended.wait_timeout(remembered, left.min(ASK_STOP_EVERY)).unwrap_or_else(PoisonError::into_inner).0;
        }
    }
}

static FINDER: Finder = Finder::new(LOOK_AGAIN_AFTER);

/// Begins the look for the build tools in the background, if it is wanted and has not begun, and returns at once. Asked early (when an agent starts, and when a command is about to run), so that
/// a command's environment usually finds the answer ready.
pub fn look_ahead() {
    FINDER.look_ahead(look);
}

/// The build tools as far as they are known, without waiting for anything: the gate weighs a command before the person is shown it, and nothing is on the screen to tell them what it waits for.
/// Not found yet reads as none; the look begins here if it has not.
pub fn known() -> Option<BuildTools> {
    FINDER.known(look)
}

/// The build tools for a command's environment: waits for a look that is going on, until `deadline` (the end of the time the command has) and not past the moment `stop` is set, which is
/// the person's Stop. A command goes on without them when the look has not come to anything by then.
pub fn found_before(deadline: Instant, stop: &AtomicBool) -> Option<BuildTools> {
    FINDER.found_before(look, deadline, stop)
}

fn look() -> Looked<Option<BuildTools>> {
    let Some(architecture) = std::env::var("PROCESSOR_ARCHITECTURE").ok().filter(|architecture| is_x64(architecture)) else { return Looked::Settled(None) };
    let Some(program_files) = std::env::var_os("ProgramFiles(x86)").map(PathBuf::from) else { return Looked::Settled(None) };
    let install = match installation(&program_files) {
        Looked::Settled(Some(install)) => install,
        Looked::Settled(None) => return Looked::Settled(None),
        Looked::Unsettled => return Looked::Unsettled,
    };
    // (the registry's root first; where the SDK puts itself by default, when it has none or the one it names is not what was wanted)
    let kits = kits_root().into_iter().chain([program_files.join("Windows Kits\\10")]);
    Looked::Settled(kits.into_iter().find_map(|kits| layout(&architecture, &install, &kits)))
}

/// The newest Visual Studio with the C++ tools, as its own `vswhere` says. Settled as none when `vswhere` is not there or has found no installation; not settled when it ran out of time or
/// could not be run, as that says nothing of what is installed.
fn installation(program_files: &Path) -> Looked<Option<PathBuf>> {
    let vswhere = program_files.join("Microsoft Visual Studio\\Installer\\vswhere.exe");
    if !vswhere.is_file() {
        return Looked::Settled(None);
    }
    let mut command = std::process::Command::new(vswhere);
    command.args(["-latest", "-products", "*", "-requires", CPP_TOOLS, "-property", "installationPath", "-utf8"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    crate::tools::no_window(&mut command);
    match output_within(command, VSWHERE_LIMIT) {
        Ran::Said(said) => Looked::Settled(said.lines().map(str::trim).find(|line| !line.is_empty()).map(PathBuf::from).filter(|folder| folder.is_dir())),
        Ran::Failed | Ran::TooSlow => Looked::Unsettled,
    }
}

/// How a program that is run for what it prints came out.
#[derive(Debug, PartialEq, Eq)]
enum Ran {
    /// It ended well within the limit, and this is all it printed.
    Said(String),
    /// It could not be run, or ended with a status that is not success.
    Failed,
    /// It did not end within the limit, or what it printed did not: a program it started may keep its output open.
    TooSlow,
}

/// What the program prints, if it and its output end within the limit. Past it, the program is ended, and a reader that is still reading is left to end by itself when the output closes.
fn output_within(mut command: std::process::Command, limit: Duration) -> Ran {
    use std::io::Read;
    let Ok(mut child) = command.spawn() else { return Ran::Failed };
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Ran::Failed;
    };
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        let _ = send.send(bytes);
    });
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            // (the output closes when the program ends, unless a program it started holds it open: that one is not waited for past the limit)
            Ok(Some(status)) => {
                return match receive.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(bytes) if status.success() => Ran::Said(String::from_utf8_lossy(&bytes).into_owned()),
                    Ok(_) => Ran::Failed,
                    Err(_) => Ran::TooSlow,
                };
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            outcome => {
                let _ = child.kill();
                let _ = child.wait();
                return if outcome.is_err() { Ran::Failed } else { Ran::TooSlow };
            }
        }
    }
}

/// Where the Windows SDK says it is: `KitsRoot10`, in the 64-bit view of the registry and then the 32-bit one, which is where its installer writes.
fn kits_root() -> Option<PathBuf> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6432KEY};
    let wide = |text: &str| -> Vec<u16> { text.encode_utf16().chain([0]).collect() };
    let (key, value) = (wide("SOFTWARE\\Microsoft\\Windows Kits\\Installed Roots"), wide("KitsRoot10"));
    for view in [0, RRF_SUBKEY_WOW6432KEY] {
        let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | view;
        let mut bytes = 0u32;
        // SAFETY: the names end in zeros, and the buffer is as long as the system says it must be.
        let text = unsafe {
            if RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value.as_ptr(), flags, std::ptr::null_mut(), std::ptr::null_mut(), &mut bytes) != 0 {
                continue;
            }
            let mut buffer = vec![0u16; bytes as usize / 2 + 1];
            if RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value.as_ptr(), flags, std::ptr::null_mut(), buffer.as_mut_ptr().cast(), &mut bytes) != 0 {
                continue;
            }
            let length = buffer.iter().position(|&unit| unit == 0).unwrap_or(buffer.len());
            String::from_utf16_lossy(&buffer[..length])
        };
        if !text.is_empty() {
            return Some(PathBuf::from(text));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    /// An installation and a Windows SDK as they are laid out on disk: a toolset version (the default one named in the file), and SDK versions that each have all of their parts.
    fn machine(root: &Path, toolsets: &[&str], default: &str, sdks: &[&str]) -> (PathBuf, PathBuf) {
        let (install, kits) = (root.join("VS\\2022\\BuildTools"), root.join("Windows Kits\\10"));
        std::fs::create_dir_all(install.join("VC\\Auxiliary\\Build")).unwrap();
        std::fs::write(install.join("VC\\Auxiliary\\Build\\Microsoft.VCToolsVersion.default.txt"), format!("{default}\r\n")).unwrap();
        for toolset in toolsets {
            for part in ["bin\\Hostx64\\x64", "lib\\x64", "include"] {
                std::fs::create_dir_all(install.join("VC\\Tools\\MSVC").join(toolset).join(part)).unwrap();
            }
        }
        for sdk in sdks {
            add_sdk(&kits, sdk);
        }
        (install, kits)
    }

    fn add_sdk(kits: &Path, sdk: &str) {
        for part in ["Lib\\{}\\um\\x64", "Lib\\{}\\ucrt\\x64", "Include\\{}\\ucrt", "Include\\{}\\um", "Include\\{}\\shared", "Include\\{}\\winrt", "bin\\{}\\x64"] {
            std::fs::create_dir_all(kits.join(part.replace("{}", sdk))).unwrap();
        }
        std::fs::write(kits.join("Lib").join(sdk).join("um\\x64\\kernel32.lib"), "").unwrap();
    }

    fn paths(folders: &[&Path]) -> Vec<PathBuf> {
        folders.iter().map(|folder| folder.to_path_buf()).collect()
    }

    #[test]
    fn the_default_toolset_and_the_newest_sdk_are_taken_by_number() {
        let dir = tempfile::tempdir().unwrap();
        // (9600 is more than 22621 as text; the toolset that is not the default is not the one taken, however new)
        let (install, kits) = machine(dir.path(), &["14.29.30133", "14.44.35207", "14.50.1"], "14.44.35207", &["10.0.9600.0", "10.0.19041.0", "10.0.22621.0"]);
        // (a folder in Lib that is no version)
        std::fs::create_dir_all(kits.join("Lib\\wdf")).unwrap();
        let found = layout("AMD64", &install, &kits).unwrap();
        let (msvc, sdk) = (install.join("VC\\Tools\\MSVC\\14.44.35207"), "10.0.22621.0");
        assert_eq!(found.lib, [msvc.join("lib\\x64"), kits.join("Lib").join(sdk).join("um\\x64"), kits.join("Lib").join(sdk).join("ucrt\\x64")]);
        assert_eq!(found.include, [msvc.join("include"), kits.join("Include").join(sdk).join("ucrt"), kits.join("Include").join(sdk).join("um"), kits.join("Include").join(sdk).join("shared"), kits.join("Include").join(sdk).join("winrt")]);
        assert_eq!(found.path, [msvc.join("bin\\Hostx64\\x64"), kits.join("bin").join(sdk).join("x64")]);
    }

    #[test]
    fn an_sdk_with_a_part_missing_is_passed_over_for_the_newest_that_has_them_all() {
        let dir = tempfile::tempdir().unwrap();
        let (install, kits) = machine(dir.path(), &["14.44.35207"], "14.44.35207", &["10.0.19041.0"]);
        // The newest has a folder of libraries and nothing in it; the next has everything but the SDK's tools.
        std::fs::create_dir_all(kits.join("Lib\\10.0.26100.0\\um\\x64")).unwrap();
        add_sdk(&kits, "10.0.22621.0");
        std::fs::remove_dir_all(kits.join("bin\\10.0.22621.0")).unwrap();
        let found = layout("AMD64", &install, &kits).unwrap();
        let of_the_sdk = [&found.lib[1..], &found.include[1..], &found.path[1..]].concat();
        assert_eq!(of_the_sdk.len(), 2 + 4 + 1);
        assert!(of_the_sdk.iter().all(|folder| folder.to_string_lossy().contains("10.0.19041.0")), "all of one version: {found:?}");
    }

    #[test]
    fn nothing_is_given_when_any_part_is_missing() {
        let parts: [(&str, bool); 12] = [
            ("VC\\Auxiliary\\Build\\Microsoft.VCToolsVersion.default.txt", true),
            ("VC\\Tools\\MSVC\\14.44.35207\\bin\\Hostx64\\x64", false),
            ("VC\\Tools\\MSVC\\14.44.35207\\lib\\x64", false),
            ("VC\\Tools\\MSVC\\14.44.35207\\include", false),
            ("kits:Lib\\10.0.22621.0\\um\\x64\\kernel32.lib", true),
            ("kits:Lib\\10.0.22621.0\\ucrt\\x64", false),
            ("kits:Include\\10.0.22621.0\\ucrt", false),
            ("kits:Include\\10.0.22621.0\\um", false),
            ("kits:Include\\10.0.22621.0\\shared", false),
            ("kits:Include\\10.0.22621.0\\winrt", false),
            ("kits:bin\\10.0.22621.0\\x64", false),
            ("kits:Lib", false),
        ];
        for (part, is_file) in parts {
            let dir = tempfile::tempdir().unwrap();
            let (install, kits) = machine(dir.path(), &["14.44.35207"], "14.44.35207", &["10.0.22621.0"]);
            assert!(layout("AMD64", &install, &kits).is_some(), "the whole layout is accepted");
            let path = part.strip_prefix("kits:").map_or_else(|| install.join(part), |part| kits.join(part));
            if is_file {
                std::fs::remove_file(&path).unwrap();
            } else {
                std::fs::remove_dir_all(&path).unwrap();
            }
            assert_eq!(layout("AMD64", &install, &kits), None, "without {part}");
        }
        assert_eq!(layout("AMD64", Path::new("C:\\no\\such\\install"), Path::new("C:\\no\\such\\kits")), None);
    }

    #[test]
    fn a_version_that_is_a_path_is_no_toolset() {
        let dir = tempfile::tempdir().unwrap();
        let (install, kits) = machine(dir.path(), &["14.44.35207"], "..\\..\\..\\..", &["10.0.22621.0"]);
        assert_eq!(layout("AMD64", &install, &kits), None);
    }

    #[test]
    fn only_an_x64_host_is_given_anything() {
        let dir = tempfile::tempdir().unwrap();
        let (install, kits) = machine(dir.path(), &["14.44.35207"], "14.44.35207", &["10.0.22621.0"]);
        for architecture in ["ARM64", "x86", ""] {
            assert_eq!(layout(architecture, &install, &kits), None, "{architecture}");
        }
        assert!(layout("amd64", &install, &kits).is_some());
    }

    fn tools() -> BuildTools {
        BuildTools { lib: paths(&[Path::new("C:\\vc\\lib"), Path::new("C:\\kit\\um")]), include: paths(&[Path::new("C:\\vc\\include")]), path: paths(&[Path::new("C:\\vc\\bin"), Path::new("C:\\kit\\bin")]) }
    }

    fn block(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect()
    }

    fn value<'a>(block: &'a [(String, String)], name: &str) -> Option<&'a str> {
        block.iter().find(|(known, _)| known == name).map(|(_, value)| value.as_str())
    }

    #[test]
    fn lib_and_include_are_set_and_the_tools_come_after_everything_on_path() {
        let mut environment = block(&[("PATH", "C:\\Windows\\System32;C:\\a"), ("SystemRoot", "C:\\Windows")]);
        add_to(&mut environment, || Some(tools()));
        assert_eq!(value(&environment, "LIB"), Some("C:\\vc\\lib;C:\\kit\\um"));
        assert_eq!(value(&environment, "INCLUDE"), Some("C:\\vc\\include"));
        assert_eq!(value(&environment, "PATH"), Some("C:\\Windows\\System32;C:\\a;C:\\vc\\bin;C:\\kit\\bin"));
        assert_eq!(value(&environment, "SystemRoot"), Some("C:\\Windows"));
        assert_eq!(environment.iter().filter(|(name, _)| name == "PATH").count(), 1);
        // A PATH that ends in a separator, and none at all.
        let mut ends = block(&[("PATH", "C:\\a;")]);
        add_to(&mut ends, || Some(tools()));
        assert_eq!(value(&ends, "PATH"), Some("C:\\a;C:\\vc\\bin;C:\\kit\\bin"));
        let mut none = Vec::new();
        add_to(&mut none, || Some(tools()));
        assert_eq!(value(&none, "PATH"), Some("C:\\vc\\bin;C:\\kit\\bin"));
        // An INCLUDE of the person's, with no LIB: theirs stays, after.
        let mut own = block(&[("INCLUDE", "C:\\own")]);
        add_to(&mut own, || Some(tools()));
        assert_eq!(value(&own, "INCLUDE"), Some("C:\\vc\\include;C:\\own"));
    }

    #[test]
    fn a_lib_the_app_has_wins_and_nothing_is_looked_for() {
        let mut environment = block(&[("PATH", "C:\\a"), ("LIB", "C:\\dev\\prompt"), ("INCLUDE", "C:\\dev\\include")]);
        let before = environment.clone();
        add_to(&mut environment, || panic!("a Developer prompt's LIB is the answer: nothing is looked for"));
        assert_eq!(environment, before);
        // An empty one is none.
        let mut empty = block(&[("LIB", "")]);
        add_to(&mut empty, || Some(tools()));
        assert_eq!(value(&empty, "LIB"), Some("C:\\vc\\lib;C:\\kit\\um"));
    }

    #[test]
    fn a_machine_with_no_build_tools_is_left_as_it_is() {
        let mut environment = block(&[("PATH", "C:\\a")]);
        add_to(&mut environment, || None);
        assert_eq!(environment, block(&[("PATH", "C:\\a")]));
    }

    /// On a machine with Visual Studio's C++ tools and a Windows SDK: what a Developer prompt would give is found, from outside any container.
    #[test]
    #[ignore = "needs Visual Studio with the C++ tools for x64, and the Windows SDK"]
    fn the_real_machines_build_tools_are_found() {
        let Looked::Settled(Some(found)) = look() else { panic!("Visual Studio with the C++ tools, and the Windows SDK") };
        println!("{found:#?}");
        assert!(found.path[0].join("link.exe").is_file() && found.path[0].join("cl.exe").is_file());
        assert!(found.path[1].join("rc.exe").is_file());
        assert!(found.lib.iter().any(|folder| folder.join("kernel32.lib").is_file()) && found.lib.iter().any(|folder| folder.join("libucrt.lib").is_file()));
        assert!(found.include.iter().any(|folder| folder.join("vector").is_file()) && found.include.iter().any(|folder| folder.join("windows.h").is_file()));
    }

    #[test]
    fn a_program_that_does_not_end_is_ended_and_one_that_does_is_heard() {
        let mut slow = std::process::Command::new("cmd");
        slow.args(["/d", "/c", "ping -n 6 127.0.0.1 >nul"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped());
        crate::tools::no_window(&mut slow);
        let began = Instant::now();
        assert_eq!(output_within(slow, Duration::from_millis(300)), Ran::TooSlow);
        assert!(began.elapsed() < Duration::from_secs(5));
        let mut quick = std::process::Command::new("cmd");
        quick.args(["/d", "/c", "echo", "C:\\a"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped());
        crate::tools::no_window(&mut quick);
        assert!(matches!(output_within(quick, Duration::from_secs(20)), Ran::Said(said) if said.trim() == "C:\\a"));
    }

    #[test]
    fn a_program_that_fails_or_is_not_there_is_not_one_that_ran_out_of_time() {
        let mut fails = std::process::Command::new("cmd");
        fails.args(["/d", "/c", "echo C:\\a & exit 3"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped());
        crate::tools::no_window(&mut fails);
        assert_eq!(output_within(fails, Duration::from_secs(20)), Ran::Failed);
        let mut missing = std::process::Command::new("C:\\no\\such\\folder\\vswhere.exe");
        missing.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped());
        assert_eq!(output_within(missing, Duration::from_secs(20)), Ran::Failed);
    }

    /// Ends what a test started, by its number, when the test ends in whatever way: `pid_file` is where the program wrote it.
    struct EndsWhatItStarted {
        pid_file: PathBuf,
    }

    impl Drop for EndsWhatItStarted {
        fn drop(&mut self) {
            if let Some(pid) = std::fs::read_to_string(&self.pid_file).ok().and_then(|text| text.trim().parse::<u32>().ok()) {
                crate::tools::kill_tree(pid);
            }
        }
    }

    #[test]
    fn a_program_whose_output_stays_open_after_it_ended_is_given_up_at_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("sleeper.pid");
        let _ends = EndsWhatItStarted { pid_file: pid_file.clone() };
        // The program starts a sleeper that shares its output, writes the sleeper's number and ends: the sleeper holds the output open for twenty seconds more.
        let program = "import subprocess, sys; s = subprocess.Popen([sys.executable, '-I', '-c', 'import time; time.sleep(20)']); open(sys.argv[1], 'w').write(str(s.pid))";
        let mut command = std::process::Command::new("python");
        command.args(["-I", "-c", program]).arg(&pid_file).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
        crate::tools::no_window(&mut command);
        let began = Instant::now();
        assert_eq!(output_within(command, Duration::from_secs(3)), Ran::TooSlow);
        // (by the limit, and not when the sleeper lets go)
        assert!(began.elapsed() < Duration::from_secs(10), "{:?}", began.elapsed());
        assert!(pid_file.is_file(), "the sleeper was started, and it is what held the output");
    }

    #[test]
    fn a_vswhere_that_is_not_there_is_a_machine_without_visual_studio_for_good() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(installation(dir.path()), Looked::Settled(None)));
    }

    /// A finder of a test's own (the process's is not for a test to count on). `begin` needs it to live as long as the process does.
    fn finder(again_after: Duration) -> &'static Finder {
        Box::leak(Box::new(Finder::new(again_after)))
    }

    /// A look that counts that it was made, and comes to `answer`.
    fn counting(calls: &Arc<AtomicUsize>, answer: Looked<Option<BuildTools>>) -> impl FnOnce() -> Looked<Option<BuildTools>> + Send + 'static {
        let calls = calls.clone();
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            answer
        }
    }

    fn within(seconds: u64) -> Instant {
        Instant::now() + Duration::from_secs(seconds)
    }

    #[test]
    fn a_look_that_ran_out_of_time_is_not_kept_and_a_later_call_looks_again() {
        let (finder, calls, go_on) = (finder(Duration::ZERO), Arc::new(AtomicUsize::new(0)), AtomicBool::new(false));
        assert_eq!(finder.found_before(counting(&calls, Looked::Unsettled), within(10), &go_on), None);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(finder.found_before(counting(&calls, Looked::Unsettled), within(10), &go_on), None);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "a later call looks again");
        // (the gate's form begins the look, and has nothing to show for it yet; a command that comes while it is going on waits for it)
        assert_eq!(finder.known(counting(&calls, Looked::Settled(Some(tools())))), None);
        assert_eq!(finder.found_before(counting(&calls, Looked::Unsettled), within(10), &go_on), Some(tools()));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        // what was found stays: no look is made again, for any form of the question
        for _ in 0..3 {
            assert_eq!(finder.found_before(counting(&calls, Looked::Unsettled), within(10), &go_on), Some(tools()));
            assert_eq!(finder.known(counting(&calls, Looked::Unsettled)), Some(tools()));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_machine_without_the_tools_is_settled_and_not_looked_at_again() {
        let (finder, calls, go_on) = (finder(Duration::ZERO), Arc::new(AtomicUsize::new(0)), AtomicBool::new(false));
        assert_eq!(finder.found_before(counting(&calls, Looked::Settled(None)), within(10), &go_on), None);
        for _ in 0..3 {
            assert_eq!(finder.found_before(counting(&calls, Looked::Settled(Some(tools()))), within(10), &go_on), None);
            assert_eq!(finder.known(counting(&calls, Looked::Settled(Some(tools())))), None);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_look_that_ran_out_of_time_is_not_made_again_within_the_minute() {
        let (finder, calls, go_on) = (finder(Duration::from_secs(3600)), Arc::new(AtomicUsize::new(0)), AtomicBool::new(false));
        assert_eq!(finder.found_before(counting(&calls, Looked::Unsettled), within(10), &go_on), None);
        let began = Instant::now();
        for _ in 0..3 {
            assert_eq!(finder.found_before(counting(&calls, Looked::Settled(Some(tools()))), within(10), &go_on), None);
            assert_eq!(finder.known(counting(&calls, Looked::Settled(Some(tools())))), None);
        }
        // (a command goes on without, and does not wait for a look that is not to be made)
        assert!(began.elapsed() < Duration::from_secs(2));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_look_that_panics_settles_nothing_and_is_made_again() {
        let (finder, calls, go_on) = (finder(Duration::ZERO), Arc::new(AtomicUsize::new(0)), AtomicBool::new(false));
        let panicking = {
            let calls = calls.clone();
            move || -> Looked<Option<BuildTools>> {
                calls.fetch_add(1, Ordering::SeqCst);
                panic!("a look that went wrong, on purpose")
            }
        };
        assert_eq!(finder.found_before(panicking, within(10), &go_on), None);
        assert_eq!(finder.found_before(counting(&calls, Looked::Settled(Some(tools()))), within(10), &go_on), Some(tools()));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn the_gate_takes_what_is_known_and_does_not_wait_for_a_look_that_is_open() {
        let (finder, calls, go_on) = (finder(LOOK_AGAIN_AFTER), Arc::new(AtomicUsize::new(0)), AtomicBool::new(false));
        let (release, held) = std::sync::mpsc::channel::<()>();
        let (began, has_begun) = std::sync::mpsc::channel::<()>();
        let open = {
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                let _ = began.send(());
                // (a vswhere that is held up: the look is not over until the test says)
                let _ = held.recv();
                Looked::Settled(Some(tools()))
            }
        };
        let asked = Instant::now();
        assert_eq!(finder.known(open), None);
        assert!(asked.elapsed() < Duration::from_secs(2), "the gate waited: {:?}", asked.elapsed());
        has_begun.recv_timeout(Duration::from_secs(10)).expect("the look began in the background");
        // asked again with the look still open: at once, and with no second look
        let asked = Instant::now();
        for _ in 0..5 {
            assert_eq!(finder.known(counting(&calls, Looked::Settled(Some(tools())))), None);
        }
        assert!(asked.elapsed() < Duration::from_secs(2), "the gate waited: {:?}", asked.elapsed());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // A command's environment waits for it, but only for the time the command has left.
        let asked = Instant::now();
        assert_eq!(finder.found_before(counting(&calls, Looked::Unsettled), Instant::now() + Duration::from_millis(300), &go_on), None);
        assert!(asked.elapsed() >= Duration::from_millis(250) && asked.elapsed() < Duration::from_secs(5), "{:?}", asked.elapsed());
        // And not past Stop, which is set while it waits.
        let stop = Arc::new(AtomicBool::new(false));
        let stopper = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                stop.store(true, Ordering::Relaxed);
            })
        };
        let asked = Instant::now();
        assert_eq!(finder.found_before(counting(&calls, Looked::Unsettled), within(60), &stop), None);
        assert!(asked.elapsed() < Duration::from_secs(5), "Stop was not heard: {:?}", asked.elapsed());
        stopper.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // When the look ends in time, the command has the tools, and the gate's form has them from then on.
        release.send(()).unwrap();
        assert_eq!(finder.found_before(counting(&calls, Looked::Unsettled), within(10), &go_on), Some(tools()));
        assert_eq!(finder.known(counting(&calls, Looked::Unsettled)), Some(tools()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
