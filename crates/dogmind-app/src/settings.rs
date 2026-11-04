//! What the app remembers: its providers and choices, as JSON in its own folder. The API keys stay out of it: a provider
//! says that Credential Manager holds its key, or names a variable or a file that does.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::keys::{self, KeyStore};
use crate::lang::{t, tf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// `straydog.exe` beside this app, started by it.
    Embedded,
    /// Any OpenAI-compatible API: Straydog on another machine, llama-server, a service with an API key.
    Server,
    /// A ChatGPT plan through the Codex CLI.
    Codex,
    /// Claude through its API, with an API key.
    Anthropic,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Embedded, Kind::Server, Kind::Codex, Kind::Anthropic];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Embedded => "On this PC (Straydog)",
            Kind::Server => "Another computer or an online service",
            Kind::Codex => "ChatGPT plan (Codex)",
            Kind::Anthropic => "Claude (API key)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub kind: Kind,
    /// `http://host:port/v1`, for a server; empty for Claude means its own.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    /// Empty: what the provider offers first.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Where its key is: see `KeySource`. Never the key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
}

/// What the setting for a key says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    None,
    /// In Credential Manager, under this name: `credential:NAME`.
    Stored(String),
    /// In a variable of the environment: `env:NAME`.
    Variable(String),
    /// In a file: a path. Nothing offers this any more, and what was set so keeps working.
    File(String),
}

impl KeySource {
    pub fn of(setting: &str) -> KeySource {
        let setting = setting.trim();
        if setting.is_empty() {
            KeySource::None
        } else if let Some(target) = setting.strip_prefix(CREDENTIAL) {
            KeySource::Stored(target.to_string())
        } else if let Some(name) = setting.strip_prefix("env:") {
            KeySource::Variable(name.to_string())
        } else {
            KeySource::File(setting.to_string())
        }
    }

    /// The key it leads to; an empty setting is no key, and one that cannot be read is an error, in words without the key.
    pub fn read(&self, owner: &str, store: &dyn KeyStore) -> Result<Option<String>, String> {
        match self {
            KeySource::None => Ok(None),
            KeySource::Stored(target) => match store.read(target)? {
                Some(key) => Ok(Some(key)),
                None => Err(tf("{0}: the saved API key is gone. Paste it again in Settings.", &[&owner])),
            },
            KeySource::Variable(name) => crate::machine::fresh_variable(name).map(Some).ok_or_else(|| tf("{0} has no API key yet: paste one in Settings, under Connections. (It would come from the variable {1}, which is not set.)", &[&owner, &name])),
            KeySource::File(path) => std::fs::read_to_string(path).map(|text| Some(text.trim().to_string())).map_err(|error| format!("{owner}: {path}: {error}")),
        }
    }
}

/// How a setting says Credential Manager has the key.
pub const CREDENTIAL: &str = "credential:";

/// A name for a new key in Credential Manager, not one that another setting uses. It stays when the provider is renamed.
pub fn new_key_target(owner: &str, in_use: &[&str]) -> String {
    let clean: String = owner.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    let taken = |name: &str| in_use.iter().any(|setting| matches!(KeySource::of(setting), KeySource::Stored(target) if target == name));
    if !taken(&clean) {
        return clean;
    }
    (2..).map(|at| format!("{clean}-{at}")).find(|name| !taken(name)).unwrap_or(clean)
}

/// Whether this can be the name of a variable of the environment; a key pasted there cannot.
pub fn is_variable_name(name: &str) -> bool {
    let mut letters = name.chars();
    letters.next().is_some_and(|first| first.is_ascii_alphabetic() || first == '_') && letters.all(|c| c.is_ascii_alphanumeric() || c == '_') && !holds_a_key_itself(name)
}

impl ProviderConfig {
    /// The key it names; an empty setting is no key, one that cannot be read is an error.
    pub fn key(&self) -> Result<Option<String>, String> {
        KeySource::of(&self.api_key).read(&self.name, keys::store())
    }

    /// What is wrong with it as typed.
    pub fn problem(&self) -> Option<String> {
        if self.name.trim().is_empty() {
            return Some(t("A connection needs a name.").into());
        }
        if self.kind == Kind::Server && dogmind_agent::http::Url::parse(self.base_url.trim()).is_err() {
            return Some(t("An address looks like http://192.168.0.100:8080/v1").into());
        }
        if self.kind == Kind::Anthropic && !self.base_url.trim().is_empty() && dogmind_agent::http::Url::parse(self.base_url.trim()).is_err() {
            return Some(t("An address looks like https://api.anthropic.com/v1, or leave it empty.").into());
        }
        if self.kind == Kind::Anthropic && self.api_key.trim().is_empty() {
            return Some(t("Claude needs an API key: paste it here.").into());
        }
        if let KeySource::Variable(name) = KeySource::of(&self.api_key) {
            if !is_variable_name(&name) {
                return Some(t(VARIABLE_NAME_ONLY).into());
            }
        }
        None
    }
}

