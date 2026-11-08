//! Changing every conversation in the app's folder to the other form, encrypted or plain, when the person turns the choice on or off.
//!
//! Each file is written whole beside itself (`<name>.converting`), on the disk, and renamed over the old one, so that a stop at any moment leaves each file either all old or all new: both
//! forms are read, so a folder half through is a folder that works, and the next run finishes what is left (it passes over what is already in the new form). A session's own file is
//! changed before its pictures when it is encrypted (they are sealed with its new id), and after them when it is decrypted (the id is gone with its header), so that a stop between them
//! never leaves a picture that nothing can open. The key file says which change is under way (`pending`) until it is done.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::conversation_key::{self, Cipher, KeyError, Keystore, Pending, Policy};
use crate::session::{self, Entry, Sealed};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Encrypt,
    Decrypt,
}

/// Why a change ended before it was done.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// A test asked it to stop here, as a crash would.
    Stopped,
    Failed(String),
}

impl std::fmt::Display for Ended {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ended::Stopped => out.write_str("stopped"),
            Ended::Failed(why) => out.write_str(why),
        }
    }
}

impl From<KeyError> for Ended {
    fn from(error: KeyError) -> Ended {
        Ended::Failed(error.to_string())
    }
}

/// What a change reports as it goes, and what a test uses to stop it.
#[derive(Default)]
pub struct Hooks<'a> {
    /// Called with how many conversations and notes files are done, and how many there are, before the first and after each.
    pub progress: Option<&'a mut dyn FnMut(usize, usize)>,
    /// A test's: the change ends as a crash would after this many of the moments at which it could (a file written whole beside its old one, and a file put in its place).
    pub stop_after: Option<usize>,
    moments: usize,
}

impl<'a> Hooks<'a> {
    /// Hooks that only tell how far the change has come.
    pub fn reporting(progress: &'a mut dyn FnMut(usize, usize)) -> Hooks<'a> {
        Hooks { progress: Some(progress), ..Hooks::default() }
    }

    fn moment(&mut self) -> Result<(), Ended> {
        if self.stop_after.is_some_and(|after| self.moments >= after) {
            return Err(Ended::Stopped);
        }
        self.moments += 1;
        Ok(())
    }

    /// How many moments there were: what a test sweeps over.
    pub fn moments(&self) -> usize {
        self.moments
    }

    fn progress(&mut self, done: usize, total: usize) {
        if let Some(progress) = self.progress.as_deref_mut() {
            progress(done, total);
        }
    }
}

/// What a change came to.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Files put in the other form.
    pub changed: usize,
    /// Files that were in it already.
    pub already: usize,
    /// What could not be carried over, in words: a line, a picture or a file that did not open and so was left out, and a file that could not be written. Nothing is left out without being here.
    pub lost: Vec<String>,
}

const TEMP: &str = "converting";

fn temp_of(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(TEMP);
    path.with_file_name(name)
}

/// A file written whole beside its old one and put in its place.
fn replace_with(path: &Path, hooks: &mut Hooks, write: impl FnOnce(&mut BufWriter<File>) -> Result<(), String>) -> Result<(), Ended> {
    let temp = temp_of(path);
    let place = |error: std::io::Error| Ended::Failed(format!("{}: {error}", path.display()));
    let done = (|| {
        let mut out = BufWriter::new(File::create(&temp).map_err(place)?);
        write(&mut out).map_err(Ended::Failed)?;
        out.flush().map_err(place)?;
        out.into_inner().map_err(|error| place(error.into_error()))?.sync_all().map_err(place)?;
        hooks.moment()?;
        fs::rename(&temp, path).map_err(place)?;
        hooks.moment()
    })();
    if matches!(done, Err(Ended::Failed(_))) {
        let _ = fs::remove_file(&temp);
    }
    done
}

/// A whole file's bytes put in its place.
fn replace_bytes(path: &Path, bytes: &[u8], hooks: &mut Hooks) -> Result<(), Ended> {
    replace_with(path, hooks, |out| out.write_all(bytes).map_err(|error| format!("{}: {error}", path.display())))
}

/// The files of a folder with this extension, by name; none where there is no folder.
fn files_in(folder: &Path, extension: &str, keep: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(folder).into_iter().flatten().flatten().map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == extension) && keep(path)).collect();
    found.sort();
    found
}

/// Every conversation and every file of notes the app keeps in its folder.
pub fn targets(data: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let sessions = files_in(&data.join("sessions"), "jsonl", |path| !crate::trajectory::is_trajectory(path));
    (sessions, files_in(&data.join("memory"), "md", |_| true))
}

