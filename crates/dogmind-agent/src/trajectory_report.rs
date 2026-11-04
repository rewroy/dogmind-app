//! Reads a trajectory (`trajectory`) and says how the run went: each request and each tool, where the time was, what the app's own guards cost, and whether the run was going round in circles.
//! `dogmind-app --trajectory <file> [--engine-log <json>]` prints it.
//!
//! With the engine's log (as `GET /dashboard/logs` gives it, `{"lines": [...]}`) it also puts each request beside the engine's own line for it: by the id the app sent (`Straydog request: id
//! <req>, ...`), and, for a server that does not log ids yet, by time and the size of the prompt.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::path::Path;

use chrono::{DateTime, FixedOffset, Utc};
use regex::Regex;
use serde_json::Value;

/// How many failed calls in a row, or how many of one call, is worth a line.
const FAILED_IN_A_ROW: usize = 3;

struct Line {
    v: Value,
    at: Option<DateTime<FixedOffset>>,
}

impl Line {
    fn ev(&self) -> &str {
        self.v["ev"].as_str().unwrap_or("")
    }
    fn s(&self, key: &str) -> &str {
        self.v[key].as_str().unwrap_or("")
    }
    fn u(&self, key: &str) -> Option<u64> {
        self.v[key].as_u64()
    }
    fn turn(&self) -> u64 {
        self.u("turn").unwrap_or(0)
    }
    fn req(&self) -> &str {
        self.s("req")
    }
    /// A tool call that did not go well: it ended in anything but `ok`, or it was a command that ran and exited with a code that says so.
    fn failed(&self) -> bool {
        self.s("outcome") != "ok" || self.v["exit_code"].as_i64().is_some_and(|code| code != 0)
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

fn load(trajectory: &str) -> (Vec<Line>, usize) {
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

/// What a request cost and did, in a line: used in the interventions, for the request after one.
fn request_summary(line: &Line) -> String {
    let prompt = line.u("prompt");
    let cached = match (prompt, line.u("cached")) {
        (Some(prompt), Some(cached)) => format!("cached {}", share(cached, prompt)),
        _ => "cached ?".to_string(),
    };
    let calls: Vec<&str> = line.v["calls"].as_array().map(|calls| calls.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    format!("{} wrote {} tokens in {}, {cached}, ended {}{}", short(line.req()), line.u("completion").or(line.u("tokens_est")).map_or("?".to_string(), |tokens| tokens.to_string()), secs(line.u("ms").unwrap_or(0)), line.s("end"), if calls.is_empty() { String::new() } else { format!(", called {}", calls.join(", ")) })
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
    let mut turns: BTreeMap<u64, Vec<&Line>> = BTreeMap::new();
    for line in &lines {
        turns.entry(line.turn()).or_default().push(line);
    }
    let mut total = Spent::default();
    for (turn, in_turn) in &turns {
        // (what comes before the first turn is the connection and the engine's warm-up, which are told below)
        if *turn == 0 {
            continue;
        }
        let spent = Spent::of(in_turn);
        let began = in_turn.iter().find(|line| line.ev() == "turn_start");
        let shown = match began {
            Some(began) => format!("turn {turn}: {} characters, {} pictures", began.u("chars").unwrap_or(0), began.u("images").unwrap_or(0)),
            None => format!("turn {turn}"),
        };
        let _ = writeln!(out, "\n{shown}");
        spent.print(&mut out, "  ");
        total.add(&spent);
    }
    if turns.len() > 1 {
        let _ = writeln!(out, "\nall turns");
        total.print(&mut out, "  ");
    }

    let connections: Vec<&Line> = lines.iter().filter(|line| line.ev() == "connection").collect();
    if !connections.is_empty() {
        let _ = writeln!(out, "\nconnections");
        for line in connections {
            let at = line.at.map_or(String::new(), |at| format!(" ending at {}", at.format("%H:%M:%S%.3f")));
            let _ = writeln!(out, "  {} took {}, {}{at}", line.s("what"), secs(line.u("ms").unwrap_or(0)), line.s("outcome"));
        }
    }

    let _ = writeln!(out, "\nrequests (first = time to the first word; gen = the rest; tok/s is the server's where it said, else counted)");
    for (at, line) in &requests {
        let (ms, first) = (line.u("ms").unwrap_or(0), line.u("first_ms"));
        let generation = first.map_or(ms, |first| ms.saturating_sub(first));
        let completion = line.u("completion");
        let rate = line.v["tps"].as_f64().map(|rate| format!("{rate:.1}")).or_else(|| completion.filter(|_| generation > 0).map(|tokens| format!("~{:.1}", tokens as f64 * 1000.0 / generation as f64))).unwrap_or_else(|| "-".to_string());
        let cached = match (line.u("prompt"), line.u("cached")) {
            (Some(prompt), Some(cached)) => share(cached, prompt),
            _ => "-".to_string(),
        };
        let calls: Vec<&str> = line.v["calls"].as_array().map(|calls| calls.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let _ = writeln!(out, "  {:>5} {:<10} first {:>8} gen {:>8} prompt {:>6} cached {:>4} wrote {:>6} tok at {:>6} tok/s  {}{}{}",
            short(line.req()), line.s("role"), first.map_or("-".to_string(), secs), secs(generation), line.u("prompt").map_or("-".to_string(), |tokens| tokens.to_string()), cached,
            completion.or(line.u("tokens_est")).map_or("-".to_string(), |tokens| tokens.to_string()), rate, line.s("end"), if calls.is_empty() { String::new() } else { format!(" [{}]", calls.join(", ")) },
            match line.v["retry_of"].as_str() { Some(of) => format!("  (repeats {} after {}, {})", short(of), secs(line.u("wait_ms").unwrap_or(0)), line.s("retry_why")), None => String::new() });
        if let Some(server) = line.v.get("server").filter(|server| server.is_object()) {
            let pick = |key: &str| server.get(key).and_then(Value::as_f64).map(|value| format!("{key} {value:.2}"));
            let said: Vec<String> = ["prefill_seconds", "ttft_seconds", "request_seconds", "decode_tokens_per_second"].iter().filter_map(|key| pick(key)).chain(server.get("resumed_from").and_then(Value::as_str).map(|from| format!("resumed_from {from}"))).collect();
            if !said.is_empty() {
                let _ = writeln!(out, "        server said: {}", said.join(", "));
            }
        }
        if let Some((entry, how)) = joined.get(at) {
            let drafts = entry.drafts.map_or(String::new(), |(kept, tried)| format!(", drafts {kept}/{tried}"));
            // (the engine writes its line as the request ends, and the app writes its own when it has read the stream's last word)
            let apart = line.at.zip(entry.at).map_or(String::new(), |(ours, theirs)| format!(", the app's line is {:+} ms from the engine's (the two machines' clocks differ a little)", (ours.with_timezone(&Utc) - theirs).num_milliseconds()));
            let _ = writeln!(out, "        engine ({how}): prompt {}, reused {} from {}, prefilled {} in {} ms, first token {:.2} s, {} tokens at {:.1} tok/s{drafts}{apart}", entry.prompt, entry.reused, entry.from, entry.new, entry.prefill_ms, entry.first_s, entry.tokens, entry.tokens_per_second);
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

    let _ = writeln!(out, "\ntools");
    for line in lines.iter().filter(|line| line.ev() == "tool") {
        let mut more = Vec::new();
        for key in ["path", "cmd0"] {
            if let Some(text) = line.v[key].as_str() {
                more.push(text.to_string());
            }
        }
        if let Some(code) = line.v["exit_code"].as_i64().filter(|code| *code != 0) {
            more.push(format!("exit {code}"));
        }
        more.push(format!("{} bytes out", line.u("out_bytes").unwrap_or(0)));
        if let Some(cut) = line.u("out_cut_bytes") {
            more.push(format!("{cut} of them cut"));
        }
        if let Some(wait) = line.u("wait_ms") {
            more.push(format!("{} waiting for the person", secs(wait)));
        }
        if let Some(runs) = line.v["browser_runs"].as_array() {
            let ended: Vec<String> = runs.iter().map(|run| format!("{} {} in {}", run["of"].as_str().unwrap_or("?"), run["end"].as_str().unwrap_or("?"), secs(run["ms"].as_u64().unwrap_or(0)))).collect();
            more.push(format!("{} browser runs: {}", runs.len(), ended.join("; ")));
        }
        let _ = writeln!(out, "  {:>5} {:<16} {:>8}  {:<15} {}", short(line.req()), line.s("name"), secs(line.u("ms").unwrap_or(0)), line.s("outcome"), more.join(", "));
    }

    let guards: Vec<usize> = lines.iter().enumerate().filter(|(_, line)| matches!(line.ev(), "intervention" | "compaction")).map(|(at, _)| at).collect();
    let _ = writeln!(out, "\ninterventions");
    if guards.is_empty() {
        let _ = writeln!(out, "  none");
    }
    for at in guards {
        let line = &lines[at];
        let next = lines[at + 1..].iter().find(|later| later.ev() == "request" && later.s("role") == "main");
        if line.ev() == "compaction" {
            let _ = writeln!(out, "  compaction ({}): {} tokens to {} in {}", line.s("stage"), line.u("before_tokens").unwrap_or(0), line.u("after_tokens").unwrap_or(0), secs(line.u("ms").unwrap_or(0)));
            continue;
        }
        let kind = line.s("kind");
        let stopped = lines.iter().find(|request| request.ev() == "request" && request.req() == line.req());
        let mut what = match kind {
            "draft_stop" => {
                let spent = stopped.map_or(String::new(), |request| format!(" in {}", secs(request.u("ms").unwrap_or(0))));
                format!("{} tokens thrown away{spent} ({} lines of code, {} fence marks)", line.u("tokens_before").unwrap_or(0), line.u("code_lines").unwrap_or(0), line.u("fences").unwrap_or(0))
            }
            "cut_write" => format!("{} bytes of {} cut ({}), after {} tokens", line.u("bytes").unwrap_or(0), line.s("path"), line.s("why"), line.u("tokens_before").unwrap_or(0)),
            "cost_note" => format!("{} tokens generated", line.u("generated").unwrap_or(0)),
            _ => line.v.as_object().map(|fields| fields.iter().filter(|(key, _)| !matches!(key.as_str(), "t" | "ev" | "turn" | "req" | "kind")).map(|(key, value)| format!("{key} {value}")).collect::<Vec<_>>().join(", ")).unwrap_or_default(),
        };
        if what.is_empty() {
            what = "-".to_string();
        }
        let _ = writeln!(out, "  {kind} at {}: {what}", short(line.req()));
        if let Some(next) = next {
            let _ = writeln!(out, "      next: {}", request_summary(next));
        }
    }

    circles(&mut out, &lines);
    out
}

/// Where the time of some lines went.
#[derive(Default)]
struct Spent {
    wall: u64,
    prefill: u64,
    generation: u64,
    helpers: u64,
    tools: BTreeMap<String, (u64, u64)>,
    waits: u64,
    compaction: u64,
    retries: u64,
    unfinished: bool,
}

impl Spent {
    fn of(lines: &[&Line]) -> Spent {
        let mut spent = Spent::default();
        let ended = lines.iter().find(|line| line.ev() == "turn_end");
        let timed: Vec<DateTime<FixedOffset>> = lines.iter().filter_map(|line| line.at).collect();
        spent.wall = ended.and_then(|end| end.u("ms")).unwrap_or_else(|| match (timed.first(), timed.last()) {
            (Some(first), Some(last)) => (*last - *first).num_milliseconds().max(0) as u64,
            _ => 0,
        });
        spent.unfinished = lines.iter().any(|line| line.ev() == "turn_start") && ended.is_none();
        for line in lines {
            match line.ev() {
                "request" => {
                    let (ms, first) = (line.u("ms").unwrap_or(0), line.u("first_ms").unwrap_or(0).min(line.u("ms").unwrap_or(0)));
                    match line.s("role") {
                        "main" => {
                            spent.prefill += first;
                            spent.generation += ms - first;
                        }
                        "helper" => spent.helpers += ms,
                        _ => {}
                    }
                    spent.retries += line.u("wait_ms").unwrap_or(0);
                }
                "tool" => {
                    let entry = spent.tools.entry(line.s("name").to_string()).or_default();
                    entry.0 += 1;
                    entry.1 += line.u("ms").unwrap_or(0);
                }
                "approval" => spent.waits += line.u("wait_ms").unwrap_or(0),
                "compaction" => spent.compaction += line.u("ms").unwrap_or(0),
                _ => {}
            }
        }
        spent
    }

    fn add(&mut self, other: &Spent) {
        self.wall += other.wall;
        self.prefill += other.prefill;
        self.generation += other.generation;
        self.helpers += other.helpers;
        self.waits += other.waits;
        self.compaction += other.compaction;
        self.retries += other.retries;
        for (name, (calls, ms)) in &other.tools {
            let entry = self.tools.entry(name.clone()).or_default();
            entry.0 += calls;
            entry.1 += ms;
        }
    }

    fn print(&self, out: &mut String, indent: &str) {
        let tools: u64 = self.tools.values().map(|(_, ms)| ms).sum();
        let model = self.prefill + self.generation;
        let accounted = model + tools + self.waits + self.compaction + self.retries;
        let rest = self.wall.saturating_sub(accounted);
        let _ = writeln!(out, "{indent}wall {}{}", secs(self.wall), if self.unfinished { " (the run ended here: no turn_end)" } else { "" });
        let _ = writeln!(out, "{indent}model {} ({}): prefill to the first word {}, generation {}", secs(model), share(model, self.wall), secs(self.prefill), secs(self.generation));
        let by_name: Vec<String> = self.tools.iter().map(|(name, (calls, ms))| format!("{name} {calls} x {}", secs(*ms))).collect();
        let _ = writeln!(out, "{indent}tools {} ({}){}{}", secs(tools), share(tools, self.wall), if by_name.is_empty() { String::new() } else { format!(": {}", by_name.join(", ")) }, if self.helpers > 0 { format!("; of which an explore helper's own requests {}", secs(self.helpers)) } else { String::new() });
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
    for line in tools.iter().filter(|line| matches!(line.s("name"), "create_file" | "apply_patch") && !line.s("path").is_empty()) {
        let entry = failed_edits.entry(line.s("path")).or_default();
        if line.failed() {
            entry.0 += 1;
            entry.1 = entry.1.max(entry.0);
            entry.2 += 1;
        } else {
            entry.0 = 0;
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
    // The same call again: the same tool with the same arguments (by their hash), where nothing was edited since the call before it.
    let mut since_edit: HashMap<(String, String), usize> = HashMap::new();
    let mut shown = std::collections::HashSet::new();
    for line in &tools {
        if matches!(line.s("name"), "create_file" | "apply_patch") && !line.failed() {
            since_edit.clear();
            continue;
        }
        let Some(hash) = line.v["args_hash"].as_str() else { continue };
        let count = since_edit.entry((line.s("name").to_string(), hash.to_string())).or_default();
        *count += 1;
        if *count >= 2 && shown.insert((line.s("name").to_string(), hash.to_string())) {
            found = true;
            let _ = writeln!(out, "  {} called again with the same arguments (hash {hash}), nothing edited between: at {}", line.s("name"), short(line.req()));
        } else if *count >= 3 {
            found = true;
            let _ = writeln!(out, "  ... and a {count}th time at {}", short(line.req()));
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
    fn a_run_is_told_by_its_requests_its_tools_its_stops_and_where_the_time_went() {
        let lines = [
            json!({"t": "2026-11-05T10:00:00.000+09:00", "ev": "turn_start", "turn": 1, "chars": 40, "images": 0}),
            request(1, "2026-11-05T10:01:43.000+09:00", "main", 2000, 103_000, 5000, 0, 8328, "stopped_app:draft_stop"),
            json!({"t": "2026-11-05T10:01:43.001+09:00", "ev": "intervention", "turn": 1, "req": "s/1", "kind": "draft_stop", "tokens_before": 8328, "reasoning_chars": 20000, "stops_before": 0, "code_lines": 36, "fences": 0}),
            request(2, "2026-11-05T10:02:20.000+09:00", "main", 1000, 37_000, 13_000, 4800, 2714, "tool_calls"),
            tool(2, "create_file", 40, "ok", Some("index.html"), Some("aaaaaaaaaaaa"), "h1"),
            json!({"t": "2026-11-05T10:02:21.000+09:00", "ev": "approval", "turn": 1, "req": "s/2", "tool": "execute_command", "wait_ms": 4000, "decision": "approve"}),
            json!({"t": "2026-11-05T10:02:30.000+09:00", "ev": "turn_end", "turn": 1, "cancelled": false, "requests": 2, "tools": 1, "ms": 150_000}),
        ];
        let said = report(&text(&lines), None);
        assert!(said.contains("turn 1: 40 characters"), "{said}");
        // Time: 140 s of the 150 in the model (3 of them before the first words), 40 ms of tools, 4 s of waiting.
        assert!(said.contains("wall 150.0 s") && said.contains("model 140.0 s (93%): prefill to the first word 3.0 s, generation 137.0 s"), "{said}");
        assert!(said.contains("create_file 1 x 0.0 s") && said.contains("waiting for the person 4.0 s"), "{said}");
        // Each request, with its cached share.
        assert!(said.contains("   /1 main") && said.contains("stopped_app:draft_stop") && said.contains("cached  36%"), "{said}");
        assert!(said.contains("index.html"), "{said}");
        // What the stop cost: the tokens thrown away and the time, and what the request after it did.
        assert!(said.contains("draft_stop at /1: 8328 tokens thrown away in 103.0 s (36 lines of code, 0 fence marks)"), "{said}");
        assert!(said.contains("next: /2 wrote 2714 tokens in 37.0 s, cached 36%, ended tool_calls"), "{said}");
        assert!(said.contains("signs of going round in circles\n  none"), "{said}");
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
        assert!(said.contains("the run ended here: no turn_end"), "{said}");
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
            request(3, "2026-11-05T10:01:40.000+09:00", "main", 5000, 9000, 1, 0, 20, "stop"),
        ];
        let said = report(&text(&lines), Some(&log));
        assert!(said.contains("engine (id): prompt 5000, reused 4000 from live, prefilled 1000 in 900 ms, first token 0.90 s, 300 tokens at 70.0 tok/s, drafts 150/200"), "{said}");
        assert!(said.contains("engine (time): prompt 7000, reused 0 from root, prefilled 7000 in 5000 ms"), "{said}");
        assert!(said.contains("engine: no line for this request"), "{said}");
        assert!(said.contains("2 of 3 requests joined to the engine's log: 1 by id, 1 by time"), "{said}");
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

    #[test]
    fn a_trajectory_that_is_not_all_there_is_read_as_far_as_it_goes() {
        let said = report("{\"t\":\"2026-11-05T10:00:00.000+09:00\",\"ev\":\"turn_start\",\"turn\":1,\"chars\":3,\"images\":0}\nnot json\n{\"t\":\"2026-11-05T10:00:01.000+09:00\",\"ev\":\"request\",\"turn\":1,\"req\":\"s/1\",\"role\":\"main\",\"ms\":1000,\"first_ms\":null,\"end\":\"cancelled_person\",\"completion\":null,\"tokens_est\":12", None);
        assert!(said.starts_with("1 lines") && said.contains("2 that could not be read") && said.contains("the run ended here"), "{said}");
        assert!(report("", None).starts_with("0 lines, no times"));
    }
}