/// What to do with a key where the name of a variable is asked for.
pub const VARIABLE_NAME_ONLY: &str = "That is not the name of an environment variable. A key itself goes in the API key field above.";

/// Whether what is in a setting that names where a key is looks like the key: the settings are a plain file, and its words would be shown in errors.
pub fn holds_a_key_itself(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value.starts_with("env:") || value.contains(['\\', '/', ' ', ':']) {
        return false;
    }
    const OPENINGS: [&str; 7] = ["sk-", "sk_", "xai-", "gsk_", "ghp_", "AIza", "pk-"];
    // (a file's name is short, or has its extension)
    OPENINGS.iter().any(|opening| value.starts_with(opening)) || (value.chars().count() >= 24 && !value.contains('.'))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub providers: Vec<ProviderConfig>,
    pub active: String,
    pub workspace: Option<PathBuf>,
    /// Folders a session was started in, the latest first.
    pub workspaces: Vec<PathBuf>,
    pub mode: String,
    pub effort: Option<String>,
    pub safe_commands: bool,
    pub show_thinking: bool,
    /// Tool calls in a row as one line.
    pub compact_tools: bool,
    /// The engine inside the app: its VRAM budget in GiB, its context in tokens and its port.
    pub vram: Option<f64>,
    pub context: Option<u32>,
    pub port: u16,
    pub models_folder: Option<PathBuf>,
    /// `straydog.exe`, when it is not beside this app.
    pub straydog: Option<PathBuf>,
    /// The first run's setup has been through.
    pub set_up: bool,
    /// The panel on the right of a conversation.
    pub right_panel: bool,
    pub web_tools: bool,
    /// What answers are written in: `auto`, `ja` or `en`.
    pub language: String,
    /// What the interface is written in: `system`, `en` or `ja`.
    pub display_language: String,
    /// How the window looks: `system`, `dark` or `light`.
    pub theme: String,
    /// The person has been told that closing the window leaves the app in the tray.
    pub tray_told: bool,
    /// Where a Brave Search API key is, as `KeySource` reads it. Empty: the free indexes, which may refuse a program.
    pub search_key: String,
    /// Where the window was left, and how large, in pixels of the screen: left, top, right, bottom.
    pub window: Option<[i32; 4]>,
    pub maximized: bool,
    /// Sessions kept at the top of the sidebar, by their files.
    pub pinned: Vec<PathBuf>,
    /// How large the conversation's words are, in percent of the standard: one of `TEXT_SIZES`.
    pub text_size: u32,
    /// The skills turned off, by name.
    pub skills_off: Vec<String>,
    /// Ask GitHub, now and then, whether a newer release is out (see `updates`). Off: the app never asks on its own. Nothing is downloaded or installed either way.
    pub check_for_updates: bool,
    /// When GitHub last answered such a question, in seconds since 1970: it is asked again a day later, at the soonest.
    pub last_update_check: Option<u64>,
    /// The release whose notice the person closed (or followed to its page): they are not told of that one again.
    pub dismissed_update: Option<String>,
}

/// The sizes offered for the conversation's words, in percent, and their names.
pub const TEXT_SIZES: [(u32, &str); 4] = [(90, "Small"), (100, "Standard"), (115, "Large"), (130, "Larger")];

impl Settings {
    /// The size chosen, or the standard for a value no version offered.
    pub fn text_percent(&self) -> u32 {
        TEXT_SIZES.iter().map(|(percent, _)| *percent).find(|percent| *percent == self.text_size).unwrap_or(100)
    }

    pub fn skill_is_on(&self, name: &str) -> bool {
        !self.skills_off.iter().any(|off| off.eq_ignore_ascii_case(name))
    }

