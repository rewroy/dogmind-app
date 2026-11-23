//! The Timeline view of the panel on the right, as decisions only: what the model and the app are doing now, where the work is heading, and the path the session took, as rows. What the files say
//! (`dogmind_agent::timeline`) and what the events say between two reads of them (`Live`) are put together here; `ui/timeline_view.rs` reads the files, measures the rows and draws them.
//!
//! Nothing is made up. The model's words are its own, and the person's request is theirs; a line that is neither is the app's (marked as such), or is drawn from the call the model made (and says so).
//! Every word that comes from a conversation is shown as the conversation shows it: what acts on text and shows nothing is shown as its mark, and keys and tokens are hidden but their beginning.

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use dogmind_agent::agent::Event;
use dogmind_agent::timeline::{self, Band, BandPart, CallRow, Heading, Lane, Part, RequestDetail, Step, StepKind, Timeline, TurnPath, WhySource};
use serde_json::Value;

use crate::cards::{duration_words, is_asking, token_count_words};
use crate::lang::{request_end_words, t, tf, tool_outcome_words};
use crate::toolrun::{partial_value, size_text};
use crate::ui::tool_name;

/// How many items of a plan or a list of tasks the top of the view shows: the rest are counted.
pub const MOST_ITEMS: usize = 6;
/// How many lines of a call's arguments or output a step that is open shows: its first lines, a row that counts the rest, and its last.
const MOST_CODE_LINES: usize = 48;
const CODE_HEAD: usize = 30;
const CODE_TAIL: usize = 14;
/// The longest a line of that is kept, in characters.
const LINE_CHARS: usize = 400;
/// How much of the reasoning of a request is kept for its last sentence, and of the arguments of a call being written for the name of its file, in bytes.
const THOUGHT_KEPT: usize = 8192;
const WRITING_KEPT: usize = 2048;
/// A counter of seconds appears at this many, as the line above the input has it.
pub const SECONDS_SHOWN_FROM: u64 = 3;

/// Who is at work: it decides the word and the colour beside a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who {
    Model,
    /// Dogmind itself: its guards, its calls to the tools, its compaction.
    App,
    /// The person is asked, and the work waits for them.
    Person,
    Idle,
}

impl Who {
    /// The word beside the line.
    pub fn word(self) -> &'static str {
        match self {
            Who::Model => t("Model"),
            Who::App => "Dogmind",
            Who::Person => t("You"),
            Who::Idle => "",
        }
    }
}

/// What a person is asked and has not answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersonWait {
    Approval { tool: String, summary: String },
    Question,
    Plan,
}

/// What the window knows besides the events: whether a turn is at work, what the person is asked, and how much waits in the background.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inputs {
    pub running: bool,
    pub wait: Option<PersonWait>,
    pub background: usize,
    /// The time now, in milliseconds since 1970: the band of a turn at work reaches it. None: it ends where the files do.
    pub wall_ms: Option<i64>,
    /// What the person looks for, in small letters: only the turns and steps with it are shown, open. Empty: all of them.
    pub query: String,
    /// The part of a turn's time the person chose to look at (by the turn, from and to milliseconds after it began): its band shows that part
    /// across its width, and only the steps that ran in it are shown.
    pub spans: BTreeMap<u64, (u64, u64)>,
}

/// The shortest part of a turn's time that can be looked at, in milliseconds.
pub const SHORTEST_SPAN: u64 = 50;

/// Whether a step has what is looked for (`query`, in small letters): in why it was taken, its calls' tools (as named here and as the model named
/// them) and what they were on, or what its helper did.
fn step_has(step: &Step, query: &str) -> bool {
    let has = |text: &str| text.to_lowercase().contains(query);
    step.why.as_ref().is_some_and(|why| has(&why.text))
        || step.calls.iter().any(|call| has(&call.name) || has(t(tool_name(&call.name))) || call.target.as_deref().is_some_and(has)
            || call.helper_steps.iter().any(|helper| helper.why.as_ref().is_some_and(|why| has(&why.text)) || helper.calls.iter().any(|(name, target)| has(name) || target.as_deref().is_some_and(has))))
        || match &step.kind {
            StepKind::Model => false,
            other => has(&harness_words(other).2),
        }
}

/// What is going on in the request that is under way, as the events say it.
#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Idle,
    /// The turn has begun and nothing of the model's has.
    Starting,
    /// The request is made; the model has not said a word.
    Asking,
    Reading { done: u64, total: u64 },
    Thinking,
    /// The arguments of a call are being written (`Live::writing` says which).
    Writing,
    Answering,
    /// The model's answer is whole; its calls are looked at before they run.
    Checking,
    Running,
    /// The calls are done and what they brought back is on its way to the model.
    Preparing,
    Compacting,
    Retrying { why: String },
    Restarting { progress: Option<String> },
}

/// A call being written: which, its start (for the name of its file) and how long its arguments are.
#[derive(Debug, Clone, PartialEq)]
struct Writing {
    index: usize,
    tool: String,
    head: String,
    bytes: usize,
}

/// A call under way.
#[derive(Debug, Clone, PartialEq)]
struct Running {
    id: String,
    tool: String,
    summary: String,
    since: Instant,
}

/// The turn at work, from the window's side: what the person asked, whether nobody did, and how many turns the files had when it began.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Begun {
    asked: String,
    woken: bool,
    files: usize,
}

/// What the events say of the present, kept between two reads of the files: the files have what has ended, and this has what is going on.
#[derive(Debug, Clone)]
pub struct Live {
    phase: Phase,
    since: Instant,
    /// The reasoning of the request under way, its end.
    reasoning: String,
    writing: Vec<Writing>,
    tools: Vec<Running>,
    /// When the person was last asked.
    asked_at: Option<Instant>,
    pending_asked: Option<String>,
    pending_woken: bool,
    begun: Option<Begun>,
    /// How many turns the files had when they were last read.
    file_turns: usize,
    /// Changes when what the rows are made of does (not with each piece of reasoning).
    version: u64,
    /// An explore's helper at work: what it was asked, the step it is on of how many it may take, and what that step calls.
    helper: Option<HelperLive>,
    /// A program at work (`id` is its `run_code` call's): the call it has come to, the tool and what that is on.
    program: Option<ProgramLive>,
}

#[derive(Debug, Clone, PartialEq)]
struct ProgramLive {
    id: String,
    call: u32,
    tool: String,
    summary: String,
}

#[derive(Debug, Clone, PartialEq)]
struct HelperLive {
    id: String,
    task: String,
    step: u32,
    most: u32,
    tool: String,
    summary: String,
}

impl Live {
    pub fn new(at: Instant) -> Live {
        Live { phase: Phase::Idle, since: at, reasoning: String::new(), writing: Vec::new(), tools: Vec::new(), asked_at: None, pending_asked: None, pending_woken: false, begun: None, file_turns: 0, version: 0, helper: None, program: None }
    }

    /// The session on screen is another: nothing of the last is its.
    pub fn reset(&mut self, at: Instant) {
        let version = self.version + 1;
        *self = Live::new(at);
        self.version = version;
    }

    /// Changes whenever the rows would be different for what the events said (the phase, the calls under way, the turn at work); the figures of a phase do not change it.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The person's words, as the turn they begin will have them: the turn that begins next is theirs.
    pub fn will_ask(&mut self, words: &str) {
        self.pending_asked = Some(ask_line(words));
    }

    /// The files were read: they have this many turns.
    pub fn timeline_came(&mut self, turns: usize) {
        self.file_turns = turns;
    }

    /// A call under way that the events did not say (a session that was out of sight and is back).
    pub fn seed_running(&mut self, id: &str, tool: &str, summary: &str, at: Instant) {
        if !is_asking(tool) && !self.tools.iter().any(|running| running.id == id) {
            self.tools.push(Running { id: id.to_string(), tool: tool.to_string(), summary: summary.to_string(), since: at });
            self.enter(Phase::Running, at);
            self.version += 1;
        }
    }

    fn enter(&mut self, phase: Phase, at: Instant) {
        if std::mem::discriminant(&self.phase) != std::mem::discriminant(&phase) {
            self.since = at;
            self.version += 1;
        }
        self.phase = phase;
    }

    /// What an event changes. Whether it changed anything the view shows.
    pub fn note(&mut self, event: &Event, at: Instant) -> bool {
        let before = (self.phase.clone(), self.reasoning.len(), self.writing.iter().map(|writing| writing.bytes).sum::<usize>(), self.tools.len(), self.helper.as_ref().map(|helper| helper.step), self.program.as_ref().map(|program| program.call));
        match event {
            Event::Woken(_) => self.pending_woken = true,
            Event::HelperStarted { id, task, most, .. } => {
                self.helper = Some(HelperLive { id: id.clone(), task: ask_line(task), step: 0, most: *most, tool: String::new(), summary: String::new() });
                self.version += 1;
            }
            Event::HelperStep { id, step, most, tool, summary, .. } => {
                if let Some(helper) = self.helper.as_mut().filter(|helper| helper.id == *id) {
                    (helper.step, helper.most, helper.tool, helper.summary) = (*step, *most, tool.clone(), summary.clone());
                    self.version += 1;
                }
            }
            // (a program's call is a step of the call that runs it, which is under way)
            Event::ProgramStep { id, call, tool, summary } => {
                if self.tools.iter().any(|running| running.id == *id) {
                    self.program = Some(ProgramLive { id: id.clone(), call: *call, tool: tool.clone(), summary: summary.clone() });
                    self.version += 1;
                }
            }
            Event::HelperEnded { id, .. } => {
                if self.helper.as_ref().is_some_and(|helper| helper.id == *id) {
                    self.helper = None;
                    self.version += 1;
                }
            }
            Event::TurnStarted => {
                self.helper = None;
                self.program = None;
                self.reasoning.clear();
                self.writing.clear();
                self.tools.clear();
                let woken = std::mem::take(&mut self.pending_woken);
                let asked = self.pending_asked.take().filter(|_| !woken).unwrap_or_default();
                self.begun = Some(Begun { asked, woken, files: self.file_turns });
                self.version += 1;
                self.enter(Phase::Starting, at);
            }
            Event::RequestStarted => {
                self.reasoning.clear();
                self.writing.clear();
                self.enter(Phase::Asking, at);
            }
            Event::Reading { done, total } => self.enter(Phase::Reading { done: *done, total: *total }, at),
            Event::Reasoning(text) => {
                self.reasoning.push_str(text);
                if self.reasoning.len() > THOUGHT_KEPT * 2 {
                    let mut from = self.reasoning.len() - THOUGHT_KEPT;
                    while !self.reasoning.is_char_boundary(from) {
                        from += 1;
                    }
                    self.reasoning.drain(..from);
                }
                self.enter(Phase::Thinking, at);
            }
            Event::Content(_) => self.enter(Phase::Answering, at),
            Event::ToolWriting { index, tool, piece } => {
                let place = match self.writing.iter().position(|writing| writing.index == *index) {
                    Some(place) => place,
                    None => {
                        self.writing.push(Writing { index: *index, tool: tool.clone(), head: String::new(), bytes: 0 });
                        self.writing.len() - 1
                    }
                };
                let writing = &mut self.writing[place];
                writing.bytes += piece.len();
                // (only its start is kept, for the name of its file)
                let room = WRITING_KEPT.saturating_sub(writing.head.len());
                let mut take = piece.len().min(room);
                while !piece.is_char_boundary(take) {
                    take -= 1;
                }
                writing.head.push_str(&piece[..take]);
                self.enter(Phase::Writing, at);
            }
            Event::AssistantEnded => {
                self.writing.clear();
                if self.tools.is_empty() {
                    self.enter(Phase::Checking, at);
                }
            }
            Event::ToolStarted { id, tool, summary } if !is_asking(tool) => {
                if let Some(place) = self.writing.iter().position(|writing| writing.tool == *tool) {
                    self.writing.remove(place);
                }
                if !self.tools.iter().any(|running| running.id == *id) {
                    self.tools.push(Running { id: id.clone(), tool: tool.clone(), summary: summary.clone(), since: at });
                    self.version += 1;
                }
                self.enter(Phase::Running, at);
            }
            Event::ToolEnded { id, .. } => {
                if let Some(place) = self.tools.iter().position(|running| running.id == *id) {
                    self.tools.remove(place);
                    if self.program.as_ref().is_some_and(|program| program.id == *id) {
                        self.program = None;
                    }
                    self.version += 1;
                    if self.tools.is_empty() {
                        self.enter(Phase::Preparing, at);
                    }
                }
            }
            Event::Compacting => self.enter(Phase::Compacting, at),
            Event::Retrying { why, .. } => self.enter(Phase::Retrying { why: why.clone() }, at),
            Event::ServerRestarting(progress) => {
                let progress = (!progress.is_null()).then(|| crate::engine::progress_text(progress));
                self.enter(Phase::Restarting { progress }, at);
            }
            Event::Approval(_) | Event::Question(_) | Event::PlanReview(_) => {
                self.asked_at = Some(at);
                self.version += 1;
            }
            Event::TurnEnded { .. } => {
                self.program = None;
                self.reasoning.clear();
                self.writing.clear();
                self.tools.clear();
                self.begun = None;
                self.version += 1;
                self.enter(Phase::Idle, at);
            }
            _ => {}
        }
        before != (self.phase.clone(), self.reasoning.len(), self.writing.iter().map(|writing| writing.bytes).sum::<usize>(), self.tools.len(), self.helper.as_ref().map(|helper| helper.step), self.program.as_ref().map(|program| program.call))
    }