/// What a stop left beside the files: a half-written file is not one of them, and is not read; it goes.
fn clear_leftovers(data: &Path) {
    let mut folders = vec![data.join("sessions"), data.join("memory")];
    folders.extend(files_in(&data.join("sessions"), "jsonl", |_| true).iter().map(|session| crate::images::folder_of(session)));
    folders.extend(files_in(&data.join("sessions"), "jsonl", |_| true).iter().map(|session| session.with_extension("checkpoints")));
    for folder in folders {
        for entry in fs::read_dir(&folder).into_iter().flatten().flatten() {
            if entry.path().extension().is_some_and(|ext| ext == TEMP) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// A plain session written again, sealed under a new id. Its pictures and its index follow.
fn encrypt_session(path: &Path, cipher: &Arc<Cipher>, hooks: &mut Hooks, report: &mut Report) -> Result<bool, Ended> {
    let existing = session::sealed_of(path).map_err(|error| Ended::Failed(format!("{}: {error}", path.display())))?;
    let (sealed, changed) = match existing {
        Some(sealed) => (sealed, false),
        None => {
            let sealed = Sealed::new(cipher).map_err(Ended::Failed)?;
            let source = BufReader::new(File::open(path).map_err(|error| Ended::Failed(format!("{}: {error}", path.display())))?);
            replace_with(path, hooks, |out| {
                let place = |error: std::io::Error| format!("{}: {error}", path.display());
                writeln!(out, "{}", sealed.header()).map_err(place)?;
                let mut number = 1;
                for line in source.split(b'\n') {
                    let bytes = line.map_err(place)?;
                    let text = String::from_utf8_lossy(&bytes);
                    let text = text.trim_end_matches('\r');
                    // (a blank line is nothing, and a line a crash cut stays as the line it is, to be skipped as it was)
                    if text.is_empty() {
                        continue;
                    }
                    writeln!(out, "{}", sealed.line(number, text)).map_err(place)?;
                    number += 1;
                }
                Ok(())
            })?;
            session::files_replaced();
            (sealed, true)
        }
    };
    // (what lies beside the session, in the form of the file that has now its id)
    for picture in files_in_any(&crate::images::folder_of(path)) {
        if crate::images::is_sealed(&picture) || fs::metadata(&picture).map_or(true, |meta| meta.len() == 0) {
            continue;
        }
        let name = picture.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        match fs::read(&picture) {
            Ok(bytes) => replace_bytes(&picture, &sealed.seal_marked("picture", &name, &bytes), hooks)?,
            Err(error) => report.lost.push(format!("{}: {error}", picture.display())),
        }
    }
    crate::checkpoint::reseal_index(path, Some(&sealed), Some(&sealed)).map_err(Ended::Failed)?;
    Ok(changed)
}

fn files_in_any(folder: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(folder).into_iter().flatten().flatten().map(|entry| entry.path())
        .filter(|path| path.is_file() && !path.extension().is_some_and(|ext| ext == TEMP || ext == "partial")).collect();
    found.sort();
    found
}

/// An encrypted session written again plain. A line that does not open is left out and said; so is a picture that does not.
fn decrypt_session(path: &Path, hooks: &mut Hooks, report: &mut Report) -> Result<bool, Ended> {
    let Some(sealed) = session::sealed_of(path).map_err(|error| Ended::Failed(format!("{}: {error}", path.display())))? else {
        return Ok(false);
    };
    // (pictures first: they open with the session's id, which is gone when its file is plain)
    for picture in files_in_any(&crate::images::folder_of(path)) {
        if !crate::images::is_sealed(&picture) {
            continue;
        }
        match crate::images::read_stored(&picture) {
            Ok(bytes) => replace_bytes(&picture, &bytes, hooks)?,
            Err(error) => report.lost.push(format!("{}: {error}", picture.display())),
        }
    }
    crate::checkpoint::reseal_index(path, Some(&sealed), None).map_err(Ended::Failed)?;
    let entries = session::entries_of(path).map_err(|error| Ended::Failed(format!("{}: {error}", path.display())))?;
    let mut damaged = Vec::new();
    replace_with(path, hooks, |out| {
        for (number, entry) in entries {
            match entry {
                Entry::Text(text) => writeln!(out, "{text}").map_err(|error| format!("{}: {error}", path.display()))?,
                Entry::Damaged => damaged.push(number as usize + 1),
            }
        }
        Ok(())
    })?;
    session::files_replaced();
    if !damaged.is_empty() {
        report.lost.push(format!("{}: {}", path.display(), session::damage_words(&damaged)));
    }
    Ok(true)
}

fn encrypt_notes(path: &Path, cipher: &Cipher, hooks: &mut Hooks, report: &mut Report) -> Result<bool, Ended> {
    let bytes = fs::read(path).map_err(|error| Ended::Failed(format!("{}: {error}", path.display())))?;
    if conversation_key::is_sealed_text(&bytes) {
        return Ok(false);
    }
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    match String::from_utf8(bytes) {
        Ok(text) => replace_bytes(path, conversation_key::seal_text(cipher, &name, &text).as_bytes(), hooks)?,
        Err(error) => {
            report.lost.push(format!("{}: {error}: it is not text, and is left as it is", path.display()));
            return Ok(false);
        }
    }
    Ok(true)
}

fn decrypt_notes(path: &Path, hooks: &mut Hooks, report: &mut Report) -> Result<bool, Ended> {
    let bytes = fs::read(path).map_err(|error| Ended::Failed(format!("{}: {error}", path.display())))?;
    if !conversation_key::is_sealed_text(&bytes) {
        return Ok(false);
    }
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    match conversation_key::open_text(&name, &bytes) {
        Ok(text) => replace_bytes(path, text.as_bytes(), hooks)?,
        Err(error) => {
            report.lost.push(format!("{}: {error}", path.display()));
            return Ok(false);
        }
    }
    Ok(true)
}

/// Every conversation and every file of notes in the data folder, put in the form `direction` says. It passes over what is in it already, so a run that was stopped is finished by another.
/// Nothing may be writing to the sessions meanwhile: the window sees to that.
pub fn convert(data: &Path, cipher: &Arc<Cipher>, direction: Direction, hooks: &mut Hooks) -> Result<Report, Ended> {
    clear_leftovers(data);
    let (sessions, notes) = targets(data);
    let (total, mut done, mut report) = (sessions.len() + notes.len(), 0, Report::default());
    hooks.progress(0, total);
    for path in &sessions {
        let changed = match direction {
            Direction::Encrypt => encrypt_session(path, cipher, hooks, &mut report)?,
            Direction::Decrypt => decrypt_session(path, hooks, &mut report)?,
        };
        report.changed += usize::from(changed);
        report.already += usize::from(!changed);
        done += 1;
        hooks.progress(done, total);
    }
    for path in &notes {
        let changed = match direction {
            Direction::Encrypt => encrypt_notes(path, cipher, hooks, &mut report)?,
            Direction::Decrypt => decrypt_notes(path, hooks, &mut report)?,
        };
        report.changed += usize::from(changed);
        report.already += usize::from(!changed);
        done += 1;
        hooks.progress(done, total);
    }
    Ok(report)
}

/// The first half of turning the choice on: a key for the conversations is made and kept (so that nothing that follows can lose it), with the change it begins marked as not done, and new
/// conversations are sealed from here on. The recovery code is returned with it, to be shown once.
pub fn begin_on(data: &Path, policy: &Policy, passphrase: &str, remember: bool) -> Result<(Keystore, Arc<Cipher>, String), Ended> {
    let (keystore, cipher, recovery) = Keystore::create(data, passphrase, remember, Some(Pending::Encrypt))?;
    policy.set_on(cipher.clone());
    Ok((keystore, cipher, recovery))
}

/// The second half: every conversation and note that is kept is sealed, and then the key file says nothing is left to do.
pub fn finish_on(data: &Path, keystore: &mut Keystore, cipher: &Arc<Cipher>, hooks: &mut Hooks) -> Result<Report, Ended> {
    let report = convert(data, cipher, Direction::Encrypt, hooks)?;
    keystore.set_pending(None)?;
    Ok(report)
}

/// Turning the choice on, whole.
pub fn turn_on(data: &Path, policy: &Policy, passphrase: &str, remember: bool, hooks: &mut Hooks) -> Result<(Arc<Cipher>, String, Report), Ended> {
    let (mut keystore, cipher, recovery) = begin_on(data, policy, passphrase, remember)?;
    let report = finish_on(data, &mut keystore, &cipher, hooks)?;
    Ok((cipher, recovery, report))
}

/// Turning the choice off: new conversations are plain from here on, every conversation and note is put back, and then the key file goes. A stop leaves the key file, saying what is
/// left to do.
pub fn turn_off(data: &Path, policy: &Policy, cipher: &Arc<Cipher>, hooks: &mut Hooks) -> Result<Report, Ended> {
    let mut keystore = Keystore::load(data)?;
    keystore.set_pending(Some(Pending::Decrypt))?;
    policy.set_off();
    let report = convert(data, cipher, Direction::Decrypt, hooks)?;
    Keystore::remove(data)?;
    conversation_key::forget(cipher.fingerprint());
    Ok(report)
}

/// What a start does once the key is unlocked, if a change was stopped before it was done: it finishes it. None when none was under way.
pub fn finish_pending(data: &Path, policy: &Policy, cipher: &Arc<Cipher>, hooks: &mut Hooks) -> Result<Option<(Pending, Report)>, Ended> {
    let mut keystore = Keystore::load(data)?;
    match keystore.pending() {
        Some(Pending::Encrypt) => {
            policy.set_on(cipher.clone());
            let report = convert(data, cipher, Direction::Encrypt, hooks)?;
            keystore.set_pending(None)?;
            Ok(Some((Pending::Encrypt, report)))
        }
        Some(Pending::Decrypt) => Ok(Some((Pending::Decrypt, turn_off(data, policy, cipher, hooks)?))),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests;