    /// Turns one skill on or off; its name is kept as found.
    pub fn set_skill(&mut self, name: &str, on: bool) {
        self.skills_off.retain(|off| !off.eq_ignore_ascii_case(name));
        if !on {
            self.skills_off.push(name.to_string());
        }
    }
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            providers: vec![
                ProviderConfig { name: "This PC".into(), kind: Kind::Embedded, base_url: String::new(), model: String::new(), api_key: String::new() },
                ProviderConfig { name: "Codex".into(), kind: Kind::Codex, base_url: String::new(), model: String::new(), api_key: String::new() },
                ProviderConfig { name: "Claude".into(), kind: Kind::Anthropic, base_url: String::new(), model: String::new(), api_key: "env:ANTHROPIC_API_KEY".into() },
            ],
            active: "This PC".into(),
            workspace: None,
            workspaces: Vec::new(),
            mode: "default".into(),
            effort: None,
            safe_commands: false,
            show_thinking: false,
            compact_tools: true,
            vram: None,
            context: None,
            port: 18480,
            models_folder: None,
            straydog: None,
            set_up: false,
            right_panel: true,
            web_tools: true,
            language: "auto".into(),
            display_language: "system".into(),
            theme: "system".into(),
            tray_told: false,
            search_key: String::new(),
            window: None,
            maximized: false,
            pinned: Vec::new(),
            text_size: 100,
            skills_off: Vec::new(),
            check_for_updates: true,
            last_update_check: None,
            dismissed_update: None,
        }
    }
}

impl Settings {
    /// What a new install begins with: files change and safe commands run without asking. Not `Default`, which fills in what a file does not say:
    /// whoever has settings keeps what they had.
    pub fn fresh() -> Settings {
        Settings { mode: "accept_edits".into(), safe_commands: true, ..Settings::default() }
    }

    /// No file is a new install. A file that cannot be read gives the defaults, and the broken one is kept beside them.
    pub fn load(path: &Path) -> Settings {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Settings::fresh(),
            // (not text, or not to be had: kept as a broken one is, or the next save would be all that is left of it)
            Err(_) => {
                let _ = std::fs::rename(path, path.with_extension("json.broken"));
                return Settings::default();
            }
        };
        match serde_json::from_str(&text) {
            Ok(settings) => settings,
            Err(_) => {
                let _ = std::fs::rename(path, path.with_extension("json.broken"));
                Settings::default()
            }
        }
    }

    /// Whole or not at all: the file changes by a rename, of one already on the disk. Unflushed, a crash leaves the new name on nothing but zeros.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(&self.without_keys()).map_err(|error| error.to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        let part = path.with_extension("json.part");
        let written = std::fs::File::create(&part).and_then(|mut file| {
            std::io::Write::write_all(&mut file, text.as_bytes())?;
            file.sync_all()
        });
        written.and_then(|()| std::fs::rename(&part, path)).map_err(|error| format!("{}: {error}", path.display()))
    }

    /// As the file may hold it: whatever else goes wrong, what looks like a key is not written.
    fn without_keys(&self) -> Settings {
        let mut clean = self.clone();
        for provider in &mut clean.providers {
            if holds_a_key_itself(&provider.api_key) {
                provider.api_key.clear();
            }
        }
        if holds_a_key_itself(&clean.search_key) {
            clean.search_key.clear();
        }
        clean
    }

    /// A key an older version let into the file goes to the store, and the file says where it is now. `true`: something moved, and it is to be saved.
    pub fn move_keys_to(&mut self, store: &dyn KeyStore) -> bool {
        let mut moved = false;
        for at in 0..self.providers.len() {
            if holds_a_key_itself(&self.providers[at].api_key) {
                let settings: Vec<&str> = self.providers.iter().map(|provider| provider.api_key.as_str()).chain([self.search_key.as_str()]).collect();
                let target = new_key_target(&self.providers[at].name, &settings);
                if store.write(&target, self.providers[at].api_key.trim()).is_ok() {
                    self.providers[at].api_key = format!("{CREDENTIAL}{target}");
                    moved = true;
                }
            }
        }
        if holds_a_key_itself(&self.search_key) && store.write(SEARCH_KEY_TARGET, self.search_key.trim()).is_ok() {
            self.search_key = format!("{CREDENTIAL}{SEARCH_KEY_TARGET}");
            moved = true;
        }
        moved
    }

    /// Whether the window is to be light now.
    pub fn is_light(&self) -> bool {
        match self.theme.as_str() {
            "light" => true,
            "dark" => false,
            _ => crate::paint::theme::system_is_light(),
        }
    }

    /// Remembers a folder as the latest one.
    pub fn use_workspace(&mut self, folder: PathBuf) {
        let same = |known: &PathBuf| known.to_string_lossy().to_lowercase() == folder.to_string_lossy().to_lowercase();
        self.workspaces.retain(|known| !same(known));
        self.workspaces.insert(0, folder.clone());
        self.workspaces.truncate(30);
        self.workspace = Some(folder);
    }

    /// Makes the provider of this kind the active one, with these details, adding it if there is none.
    pub fn use_provider(&mut self, kind: Kind, base_url: &str, model: &str, api_key: &str) {
        let name = match self.providers.iter().find(|provider| provider.kind == kind) {
            Some(provider) => provider.name.clone(),
            None => {
                let name = match kind {
                    Kind::Embedded => "This PC",
                    Kind::Server => "Server",
                    Kind::Codex => "Codex",
                    Kind::Anthropic => "Claude",
                };
                self.providers.push(ProviderConfig { name: name.into(), kind, base_url: String::new(), model: String::new(), api_key: String::new() });
                name.to_string()
            }
        };
        if let Some(provider) = self.providers.iter_mut().find(|provider| provider.name == name) {
            provider.base_url = base_url.to_string();
            provider.model = model.to_string();
            provider.api_key = api_key.to_string();
        }
        self.active = name;
    }

    pub fn active_provider(&self) -> Option<&ProviderConfig> {
        self.providers.iter().find(|provider| provider.name == self.active).or_else(|| self.providers.first())
    }
}

