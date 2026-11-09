//! An `explore` helper's conversation: kept as a session of its own beside the one that asked, written as a session is (so encrypted when conversations are), told to the window as
//! it goes, stopped by the person alone, and carried by a branch, deleted with the session, and left out of the list and the search.

use super::*;
use crate::conversation_key::{Cipher, Policy};
use crate::session::{self, helper_file_of, helpers_of, is_helper};

fn run_under(workspace: &Path, client: Client, policy: Arc<Policy>) -> Run {
    let (sender, events) = channel();
    let sessions = tempfile::tempdir().unwrap();
    let handle = start(Config { workspace: workspace.to_path_buf(), sessions: sessions.path().to_path_buf(), rules: sessions.path().join("permissions.json"), mode: Mode::Auto, effort: None, web: WebSettings::default(), language: "auto".into(), japanese_system: false, instructions: None, home: None, skills_off: Vec::new(), memory: None, mcp: None, policy }, sender, Box::new(|| {}));
    handle.commands.send(Command::Provider(Arc::new(client), "model".into(), 100_000)).unwrap();
    Run { handle, events, _sessions: sessions }
}

/// The session's file, and the helper events until the turn ends, in the order they came, each with a word for what it was.
fn until_over(run: &Run) -> (Option<PathBuf>, Vec<String>) {
    let (mut file, mut told) = (None, Vec::new());
    loop {
        match run.events.recv_timeout(Duration::from_secs(20)).expect("the turn ended") {
            Event::SessionFile(path) => file = Some(path),
            Event::HelperStarted { id, task, most, file } => told.push(format!("started {id} <{task}> most {most} file {}", file.unwrap_or_default())),
            Event::HelperStep { id, step, most, tool, summary, .. } => told.push(format!("step {id} {step}/{most} {tool} <{summary}>")),
            Event::HelperEnded { id, how, steps, files, .. } => told.push(format!("ended {id} {how:?} steps {steps} files {files}")),
            Event::ToolEnded { id, ok, output, .. } => told.push(format!("tool {id} {ok} {}", output.lines().next().unwrap_or(""))),
            Event::TurnEnded { .. } => return (file, told),
            _ => {}
        }
    }
}

/// A helper that reads a file, looks in another and answers.
fn looking_server() -> (Client, Arc<Mutex<Vec<Value>>>) {
    server(vec![
        call_answer("explore", serde_json::json!({"task": "Where is the xylophone?"})),
        call_answer("read_file", serde_json::json!({"path": "a.txt"})),
        call_answer("list_files", serde_json::json!({"path": "."})),
        text_answer("The xylophone is in a.txt."),
        text_answer("found it"),
    ])
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "the xylophone is here\n").unwrap();
    dir
}

#[test]
fn a_helpers_conversation_is_kept_as_a_session_of_its_own_and_the_call_says_which() {
    let dir = workspace();
    let (client, _) = looking_server();
    let run = run_agent(dir.path(), client, Mode::Default);
    run.handle.commands.send(Command::Send("find it".into())).unwrap();
    let (session, _) = until_over(&run);
    let session = session.expect("the session has a file");
    let kept = helper_file_of(&session, 1);
    assert!(kept.is_file() && is_helper(&kept), "{}", kept.display());
    assert_eq!(helpers_of(&session), [(1, kept.clone())]);
    // its messages: the prompt and the task, each request of its own with the calls it made and what came back, and its answer
    let messages = session::read(&kept).unwrap();
    let shape: Vec<(&str, Option<&str>)> = messages.iter().map(|message| (message.role.as_str(), message.content.as_deref().map(|text| text.lines().next().unwrap_or("")))).collect();
    assert_eq!(shape[0].0, "system");
    assert_eq!(shape[1], ("user", Some("Where is the xylophone?")));
    assert_eq!(messages[2].tool_calls.as_ref().unwrap()[0].function.name, "read_file");
    assert_eq!((messages[3].role.as_str(), messages[3].tool_call_id.as_deref()), ("tool", Some("call-1")));
    assert!(messages[3].content.as_deref().unwrap().contains("the xylophone is here"));
    assert_eq!(messages[4].tool_calls.as_ref().unwrap()[0].function.name, "list_files");
    assert_eq!((messages.len(), messages[6].role.as_str(), messages[6].content.as_deref()), (7, "assistant", Some("The xylophone is in a.txt.")));
    assert!(messages.iter().all(|message| message.helper.is_none()), "a helper's conversation holds no helper of its own");
    // the call's result says where it came from, and how it went; the session still has one conversation
    let parent = session::read(&session).unwrap();
    let result = parent.iter().find(|message| message.role == "tool").unwrap();
    let helper = result.helper.as_ref().expect("the result records its helper");
    assert_eq!((helper.number, helper.steps, helper.files, helper.stopped), (1, 2, 1, false));
    assert!(helper.tokens > 0);
    assert!(parent.iter().filter(|message| message.role == "tool").count() == 1 && parent.iter().filter(|message| message.helper.is_some()).count() == 1);
    assert_eq!(session::list(session.parent().unwrap()).len(), 1, "a helper's file is not a session");
    // the link comes back with the session
    let (_, _, reopened) = session::Session::open(&session).unwrap();
    assert_eq!(reopened.iter().find(|message| message.role == "tool").unwrap().helper.as_ref().map(|helper| helper.number), Some(1));
    // what the helper's conversation holds is not found as a session: the search looks at the conversations
    assert!(session::search(session.parent().unwrap(), "xylophone").is_empty());
    assert_eq!(session::search(session.parent().unwrap(), "find it").len(), 1);
}

