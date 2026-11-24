//! The path a session took, turn by turn: for a panel that says what the model and the app are doing, and where they are heading.
//!
//! It is read from two files that are kept apart on purpose. The trajectory (`trajectory`) has the times, the sizes, the outcomes and the ids, and never what was said; the session has what was said.
//! Here they are put together: a turn is the person's request (their own words, the first line of them), then the steps taken in order. A model step is one request to the model with the calls
//! it made, and its `why` is the model's own words about it, never a sentence made up for it. The app's own steps (a compaction, a guard that fired, a request asked again, an approval waited for, a
//! wake) sit among them as steps of another kind. Above the path is where it is heading: the plan the person approved, or the checklist the model kept, or nothing.
//!
//! A request is put beside its message by the `message` its line says, when that place is still what it was and the message is what the line says of it (its calls, the length of its words); and, for a
//! trajectory from before that was written, or where the place has moved (a compaction, a conversation cut back), by order within the turn. A call is put beside its tool line by its id.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::Path;

use chrono::{DateTime, Duration, SecondsFormat};
use serde_json::{Map, Value};

use crate::agent::{first_word_of, plan_outcome, PlanOutcome};
use crate::message::{Message, ToolCall};
use crate::prompt;
use crate::session;
use crate::tools::apply_todo;
use crate::trajectory;
use crate::trajectory_report::{load, Line};

/// How long a sentence is shown, in characters.
const SENTENCE_CHARS: usize = 120;
/// How long what the person asked is shown, in characters.
const ASKED_CHARS: usize = 120;
/// How long a plan's item is shown, in characters.
const ITEM_CHARS: usize = 160;
/// How long a call's target is shown, in characters.
const TARGET_CHARS: usize = 80;
/// What each end of a long output (or of long arguments) keeps, in bytes.
const END_BYTES: usize = 2048;
/// Arguments up to this are kept whole.
const ARGUMENTS_WHOLE: usize = 8192;

/// The whole of it: the turns of a session, and where its work is heading.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Timeline {
    pub turns: Vec<TurnPath>,
    pub heading: Option<Heading>,
}

/// One turn: what was asked and the steps taken.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnPath {
    pub turn: u64,
    /// The first line of what the person wrote, trimmed and cut to a length. Empty for a turn nobody sent (`woken`), and for one whose message is gone from the session (a compaction took it).
    pub asked: String,
    /// Nobody sent it: a background command's end, a line it printed or a wait woke the session (the `Wake` steps say which).
    pub woken: bool,
    /// When the turn began, as the trajectory says it.
    pub started: String,
    /// The pictures the person sent with it: their files in the session's folder of pictures ([`crate::images::folder_of`]).
    pub asked_images: Vec<String>,
    pub steps: Vec<Step>,
    /// None: the run ended in it (it was killed, or it is going on).
    pub ended: Option<TurnEnd>,
}

/// How a turn ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnEnd {
    pub ms: u64,
    /// The person's Stop.
    pub cancelled: bool,
    pub failed: bool,
}

/// What a step is. The app's own steps are not the model's: the person is shown which is which.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum StepKind {
    /// One main request to the model, with the calls it made.
    #[default]
    Model,
    /// The session made shorter: `stage` as the trajectory says it (`checkpoint`, `trim`, `truncate`...), and whether it came to anything.
    Compaction { stage: String, ok: bool },
    /// A guard of the app's that fired (`draft_stop`, `cut_write`, `check_nudge`, `no_change_guard`...).
    Intervention { kind: String },
    /// A request asked again, after the wait: `why` is `transient`, `overflow`, `empty` or `server_restart`.
    Retry { why: String, wait_ms: u64 },
    /// The person was asked to approve a call, and answered: `outcome` is their decision.
    Approval { tool: String, outcome: String, wait_ms: u64 },
    /// What woke a turn nobody sent: `command_ended`, `command_line` or `wait`.
    Wake { cause: String },
    /// The turn failed: `kind` as the trajectory says it.
    Failed { kind: String },
}

/// A step of a turn.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Step {
    pub kind: StepKind,
    /// When it began, as the trajectory says times (RFC 3339 to the millisecond).
    pub started: String,
    /// A model step: its request's time. A compaction: its time. A retry or an approval: the wait. The others take no time.
    pub ms: u64,
    /// A model step's one line of why, from the model's own words.
    pub why: Option<Why>,
    /// A model step's main request; a compaction's last checkpoint request.
    pub request: Option<RequestDetail>,
    /// A model step's calls, in the order the model made them.
    pub calls: Vec<CallRow>,
    /// The rest of what the trajectory says of a step of the app's (its numbers, the paths and sizes of a guard): ids, sizes and times, as the trajectory has them.
    pub fields: Map<String, Value>,
}

/// Where a `Why` comes from: only the first two are the model's words as they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhySource {
    /// The first sentence of what the model said.
    Content,
    /// The last sentence of its reasoning: models often end with what they will look at next.
    Reasoning,
    /// Said by neither: the tool and what it is on (a file, a program, an address), as the first call has them. The panel marks it as derived.
    Target,
}

/// A step's why, in a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Why {
    pub text: String,
    pub source: WhySource,
}

/// A request's numbers: the tokens, the times and what the server said of its own work.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestDetail {
    /// `<session id>/<n>`.
    pub id: String,
    /// `main`, or `checkpoint` for a compaction's.
    pub role: String,
    pub started: String,
    /// The time to the first word, where it was told.
    pub first_ms: Option<u64>,
    pub ms: u64,
    pub prompt: Option<u64>,
    pub cached: Option<u64>,
    pub completion: Option<u64>,
    pub tps: Option<f64>,
    /// How it ended (`stop`, `tool_calls`, `length`, `error:transient`, `cancelled_person`, `stopped_app:draft_stop`...).
    pub end: String,
    pub reasoning_chars: u64,
    pub content_chars: u64,
    /// When it began to write a call and its answer, from its start (trajectories from before these were kept have neither).
    pub writing_ms: Option<u64>,
    pub answer_ms: Option<u64>,
    /// What the server said of its work, as `trajectory::kept_timings` keeps it (`prefill_seconds`, `ttft_seconds`, `cached_tokens`, `resumed_from`...).
    pub server: BTreeMap<String, Value>,
}

/// A call the model made, with what became of it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallRow {
    pub id: String,
    pub name: String,
    /// A file, a command's program (its first word), an address or what is searched for.
    pub target: Option<String>,
    pub started: String,
    pub ms: u64,
    /// How long the person was waited for within the call, where they were.
    pub wait_ms: Option<u64>,
    /// `ok`, `error`, `timeout`, `declined_person`... Empty when the trajectory has no line for the call: it did not run, or the run ended first.
    pub outcome: String,
    pub exit_code: Option<i64>,
    /// As the model wrote them (JSON); long ones keep their start and end. None when the session has no message for the call.
    pub arguments: Option<String>,
    /// What the call brought back, as the model read it; a long one keeps the start and the end. None when the session has no result for it.
    pub output: Option<String>,
    /// For an `explore`: the file its helper's conversation is kept in, beside the session.
    pub helper_file: Option<String>,
    /// For an `explore`: the steps its helper took, read from that file by [`read`] (none where it is not there).
    pub helper_steps: Vec<HelperStep>,
    /// The pictures its result opened for the model to see (as `asked_images`).
    pub images: Vec<String>,
}

impl CallRow {
    /// A successful question or plan review returned only after the person answered.
    pub fn answered_by_person(&self) -> bool {
        crate::trajectory_report::answered_by_person(&self.name, &self.outcome)
    }
}

/// A step of an `explore` helper's own conversation: why, in its words, and the calls it made (each its tool and what it was on).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HelperStep {
    pub why: Option<Why>,
    pub calls: Vec<(String, Option<String>)>,
}

/// The steps of a helper's conversation, from its messages: one for each answer of its model.
pub fn helper_steps(messages: &[Message]) -> Vec<HelperStep> {
    messages.iter().filter(|message| message.role == "assistant").map(|message| HelperStep {
        why: why_of(message),
        calls: message.tool_calls.iter().flatten().map(|call| (call.function.name.clone(), target_of(&call.function.arguments))).collect(),
    }).collect()
}

/// Where the work is heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heading {
    /// The last plan the person approved: its list items, or its lines when it has no list.
    Plan { items: Vec<String> },
    /// The checklist the model kept, as its `todo` calls left it.
    Todos { items: Vec<TodoItem> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub text: String,
    /// `pending`, `doing`, `done` or `blocked`.
    pub status: String,
}

