//! Straydog as a program beside this one. The app knows it by its command line and its HTTP API, and nothing of what is
//! inside it, so that a new `straydog.exe` is all an update takes.

use crate::lang::{t, tf};
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

const QWEN: &str = "Qwen3.8-Flash-Next";
const DEEPSEEK: &str = "DeepSeek-V4.1-Flash-1017";
const ISTA_DASLAB: &str = "ISTA-DASLab";
const UNSLOTH: &str = "Unsloth";

/// A model bundle by Straydog's three labels: its model, who quantized it, and its quantization. None of the three names a bundle alone: Qwen3.8 and
/// DeepSeek each have a `UD-IQ1_M`, and ISTA-DASLab and Unsloth may each quantize a model to a name the other has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    /// `model` and `provider` are `None` only for settings an older setup wrote of a bundle that `legacy_labels` does not know: such a bundle is
    /// found, and set up, by its quantization alone.
    pub model: Option<String>,
    pub provider: Option<String>,
    pub quant: String,
}

impl Bundle {
    pub fn new(model: &str, provider: &str, quant: &str) -> Bundle {
        Bundle { model: Some(model.to_string()), provider: Some(provider.to_string()), quant: quant.to_string() }
    }

    /// The bundle the settings name, in `model_name`, `provider` and `quant`. Settings written up to 0.1.3 have no `model_name` and no `provider`, and
    /// the quantization names of those versions: `legacy_labels` reads them.
    pub fn from_settings(model_name: Option<&str>, provider: Option<&str>, quant: &str) -> Bundle {
        match model_name.is_none().then(|| legacy_labels(quant)).flatten() {
            Some((model, old_provider, quant)) => Bundle::new(model, provider.unwrap_or(old_provider), quant),
            None => Bundle { model: model_name.map(str::to_string), provider: provider.map(str::to_string), quant: quant.to_string() },
        }
    }

    /// Whether `set_up`, what the settings name, is this bundle. A label that one of the two lacks is not compared.
    pub fn is(&self, set_up: &Bundle) -> bool {
        let same = |mine: &Option<String>, theirs: &Option<String>| mine.is_none() || theirs.is_none() || mine == theirs;
        self.quant == set_up.quant && same(&self.model, &set_up.model) && same(&self.provider, &set_up.provider)
    }

    /// What a person reads as its name: the labels it has, in order.
    fn name(&self) -> String {
        [self.model.as_deref(), self.provider.as_deref(), Some(self.quant.as_str())].into_iter().flatten().collect::<Vec<_>>().join(" · ")
    }

    /// What `setup` is given to choose it: each label that is known.
    fn arguments(&self) -> Vec<String> {
        let labels = [("--model-name", self.model.as_deref()), ("--provider", self.provider.as_deref()), ("--quant", Some(self.quant.as_str()))];
        labels.into_iter().filter_map(|(flag, value)| Some([flag.to_string(), value?.to_string()])).flatten().collect()
    }
}

/// A bundle named by its quantization alone, as settings written up to 0.1.3 name it, by the three labels Straydog gives it now, with the quantization
/// under its name now. This table is frozen at 0.1.3 and never grows: a setup after it writes all three labels, and no other bundle was named by the
/// quantization alone.
fn legacy_labels(quant: &str) -> Option<(&str, &str, &str)> {
    Some(match quant {
        "GSQ-RCO-IQ2_XS" => (DEEPSEEK, ISTA_DASLAB, "IQ2_XS"),
        "GSQ-RCO-IQ2_XXS" => (DEEPSEEK, ISTA_DASLAB, "IQ2_XXS"),
        // (Qwen had no UD-IQ1_M before 0.1.3: an old one is DeepSeek's)
        "UD-IQ2_XXS" | "UD-IQ1_M" => (DEEPSEEK, UNSLOTH, quant),
        "Q2_0" | "IQ2_XS" | "IQ3_XXS" | "IQ3_S" => (QWEN, ISTA_DASLAB, quant),
        _ => return None,
    })
}

/// Whose model it is, and who made this file of it: its `creator` as Straydog says it (none, for a bundle that has none), and who quantized it.
fn origin(creator: &str, provider: &str) -> String {
    if creator.is_empty() { format!("quantized by {provider}") } else { format!("by {creator}, quantized by {provider}") }
}

/// A model's own terms, for the person to read before the download, as Straydog says them of the bundle: the licence's name, and the address of its terms
/// (a quantized file has its base model's).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelLicence {
    pub name: String,
    pub terms: String,
}

/// The licence a bundle says it has, in `license` and `license_url`. A bundle that says neither has no line, as Straydog's own choice has none; nor has one
/// whose address is not an `https` one, for the address is opened in the browser.
fn licence_in(bundle: &Value) -> Option<ModelLicence> {
    let word = |name: &str| bundle.get(name).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let (name, terms) = (word("license"), word("license_url"));
    (!name.is_empty() && terms.starts_with("https://")).then_some(ModelLicence { name, terms })
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatalogEntry {
    /// `None`: Straydog picks the best for this PC.
    pub bundle: Option<Bundle>,
    /// The bundle's labels, `model · provider · quant`.
    pub name: String,
    /// Whose model it is, and who made this file of it.
    pub origin: String,
    pub detail: String,
    /// The most this model takes on this machine, when Straydog says.
    pub context: Option<u64>,
    /// What it takes on the drive once it is all there, when Straydog says.
    pub disk_bytes: Option<u64>,
    /// `None` for Straydog's own choice, which is not yet a model.
    pub licence: Option<ModelLicence>,
    /// What it all takes to download, and of that the vision file and the drafter, when Straydog says.
    pub download_bytes: Option<u64>,
    pub vision_bytes: Option<u64>,
    pub drafter_bytes: Option<u64>,
    /// Straydog says it runs on this PC as it is now. One that does not is short of graphics memory: shown greyed, and set up only when chosen.
    pub runs: bool,
}

impl CatalogEntry {
    /// Whether this is the bundle set up, by what the settings name.
    pub fn is(&self, set_up: &Bundle) -> bool {
        self.bundle.as_ref().is_some_and(|bundle| bundle.is(set_up))
    }

    /// The line before a long download, as a link to the terms.
    pub fn licence_line(&self) -> Option<String> {
        self.licence.as_ref().map(|licence| tf("This model's licence: {0}. Read its terms", &[&licence.name]))
    }
}

/// Why this PC cannot run a model itself, for the person who has to do something about it.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// There is no NVIDIA driver: another maker's GPU, or none.
    NoNvidia,
    /// An NVIDIA driver is there and CUDA refuses it, or finds no card: what it said.
    Driver(String),
    /// A GPU, with less memory than the smallest model takes: both as Straydog says them.
    TooLittleMemory { needs: String, has: String },
    /// A card this build of the engine has no code for: a driver would not change that.
    OldCard,
    /// `straydog.exe` does not begin at all, and why.
    DoesNotStart(String),
}

impl Refusal {
    /// In a line of a list.
    pub fn short(&self) -> &'static str {
        match self {
            Refusal::NoNvidia => t("Cannot run here: this PC has no NVIDIA GPU"),
            Refusal::Driver(_) => t("Cannot run here yet: the NVIDIA driver needs updating"),
            Refusal::TooLittleMemory { .. } => t("Cannot run here: too little graphics memory"),
            Refusal::OldCard => t("Cannot run here: this GPU is too old"),
            Refusal::DoesNotStart(_) => t("Straydog cannot start on this PC"),
        }
    }

    /// In full, with what to do.
    pub fn words(&self) -> String {
        match self {
            Refusal::NoNvidia => t("Straydog needs an NVIDIA GPU with its driver, and this PC has none (an AMD or Intel GPU does not work). You can still use a model on another computer, an online service, Codex or Claude.").to_string(),
            Refusal::Driver(said) => tf("CUDA cannot use this PC's NVIDIA GPU: {0}. Install the newest driver from nvidia.com and try again, or use a model on another computer, an online service, Codex or Claude.", &[&said]),
            Refusal::TooLittleMemory { needs, has } => tf("These models need {0} of graphics memory, and this PC's GPU has {1}. You can still use a model on another computer, an online service, Codex or Claude.", &[&needs, &has]),
            Refusal::OldCard => t("Straydog runs on an NVIDIA GeForce RTX 20 series or newer, and this PC's GPU is older. You can still use a model on another computer, an online service, Codex or Claude.").to_string(),
            Refusal::DoesNotStart(why) => why.clone(),
        }
    }
}

/// What Straydog says of this PC and what it can run on it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Catalog {
    pub entries: Vec<CatalogEntry>,
    /// This PC's GPU and memory in a line, when Straydog says.
    pub machine: String,
    /// When nothing can run here. `entries` has only bundles short of graphics memory then, if any.
    pub refusal: Option<Refusal>,
}

/// The bundles `straydog fetch --help` offers, for a Straydog without `setup --list`: `Model: QUANT (68.0 GB), QUANT (75.8 GB); Model: ...`, in the
/// quantization names of 0.1.3 and before, which `legacy_labels` reads. What it does not say or does not read is not offered, and Straydog's own choice
/// always is. It says no creator and no licence of a bundle.
pub fn parse_catalog(help: &str) -> Vec<CatalogEntry> {
    let mut entries = vec![CatalogEntry { download_bytes: None, vision_bytes: None, drafter_bytes: None, bundle: None, name: t("The best for this PC").into(), origin: String::new(), detail: t("Straydog looks at this machine and chooses").into(), context: None, disk_bytes: None, licence: None, runs: true }];
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
                // (the model the help names is the table's, under the name it had then: `DeepSeek-V4.1-Flash` is `DeepSeek-V4.1-Flash-1017`'s start)
                let Some((labelled, provider, quant)) = legacy_labels(quant).filter(|(labelled, ..)| labelled.starts_with(model)) else { continue };
                let bundle = Bundle::new(labelled, provider, quant);
                entries.push(CatalogEntry { download_bytes: None, vision_bytes: None, drafter_bytes: None, name: bundle.name(), bundle: Some(bundle), origin: origin("", provider), detail: size.to_string(), context: None, disk_bytes: None, licence: None, runs: true });
            }
        }
    }
    entries
}

fn hidden(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
}

/// The size of a download or of what is on a drive, in the units Straydog gives sizes in (10^9 bytes, which it writes "GB": its help, its progress and its words of the disk), so that the list of
/// models, the progress and Straydog's own words agree on one number. (Explorer counts a file in 2^30, and shows the same file as a smaller number.)
pub fn gb(bytes: u64) -> String {
    format!("{:.0} GB", bytes as f64 / 1e9)
}