/// Under what Credential Manager keeps the search service's key.
pub const SEARCH_KEY_TARGET: &str = "web-search";

/// A VRAM budget as a person writes it: a number of GiB, or `auto` (or nothing) for what Straydog finds free.
pub fn parse_vram(text: &str) -> Result<Option<f64>, String> {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    match text.parse::<f64>() {
        Ok(gib) if gib > 0.0 => Ok(Some(gib)),
        _ => Err(t("The graphics memory limit is a number of GB, or leave it empty.").to_string()),
    }
}

/// As it stands in its field: empty is what the field's hint says, automatic.
pub fn show_vram(vram: Option<f64>) -> String {
    vram.map_or(String::new(), |gib| gib.to_string())
}

/// The app's folder: settings, sessions, the engine's settings and its log.
pub fn data_folder() -> PathBuf {
    data_folder_from(std::env::var_os("DOGMIND_DATA").map(PathBuf::from), std::env::var_os("LOCALAPPDATA").map(PathBuf::from), std::env::temp_dir())
}

/// (`named`: for running the app beside another copy, with the sandbox's own profile left where the system keeps it, which moving LOCALAPPDATA would not. Only a folder of its own: this is
/// what Remove empties. A variable with nothing in it, or that says nothing of where, would otherwise put all this wherever the app was started.)
fn data_folder_from(named: Option<PathBuf>, local: Option<PathBuf>, temp: PathBuf) -> PathBuf {
    if let Some(named) = named.filter(|folder| folder.is_absolute() && folder.parent().is_some()) {
        return named;
    }
    local.filter(|folder| folder.is_absolute()).unwrap_or(temp).join("DogmindApp")
}

/// Whether `path` is `folder` or lies under it, as the same folder may be spelled in another case by the system and by a dialog.
pub fn same_or_inside(path: &Path, folder: &Path) -> bool {
    let key = |path: &Path| path.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let (path, folder) = (key(path), key(folder));
    path == folder || path.starts_with(&format!("{folder}\\"))
}

/// The folders this app makes in its data folder, by name.
const MADE_FOLDERS: [&str; 6] = ["sessions", "memory", "preview", "crashes", "attachments", "models"];
/// The beginnings of the names of the files it makes there: each as it is, written beside itself first (`.part`, `.partial`), or
/// moved aside when it could not be read (`.broken`, `.unreadable-TIME.json`), or the engine's copies of its own (`engine.toml.old`).
const MADE_FILES: [&str; 11] = ["settings.json", "engine.toml", "engine.log", "permissions.json", "permissions.partial", "permissions.unreadable-", "workspace-grants.json", "read-folders.json", "mcp.json", "mcp.partial", "report.txt"];

/// Whether the app makes this, at the top of its data folder: a name of its own, a folder as a folder and a file as a file.
fn made_here(path: &Path) -> bool {
    let name = path.file_name().map(|name| name.to_string_lossy().to_lowercase()).unwrap_or_default();
    match std::fs::symlink_metadata(path) {
        Ok(what) if what.is_dir() => MADE_FOLDERS.contains(&name.as_str()),
        Ok(what) if what.is_file() => MADE_FILES.iter().any(|made| name.starts_with(made)),
        _ => false,
    }
}

