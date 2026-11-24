//! Reads a trajectory (`trajectory`) and says how the run went: each request and each tool, where the time was, what the app's own guards cost, and whether the run was going round in circles.
//! `dogmind-app --trajectory <file> [--engine-log <json>]` prints it.
//!
//! With the engine's log (as `GET /dashboard/logs` gives it, `{"lines": [...]}`) it also puts each request beside the engine's own line for it: by the id the app sent (`Straydog request: id
//! <req>, ...`), and, for a server that does not log ids yet, by time and the size of the prompt.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
use std::path::Path;

use chrono::{DateTime, FixedOffset, Utc};
use regex::Regex;
use serde_json::Value;

/// How many failed calls in a row, or how many of one call, is worth a line.
const FAILED_IN_A_ROW: usize = 3;

/// These two tools complete successfully only after the person answers.
pub(crate) fn answered_by_person(tool: &str, outcome: &str) -> bool {
    outcome == "ok" && matches!(tool, "ask_user" | "submit_plan")
}

/// One line of a trajectory, and when it was written.
pub(crate) struct Line {
    pub(crate) v: Value,
    pub(crate) at: Option<DateTime<FixedOffset>>,
}

impl Line {
    pub(crate) fn ev(&self) -> &str {
        self.v["ev"].as_str().unwrap_or("")
    }
    pub(crate) fn s(&self, key: &str) -> &str {
        self.v[key].as_str().unwrap_or("")
    }
    pub(crate) fn u(&self, key: &str) -> Option<u64> {
        self.v[key].as_u64()
    }
    fn estimated_tokens(&self) -> Option<u64> {
        // Older writers recorded zero for these requests even though they had no stream meter.
        self.u("tokens_est").filter(|&n| n != 0 || !matches!(self.s("role"), "warm" | "helper" | "checkpoint"))
    }
    fn written_tokens(&self) -> Option<String> {
        self.u("completion").map(|n| n.to_string()).or_else(|| self.estimated_tokens().map(|n| format!("~{n}")))
    }
    fn cache_percent(&self) -> Option<u64> {
        crate::llm::cache_percent(self.u("prompt")?, self.u("cached"))
    }
    /// The request table uses a counted speed only if the provider did not report one.
    fn table_rate(&self, generation_ms: u64) -> (Option<f64>, bool) {
        if let Some(rate) = crate::trajectory::request_rate(&self.v) {
            return (Some(rate), false);
        }
        let counted = self.u("completion").filter(|_| generation_ms > 0).map(|tokens| tokens as f64 * 1000.0 / generation_ms as f64);
        (counted, counted.is_some())
    }
    pub(crate) fn turn(&self) -> u64 {
        self.u("turn").unwrap_or(0)
    }
    pub(crate) fn req(&self) -> &str {
        self.s("req")
    }
    /// A command can run successfully as a tool call yet return a failing exit code.
    fn tool_outcome(&self) -> &str {
        match self.s("outcome") {
            "ok" if self.v["exit_code"].as_i64().is_some_and(|code| code != 0) => "failed",
            outcome => outcome,
        }
    }

    fn failed(&self) -> bool {
        self.tool_outcome() != "ok"
    }

    fn is_file_edit(&self) -> bool {
        matches!(self.s("name"), "create_file" | "apply_patch" | "edit")
    }

    fn target_paths(&self) -> impl Iterator<Item = &str> {
        let edited = self.v["edited"].as_array().into_iter().flatten().filter_map(|entry| entry["path"].as_str());
        self.v["path"].as_str().into_iter().chain(edited).filter(|path| !path.is_empty())
    }
}

fn secs(ms: u64) -> String {
    format!("{:.1} s", ms as f64 / 1000.0)
}

fn share(part: u64, whole: u64) -> String {
    if whole == 0 { "-".to_string() } else { format!("{}%", part * 100 / whole) }
}

/// A request's id without its session: `/12`.
fn short(req: &str) -> String {
    req.rsplit_once('/').map_or(req.to_string(), |(_, number)| format!("/{number}"))
}

/// What the engine said of a request, from its log line.
#[derive(Debug, Clone, Default)]
pub struct EngineLine {
    pub at: Option<DateTime<Utc>>,
    pub id: Option<String>,
    pub failed: bool,
    pub prompt: u64,
    pub reused: u64,
    pub from: String,
    pub new: u64,
    pub prefill_ms: u64,
    pub first_s: f64,
    pub tokens: u64,
    pub tokens_per_second: f64,
    pub drafts: Option<(u64, u64)>,
}

