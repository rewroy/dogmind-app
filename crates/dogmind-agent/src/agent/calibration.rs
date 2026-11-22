//! What the draft stop's rule does on real reasoning: the rule as it was, and the rule as it is, over every finished answer's reasoning in the sessions of a folder. Not a test that
//! passes or fails: `DOGMIND_CALIBRATE=<folder of *-data> cargo test -p dogmind-agent calibrate_the_draft_stop -- --ignored --nocapture` prints the table it was set from. The sessions
//! are only read, and nothing of them is kept here.
//!
//! A true draft is a reasoning that has a good many lines of code of its own, of which at least half are in what the same answer then wrote with `create_file` or `apply_patch`: it
//! typed the file twice. (A reasoning that was stopped is not in a session: only what it had thought before its code is, in the note. The answers that were not stopped are.)

use std::collections::BTreeSet;
use std::ops::Bound;
use std::path::Path;

use super::*;

/// The rule as it was: lines of code of its own counted over the whole reasoning, and the second fenced block of its own.
mod old {
    use super::*;

    const DRAFT_FENCES: usize = 3;
    const DRAFT_LINES: usize = 10;

    #[derive(Default)]
    struct Block {
        lines: usize,
        known: usize,
        quoted: usize,
    }

    impl Block {
        fn is_new(known: usize, quoted: usize) -> bool {
            known == 0 || quoted * 4 < known * 3
        }
    }

    pub struct DraftWatch {
        shown: BTreeSet<String>,
        line_starts: usize,
        code_lines: usize,
        after_quote: bool,
        marks: usize,
        block: Option<Block>,
    }

    impl DraftWatch {
        pub fn new(shown: BTreeSet<String>) -> DraftWatch {
            DraftWatch { shown, line_starts: 0, code_lines: 0, after_quote: false, marks: 0, block: None }
        }

        fn quote(&self, line: &str) -> Option<bool> {
            (line.len() >= QUOTE_MIN).then(|| self.shown.contains(line))
        }

        fn begins_a_quote(&self, part: &str) -> bool {
            self.shown.range::<str, _>((Bound::Included(part), Bound::Unbounded)).next().is_some_and(|line| line.starts_with(part))
        }

        fn whole_line(&mut self, line: &str) {
            let marks = line.matches("```").count();
            if marks > 0 {
                for _ in 0..marks {
                    match self.block.take() {
                        None => self.block = Some(Block::default()),
                        Some(done) => self.marks += if Block::is_new(done.known, done.quoted) { 2 } else { 0 },
                    }
                }
                return;
            }
            let line = line.trim();
            let quote = self.quote(line);
            if let (Some(block), false) = (&mut self.block, line.is_empty()) {
                block.lines += 1;
                if let Some(quoted) = quote {
                    block.known += 1;
                    block.quoted += quoted as usize;
                }
            }
            if is_code_line(line) && !quote.unwrap_or(self.after_quote) {
                self.code_lines += 1;
            }
            if let Some(quoted) = quote {
                self.after_quote = quoted;
            }
        }

        fn open_block_is_new(&self, unfinished: &str) -> bool {
            let Some(block) = &self.block else { return false };
            let part = if unfinished.contains("```") { "" } else { unfinished.trim() };
            let (mut known, mut quoted) = (block.known, block.quoted);
            if !part.is_empty() {
                if self.begins_a_quote(part) {
                    (known, quoted) = (known + 1, quoted + 1);
                } else if part.len() >= QUOTE_MIN {
                    known += 1;
                }
            }
            (block.lines > 0 || !part.is_empty()) && Block::is_new(known, quoted)
        }

        pub fn is_a_draft(&mut self, reasoned: &str) -> bool {
            if let Some(end) = reasoned.rfind('\n').filter(|end| *end >= self.line_starts) {
                for line in reasoned[self.line_starts..end].split('\n') {
                    self.whole_line(line);
                }
                self.line_starts = end + 1;
            }
            let marks = self.marks + usize::from(self.open_block_is_new(&reasoned[self.line_starts..]));
            self.code_lines >= DRAFT_LINES || marks >= DRAFT_FENCES
        }
    }
}

/// What an answer wrote with `create_file` and `apply_patch`: the content, and what each edit put in.
fn written(message: &Message) -> String {
    let mut out = String::new();
    for call in message.tool_calls.iter().flatten().filter(|call| matches!(call.function.name.as_str(), "create_file" | "apply_patch")) {
        let Ok(arguments) = serde_json::from_str::<Value>(&call.function.arguments) else { continue };
        out.push_str(arguments.get("content").and_then(Value::as_str).unwrap_or(""));
        out.push('\n');
        for edit in arguments.get("edits").and_then(Value::as_array).into_iter().flatten() {
            out.push_str(edit.get("new_string").and_then(Value::as_str).unwrap_or(""));
            out.push('\n');
        }
    }
    out
}

