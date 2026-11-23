//! The trajectory of a run: how it went, one JSON line an event, in a file beside the session (`<session>.trajectory.jsonl`).
//!
//! The session file is for going on with a conversation. This is for measuring one: when each request began, when its first word came and when it ended, how many tokens it read,
//! reused and wrote, how each tool ended and how long it ran, how long a person was waited for, and which of the app's own guards fired and what each cost. A line is written when its
//! event ends, whole and in one write, so a run that is killed loses at most the event in flight. Nothing is buffered across events.
//!
//! It holds names, sizes, counts, times, ids, outcomes and short hashes, and never what was said: no message, no reasoning, no file content, no command. The session file has those, and
//! a request id (`<session id>/<n>`) says which of its messages a line is about: a request line says, as `message`, the place of the assistant message it made among the session's messages, and a
//! tool line the id of its call as `id`, which is the call's in that message. The one exception is a `failed` line, which holds the one line a server said of its failure (cut, and with its
//! keys and headers hidden): it is the server's own error, and what a person who sends the trajectory is asked for.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::{DateTime, Local, SecondsFormat};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::llm::{LlmError, Usage};

const SUFFIX: &str = "trajectory.jsonl";

/// The trajectory of a session, beside it: with the session's other companions it goes when the session does (`checkpoint::remove_for`).
pub fn file_of(session: &Path) -> PathBuf {
    session.with_extension(SUFFIX)
}

/// Whether a file of the sessions' folder is a trajectory and not a session.
pub fn is_trajectory(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.ends_with(".trajectory.jsonl"))
}

/// With a session that is deleted.
pub fn remove_for(session: &Path) {
    let _ = fs::remove_file(file_of(session));
}

/// A time as every line has it: RFC 3339 to the millisecond, with the offset.
pub fn stamp(at: DateTime<Local>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, false)
}

pub fn now() -> String {
    stamp(Local::now())
}

/// A time as a file gives it (RFC 3339: an engine's log says its times in UTC), in this PC's zone as [`now`] writes it. None: not such a time.
pub fn local(time: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(time).ok().map(|at| stamp(at.with_timezone(&Local)))
}

/// A short hash of a file's bytes, to tell whether a file has come back to a state it had: twelve hex digits, which say nothing of what it holds.
pub fn short_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().take(6).map(|byte| format!("{byte:02x}")).collect()
}

/// Where a request began and when its first word came: the first of reasoning, content or a call being written. And when it began to write a
/// call and its answer, which split the time after the first word between thinking and writing (the timeline's band).
pub struct Clock {
    pub began: Instant,
    pub at: DateTime<Local>,
    first: Option<Instant>,
    writing: Option<Instant>,
    answering: Option<Instant>,
}

impl Clock {
    pub fn start() -> Clock {
        Clock { began: Instant::now(), at: Local::now(), first: None, writing: None, answering: None }
    }

    /// Called with each piece that comes; the first one counts.
    pub fn word(&mut self) {
        self.first.get_or_insert_with(Instant::now);
    }

    /// Called with each piece of a call; the first one counts.
    pub fn writing(&mut self) {
        self.writing.get_or_insert_with(Instant::now);
    }

    /// Called with each piece of the answer's words; the first one counts.
    pub fn answering(&mut self) {
        self.answering.get_or_insert_with(Instant::now);
    }

    pub fn first_ms(&self) -> Option<u64> {
        self.first.map(|first| first.duration_since(self.began).as_millis() as u64)
    }

    pub fn writing_ms(&self) -> Option<u64> {
        self.writing.map(|at| at.duration_since(self.began).as_millis() as u64)
    }

    pub fn answer_ms(&self) -> Option<u64> {
        self.answering.map(|at| at.duration_since(self.began).as_millis() as u64)
    }

    pub fn ms(&self) -> u64 {
        self.began.elapsed().as_millis() as u64
    }
}

