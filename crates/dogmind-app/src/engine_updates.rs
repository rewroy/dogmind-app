//! Background updates of the bundled Straydog engine. Nothing here replaces a running program.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use dogmind_agent::http::{self, Url};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::engine;
use crate::lang::{t, tf};
use crate::settings::Settings;
use crate::updates::{self, Setup};

const RELEASES: &str = "https://api.github.com/repos/rewroy/Straydog/releases?per_page=30";
const EXE: &str = "straydog-windows-x86_64-cuda.exe";
const SUMS: &str = "SHA256SUMS";
const ROOT: &str = "engine-updates";
const SELECTED: &str = "selected.txt";
const PENDING: &str = "awaiting-ready.txt";
const FAILED: &str = "failed.txt";
const MOST_RELEASES: usize = 4 << 20;
const MOST_EXE: u64 = 128 << 20;

#[derive(Debug, Clone)]
struct Candidate {
    version: String,
    exe: Setup,
    sums: Setup,
    zip: Option<Setup>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Result {
    Ready(String),
    ModelNotReady(String),
    Failed(String),
}

pub fn log_failure(data: &Path, reason: &str) {
    if let Ok(mut log) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(engine::log_path(data))
    {
        let _ = writeln!(log, "[Dogmind Straydog update] {reason}");
    }
}

fn plain_version(version: &str) -> bool {
    version.starts_with("0.1.")
        && version.len() <= 40
        && version
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
}

fn asset(release: &Value, name: &str, limit: u64, tag: &str) -> Option<Setup> {
    let found = release
        .get("assets")?
        .as_array()?
        .iter()
        .find(|asset| asset.get("name").and_then(Value::as_str) == Some(name))?;
    let size = found
        .get("size")?
        .as_u64()
        .filter(|size| (1..=limit).contains(size))?;
    let sha256 = found.get("digest")?.as_str()?.strip_prefix("sha256:")?;
    if sha256.len() != 64 || !sha256.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let url = found.get("browser_download_url")?.as_str()?;
    if url != format!("https://github.com/rewroy/Straydog/releases/download/{tag}/{name}") {
        return None;
    }
    Some(Setup {
        url: url.to_string(),
        size,
        sha256: sha256.to_ascii_lowercase(),
        parts: None,
    })
}

fn candidate(release: &Value) -> Option<Candidate> {
    if release.get("draft")?.as_bool()? || release.get("published_at")?.is_null() {
        return None;
    }
    let tag = release.get("tag_name")?.as_str()?;
    let version = tag.strip_prefix('v')?;
    if !plain_version(version) {
        return None;
    }
    let exe = asset(release, EXE, MOST_EXE, tag)?;
    let sums = asset(release, SUMS, 1 << 20, tag)?;
    let zip_name = format!("straydog-v{version}-windows-x86_64-cuda.zip");
    let zip = asset(release, &zip_name, 2 << 30, tag);
    Some(Candidate {
        version: version.to_string(),
        exe,
        sums,
        zip,
    })
}

fn newest(body: &str, running: &str) -> Option<Candidate> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .as_array()?
        .iter()
        .filter_map(candidate)
        .filter(|release| updates::is_newer(&release.version, running))
        .max_by(|a, b| {
            if updates::is_newer(&a.version, &b.version) {
                std::cmp::Ordering::Greater
            } else if updates::is_newer(&b.version, &a.version) {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
}

fn release_list(stop: &AtomicBool) -> Option<String> {
    release_list_at(RELEASES, Duration::from_secs(10), stop)
}

fn release_list_at(address: &str, timeout: Duration, stop: &AtomicBool) -> Option<String> {
    let (send, receive) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let (address, for_request) = (address.to_string(), cancelled.clone());
    std::thread::spawn(move || {
        let _ = send.send(release_list_now(&address, timeout, &for_request));
    });
    let start = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            cancelled.store(true, Ordering::Relaxed);
            return None;
        }
        let Some(remaining) = timeout.checked_sub(start.elapsed()) else {
            cancelled.store(true, Ordering::Relaxed);
            return None;
        };
        match receive.recv_timeout(remaining.min(Duration::from_millis(25))) {
            Ok(answer) => return answer,
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn release_list_now(address: &str, timeout: Duration, stop: &AtomicBool) -> Option<String> {
    let url = Url::parse(address).ok()?;
    let headers = [
        (
            "Accept".to_string(),
            "application/vnd.github+json".to_string(),
        ),
        (
            "User-Agent".to_string(),
            format!("Dogmind/{}", env!("CARGO_PKG_VERSION")),
        ),
    ];
    let mut response = http::request("GET", &url, &headers, None, timeout, stop).ok()?;
    if response.status != 200 {
        return None;
    }
    let mut body = Vec::new();
    while let Some(bytes) = response.next().ok()? {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        body.extend_from_slice(&bytes);
        if body.len() > MOST_RELEASES {
            return None;
        }
    }
    String::from_utf8(body).ok()
}

fn sha256(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut bytes = [0u8; 1 << 20];
    loop {
        let n = file.read(&mut bytes).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&bytes[..n]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn sum_for(text: &str, name: &str) -> Option<String> {
    let matching: Vec<&str> = text
        .lines()
        .filter_map(|line| {
            let (sum, file) = line.split_once("  ")?;
            (file == name && sum.len() == 64 && sum.bytes().all(|c| c.is_ascii_hexdigit()))
                .then_some(sum)
        })
        .collect();
    (matching.len() == 1).then(|| matching[0].to_ascii_lowercase())
}

fn version(path: &Path) -> Option<String> {
    let said = engine::version(path)?;
    let version = said.strip_prefix("straydog ")?.trim();
    plain_version(version).then(|| version.to_string())
}

fn check_model(exe: &Path, settings: &Settings, data: &Path) -> std::result::Result<(), String> {
    if !engine::is_set_up(data) {
        return Ok(());
    }
    let mut command = Command::new(exe);
    command
        .args(engine::engine_arguments(settings, data))
        .arg("--check-model")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().map_err(|error| {
        tf(
            "The new Straydog model check could not start: {0}",
            &[&error],
        )
    })?;
    let start = Instant::now();
    loop {
        if child
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            break;
        }
        if start.elapsed() >= Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(t("The new Straydog model check timed out.").into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        let said = String::from_utf8_lossy(&output.stdout);
        if said.lines().count() == 1 && said.starts_with("Straydog model ready: ") {
            return Ok(());
        }
        return Err(t("The new Straydog returned an unreadable model check result.").into());
    }
    if output.status.code() == Some(2) {
        return Err(
            t("The new Straydog does not support model checks, so it was not selected.").into(),
        );
    }
    let reason = String::from_utf8_lossy(&output.stderr)
        .lines()
        .next()
        .unwrap_or(t("Model check failed."))
        .to_string();
    Err(tf(
        "The new Straydog cannot use this model: {0}",
        &[&reason],
    ))
}

fn root(data: &Path) -> PathBuf {
    data.join(ROOT)
}

pub fn selected(data: &Path) -> Option<PathBuf> {
    selection(data)
        .into_iter()
        .map(|version| root(data).join(format!("v{version}")).join("straydog.exe"))
        .find(|exe| exe.is_file())
}

pub fn rollback(data: &Path) -> bool {
    let Some(exe) = selected(data) else {
        return false;
    };
    let Some(version) = exe
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix('v'))
    else {
        return false;
    };
    let folder = root(data);
    if fs::write(folder.join(FAILED), format!("v{version}")).is_err() {
        return false;
    }
    let previous = selection(data).into_iter().find(|other| {
        other != version
            && folder
                .join(format!("v{other}"))
                .join("straydog.exe")
                .is_file()
    });
    let restored = match previous {
        Some(previous) => atomic_selection(&folder, &previous).is_ok(),
        None => fs::remove_file(folder.join(SELECTED)).is_ok(),
    };
    if restored {
        let _ = fs::remove_file(folder.join(PENDING));
    }
    restored
}

/// Only the first start after a switch can cause an automatic rollback. A later unrelated
/// connection error must not replace an engine that has already served successfully.
pub fn rollback_if_pending(data: &Path, attempted: &Path) -> bool {
    if selected(data).as_deref() != Some(attempted) {
        return false;
    }
    let pending = fs::read_to_string(root(data).join(PENDING)).ok();
    let active =
        selected(data).and_then(|exe| exe.parent()?.file_name()?.to_str().map(str::to_string));
    if pending.is_none() || pending.as_deref().map(str::trim) != active.as_deref() {
        return false;
    }
    rollback(data)
}

/// A newer engine that fails before it is ready goes back to the previous one, whichever way the
/// failure is seen first: the connection giving up, or the process ending. `true` when it did.
pub fn roll_back_after_failed_start(
    data: &Path,
    embedded: bool,
    never_ready: bool,
    attempted: Option<&Path>,
) -> bool {
    embedded
        && never_ready
        && attempted.is_some_and(|attempted| rollback_if_pending(data, attempted))
}

pub fn mark_ready(data: &Path, started: &Path) {
    if selected(data).as_deref() != Some(started) {
        return;
    }
    let pending = fs::read_to_string(root(data).join(PENDING)).ok();
    let active =
        selected(data).and_then(|exe| exe.parent()?.file_name()?.to_str().map(str::to_string));
    if pending.is_some() && pending.as_deref().map(str::trim) == active.as_deref() {
        let _ = fs::remove_file(root(data).join(PENDING));
    }
}

fn select(data: &Path, version: &str) -> std::io::Result<()> {
    let folder = root(data);
    let old = selected(data).and_then(|exe| {
        exe.parent()?
            .file_name()?
            .to_str()?
            .strip_prefix('v')
            .map(str::to_string)
    });
    let list = old
        .filter(|old| old != version)
        .map_or_else(|| version.to_string(), |old| format!("{version}\n{old}"));
    fs::write(folder.join(PENDING), format!("v{version}"))?;
    if let Err(error) = atomic_selection(&folder, &list) {
        let _ = fs::remove_file(folder.join(PENDING));
        return Err(error);
    }
    Ok(())
}

fn selection(data: &Path) -> Vec<String> {
    fs::read_to_string(root(data).join(SELECTED))
        .ok()
        .into_iter()
        .flat_map(|text| text.lines().take(2).map(str::to_string).collect::<Vec<_>>())
        .filter(|version| plain_version(version))
        .collect()
}

fn atomic_selection(folder: &Path, text: &str) -> std::io::Result<()> {
    let pending = folder.join("selected.partial");
    let active = folder.join(SELECTED);
    fs::write(&pending, text)?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let wide = |path: &Path| {
            path.as_os_str()
                .encode_wide()
                .chain([0])
                .collect::<Vec<_>>()
        };
        // SAFETY: both paths are NUL-terminated UTF-16 strings and the source is closed.
        if unsafe {
            MoveFileExW(
                wide(&pending).as_ptr(),
                wide(&active).as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(pending, active)
    }
}

pub fn prefer_bundled_if_newer(data: &Path) {
    let Some(active) = selected(data) else { return };
    let Some(bundled) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|folder| folder.join("straydog.exe")))
    else {
        return;
    };
    let (Some(here), Some(built_in)) = (version(&active), version(&bundled)) else {
        return;
    };
    if !updates::is_newer(&here, &built_in) {
        let _ = fs::remove_file(root(data).join(SELECTED));
        let _ = fs::remove_file(root(data).join(PENDING));
    }
}

/// Check in the background after app startup. An unreachable GitHub is a normal offline start.
/// Only a bundled engine is eligible; a person-supplied path remains their choice.
pub fn check_and_stage(settings: &Settings, data: &Path, stop: &AtomicBool) -> Option<Result> {
    if !settings.check_straydog_updates
        || settings.straydog.is_some()
        || stop.load(Ordering::Relaxed)
    {
        return None;
    }
    let bundled = std::env::current_exe().ok()?.parent()?.join("straydog.exe");
    if !bundled.is_file() {
        return None;
    }
    let running = selected(data).unwrap_or(bundled.clone());
    let running_version = version(&running)?;
    let release = newest(&release_list(stop)?, &running_version)?;
    if fs::read_to_string(root(data).join(FAILED))
        .ok()
        .is_some_and(|failed| failed.trim() == format!("v{}", release.version))
    {
        return None;
    }
    if crate::machine::connection_is_metered() || stop.load(Ordering::Relaxed) {
        return None;
    }
    Some(stage(&release, &running, settings, data, stop))
}

fn stage(
    release: &Candidate,
    running: &Path,
    settings: &Settings,
    data: &Path,
    stop: &AtomicBool,
) -> Result {
    stage_with(
        release,
        running,
        settings,
        data,
        stop,
        &mut updates::fetch_straydog_asset,
    )
}

fn stage_with<F>(
    release: &Candidate,
    running: &Path,
    settings: &Settings,
    data: &Path,
    stop: &AtomicBool,
    fetch: &mut F,
) -> Result
where
    F: FnMut(&Setup, &Path, &AtomicBool) -> std::result::Result<(), updates::Failed>,
{
    let root = root(data);
    let version_folder = root.join(format!("v{}", release.version));
    let partial = root.join(format!("v{}.partial", release.version));
    let mut attempt = || -> std::result::Result<Result, String> {
        fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        if fs::symlink_metadata(&root).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(t("The engine update folder cannot be a link.").into());
        }
        if version_folder.exists() {
            if fs::symlink_metadata(&version_folder).is_ok_and(|meta| meta.file_type().is_symlink())
            {
                return Err(t("The engine update folder cannot be a link.").into());
            }
            let ready = version_folder.join("straydog.exe");
            let libraries_present = needed_libraries(&ready)
                .is_some_and(|names| names.iter().all(|name| version_folder.join(name).is_file()));
            if libraries_present
                && sha256(&ready).as_deref() == Some(release.exe.sha256.as_str())
                && version(&ready).as_deref() == Some(release.version.as_str())
            {
                grant_worker_access(&version_folder)?;
                if let Err(reason) = check_model(&ready, settings, data) {
                    return Ok(Result::ModelNotReady(reason));
                }
                select(data, &release.version).map_err(|error| error.to_string())?;
                return Ok(Result::Ready(release.version.clone()));
            }
            fs::remove_dir_all(&version_folder).map_err(|error| error.to_string())?;
        }
        if partial.exists() {
            if fs::symlink_metadata(&partial).is_ok_and(|meta| meta.file_type().is_symlink()) {
                return Err(t("The engine update folder cannot be a link.").into());
            }
            fs::remove_dir_all(&partial).map_err(|error| error.to_string())?
        }
        fs::create_dir(&partial).map_err(|error| error.to_string())?;
        let sums_file = partial.join(SUMS);
        fetch(&release.sums, &sums_file, stop).map_err(|error| format!("SHA256SUMS: {error:?}"))?;
        if stop.load(Ordering::Relaxed) {
            return Err(t("The engine update was cancelled.").into());
        }
        let sums = fs::read_to_string(&sums_file).map_err(|error| error.to_string())?;
        let expected =
            sum_for(&sums, EXE).ok_or(t("SHA256SUMS does not list the standalone engine."))?;
        if expected != release.exe.sha256 {
            return Err(t("The engine digest disagrees with SHA256SUMS.").into());
        }
        let exe = partial.join("straydog.exe");
        fetch(&release.exe, &exe, stop).map_err(|error| format!("Straydog EXE: {error:?}"))?;
        if stop.load(Ordering::Relaxed) {
            return Err(t("The engine update was cancelled.").into());
        }
        if sha256(&exe).as_deref() != Some(expected.as_str()) {
            return Err(t("The engine file failed its SHA-256 check.").into());
        }
        copy_libraries(running, &exe, &partial, release, &sums, stop, fetch)?;
        if stop.load(Ordering::Relaxed) {
            return Err(t("The engine update was cancelled.").into());
        }
        if version(&exe).as_deref() != Some(release.version.as_str()) {
            return Err(t("The engine version disagrees with the release tag.").into());
        }
        grant_worker_access(&partial)?;
        if let Err(reason) = check_model(&exe, settings, data) {
            fs::rename(&partial, &version_folder).map_err(|error| error.to_string())?;
            return Ok(Result::ModelNotReady(reason));
        }
        if stop.load(Ordering::Relaxed) {
            return Err(t("The engine update was cancelled.").into());
        }
        fs::rename(&partial, &version_folder).map_err(|error| error.to_string())?;
        select(data, &release.version).map_err(|error| error.to_string())?;
        Ok(Result::Ready(release.version.clone()))
    };
    let outcome = attempt().unwrap_or_else(Result::Failed);
    match &outcome {
        Result::Failed(reason) | Result::ModelNotReady(reason) => log_failure(data, reason),
        Result::Ready(_) => {}
    }
    if !matches!(outcome, Result::Ready(_)) {
        let _ = fs::remove_dir_all(&partial);
    }
    outcome
}

fn imported_dll(exe: &Path, prefix: &[u8]) -> Option<String> {
    let body = fs::read(exe).ok()?;
    body.windows(prefix.len())
        .enumerate()
        .filter_map(|(at, window)| {
            if window != prefix {
                return None;
            }
            let tail = &body[at + prefix.len()..];
            let digits = tail.iter().take_while(|byte| byte.is_ascii_digit()).count();
            if !(1..=3).contains(&digits) || tail.get(digits..digits + 4)? != b".dll" {
                return None;
            }
            let major = std::str::from_utf8(&tail[..digits])
                .ok()?
                .parse::<u32>()
                .ok()?;
            let name = String::from_utf8(body[at..at + prefix.len() + digits + 4].to_vec()).ok()?;
            Some((major, name))
        })
        .max_by_key(|(major, _)| *major)
        .map(|(_, name)| name)
}

fn needed_libraries(exe: &Path) -> Option<[String; 2]> {
    // The executable imports cuBLAS. cuBLAS loads cuBLASLt itself, so the latter's
    // name need not appear in the executable's PE import table.
    let cublas = imported_dll(exe, b"cublas64_")?;
    let major = cublas.strip_prefix("cublas64_")?.strip_suffix(".dll")?;
    Some([cublas.clone(), format!("cublasLt64_{major}.dll")])
}

fn copy_libraries<F>(
    running: &Path,
    exe: &Path,
    partial: &Path,
    release: &Candidate,
    sums: &str,
    stop: &AtomicBool,
    fetch: &mut F,
) -> std::result::Result<(), String>
where
    F: FnMut(&Setup, &Path, &AtomicBool) -> std::result::Result<(), updates::Failed>,
{
    let names =
        needed_libraries(exe).ok_or(t("Cannot identify the new engine's cuBLAS dependency."))?;
    let old = running
        .parent()
        .ok_or(t("The current engine folder is missing."))?;
    let old_names = needed_libraries(running)
        .ok_or(t("Cannot identify the current engine's cuBLAS dependency."))?;
    if names == old_names {
        if !names.iter().all(|name| old.join(name).is_file()) {
            return Err(t("The current engine's cuBLAS DLL is missing.").into());
        }
        for name in &names {
            fs::copy(old.join(name), partial.join(name)).map_err(|error| error.to_string())?;
        }
        let license = old.join("NVIDIA-CUDA-LICENSE.txt");
        if license.is_file() {
            fs::copy(license, partial.join("NVIDIA-CUDA-LICENSE.txt"))
                .map_err(|error| error.to_string())?;
        }
        return Ok(());
    }
    let zip_name = format!("straydog-v{}-windows-x86_64-cuda.zip", release.version);
    let zip = release.zip.as_ref().ok_or(t(
        "The new cuBLAS requires a release ZIP, but none was published.",
    ))?;
    if sum_for(sums, &zip_name).as_deref() != Some(zip.sha256.as_str()) {
        return Err(t("The ZIP digest disagrees with SHA256SUMS.").into());
    }
    let zip_path = partial.join(&zip_name);
    fetch(zip, &zip_path, stop).map_err(|error| format!("Straydog ZIP: {error:?}"))?;
    let file = fs::File::open(&zip_path).map_err(|error| error.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| error.to_string())?;
    let prefix = format!(
        "{zip_name_trimmed}/",
        zip_name_trimmed = zip_name.trim_end_matches(".zip")
    );
    for name in &names {
        let mut source = archive
            .by_name(&format!("{prefix}{name}"))
            .map_err(|error| tf("The release ZIP is missing {0}: {1}", &[name, &error]))?;
        if source.size() == 0 || source.size() > 512 << 20 {
            return Err(tf("The release ZIP has an invalid size for {0}.", &[name]));
        }
        let mut target = fs::File::create(partial.join(name)).map_err(|error| error.to_string())?;
        let copied = std::io::copy(&mut source, &mut target).map_err(|error| error.to_string())?;
        if copied != source.size() {
            return Err(tf("The release ZIP did not fully extract {0}.", &[name]));
        }
        target.sync_all().map_err(|error| error.to_string())?;
    }
    let mut license = archive
        .by_name(&format!("{prefix}NVIDIA-CUDA-LICENSE.txt"))
        .map_err(|error| {
            tf(
                "The release ZIP is missing the NVIDIA license: {0}",
                &[&error],
            )
        })?;
    if license.size() == 0 || license.size() > 1 << 20 {
        return Err(t("The release ZIP has an invalid NVIDIA license size.").into());
    }
    let mut target = fs::File::create(partial.join("NVIDIA-CUDA-LICENSE.txt"))
        .map_err(|error| error.to_string())?;
    std::io::copy(&mut license, &mut target).map_err(|error| error.to_string())?;
    target.sync_all().map_err(|error| error.to_string())?;
    drop(license);
    drop(archive);
    fs::remove_file(zip_path).map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(windows)]
fn worker_sid() -> std::result::Result<String, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::FreeSid;
    use windows_sys::Win32::Security::Isolation::DeriveAppContainerSidFromAppContainerName;
    let name: Vec<u16> = std::ffi::OsStr::new("Straydog.Worker")
        .encode_wide()
        .chain([0])
        .collect();
    let mut sid = std::ptr::null_mut();
    // SAFETY: The name is NUL terminated; Windows allocates sid and the string, freed below.
    unsafe {
        let result = DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid);
        if result != 0 {
            return Err(tf(
                "Cannot obtain the AppContainer SID: {0}",
                &[&format!("0x{result:08x}")],
            ));
        }
        let mut text = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut text) == 0 {
            FreeSid(sid);
            return Err(tf(
                "Cannot read the AppContainer SID: {0}",
                &[&std::io::Error::last_os_error()],
            ));
        }
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        let value = String::from_utf16(std::slice::from_raw_parts(text, len))
            .map_err(|error| error.to_string());
        LocalFree(text.cast());
        FreeSid(sid);
        value
    }
}

