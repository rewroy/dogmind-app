//! Sessions encrypted at rest: what is in the file, what comes back, and what is damage.

use super::*;
use crate::message::ImageRef;

fn cipher() -> Arc<Cipher> {
    Cipher::generate().unwrap()
}

fn file_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path).unwrap().lines().map(str::to_string).collect()
}

fn put_lines(path: &Path, lines: &[String]) {
    fs::write(path, format!("{}\n", lines.join("\n"))).unwrap();
}

/// A line with one byte of its ciphertext changed, as a flipped bit on a disk would.
fn flipped(line: &str) -> String {
    let mut blob = b64d(line).unwrap();
    let middle = blob.len() / 2;
    blob[middle] ^= 0x10;
    b64e(&blob)
}

/// A session of a few messages under a key of its own, and the key.
fn sealed_session(dir: &Path, stamp: &str) -> (Session, Arc<Cipher>) {
    let key = cipher();
    let mut session = Session::create_with(dir, Path::new("C:/work"), "now", stamp, Some(&key)).unwrap();
    session.set_tools(&serde_json::json!([{"function": {"name": "read_file"}}])).unwrap();
    session.append(&Message::text("system", "the system prompt")).unwrap();
    session.append(&Message::text("user", crate::prompt::user_message("ハッシュテーブルの話をして", "now"))).unwrap();
    session.append(&Message::text("assistant", "A HashMap keeps Cherry in a bucket")).unwrap();
    (session, key)
}

#[test]
fn an_encrypted_session_begins_with_a_header_that_holds_nothing_and_every_line_after_it_is_one_sealed_message() {
    let dir = tempfile::tempdir().unwrap();
    let (mut session, key) = sealed_session(dir.path(), "1");
    session.mark_web_seen().unwrap();
    session.set_goal(Some("ship the page")).unwrap();
    let path = session.path.clone();
    rename(&path, "a name given later").unwrap();
    let lines = file_lines(&path);
    let header: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(header["kind"], "encrypted");
    assert_eq!(header["key"], key.fingerprint());
    assert_eq!(header["id"].as_str().unwrap().len(), 32);
    assert_eq!(header.as_object().unwrap().len(), 4, "a kind, a version, an id and a key: nothing of the conversation: {}", lines[0]);
    assert_eq!(lines.len(), 1 + 1 + 1 + 3 + 2 + 1, "the header, the first line (where and when), tools, three messages, the mark and the goal, the name");
    for line in &lines[1..] {
        let blob = b64d(line).expect("one base64 line");
        assert!(blob.len() >= 12 + 16 + 2, "a nonce, a tag and something between: {line}");
        assert!(!line.contains('{'), "no JSON in the clear: {line}");
    }
    let bytes = fs::read(&path).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    for word in ["HashMap", "Cherry", "ハッシュ", "C:/work", "ship the page", "a name given later", "read_file", "system prompt", "workspace"] {
        assert!(!text.contains(word), "{word} is in the file");
    }
    // everything reads back: by `read`, `open`, `list`, `search` and the readers of a single thing
    assert_eq!(read(&path).unwrap().len(), 3);
    let (_, workspace, messages) = Session::open(&path).unwrap();
    assert_eq!((workspace.as_path(), messages.len()), (Path::new("C:/work"), 3));
    assert!(web_seen(&path));
    assert_eq!(goal_of(&path).as_deref(), Some("ship the page"));
    assert!(tools_of(&path).is_some());
    let listed = list(dir.path());
    assert_eq!((listed[0].title.as_str(), listed[0].messages, listed[0].workspace.as_path(), listed[0].created.as_str()), ("a name given later", 3, Path::new("C:/work"), "now"));
    assert_eq!(search(dir.path(), "cherry bucket").len(), 1);
    assert_eq!(search(dir.path(), "name given").len(), 1, "the name");
    assert!(search(dir.path(), "system").is_empty());
}

#[test]
fn the_same_message_is_a_different_line_each_time_and_two_files_have_two_ids() {
    let dir = tempfile::tempdir().unwrap();
    let key = cipher();
    let mut one = Session::create_with(dir.path(), Path::new("w"), "now", "1", Some(&key)).unwrap();
    let mut two = Session::create_with(dir.path(), Path::new("w"), "now", "2", Some(&key)).unwrap();
    for session in [&mut one, &mut two] {
        session.append(&Message::text("user", "same words")).unwrap();
        session.append(&Message::text("user", "same words")).unwrap();
    }
    let (a, b) = (file_lines(&one.path), file_lines(&two.path));
    assert_ne!(a[0], b[0], "an id of its own");
    assert_ne!(a[2], a[3], "a nonce each time");
    assert_ne!(a[2], b[2]);
}

