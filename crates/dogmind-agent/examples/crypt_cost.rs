//! What encrypting the conversations costs this PC: a data folder of test sessions of a real size is made in a folder of the caller's own, copied, the copy is encrypted, and the same things
//! are timed on both: unlocking, opening the largest session, appending a line, listing and searching every session. It never reads the app's own folder, and holds at most a few hundred MiB.
//!
//! `cargo run --release -p dogmind-agent --example crypt_cost -- <empty folder> [sessions] [total MiB] [largest MiB]`
//! `cargo run --release -p dogmind-agent --example crypt_cost -- make <empty folder> [sessions] [total MiB] [largest MiB] [workspace]` only makes the folder of test sessions.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dogmind_agent::conversation_key::{Keystore, Policy};
use dogmind_agent::conversion::{self, Hooks};
use dogmind_agent::message::{FunctionCall, Message, ToolCall};
use dogmind_agent::session::{self, Session};

/// A small deterministic generator: the same folder every time, and nothing of anyone's words in it.
struct Words(u64);

impl Words {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn text(&mut self, bytes: usize) -> String {
        const VOCABULARY: [&str; 24] = ["build", "cargo", "folder", "session", "model", "answer", "window", "function", "return", "buffer", "ハッシュ", "テーブル", "会話", "暗号", "ファイル", "確認", "fn", "let", "match", "error", "warning", "path", "line", "token"];
        let mut out = String::with_capacity(bytes + 16);
        while out.len() < bytes {
            let word = VOCABULARY[(self.next() % VOCABULARY.len() as u64) as usize];
            out.push_str(word);
            out.push(if self.next() % 11 == 0 { '\n' } else { ' ' });
            if self.next() % 97 == 0 {
                out.push_str(&format!("{:08x} ", self.next() as u32));
            }
        }
        out
    }
}

fn call(id: &str) -> ToolCall {
    ToolCall { id: id.into(), kind: "function".into(), function: FunctionCall { name: "read_file".into(), arguments: "{\"path\":\"src/main.rs\"}".into() } }
}

/// A session of about `bytes` bytes: turns of a question, an answer that reads a file, the file, and an answer.
fn make_session(folder: &Path, workspace: &Path, stamp: &str, bytes: usize, words: &mut Words) -> PathBuf {
    let mut session = Session::create(folder, workspace, "2026-11-07 12:00:00 +09:00", stamp).expect("a session is made");
    session.set_tools(&serde_json::json!([{"function": {"name": "read_file", "description": words.text(3000)}}])).unwrap();
    session.append(&Message::text("system", words.text(9000))).unwrap();
    let mut written = 12_000;
    let mut turn = 0;
    while written < bytes {
        turn += 1;
        let ask = Message::text("user", dogmind_agent::prompt::user_message(&words.text(300), "2026-11-07 12:00:00 +09:00"));
        let asking = Message { role: "assistant".into(), tool_calls: Some(vec![call(&format!("c{turn}"))]), ..Message::default() };
        let long = 6000 + (words.next() % 20_000) as usize;
        let read = Message::tool_result(&format!("c{turn}"), words.text(long));
        let answer = Message::text("assistant", words.text(1500));
        for message in [ask, asking, read, answer] {
            written += serde_json::to_string(&message).map_or(0, |line| line.len());
            session.append(&message).unwrap();
        }
    }
    session.path.clone()
}

/// Test sessions of sizes that fall off like a real folder's: a few large, many small.
fn make_folder(data: &Path, workspace: &Path, count: usize, total: usize, largest: usize) {
    let mut words = Words(0x9E37_79B9_7F4A_7C15);
    let sessions = data.join("sessions");
    let mut sizes: Vec<f64> = (0..count).map(|at| 1.0 / (1.0 + at as f64).powf(0.9)).collect();
    let sum: f64 = sizes.iter().sum();
    for size in &mut sizes {
        *size = (*size / sum * total as f64).max(20_000.0);
    }
    sizes[0] = largest as f64;
    for (at, size) in sizes.iter().enumerate() {
        make_session(&sessions, workspace, &format!("20261107-{:06}", at), *size as usize, &mut words);
    }
    let memory = data.join("memory");
    std::fs::create_dir_all(&memory).unwrap();
    std::fs::write(memory.join("user.md"), "- the person prefers short answers\n- answers in Japanese when asked in Japanese\n").unwrap();
    std::fs::write(memory.join("project-0123456789abcdef.md"), "- builds with cargo, tests with cargo test --release\n").unwrap();
}

