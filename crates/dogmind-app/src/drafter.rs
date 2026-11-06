//! The MTP drafter ("faster-writing data") of a model that is set up already. `straydog setup` has no way to fetch it alone: with `--mtp` it reads every shard of the model again,
//! for a file of under a gigabyte. `straydog fetch-mtp` builds just that file (and keeps one that is there, after checking it), and this module puts it in the settings as setup
//! would: `model_draft` is the folder the file is in. A model yet to be set up gets its drafter from setup itself (`--mtp`, in `engine::setup_arguments`).

use crate::lang::{t, tf};
use crate::vision::Installed;
use std::path::{Path, PathBuf};

const KEY: &str = "model_draft";

/// Where setup puts the drafter of a model: `mtp`, beside the folders of the model's bundles, which all of them share.
pub fn directory(installed: &Installed) -> PathBuf {
    installed.model.parent().map_or_else(|| installed.model.join("mtp"), |parent| parent.join("mtp"))
}

const FETCH: &str = "fetch-mtp";

/// `straydog fetch-mtp`, for the folder the drafter goes in.
pub fn fetch_arguments(directory: &Path) -> Vec<String> {
    vec![FETCH.into(), "--model-draft".into(), directory.display().to_string()]
}

/// Whether a run of `straydog` is this fetch. It downloads and converts on the CPU: it reads no shard of the model and takes nothing of the card, so the engine that is running keeps
/// running beside it, and the fetch is no second engine for one that starts.
pub fn is_fetch(args: &[String]) -> bool {
    args.first().is_some_and(|first| first == FETCH)
}

/// Whether a line of the settings is `model_draft`'s: one that sets it, or with `or_put_aside` one that a `#` has put aside too.
fn is_key(line: &str, or_put_aside: bool) -> bool {
    let line = line.trim_start();
    let bare = if or_put_aside { line.trim_start_matches('#').trim_start() } else { line };
    (or_put_aside || !line.starts_with('#')) && bare.split_once('=').is_some_and(|(name, _)| name.trim() == KEY)
}

/// A path as a TOML string: literal where it can be, as Windows paths are written by setup too.
fn quoted(path: &Path) -> String {
    let text = path.display().to_string();
    if text.contains(['\'', '\r', '\n']) { format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\"")) } else { format!("'{text}'") }
}

/// The settings with `model_draft` naming `directory`: the line there already, set or put aside, is replaced; with none, it goes after `model`, with the rest of what setup writes
/// at the top of the file.
fn with_drafter(config: &str, directory: &Path) -> Option<String> {
    let line = format!("{KEY} = {}", quoted(directory));
    let mut lines: Vec<String> = Vec::new();
    let mut placed = false;
    for existing in config.lines() {
        if is_key(existing, true) {
            if !placed {
                lines.push(line.clone());
                placed = true;
            }
        } else {
            lines.push(existing.to_string());
        }
    }
    if !placed {
        let at = lines.iter().position(|existing| existing.trim_start().split_once('=').is_some_and(|(name, _)| name.trim() == "model"))?;
        lines.insert(at + 1, line);
    }
    Some(lines.join("\n") + "\n")
}

/// The settings with `model_draft` put aside by a `#`: its value stays there to read.
fn without_drafter(config: &str) -> String {
    config.lines().map(|line| if is_key(line, false) { format!("# {}", line.trim_start()) } else { line.to_string() }).collect::<Vec<_>>().join("\n") + "\n"
}

/// Puts the drafter that `fetch-mtp` has made in `directory` into the settings. Whether anything was changed.
pub fn turn_on(config_file: &Path, directory: &Path) -> Result<bool, String> {
    let has_the_file = std::fs::read_dir(directory).into_iter().flatten().flatten().any(|entry| entry.path().extension().is_some_and(|extension| extension == "gguf"));
    if !has_the_file {
        return Err(tf("The faster-writing data is not in {0}.", &[&directory.display()]));
    }
    let config = std::fs::read_to_string(config_file).map_err(|error| format!("{}: {error}", config_file.display()))?;
    let written = with_drafter(&config, directory).ok_or_else(|| t("No model is set up here.").to_string())?;
    if written.trim_end() == config.trim_end() {
        return Ok(false);
    }
    std::fs::write(config_file, written).map_err(|error| format!("{}: {error}", config_file.display()))?;
    Ok(true)
}