#[test]
fn a_flipped_byte_is_damage_that_is_told_and_what_is_left_still_opens() {
    let dir = tempfile::tempdir().unwrap();
    let (session, _) = sealed_session(dir.path(), "1");
    let path = session.path.clone();
    drop(session);
    let mut lines = file_lines(&path);
    // the third message (the assistant's), which is the 7th line of the file counting from 1
    lines[5] = flipped(&lines[5]);
    put_lines(&path, &lines);
    let opened = Session::open_checked(&path).unwrap();
    assert_eq!(opened.damaged, [6]);
    assert_eq!(opened.messages.len(), 2, "the two that did open");
    assert_eq!(read_checked(&path).unwrap().1, [6]);
    assert!(damage_words(&opened.damaged).ends_with("lines 6"), "{}", damage_words(&opened.damaged));
    // a branch would leave it out without a word: refused, in the same words
    let error = fork(&path, 0, None).unwrap_err();
    assert!(error.contains(DAMAGE_NOTICE), "{error}");
    // the list counts what opens and the session is there
    assert_eq!(list(dir.path())[0].messages, 2);
    // a header that was changed is not a header: nothing opens, and it is said
    let mut changed = file_lines(&path);
    changed[0] = changed[0].replace("\"id\":\"", "\"id\":\"00");
    put_lines(&path, &changed);
    assert!(read(&path).is_err());
}

#[test]
fn a_line_moved_to_another_file_or_to_another_place_is_damage() {
    let dir = tempfile::tempdir().unwrap();
    let key = cipher();
    let make = |stamp: &str, words: &[&str]| {
        let mut session = Session::create_with(dir.path(), Path::new("w"), "now", stamp, Some(&key)).unwrap();
        for word in words {
            session.append(&Message::text("user", *word)).unwrap();
        }
        session.path.clone()
    };
    let (first, second) = (make("1", &["one", "two", "three"]), make("2", &["four", "five", "six"]));
    // a line of the first put in the same place of the second: another file, though the same key and the same number
    let mut moved = file_lines(&second);
    moved[2] = file_lines(&first)[2].clone();
    put_lines(&second, &moved);
    let opened = Session::open_checked(&second).unwrap();
    assert_eq!((opened.damaged.clone(), opened.messages.len()), (vec![3], 2));
    // two lines of a file exchanged: each is at a place it was not sealed for
    let mut swapped = file_lines(&first);
    swapped.swap(2, 3);
    put_lines(&first, &swapped);
    assert_eq!(read_checked(&first).unwrap().1, [3, 4]);
    assert_eq!(read(&first).unwrap().len(), 1);
    // a line copied to a later place (a replay)
    let third = make("3", &["a", "b", "c"]);
    let mut replayed = file_lines(&third);
    replayed.push(replayed[2].clone());
    put_lines(&third, &replayed);
    assert_eq!(read_checked(&third).unwrap().1, [6]);
    // a plain line put among them is not taken for one of them
    let fourth = make("4", &["x", "y"]);
    let mut injected = file_lines(&fourth);
    injected.insert(2, serde_json::to_string(&Line::Message { message: Message::text("user", "forged") }).unwrap());
    put_lines(&fourth, &injected);
    let (messages, damaged) = read_checked(&fourth).unwrap();
    assert!(damaged.contains(&3) && messages.iter().all(|message| message.content.as_deref() != Some("forged")), "{damaged:?}");
    // and a blank line is a line that does not open
    let mut blank = file_lines(&fourth);
    blank.insert(1, String::new());
    put_lines(&fourth, &blank);
    assert!(read_checked(&fourth).unwrap().1.contains(&2));
}

