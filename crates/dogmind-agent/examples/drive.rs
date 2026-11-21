//! Drives the agent against a server with no window, to measure a job: `drive --server URL --workspace DIR --data DIR --turns FILE [--mode auto] [--limit SECONDS] [--wake-for SECONDS] [--refuse] [--warm-wait SECONDS] [--between SECONDS [--warm-between]] [--key-file FILE] [--code-mode]`; `drive --report TRAJECTORY [--engine-log FILE]` reads a trajectory as `dogmind-app --trajectory` does.
//!
//! The turns are the lines of the file (a blank line is skipped), each sent when the one before it has ended. Approvals are given, as a person at the keyboard would who trusts the job;
//! with `--refuse` every approval is answered no, so that no command runs. A call being written is shown as its pieces come, with the time into the turn.
//! `--code-mode` offers the model `run_code`, a short program that calls the tools; the calls it makes are lines of the trajectory with the program's call as their `parent`.
//! `--wake-for` goes on after the last turn for as long as the session has a background command or a wait, up to that many seconds, and shows the turns it wakes itself for.
//! The session, and the trajectory beside it (`dogmind-app --trajectory`), are in `--data\sessions`. It never touches the app's own data folder.

use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dogmind_agent::agent::{self, Command, Config, Decision, Event};
use dogmind_agent::llm::{Client, Health};
use dogmind_agent::safety::Mode;
use dogmind_agent::web::WebSettings;

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut server, mut workspace, mut data, mut mode, mut turns, mut limit, mut wake_for) = (String::new(), PathBuf::new(), PathBuf::new(), "auto".to_string(), PathBuf::new(), 1800u64, 0u64);
    let (mut report, mut engine_log) = (None, None);
    // `--refuse`: every approval is answered no, for a job that must run no command (nothing the sandbox starts runs then).
    let mut refuse = false;
    // `--warm-wait SECONDS`: the new conversation is warmed as the app warms one it opens, and its first message goes that long after.
    let mut warm_wait = 0u64;
    // `--between SECONDS`: before each turn after the first, that long passes, as while the person is in another conversation; with
    // `--warm-between`, another conversation is opened on the same server meanwhile, and warmed as the app warms one it opens.
    let (mut between, mut warm_between) = (0u64, false);
    // `--key-file FILE`: the server's key, read from the file (its first line) and sent as the app sends one; never printed.
    let mut key_file: Option<PathBuf> = None;
    // `--code-mode`: the model is offered `run_code`.
    let mut code_mode = false;
    // `--effort LEVEL`: the thinking level the app sends (`reasoning_effort`), as the person's setting would; none sends nothing.
    let mut effort: Option<String> = None;
    // `--instructions FILE`, `--language LANG`, `--skills-off a,b`: the person's own instructions, answer language and skills turned off, as their settings
    // give them, to run a job as they would.
    let (mut instructions, mut language, mut skills_off): (Option<PathBuf>, String, Vec<String>) = (None, "auto".to_string(), Vec::new());
    // `--web`: web search and reading are offered, as with the person's setting on.
    let mut web = false;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--refuse" => refuse = true,
            "--code-mode" => code_mode = true,
            "--effort" => effort = args.next().filter(|level| !level.is_empty()),
            "--instructions" => instructions = args.next().map(PathBuf::from),
            "--web" => web = true,
            "--language" => language = args.next().unwrap_or_else(|| "auto".to_string()),
            "--skills-off" => skills_off = args.next().unwrap_or_default().split(',').map(str::trim).filter(|name| !name.is_empty()).map(str::to_string).collect(),
            "--warm-wait" => warm_wait = args.next().and_then(|text| text.parse().ok()).unwrap_or(0),
            "--between" => between = args.next().and_then(|text| text.parse().ok()).unwrap_or(0),
            "--warm-between" => warm_between = true,
            "--key-file" => key_file = args.next().map(PathBuf::from),
            "--report" => report = args.next().map(PathBuf::from),
            "--engine-log" => engine_log = args.next().map(PathBuf::from),
            "--server" => server = args.next().unwrap_or_default(),
            "--workspace" => workspace = PathBuf::from(args.next().unwrap_or_default()),
            "--data" => data = PathBuf::from(args.next().unwrap_or_default()),
            "--mode" => mode = args.next().unwrap_or_default(),
            "--turns" => turns = PathBuf::from(args.next().unwrap_or_default()),
            "--limit" => limit = args.next().and_then(|text| text.parse().ok()).unwrap_or(limit),
            "--wake-for" => wake_for = args.next().and_then(|text| text.parse().ok()).unwrap_or(wake_for),
            other => panic!("unknown flag {other}"),
        }
    }
    // `--report TRAJECTORY [--engine-log FILE]`: how a run went, as `dogmind-app --trajectory` tells it.
    if let Some(trajectory) = report {
        println!("{}", dogmind_agent::trajectory_report::report_files(&trajectory, engine_log.as_deref()).expect("a trajectory"));
        return;
    }
    assert!(!server.is_empty() && workspace.is_dir() && !data.as_os_str().is_empty() && turns.is_file(), "--server, --workspace (a folder that is there), --data and --turns are needed");
    std::fs::create_dir_all(&data).unwrap();
    let mode = match mode.as_str() {
        "default" => Mode::Default,
        "accept_edits" => Mode::AcceptEdits,
        _ => Mode::Auto,
    };
    let key = key_file.map(|file| std::fs::read_to_string(&file).expect("the key file").lines().next().unwrap_or("").trim().to_string()).filter(|key| !key.is_empty());
    let mut client = Client::new(&server, key).expect("an address");
    if let Ok(Health::Loading(_)) = client.health() {
        panic!("the server is loading: wait and run again");
    }
    let info = client.describe().expect("the server answers");
    client.replay_reasoning = info.straydog;
    client.suppresses_letters = info.suppresses_letters;
    client.tool_writing = info.tool_writing;
    client.image_digests = info.image_digests;
    client.app_role = info.app_role();
    client.vision = info.vision;
    println!("server: model {} window {} straydog {}", info.model, info.context, info.straydog);

    let (sender, events) = channel();
    let config_in = |data: &std::path::Path| Config {
        workspace: workspace.clone(),
        sessions: data.join("sessions"),
        rules: data.join("permissions.json"),
        mode,
        effort: effort.clone(),
        web: WebSettings { enabled: web, ..WebSettings::default() },
        language: language.clone(),
        japanese_system: false,
        instructions: instructions.clone(),
        home: None,
        skills_off: skills_off.clone(),
        skills_on: Vec::new(),
        memory: None,
        mcp: None,
        policy: Default::default(),
        code_mode,
    };
    let handle = agent::start(config_in(&data), sender, Box::new(|| {}));
    let send = |command: Command| {
        let _ = handle.commands.send(command);
    };
    let provider = Arc::new(client);
    send(Command::Provider(provider.clone(), info.model.clone(), info.context));
    send(Command::NewSession(workspace.clone()));
    if warm_wait > 0 {
        send(Command::WarmPrefix);
        println!("warmed; the first message goes in {warm_wait} s");
        std::thread::sleep(Duration::from_secs(warm_wait));
    }
    let began = Instant::now();
    for (at, turn) in std::fs::read_to_string(&turns).unwrap().lines().map(str::trim).filter(|line| !line.is_empty()).enumerate() {
        if at > 0 && between > 0 {
            if warm_between {
                let (other_sender, other_events) = channel();
                let other = agent::start(config_in(&data.join("between")), other_sender, Box::new(|| {}));
                let _ = other.commands.send(Command::Provider(provider.clone(), info.model.clone(), info.context));
                let _ = other.commands.send(Command::NewSession(workspace.clone()));
                let _ = other.commands.send(Command::WarmPrefix);
                println!("another conversation opened and warmed; back to this one in {between} s");
                std::thread::sleep(Duration::from_secs(between));
                let _ = other.commands.send(Command::Quit);
                drop(other_events);
            } else {
                println!("back to this conversation in {between} s");
                std::thread::sleep(Duration::from_secs(between));
            }
        }
        let turn_began = Instant::now();
        println!("--- turn {} ({} chars)", at + 1, turn.chars().count());
        send(Command::Send(turn.to_string()));
        let (mut approvals, mut tools, mut failed) = (0, 0, None);
        let mut seen = Seen { began: turn_began, refuse, ..Seen::default() };
        loop {
            if began.elapsed() > Duration::from_secs(limit) {
                println!("time limit: stopping");
                handle.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                send(Command::Quit);
                return;
            }
            let Ok(event) = events.recv_timeout(Duration::from_secs(1)) else { continue };
            if let Some(cancelled) = show(event, &mut tools, &mut approvals, &mut failed, &mut seen) {
                seen.summary();
                println!("turn {} ended: {:.1}s, cancelled={cancelled}, tools={tools}, approvals={approvals}, failed={}", at + 1, turn_began.elapsed().as_secs_f64(), failed.is_some());
                break;
            }
        }
    }
    // The turns the session wakes itself for: what a background command or a wait has to say, with nobody sending anything.
    if wake_for > 0 {
        let waiting_since = Instant::now();
        let (mut tools, mut approvals, mut failed, mut woken_turns) = (0, 0, None, 0);
        let mut turn_began = Instant::now();
        let mut seen = Seen { began: turn_began, refuse, ..Seen::default() };
        let mut in_a_turn = false;
        while waiting_since.elapsed() < Duration::from_secs(wake_for) {
            if !in_a_turn && handle.background.snapshot().is_empty() && !handle.background.has_pending() {
                println!("nothing is left going or waiting");
                break;
            }
            let Ok(event) = events.recv_timeout(Duration::from_secs(1)) else { continue };
            if matches!(event, Event::Woken(_)) {
                in_a_turn = true;
                woken_turns += 1;
                turn_began = Instant::now();
                (tools, approvals, failed) = (0, 0, None);
                seen = Seen { began: turn_began, refuse, ..Seen::default() };
            }
            if let Some(cancelled) = show(event, &mut tools, &mut approvals, &mut failed, &mut seen) {
                seen.summary();
                in_a_turn = false;
                println!("woken turn {woken_turns} ended: {:.1}s, cancelled={cancelled}, tools={tools}, approvals={approvals}, failed={}", turn_began.elapsed().as_secs_f64(), failed.is_some());
            }
        }
        println!("woken turns: {woken_turns}");
    }
    println!("=== {:.1}s in all", began.elapsed().as_secs_f64());
    send(Command::Quit);
    std::thread::sleep(Duration::from_millis(500));
    for entry in std::fs::read_dir(data.join("sessions")).into_iter().flatten().flatten() {
        println!("kept: {}", entry.path().display());
    }
}