    /// Whether a call of this id is under way.
    pub fn is_running(&self, id: &str) -> bool {
        self.tools.iter().any(|running| running.id == id)
    }

    /// What the part of the band that is going on now is, in which lane, and since when: the person asked, a call running, the model reading, thinking or
    /// writing, or a wait of the app's or the server's. None when no turn is at work.
    pub fn band_part(&self, inputs: &Inputs) -> Option<(Lane, Part, Instant)> {
        if !inputs.running {
            return None;
        }
        if inputs.wait.is_some() {
            return Some((Lane::Calls, Part::Person, self.asked_at.unwrap_or(self.since)));
        }
        let (lane, part) = match self.phase {
            Phase::Idle => return None,
            Phase::Running => {
                let since = self.tools.iter().map(|running| running.since).min().unwrap_or(self.since);
                return Some((Lane::Calls, Part::Call, since));
            }
            Phase::Reading { .. } => (Lane::Requests, Part::Reading),
            Phase::Thinking => (Lane::Requests, Part::Thinking),
            Phase::Writing | Phase::Answering => (Lane::Requests, Part::Writing),
            Phase::Starting | Phase::Asking | Phase::Checking | Phase::Preparing | Phase::Compacting | Phase::Retrying { .. } | Phase::Restarting { .. } => (Lane::Requests, Part::Waiting),
        };
        Some((lane, part, self.since))
    }

    /// Whether the files, read when they had this many turns, can have the turn that is at work.
    fn files_have_the_turn(&self, turns: usize) -> bool {
        self.begun.as_ref().is_none_or(|begun| turns > begun.files)
    }

    /// The turn at work as the window knows it, for the time before the files do.
    fn turn_not_in_file(&self, number: u64) -> TurnPath {
        let begun = self.begun.clone().unwrap_or(Begun { asked: String::new(), woken: false, files: 0 });
        TurnPath { turn: number, asked: begun.asked, woken: begun.woken, ..TurnPath::default() }
    }

    /// The last sentence of what the model has reasoned in the request under way: its own words.
    pub fn thought(&self) -> Option<String> {
        timeline::last_sentence(&self.reasoning).map(|sentence| safe(&sentence))
    }

    /// What is going on now.
    pub fn now(&self, inputs: &Inputs, at: Instant) -> Now {
        let seconds = |from: Instant| Some(at.saturating_duration_since(from).as_secs());
        if let Some(wait) = &inputs.wait {
            let text = match wait {
                PersonWait::Approval { tool, summary } => tf("Waiting for your approval: {0}", &[&call_words(tool, summary)]),
                PersonWait::Question => t("Waiting for your answer").to_string(),
                PersonWait::Plan => t("Waiting for you to review a plan").to_string(),
            };
            return Now { who: Who::Person, text, seconds: self.asked_at.and_then(seconds) };
        }
        if !inputs.running {
            return if inputs.background > 0 {
                Now { who: Who::App, text: t("Waiting for background work").to_string(), seconds: None }
            } else {
                Now { who: Who::Idle, text: t("Idle").to_string(), seconds: None }
            };
        }
        let (who, text, from) = match &self.phase {
            Phase::Idle => (Who::App, t("Working").to_string(), self.since),
            Phase::Starting => (Who::App, t("Starting the turn").to_string(), self.since),
            Phase::Asking => (Who::Model, t("Waiting for the model's first words").to_string(), self.since),
            Phase::Reading { done, total } => {
                let percent = (done * 100).checked_div(*total).unwrap_or(0);
                (Who::Model, tf("Reading the conversation: {0} of {1} tokens ({2}%)", &[&token_count_words(*done), &token_count_words(*total), &percent]), self.since)
            }
            Phase::Thinking => (Who::Model, t("Thinking…").to_string(), self.since),
            Phase::Writing => {
                let writing = self.writing.iter().max_by_key(|writing| writing.index);
                let words = writing.map_or(String::new(), |writing| {
                    let target = partial_value(&writing.head, "path").map(|path| safe(&path)).unwrap_or_default();
                    tf("Writing the arguments of {0}: {1}", &[&call_words(&writing.tool, &target), &size_text(writing.bytes)])
                });
                (Who::Model, if words.is_empty() { t("Writing a call").to_string() } else { words }, self.since)
            }
            Phase::Answering => (Who::Model, t("Writing the answer").to_string(), self.since),
            Phase::Checking => (Who::App, t("Checking the calls").to_string(), self.since),
            // (an explore's helper at work: its own model, the step it is on, and what that step calls)
            Phase::Running if self.helper.is_some() => {
                let helper = self.helper.as_ref().expect("a helper");
                let words = if helper.step == 0 {
                    tf("A helper explores: {0}", &[&helper.task])
                } else {
                    tf("A helper explores, step {0} of {1}: {2}", &[&helper.step, &helper.most, &call_words(&helper.tool, &helper.summary)])
                };
                (Who::Model, words, self.tools.iter().map(|running| running.since).min().unwrap_or(self.since))
            }
            // (a program at work: what it is, and the call it has come to)
            Phase::Running if self.program.as_ref().is_some_and(|program| self.tools.iter().any(|running| running.id == program.id)) => {
                let program = self.program.as_ref().expect("a program");
                let running = self.tools.iter().find(|running| running.id == program.id).expect("its call");
                let mut words = tf("A program runs: {0}, call {1}: {2}", &[&running.summary, &program.call, &call_words(&program.tool, &program.summary)]);
                if self.tools.len() > 1 {
                    words = format!("{words}  ·  {}", tf("and {0} more", &[&(self.tools.len() - 1)]));
                }
                (Who::App, words, self.tools.iter().map(|running| running.since).min().unwrap_or(self.since))
            }
            Phase::Running => {
                let first = self.tools.first();
                let mut words = first.map_or(String::new(), |running| tf("Running {0}", &[&call_words(&running.tool, &running.summary)]));
                if self.tools.len() > 1 {
                    words = format!("{words}  ·  {}", tf("and {0} more", &[&(self.tools.len() - 1)]));
                }
                (Who::App, if words.is_empty() { t("Working").to_string() } else { words }, self.tools.iter().map(|running| running.since).min().unwrap_or(self.since))
            }
            Phase::Preparing => (Who::App, t("Sending the results to the model").to_string(), self.since),
            Phase::Compacting => (Who::App, t("Compacting the session…").to_string(), self.since),
            Phase::Retrying { why } => (Who::App, tf("Retrying the request: {0}", &[&retry_words(why)]), self.since),
            Phase::Restarting { progress } => (Who::App, progress.as_ref().map_or_else(|| t("Waiting for the server to restart…").to_string(), |progress| tf("Waiting for the server: {0}", &[progress])), self.since),
        };
        Now { who, text, seconds: seconds(from) }
    }
}

/// What is going on, in a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Now {
    pub who: Who,
    pub text: String,
    /// How long it has gone on; none for what takes no time (idle, waiting in the background).
    pub seconds: Option<u64>,
}

/// A tool and what it is on, as the line above the input has them.
fn call_words(tool: &str, target: &str) -> String {
    let name = t(tool_name(tool));
    if target.is_empty() { name.to_string() } else { format!("{name}  {target}") }
}

fn retry_words(why: &str) -> String {
    match why {
        "transient" => t("the connection broke").to_string(),
        "overflow" => t("the conversation was too long").to_string(),
        "empty" => t("the answer was empty").to_string(),
        "server_restart" => t("the server was restarting").to_string(),
        other => other.replace('_', " "),
    }
}

/// The mark of a tool by what it does: reads, searches, commands, the web, writes, else a tool.
fn tool_icon(name: &str) -> &'static str {
    match name {
        "read_file" | "list_files" | "find_files" => "file",
        "search_files" => "search",
        "execute_command" | "command_status" | "command_stop" => "terminal",
        "web_search" | "web_read" | "check_page" => "globe",
        "create_file" | "apply_patch" => "pencil",
        _ => "wrench",
    }
}

/// The model's words as one line shows them: a link as its words without its address, code and strong words without their marks. Nothing
/// else of them changes.
fn plain_words(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if c == '[' {
            if let crate::markdown::Link::Whole { text: words, length, .. } = crate::markdown::parse_link(rest) {
                out.push_str(words);
                rest = &rest[length.min(rest.len())..];
                continue;
            }
        }
        // (a pair of stars, or a backtick: marks the conversation does not show as they are)
        if rest.starts_with("**") || rest.starts_with("__") {
            rest = &rest[2..];
            continue;
        }
        if c == '`' {
            rest = &rest[1..];
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Words as the conversation shows them.
fn safe(text: &str) -> String {
    crate::mask::hide_keys(&crate::markdown::show_hidden(text)).into_owned()
}

/// The first line of the person's words that has any, in the length the files keep it to.
fn ask_line(words: &str) -> String {
    let line = words.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
    cap(line, 120)
}

fn cap(text: &str, most: usize) -> String {
    if text.chars().count() <= most {
        return text.to_string();
    }
    format!("{}…", text.chars().take(most).collect::<String>().trim_end())
}

// ---- Where the work is heading.

/// Where a list the top of the view shows comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The plan the person approved.
    Plan,
    /// The checklist the model keeps.
    Todos,
    /// The model's own last sentence of reasoning, for want of either.
    Thought,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// A plan's item, which has none.
    None,
    Pending,
    Doing,
    Done,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub text: String,
    pub progress: Progress,
}

/// Where the work is heading: the items, and how many there were before and after those shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadingView {
    pub source: Source,
    pub caption: String,
    pub items: Vec<Item>,
    pub before: usize,
    pub after: usize,
}

fn progress_of(status: &str) -> Progress {
    match status {
        "done" | "completed" => Progress::Done,
        "doing" | "in_progress" => Progress::Doing,
        "blocked" => Progress::Blocked,
        _ => Progress::Pending,
    }
}

/// The items that fit, with the one the work is at (`anchor`) among them and the one before it for what it follows from.
fn listed(source: Source, caption: String, items: Vec<Item>, anchor: usize) -> HeadingView {
    let start = if items.len() > MOST_ITEMS { anchor.saturating_sub(1).min(items.len() - MOST_ITEMS) } else { 0 };
    let end = (start + MOST_ITEMS).min(items.len());
    let after = items.len() - end;
    HeadingView { source, caption, items: items.into_iter().skip(start).take(end - start).collect(), before: start, after }
}

/// Where the work is heading: the plan the person approved, else the model's checklist, else, while a turn runs, the last sentence the model has reasoned in the request under way, else nothing.
pub fn heading(timeline: Option<&Timeline>, live: &Live, inputs: &Inputs) -> Option<HeadingView> {
    match timeline.and_then(|timeline| timeline.heading.as_ref()) {
        Some(Heading::Plan { items }) => Some(listed(Source::Plan, t("The approved plan").to_string(), items.iter().map(|text| Item { text: safe(text), progress: Progress::None }).collect(), 0)),
        Some(Heading::Todos { items }) => {
            let items: Vec<Item> = items.iter().map(|item| Item { text: safe(&item.text), progress: progress_of(&item.status) }).collect();
            let anchor = items.iter().position(|item| item.progress != Progress::Done).unwrap_or(items.len());
            Some(listed(Source::Todos, t("The model's task list").to_string(), items, anchor))
        }
        None if inputs.running => live.thought().map(|sentence| HeadingView { source: Source::Thought, caption: t("The model's latest thought").to_string(), items: vec![Item { text: sentence, progress: Progress::None }], before: 0, after: 0 }),
        None => None,
    }
}

// ---- The path.

/// What a click on a row opens or shuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Turn(u64),
    /// A turn's step, by its place among the turn's steps.
    Step(u64, usize),
    /// The rest of what the server says of a step's request, folded under its first numbers.
    Server(u64, usize),
    /// A press on it shows the whole of a turn's time again, where a part of it was chosen.
    Whole(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    /// A turn: folded to a line, or its request with the steps under it.
    Turn,
    Step,
    Call,
    /// A name and its value, in a step's details.
    #[default]
    Fact,
    /// The name of a group of lines in the details.
    Caption,
    /// A line of arguments or of output, in the face for code.
    Code,
    /// The step in progress, which the files do not have yet.
    Live,
    /// A word on its own: there is nothing to show.
    Note,
    /// An open turn's time as a band (`Row::band`), with what its colours are under it.
    Band,
    /// Which part of the turn's time its band shows, and a press to show all of it (`Key::Whole`).
    Span,
    /// Small copies of pictures (`Row::pictures`): those the person sent with the request, or those a call's result opened.
    Pictures,
}

/// Whose words a row has, or how it came out: the colour of a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tone {
    /// The person's.
    Person,
    /// The model's own.
    Model,
    /// Made from the call it made, not said by the model.
    Derived,
    /// Dogmind's.
    App,
    Good,
    Bad,
    #[default]
    Plain,
    Muted,
}