#[test]
fn a_plain_session_opens_and_goes_on_plain_whatever_is_chosen() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = Session::create(dir.path(), Path::new("w"), "now", "1").unwrap();
    old.append(&Message::text("user", "plain words")).unwrap();
    let path = old.path.clone();
    drop(old);
    let key = cipher();
    let policy = Policy::on(key.clone());
    let (mut again, _, messages) = Session::open(&path).unwrap();
    assert_eq!(messages.len(), 1);
    again.append(&Message::text("assistant", "still plain")).unwrap();
    rename(&path, "plain name").unwrap();
    assert!(fs::read_to_string(&path).unwrap().contains("still plain"));
    assert!(!is_encrypted(&path));
    assert_eq!(search(dir.path(), "plain name").len(), 1);
    // a new one, under the same policy, is encrypted; both are in the list
    let mut new = Session::create_under(&policy, dir.path(), Path::new("w"), "now", "2").unwrap();
    new.append(&Message::text("user", "sealed words")).unwrap();
    assert!(is_encrypted(&new.path));
    assert_eq!(list(dir.path()).len(), 2);
    assert_eq!(search(dir.path(), "words").len(), 2);
}

#[test]
fn a_session_goes_on_after_a_cut_line_and_the_cut_is_told() {
    let dir = tempfile::tempdir().unwrap();
    let (mut session, _) = sealed_session(dir.path(), "1");
    session.append(&Message::text("user", "the line the crash cuts")).unwrap();
    let path = session.path.clone();
    drop(session);
    // the last line cut in two, with nothing after it
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, &bytes[..bytes.len() - 20]).unwrap();
    let mut opened = Session::open_checked(&path).unwrap();
    assert_eq!(opened.damaged, [7], "the cut line is told as damage");
    assert_eq!(opened.messages.len(), 3);
    opened.session.append(&Message::text("assistant", "after the cut")).unwrap();
    rename(&path, "named after the cut").unwrap();
    let (messages, damaged) = read_checked(&path).unwrap();
    assert_eq!(damaged, [7], "and only that line: what is written after it is sealed under its own place");
    assert_eq!(messages.last().and_then(|message| message.content.as_deref()), Some("after the cut"));
    assert_eq!(list(dir.path())[0].title, "named after the cut");
}

#[test]
fn a_name_given_by_the_window_while_the_agent_writes_is_not_a_line_out_of_place() {
    let dir = tempfile::tempdir().unwrap();
    let (mut session, _) = sealed_session(dir.path(), "1");
    let path = session.path.clone();
    for round in 0..5 {
        rename(&path, &format!("name {round}")).unwrap();
        session.append(&Message::text("user", format!("after name {round}"))).unwrap();
    }
    let (messages, damaged) = read_checked(&path).unwrap();
    assert_eq!((messages.len(), damaged.len()), (3 + 5, 0), "{damaged:?}");
    assert_eq!(list(dir.path())[0].title, "name 4");
    // a session opened again in this program counts from the file
    let (mut again, _, _) = Session::open(&path).unwrap();
    again.append(&Message::text("user", "more")).unwrap();
    assert_eq!(read_checked(&path).unwrap().1.len(), 0);
}

#[test]
fn a_session_whose_key_is_not_here_is_listed_as_such_and_never_opened_or_written() {
    let dir = tempfile::tempdir().unwrap();
    let (session, key) = sealed_session(dir.path(), "1");
    let path = session.path.clone();
    drop(session);
    crate::conversation_key::forget(key.fingerprint());
    assert!(read(&path).unwrap_err().contains("encrypted"), "{}", read(&path).unwrap_err());
    assert!(is_unreadable(&entries_of(&path).err().unwrap()));
    assert!(Session::open(&path).is_err() && rename(&path, "x").is_err() && fork(&path, 0, None).is_err());
    let listed = list(dir.path());
    assert_eq!((listed.len(), listed[0].messages), (1, 0));
    assert!(listed[0].unreadable.is_some() && listed[0].title.contains("encrypted"), "{:?}", listed[0]);
    assert!(search(dir.path(), "hashmap").is_empty());
    assert!(is_encrypted(&path));
    // the policy that waits for its key writes nothing
    assert!(Session::create_under(&Policy::locked(), dir.path(), Path::new("w"), "now", "2").is_err());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn a_branch_of_an_encrypted_session_is_encrypted_with_an_id_of_its_own_and_its_pictures_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let key = cipher();
    let mut session = Session::create_with(dir.path(), Path::new("C:/w"), "now", "1", Some(&key)).unwrap();
    session.set_tools(&serde_json::json!([{"function": {"name": "read_file"}}])).unwrap();
    session.append(&Message::text("system", "s")).unwrap();
    let mut asked = Message::text("user", "look at this");
    asked.images.push(ImageRef { file: "a.png".into(), media_type: "image/png".into(), width: 1, height: 1 });
    session.append(&asked).unwrap();
    session.append(&Message::text("assistant", "first answer")).unwrap();
    session.append(&Message::text("user", "then this")).unwrap();
    let path = session.path.clone();
    crate::images::store(&path, "a.png", b"pretend this is a png").unwrap();
    let stored = fs::read(crate::images::folder_of(&path).join("a.png")).unwrap();
    assert!(crate::images::is_sealed(&crate::images::folder_of(&path).join("a.png")) && !String::from_utf8_lossy(&stored).contains("pretend"));
    let before = fs::read(&path).unwrap();
    let branch = fork(&path, 2, Some("a title (branch)")).unwrap();
    assert_eq!(fs::read(&path).unwrap(), before, "the original is only read");
    assert!(is_encrypted(&branch));
    assert_ne!(file_lines(&path)[0], file_lines(&branch)[0], "an id of its own");
    let (_, workspace, messages) = Session::open(&branch).unwrap();
    assert_eq!((workspace.as_path(), messages.len()), (Path::new("C:/w"), 3));
    assert!(tools_of(&branch).is_some());
    assert_eq!(list(dir.path()).iter().filter(|summary| summary.title == "a title (branch)").count(), 1);
    // the picture was read from the original and sealed again for the branch, under its id
    let there = crate::images::folder_of(&branch).join("a.png");
    assert_eq!(crate::images::read_stored(&there).unwrap(), b"pretend this is a png");
    assert_ne!(fs::read(&there).unwrap(), stored, "sealed again, not copied");
    assert!(fork(&path, 9, None).is_err());
}