/// How a request ended, as `end` says it: `stop`, `tool_calls`, `length`, or `error:<kind>` for what the provider failed with. What ended it from outside (the person's Stop,
/// the app's own stop) is not in what the provider returned, so the caller says it.
pub fn end_of(result: &Result<crate::llm::Completion, LlmError>) -> String {
    match result {
        Ok(completion) if completion.finish.is_empty() && completion.tool_calls.is_empty() && completion.content.is_empty() => "error:unfinished".into(),
        Ok(completion) if completion.finish.is_empty() => "stop".into(),
        Ok(completion) => completion.finish.clone(),
        Err(LlmError::Cancelled) => "cancelled_person".into(),
        Err(LlmError::Transient(_)) => "error:transient".into(),
        Err(LlmError::Overflow(_)) => "error:overflow".into(),
        Err(LlmError::Rejected(_)) => "error:rejected".into(),
        Err(LlmError::Down(_)) => "error:down".into(),
        Err(LlmError::NoVision(_)) => "error:no_vision".into(),
    }
}

/// What the server's own `timings` came to, as it came: its numbers, and its short strings (`resumed_from`). Whatever else is in it (lists, nested counters) is left out.
pub fn kept_timings(timings: &Value) -> Option<Value> {
    let fields = timings.as_object()?;
    let kept: Map<String, Value> = fields.iter().filter(|(_, value)| value.is_number() || value.is_boolean() || value.as_str().is_some_and(|text| text.chars().count() < 32)).map(|(name, value)| (name.clone(), value.clone())).collect();
    (!kept.is_empty()).then_some(Value::Object(kept))
}

/// One request to a model, as `request` records it.
pub struct RequestNote<'a> {
    pub id: &'a str,
    /// `main`, `helper` (an `explore`'s own), `checkpoint` (a compaction's summary) or `warm` (the engine reading the prompt ahead).
    pub role: &'static str,
    pub clock: &'a Clock,
    pub end: String,
    pub usage: Option<&'a Usage>,
    pub timings: Option<&'a Value>,
    pub reasoning_chars: usize,
    pub content_chars: usize,
    pub calls: Vec<String>,
    /// What the stream's own pieces came to, counted roughly: for a request that ended with no count from the server.
    pub tokens_est: Option<u64>,
    /// The request this one repeats, why, and how long it waited: after a connection that dropped, or an answer with nothing in it.
    pub retry: Option<(String, &'static str, u64)>,
    /// Whether the app was watching this request's reasoning for a draft: `on`, or `off:` and why (the person asked for no change, a file has been written, it has stopped one this turn).
    pub draft_watch: Option<&'static str>,
    /// For a helper's request: the name of the file its conversation is kept in, beside the session.
    pub helper_file: Option<&'a str>,
    /// Where the draft watch saw its reasoning come to a mark (its fenced lines past 40, 60 and 80, a line saying it writes the code now): the
    /// characters and seconds into the reasoning, for a later look at where a draft could have been stopped.
    pub draft_marks: Option<Value>,
}

/// Provider-reported usage of the main conversation across all of its turns.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SessionUsage {
    pub requests: u64,
    pub prompt: u64,
    pub completion: u64,
    pub cached: u64,
    /// Prompt tokens from requests whose provider supplied a cache count.
    pub cache_prompt: u64,
    pub last_rate: Option<f64>,
    /// Cache reuse of the last completed main request, even when it reported no usage.
    pub last_cache_percent: Option<u64>,
}

impl SessionUsage {
    fn record_request(&mut self, fields: &Value) {
        if fields["role"] != "main" {
            return;
        }
        // A stopped or failed request leaves the previous completed request on the status bar.
        // An empty answer still came back from the provider; the live worker reports its cache data.
        if let Some(end) = fields["end"].as_str() {
            if end == "error:unfinished" || !(end.starts_with("error:") || end.starts_with("stopped_app:") || end == "cancelled_person") {
                self.last_cache_percent = fields["prompt"].as_u64().and_then(|prompt| crate::llm::cache_percent(prompt, fields["cached"].as_u64()));
            }
        }
        let (Some(prompt), Some(completion)) = (fields["prompt"].as_u64(), fields["completion"].as_u64()) else { return };
        // Providers can send timing information with empty token counters.
        if prompt == 0 && completion == 0 {
            return;
        }
        self.requests = self.requests.saturating_add(1);
        self.prompt = self.prompt.saturating_add(prompt);
        self.completion = self.completion.saturating_add(completion);
        if let Some(cached) = fields["cached"].as_u64() {
            self.cache_prompt = self.cache_prompt.saturating_add(prompt);
            self.cached = self.cached.saturating_add(cached.min(prompt));
        }
        self.last_rate = fields["tps"].as_f64();
    }

