//! A session that goes on by itself: a background command's end, a line it prints, a wait that is up. The fake provider answers; nothing here sleeps for a minute (a clock the test moves
//! carries the waits), and what the commands "print" is what the test gives them.

use super::*;
use crate::background::{Paused, MOST_IN_AN_HOUR, MOST_IN_A_ROW};
use crate::message::WakeCause;
use crate::tools::{CommandResult, Live};

const OK: &str = "stdout:\nbuilt\nstderr:\n\nexit_code: 0";

fn finished(report: &str) -> CommandResult {
    Arc::new(Mutex::new(Some(Ok(report.to_string()))))
}

fn running() -> CommandResult {
    Arc::new(Mutex::new(None))
}

/// A command the session started in the background, with the result it has (none: it runs) and what it has printed.
fn started(agent: &mut Agent, command: &str, result: CommandResult, live: Arc<Live>) -> String {
    agent.bench.add_job(&json!({"command": command}), Instant::now(), Arc::new(AtomicBool::new(false)), result, live).unwrap()
}

/// An agent in the middle of a session: it began a build in the background and said so. No turn has just ended.
fn after_a_turn(steps: Vec<Step>) -> (Agent, Arc<Scripted>, Receiver<Event>, [tempfile::TempDir; 2]) {
    let messages = vec![Message::text("system", "s"), Message::text("user", "build it"), Message::text("assistant", "It is building in the background.")];
    let (mut agent, provider, events, kept) = agent_holding(messages, 100_000, steps);
    agent.quiet_until = None;
    (agent, provider, events, kept)
}

/// What the window would be told of the turns, in a word each.
fn told(events: &Receiver<Event>) -> Vec<String> {
    events.try_iter().filter_map(|event| match event {
        Event::Woken(wakes) => Some(format!("woken:{}", wakes.iter().map(|wake| format!("{}/{}", wake.id, wake.cause.name())).collect::<Vec<_>>().join(","))),
        Event::TurnStarted => Some("started".to_string()),
        Event::TurnEnded { cancelled } => Some(format!("ended:{cancelled}")),
        Event::Notice(text) => Some(format!("notice:{text}")),
        Event::Failed(text) => Some(format!("failed:{text}")),
        _ => None,
    }).collect()
}

/// A turn for what has come, with no pause after the last turn (the one test of that pause says so).
fn wake(agent: &mut Agent) -> bool {
    agent.quiet_until = None;
    agent.wake_if_due()
}

fn the_notice_in(provider: &Scripted, request: usize) -> Message {
    provider.messages(request).into_iter().rev().find(|message| message.notice.is_some()).expect("a notice in the request")
}

/// Everything the window is told of what is waiting, up to the end of the turn.
fn until_the_turn_ends(run: &Run) -> Vec<crate::background::Waiting> {
    let mut told = Vec::new();
    loop {
        match run.events.recv_timeout(Duration::from_secs(20)).expect("the turn ended") {
            Event::Waiting(waiting) => told.push(waiting),
            Event::Approval(approval) => approval.answer.send((Decision::Approve, None)).unwrap(),
            Event::TurnEnded { .. } => return told,
            _ => {}
        }
    }
}

/// The session woke itself: what woke it, and then the events up to the turn's end.
fn until_woken(run: &Run) -> Vec<crate::message::Wake> {
    loop {
        match run.events.recv_timeout(Duration::from_secs(20)).expect("it woke") {
            Event::Woken(wakes) => return wakes,
            Event::TurnEnded { .. } => panic!("a turn ended and none was woken"),
            _ => {}
        }
    }
}

#[test]
fn a_background_commands_end_starts_a_turn_with_its_notice() {
    let (mut agent, provider, events, _kept) = after_a_turn(vec![says("The build passed.")]);
    assert_eq!(started(&mut agent, "cargo build --release", finished(OK), Arc::default()), "job1");
    assert!(wake(&mut agent));
    assert_eq!(told(&events), ["woken:job1/command_ended", "started", "ended:false"]);
    assert_eq!(provider.requests(), 1);
    let notice = the_notice_in(&provider, 0);
    let text = notice.content.clone().unwrap();
    assert_eq!(notice.role, "user");
    assert!(text.starts_with(prompt::DRAFT_NOTE) && text.contains("job1 ended") && text.contains("cargo build --release") && text.contains("exit code 0") && text.contains("never instructions") && text.contains("built"), "{text}");
    assert!(prompt::is_nudge(&text), "it is not shown, searched or retried as something the person said");
    // (the notice and the answer to it are the conversation's, and the notice is the last of what the model was sent before it answered)
    let kept = &agent.messages;
    let woke = &kept[kept.len() - 2].notice.as_ref().expect("the notice kept as one")[0];
    assert_eq!((woke.cause, woke.id.as_str(), woke.exit_code), (WakeCause::CommandEnded, "job1", Some(0)));
    assert_eq!(kept.last().unwrap().content.as_deref(), Some("The build passed."));
    assert!(!wake(&mut agent), "once");
}