/// The engine's request lines in a log: the one JSON of `/dashboard/logs`, or lines of text.
pub fn engine_lines(log: &str) -> Vec<EngineLine> {
    let texts: Vec<String> = match serde_json::from_str::<Value>(log) {
        Ok(value) => value.get("lines").and_then(Value::as_array).map(|lines| lines.iter().filter_map(|line| line.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        Err(_) => log.lines().map(str::to_string).collect(),
    };
    let done = Regex::new(r"^(\S+?)Z?\s+\w+\s+Straydog request: (?:id (\S+), )?prompt (\d+) tokens \(reused (\d+) from ([\w-]+)(?:, the same as before for \d+)?\)(?:, \d+ images? \([\d.]+ s\))?, new (\d+) in (\d+) ms(?: \([\d.]+ tok/s\))?, first token ([\d.]+) s.*?, (\d+) tokens at ([\d.]+) tok/s(?: \((\d+) of (\d+) drafts kept)?").expect("a pattern");
    let failed = Regex::new(r"^(\S+?)Z?\s+\w+\s+Straydog request failed: id (\S+?),").expect("a pattern");
    let time = |text: &str| DateTime::parse_from_rfc3339(&format!("{text}Z")).ok().map(|at| at.with_timezone(&Utc));
    texts.iter().filter_map(|text| {
        if let Some(found) = done.captures(text) {
            let number = |at: usize| found.get(at).and_then(|m| m.as_str().parse::<u64>().ok()).unwrap_or(0);
            return Some(EngineLine {
                at: time(&found[1]),
                id: found.get(2).map(|m| m.as_str().to_string()),
                failed: false,
                prompt: number(3),
                reused: number(4),
                from: found[5].to_string(),
                new: number(6),
                prefill_ms: number(7),
                first_s: found[8].parse().unwrap_or(0.0),
                tokens: number(9),
                tokens_per_second: found[10].parse().unwrap_or(0.0),
                drafts: found.get(11).map(|kept| (kept.as_str().parse().unwrap_or(0), number(12))),
            });
        }
        let found = failed.captures(text)?;
        Some(EngineLine { at: time(&found[1]), id: Some(found[2].to_string()), failed: true, ..EngineLine::default() })
    }).collect()
}

/// Which engine line goes with each request line of the trajectory (by its index among all lines), and how it was found.
fn join<'a>(requests: &[(usize, &Line)], engine: &'a [EngineLine]) -> HashMap<usize, (&'a EngineLine, &'static str)> {
    let mut joined = HashMap::new();
    let mut used = vec![false; engine.len()];
    for (at, line) in requests {
        if let Some(found) = engine.iter().position(|entry| entry.id.as_deref() == Some(line.req())) {
            used[found] = true;
            joined.insert(*at, (&engine[found], "id"));
        }
    }
    // A server that logs no id: the entry that ended nearest this request's end, within a few seconds, and of the same prompt where the app knows it.
    for (at, line) in requests {
        if joined.contains_key(at) || line.u("prompt").is_none() {
            continue;
        }
        let Some(ended) = line.at.map(|at| at.with_timezone(&Utc)) else { continue };
        let best = engine.iter().enumerate()
            .filter(|(found, entry)| !used[*found] && entry.id.is_none() && !entry.failed && Some(entry.prompt) == line.u("prompt"))
            .filter_map(|(found, entry)| Some((found, (entry.at? - ended).num_milliseconds().unsigned_abs())))
            .filter(|(_, apart)| *apart <= 5000).min_by_key(|(_, apart)| *apart);
        if let Some((found, _)) = best {
            used[found] = true;
            joined.insert(*at, (&engine[found], "time"));
        }
    }
    joined
}

/// The lines of a trajectory that can be read, and how many could not.
pub(crate) fn load(trajectory: &str) -> (Vec<Line>, usize) {
    let mut bad = 0;
    let lines = trajectory.lines().filter(|text| !text.trim().is_empty()).filter_map(|text| match serde_json::from_str::<Value>(text) {
        Ok(v) => {
            let at = v["t"].as_str().and_then(|t| DateTime::parse_from_rfc3339(t).ok());
            Some(Line { v, at })
        }
        Err(_) => {
            bad += 1;
            None
        }
    }).collect();
    (lines, bad)
}

/// Connections and warm-up before the first turn have no turn of their own.
fn lines_by_turn(lines: &[Line]) -> BTreeMap<u64, Vec<&Line>> {
    let mut turns: BTreeMap<u64, Vec<&Line>> = BTreeMap::new();
    for line in lines {
        let turn = line.turn();
        if turn != 0 {
            turns.entry(turn).or_default().push(line);
        }
    }
    turns
}

/// What a request cost and did, in a line: used in the interventions, for the request after one.
fn request_summary(line: &Line) -> String {
    let cached = line.cache_percent().map_or_else(|| "cached ?".to_string(), |value| format!("cached {value}%"));
    let calls: Vec<&str> = line.v["calls"].as_array().map(|calls| calls.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    format!("{} wrote {} tokens in {}, {cached}, ended {}{}", short(line.req()), line.written_tokens().unwrap_or_else(|| "?".into()), secs(line.u("ms").unwrap_or(0)), line.s("end"), if calls.is_empty() { String::new() } else { format!(", called {}", calls.join(", ")) })
}

/// The report for a trajectory, and with the engine's log (as text) the engine's lines beside the requests.
pub fn report(trajectory: &str, engine_log: Option<&str>) -> String {
    let (lines, bad) = load(trajectory);
    let mut out = String::new();
    let (first, last) = (lines.iter().find_map(|line| line.at), lines.iter().rev().find_map(|line| line.at));
    let span = match (first, last) {
        (Some(first), Some(last)) => format!("{} to {} ({})", first.format("%Y-%m-%d %H:%M:%S%.3f %:z"), last.format("%H:%M:%S%.3f"), secs((last - first).num_milliseconds().max(0) as u64)),
        _ => "no times".to_string(),
    };
    let _ = writeln!(out, "{} lines, {span}{}", lines.len(), if bad > 0 { format!(", {bad} that could not be read") } else { String::new() });
    let engine: Vec<EngineLine> = engine_log.map(engine_lines).unwrap_or_default();
    let requests: Vec<(usize, &Line)> = lines.iter().enumerate().filter(|(_, line)| line.ev() == "request").collect();
    let joined = join(&requests, &engine);

    // Turns, each with where its time went.
    let turns = lines_by_turn(&lines);
    let mut total = Spent::default();
    for (turn, in_turn) in &turns {
        let spent = Spent::of(in_turn);
        let began = in_turn.iter().find(|line| line.ev() == "turn_start");
        let shown = match began {
            // (a turn nobody sent: what woke it is under `wakes`)
            Some(began) if began.v["woken"] == true => format!("turn {turn}: woken by the harness, with no word from the person"),
            Some(began) => format!("turn {turn}: {} characters, {} pictures", began.u("chars").unwrap_or(0), began.u("images").unwrap_or(0)),
            None => format!("turn {turn}"),
        };
        let _ = writeln!(out, "\n{shown}");
        spent.print(&mut out, " (no turn_end recorded, timed through the last turn event)");
        total.add(&spent);
    }
    if turns.len() > 1 {
        let _ = writeln!(out, "\nall turns");
        total.print(&mut out, " (unfinished turns timed through their last turn event)");
    }

    let connections: Vec<&Line> = lines.iter().filter(|line| line.ev() == "connection").collect();
    if !connections.is_empty() {
        let _ = writeln!(out, "\nconnections");
        for line in connections {
            let at = line.at.map_or(String::new(), |at| format!(" ending at {}", at.format("%H:%M:%S%.3f")));
            let _ = writeln!(out, "  {} took {}, {}{at}", line.s("what"), secs(line.u("ms").unwrap_or(0)), line.s("outcome"));
        }
    }

    let _ = writeln!(out, "\nrequests (first = time to the first word; gen = the rest; ~ before tokens means estimated; tok/s is the server's where it said, else counted)");
    for (at, line) in &requests {
        let (ms, first) = (line.u("ms").unwrap_or(0), line.u("first_ms"));
        let generation = first.map_or(ms, |first| ms.saturating_sub(first));
        let completion = line.u("completion");
        let (rate, counted) = line.table_rate(generation);
        let rate = rate.map_or_else(|| "-".to_string(), |rate| if counted { format!("~{rate:.1}") } else { format!("{rate:.1}") });
        let cached = line.cache_percent().map_or_else(|| "-".to_string(), |value| format!("{value}%"));
        let calls: Vec<&str> = line.v["calls"].as_array().map(|calls| calls.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let _ = writeln!(out, "  {:>5} {:<10} first {:>8} gen {:>8} prompt {:>6} cached {:>4} wrote {:>6} tok at {:>6} tok/s  {}{}{}{}",
            short(line.req()), line.s("role"), first.map_or("-".to_string(), secs), secs(generation), line.u("prompt").map_or("-".to_string(), |tokens| tokens.to_string()), cached,
            line.written_tokens().unwrap_or_else(|| "-".into()), rate, line.s("end"), if calls.is_empty() { String::new() } else { format!(" [{}]", calls.join(", ")) },
            match line.v["retry_of"].as_str() { Some(of) => format!("  (repeats {} after {}, {})", short(of), secs(line.u("wait_ms").unwrap_or(0)), line.s("retry_why")), None => String::new() },
            match line.v["helper_file"].as_str() { Some(file) => format!("  (the helper's steps are in {file})"), None => String::new() });
        if let Some(server) = line.v.get("server").filter(|server| server.is_object()) {
            // The app's first word minus the server's own time to its first token is a wait before the server began: the request was behind another's.
            if let (Some(first_ms), Some(ttft)) = (first.map(|first| first as f64), server.get("ttft_seconds").and_then(Value::as_f64)) {
                // A call comes whole when the server has written it, so for an answer that was only a call the app's first word is the end of the generation, not the first token: the time
                // the server took to write it (its tokens at its speed) is not a wait. It is taken where the app saw much less of the generation than the server says it made.
                let after_ms = generation as f64;
                let made_ms = completion.zip(crate::trajectory::request_rate(&line.v)).map(|(tokens, rate)| tokens as f64 * 1000.0 / rate);
                let unseen_ms = made_ms.filter(|made| after_ms * 2.0 < *made).map_or(0.0, |made| made - after_ms);
                let queued = (first_ms - ttft * 1000.0 - unseen_ms) / 1000.0;
                // (no speed from the server to take its writing from the wait: the first word of an answer that was only a call comes at the end of it)
                let only_a_call = !calls.is_empty() && line.u("reasoning_chars") == Some(0) && line.u("content_chars") == Some(0);
                if made_ms.is_none() && only_a_call && after_ms < 1000.0 && first_ms - ttft * 1000.0 >= 1000.0 {
                    let _ = writeln!(out, "        queued at the server: cannot be told (the answer was only a call, which the server sends whole once it has written it, and it did not say how long that took)");
                } else if queued >= 1.0 && unseen_ms > 0.0 {
                    let _ = writeln!(out, "        queued at the server: {queued:.1} s (the app's first word came after {:.1} s, when the server had written the answer, which took it {:.1} s after its first token {ttft:.2} s after it began)", first_ms / 1000.0, unseen_ms / 1000.0);
                } else if queued >= 1.0 {
                    let _ = writeln!(out, "        queued at the server: {queued:.1} s (the app's first word came after {:.1} s, the server's own first token {ttft:.2} s after it began)", first_ms / 1000.0);
                }
            }
            let pick = |key: &str| server.get(key).and_then(Value::as_f64).map(|value| format!("{key} {value:.2}"));
            let said: Vec<String> = ["prefill_seconds", "ttft_seconds", "request_seconds", "decode_tokens_per_second", "predicted_per_second"].iter().filter_map(|key| pick(key)).chain(server.get("resumed_from").and_then(Value::as_str).map(|from| format!("resumed_from {from}"))).collect();
            if !said.is_empty() {
                let _ = writeln!(out, "        server said: {}", said.join(", "));
            }
        }
        if let Some((entry, how)) = joined.get(at) {
            // (the engine writes its line as the request ends, and the app writes its own when it has read the stream's last word)
            let apart = line.at.zip(entry.at).map_or(String::new(), |(ours, theirs)| format!(", the app's line is {:+} ms from the engine's (the two machines' clocks differ a little)", (ours.with_timezone(&Utc) - theirs).num_milliseconds()));
            if entry.failed {
                let _ = writeln!(out, "        engine ({how}): request failed{apart}");
            } else {
                let drafts = entry.drafts.map_or(String::new(), |(kept, tried)| format!(", drafts {kept}/{tried}"));
                let _ = writeln!(out, "        engine ({how}): prompt {}, reused {} from {}, prefilled {} in {} ms, first token {:.2} s, {} tokens at {:.1} tok/s{drafts}{apart}", entry.prompt, entry.reused, entry.from, entry.new, entry.prefill_ms, entry.first_s, entry.tokens, entry.tokens_per_second);
            }
        } else if !engine.is_empty() && line.s("role") != "warm" {
            let _ = writeln!(out, "        engine: no line for this request");
        }
    }
    if !engine.is_empty() {
        let by_id = joined.values().filter(|(_, how)| *how == "id").count();
        let by_time = joined.len() - by_id;
        let matched: std::collections::HashSet<*const EngineLine> = joined.values().map(|(entry, _)| *entry as *const EngineLine).collect();
        let (from, to) = (first.map(|at| at.with_timezone(&Utc)), last.map(|at| at.with_timezone(&Utc)));
        let others = engine.iter().filter(|entry| !matched.contains(&(*entry as *const EngineLine)) && matches!((entry.at, from, to), (Some(at), Some(from), Some(to)) if at >= from - chrono::Duration::seconds(5) && at <= to + chrono::Duration::seconds(5))).count();
        let _ = writeln!(out, "  {} of {} requests joined to the engine's log: {by_id} by id, {by_time} by time; {others} other engine lines in this run's time", joined.len(), requests.len());
    }

    // Turns that nobody sent: what woke each, how long after it happened the turn began, and the request the turn made first.
    let wakes: Vec<&Line> = lines.iter().filter(|line| line.ev() == "wake").collect();
    if !wakes.is_empty() {
        let _ = writeln!(out, "\nwakes (turns nobody sent)");
        for line in wakes {
            let cause = match line.s("cause") {
                "command_ended" => "ended",
                "command_line" => "printed the line it was to wake for",
                "wait" => "is up",
                other => other,
            };
            let program = line.v["cmd0"].as_str().map_or(String::new(), |program| format!(" ({program})"));
            let together = line.u("together").filter(|count| *count > 1).map_or(String::new(), |count| format!(", with {} others in the same turn", count - 1));
            let _ = writeln!(out, "  {}{program} {cause}: turn {} began {} after, its first request {}{together}", line.s("id"), line.turn(), secs(line.u("after_ms").unwrap_or(0)), short(line.req()));
        }
    }

    let _ = writeln!(out, "\ntools");
    for line in lines.iter().filter(|line| line.ev() == "tool") {
        let more = tool_notes(line);
        let _ = writeln!(out, "  {:>5} {:<16} {:>8}  {:<15} {}", short(line.req()), line.s("name"), secs(line.u("ms").unwrap_or(0)), line.tool_outcome(), more.join(", "));
    }

    let guards: Vec<usize> = lines.iter().enumerate().filter(|(_, line)| matches!(line.ev(), "intervention" | "compaction")).map(|(at, _)| at).collect();
    let _ = writeln!(out, "\ninterventions");
    if guards.is_empty() {
        let _ = writeln!(out, "  none");
    }
    for at in guards {
        let line = &lines[at];
        if line.ev() == "compaction" {
            let window = line.u("window").map_or(String::new(), |window| format!(", of a window of {window}"));
            let _ = writeln!(out, "  compaction ({}): {} tokens to {} in {}{window}", line.s("stage"), line.u("before_tokens").unwrap_or(0), line.u("after_tokens").unwrap_or(0), secs(line.u("ms").unwrap_or(0)));
            continue;
        }
        let kind = line.s("kind");
        let stopped = lines.iter().find(|request| request.ev() == "request" && request.req() == line.req());
        let mut what = match kind {
            "draft_stop" => {
                let spent = stopped.and_then(|request| request.u("ms")).map_or(String::new(), |ms| format!(" in {}", secs(ms)));
                // Older writers counted code lines and fence marks instead of runs and lines inside fences.
                let counts: Vec<_> = [
                    line.u("run").map(|n| format!("longest run of {n} lines of code")),
                    line.u("fenced").map(|n| format!("{n} lines across fenced blocks")),
                    line.u("code_lines").map(|n| format!("{n} lines of code in all")),
                    line.u("fences").map(|n| format!("{n} fence marks in all")),
                ].into_iter().flatten().collect();
                let shape = if counts.is_empty() { "line counts unavailable".to_string() } else { counts.join(", ") };
                let tokens = line.u("tokens_before").map_or_else(|| "?".to_string(), |n| format!("~{n}"));
                let kept = match line.v["reasoning_kept"].as_bool() {
                    Some(true) => ", reasoning kept",
                    Some(false) => ", reasoning discarded",
                    None => "",
                };
                format!("{tokens} tokens generated{spent} before the stop ({shape}{kept})")
            }
            "cut_write" => format!("{} bytes of {} cut ({}), after {} tokens", line.u("bytes").unwrap_or(0), line.s("path"), line.s("why"), line.u("tokens_before").unwrap_or(0)),
            "cost_note" => format!("{} tokens generated", line.u("generated").unwrap_or(0)),
            _ => line.v.as_object().map(|fields| fields.iter().filter(|(key, _)| !matches!(key.as_str(), "t" | "ev" | "turn" | "req" | "kind")).map(|(key, value)| format!("{key} {value}")).collect::<Vec<_>>().join(", ")).unwrap_or_default(),
        };
        if what.is_empty() {
            what = "-".to_string();
        }
        let _ = writeln!(out, "  {kind} at {}: {what}", short(line.req()));
        let next = lines[at + 1..].iter()
            .take_while(|later| !matches!(later.ev(), "turn_start" | "turn_end"))
            .find(|later| later.turn() == line.turn() && later.ev() == "request" && later.s("role") == "main");
        if let Some(next) = next {
            let _ = writeln!(out, "      next: {}", request_summary(next));
        }
    }

    // How the turns that failed ended, with the one line the server said of it where it said any.
    let failures: Vec<&Line> = lines.iter().filter(|line| line.ev() == "failed").collect();
    if !failures.is_empty() {
        let _ = writeln!(out, "\nfailures");
        for line in failures {
            let said = line.s("server_said");
            let _ = writeln!(out, "  turn {} at {}: {}{}", line.turn(), short(line.req()), line.s("kind"), if said.is_empty() { String::new() } else { format!(", the server said: {said}") });
        }
    }

    circles(&mut out, &lines);
    out
}

/// What a tool's line says besides its name, time and outcome: its file or program, the exit code, the bytes, the wait for the person, a command's parts, a browser's runs.
fn tool_notes(line: &Line) -> Vec<String> {
    let mut more = Vec::new();
    let mut seen = HashSet::new();
    for path in line.target_paths() {
        if seen.insert(path) {
            more.push(path.to_string());
        }
    }
    if let Some(program) = line.v["cmd0"].as_str() {
        more.push(program.to_string());
    }
    if let Some(code) = line.v["exit_code"].as_i64().filter(|code| *code != 0) {
        more.push(format!("exit {code}"));
    }
    more.push(format!("{} bytes out", line.u("out_bytes").unwrap_or(0)));
    if let Some(cut) = line.u("out_cut_bytes") {
        more.push(format!("{cut} of them cut"));
    }
    let approval = line.u("wait_ms");
    let answered = answered_by_person(line.s("name"), line.s("outcome"));
    if approval.is_some() || answered {
        let answer = if answered { line.u("ms").unwrap_or(0) } else { 0 };
        more.push(format!("{} waiting for the person", secs(approval.unwrap_or(0).saturating_add(answer))));
    }
    // (a command's time apart from the program's: what the app does before it starts and after it ends)
    if let Some(prepare) = line.u("prepare_ms") {
        let part = |key: &str, name: &str| line.u(key).filter(|ms| *ms > 0).map(|ms| format!("{name} {}", secs(ms)));
        let parts: Vec<String> = [part("grant_ms", "grants"), part("repos_ms", "repositories"), part("toolchains_ms", "toolchains"), part("env_ms", "environment")].into_iter().flatten().collect();
        more.push(format!("prepared in {}{}, ran {}, looked after in {}", secs(prepare), if parts.is_empty() { String::new() } else { format!(" ({})", parts.join(", ")) }, secs(line.u("run_ms").unwrap_or(0)), secs(line.u("after_ms").unwrap_or(0))));
    }
    if let Some(runs) = line.v["browser_runs"].as_array() {
        let ended: Vec<String> = runs.iter().map(|run| format!("{} {} in {}", run["of"].as_str().unwrap_or("?"), run["end"].as_str().unwrap_or("?"), secs(run["ms"].as_u64().unwrap_or(0)))).collect();
        more.push(format!("{} browser runs: {}", runs.len(), ended.join("; ")));
    }
    more
}

/// 3rd, 4th, 21st: the count as an English ordinal.
fn ordinal(count: usize) -> String {
    let suffix = match (count % 10, count % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{count}{suffix}")
}

/// Where the time of some lines went.
#[derive(Default)]
struct Spent {
    wall: u64,
    prefill: u64,
    generation: u64,
    helpers: u64,
    // Helper request time already included in completed explore calls.
    covered_helpers: u64,
    tools: BTreeMap<String, (u64, u64)>,
    // Time also present in a recorded parent call, retained in the per-tool breakdown.
    nested_ms: u64,
    // Time shared by parallel calls in one request, also retained in their individual rows.
    parallel_overlap_ms: u64,
    waits: u64,
    compaction: u64,
    retries: u64,
    unfinished: bool,
}

impl Spent {
    fn of(lines: &[&Line]) -> Spent {
        let mut spent = Spent::default();
        let ended = lines.iter().find(|line| line.ev() == "turn_end");
        spent.wall = ended.and_then(|end| end.u("ms")).unwrap_or_else(|| {
            // Reconnecting or warming the engine between turns can still carry the preceding turn's number.
            let mut timed = lines.iter()
                .filter(|line| line.ev() != "connection" && !(line.ev() == "request" && line.s("role") == "warm"))
                .filter_map(|line| line.at);
            match (timed.next(), timed.next_back()) {
                (Some(first), Some(last)) => (last - first).num_milliseconds().max(0) as u64,
                _ => 0,
            }
        });
        spent.unfinished = lines.iter().any(|line| line.ev() == "turn_start") && ended.is_none();
        // Children finish first. Match their parent within this request, even if its row comes later.
        let mut pending_helpers = 0;
        let mut parallel = HashMap::<&str, Vec<(i64, i64)>>::new();
        let tool_ids: std::collections::HashSet<_> = lines.iter().filter(|line| line.ev() == "tool")
            .filter_map(|line| Some((line.req(), line.v["id"].as_str()?))).collect();
        for line in lines {
            match line.ev() {
                "request" => {
                    let (ms, first) = (line.u("ms").unwrap_or(0), line.u("first_ms").unwrap_or(0).min(line.u("ms").unwrap_or(0)));
                    match line.s("role") {
                        "main" => {
                            pending_helpers = 0;
                            spent.prefill += first;
                            spent.generation += ms - first;
                        }
                        "helper" => {
                            spent.helpers += ms;
                            pending_helpers += ms;
                        }
                        _ => {}
                    }
                    spent.retries += line.u("wait_ms").unwrap_or(0);
                }
                "tool" if answered_by_person(line.s("name"), line.s("outcome")) => {
                    spent.waits += line.u("ms").unwrap_or(0);
                }
                "tool" => {
                    let entry = spent.tools.entry(line.s("name").to_string()).or_default();
                    entry.0 += 1;
                    let ms = line.u("ms").unwrap_or(0);
                    entry.1 += ms;
                    // Explore writes its helper requests before its own completion. If interrupted,
                    // only the requests are present, and their measured time still belongs to tools.
                    if line.s("name") == "explore" {
                        spent.covered_helpers += std::mem::take(&mut pending_helpers).min(ms);
                    }
                    if line.v["parent"].as_str().is_some_and(|parent| tool_ids.contains(&(line.req(), parent))) {
                        spent.nested_ms += ms;
                    } else if line.u("parallel").unwrap_or(0) > 1 && !line.req().is_empty() && line.u("wait_ms").unwrap_or(0) == 0 {
                        if let Ok(started) = DateTime::parse_from_rfc3339(line.s("started")) {
                            let start = started.timestamp_millis();
                            parallel.entry(line.req()).or_default().push((start, start.saturating_add_unsigned(ms)));
                        }
                    }
                }
                "approval" => spent.waits += line.u("wait_ms").unwrap_or(0),
                "compaction" => spent.compaction += line.u("ms").unwrap_or(0),
                _ => {}
            }
        }
        // Completions can be written out of order. Count only the part of each interval already covered.
        for intervals in parallel.values_mut() {
            intervals.sort_unstable();
            let mut covered_until = i64::MIN;
            for &(start, end) in intervals.iter() {
                if start < covered_until {
                    spent.parallel_overlap_ms += start.abs_diff(end.min(covered_until));
                }
                covered_until = covered_until.max(end);
            }
        }
        spent
    }

    fn add(&mut self, other: &Spent) {
        self.wall += other.wall;
        self.unfinished |= other.unfinished;
        self.nested_ms += other.nested_ms;
        self.parallel_overlap_ms += other.parallel_overlap_ms;
        self.prefill += other.prefill;
        self.generation += other.generation;
        self.helpers += other.helpers;
        self.covered_helpers += other.covered_helpers;
        self.waits += other.waits;
        self.compaction += other.compaction;
        self.retries += other.retries;
        for (name, (calls, ms)) in &other.tools {
            let entry = self.tools.entry(name.clone()).or_default();
            entry.0 += calls;
            entry.1 += ms;
        }
    }

    fn tool_ms(&self) -> u64 {
        self.tools.values().map(|(_, ms)| ms).sum::<u64>() - self.nested_ms - self.parallel_overlap_ms
            + (self.helpers - self.covered_helpers)
    }

    fn print(&self, out: &mut String, unfinished_note: &str) {
        let indent = "  ";
        let tools = self.tool_ms();
        let model = self.prefill + self.generation;
        let accounted = model + tools + self.waits + self.compaction + self.retries;
        let rest = self.wall.saturating_sub(accounted);
        let _ = writeln!(out, "{indent}wall {}{}", secs(self.wall), if self.unfinished { unfinished_note } else { "" });
        let _ = writeln!(out, "{indent}model {} ({}): prefill to the first word {}, generation {}", secs(model), share(model, self.wall), secs(self.prefill), secs(self.generation));
        let by_name: Vec<String> = self.tools.iter().map(|(name, (calls, ms))| format!("{name} (calls: {calls}, total: {})", secs(*ms))).collect();
        let nested = if self.nested_ms > 0 { " (individual times include nested calls)" } else { "" };
        let parallel = if self.parallel_overlap_ms > 0 { " (parallel calls overlap)" } else { "" };
        let _ = writeln!(out, "{indent}tools {} ({}){}{nested}{parallel}{}", secs(tools), share(tools, self.wall), if by_name.is_empty() { String::new() } else { format!(": {}", by_name.join(", ")) }, if self.helpers > 0 { format!("; of which an explore helper's own requests {}", secs(self.helpers)) } else { String::new() });
        let _ = writeln!(out, "{indent}waiting for the person {}, compaction {}, waiting to retry {}", secs(self.waits), secs(self.compaction), secs(self.retries));
        let _ = writeln!(out, "{indent}the rest {} ({}): the app's own work, and what is between one step and the next", secs(rest), share(rest, self.wall));
    }
}

/// Signs of a run going round and round: what it did again and got nowhere with.
fn circles(out: &mut String, lines: &[Line]) {
    let _ = writeln!(out, "\nsigns of going round in circles");
    let tools: Vec<&Line> = lines.iter().filter(|line| line.ev() == "tool").collect();
    let mut found = false;
    // Edits to one file that failed: in a row, and in all.
    let mut failed_edits: BTreeMap<&str, (usize, usize, usize)> = BTreeMap::new();
    for line in tools.iter().filter(|line| line.is_file_edit()) {
        if line.failed() {
            let path = line.s("path");
            if path.is_empty() { continue; }
            let entry = failed_edits.entry(path).or_default();
            entry.0 += 1;
            entry.1 = entry.1.max(entry.0);
            entry.2 += 1;
        } else {
            for path in line.target_paths() {
                if let Some(entry) = failed_edits.get_mut(path) {
                    entry.0 = 0;
                }
            }
        }
    }
    for (path, (_, in_a_row, in_all)) in &failed_edits {
        if *in_a_row >= 2 || *in_all >= FAILED_IN_A_ROW {
            found = true;
            let _ = writeln!(out, "  edits to {path} that failed: {in_a_row} in a row, {in_all} in all");
        }
    }
    // A file that came back to a state it had.
    let mut states: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for line in tools.iter().filter(|line| !line.failed()) {
        let edited: Vec<(String, String)> = match line.v["edited"].as_array() {
            Some(list) => list.iter().filter_map(|one| Some((one["path"].as_str()?.to_string(), one["hash"].as_str()?.to_string()))).collect(),
            None => match (line.v["path"].as_str(), line.v["edit_hash"].as_str()) {
                (Some(path), Some(hash)) => vec![(path.to_string(), hash.to_string())],
                _ => Vec::new(),
            },
        };
        for (path, hash) in edited {
            let history = states.entry(path.clone()).or_default();
            if let Some(earlier) = history.iter().position(|(seen, _)| *seen == hash) {
                if earlier + 1 < history.len() {
                    found = true;
                    let _ = writeln!(out, "  {path} came back to the state it had after {} (hash {hash}), at {}", history[earlier].1, short(line.req()));
                }
            }
            history.push((hash, short(line.req())));
        }
    }
    // The same call again: the same tool with the same arguments (by their hash), where nothing was edited since the call before it. A program's calls of one run
    // are its loop's, which the model wrote once (a benchmark of a page, each file of a list): only a call again from another of its steps is going round.
    let mut since_edit: HashMap<(String, String), (usize, Option<String>)> = HashMap::new();
    let mut shown = std::collections::HashSet::new();
    for line in &tools {
        if line.is_file_edit() && !line.failed() {
            since_edit.clear();
            continue;
        }
        let Some(hash) = line.v["args_hash"].as_str() else { continue };
        let parent = line.v["parent"].as_str().map(str::to_string);
        let (count, last) = since_edit.entry((line.s("name").to_string(), hash.to_string())).or_default();
        if parent.is_some() && *last == parent {
            continue;
        }
        *count += 1;
        *last = parent;
        let count = *count;
        if count >= 2 && shown.insert((line.s("name").to_string(), hash.to_string())) {
            found = true;
            let _ = writeln!(out, "  {} called again with the same arguments (hash {hash}), nothing edited between: at {}", line.s("name"), short(line.req()));
        } else if count >= 3 {
            found = true;
            let _ = writeln!(out, "  ... and a {} time at {}", ordinal(count), short(line.req()));
        }
    }
    // Failed calls one after another.
    let (mut run, mut longest, mut at) = (0usize, 0usize, String::new());
    for line in &tools {
        if line.failed() {
            run += 1;
            if run > longest {
                (longest, at) = (run, short(line.req()));
            }
        } else {
            run = 0;
        }
    }
    if longest >= FAILED_IN_A_ROW {
        found = true;
        let _ = writeln!(out, "  {longest} failed calls in a row, the longest run ending at {at}");
    }
    if !found {
        let _ = writeln!(out, "  none");
    }
}

/// A request of a turn, as a row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestRow {
    /// `/12`: its number in the session.
    pub id: String,
    pub role: String,
    /// The time to the first word, where it was told.
    pub first_ms: Option<u64>,
    /// The rest of the request after the first word.
    pub generation_ms: u64,
    pub prompt: Option<u64>,
    /// The share of the prompt the engine had already, in percent.
    pub cached_percent: Option<u64>,
    /// What it wrote, counted where the server did not say.
    pub tokens: Option<u64>,
    pub tokens_estimated: bool,
    /// Tokens a second: the server's where it said, else counted (and then `counted`).
    pub rate: Option<f64>,
    pub counted: bool,
    pub end: String,
    pub calls: Vec<String>,
    /// The request this one repeats, and why.
    pub retry_of: Option<(String, String)>,
    /// For a helper's request: the name of the file beside the session that holds the helper's conversation.
    pub helper_file: Option<String>,
}

/// A tool call of a turn, as a row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolRow {
    pub id: String,
    pub name: String,
    pub ms: u64,
    pub outcome: String,
    /// The file or program, the exit code, the bytes: as the report says them.
    pub notes: Vec<String>,
}

/// A turn: where its time went, as the report adds it up, and its requests and tools.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnSummary {
    pub turn: u64,
    pub chars: u64,
    pub pictures: u64,
    /// Nobody sent it: a background command's end, a line it printed or a wait woke the session (the `wake` lines say which).
    pub woken: bool,
    /// The main requests' time to the first word, and the rest of them.
    pub prefill_ms: u64,
    pub generation_ms: u64,
    pub tools_ms: u64,
    pub wall_ms: u64,
    /// What it waited for the person, what compaction took, what retries waited.
    pub waits_ms: u64,
    pub compaction_ms: u64,
    pub retries_ms: u64,
    /// A turn start was recorded without a matching end.
    pub unfinished: bool,
    /// How it failed, where it did.
    pub failed: Option<String>,
    pub requests: Vec<RequestRow>,
    pub tools: Vec<ToolRow>,
}

/// A trajectory as data: the turns (the connection before the first is not one) and the whole of the time.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub turns: Vec<TurnSummary>,
    pub lines: usize,
    /// Lines that could not be read.
    pub unreadable: usize,
}