/// Takes `model_draft` out of the settings, for the engine's next start: its file stays where it is. Whether anything was changed.
pub fn turn_off(config_file: &Path) -> Result<bool, String> {
    let config = std::fs::read_to_string(config_file).map_err(|error| format!("{}: {error}", config_file.display()))?;
    if !config.lines().any(|line| is_key(line, false)) {
        return Ok(false);
    }
    std::fs::write(config_file, without_drafter(&config)).map_err(|error| format!("{}: {error}", config_file.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Bundle;

    fn installed(model: &str) -> Installed {
        Installed { model: PathBuf::from(model), bundle: Bundle::new("Qwen3.8-Flash-Next", "Unsloth", "UD-IQ1_S"), drafter: false, named: None }
    }

    #[test]
    fn the_drafter_is_fetched_alone_into_the_folder_setup_would_use() {
        let one = installed(r"D:\models\Qwen3.8-Flash-Next\Unsloth-UD-IQ1_S");
        assert_eq!(directory(&one), PathBuf::from(r"D:\models\Qwen3.8-Flash-Next\mtp"));
        assert_eq!(fetch_arguments(&directory(&one)), ["fetch-mtp", "--model-draft", r"D:\models\Qwen3.8-Flash-Next\mtp"]);
        let args = fetch_arguments(&directory(&one)).join(" ");
        assert!(!args.contains("setup") && !args.contains("--quant"), "no bundle is named, and none of its shards is read: {args}");
    }

    #[test]
    fn only_the_fetch_is_the_run_that_leaves_the_engine_alone() {
        let one = installed(r"D:\models\Qwen3.8-Flash-Next\Unsloth-UD-IQ1_S");
        assert!(is_fetch(&fetch_arguments(&directory(&one))));
        let config = Path::new("C");
        let bundle = Bundle::new("Qwen3.8-Flash-Next", "Unsloth", "UD-IQ1_S");
        let setup = crate::engine::setup_arguments(Some(&bundle), Path::new(r"D:\models"), config, true, true);
        assert!(!is_fetch(&setup), "the model's own setup reads its shards and probes the card: {setup:?}");
        let vision = crate::engine::vision_arguments(&Installed { drafter: true, ..one }, config);
        assert!(!is_fetch(&vision), "the vision file's setup too: {vision:?}");
        assert!(!is_fetch(&[]));
    }

    #[test]
    fn the_settings_name_the_drafter_after_the_model_and_the_installed_model_reads_it_back() {
        let config = "# Written by `straydog setup`\nmodel = 'D:\\models\\Qwen3.8-Flash-Next\\Unsloth-UD-IQ1_S'\nmodel_name = \"Qwen3.8-Flash-Next\"\nprovider = \"Unsloth\"\nquant = \"UD-IQ1_S\"\nport = 9000\n";
        let dir = Path::new(r"D:\models\Qwen3.8-Flash-Next\mtp");
        let written = with_drafter(config, dir).unwrap();
        assert_eq!(crate::vision::installed(config).unwrap().drafter, false);
        let read = crate::vision::installed(&written).unwrap();
        assert!(read.drafter, "{written}");
        assert!(written.contains("model_draft = 'D:\\models\\Qwen3.8-Flash-Next\\mtp'\nmodel_name"), "{written}");
        assert!(written.contains("port = 9000") && written.contains("# Written by"), "the rest stays: {written}");
        // once, and the same again changes nothing
        assert_eq!(written.matches("model_draft").count(), 1);
        assert_eq!(with_drafter(&written, dir).unwrap(), written);
        // no model set up: nothing to put it after
        assert_eq!(with_drafter("port = 9000\n", dir), None);
    }

    #[test]
    fn a_line_put_aside_is_replaced_and_a_path_with_a_quote_is_written_as_toml_reads_it() {
        let aside = "model = 'M'\n# model_draft = 'D:\\old\\mtp'\nquant = \"X\"\n";
        let written = with_drafter(aside, Path::new(r"D:\new\mtp")).unwrap();
        assert_eq!(written, "model = 'M'\nmodel_draft = 'D:\\new\\mtp'\nquant = \"X\"\n");
        let odd = with_drafter("model = 'M'\n", Path::new(r"D:\it's\mtp")).unwrap();
        assert!(odd.contains(r#"model_draft = "D:\\it's\\mtp""#), "{odd}");
        assert_eq!(crate::vision::installed(&format!("{odd}quant = \"X\"\n")).unwrap().drafter, true);
    }

    #[test]
    fn turning_it_off_puts_the_line_aside_and_the_model_has_no_drafter_for_the_next_start() {
        let config = "model = 'M'\nmodel_draft = 'D:\\m\\mtp'\nquant = \"X\"\n";
        let off = without_drafter(config);
        assert_eq!(off, "model = 'M'\n# model_draft = 'D:\\m\\mtp'\nquant = \"X\"\n");
        assert!(!crate::vision::installed(&off).unwrap().drafter);
        assert!(crate::vision::installed(config).unwrap().drafter);
        // (and an earlier line that was put aside is not put aside twice)
        assert_eq!(without_drafter(&off), off);
    }

    #[test]
    fn the_files_are_turned_on_and_off_in_the_settings_and_the_drafter_file_stays() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("engine.toml");
        let mtp = dir.path().join("Qwen").join("mtp");
        std::fs::write(&config, "model = 'M'\nquant = \"X\"\n").unwrap();
        // nothing built yet: the settings are not changed, whatever was asked
        assert!(turn_on(&config, &mtp).unwrap_err().contains("mtp"));
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "model = 'M'\nquant = \"X\"\n");
        std::fs::create_dir_all(&mtp).unwrap();
        std::fs::write(mtp.join("mtp-bf16.safetensors"), b"x").unwrap();
        assert!(turn_on(&config, &mtp).is_err(), "the tensors it is built from are not the drafter");
        let file = mtp.join("Qwen3.8-Flash-Next-MTP-Q2_0.gguf");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(turn_on(&config, &mtp), Ok(true));
        assert_eq!(turn_on(&config, &mtp), Ok(false), "already so");
        assert!(crate::vision::installed(&std::fs::read_to_string(&config).unwrap()).unwrap().drafter);
        assert_eq!(turn_off(&config), Ok(true));
        assert_eq!(turn_off(&config), Ok(false), "already so");
        assert!(!crate::vision::installed(&std::fs::read_to_string(&config).unwrap()).unwrap().drafter);
        assert!(file.is_file(), "its file is the person's disk to free, not this switch's");
        // and back on, with no second line
        assert_eq!(turn_on(&config, &mtp), Ok(true));
        assert_eq!(std::fs::read_to_string(&config).unwrap().matches("model_draft").count(), 1);
    }
}