/// A row of the path.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Row {
    pub kind: Kind,
    pub key: Option<Key>,
    /// What `key` names is open.
    pub open: bool,
    pub tone: Tone,
    pub icon: &'static str,
    /// A word before the text, in small type: "This turn's request", or a fact's name.
    pub label: String,
    pub text: String,
    /// A small word after the text: where it comes from.
    pub tag: String,
    /// At the right edge: how it came out, or how long it took.
    pub right: String,
    pub right_tone: Tone,
    pub indent: u8,
    /// The step in progress.
    pub current: bool,
    /// A band row's band; for a turn at work its last part is the one going on now (`live_part`).
    pub band: Option<Band>,
    /// The part of the band going on now, at its end.
    pub live_part: Option<BandPart>,
    /// The turn a band row is of: a press on a part of it goes to the step it was of.
    pub turn: u64,
    /// A band row's whole length, and the part of it the band shows (from and to milliseconds after the turn began), when a part was chosen.
    pub whole: u64,
    pub span: Option<(u64, u64)>,
    /// A pictures row's pictures: their files in the session's folder of pictures.
    pub pictures: Vec<String>,
}

/// When each step of a turn ran, by its place: from and to milliseconds after the turn began, its calls' time being its own.
fn step_times(turn: &TurnPath) -> Vec<(usize, u64, u64)> {
    let Some(start) = timeline::millis(&turn.started) else { return Vec::new() };
    let after = |time: &str| timeline::millis(time).map(|at| (at - start).max(0) as u64);
    turn.steps.iter().enumerate().filter_map(|(place, step)| {
        let begin = after(&step.started)?;
        // (a step lasts while its calls run too: they are its)
        let calls_end = step.calls.iter().filter_map(|call| after(&call.started).map(|from| from + call.wait_ms.unwrap_or(0) + call.ms)).max().unwrap_or(0);
        Some((place, begin, (begin + step.ms).max(calls_end).max(begin + 1)))
    }).collect()
}

/// The step of a turn a time of its band (milliseconds after it began) was in: the one whose request or calls ran then, else the last that had
/// begun by then.
pub fn step_at(turn: &TurnPath, ms: u64) -> Option<usize> {
    let spans = step_times(turn);
    spans.iter().find(|(_, begin, end)| (*begin..*end).contains(&ms)).or_else(|| spans.iter().filter(|(_, begin, _)| *begin <= ms).last()).map(|(place, ..)| *place)
}

/// A band's part from `from` to `to`, laid from its start: its parts and marks in it, cut at its ends.
fn band_within(band: &Band, from: u64, to: u64) -> Band {
    Band {
        length: to - from,
        parts: band.parts.iter().filter_map(|part| part_within(part, from, to)).collect(),
        marks: band.marks.iter().filter(|at| (from..=to).contains(*at)).map(|at| at - from).collect(),
    }
}

fn part_within(part: &BandPart, from: u64, to: u64) -> Option<BandPart> {
    (part.to > from && part.from < to).then(|| BandPart { from: part.from.max(from) - from, to: part.to.min(to) - from, ..*part })
}

/// A chosen part of a turn's time, kept inside the turn as long as it is now and never shorter than the shortest that can be chosen.
pub fn span_inside(span: (u64, u64), whole: u64) -> Option<(u64, u64)> {
    let length = (span.1.saturating_sub(span.0)).max(SHORTEST_SPAN);
    if whole <= length {
        return None;
    }
    let from = span.0.min(whole - length);
    Some((from, from + length))
}

/// The parts a band's key names, in the order it names them: those that are in the band, then whether it has the app's own marks.
pub fn band_key(band: &Band, live: Option<&BandPart>) -> (Vec<Part>, bool) {
    let mut parts: Vec<Part> = band.parts.iter().chain(live).map(|part| part.part).collect();
    parts.sort();
    parts.dedup();
    (parts, !band.marks.is_empty())
}

/// What a part of the band is, in a few words.
pub fn part_words(part: Part) -> &'static str {
    match part {
        Part::Waiting => t("Waiting (queue, network)"),
        Part::Parking => t("Setting another conversation aside"),
        Part::Growing => t("Growing the K/V"),
        Part::Reading => t("Reading the prompt"),
        Part::Rereading => t("Reading it all again"),
        Part::Thinking => t("Thinking"),
        Part::Writing => t("Writing"),
        Part::Generating => t("Thinking and writing"),
        Part::Call => t("Calls"),
        Part::CallFailed => t("Failed calls"),
        Part::Person => t("Waiting for you"),
    }
}

/// An open turn's band: from the files, and for a turn at work to the time now, its last part the one going on.
fn band_row(turn: &TurnPath, is_live: bool, context: &Context) -> Option<Row> {
    let mut band = timeline::band(turn).unwrap_or_default();
    let mut live_part = None;
    if is_live {
        let start = timeline::millis(&turn.started);
        if let (Some(start), Some(now), Some((lane, part, since))) = (start, context.inputs.wall_ms, context.live.band_part(context.inputs)) {
            let now = (now - start).max(0) as u64;
            // (from when it began, as the window's clock has it, and not before what the files have of its lane)
            let began = now.saturating_sub(context.at.saturating_duration_since(since).as_millis() as u64);
            let known = band.parts.iter().filter(|known| known.lane == lane).map(|known| known.to).max().unwrap_or(0);
            let from = began.max(known).min(now);
            band.length = band.length.max(now);
            live_part = Some(BandPart { lane, from, to: now, part });
        }
    }
    if band.length == 0 {
        return None;
    }
    let whole = band.length;
    // (a part of it chosen: that part across the band's width)
    let span = context.inputs.spans.get(&turn.turn).and_then(|span| span_inside(*span, whole));
    if let Some((from, to)) = span {
        band = band_within(&band, from, to);
        live_part = live_part.and_then(|part| part_within(&part, from, to));
    }
    Some(Row { kind: Kind::Band, indent: 1, band: Some(band), live_part, turn: turn.turn, whole, span, ..Row::default() })
}

/// Under a band that shows a part of its turn's time: which part, and a press to show it all.
fn span_row(turn: u64, (from, to): (u64, u64)) -> Row {
    Row { kind: Kind::Span, key: Some(Key::Whole(turn)), indent: 1, icon: "x", tone: Tone::Muted, text: tf("{0} to {1} of the turn", &[&ms_words(from), &ms_words(to)]), right: t("Show all of it").to_string(), right_tone: Tone::Model, turn, ..Row::default() }
}

/// What the rows are made from besides the files.
struct Context<'a> {
    live: &'a Live,
    inputs: &'a Inputs,
    opened: &'a HashMap<Key, bool>,
    /// The window's clock now: what is going on began this long before it.
    at: Instant,
}

/// The rows of the path, the earlier turns folded to a line each and the latest open; a click's choices in `opened` win over both. While a turn runs, the step in progress is the last row of it:
/// the model reading, thinking, writing or the app at work on a call, or, when calls of the last step are under way, that step itself.
pub fn rows(timeline: Option<&Timeline>, live: &Live, inputs: &Inputs, opened: &HashMap<Key, bool>, at: Instant) -> Vec<Row> {
    let turns: &[TurnPath] = timeline.map_or(&[], |timeline| timeline.turns.as_slice());
    // (the turn at work may not be in the files yet: the app writes its first line after it says the turn has begun)
    let in_file = inputs.running && turns.last().is_some_and(|last| last.ended.is_none()) && live.files_have_the_turn(turns.len());
    let made = (inputs.running && !in_file).then(|| live.turn_not_in_file(turns.last().map_or(1, |last| last.turn + 1)));
    let all: Vec<&TurnPath> = turns.iter().chain(made.iter()).collect();
    let context = Context { live, inputs, opened, at };
    let now = live.now(inputs, at);
    let mut out = Vec::new();
    let query = inputs.query.trim().to_lowercase();
    let query = query.as_str();
    for (place, turn) in all.iter().enumerate() {
        let latest = place + 1 == all.len();
        // (looked for: a turn with it in its request or in a step is shown, open, with only the steps that have it)
        if !query.is_empty() && !turn.asked.to_lowercase().contains(query) && !turn.steps.iter().any(|step| step_has(step, query)) {
            continue;
        }
        turn_rows(&mut out, turn, latest && inputs.running, latest, &context, &now);
    }
    if out.is_empty() {
        let none = if query.is_empty() { t("Nothing has happened in this session yet.") } else { t("No matches") };
        out.push(Row { kind: Kind::Note, tone: Tone::Muted, text: none.to_string(), ..Row::default() });
    }
    out
}

fn turn_rows(out: &mut Vec<Row>, turn: &TurnPath, is_live: bool, latest: bool, context: &Context, now: &Now) {
    let key = Key::Turn(turn.turn);
    let query = context.inputs.query.trim().to_lowercase();
    let query = query.as_str();
    let open = !query.is_empty() || context.opened.get(&key).copied().unwrap_or(latest);
    let (result, result_tone) = turn_result(turn, is_live);
    let asked = if turn.woken {
        String::new()
    } else if turn.asked.is_empty() {
        t("Its request is no longer in the conversation").to_string()
    } else {
        safe(&turn.asked)
    };
    if !open {
        let (text, tone) = if turn.woken { (t("Woken by background work").to_string(), Tone::App) } else { (asked, Tone::Person) };
        out.push(Row { kind: Kind::Turn, key: Some(key), open: false, tone, icon: "chevron-right", text, right: result, right_tone: result_tone, ..Row::default() });
        return;
    }
    let label = if turn.woken { t("Woken by background work") } else { t("This turn's request") };
    out.push(Row { kind: Kind::Turn, key: Some(key), open: true, tone: if turn.woken { Tone::App } else { Tone::Person }, icon: "chevron-down", label: label.to_string(), text: asked, right: result, right_tone: result_tone, ..Row::default() });
    // (what the person sent with it, under their words)
    if !turn.asked_images.is_empty() {
        out.push(Row { kind: Kind::Pictures, indent: 1, pictures: turn.asked_images.clone(), ..Row::default() });
    }
    // (its time as a band under the request, where the wait was: the steps under it say what each part was)
    let band = band_row(turn, is_live, context);
    // (a part of its time chosen: which, and only the steps that ran in it)
    let span = band.as_ref().and_then(|band| band.span);
    out.extend(band);
    out.extend(span.map(|span| span_row(turn.turn, span)));
    let times = span.map(|_| step_times(turn)).unwrap_or_default();
    let in_span = |place: usize| span.is_none_or(|(from, to)| times.iter().any(|(at, begin, end)| *at == place && *begin < to && *end > from));
    // The step whose calls are under way is the one in progress; with none, the step in progress is a row of its own.
    let last_model = turn.steps.iter().rposition(|step| step.kind == StepKind::Model);
    let running_here = is_live && last_model.is_some_and(|at| turn.steps[at].calls.iter().any(|call| context.live.is_running(&call.id)));
    for (place, step) in turn.steps.iter().enumerate() {
        // (looked for: only the steps that have it, unless it is in the turn's request)
        if !query.is_empty() && !turn.asked.to_lowercase().contains(query) && !step_has(step, query) {
            continue;
        }
        if !in_span(place) {
            continue;
        }
        step_rows(out, turn, place, step, is_live, running_here && Some(place) == last_model, context);
    }
    if is_live && !running_here {
        let tone = match now.who {
            Who::Model => Tone::Model,
            Who::Person => Tone::Person,
            _ => Tone::App,
        };
        out.push(Row { kind: Kind::Live, tone, icon: "cpu", label: now.who.word().to_string(), text: now.text.clone(), indent: 1, current: true, ..Row::default() });
    }
}

/// How a turn came out, in a word, with its time: a turn that is at work has none yet.
fn turn_result(turn: &TurnPath, is_live: bool) -> (String, Tone) {
    match (&turn.ended, is_live) {
        (_, true) => (t("Running").to_string(), Tone::Muted),
        (Some(end), _) if end.failed => (format!("{}  ·  {}", t("Failed"), ms_words(end.ms)), Tone::Bad),
        (Some(end), _) if end.cancelled => (format!("{}  ·  {}", t("Stopped"), ms_words(end.ms)), Tone::Muted),
        (Some(end), _) => (format!("{}  ·  {}", t("Done"), ms_words(end.ms)), Tone::Good),
        (None, _) => (t("Interrupted").to_string(), Tone::Muted),
    }
}

/// A time of a few milliseconds up to a few minutes, in words.
pub fn ms_words(ms: u64) -> String {
    match ms {
        0..=999 => tf("{0} ms", &[&ms]),
        1000..=9999 => tf("{0} s", &[&format!("{:.1}", ms as f64 / 1000.0)]),
        _ => duration_words(ms / 1000),
    }
}

/// The time of day of a time the files give (`2026-11-05T10:00:03.250+09:00`): its hours, minutes and seconds.
fn clock_of(started: &str) -> String {
    match (started.get(10..11), started.get(11..19)) {
        (Some("T"), Some(clock)) => clock.to_string(),
        _ => String::new(),
    }
}