struct Row {
    session: String,
    at: usize,
    chars: usize,
    true_draft: bool,
    wrote: bool,
    old: bool,
    /// The new rule's run was long enough, and whether the app would be watching: the person had not said that no file is to change.
    new: bool,
    watched: bool,
    longest: usize,
}

fn rows_of(root: &Path) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut folders: Vec<_> = std::fs::read_dir(root).unwrap().flatten().map(|entry| entry.path()).filter(|path| path.file_name().is_some_and(|name| name.to_string_lossy().ends_with("-data"))).collect();
    folders.sort();
    for folder in folders {
        let tag = folder.file_name().unwrap().to_string_lossy().trim_end_matches("-data").to_string();
        let mut files: Vec<_> = std::fs::read_dir(folder.join("sessions")).into_iter().flatten().flatten().map(|entry| entry.path()).filter(|path| path.extension().is_some_and(|ext| ext == "jsonl") && !crate::trajectory::is_trajectory(path) && !crate::session::is_helper(path)).collect();
        files.sort();
        for file in files {
            // (a session that does not open, an encrypted one whose key is not unlocked here among them, is said, and not left out of the count without a word)
            let messages = match crate::session::read(&file) {
                Ok(messages) => messages,
                Err(error) => {
                    eprintln!("not read: {error}");
                    continue;
                }
            };
            for (at, message) in messages.iter().enumerate() {
                let Some(reasoning) = message.reasoning_content.as_deref().filter(|text| message.role == "assistant" && text.chars().count() > 200) else { continue };
                let shown = lines_shown(&messages[..at]);
                let own: BTreeSet<&str> = reasoning.lines().map(str::trim).filter(|line| is_code_line(line) && line.len() >= QUOTE_MIN && !shown.contains(*line)).collect();
                let wrote = written(message);
                let put: BTreeSet<&str> = wrote.lines().map(str::trim).collect();
                let true_draft = !wrote.trim().is_empty() && own.len() >= 8 && own.iter().filter(|line| put.contains(*line)).count() * 2 >= own.len();
                let (mut old, mut new) = (old::DraftWatch::new(shown.clone()), DraftWatch::new(shown, DRAFT_LINES));
                let (old_fires, new_fires) = (old.is_a_draft(reasoning), new.is_a_draft(reasoning));
                let asked = messages[..at].iter().rev().find(|message| message.role == "user" && !message.content.as_deref().is_some_and(prompt::is_nudge)).and_then(|message| message.content.as_deref()).unwrap_or("");
                let watched = !prompt::forbids_changes(prompt::typed_text(asked));
                rows.push(Row { session: tag.clone(), at, chars: reasoning.chars().count(), true_draft, wrote: !wrote.trim().is_empty(), old: old_fires, new: new_fires, watched, longest: new.stats() });
            }
        }
    }
    rows
}

#[test]
#[ignore = "reads the sessions of the folder DOGMIND_CALIBRATE names"]
fn calibrate_the_draft_stop() {
    let Some(root) = std::env::var_os("DOGMIND_CALIBRATE") else {
        eprintln!("DOGMIND_CALIBRATE names no folder: nothing to read");
        return;
    };
    let rows = rows_of(Path::new(&root));
    println!("{} reasonings over 200 characters, DRAFT_LINES {DRAFT_LINES}, PROSE_BETWEEN {PROSE_BETWEEN}", rows.len());
    let count = |keep: &dyn Fn(&Row) -> bool| rows.iter().filter(|row| keep(row)).count();
    println!("true drafts (a file written from its own lines): {}", count(&|row| row.true_draft));
    println!("old rule: stops {} (of true drafts {}, of the others {})", count(&|row| row.old), count(&|row| row.old && row.true_draft), count(&|row| row.old && !row.true_draft));
    println!("old rule, of its stops: in a request where the person said no file is to change: {}", count(&|row| row.old && !row.watched));
    println!("new rule: stops {} (of true drafts {}, of the others {})", count(&|row| row.new && row.watched), count(&|row| row.new && row.watched && row.true_draft), count(&|row| row.new && row.watched && !row.true_draft));
    println!("new rule, the run alone, whatever the person said: {} (of true drafts {})", count(&|row| row.new), count(&|row| row.new && row.true_draft));
    println!("{:24} {:>4} {:>7} {:>7} {:>4} {:>4} {:>4} {:>7} {:>4}", "session", "msg", "chars", "longest", "true", "wrote", "old", "watched", "new");
    for row in rows.iter().filter(|row| row.old || row.new || row.true_draft) {
        println!("{:24} {:>4} {:>7} {:>7} {:>4} {:>4} {:>4} {:>7} {:>4}", row.session, row.at, row.chars, row.longest, u8::from(row.true_draft), u8::from(row.wrote), u8::from(row.old), u8::from(row.watched), u8::from(row.new && row.watched));
    }
}