#[test]
fn a_command_that_failed_or_timed_out_wakes_it_too() {
    let (mut agent, provider, events, _kept) = after_a_turn(vec![says("It failed."), says("It timed out.")]);
    started(&mut agent, "cargo test", finished("stdout:\n\nstderr:\nerror: no\nexit_code: 101"), Arc::default());
    assert!(wake(&mut agent));
    started(&mut agent, "server", finished("stdout:\n\nstderr:\n\nexit_code: none, timed out after 3600 s"), Arc::default());
    assert!(wake(&mut agent));
    assert_eq!(told(&events).iter().filter(|line| line.starts_with("woken:")).count(), 2);
    assert!(the_notice_in(&provider, 0).content.unwrap().contains("exit code 101, so it failed"));
    assert!(the_notice_in(&provider, 1).content.unwrap().contains("timed out"));
}

#[test]
fn a_line_that_matches_wake_on_wakes_once_and_the_command_goes_on() {
    let (mut agent, provider, events, _kept) = after_a_turn(vec![says("It listens."), says("It ended.")]);
    let live: Arc<Live> = Arc::default();
    live.watch_for(regex::Regex::new("Listening on").unwrap());
    let done = running();
    agent.bench.add_job(&json!({"command": "npm run dev", "wake_on": "Listening on"}), Instant::now(), Arc::new(AtomicBool::new(false)), done.clone(), live.clone()).unwrap();
    live.push(b"compiling\n");
    assert!(!wake(&mut agent), "nothing it printed matches yet");
    live.push(b"Listening on http://localhost:5173\n");
    assert!(wake(&mut agent));
    assert_eq!(told(&events), ["woken:job1/command_line", "started", "ended:false"]);
    assert!(the_notice_in(&provider, 0).content.unwrap().contains("Listening on http://localhost:5173"));
    live.push(b"Listening on http://localhost:5174\n");
    assert!(!wake(&mut agent), "the next that matches wakes nothing: once");
    // (it went on: its end is a wake of its own)
    *done.lock().unwrap() = Some(Ok(OK.to_string()));
    assert!(wake(&mut agent));
    assert_eq!(told(&events), ["woken:job1/command_ended", "started", "ended:false"]);
    assert_eq!(provider.requests(), 2);
}

#[test]
fn a_command_the_model_stopped_itself_wakes_nothing() {
    let (mut agent, provider, events, _kept) = after_a_turn(vec![]);
    let done = running();
    let id = started(&mut agent, "npm run dev", done.clone(), Arc::default());
    let (workspace, cancel) = (agent.workspace.clone(), AtomicBool::new(false));
    let said = agent.bench.run("command_stop", &json!({"id": id}), &workspace, &cancel).unwrap();
    assert!(said.contains("will not wake you"), "{said}");
    // (the command ends, as a stopped one does)
    *done.lock().unwrap() = Some(Ok("stdout:\n\nstderr:\n\nexit_code: none, cancelled after 2 s".to_string()));
    assert!(!wake(&mut agent));
    assert_eq!((provider.requests(), told(&events).len()), (0, 0));
    assert!(agent.background.snapshot().is_empty());
}

#[test]
fn a_command_whose_end_the_model_has_read_wakes_nothing() {
    let (mut agent, provider, _events, _kept) = after_a_turn(vec![]);
    let id = started(&mut agent, "make", finished(OK), Arc::default());
    let (workspace, cancel) = (agent.workspace.clone(), AtomicBool::new(false));
    // (it looked, in the turn, and its end is in the conversation already)
    let looked = agent.bench.run("command_status", &json!({"id": id}), &workspace, &cancel).unwrap();
    assert!(looked.contains("has ended") && looked.contains("built"));
    assert!(!wake(&mut agent));
    assert_eq!(provider.requests(), 0);
}