#[test]
fn the_helpers_events_come_in_order_start_steps_end_and_then_the_calls_end() {
    let dir = workspace();
    let (client, _) = looking_server();
    let run = run_agent(dir.path(), client, Mode::Default);
    run.handle.commands.send(Command::Send("find it".into())).unwrap();
    let (session, told) = until_over(&run);
    let name = helper_file_of(&session.unwrap(), 1).file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(told, [
        format!("started call-1 <Where is the xylophone?> most 20 file {name}"),
        "step call-1 1/20 read_file <a.txt>".to_string(),
        "step call-1 2/20 list_files <.>".to_string(),
        "ended call-1 Answered steps 2 files 1".to_string(),
        "tool call-1 true The xylophone is in a.txt.".to_string(),
    ]);
}

#[test]
fn a_helper_that_failed_is_told_as_failed_and_its_conversation_is_kept_as_far_as_it_went() {
    let dir = workspace();
    let (client, _) = server(vec![call_answer("explore", serde_json::json!({"task": "Look"})), text_answer(""), text_answer("went on")]);
    let run = run_agent(dir.path(), client, Mode::Default);
    run.handle.commands.send(Command::Send("look".into())).unwrap();
    let (session, told) = until_over(&run);
    assert!(told.iter().any(|line| line.starts_with("ended call-1 Failed(")), "{told:?}");
    assert_eq!(session::read(&helper_file_of(&session.unwrap(), 1)).unwrap().len(), 3);
}