/// The sizes of a draft the stop could be set at, below and at `DRAFT_LINES`.
const STOP_AT: [usize; 7] = [20, 30, 40, 50, 60, 80, 100];

/// Where, in characters of the reasoning, its run of code of its own first reached each of [`STOP_AT`]: the point a stop at that size would have
/// fired, fed as it streams, a line at a time. None where it never did.
fn reached(shown: BTreeSet<String>, reasoning: &str) -> [Option<usize>; 7] {
    let mut watch = DraftWatch::new(shown, DRAFT_LINES);
    let mut at = [None; 7];
    let mut end = 0;
    for line in reasoning.split_inclusive('\n') {
        end += line.len();
        watch.is_a_draft(&reasoning[..end]);
        for (slot, size) in at.iter_mut().zip(STOP_AT) {
            if slot.is_none() && watch.stats() >= size {
                *slot = Some(end);
            }
        }
    }
    at
}

#[test]
#[ignore = "reads the sessions of the folder DOGMIND_CALIBRATE names"]
fn calibrate_where_a_draft_reaches_each_size() {
    let Some(root) = std::env::var_os("DOGMIND_CALIBRATE") else {
        eprintln!("DOGMIND_CALIBRATE names no folder: nothing to read");
        return;
    };
    // One line a reasoning, tab-separated: what it is, its length, its longest run, whether it was a true draft, whether the person allowed changes, where
    // the run began, and where it reached each size (- for never).
    println!("session\tmsg\tchars\tlongest\ttrue\twatched\tbegan\t{}", STOP_AT.map(|size| format!("at{size}")).join("\t"));
    let mut folders: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().map(|entry| entry.path()).filter(|path| path.file_name().is_some_and(|name| name.to_string_lossy().ends_with("-data"))).collect();
    folders.sort();
    for folder in folders {
        let tag = folder.file_name().unwrap().to_string_lossy().trim_end_matches("-data").to_string();
        let mut files: Vec<_> = std::fs::read_dir(folder.join("sessions")).into_iter().flatten().flatten().map(|entry| entry.path()).filter(|path| path.extension().is_some_and(|ext| ext == "jsonl") && !crate::trajectory::is_trajectory(path) && !crate::session::is_helper(path)).collect();
        files.sort();
        for file in files {
            let Ok(messages) = crate::session::read(&file) else { continue };
            let name = format!("{tag}/{}", file.file_stem().unwrap().to_string_lossy());
            for (at, message) in messages.iter().enumerate() {
                let Some(reasoning) = message.reasoning_content.as_deref().filter(|text| message.role == "assistant" && text.chars().count() > 200) else { continue };
                let shown = lines_shown(&messages[..at]);
                let own: BTreeSet<&str> = reasoning.lines().map(str::trim).filter(|line| is_code_line(line) && line.len() >= QUOTE_MIN && !shown.contains(*line)).collect();
                let wrote = written(message);
                let put: BTreeSet<&str> = wrote.lines().map(str::trim).collect();
                let true_draft = !wrote.trim().is_empty() && own.len() >= 8 && own.iter().filter(|line| put.contains(*line)).count() * 2 >= own.len();
                let asked = messages[..at].iter().rev().find(|message| message.role == "user" && !message.content.as_deref().is_some_and(prompt::is_nudge)).and_then(|message| message.content.as_deref()).unwrap_or("");
                let watched = !prompt::forbids_changes(prompt::typed_text(asked));
                let mut whole = DraftWatch::new(shown.clone(), DRAFT_LINES);
                whole.is_a_draft(&format!("{reasoning}\n"));
                let at_sizes = reached(shown, reasoning);
                let cell = |value: Option<usize>| value.map_or("-".to_string(), |chars| chars.to_string());
                println!("{name}\t{at}\t{}\t{}\t{}\t{}\t{}\t{}", reasoning.len(), whole.stats(), u8::from(true_draft), u8::from(watched), whole.began_at(), at_sizes.map(cell).join("\t"));
            }
        }
    }
}

/// The note a stop sends, in any of its forms: the stop's own turn goes on after it.
fn is_stop_note(text: &str) -> bool {
    text.starts_with(prompt::DRAFT_NOTE) && text.contains("in your reasoning") && text.to_ascii_lowercase().contains("write the code now")
}