/// A session's timeline, from its file and the trajectory beside it. A session that has no trajectory has no turns, and that is no error.
pub fn read(session: &Path) -> Result<Timeline, String> {
    let messages = session::read(session)?;
    let path = trajectory::file_of(session);
    let trajectory = match std::fs::read(&path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let mut timeline = build(&messages, &trajectory);
    // (an explore's helper keeps its conversation in a file of its own beside the session: what it did is read from there)
    for call in timeline.turns.iter_mut().flat_map(|turn| turn.steps.iter_mut()).flat_map(|step| step.calls.iter_mut()) {
        // (a name alone: the trajectory's word is not a way to another folder)
        if let Some(file) = call.helper_file.as_deref().filter(|file| !file.contains(['/', '\\', ':'])) {
            if let Ok(helper) = session::read(&session.with_file_name(file)) {
                call.helper_steps = helper_steps(&helper);
            }
        }
    }
    Ok(timeline)
}

/// The timeline of a conversation's messages and its trajectory (the file's text).
pub fn build(messages: &[Message], trajectory: &str) -> Timeline {
    let (lines, _) = load(trajectory);
    let trusted = trusted_places(&lines);
    let mut by_turn: BTreeMap<u64, Vec<(usize, &Line)>> = BTreeMap::new();
    for (at, line) in lines.iter().enumerate() {
        // (what comes before the first turn is the connection and the engine's warm-up)
        if line.turn() > 0 {
            by_turn.entry(line.turn()).or_default().push((at, line));
        }
    }
    let starts: Vec<Option<(bool, u64)>> = by_turn.values().map(|in_turn| in_turn.iter().find(|(_, line)| line.ev() == "turn_start").map(|(_, line)| (line.v["woken"] == true, line.u("chars").unwrap_or(0)))).collect();
    let ranges = turn_ranges(messages, &starts);
    let turns = by_turn.iter().zip(ranges).map(|((turn, in_turn), range)| path_of(*turn, in_turn, range, messages, &trusted)).collect();
    Timeline { turns, heading: heading_of(messages) }
}

/// The last sentence of a text: the end of what a model reasoned says what it is to do next. Sentences end at 。．.!?！？ and at a line's end; a full stop in a name or a number (`a.rs`, `3.14`)
/// does not end one. A code block is not a sentence, and a list's bullet or a heading's marks are not part of one. Cut to a length, with `…`; never put in other words.
pub fn last_sentence(text: &str) -> Option<String> {
    // (an unclosed block at the end is code being written: the sentence is what came before it)
    let mut inside = text.lines().filter(|line| is_fence(line)).count() % 2 == 1;
    for line in text.lines().rev() {
        if is_fence(line) {
            inside = !inside;
        } else if !inside {
            if let Some(sentence) = sentences_of(line).pop() {
                return Some(cap(&sentence, SENTENCE_CHARS));
            }
        }
    }
    None
}

/// The first sentence of a text, as `last_sentence` reads them.
pub fn first_sentence(text: &str) -> Option<String> {
    let mut inside = false;
    for line in text.lines() {
        if is_fence(line) {
            inside = !inside;
        } else if !inside {
            if let Some(sentence) = sentences_of(line).into_iter().next() {
                return Some(cap(&sentence, SENTENCE_CHARS));
            }
        }
    }
    None
}

fn is_fence(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("```") || line.starts_with("~~~")
}

fn is_terminator(c: char) -> bool {
    matches!(c, '。' | '．' | '.' | '!' | '?' | '！' | '？')
}

fn is_closer(c: char) -> bool {
    matches!(c, ')' | ']' | '}' | '"' | '\'' | '”' | '’' | '」' | '』' | '）' | '】' | '》' | '〉')
}

/// The sentences of one line, without the marks of Markdown that open it.
fn sentences_of(line: &str) -> Vec<String> {
    let chars: Vec<char> = unmarked(line).chars().collect();
    let (mut out, mut start, mut at) = (Vec::new(), 0, 0);
    while at < chars.len() {
        if is_terminator(chars[at]) {
            let mut end = at + 1;
            while end < chars.len() && (is_terminator(chars[end]) || is_closer(chars[end])) {
                end += 1;
            }
            // (a full stop that is not followed by a space is inside a name or a number; the full-width ones end a sentence wherever they stand)
            if !chars[at].is_ascii() || chars.get(end).is_none_or(|next| next.is_whitespace()) {
                let sentence: String = chars[start..end].iter().collect();
                if sentence.chars().any(char::is_alphanumeric) {
                    out.push(sentence.trim().to_string());
                }
                (start, at) = (end, end);
                continue;
            }
        }
        at += 1;
    }
    let rest: String = chars[start..].iter().collect();
    if rest.chars().any(char::is_alphanumeric) {
        out.push(rest.trim().to_string());
    }
    out
}

/// A line without the marks of Markdown that open it: a heading's hashes, a quote's bracket, a list's bullet or number, a checkbox.
fn unmarked(line: &str) -> &str {
    let mut text = line.trim();
    let hashes = text.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) {
        if let Some(rest) = text[hashes..].strip_prefix(' ') {
            text = rest.trim_start();
        }
    }
    if let Some(rest) = text.strip_prefix('>') {
        text = rest.trim_start();
    }
    if let Some(rest) = list_marker(text) {
        text = rest;
    }
    for check in ["[ ] ", "[x] ", "[X] "] {
        if let Some(rest) = text.strip_prefix(check) {
            return rest.trim_start();
        }
    }
    text
}

/// What follows a list's bullet or number, when the text opens with one.
fn list_marker(text: &str) -> Option<&str> {
    for bullet in ["- ", "* ", "+ ", "• "] {
        if let Some(rest) = text.strip_prefix(bullet) {
            return Some(rest.trim_start());
        }
    }
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let rest = &text[digits..];
    rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")).map(str::trim_start)
}

/// At most `most` characters of a text, with `…` where it was cut.
fn cap(text: &str, most: usize) -> String {
    if text.chars().count() <= most {
        return text.to_string();
    }
    format!("{}…", text.chars().take(most).collect::<String>().trim_end())
}

/// A long text as its two ends, with a word between them for what is left out.
fn ends_of(text: &str, each: usize) -> String {
    if text.len() <= each * 2 {
        return text.to_string();
    }
    let mut head = each;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - each;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}\n... {} bytes left out ...\n{}", &text[..head], tail - head, &text[tail..])
}

/// A step with the time it began, in milliseconds, for putting it in its place among the others.
struct Item {
    at: i64,
    step: Step,
}

/// The requests whose `message` is still where it says, from the last one back: a compaction that changed how many messages there are moves them all, and a conversation cut back to an earlier
/// place and gone on from moves what came after the cut (a place that is not before the next one's was one of those). Whether the message there is what the request made is looked at again, in `fits`.
fn trusted_places(lines: &[Line]) -> HashSet<usize> {
    let (mut trusted, mut before) = (HashSet::new(), None::<u64>);
    for (at, line) in lines.iter().enumerate().rev() {
        match line.ev() {
            "compaction" if line.u("messages_before") != line.u("messages_after") => break,
            "request" if line.s("role") == "main" => {
                if let Some(place) = line.u("message") {
                    if before.is_none_or(|next| place < next) {
                        trusted.insert(at);
                        before = Some(place);
                    }
                }
            }
            _ => {}
        }
    }
    trusted
}

/// Whether a message begins the turn that began as `(woken, characters typed)`: a notice of the harness's for a turn nobody sent, and the person's words (the app's own nudges are not) for the others.
fn begins_turn(message: &Message, (woken, chars): (bool, u64)) -> bool {
    if message.role != "user" {
        return false;
    }
    if woken {
        return message.notice.is_some();
    }
    let content = message.content.as_deref().unwrap_or("");
    message.notice.is_none() && !prompt::is_nudge(content) && prompt::typed_text(content).chars().count() as u64 == chars
}

/// Where each turn's messages are: from the message that begins it to the one that begins the next. From the last turn back, since what a compaction leaves is the newest turns; a turn whose first message
/// is not found (a compaction took it, or its words are not the ones the trajectory counted) has none. What the person added while a turn went on belongs to that turn.
fn turn_ranges(messages: &[Message], starts: &[Option<(bool, u64)>]) -> Vec<Option<Range<usize>>> {
    let mut found: Vec<Option<usize>> = vec![None; starts.len()];
    let mut before = messages.len();
    for (n, start) in starts.iter().enumerate().rev() {
        let Some(start) = start else { continue };
        if let Some(at) = messages[..before].iter().rposition(|message| begins_turn(message, *start)) {
            found[n] = Some(at);
            before = at;
        }
    }
    (0..starts.len()).map(|n| {
        let begin = found[n]?;
        let end = found[n + 1..].iter().flatten().next().copied().unwrap_or(messages.len());
        Some(begin..end)
    }).collect()
}

/// The first line of the person's words in a message of theirs.
fn asked_in(message: &Message) -> Option<String> {
    prompt::typed_text(message.content.as_deref()?).lines().map(str::trim).find(|line| !line.is_empty()).map(|line| cap(line, ASKED_CHARS))
}

fn path_of(turn: u64, in_turn: &[(usize, &Line)], range: Option<Range<usize>>, messages: &[Message], trusted: &HashSet<usize>) -> TurnPath {
    let begun = in_turn.iter().map(|(_, line)| *line).find(|line| line.ev() == "turn_start");
    let woken = begun.is_some_and(|line| line.v["woken"] == true);
    let asked = if woken { String::new() } else { range.as_ref().and_then(|range| asked_in(&messages[range.start])).unwrap_or_default() };
    let asked_images = if woken { Vec::new() } else { range.as_ref().map_or_else(Vec::new, |range| messages[range.start].images.iter().map(|image| image.file.clone()).collect()) };
    let failed = in_turn.iter().any(|(_, line)| line.ev() == "failed");
    let ended = in_turn.iter().map(|(_, line)| *line).find(|line| line.ev() == "turn_end")
        .map(|line| TurnEnd { ms: line.u("ms").unwrap_or(0), cancelled: line.v["cancelled"] == true, failed: failed || line.s("ended") == "failed" });
    TurnPath { turn, asked, woken, started: begun.map_or(String::new(), |line| line.s("t").to_string()), asked_images, steps: steps_of(in_turn, range, messages, trusted), ended }
}

/// How many characters of words a message has.
fn content_chars_of(message: &Message) -> u64 {
    message.content.as_deref().map_or(0, |content| content.chars().count()) as u64
}

/// Whether `inner` is made of some of `outer`, in its order.
fn is_subsequence(inner: &[&str], outer: &[&str]) -> bool {
    let mut rest = outer.iter();
    inner.iter().all(|name| rest.any(|other| other == name))
}