/// What Remove takes out of the data folder, and only if it is what it should be: that folder can be named by a variable. It says
/// what it is by having this app's settings in it, and it does not hold the person's home. Even then only what this app makes there
/// goes, by its name: a file or a folder of another's stays, and is said (`what_remove_leaves`). `keep`: the model's folder, with
/// whatever holds it, or what lies in it.
pub fn what_remove_takes(data: &Path, keep: Option<&Path>, home: Option<&Path>) -> Result<Vec<PathBuf>, String> {
    // (never started, so nothing to take)
    if !data.exists() {
        return Ok(Vec::new());
    }
    // (another program's settings.json is no sign: this app's always has its connections in it)
    let ours = std::fs::read_to_string(data.join("settings.json")).ok().and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok()).is_some_and(|settings| settings.get("providers").is_some_and(serde_json::Value::is_array));
    if data.parent().is_none() || home.is_some_and(|home| same_or_inside(home, data)) || !ours {
        return Err(tf("{0} does not look like this app's folder, so nothing in it was removed.", &[&data.display()]));
    }
    let kept = |path: &Path| keep.is_some_and(|keep| same_or_inside(keep, path) || same_or_inside(path, keep));
    Ok(std::fs::read_dir(data).into_iter().flatten().flatten().map(|entry| entry.path()).filter(|path| made_here(path) && !kept(path)).collect())
}