impl Summary {
    /// The turns added up: its request and tool rows are not kept.
    pub fn total(&self) -> TurnSummary {
        let mut all = TurnSummary::default();
        for turn in &self.turns {
            all.prefill_ms += turn.prefill_ms;
            all.generation_ms += turn.generation_ms;
            all.tools_ms += turn.tools_ms;
            all.wall_ms += turn.wall_ms;
            all.waits_ms += turn.waits_ms;
            all.compaction_ms += turn.compaction_ms;
            all.retries_ms += turn.retries_ms;
            all.unfinished |= turn.unfinished;
        }
        all
    }
}

/// The trajectory's turns, each with where its time went (as the report adds it up), its requests and its tools: what a page draws as a table.
pub fn summary(trajectory: &str) -> Summary {
    let (lines, bad) = load(trajectory);
    let turns = lines_by_turn(&lines);
    let mut out = Summary { turns: Vec::new(), lines: lines.len(), unreadable: bad };
    for (turn, in_turn) in &turns {
        let spent = Spent::of(in_turn);
        let began = in_turn.iter().find(|line| line.ev() == "turn_start");
        let mut row = TurnSummary {
            turn: *turn,
            chars: began.and_then(|line| line.u("chars")).unwrap_or(0),
            pictures: began.and_then(|line| line.u("images")).unwrap_or(0),
            woken: began.is_some_and(|line| line.v["woken"] == true),
            prefill_ms: spent.prefill,
            generation_ms: spent.generation,
            tools_ms: spent.tool_ms(),
            wall_ms: spent.wall,
            waits_ms: spent.waits,
            compaction_ms: spent.compaction,
            retries_ms: spent.retries,
            unfinished: spent.unfinished,
            ..TurnSummary::default()
        };
        for line in in_turn {
            match line.ev() {
                "request" => {
                    let (ms, first) = (line.u("ms").unwrap_or(0), line.u("first_ms"));
                    let generation = first.map_or(ms, |first| ms.saturating_sub(first));
                    let completion = line.u("completion");
                    let (rate, counted) = line.table_rate(generation);
                    let estimate = line.estimated_tokens();
                    row.requests.push(RequestRow {
                        id: short(line.req()),
                        role: line.s("role").to_string(),
                        first_ms: first,
                        generation_ms: generation,
                        prompt: line.u("prompt"),
                        cached_percent: line.cache_percent(),
                        tokens: completion.or(estimate),
                        tokens_estimated: completion.is_none() && estimate.is_some(),
                        rate,
                        counted,
                        end: line.s("end").to_string(),
                        calls: line.v["calls"].as_array().map(|calls| calls.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default(),
                        retry_of: line.v["retry_of"].as_str().map(|of| (short(of), line.s("retry_why").to_string())),
                        helper_file: line.v["helper_file"].as_str().map(str::to_string),
                    });
                }
                "tool" => row.tools.push(ToolRow { id: short(line.req()), name: line.s("name").to_string(), ms: line.u("ms").unwrap_or(0), outcome: line.tool_outcome().to_string(), notes: tool_notes(line) }),
                "failed" if row.failed.is_none() => row.failed = Some(line.s("kind").to_string()),
                _ => {}
            }
        }
        out.turns.push(row);
    }
    out
}

/// `summary` for a file.
pub fn summary_file(trajectory: &Path) -> Result<Summary, String> {
    let text = std::fs::read(trajectory).map_err(|error| format!("{}: {error}", trajectory.display()))?;
    Ok(summary(&String::from_utf8_lossy(&text)))
}

/// `report` for files.
pub fn report_files(trajectory: &Path, engine_log: Option<&Path>) -> Result<String, String> {
    let text = std::fs::read(trajectory).map_err(|error| format!("{}: {error}", trajectory.display()))?;
    let engine = match engine_log {
        Some(path) => Some(String::from_utf8_lossy(&std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?).trim_start_matches('\u{feff}').to_string()),
        None => None,
    };
    Ok(report(&String::from_utf8_lossy(&text), engine.as_deref()))
}

/// What `dogmind-app --trajectory <file> [--engine-log <json>]` does, from its arguments after the first: the report, or why it cannot.
pub fn run(arguments: &[String]) -> Result<String, String> {
    let (mut trajectory, mut engine) = (None, None);
    let mut rest = arguments.iter();
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "--engine-log" => engine = Some(rest.next().ok_or("--engine-log needs a file")?.clone()),
            other if trajectory.is_none() => trajectory = Some(other.to_string()),
            other => return Err(format!("what is {other}? Usage: dogmind-app --trajectory <file> [--engine-log <json>]")),
        }
    }
    let trajectory = trajectory.ok_or("Usage: dogmind-app --trajectory <file> [--engine-log <json>]")?;
    report_files(Path::new(&trajectory), engine.as_deref().map(Path::new))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(lines: &[Value]) -> String {
        lines.iter().map(|line| format!("{line}\n")).collect()
    }

    #[test]
    fn a_tool_row_lists_every_edited_path_once_in_recorded_order() {
        let mut patch = tool(1, "apply_patch", 5, "ok", Some("src/main.js"), None, "args");
        patch["edited"] = json!([{"path":"src/main.js"}, {"path":"styles.css"}, {"path":"src/main.js"}]);
        let legacy = tool(2, "apply_patch", 4, "ok", Some("README.md"), None, "args2");
        let trajectory = text(&[patch, legacy]);
        let model = summary(&trajectory);
        let tools = &model.turns[0].tools;
        assert_eq!(tools[0].notes[..2], ["src/main.js", "styles.css"]);
        assert_eq!(tools[0].notes.iter().filter(|note| *note == "src/main.js").count(), 1);
        assert_eq!(tools[1].notes[0], "README.md");
        assert!(report(&trajectory, None).lines().any(|line| line.contains("apply_patch") && line.contains("src/main.js, styles.css,")));
    }

    #[test]
    fn a_multi_file_success_breaks_each_targets_failure_streak() {
        let fail = |n, path| tool(n, "apply_patch", 5, "error", Some(path), None, "bad");
        let mut ok = tool(4, "apply_patch", 5, "ok", Some("main.js"), None, "fix");
        ok["edited"] = json!([{"path":"main.js"}, {"path":"styles.css"}]);
        let mut lines = vec![fail(1, "styles.css"), fail(2, "other.txt"), ok, fail(5, "styles.css"), fail(6, "other.txt")];
        let result = report(&text(&lines), None);
        assert!(!result.contains("edits to styles.css that failed"), "{result}");
        assert!(result.contains("edits to other.txt that failed: 2 in a row, 2 in all"), "{result}");
        lines.push(fail(7, "styles.css"));
        assert!(report(&text(&lines), None).contains("edits to styles.css that failed: 2 in a row, 3 in all"));
    }

    fn request(n: u64, at: &str, role: &str, first: u64, ms: u64, prompt: u64, cached: u64, completion: u64, end: &str) -> Value {
        json!({"t": at, "ev": "request", "turn": 1, "req": format!("s/{n}"), "role": role, "first_ms": first, "ms": ms, "prompt": prompt, "cached": cached, "completion": completion, "tps": null, "end": end, "calls": []})
    }

    fn tool(n: u64, name: &str, ms: u64, outcome: &str, path: Option<&str>, hash: Option<&str>, args: &str) -> Value {
        let mut line = json!({"t": "2026-11-05T10:00:30.000+09:00", "ev": "tool", "turn": 1, "req": format!("s/{n}"), "name": name, "ms": ms, "outcome": outcome, "out_bytes": 10, "args_hash": args});
        if let Some(path) = path {
            line["path"] = json!(path);
        }
        if let Some(hash) = hash {
            line["edit_hash"] = json!(hash);
        }
        line
    }

    #[test]
    fn request_speeds_agree_across_the_report_table_and_timeline() {
        for (tps, server, tokens, measured, expected, display) in [
            (None, json!({"decode_tokens_per_second":12.5}), None, Some(12.5), Some(12.5), "12.5"),
            (Some(0.0), json!({"predicted_per_second":15.0}), Some(20), Some(15.0), Some(15.0), "15.0"),
            (Some(9.0), json!({"decode_tokens_per_second":12.5}), Some(20), Some(9.0), Some(9.0), "9.0"),
            (None, json!({}), Some(20), None, Some(20.0), "~20.0"),
            (None, json!({}), None, None, None, "-"),
        ] {
            let trace = text(&[json!({"ev":"turn_start","turn":1}), json!({"ev":"request","turn":1,"req":"s/1","role":"main","end":"stop","ms":1100,"first_ms":100,"tps":tps,"completion":tokens,"server":server})]);
            let printed = report(&trace, None);
            let row = &summary(&trace).turns[0].requests[0];
            assert!(printed.contains(&format!("tok at {display:>6} tok/s")), "{printed}");
            assert_eq!((row.rate, row.counted), (expected, measured.is_none() && expected.is_some()));
            let timeline = crate::timeline::build(&[], &trace);
            let detail = timeline.turns[0].steps.iter().find_map(|step| step.request.as_ref()).unwrap();
            assert_eq!(detail.tps, measured);
        }
    }

    #[test]
    fn draft_stop_reports_only_the_line_counts_recorded_by_the_writer() {
        for (counts, expected) in [
            (json!({"run":25,"fenced":100}), "longest run of 25 lines of code, 100 lines across fenced blocks"),
            (json!({"run":39,"fenced":60}), "longest run of 39 lines of code, 60 lines across fenced blocks"),
            (json!({"fenced":0}), "0 lines across fenced blocks"),
            (json!({"code_lines":12,"fences":2}), "12 lines of code in all, 2 fence marks in all"),
            (json!({"code_lines":5}), "5 lines of code in all"),
            (json!({}), "line counts unavailable"),
        ] {
            let mut line = counts;
            line["ev"] = json!("intervention");
            line["kind"] = json!("draft_stop");
            let printed = report(&text(&[line]), None);
            assert!(printed.contains(&format!("before the stop ({expected})")), "{printed}");
        }
    }

    #[test]
    fn request_counts_distinguish_estimates_from_reported_zero_and_missing_usage() {
        let lines = [
            json!({"ev":"request", "turn":1, "req":"s/1", "role":"main", "tokens_est":27}),
            json!({"ev":"intervention", "turn":1, "req":"s/1", "kind":"draft_stop", "tokens_before":27, "reasoning_kept":true}),
            json!({"ev":"request", "turn":1, "req":"s/2", "role":"main", "completion":0, "tokens_est":15}),
            json!({"ev":"request", "turn":1, "req":"s/3", "role":"main"}),
        ];
        let printed = report(&text(&lines), None);
        let written: Vec<_> = printed.lines().filter_map(|line| line.split_once(" wrote ")?.1.split_whitespace().next()).collect();
        assert_eq!(written[..3], ["~27", "0", "-"]);
        assert!(printed.contains("~27 tokens generated before the stop (line counts unavailable, reasoning kept)"));
        assert!(printed.contains("next: /2 wrote 0 tokens"));
    }

    #[test]
    fn an_intervention_cannot_claim_a_later_turns_request() {
        let guard = json!({"ev":"intervention","turn":1,"req":"s/1","kind":"draft_stop"});
        let main = json!({"ev":"request","turn":1,"req":"s/2","role":"main","completion":36});
        let other = json!({"ev":"request","turn":2,"req":"s/3","role":"main","completion":18});
        for tail in [
            vec![json!({"ev":"turn_end","turn":1}), main.clone()],
            vec![json!({"ev":"turn_start","turn":1}), main.clone()],
            vec![other],
        ] {
            let mut lines = vec![guard.clone()];
            lines.extend(tail);
            let said = report(&text(&lines), None);
            assert!(!said.contains("next:"), "{said}");
        }
        let helper = json!({"ev":"request","turn":1,"req":"s/helper","role":"helper"});
        let said = report(&text(&[guard, helper, main]), None);
        assert!(said.contains("next: /2 wrote 36 tokens"), "{said}");
    }

    #[test]
    fn an_unmetered_request_in_an_older_file_has_no_token_estimate() {
        for (role, reported, estimate, label, value) in [
            ("warm", None, 0, "-", None),
            ("helper", None, 0, "-", None),
            ("checkpoint", None, 0, "-", None),
            ("helper", Some(0), 0, "0", Some(0)),
            ("main", None, 0, "~0", Some(0)),
            ("helper", None, 17, "~17", Some(17)),
        ] {
            let trace = json!({"ev":"request","turn":1,"role":role,"completion":reported,"tokens_est":estimate}).to_string();
            assert!(report(&trace, None).contains(&format!("wrote {label:>6} tok")), "{trace}");
            let row = &summary(&trace).turns[0].requests[0];
            assert_eq!((row.tokens, row.tokens_estimated), (value, label.starts_with('~')), "{trace}");
        }
    }

    #[test]
    fn a_run_is_told_by_its_requests_its_tools_its_stops_and_where_the_time_went() {
        let lines = [
            json!({"t": "2026-11-05T10:00:00.000+09:00", "ev": "turn_start", "turn": 1, "chars": 40, "images": 0}),
            request(1, "2026-11-05T10:01:43.000+09:00", "main", 2000, 103_000, 5000, 0, 8328, "stopped_app:draft_stop"),
            json!({"t": "2026-11-05T10:01:43.001+09:00", "ev": "intervention", "turn": 1, "req": "s/1", "kind": "draft_stop", "tokens_before": 8328, "reasoning_chars": 20000, "stops_before": 0, "run": 61}),
            request(2, "2026-11-05T10:02:20.000+09:00", "main", 1000, 37_000, 13_000, 4800, 2714, "tool_calls"),
            tool(2, "create_file", 40, "ok", Some("index.html"), Some("aaaaaaaaaaaa"), "h1"),
            json!({"t": "2026-11-05T10:02:21.000+09:00", "ev": "approval", "turn": 1, "req": "s/2", "tool": "execute_command", "wait_ms": 4000, "decision": "approve"}),
            json!({"t": "2026-11-05T10:02:30.000+09:00", "ev": "turn_end", "turn": 1, "cancelled": false, "requests": 2, "tools": 1, "ms": 150_000}),
        ];
        let said = report(&text(&lines), None);
        assert!(said.contains("turn 1: 40 characters"), "{said}");
        // Time: 140 s of the 150 in the model (3 of them before the first words), 40 ms of tools, 4 s of waiting.
        assert!(said.contains("wall 150.0 s") && said.contains("model 140.0 s (93%): prefill to the first word 3.0 s, generation 137.0 s"), "{said}");
        assert!(said.contains("create_file (calls: 1, total: 0.0 s)") && said.contains("waiting for the person 4.0 s"), "{said}");
        // Each request, with its cached share.
        assert!(said.contains("   /1 main") && said.contains("stopped_app:draft_stop") && said.contains("cached  36%"), "{said}");
        assert!(said.contains("index.html"), "{said}");
        // What the stop cost: the tokens thrown away and the time, and what the request after it did.
        assert!(said.contains("draft_stop at /1: ~8328 tokens generated in 103.0 s before the stop (longest run of 61 lines of code)"), "{said}");
        assert!(said.contains("next: /2 wrote 2714 tokens in 37.0 s, cached 36%, ended tool_calls"), "{said}");
        assert!(said.contains("signs of going round in circles\n  none"), "{said}");
    }

    #[test]
    fn a_report_bounds_cache_reuse_and_distinguishes_a_missing_count_from_zero() {
        for (cached, expected, shown) in [(Some(9600), Some(100), "100%"), (Some(0), Some(0), "0%"), (None, None, "-")] {
            let mut entry = request(1, "2026-11-05T10:00:01+09:00", "main", 200, 1000, 8000, 0, 10, "stop");
            entry["cached"] = json!(cached);
            let input = text(&[entry]);
            assert_eq!(summary(&input).turns[0].requests[0].cached_percent, expected);
            assert!(report(&input, None).contains(&format!("cached {shown:>4} wrote")));
        }
    }

    #[test]
    fn a_trajectory_as_data_has_each_turn_with_its_time_its_requests_and_its_tools() {
        let lines = [
            json!({"t": "2026-11-05T09:59:50.000+09:00", "ev": "connection", "turn": 0, "what": "connect", "ms": 1200, "outcome": "ok"}),
            json!({"t": "2026-11-05T10:00:00.000+09:00", "ev": "turn_start", "turn": 1, "chars": 40, "images": 2}),
            request(1, "2026-11-05T10:01:43.000+09:00", "main", 2000, 103_000, 5000, 0, 8328, "stopped_app:draft_stop"),
            request(2, "2026-11-05T10:02:20.000+09:00", "main", 1000, 37_000, 13_000, 4800, 2714, "tool_calls"),
            tool(2, "create_file", 40, "ok", Some("index.html"), Some("aaaaaaaaaaaa"), "h1"),
            json!({"t": "2026-11-05T10:02:21.000+09:00", "ev": "approval", "turn": 1, "req": "s/2", "tool": "execute_command", "wait_ms": 4000, "decision": "approve"}),
            json!({"t": "2026-11-05T10:02:30.000+09:00", "ev": "turn_end", "turn": 1, "cancelled": false, "requests": 2, "tools": 1, "ms": 150_000}),
            json!({"t": "2026-11-05T10:03:00.000+09:00", "ev": "turn_start", "turn": 2, "chars": 4, "images": 0}),
            json!({"t": "2026-11-05T10:03:05.000+09:00", "ev": "failed", "turn": 2, "req": "s/3", "kind": "server_gone"}),
        ];
        let single = report(&text(&lines[..7]), None);
        assert!(single.contains("turn 1:") && single.contains("connections"));
        assert!(!single.contains("all turns"));
        assert!(report(&text(&lines), None).contains("all turns\n  wall 155.0 s (unfinished turns timed through their last turn event)"));
        let said = summary(&text(&lines));
        // (the connection before the first turn is not a turn)
        assert_eq!(said.turns.len(), 2);
        let first = &said.turns[0];
        assert_eq!((first.turn, first.chars, first.pictures), (1, 40, 2));
        // The report's own sums: 3 s of the 140 s in the model before the first words, 137 s after, 40 ms of tools, 4 s waited, 150 s in all.
        assert_eq!((first.prefill_ms, first.generation_ms, first.tools_ms, first.wall_ms, first.waits_ms), (3000, 137_000, 40, 150_000, 4000));
        assert_eq!(first.requests.len(), 2);
        assert_eq!((first.requests[0].id.as_str(), first.requests[0].role.as_str(), first.requests[0].first_ms, first.requests[0].generation_ms), ("/1", "main", Some(2000), 101_000));
        assert_eq!((first.requests[1].cached_percent, first.requests[1].tokens, first.requests[1].end.as_str()), (Some(36), Some(2714), "tool_calls"));
        assert!(first.requests[1].counted && first.requests[1].rate.is_some_and(|rate| (rate - 2714.0 * 1000.0 / 36_000.0).abs() < 0.01), "{:?}", first.requests[1]);
        assert_eq!((first.tools[0].name.as_str(), first.tools[0].ms, first.tools[0].outcome.as_str()), ("create_file", 40, "ok"));
        assert_eq!(first.tools[0].notes, vec!["index.html".to_string(), "10 bytes out".to_string()]);
        assert!(!first.unfinished);
        // A turn with no end, and how it failed.
        let second = &said.turns[1];
        assert!(second.unfinished);
        assert_eq!(second.failed.as_deref(), Some("server_gone"));
        let all = said.total();
        assert_eq!((all.prefill_ms, all.tools_ms), (3000, 40));
        // The printed report is of the same trajectory, and says the same of it.
        assert!(report(&text(&lines), None).contains("tools 0.0 s (0%): create_file (calls: 1, total: 0.0 s)"));
    }

    #[test]
    fn answered_questions_and_plan_reviews_are_person_time_in_the_report_and_summary() {
        let lines = [
            json!({"ev":"tool","turn":1,"name":"ask_user","outcome":"ok","ms":148039}),
            json!({"ev":"tool","turn":1,"name":"submit_plan","outcome":"ok","ms":2000}),
            json!({"ev":"approval","turn":1,"wait_ms":112374}),
            json!({"ev":"tool","turn":1,"name":"execute_command","outcome":"declined_person","ms":5,"wait_ms":112374}),
            json!({"ev":"tool","turn":1,"name":"ask_user","outcome":"error","ms":120}),
            json!({"ev":"tool","turn":1,"name":"present","outcome":"ok","ms":50}),
            json!({"ev":"turn_end","turn":1,"ms":300000}),
        ];
        let trace = text(&lines);
        let totals = summary(&trace).total();
        assert_eq!((totals.waits_ms, totals.tools_ms, totals.wall_ms), (262413, 175, 300000));
        let printed = report(&trace, None);
        assert!(printed.contains("waiting for the person 262.4 s"), "{printed}");
        assert!(printed.lines().any(|line| line.contains("ask_user") && line.contains("148.0 s waiting for the person")), "{printed}");
    }

    #[test]
    fn a_failed_turn_is_told_with_the_line_the_server_said() {
        let lines = [
            json!({"t": "2026-11-05T10:00:00.000+09:00", "ev": "turn_start", "turn": 1, "chars": 4, "images": 0}),
            request(1, "2026-11-05T10:00:05.000+09:00", "main", 100, 5000, 5000, 0, 10, "error:down"),
            json!({"t": "2026-11-05T10:00:05.001+09:00", "ev": "failed", "turn": 1, "req": "s/1", "kind": "server_gone", "server_said": "Straydog's worker exited (exit code 0xC0000017 STATUS_NO_MEMORY: out of memory)"}),
            json!({"t": "2026-11-05T10:00:06.000+09:00", "ev": "failed", "turn": 1, "req": "s/1", "kind": "unfinished"}),
        ];
        let said = report(&text(&lines), None);
        assert!(said.contains("\nfailures\n  turn 1 at /1: server_gone, the server said: Straydog's worker exited (exit code 0xC0000017 STATUS_NO_MEMORY: out of memory)\n  turn 1 at /1: unfinished\n"), "{said}");
        // A run that did not fail has no such section.
        assert!(!report(&text(&lines[..2]), None).contains("failures"));
    }

    #[test]
    fn later_connections_and_warmups_do_not_extend_an_unfinished_turn() {
        let mut lines = vec![
            json!({"t":"2026-11-05T08:00:00Z","turn":1,"ev":"connection","what":"connect","ms":20,"outcome":"ok"}),
            json!({"t":"2026-11-05T10:00:00Z","turn":1,"ev":"turn_start"}),
            json!({"t":"2026-11-05T10:00:02Z","turn":1,"ev":"tool","name":"read_file","ms":100}),
            json!({"t":"2026-11-05T12:00:00Z","turn":1,"ev":"connection","what":"connect","ms":50,"outcome":"error:no_answer"}),
            json!({"t":"2026-11-05T12:00:05Z","turn":1,"ev":"request","role":"warm","ms":5000}),
        ];
        let unfinished = summary(&text(&lines));
        assert!(unfinished.turns[0].unfinished);
        assert_eq!(unfinished.total().wall_ms, 2000);
        assert_eq!(summary(&text(&lines[..2])).total().wall_ms, 0);
        assert!(report(&text(&lines), None).contains("error:no_answer"));
        lines.push(json!({"t":"2026-11-05T12:00:10Z","turn":1,"ev":"turn_end","ms":12345}));
        assert_eq!(summary(&text(&lines)).total().wall_ms, 12345);
    }

    #[test]
    fn parallel_calls_share_only_the_overlapping_time_in_the_same_request() {
        let call = |req: &str, start: &str, ms: u64| json!({
            "ev":"tool", "turn":1, "req":req, "name":"read_file", "parallel":3, "started":start, "ms":ms,
        });
        let input = text(&[
            call("s/1", "2026-11-23T10:00:00.500Z", 1000),
            call("s/1", "2026-11-23T10:00:00.000Z", 1000),
            call("s/1", "2026-11-23T10:00:00.600Z", 100),
            call("s/1", "2026-11-23T10:00:02.000Z", 200),
            call("s/2", "2026-11-23T10:00:00.000Z", 100),
            call("s/1", "", 300),
        ]);
        let details = summary(&input);
        assert_eq!(details.turns[0].tools.len(), 6);
        assert_eq!(details.turns[0].tools_ms, 2100);
        let said = report(&input, None);
        assert!(said.contains("tools 2.1 s") && said.contains("read_file (calls: 6, total: 2.7 s)"), "{said}");
    }

    #[test]
    fn an_interrupted_explore_counts_its_recorded_helper_requests() {
        let lines = [
            json!({"ev": "turn_start", "turn": 1}),
            json!({"ev": "request", "role": "main", "turn": 1, "req": "s/1", "calls": ["explore", "explore"]}),
            json!({"ev": "request", "role": "helper", "turn": 1, "req": "s/2", "ms": 600}),
            json!({"ev": "tool", "turn": 1, "req": "s/1", "name": "explore", "ms": 650}),
            json!({"ev": "request", "role": "helper", "turn": 1, "req": "s/3", "ms": 300}),
            json!({"ev": "request", "role": "helper", "turn": 1, "req": "s/4", "ms": 200}),
            // The previous explore's completion is missing. The next request finishes its own explore.
            json!({"ev": "request", "role": "main", "turn": 1, "req": "s/5", "calls": ["explore"]}),
            json!({"ev": "request", "role": "helper", "turn": 1, "req": "s/6", "ms": 100}),
            json!({"ev": "tool", "turn": 1, "req": "s/5", "name": "explore", "ms": 125}),
        ];
        assert_eq!(summary(&text(&lines[..4])).total().tools_ms, 650);
        assert_eq!(summary(&text(&lines[..6])).total().tools_ms, 1150);
        let result = summary(&text(&lines));
        assert_eq!(result.turns[0].tools_ms, 1275);
        assert_eq!(result.total().tools_ms, 1275);
        let mut both_turns = lines.to_vec();
        both_turns.push(json!({"ev": "request", "role": "helper", "turn": 2, "ms": 500}));
        both_turns.push(json!({"ev": "tool", "name": "explore", "turn": 2}));
        let said = report(&text(&both_turns), None);
        assert!(said.contains("tools 1.8 s") && said.contains("helper's own requests 1.7 s"), "{said}");
    }

    #[test]
    fn nested_tool_time_is_included_once_and_orphan_calls_still_count() {
        let input = text(&[
            json!({"ev":"tool", "turn":1, "req":"s/1", "id":"c.1", "parent":"c", "name":"read_file", "ms":800}),
            json!({"ev":"tool", "turn":1, "req":"s/1", "id":"c", "name":"run_code", "ms":1000}),
            // A different request reused the id. Its own parent never finished.
            json!({"ev":"tool", "turn":1, "req":"s/2", "id":"c.1", "parent":"c", "name":"read_file", "ms":300}),
            json!({"ev":"tool", "turn":2, "name":"read_file", "ms":200}),
        ]);
        let details = summary(&input);
        assert_eq!(details.turns[0].tools_ms, 1300);
        assert_eq!(details.turns[0].tools.len(), 3);
        assert_eq!(details.turns[0].tools[0].ms, 800);
        assert_eq!(details.total().tools_ms, 1500);
        let said = report(&input, None);
        assert!(said.contains("tools 1.5 s") && said.contains("read_file (calls: 3, total: 1.3 s)"), "{said}");
    }

    #[test]
    fn command_exit_status_agrees_in_the_report_and_turn_details() {
        let cases = [
            ("ok", Some(7), "failed"),
            ("ok", Some(-1), "failed"),
            ("ok", Some(0), "ok"),
            ("ok", None, "ok"),
            ("timeout", Some(1), "timeout"),
        ];
        for (recorded, code, expected) in cases {
            let mut event = tool(1, "execute_command", 5, recorded, None, None, "cmd");
            if let Some(code) = code {
                event["exit_code"] = json!(code);
            }
            let input = text(&[event]);
            let said = report(&input, None);
            let row = said.lines().find(|line| line.split_whitespace().nth(1) == Some("execute_command")).unwrap();
            assert_eq!(row.split_whitespace().nth(4), Some(expected), "{row}");
            let details = summary(&input);
            let tool = &details.turns[0].tools[0];
            assert_eq!(tool.outcome, expected);
            if let Some(code) = code.filter(|code| *code != 0) {
                let exit = format!("exit {code}");
                assert!(row.contains(&exit) && tool.notes.contains(&exit));
            }
        }
    }

    #[test]
    fn a_run_going_round_is_told_by_its_failed_edits_its_files_that_come_back_and_its_calls_again() {
        let lines = [
            json!({"t": "2026-11-05T10:00:00.000+09:00", "ev": "turn_start", "turn": 1, "chars": 4, "images": 0}),
            tool(1, "apply_patch", 5, "error", Some("a.rs"), None, "p1"),
            tool(2, "apply_patch", 5, "error", Some("a.rs"), None, "p2"),
            tool(3, "apply_patch", 5, "ok", Some("a.rs"), Some("111111111111"), "p3"),
            tool(4, "apply_patch", 5, "ok", Some("a.rs"), Some("222222222222"), "p4"),
            tool(5, "apply_patch", 5, "ok", Some("a.rs"), Some("111111111111"), "p5"),
            tool(6, "read_file", 5, "ok", Some("b.rs"), None, "same"),
            tool(7, "read_file", 5, "ok", Some("b.rs"), None, "same"),
            tool(8, "execute_command", 5, "error", None, None, "c1"),
            tool(9, "execute_command", 5, "ok", None, None, "c2"),
            json!({"t": "2026-11-05T10:00:40.000+09:00", "ev": "tool", "turn": 1, "req": "s/10", "name": "execute_command", "ms": 5, "outcome": "ok", "exit_code": 1, "args_hash": "c3"}),
            json!({"t": "2026-11-05T10:00:41.000+09:00", "ev": "tool", "turn": 1, "req": "s/11", "name": "execute_command", "ms": 5, "outcome": "ok", "exit_code": 2, "args_hash": "c4"}),
            json!({"t": "2026-11-05T10:00:42.000+09:00", "ev": "tool", "turn": 1, "req": "s/12", "name": "execute_command", "ms": 5, "outcome": "timeout", "args_hash": "c5"}),
        ];
        let said = report(&text(&lines), None);
        assert!(said.contains("edits to a.rs that failed: 2 in a row, 2 in all"), "{said}");
        assert!(said.contains("a.rs came back to the state it had after /3 (hash 111111111111), at /5"), "{said}");
        assert!(said.contains("read_file called again with the same arguments (hash same), nothing edited between: at /7"), "{said}");
        assert!(said.contains("3 failed calls in a row, the longest run ending at /12"), "{said}");
        assert!(said.contains("no turn_end recorded, timed through the last turn event"), "{said}");
    }

    /// A program's loop calls a tool again by design (here a page timed four times): that is not going round. A call again from the model's next step is.
    #[test]
    fn a_programs_own_calls_again_are_not_going_round_and_one_from_another_step_is() {
        let nested = |req: u32, id: &str, parent: &str| {
            json!({"t": "2026-11-05T10:00:00.000+09:00", "ev": "tool", "turn": 1, "req": format!("s/{req}"), "id": id, "name": "check_page", "ms": 1500, "outcome": "ok", "args_hash": "page", "parent": parent})
        };
        let lines = [
            json!({"t": "2026-11-05T10:00:00.000+09:00", "ev": "turn_start", "turn": 1, "chars": 4, "images": 0}),
            nested(9, "a.1", "a"), nested(9, "a.2", "a"), nested(9, "a.3", "a"), nested(9, "a.4", "a"),
        ];
        let said = report(&text(&lines), None);
        assert!(!said.contains("called again"), "{said}");
        let mut more = lines.to_vec();
        more.extend([nested(10, "b.1", "b"), nested(11, "c.1", "c")]);
        let said = report(&text(&more), None);
        assert!(said.contains("check_page called again with the same arguments (hash page), nothing edited between: at /10"), "{said}");
        assert!(said.contains("... and a 3rd time at /11"), "{said}");
    }

    #[test]
    fn the_engines_lines_are_put_beside_the_requests_by_id_and_where_there_is_none_by_time() {
        let log = json!({"next": 3, "lines": [
            "2026-11-05T01:00:10.500000Z  INFO Straydog request: id s/1, prompt 5000 tokens (reused 4000 from live), new 1000 in 900 ms (1111 tok/s), first token 0.90 s, 300 tokens at 70.0 tok/s (150 of 200 drafts kept in 99 passes; DSpark 1 of 2); prefill expert reads: VRAM 1",
            "2026-11-05T01:00:20.000000Z  INFO Straydog request: prompt 7000 tokens (reused 0 from root, the same as before for 10), 1 image (0.29 s), new 7000 in 5000 ms, first token 5.00 s, 20 tokens at 40.0 tok/s",
            "2026-11-05T01:00:25.000000Z  INFO Straydog request: id other/1, prompt 9 tokens (reused 0 from -), new 9 in 9 ms, first token 0.01 s, 1 tokens at 10.0 tok/s",
            "2026-11-05T01:00:30.000000Z  WARN Straydog request failed: id s/9, the worker went",
        ]}).to_string();
        let lines = [
            json!({"t": "2026-11-05T10:00:00.000+09:00", "ev": "turn_start", "turn": 1, "chars": 4, "images": 0}),
            request(1, "2026-11-05T10:00:10.600+09:00", "main", 900, 10_000, 5000, 4000, 300, "stop"),
            request(2, "2026-11-05T10:00:20.400+09:00", "main", 5000, 9000, 7000, 0, 20, "stop"),
            json!({"t":"2026-11-05T10:00:30.300+09:00","ev":"request","turn":1,"req":"s/9","role":"main","end":"error:down"}),
            request(3, "2026-11-05T10:01:40.000+09:00", "main", 5000, 9000, 1, 0, 20, "stop"),
        ];
        let said = report(&text(&lines), Some(&log));
        assert!(said.contains("engine (id): prompt 5000, reused 4000 from live, prefilled 1000 in 900 ms, first token 0.90 s, 300 tokens at 70.0 tok/s, drafts 150/200"), "{said}");
        assert!(said.contains("engine (time): prompt 7000, reused 0 from root, prefilled 7000 in 5000 ms"), "{said}");
        assert!(said.contains("engine: no line for this request"), "{said}");
        assert!(said.contains("engine (id): request failed, the app's line is +300 ms"), "{said}");
        assert!(!said.contains("engine (id): prompt 0"), "{said}");
        assert!(said.contains("3 of 4 requests joined to the engine's log: 2 by id, 1 by time"), "{said}");
    }

    #[test]
    fn the_connection_before_the_first_turn_is_told_apart_from_the_turns_and_the_engines_line_says_how_far_apart() {
        let log = json!({"lines": ["2026-11-05T01:00:10.500000Z  INFO Straydog request: id s/1, prompt 5000 tokens (reused 4000 from live), new 1000 in 900 ms, first token 0.90 s, 300 tokens at 70.0 tok/s"]}).to_string();
        let lines = [
            json!({"t": "2026-11-05T09:59:50.000+09:00", "ev": "connection", "turn": 0, "what": "connect", "ms": 1200, "outcome": "ok"}),
            json!({"t": "2026-11-05T10:00:00.000+09:00", "ev": "turn_start", "turn": 1, "chars": 4, "images": 0}),
            request(1, "2026-11-05T10:00:10.512+09:00", "main", 900, 10_000, 5000, 4000, 300, "stop"),
            json!({"t": "2026-11-05T10:00:11.000+09:00", "ev": "turn_end", "turn": 1, "cancelled": false, "requests": 1, "tools": 0, "ms": 11_000}),
        ];
        let said = report(&text(&lines), Some(&log));
        assert!(said.contains("connections\n  connect took 1.2 s, ok ending at 09:59:50.000"), "{said}");
        assert!(!said.contains("before the first turn"), "{said}");
        assert!(said.contains("the app's line is +12 ms from the engine's"), "{said}");
    }

    /// A command's time besides the program's is on its line, and a request that waited behind another at the server says so.
    #[test]
    fn a_commands_own_parts_and_a_wait_at_the_server_are_told() {
        let mut command = tool(2, "execute_command", 900, "ok", None, None, "h");
        for (key, ms) in [("prepare_ms", 320u64), ("grant_ms", 10), ("repos_ms", 250), ("toolchains_ms", 0), ("env_ms", 40), ("run_ms", 560), ("after_ms", 90)] {
            command[key] = json!(ms);
        }
        let mut queued = request(1, "2026-11-05T10:00:30.000+09:00", "main", 24_500, 30_000, 5000, 0, 100, "stop");
        queued["server"] = json!({"ttft_seconds": 1.17});
        let mut prompt = request(2, "2026-11-05T10:00:50.000+09:00", "main", 1500, 5000, 5000, 0, 100, "stop");
        prompt["server"] = json!({"ttft_seconds": 1.2});
        let lines = [json!({"t": "2026-11-05T10:00:00.000+09:00", "ev": "turn_start", "turn": 1, "chars": 4, "images": 0}), queued, prompt, command];
        let said = report(&text(&lines), None);
        assert!(said.contains("queued at the server: 23.3 s (the app's first word came after 24.5 s, the server's own first token 1.17 s after it began)"), "{said}");
        assert_eq!(said.matches("queued at the server").count(), 1, "a first word that is the server's own is not a queue: {said}");
        assert!(said.contains("prepared in 0.3 s (grants 0.0 s, repositories 0.2 s, environment 0.0 s), ran 0.6 s, looked after in 0.1 s") || said.contains("prepared in 0.3 s (grants 0.0 s, repositories 0.3 s, environment 0.0 s), ran 0.6 s, looked after in 0.1 s"), "{said}");
        let compaction = json!({"t": "2026-11-05T10:01:00.000+09:00", "ev": "compaction", "turn": 1, "stage": "checkpoint", "before_tokens": 24962, "after_tokens": 18137, "window": 32768, "ms": 41341, "outcome": "ok"});
        assert!(report(&text(&[json!({"t": "2026-11-05T10:00:00.000+09:00", "ev": "turn_start", "turn": 1, "chars": 4, "images": 0}), compaction]), None).contains("compaction (checkpoint): 24962 tokens to 18137 in 41.3 s, of a window of 32768"));
    }

    /// A call the server sends whole, once it has written it, is the app's first word at the end of the generation: the server's time to write it is not a wait before it began.
    #[test]
    fn a_call_sent_whole_at_the_end_is_not_counted_as_a_wait_at_the_server() {
        let call_only = |n: u64, first: u64, completion: u64, server: Value| {
            // 305 tokens: 7.3 s at 42 tokens a second, and the whole of the request is the wait for them (nothing after the first word)
            let mut line = request(n, "2026-11-05T10:00:30.000+09:00", "main", first, first + 10, 5000, 0, completion, "stop");
            line["server"] = server;
            line["calls"] = json!(["create_file"]);
            line["reasoning_chars"] = json!(0);
            line["content_chars"] = json!(0);
            line
        };
        let speed = json!({"ttft_seconds": 0.05, "decode_tokens_per_second": 42.0});
        let begin = json!({"t": "2026-11-05T10:00:00.000+09:00", "ev": "turn_start", "turn": 1, "chars": 4, "images": 0});
        // 305 tokens at 42 tokens a second take 7.3 s, and that is the first word: no wait
        let said = report(&text(&[begin.clone(), call_only(1, 7300, 305, speed.clone())]), None);
        assert!(!said.contains("queued at the server"), "{said}");
        // the same behind a request of 20 s
        let said = report(&text(&[begin.clone(), call_only(1, 27_300, 305, speed.clone())]), None);
        assert!(said.contains("queued at the server: 20.0 s (the app's first word came after 27.3 s, when the server had written the answer, which took it 7.3 s after its first token 0.05 s after it began)"), "{said}");
        let alternate = call_only(1, 27_300, 305, json!({"ttft_seconds": 0.05, "predicted_per_second": 42.0}));
        let said = report(&text(&[begin.clone(), alternate]), None);
        assert!(said.contains("queued at the server: 20.0 s") && said.contains("predicted_per_second 42.00"), "{said}");
        let mut saved = call_only(1, 27_300, 305, json!({"ttft_seconds": 0.05}));
        saved["tps"] = json!(42.0);
        let said = report(&text(&[begin.clone(), saved]), None);
        assert!(said.contains("queued at the server: 20.0 s"), "{said}");
        // no speed told: a first word that long cannot be taken for a wait, nor for none
        let said = report(&text(&[begin.clone(), call_only(1, 7300, 305, json!({"ttft_seconds": 0.05}))]), None);
        assert!(said.contains("queued at the server: cannot be told") && !said.contains("queued at the server: 7"), "{said}");
        // an answer streamed as it was written: its first word is the first token, as it was
        let mut streamed = request(1, "2026-11-05T10:00:30.000+09:00", "main", 10_400, 17_700, 5000, 0, 305, "stop");
        streamed["server"] = json!({"ttft_seconds": 0.4, "decode_tokens_per_second": 42.0});
        streamed["reasoning_chars"] = json!(900);
        streamed["content_chars"] = json!(0);
        let said = report(&text(&[begin.clone(), streamed]), None);
        assert!(said.contains("queued at the server: 10.0 s (the app's first word came after 10.4 s, the server's own first token 0.40 s after it began)"), "{said}");
    }

    #[test]
    fn a_trajectory_that_is_not_all_there_is_read_as_far_as_it_goes() {
        let said = report("{\"t\":\"2026-11-05T10:00:00.000+09:00\",\"ev\":\"turn_start\",\"turn\":1,\"chars\":3,\"images\":0}\nnot json\n{\"t\":\"2026-11-05T10:00:01.000+09:00\",\"ev\":\"request\",\"turn\":1,\"req\":\"s/1\",\"role\":\"main\",\"ms\":1000,\"first_ms\":null,\"end\":\"cancelled_person\",\"completion\":null,\"tokens_est\":12", None);
        assert!(said.starts_with("1 lines") && said.contains("2 that could not be read") && said.contains("no turn_end recorded"), "{said}");
        assert!(report("", None).starts_with("0 lines, no times"));
    }
}