#[test]
fn the_persons_stop_on_the_helper_ends_it_with_the_stated_result_and_the_parents_turn_goes_on() {
    let dir = workspace();
    let entered = Arc::new(AtomicBool::new(false));
    let waiting = entered.clone();
    // the helper's second request is under way when the stop comes, and ends with the flag its stream was given
    let blocked: Step = Box::new(move |cancel: &AtomicBool| {
        waiting.store(true, Ordering::Relaxed);
        for _ in 0..4000 {
            if cancel.load(Ordering::Relaxed) {
                return Err(LlmError::Cancelled);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("nothing stopped the helper");
    });
    let (run, provider) = scripted(dir.path(), vec![calls(&[("explore", serde_json::json!({"task": "Look at everything"}))]), calls(&[("read_file", serde_json::json!({"path": "a.txt"}))]), blocked, says("I went on without it")], Mode::Default);
    run.handle.commands.send(Command::Send("look".into())).unwrap();
    let (mut told, mut stopped) = (Vec::new(), false);
    let (mut session, began) = (None, Instant::now());
    loop {
        // (the stop comes once the second request is waiting: not before, or it is the check at the top of the loop that ends it)
        if told.len() == 1 && entered.load(Ordering::Relaxed) && !stopped {
            run.handle.helper_stop.stop("c0");
            stopped = true;
        }
        let Ok(event) = run.events.recv_timeout(Duration::from_millis(20)) else {
            assert!(began.elapsed() < Duration::from_secs(20), "the turn ended");
            continue;
        };
        match event {
            Event::SessionFile(path) => session = Some(path),
            Event::HelperStep { step, .. } => told.push(format!("step {step}")),
            Event::HelperEnded { how, steps, helper, .. } => told.push(format!("ended {how:?} {steps} {}", helper.as_ref().is_some_and(|helper| helper.stopped))),
            Event::ToolEnded { ok, output, .. } => told.push(format!("tool {ok} {output}")),
            Event::TurnEnded { cancelled } => {
                assert!(!cancelled, "only the helper was stopped");
                break;
            }
            _ => {}
        }
    }
    assert_eq!(told, ["step 1", "ended Stopped 1 true", "tool true stopped by the person"], "{told:?}");
    assert_eq!(provider.requests(), 4, "the parent asked again after it");
    let last = provider.messages(3);
    assert_eq!(last.last().unwrap().content.as_deref(), Some(HELPER_STOPPED), "the parent's next request has the stated result");
    assert!(!run.handle.cancel.load(Ordering::Relaxed), "the turn's own stop is not set");
    // what the helper had done is kept, and the call's record of it says it was stopped
    let parent = session::read(&session.unwrap()).unwrap();
    assert_eq!(parent.last().unwrap().content.as_deref(), Some("I went on without it"));
    let helper = parent.iter().find_map(|message| message.helper.clone()).expect("the record is there");
    assert!(helper.stopped && helper.steps == 1 && helper.files == 1, "{helper:?}");
}

#[test]
fn a_stop_pressed_on_a_helper_that_has_ended_is_not_for_the_next() {
    let dir = workspace();
    let (client, _) = server(vec![
        call_answer("explore", serde_json::json!({"task": "One"})),
        text_answer("first answer"),
        call_answer("explore", serde_json::json!({"task": "Two"})),
        text_answer("second answer"),
        text_answer("done"),
    ]);
    let run = run_agent(dir.path(), client, Mode::Default);
    // (the same call id each time, as this server gives: a stop left over from the first would end the second at once)
    run.handle.helper_stop.stop("call-1");
    run.handle.commands.send(Command::Send("look twice".into())).unwrap();
    let (_, told) = until_over(&run);
    assert!(told.iter().filter(|line| line.starts_with("ended")).all(|line| line.contains("Answered")), "{told:?}");
    assert!(told.iter().any(|line| line == "tool call-1 true second answer"), "{told:?}");
}

#[test]
fn the_persons_stop_on_the_turn_ends_the_helper_too() {
    let dir = workspace();
    let blocked: Step = Box::new(|cancel: &AtomicBool| {
        for _ in 0..4000 {
            if cancel.load(Ordering::Relaxed) {
                return Err(LlmError::Cancelled);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("nothing stopped the helper");
    });
    let (run, provider) = scripted(dir.path(), vec![calls(&[("explore", serde_json::json!({"task": "Look"}))]), blocked], Mode::Default);
    run.handle.commands.send(Command::Send("look".into())).unwrap();
    while provider.requests() < 2 {
        std::thread::sleep(Duration::from_millis(5));
    }
    run.handle.cancel.store(true, Ordering::Relaxed);
    let (mut ended, mut over) = (None, false);
    while !over {
        match run.events.recv_timeout(Duration::from_secs(20)).expect("the turn ended") {
            Event::HelperEnded { how, .. } => ended = Some(how),
            Event::TurnEnded { cancelled } => over = cancelled,
            _ => {}
        }
    }
    assert_eq!(ended, Some(HelperEnd::Stopped));
}

#[test]
fn the_trajectory_names_the_file_of_each_helper_request() {
    let dir = workspace();
    let (client, _) = looking_server();
    let run = run_agent(dir.path(), client, Mode::Default);
    run.handle.commands.send(Command::Send("find it".into())).unwrap();
    let (session, _) = until_over(&run);
    let session = session.unwrap();
    let name = helper_file_of(&session, 1).file_name().unwrap().to_string_lossy().into_owned();
    let lines = trajectory_of(&run);
    let requests = of_kind(&lines, "request");
    let files: Vec<(&str, Option<&str>)> = requests.iter().map(|line| (line["role"].as_str().unwrap(), line["helper_file"].as_str())).collect();
    assert_eq!(files, [("main", None), ("helper", Some(name.as_str())), ("helper", Some(name.as_str())), ("helper", Some(name.as_str())), ("main", None)]);
    // and the report points to it from each of those requests, and from no other
    let said = crate::trajectory_report::report_files(&crate::trajectory::file_of(&session), None).unwrap();
    assert_eq!(said.matches(&format!("(the helper's steps are in {name})")).count(), 3, "{said}");
}

#[test]
fn a_helpers_file_is_sealed_when_conversations_are() {
    let dir = workspace();
    let key = Cipher::generate().unwrap();
    let policy = Policy::locked();
    policy.set_on(key);
    let (client, _) = looking_server();
    let run = run_under(dir.path(), client, policy);
    run.handle.commands.send(Command::Send("find it".into())).unwrap();
    let (session, _) = until_over(&run);
    let session = session.unwrap();
    let kept = helper_file_of(&session, 1);
    assert!(session::is_encrypted(&session) && session::is_encrypted(&kept));
    let bytes = String::from_utf8_lossy(&std::fs::read(&kept).unwrap()).into_owned();
    assert!(!bytes.contains("xylophone") && !bytes.contains("a.txt"), "nothing of what it read or was asked is in the file");
    assert_eq!(session::read(&kept).unwrap().len(), 7, "it reads back with the key");
    assert_eq!(session::list(session.parent().unwrap()).len(), 1);
    let header = |path: &Path| std::fs::read_to_string(path).unwrap().lines().next().unwrap().to_string();
    assert_ne!(header(&session), header(&kept), "a file id of its own");
}

fn parent_with_helpers(folder: &Path, key: Option<&Arc<Cipher>>) -> (PathBuf, [PathBuf; 2]) {
    let stamp = if key.is_some() { "20261107-100000" } else { "20261107-090000" };
    let mut parent = session::Session::create_with(folder, Path::new("C:/w"), "now", stamp, key).unwrap();
    parent.append(&Message::text("system", "s")).unwrap();
    parent.append(&Message::text("user", "look into two things")).unwrap();
    let mut files = Vec::new();
    for (number, word) in [(1u32, "first"), (2, "second")] {
        let policy = Policy::locked();
        match key {
            Some(key) => policy.set_on(key.clone()),
            None => policy.set_off(),
        }
        let (mut helper, made) = session::Session::create_helper(&policy, &parent.path, Path::new("C:/w"), "now").unwrap();
        assert_eq!(made, number);
        helper.append(&Message::text("user", format!("look into the {word} thing"))).unwrap();
        helper.append(&Message::text("assistant", format!("the {word} answer"))).unwrap();
        files.push(helper.path.clone());
        let mut result = Message::tool_result(&format!("c{number}"), format!("the {word} answer"));
        result.helper = Some(crate::message::HelperRef { number, steps: 1, files: 1, ms: 5, tokens: 9, stopped: false });
        let call = crate::message::ToolCall { id: format!("c{number}"), kind: "function".into(), function: crate::message::FunctionCall { name: "explore".into(), arguments: "{}".into() } };
        parent.append(&Message { role: "assistant".into(), tool_calls: Some(vec![call]), ..Message::default() }).unwrap();
        parent.append(&result).unwrap();
    }
    parent.append(&Message::text("assistant", "both done")).unwrap();
    (parent.path.clone(), [files.remove(0), files.remove(0)])
}

#[test]
fn a_branch_carries_the_helpers_of_the_messages_it_keeps_and_leaves_the_rest() {
    for encrypted in [false, true] {
        let folder = tempfile::tempdir().unwrap();
        let key = Cipher::generate().unwrap();
        let (parent, [first, second]) = parent_with_helpers(folder.path(), encrypted.then_some(&key));
        let before = (std::fs::read(&parent).unwrap(), std::fs::read(&first).unwrap(), std::fs::read(&second).unwrap());
        // through the first helper's result (the system prompt, the person's words, the call, its result)
        let branch = session::fork(&parent, 3, Some("a branch")).unwrap();
        let carried = helpers_of(&branch);
        assert_eq!(carried.iter().map(|(number, _)| *number).collect::<Vec<_>>(), [1], "only the helper whose result the branch keeps (encrypted: {encrypted})");
        assert_eq!(std::fs::read(&carried[0].1).unwrap(), before.1, "as it was, line for line");
        assert_eq!(session::read(&carried[0].1).unwrap().len(), 2);
        assert!(carried[0].1.file_name().unwrap().to_string_lossy().starts_with(&branch.file_stem().unwrap().to_string_lossy().into_owned()));
        // the original is as it was
        assert_eq!((std::fs::read(&parent).unwrap(), std::fs::read(&first).unwrap(), std::fs::read(&second).unwrap()), before);
        // the whole of it carries both
        let whole = session::fork(&parent, 6, None).unwrap();
        assert_eq!(helpers_of(&whole).len(), 2);
        let titles: Vec<String> = session::list(folder.path()).into_iter().map(|summary| summary.title).collect();
        assert_eq!(titles.len(), 3, "three sessions, and no helper among them: {titles:?}");
        // a result the branch has still says its helper, which it has
        let kept = session::read(&branch).unwrap();
        let number = kept.iter().find_map(|message| message.helper.as_ref().map(|helper| helper.number)).unwrap();
        assert!(session::helper_path(&branch, number).is_some());
    }
}

#[test]
fn naming_a_session_leaves_its_helpers_where_they_are_and_deleting_it_takes_them() {
    let folder = tempfile::tempdir().unwrap();
    let (parent, [first, second]) = parent_with_helpers(folder.path(), None);
    let other = session::Session::create(folder.path(), Path::new("C:/w"), "now", "20261107-090000-2").unwrap().path;
    let mut other_session = session::Session::open(&other).unwrap().0;
    other_session.append(&Message::text("user", "an unrelated session")).unwrap();
    let (_, other_helper) = session::Session::create_helper(&Policy::default(), &other, Path::new("C:/w"), "now").map(|(made, number)| (number, made.path)).unwrap();
    // a new name changes nobody's file name: the helpers are still its own
    session::rename(&parent, "the two looks").unwrap();
    assert_eq!(helpers_of(&parent).len(), 2);
    let mut titles: Vec<String> = session::list(folder.path()).into_iter().map(|summary| summary.title).collect();
    titles.sort();
    assert_eq!(titles, ["an unrelated session", "the two looks"]);
    assert!(first.is_file() && second.is_file());
    // deleted, the session takes its own helpers and no other's, nor a name that only begins the same
    crate::checkpoint::remove_for(&parent).unwrap();
    std::fs::remove_file(&parent).unwrap();
    assert!(!first.exists() && !second.exists());
    assert!(other_helper.is_file(), "the helper of the session whose name begins with this one's stays");
    assert_eq!(helpers_of(&other).len(), 1);
}

#[test]
fn a_helpers_number_goes_on_from_the_files_there_are_and_a_file_name_that_is_no_helpers_is_none() {
    let folder = tempfile::tempdir().unwrap();
    let (parent, _) = parent_with_helpers(folder.path(), None);
    let (made, number) = session::Session::create_helper(&Policy::default(), &parent, Path::new("C:/w"), "now").unwrap();
    assert_eq!(number, 3);
    assert_eq!(made.path, helper_file_of(&parent, 3));
    for name in ["20261107-090000.jsonl", "20261107-090000.trajectory.jsonl", "a.helper-.jsonl", "a.helper-x.jsonl", "a.helper-1.json", "helper-1.txt"] {
        assert!(!is_helper(Path::new(name)), "{name}");
    }
    assert!(is_helper(Path::new("C:/data/sessions/20261107-090000-2.helper-12.jsonl")));
}

#[test]
fn turning_conversations_on_later_seals_the_helpers_files_with_the_sessions() {
    let data = tempfile::tempdir().unwrap();
    let sessions = data.path().join("sessions");
    let (parent, [first, _]) = parent_with_helpers(&sessions, None);
    assert!(!session::is_encrypted(&first));
    let (files, _) = crate::conversion::targets(data.path());
    assert!(files.contains(&parent) && files.contains(&first), "a helper's file is converted with the sessions: {files:?}");
}
