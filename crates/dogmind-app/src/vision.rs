//! Whether the model on this PC reads pictures, from its files: its vision file beside it, or set aside. The engine takes any such file it finds when it starts,
//! and has no word for leaving one alone: a file set aside under another name is not found, is not a thing setup would fetch again while it is left out, and comes back by its own name.

use crate::engine::Bundle;
use std::path::{Path, PathBuf};

const SET_ASIDE: &str = ".off";

/// What `engine.toml` says of the model set up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub model: PathBuf,
    /// The bundle, by `model_name`, `provider` and `quant`: settings an older setup wrote are read as Straydog reads them.
    pub bundle: Bundle,
    /// It has an MTP drafter: setup is to keep it.
    pub drafter: bool,
    /// A file named by hand, in `model_vision`.
    pub named: Option<PathBuf>,
}

/// What is on the disk now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    On(PathBuf),
    /// Set aside: no download to have it again, and the room to win by deleting it.
    Off(PathBuf),
    Absent,
}

/// The value of `key = "..."` or `key = '...'` on a line of its own.
fn value_of(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        if name.trim() != key {
            return None;
        }
        let value = value.trim();
        let quoted = value.strip_prefix('\'').and_then(|rest| rest.strip_suffix('\''));
        quoted.map(str::to_string).or_else(|| value.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')).map(|inner| inner.replace("\\\\", "\\")))
    })
}

pub fn installed(config: &str) -> Option<Installed> {
    Some(Installed {
        model: PathBuf::from(value_of(config, "model")?),
        bundle: Bundle::from_settings(value_of(config, "model_name").as_deref(), value_of(config, "provider").as_deref(), &value_of(config, "quant")?),
        drafter: value_of(config, "model_draft").is_some(),
        named: value_of(config, "model_vision").map(PathBuf::from),
    })
}

/// Whether a file's name is a vision file's: Qwen3.8's `mmproj-*.gguf`, or the one a DeepSeek bundle keeps with its shards.
fn is_vision_file(name: &str) -> bool {
    name.ends_with(".gguf") && (name.starts_with("mmproj") || name.contains("-Vision-"))
}

/// Where the engine looks: the bundle's folder and the one above it, which all a model's quantizations share.
fn places(installed: &Installed) -> Vec<PathBuf> {
    std::iter::once(installed.model.clone()).chain(installed.model.parent().map(Path::to_path_buf)).collect()
}

fn found(installed: &Installed, wanted: impl Fn(&str) -> bool) -> Option<PathBuf> {
    places(installed).into_iter().find_map(|place| {
        let mut files: Vec<PathBuf> = std::fs::read_dir(place).into_iter().flatten().flatten().map(|entry| entry.path())
            .filter(|path| path.is_file() && path.file_name().and_then(|name| name.to_str()).is_some_and(&wanted)).collect();
        files.sort();
        files.into_iter().next()
    })
}

pub fn state(installed: &Installed) -> State {
    if let Some(named) = installed.named.as_ref().filter(|named| named.is_file()) {
        return State::On(named.clone());
    }
    if let Some(on) = found(installed, is_vision_file) {
        return State::On(on);
    }
    match found(installed, |name| name.strip_suffix(SET_ASIDE).is_some_and(is_vision_file)) {
        Some(off) => State::Off(off),
        None => State::Absent,
    }
}

/// The settings with `model_vision` on or off, by a `#` before it: a name left there for a file set aside would stop the engine starting.
fn with_key(config: &str, on: bool) -> String {
    config.lines().map(|line| {
        let bare = line.trim_start().trim_start_matches('#').trim_start();
        if bare.split_once('=').is_some_and(|(name, _)| name.trim() == "model_vision") {
            if on { bare.to_string() } else { format!("# {bare}") }
        } else {
            line.to_string()
        }
    }).collect::<Vec<_>>().join("\n") + "\n"
}