#[cfg(windows)]
fn system_icacls() -> std::result::Result<PathBuf, String> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buffer = [0u16; 32768];
    // SAFETY: buffer is writable for the given length; Windows returns its UTF-16 length.
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length == 0 || length >= buffer.len() {
        return Err(t("Cannot locate the Windows system directory.").into());
    }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length])).join("icacls.exe"))
}

#[cfg(windows)]
fn grant_worker_access(folder: &Path) -> std::result::Result<(), String> {
    let sid = worker_sid()?;
    let icacls = system_icacls()?;
    let items = [
        (
            folder
                .parent()
                .ok_or(t("The engine update parent folder is missing."))?
                .to_path_buf(),
            format!("*{sid}:(RX)"),
        ),
        (folder.to_path_buf(), format!("*{sid}:(OI)(CI)(RX)")),
        (folder.join("straydog.exe"), format!("*{sid}:(RX)")),
    ];
    for (path, grant) in items.into_iter().chain(
        fs::read_dir(folder)
            .map_err(|error| error.to_string())?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("dll"))
            })
            .map(|path| (path, format!("*{sid}:(RX)"))),
    ) {
        let mut grant_command = Command::new(&icacls);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            grant_command.creation_flags(0x0800_0000);
        }
        let status = grant_command
            .arg(&path)
            .arg("/grant")
            .arg(grant)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| error.to_string())?;
        if !status.success() {
            return Err(tf(
                "Cannot grant AppContainer access to {0}.",
                &[&path.display()],
            ));
        }
        let mut verify_command = Command::new(&icacls);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            verify_command.creation_flags(0x0800_0000);
        }
        let status = verify_command
            .arg(&path)
            .arg("/verify")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| error.to_string())?;
        if !status.success() {
            return Err(tf(
                "Cannot verify the access rights of {0}.",
                &[&path.display()],
            ));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn grant_worker_access(_folder: &Path) -> std::result::Result<(), String> {
    Err(t("Engine updates are supported only on Windows.").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, draft: bool, assets: &[(&str, &str)]) -> Value {
        serde_json::json!({"tag_name":tag,"draft":draft,"prerelease":true,"published_at":"2026-11-10T00:00:00Z","assets": assets.iter().map(|(name, digest)| serde_json::json!({"name":name,"size":100,"digest":format!("sha256:{digest}"),"browser_download_url":format!("https://github.com/rewroy/Straydog/releases/download/{tag}/{name}")})).collect::<Vec<_>>()})
    }

    #[test]
    fn picks_highest_published_compatible_release_with_required_assets() {
        let hash = "a".repeat(64);
        let files = [(EXE, hash.as_str()), (SUMS, hash.as_str())];
        let body = serde_json::json!([
            release("v0.1.12", true, &files),
            release("v0.1.11", false, &files),
            release("v0.1.13", false, &files[..1]),
            release("v0.2.0", false, &files),
            release("v0.1.9", false, &files)
        ])
        .to_string();
        assert_eq!(newest(&body, "0.1.10").unwrap().version, "0.1.11");
        assert!(newest(&body, "0.1.11").is_none());
    }

    #[test]
    fn sha256sums_requires_one_exact_name_and_a_digest() {
        let hash = "a".repeat(64);
        assert_eq!(
            sum_for(&format!("{hash}  {EXE}\n"), EXE),
            Some(hash.clone())
        );
        assert_eq!(sum_for(&format!("{hash}  {EXE}.old\n"), EXE), None);
        assert_eq!(
            sum_for(&format!("{hash}  {EXE}\n{hash}  {EXE}\n"), EXE),
            None
        );
    }

    #[test]
    fn a_new_cublas_major_is_found_even_if_an_old_name_is_in_the_binary() {
        let folder = tempfile::tempdir().unwrap();
        let exe = folder.path().join("straydog.exe");
        fs::write(&exe, b"error cublas64_13.dll\0import cublas64_14.dll\0").unwrap();
        assert_eq!(
            imported_dll(&exe, b"cublas64_").as_deref(),
            Some("cublas64_14.dll")
        );
        assert_eq!(
            needed_libraries(&exe).unwrap(),
            ["cublas64_14.dll", "cublasLt64_14.dll"]
        );
    }

    #[test]
    fn a_new_cublas_major_comes_only_from_the_verified_release_zip() {
        use std::io::Write;
        let data = tempfile::tempdir().unwrap();
        let old = data.path().join("old");
        let partial = data.path().join("partial");
        fs::create_dir(&old).unwrap();
        fs::create_dir(&partial).unwrap();
        let running = old.join("straydog.exe");
        fs::write(&running, b"old cublas64_13.dll cublasLt64_13.dll").unwrap();
        let exe = partial.join("straydog.exe");
        fs::write(&exe, b"new cublas64_14.dll cublasLt64_14.dll").unwrap();
        let zip_name = "straydog-v0.1.11-windows-x86_64-cuda.zip";
        let source = data.path().join(zip_name);
        let mut archive = zip::ZipWriter::new(fs::File::create(&source).unwrap());
        let prefix = "straydog-v0.1.11-windows-x86_64-cuda/";
        for (name, body) in [
            ("cublas64_14.dll", b"MZ14".as_slice()),
            ("cublasLt64_14.dll", b"MZLT14".as_slice()),
            ("NVIDIA-CUDA-LICENSE.txt", b"license".as_slice()),
        ] {
            archive
                .start_file(
                    format!("{prefix}{name}"),
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            archive.write_all(body).unwrap();
        }
        archive.finish().unwrap();
        let digest = sha256(&source).unwrap();
        let release = Candidate {
            version: "0.1.11".into(),
            exe: Setup {
                url: String::new(),
                size: 1,
                sha256: String::new(),
                parts: None,
            },
            sums: Setup {
                url: String::new(),
                size: 1,
                sha256: String::new(),
                parts: None,
            },
            zip: Some(Setup {
                url: String::new(),
                size: fs::metadata(&source).unwrap().len(),
                sha256: digest.clone(),
                parts: None,
            }),
        };
        let sums = format!("{digest}  {zip_name}\n");
        let stop = AtomicBool::new(false);
        let mut fetch = |_: &Setup, target: &Path, _: &AtomicBool| {
            fs::copy(&source, target).unwrap();
            Ok(())
        };
        copy_libraries(&running, &exe, &partial, &release, &sums, &stop, &mut fetch).unwrap();
        assert_eq!(fs::read(partial.join("cublas64_14.dll")).unwrap(), b"MZ14");
        assert_eq!(
            fs::read(partial.join("cublasLt64_14.dll")).unwrap(),
            b"MZLT14"
        );
        assert_eq!(
            fs::read(partial.join("NVIDIA-CUDA-LICENSE.txt")).unwrap(),
            b"license"
        );
        assert!(!partial.join(zip_name).exists());
        assert!(
            copy_libraries(&running, &exe, &partial, &release, "bad sum", &stop, &mut fetch)
                .is_err()
        );
    }

    #[test]
    fn a_missing_same_major_library_does_not_trigger_the_full_zip() {
        let data = tempfile::tempdir().unwrap();
        let old = data.path().join("old");
        let partial = data.path().join("partial");
        fs::create_dir(&old).unwrap();
        fs::create_dir(&partial).unwrap();
        let running = old.join("straydog.exe");
        let exe = partial.join("straydog.exe");
        fs::write(&running, b"cublas64_13.dll").unwrap();
        fs::write(&exe, b"cublas64_13.dll").unwrap();
        let release = Candidate {
            version: "0.1.11".into(),
            exe: Setup {
                url: String::new(),
                size: 1,
                sha256: String::new(),
                parts: None,
            },
            sums: Setup {
                url: String::new(),
                size: 1,
                sha256: String::new(),
                parts: None,
            },
            zip: None,
        };
        let stop = AtomicBool::new(false);
        let mut fetched = false;
        let mut fetch = |_: &Setup, _: &Path, _: &AtomicBool| {
            fetched = true;
            Ok(())
        };
        let error =
            copy_libraries(&running, &exe, &partial, &release, "", &stop, &mut fetch).unwrap_err();
        assert!(error.contains("cuBLAS DLL"), "{error}");
        assert!(!fetched);
    }

    #[test]
    fn an_unresponsive_release_server_does_not_hold_the_check_open() {
        use std::net::TcpListener;
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!(
            "http://127.0.0.1:{}/releases",
            server.local_addr().unwrap().port()
        );
        let waiting = std::thread::spawn(move || {
            let (client, _) = server.accept().unwrap();
            std::thread::sleep(Duration::from_millis(300));
            drop(client);
        });
        let start = Instant::now();
        assert!(release_list_at(
            &address,
            Duration::from_millis(100),
            &AtomicBool::new(false)
        )
        .is_none());
        assert!(start.elapsed() < Duration::from_millis(250));
        waiting.join().unwrap();
    }

    #[test]
    fn a_failed_engine_returns_to_the_previous_selected_version() {
        let data = tempfile::tempdir().unwrap();
        let folder = root(data.path());
        for version in ["0.1.11", "0.1.12"] {
            let location = folder.join(format!("v{version}"));
            fs::create_dir_all(&location).unwrap();
            fs::write(location.join("straydog.exe"), b"MZ").unwrap();
        }
        select(data.path(), "0.1.11").unwrap();
        let first = selected(data.path()).unwrap();
        mark_ready(data.path(), &first);
        assert!(
            !rollback_if_pending(data.path(), &first),
            "a later unrelated failure keeps the working engine"
        );
        select(data.path(), "0.1.12").unwrap();
        assert!(selected(data.path())
            .unwrap()
            .to_string_lossy()
            .contains("v0.1.12"));
        mark_ready(data.path(), &first);
        assert_eq!(fs::read_to_string(folder.join(PENDING)).unwrap(), "v0.1.12");
        let second = selected(data.path()).unwrap();
        assert!(!rollback_if_pending(data.path(), &first));
        assert!(rollback_if_pending(data.path(), &second));
        assert!(selected(data.path())
            .unwrap()
            .to_string_lossy()
            .contains("v0.1.11"));
        assert_eq!(fs::read_to_string(folder.join(FAILED)).unwrap(), "v0.1.12");
    }

    #[test]
    fn an_engine_that_ends_before_it_is_ready_returns_to_the_previous_version() {
        let data = tempfile::tempdir().unwrap();
        let folder = root(data.path());
        for version in ["0.1.10", "0.1.11"] {
            let location = folder.join(format!("v{version}"));
            fs::create_dir_all(&location).unwrap();
            fs::write(location.join("straydog.exe"), b"MZ").unwrap();
        }
        select(data.path(), "0.1.10").unwrap();
        let previous = selected(data.path()).unwrap();
        mark_ready(data.path(), &previous);
        select(data.path(), "0.1.11").unwrap();
        let newer = selected(data.path()).unwrap();
        assert!(newer.to_string_lossy().contains("v0.1.11"));

        // None of these is the first start of the newer engine failing before it was ready.
        assert!(
            !roll_back_after_failed_start(data.path(), true, false, Some(&newer)),
            "an engine that had been ready is not rolled back"
        );
        assert!(
            !roll_back_after_failed_start(data.path(), false, true, Some(&newer)),
            "only the bundled engine is rolled back"
        );
        assert!(
            !roll_back_after_failed_start(data.path(), true, true, Some(&previous)),
            "an engine that is not the selected one is not rolled back"
        );
        assert!(
            !roll_back_after_failed_start(data.path(), true, true, None),
            "no engine was started"
        );
        assert_eq!(selected(data.path()).as_deref(), Some(newer.as_path()));
        assert!(folder.join(PENDING).is_file());
        assert!(!folder.join(FAILED).exists());

        assert!(roll_back_after_failed_start(
            data.path(),
            true,
            true,
            Some(&newer)
        ));
        let now = selected(data.path()).unwrap();
        assert_eq!(now, previous);
        assert!(!now.to_string_lossy().contains("v0.1.11"));
        assert_eq!(fs::read_to_string(folder.join(FAILED)).unwrap(), "v0.1.11");
        assert!(!folder.join(PENDING).exists());

        assert!(
            !roll_back_after_failed_start(data.path(), true, true, Some(&newer)),
            "a second failure does not roll back again"
        );
        assert_eq!(selected(data.path()).as_deref(), Some(previous.as_path()));
    }

    #[cfg(windows)]
    #[test]
    fn staged_engine_and_libraries_get_the_workers_acl() {
        let data = tempfile::tempdir().unwrap();
        let folder = data.path().join("v0.1.11.partial");
        fs::create_dir(&folder).unwrap();
        for name in ["straydog.exe", "cublas64_13.dll", "cublasLt64_13.dll"] {
            fs::write(folder.join(name), b"MZ").unwrap();
        }
        grant_worker_access(&folder).unwrap();
        let sid = worker_sid().unwrap();
        for path in [
            folder.clone(),
            folder.join("straydog.exe"),
            folder.join("cublas64_13.dll"),
            folder.join("cublasLt64_13.dll"),
        ] {
            let output = Command::new("icacls.exe").arg(&path).output().unwrap();
            assert!(output.status.success(), "{}", path.display());
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(
                text.contains(&sid) || text.contains("Straydog.Worker"),
                "{}: {text}",
                path.display()
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_new_engine_must_confirm_the_model_before_it_can_be_selected() {
        let data = tempfile::tempdir().unwrap();
        let source = data.path().join("fake.rs");
        fs::write(&source, r#"
            fn main() {
                let args: Vec<String> = std::env::args().collect();
                if args.iter().any(|arg| arg == "--version") { println!("straydog 0.1.11"); return }
                assert!(args.iter().any(|arg| arg == "--check-model"));
                assert!(args.iter().any(|arg| arg == "--config"));
                let name = std::env::current_exe().unwrap().file_stem().unwrap().to_string_lossy().to_string();
                match name.as_str() {
                    "ready" => println!("Straydog model ready: Qwen3.8-Flash-Next / ISTA-DASLab / IQ2_XS in C:\\models (3 files)"),
                    "missing" => { eprintln!("verification tree is missing"); std::process::exit(1) }
                    _ => std::process::exit(2),
                }
            }
        "#).unwrap();
        let ready = data.path().join("ready.exe");
        let status = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .arg(&source)
            .arg("-o")
            .arg(&ready)
            .status()
            .unwrap();
        assert!(status.success());
        let missing = data.path().join("missing.exe");
        let unknown = data.path().join("unknown.exe");
        fs::copy(&ready, &missing).unwrap();
        fs::copy(&ready, &unknown).unwrap();
        let settings = Settings::default();
        assert!(
            check_model(&ready, &settings, data.path()).is_ok(),
            "no model has been configured"
        );
        fs::write(engine::config_path(data.path()), "model = 'C:/models'\n").unwrap();
        assert!(check_model(&ready, &settings, data.path()).is_ok());
        assert!(check_model(&missing, &settings, data.path())
            .unwrap_err()
            .contains("verification tree is missing"));
        assert!(check_model(&unknown, &settings, data.path())
            .unwrap_err()
            .contains("Straydog"));
    }

    #[cfg(windows)]
    #[test]
    fn failed_digest_or_model_check_cannot_replace_the_running_engine() {
        let data = tempfile::tempdir().unwrap();
        let source = data.path().join("candidate.rs");
        fs::write(&source, r#"
            fn main() {
                let args: Vec<String> = std::env::args().collect();
                if args.iter().any(|arg| arg == "--version") { println!("straydog 0.1.11"); return }
                if args.iter().any(|arg| arg == "--check-model") {
                    let at = args.iter().position(|arg| arg == "--config").unwrap();
                    if std::fs::read_to_string(&args[at + 1]).unwrap().contains("missing") {
                        eprintln!("verification tree is missing"); std::process::exit(1);
                    }
                    println!("Straydog model ready: Qwen3.8-Flash-Next / ISTA-DASLab / IQ2_XS in C:\\models (3 files)"); return;
                }
                if args.iter().any(|arg| arg == "--dlls") { println!("cublas64_13.dll cublasLt64_13.dll"); }
            }
        "#).unwrap();
        let candidate_exe = data.path().join("candidate.exe");
        assert!(
            Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
                .arg(&source)
                .arg("-o")
                .arg(&candidate_exe)
                .status()
                .unwrap()
                .success()
        );
        let old = data.path().join("installed");
        fs::create_dir(&old).unwrap();
        let running = old.join("straydog.exe");
        fs::copy(&candidate_exe, &running).unwrap();
        for name in ["cublas64_13.dll", "cublasLt64_13.dll"] {
            fs::write(old.join(name), b"MZ").unwrap();
        }
        assert!(imported_dll(&candidate_exe, b"cublas64_").as_deref() == Some("cublas64_13.dll"));
        let hash = sha256(&candidate_exe).unwrap();
        let sums = format!("{hash}  {EXE}\n");
        let release = Candidate {
            version: "0.1.11".into(),
            exe: Setup {
                url: EXE.into(),
                size: fs::metadata(&candidate_exe).unwrap().len(),
                sha256: hash.clone(),
                parts: None,
            },
            sums: Setup {
                url: SUMS.into(),
                size: sums.len() as u64,
                sha256: format!("{:x}", Sha256::digest(sums.as_bytes())),
                parts: None,
            },
            zip: None,
        };
        let settings = Settings::default();
        fs::write(engine::config_path(data.path()), "model = 'missing'\n").unwrap();
        let stop = AtomicBool::new(false);
        let mut corrupted = |asset: &Setup, path: &Path, _: &AtomicBool| {
            fs::write(
                path,
                if asset.url == SUMS {
                    sums.as_bytes()
                } else {
                    b"corrupt"
                },
            )
            .unwrap();
            if asset.url == EXE {
                Err(updates::Failed::DidNotMatch)
            } else {
                Ok(())
            }
        };
        assert!(matches!(
            stage_with(
                &release,
                &running,
                &settings,
                data.path(),
                &stop,
                &mut corrupted
            ),
            Result::Failed(_)
        ));
        assert!(selected(data.path()).is_none());
        assert!(!root(data.path()).join("v0.1.11.partial").exists());
        assert!(running.is_file());
        let mut fetched = |asset: &Setup, path: &Path, _: &AtomicBool| {
            if asset.url == SUMS {
                fs::write(path, &sums).unwrap()
            } else {
                fs::copy(&candidate_exe, path).unwrap();
            }
            Ok(())
        };
        assert!(matches!(
            stage_with(
                &release,
                &running,
                &settings,
                data.path(),
                &stop,
                &mut fetched
            ),
            Result::ModelNotReady(_)
        ));
        assert!(selected(data.path()).is_none());
        assert!(root(data.path()).join("v0.1.11/straydog.exe").is_file());
        fs::write(engine::config_path(data.path()), "model = 'ready'\n").unwrap();
        let mut no_download =
            |_: &Setup, _: &Path, _: &AtomicBool| -> std::result::Result<(), updates::Failed> {
                panic!("verified candidate should be reused")
            };
        assert!(matches!(
            stage_with(
                &release,
                &running,
                &settings,
                data.path(),
                &stop,
                &mut no_download
            ),
            Result::Ready(_)
        ));
        assert!(selected(data.path()).is_some());
        let attempted = selected(data.path()).unwrap();
        assert!(rollback_if_pending(data.path(), &attempted));
        assert!(selected(data.path()).is_none());
        assert!(running.is_file());
    }

    /// Run only against an explicitly prepared, isolated DOGMIND_DATA and model.
    #[cfg(windows)]
    #[test]
    #[ignore = "requires a real Windows CUDA engine and local model"]
    fn live_staged_engine_connect_and_rollback() {
        dogmind_agent::sandbox::no_error_boxes();
        let data = PathBuf::from(std::env::var_os("DOGMIND_DATA").expect("DOGMIND_DATA"));
        let app = PathBuf::from(std::env::var_os("DOGMIND_LIVE_APP").expect("DOGMIND_LIVE_APP"));
        let case = std::env::var("DOGMIND_LIVE_CASE").expect("DOGMIND_LIVE_CASE");
        let settings = Settings::load(&data.join("settings.json"));
        let attempted = selected(&data).expect("selected staged executable");
        let start_and_wait = |exe: &Path| -> std::result::Result<(), String> {
            let start = Instant::now();
            let keep_going = || start.elapsed() < Duration::from_secs(180);
            let mut say = |words: String| println!("engine status: {words}");
            let engine = engine::start_alone(exe, &settings, &data, &keep_going, &mut say)?;
            println!("engine started: {} port={}", exe.display(), engine.port);
            let client = engine::client(engine.port);
            engine::wait_ready(&client, &keep_going, &mut say)?;
            let info = client.describe()?;
            assert!(info.straydog, "the server must identify as Straydog");
            println!("engine connected: {} port={}", exe.display(), engine.port);
            drop(engine);
            Ok(())
        };
        match case.as_str() {
            "ready" => {
                start_and_wait(&attempted).unwrap();
                mark_ready(&data, &attempted);
                assert!(!root(&data).join(PENDING).exists());
                println!("ready marker cleared; selected={}", attempted.display());
            }
            "rollback" => {
                let failure = start_and_wait(&attempted).unwrap_err();
                println!("staged engine failed: {failure}");
                assert!(rollback_if_pending(&data, &attempted));
                assert_eq!(
                    fs::read_to_string(root(&data).join(FAILED)).unwrap(),
                    "v0.1.11"
                );
                assert!(selected(&data).is_none());
                let bundled = app.join("straydog.exe");
                start_and_wait(&bundled).unwrap();
                println!(
                    "bundled engine connected after rollback: {}",
                    bundled.display()
                );
            }
            other => panic!("unknown live test case: {other}"),
        }
    }
}