    pub fn cache_percent(&self) -> Option<u64> {
        crate::llm::cache_percent(self.cache_prompt, Some(self.cached))
    }
}

pub struct Trajectory {
    file: Option<File>,
    session_id: String,
    pub turn: u64,
    pub usage: SessionUsage,
    requests: u64,
    warm: u64,
    /// What happened before the session had a file (the engine warming up), kept for it.
    held: Vec<String>,
    /// This turn's requests and tools, for its end.
    pub turn_requests: u64,
    pub turn_tools: u64,
    /// A request's line that waits for the message it made ([`Trajectory::message_at`]): written with it, or as it is before whatever line comes next.
    waiting: Option<(String, Value)>,
}

impl Default for Trajectory {
    fn default() -> Trajectory {
        Trajectory::new()
    }
}

/// How many lines wait for a session's file, at the most.
const HELD_MOST: usize = 64;

impl Trajectory {
    pub fn new() -> Trajectory {
        Trajectory { file: None, session_id: String::new(), turn: 0, usage: SessionUsage::default(), requests: 0, warm: 0, held: Vec::new(), turn_requests: 0, turn_tools: 0, waiting: None }
    }

    /// Begins writing beside this session. A session that is opened again goes on where its file left off: its turns and its requests keep counting.
    pub fn open(&mut self, session: &Path) {
        let path = file_of(session);
        self.session_id = session.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
        (self.turn, self.requests) = (0, 0);
        self.usage = SessionUsage::default();
        let mut ends_a_line = true;
        if let Ok(bytes) = fs::read(&path) {
            ends_a_line = bytes.last().is_none_or(|last| *last == b'\n');
            for line in String::from_utf8_lossy(&bytes).lines() {
                let Ok(event) = serde_json::from_str::<Value>(line) else { continue };
                if event["ev"] == "request" {
                    self.usage.record_request(&event);
                }
                self.turn = self.turn.max(event.get("turn").and_then(Value::as_u64).unwrap_or(0));
                if let Some((id, number)) = event.get("req").and_then(Value::as_str).and_then(|req| req.rsplit_once('/')) {
                    if id == self.session_id {
                        self.requests = self.requests.max(number.parse().unwrap_or(0));
                    }
                }
            }
        }
        self.file = OpenOptions::new().create(true).append(true).open(&path).ok();
        // (after a line a crash cut short, on a line of its own)
        if !ends_a_line {
            self.put("\n");
        }
        for line in std::mem::take(&mut self.held) {
            self.put(&line);
        }
    }

    /// No session: what follows waits for the next.
    pub fn close(&mut self) {
        self.write_waiting();
        self.file = None;
        self.session_id.clear();
        self.usage = SessionUsage::default();
        (self.turn, self.requests, self.turn_requests, self.turn_tools) = (0, 0, 0, 0);
    }

    pub fn is_open(&self) -> bool {
        self.file.is_some()
    }

    /// The id of the next request, which is the same string the server is given as `X-Request-Id`: `<session id>/<n>`, counting the session's requests from 1. Before the session has
    /// a name, the engine's warm-up is `warm/<n>`.
    pub fn request_id(&mut self) -> String {
        if self.session_id.is_empty() {
            self.warm += 1;
            return format!("warm/{}", self.warm);
        }
        self.requests += 1;
        format!("{}/{}", self.session_id, self.requests)
    }

    /// A new turn of the person's.
    pub fn begin_turn(&mut self) -> u64 {
        self.turn += 1;
        (self.turn_requests, self.turn_tools) = (0, 0);
        self.turn
    }

    fn put(&mut self, text: &str) {
        if let Some(file) = &mut self.file {
            // (in one write: a crash then cuts at most this line; and a trajectory that cannot be written is no reason to stop a run)
            let _ = file.write_all(text.as_bytes());
        }
    }