/// The size of a file of about a gigabyte, which `gb` would call nothing or one.
pub fn gb_file(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// The memory of the PC and of its graphics card as Windows counts it: in units of 2^30, which it writes "GB" (Task Manager, the box of the card): a card of 8 GiB reads 8 GB, and a PC of 32 GiB
/// 32 GB. (Straydog's words of memory, on the same line, are in GiB.)
pub fn memory_gb(bytes: u64) -> String {
    format!("{:.0} GB", bytes as f64 / (1u64 << 30) as f64)
}

/// What the verdict of `setup --list` means for the person choosing, in words of theirs: Straydog's own speak of its experts.
fn how_it_runs(fit: &str) -> &'static str {
    match fit {
        "vram" => t("Runs on the graphics card alone: fastest"),
        "ram" => t("Part of it waits in memory: a little slower"),
        "drive" => t("Part of it is read from the drive: slower"),
        _ => "",
    }
}

/// Straydog's words for too little graphics memory: `needs 10 GiB of VRAM; this machine has 6 GiB`, or as it says it now,
/// `needs 14.8 GiB of VRAM free, with what `auto` leaves for the desktop; this machine has 12.2 GiB for it`.
fn memory_short(note: &str) -> Option<(String, String)> {
    let (needs, rest) = note.strip_prefix("needs ")?.split_once(" of VRAM")?;
    let has = rest.split_once("this machine has ")?.1;
    let has = has.strip_suffix(" for it").unwrap_or(has);
    Some((needs.to_string(), has.to_string()))
}

/// Whether an NVIDIA driver is on this PC at all: CUDA says the same of an old driver and of none.
pub fn nvidia_driver_is_here() -> bool {
    // (walking the screens of a PC without one, on a PC with one)
    if cfg!(debug_assertions) && std::env::var_os("DOGMIND_TEST_NO_NVIDIA").is_some() {
        return false;
    }
    std::env::var_os("SystemRoot").is_some_and(|root| Path::new(&root).join("System32").join("nvcuda.dll").is_file())
}

/// What `straydog setup --list` says of this PC and of the bundles this app offers, the recommended first: `None` when it is not that JSON
/// (an older `straydog.exe`).
pub fn parse_list(json: &str, driver_here: bool) -> Option<Catalog> {
    let value: Value = serde_json::from_str(json.trim()).ok()?;
    let word = |item: &Value, name: &str| item.get(name).and_then(Value::as_str).unwrap_or("").to_string();
    let bundles = value.get("bundles")?.as_array()?;
    let mut entries = Vec::new();
    // Those short of graphics memory as this PC is now: after the rest, greyed and not recommended, and there to choose for whoever wants to
    // try (with what else holds the card closed, it may start).
    let mut short_entries = Vec::new();
    let mut shortest: Option<(f64, String, String)> = None;
    for bundle in bundles {
        let (model, provider, quant) = (word(bundle, "model"), word(bundle, "provider"), word(bundle, "quant"));
        // (a bundle with no provider is an older Straydog's: it has no name here)
        // (every bundle the list has is offered: which come first, and which is recommended, is Straydog's to say)
        if quant.is_empty() || provider.is_empty() {
            continue;
        }
        let runs = bundle.get("runs").and_then(Value::as_bool).unwrap_or(false);
        let short = if runs { None } else { memory_short(&word(bundle, "note")) };
        if let Some((needs, has)) = &short {
            let amount = needs.split(' ').next().and_then(|number| number.parse().ok()).unwrap_or(f64::MAX);
            if shortest.as_ref().is_none_or(|(least, ..)| amount < *least) {
                shortest = Some((amount, needs.clone(), has.clone()));
            }
        }
        // (one that needs a GPU this PC has not, or a newer one, is not offered at all)
        if !runs && short.is_none() {
            continue;
        }
        let download = bundle.get("download_bytes").and_then(Value::as_u64).unwrap_or(0);
        let disk_bytes = bundle.get("disk_bytes").and_then(Value::as_u64);
        let recommended = runs && bundle.get("recommended").and_then(Value::as_bool).unwrap_or(false);
        let size = match disk_bytes {
            Some(disk) => tf("{0} to download, {1} on the drive", &[&gb(download), &gb(disk)]),
            None => tf("{0} to download", &[&gb(download)]),
        };
        let how = match &short {
            Some((needs, has)) => tf("Not recommended: it needs {0} of graphics memory free, and this PC has {1} for it", &[needs, has]),
            None => how_it_runs(&word(bundle, "fit")).to_string(),
        };
        let extra = |name: &str| bundle.get("extras").and_then(Value::as_array).into_iter().flatten().find(|extra| word(extra, "name") == name).and_then(|extra| extra.get("download_bytes")).and_then(Value::as_u64);
        let named = Bundle::new(&model, &provider, &quant);
        let entry = CatalogEntry {
            download_bytes: Some(download),
            vision_bytes: extra("vision"),
            drafter_bytes: extra("mtp"),
            name: named.name(),
            licence: licence_in(bundle),
            origin: origin(&word(bundle, "creator"), &provider),
            detail: if short.is_some() {
                format!("{how}  ·  {size}")
            } else {
                format!("{}{size}{}{how}", if recommended { t("Recommended for this PC  ·  ") } else { "" }, if how.is_empty() { "" } else { "  ·  " })
            },
            bundle: Some(named),
            context: bundle.get("context").and_then(Value::as_u64),
            disk_bytes,
            runs,
        };
        if !runs {
            short_entries.push(entry);
        } else if recommended {
            entries.insert(0, entry);
        } else {
            entries.push(entry);
        }
    }
    let runnable = !entries.is_empty();
    // Unsloth's bundles first, the ones supported first; in each part, the recommended first and then the list's order (a stable sort)
    let unsloth_last = |entry: &CatalogEntry| !entry.bundle.as_ref().is_some_and(|bundle| bundle.provider.as_deref() == Some(UNSLOTH));
    entries.sort_by_key(unsloth_last);
    short_entries.sort_by_key(unsloth_last);
    entries.extend(short_entries);
    let machine = value.get("machine");
    let gpus: Vec<String> = machine.and_then(|machine| machine.get("gpus")).and_then(Value::as_array).into_iter().flatten().map(|gpu| {
        format!("{} ({})", word(gpu, "name"), memory_gb(gpu.get("total_bytes").and_then(Value::as_u64).unwrap_or(0)))
    }).collect();
    let ram = machine.and_then(|machine| machine.get("ram_bytes")).and_then(Value::as_u64).unwrap_or(0);
    // (what is free now, when Straydog says: what other programs hold decides whether a model's experts stay in memory)
    let free_ram = machine.and_then(|machine| machine.get("free_ram_bytes")).and_then(Value::as_u64);
    let mut parts = gpus.clone();
    if ram > 0 {
        parts.push(match free_ram {
            Some(free) => tf("{0} of memory, {1} free now", &[&memory_gb(ram), &memory_gb(free)]),
            None => tf("{0} of memory", &[&memory_gb(ram)]),
        });
    }
    // (said whether or not the list has some that are short: they are there to try, and not what this PC runs)
    let refusal = if runnable {
        None
    } else if gpus.is_empty() {
        let said = machine.map_or(String::new(), |machine| word(machine, "gpu_error"));
        Some(if said.contains(NO_CODE) { Refusal::OldCard } else if driver_here { Refusal::Driver(if said.is_empty() { t("it finds no GPU").to_string() } else { said }) } else { Refusal::NoNvidia })
    } else {
        shortest.map(|(_, needs, has)| Refusal::TooLittleMemory { needs, has })
    };
    Some(Catalog { entries, machine: parts.join("  ·  "), refusal })
}

/// What `straydog --version` says.
pub fn version(straydog: &Path) -> Option<String> {
    let mut command = Command::new(straydog);
    command.arg("--version").stdin(Stdio::null());
    hidden(&mut command);
    let output = command.output().ok()?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string()).filter(|text| !text.is_empty())
}

/// Windows ended a program before its first line: what that was, by the status it gave.
pub fn why_it_did_not_begin(code: Option<i32>) -> Option<String> {
    match code? as u32 {
        // STATUS_DLL_NOT_FOUND, and its library's entry point or image being wrong.
        0xC000_0135 => Some(t("A library Straydog needs is missing from its folder (cublas64_13.dll, cublasLt64_13.dll). Extract the whole zip again, and keep its files together.").to_string()),
        0xC000_0139 | 0xC000_007B | 0xC000_0142 => Some(t("A library beside Straydog is damaged or from another version. Extract the whole zip again, over the old folder.").to_string()),
        0xC000_0022 => Some(t("Windows refused to run straydog.exe (access denied). An antivirus or a policy on this PC may be blocking it.").to_string()),
        _ => None,
    }
}

/// Why a program could not be started at all, for a person: the ones Windows gives when something on the PC stands in the way.
pub fn start_failure(program: &Path, error: &std::io::Error) -> String {
    match error.raw_os_error() {
        // ERROR_VIRUS_INFECTED, ERROR_VIRUS_DELETED, ERROR_ACCESS_DISABLED_BY_POLICY, ERROR_SYSTEM_INTEGRITY_POLICY_VIOLATION, ERROR_ACCESS_DENIED
        Some(225 | 226 | 1260 | 4551 | 5) => tf("Windows refused to run {0}. Smart App Control, an antivirus or a policy on this PC may be blocking it: this app is not signed yet.", &[&program.display()]),
        _ => format!("{}: {error}", program.display()),
    }
}

/// Whether this program runs from a temporary folder: Explorer's way of running what is inside a zip, with nothing beside it.
fn started_inside_a_zip() -> bool {
    let exe = std::env::current_exe().ok().map(|exe| exe.display().to_string().to_lowercase());
    let temporary = std::env::var_os("TEMP").map(|folder| folder.to_string_lossy().to_lowercase());
    matches!((exe, temporary), (Some(exe), Some(temporary)) if !temporary.is_empty() && exe.starts_with(&temporary))
}

/// `straydog.exe` was not found: why that may be and what to do.
pub fn not_found_words() -> &'static str {
    if started_inside_a_zip() {
        t("This program was started from inside its zip file. Extract the whole zip to a folder, and start it there.")
    } else {
        t("straydog.exe was not found beside this app. Keep all the files of the zip together, or choose another way.")
    }
}

/// The same in a line of a list.
pub fn not_found_short() -> &'static str {
    if started_inside_a_zip() { t("Started from inside the zip: extract it first") } else { t("straydog.exe is not beside this app") }
}