/// What a turn has shown so far of the calls being written: when it began, and the pieces of each call.
struct Seen {
    began: Instant,
    refuse: bool,
    /// Requests the turn has made so far.
    requests: usize,
    /// When the first reasoning or answer text of the request in hand came.
    first_text: Option<f64>,
    /// (request, call index, tool, pieces, characters, seconds into the turn of the first and of the last piece)
    writing: Vec<(usize, usize, String, u32, usize, f64, f64)>,
}

impl Default for Seen {
    fn default() -> Seen {
        Seen { began: Instant::now(), refuse: false, requests: 0, first_text: None, writing: Vec::new() }
    }
}

impl Seen {
    fn at(&self) -> f64 {
        self.began.elapsed().as_secs_f64()
    }

    /// One line a call being written: how many pieces came, how much, from when to when.
    fn summary(&self) {
        for (request, index, tool, pieces, chars, first, last) in &self.writing {
            println!("  written in request {request}, call {index} {tool}: {pieces} pieces, {chars} chars, from {first:.1}s to {last:.1}s");
        }
    }
}

/// Says what an event is, and answers what asks, as a person who trusts the job would (or, with `--refuse`, as one who lets nothing
/// run). Some(cancelled) when it is the end of a turn.
fn show(event: Event, tools: &mut u32, approvals: &mut u32, failed: &mut Option<String>, seen: &mut Seen) -> Option<bool> {
    match event {
        Event::Approval(approval) => {
            *approvals += 1;
            let decision = if seen.refuse { Decision::Reject } else { Decision::Approve };
            let from = approval.program.as_ref().map_or(String::new(), |program| format!(" from a program: {}", program.chars().take(60).collect::<String>()));
            println!("  {} {} ({}){from}", if seen.refuse { "refuse" } else { "approve" }, approval.tool, approval.summary.chars().take(80).collect::<String>());
            let _ = approval.answer.send((decision, None));
        }
        // (a piece of a call the model is still writing: when it came and how much it was)
        Event::ToolWriting { index, tool, piece } => {
            let (at, request) = (seen.at(), seen.requests);
            let chars = piece.chars().count();
            let total = match seen.writing.iter_mut().find(|call| call.0 == request && call.1 == index && call.2 == tool) {
                Some(call) => {
                    call.3 += 1;
                    call.4 += chars;
                    call.6 = at;
                    call.4
                }
                None => {
                    seen.writing.push((request, index, tool.clone(), 1, chars, at, at));
                    chars
                }
            };
            println!("  writing [{at:.1}s] call {index} {tool}: +{chars} chars, {total} in all");
        }
        Event::RequestStarted => {
            seen.requests += 1;
            seen.first_text = None;
            println!("  request {} [{:.1}s]", seen.requests, seen.at());
        }
        Event::Reasoning(_) | Event::Content(_) if seen.first_text.is_none() => {
            let at = seen.at();
            seen.first_text = Some(at);
            println!("  first text of request {} [{at:.1}s]", seen.requests);
        }
        Event::Question(question) => {
            println!("  question: {}", question.question);
            let _ = question.answer.send(question.options.first().map(|(label, _)| label.clone()).unwrap_or_default());
        }
        Event::PlanReview(review) => {
            let _ = review.answer.send(Ok(()));
        }
        Event::ToolStarted { id, tool, summary } => {
            *tools += 1;
            println!("  tool [{:.1}s] {id} {tool}: {}", seen.at(), summary.chars().take(100).collect::<String>());
        }
        Event::ToolEnded { id, ok, elapsed, output, .. } => println!("  ended {id} ok={ok} {:.1}s {} bytes", elapsed.as_secs_f64(), output.len()),
        Event::Failed(message) => {
            println!("  FAILED: {message}");
            *failed = Some(message);
        }
        Event::Notice(text) => println!("  notice: {text}"),
        Event::Woken(wakes) => {
            for wake in wakes {
                println!("--- woken by {} ({}): {}{}", wake.id, wake.cause.name(), wake.what, wake.exit_code.map_or(String::new(), |code| format!(", exit code {code}")));
            }
        }
        Event::Waiting(waiting) => {
            println!("  waiting: {} commands, {} waits{}", waiting.commands.len(), waiting.waits.len(), waiting.paused.map_or(String::new(), |paused| format!(", paused: {}", paused.sentence())));
        }
        Event::TurnEnded { cancelled } => return Some(cancelled),
        _ => {}
    }
    None
}