    /// One line: its kind, the time, the turn, and the request it belongs to when it does, then its own fields.
    pub fn write(&mut self, ev: &str, req: Option<&str>, fields: Value) {
        // (a request's line that waits for its message is written first, as it is: it made none, and the order of the lines is the order of what happened)
        self.write_waiting();
        self.write_line(ev, req, fields);
    }

    fn write_line(&mut self, ev: &str, req: Option<&str>, fields: Value) {
        let mut line = Map::new();
        line.insert("t".into(), json!(now()));
        line.insert("ev".into(), json!(ev));
        line.insert("turn".into(), json!(self.turn));
        if let Some(req) = req {
            line.insert("req".into(), json!(req));
        }
        if let Value::Object(fields) = fields {
            line.extend(fields);
        }
        let mut text = Value::Object(line).to_string();
        text.push('\n');
        if self.file.is_some() {
            self.put(&text);
        } else if self.held.len() < HELD_MOST {
            self.held.push(text);
        }
    }

    pub fn request(&mut self, note: RequestNote) {
        let id = note.id.to_string();
        let fields = self.request_fields(note);
        self.write("request", Some(&id), fields);
    }

    /// A request whose line says where the message it made is in the session: it waits for [`Trajectory::message_at`], or for the next line
    /// to be written (it made none). Said once the message is in, where it is cannot be wrong: the reader no longer has to guess which it was.
    pub fn request_waiting(&mut self, note: RequestNote) {
        self.write_waiting();
        let id = note.id.to_string();
        self.waiting = Some((id, self.request_fields(note)));
    }

    /// The message the request that waits made is at `index` of the session's messages: its line is written with that.
    pub fn message_at(&mut self, index: usize) {
        if let Some((id, mut fields)) = self.waiting.take() {
            fields["message"] = json!(index);
            self.write_line("request", Some(&id), fields);
        }
    }

    fn write_waiting(&mut self) {
        if let Some((id, fields)) = self.waiting.take() {
            self.write_line("request", Some(&id), fields);
        }
    }

    fn request_fields(&mut self, note: RequestNote) -> Value {
        if note.role != "warm" {
            self.turn_requests += 1;
        }
        let (prompt, cached, completion, tps) = match note.usage {
            Some(usage) => (json!(usage.prompt), json!(usage.cached), json!(usage.completion), json!(usage.tokens_per_second.map(|rate| (rate * 10.0).round() / 10.0))),
            None => (Value::Null, Value::Null, Value::Null, Value::Null),
        };
        let mut fields = json!({
            "role": note.role,
            "started": stamp(note.clock.at),
            "first_ms": note.clock.first_ms(),
            "ms": note.clock.ms(),
            "prompt": prompt,
            "cached": cached,
            "completion": completion,
            "tps": tps,
            "tokens_est": note.tokens_est,
            "reasoning_chars": note.reasoning_chars,
            "content_chars": note.content_chars,
            "calls": note.calls,
            "end": note.end,
        });
        self.usage.record_request(&fields);
        if let Some(watch) = note.draft_watch {
            fields["draft_watch"] = json!(watch);
        }
        if let Some(file) = note.helper_file {
            fields["helper_file"] = json!(file);
        }
        if let Some(marks) = note.draft_marks {
            fields["draft_marks"] = marks;
        }
        // (when it began to write a call and its answer, from its start: what is before them after the first word is thinking)
        if let Some(writing) = note.clock.writing_ms() {
            fields["writing_ms"] = json!(writing);
        }
        if let Some(answer) = note.clock.answer_ms() {
            fields["answer_ms"] = json!(answer);
        }
        if let Some((again, why, wait_ms)) = note.retry {
            fields["retry_of"] = json!(again);
            fields["retry_why"] = json!(why);
            fields["wait_ms"] = json!(wait_ms);
        }
        if let Some(server) = note.timings.and_then(kept_timings) {
            fields["server"] = server;
        }
        fields
    }