/// Whether a message is the one a request made: the calls it names, and the length of its words. A request that broke off made only what was written of it, or a `create_file` cut where its
/// last whole line is. `by_place`: the request says where the message is, so this only has to rule out a place that moved; going by order, a broken request is not taken for a message it did
/// not make (the one made when it was asked again is not its).
fn fits(line: &Line, message: &Message, by_place: bool) -> bool {
    if message.role != "assistant" || line.u("content_chars").is_some_and(|chars| chars != content_chars_of(message)) {
        return false;
    }
    let asked: Vec<&str> = line.v["calls"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let made: Vec<&str> = message.tool_calls.iter().flatten().map(|call| call.function.name.as_str()).collect();
    match line.s("end") {
        "stop" | "tool_calls" | "" => made == asked,
        // (the second, a long write the app cut, is in sessions from before it stopped doing so)
        "length" | "stopped_app:cut_write" => is_subsequence(&made, &asked),
        _ if by_place => is_subsequence(&made, &asked),
        _ => made.is_empty() && content_chars_of(message) > 0,
    }
}

/// Whether a main request can have made a message: it named a call, or wrote words.
fn can_make_a_message(line: &Line) -> bool {
    line.v["calls"].as_array().is_none_or(|calls| !calls.is_empty()) || line.u("content_chars").is_none_or(|chars| chars > 0)
}

/// Which message each main request of a turn made (by its line's place in the trajectory, to the message's place in the session).
fn join(requests: &[(usize, &Line)], range: Option<&Range<usize>>, messages: &[Message], trusted: &HashSet<usize>) -> HashMap<usize, usize> {
    let mut joined = HashMap::new();
    let Some(range) = range else { return joined };
    let assistants: Vec<usize> = range.clone().filter(|at| messages[*at].role == "assistant").collect();
    let mut next = 0;
    for (at, line) in requests {
        let by_place = line.u("message").filter(|_| trusted.contains(at)).and_then(|place| assistants.iter().position(|made| *made as u64 == place)).filter(|found| fits(line, &messages[assistants[*found]], true));
        let found = by_place.or_else(|| {
            if !can_make_a_message(line) {
                return None;
            }
            assistants[next..].iter().position(|made| fits(line, &messages[*made], false)).map(|found| next + found)
        });
        if let Some(found) = found {
            joined.insert(*at, assistants[found]);
            next = next.max(found + 1);
        }
    }
    joined
}

fn steps_of(in_turn: &[(usize, &Line)], range: Option<Range<usize>>, messages: &[Message], trusted: &HashSet<usize>) -> Vec<Step> {
    let requests: Vec<(usize, &Line)> = in_turn.iter().filter(|(_, line)| line.ev() == "request" && line.s("role") == "main").copied().collect();
    let joined = join(&requests, range.as_ref(), messages, trusted);
    // The tools of each request, in the order they ended; and the helper an `explore` call had, which wrote its own requests before the call's line.
    let mut tools: HashMap<&str, Vec<(usize, &Line)>> = HashMap::new();
    let mut helper_of: HashMap<usize, &str> = HashMap::new();
    let mut helper: Option<&str> = None;
    for (at, line) in in_turn {
        match (line.ev(), line.s("role")) {
            ("request", "main") => helper = None,
            ("request", "helper") => helper = line.v["helper_file"].as_str().or(helper),
            ("tool", _) => {
                tools.entry(line.s("req")).or_default().push((*at, line));
                if line.s("name") == "explore" {
                    if let Some(file) = helper.take() {
                        helper_of.insert(*at, file);
                    }
                }
            }
            _ => {}
        }
    }
    let mut items: Vec<Item> = Vec::new();
    let mut checkpoint: Option<RequestDetail> = None;
    let mut known: HashSet<&str> = HashSet::new();
    for (at, line) in in_turn {
        match (line.ev(), line.s("role")) {
            ("request", "main") => {
                known.insert(line.s("req"));
                if line.v["retry_of"].is_string() {
                    let wait = line.u("wait_ms").unwrap_or(0);
                    let kind = StepKind::Retry { why: line.s("retry_why").to_string(), wait_ms: wait };
                    let mut item = harness_item(line, kind, wait, line.u("ms").unwrap_or(0) + wait);
                    item.step.fields.insert("of".into(), line.v["retry_of"].clone());
                    items.push(item);
                }
                items.push(model_item(line, joined.get(at).copied(), messages, tools.get(line.s("req")).map_or(&[][..], Vec::as_slice), &helper_of));
            }
            ("request", "checkpoint") => checkpoint = Some(request_detail(line)),
            ("compaction", _) => {
                let kind = StepKind::Compaction { stage: line.s("stage").to_string(), ok: line.s("outcome") == "ok" };
                let ms = line.u("ms").unwrap_or(0);
                let mut item = harness_item(line, kind, ms, ms);
                item.step.request = checkpoint.take();
                items.push(item);
            }
            ("intervention", _) => items.push(harness_item(line, StepKind::Intervention { kind: line.s("kind").to_string() }, 0, 0)),
            ("approval", _) => {
                let wait = line.u("wait_ms").unwrap_or(0);
                items.push(harness_item(line, StepKind::Approval { tool: line.s("tool").to_string(), outcome: line.s("decision").to_string(), wait_ms: wait }, wait, wait));
            }
            ("wake", _) => items.push(harness_item(line, StepKind::Wake { cause: line.s("cause").to_string() }, 0, 0)),
            ("failed", _) => items.push(harness_item(line, StepKind::Failed { kind: line.s("kind").to_string() }, 0, 0)),
            _ => {}
        }
    }
    // (a checkpoint asked for and never summed up: the run ended in the middle of it)
    if let Some(request) = checkpoint {
        items.push(Item { at: millis(&request.started).unwrap_or(0), step: Step { kind: StepKind::Compaction { stage: String::new(), ok: false }, started: request.started.clone(), ms: request.ms, request: Some(request), ..Step::default() } });
    }
    // Calls of a request whose line is not here: the steps they were in are told by them alone.
    let mut orphans: Vec<&Vec<(usize, &Line)>> = tools.iter().filter_map(|(req, group)| (!known.contains(req)).then_some(group)).collect();
    orphans.sort_by_key(|group| group[0].0);
    for group in orphans {
        let calls: Vec<CallRow> = group.iter().map(|tool| call_row(None, Some(*tool), None, &helper_of)).collect();
        let started = calls[0].started.clone();
        items.push(Item { at: millis(&started).unwrap_or(0), step: Step { started, why: calls.first().map(derived_why), calls, ..Step::default() } });
    }
    // (a line with no time that can be read goes where the one before it is)
    let mut last = 0;
    for item in &mut items {
        if item.at == 0 {
            item.at = last;
        } else {
            last = item.at;
        }
    }
    // (by when each began: a compaction's line is written when it ends, and a request's when it does)
    items.sort_by_key(|item| item.at);
    items.into_iter().map(|item| item.step).collect()
}

/// A time as the files give it (`2026-11-05T10:00:03.250+09:00`), in milliseconds since 1970.
pub fn millis(time: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(time).ok().map(|at| at.timestamp_millis())
}

/// What a part of a turn's band was: the parts of a request before its first word, as the server tells them, the parts after it, and the calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Part {
    /// Before the server began on it, or what it does not tell: the way there and back, a queue behind another request.
    Waiting,
    /// Another conversation put aside for this one (`park_seconds`).
    Parking,
    /// The K/V grown to the prompt (`kv_growth_seconds`).
    Growing,
    /// The prompt read: what the server kept gone on from, and the rest read.
    Reading,
    /// The prompt read from its start, as nothing the server kept could be gone on from: the wait a person most wants to see.
    Rereading,
    /// After the first word, until a call or the answer is being written.
    Thinking,
    /// A call or the answer being written.
    Writing,
    /// After the first word, of a request kept before the band split it.
    Generating,
    /// A call running, one that failed, and time spent waiting for the person's answer or approval.
    Call,
    CallFailed,
    Person,
}

/// The lane of a part: the model's requests, or the calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lane {
    #[default]
    Requests,
    Calls,
}

/// A turn as a band of time: its parts, each in its lane from and to a number of milliseconds after the turn began, and where the app's own steps were.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Band {
    /// From the turn's start to its end, or to the end of its last part.
    pub length: u64,
    pub parts: Vec<BandPart>,
    /// Where the app's own steps began: compactions, its interventions, requests asked again, wakes, failures.
    pub marks: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BandPart {
    pub lane: Lane,
    pub from: u64,
    pub to: u64,
    pub part: Part,
}

/// A request that read its prompt from the start is told apart when that took this long: a short prompt read again is nothing to look at.
const REREAD_MARKED_MS: u64 = 1000;

/// The band of a turn, or none when it has no time to show (no start, nothing in it).
pub fn band(turn: &TurnPath) -> Option<Band> {
    let start = millis(&turn.started)?;
    let after = |time: &str| millis(time).map(|at| (at - start).max(0) as u64);
    let mut band = Band::default();
    for step in &turn.steps {
        match (&step.kind, &step.request) {
            (StepKind::Model, Some(request)) => {
                let wrote = !step.calls.is_empty() || request.content_chars > 0;
                request_parts(&mut band.parts, after(&request.started).unwrap_or(0), request, wrote);
            }
            (StepKind::Model, None) => {}
            _ => band.marks.extend(after(&step.started)),
        }
        for call in step.calls.iter().filter(|call| !call.outcome.is_empty()) {
            let Some(from) = after(&call.started) else { continue };
            // (the person is asked before it runs, and its own time leaves that wait out)
            let wait = call.wait_ms.unwrap_or(0);
            if wait > 0 {
                band.parts.push(BandPart { lane: Lane::Calls, from, to: from + wait, part: Part::Person });
            }
            let part = if call.answered_by_person() {
                Part::Person
            } else if call.outcome == "ok" && !call.exit_code.is_some_and(|code| code != 0) {
                Part::Call
            } else {
                Part::CallFailed
            };
            band.parts.push(BandPart { lane: Lane::Calls, from: from + wait, to: from + wait + call.ms.max(1), part });
        }
    }
    let ended = turn.ended.as_ref().map_or(0, |end| end.ms);
    band.length = band.parts.iter().map(|part| part.to).chain(band.marks.iter().copied()).max().unwrap_or(0).max(ended);
    (band.length > 0).then_some(band)
}

