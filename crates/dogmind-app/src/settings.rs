//! What the app remembers: its providers and choices, as JSON in its own folder. The API keys stay out of it: a provider
//! names a file or a variable that holds its key.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
            Kind::Embedded => "Straydog on this PC",
            Kind::Server => "OpenAI-compatible (server or service)",
            Kind::Codex => "Codex (ChatGPT plan)",
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
    /// A file holding the key, or `env:NAME`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
}

impl ProviderConfig {
    /// The key it names; an empty setting is no key, one that cannot be read is an error.
    pub fn key(&self) -> Result<Option<String>, String> {
        if self.api_key.is_empty() {
            return Ok(None);
        }
        if let Some(name) = self.api_key.strip_prefix("env:") {
            return std::env::var(name).map(Some).map_err(|_| format!("{}: the variable {name} is not set", self.name));
        }
        std::fs::read_to_string(&self.api_key).map(|text| Some(text.trim().to_string())).map_err(|error| format!("{}: {}: {error}", self.name, self.api_key))
    }

    /// What is wrong with it as typed.
    pub fn problem(&self) -> Option<String> {
        if self.name.trim().is_empty() {
            return Some("a provider needs a name".into());
        }
        if self.kind == Kind::Server && dogmind_agent::http::Url::parse(self.base_url.trim()).is_err() {
            return Some("a server's URL looks like http://192.168.0.100:8080/v1".into());
        }
        if self.kind == Kind::Anthropic && !self.base_url.trim().is_empty() && dogmind_agent::http::Url::parse(self.base_url.trim()).is_err() {
            return Some("the URL looks like https://api.anthropic.com/v1, or leave it empty".into());
        }
        if self.kind == Kind::Anthropic && self.api_key.trim().is_empty() {
            return Some("Claude needs an API key: a file holding it, or env:ANTHROPIC_API_KEY".into());
        }
        None
    }
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
    /// A file holding a Brave Search API key, or `env:NAME`. Empty: the free indexes, which may refuse a program.
    pub search_key: String,
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
            vram: None,
            context: None,
            port: 18480,
            models_folder: None,
            straydog: None,
            set_up: false,
            right_panel: true,
            web_tools: true,
            language: "auto".into(),
            search_key: String::new(),
        }
    }
}

impl Settings {
    /// A file that cannot be read gives the defaults, and the broken one is kept beside them.
    pub fn load(path: &Path) -> Settings {
        let Ok(text) = std::fs::read_to_string(path) else { return Settings::default() };
        match serde_json::from_str(&text) {
            Ok(settings) => settings,
            Err(_) => {
                let _ = std::fs::rename(path, path.with_extension("json.broken"));
                Settings::default()
            }
        }
    }

    /// Whole or not at all: the file changes by a rename.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        let part = path.with_extension("json.part");
        std::fs::write(&part, text).and_then(|()| std::fs::rename(&part, path)).map_err(|error| format!("{}: {error}", path.display()))
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

/// A VRAM budget as a person writes it: a number of GiB, or `auto` (or nothing) for what Straydog finds free.
pub fn parse_vram(text: &str) -> Result<Option<f64>, String> {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    match text.parse::<f64>() {
        Ok(gib) if gib > 0.0 => Ok(Some(gib)),
        _ => Err("VRAM is a number of GiB, or auto".to_string()),
    }
}

pub fn show_vram(vram: Option<f64>) -> String {
    vram.map_or("auto".to_string(), |gib| gib.to_string())
}

/// The app's folder: settings, sessions, the engine's settings and its log.
pub fn data_folder() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map_or_else(std::env::temp_dir, PathBuf::from).join("DogmindApp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vram_budget_is_auto_unless_a_number_is_given() {
        assert_eq!(Settings::default().vram, None);
        assert_eq!(show_vram(None), "auto");
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
        assert_eq!(Settings::load(&path), Settings::default());
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