/// What Straydog can run here. For another thread: it looks at the GPU, which takes a moment.
pub fn load_catalog(straydog: &Path) -> Result<Catalog, String> {
    let run = |args: &[&str]| {
        let mut command = Command::new(straydog);
        command.args(args).stdin(Stdio::null());
        hidden(&mut command);
        command.output().map_err(|error| start_failure(straydog, &error))
    };
    let listed = run(&["setup", "--list"])?;
    if let Some(why) = why_it_did_not_begin(listed.status.code()) {
        return Err(why);
    }
    if listed.status.success() {
        if let Some(catalog) = parse_list(&String::from_utf8_lossy(&listed.stdout), nvidia_driver_is_here()) {
            if !catalog.entries.is_empty() || catalog.refusal.is_some() {
                return Ok(catalog);
            }
        }
    }
    let help = run(&["fetch", "--help"])?;
    Ok(Catalog { entries: parse_catalog(&String::from_utf8_lossy(&help.stdout)), ..Catalog::default() })
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

/// For a model that is there and has no vision file: setup does not fetch again what it has, and checks it. The drafter stays as the settings have it, for setup rewrites them; a DeepSeek drafter that is not there is not wanted here.
pub fn vision_arguments(installed: &crate::vision::Installed, config: &Path) -> Vec<String> {
    let mut args = setup_arguments(Some(&installed.bundle), &installed.model, config, installed.drafter, true);
    // (the model is there: without this its shards are all read again, for one small file)
    args.extend(["--no-dspark".to_string(), "--vision-only".to_string()]);
    args
}

/// Whether this Straydog's setup takes `--force`: a bundle it finds too big for this PC, set up because the person chose it.
pub fn setup_can_force(straydog: &Path) -> bool {
    let mut command = Command::new(straydog);
    command.args(["setup", "--help"]).stdin(Stdio::null());
    hidden(&mut command);
    command.output().is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("--force"))
}

/// `bundle` names the bundle by all three labels, or only those it has (settings an older setup wrote); `None` leaves the choice to Straydog.
pub fn setup_arguments(bundle: Option<&Bundle>, folder: &Path, config: &Path, drafter: bool, vision: bool) -> Vec<String> {
    let mut args: Vec<String> = vec!["setup".into(), "--yes".into(), "--dir".into(), folder.display().to_string(), "--config".into(), config.display().to_string()];
    if let Some(bundle) = bundle {
        args.extend(bundle.arguments());
    }
    if !drafter {
        args.push("--no-mtp".into());
    }
    if !vision {
        args.push("--no-vision".into());
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
            // (held at once, so that every way out below closes it)
            let held = Job(job);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(job, JobObjectExtendedLimitInformation, std::ptr::addr_of!(limits).cast(), std::mem::size_of_val(&limits) as u32) == 0 {
                return Err(format!("the job's limit could not be set: {}", std::io::Error::last_os_error()));
            }
            if AssignProcessToJobObject(job, child.as_raw_handle() as _) == 0 {
                return Err(format!("the process could not be put in the job: {}", std::io::Error::last_os_error()));
            }
            Ok(held)
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle is this value's; closing it ends what is in the job.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// The last lines an engine wrote to its error stream.
type Said = std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>;

/// How many of those lines are kept: a panic is three lines, and what an engine says after it is little.
const LINES_KEPT: usize = 20;

/// Reads an engine's error stream, all the time and to its end: a pipe nobody reads fills up, and the engine, its worker with it, then stops at its next line. Keeps the last lines.
fn listen(stderr: std::process::ChildStderr) -> Said {
    let said = Said::default();
    let kept = said.clone();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(stderr);
        let mut line = Vec::new();
        while matches!(reader.read_until(b'\n', &mut line), Ok(count) if count > 0) {
            let text = String::from_utf8_lossy(&line);
            if !text.trim().is_empty() {
                let mut lines = kept.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                lines.push_back(text.trim_end().to_string());
                while lines.len() > LINES_KEPT {
                    lines.pop_front();
                }
            }
            line.clear();
        }
    });
    said
}

/// Why an engine that ended did, from the last lines it wrote. A panic is its place and its message: Rust writes `thread 'main' (4412) panicked at src\main.rs:3:5:` and then the message (older
/// Rust, `thread 'main' panicked at ...:` and the message, and older still, the message on that line), and after them `note: run with `RUST_BACKTRACE=1` ...`, which says nothing of what went
/// wrong and was all that was shown. Anything else is the last line, as an error that `main` returns is written in one; the note is not taken for that either.
fn why_it_said(lines: &[String]) -> Option<String> {
    if let Some(at) = lines.iter().rposition(|line| line.trim_start().starts_with("thread '") && line.contains(" panicked at ")) {
        let message = lines[at + 1..].iter().map(|line| line.trim()).take_while(|line| !is_backtrace_note(line) && !line.starts_with("stack backtrace")).take(3);
        return Some(std::iter::once(lines[at].trim()).chain(message).collect::<Vec<_>>().join(" "));
    }
    lines.iter().rev().map(|line| line.trim()).find(|line| !line.is_empty() && !is_backtrace_note(line)).map(str::to_string)
}

fn is_backtrace_note(line: &str) -> bool {
    line.starts_with("note: run with `RUST_BACKTRACE")
}

pub struct Engine {
    child: Child,
    _job: Option<Job>,
    /// Why it is not held by a job, when it is not: it then does not end with this app if the app is killed, and the trajectory says so.
    pub not_held: Option<String>,
    /// The last lines it wrote to its error stream (see `listen`).
    said: Said,
    /// Where it listens: not always the port asked for.
    pub port: u16,
}