/// A request's parts from `from`: the wait to its first word as the server tells its share of it (what it says it spent putting another conversation aside,
/// growing the K/V and reading, in that order, and the rest of its time to the first token), the rest of that wait before, and after the first word its
/// thinking and its writing. `wrote`: it made a call or wrote words, which a request kept before the band split it shows as one part.
fn request_parts(parts: &mut Vec<BandPart>, from: u64, request: &RequestDetail, wrote: bool) {
    let end = from + request.ms;
    let first = request.first_ms.unwrap_or(request.ms).min(request.ms);
    let server_ms = |name: &str| request.server.get(name).and_then(Value::as_f64).map_or(0, |seconds| (seconds.max(0.0) * 1000.0).round() as u64);
    let mut at = from;
    let push = |parts: &mut Vec<BandPart>, at: &mut u64, part: Part, ms: u64| {
        if ms > 0 {
            parts.push(BandPart { lane: Lane::Requests, from: *at, to: *at + ms, part });
            *at += ms;
        }
    };
    match request.server.get("ttft_seconds").and_then(Value::as_f64) {
        Some(ttft) => {
            let ttft = ((ttft.max(0.0) * 1000.0).round() as u64).min(first);
            push(parts, &mut at, Part::Waiting, first - ttft);
            let read = server_ms("resume_seconds") + server_ms("prefill_seconds");
            let reread = request.server.get("resumed_from").and_then(Value::as_str) == Some("none") && read >= REREAD_MARKED_MS;
            let mut left = ttft;
            for (part, ms) in [(Part::Parking, server_ms("park_seconds")), (Part::Growing, server_ms("kv_growth_seconds")), (if reread { Part::Rereading } else { Part::Reading }, read)] {
                let ms = ms.min(left);
                push(parts, &mut at, part, ms);
                left -= ms;
            }
            push(parts, &mut at, Part::Waiting, left);
        }
        None => push(parts, &mut at, Part::Waiting, first),
    }
    let writing = [request.writing_ms, request.answer_ms].into_iter().flatten().map(|ms| (from + ms).clamp(at, end)).min();
    match writing {
        Some(writing) => {
            let thinking = writing - at;
            push(parts, &mut at, Part::Thinking, thinking);
            let rest = end - at;
            push(parts, &mut at, Part::Writing, rest);
        }
        None => {
            let rest = end - at;
            push(parts, &mut at, if wrote { Part::Generating } else { Part::Thinking }, rest);
        }
    }
}

/// The fields of a line besides those every line has: what a step of the app's holds, for its details.
fn fields_of(line: &Line) -> Map<String, Value> {
    line.v.as_object().map(|fields| fields.iter().filter(|(name, _)| !matches!(name.as_str(), "t" | "ev" | "turn" | "req")).map(|(name, value)| (name.clone(), value.clone())).collect()).unwrap_or_default()
}

/// A step of the app's from its line, which was written `back` ms after the step began.
fn harness_item(line: &Line, kind: StepKind, ms: u64, back: u64) -> Item {
    let began = line.at.map(|at| at - Duration::milliseconds(back as i64));
    let started = began.map_or_else(|| line.s("t").to_string(), |at| at.to_rfc3339_opts(SecondsFormat::Millis, false));
    Item { at: began.map_or(0, |at| at.timestamp_millis()), step: Step { kind, started, ms, fields: fields_of(line), ..Step::default() } }
}

fn request_detail(line: &Line) -> RequestDetail {
    let ms = line.u("ms").unwrap_or(0);
    let started = match line.s("started") {
        "" => line.at.map_or_else(|| line.s("t").to_string(), |at| (at - Duration::milliseconds(ms as i64)).to_rfc3339_opts(SecondsFormat::Millis, false)),
        started => started.to_string(),
    };
    RequestDetail {
        id: line.s("req").to_string(),
        role: line.s("role").to_string(),
        started,
        first_ms: line.u("first_ms"),
        ms,
        prompt: line.u("prompt"),
        cached: line.u("cached"),
        completion: line.u("completion"),
        tps: crate::trajectory::request_rate(&line.v),
        end: line.s("end").to_string(),
        reasoning_chars: line.u("reasoning_chars").unwrap_or(0),
        content_chars: line.u("content_chars").unwrap_or(0),
        writing_ms: line.u("writing_ms"),
        answer_ms: line.u("answer_ms"),
        server: line.v["server"].as_object().map(|server| server.iter().map(|(name, value)| (name.clone(), value.clone())).collect()).unwrap_or_default(),
    }
}

fn model_item(line: &Line, made: Option<usize>, messages: &[Message], tools: &[(usize, &Line)], helper_of: &HashMap<usize, &str>) -> Item {
    let request = request_detail(line);
    let calls = call_rows(line, made, messages, tools, helper_of);
    let why = made.and_then(|at| why_of(&messages[at])).or_else(|| calls.first().map(derived_why));
    Item { at: millis(&request.started).unwrap_or(0), step: Step { kind: StepKind::Model, started: request.started.clone(), ms: request.ms, why, request: Some(request), calls, fields: Map::new() } }
}

/// What the model said of a step, in its own words: the first sentence of what it wrote, else the last of what it reasoned.
fn why_of(message: &Message) -> Option<Why> {
    if let Some(text) = message.content.as_deref().and_then(first_sentence) {
        return Some(Why { text, source: WhySource::Content });
    }
    message.reasoning_content.as_deref().and_then(last_sentence).map(|text| Why { text, source: WhySource::Reasoning })
}

/// A step's why when the model said none: the tool of the first call and what it is on.
fn derived_why(call: &CallRow) -> Why {
    let text = match &call.target {
        Some(target) => format!("{} {target}", call.name),
        None => call.name.clone(),
    };
    Why { text: cap(&text, SENTENCE_CHARS), source: WhySource::Target }
}

/// The calls of a step: those the message made, each with its line in the trajectory, then any line the message has no call for. Without a message, the lines, and the calls the request named that
/// never ran (the turn ended first).
fn call_rows(request: &Line, made: Option<usize>, messages: &[Message], tools: &[(usize, &Line)], helper_of: &HashMap<usize, &str>) -> Vec<CallRow> {
    let mut unused: Vec<(usize, &Line)> = tools.to_vec();
    let mut rows = Vec::new();
    if let Some(at) = made {
        for call in messages[at].tool_calls.iter().flatten() {
            let found = unused.iter().position(|(_, tool)| !call.id.is_empty() && tool.s("id") == call.id)
                .or_else(|| unused.iter().position(|(_, tool)| tool.s("id").is_empty() && tool.s("name") == call.function.name));
            let tool = found.map(|place| unused.remove(place));
            let mut row = call_row(Some(call), tool, output_after(messages, at, &call.id), helper_of);
            row.images = images_after(messages, at, &call.id);
            rows.push(row);
        }
    }
    for tool in unused {
        rows.push(call_row(None, Some(tool), None, helper_of));
    }
    if made.is_none() {
        let mut ran: Vec<String> = rows.iter().map(|row| row.name.clone()).collect();
        for name in request.v["calls"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            match ran.iter().position(|ran| ran == name) {
                Some(place) => {
                    ran.remove(place);
                }
                None => rows.push(CallRow { name: name.to_string(), ..CallRow::default() }),
            }
        }
    }
    rows
}

/// What the call `id` brought back, from the results that follow the message `at` (they come before the next answer).
fn output_after(messages: &[Message], at: usize, id: &str) -> Option<String> {
    messages[at + 1..].iter().take_while(|message| message.role != "assistant")
        .find(|message| message.role == "tool" && message.tool_call_id.as_deref() == Some(id))
        .map(|message| ends_of(message.content.as_deref().unwrap_or(""), END_BYTES))
}

/// The pictures the result of the call `id` opened, from the results that follow the message `at`.
fn images_after(messages: &[Message], at: usize, id: &str) -> Vec<String> {
    messages[at + 1..].iter().take_while(|message| message.role != "assistant")
        .find(|message| message.role == "tool" && message.tool_call_id.as_deref() == Some(id))
        .map_or_else(Vec::new, |message| message.images.iter().map(|image| image.file.clone()).collect())
}

fn call_row(call: Option<&ToolCall>, tool: Option<(usize, &Line)>, output: Option<String>, helper_of: &HashMap<usize, &str>) -> CallRow {
    let line = tool.map(|(_, line)| line);
    let target = line.and_then(|line| line.v["path"].as_str().or_else(|| line.v["cmd0"].as_str()).map(str::to_string)).or_else(|| call.and_then(|call| target_of(&call.function.arguments)));
    CallRow {
        id: call.map(|call| call.id.clone()).or_else(|| line.map(|line| line.s("id").to_string())).unwrap_or_default(),
        name: call.map(|call| call.function.name.clone()).or_else(|| line.map(|line| line.s("name").to_string())).unwrap_or_default(),
        target,
        started: line.map_or(String::new(), |line| line.s("started").to_string()),
        ms: line.and_then(|line| line.u("ms")).unwrap_or(0),
        wait_ms: line.and_then(|line| line.u("wait_ms")),
        outcome: line.map_or(String::new(), |line| line.s("outcome").to_string()),
        exit_code: line.and_then(|line| line.v["exit_code"].as_i64()),
        arguments: call.map(|call| if call.function.arguments.len() <= ARGUMENTS_WHOLE { call.function.arguments.clone() } else { ends_of(&call.function.arguments, END_BYTES) }),
        output,
        helper_file: tool.and_then(|(at, _)| helper_of.get(&at)).map(|file| file.to_string()),
        helper_steps: Vec::new(),
        images: Vec::new(),
    }
}

/// What a call is on, from its arguments: the file, the address, the program of a command (its first word, never its arguments), or what is searched for.
fn target_of(arguments: &str) -> Option<String> {
    let arguments: Value = serde_json::from_str(arguments).ok()?;
    let at = |pointer: &str| arguments.pointer(pointer).and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty());
    let found = at("/path").or_else(|| at("/edits/0/path")).or_else(|| at("/url")).map(str::to_string)
        .or_else(|| at("/command").and_then(first_word_of))
        .or_else(|| ["/pattern", "/query", "/queries/0", "/task"].iter().find_map(|pointer| at(pointer)).map(str::to_string));
    found.map(|target| cap(&target, TARGET_CHARS))
}

/// Where the work is heading: the last plan the person approved, else the checklist the model kept, else nothing.
fn heading_of(messages: &[Message]) -> Option<Heading> {
    if let Some(plan) = approved_plan(messages) {
        let items = plan_items(&plan);
        if !items.is_empty() {
            return Some(Heading::Plan { items });
        }
    }
    let todos = todos_of(messages);
    (!todos.is_empty()).then_some(Heading::Todos { items: todos })
}