/// The line a draft is stopped at after a stop in the same turn (one lower than the first stop's, against today's `LONG_DRAFT_LINES + 1`): every request after a stop,
/// with whether a watch at `LOWER` and one at `LONG_DRAFT_LINES + 1` fires, where in its reasoning (characters) each does, whether a file was written since the stop,
/// and what the request was: stopped itself (a kept reasoning, its note after it), a true draft (its own code mostly in what it then wrote), or neither.
#[test]
#[ignore = "reads the sessions of the folder DOGMIND_CALIBRATE names"]
fn calibrate_the_line_after_a_stop() {
    const LOWER: usize = 60;
    let Some(root) = std::env::var_os("DOGMIND_CALIBRATE") else {
        eprintln!("DOGMIND_CALIBRATE names no folder: nothing to read");
        return;
    };
    let fires_at = |shown: &BTreeSet<String>, reasoning: &str, line: usize| -> Option<usize> {
        let mut watch = DraftWatch::new(shown.clone(), line);
        let mut end = 0;
        for piece in reasoning.split_inclusive('\n') {
            end += piece.len();
            if watch.is_a_draft(&reasoning[..end]) {
                return Some(end);
            }
        }
        watch.is_a_draft(&format!("{reasoning}\n")).then_some(reasoning.len())
    };
    println!("session\tmsg\tchars\tkind\twrote_since\tat{LOWER}\tat{}", LONG_DRAFT_LINES + 1);
    let (mut seen, mut lower, mut long) = (0, [0usize; 3], [0usize; 3]);
    let mut folders: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().map(|entry| entry.path()).filter(|path| path.file_name().is_some_and(|name| name.to_string_lossy().ends_with("-data"))).collect();
    folders.sort();
    for folder in folders {
        let tag = folder.file_name().unwrap().to_string_lossy().trim_end_matches("-data").to_string();
        let mut files: Vec<_> = std::fs::read_dir(folder.join("sessions")).into_iter().flatten().flatten().map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl") && !path.to_string_lossy().contains(".trajectory.") && !path.to_string_lossy().contains("helper")).collect();
        files.sort();
        for file in files {
            let Ok(messages) = crate::session::read(&file) else { continue };
            let name = format!("{tag}/{}", file.file_stem().unwrap().to_string_lossy());
            let (mut stopped, mut wrote_since) = (false, false);
            for (at, message) in messages.iter().enumerate() {
                if message.role == "user" {
                    let text = message.content.as_deref().unwrap_or("");
                    if is_stop_note(text) {
                        (stopped, wrote_since) = (true, false);
                    } else if !text.starts_with("[From the app") {
                        // (the person's next message: a turn of its own, with no stop in it yet)
                        (stopped, wrote_since) = (false, false);
                    }
                    continue;
                }
                if message.role != "assistant" {
                    continue;
                }
                let wrote = written(message);
                if let (true, Some(reasoning)) = (stopped, message.reasoning_content.as_deref().filter(|text| text.chars().count() > 200)) {
                    let shown = lines_shown(&messages[..at]);
                    let own: BTreeSet<&str> = reasoning.lines().map(str::trim).filter(|line| is_code_line(line) && line.len() >= QUOTE_MIN && !shown.contains(*line)).collect();
                    let put: BTreeSet<&str> = wrote.lines().map(str::trim).collect();
                    let stopped_itself = messages.get(at + 1).is_some_and(|next| next.role == "user" && is_stop_note(next.content.as_deref().unwrap_or("")));
                    let true_draft = !wrote.trim().is_empty() && own.len() >= 8 && own.iter().filter(|line| put.contains(*line)).count() * 2 >= own.len();
                    let kind = if stopped_itself { 0 } else if true_draft { 1 } else { 2 };
                    let (at_lower, at_long) = (fires_at(&shown, reasoning, LOWER), fires_at(&shown, reasoning, LONG_DRAFT_LINES + 1));
                    seen += 1;
                    lower[kind] += usize::from(at_lower.is_some());
                    long[kind] += usize::from(at_long.is_some());
                    let cell = |value: Option<usize>| value.map_or("-".to_string(), |chars| chars.to_string());
                    println!("{name}\t{at}\t{}\t{}\t{}\t{}\t{}", reasoning.len(), ["stopped", "true draft", "other"][kind], u8::from(wrote_since), cell(at_lower), cell(at_long));
                }
                if !wrote.trim().is_empty() {
                    wrote_since = true;
                }
            }
        }
    }
    println!("requests after a stop with a reasoning over 200 characters: {seen}");
    println!("a watch at {LOWER} fires: stopped {}, true drafts {}, others {}", lower[0], lower[1], lower[2]);
    println!("a watch at {} fires: stopped {}, true drafts {}, others {}", LONG_DRAFT_LINES + 1, long[0], long[1], long[2]);
}