fn step_rows(out: &mut Vec<Row>, turn: &TurnPath, place: usize, step: &Step, is_live: bool, current: bool, context: &Context) {
    let key = Key::Step(turn.turn, place);
    let open = context.opened.get(&key).copied().unwrap_or(false);
    let time = if step.ms > 0 { ms_words(step.ms) } else { String::new() };
    match &step.kind {
        StepKind::Model => {
            let (text, tone, tag) = match &step.why {
                Some(why) => match why.source {
                    WhySource::Content => (safe(&plain_words(&why.text)), Tone::Model, String::new()),
                    WhySource::Reasoning => (safe(&plain_words(&why.text)), Tone::Model, t("from its thinking").to_string()),
                    // (made from its first call: named as the calls under it are, not by the tool's name in the protocol)
                    WhySource::Target => {
                        let named = step.calls.first().map(|call| call_words(&call.name, call.target.as_deref().unwrap_or("")));
                        (safe(&named.unwrap_or_else(|| why.text.clone())), Tone::Derived, t("from the call").to_string())
                    }
                },
                None => (t("A step with no words and no calls").to_string(), Tone::Muted, String::new()),
            };
            out.push(Row { kind: Kind::Step, key: Some(key), open, tone, icon: "cpu", text, tag, right: time, indent: 1, current, ..Row::default() });
            if open {
                if !step.started.is_empty() {
                    out.push(fact(2, t("Started"), clock_of(&step.started)));
                }
                if let Some(request) = &step.request {
                    out.extend(request_facts(request, 2, server_fold(turn, place, context)));
                }
            }
            let ordered = in_calling_order(&step.calls);
            for call in ordered {
                call_rows(out, call, open, is_live, context);
            }
        }
        other => {
            let (icon, tone, text) = harness_words(other);
            out.push(Row { kind: Kind::Step, key: Some(key), open, tone, icon, text, tag: String::new(), right: time, indent: 1, ..Row::default() });
            if open {
                if !step.started.is_empty() {
                    out.push(fact(2, t("Started"), clock_of(&step.started)));
                }
                for (name, value) in &step.fields {
                    out.push(fact(2, &field_label(name), safe(&value_words(value))));
                }
                if let Some(request) = &step.request {
                    out.extend(request_facts(request, 2, server_fold(turn, place, context)));
                }
            }
        }
    }
}

/// The calls of a step with those a program made (their ids are the program's, a point and a number) under the call that ran it.
fn in_calling_order(calls: &[CallRow]) -> Vec<&CallRow> {
    let nested_in = |call: &CallRow, parent: &CallRow| !parent.id.is_empty() && call.id.starts_with(&format!("{}.", parent.id));
    let is_nested = |call: &CallRow| calls.iter().any(|parent| nested_in(call, parent));
    let mut ordered = Vec::with_capacity(calls.len());
    for call in calls.iter().filter(|call| !is_nested(call)) {
        ordered.push(call);
        ordered.extend(calls.iter().filter(|other| nested_in(other, call)));
    }
    // (a nested call has its parent in the list by definition, but not one that was put before it above)
    for call in calls {
        if !ordered.iter().any(|known| std::ptr::eq(*known, call)) {
            ordered.push(call);
        }
    }
    ordered
}

fn call_rows(out: &mut Vec<Row>, call: &CallRow, open: bool, is_live: bool, context: &Context) {
    let nested = call.id.contains('.');
    let running = is_live && context.live.is_running(&call.id);
    let failed_exit = call.exit_code.is_some_and(|code| code != 0);
    let (icon, tone, right) = match call.outcome.as_str() {
        _ if running => ("clock", Tone::App, t("running").to_string()),
        "" if is_live => ("clock", Tone::Muted, String::new()),
        "" => ("square", Tone::Muted, t("did not run").to_string()),
        "ok" if !failed_exit => ("check", Tone::Good, if call.ms > 0 { ms_words(call.ms) } else { String::new() }),
        other => ("x", Tone::Bad, tool_outcome_words(other, failed_exit)),
    };
    let indent = if nested { 3 } else { 2 };
    let name = t(tool_name(&call.name)).to_string();
    out.push(Row { kind: Kind::Call, tone, icon, label: name, text: call.target.as_deref().map(safe).unwrap_or_default(), right, right_tone: tone, indent, ..Row::default() });
    if !open {
        return;
    }
    // (what it opened for the model to see, as the model saw it)
    if !call.images.is_empty() {
        out.push(Row { kind: Kind::Pictures, indent: indent + 1, pictures: call.images.clone(), ..Row::default() });
    }
    let inner = indent + 1;
    if !call.outcome.is_empty() && call.outcome != "ok" {
        out.push(fact(inner, t("Result"), tool_outcome_words(&call.outcome, false)));
    }
    if let Some(code) = call.exit_code {
        out.push(fact(inner, t("Exit code"), code.to_string()));
    }
    if let Some(wait) = call.wait_ms.filter(|wait| *wait > 0) {
        out.push(fact(inner, t("Waited for you"), ms_words(wait)));
    }
    if call.ms > 0 && call.outcome == "ok" && !call.started.is_empty() {
        out.push(fact(inner, t("Started"), clock_of(&call.started)));
    }
    if let Some(file) = &call.helper_file {
        out.push(fact(inner, t("Its steps are kept in"), file.clone()));
    }
    // An explore's helper: each of its steps, why in its own words, and what it called.
    if !call.helper_steps.is_empty() {
        out.push(Row { kind: Kind::Caption, text: t("What the helper did").to_string(), indent: inner, ..Row::default() });
        for step in &call.helper_steps {
            if let Some(why) = &step.why {
                out.push(Row { kind: Kind::Call, tone: Tone::Model, icon: "paw-print", text: safe(&plain_words(&why.text)), indent: inner, ..Row::default() });
            }
            for (name, target) in &step.calls {
                out.push(Row { kind: Kind::Call, tone: Tone::Muted, icon: tool_icon(name), label: t(tool_name(name)).to_string(), text: target.as_deref().map(safe).unwrap_or_default(), indent: inner + 1, ..Row::default() });
            }
        }
    }
    if let Some(arguments) = call.arguments.as_deref().filter(|arguments| !arguments.trim().is_empty()) {
        out.push(Row { kind: Kind::Caption, text: t("Arguments").to_string(), indent: inner, ..Row::default() });
        out.extend(code_rows(argument_lines(&safe(arguments)), inner, Tone::Plain));
    }
    if let Some(output) = call.output.as_deref().filter(|output| !output.is_empty()) {
        out.push(Row { kind: Kind::Caption, text: t("Output").to_string(), indent: inner, ..Row::default() });
        out.extend(code_rows(plain_lines(&safe(output)), inner, if call.outcome == "ok" || call.outcome.is_empty() { Tone::Plain } else { Tone::Bad }));
    }
}

fn fact(indent: u8, label: &str, text: String) -> Row {
    Row { kind: Kind::Fact, label: label.to_string(), text, indent, ..Row::default() }
}

/// The lines of a call's arguments: JSON laid out, with a long text of several lines (a file's content) as the lines it has and not as one line with its breaks written out.
pub fn argument_lines(arguments: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(arguments) else { return plain_lines(arguments) };
    let Value::Object(fields) = &value else { return plain_lines(&serde_json::to_string_pretty(&value).unwrap_or_default()) };
    let mut lines = vec!["{".to_string()];
    for (name, field) in fields {
        match field {
            Value::String(text) if text.contains('\n') => {
                lines.push(format!("  \"{name}\":"));
                lines.extend(text.lines().map(|line| format!("    {line}")));
            }
            other => {
                let shown = serde_json::to_string_pretty(other).unwrap_or_default();
                let mut parts = shown.lines();
                lines.push(format!("  \"{name}\": {}", parts.next().unwrap_or("")));
                lines.extend(parts.map(|part| format!("  {part}")));
            }
        }
    }
    lines.push("}".to_string());
    lines.into_iter().map(|line| cap_line(&line)).collect()
}

/// The lines of an output: tabs as spaces, each kept to a length.
pub fn plain_lines(text: &str) -> Vec<String> {
    text.lines().map(cap_line).collect()
}

fn cap_line(line: &str) -> String {
    let line = line.trim_end().replace('\t', "    ");
    if line.chars().count() <= LINE_CHARS { line } else { format!("{}…", line.chars().take(LINE_CHARS).collect::<String>()) }
}

/// Lines as rows of code, the first and the last of a long run of them with a row that counts the rest between.
fn code_rows(lines: Vec<String>, indent: u8, tone: Tone) -> Vec<Row> {
    let row = |text: String, tone: Tone| Row { kind: Kind::Code, tone, text, indent, ..Row::default() };
    if lines.len() <= MOST_CODE_LINES {
        return lines.into_iter().map(|line| row(line, tone)).collect();
    }
    let left_out = lines.len() - CODE_HEAD - CODE_TAIL;
    let mut rows: Vec<Row> = lines[..CODE_HEAD].iter().map(|line| row(line.clone(), tone)).collect();
    rows.push(row(tf("… {0} more lines", &[&left_out]), Tone::Muted));
    rows.extend(lines[lines.len() - CODE_TAIL..].iter().map(|line| row(line.clone(), tone)));
    rows
}

/// The fold of the rest of what the server says of a step's request: its key, and whether it is open.
fn server_fold(turn: &TurnPath, place: usize, context: &Context) -> (Key, bool) {
    let key = Key::Server(turn.turn, place);
    (key, context.opened.get(&key).copied().unwrap_or(false))
}

/// What the server says of a request that is shown first, in this order: where the wait to the first word went. The rest is folded under them.
const SERVER_FIRST: [&str; 5] = ["ttft_seconds", "prefill_seconds", "resumed_from", "cached_tokens", "decode_tokens_per_second"];

/// A request's own numbers, as facts; of what the server says, the first numbers, and the rest when `fold` is open.
fn request_facts(request: &RequestDetail, indent: u8, fold: (Key, bool)) -> Vec<Row> {
    let mut facts = Vec::new();
    if let Some(first) = request.first_ms {
        facts.push(fact(indent, t("Time to the first word"), ms_words(first)));
    }
    facts.push(fact(indent, t("Total time"), ms_words(request.ms)));
    for (label, count) in [(t("Prompt"), request.prompt), (t("Cached"), request.cached), (t("Written"), request.completion)] {
        if let Some(count) = count {
            facts.push(fact(indent, label, tf("{0} tokens", &[&count])));
        }
    }
    if let Some(speed) = request.tps {
        facts.push(fact(indent, t("Speed"), tf("{0} tokens per second", &[&format!("{speed:.1}")])));
    }
    if !request.end.is_empty() {
        facts.push(fact(indent, t("Ended"), request_end_words(&request.end)));
    }
    if request.reasoning_chars > 0 {
        facts.push(fact(indent, t("Thought"), tf("{0} characters", &[&request.reasoning_chars])));
    }
    if request.content_chars > 0 {
        facts.push(fact(indent, t("Said"), tf("{0} characters", &[&request.content_chars])));
    }
    if !request.id.is_empty() {
        facts.push(fact(indent, t("Request"), request.id.clone()));
    }
    if !request.server.is_empty() {
        let (key, open) = fold;
        let first: Vec<(&String, &Value)> = SERVER_FIRST.iter().filter_map(|name| request.server.get_key_value(*name)).collect();
        let rest: Vec<(&String, &Value)> = request.server.iter().filter(|(name, _)| !SERVER_FIRST.contains(&name.as_str())).collect();
        let caption = if rest.is_empty() || open { t("What the server says").to_string() } else { tf("What the server says ({0} more)", &[&rest.len()]) };
        facts.push(Row { kind: Kind::Caption, key: (!rest.is_empty()).then_some(key), open: open && !rest.is_empty(), text: caption, indent, ..Row::default() });
        facts.extend(server_facts(&first, indent));
        if open {
            facts.extend(server_facts(&rest, indent));
        }
    }
    facts
}

fn server_facts(server: &[(&String, &Value)], indent: u8) -> Vec<Row> {
    server.iter().map(|(name, value)| {
        let text = match value.as_f64() {
            Some(seconds) if name.ends_with("_seconds") => tf("{0} s", &[&format!("{seconds:.2}")]),
            Some(rate) if name.ends_with("_per_second") => format!("{rate:.1}"),
            _ if name.as_str() == "resumed_from" => resumed_words(value.as_str().unwrap_or("")),
            _ => value_words(value),
        };
        fact(indent, &server_label(name), text)
    }).collect()
}

/// What the engine calls a number of its own, in words; a name this does not know is shown as it is, with its underscores gone.
fn server_label(name: &str) -> String {
    let words = match name {
        "prefill_seconds" => "Reading the prompt",
        "prefill_tokens" => "Tokens read",
        "prefill_tokens_per_second" => "Reading speed (tokens per second)",
        "prefill_drive_bytes" => "Bytes read from the drive while reading",
        "ttft_seconds" => "To the first token",
        "first_text_seconds" => "To the first words",
        "request_seconds" => "The request",
        "resume_seconds" => "Going on from what it kept",
        "checkpoint_seconds" => "Keeping where to go on from",
        "encode_seconds" => "Turning the prompt into tokens",
        "kv_growth_seconds" => "Growing the K/V",
        "park_seconds" => "Setting another conversation aside",
        "decode_seconds" => "Time writing",
        "decode_tokens_per_second" => "Writing speed (tokens per second)",
        "decode_passes" => "Passes while writing",
        "decode_forward_tokens" => "Tokens through those passes",
        "generated_tokens" => "Tokens written",
        "cached_tokens" => "Tokens it already had",
        "agreed_tokens" => "Tokens that matched what it had",
        "draft_tokens" => "Drafted tokens",
        "accepted_drafts" => "Drafted tokens accepted",
        "engine_draft_runs" => "The engine's own drafts",
        "engine_draft_tokens" => "Tokens the engine drafted",
        "accepted_engine_drafts" => "Of those, accepted",
        "resumed_from" => "Went on from",
        other => return other.replace('_', " "),
    };
    t(words).to_string()
}

