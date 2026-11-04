//! What a Developer prompt for x64 gives a build, for commands that cannot find Visual Studio themselves. Rust finds MSVC through the Visual Studio setup configuration (COM and
//! `C:\ProgramData`), which a container cannot reach, and `cl` and `link` are on no PATH of a person's unless they started from a Developer prompt. The files are readable, so only the
//! variables are missing: found once per process, outside the sandbox, and given to a command as `LIB`, `INCLUDE` and the tool folders at the end of its PATH.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// `vswhere` is a small program that reads a few files; when it takes longer than this, something is wrong and a build goes on without.
const VSWHERE_LIMIT: Duration = Duration::from_secs(10);
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

/// The build tools of this machine, looked for once per process. The first command waits for the lookup, which takes a moment, and none after it does.
pub fn found() -> Option<BuildTools> {
    static FOUND: OnceLock<Option<BuildTools>> = OnceLock::new();
    FOUND.get_or_init(look).clone()
}

fn look() -> Option<BuildTools> {
    let architecture = std::env::var("PROCESSOR_ARCHITECTURE").ok().filter(|architecture| is_x64(architecture))?;
    let program_files = std::env::var_os("ProgramFiles(x86)").map(PathBuf::from)?;
    let install = installation(&program_files)?;
    // (the registry's root first; where the SDK puts itself by default, when it has none or the one it names is not what was wanted)
    let kits = kits_root().into_iter().chain([program_files.join("Windows Kits\\10")]);
    kits.into_iter().find_map(|kits| layout(&architecture, &install, &kits))
}

/// The newest Visual Studio with the C++ tools, as its own `vswhere` says.
fn installation(program_files: &Path) -> Option<PathBuf> {
    let vswhere = program_files.join("Microsoft Visual Studio\\Installer\\vswhere.exe");
    if !vswhere.is_file() {
        return None;
    }
    let mut command = std::process::Command::new(vswhere);
    command.args(["-latest", "-products", "*", "-requires", CPP_TOOLS, "-property", "installationPath", "-utf8"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    crate::tools::no_window(&mut command);
    let said = output_within(command, VSWHERE_LIMIT)?;
    said.lines().map(str::trim).find(|line| !line.is_empty()).map(PathBuf::from).filter(|folder| folder.is_dir())
}

/// What the program prints, if it ends well within the limit. Past it, the program is ended.
fn output_within(mut command: std::process::Command, limit: Duration) -> Option<String> {
    use std::io::Read;
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return reader.join().ok().filter(|_| status.success()).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            // (the reader ends by itself when the program's end closes its pipe)
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
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
        let found = look().expect("Visual Studio with the C++ tools, and the Windows SDK");
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
        assert_eq!(output_within(slow, Duration::from_millis(300)), None);
        assert!(began.elapsed() < Duration::from_secs(5));
        let mut quick = std::process::Command::new("cmd");
        quick.args(["/d", "/c", "echo", "C:\\a"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped());
        crate::tools::no_window(&mut quick);
        assert_eq!(output_within(quick, Duration::from_secs(20)).as_deref().map(str::trim), Some("C:\\a"));
    }
}