/// For the engine's next start, which is not this one: a running engine has the file open.
pub fn switch(config_file: &Path, on: bool) -> Result<(), String> {
    let config = std::fs::read_to_string(config_file).map_err(|error| format!("{}: {error}", config_file.display()))?;
    if config.lines().any(|line| line.trim_start().trim_start_matches('#').trim_start().split_once('=').is_some_and(|(name, _)| name.trim() == "model_vision")) {
        std::fs::write(config_file, with_key(&config, on)).map_err(|error| format!("{}: {error}", config_file.display()))?;
    }
    let config = std::fs::read_to_string(config_file).map_err(|error| format!("{}: {error}", config_file.display()))?;
    let Some(installed) = installed(&config) else { return Ok(()) };
    if on {
        if let Some(off) = found(&installed, |name| name.strip_suffix(SET_ASIDE).is_some_and(is_vision_file)) {
            let name = off.to_string_lossy().strip_suffix(SET_ASIDE).map(PathBuf::from).unwrap_or_else(|| off.clone());
            std::fs::rename(&off, &name).map_err(|error| format!("{}: {error}", off.display()))?;
        }
    } else {
        // (every one the engine would find)
        while let Some(file) = found(&installed, is_vision_file) {
            let aside = format!("{}{SET_ASIDE}", file.display());
            std::fs::rename(&file, &aside).map_err(|error| format!("{}: {error}", file.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    fn setup(dir: &Path, extra: &str) -> (PathBuf, Installed) {
        let model = dir.join("Qwen").join("IQ2_XS");
        std::fs::create_dir_all(&model).unwrap();
        let config = dir.join("engine.toml");
        std::fs::write(&config, format!("# a comment\nmodel = '{}'\nquant = \"IQ2_XS\"\n{extra}", model.display())).unwrap();
        let read = installed(&std::fs::read_to_string(&config).unwrap()).unwrap();
        (config, read)
    }

    #[test]
    fn the_settings_are_read_as_setup_writes_them() {
        // (settings of 0.1.3 and before have no model and no provider: the table read them)
        let read = installed("# Written by setup\nmodel = 'C:\\m\\IQ3_XXS'\nmodel_draft = 'C:\\m\\mtp'\nquant = \"IQ3_XXS\"\n").unwrap();
        assert_eq!(read, Installed { model: PathBuf::from(r"C:\m\IQ3_XXS"), bundle: Bundle::new("Qwen3.8-Flash-Next", "ISTA-DASLab", "IQ3_XXS"), drafter: true, named: None });
        assert_eq!(installed("port = 9000\n"), None, "no model set up");
        // (a newer setup says whose bundle it is, and who quantized it: two models may each have a UD-IQ1_M)
        let read = installed("model = 'C:\\m\\UD-IQ1_M'\nmodel_name = \"Qwen3.8-Flash-Next\"\nprovider = \"Unsloth\"\nquant = \"UD-IQ1_M\"\n").unwrap();
        assert_eq!(read.bundle, Bundle::new("Qwen3.8-Flash-Next", "Unsloth", "UD-IQ1_M"));
        let old = installed("model = 'C:\\m\\GSQ-RCO-IQ2_XS'\nquant = \"GSQ-RCO-IQ2_XS\"\n").unwrap();
        assert_eq!(old.bundle, Bundle::new("DeepSeek-V4.1-Flash-1017", "ISTA-DASLab", "IQ2_XS"));
    }

    #[test]
    fn the_state_is_what_is_on_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let (_, read) = setup(dir.path(), "");
        assert_eq!(state(&read), State::Absent);
        // Beside the bundle's folder, where setup puts Qwen's.
        let beside = dir.path().join("Qwen").join("mmproj-Qwen3.8-Flash-Next-BF16.gguf");
        file(&beside);
        assert_eq!(state(&read), State::On(beside));
    }

    #[test]
    fn deepseeks_file_with_its_shards_counts_and_an_unfinished_one_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let (_, read) = setup(dir.path(), "");
        file(&read.model.join("DeepSeek-V4.1-Flash-1017-Vision-BF16.gguf.part"));
        file(&read.model.join("DeepSeek-V4.1-Flash-1017-GSQ-RCO-IQ2_XS.gguf"));
        assert_eq!(state(&read), State::Absent);
        let whole = read.model.join("DeepSeek-V4.1-Flash-1017-Vision-BF16.gguf");
        file(&whole);
        assert_eq!(state(&read), State::On(whole));
    }

    #[test]
    fn turning_it_off_keeps_the_file_where_the_engine_will_not_find_it_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let (config, read) = setup(dir.path(), "");
        let beside = dir.path().join("Qwen").join("mmproj-x.gguf");
        file(&beside);
        switch(&config, false).unwrap();
        assert!(!beside.exists());
        let aside = dir.path().join("Qwen").join("mmproj-x.gguf.off");
        assert_eq!(state(&read), State::Off(aside.clone()));
        assert!(aside.is_file());
        switch(&config, true).unwrap();
        assert_eq!(state(&read), State::On(beside));
    }

    #[test]
    fn a_file_named_by_hand_is_turned_off_in_the_settings() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("mine").join("encoder.gguf");
        file(&elsewhere);
        let (config, read) = setup(dir.path(), &format!("model_vision = '{}'\n", elsewhere.display()));
        assert_eq!(state(&read), State::On(elsewhere.clone()));
        switch(&config, false).unwrap();
        let text = std::fs::read_to_string(&config).unwrap();
        assert_eq!(installed(&text).unwrap().named, None, "{text}");
        assert!(text.contains("# model_vision"), "kept, for turning it on again");
        assert!(elsewhere.is_file(), "where the person put it");
        switch(&config, true).unwrap();
        assert_eq!(installed(&std::fs::read_to_string(&config).unwrap()).unwrap().named, Some(elsewhere));
    }
}