    /// An intervention of the app's own, with what it came to. `req`: the request it ended or followed.
    pub fn intervention(&mut self, kind: &str, req: Option<&str>, fields: Value) {
        // (the kind first, as the line is read)
        let mut ordered = Map::new();
        ordered.insert("kind".into(), json!(kind));
        if let Value::Object(rest) = fields {
            ordered.extend(rest);
        }
        self.write("intervention", req, Value::Object(ordered));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trajectory_is_beside_its_session_and_is_not_one() {
        let session = Path::new("C:/data/sessions/20261105-120000.jsonl");
        assert_eq!(file_of(session), Path::new("C:/data/sessions/20261105-120000.trajectory.jsonl"));
        assert!(is_trajectory(&file_of(session)) && !is_trajectory(session));
    }

    #[test]
    fn a_time_is_to_the_millisecond_with_its_offset() {
        let at = stamp(Local::now());
        let shape = regex::Regex::new(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}([+-]\d\d:\d\d|Z)$").unwrap();
        assert!(shape.is_match(&at), "{at}");
    }

    #[test]
    fn requests_are_counted_from_one_in_a_session_and_go_on_when_it_is_opened_again() {
        let dir = crate::in_container::tempdir().unwrap();
        let session = dir.path().join("20261105-120000.jsonl");
        let mut traj = Trajectory::new();
        assert_eq!(traj.request_id(), "warm/1", "before a session has a name");
        traj.open(&session);
        assert_eq!(traj.request_id(), "20261105-120000/1");
        traj.begin_turn();
        traj.write("turn_start", None, json!({"chars": 3}));
        let second = traj.request_id();
        traj.write("request", Some(&second), json!({"end": "stop"}));
        drop(traj);
        let mut again = Trajectory::new();
        again.open(&session);
        assert_eq!(again.turn, 1);
        assert_eq!(again.request_id(), "20261105-120000/3", "the second was written, and the numbers do not begin again");
    }

    #[test]
    fn resumed_usage_adds_metered_main_requests_once_and_weights_only_reported_cache() {
        let dir = crate::in_container::tempdir().unwrap();
        let session = dir.path().join("s.jsonl");
        let mut traj = Trajectory::new();
        traj.open(&session);
        traj.write("request", None, json!({"role":"main","prompt":500,"completion":3}));
        traj.write("request", None, json!({"role":"main","tokens_est":300}));
        traj.write("request", None, json!({"role":"main","prompt":0,"completion":0,"tps":37.0}));
        traj.write("request", None, json!({"role":"helper","prompt":800,"cached":800,"completion":5}));
        traj.write("turn_end", None, json!({"role":"main","prompt":200,"completion":1}));
        traj.close();
        traj.open(&session);
        assert_eq!(traj.usage.requests, 1);
        assert_eq!(traj.usage.cache_percent(), None);

        let clock = Clock::start();
        let missed = Usage { prompt: 10, cached: Some(0), completion: 5, ..Usage::default() };
        let note = RequestNote { id: "s/2", role: "main", clock: &clock, end: "stop".into(), usage: Some(&missed), timings: None, reasoning_chars: 0, content_chars: 0, calls: vec![], tokens_est: None, retry: None, draft_watch: None, helper_file: None, draft_marks: None };
        traj.request_waiting(note);
        assert_eq!(traj.usage.cache_percent(), Some(0));
        traj.message_at(4);
        let hit = Usage { prompt: 90, cached: Some(90), completion: 7, tokens_per_second: Some(19.5), ..Usage::default() };
        traj.request_waiting(RequestNote { id: "s/3", role: "main", clock: &clock, end: "stop".into(), usage: Some(&hit), timings: None, reasoning_chars: 0, content_chars: 0, calls: vec![], tokens_est: None, retry: None, draft_watch: None, helper_file: None, draft_marks: None });
        let expected = SessionUsage { requests: 3, prompt: 600, completion: 15, cached: 90, cache_prompt: 100, last_rate: Some(19.5), last_cache_percent: Some(100) };
        assert_eq!(traj.usage, expected);
        assert_eq!(traj.usage.cache_percent(), Some(90));
        traj.close();
        assert_eq!(traj.usage, SessionUsage::default());
        traj.open(&session);
        assert_eq!(traj.usage, expected);
    }

    #[test]
    fn resumed_cache_follows_the_last_completed_main_request_in_each_session() {
        let dir = crate::in_container::tempdir().unwrap();
        let session = dir.path().join("a.jsonl");
        let mut traj = Trajectory::new();
        traj.open(&session);
        let request = |role: &str, end: &str, cached: Option<u64>| json!({"role":role,"end":end,"prompt":100,"completion":8,"cached":cached});
        traj.write("request", None, request("main", "stop", Some(25)));
        traj.write("request", None, request("main", "cancelled_person", None));
        traj.write("request", None, request("main", "error:transient", None));
        traj.write("request", None, request("main", "stopped_app:draft_stop", None));
        traj.write("request", None, request("warm", "stop", Some(100)));
        traj.write("request", None, request("helper", "stop", Some(100)));
        traj.close();
        traj.open(&session);
        assert_eq!(traj.usage.last_cache_percent, Some(25));
        traj.write("request", None, request("main", "tool_calls", Some(0)));
        traj.close();
        traj.open(&session);
        assert_eq!(traj.usage.last_cache_percent, Some(0));
        traj.write("request", None, json!({"role":"main","end":"stop","tokens_est":12}));
        traj.close();
        traj.open(&session);
        assert_eq!(traj.usage.last_cache_percent, None);
        assert!(traj.usage.cache_percent().is_some());
        traj.close();
        traj.open(&dir.path().join("b.jsonl"));
        assert_eq!(traj.usage, SessionUsage::default());
    }

    #[test]
    fn what_happens_before_a_session_has_a_file_waits_for_it() {
        let dir = crate::in_container::tempdir().unwrap();
        let session = dir.path().join("s.jsonl");
        let mut traj = Trajectory::new();
        traj.write("request", Some("warm/1"), json!({"role": "warm"}));
        assert!(!file_of(&session).exists());
        traj.open(&session);
        let text = fs::read_to_string(file_of(&session)).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("\"warm/1\""));
    }