#[test]
fn a_notice_that_comes_during_a_turn_waits_for_its_end_and_then_starts_the_next() {
    let done = running();
    let during = done.clone();
    let working: Step = Box::new(move |_| {
        *during.lock().unwrap() = Some(Ok(OK.to_string()));
        Ok(llm::Completion { content: "Working on it.".into(), finish: "stop".into(), ..llm::Completion::default() })
    });
    let (mut agent, provider, events, _kept) = after_a_turn(vec![working, says("It finished.")]);
    started(&mut agent, "make", done, Arc::default());
    agent.turn("anything else?", Vec::new(), Vec::new());
    assert_eq!(told(&events), ["started", "ended:false"], "nothing woke it in the middle of the turn");
    assert_eq!(provider.requests(), 1);
    // (the person's own words may be on their way as the turn ends: a turn nobody sent is not begun at once)
    assert!(!agent.wake_if_due());
    std::thread::sleep(WAKE_AFTER_A_TURN + Duration::from_millis(100));
    assert!(agent.wake_if_due());
    assert_eq!(told(&events), ["woken:job1/command_ended", "started", "ended:false"]);
    assert_eq!(provider.requests(), 2);
}

#[test]
fn notices_that_come_together_make_one_turn() {
    let (mut agent, provider, events, _kept) = after_a_turn(vec![says("Both are done."), says("not asked for")]);
    started(&mut agent, "build a", finished(OK), Arc::default());
    started(&mut agent, "build b", finished("stdout:\n\nstderr:\n\nexit_code: 2"), Arc::default());
    agent.bench.run("wake_at", &json!({"delay_seconds": 60, "note": "look at both"}), &agent.workspace.clone(), &AtomicBool::new(false)).unwrap();
    agent.background.skip(Duration::from_secs(61));
    assert!(wake(&mut agent));
    assert!(!wake(&mut agent));
    assert_eq!(provider.requests(), 1, "one request, for the three of them");
    let said = told(&events);
    let mut woken: Vec<&str> = said[0].strip_prefix("woken:").unwrap().split(',').collect();
    woken.sort_unstable();
    assert_eq!((woken, &said[1..]), (vec!["job1/command_ended", "job2/command_ended", "wait1/wait"], &["started".to_string(), "ended:false".to_string()][..]));
    let text = the_notice_in(&provider, 0).content.unwrap();
    assert!(text.contains("job1 ended") && text.contains("job2 ended") && text.contains("wait1 is up") && text.contains("look at both"), "{text}");
    assert_eq!(provider.messages(0).iter().filter(|message| message.notice.is_some()).count(), 1, "one message");
}

#[test]
fn a_wait_wakes_the_session_when_the_clock_says_so() {
    let (mut agent, provider, events, _kept) = after_a_turn(vec![says("Looked: all fine.")]);
    let (workspace, cancel) = (agent.workspace.clone(), AtomicBool::new(false));
    let set = agent.bench.run("wake_at", &json!({"delay_seconds": 600, "note": "look at the server"}), &workspace, &cancel).unwrap();
    assert!(set.starts_with("wait1 is set"), "{set}");
    assert!(!wake(&mut agent));
    agent.background.skip(Duration::from_secs(599));
    assert!(!wake(&mut agent));
    agent.background.skip(Duration::from_secs(2));
    assert!(wake(&mut agent));
    assert_eq!(told(&events), ["woken:wait1/wait", "started", "ended:false"]);
    assert!(the_notice_in(&provider, 0).content.unwrap().contains("look at the server"));
    assert!(!wake(&mut agent), "once");
}

#[test]
fn a_wait_is_cancelled_by_command_stop_and_listed_by_command_status() {
    let (mut agent, _provider, _events, _kept) = after_a_turn(vec![]);
    let (workspace, cancel) = (agent.workspace.clone(), AtomicBool::new(false));
    agent.bench.run("wake_at", &json!({"delay_seconds": 3600, "note": "check the deploy"}), &workspace, &cancel).unwrap();
    let listed = agent.bench.run("command_status", &json!({}), &workspace, &cancel).unwrap();
    assert!(listed.starts_with("wait1 is set, up in 1 h 0 min") && listed.contains("check the deploy"), "{listed}");
    let cancelled = agent.bench.run("command_stop", &json!({"id": "wait1"}), &workspace, &cancel).unwrap();
    assert!(cancelled.contains("wait1 is cancelled"), "{cancelled}");
    agent.background.skip(Duration::from_secs(4000));
    assert!(!wake(&mut agent));
    assert!(agent.bench.run("command_stop", &json!({"id": "wait1"}), &workspace, &cancel).is_err());
    assert!(agent.bench.run("command_status", &json!({}), &workspace, &cancel).unwrap().starts_with("no command"));
}