/// Where the engine went on from (`resumed_from`), in words.
fn resumed_words(from: &str) -> String {
    match from {
        "live" => t("the conversation it had open"),
        "root" => t("the system prompt it kept"),
        "root_before" => t("the system prompt before the last"),
        "turn" => t("the start of this turn"),
        "prior" => t("the turn before"),
        "prefix" => t("a beginning it kept"),
        // (a point the engine kept in the request before: in its reply, or where its reply began)
        "step" => t("a point it kept in the last step"),
        "mark" => t("a point it kept partway through a reply"),
        "mark_before" => t("the point it kept before the last"),
        "none" => t("nothing: it read it all"),
        other => return other.to_string(),
    }
    .to_string()
}

fn field_label(name: &str) -> String {
    name.replace('_', " ")
}

fn value_words(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => match number.as_f64() {
            Some(float) if number.is_f64() => format!("{float:.2}"),
            _ => number.to_string(),
        },
        Value::Bool(true) => t("yes").to_string(),
        Value::Bool(false) => t("no").to_string(),
        Value::Null => String::new(),
        other => cap(&other.to_string(), 160),
    }
}

/// The icon, the colour and the words of a step of the app's.
fn harness_words(kind: &StepKind) -> (&'static str, Tone, String) {
    match kind {
        StepKind::Model => ("cpu", Tone::Model, String::new()),
        StepKind::Compaction { stage, ok: true } => ("settings", Tone::App, tf("Made the conversation shorter: {0}", &[&stage_words(stage)])),
        StepKind::Compaction { ok: false, .. } => ("settings", Tone::Bad, t("Could not make the conversation shorter").to_string()),
        StepKind::Intervention { kind } => ("info", Tone::App, intervention_words(kind)),
        StepKind::Retry { why, .. } => ("rotate-cw", Tone::App, tf("Asked the model again: {0}", &[&retry_words(why)])),
        StepKind::Approval { tool, outcome, .. } => ("lock", Tone::App, tf("Asked you about {0}: {1}", &[&t(tool_name(tool)), &decision_words(outcome)])),
        StepKind::Wake { cause } => ("clock", Tone::App, match cause.as_str() {
            "command_ended" => t("A background command ended").to_string(),
            "command_line" => t("A background command printed the line it was watched for").to_string(),
            "wait" => t("A wait is up").to_string(),
            other => other.replace('_', " "),
        }),
        StepKind::Failed { kind } => ("x", Tone::Bad, tf("The turn failed: {0}", &[&failure_words(kind)])),
    }
}

fn stage_words(stage: &str) -> String {
    match stage {
        "checkpoint" => t("summed up").to_string(),
        "checkpoint_from_thinking" => t("summed up from its thinking").to_string(),
        "checkpoint_trim_all" => t("summed up, old results dropped").to_string(),
        "trim" => t("old results dropped").to_string(),
        "truncate" => t("the oldest messages dropped").to_string(),
        "" => t("not finished").to_string(),
        other => other.replace('_', " "),
    }
}

fn intervention_words(kind: &str) -> String {
    match kind {
        "draft_stop" => t("Stopped a draft that went on too long").to_string(),
        "continue_nudge" => t("Asked the model to go on").to_string(),
        "calls_broken_off" => t("Calls were cut off by a broken connection").to_string(),
        "cost_note" => t("Told the model how much it has written").to_string(),
        "check_nudge" => t("Asked the model to check its work").to_string(),
        "slip_note" => t("Pointed out letters Japanese is not written with").to_string(),
        "no_change_guard" => t("Kept the turn from changing anything").to_string(),
        "server_restart_wait" => t("Waited for the server to restart").to_string(),
        "cut_write" => t("Kept a file write that was cut off, to go on from").to_string(),
        "helper_stopped" => t("Stopped a helper").to_string(),
        "helper_enough" => t("Told a helper it had looked enough").to_string(),
        "repeat_note" => t("Noted a call repeated").to_string(),
        other => other.replace('_', " "),
    }
}

fn decision_words(decision: &str) -> String {
    match decision {
        "approve" => t("approved").to_string(),
        "always" => t("approved for good").to_string(),
        "reject" => t("declined").to_string(),
        "stopped" => t("stopped").to_string(),
        "closed" => t("the window closed").to_string(),
        other => other.replace('_', " "),
    }
}

fn failure_words(kind: &str) -> String {
    match kind {
        "failed_calls" => t("calls failed in a row").to_string(),
        "repeats" => t("the same call was made again and again").to_string(),
        "cut_off" => t("the answer was cut off").to_string(),
        "iterations" => t("too many steps").to_string(),
        "unfinished" => t("the answer did not finish").to_string(),
        "context_full" => t("the conversation is too long").to_string(),
        "server_gone" => t("the server went away").to_string(),
        "refused" => t("the server refused the request").to_string(),
        "busy" => t("the server was busy").to_string(),
        "not_ready" => t("the server was not ready").to_string(),
        "no_vision" => t("the model cannot read pictures").to_string(),
        "no_answer" => t("the model gave no answer").to_string(),
        "server_error" => t("the server answered with an error").to_string(),
        other => other.replace('_', " "),
    }
}