/// The text of the last plan put to the person (`submit_plan`) that they approved: the result the session kept for the call says so (`plan_outcome`).
fn approved_plan(messages: &[Message]) -> Option<String> {
    let mut plan = None;
    for (at, message) in messages.iter().enumerate() {
        for call in message.tool_calls.iter().flatten().filter(|call| call.function.name == "submit_plan") {
            let approved = output_after(messages, at, &call.id).is_some_and(|result| plan_outcome(&result) == PlanOutcome::Approved);
            let text = serde_json::from_str::<Value>(&call.function.arguments).ok().and_then(|arguments| arguments.get("plan")?.as_str().map(str::to_string));
            if let (true, Some(text)) = (approved, text) {
                plan = Some(text);
            }
        }
    }
    plan
}

/// The items of a plan written in Markdown: its list items (the outermost ones, not the details under them), or its lines where it has no list. Outside code blocks.
pub fn plan_items(plan: &str) -> Vec<String> {
    let (mut listed, mut lines): (Vec<(usize, String)>, Vec<String>) = (Vec::new(), Vec::new());
    let mut inside = false;
    for line in plan.lines() {
        if is_fence(line) {
            inside = !inside;
            continue;
        }
        let text = line.trim_start();
        if inside || text.is_empty() {
            continue;
        }
        let words = unmarked(text);
        if words.is_empty() {
            continue;
        }
        if list_marker(text).is_some() {
            listed.push((line.len() - text.len(), cap(words, ITEM_CHARS)));
        }
        lines.push(cap(words, ITEM_CHARS));
    }
    if listed.is_empty() {
        return lines;
    }
    let outermost = listed.iter().map(|(indent, _)| *indent).min().unwrap_or(0);
    listed.into_iter().filter(|(indent, _)| *indent == outermost).map(|(_, text)| text).collect()
}