#[test]
fn a_sixth_wait_is_refused_with_a_line_that_says_so() {
    let (mut agent, _provider, _events, _kept) = after_a_turn(vec![]);
    let (workspace, cancel) = (agent.workspace.clone(), AtomicBool::new(false));
    let bench = &mut agent.bench;
    for at in 1..=5 {
        bench.run("wake_at", &json!({"delay_seconds": 120, "note": format!("n{at}")}), &workspace, &cancel).unwrap();
    }
    let refused = bench.run("wake_at", &json!({"delay_seconds": 120, "note": "n6"}), &workspace, &cancel).unwrap_err();
    assert!(refused.contains("refused") && refused.contains("5 waits are set already") && refused.contains("command_stop"), "{refused}");
    assert!(bench.run("wake_at", &json!({"delay_seconds": 30, "note": "too soon"}), &workspace, &cancel).unwrap_err().contains("60 to 86400"));
}

/// Thirty turns in a row that nobody sent is as many as a session wakes itself for: the 31st is held, the window is told once, and the person's next word lets it go on, with what was held.
#[test]
fn thirty_woken_turns_in_a_row_stop_the_waking_until_the_person_writes() {
    let steps: Vec<Step> = (0..MOST_IN_A_ROW + 2).map(|_| says("noted")).collect();
    let (mut agent, provider, events, _kept) = after_a_turn(steps);
    for at in 0..MOST_IN_A_ROW {
        started(&mut agent, &format!("job {at}"), finished(OK), Arc::default());
        assert!(wake(&mut agent), "{at}");
    }
    assert_eq!(provider.requests(), MOST_IN_A_ROW as usize);
    told(&events);
    started(&mut agent, "one more", finished(OK), Arc::default());
    assert!(!wake(&mut agent));
    let said = told(&events);
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].starts_with("notice:Waking is paused") && said[0].contains("30 times in a row") && said[0].contains("Write a message"), "{said:?}");
    assert!(!wake(&mut agent));
    assert!(told(&events).is_empty(), "said once");
    assert_eq!(agent.background.snapshot().paused, Some(Paused::InARow(MOST_IN_A_ROW)));
    // (an hour does not lift it)
    agent.background.skip(Duration::from_secs(7200));
    assert!(!wake(&mut agent));
    // The person writes: the count starts again, and what was held is told after their turn.
    agent.turn("go on", Vec::new(), Vec::new());
    agent.quiet_until = None;
    assert!(wake(&mut agent));
    assert!(the_notice_in(&provider, MOST_IN_A_ROW as usize + 1).content.unwrap().contains("one more"));
    assert!(agent.background.snapshot().paused.is_none());
}

#[test]
fn sixty_woken_turns_in_an_hour_stop_the_waking_until_the_hour_has_passed() {
    let steps: Vec<Step> = (0..MOST_IN_AN_HOUR + 2).map(|_| says("noted")).collect();
    let (mut agent, provider, events, _kept) = after_a_turn(steps);
    for at in 0..MOST_IN_AN_HOUR {
        started(&mut agent, &format!("job {at}"), finished(OK), Arc::default());
        assert!(wake(&mut agent), "{at}");
        agent.background.skip(Duration::from_secs(30));
        // (the person writes now and then: that is no hour)
        if at % 20 == 19 {
            agent.background.person_spoke();
        }
    }
    told(&events);
    started(&mut agent, "one too many", finished(OK), Arc::default());
    assert!(!wake(&mut agent));
    let said = told(&events);
    assert!(said.len() == 1 && said[0].starts_with("notice:Waking is paused") && said[0].contains("60 times in the last hour"), "{said:?}");
    agent.background.person_spoke();
    assert!(!wake(&mut agent), "the person's word is no hour");
    // (the first of them is an hour old once so much of it has passed)
    agent.background.skip(Duration::from_secs(3600 - 30 * (MOST_IN_AN_HOUR as u64 - 1)));
    assert!(wake(&mut agent));
    assert_eq!(provider.requests(), MOST_IN_AN_HOUR + 1);
}

