//! Turning the choice on and off over a folder of conversations, and a stop at every moment of it.

use super::*;
use crate::message::{ImageRef, Message};
use crate::session::Session;

const PICTURE: &[u8] = b"pretend this is a picture of a dog";

/// A data folder as the app leaves one: sessions (one with a picture and checkpoints, one cut by a crash, one that only has a trajectory beside it), notes, and what is not conversation.
fn folder() -> tempfile::TempDir {
    let data = tempfile::tempdir().unwrap();
    let sessions = data.path().join("sessions");
    let mut first = Session::create(&sessions, Path::new("C:/work"), "now", "20261101-000001").unwrap();
    first.set_tools(&serde_json::json!([{"function": {"name": "read_file"}}])).unwrap();
    first.append(&Message::text("system", "the system prompt")).unwrap();
    let mut asked = Message::text("user", "alpha one: look at the dog");
    asked.images.push(ImageRef { file: "dog.png".into(), media_type: "image/png".into(), width: 1, height: 1 });
    first.append(&asked).unwrap();
    first.append(&Message::text("assistant", "alpha two: a good dog")).unwrap();
    session::rename(&first.path, "the dog session").unwrap();
    crate::images::store(&first.path, "dog.png", PICTURE).unwrap();
    crate::checkpoint::Checkpoints::open(&first.path).begin("alpha one: look at the dog", "now", Some(1)).unwrap();
    let mut second = Session::create(&sessions, Path::new("C:/work"), "now", "20261101-000002").unwrap();
    second.append(&Message::text("user", "beta one")).unwrap();
    second.append(&Message::text("assistant", "beta two")).unwrap();
    let mut bytes = fs::read(&second.path).unwrap();
    bytes.extend_from_slice(b"{\"kind\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"\xE3\x81");
    fs::write(&second.path, bytes).unwrap();
    fs::write(sessions.join("20261101-000001.trajectory.jsonl"), "{\"ev\":\"turn_start\"}\n").unwrap();
    fs::create_dir_all(data.path().join("memory")).unwrap();
    fs::write(data.path().join("memory").join("user.md"), "- the user likes tea\n").unwrap();
    fs::write(data.path().join("memory").join("project-0123456789abcdef.md"), "- builds with make\n").unwrap();
    fs::write(data.path().join("memory").join("forgotten.md"), "\n## 2026-11-01 (you)\n- an old note\n").unwrap();
    fs::write(data.path().join("settings.json"), "{}").unwrap();
    data
}

/// What a person would see of the folder: each conversation's messages and name, the picture, the checkpoint's title, and the notes.
fn seen(data: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let (sessions, notes) = targets(data);
    for path in sessions {
        let messages = session::read(&path).unwrap_or_else(|error| panic!("{error}"));
        out.push(format!("{}: {:?} / {:?}", path.file_name().unwrap().to_string_lossy(), messages.iter().map(|message| message.content.clone().unwrap_or_default()).collect::<Vec<_>>(), session::list(path.parent().unwrap()).iter().find(|summary| summary.path == path).map(|summary| summary.title.clone())));
        let picture = crate::images::folder_of(&path).join("dog.png");
        if picture.exists() {
            out.push(format!("picture {:?}", crate::images::read_stored(&picture).unwrap_or_else(|error| panic!("{}: {error}", picture.display()))));
        }
        let turns: Vec<String> = crate::checkpoint::Checkpoints::open(&path).turns().iter().map(|turn| turn.title.clone()).collect();
        if path.with_extension("checkpoints").exists() {
            out.push(format!("turns {turns:?}"));
        }
        out.push(format!("tools {:?}", session::tools_of(&path).map(|tools| tools.to_string())));
    }
    for path in notes {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        out.push(format!("{name}: {}", conversation_key::open_text(&name, &fs::read(&path).unwrap()).unwrap_or_else(|error| panic!("{name}: {error}"))));
    }
    out
}

fn raw(data: &Path, relative: &str) -> Vec<u8> {
    fs::read(data.join(relative)).unwrap()
}

fn all_sealed(data: &Path) -> bool {
    let (sessions, notes) = targets(data);
    sessions.iter().all(|path| session::is_encrypted(path)) && notes.iter().all(|path| conversation_key::is_sealed_text(&fs::read(path).unwrap()))
}

fn none_sealed(data: &Path) -> bool {
    let (sessions, notes) = targets(data);
    sessions.iter().all(|path| !session::is_encrypted(path)) && notes.iter().all(|path| !conversation_key::is_sealed_text(&fs::read(path).unwrap()))
}

#[test]
fn turning_it_on_seals_every_conversation_its_pictures_its_index_and_its_notes_and_turning_it_off_puts_them_back() {
    let data = folder();
    let before = seen(data.path());
    let (trajectory, settings) = (raw(data.path(), "sessions/20261101-000001.trajectory.jsonl"), raw(data.path(), "settings.json"));
    let policy = Policy::off();
    let mut steps = Vec::new();
    let mut progress = |done: usize, total: usize| steps.push((done, total));
    let (cipher, recovery, report) = turn_on(data.path(), &policy, "a passphrase of the test", false, &mut Hooks { progress: Some(&mut progress), ..Hooks::default() }).unwrap();
    assert_eq!(steps, [(0, 5), (1, 5), (2, 5), (3, 5), (4, 5), (5, 5)], "progress, before and after each");
    assert_eq!((report.changed, report.already, report.lost.len()), (5, 0, 0), "{report:?}");
    assert!(all_sealed(data.path()) && !recovery.is_empty());
    assert_eq!(seen(data.path()), before, "what a person sees is the same");
    assert!(policy.writer().unwrap().is_some());
    // what holds no words of the conversation is as it was; the words are in none of what is sealed
    assert_eq!(raw(data.path(), "sessions/20261101-000001.trajectory.jsonl"), trajectory);
    assert_eq!(raw(data.path(), "settings.json"), settings);
    let mut every_byte = Vec::new();
    for entry in walk(data.path()) {
        if !entry.to_string_lossy().contains("trajectory") && !entry.ends_with("settings.json") && !entry.ends_with("conversation-key.json") {
            every_byte.extend(fs::read(&entry).unwrap());
        }
    }
    let text = String::from_utf8_lossy(&every_byte);
    for word in ["alpha one", "beta one", "the dog session", "the user likes tea", "builds with make", "an old note", "look at the dog", "pretend this", "C:/work", "read_file"] {
        assert!(!text.contains(word), "{word} is in the clear");
    }
    assert_eq!(conversation_key::Keystore::load(data.path()).unwrap().pending(), None, "nothing is left to do");
    assert!(crate::conversation_key::exists(data.path()));
    // a second run passes over what is done
    let again = convert(data.path(), &cipher, Direction::Encrypt, &mut Hooks::default()).unwrap();
    assert_eq!((again.changed, again.already), (0, 5));
    // and off: every file is as it was, and the key file goes
    let report = turn_off(data.path(), &policy, &cipher, &mut Hooks::default()).unwrap();
    assert_eq!((report.changed, report.lost.len()), (5, 0), "{report:?}");
    assert!(none_sealed(data.path()));
    assert_eq!(seen(data.path()), before);
    assert!(!crate::conversation_key::exists(data.path()) && policy.writer().unwrap().is_none());
    assert!(!session::is_encrypted(&data.path().join("sessions/20261101-000001.jsonl")));
    // the plain files are what the plain writer wrote
    assert!(fs::read_to_string(data.path().join("sessions/20261101-000001.jsonl")).unwrap().contains("alpha two"));
    assert!(walk(data.path()).iter().all(|path| path.extension().is_none_or(|ext| ext != "converting")));
}

fn walk(folder: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(folder).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// A stop at any moment of turning it on, as a crash would be, leaves every file either old or new and every conversation readable; the next start finishes it.
#[test]
fn a_stop_at_every_moment_of_turning_it_on_leaves_every_file_whole_and_the_next_run_finishes() {
    let reference = folder();
    let before = seen(reference.path());
    let mut counting = Hooks::default();
    turn_on(reference.path(), &Policy::off(), "p", false, &mut counting).unwrap();
    let moments = counting.moments();
    assert!(moments >= 2 * 5, "{moments}");
    for stop in 0..moments {
        let data = folder();
        let policy = Policy::off();
        let outcome = turn_on(data.path(), &policy, "a passphrase of the test", false, &mut Hooks { stop_after: Some(stop), ..Hooks::default() });
        assert_eq!(outcome.err(), Some(Ended::Stopped), "stop {stop}");
        // every file is whole: each reads as it did, whichever form it is in
        assert_eq!(seen(data.path()), before, "stop {stop}");
        let keystore = conversation_key::Keystore::load(data.path()).unwrap();
        assert_eq!(keystore.pending(), Some(Pending::Encrypt), "stop {stop}: the key file says what is left");
        // the next start unlocks and finishes
        let cipher = keystore.unlock("a passphrase of the test").unwrap();
        let (kind, report) = finish_pending(data.path(), &policy, &cipher, &mut Hooks::default()).unwrap().expect("a change was under way");
        assert_eq!(kind, Pending::Encrypt);
        assert!(report.lost.is_empty(), "stop {stop}: {report:?}");
        assert!(all_sealed(data.path()), "stop {stop}");
        assert_eq!(seen(data.path()), before, "stop {stop}, finished");
        assert_eq!(conversation_key::Keystore::load(data.path()).unwrap().pending(), None);
        assert!(finish_pending(data.path(), &policy, &cipher, &mut Hooks::default()).unwrap().is_none());
        assert!(walk(data.path()).iter().all(|path| path.extension().is_none_or(|ext| ext != "converting")), "stop {stop}: nothing half-written is left after the next run");
        // the pictures are sealed under the new id, whichever moment it was
        let picture = crate::images::folder_of(&data.path().join("sessions/20261101-000001.jsonl")).join("dog.png");
        assert!(crate::images::is_sealed(&picture), "stop {stop}");
    }
}

#[test]
fn a_stop_at_every_moment_of_turning_it_off_leaves_every_file_whole_and_the_next_run_finishes() {
    let reference = folder();
    let before = seen(reference.path());
    let policy = Policy::off();
    let (cipher, _, _) = turn_on(reference.path(), &policy, "p", false, &mut Hooks::default()).unwrap();
    let mut counting = Hooks::default();
    turn_off(reference.path(), &policy, &cipher, &mut counting).unwrap();
    let moments = counting.moments();
    for stop in 0..moments {
        let data = folder();
        let policy = Policy::off();
        let (cipher, _, _) = turn_on(data.path(), &policy, "a passphrase of the test", false, &mut Hooks::default()).unwrap();
        let outcome = turn_off(data.path(), &policy, &cipher, &mut Hooks { stop_after: Some(stop), ..Hooks::default() });
        assert_eq!(outcome.err(), Some(Ended::Stopped), "stop {stop}");
        assert_eq!(seen(data.path()), before, "stop {stop}");
        // the key file is still there, saying what is left; a new session in the meantime is plain
        let keystore = conversation_key::Keystore::load(data.path()).unwrap();
        assert_eq!(keystore.pending(), Some(Pending::Decrypt), "stop {stop}");
        assert!(policy.writer().unwrap().is_none());
        // a start that unlocks finishes it, and the key file goes
        let policy = Policy::locked();
        let cipher = keystore.unlock("a passphrase of the test").unwrap();
        let (kind, report) = finish_pending(data.path(), &policy, &cipher, &mut Hooks::default()).unwrap().unwrap();
        assert_eq!((kind, report.lost.len()), (Pending::Decrypt, 0), "stop {stop}: {report:?}");
        assert!(none_sealed(data.path()), "stop {stop}");
        assert_eq!(seen(data.path()), before, "stop {stop}, finished");
        assert!(!crate::conversation_key::exists(data.path()) && policy.writer().unwrap().is_none());
        assert!(walk(data.path()).iter().all(|path| path.extension().is_none_or(|ext| ext != "converting" && ext != "partial")), "stop {stop}");
        let picture = crate::images::folder_of(&data.path().join("sessions/20261101-000001.jsonl")).join("dog.png");
        assert!(!crate::images::is_sealed(&picture), "stop {stop}");
    }
}

/// An agent that holds a session open while every session is changed finds the new file by its name and goes on in it, in the form it now has.
#[test]
fn a_session_held_open_goes_on_in_the_new_file() {
    let data = folder();
    let path = data.path().join("sessions/20261101-000001.jsonl");
    let (mut session, _, _) = Session::open(&path).unwrap();
    let policy = Policy::off();
    let (cipher, _, _) = turn_on(data.path(), &policy, "p", false, &mut Hooks::default()).unwrap();
    session.append(&Message::text("user", "after the change on")).unwrap();
    let (messages, damaged) = session::read_checked(&path).unwrap();
    assert!(damaged.is_empty() && messages.last().unwrap().content.as_deref() == Some("after the change on"), "{damaged:?}");
    assert!(session::is_encrypted(&path));
    turn_off(data.path(), &policy, &cipher, &mut Hooks::default()).unwrap();
    session.append(&Message::text("user", "after the change off")).unwrap();
    assert!(fs::read_to_string(&path).unwrap().contains("after the change off"));
    assert_eq!(session::read(&path).unwrap().len(), 5);
}

/// What does not open is not carried over without a word: a line that is damaged, or a picture, is in the report and is left out; the rest is carried over.
#[test]
fn what_does_not_open_is_left_out_with_a_word_when_turning_it_off() {
    let data = folder();
    let policy = Policy::off();
    let (cipher, _, _) = turn_on(data.path(), &policy, "p", false, &mut Hooks::default()).unwrap();
    let path = data.path().join("sessions/20261101-000001.jsonl");
    let mut lines: Vec<String> = fs::read_to_string(&path).unwrap().lines().map(str::to_string).collect();
    let last = lines.len() - 2;
    lines[last] = lines[last].replace(&lines[last][10..14], "AAAA");
    fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
    let picture = crate::images::folder_of(&path).join("dog.png");
    let mut bytes = fs::read(&picture).unwrap();
    let end = bytes.len() - 1;
    bytes[end] ^= 1;
    fs::write(&picture, bytes).unwrap();
    let report = turn_off(data.path(), &policy, &cipher, &mut Hooks::default()).unwrap();
    assert_eq!(report.lost.len(), 2, "{report:?}");
    assert!(report.lost.iter().any(|line| line.contains(session::DAMAGE_NOTICE)) && report.lost.iter().any(|line| line.contains("dog.png")), "{report:?}");
    assert!(!session::is_encrypted(&path) && !crate::conversation_key::exists(data.path()));
}