/// Which rows a key can be moved to and acted on from: the rows with a key, in order.
pub fn keyed(rows: &[Row]) -> Vec<usize> {
    rows.iter().enumerate().filter(|(_, row)| row.key.is_some()).map(|(at, _)| at).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dogmind_agent::timeline::{HelperStep, TodoItem, TurnEnd, Why};
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn later(from: Instant, seconds: u64) -> Instant {
        from + Duration::from_secs(seconds)
    }

    fn model(why: Option<(&str, WhySource)>, calls: Vec<CallRow>) -> Step {
        Step { kind: StepKind::Model, ms: 2100, why: why.map(|(text, source)| Why { text: text.into(), source }), calls, started: "2026-11-05T10:00:01.250+09:00".into(), ..Step::default() }
    }

    fn call(id: &str, name: &str, target: &str, outcome: &str) -> CallRow {
        CallRow { id: id.into(), name: name.into(), target: (!target.is_empty()).then(|| target.to_string()), ms: 30, outcome: outcome.into(), ..CallRow::default() }
    }

    fn turn(number: u64, asked: &str, steps: Vec<Step>, ended: Option<TurnEnd>) -> TurnPath {
        TurnPath { turn: number, asked: asked.into(), steps, ended, ..TurnPath::default() }
    }

    fn done(ms: u64) -> Option<TurnEnd> {
        Some(TurnEnd { ms, cancelled: false, failed: false })
    }

    fn running() -> Inputs {
        Inputs { running: true, ..Inputs::default() }
    }

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|row| if row.label.is_empty() { row.text.clone() } else { format!("{}: {}", row.label, row.text) }).collect()
    }

    fn noted(live: &mut Live, events: &[Event], at: Instant) {
        for event in events {
            live.note(event, at);
        }
    }

    fn ended(id: &str) -> Event {
        Event::ToolEnded { id: id.into(), ok: true, declined: false, elapsed: Duration::from_millis(30), output: String::new(), images: Vec::new() }
    }

    fn started(id: &str, tool: &str, summary: &str) -> Event {
        Event::ToolStarted { id: id.into(), tool: tool.into(), summary: summary.into() }
    }

    #[test]
    fn with_no_turn_at_work_it_is_idle_or_waits_for_what_runs_in_the_background() {
        let at = Instant::now();
        let live = Live::new(at);
        assert_eq!(live.now(&Inputs::default(), at), Now { who: Who::Idle, text: "Idle".into(), seconds: None });
        let waiting = live.now(&Inputs { background: 2, ..Inputs::default() }, at);
        assert_eq!((waiting.who, waiting.text.as_str(), waiting.seconds), (Who::App, "Waiting for background work", None));
    }

    #[test]
    fn the_model_is_said_to_read_think_write_a_call_and_write_the_answer_as_the_events_say_it() {
        let at = Instant::now();
        let mut live = Live::new(at);
        let now = |live: &Live, seconds: u64| live.now(&running(), later(at, seconds));
        noted(&mut live, &[Event::TurnStarted], at);
        assert_eq!((now(&live, 0).who, now(&live, 0).text), (Who::App, "Starting the turn".to_string()));
        noted(&mut live, &[Event::RequestStarted], at);
        assert_eq!((now(&live, 0).who, now(&live, 0).text), (Who::Model, "Waiting for the model's first words".to_string()));
        noted(&mut live, &[Event::Reading { done: 12_400, total: 31_000 }], later(at, 1));
        assert_eq!(now(&live, 2).text, "Reading the conversation: 12.4k of 31.0k tokens (40%)");
        assert_eq!(now(&live, 2).seconds, Some(1), "counted from when the reading began");
        noted(&mut live, &[Event::Reading { done: 20_000, total: 31_000 }], later(at, 3));
        assert_eq!(now(&live, 4).seconds, Some(3), "more of the same reading does not begin it again");
        noted(&mut live, &[Event::Reasoning("Let me look at the file.".into())], later(at, 5));
        assert_eq!((now(&live, 6).who, now(&live, 6).text, now(&live, 6).seconds), (Who::Model, "Thinking…".to_string(), Some(1)));
        noted(&mut live, &[Event::ToolWriting { index: 0, tool: "create_file".into(), piece: "{\"path\": \"src/lib.rs\", \"content\": \"".into() }], later(at, 7));
        noted(&mut live, &[Event::ToolWriting { index: 0, tool: "create_file".into(), piece: "x".repeat(4096) }], later(at, 8));
        assert_eq!(now(&live, 9).text, "Writing the arguments of Write  src/lib.rs: 4 KB");
        noted(&mut live, &[Event::RequestStarted, Event::Content("Done.".into())], later(at, 10));
        assert_eq!((now(&live, 11).who, now(&live, 11).text), (Who::Model, "Writing the answer".to_string()));
    }

    #[test]
    fn the_app_is_said_to_check_run_and_hand_back_what_a_call_brought() {
        let at = Instant::now();
        let mut live = Live::new(at);
        noted(&mut live, &[Event::TurnStarted, Event::RequestStarted, Event::AssistantEnded], at);
        assert_eq!(live.now(&running(), at).text, "Checking the calls");
        noted(&mut live, &[started("c1", "read_file", "src/lib.rs")], later(at, 1));
        let now = live.now(&running(), later(at, 5));
        assert_eq!((now.who, now.text.as_str(), now.seconds), (Who::App, "Running Read  src/lib.rs", Some(4)));
        noted(&mut live, &[started("c2", "search_files", "foo")], later(at, 3));
        let now = live.now(&running(), later(at, 6));
        assert_eq!(now.text, "Running Read  src/lib.rs  ·  and 1 more");
        assert_eq!(now.seconds, Some(5), "counted from the first of them");
        noted(&mut live, &[ended("c1")], later(at, 6));
        assert!(live.now(&running(), later(at, 7)).text.starts_with("Running Search"));
        noted(&mut live, &[ended("c2")], later(at, 7));
        assert_eq!(live.now(&running(), later(at, 7)).text, "Sending the results to the model");
        noted(&mut live, &[Event::Compacting], later(at, 8));
        assert_eq!(live.now(&running(), later(at, 8)).text, "Compacting the session…");
        noted(&mut live, &[Event::Retrying { why: "overflow".into(), wait_ms: 1500 }], later(at, 9));
        assert_eq!(live.now(&running(), later(at, 9)).text, "Retrying the request: the conversation was too long");
        noted(&mut live, &[Event::ServerRestarting(Value::Null)], later(at, 10));
        assert_eq!(live.now(&running(), later(at, 10)).text, "Waiting for the server to restart…");
        noted(&mut live, &[Event::TurnEnded { cancelled: false }], later(at, 11));
        assert_eq!(live.now(&Inputs::default(), later(at, 11)).who, Who::Idle);
    }

    #[test]
    fn a_question_to_the_person_is_theirs_to_answer_and_counted_from_when_it_was_asked() {
        let at = Instant::now();
        let mut live = Live::new(at);
        noted(&mut live, &[Event::TurnStarted, Event::RequestStarted], at);
        let (sender, _answers) = std::sync::mpsc::channel();
        live.note(&Event::Question(dogmind_agent::agent::Question { question: "Which one?".into(), options: Vec::new(), answer: sender }), later(at, 2));
        let asking = |wait| Inputs { running: true, wait: Some(wait), background: 0, ..Inputs::default() };
        let now = live.now(&asking(PersonWait::Question), later(at, 9));
        assert_eq!((now.who, now.text.as_str(), now.seconds), (Who::Person, "Waiting for your answer", Some(7)));
        assert_eq!(live.now(&asking(PersonWait::Plan), later(at, 9)).text, "Waiting for you to review a plan");
        let approval = asking(PersonWait::Approval { tool: "execute_command".into(), summary: "cargo test".into() });
        assert_eq!(live.now(&approval, later(at, 9)).text, "Waiting for your approval: Run  cargo test");
        assert_eq!(live.now(&running(), later(at, 9)).who, Who::Model, "answered: it is the model's again");
    }

    #[test]
    fn the_hand_out_of_a_session_back_in_sight_with_nothing_said_is_working() {
        let at = Instant::now();
        let live = Live::new(at);
        assert_eq!(live.now(&running(), at).text, "Working");
    }

    #[test]
    fn where_the_work_is_heading_is_the_plan_first_then_the_tasks_then_the_latest_thought_and_otherwise_nothing() {
        let at = Instant::now();
        let mut live = Live::new(at);
        noted(&mut live, &[Event::TurnStarted, Event::RequestStarted, Event::Reasoning("I should read the library first. Then the tests".into())], at);
        let plan = Timeline { heading: Some(Heading::Plan { items: vec!["Read the code".into(), "Change it".into()] }), ..Timeline::default() };
        let todos = Timeline { heading: Some(Heading::Todos { items: vec![TodoItem { text: "Read".into(), status: "done".into() }, TodoItem { text: "Write".into(), status: "doing".into() }] }), ..Timeline::default() };
        let neither = Timeline::default();
        let view = heading(Some(&plan), &live, &running()).unwrap();
        assert_eq!((view.source, view.items.iter().map(|item| item.text.as_str()).collect::<Vec<_>>()), (Source::Plan, vec!["Read the code", "Change it"]));
        let view = heading(Some(&todos), &live, &running()).unwrap();
        assert_eq!(view.source, Source::Todos);
        assert_eq!(view.items.iter().map(|item| item.progress).collect::<Vec<_>>(), [Progress::Done, Progress::Doing]);
        // (the thought is the model's sentence as it wrote it, and only while a turn runs)
        let view = heading(Some(&neither), &live, &running()).unwrap();
        assert_eq!((view.source, view.caption.as_str(), view.items[0].text.as_str()), (Source::Thought, "The model's latest thought", "Then the tests"));
        assert_eq!(heading(None, &live, &running()).unwrap().source, Source::Thought, "even before the files are read");
        assert_eq!(heading(Some(&neither), &live, &Inputs::default()), None, "idle: nothing is made up");
        assert_eq!(heading(Some(&plan), &live, &Inputs::default()).unwrap().source, Source::Plan, "the plan stays when the work is done");
        // (no reasoning, no thought)
        let quiet = Live::new(at);
        assert_eq!(heading(Some(&neither), &quiet, &running()), None);
    }

    #[test]
    fn a_thought_belongs_to_the_request_under_way_and_goes_with_it() {
        let at = Instant::now();
        let mut live = Live::new(at);
        noted(&mut live, &[Event::TurnStarted, Event::RequestStarted, Event::Reasoning("First request thought.".into()), Event::AssistantEnded], at);
        assert_eq!(live.thought().as_deref(), Some("First request thought."), "kept while its calls run");
        noted(&mut live, &[Event::RequestStarted], at);
        assert_eq!(live.thought(), None, "the next request begins with none");
        noted(&mut live, &[Event::Reasoning("A key sk-abcdefghijklmnopqrstuvwxyz0123456789 is here.".into())], at);
        let shown = live.thought().unwrap();
        assert!(!shown.contains("abcdefghijklmnopqrstuvwxyz0123456789"), "{shown}");
    }

    #[test]
    fn a_long_list_shows_the_items_the_work_is_at_and_counts_the_rest() {
        let items: Vec<TodoItem> = (1..=10).map(|n| TodoItem { text: format!("Task {n}"), status: if n <= 4 { "done" } else if n == 5 { "doing" } else { "pending" }.into() }).collect();
        let timeline = Timeline { heading: Some(Heading::Todos { items }), ..Timeline::default() };
        let view = heading(Some(&timeline), &Live::new(Instant::now()), &Inputs::default()).unwrap();
        assert_eq!((view.before, view.after, view.items.len()), (3, 1, MOST_ITEMS));
        assert_eq!(view.items[1].text, "Task 5", "the one before it stands above the one in hand");
        let all_done: Vec<TodoItem> = (1..=10).map(|n| TodoItem { text: format!("Task {n}"), status: "done".into() }).collect();
        let timeline = Timeline { heading: Some(Heading::Todos { items: all_done }), ..Timeline::default() };
        let view = heading(Some(&timeline), &Live::new(Instant::now()), &Inputs::default()).unwrap();
        assert_eq!((view.before, view.after, view.items.last().map(|item| item.text.as_str())), (4, 0, Some("Task 10")));
    }

    #[test]
    fn the_latest_turn_is_open_the_earlier_ones_are_folded_to_a_line_and_a_click_wins_over_both() {
        let timeline = Timeline {
            turns: vec![
                turn(1, "Fix the parser", vec![model(Some(("I will read it.", WhySource::Content)), vec![call("c1", "read_file", "src/lib.rs", "ok")])], done(12_000)),
                turn(2, "Now the tests", vec![model(Some(("Looking.", WhySource::Content)), Vec::new())], done(3_400)),
            ],
            heading: None,
        };
        let live = Live::new(Instant::now());
        let opened = HashMap::new();
        let rows = rows(Some(&timeline), &live, &Inputs::default(), &opened, Instant::now());
        assert_eq!(texts(&rows), ["Fix the parser", "This turn's request: Now the tests", "Looking."]);
        assert_eq!((rows[0].open, rows[0].right.as_str(), rows[0].tone), (false, "Done  ·  12 s", Tone::Person));
        assert_eq!((rows[1].open, rows[1].tone, rows[1].key), (true, Tone::Person, Some(Key::Turn(2))));
        // (a click on the folded turn opens it, and one on the latest folds it)
        let opened = HashMap::from([(Key::Turn(1), true), (Key::Turn(2), false)]);
        let rows = rows_for(&timeline, &live, &opened);
        assert_eq!(texts(&rows), ["This turn's request: Fix the parser", "I will read it.", "Read: src/lib.rs", "Now the tests"]);
    }

    fn rows_for(timeline: &Timeline, live: &Live, opened: &HashMap<Key, bool>) -> Vec<Row> {
        rows(Some(timeline), live, &Inputs::default(), opened, Instant::now())
    }

    #[test]
    fn a_turn_nobody_sent_says_what_woke_it_and_a_turn_whose_request_is_gone_says_so() {
        let woken = TurnPath { turn: 3, woken: true, steps: vec![Step { kind: StepKind::Wake { cause: "command_ended".into() }, ..Step::default() }], ended: done(900), ..TurnPath::default() };
        let gone = turn(2, "", Vec::new(), done(900));
        let timeline = Timeline { turns: vec![gone.clone(), woken.clone()], heading: None };
        let rows = rows_for(&timeline, &Live::new(Instant::now()), &HashMap::new());
        assert_eq!(texts(&rows), ["Its request is no longer in the conversation", "Woken by background work: ", "A background command ended"]);
        assert_eq!(rows[1].tone, Tone::App, "it is the app's, not the person's");
        let folded = rows_for(&Timeline { turns: vec![woken, gone], heading: None }, &Live::new(Instant::now()), &HashMap::new());
        assert_eq!(folded[0].text, "Woken by background work");
    }

    #[test]
    fn a_step_says_why_in_the_models_words_and_marks_what_is_made_from_its_call() {
        let said = model(Some(("I will read it.", WhySource::Content)), Vec::new());
        let thought = model(Some(("Next the tests", WhySource::Reasoning)), Vec::new());
        let derived = model(Some(("read_file src/lib.rs", WhySource::Target)), vec![call("c1", "read_file", "src/lib.rs", "ok")]);
        let timeline = Timeline { turns: vec![turn(1, "Go", vec![said, thought, derived], done(100))], heading: None };
        let rows = rows_for(&timeline, &Live::new(Instant::now()), &HashMap::new());
        let steps: Vec<&Row> = rows.iter().filter(|row| row.kind == Kind::Step).collect();
        assert_eq!(steps.iter().map(|row| (row.tone, row.tag.as_str())).collect::<Vec<_>>(), [(Tone::Model, ""), (Tone::Model, "from its thinking"), (Tone::Derived, "from the call")]);
        // (what is made from the call names it as the call's own row does, not by the tool's name in the protocol)
        assert_eq!(steps[2].text, call_words("read_file", "src/lib.rs"));
        assert_ne!(steps[2].text, "read_file src/lib.rs");
        assert_eq!(steps[0].right, "2.1 s");
    }

    #[test]
    fn a_calls_row_says_what_it_was_on_how_it_came_out_and_how_long_it_took() {
        let step = model(None, vec![call("a", "read_file", "src/lib.rs", "ok"), call("b", "execute_command", "cargo", "error"), call("c", "web_read", "", "declined_person"), call("d", "search_files", "foo", "")]);
        let timeline = Timeline { turns: vec![turn(1, "Go", vec![step], done(100))], heading: None };
        let rows = rows_for(&timeline, &Live::new(Instant::now()), &HashMap::new());
        let calls: Vec<&Row> = rows.iter().filter(|row| row.kind == Kind::Call).collect();
        assert_eq!(calls.iter().map(|row| (row.icon, row.tone, row.label.as_str(), row.text.as_str(), row.right.as_str())).collect::<Vec<_>>(), [
            ("check", Tone::Good, "Read", "src/lib.rs", "30 ms"),
            ("x", Tone::Bad, "Run", "cargo", "Failed"),
            ("x", Tone::Bad, "Read the web", "", "You declined"),
            ("square", Tone::Muted, "Search", "foo", "did not run"),
        ]);
    }

    /// The pictures the person sent are under their request; those a call opened are under the call, when its step is open.
    #[test]
    fn pictures_are_under_the_request_that_sent_them_and_the_call_that_opened_them() {
        let step = model(Some(("Looking.", WhySource::Content)), vec![CallRow { images: vec!["2-shot.png".into()], ..call("a", "read_file", "shot.png", "ok") }]);
        let path = TurnPath { asked_images: vec!["1-a.png".into()], ..turn(1, "What is this?", vec![step], done(100)) };
        let timeline = Timeline { turns: vec![path], heading: None };
        let pictures = |opened: &HashMap<Key, bool>| -> Vec<(Vec<String>, u8)> {
            rows_for(&timeline, &Live::new(Instant::now()), opened).into_iter().filter(|row| row.kind == Kind::Pictures).map(|row| (row.pictures, row.indent)).collect()
        };
        assert_eq!(pictures(&HashMap::new()), [(vec!["1-a.png".to_string()], 1)], "the step shut: only the request's");
        let open = HashMap::from([(Key::Step(1, 0), true)]);
        assert_eq!(pictures(&open), [(vec!["1-a.png".to_string()], 1), (vec!["2-shot.png".to_string()], 3)]);
    }

    #[test]
    fn a_step_opened_shows_the_numbers_of_its_request_and_for_each_call_its_arguments_and_output() {
        let mut step = model(Some(("Reading.", WhySource::Content)), vec![CallRow { arguments: Some(r#"{"path": "src/lib.rs"}"#.into()), output: Some("1|fn main() {}\n2|".into()), ..call("a", "read_file", "src/lib.rs", "ok") }]);
        step.request = Some(RequestDetail {
            id: "s/3".into(),
            first_ms: Some(120),
            ms: 2100,
            prompt: Some(5000),
            cached: Some(4000),
            completion: Some(50),
            tps: Some(40.5),
            end: "tool_calls".into(),
            reasoning_chars: 10,
            server: BTreeMap::from([("prefill_seconds".to_string(), Value::from(0.123)), ("resumed_from".to_string(), Value::from("live"))]),
            ..RequestDetail::default()
        });
        let timeline = Timeline { turns: vec![turn(1, "Go", vec![step], done(100))], heading: None };
        let closed = rows_for(&timeline, &Live::new(Instant::now()), &HashMap::new());
        assert_eq!(closed.iter().filter(|row| matches!(row.kind, Kind::Fact | Kind::Caption | Kind::Code)).count(), 0, "closed: only the step and its calls");
        let opened = HashMap::from([(Key::Step(1, 0), true)]);
        let rows = rows_for(&timeline, &Live::new(Instant::now()), &opened);
        let facts = texts(&rows);
        for wanted in ["Started: 10:00:01", "Time to the first word: 120 ms", "Total time: 2.1 s", "Prompt: 5000 tokens", "Cached: 4000 tokens", "Written: 50 tokens", "Speed: 40.5 tokens per second", "Ended: It asked for calls", "Request: s/3", "What the server says",
            "Reading the prompt: 0.12 s", "Went on from: the conversation it had open", "Arguments", "Output", "1|fn main() {}"] {
            assert!(facts.iter().any(|fact| fact == wanted), "{wanted} in {facts:#?}");
        }
        let code: Vec<&str> = rows.iter().filter(|row| row.kind == Kind::Code).map(|row| row.text.as_str()).collect();
        assert_eq!(code, ["{", "  \"path\": \"src/lib.rs\"", "}", "1|fn main() {}", "2|"]);
        assert!(rows.iter().find(|row| row.kind == Kind::Step).unwrap().open);
    }

    #[test]
    fn a_program_s_calls_stand_under_the_call_that_ran_it() {
        let step = model(None, vec![call("p", "run_code", "", "ok"), call("q", "read_file", "a.rs", "ok"), call("p.1", "read_file", "b.rs", "ok"), call("p.2", "search_files", "x", "ok")]);
        let timeline = Timeline { turns: vec![turn(1, "Go", vec![step], done(100))], heading: None };
        let rows = rows_for(&timeline, &Live::new(Instant::now()), &HashMap::new());
        let calls: Vec<(String, u8)> = rows.iter().filter(|row| row.kind == Kind::Call).map(|row| (row.text.clone(), row.indent)).collect();
        assert_eq!(calls, [(String::new(), 2), ("b.rs".to_string(), 3), ("x".to_string(), 3), ("a.rs".to_string(), 2)]);
    }

    #[test]
    fn a_step_of_the_apps_has_its_own_icon_and_words_and_its_fields_when_opened() {
        let compaction = Step { kind: StepKind::Compaction { stage: "checkpoint".into(), ok: true }, ms: 14_000, fields: serde_json::Map::from_iter([("before_tokens".to_string(), Value::from(90_000)), ("forced".to_string(), Value::from(false))]), ..Step::default() };
        let retry = Step { kind: StepKind::Retry { why: "transient".into(), wait_ms: 1500 }, ms: 1500, ..Step::default() };
        let approval = Step { kind: StepKind::Approval { tool: "execute_command".into(), outcome: "approve".into(), wait_ms: 4000 }, ms: 4000, ..Step::default() };
        let guard = Step { kind: StepKind::Intervention { kind: "draft_stop".into() }, ..Step::default() };
        let failed = Step { kind: StepKind::Failed { kind: "busy".into() }, ..Step::default() };
        let timeline = Timeline { turns: vec![turn(1, "Go", vec![compaction, retry, approval, guard, failed], None)], heading: None };
        let opened = HashMap::from([(Key::Step(1, 0), true)]);
        let rows = rows_for(&timeline, &Live::new(Instant::now()), &opened);
        let steps: Vec<(&str, Tone, &str, &str)> = rows.iter().filter(|row| row.kind == Kind::Step).map(|row| (row.icon, row.tone, row.text.as_str(), row.right.as_str())).collect();
        assert_eq!(steps, [
            ("settings", Tone::App, "Made the conversation shorter: summed up", "14 s"),
            ("rotate-cw", Tone::App, "Asked the model again: the connection broke", "1.5 s"),
            ("lock", Tone::App, "Asked you about Run: approved", "4.0 s"),
            ("info", Tone::App, "Stopped a draft that went on too long", ""),
            ("x", Tone::Bad, "The turn failed: the server was busy", ""),
        ]);
        let facts = texts(&rows);
        assert!(facts.contains(&"before tokens: 90000".to_string()) && facts.contains(&"forced: no".to_string()), "{facts:#?}");
        assert_eq!(rows[0].right, "Interrupted", "a turn with no end, in a session nothing runs in, was cut short");
    }

    #[test]
    fn a_turn_that_runs_ends_in_the_step_in_progress_and_the_model_working_on_it_is_that_row() {
        let at = Instant::now();
        let mut live = Live::new(at);
        let step = model(Some(("Reading it.", WhySource::Content)), vec![call("c1", "read_file", "src/lib.rs", "ok")]);
        let timeline = Timeline { turns: vec![turn(1, "Fix it", vec![step], None)], heading: None };
        noted(&mut live, &[Event::TurnStarted, Event::RequestStarted, Event::Reasoning("Now the test.".into())], at);
        live.timeline_came(0);
        let rows = rows(Some(&timeline), &live, &running(), &HashMap::new(), later(at, 4));
        let last = rows.last().unwrap();
        assert_eq!((last.kind, last.tone, last.label.as_str(), last.text.as_str(), last.current), (Kind::Live, Tone::Model, "Model", "Thinking…", true));
        assert_eq!(rows[0].right, "Running");
        assert_eq!(rows.iter().filter(|row| row.current).count(), 1);
    }

    #[test]
    fn calls_under_way_light_the_step_that_made_them_and_the_turn_the_files_lack_is_made_from_what_the_window_knows() {
        let at = Instant::now();
        let mut live = Live::new(at);
        // (the files have one turn, ended; the window has begun another)
        live.timeline_came(1);
        live.will_ask("Run the tests\nand tell me");
        noted(&mut live, &[Event::TurnStarted], at);
        let old = Timeline { turns: vec![turn(1, "Before", Vec::new(), done(100))], heading: None };
        let rows_now = rows(Some(&old), &live, &running(), &HashMap::new(), at);
        assert_eq!(texts(&rows_now), ["Before", "This turn's request: Run the tests", "Dogmind: Starting the turn"]);
        assert_eq!((rows_now[1].tone, rows_now[2].tone, rows_now[2].current), (Tone::Person, Tone::App, true));
        // (the files catch up: the turn is theirs, with the step whose call is under way)
        let step = model(Some(("Running them.", WhySource::Content)), vec![call("c9", "execute_command", "cargo", "")]);
        let caught_up = Timeline { turns: vec![turn(1, "Before", Vec::new(), done(100)), turn(2, "Run the tests", vec![step], None)], heading: None };
        noted(&mut live, &[started("c9", "execute_command", "cargo test")], later(at, 1));
        let rows_later = rows(Some(&caught_up), &live, &running(), &HashMap::new(), later(at, 2));
        let current: Vec<&Row> = rows_later.iter().filter(|row| row.current).collect();
        assert_eq!(current.len(), 1);
        assert_eq!((current[0].kind, current[0].text.as_str()), (Kind::Step, "Running them."));
        assert!(rows_later.iter().all(|row| row.kind != Kind::Live), "the call itself is in the step");
        let call_row = rows_later.iter().find(|row| row.kind == Kind::Call).unwrap();
        assert_eq!((call_row.icon, call_row.right.as_str()), ("clock", "running"));
    }

    #[test]
    fn a_turn_woken_by_background_work_is_not_given_the_words_of_a_request() {
        let at = Instant::now();
        let mut live = Live::new(at);
        live.will_ask("left over from nothing");
        noted(&mut live, &[Event::Woken(Vec::new()), Event::TurnStarted], at);
        let rows = rows(None, &live, &running(), &HashMap::new(), at);
        assert_eq!(texts(&rows)[0], "Woken by background work: ");
        assert_eq!(rows[0].tone, Tone::App);
    }

    #[test]
    fn waiting_for_the_person_is_the_row_in_progress_and_theirs() {
        let at = Instant::now();
        let mut live = Live::new(at);
        noted(&mut live, &[Event::TurnStarted, Event::RequestStarted, Event::AssistantEnded], at);
        let waiting = Inputs { running: true, wait: Some(PersonWait::Approval { tool: "create_file".into(), summary: "a.rs".into() }), background: 0, ..Inputs::default() };
        let rows = rows(None, &live, &waiting, &HashMap::new(), at);
        let last = rows.last().unwrap();
        assert_eq!((last.kind, last.tone, last.label.as_str(), last.text.as_str()), (Kind::Live, Tone::Person, "You", "Waiting for your approval: Write  a.rs"));
    }

    #[test]
    fn a_session_with_no_turn_says_so_and_what_is_shown_is_shown_as_the_conversation_shows_it() {
        let rows_none = rows(None, &Live::new(Instant::now()), &Inputs::default(), &HashMap::new(), Instant::now());
        assert_eq!((rows_none.len(), rows_none[0].kind), (1, Kind::Note));
        let step = model(Some(("Reading \u{202E}dlrow", WhySource::Content)), vec![CallRow { output: Some("token sk-abcdefghijklmnopqrstuvwxyz0123456789 x".into()), ..call("a", "read_file", "a.rs", "ok") }]);
        let timeline = Timeline { turns: vec![turn(1, "Go \u{202E}evil", vec![step], done(1))], heading: None };
        let rows = rows_for(&timeline, &Live::new(Instant::now()), &HashMap::from([(Key::Step(1, 0), true)]));
        let words = texts(&rows).join("\n");
        assert!(!words.contains('\u{202E}'), "{words:?}");
        assert!(!words.contains("abcdefghijklmnopqrstuvwxyz0123456789"), "{words}");
    }

    #[test]
    fn arguments_are_laid_out_with_a_files_content_as_the_lines_it_has() {
        let lines = argument_lines(r#"{"path": "a.rs", "content": "fn a() {}\nfn b() {}\n", "options": {"overwrite": true}}"#);
        assert_eq!(lines, ["{", "  \"path\": \"a.rs\"", "  \"content\":", "    fn a() {}", "    fn b() {}", "  \"options\": {", "    \"overwrite\": true", "  }", "}"]);
        // (cut by the reader: not JSON any more, and shown as it is)
        assert_eq!(argument_lines("{\"path\": \"a\"\n... 4000 bytes left out ...\n}"), ["{\"path\": \"a\"", "... 4000 bytes left out ...", "}"]);
    }

    #[test]
    fn a_long_output_shows_its_first_and_last_lines_and_counts_the_rest() {
        let step = model(None, vec![CallRow { output: Some((1..=100).map(|n| format!("line {n}\n")).collect()), ..call("a", "execute_command", "cargo", "ok") }]);
        let timeline = Timeline { turns: vec![turn(1, "Go", vec![step], done(1))], heading: None };
        let rows = rows_for(&timeline, &Live::new(Instant::now()), &HashMap::from([(Key::Step(1, 0), true)]));
        let code: Vec<&str> = rows.iter().filter(|row| row.kind == Kind::Code).map(|row| row.text.as_str()).collect();
        assert_eq!(code.len(), CODE_HEAD + 1 + CODE_TAIL);
        assert_eq!((code[0], code[CODE_HEAD], code[code.len() - 1]), ("line 1", "… 56 more lines", "line 100"));
    }

    #[test]
    fn the_rows_that_a_key_can_move_to_are_the_turns_and_the_steps() {
        let step = model(Some(("Reading.", WhySource::Content)), vec![call("a", "read_file", "a.rs", "ok")]);
        let timeline = Timeline { turns: vec![turn(1, "Go", vec![step], done(1))], heading: None };
        let rows = rows_for(&timeline, &Live::new(Instant::now()), &HashMap::new());
        assert_eq!(keyed(&rows), [0, 1]);
        assert_eq!(rows[1].key, Some(Key::Step(1, 0)));
    }

    #[test]
    fn the_version_changes_with_what_the_rows_are_made_of_and_not_with_each_piece_of_reasoning() {
        let at = Instant::now();
        let mut live = Live::new(at);
        live.note(&Event::TurnStarted, at);
        live.note(&Event::RequestStarted, at);
        live.note(&Event::Reasoning("a".into()), at);
        let version = live.version();
        for _ in 0..5 {
            live.note(&Event::Reasoning("more".into()), at);
        }
        assert_eq!(live.version(), version);
        assert!(!live.note(&Event::Usage { used: 1, window: 2, usage: Default::default() }, at), "an event that changes nothing shown says so");
        assert!(live.note(&Event::Reasoning("again".into()), at), "and one that changes the thought says that");
        live.note(&started("c", "read_file", "a"), at);
        assert_ne!(live.version(), version);
    }

    #[test]
    fn times_are_in_the_unit_that_reads_best() {
        assert_eq!(ms_words(30), "30 ms");
        assert_eq!(ms_words(2100), "2.1 s");
        assert_eq!(ms_words(65_000), "1 min 5 s");
    }

    /// An open turn has its band under its request, a folded one none; for the turn at work the part going on now runs from where its lane's parts
    /// end to the time now; and the key names what the band has, the app's marks too.
    #[test]
    fn an_open_turn_has_its_band_and_the_turn_at_work_has_the_part_going_on_reach_now() {
        let request = RequestDetail { started: "2026-11-05T10:00:01.000+09:00".into(), first_ms: Some(500), ms: 2000, server: [("ttft_seconds".to_string(), serde_json::json!(0.4)), ("prefill_seconds".to_string(), serde_json::json!(0.3))].into_iter().collect(), ..RequestDetail::default() };
        let mut step = model(Some(("Reading.", WhySource::Content)), Vec::new());
        step.request = Some(request);
        let retry = Step { kind: StepKind::Retry { why: "empty".into(), wait_ms: 0 }, started: "2026-11-05T10:00:04.000+09:00".into(), ..Step::default() };
        let started = |turn: TurnPath| TurnPath { started: "2026-11-05T10:00:00.000+09:00".into(), ..turn };
        let earlier = started(turn(1, "First", vec![step.clone()], done(3000)));
        let latest = started(turn(2, "Go", vec![step, retry], None));
        let timeline = Timeline { turns: vec![earlier, latest], heading: None };
        let finished = rows_for(&timeline, &Live::new(Instant::now()), &HashMap::new());
        let kinds: Vec<Kind> = finished.iter().map(|row| row.kind).collect();
        assert_eq!(kinds[..3], [Kind::Turn, Kind::Turn, Kind::Band], "the folded turn has none, the open one has it under its request");
        let band = finished[2].band.as_ref().unwrap();
        assert_eq!(band.parts.iter().map(|part| part.part).collect::<Vec<_>>(), [Part::Waiting, Part::Reading, Part::Waiting, Part::Thinking]);
        assert_eq!(band_key(band, None), (vec![Part::Waiting, Part::Reading, Part::Thinking], true));
        assert_eq!(part_words(Part::Reading), "Reading the prompt");
        // The turn at work, thinking for the last four seconds, ten seconds after it began.
        let begun = Instant::now();
        let mut live = Live::new(begun);
        for event in [Event::TurnStarted, Event::RequestStarted, Event::Reasoning("so".into())] {
            live.note(&event, begun);
        }
        let wall = timeline::millis("2026-11-05T10:00:10.000+09:00");
        let inputs = Inputs { running: true, wall_ms: wall, ..Inputs::default() };
        let at_work = rows(Some(&timeline), &live, &inputs, &HashMap::new(), begun + Duration::from_secs(4));
        let row = at_work.iter().find(|row| row.kind == Kind::Band).unwrap();
        let now = row.live_part.expect("the part going on now");
        assert_eq!((now.lane, now.part, now.from, now.to), (Lane::Requests, Part::Thinking, 6000, 10_000), "from when it began, not from the turn's start");
        assert_eq!(row.band.as_ref().unwrap().length, 10_000);
    }

    /// A program at work is what goes on now, at the call it has come to; it is its own call's, and ends with it.
    #[test]
    fn a_program_at_work_is_shown_now_with_the_call_it_has_come_to() {
        let at = Instant::now();
        let mut live = Live::new(at);
        for event in [Event::TurnStarted, Event::RequestStarted, Event::ToolStarted { id: "p".into(), tool: "run_code".into(), summary: "read both files".into() }] {
            live.note(&event, at);
        }
        let inputs = Inputs { running: true, ..Inputs::default() };
        assert!(live.now(&inputs, at).text.starts_with("Running"), "before its first call it is the call that runs it");
        let step = |call: u32, path: &str| Event::ProgramStep { id: "p".into(), call, tool: "read_file".into(), summary: path.into() };
        assert!(live.note(&step(1, "a.txt"), at), "a call of the program is a change to show");
        live.note(&step(2, "b.txt"), at);
        let now = live.now(&inputs, at);
        assert_eq!((now.who, now.text), (Who::App, format!("A program runs: read both files, call 2: {}", call_words("read_file", "b.txt"))));
        // (a step of a call that is not under way is no one's)
        live.note(&Event::ProgramStep { id: "other".into(), call: 9, tool: "read_file".into(), summary: "c.txt".into() }, at);
        assert!(live.now(&inputs, at).text.contains("call 2"));
        live.note(&Event::ToolEnded { id: "p".into(), ok: true, declined: false, elapsed: Duration::from_millis(30), output: String::new(), images: Vec::new() }, at);
        assert!(live.program.is_none(), "it ends with its call");
    }

    /// An explore's helper at work is what goes on now, step by step; read from the files, its steps are under its call when the step is open.
    #[test]
    fn an_explore_helper_is_shown_now_at_its_step_and_after_with_its_steps_under_the_call() {
        let at = Instant::now();
        let mut live = Live::new(at);
        for event in [Event::TurnStarted, Event::RequestStarted, Event::ToolStarted { id: "e1".into(), tool: "explore".into(), summary: "Where is it?".into() },
                      Event::HelperStarted { id: "e1".into(), task: "Where is it?".into(), most: 12, file: None }] {
            live.note(&event, at);
        }
        let inputs = Inputs { running: true, ..Inputs::default() };
        assert_eq!(live.now(&inputs, at).text, "A helper explores: Where is it?");
        let step = Event::HelperStep { id: "e1".into(), step: 2, most: 12, tool: "read_file".into(), summary: "src/a.rs".into(), tokens: 900, elapsed: Duration::from_secs(3) };
        assert!(live.note(&step, at), "a step of the helper is a change to show");
        let now = live.now(&inputs, at);
        assert_eq!((now.who, now.text), (Who::Model, format!("A helper explores, step 2 of 12: {}", call_words("read_file", "src/a.rs"))));
        let ended = Event::HelperEnded { id: "e1".into(), how: dogmind_agent::agent::HelperEnd::Answered, steps: 2, files: 1, tokens: 900, elapsed: Duration::from_secs(4), helper: None };
        live.note(&ended, at);
        assert!(live.now(&inputs, at).text.starts_with("Running"), "the explore call itself, its helper done");
        // Read from the files: the helper's steps under the call, when the step is open.
        let mut explore = call("e1", "explore", "Where is it?", "ok");
        explore.helper_steps = vec![
            HelperStep { why: Some(Why { text: "I will search for it.".into(), source: WhySource::Content }), calls: vec![("search_files".into(), Some("needle".into()))] },
            HelperStep { why: None, calls: vec![("read_file".into(), Some("a.txt".into()))] },
        ];
        let timeline = Timeline { turns: vec![turn(1, "Go", vec![model(None, vec![explore])], done(100))], heading: None };
        let shut = texts(&rows_for(&timeline, &Live::new(at), &HashMap::new()));
        assert!(!shut.iter().any(|text| text.contains("needle")), "not before the step is open");
        let open = texts(&rows_for(&timeline, &Live::new(at), &HashMap::from([(Key::Step(1, 0), true)])));
        let from = open.iter().position(|text| text == "What the helper did").expect("its caption");
        assert!(open[from + 1].contains("I will search for it.") && open[from + 2].contains("needle") && open[from + 3].contains("a.txt"), "{open:#?}");
    }

    #[test]
    fn a_reason_in_markdown_reads_as_plain_words() {
        assert_eq!(plain_words("[a.txt](a.txt) says **Thursday**, see `b.txt`."), "a.txt says Thursday, see b.txt.");
        assert_eq!(plain_words("2 * 3 is [six"), "2 * 3 is [six", "what is no mark stays");
    }

    /// Of what the server says, where the wait to the first word went is shown first; the rest is folded under it, and opens on its own.
    #[test]
    fn the_servers_first_numbers_are_shown_and_the_rest_folded_under_them() {
        let server: BTreeMap<String, Value> = [("ttft_seconds", serde_json::json!(0.6)), ("prefill_seconds", serde_json::json!(0.5)), ("resumed_from", serde_json::json!("root")),
            ("draft_tokens", serde_json::json!(65)), ("decode_passes", serde_json::json!(28)), ("agreed_tokens", serde_json::json!(4483))].into_iter().map(|(name, value)| (name.to_string(), value)).collect();
        let mut step = model(Some(("Reading.", WhySource::Content)), Vec::new());
        step.request = Some(RequestDetail { id: "s/1".into(), ms: 900, server, ..RequestDetail::default() });
        let timeline = Timeline { turns: vec![turn(1, "Go", vec![step], done(100))], heading: None };
        let opened = |extra: Option<Key>| HashMap::from_iter([(Key::Step(1, 0), true)].into_iter().chain(extra.map(|key| (key, true))));
        let folded = rows_for(&timeline, &Live::new(Instant::now()), &opened(None));
        let caption = folded.iter().find(|row| row.kind == Kind::Caption).unwrap();
        assert_eq!((caption.text.as_str(), caption.key, caption.open), ("What the server says (3 more)", Some(Key::Server(1, 0)), false));
        let shown = texts(&folded);
        assert!(shown.contains(&"To the first token: 0.60 s".to_string()) && !shown.iter().any(|text| text.starts_with("Drafted tokens")), "{shown:#?}");
        let all = texts(&rows_for(&timeline, &Live::new(Instant::now()), &opened(Some(Key::Server(1, 0)))));
        assert!(all.contains(&"Drafted tokens: 65".to_string()) && all.contains(&"What the server says".to_string()), "{all:#?}");
    }

    /// What is looked for shows only the turns and steps that have it, open: in why a step was taken, its calls and what they were on, or the turn's
    /// request (then all its steps); nothing that has it says so.
    #[test]
    fn what_is_looked_for_shows_only_the_turns_and_steps_with_it() {
        let reading = model(Some(("I will read the budget.", WhySource::Content)), vec![call("a", "read_file", "budget.txt", "ok")]);
        let testing = model(Some(("Now the tests.", WhySource::Content)), vec![call("b", "execute_command", "cargo", "ok")]);
        let other = model(Some(("Writing it.", WhySource::Content)), vec![call("c", "create_file", "notes.md", "ok")]);
        let timeline = Timeline { turns: vec![turn(1, "Read the budget", vec![reading.clone()], done(100)), turn(2, "Run the tests", vec![testing, other], done(100))], heading: None };
        let shown = |query: &str| {
            let inputs = Inputs { query: query.into(), ..Inputs::default() };
            texts(&rows(Some(&timeline), &Live::new(Instant::now()), &inputs, &HashMap::new(), Instant::now()))
        };
        let cargo = shown("cargo");
        assert!(cargo.iter().any(|text| text.contains("Run the tests")) && !cargo.iter().any(|text| text.contains("Read the budget")), "{cargo:#?}");
        assert!(cargo.iter().any(|text| text == "Now the tests.") && !cargo.iter().any(|text| text == "Writing it."), "only the step with it: {cargo:#?}");
        // (by the tool's own name too, and in small or capital letters alike)
        assert!(shown("create_file").iter().any(|text| text == "Writing it."));
        assert!(shown("BUDGET").iter().any(|text| text == "I will read the budget."));
        assert_eq!(shown("nowhere"), ["No matches"]);
        // Empty: all of it, the earlier turn folded as before.
        assert!(shown("").iter().any(|text| text.contains("Read the budget")));
    }

    /// A press on the band goes to the step whose request or calls ran at that time; between two, to the one begun before.
    #[test]
    fn a_time_of_the_band_is_in_the_step_that_ran_then() {
        let at = |seconds: &str| format!("2026-11-05T10:00:{seconds}+09:00");
        let step = |started: &str, ms: u64, calls: Vec<CallRow>| Step { kind: StepKind::Model, started: at(started), ms, calls, ..Step::default() };
        let running = CallRow { started: at("03.000"), ms: 4000, ..call("c", "execute_command", "cargo", "ok") };
        let first = step("01.000", 2000, vec![running]);
        let second = step("09.000", 1000, Vec::new());
        let turn = TurnPath { turn: 1, started: at("00.000"), steps: vec![first, second], ..TurnPath::default() };
        assert_eq!(step_at(&turn, 1500), Some(0), "its request");
        assert_eq!(step_at(&turn, 5000), Some(0), "its call");
        assert_eq!(step_at(&turn, 8000), Some(0), "between: the one begun before");
        assert_eq!(step_at(&turn, 9500), Some(1));
        assert_eq!(step_at(&turn, 500), None, "before any began");
    }

    /// A part of a turn's time chosen: its band is that part across its width, a row under it says which with a press to show it all, and only the
    /// steps that ran in it are shown.
    #[test]
    fn a_part_of_a_turns_time_chosen_shows_that_part_of_its_band_and_only_the_steps_that_ran_in_it() {
        let at = |seconds: &str| format!("2026-11-05T10:00:{seconds}+09:00");
        let request = |started: &str| RequestDetail { started: at(started), first_ms: Some(500), ms: 2000, ..RequestDetail::default() };
        let step = |started: &str, words: &str| Step { kind: StepKind::Model, started: at(started), ms: 2000, why: Some(Why { text: words.into(), source: WhySource::Content }), request: Some(request(started)), ..Step::default() };
        let path = TurnPath { turn: 1, asked: "Go".into(), started: at("00.000"), steps: vec![step("01.000", "First."), step("05.000", "Second."), step("09.000", "Third.")], ended: done(11_000), ..TurnPath::default() };
        let timeline = Timeline { turns: vec![path], heading: None };
        let shown = |spans: BTreeMap<u64, (u64, u64)>| rows(Some(&timeline), &Live::new(Instant::now()), &Inputs { spans, ..Inputs::default() }, &HashMap::new(), Instant::now());
        let all = shown(BTreeMap::new());
        let band = all.iter().find(|row| row.kind == Kind::Band).unwrap();
        assert_eq!((band.whole, band.span), (11_000, None));
        assert!(!all.iter().any(|row| row.kind == Kind::Span));
        // From 4.5 to 8 seconds: the second step only, and the band 3.5 seconds long, its parts laid from 4.5.
        let part = shown([(1, (4500, 8000))].into_iter().collect());
        let band = part.iter().find(|row| row.kind == Kind::Band).unwrap();
        assert_eq!((band.whole, band.span, band.band.as_ref().unwrap().length), (11_000, Some((4500, 8000)), 3500));
        assert!(band.band.as_ref().unwrap().parts.iter().all(|part| part.to <= 3500 && part.from < part.to));
        let said = texts(&part);
        assert!(said.contains(&"Second.".to_string()) && !said.contains(&"First.".to_string()) && !said.contains(&"Third.".to_string()), "{said:#?}");
        let span = part.iter().find(|row| row.kind == Kind::Span).unwrap();
        assert_eq!((span.key, span.text.as_str()), (Some(Key::Whole(1)), "4.5 s to 8.0 s of the turn"));
        // (kept inside the turn, never shorter than the shortest, and all of it is no part of it)
        assert_eq!(span_inside((10_990, 11_000), 11_000), Some((11_000 - SHORTEST_SPAN, 11_000)));
        assert_eq!(span_inside((0, 20_000), 11_000), None);
    }

    #[test]
    fn the_servers_numbers_have_names_in_words_and_where_it_went_on_from_is_said() {
        let server: BTreeMap<String, Value> = [("resumed_from".to_string(), serde_json::json!("root")), ("agreed_tokens".to_string(), serde_json::json!(4483)), ("decode_tokens_per_second".to_string(), serde_json::json!(77.67))].into_iter().collect();
        let facts = server_facts(&server.iter().collect::<Vec<_>>(), 2);
        let said: Vec<(&str, &str)> = facts.iter().map(|row| (row.label.as_str(), row.text.as_str())).collect();
        assert_eq!(said, [("Tokens that matched what it had", "4483"), ("Writing speed (tokens per second)", "77.7"), ("Went on from", "the system prompt it kept")]);
        assert!(facts.iter().all(|row| !row.label.contains('_')));
        // (the points an engine keeps in a request, in words too)
        for kind in ["step", "mark", "mark_before"] {
            assert_ne!(resumed_words(kind), kind);
        }
    }
}