/// The person's Stop ends the turn, the commands and the waits of that session, and the window is told there is nothing left.
#[test]
fn stop_ends_the_commands_and_the_waits_of_the_session() {
    let (mut agent, provider, events, _kept) = after_a_turn(vec![]);
    let still_going = Arc::new(AtomicBool::new(false));
    agent.bench.add_job(&json!({"command": "npm run dev"}), Instant::now(), still_going.clone(), running(), Arc::default()).unwrap();
    let (workspace, cancel) = (agent.workspace.clone(), AtomicBool::new(false));
    agent.bench.run("wake_at", &json!({"delay_seconds": 600, "note": "later"}), &workspace, &cancel).unwrap();
    agent.tell_waiting();
    assert!(events.try_iter().any(|event| matches!(event, Event::Waiting(waiting) if waiting.commands.len() == 1 && waiting.waits.len() == 1)));
    let person = agent.cancel.clone();
    let stop: Step = Box::new(move |_| {
        person.store(true, Ordering::Relaxed);
        Err(LlmError::Cancelled)
    });
    provider.steps.lock().unwrap().push_back(stop);
    agent.turn("what now?", Vec::new(), Vec::new());
    assert!(still_going.load(Ordering::Relaxed), "the command was told to end");
    assert!(agent.background.snapshot().is_empty());
    let all: Vec<Event> = events.try_iter().collect();
    assert!(all.iter().any(|event| matches!(event, Event::Waiting(waiting) if waiting.is_empty())), "the window is told it is empty");
    assert!(matches!(all.last(), Some(Event::TurnEnded { cancelled: true })));
    // (and a wait that was cancelled does not come up)
    agent.background.skip(Duration::from_secs(700));
    agent.quiet_until = None;
    assert!(!wake(&mut agent));
}

#[test]
fn a_session_of_its_own_begins_with_nothing_of_the_last_waiting() {
    let (mut agent, _provider, events, _kept) = after_a_turn(vec![]);
    let cancel = Arc::new(AtomicBool::new(false));
    agent.bench.add_job(&json!({"command": "npm run dev"}), Instant::now(), cancel.clone(), running(), Arc::default()).unwrap();
    agent.bench.run("wake_at", &json!({"delay_seconds": 600, "note": "later"}), &agent.workspace.clone(), &AtomicBool::new(false)).unwrap();
    let workspace = agent.workspace.clone();
    agent.new_session(workspace);
    assert!(agent.background.snapshot().is_empty() && cancel.load(Ordering::Relaxed));
    agent.tell_waiting();
    drop(events);
}

#[test]
fn the_session_is_not_woken_when_there_is_nothing_to_answer_with() {
    let (mut agent, provider, _events, _kept) = after_a_turn(vec![says("late")]);
    started(&mut agent, "make", finished(OK), Arc::default());
    agent.provider = None;
    assert!(!wake(&mut agent), "no model to answer: what came is kept");
    agent.provider = Some(provider.clone());
    assert!(wake(&mut agent));
}