#[test]
fn a_picture_opens_only_as_its_own_sessions_and_only_under_its_own_name() {
    let dir = tempfile::tempdir().unwrap();
    let key = cipher();
    let make = |stamp: &str| Session::create_with(dir.path(), Path::new("w"), "now", stamp, Some(&key)).unwrap().path.clone();
    let (one, two) = (make("1"), make("2"));
    crate::images::store(&one, "a.png", b"the first picture").unwrap();
    let kept = crate::images::folder_of(&one).join("a.png");
    assert_eq!(crate::images::read_stored(&kept).unwrap(), b"the first picture");
    // stored twice is one file, as for a plain session
    let first_bytes = fs::read(&kept).unwrap();
    crate::images::store(&one, "a.png", b"another").unwrap();
    assert_eq!(fs::read(&kept).unwrap(), first_bytes);
    // put in the other session's folder, or under another name in its own folder: it does not open
    fs::create_dir_all(crate::images::folder_of(&two)).unwrap();
    fs::copy(&kept, crate::images::folder_of(&two).join("a.png")).unwrap();
    assert!(crate::images::read_stored(&crate::images::folder_of(&two).join("a.png")).is_err());
    fs::copy(&kept, crate::images::folder_of(&one).join("b.png")).unwrap();
    assert!(crate::images::read_stored(&crate::images::folder_of(&one).join("b.png")).is_err());
    // one byte changed
    let mut changed = first_bytes.clone();
    let last = changed.len() - 1;
    changed[last] ^= 1;
    fs::write(&kept, changed).unwrap();
    assert!(crate::images::read_stored(&kept).is_err());
    // a plain picture in a session of either kind is read as it is
    fs::write(crate::images::folder_of(&one).join("plain.png"), b"plain bytes").unwrap();
    assert_eq!(crate::images::read_stored(&crate::images::folder_of(&one).join("plain.png")).unwrap(), b"plain bytes");
    // and a request that names the damaged one says it could not be read, not that it is gone
    let mut message = Message::text("user", "see");
    message.images.push(ImageRef { file: "a.png".into(), media_type: "image/png".into(), width: 1, height: 1 });
    message.images.push(ImageRef { file: "gone.png".into(), media_type: "image/png".into(), width: 1, height: 1 });
    let (pictures, text) = crate::images::loaded(&message, Some(&crate::images::folder_of(&one)));
    assert!(pictures.is_empty() && text.contains("a.png, could not be read") && text.contains("gone.png, is no longer"), "{text}");
}

#[test]
fn a_plain_session_keeps_its_pictures_as_they_were() {
    let dir = tempfile::tempdir().unwrap();
    let session = Session::create(dir.path(), Path::new("w"), "now", "1").unwrap().path.clone();
    crate::images::store(&session, "a.png", b"plain picture").unwrap();
    assert_eq!(fs::read(crate::images::folder_of(&session).join("a.png")).unwrap(), b"plain picture");
    assert!(!crate::images::is_sealed(&crate::images::folder_of(&session).join("a.png")));
}