/// What is in the data folder that this app did not make, and Remove leaves where it is: for the person to be told.
pub fn what_remove_leaves(data: &Path, keep: Option<&Path>) -> Vec<PathBuf> {
    let kept = |path: &Path| keep.is_some_and(|keep| same_or_inside(keep, path) || same_or_inside(path, keep));
    std::fs::read_dir(data).into_iter().flatten().flatten().map(|entry| entry.path()).filter(|path| !made_here(path) && !kept(path)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_size_and_skills_off_come_from_an_older_file_as_standard_and_none() {
        let mut older: Settings = serde_json::from_str(r#"{"active": "This PC", "theme": "dark"}"#).unwrap();
        assert_eq!((older.text_percent(), older.skills_off.len()), (100, 0));
        older.text_size = 97;
        assert_eq!(older.text_percent(), 100, "a size no version offered");
        older.set_skill("PDF", false);
        assert!(!older.skill_is_on("pdf") && older.skill_is_on("xlsx"));
        older.set_skill("pdf", true);
        assert!(older.skill_is_on("PDF") && older.skills_off.is_empty());
    }

    #[test]
    fn the_update_check_comes_from_an_older_file_as_on_and_never_made_and_is_kept() {
        let older: Settings = serde_json::from_str(r#"{"active": "This PC", "theme": "dark"}"#).unwrap();
        assert_eq!((older.check_for_updates, older.last_update_check, older.dismissed_update.clone()), (true, None, None));
        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("settings.json");
        let mut settings = older;
        settings.check_for_updates = false;
        settings.last_update_check = Some(1_793_000_000);
        settings.dismissed_update = Some("0.2.0".into());
        settings.save(&file).unwrap();
        let kept = Settings::load(&file);
        assert_eq!((kept.check_for_updates, kept.last_update_check, kept.dismissed_update.as_deref()), (false, Some(1_793_000_000), Some("0.2.0")));
        // a new install asks, too: it is a setting the person can turn off, and it starts on
        assert!(Settings::fresh().check_for_updates);
    }

    #[test]
    fn the_data_folder_is_a_folder_of_its_own_wherever_it_is_named() {
        let temp = PathBuf::from("C:\\Temp");
        let local = Some(PathBuf::from("C:\\Users\\me\\AppData\\Local"));
        let at = |named: Option<&str>, local: Option<PathBuf>| data_folder_from(named.map(PathBuf::from), local, temp.clone());
        assert_eq!(at(None, local.clone()), PathBuf::from("C:\\Users\\me\\AppData\\Local\\DogmindApp"));
        assert_eq!(at(Some("D:\\other copy"), local.clone()), PathBuf::from("D:\\other copy"));
        for not_one in ["C:\\", "D:\\", "relative", "", "\\rooted"] {
            assert_eq!(at(Some(not_one), local.clone()), PathBuf::from("C:\\Users\\me\\AppData\\Local\\DogmindApp"), "{not_one}");
        }
        for empty in ["", "relative"] {
            assert_eq!(at(None, Some(PathBuf::from(empty))), PathBuf::from("C:\\Temp\\DogmindApp"), "{empty:?}");
        }
    }

    #[test]
    fn remove_takes_from_this_apps_folder_alone_and_leaves_the_model_however_its_path_is_spelled() {
        let dir = tempfile::tempdir().unwrap();
        // A folder a variable might name, with a person's files in it and no settings.
        let other = dir.path().join("Documents");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("thesis.docx"), "years").unwrap();
        assert!(what_remove_takes(&other, None, None).unwrap_err().contains("does not look like"));
        // Another program's settings are no sign of this app's folder.
        std::fs::write(other.join("settings.json"), "{\"theme\": \"dark\"}").unwrap();
        assert!(what_remove_takes(&other, None, None).unwrap_err().contains("does not look like"));
        // One that holds the home, whatever is in it.
        let home = dir.path().join("Documents").join("me");
        std::fs::write(other.join("settings.json"), "{\"providers\": []}").unwrap();
        assert!(what_remove_takes(&other, None, Some(&home)).is_err());
        assert!(what_remove_takes(Path::new("C:\\"), None, None).is_err());
        // Its own.
        let data = dir.path().join("DogmindApp");
        std::fs::create_dir_all(data.join("sessions")).unwrap();
        std::fs::create_dir_all(data.join("models").join("q")).unwrap();
        std::fs::write(data.join("settings.json"), "{\"providers\": []}").unwrap();
        let sorted = |mut paths: Vec<PathBuf>| {
            paths.sort();
            paths
        };
        assert_eq!(sorted(what_remove_takes(&data, None, None).unwrap()), [data.join("models"), data.join("sessions"), data.join("settings.json")]);
        let shouting = PathBuf::from(data.join("MODELS").join("q").to_string_lossy().to_uppercase());
        assert_eq!(sorted(what_remove_takes(&data, Some(&shouting), None).unwrap()), [data.join("sessions"), data.join("settings.json")], "as a dialog may spell it");
        assert_eq!(what_remove_takes(&data, Some(&data), None).unwrap(), Vec::<PathBuf>::new(), "the model is loose in the folder: nothing can be told from it");
        assert!(same_or_inside(&shouting, &data) && !same_or_inside(&dir.path().join("DogmindApp2"), &data));
    }

    /// Even in its own folder, Remove takes only what this app makes there, by name: what another put there stays, and is said.
    #[test]
    fn remove_takes_what_this_app_makes_and_leaves_what_another_put_there() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("DogmindApp");
        for folder in ["sessions", "memory", "preview", "crashes", "attachments", "my photos", "settings.json.d"] {
            std::fs::create_dir_all(data.join(folder)).unwrap();
        }
        let made = ["settings.json", "settings.json.broken", "settings.json.part", "engine.toml", "engine.toml.old", "engine.toml.before-drafter", "engine.log", "permissions.json", "permissions.partial", "permissions.unreadable-20261104-093000.json", "workspace-grants.json", "read-folders.json", "mcp.json", "report.txt"];
        let theirs = ["notes.txt", "settings.ini", "permissions.txt", "Sessions.lnk"];
        std::fs::write(data.join("settings.json"), "{\"providers\": [], \"active\": \"\"}").unwrap();
        for file in made.iter().skip(1).chain(&theirs) {
            std::fs::write(data.join(file), "x").unwrap();
        }
        let names = |paths: Vec<PathBuf>| {
            let mut names: Vec<String> = paths.iter().map(|path| path.file_name().unwrap().to_string_lossy().into_owned()).collect();
            names.sort();
            names
        };
        let mut expected: Vec<String> = made.iter().chain(&["sessions", "memory", "preview", "crashes", "attachments"]).map(|name| name.to_string()).collect();
        expected.sort();
        assert_eq!(names(what_remove_takes(&data, None, None).unwrap()), expected);
        assert_eq!(names(what_remove_leaves(&data, None)), ["Sessions.lnk", "my photos", "notes.txt", "permissions.txt", "settings.ini", "settings.json.d"], "a file of a folder's name is not the folder, nor a folder of a file's");
    }

    #[test]
    fn a_vram_budget_is_auto_unless_a_number_is_given() {
        assert_eq!(Settings::default().vram, None);
        assert_eq!(show_vram(None), "");
        assert_eq!(show_vram(Some(8.0)), "8");
        for text in ["", "  ", "auto", "AUTO", " Auto "] {
            assert_eq!(parse_vram(text), Ok(None), "{text:?}");
        }
        assert_eq!(parse_vram(" 5.5 "), Ok(Some(5.5)));
        for text in ["-1", "0", "lots"] {
            assert!(parse_vram(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn settings_come_back_as_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/settings.json");
        assert_eq!(Settings::load(&path), Settings::fresh());
        let mut settings = Settings::default();
        settings.providers.push(ProviderConfig { name: "homelab".into(), kind: Kind::Server, base_url: "http://192.168.0.100:8080/v1".into(), model: String::new(), api_key: "env:KEY".into() });
        settings.active = "homelab".into();
        settings.vram = Some(5.0);
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path), settings);
        assert_eq!(Settings::load(&path).active_provider().unwrap().kind, Kind::Server);
    }

    #[test]
    fn setup_makes_a_provider_of_its_kind_active() {
        let mut settings = Settings::default();
        settings.use_provider(Kind::Server, "http://h:1/v1", "m", "env:K");
        let active = settings.active_provider().unwrap();
        assert_eq!((active.name.as_str(), active.base_url.as_str(), active.model.as_str(), active.api_key.as_str()), ("Server", "http://h:1/v1", "m", "env:K"));
        settings.use_provider(Kind::Codex, "", "", "");
        assert_eq!(settings.active_provider().unwrap().kind, Kind::Codex);
        assert_eq!(settings.providers.iter().filter(|provider| provider.kind == Kind::Codex).count(), 1);
    }

    #[test]
    fn the_latest_folder_comes_first_once() {
        let mut settings = Settings::default();
        settings.use_workspace(PathBuf::from("C:\\a"));
        settings.use_workspace(PathBuf::from("C:\\b"));
        settings.use_workspace(PathBuf::from("c:\\A"));
        assert_eq!(settings.workspaces, [PathBuf::from("c:\\A"), PathBuf::from("C:\\b")]);
        assert_eq!(settings.workspace, Some(PathBuf::from("c:\\A")));
    }

    #[test]
    fn a_broken_file_gives_the_defaults_and_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
        assert!(path.with_extension("json.broken").is_file());
        // Not even text: the same.
        std::fs::remove_file(path.with_extension("json.broken")).unwrap();
        std::fs::write(&path, [0xFFu8, 0xFE, 0x00, b'{']).unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
        assert_eq!(std::fs::read(path.with_extension("json.broken")).unwrap(), [0xFFu8, 0xFE, 0x00, b'{'], "kept");
    }

    #[test]
    fn a_setting_missing_from_an_older_file_takes_its_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"active": "Codex"}"#).unwrap();
        let settings = Settings::load(&path);
        assert_eq!(settings.active_provider().unwrap().name, "Codex");
        assert_eq!(settings.port, 18480);
    }

    #[test]
    fn only_a_new_install_begins_by_accepting_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let new = Settings::load(&path);
        assert_eq!((new.mode.as_str(), new.safe_commands), ("accept_edits", true), "no file: a new install");
        std::fs::write(&path, r#"{"active": "Codex"}"#).unwrap();
        let older = Settings::load(&path);
        assert_eq!((older.mode.as_str(), older.safe_commands), ("default", false), "a file that does not say keeps what it had");
        std::fs::write(&path, r#"{"mode": "plan", "safe_commands": false}"#).unwrap();
        let saved = Settings::load(&path);
        assert_eq!((saved.mode.as_str(), saved.safe_commands), ("plan", false), "what was chosen stays");
        std::fs::write(&path, "{ not json").unwrap();
        let broken = Settings::load(&path);
        assert_eq!((broken.mode.as_str(), broken.safe_commands), ("default", false), "a broken file is not a new install");
    }

    #[test]
    fn a_key_comes_from_where_the_provider_says() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("key");
        std::fs::write(&file, "secret\n").unwrap();
        let mut provider = ProviderConfig { name: "p".into(), kind: Kind::Server, base_url: String::new(), model: String::new(), api_key: String::new() };
        assert_eq!(provider.key().unwrap(), None);
        provider.api_key = file.display().to_string();
        assert_eq!(provider.key().unwrap().as_deref(), Some("secret"));
        provider.api_key = "env:DOGMIND_APP_NO_SUCH_VARIABLE".into();
        assert!(provider.key().is_err());
    }

    #[test]
    fn what_looks_like_a_key_is_told_from_where_a_key_is() {
        for key in ["sk-abc123", "  sk-proj-0123456789abcdefghij ", "ghp_abcdef", "AIzaSyA0123456789", "abcdefghijklmnopqrstuvwxyz012345"] {
            assert!(holds_a_key_itself(key), "{key}");
        }
        for place in ["", "env:OPENAI_API_KEY", "credential:Claude", "C:\\keys\\openai.txt", "keys/openai.txt", "openai.key", "key", "a-long-file-name-of-a-key-for-this.txt"] {
            assert!(!holds_a_key_itself(place), "{place}");
        }
    }

    #[test]
    fn a_key_is_never_written_to_the_settings() {
        const KEY: &str = "sk-very-secret-0123456789";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let store = keys::MemoryKeys::default();
        let target = new_key_target("Claude", &[]);
        store.write(&target, KEY).unwrap();
        let mut settings = Settings::default();
        settings.providers[2].api_key = format!("{CREDENTIAL}{target}");
        settings.save(&path).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains(KEY));
        assert!(written.contains("credential:Claude"));
        assert_eq!(KeySource::of(&Settings::load(&path).providers[2].api_key).read("Claude", &store).unwrap().as_deref(), Some(KEY), "and it comes back from the store");
        // Whatever put one there, the file does not take it.
        settings.providers[0].api_key = KEY.into();
        settings.search_key = KEY.into();
        settings.save(&path).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains(KEY));
        // Nor does what is shown of a setting or of a failure.
        let gone = KeySource::Stored("nothing-here".into()).read("Claude", &store).unwrap_err();
        assert!(!gone.contains(KEY) && !format!("{:?}", KeySource::of(&format!("{CREDENTIAL}{target}"))).contains(KEY), "{gone}");
    }

    #[test]
    fn a_key_an_older_file_holds_moves_to_the_store() {
        const KEY: &str = "sk-from-an-older-file-012345";
        let store = keys::MemoryKeys::default();
        let mut settings = Settings::default();
        settings.providers[2].api_key = KEY.into();
        settings.search_key = "BSAabcdefghijklmnopqrstuvwxyz0123".into();
        assert!(settings.move_keys_to(&store));
        assert_eq!(settings.providers[2].api_key, "credential:Claude");
        assert_eq!(store.read("Claude").unwrap().as_deref(), Some(KEY));
        assert_eq!(settings.search_key, format!("{CREDENTIAL}{SEARCH_KEY_TARGET}"));
        assert!(!settings.move_keys_to(&store), "nothing left to move");
    }

    #[test]
    fn what_a_setting_says_about_a_key_is_read_as_before() {
        assert_eq!(KeySource::of(""), KeySource::None);
        assert_eq!(KeySource::of(" env:ANTHROPIC_API_KEY "), KeySource::Variable("ANTHROPIC_API_KEY".into()));
        assert_eq!(KeySource::of("C:\\keys\\a.txt"), KeySource::File("C:\\keys\\a.txt".into()));
        assert_eq!(KeySource::of("credential:x"), KeySource::Stored("x".into()));
    }

    #[test]
    fn a_key_gets_a_name_of_its_own() {
        assert_eq!(new_key_target("Claude", &[]), "Claude");
        assert_eq!(new_key_target("my server", &[]), "my-server");
        assert_eq!(new_key_target("Claude", &["credential:Claude"]), "Claude-2");
        assert_eq!(new_key_target("Claude", &["credential:Claude", "credential:Claude-2", "env:Claude-3"]), "Claude-3");
    }

    #[test]
    fn a_variable_has_a_name_and_a_key_has_not() {
        assert!(is_variable_name("ANTHROPIC_API_KEY") && is_variable_name("_x1"));
        for name in ["", "1A", "a b", "sk-abc", "sk_live_0123456789abcdef0123456789"] {
            assert!(!is_variable_name(name), "{name}");
        }
        let provider = ProviderConfig { name: "p".into(), kind: Kind::Server, base_url: "http://h:1/v1".into(), model: String::new(), api_key: "env:sk-oops".into() };
        assert_eq!(provider.problem().as_deref(), Some(VARIABLE_NAME_ONLY));
    }

    #[test]
    fn a_server_needs_a_url_it_can_reach() {
        let mut provider = ProviderConfig { name: "p".into(), kind: Kind::Server, base_url: "ftp://x/v1".into(), model: String::new(), api_key: String::new() };
        assert!(provider.problem().is_some());
        provider.base_url = "http://192.168.0.100:8080/v1".into();
        assert!(provider.problem().is_none());
        provider.base_url = "https://x/v1".into();
        assert!(provider.problem().is_none(), "an https server is reachable too");
        provider.name = " ".into();
        assert!(provider.problem().is_some());
        let claude = ProviderConfig { name: "c".into(), kind: Kind::Anthropic, base_url: String::new(), model: String::new(), api_key: String::new() };
        assert!(claude.problem().is_some(), "no key");
        assert!(ProviderConfig { api_key: "env:K".into(), ..claude }.problem().is_none());
    }
}