/// Waking goes on through the loop, with nobody sending anything; a wait is the one thing here that the test's clock moves.
#[test]
fn a_session_in_the_loop_wakes_by_itself_for_a_wait_and_the_trajectory_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let (run, provider) = scripted(dir.path(), vec![calls(&[("wake_at", json!({"delay_seconds": 60, "note": "look at the server"}))]), says("I will look again in a minute."), says("Looked: all fine.")], Mode::Auto);
    run.handle.commands.send(Command::Send("check on it later".into())).unwrap();
    // (what the window is told it lists meanwhile: the wait, with when it is up)
    let told_of = until_the_turn_ends(&run);
    let waiting = told_of.into_iter().find(|waiting| !waiting.is_empty()).expect("the window was told of the wait");
    assert_eq!((waiting.waits.len(), waiting.waits[0].id.as_str(), waiting.waits[0].note.as_str()), (1, "wait1", "look at the server"));
    std::thread::sleep(Duration::from_millis(900));
    assert_eq!(provider.requests(), 2, "not before it is up");
    run.handle.background.skip(Duration::from_secs(61));
    let wakes = until_woken(&run);
    assert_eq!((wakes[0].cause, wakes[0].id.as_str()), (WakeCause::Wait, "wait1"));
    let (_, cancelled) = until_end(&run, Decision::Approve);
    assert!(!cancelled);
    assert_eq!(provider.requests(), 3);
    assert!(the_notice_in(&provider, 2).content.unwrap().contains("look at the server"));
    // The session file holds the notice as one, with the answer after it, for a window that opens the session again.
    let session = std::fs::read_dir(run._sessions.path()).unwrap().flatten().map(|entry| entry.path()).find(|path| path.extension().is_some_and(|ext| ext == "jsonl") && !trajectory::is_trajectory(path)).unwrap();
    let kept = crate::session::read(&session).unwrap();
    let at = kept.iter().position(|message| message.notice.is_some()).expect("the notice is in the file");
    assert_eq!(kept[at].notice.as_ref().unwrap()[0].id, "wait1");
    assert_eq!(kept[at + 1].content.as_deref(), Some("Looked: all fine."));
    // The trajectory: the wake, which wait it was, how long after it was up the turn began, and the request the turn made first.
    let lines = trajectory_of(&run);
    let wake = of_kind(&lines, "wake");
    assert_eq!(wake.len(), 1, "{lines:?}");
    assert_eq!((wake[0]["cause"].as_str(), wake[0]["id"].as_str()), (Some("wait"), Some("wait1")));
    assert!(wake[0]["after_ms"].as_u64().is_some() && wake[0].get("cmd0").is_none());
    let woken_turn = of_kind(&lines, "turn_start").into_iter().find(|line| line["woken"] == true).expect("a woken turn");
    assert_eq!(woken_turn["turn"], wake[0]["turn"]);
    let first = of_kind(&lines, "request").into_iter().find(|line| line["turn"] == wake[0]["turn"]).unwrap();
    assert_eq!(first["req"], wake[0]["req"], "the wake line names the turn's first request");
    let said = crate::trajectory_report::report_files(&trajectory::file_of(&session), None).unwrap();
    assert!(said.contains("woken") && said.contains("wait1") && said.contains("wait"), "{said}");
}

/// A woken turn asks as any turn does, and waits: nothing approves for it, whatever it waited for.
#[test]
fn a_woken_turn_asks_and_waits_and_nothing_approves_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let (run, provider) = scripted(dir.path(), vec![
        calls(&[("wake_at", json!({"delay_seconds": 60, "note": "run the check"}))]),
        says("Waiting."),
        calls(&[("execute_command", json!({"command": "python check.py", "purpose": "check it"}))]),
        says("It was declined."),
    ], Mode::Default);
    run.handle.commands.send(Command::Send("check later".into())).unwrap();
    until_end(&run, Decision::Approve);
    run.handle.background.skip(Duration::from_secs(61));
    until_woken(&run);
    let approval = loop {
        match run.events.recv_timeout(Duration::from_secs(20)).expect("the woken turn asks") {
            Event::Approval(approval) => break approval,
            Event::TurnEnded { .. } => panic!("it ended without asking"),
            _ => {}
        }
    };
    assert_eq!(approval.tool, "execute_command");
    // Nobody answers for a while: it does not run, and the turn does not end.
    let waited = Instant::now();
    while waited.elapsed() < Duration::from_millis(1200) {
        match run.events.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::ToolEnded { .. } | Event::TurnEnded { .. }) => panic!("it went on without an answer"),
            _ => {}
        }
    }
    assert_eq!(provider.requests(), 3);
    approval.answer.send((Decision::Reject, None)).unwrap();
    let (log, cancelled) = until_end(&run, Decision::Reject);
    assert!(!cancelled && log.iter().any(|line| line.starts_with("tool:false:true:")), "{log:?}");
}

/// A real command, through the sandbox: it ends, and the session wakes with what it printed.
#[cfg(windows)]
#[test]
fn a_real_background_command_wakes_the_session_when_it_ends() {
    let dir = tempfile::tempdir().unwrap();
    let (run, provider) = scripted(dir.path(), vec![
        calls(&[("execute_command", json!({"command": "echo finished-in-background", "background": true}))]),
        says("It is running; I will hear when it ends."),
        says("It ended."),
    ], Mode::Auto);
    run.handle.commands.send(Command::Send("run it in the background".into())).unwrap();
    until_end(&run, Decision::Approve);
    let wakes = until_woken(&run);
    assert_eq!((wakes[0].cause, wakes[0].id.as_str(), wakes[0].exit_code), (WakeCause::CommandEnded, "job1", Some(0)));
    until_end(&run, Decision::Approve);
    let text = the_notice_in(&provider, 2).content.unwrap();
    assert!(text.contains("job1 ended") && text.contains("finished-in-background") && text.contains("exit code 0"), "{text}");
}