    #[test]
    fn the_servers_timings_keep_their_numbers_and_short_strings_and_nothing_else() {
        let kept = kept_timings(&json!({"ttft_seconds": 1.5, "resumed_from": "live", "long": "x".repeat(40), "prefill_experts": {"vram": 1}, "engine_draft_widths": [1, 2], "images": 2})).unwrap();
        assert_eq!(kept, json!({"ttft_seconds": 1.5, "resumed_from": "live", "images": 2}));
        assert_eq!(kept_timings(&json!({"a": [1]})), None);
        assert_eq!(kept_timings(&Value::Null), None);
    }

    #[test]
    fn a_request_says_where_the_message_it_made_is_and_a_request_that_made_none_says_nothing() {
        let dir = crate::in_container::tempdir().unwrap();
        let session = dir.path().join("s.jsonl");
        let mut traj = Trajectory::new();
        traj.open(&session);
        let clock = Clock::start();
        let note = |id: &'static str| RequestNote { id, role: "main", clock: &clock, end: "tool_calls".into(), usage: None, timings: None, reasoning_chars: 0, content_chars: 0, calls: vec!["read_file".into()], tokens_est: Some(0), retry: None, draft_watch: None, helper_file: None, draft_marks: None };
        traj.request_waiting(note("s/1"));
        traj.message_at(2);
        // (the second makes none: the next line writes it as it is, before itself)
        traj.request_waiting(note("s/2"));
        traj.write("turn_end", None, json!({}));
        let lines: Vec<Value> = fs::read_to_string(file_of(&session)).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(lines[0]["message"], 2);
        assert!(lines[1].get("message").is_none(), "{}", lines[1]);
        assert_eq!((lines[1]["ev"].as_str(), lines[2]["ev"].as_str()), (Some("request"), Some("turn_end")), "in the order they happened");
    }

    #[test]
    fn a_session_cut_in_the_middle_of_a_line_goes_on_on_a_line_of_its_own() {
        let dir = crate::in_container::tempdir().unwrap();
        let session = dir.path().join("s.jsonl");
        fs::write(file_of(&session), "{\"t\":\"x\",\"ev\":\"turn_st").unwrap();
        let mut traj = Trajectory::new();
        traj.open(&session);
        traj.write("turn_end", None, json!({}));
        let text = fs::read_to_string(file_of(&session)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(serde_json::from_str::<Value>(lines[1]).is_ok());
    }
}
