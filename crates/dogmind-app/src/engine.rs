//! Straydog as a program beside this one. The app knows it by its command line and its HTTP API, and nothing of what is
//! inside it, so that a new `straydog.exe` is all an update takes.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use std::time::Duration;

use dogmind_agent::llm::{Client, Health};
use serde_json::Value;

use crate::settings::Settings;

/// `straydog.exe`: the one the settings name, beside this executable, or on PATH.
pub fn find(settings: &Settings) -> Option<PathBuf> {
    let beside = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|folder| folder.join("straydog.exe")));
    let on_path = std::env::var_os("PATH").and_then(|path| std::env::split_paths(&path).map(|folder| folder.join("straydog.exe")).find(|file| file.is_file()));
    settings.straydog.clone().filter(|file| file.is_file()).or(beside.filter(|file| file.is_file())).or(on_path)
}

/// The bundles this app offers: the GSQ-RCO ones, by ISTA-DASLab. `straydog` itself still has the others, for who wants them.
/// Straydog does not say where a bundle comes from, so a new one appears here when a line says so; `crates/straydog-engine/src/assets*.json`
/// there name each bundle's repository.
fn origin(model: &str, quant: &str) -> Option<String> {
    match model {
        "Qwen3.8-Flash-Next" => Some("by Qwen, quantized by ISTA-DASLab".to_string()),
        "DeepSeek-V4.1-Flash" if quant.starts_with("GSQ-RCO") => Some("by DeepSeek, quantized by ISTA-DASLab".to_string()),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatalogEntry {
    /// `None`: Straydog picks the best for this PC.
    pub quant: Option<String>,
    pub name: String,
    /// Whose model it is, and who made this file of it.
    pub origin: String,
    pub detail: String,
    /// The most this model takes on this machine, when Straydog says.
    pub context: Option<u64>,
}

/// The bundles `straydog fetch --help` offers: `Model: QUANT (68.0 GB), QUANT (75.8 GB); Model: ...`. What it does not say
/// is not offered, and Straydog's own choice always is.
pub fn parse_catalog(help: &str) -> Vec<CatalogEntry> {
    let mut entries = vec![CatalogEntry { quant: None, name: "The best for this PC".into(), origin: String::new(), detail: "Straydog looks at this machine and chooses".into(), context: None }];
    for line in help.lines().filter(|line| line.contains(" GB)")) {
        let line = line.split(". ").next().unwrap_or(line);
        for part in line.split(';') {
            let Some((model, bundles)) = part.split_once(':') else { continue };
            // The first model follows the option's own name on its line.
            let model = model.split_whitespace().last().unwrap_or("");
            for bundle in bundles.split("),") {
                let bundle = bundle.trim().trim_end_matches(')');
                let Some((quant, size)) = bundle.split_once(" (") else { continue };
                if quant.is_empty() || quant.contains(' ') || !size.ends_with("GB") {
                    continue;
                }
                if let Some(origin) = origin(model, quant) {
                    entries.push(CatalogEntry { quant: Some(quant.to_string()), name: format!("{model} {quant}"), origin, detail: size.to_string(), context: None });
                }
            }
        }
    }
    entries
}

fn hidden(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
}

/// What `straydog setup --list` says of the bundles this app offers, the recommended first: `None` when it is not that JSON
/// (an older `straydog.exe`).
pub fn parse_list(json: &str) -> Option<Vec<CatalogEntry>> {
    let value: Value = serde_json::from_str(json.trim()).ok()?;
    let word = |item: &Value, name: &str| item.get(name).and_then(Value::as_str).unwrap_or("").to_string();
    let mut entries = Vec::new();
    for bundle in value.get("bundles")?.as_array()? {
        let quant = word(bundle, "quant");
        if quant.is_empty() || !bundle.get("runs").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let Some(origin) = origin(&word(bundle, "model"), &quant) else { continue };
        let gigabytes = bundle.get("download_bytes").and_then(Value::as_u64).unwrap_or(0) as f64 / 1e9;
        let recommended = bundle.get("recommended").and_then(Value::as_bool).unwrap_or(false);
        let entry = CatalogEntry {
            name: format!("{} {quant}", word(bundle, "model")),
            origin,
            detail: format!("{}{gigabytes:.1} GB  ·  {}", if recommended { "Recommended for this PC  ·  " } else { "" }, word(bundle, "note")),
            quant: Some(quant),
            context: bundle.get("context").and_then(Value::as_u64),
        };
        if recommended {
            entries.insert(0, entry);
        } else {
            entries.push(entry);
        }
    }
    Some(entries)
}

/// What `straydog --version` says.
pub fn version(straydog: &Path) -> Option<String> {
    let mut command = Command::new(straydog);
    command.arg("--version").stdin(Stdio::null());
    hidden(&mut command);
    let output = command.output().ok()?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string()).filter(|text| !text.is_empty())
}

pub fn load_catalog(straydog: &Path) -> Result<Vec<CatalogEntry>, String> {
    let run = |args: &[&str]| {
        let mut command = Command::new(straydog);
        command.args(args).stdin(Stdio::null());
        hidden(&mut command);
        command.output().map_err(|error| format!("{}: {error}", straydog.display()))
    };
    let listed = run(&["setup", "--list"])?;
    if listed.status.success() {
        if let Some(entries) = parse_list(&String::from_utf8_lossy(&listed.stdout)).filter(|entries| !entries.is_empty()) {
            return Ok(entries);
        }
    }
    Ok(parse_catalog(&String::from_utf8_lossy(&run(&["fetch", "--help"])?.stdout)))
}

/// The engine's settings file, this app's own: a `straydog` server the person runs keeps its own.
pub fn config_path(folder: &Path) -> PathBuf {
    folder.join("engine.toml")
}

pub fn log_path(folder: &Path) -> PathBuf {
    folder.join("engine.log")
}

pub fn is_set_up(folder: &Path) -> bool {
    config_path(folder).is_file()
}

pub fn engine_arguments(settings: &Settings, folder: &Path) -> Vec<String> {
    let mut args: Vec<String> = vec!["--config".into(), config_path(folder).display().to_string(),
        "--host".into(), "127.0.0.1".into(), "--port".into(), settings.port.to_string(), "--log-file".into(), log_path(folder).display().to_string()];
    if let Some(vram) = settings.vram {
        args.extend(["--vram".to_string(), vram.to_string()]);
    }
    if let Some(context) = settings.context {
        args.extend(["--ctx-size".to_string(), context.to_string()]);
    }
    args
}

pub fn setup_arguments(quant: Option<&str>, folder: &Path, config: &Path, drafter: bool) -> Vec<String> {
    let mut args: Vec<String> = vec!["setup".into(), "--yes".into(), "--dir".into(), folder.display().to_string(), "--config".into(), config.display().to_string()];
    if let Some(quant) = quant {
        args.extend(["--quant".to_string(), quant.to_string()]);
    }
    if !drafter {
        args.push("--no-mtp".into());
    }
    args
}

/// Ends with this app, whichever way it ends, with everything it started.
struct Job(windows_sys::Win32::Foundation::HANDLE);

// A handle is valid on any thread.
unsafe impl Send for Job {}

impl Job {
    fn holding(child: &Child) -> Result<Job, String> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::*;
        // SAFETY: the structure is as long as the call is told, and the handles are live.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err("no Job Object".into());
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(job, JobObjectExtendedLimitInformation, std::ptr::addr_of!(limits).cast(), std::mem::size_of_val(&limits) as u32);
            AssignProcessToJobObject(job, child.as_raw_handle() as _);
            Ok(Job(job))
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle is this value's; closing it ends what is in the job.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

pub struct Engine {
    child: Child,
    _job: Option<Job>,
}

impl Engine {
    pub fn start(straydog: &Path, settings: &Settings, folder: &Path) -> Result<Engine, String> {
        std::fs::create_dir_all(folder).map_err(|error| format!("{}: {error}", folder.display()))?;
        let mut command = Command::new(straydog);
        command.args(engine_arguments(settings, folder)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        hidden(&mut command);
        let child = command.spawn().map_err(|error| format!("{}: {error}", straydog.display()))?;
        let job = Job::holding(&child).ok();
        Ok(Engine { child, _job: job })
    }

    /// Why it ended, if it has.
    pub fn ended(&mut self) -> Option<String> {
        let status = self.child.try_wait().ok()??;
        let mut said = String::new();
        if let Some(stderr) = &mut self.child.stderr {
            let _ = stderr.read_to_string(&mut said);
        }
        let last = said.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("").to_string();
        Some(if last.is_empty() { format!("the engine ended ({status})") } else { last })
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn client(settings: &Settings) -> Client {
    Client::new(&format!("http://127.0.0.1:{}/v1", settings.port), None).expect("the URL is well formed")
}

/// What `/health`'s loading progress says, in a line.
pub fn progress_text(progress: &Value) -> String {
    let stage = progress.get("stage").and_then(Value::as_str).unwrap_or("loading");
    match (progress.get("done").and_then(Value::as_f64), progress.get("total").and_then(Value::as_f64)) {
        (Some(done), Some(total)) if total > 0.0 => format!("{stage} {:.0}%", 100.0 * done / total),
        _ => stage.to_string(),
    }
}

/// Waits for the server. `say` hears its loading; ends when it is ready or cannot be.
pub fn wait_ready(client: &Client, keep_going: &dyn Fn() -> bool, say: &mut dyn FnMut(String)) -> Result<(), String> {
    let mut reached = false;
    while keep_going() {
        match client.health() {
            Ok(Health::Ready) => return Ok(()),
            Ok(Health::Loading(progress)) => {
                reached = true;
                say(progress_text(&progress));
            }
            Ok(Health::Failed(message)) => return Err(message),
            Err(_) if reached => return Err("the engine stopped answering".into()),
            Err(_) => say("starting the engine".to_string()),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err("stopped".into())
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetupLine {
    Progress { stage: String, done: f64, total: f64 },
    Text(String),
}

/// A line of `straydog setup`: its progress reads `stage: 1.2/3.4 GB`.
pub fn parse_setup_line(line: &str) -> SetupLine {
    let parsed = line.strip_suffix(" GB").and_then(|rest| {
        let (stage, numbers) = rest.rsplit_once(": ")?;
        let (done, total) = numbers.split_once('/')?;
        Some(SetupLine::Progress { stage: stage.to_string(), done: done.parse().ok()?, total: total.parse().ok()? })
    });
    parsed.unwrap_or_else(|| SetupLine::Text(line.to_string()))
}

/// Runs `straydog setup`, sending each line of it; `Ok` when the model is ready to serve. `cancel` ends it and all it started;
/// what it downloaded stays, and the next run goes on from there.
pub fn run_setup(straydog: &Path, args: &[String], lines: &Sender<SetupLine>, wake: &(dyn Fn() + Send + Sync), cancel: &AtomicBool) -> Result<(), String> {
    let mut command = Command::new(straydog);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    hidden(&mut command);
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let job = Mutex::new(Job::holding(&child).ok());
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    let mut last_error = String::new();
    let finished = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let (job, finished, running) = (&job, &finished, &mut child);
        scope.spawn(move || {
            while !finished.load(Ordering::Relaxed) {
                if cancel.load(Ordering::Relaxed) {
                    let _ = running.kill();
                    // Closing the job ends what straydog started, which holds the pipes open too.
                    drop(job.lock().map(|mut held| held.take()));
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let out = lines.clone();
        scope.spawn(move || {
            let mut text = Vec::new();
            let _ = stdout.read_to_end(&mut text);
            for line in String::from_utf8_lossy(&text).lines() {
                let _ = out.send(SetupLine::Text(line.to_string()));
            }
            wake();
        });
        // Progress rewrites its line with `\r`.
        let mut pending = Vec::new();
        let mut block = [0u8; 4096];
        while let Ok(count) = stderr.read(&mut block) {
            if count == 0 {
                break;
            }
            pending.extend_from_slice(&block[..count]);
            while let Some(at) = pending.iter().position(|&byte| byte == b'\r' || byte == b'\n') {
                let line: Vec<u8> = pending.drain(..=at).collect();
                let line = String::from_utf8_lossy(&line[..line.len() - 1]).trim().to_string();
                if !line.is_empty() {
                    let parsed = parse_setup_line(&line);
                    if matches!(parsed, SetupLine::Text(_)) {
                        last_error = line;
                    }
                    let _ = lines.send(parsed);
                    wake();
                }
            }
        }
        finished.store(true, Ordering::Relaxed);
    });
    let status = child.wait().map_err(|error| error.to_string())?;
    drop(job);
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".to_string());
    }
    if status.success() { Ok(()) } else { Err(if last_error.is_empty() { format!("setup ended ({status})") } else { last_error }) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_bundles_come_from_straydogs_own_help() {
        let help = "Download and authenticate a model bundle\n\nOptions:\n  -m, --model <DIR>  The bundle directory\n      --quant <NAME>  Qwen3.8-Flash-Next: Q2_0 (66.4 GB), IQ2_XS (68.0 GB); DeepSeek-V4.1-Flash: GSQ-RCO-IQ2_XXS (248.8 GB); Qwen3.6-35B-A3B: UD-Q8_K_XL (38.5 GB). `straydog setup` chooses for you\n";
        let entries = parse_catalog(help);
        assert_eq!(entries[0].quant, None);
        let quants: Vec<&str> = entries.iter().filter_map(|entry| entry.quant.as_deref()).collect();
        assert_eq!(quants, ["Q2_0", "IQ2_XS", "GSQ-RCO-IQ2_XXS"]);
        assert_eq!(entries[2].detail, "68.0 GB");
        assert_eq!(entries[2].name, "Qwen3.8-Flash-Next IQ2_XS");
        assert_eq!(parse_catalog("something else entirely").len(), 1, "an unknown help still offers Straydog's choice");
    }

    #[test]
    fn the_list_puts_the_recommended_first_and_leaves_out_what_cannot_run() {
        let list = r#"{"machine":{"backend":"cuda"},"bundles":[
            {"model":"Qwen3.8-Flash-Next","quant":"IQ3_S","download_bytes":83600000000,"runs":true,"recommended":false,"note":"slow"},
            {"model":"Qwen3.8-Flash-Next","quant":"IQ2_XS","download_bytes":68000000000,"runs":true,"recommended":true,"note":"fits","context":262144},
            {"model":"DeepSeek-V4.1-Flash","quant":"UD-IQ2_XXS","download_bytes":300500000000,"runs":false,"recommended":false,"note":"too little VRAM"}]}"#;
        let entries = parse_list(list).unwrap();
        let quants: Vec<&str> = entries.iter().filter_map(|entry| entry.quant.as_deref()).collect();
        assert_eq!(quants, ["IQ2_XS", "IQ3_S"]);
        assert!(entries[0].detail.starts_with("Recommended for this PC  ·  68.0 GB") && !entries[1].detail.contains("Recommended"));
        assert_eq!(entries[0].origin, "by Qwen, quantized by ISTA-DASLab");
        assert_eq!(entries[0].context, Some(262144));
        assert!(parse_list("error: unexpected argument '--list'").is_none());
        assert!(parse_list("{}").is_none());
    }

    #[test]
    fn only_the_gsq_rco_bundles_are_offered() {
        let bundle = |model: &str, quant: &str| format!(r#"{{"model":"{model}","quant":"{quant}","download_bytes":1,"runs":true,"recommended":false,"note":"n"}}"#);
        let list = format!(r#"{{"bundles":[{},{},{},{},{},{},{},{}]}}"#,
            bundle("DeepSeek-V4.1-Flash", "GSQ-RCO-IQ2_XXS"), bundle("DeepSeek-V4.1-Flash", "GSQ-RCO-IQ2_XS"), bundle("DeepSeek-V4.1-Flash", "UD-IQ2_XXS"),
            bundle("Qwen3.8-Flash-Next", "IQ3_S"), bundle("Qwen3.8-Flash-Next", "IQ3_XXS"), bundle("Qwen3.8-Flash-Next", "IQ2_XS"), bundle("Qwen3.8-Flash-Next", "Q2_0"),
            bundle("Qwen3.6-35B-A3B", "UD-Q8_K_XL"));
        let quants: Vec<String> = parse_list(&list).unwrap().into_iter().filter_map(|entry| entry.quant).collect();
        assert_eq!(quants, ["GSQ-RCO-IQ2_XXS", "GSQ-RCO-IQ2_XS", "IQ3_S", "IQ3_XXS", "IQ2_XS", "Q2_0"]);
    }

    #[test]
    fn a_setup_can_be_cancelled_with_all_it_started() {
        let (lines, _seen) = std::sync::mpsc::channel();
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            flag.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();
        // A command that would take a minute, through a shell, as what is started by what was started must end too.
        let args = ["/c", "ping", "-n", "60", "127.0.0.1"].map(String::from);
        let result = run_setup(Path::new("cmd.exe"), &args, &lines, &|| {}, &cancel);
        assert_eq!(result, Err("cancelled".to_string()));
        assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
    }

    #[test]
    fn progress_lines_are_told_from_the_rest() {
        assert_eq!(parse_setup_line("IQ2_XS: 1.5/68.0 GB"), SetupLine::Progress { stage: "IQ2_XS".into(), done: 1.5, total: 68.0 });
        assert_eq!(parse_setup_line("verified a: b"), SetupLine::Text("verified a: b".into()));
        assert_eq!(parse_setup_line("not enough space: 5/x GB"), SetupLine::Text("not enough space: 5/x GB".into()));
    }

    #[test]
    fn the_engine_is_started_with_this_apps_settings() {
        let settings = Settings { vram: Some(5.0), context: Some(262144), ..Settings::default() };
        let args = engine_arguments(&settings, Path::new("D"));
        assert!(args.windows(2).any(|pair| pair == ["--vram", "5"]), "{args:?}");
        assert!(args.windows(2).any(|pair| pair == ["--ctx-size", "262144"]), "{args:?}");
        assert!(args.windows(2).any(|pair| pair[0] == "--host" && pair[1] == "127.0.0.1"), "only this machine reaches it: {args:?}");
        let plain = engine_arguments(&Settings::default(), Path::new("D"));
        assert!(!plain.iter().any(|arg| arg == "--vram" || arg == "--ctx-size"));
    }

    #[test]
    fn setup_asks_nothing_and_writes_this_apps_file() {
        let args = setup_arguments(Some("IQ2_XS"), Path::new("M"), Path::new("C"), false);
        assert_eq!(&args[..2], ["setup", "--yes"]);
        assert!(args.contains(&"--no-mtp".to_string()) && args.contains(&"IQ2_XS".to_string()));
        let chosen = setup_arguments(None, Path::new("M"), Path::new("C"), true);
        assert!(!chosen.contains(&"--no-mtp".to_string()) && !chosen.contains(&"--quant".to_string()));
    }

    #[test]
    fn loading_reads_in_a_line() {
        assert_eq!(progress_text(&json!({"stage": "weights", "done": 1, "total": 4})), "weights 25%");
        assert_eq!(progress_text(&json!(null)), "loading");
    }
}
