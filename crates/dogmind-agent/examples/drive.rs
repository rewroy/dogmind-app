//! Drives the agent against a server with no window, to measure a job: `drive --server URL --workspace DIR --data DIR --turns FILE [--mode auto] [--limit SECONDS]`; `drive --report TRAJECTORY [--engine-log FILE]` reads a trajectory as `dogmind-app --trajectory` does.
//!
//! The turns are the lines of the file (a blank line is skipped), each sent when the one before it has ended. Approvals are given, as a person at the keyboard would who trusts the job.
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
    let (mut server, mut workspace, mut data, mut mode, mut turns, mut limit) = (String::new(), PathBuf::new(), PathBuf::new(), "auto".to_string(), PathBuf::new(), 1800u64);
    let (mut report, mut engine_log) = (None, None);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--report" => report = args.next().map(PathBuf::from),
            "--engine-log" => engine_log = args.next().map(PathBuf::from),
            "--server" => server = args.next().unwrap_or_default(),
            "--workspace" => workspace = PathBuf::from(args.next().unwrap_or_default()),
            "--data" => data = PathBuf::from(args.next().unwrap_or_default()),
            "--mode" => mode = args.next().unwrap_or_default(),
            "--turns" => turns = PathBuf::from(args.next().unwrap_or_default()),
            "--limit" => limit = args.next().and_then(|text| text.parse().ok()).unwrap_or(limit),
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
    let mut client = Client::new(&server, None).expect("an address");
    if let Ok(Health::Loading(_)) = client.health() {
        panic!("the server is loading: wait and run again");
    }
    let info = client.describe().expect("the server answers");
    client.replay_reasoning = info.straydog;
    client.suppresses_letters = info.suppresses_letters;
    println!("server: model {} window {} straydog {}", info.model, info.context, info.straydog);

    let (sender, events) = channel();
    let config = Config {
        workspace: workspace.clone(),
        sessions: data.join("sessions"),
        rules: data.join("permissions.json"),
        mode,
        effort: None,
        web: WebSettings::default(),
        language: "auto".into(),
        japanese_system: false,
        instructions: None,
        home: None,
        skills_off: Vec::new(),
        memory: None,
        mcp: None,
    };
    let handle = agent::start(config, sender, Box::new(|| {}));
    let send = |command: Command| {
        let _ = handle.commands.send(command);
    };
    send(Command::Provider(Arc::new(client), info.model.clone(), info.context));
    send(Command::NewSession(workspace.clone()));
    let began = Instant::now();
    for (at, turn) in std::fs::read_to_string(&turns).unwrap().lines().map(str::trim).filter(|line| !line.is_empty()).enumerate() {
        let turn_began = Instant::now();
        println!("--- turn {} ({} chars)", at + 1, turn.chars().count());
        send(Command::Send(turn.to_string()));
        let (mut approvals, mut tools, mut failed) = (0, 0, None);
        loop {
            if began.elapsed() > Duration::from_secs(limit) {
                println!("time limit: stopping");
                handle.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                send(Command::Quit);
                return;
            }
            let Ok(event) = events.recv_timeout(Duration::from_secs(1)) else { continue };
            match event {
                Event::Approval(approval) => {
                    approvals += 1;
                    println!("  approve {} ({})", approval.tool, approval.summary.chars().take(80).collect::<String>());
                    let _ = approval.answer.send((Decision::Approve, None));
                }
                Event::Question(question) => {
                    println!("  question: {}", question.question);
                    let _ = question.answer.send(question.options.first().map(|(label, _)| label.clone()).unwrap_or_default());
                }
                Event::PlanReview(review) => {
                    let _ = review.answer.send(Ok(()));
                }
                Event::ToolStarted { id, tool, summary } => {
                    tools += 1;
                    println!("  tool {id} {tool}: {}", summary.chars().take(100).collect::<String>());
                }
                Event::ToolEnded { id, ok, elapsed, output, .. } => println!("  ended {id} ok={ok} {:.1}s {} bytes", elapsed.as_secs_f64(), output.len()),
                Event::Failed(message) => {
                    println!("  FAILED: {message}");
                    failed = Some(message);
                }
                Event::Notice(text) => println!("  notice: {text}"),
                Event::TurnEnded { cancelled } => {
                    println!("turn {} ended: {:.1}s, cancelled={cancelled}, tools={tools}, approvals={approvals}, failed={}", at + 1, turn_began.elapsed().as_secs_f64(), failed.is_some());
                    break;
                }
                _ => {}
            }
        }
    }
    println!("=== {:.1}s in all", began.elapsed().as_secs_f64());
    send(Command::Quit);
    std::thread::sleep(Duration::from_millis(500));
    for entry in std::fs::read_dir(data.join("sessions")).into_iter().flatten().flatten() {
        println!("kept: {}", entry.path().display());
    }
}