impl Engine {
    pub fn start(straydog: &Path, settings: &Settings, folder: &Path) -> Result<Engine, String> {
        std::fs::create_dir_all(folder).map_err(|error| format!("{}: {error}", folder.display()))?;
        let mut command = Command::new(straydog);
        command.args(engine_arguments(settings, folder)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        hidden(&mut command);
        let mut child = command.spawn().map_err(|error| start_failure(straydog, &error))?;
        let (job, not_held) = match Job::holding(&child) {
            Ok(job) => (Some(job), None),
            Err(why) => (None, Some(why)),
        };
        let said = child.stderr.take().map_or_else(Said::default, listen);
        Ok(Engine { child, _job: job, not_held, said, port: settings.port })
    }

    /// Its process number: an installer that is to replace its files waits for it to end.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Why it ended, if it has.
    pub fn ended(&mut self) -> Option<String> {
        let status = self.child.try_wait().ok()??;
        // (its last words may still be on their way to the reader)
        std::thread::sleep(Duration::from_millis(100));
        let lines: Vec<String> = self.said.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter().cloned().collect();
        Some(why_it_did_not_begin(status.code()).or_else(|| why_it_said(&lines)).unwrap_or_else(|| format!("the engine ended ({status})")))
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// How long an engine that was just stopped has to go: a worker takes seconds to hand back its memory and the GPU.
pub const ENGINE_EXIT: Duration = Duration::from_secs(30);

/// One at a time: a start waits for the one before it, so that it sees that one's processes.
static STARTING: Mutex<()> = Mutex::new(());

/// Every `straydog.exe` on this PC, this app's or not: two on one card fight over its memory and the machine's.
pub fn running_engines() -> Vec<u32> {
    // (a test's stand-in has nothing to do with an engine someone is running on this machine)
    #[cfg(debug_assertions)]
    if std::env::var_os("DOGMIND_TEST_IGNORE_ENGINES").is_some() {
        return Vec::new();
    }
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
    let mut found = Vec::new();
    // SAFETY: the snapshot is this function's, and the entry is as long as it says.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return found;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            let length = entry.szExeFile.iter().position(|&unit| unit == 0).unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..length]).eq_ignore_ascii_case("straydog.exe") {
                found.push(entry.th32ProcessID);
            }
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    found
}

/// Waits for the other engines to end, for as long as `patience`; one still there is the person's to deal with.
pub fn wait_for_others(keep_going: &dyn Fn() -> bool, running: &dyn Fn() -> Vec<u32>, patience: Duration, say: &mut dyn FnMut(String)) -> Result<(), String> {
    let began = std::time::Instant::now();
    let mut said = false;
    loop {
        let others = running();
        if others.is_empty() {
            return Ok(());
        }
        if !keep_going() {
            return Err("superseded".into());
        }
        if began.elapsed() >= patience {
            let names: Vec<String> = others.iter().map(u32::to_string).collect();
            return Err(tf("another Straydog is running on this PC (process {0}). Two on one GPU fight over its memory and the machine's, so this one was not started. Close it and restart the engine, or reach it as an OpenAI-compatible provider.", &[&names.join(", ")]));
        }
        if !said {
            say("waiting for the last engine to end".to_string());
            said = true;
        }
        std::thread::sleep(Duration::from_millis(250).min(patience));
    }
}

/// What the engine says of a card its build has no code for.
const NO_CODE: &str = "has no GPU code for";

/// What the GPU's library says, as a person can act on it; anything else as it is.
pub fn in_plain_words(error: &str) -> String {
    if error.contains(NO_CODE) {
        return Refusal::OldCard.words();
    }
    let lower = error.to_lowercase();
    if lower.contains("driver version is insufficient") || lower.contains("no cuda-capable device") || lower.contains("cuda initialization") {
        return if nvidia_driver_is_here() { Refusal::Driver(error.trim().to_string()).words() } else { Refusal::NoNvidia.words() };
    }
    error.to_string()
}

/// How many ports after the one asked for are tried: nothing but this app uses the engine's, so any free one will do.
const PORTS_TRIED: u16 = 64;

/// A port for the engine: this one, or the next that is free, as the app would otherwise take a stranger for its engine.
pub fn free_port(port: u16, is_free: &dyn Fn(u16) -> bool) -> Result<u16, String> {
    (0..PORTS_TRIED).filter_map(|step| port.checked_add(step)).find(|&candidate| is_free(candidate))
        .ok_or_else(|| tf("port {0} on this PC is taken. Choose another port for the engine in settings.json, or close what holds it.", &[&port]))
}

pub fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// The engine, once no other engine and nothing else is in its way. For another thread: it can wait for half a minute.
pub fn start_alone(straydog: &Path, settings: &Settings, folder: &Path, keep_going: &dyn Fn() -> bool, say: &mut dyn FnMut(String)) -> Result<Engine, String> {
    let _turn = STARTING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    wait_for_others(keep_going, &running_engines, ENGINE_EXIT, say)?;
    if !keep_going() {
        return Err("superseded".into());
    }
    let settings = Settings { port: free_port(settings.port, &port_is_free)?, ..settings.clone() };
    Engine::start(straydog, &settings, folder)
}

pub fn client(port: u16) -> Client {
    Client::new(&format!("http://127.0.0.1:{port}/v1"), None).expect("the URL is well formed")
}

/// What `/health`'s loading progress says, in a line.
pub fn progress_text(progress: &Value) -> String {
    let stage = t(progress.get("stage").and_then(Value::as_str).unwrap_or("loading"));
    match (progress.get("done").and_then(Value::as_f64), progress.get("total").and_then(Value::as_f64)) {
        (Some(done), Some(total)) if total > 0.0 => format!("{stage} {:.0}%", 100.0 * done / total),
        _ => stage.to_string(),
    }
}

/// How many polls in a row may fail, once the engine has answered, before it is said to have stopped. A poll gives up after 20 seconds, so an engine that goes silent ends the wait in
/// about a minute, and one that is gone (the connection refused) in the few seconds three refusals and the pauses between them take. One that is slow for a moment, loading a model on a
/// machine that is short of memory, is waited for.
const FAILED_POLLS_IN_A_ROW: usize = 3;

/// Waits for the server. `say` hears its loading; ends when it is ready or cannot be.
pub fn wait_ready(client: &Client, keep_going: &dyn Fn() -> bool, say: &mut dyn FnMut(String)) -> Result<(), String> {
    wait_until_ready(&mut || client.health(), Duration::from_millis(500), keep_going, say)
}

/// `wait_ready` for any way of asking after `/health`, and any pause between two asks.
fn wait_until_ready(ask: &mut dyn FnMut() -> Result<Health, String>, pause: Duration, keep_going: &dyn Fn() -> bool, say: &mut dyn FnMut(String)) -> Result<(), String> {
    let mut reached = false;
    let mut failed_in_a_row = 0;
    while keep_going() {
        match ask() {
            Ok(Health::Ready) => return Ok(()),
            Ok(Health::Loading(progress)) => {
                (reached, failed_in_a_row) = (true, 0);
                say(progress_text(&progress));
            }
            Ok(Health::Failed(message)) => return Err(message),
            Err(_) if reached => {
                failed_in_a_row += 1;
                if failed_in_a_row >= FAILED_POLLS_IN_A_ROW {
                    return Err(t("the engine stopped answering: /log shows why, and writing again tries once more").to_string());
                }
            }
            Err(_) => say(t("starting the engine").to_string()),
        }
        std::thread::sleep(pause);
    }
    Err("stopped".into())
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetupLine {
    Progress { stage: String, done: f64, total: f64 },
    Text(String),
}

/// What `straydog setup` has said of the whole, across its stages: each stage and file counts from nothing by itself, so a bar that follows them starts again at every one.
/// Summed from what it says: the bundle's size and what is here already in its first lines, and each download's bytes as they come. A run after a dropped connection says them anew.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SetupTally {
    /// In GB, as Straydog gives them.
    total: f64,
    already_here: f64,
    /// Each file being or been downloaded in this run: its name, where it started (what had come of it is in `already_here`) and how far it is.
    files: Vec<(String, f64, f64)>,
}

impl SetupTally {
    pub fn hear(&mut self, line: &SetupLine) {
        match line {
            SetupLine::Text(text) => {
                if let Some((total, here)) = parse_setup_sizes(text) {
                    *self = SetupTally { total, already_here: here, files: Vec::new() };
                }
            }
            SetupLine::Progress { stage, done, .. } if stage.starts_with("downloading ") => match self.files.iter_mut().find(|(name, ..)| name == stage) {
                Some((_, _, latest)) => *latest = latest.max(*done),
                None => self.files.push((stage.clone(), *done, *done)),
            },
            SetupLine::Progress { .. } => {}
        }
    }

    /// GB there and GB in all, once Straydog has said the size. Hashing and building add nothing to it: those bytes are there.
    pub fn overall(&self) -> Option<(f64, f64)> {
        (self.total > 0.0).then(|| ((self.already_here + self.files.iter().map(|(_, first, latest)| latest - first).sum::<f64>()).min(self.total), self.total))
    }
}

/// `  12.3 GB of 75.0 GB download, 62.7 GB already here (resumed or verified)` or `  75.0 GB to download, 76.0 GB on the disk once done`: the size of the whole and how much is here.
fn parse_setup_sizes(line: &str) -> Option<(f64, f64)> {
    let amount = |text: &str| text.trim().strip_suffix(" GB").and_then(|number| number.parse::<f64>().ok());
    let line = line.trim();
    if let Some((left, rest)) = line.split_once(" of ") {
        if rest.contains(" already here") {
            let total = amount(rest.split_once(" download,")?.0)?;
            return Some((total, (total - amount(left)?).max(0.0)));
        }
    }
    let (total, rest) = line.split_once(" to download,")?;
    rest.contains("on the disk").then_some(())?;
    Some((amount(total)?, 0.0))
}

/// The bar's place and the line under it for a line of progress: the whole where it is known, and always what is going on now.
pub fn setup_progress(stage: &str, done: f64, total: f64, tally: &SetupTally) -> (f64, String) {
    let words = stage_words(stage);
    // (the drafter counts values and not bytes)
    let now = if stage.starts_with("building the drafter") { format!("{words}: {:.0}%", 100.0 * done / total.max(0.001)) } else { format!("{words}: {done:.1} / {total:.1} GB") };
    match tally.overall() {
        Some((here, whole)) => (here / whole, format!("{now}  ·  {}", tf("{0} of {1} GB in all ({2}%)", &[&format!("{here:.1}"), &format!("{whole:.1}"), &format!("{:.0}", 100.0 * here / whole)]))),
        None => (done / total.max(0.001), now),
    }
}

/// A stage of `straydog setup` in the person's language: what is left is the name of a file.
pub fn stage_words(stage: &str) -> String {
    for (english, words) in [("downloading ", t("Downloading {0}")), ("checking the partial ", t("Checking what has arrived of {0}")), ("authenticating ", t("Checking {0}")), ("building the verity tree of ", t("Preparing {0}")), ("building the drafter", t("Building the faster-writing data {0}"))] {
        if let Some(rest) = stage.strip_prefix(english) {
            return words.replace("{0}", rest.trim());
        }
    }
    stage.to_string()
}

/// Whether a failed setup may go on by itself: a network that came back, a server that was busy. A drive too small or a model that cannot run stay as they are.
pub fn setup_may_recover(error: &str) -> bool {
    let lower = error.to_lowercase();
    ["download ended early", "timed out", "timeout", "connection", "connect", "dns", "resolve", "network", "unexpected model response status", "resume range mismatch", "os error 100", "os error 110", "unexpected end of file", "reset"].iter().any(|words| lower.contains(words))
}

/// What a setup's failure is, in words for the person: what has come stays.
pub fn setup_failure_words(error: &str) -> String {
    if setup_may_recover(error) {
        tf("The download stopped and could not go on: {0}. What has arrived stays in the folder, and downloading again goes on from there.", &[&error])
    } else {
        in_plain_words(error)
    }
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
    let mut child = command.spawn().map_err(|error| start_failure(straydog, &error))?;
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
            use std::io::BufRead;
            // As it comes: its first lines say how large the whole is, which the bar wants now and not at the end.
            let mut reader = std::io::BufReader::new(&mut stdout);
            let mut line = Vec::new();
            while matches!(reader.read_until(b'\n', &mut line), Ok(count) if count > 0) {
                let _ = out.send(SetupLine::Text(String::from_utf8_lossy(&line).trim().to_string()));
                wake();
                line.clear();
            }
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
    if status.success() {
        Ok(())
    } else if let Some(why) = why_it_did_not_begin(status.code()) {
        Err(why)
    } else {
        Err(if last_error.is_empty() { format!("setup ended ({status})") } else { last_error })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The quantizations of the bundles an entry list has, and the names it shows.
    fn quants(entries: &[CatalogEntry]) -> Vec<&str> {
        entries.iter().filter_map(|entry| entry.bundle.as_ref()).map(|bundle| bundle.quant.as_str()).collect()
    }

    fn names(entries: &[CatalogEntry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.name.as_str()).collect()
    }

    /// A bundle as `setup --list` says it, with its model's creator and licence.
    fn listed(model: &str, provider: &str, quant: &str) -> String {
        let (creator, license, url) = if model.starts_with("DeepSeek") {
            ("DeepSeek", "MIT", "https://huggingface.co/deepseek-ai/DeepSeek-V4.1-Flash-1017/blob/main/LICENSE")
        } else {
            ("Qwen", "Qwen Community License 1.0", "https://huggingface.co/Qwen/Qwen3.8-Flash-Next/blob/main/LICENSE")
        };
        format!(r#"{{"model":"{model}","provider":"{provider}","quant":"{quant}","creator":"{creator}","license":"{license}","license_url":"{url}","download_bytes":1,"runs":true,"note":"n"}}"#)
    }

    #[test]
    fn a_vision_file_alone_keeps_the_rest_as_the_settings_have_it() {
        let installed = crate::vision::Installed { model: PathBuf::from(r"D:\models\Qwen\IQ2_XS"), bundle: Bundle::from_settings(None, None, "IQ2_XS"), drafter: true, named: None };
        let args = vision_arguments(&installed, Path::new("C"));
        let joined = args.join(" ");
        assert!(joined.contains(r"--dir D:\models\Qwen\IQ2_XS"), "the bundle's own folder, which setup then does not put a level down: {joined}");
        assert!(joined.contains("--model-name Qwen3.8-Flash-Next --provider ISTA-DASLab --quant IQ2_XS") && !joined.contains("--no-vision") && !joined.contains("--no-mtp") && joined.contains("--no-dspark"), "{joined}");
        let without = vision_arguments(&crate::vision::Installed { drafter: false, ..installed }, Path::new("C"));
        assert!(without.contains(&"--no-mtp".to_string()), "a drafter it has not got is not fetched now");
    }

    #[test]
    fn the_list_says_what_each_extra_takes() {
        let list = r#"{"machine": {"gpus": [], "ram_bytes": 0}, "bundles": [
            {"model": "Qwen3.8-Flash-Next", "provider": "ISTA-DASLab", "creator": "Qwen", "license": "Qwen Community License 1.0", "license_url": "https://huggingface.co/Qwen/Qwen3.8-Flash-Next/blob/main/LICENSE", "quant": "IQ2_XS", "runs": true, "recommended": true, "fit": "fits", "download_bytes": 1000, "disk_bytes": 2000,
             "extras": [{"name": "mtp", "download_bytes": 300}, {"name": "vision", "download_bytes": 100}]},
            {"model": "DeepSeek-V4.1-Flash-1017", "provider": "ISTA-DASLab", "creator": "DeepSeek", "license": "MIT", "license_url": "https://huggingface.co/deepseek-ai/DeepSeek-V4.1-Flash-1017/blob/main/LICENSE", "quant": "IQ2_XXS", "runs": true, "fit": "fits", "download_bytes": 5000, "disk_bytes": 6000, "extras": []}]}"#;
        let catalog = parse_list(list, true).unwrap();
        let of = |quant: &str| catalog.entries.iter().find(|entry| entry.bundle.as_ref().is_some_and(|bundle| bundle.quant == quant));
        let Some(first) = of("IQ2_XS") else { return };
        assert_eq!((first.download_bytes, first.vision_bytes, first.drafter_bytes), (Some(1000), Some(100), Some(300)));
        if let Some(second) = of("IQ2_XXS") {
            assert_eq!(second.vision_bytes, None, "no file for it: no row for it");
        }
    }

    #[test]
    fn an_engine_waits_for_the_last_one_and_refuses_one_that_stays() {
        let yes = || true;
        let mut said = Vec::new();
        assert!(wait_for_others(&yes, &Vec::new, Duration::from_secs(5), &mut |text| said.push(text)).is_ok());
        assert!(said.is_empty(), "nothing to wait for, nothing to say");

        let left = std::cell::Cell::new(2);
        let going = || {
            left.set(left.get() - 1);
            if left.get() > 0 { vec![7u32] } else { Vec::new() }
        };
        assert!(wait_for_others(&yes, &going, Duration::from_secs(5), &mut |text| said.push(text)).is_ok());
        assert_eq!(said.len(), 1);

        let error = wait_for_others(&yes, &|| vec![7u32, 9], Duration::ZERO, &mut |_| {}).unwrap_err();
        assert!(error.contains("process 7, 9"), "{error}");
        assert_eq!(wait_for_others(&|| false, &|| vec![7u32], Duration::from_secs(5), &mut |_| {}).unwrap_err(), "superseded");
    }

    #[test]
    fn a_taken_port_gives_way_to_the_next_free_one() {
        let holder = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = holder.local_addr().unwrap().port();
        assert!(!port_is_free(port));
        drop(holder);
        assert!(port_is_free(port));
        assert_eq!(free_port(18480, &|_| true), Ok(18480));
        assert_eq!(free_port(18480, &|port| port >= 18483), Ok(18483));
        assert!(free_port(18480, &|_| false).unwrap_err().contains("18480"), "none free is said");
        assert!(free_port(65535, &|_| false).is_err(), "and the numbers end");
    }

    #[test]
    fn a_program_that_does_not_begin_is_told_from_one_that_failed() {
        assert!(why_it_did_not_begin(Some(0xC000_0135u32 as i32)).unwrap().contains("cublas64_13.dll"));
        assert!(why_it_did_not_begin(Some(0xC000_0022u32 as i32)).unwrap().contains("access denied"));
        assert_eq!(why_it_did_not_begin(Some(1)), None);
        assert_eq!(why_it_did_not_begin(None), None);
        let blocked = start_failure(Path::new("straydog.exe"), &std::io::Error::from_raw_os_error(4551));
        assert!(blocked.contains("Smart App Control"), "{blocked}");
        assert!(start_failure(Path::new("x"), &std::io::Error::from_raw_os_error(2)).starts_with("x: "));
    }

    #[test]
    fn a_network_failure_goes_on_and_a_refusal_does_not() {
        for error in ["size mismatch: a.gguf.part (the download ended early; run again to resume)", "Connection reset by peer (os error 10054)", "timeout: global", "unexpected model response status", "dns failed"] {
            assert!(setup_may_recover(error), "{error}");
            assert!(setup_failure_words(error).contains("goes on from there"));
        }
        for error in ["Qwen3.8-Flash-Next IQ2_XS cannot run on this machine: needs 10 GiB of VRAM; this machine has 6 GiB", "Qwen3.8-Flash-Next IQ2_XS needs 75.1 GB more on the disk of D:\\m, which has 3.0 GB free.", "unknown bundle Q8", "none of the models can run here", "cancelled"] {
            assert!(!setup_may_recover(error), "{error}");
        }
    }

    #[test]
    fn what_cuda_says_is_put_in_words_a_person_can_act_on() {
        let said = "CUDA driver version is insufficient for CUDA runtime version";
        let words = in_plain_words(said);
        assert!(words.contains("NVIDIA"), "{words}");
        assert_eq!(in_plain_words("a folder could not be opened"), "a folder could not be opened");
    }

    #[test]
    fn the_whole_download_is_summed_across_its_files_and_does_not_start_again() {
        let mut tally = SetupTally::default();
        assert_eq!(tally.overall(), None, "nothing said yet");
        let (fraction, words) = setup_progress("downloading a.gguf", 1.0, 2.0, &tally);
        assert_eq!(fraction, 0.5, "by the stage, as long as the whole is not known");
        assert_eq!(words, "Downloading a.gguf: 1.0 / 2.0 GB");
        let progress = |stage: &str, done: f64, total: f64| SetupLine::Progress { stage: stage.into(), done, total };

        tally.hear(&SetupLine::Text("  75.0 GB to download, 76.0 GB on the disk once done".into()));
        assert_eq!(tally.overall(), Some((0.0, 75.0)));
        for line in [progress("downloading a.gguf", 0.0, 40.0), progress("downloading a.gguf", 40.0, 40.0), progress("downloading b.gguf", 0.0, 30.0)] {
            tally.hear(&line);
        }
        assert_eq!(tally.overall(), Some((40.0, 75.0)), "the second file adds to the first");
        tally.hear(&progress("downloading b.gguf", 10.0, 30.0));
        tally.hear(&progress("authenticating a.gguf", 3.0, 40.0));
        assert_eq!(tally.overall(), Some((50.0, 75.0)), "checking what has come is not more come");
        let (fraction, words) = setup_progress("building the verity tree of a.gguf", 3.0, 40.0, &tally);
        assert!((fraction - 50.0 / 75.0).abs() < 1e-9, "the bar stays where it was: {fraction}");
        assert_eq!(words, "Preparing a.gguf: 3.0 / 40.0 GB  ·  50.0 of 75.0 GB in all (67%)");
    }

    #[test]
    fn a_resumed_download_counts_what_is_here_once() {
        let mut tally = SetupTally::default();
        // A dropped connection: the next run says it all again, the partial file among what is here, and goes on from its end.
        tally.hear(&SetupLine::Text("  12.0 GB of 75.0 GB download, 63.0 GB already here (resumed or verified)".into()));
        assert_eq!(tally.overall(), Some((63.0, 75.0)));
        tally.hear(&SetupLine::Progress { stage: "downloading a.gguf".into(), done: 28.0, total: 40.0 });
        assert_eq!(tally.overall(), Some((63.0, 75.0)), "those 28 GB are among the 63");
        tally.hear(&SetupLine::Progress { stage: "downloading a.gguf".into(), done: 30.0, total: 40.0 });
        assert_eq!(tally.overall(), Some((65.0, 75.0)));
        tally.hear(&SetupLine::Text("  10.0 GB of 75.0 GB download, 65.0 GB already here (resumed or verified)".into()));
        assert_eq!(tally.overall(), Some((65.0, 75.0)), "a new run begins again from what it says");
        // Trees are counted among what is there, and can pass the download itself.
        let mut full = SetupTally::default();
        full.hear(&SetupLine::Text("  0.0 GB of 75.0 GB download, 75.6 GB already here (resumed or verified)".into()));
        assert_eq!(full.overall(), Some((75.0, 75.0)));
        full.hear(&SetupLine::Text("Setting up Qwen in D:\\m of the disk".into()));
        assert_eq!(full.overall(), Some((75.0, 75.0)), "another line changes nothing");
    }

    #[test]
    fn a_setups_first_lines_come_while_it_runs() {
        let (lines, seen) = std::sync::mpsc::channel();
        let cancel = AtomicBool::new(false);
        let args = ["/c", "echo   75.0 GB to download, 76.0 GB on the disk once done"].map(String::from);
        assert_eq!(run_setup(Path::new("cmd.exe"), &args, &lines, &|| {}, &cancel), Ok(()));
        drop(lines);
        let mut tally = SetupTally::default();
        for line in seen {
            tally.hear(&line);
        }
        assert_eq!(tally.overall(), Some((0.0, 75.0)));
    }

    #[test]
    fn a_model_says_its_licence_before_it_is_fetched() {
        let list = format!(r#"{{"bundles":[{},{},
            {{"model":"Qwen3.8-Flash-Next","provider":"Unsloth","quant":"UD-IQ1_M","creator":"Qwen","download_bytes":1,"runs":true}},
            {{"model":"Qwen3.8-Flash-Next","provider":"ISTA-DASLab","quant":"IQ3_S","license":"Qwen Community License 1.0","license_url":"file:///C:/Windows/System32/calc.exe","download_bytes":1,"runs":true}}]}}"#,
            listed("Qwen3.8-Flash-Next", "ISTA-DASLab", "IQ2_XS"), listed("DeepSeek-V4.1-Flash-1017", "ISTA-DASLab", "IQ2_XS"));
        let entries = parse_list(&list, true).unwrap().entries;
        // (Unsloth's first: then the list's order)
        assert_eq!(entries.len(), 4);
        let qwen = entries[1].licence.clone().unwrap();
        assert_eq!(qwen.name, "Qwen Community License 1.0");
        assert!(qwen.terms.starts_with("https://huggingface.co/Qwen/"), "the base model's terms, which the quantized file inherits");
        assert_eq!(entries[2].licence.clone().unwrap().name, "MIT");
        assert!(entries[1].licence_line().unwrap().contains("Qwen Community License 1.0"));
        assert_eq!(entries[0].licence, None, "a bundle that says no licence has no line");
        assert_eq!(entries[3].licence, None, "an address that is no web address is not one to open in a browser");
        let help = parse_catalog("      --quant <NAME>  Qwen3.8-Flash-Next: IQ2_XS (68.0 GB). x\n");
        assert_eq!(help[0].licence_line(), None, "Straydog's own choice is no model yet");
        assert_eq!(help[1].licence, None, "the help of an older Straydog says none");
    }

    #[test]
    fn a_stage_of_setup_is_said_with_its_file() {
        assert_eq!(stage_words("downloading a.gguf"), "Downloading a.gguf");
        assert_eq!(stage_words("something new"), "something new");
    }

    #[test]
    fn the_bundles_come_from_straydogs_own_help() {
        let help = "Download and authenticate a model bundle\n\nOptions:\n  -m, --model <DIR>  The bundle directory\n      --quant <NAME>  Qwen3.8-Flash-Next: Q2_0 (66.4 GB), IQ2_XS (68.0 GB); DeepSeek-V4.1-Flash: GSQ-RCO-IQ2_XXS (248.8 GB); Qwen3.6-35B-A3B: UD-Q8_K_XL (38.5 GB). `straydog setup` chooses for you\n";
        let entries = parse_catalog(help);
        assert_eq!(entries[0].bundle, None);
        // (what it names is read as the settings of 0.1.3 are: DeepSeek's GSQ-RCO-IQ2_XXS is now ISTA-DASLab's IQ2_XXS)
        assert_eq!(quants(&entries), ["Q2_0", "IQ2_XS", "IQ2_XXS"]);
        assert_eq!(entries[2].detail, "68.0 GB");
        assert_eq!(entries[2].name, "Qwen3.8-Flash-Next · ISTA-DASLab · IQ2_XS");
        assert_eq!(entries[3].bundle, Some(Bundle::new("DeepSeek-V4.1-Flash-1017", "ISTA-DASLab", "IQ2_XXS")));
        assert_eq!(entries[3].origin, "quantized by ISTA-DASLab", "it says no creator");
        // (what the table does not know, and what it gives to another model, are left out; DeepSeek's by Unsloth is offered as the rest)
        let left_out = parse_catalog("      --quant <NAME>  Qwen3.8-Flash-Next: Qwen-UD-IQ1_M (68.0 GB), UD-IQ1_M (68.0 GB); DeepSeek-V4.1-Flash: UD-IQ2_XXS (1.0 GB), IQ3_S (1.0 GB). x\n");
        assert_eq!(names(&left_out), ["The best for this PC", "DeepSeek-V4.1-Flash-1017 · Unsloth · UD-IQ2_XXS"]);
        assert_eq!(parse_catalog("something else entirely").len(), 1, "an unknown help still offers Straydog's choice");
    }

    #[test]
    fn a_card_without_code_is_too_old_and_not_a_driver_to_update() {
        let error = "GPU 0: this Straydog build has no GPU code for sm_61; build it for this GPU";
        let json = format!(r#"{{"machine":{{"gpus":[],"gpu_error":"{error}","ram_bytes":1}},"bundles":[]}}"#);
        assert_eq!(parse_list(&json, true).unwrap().refusal, Some(Refusal::OldCard));
        assert_eq!(in_plain_words(error), Refusal::OldCard.words());
    }

    #[test]
    fn the_list_puts_the_recommended_first_and_leaves_out_what_cannot_run() {
        let list = r#"{"machine":{"backend":"cuda"},"bundles":[
            {"model":"Qwen3.8-Flash-Next","provider":"ISTA-DASLab","creator":"Qwen","license":"Qwen Community License 1.0","license_url":"https://huggingface.co/Qwen/Qwen3.8-Flash-Next/blob/main/LICENSE","quant":"IQ3_S","download_bytes":83600000000,"runs":true,"recommended":false,"note":"slow"},
            {"model":"Qwen3.8-Flash-Next","provider":"ISTA-DASLab","creator":"Qwen","license":"Qwen Community License 1.0","license_url":"https://huggingface.co/Qwen/Qwen3.8-Flash-Next/blob/main/LICENSE","quant":"IQ2_XS","download_bytes":68000000000,"runs":true,"recommended":true,"note":"fits","context":262144},
            {"model":"DeepSeek-V4.1-Flash-1017","provider":"ISTA-DASLab","creator":"DeepSeek","license":"MIT","license_url":"https://huggingface.co/deepseek-ai/DeepSeek-V4.1-Flash-1017/blob/main/LICENSE","quant":"IQ2_XXS","download_bytes":300500000000,"runs":false,"recommended":false,"note":"too little VRAM"}]}"#;
        let catalog = parse_list(list, true).unwrap();
        let entries = &catalog.entries;
        assert_eq!(quants(entries), ["IQ2_XS", "IQ3_S"]);
        assert!(entries[0].detail.starts_with("Recommended for this PC  ·  68 GB to download") && !entries[1].detail.contains("Recommended"), "{}", entries[0].detail);
        assert_eq!(entries[0].origin, "by Qwen, quantized by ISTA-DASLab");
        assert_eq!(entries[0].context, Some(262144));
        assert_eq!(catalog.refusal, None);
        assert!(parse_list("error: unexpected argument '--list'", true).is_none());
        assert!(parse_list("{}", true).is_none());
    }

    #[test]
    fn the_bundle_set_up_is_found_by_all_three_labels() {
        // Qwen3.8 and DeepSeek each have a UD-IQ1_M, and ISTA-DASLab and Unsloth each have a bundle of a name the other has
        let list = format!(r#"{{"bundles":[{}]}}"#, [
            listed("Qwen3.8-Flash-Next", "Unsloth", "UD-IQ1_M"), listed("Qwen3.8-Flash-Next", "ISTA-DASLab", "IQ2_XS"), listed("Qwen3.8-Flash-Next", "Unsloth", "IQ2_XS"),
            listed("DeepSeek-V4.1-Flash-1017", "ISTA-DASLab", "IQ2_XS"),
        ].join(","));
        let entries = parse_list(&list, true).unwrap().entries;
        let found = |set_up: &Bundle| -> Vec<&str> { entries.iter().filter(|entry| entry.is(set_up)).map(|entry| entry.name.as_str()).collect() };
        assert_eq!(found(&Bundle::new("Qwen3.8-Flash-Next", "Unsloth", "UD-IQ1_M")), ["Qwen3.8-Flash-Next · Unsloth · UD-IQ1_M"]);
        assert!(found(&Bundle::new("DeepSeek-V4.1-Flash-1017", "Unsloth", "UD-IQ1_M")).is_empty(), "DeepSeek's UD-IQ1_M is not Qwen's, and is not offered");
        assert_eq!(found(&Bundle::new("Qwen3.8-Flash-Next", "ISTA-DASLab", "IQ2_XS")), ["Qwen3.8-Flash-Next · ISTA-DASLab · IQ2_XS"]);
        assert_eq!(found(&Bundle::new("Qwen3.8-Flash-Next", "Unsloth", "IQ2_XS")), ["Qwen3.8-Flash-Next · Unsloth · IQ2_XS"], "an ISTA-DASLab bundle is not an Unsloth one of its name");
        assert_eq!(found(&Bundle::new("DeepSeek-V4.1-Flash-1017", "ISTA-DASLab", "IQ2_XS")), ["DeepSeek-V4.1-Flash-1017 · ISTA-DASLab · IQ2_XS"], "nor is Qwen's one of DeepSeek's");
        // a label the settings lack is not compared: a bundle of a name they do not know is found by its quantization alone, as it was
        let odd = |model: Option<&str>, provider: Option<&str>| Bundle { model: model.map(str::to_string), provider: provider.map(str::to_string), quant: "IQ2_XS".into() };
        assert_eq!(found(&odd(None, None)).len(), 3);
        assert_eq!(found(&odd(Some("Qwen3.8-Flash-Next"), None)).len(), 2);
        assert_eq!(found(&odd(None, Some("Unsloth"))), ["Qwen3.8-Flash-Next · Unsloth · IQ2_XS"]);
        assert!(found(&Bundle::from_settings(None, None, "Q8_0")).is_empty());
        // Straydog's own choice is no bundle yet
        assert!(!parse_catalog("x")[0].is(&Bundle::from_settings(None, None, "IQ2_XS")));
    }

    #[test]
    fn settings_written_up_to_0_1_3_are_read_by_the_table_frozen_there() {
        let read = |quant: &str| Bundle::from_settings(None, None, quant);
        let (deepseek, qwen) = ("DeepSeek-V4.1-Flash-1017", "Qwen3.8-Flash-Next");
        assert_eq!(read("GSQ-RCO-IQ2_XS"), Bundle::new(deepseek, "ISTA-DASLab", "IQ2_XS"));
        assert_eq!(read("GSQ-RCO-IQ2_XXS"), Bundle::new(deepseek, "ISTA-DASLab", "IQ2_XXS"));
        assert_eq!(read("UD-IQ2_XXS"), Bundle::new(deepseek, "Unsloth", "UD-IQ2_XXS"));
        assert_eq!(read("UD-IQ1_M"), Bundle::new(deepseek, "Unsloth", "UD-IQ1_M"), "Qwen had none before, so an old one is DeepSeek's");
        for quant in ["Q2_0", "IQ2_XS", "IQ3_XXS", "IQ3_S"] {
            assert_eq!(read(quant), Bundle::new(qwen, "ISTA-DASLab", quant), "{quant}");
        }
        // anything else keeps its quantization, and its model and provider are unknown
        assert_eq!(read("Qwen-UD-IQ1_M"), Bundle { model: None, provider: None, quant: "Qwen-UD-IQ1_M".into() });
        // a setup that says its model is not read by the table: IQ2_XS of DeepSeek's is not Qwen's
        assert_eq!(Bundle::from_settings(Some(deepseek), Some("ISTA-DASLab"), "IQ2_XS"), Bundle::new(deepseek, "ISTA-DASLab", "IQ2_XS"));
        assert_eq!(Bundle::from_settings(Some(qwen), Some("Unsloth"), "UD-IQ1_M"), Bundle::new(qwen, "Unsloth", "UD-IQ1_M"));
        assert_eq!(Bundle::from_settings(Some(qwen), None, "UD-IQ1_M"), Bundle { model: Some(qwen.into()), provider: None, quant: "UD-IQ1_M".into() }, "what it does not say is not guessed");
    }

    #[test]
    fn setup_is_told_the_model_the_provider_and_the_quantization() {
        let whole = |bundle: Option<&Bundle>| setup_arguments(bundle, Path::new("M"), Path::new("C"), true, true).join(" ");
        let args = whole(Some(&Bundle::new("Qwen3.8-Flash-Next", "Unsloth", "UD-IQ1_M")));
        assert!(args.contains("--model-name Qwen3.8-Flash-Next --provider Unsloth --quant UD-IQ1_M"), "{args}");
        // only the ones known, for a bundle an older setup wrote that no table knows
        let odd = whole(Some(&Bundle::from_settings(None, None, "Q8_0")));
        assert!(odd.contains("--quant Q8_0") && !odd.contains("--model-name") && !odd.contains("--provider"), "{odd}");
        // and none for Straydog's own choice
        let own = whole(None);
        assert!(!own.contains("--model-name") && !own.contains("--provider") && !own.contains("--quant"), "{own}");
        // the vision file of a model set up by 0.1.3 asks for it by the labels it has now
        let installed = crate::vision::Installed { model: PathBuf::from(r"D:\m\GSQ-RCO-IQ2_XS"), bundle: Bundle::from_settings(None, None, "GSQ-RCO-IQ2_XS"), drafter: true, named: None };
        let vision = vision_arguments(&installed, Path::new("C")).join(" ");
        assert!(vision.contains("--model-name DeepSeek-V4.1-Flash-1017 --provider ISTA-DASLab --quant IQ2_XS"), "{vision}");
    }

    #[test]
    fn a_bundle_is_shown_by_its_three_labels_and_says_who_made_it_and_who_quantized_it() {
        let list = format!(r#"{{"bundles":[{},{},
            {{"model":"Qwen3.8-Flash-Next","provider":"ISTA-DASLab","quant":"IQ2_XS","download_bytes":1,"runs":true}}]}}"#,
            listed("Qwen3.8-Flash-Next", "Unsloth", "UD-IQ1_M"), listed("DeepSeek-V4.1-Flash-1017", "ISTA-DASLab", "IQ2_XXS"));
        let entries = parse_list(&list, true).unwrap().entries;
        assert_eq!(names(&entries), ["Qwen3.8-Flash-Next · Unsloth · UD-IQ1_M", "DeepSeek-V4.1-Flash-1017 · ISTA-DASLab · IQ2_XXS", "Qwen3.8-Flash-Next · ISTA-DASLab · IQ2_XS"]);
        assert_eq!(entries[0].origin, "by Qwen, quantized by Unsloth");
        assert_eq!(entries[1].origin, "by DeepSeek, quantized by ISTA-DASLab");
        assert_eq!(entries[2].origin, "quantized by ISTA-DASLab", "a bundle with no creator says only who quantized it");
        assert_eq!(origin("Qwen", "ISTA-DASLab"), "by Qwen, quantized by ISTA-DASLab");
        assert_eq!(origin("", "Unsloth"), "quantized by Unsloth");
    }

    #[test]
    fn all_the_list_has_is_offered_unsloths_first() {
        let (qwen, deepseek) = ("Qwen3.8-Flash-Next", "DeepSeek-V4.1-Flash-1017");
        let list = format!(r#"{{"bundles":[{}]}}"#, [
            listed(deepseek, "ISTA-DASLab", "IQ2_XXS"), listed(deepseek, "ISTA-DASLab", "IQ2_XS"), listed(deepseek, "Unsloth", "UD-IQ2_XXS"), listed(deepseek, "Unsloth", "UD-IQ1_M"),
            listed(qwen, "ISTA-DASLab", "IQ3_S"), listed(qwen, "ISTA-DASLab", "IQ3_XXS"), listed(qwen, "ISTA-DASLab", "IQ2_XS"), listed(qwen, "ISTA-DASLab", "Q2_0"), listed(qwen, "Unsloth", "UD-IQ1_M"),
            listed("Qwen3.6-35B-A3B", "Unsloth", "UD-Q8_K_XL"),
            // (a list of a Straydog that does not say who quantized)
            format!(r#"{{"model":"{qwen}","quant":"Q2_0","download_bytes":1,"runs":true}}"#),
        ].join(","));
        let entries = parse_list(&list, true).unwrap().entries;
        // (a model Straydog lists that this app has never heard of needs nothing of it; one without a provider is an older Straydog's;
        // Unsloth's come first, each part in the list's order)
        assert_eq!(names(&entries), [
            "DeepSeek-V4.1-Flash-1017 · Unsloth · UD-IQ2_XXS", "DeepSeek-V4.1-Flash-1017 · Unsloth · UD-IQ1_M",
            "Qwen3.8-Flash-Next · Unsloth · UD-IQ1_M", "Qwen3.6-35B-A3B · Unsloth · UD-Q8_K_XL",
            "DeepSeek-V4.1-Flash-1017 · ISTA-DASLab · IQ2_XXS", "DeepSeek-V4.1-Flash-1017 · ISTA-DASLab · IQ2_XS",
            "Qwen3.8-Flash-Next · ISTA-DASLab · IQ3_S", "Qwen3.8-Flash-Next · ISTA-DASLab · IQ3_XXS", "Qwen3.8-Flash-Next · ISTA-DASLab · IQ2_XS", "Qwen3.8-Flash-Next · ISTA-DASLab · Q2_0"]);
        // one ISTA-DASLab's that Straydog recommends comes after Unsloth's, first of its own; and so with those short of graphics memory
        let recommended = listed(qwen, "ISTA-DASLab", "IQ3_XXS").replace(r#""runs":true"#, r#""runs":true,"recommended":true"#);
        let short = |provider: &str, quant: &str| listed(qwen, provider, quant).replace(r#""runs":true,"note":"n""#, r#""runs":false,"note":"needs 20 GiB of VRAM; this machine has 16 GiB""#);
        let list = format!(r#"{{"bundles":[{}]}}"#, [listed(qwen, "ISTA-DASLab", "IQ2_XS"), recommended, listed(qwen, "Unsloth", "UD-IQ1_M"), short("ISTA-DASLab", "IQ3_S"), short("Unsloth", "UD-IQ3_XXS")].join(","));
        assert_eq!(names(&parse_list(&list, true).unwrap().entries), [
            "Qwen3.8-Flash-Next · Unsloth · UD-IQ1_M", "Qwen3.8-Flash-Next · ISTA-DASLab · IQ3_XXS", "Qwen3.8-Flash-Next · ISTA-DASLab · IQ2_XS",
            "Qwen3.8-Flash-Next · Unsloth · UD-IQ3_XXS", "Qwen3.8-Flash-Next · ISTA-DASLab · IQ3_S"]);
    }

    #[test]
    fn a_pc_that_can_run_nothing_is_told_why_and_how() {
        let nothing = r#"{"machine":{"backend":"CUDA","gpus":[],"gpu_error":"CUDA driver version is insufficient for CUDA runtime version","ram_bytes":34359738368},"bundles":[
            {"model":"Qwen3.8-Flash-Next","provider":"ISTA-DASLab","creator":"Qwen","license":"Qwen Community License 1.0","license_url":"https://huggingface.co/Qwen/Qwen3.8-Flash-Next/blob/main/LICENSE","quant":"IQ2_XS","download_bytes":75000000000,"runs":false,"recommended":false,"fit":"no_gpu","note":"needs a GPU"}]}"#;
        assert_eq!(parse_list(nothing, false).unwrap().refusal, Some(Refusal::NoNvidia), "an AMD or Intel PC has no driver of NVIDIA's");
        let old = parse_list(nothing, true).unwrap();
        assert!(matches!(&old.refusal, Some(Refusal::Driver(said)) if said.contains("insufficient")), "{old:?}");
        assert!(old.refusal.unwrap().words().contains("newest driver"));
        assert!(old.entries.is_empty());
        let small = r#"{"machine":{"gpus":[{"ordinal":0,"name":"RTX 2060","total_bytes":6442450944,"free_bytes":5000000000}],"gpu_error":null,"ram_bytes":34359738368},"bundles":[
            {"model":"Qwen3.8-Flash-Next","provider":"ISTA-DASLab","creator":"Qwen","license":"Qwen Community License 1.0","license_url":"https://huggingface.co/Qwen/Qwen3.8-Flash-Next/blob/main/LICENSE","quant":"IQ3_S","download_bytes":1,"runs":false,"note":"needs 11 GiB of VRAM; this machine has 6 GiB"},
            {"model":"Qwen3.8-Flash-Next","provider":"ISTA-DASLab","creator":"Qwen","license":"Qwen Community License 1.0","license_url":"https://huggingface.co/Qwen/Qwen3.8-Flash-Next/blob/main/LICENSE","quant":"Q2_0","download_bytes":1,"runs":false,"note":"needs 10 GiB of VRAM; this machine has 6 GiB"},
            {"model":"DeepSeek-V4.1-Flash-1017","provider":"ISTA-DASLab","creator":"DeepSeek","license":"MIT","license_url":"https://huggingface.co/deepseek-ai/DeepSeek-V4.1-Flash-1017/blob/main/LICENSE","quant":"IQ2_XXS","download_bytes":1,"runs":false,"note":"needs 30 GiB of VRAM; this machine has 6 GiB"}]}"#;
        let small = parse_list(small, true).unwrap();
        assert_eq!(small.refusal, Some(Refusal::TooLittleMemory { needs: "10 GiB".into(), has: "6 GiB".into() }), "the least it would take");
        assert_eq!(small.machine, "RTX 2060 (6 GB)  ·  32 GB of memory");
        // (a Straydog that measures what is free says so too)
        let free = r#"{"machine":{"gpus":[{"ordinal":0,"name":"RTX 5070 Ti","total_bytes":17066033152}],"ram_bytes":66126708736,"free_ram_bytes":30064771072},"bundles":[]}"#;
        assert_eq!(parse_list(free, true).unwrap().machine, "RTX 5070 Ti (16 GB)  ·  62 GB of memory, 28 GB free now");
    }

    /// An engine stand-in: a program that writes `lines` to its error stream and exits with `code`, listened to as the engine is.
    fn stand_in(lines: &[&str], code: i32) -> Engine {
        let mut steps: Vec<String> = lines.iter().map(|line| format!("echo {line} 1>&2")).collect();
        steps.push(format!("exit /b {code}"));
        let mut command = Command::new("cmd.exe");
        command.args(["/c", &steps.join(" & ")]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        hidden(&mut command);
        let mut child = command.spawn().unwrap();
        let said = child.stderr.take().map_or_else(Said::default, listen);
        Engine { child, _job: None, not_held: None, said, port: 0 }
    }

    fn why_it_ended(lines: &[&str], code: i32) -> String {
        let mut engine = stand_in(lines, code);
        for _ in 0..100 {
            if let Some(why) = engine.ended() {
                return why;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the stand-in did not end");
    }

    /// An engine that panics is shown the panic and its message, and not the note about a backtrace that comes last; one that writes an error and ends is shown that, as before.
    #[test]
    fn an_engine_that_panicked_is_shown_why_and_not_the_note_that_follows() {
        let note = "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace";
        let now = why_it_ended(&["thread 'main' (4412) panicked at src\\main.rs:3:5:", "index out of bounds: the len is 3 but the index is 7", note], 101);
        assert_eq!(now, "thread 'main' (4412) panicked at src\\main.rs:3:5: index out of bounds: the len is 3 but the index is 7");
        let before = why_it_ended(&["Straydog loading", "thread 'main' panicked at src\\main.rs:3:5:", "no such expert", note], 101);
        assert_eq!(before, "thread 'main' panicked at src\\main.rs:3:5: no such expert");
        // a worker's panic, the engine going on after it
        let worker = why_it_ended(&["thread 'worker-1' (77) panicked at gpu.rs:9:1:", "CUDA out of memory", note, "worker 1 exited with code 101"], 1);
        assert!(worker.contains("CUDA out of memory") && !worker.contains("RUST_BACKTRACE"), "{worker}");
        // an error that main returns is one line, and is the reason as it was
        assert_eq!(why_it_ended(&["Straydog loading", "Error: no model in D:\\m"], 1), "Error: no model in D:\\m");
        assert_eq!(why_it_ended(&[], 3).starts_with("the engine ended ("), true, "nothing said");
    }

    #[test]
    fn only_the_last_lines_are_kept_and_the_note_is_never_the_reason() {
        let note = "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace".to_string();
        // the panic scrolled out, and its note alone is left: the line before it
        assert_eq!(why_it_said(&["the last thing it said".to_string(), note.clone()]).as_deref(), Some("the last thing it said"));
        assert_eq!(why_it_said(&[note.clone()]), None);
        assert_eq!(why_it_said(&[]), None);
        // the last panic of several, and a backtrace that is not cut by the note
        let two: Vec<String> = ["thread 'a' panicked at x.rs:1:1:", "first", "thread 'b' (2) panicked at y.rs:2:2:", "second", "stack backtrace:", "   0: frame"].map(String::from).to_vec();
        assert_eq!(why_it_said(&two).as_deref(), Some("thread 'b' (2) panicked at y.rs:2:2: second"));
        // the oldest form has its message on the line
        assert_eq!(why_it_said(&["thread 'main' panicked at 'boom', src/main.rs:3:5".to_string(), note]).as_deref(), Some("thread 'main' panicked at 'boom', src/main.rs:3:5"));
        // twenty lines are kept of thirty
        let many: Vec<String> = (1..=30).map(|at| format!("line {at}")).collect();
        let mut engine = stand_in(&many.iter().map(String::as_str).collect::<Vec<_>>(), 1);
        while engine.ended().is_none() {
            std::thread::sleep(Duration::from_millis(50));
        }
        let kept: Vec<String> = engine.said.lock().unwrap().iter().cloned().collect();
        assert_eq!((kept.len(), kept.first().map(String::as_str), kept.last().map(String::as_str)), (LINES_KEPT, Some("line 11"), Some("line 30")));
    }

    /// Memory is shown as Windows shows it (units of 2^30, written GB), and the sizes of downloads as Straydog writes them (10^9).
    #[test]
    fn memory_is_counted_as_windows_counts_it() {
        let machine = |card: u64, ram: u64| parse_list(&format!(r#"{{"machine":{{"gpus":[{{"ordinal":0,"name":"Card","total_bytes":{card}}}],"ram_bytes":{ram}}},"bundles":[]}}"#), true).unwrap().machine;
        // as CUDA gives them: an 8 GiB card reads 8 GB (9 when divided by 10^9), and so on up
        assert_eq!(machine(8_585_216_000, 34_359_738_368), "Card (8 GB)  ·  32 GB of memory");
        for (card, wanted) in [(4_294_967_296u64, "4 GB"), (6_442_450_944, "6 GB"), (8_589_934_592, "8 GB"), (12_884_901_888, "12 GB"), (17_171_480_576, "16 GB"), (25_757_220_864, "24 GB")] {
            assert_eq!(machine(card, 1 << 34), format!("Card ({wanted})  ·  16 GB of memory"));
        }
        assert_eq!(memory_gb(34_359_738_368), "32 GB");
        assert_eq!(memory_gb(0), "0 GB");
        // a download is as Straydog says it: 75 GB is 75, and not 70
        assert_eq!(gb(75_000_000_000), "75 GB");
        assert_eq!(gb_file(1_800_000_000), "1.8 GB");
    }

    /// What Straydog 0.1.1 says on a 16 GB card: DeepSeek does not run. It is in the list after the rest, greyed and not recommended, with why,
    /// for whoever wants to try.
    #[test]
    fn a_model_short_of_graphics_memory_is_listed_after_the_rest_not_recommended() {
        let note = |needs: &str| format!("needs {needs} GiB of VRAM free, with what `auto` leaves for the desktop; this machine has 12.2 GiB for it");
        let list = format!(r#"{{"machine":{{"gpus":[{{"name":"RTX 5070 Ti","total_bytes":17179869184}}],"ram_bytes":66571993088}},"bundles":[
            {{"model":"DeepSeek-V4.1-Flash-1017","provider":"ISTA-DASLab","creator":"DeepSeek","license":"MIT","license_url":"https://huggingface.co/deepseek-ai/DeepSeek-V4.1-Flash-1017/blob/main/LICENSE","quant":"IQ2_XXS","download_bytes":1,"runs":false,"fit":"too_little_vram","note":"{}"}},
            {{"model":"DeepSeek-V4.1-Flash-1017","provider":"ISTA-DASLab","creator":"DeepSeek","license":"MIT","license_url":"https://huggingface.co/deepseek-ai/DeepSeek-V4.1-Flash-1017/blob/main/LICENSE","quant":"IQ2_XS","download_bytes":1,"runs":false,"fit":"too_little_vram","note":"{}"}},
            {{"model":"Qwen3.8-Flash-Next","provider":"ISTA-DASLab","creator":"Qwen","license":"Qwen Community License 1.0","license_url":"https://huggingface.co/Qwen/Qwen3.8-Flash-Next/blob/main/LICENSE","quant":"IQ2_XS","download_bytes":1,"runs":true,"fit":"ram"}}]}}"#, note("16.9"), note("14.8"));
        let catalog = parse_list(&list, true).unwrap();
        let names: Vec<(&str, bool)> = catalog.entries.iter().map(|entry| (entry.name.as_str(), entry.runs)).collect();
        assert_eq!(names, [("Qwen3.8-Flash-Next · ISTA-DASLab · IQ2_XS", true), ("DeepSeek-V4.1-Flash-1017 · ISTA-DASLab · IQ2_XXS", false), ("DeepSeek-V4.1-Flash-1017 · ISTA-DASLab · IQ2_XS", false)]);
        assert_eq!(catalog.refusal, None);
        assert!(catalog.entries[1].detail.contains("16.9 GiB") && catalog.entries[1].detail.contains("12.2 GiB") && !catalog.entries[1].detail.starts_with("Recommended"), "{}", catalog.entries[1].detail);
        // (and with nothing that runs, the least of it is the refusal's, in these words too; what is short is still there to try)
        let none = list.replace(r#""runs":true"#, &format!(r#""runs":false,"note":"{}""#, note("11.0")));
        let none = parse_list(&none, true).unwrap();
        assert_eq!(none.refusal, Some(Refusal::TooLittleMemory { needs: "11.0 GiB".into(), has: "12.2 GiB".into() }));
        assert_eq!(none.entries.len(), 3);
        // (one that needs a GPU this PC has not is not offered at all)
        let no_gpu = list.replace(&note("16.9"), "needs a GPU");
        assert_eq!(parse_list(&no_gpu, true).unwrap().entries.len(), 2);
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
        let args = setup_arguments(Some(&Bundle::new("Qwen3.8-Flash-Next", "ISTA-DASLab", "IQ2_XS")), Path::new("M"), Path::new("C"), false, true);
        assert_eq!(&args[..2], ["setup", "--yes"]);
        assert!(args.contains(&"--no-mtp".to_string()) && args.contains(&"IQ2_XS".to_string()));
        let chosen = setup_arguments(None, Path::new("M"), Path::new("C"), true, false);
        assert!(chosen.contains(&"--no-vision".to_string()), "left out when asked");
        assert!(!chosen.contains(&"--no-mtp".to_string()) && !chosen.contains(&"--quant".to_string()));
        assert!(!chosen.contains(&"--vision-only".to_string()), "only for a model already there");
    }

    #[test]
    fn loading_reads_in_a_line() {
        assert_eq!(progress_text(&json!({"stage": "weights", "done": 1, "total": 4})), "weights 25%");
        assert_eq!(progress_text(&json!(null)), "loading");
    }

    /// An engine at a free port that answers `/health` as `script` says (its loading, then ready), one answer for each connection, each on a thread of its own so that a close it never
    /// hears blocks no other. When the script is over it stops listening, and what comes next is refused. The connections it took are counted.
    ///
    /// (A failed poll is not made with this: on this machine the loopback has a filter that reads the traffic, which turns an answer that is no answer, or a connection that is dropped, into a
    /// page of its own or into a reset that comes or does not. A poll that fails is scripted in `wait_until_ready` below, and a refused connection is a real one.)
    fn engine_that_answers(script: Vec<Health>) -> (Client, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let taken = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = taken.clone();
        std::thread::spawn(move || {
            for step in script {
                let Ok((mut stream, _)) = listener.accept() else { return };
                counted.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let (mut head, mut block) = (Vec::new(), [0u8; 4096]);
                    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut block) {
                            Ok(0) | Err(_) => return,
                            Ok(count) => head.extend_from_slice(&block[..count]),
                        }
                    }
                    let body = match step {
                        Health::Loading(progress) => json!({"status": "loading", "progress": progress}).to_string(),
                        _ => json!({"status": "ok"}).to_string(),
                    };
                    let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes());
                    // (the client closes first)
                    while matches!(stream.read(&mut block), Ok(count) if count > 0) {}
                });
            }
        });
        (client(port), taken)
    }

    const STOPPED_ANSWERING: &str = "the engine stopped answering: /log shows why, and writing again tries once more";

    fn loading() -> Health {
        Health::Loading(json!({"stage": "weights", "done": 1, "total": 4}))
    }

    /// `wait_until_ready` over a list of what each ask comes to, with no pause: what it comes to, and how many asks were made.
    fn waited_over(asks: Vec<Result<Health, String>>) -> (Result<(), String>, usize, Vec<String>) {
        let (mut asks, mut made, mut said) = (asks.into_iter(), 0, Vec::new());
        let result = wait_until_ready(&mut || { made += 1; asks.next().expect("asked after the list was over") }, Duration::ZERO, &|| true, &mut |line| said.push(line));
        (result, made, said)
    }

    #[test]
    fn once_the_engine_has_answered_one_or_two_failed_polls_in_a_row_do_not_end_the_wait_and_an_answer_clears_the_count() {
        let failed = || Err("no answer".to_string());
        // (four failed polls in all, but never three in a row)
        let (result, made, _) = waited_over(vec![Ok(loading()), failed(), failed(), Ok(loading()), failed(), failed(), Ok(Health::Ready)]);
        assert_eq!((result, made), (Ok(()), 7));
    }

    #[test]
    fn three_failed_polls_in_a_row_after_the_engine_has_answered_end_the_wait_and_it_asks_no_more() {
        let failed = || Err("no answer".to_string());
        let (result, made, _) = waited_over(vec![Ok(loading()), failed(), failed(), failed()]);
        assert_eq!((result, made), (Err(t(STOPPED_ANSWERING).to_string()), 4));
    }

    #[test]
    fn before_the_engine_has_answered_no_number_of_failed_polls_ends_the_wait() {
        let failed = || Err("no answer".to_string());
        let (result, made, said) = waited_over(vec![failed(), failed(), failed(), failed(), failed(), Ok(Health::Ready)]);
        assert_eq!((result, made), (Ok(()), 6));
        assert_eq!(said, vec![t("starting the engine").to_string(); 5], "it says it is starting, as it did");
    }

    #[test]
    fn an_engine_that_answers_loading_and_then_ready_is_waited_for_over_a_real_connection() {
        let (client, taken) = engine_that_answers(vec![loading(), loading(), Health::Ready]);
        assert_eq!(wait_ready(&client, &|| true, &mut |_| {}), Ok(()));
        assert_eq!(taken.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn an_engine_that_has_answered_and_then_stops_listening_ends_the_wait_in_a_moment() {
        let (client, _) = engine_that_answers(vec![loading()]);
        let started = std::time::Instant::now();
        assert_eq!(wait_ready(&client, &|| true, &mut |_| {}), Err(t(STOPPED_ANSWERING).to_string()));
        // (three refused connections and the pauses between them: seconds, as a refused connection takes a couple of seconds to be said on Windows; not the minute of three silent polls)
        assert!(started.elapsed() < Duration::from_secs(30), "{:?}", started.elapsed());
    }
}