fn copy_all(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_all(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn size_of(folder: &Path) -> u64 {
    std::fs::read_dir(folder).into_iter().flatten().flatten().map(|entry| if entry.path().is_dir() { size_of(&entry.path()) } else { entry.metadata().map_or(0, |meta| meta.len()) }).sum()
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

fn timed(runs: usize, mut work: impl FnMut()) -> Duration {
    median((0..runs).map(|_| {
        let began = Instant::now();
        work();
        began.elapsed()
    }).collect())
}

fn ms(time: Duration) -> String {
    if time < Duration::from_millis(1) { format!("{:.1} us", time.as_secs_f64() * 1e6) } else { format!("{:.2} ms", time.as_secs_f64() * 1000.0) }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let only_make = args.first().is_some_and(|first| first == "make");
    if only_make {
        args.remove(0);
    }
    let Some(work) = args.first().map(PathBuf::from) else {
        eprintln!("usage: crypt_cost [make] <empty folder> [sessions] [total MiB] [largest MiB]");
        std::process::exit(2);
    };
    let number = |at: usize, default: usize| args.get(at).and_then(|text| text.parse().ok()).unwrap_or(default);
    let (count, total, largest) = (number(1, 200), number(2, 150) << 20, number(3, 24) << 20);
    let plain = if only_make { work.clone() } else { work.join("plain") };
    assert!(!plain.exists() || std::fs::read_dir(&plain).is_ok_and(|mut folder| folder.next().is_none()), "the folder is to be empty: nothing in it is overwritten");
    let workspace = args.get(4).map_or_else(|| PathBuf::from("C:/work/test"), PathBuf::from);
    make_folder(&plain, &workspace, count, total, largest);
    println!("{count} test sessions, {:.1} MiB in all", size_of(&plain) as f64 / 1048576.0);
    if only_make {
        return;
    }
    let sealed = work.join("sealed");
    copy_all(&plain, &sealed);
    let policy = Policy::off();
    let began = Instant::now();
    let (_, _, report) = conversion::turn_on(&sealed, &policy, "a passphrase of the measurement", true, &mut Hooks::default()).expect("the copy is encrypted");
    println!("encrypting all of it, once: {:.2} s ({} files); {:.1} MiB after", began.elapsed().as_secs_f64(), report.changed, size_of(&sealed) as f64 / 1048576.0);

    // the largest session, by file size
    let mut files: Vec<(u64, PathBuf)> = session::list(&plain.join("sessions")).into_iter().map(|summary| (std::fs::metadata(&summary.path).map_or(0, |meta| meta.len()), summary.path)).collect();
    files.sort();
    let (largest_size, largest_plain) = files.pop().expect("there are sessions");
    let largest_sealed = sealed.join("sessions").join(largest_plain.file_name().unwrap());
    println!("largest session: {:.1} MiB plain, {:.1} MiB encrypted", largest_size as f64 / 1048576.0, std::fs::metadata(&largest_sealed).map_or(0, |meta| meta.len()) as f64 / 1048576.0);

    println!("\n{:<44}{:>14}{:>14}", "median of the runs", "plain", "encrypted");
    let row = |name: &str, plain: Duration, sealed: Duration| println!("{name:<44}{:>14}{:>14}", ms(plain), ms(sealed));
    // unlocking: only the encrypted folder has a key file; the passphrase pays scrypt, the copy for this PC pays DPAPI
    let keystore = Keystore::load(&sealed).unwrap();
    println!("{:<44}{:>14}{:>14}", "unlock with the passphrase (scrypt)", "-", ms(timed(7, || drop(keystore.unlock("a passphrase of the measurement").unwrap()))));
    println!("{:<44}{:>14}{:>14}", "unlock with the copy for this PC (DPAPI)", "-", ms(timed(7, || drop(keystore.unlock_remembered().unwrap()))));
    row("open the largest session", timed(3, || drop(Session::open(&largest_plain).unwrap())), timed(3, || drop(Session::open(&largest_sealed).unwrap())));
    row("read the largest session (no append)", timed(3, || drop(session::read(&largest_plain).unwrap())), timed(3, || drop(session::read(&largest_sealed).unwrap())));
    // appending a line: on copies, so that the folders measured are as they were made
    let mut line_words = Words(7);
    let line = Message::text("assistant", line_words.text(1500));
    let append = |path: &Path| {
        let (mut open, _, _) = Session::open(path).unwrap();
        median((0..300).map(|_| {
            let began = Instant::now();
            open.append(&line).unwrap();
            began.elapsed()
        }).collect())
    };
    let (copy_plain, copy_sealed) = (work.join("append-plain.jsonl"), work.join("append-sealed.jsonl"));
    std::fs::copy(&largest_plain, &copy_plain).unwrap();
    std::fs::copy(&largest_sealed, &copy_sealed).unwrap();
    row("append a line of 1.5 KB (300 lines)", append(&copy_plain), append(&copy_sealed));
    row("list every session", timed(3, || drop(session::list(&plain.join("sessions")))), timed(3, || drop(session::list(&sealed.join("sessions")))));
    row("search every session (no session has it)", timed(3, || drop(session::search(&plain.join("sessions"), "ハッシュ deadbeef"))), timed(3, || drop(session::search(&sealed.join("sessions"), "ハッシュ deadbeef"))));
    row("search every session (every session has it)", timed(3, || drop(session::search(&plain.join("sessions"), "cargo"))), timed(3, || drop(session::search(&sealed.join("sessions"), "cargo"))));
    let _ = std::fs::remove_file(copy_plain);
    let _ = std::fs::remove_file(copy_sealed);
}
