//! Conversations encrypted at rest, as the agent keeps them: nothing is opened or written until the key is here, a new session is sealed once it is, and what it notes and what is
//! damaged in it are told.

use super::*;
use crate::conversation_key::{Cipher, Policy, LOCKED};

fn run_under(workspace: &Path, client: Client, policy: Arc<Policy>, memory: Option<PathBuf>) -> Run {
    let (sender, events) = channel();
    let sessions = tempfile::tempdir().unwrap();
    let handle = start(Config { workspace: workspace.to_path_buf(), sessions: sessions.path().to_path_buf(), rules: sessions.path().join("permissions.json"), mode: Mode::Auto, effort: None, web: WebSettings::default(), language: "auto".into(), japanese_system: false, instructions: None, home: None, skills_off: Vec::new(), memory, mcp: None, policy }, sender, Box::new(|| {}));
    handle.commands.send(Command::Provider(Arc::new(client), "model".into(), 100_000)).unwrap();
    Run { handle, events, _sessions: sessions }
}

fn session_file(run: &Run) -> PathBuf {
    loop {
        if let Event::SessionFile(path) = run.events.recv_timeout(Duration::from_secs(20)).unwrap() {
            return path;
        }
    }
}

#[test]
fn while_the_key_is_not_unlocked_no_session_is_opened_or_written() {
    let dir = tempfile::tempdir().unwrap();
    let plain = tempfile::tempdir().unwrap();
    let old = {
        let mut session = crate::session::Session::create(plain.path(), dir.path(), "now", "1").unwrap();
        session.append(&Message::text("user", "an old plain conversation")).unwrap();
        session.path.clone()
    };
    let (client, _) = server(vec![text_answer("never asked"), text_answer("not this either")]);
    let policy = Policy::locked();
    let run = run_under(dir.path(), client, policy.clone(), None);
    run.handle.commands.send(Command::Send("hello".into())).unwrap();
    let mut said = String::new();
    loop {
        match run.events.recv_timeout(Duration::from_secs(20)).unwrap() {
            Event::Failed(message) => said = message,
            Event::TurnEnded { .. } => break,
            Event::SessionFile(path) => panic!("a session was begun while locked: {}", path.display()),
            _ => {}
        }
    }
    assert_eq!(said, LOCKED);
    run.handle.commands.send(Command::Resume(old.clone())).unwrap();
    loop {
        match run.events.recv_timeout(Duration::from_secs(20)).unwrap() {
            Event::Failed(message) => {
                assert_eq!(message, LOCKED, "a plain session is not opened either");
                break;
            }
            Event::SessionStarted { path, .. } if path == old => panic!("it was opened"),
            _ => {}
        }
    }
    assert_eq!(std::fs::read_dir(run._sessions.path()).unwrap().flatten().filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl")).count(), 0);
    // unlocked, the same agent goes on
    policy.set_off();
    run.handle.commands.send(Command::Resume(old)).unwrap();
    loop {
        if let Event::SessionStarted { transcript, .. } = run.events.recv_timeout(Duration::from_secs(20)).unwrap() {
            if !transcript.is_empty() {
                assert_eq!(transcript.len(), 1);
                break;
            }
        }
    }
}

#[test]
fn a_new_session_is_sealed_once_the_key_is_here_and_what_the_model_keeps_is_sealed_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let notes = tempfile::tempdir().unwrap();
    let key = Cipher::generate().unwrap();
    let policy = Policy::locked();
    let remember = serde_json::json!({"note": "the person prefers answers in Japanese", "scope": "user"});
    let (client, bodies) = server(vec![call_answer("remember", remember), text_answer("kept it")]);
    let run = run_under(dir.path(), client, policy.clone(), Some(notes.path().to_path_buf()));
    policy.set_on(key.clone());
    run.handle.commands.send(Command::Send("remember my secret plan banana".into())).unwrap();
    let path = session_file(&run);
    until_end(&run, Decision::Reject);
    assert!(crate::session::is_encrypted(&path));
    let bytes = std::fs::read(&path).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("banana") && !String::from_utf8_lossy(&bytes).contains("kept it"));
    assert!(bodies.lock().unwrap()[0].to_string().contains("banana"), "the model is sent what was said, as always");
    // the note is in a sealed file, and the notes read back
    let file = notes.path().join("user.md");
    let kept = std::fs::read(&file).unwrap();
    assert!(crate::conversation_key::is_sealed_text(&kept) && !String::from_utf8_lossy(&kept).contains("Japanese"));
    assert_eq!(crate::memory::notes(notes.path(), dir.path()).len(), 1);
    // the session is back as it was, and its checkpoint index is sealed
    run.handle.commands.send(Command::NewSession(dir.path().to_path_buf())).unwrap();
    run.handle.commands.send(Command::Resume(path.clone())).unwrap();
    loop {
        if let Event::SessionStarted { path: opened, transcript, .. } = run.events.recv_timeout(Duration::from_secs(20)).unwrap() {
            if opened == path {
                assert!(transcript.iter().any(|message| message.content.as_deref().is_some_and(|text| text.contains("banana"))));
                break;
            }
        }
    }
    let index = std::fs::read(path.with_extension("checkpoints").join("index.json")).unwrap();
    assert!(index.starts_with(crate::session::SEALED_MARK) && !String::from_utf8_lossy(&index).contains("banana"), "the turn's title is the first words of what was asked");
    assert!(crate::checkpoint::Checkpoints::open(&path).turns().iter().any(|turn| turn.title.contains("banana")), "and it reads back");
}

#[test]
fn a_session_opened_with_lines_that_did_not_open_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let key = Cipher::generate().unwrap();
    let session_folder = tempfile::tempdir().unwrap();
    let path = {
        let mut session = crate::session::Session::create_with(session_folder.path(), dir.path(), "now", "1", Some(&key)).unwrap();
        session.append(&Message::text("system", "s")).unwrap();
        session.append(&Message::text("user", "kept")).unwrap();
        session.append(&Message::text("assistant", "damaged")).unwrap();
        session.path.clone()
    };
    let mut lines: Vec<String> = std::fs::read_to_string(&path).unwrap().lines().map(str::to_string).collect();
    lines[4] = lines[4].replace(&lines[4][20..24], "AAAA");
    std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
    let (client, _) = server(vec![]);
    let run = run_under(dir.path(), client, Policy::on(key), None);
    run.handle.commands.send(Command::Resume(path)).unwrap();
    let (mut started, mut told) = (false, false);
    loop {
        match run.events.recv_timeout(Duration::from_secs(20)).unwrap() {
            Event::SessionStarted { transcript, .. } => {
                started = true;
                assert_eq!(transcript.len(), 2, "what did open is shown");
            }
            Event::Notice(text) if text.starts_with(crate::session::DAMAGE_NOTICE) => {
                assert!(started && text.ends_with("lines 5"), "{text}");
                told = true;
            }
            Event::Usage { .. } if started => break,
            _ => {}
        }
    }
    assert!(told, "it opened without a word");
}