/// The checklist the model's `todo` calls leave, replayed as the tool does (`apply_todo`).
fn todos_of(messages: &[Message]) -> Vec<TodoItem> {
    let mut todos: Vec<Value> = Vec::new();
    for call in messages.iter().flat_map(|message| message.tool_calls.iter().flatten()).filter(|call| call.function.name == "todo") {
        if let Ok(arguments) = serde_json::from_str::<Value>(&call.function.arguments) {
            let _ = apply_todo(&mut todos, &arguments);
        }
    }
    todos.iter().map(|item| {
        let text = item["text"].as_str().filter(|text| !text.trim().is_empty()).or_else(|| item["id"].as_str()).unwrap_or("");
        TodoItem { text: text.trim().to_string(), status: item["status"].as_str().unwrap_or("pending").to_string() }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(lines: &[Value]) -> String {
        lines.iter().map(|line| format!("{line}\n")).collect()
    }

    fn user(words: &str) -> Message {
        Message::text("user", words)
    }

    fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
        ToolCall { id: id.into(), kind: "function".into(), function: crate::message::FunctionCall { name: name.into(), arguments: arguments.to_string() } }
    }

    fn assistant(content: &str, reasoning: &str, calls: Vec<ToolCall>) -> Message {
        Message {
            role: "assistant".into(),
            content: (!content.is_empty()).then(|| content.to_string()),
            reasoning_content: (!reasoning.is_empty()).then(|| reasoning.to_string()),
            tool_calls: (!calls.is_empty()).then_some(calls),
            ..Message::default()
        }
    }

    fn turn_start(turn: u64, at: &str, chars: u64) -> Value {
        json!({"t": at, "ev": "turn_start", "turn": turn, "chars": chars, "images": 0})
    }

    /// A main request that began at `started` and ended `ms` later.
    fn request(turn: u64, n: u64, started: &str, ms: u64, content_chars: u64, calls: &[&str], end: &str) -> Value {
        let ended = DateTime::parse_from_rfc3339(started).unwrap() + Duration::milliseconds(ms as i64);
        json!({"t": ended.to_rfc3339_opts(SecondsFormat::Millis, false), "ev": "request", "turn": turn, "req": format!("s/{n}"), "role": "main", "started": started, "first_ms": 100, "ms": ms,
            "prompt": 5000, "cached": 4000, "completion": 50, "tps": 40.5, "reasoning_chars": 10, "content_chars": content_chars, "calls": calls, "end": end})
    }

    fn tool(turn: u64, n: u64, id: &str, name: &str, started: &str, path: Option<&str>) -> Value {
        let mut line = json!({"t": started, "ev": "tool", "turn": turn, "req": format!("s/{n}"), "id": id, "name": name, "started": started, "ms": 30, "outcome": "ok", "out_bytes": 10});
        if let Some(path) = path {
            line["path"] = json!(path);
        }
        line
    }

    fn model_steps(path: &TurnPath) -> Vec<&Step> {
        path.steps.iter().filter(|step| step.kind == StepKind::Model).collect()
    }

    #[test]
    fn a_step_says_why_in_the_models_words_first_what_it_wrote_then_the_end_of_what_it_reasoned_then_the_call() {
        let wrote = "Let me read the library first. Then I will look further.";
        let lines = [
            turn_start(1, "2026-11-05T10:00:00.000+09:00", 11),
            request(1, 1, "2026-11-05T10:00:01.000+09:00", 2000, wrote.chars().count() as u64, &["read_file"], "tool_calls"),
            tool(1, 1, "c1", "read_file", "2026-11-05T10:00:03.000+09:00", Some("src/lib.rs")),
            request(1, 2, "2026-11-05T10:00:04.000+09:00", 2000, 0, &["search_files"], "tool_calls"),
            tool(1, 2, "c2", "search_files", "2026-11-05T10:00:06.000+09:00", None),
            request(1, 3, "2026-11-05T10:00:07.000+09:00", 2000, 0, &["execute_command"], "tool_calls"),
            tool(1, 3, "c3", "execute_command", "2026-11-05T10:00:09.000+09:00", None),
            json!({"t": "2026-11-05T10:00:10.000+09:00", "ev": "turn_end", "turn": 1, "cancelled": false, "ended": "done", "requests": 3, "tools": 3, "ms": 10000}),
        ];
        let messages = vec![
            Message::text("system", "s"),
            user("fix the bug"),
            assistant(wrote, "I should look.", vec![call("c1", "read_file", json!({"path": "src/lib.rs"}))]),
            Message::tool_result("c1", "fn main() {}"),
            assistant("", "The file is short. Next I'll search for the caller of parse.", vec![call("c2", "search_files", json!({"pattern": "parse"}))]),
            Message::tool_result("c2", "none"),
            assistant("", "", vec![call("c3", "execute_command", json!({"command": "cargo test --lib"}))]),
            Message::tool_result("c3", "ok"),
        ];
        let said = build(&messages, &text(&lines));
        let path = &said.turns[0];
        assert_eq!((path.asked.as_str(), path.woken), ("fix the bug", false));
        let steps = model_steps(path);
        let whys: Vec<(String, WhySource)> = steps.iter().map(|step| step.why.clone().map(|why| (why.text, why.source)).unwrap()).collect();
        assert_eq!(whys, [
            ("Let me read the library first.".to_string(), WhySource::Content),
            ("Next I'll search for the caller of parse.".to_string(), WhySource::Reasoning),
            ("execute_command cargo".to_string(), WhySource::Target),
        ]);
        // The calls are beside their steps with what the trajectory and the session say of each.
        let first = &steps[0].calls[0];
        assert_eq!((first.id.as_str(), first.name.as_str(), first.target.as_deref(), first.outcome.as_str(), first.ms), ("c1", "read_file", Some("src/lib.rs"), "ok", 30));
        assert_eq!((first.arguments.as_deref(), first.output.as_deref()), (Some(r#"{"path":"src/lib.rs"}"#), Some("fn main() {}")));
        let request = steps[0].request.as_ref().unwrap();
        assert_eq!((request.id.as_str(), request.ms, request.prompt, request.cached, request.completion, request.end.as_str()), ("s/1", 2000, Some(5000), Some(4000), Some(50), "tool_calls"));
        assert_eq!(path.ended, Some(TurnEnd { ms: 10000, cancelled: false, failed: false }));
    }

    #[test]
    fn a_request_the_session_has_no_message_for_has_its_calls_from_the_trajectory_alone() {
        let lines = [
            turn_start(1, "2026-11-05T10:00:00.000+09:00", 3),
            request(1, 1, "2026-11-05T10:00:01.000+09:00", 2000, 0, &["read_file", "apply_patch"], "tool_calls"),
            tool(1, 1, "c1", "read_file", "2026-11-05T10:00:03.000+09:00", Some("a.rs")),
        ];
        let said = build(&[], &text(&lines));
        let step = &said.turns[0].steps[0];
        assert_eq!(said.turns[0].asked, "", "the words of the person are in the session");
        let calls: Vec<(&str, &str)> = step.calls.iter().map(|call| (call.name.as_str(), call.outcome.as_str())).collect();
        assert_eq!(calls, [("read_file", "ok"), ("apply_patch", "")], "a call with no line did not run");
        assert!(step.calls[0].arguments.is_none() && step.calls[0].output.is_none());
        assert_eq!(step.why, Some(Why { text: "read_file a.rs".into(), source: WhySource::Target }));
        assert!(said.turns[0].ended.is_none(), "the run ended in this turn");
    }

    #[test]
    fn sentences_end_at_full_stops_and_line_ends_and_not_inside_a_name_or_a_number() {
        assert_eq!(first_sentence("Let me look at a.rs and v3.14 first. Then more.").as_deref(), Some("Let me look at a.rs and v3.14 first."));
        assert_eq!(last_sentence("Done with that. Next I'll open config.toml").as_deref(), Some("Next I'll open config.toml"));
        assert_eq!(first_sentence("まず設定を読みます。次に直します。").as_deref(), Some("まず設定を読みます。"));
        assert_eq!(last_sentence("まず設定を読みます。次にテストを走らせます！").as_deref(), Some("次にテストを走らせます！"));
        assert_eq!(last_sentence("Is it there? Let me check...\n").as_deref(), Some("Let me check..."));
        assert_eq!(first_sentence("He said \"go.\" Then left.").as_deref(), Some("He said \"go.\""));
        // The marks of Markdown are not part of a sentence, a line is one of its own, and a block of code is none.
        assert_eq!(first_sentence("## Plan\n- read the file\n").as_deref(), Some("Plan"));
        assert_eq!(last_sentence("- [ ] write the test\n\n").as_deref(), Some("write the test"));
        assert_eq!(first_sentence("3. Read the file. Then more.").as_deref(), Some("Read the file."));
        assert_eq!(first_sentence("```rust\nfn main() {}\n```\nThen run it.").as_deref(), Some("Then run it."));
        assert_eq!(last_sentence("I will write it now.\n```rust\nfn main() {\n").as_deref(), Some("I will write it now."));
        // Nothing to say is none, and a long sentence is cut.
        assert_eq!((first_sentence("  \n---\n"), last_sentence("")), (None, None));
        let long = "word ".repeat(60);
        let cut = first_sentence(&long).unwrap();
        assert!(cut.ends_with('…') && cut.chars().count() <= SENTENCE_CHARS + 1, "{cut}");
    }

    #[test]
    fn a_compaction_a_guard_and_a_retry_sit_among_the_model_steps_by_when_they_began() {
        let lines = [
            turn_start(1, "2026-11-05T10:00:00.000+09:00", 4),
            request(1, 1, "2026-11-05T10:00:01.000+09:00", 3000, 0, &[], "stopped_app:draft_stop"),
            json!({"t": "2026-11-05T10:00:04.001+09:00", "ev": "intervention", "turn": 1, "req": "s/1", "kind": "draft_stop", "tokens_before": 800, "run": 40}),
            json!({"t": "2026-11-05T10:00:05.000+09:00", "ev": "request", "turn": 1, "req": "s/2", "role": "checkpoint", "started": "2026-11-05T10:00:04.100+09:00", "ms": 900, "end": "stop", "calls": []}),
            json!({"t": "2026-11-05T10:00:05.250+09:00", "ev": "compaction", "turn": 1, "req": "s/1", "forced": false, "refused": false, "stage": "checkpoint", "before_tokens": 90000, "after_tokens": 20000, "ms": 1200, "outcome": "ok", "messages_before": 80, "messages_after": 12}),
            json!({"t": "2026-11-05T10:00:09.000+09:00", "ev": "request", "turn": 1, "req": "s/3", "role": "main", "started": "2026-11-05T10:00:06.500+09:00", "ms": 2500, "content_chars": 0, "calls": [], "end": "error:transient"}),
            // (asked again after 1.5 s: the wait is before the request that repeats it)
            json!({"t": "2026-11-05T10:00:12.000+09:00", "ev": "request", "turn": 1, "req": "s/4", "role": "main", "started": "2026-11-05T10:00:10.500+09:00", "ms": 1500, "content_chars": 2, "calls": [], "end": "stop", "retry_of": "s/3", "retry_why": "transient", "wait_ms": 1500}),
            json!({"t": "2026-11-05T10:00:12.100+09:00", "ev": "approval", "turn": 1, "req": "s/4", "tool": "create_file", "asked": "2026-11-05T10:00:10.600+09:00", "answered": "2026-11-05T10:00:12.100+09:00", "wait_ms": 1500, "decision": "approve"}),
            json!({"t": "2026-11-05T10:00:13.000+09:00", "ev": "turn_end", "turn": 1, "cancelled": true, "ended": "cancelled", "requests": 3, "tools": 0, "ms": 13000}),
        ];
        let said = build(&[], &text(&lines));
        let kinds: Vec<String> = said.turns[0].steps.iter().map(|step| match &step.kind {
            StepKind::Model => format!("model {}", step.request.as_ref().unwrap().id),
            StepKind::Compaction { stage, ok } => format!("compaction {stage} {ok}"),
            StepKind::Intervention { kind } => format!("guard {kind}"),
            StepKind::Retry { why, wait_ms } => format!("retry {why} {wait_ms}"),
            StepKind::Approval { tool, outcome, .. } => format!("approval {tool} {outcome}"),
            other => format!("{other:?}"),
        }).collect();
        // The checkpoint request is the compaction's own, and not a step of the path.
        assert_eq!(kinds, ["model s/1", "guard draft_stop", "compaction checkpoint true", "model s/3", "retry transient 1500", "model s/4", "approval create_file approve"]);
        let steps = &said.turns[0].steps;
        assert_eq!(steps[2].request.as_ref().map(|request| (request.role.as_str(), request.ms)), Some(("checkpoint", 900)));
        assert_eq!((steps[2].ms, steps[2].fields["before_tokens"].as_u64(), steps[2].fields["messages_after"].as_u64()), (1200, Some(90000), Some(12)));
        assert_eq!(steps[1].fields["tokens_before"].as_u64(), Some(800));
        assert_eq!(steps[4].started, "2026-11-05T10:00:09.000+09:00", "the retry begins when the request that failed ended");
        assert_eq!(steps[6].ms, 1500);
        assert_eq!(said.turns[0].ended, Some(TurnEnd { ms: 13000, cancelled: true, failed: false }));
    }

    #[test]
    fn a_trajectory_from_before_places_and_ids_were_recorded_is_joined_by_order_within_each_turn() {
        // Two turns, the first with a request that made no message (an answer with nothing in it, asked again) and a call with no id on its line.
        let lines = [
            turn_start(1, "2026-11-05T10:00:00.000+09:00", 5),
            json!({"t": "2026-11-05T10:00:03.000+09:00", "ev": "request", "turn": 1, "req": "s/1", "role": "main", "started": "2026-11-05T10:00:01.000+09:00", "ms": 2000, "content_chars": 0, "calls": [], "end": "stop"}),
            json!({"t": "2026-11-05T10:00:06.000+09:00", "ev": "request", "turn": 1, "req": "s/2", "role": "main", "started": "2026-11-05T10:00:04.000+09:00", "ms": 2000, "content_chars": 0, "calls": ["read_file"], "end": "tool_calls", "retry_of": "s/1", "retry_why": "empty", "wait_ms": 0}),
            json!({"t": "2026-11-05T10:00:07.000+09:00", "ev": "tool", "turn": 1, "req": "s/2", "name": "read_file", "started": "2026-11-05T10:00:06.500+09:00", "ms": 10, "outcome": "ok", "path": "a.txt"}),
            json!({"t": "2026-11-05T10:00:10.000+09:00", "ev": "request", "turn": 1, "req": "s/3", "role": "main", "started": "2026-11-05T10:00:08.000+09:00", "ms": 2000, "content_chars": "Found the file.".chars().count(), "calls": [], "end": "stop"}),
            json!({"t": "2026-11-05T10:00:11.000+09:00", "ev": "turn_end", "turn": 1, "cancelled": false, "requests": 3, "tools": 1, "ms": 11000}),
            turn_start(2, "2026-11-05T10:01:00.000+09:00", 6),
            json!({"t": "2026-11-05T10:01:03.000+09:00", "ev": "request", "turn": 2, "req": "s/4", "role": "main", "started": "2026-11-05T10:01:01.000+09:00", "ms": 2000, "content_chars": "Reading the second file now".chars().count(), "calls": ["read_file"], "end": "tool_calls"}),
            json!({"t": "2026-11-05T10:01:04.000+09:00", "ev": "tool", "turn": 2, "req": "s/4", "name": "read_file", "started": "2026-11-05T10:01:03.500+09:00", "ms": 10, "outcome": "ok", "path": "b.txt"}),
        ];
        let messages = vec![
            Message::text("system", "s"),
            user("first"),
            assistant("", "", vec![call("x1", "read_file", json!({"path": "a.txt"}))]),
            Message::tool_result("x1", "A"),
            assistant("Found the file.", "", vec![]),
            user("second"),
            // (what the person added while the turn went on is in the turn)
            user("also this"),
            assistant("Reading the second file now", "", vec![call("x2", "read_file", json!({"path": "b.txt"}))]),
        ];
        let said = build(&messages, &text(&lines));
        assert_eq!(said.turns.iter().map(|path| path.asked.as_str()).collect::<Vec<_>>(), ["first", "second"]);
        let first = model_steps(&said.turns[0]);
        assert_eq!(first.len(), 3);
        assert!(first[0].why.is_none() && first[0].calls.is_empty(), "the answer with nothing in it has no message and no why");
        assert_eq!((first[1].calls[0].id.as_str(), first[1].calls[0].output.as_deref()), ("x1", Some("A")));
        assert_eq!(first[1].calls[0].target.as_deref(), Some("a.txt"));
        assert_eq!(first[2].why.as_ref().map(|why| why.text.as_str()), Some("Found the file."));
        let second = model_steps(&said.turns[1]);
        assert_eq!((second[0].why.as_ref().map(|why| why.text.as_str()), second[0].calls[0].id.as_str()), (Some("Reading the second file now"), "x2"));
        assert_eq!(second[0].calls[0].output, None, "the session has no result for the call yet");
        // The retry of the answer that was empty sits between the two.
        assert!(matches!(&said.turns[0].steps[1].kind, StepKind::Retry { why, .. } if why == "empty"));
    }

    #[test]
    fn a_place_the_trajectory_gives_joins_the_message_and_one_a_compaction_moved_does_not() {
        let lines = [
            turn_start(1, "2026-11-05T10:00:00.000+09:00", 3),
            json!({"t": "2026-11-05T10:00:03.000+09:00", "ev": "request", "turn": 1, "req": "s/1", "role": "main", "started": "2026-11-05T10:00:01.000+09:00", "ms": 2000, "content_chars": 5, "calls": [], "end": "stop", "message": 2}),
            json!({"t": "2026-11-05T10:00:04.000+09:00", "ev": "compaction", "turn": 1, "stage": "checkpoint", "ms": 900, "outcome": "ok", "messages_before": 30, "messages_after": 4}),
            turn_start(2, "2026-11-05T10:01:00.000+09:00", 3),
            json!({"t": "2026-11-05T10:01:03.000+09:00", "ev": "request", "turn": 2, "req": "s/2", "role": "main", "started": "2026-11-05T10:01:01.000+09:00", "ms": 2000, "content_chars": 5, "calls": [], "end": "stop", "message": 4}),
        ];
        // After the compaction the session holds a summary, and the second turn's words and answer. Place 2 is now where the summary's answer is, and it is "Older", which the first request
        // would take for its own (it has the same length of words).
        let messages = vec![Message::text("system", "s"), Message::text("user", "[summary of earlier turns]"), assistant("Older", "", vec![]), user("two"), assistant("Newer", "", vec![])];
        let said = build(&messages, &text(&lines));
        assert_eq!(said.turns[1].steps[0].why.as_ref().map(|why| why.text.as_str()), Some("Newer"), "after the compaction the place is the session's own");
        // The first turn's words are gone, and so is its place: not trusted past the compaction.
        assert_eq!(said.turns[0].asked, "");
        assert!(said.turns[0].steps.iter().find(|step| step.kind == StepKind::Model).unwrap().why.is_none(), "{:?}", said.turns[0].steps);
    }

    #[test]
    fn the_place_a_request_gives_goes_before_order_when_a_line_of_an_earlier_request_is_missing() {
        // The first request's line is not in the trajectory (a crash cut it); the second says its message is the one at 4, and not the first, which would do by order.
        let mut line = request(1, 2, "2026-11-05T10:00:04.000+09:00", 2000, 0, &["read_file"], "tool_calls");
        line["message"] = json!(4);
        let lines = [turn_start(1, "2026-11-05T10:00:00.000+09:00", 2), line, tool(1, 2, "b1", "read_file", "2026-11-05T10:00:06.000+09:00", Some("b.txt"))];
        let messages = vec![
            Message::text("system", "s"),
            user("go"),
            assistant("", "", vec![call("a1", "read_file", json!({"path": "a.txt"}))]),
            Message::tool_result("a1", "A"),
            assistant("", "", vec![call("b1", "read_file", json!({"path": "b.txt"}))]),
            Message::tool_result("b1", "B"),
        ];
        let said = build(&messages, &text(&lines));
        let calls = &said.turns[0].steps[0].calls;
        assert_eq!(calls.iter().map(|call| (call.id.as_str(), call.output.as_deref())).collect::<Vec<_>>(), [("b1", Some("B"))]);
    }

    #[test]
    fn an_explore_call_keeps_the_file_of_its_helper_and_the_helpers_own_requests_are_no_steps() {
        let helper = |n: u64, started: &str| json!({"t": started, "ev": "request", "turn": 1, "req": format!("s/{n}"), "role": "helper", "started": started, "ms": 500, "content_chars": 0, "calls": ["search_files"], "end": "tool_calls", "helper_file": "s.helper-1.jsonl"});
        let lines = [
            turn_start(1, "2026-11-05T10:00:00.000+09:00", 4),
            request(1, 1, "2026-11-05T10:00:01.000+09:00", 1000, 0, &["explore"], "tool_calls"),
            helper(2, "2026-11-05T10:00:02.100+09:00"),
            helper(3, "2026-11-05T10:00:03.100+09:00"),
            tool(1, 1, "e1", "explore", "2026-11-05T10:00:02.000+09:00", None),
            request(1, 4, "2026-11-05T10:00:05.000+09:00", 1000, 4, &[], "stop"),
        ];
        let messages = vec![
            Message::text("system", "s"),
            user("find"),
            assistant("", "", vec![call("e1", "explore", json!({"task": "Where is the needle?"}))]),
            Message::tool_result("e1", "At a.txt:1."),
            assistant("Done", "", vec![]),
        ];
        let said = build(&messages, &text(&lines));
        assert_eq!(said.turns[0].steps.len(), 2, "{:?}", said.turns[0].steps);
        let explore = &said.turns[0].steps[0].calls[0];
        assert_eq!((explore.name.as_str(), explore.target.as_deref(), explore.helper_file.as_deref()), ("explore", Some("Where is the needle?"), Some("s.helper-1.jsonl")));
        assert_eq!(said.turns[0].steps[0].why.as_ref().map(|why| why.source), Some(WhySource::Target));
        assert_eq!(said.turns[0].steps[1].calls.len(), 0);
    }

    /// Read from the files, an explore's call has the steps of its helper's own conversation: why in the helper's words, and what it called.
    #[test]
    fn an_explore_call_read_from_the_files_has_its_helpers_steps() {
        let dir = crate::in_container::tempdir().unwrap();
        let mut session = session::Session::create(dir.path(), dir.path(), "2026-11-05 10:00:00 +09:00", "20261105-100000").unwrap();
        for message in [Message::text("system", "s"), user("find"), assistant("", "", vec![call("e1", "explore", json!({"task": "Where is the needle?"}))]), Message::tool_result("e1", "At a.txt:1."), assistant("Done", "", vec![])] {
            session.append(&message).unwrap();
        }
        let (mut helper, _) = session::Session::create_helper(&Default::default(), &session.path, dir.path(), "2026-11-05 10:00:02 +09:00").unwrap();
        for message in [Message::text("system", "h"), user("Where is the needle?"), assistant("I will search for it.", "", vec![call("h1", "search_files", json!({"pattern": "needle"}))]), Message::tool_result("h1", "a.txt:1"), assistant("", "It is in a.txt, so I answer.", vec![])] {
            helper.append(&message).unwrap();
        }
        let helper_file = helper.path.file_name().unwrap().to_string_lossy().into_owned();
        let helper_line = json!({"t": "2026-11-05T10:00:02.100+09:00", "ev": "request", "turn": 1, "req": "20261105-100000/2", "role": "helper", "started": "2026-11-05T10:00:02.100+09:00", "ms": 500, "content_chars": 0, "calls": ["search_files"], "end": "tool_calls", "helper_file": helper_file});
        let lines = [turn_start(1, "2026-11-05T10:00:00.000+09:00", 4), request(1, 1, "2026-11-05T10:00:01.000+09:00", 1000, 0, &["explore"], "tool_calls"), helper_line,
                     tool(1, 1, "e1", "explore", "2026-11-05T10:00:02.000+09:00", None), request(1, 3, "2026-11-05T10:00:05.000+09:00", 1000, 4, &[], "stop")];
        std::fs::write(trajectory::file_of(&session.path), text(&lines)).unwrap();
        let read_back = read(&session.path).unwrap();
        let explore = &read_back.turns[0].steps[0].calls[0];
        let steps: Vec<(Option<&str>, Vec<(&str, Option<&str>)>)> = explore.helper_steps.iter().map(|step| (step.why.as_ref().map(|why| why.text.as_str()), step.calls.iter().map(|(name, target)| (name.as_str(), target.as_deref())).collect())).collect();
        assert_eq!(steps, [(Some("I will search for it."), vec![("search_files", Some("needle"))]), (Some("It is in a.txt, so I answer."), vec![])]);
    }

    #[test]
    fn a_turn_nobody_sent_says_what_woke_it_and_has_no_words_of_the_person() {
        let lines = [
            json!({"t": "2026-11-05T10:00:00.000+09:00", "ev": "turn_start", "turn": 1, "chars": 90, "images": 0, "first": false, "woken": true}),
            json!({"t": "2026-11-05T10:00:00.010+09:00", "ev": "wake", "turn": 1, "req": "s/1", "cause": "command_ended", "id": "job1", "after_ms": 700, "together": 1, "cmd0": "cargo"}),
            request(1, 1, "2026-11-05T10:00:00.020+09:00", 2000, "The build passed.".chars().count() as u64, &[], "stop"),
            json!({"t": "2026-11-05T10:00:03.000+09:00", "ev": "failed", "turn": 1, "req": "s/1", "kind": "server_gone", "server_said": "the worker exited"}),
            json!({"t": "2026-11-05T10:00:03.001+09:00", "ev": "turn_end", "turn": 1, "cancelled": false, "ended": "failed", "requests": 1, "tools": 0, "ms": 3000}),
        ];
        let notice = Message { notice: Some(vec![crate::message::Wake { cause: crate::message::WakeCause::CommandEnded, id: "job1".into(), what: "cargo test".into(), exit_code: Some(0), ended: None, seconds: 3 }]), ..Message::text("user", "[From the app] job1 ended") };
        let messages = vec![Message::text("system", "s"), notice, assistant("The build passed.", "", vec![])];
        let said = build(&messages, &text(&lines));
        let path = &said.turns[0];
        assert_eq!((path.woken, path.asked.as_str()), (true, ""));
        assert!(matches!(&path.steps[0].kind, StepKind::Wake { cause } if cause == "command_ended"), "{:?}", path.steps);
        assert_eq!((path.steps[0].fields["id"].as_str(), path.steps[0].fields["cmd0"].as_str()), (Some("job1"), Some("cargo")));
        assert_eq!(path.steps[1].why.as_ref().map(|why| why.text.as_str()), Some("The build passed."));
        assert!(matches!(&path.steps[2].kind, StepKind::Failed { kind } if kind == "server_gone"));
        assert_eq!(path.ended, Some(TurnEnd { ms: 3000, cancelled: false, failed: true }));
    }

    #[test]
    fn it_heads_to_the_plan_the_person_approved_or_the_checklist_the_model_kept_or_nothing() {
        let plan = "## Plan\n\n1. Read the parser\n   - check the tests too\n2. Change `parse`\n3. Run the tests\n";
        let planned = vec![
            Message::text("system", "s"),
            user("plan it"),
            assistant("", "", vec![call("p1", "submit_plan", json!({"plan": "1. Old idea"}))]),
            Message::tool_result("p1", &format!("{}use a modal", crate::agent::PLAN_CHANGE)),
            assistant("", "", vec![call("p2", "submit_plan", json!({"plan": plan}))]),
            Message::tool_result("p2", crate::agent::PLAN_APPROVED),
            assistant("", "", vec![call("p3", "submit_plan", json!({"plan": "1. A plan nobody has decided on"}))]),
        ];
        assert_eq!(build(&planned, "").heading, Some(Heading::Plan { items: vec!["Read the parser".into(), "Change `parse`".into(), "Run the tests".into()] }));
        // A plan with no list is its lines.
        assert_eq!(plan_items("# Goal\nRead it.\n```\ncode\n```\nChange it."), ["Goal", "Read it.", "Change it."]);
        // No plan approved: the checklist, replayed as the tool does (merged by id, the first of a new list doing).
        let kept = vec![
            Message::text("system", "s"),
            assistant("", "", vec![call("t1", "todo", json!({"todos": [{"id": "a", "text": "read"}, {"id": "b", "text": "write"}]}))]),
            Message::tool_result("t1", "[doing] a: read\n[pending] b: write"),
            assistant("", "", vec![call("t2", "todo", json!({"todos": [{"id": "a", "status": "done"}, {"id": "b", "status": "doing"}]}))]),
            Message::tool_result("t2", "..."),
            assistant("", "", vec![call("t3", "todo", json!({"todos": [{"id": "c"}], "merge": true}))]),
        ];
        let todos = |heading: Option<Heading>| match heading {
            Some(Heading::Todos { items }) => items.into_iter().map(|item| (item.text, item.status)).collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };
        assert_eq!(todos(build(&kept, "").heading), [("read".to_string(), "done".to_string()), ("write".into(), "doing".into()), ("c".into(), "pending".into())]);
        // A list begun afresh replaces the one before; and with neither there is nothing, and none is made up.
        let afresh = [kept.clone(), vec![assistant("", "", vec![call("t4", "todo", json!({"todos": [{"id": "z", "text": "only this"}], "merge": false}))])]].concat();
        assert_eq!(todos(build(&afresh, "").heading), [("only this".to_string(), "doing".to_string())]);
        assert_eq!(build(&[Message::text("system", "s"), user("hello")], "").heading, None);
    }

    /// The pictures the person sent with a request, and those a call's result opened, are named by their files.
    #[test]
    fn the_pictures_of_a_request_and_of_a_calls_result_are_named_by_their_files() {
        let lines = [
            json!({"t": "2026-11-05T10:00:00.000+09:00", "ev": "turn_start", "turn": 1, "chars": 17, "images": 2}),
            request(1, 1, "2026-11-05T10:00:01.000+09:00", 100, 0, &["read_file"], "tool_calls"),
            tool(1, 1, "c1", "read_file", "2026-11-05T10:00:02.000+09:00", Some("shot.png")),
        ];
        let picture = |file: &str| crate::message::ImageRef { file: file.into(), media_type: "image/png".into(), width: 10, height: 10 };
        let mut asked = user("what is in these?");
        asked.images = vec![picture("1-a.png"), picture("1-b.png")];
        let mut opened = Message::tool_result("c1", "shot.png: this picture");
        opened.images = vec![picture("2-shot.png")];
        let messages = vec![Message::text("system", "s"), asked, assistant("", "", vec![call("c1", "read_file", json!({"path": "shot.png"}))]), opened];
        let said = build(&messages, &text(&lines));
        assert_eq!(said.turns[0].asked_images, ["1-a.png", "1-b.png"]);
        assert_eq!(said.turns[0].steps[0].calls[0].images, ["2-shot.png"]);
    }

    #[test]
    fn a_calls_long_arguments_and_output_keep_their_ends() {
        let long = format!("start{}end", "あ".repeat(5000));
        let big = json!({"path": "p.txt", "content": long});
        let lines = [
            turn_start(1, "2026-11-05T10:00:00.000+09:00", 2),
            request(1, 1, "2026-11-05T10:00:01.000+09:00", 100, 0, &["create_file"], "tool_calls"),
            tool(1, 1, "c1", "create_file", "2026-11-05T10:00:02.000+09:00", Some("p.txt")),
        ];
        let messages = vec![Message::text("system", "s"), user("go"), assistant("", "", vec![call("c1", "create_file", big)]), Message::tool_result("c1", &long)];
        let said = build(&messages, &text(&lines));
        let row = &said.turns[0].steps[0].calls[0];
        for shown in [row.arguments.as_deref().unwrap(), row.output.as_deref().unwrap()] {
            assert!(shown.contains("bytes left out") && shown.len() < 2 * END_BYTES + 100, "{} bytes", shown.len());
        }
        assert!(row.output.as_deref().unwrap().starts_with("start") && row.output.as_deref().unwrap().ends_with("end"));
        assert!(row.arguments.as_deref().unwrap().contains("start"));
    }

    #[test]
    fn a_session_is_read_with_the_trajectory_beside_it_and_one_without_is_no_error() {
        let dir = crate::in_container::tempdir().unwrap();
        let mut session = session::Session::create(dir.path(), dir.path(), "2026-11-05 10:00:00 +09:00", "20261105-100000").unwrap();
        for message in [Message::text("system", "s"), user("hello there"), assistant("Hi, I can help.", "", vec![])] {
            session.append(&message).unwrap();
        }
        let path = session.path.clone();
        let alone = read(&path).unwrap();
        assert_eq!((alone.turns.len(), alone.heading), (0, None));
        let lines = [turn_start(1, "2026-11-05T10:00:00.000+09:00", 11), request(1, 1, "2026-11-05T10:00:01.000+09:00", 100, "Hi, I can help.".chars().count() as u64, &[], "stop")];
        std::fs::write(trajectory::file_of(&path), text(&lines)).unwrap();
        let said = read(&path).unwrap();
        assert_eq!(said.turns[0].asked, "hello there");
        assert_eq!(said.turns[0].steps[0].why.as_ref().map(|why| why.text.as_str()), Some("Hi, I can help."));
        assert!(read(&dir.path().join("none.jsonl")).is_err(), "a session that is not there is an error");
    }

    fn at(seconds: &str) -> String {
        format!("2026-11-05T10:00:{seconds}+09:00")
    }

    /// The wait before the first word is split as the server tells it (the rest of it is waiting), a prompt read from its start for long is told
    /// apart, after the first word the thinking comes before the writing, a call's wait for the person before its run, and the app's own steps are marks.
    #[test]
    fn a_turns_band_splits_each_request_where_the_server_and_the_stream_say_and_puts_the_calls_in_a_lane_of_their_own() {
        let server: BTreeMap<String, Value> = [("ttft_seconds", 2.5), ("prefill_seconds", 1.2), ("resume_seconds", 0.3), ("kv_growth_seconds", 0.4), ("park_seconds", 0.1)].into_iter()
            .map(|(name, seconds)| (name.to_string(), serde_json::json!(seconds))).chain([("resumed_from".to_string(), serde_json::json!("none"))]).collect();
        let request = RequestDetail { started: at("01.000"), first_ms: Some(3000), ms: 5000, writing_ms: Some(4000), server, ..RequestDetail::default() };
        let call = CallRow { id: "c".into(), name: "execute_command".into(), started: at("06.000"), ms: 800, wait_ms: Some(1200), outcome: "ok".into(), ..CallRow::default() };
        let failed = CallRow { id: "d".into(), started: at("08.000"), ms: 100, exit_code: Some(7), wait_ms: None, ..call.clone() };
        let model = Step { kind: StepKind::Model, started: at("01.000"), ms: 5000, why: None, request: Some(request), calls: vec![call, failed], fields: Map::new() };
        let retry = Step { kind: StepKind::Retry { why: "transient".into(), wait_ms: 500 }, started: at("09.000"), ms: 0, why: None, request: None, calls: Vec::new(), fields: Map::new() };
        let turn = TurnPath { turn: 1, started: at("00.000"), steps: vec![model, retry], ended: Some(TurnEnd { ms: 10_000, cancelled: false, failed: false }), ..TurnPath::default() };
        let band = band(&turn).expect("a band");
        let parts: Vec<(Lane, u64, u64, Part)> = band.parts.iter().map(|part| (part.lane, part.from, part.to, part.part)).collect();
        assert_eq!(parts, [
            (Lane::Requests, 1000, 1500, Part::Waiting),
            (Lane::Requests, 1500, 1600, Part::Parking),
            (Lane::Requests, 1600, 2000, Part::Growing),
            (Lane::Requests, 2000, 3500, Part::Rereading),
            (Lane::Requests, 3500, 4000, Part::Waiting),
            (Lane::Requests, 4000, 5000, Part::Thinking),
            (Lane::Requests, 5000, 6000, Part::Writing),
            (Lane::Calls, 6000, 7200, Part::Person),
            (Lane::Calls, 7200, 8000, Part::Call),
            (Lane::Calls, 8000, 8100, Part::CallFailed),
        ]);
        assert_eq!((band.marks, band.length), (vec![9000], 10_000));
    }

    /// A short prompt read again is reading like any other; a request with no server's word waits for its first word as a whole; one kept before the
    /// stream's marks were is one part after its first word, as is one that only thought.
    #[test]
    fn a_band_without_the_servers_word_or_the_streams_marks_shows_what_it_can() {
        let server: BTreeMap<String, Value> = [("ttft_seconds".to_string(), serde_json::json!(0.5)), ("prefill_seconds".to_string(), serde_json::json!(0.4)), ("resumed_from".to_string(), serde_json::json!("none"))].into_iter().collect();
        let request = |server: BTreeMap<String, Value>, content_chars: u64| RequestDetail { started: at("00.000"), first_ms: Some(1000), ms: 3000, content_chars, server, ..RequestDetail::default() };
        let step = |request: RequestDetail| Step { kind: StepKind::Model, started: at("00.000"), ms: 3000, why: None, request: Some(request), calls: Vec::new(), fields: Map::new() };
        let turn = |steps: Vec<Step>| TurnPath { turn: 1, started: at("00.000"), steps, ..TurnPath::default() };
        let kinds = |turn: &TurnPath| band(turn).unwrap().parts.iter().map(|part| part.part).collect::<Vec<_>>();
        assert_eq!(kinds(&turn(vec![step(request(server.clone(), 40))])), [Part::Waiting, Part::Reading, Part::Waiting, Part::Generating]);
        assert_eq!(kinds(&turn(vec![step(request(BTreeMap::new(), 0))])), [Part::Waiting, Part::Thinking]);
        assert!(band(&TurnPath { started: "not a time".into(), ..TurnPath::default() }).is_none());
    }
}
