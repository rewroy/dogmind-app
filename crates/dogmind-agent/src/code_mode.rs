//! Code Mode's engine and its bridge. Instead of calling the tools one by one (a round trip to the model for each), the model writes a short JavaScript program that calls them as
//! `await tools.read_file({ path: "a.txt" })`, and `run` runs it in an embedded engine and hands back only what the program returns, what it logged and how it ended.
//!
//! The program is the body of an async function, so it may `await` and `return`. It runs on a thread of its own (a fresh engine for each run, its memory and its stack bounded); a call
//! of `tools.<name>` sends the name and the arguments over a channel and waits for the answer, which `run` gives on the caller's thread by calling the closure it was handed: the
//! agent that owns the tools stays the only one that touches them, and its approvals work as they always did. The program has the language's own built-ins and these two things
//! (`tools`, `console`), no file, network or process access, and no clock to wait on.
//!
//! The time limit counts the program's own running time and not the time it waits for an answer: a call may wait minutes for a person's approval.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;
use rquickjs::context::EvalOptions;
use rquickjs::{Context, Ctx, Error as EngineError, Function, Promise, Runtime, Value};
use serde_json::{json, Value as Json};

use crate::tools::cap;

/// What a run's time limit may be: the one it has unless the model asks for another, and the most and least it may ask for.
pub const TIME_DEFAULT: Duration = Duration::from_secs(120);
pub const TIME_MOST: Duration = Duration::from_secs(600);
pub const TIME_LEAST: Duration = Duration::from_secs(1);
/// The least memory an engine can start in (its own built-ins take a few hundred KiB).
const MEMORY_LEAST: usize = 2 << 20;
/// The depth the program's calls may nest to, in bytes of the engine's stack, and the stack of the thread it runs on (a debug build's frames are large).
const ENGINE_STACK: usize = 1 << 20;
const THREAD_STACK: usize = 8 << 20;
/// How much a program's `console.log` may keep; the rest is dropped and the log says so.
const LOG_MOST: usize = 256 * 1024;
/// How often `run` looks at the cancel flag while the program runs, and how long past its time limit it waits for a program that does not answer the engine's interrupt.
const POLL: Duration = Duration::from_millis(20);
const GRACE: Duration = Duration::from_secs(5);
/// The name the program's source has, in the engine's errors (the line numbers in them are the body's own).
const PROGRAM_FILE: &str = "program.js";

/// What bounds a run.
#[derive(Debug, Clone)]
pub struct Limits {
    /// The program's own running time, not counting the time it waits for a call's answer. At most [`TIME_MOST`].
    pub time: Duration,
    /// The engine's memory, in bytes.
    pub memory: usize,
    /// How many calls the program may make.
    pub calls: u32,
    /// How much of the result the model is shown (bytes).
    pub output: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { time: TIME_DEFAULT, memory: 64 << 20, calls: 50, output: 32 * 1024 }
    }
}

impl Limits {
    /// These limits with the time the model asked for (`timeout_seconds`): none asked, or a number that is not one, keeps the time they have; one asked is held between
    /// [`TIME_LEAST`] and [`TIME_MOST`].
    pub fn with_timeout_seconds(mut self, seconds: Option<f64>) -> Self {
        if let Some(seconds) = seconds.filter(|seconds| seconds.is_finite()) {
            self.time = Duration::from_secs_f64(seconds.max(0.0)).clamp(TIME_LEAST, TIME_MOST);
        }
        self
    }
}

/// Why a run was stopped before the program ended by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Time,
    Memory,
    Calls,
    Cancelled,
}

impl Stop {
    fn code(self) -> u8 {
        match self {
            Stop::Time => 1,
            Stop::Memory => 2,
            Stop::Calls => 3,
            Stop::Cancelled => 4,
        }
    }

    fn from_code(code: u8) -> Option<Stop> {
        match code {
            1 => Some(Stop::Time),
            2 => Some(Stop::Memory),
            3 => Some(Stop::Calls),
            4 => Some(Stop::Cancelled),
            _ => None,
        }
    }

    /// What the model reads of it, with the limit that was reached.
    pub fn sentence(self, limits: &Limits) -> String {
        match self {
            Stop::Time => format!(
                "the program ran past its time limit of {} s (its own running time: the time spent inside tool calls is not counted)",
                limits.time.as_secs_f64()
            ),
            Stop::Memory => format!("the program used more memory than its limit of {} MiB", limits.memory as f64 / (1 << 20) as f64),
            Stop::Calls => format!("the program made more tool calls than its limit of {}", limits.calls),
            Stop::Cancelled => "the run was cancelled".to_string(),
        }
    }
}

/// A tool the program called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NestedCall {
    pub name: String,
    /// Whether it gave its text (`Ok`) and did not fail.
    pub ok: bool,
    pub ms: u64,
}

/// How a run ended.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    /// What the program returned: a string as it is, anything else as JSON. None for `undefined` (and for a run that failed).
    pub value: Option<String>,
    /// What it wrote with `console.log` and `console.error`, a line to each call.
    pub log: String,
    /// The uncaught error, as the engine says it, with the program's line: `TypeError: x is not a function (line 3)`.
    pub error: Option<String>,
    pub stopped: Option<Stop>,
    /// The calls the program made, in order.
    pub calls: Vec<NestedCall>,
    /// The program's own running time, not counting the time it waited for a call's answer.
    pub ms: u64,
    /// A line for the model to read after the error, when the error is one it is known to make.
    pub hint: Option<&'static str>,
}

/// Said to a model whose program does not parse and looks like TypeScript: the engine runs plain JavaScript.
const TYPESCRIPT_HINT: &str = "Write plain JavaScript, without type annotations (no `: type`, `interface`, `type X =` or `as Type`).";

/// What the program's thread asks `run` for, or ends with.
enum Message {
    Call { name: String, args: serde_json::Value, reply: mpsc::Sender<Result<String, String>> },
    Done(Outcome),
}

/// What the program's thread and `run` share while the program runs.
#[derive(Default)]
struct Shared {
    /// The first reason the run is to stop ([`Stop::code`]), 0 while there is none.
    stop: AtomicU8,
    /// The caller's cancel flag, as `run` last saw it: the engine's interrupt handler can hold no borrow of it.
    cancel: AtomicBool,
    /// Nanoseconds the program spent waiting for the answers of its calls, which its time limit does not count.
    waited: AtomicU64,
}

impl Shared {
    /// Only the first reason is kept.
    fn stop(&self, why: Stop) {
        let _ = self.stop.compare_exchange(0, why.code(), Ordering::SeqCst, Ordering::SeqCst);
    }

    fn stopped(&self) -> Option<Stop> {
        Stop::from_code(self.stop.load(Ordering::SeqCst))
    }

    /// Notes a cancel and says whether the run is to stop.
    fn should_stop(&self) -> bool {
        if self.cancel.load(Ordering::SeqCst) {
            self.stop(Stop::Cancelled);
        }
        self.stopped().is_some()
    }
}

/// The program's side of the channel to `run`: what `tools.<name>` stands on.
struct Bridge {
    shared: Arc<Shared>,
    to_run: mpsc::Sender<Message>,
    made: Cell<u32>,
    most: u32,
}

impl Bridge {
    fn call(&self, name: &str, args: &str) -> Result<String, String> {
        if self.shared.should_stop() {
            return Err(self.refusal());
        }
        if self.made.get() >= self.most {
            self.shared.stop(Stop::Calls);
            return Err(self.refusal());
        }
        let args: serde_json::Value = serde_json::from_str(args).map_err(|error| format!("the arguments are not JSON: {error}"))?;
        self.made.set(self.made.get() + 1);
        let (reply, answer) = mpsc::channel();
        let waiting = Instant::now();
        if self.to_run.send(Message::Call { name: name.to_string(), args, reply }).is_err() {
            self.shared.stop(Stop::Cancelled);
            return Err(self.refusal());
        }
        let answer = answer.recv();
        self.shared.waited.fetch_add(u64::try_from(waiting.elapsed().as_nanos()).unwrap_or(u64::MAX), Ordering::SeqCst);
        // A cancel that came while the call ran ends the run here, whatever the call answered.
        if self.shared.should_stop() {
            return Err(self.refusal());
        }
        match answer {
            Ok(answer) => answer,
            Err(_) => {
                self.shared.stop(Stop::Cancelled);
                Err(self.refusal())
            }
        }
    }

    /// What the program's `await` throws for a call that is not made because the run is stopping.
    fn refusal(&self) -> String {
        match self.shared.stopped() {
            Some(Stop::Calls) => format!("no more tool calls: a program may make {} and has made them", self.most),
            Some(Stop::Time) => "the program's time is up".to_string(),
            _ => "the run was cancelled".to_string(),
        }
    }
}

/// The program's side of `tools` and `console`, in JavaScript. `host(name, json)` answers a string whose first character says how the call went ("0" it gave its text,
/// "1" it failed), so a failure throws an Error with the tool's own text in the program. The setup function is called once with what only Rust holds, and leaves
/// `tools` and `console` as the globals; nothing else of it stays in reach of the program.
const SETUP: &str = r#"(function (host, write, names) {
    const shown = (value) => {
        if (typeof value === 'string') return value;
        try {
            const json = JSON.stringify(value);
            return json === undefined ? String(value) : json;
        } catch (error) {
            return String(value);
        }
    };
    const tools = {};
    for (const name of names) {
        tools[name] = async function (args) {
            if (args === undefined) args = {};
            if (args === null || typeof args !== 'object' || Array.isArray(args)) {
                throw new TypeError('tools.' + name + ' takes one object of arguments, like tools.' + name + '({ ... })');
            }
            const answer = host(name, JSON.stringify(args));
            if (answer.charCodeAt(0) === 49) throw new Error(answer.slice(1));
            return answer.slice(1);
        };
    }
    const log = (...parts) => write(parts.map(shown).join(' '));
    globalThis.tools = Object.freeze(tools);
    globalThis.console = Object.freeze({ log, error: log, warn: log, info: log, debug: log });
})"#;

/// Runs `code`, the body of an async function, with `tools` callable as `tools.<name>(args)`, and answers each call by `call(name, args)` on this thread. `cancel` set ends the run
/// at the program's next step, or at once when it waits for a call. Everything about the run, including how it failed, is in the [`Outcome`].
pub fn run(code: &str, tools: &[&str], limits: &Limits, cancel: &AtomicBool, call: &mut dyn FnMut(&str, serde_json::Value) -> Result<String, String>) -> Outcome {
    let mut limits = limits.clone();
    limits.time = limits.time.min(TIME_MOST);
    let shared = Arc::new(Shared::default());
    let (to_run, from_program) = mpsc::channel::<Message>();
    let names: Vec<String> = tools.iter().map(|name| name.to_string()).collect();
    let program = wrap(code);
    let started = Instant::now();
    let thread = {
        let (shared, limits, names, code) = (shared.clone(), limits.clone(), names.clone(), code.to_string());
        std::thread::Builder::new().name("code-mode".to_string()).stack_size(THREAD_STACK).spawn(move || {
            let outcome = execute(&program, &code, &names, &limits, &shared, &to_run);
            let _ = to_run.send(Message::Done(outcome));
        })
    };
    let thread = match thread {
        Ok(thread) => thread,
        Err(error) => return Outcome { error: Some(format!("the program's thread could not be started: {error}")), ..Outcome::default() },
    };

    let mut calls: Vec<NestedCall> = Vec::new();
    let mut in_calls = Duration::ZERO;
    let mut finished: Option<Outcome> = None;
    let mut abandoned = false;
    loop {
        match from_program.recv_timeout(POLL) {
            Ok(Message::Done(outcome)) => {
                finished = Some(outcome);
                break;
            }
            Ok(Message::Call { name, args, reply }) => {
                let began = Instant::now();
                let answer = if cancel.load(Ordering::SeqCst) {
                    Err("the run was cancelled".to_string())
                } else if !tools.contains(&name.as_str()) {
                    Err(format!("{name} is not a tool this program has"))
                } else {
                    call(&name, args)
                };
                // The flag may have been set while the call ran (a person pressing stop): the program's side sees it before it takes the answer.
                if cancel.load(Ordering::SeqCst) {
                    shared.cancel.store(true, Ordering::SeqCst);
                }
                let spent = began.elapsed();
                in_calls += spent;
                calls.push(NestedCall { name, ok: answer.is_ok(), ms: millis(spent) });
                let _ = reply.send(answer);
            }
            Err(RecvTimeoutError::Timeout) => {
                if cancel.load(Ordering::SeqCst) {
                    shared.cancel.store(true, Ordering::SeqCst);
                }
                // A program the engine's interrupt cannot reach (it is inside something that does not look at the interrupt) is left behind once it is well past its time.
                if started.elapsed().saturating_sub(in_calls) > limits.time + GRACE {
                    shared.stop(Stop::Time);
                    abandoned = true;
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if !abandoned {
        let _ = thread.join();
    }
    let mut outcome = finished.unwrap_or_else(|| Outcome {
        error: (!abandoned).then(|| "the program's engine stopped without an answer".to_string()),
        stopped: shared.stopped(),
        ms: millis(started.elapsed().saturating_sub(in_calls)),
        ..Outcome::default()
    });
    outcome.calls = calls;
    outcome
}

/// The body as the function the engine runs. Its opening shares the body's first line, and its closing is on a line after the body's last, so a line the engine names is the body's line.
fn wrap(code: &str) -> String {
    format!("(async () => {{{code}\n}})()")
}

fn millis(time: Duration) -> u64 {
    u64::try_from(time.as_millis()).unwrap_or(u64::MAX)
}

/// The program's thread: a fresh engine, the program run to its end, and how it went.
fn execute(program: &str, code: &str, names: &[String], limits: &Limits, shared: &Arc<Shared>, to_run: &mpsc::Sender<Message>) -> Outcome {
    let started = Instant::now();
    let own_time = {
        let shared = shared.clone();
        move || started.elapsed().saturating_sub(Duration::from_nanos(shared.waited.load(Ordering::SeqCst)))
    };
    let (runtime, context) = match Runtime::new().and_then(|runtime| {
        runtime.set_memory_limit(limits.memory.max(MEMORY_LEAST));
        runtime.set_max_stack_size(ENGINE_STACK);
        let handler_shared = shared.clone();
        let time = limits.time;
        let own_time = own_time.clone();
        // The engine asks this every few thousand steps; true stops the program where it is, and the stop cannot be caught by it.
        runtime.set_interrupt_handler(Some(Box::new(move || {
            if handler_shared.should_stop() {
                return true;
            }
            if own_time() > time {
                handler_shared.stop(Stop::Time);
                return true;
            }
            false
        })));
        Context::full(&runtime).map(|context| (runtime, context))
    }) {
        Ok(both) => both,
        Err(error) => return Outcome { error: Some(format!("the program's engine could not be started: {error}")), ..Outcome::default() },
    };

    let log = Rc::new(RefCell::new(Log::default()));
    let bridge = Bridge { shared: shared.clone(), to_run: to_run.clone(), made: Cell::new(0), most: limits.calls };
    let result = context.with(|ctx| -> Result<Option<String>, String> {
        let host = Function::new(ctx.clone(), move |name: String, args: String| -> String {
            match bridge.call(&name, &args) {
                Ok(text) => format!("0{text}"),
                Err(text) => format!("1{text}"),
            }
        })
        .map_err(|error| describe_engine_error(&ctx, error))?;
        let write = {
            let log = log.clone();
            Function::new(ctx.clone(), move |line: String| log.borrow_mut().push(&line)).map_err(|error| describe_engine_error(&ctx, error))?
        };
        let setup: Function = ctx.eval_with_options(SETUP, options("tools.js")).map_err(|error| describe_engine_error(&ctx, error))?;
        setup.call::<_, ()>((host, write, names.to_vec())).map_err(|error| describe_engine_error(&ctx, error))?;
        // The program's own time starts here, after the setup.
        let promise: Promise = ctx.eval_with_options(program, options(PROGRAM_FILE)).map_err(|error| describe_engine_error(&ctx, error))?;
        let value: Value = promise.finish().map_err(|error| describe_engine_error(&ctx, error))?;
        returned(&ctx, value)
    });
    let ms = millis(own_time());
    let log = log.borrow().text();
    drop(context);
    drop(runtime);

    let mut stopped = shared.stopped();
    let (value, mut error) = match result {
        Ok(value) => (value, None),
        Err(error) => (None, Some(error)),
    };
    // The engine's out of memory is an error the program could catch; either way it is the memory limit that was reached.
    if stopped.is_none() && error.as_deref().is_some_and(|error| error.starts_with("InternalError: out of memory")) {
        stopped = Some(Stop::Memory);
    }
    // A stop that the engine reports as an error of its own ("interrupted"), or as the error a call threw because the run was ending, says nothing the stop does not.
    if matches!(stopped, Some(Stop::Time | Stop::Calls | Stop::Cancelled)) {
        error = None;
    }
    let hint = error.as_deref().filter(|error| error.starts_with("SyntaxError") && looks_like_typescript(code)).map(|_| TYPESCRIPT_HINT);
    Outcome { value, log, error, stopped, calls: Vec::new(), ms, hint }
}

fn options(file: &str) -> EvalOptions {
    // Sloppy mode: a program that forgets a `const` still runs.
    let mut options = EvalOptions::default();
    options.strict = false;
    options.filename = Some(file.to_string());
    options
}

/// What `console.log` keeps.
#[derive(Default)]
struct Log {
    lines: Vec<String>,
    bytes: usize,
    cut: bool,
}

impl Log {
    fn push(&mut self, line: &str) {
        if self.cut {
            return;
        }
        let room = LOG_MOST.saturating_sub(self.bytes);
        if line.len() > room {
            let mut end = room;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            if end > 0 {
                self.lines.push(line[..end].to_string());
            }
            self.cut = true;
            return;
        }
        self.bytes += line.len() + 1;
        self.lines.push(line.to_string());
    }

    fn text(&self) -> String {
        let mut text = self.lines.join("\n");
        if self.cut {
            text.push_str(&format!("\n(the log was cut here: it holds at most {} KiB)", LOG_MOST / 1024));
        }
        text
    }
}

/// The value the program returned, as text: a string as it is, `undefined` (and a function or a symbol, which JSON has no text for) as none, anything else as JSON.
fn returned<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Result<Option<String>, String> {
    if value.is_undefined() {
        return Ok(None);
    }
    if let Some(text) = value.as_string() {
        return text.to_string().map(Some).map_err(|error| describe_engine_error(ctx, error));
    }
    match ctx.json_stringify(value) {
        Ok(Some(json)) => json.to_string().map(Some).map_err(|error| describe_engine_error(ctx, error)),
        Ok(None) => Ok(None),
        Err(error) => Err(format!("the value the program returned cannot be written as JSON: {}", describe_engine_error(ctx, error))),
    }
}

/// An engine error as the model should read it: what was thrown, with the program's line.
fn describe_engine_error<'js>(ctx: &Ctx<'js>, error: EngineError) -> String {
    match error {
        EngineError::Exception => describe_thrown(ctx, &ctx.catch()),
        EngineError::WouldBlock => "the program never finished: it waits on a promise that nothing settles".to_string(),
        other => other.to_string(),
    }
}

fn describe_thrown<'js>(ctx: &Ctx<'js>, thrown: &Value<'js>) -> String {
    let Some(error) = thrown.as_exception() else {
        // Something that is not an Error was thrown (`throw "no"`, `throw { code: 3 }`).
        if let Some(text) = thrown.as_string() {
            return text.to_string().unwrap_or_default();
        }
        return ctx
            .json_stringify(thrown.clone())
            .ok()
            .flatten()
            .and_then(|json| json.to_string().ok())
            .unwrap_or_else(|| "a value that cannot be shown".to_string());
    };
    let name = error.as_object().get::<_, Option<String>>("name").ok().flatten().unwrap_or_else(|| "Error".to_string());
    let message = error.message().unwrap_or_default();
    let mut text = if message.is_empty() { name } else { format!("{name}: {message}") };
    if let Some(line) = error.stack().as_deref().and_then(program_line) {
        text.push_str(&format!(" (line {line})"));
    }
    text
}

/// The line of the program's own code that is first in an error's stack (`    at fetch_all (program.js:3:15)`); frames in the engine's setup are not the program's.
fn program_line(stack: &str) -> Option<u32> {
    let marker = format!("{PROGRAM_FILE}:");
    stack.lines().find_map(|frame| {
        let after = &frame[frame.find(&marker)? + marker.len()..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    })
}

/// Whether a program that does not parse looks like it was written in TypeScript: a type on a declaration or a parameter, `interface`, `type X =`, `as Type`.
fn looks_like_typescript(code: &str) -> bool {
    static TYPED: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = TYPED.get_or_init(|| {
        const PRIMITIVE: &str = "string|number|boolean|any|unknown|void|never|object";
        [
            r"(?m)^\s*(?:export\s+)?(?:interface|enum)\s+[A-Za-z_]\w*".to_string(),
            r"(?m)^\s*(?:export\s+)?type\s+[A-Za-z_]\w*\s*(?:<[^>]*>)?\s*=".to_string(),
            r"\b(?:let|const|var)\s+[A-Za-z_$][\w$]*\??\s*:\s*[A-Za-z_{\[(]".to_string(),
            format!(r"\bas\s+(?:{PRIMITIVE}|const|[A-Z][A-Za-z0-9_]*)\b"),
            format!(r"[(,]\s*[A-Za-z_$][\w$]*\??\s*:\s*(?:{PRIMITIVE})\b"),
            format!(r"\)\s*:\s*(?:{PRIMITIVE}|Promise\s*<)"),
        ]
        .iter()
        .filter_map(|pattern| Regex::new(pattern).ok())
        .collect()
    });
    patterns.iter().any(|pattern| pattern.is_match(code))
}

/// What the model is given back for a run: the value (or that there was none), then the log if there is one, then the error and why it was stopped. Cut to `limits.output` bytes
/// with its head and its tail kept and what was left out said between them.
pub fn render(outcome: &Outcome, limits: &Limits) -> String {
    let mut text = outcome.value.clone().unwrap_or_else(|| "(no value returned)".to_string());
    if !outcome.log.is_empty() {
        text.push_str("\n\nLog:\n");
        text.push_str(&outcome.log);
    }
    if let Some(error) = &outcome.error {
        text.push_str("\n\nUncaught ");
        text.push_str(error);
    }
    if let Some(stop) = outcome.stopped {
        text.push_str("\n\nStopped: ");
        text.push_str(&stop.sentence(limits));
    }
    if let Some(hint) = outcome.hint {
        text.push_str("\n\n");
        text.push_str(hint);
    }
    cap(&text, limits.output)
}

/// What a program cannot call: `run_code` itself, the tools that wait for the person's answer or decision (it has no one to ask), `explore` (a helper of its own) and `wake_at` (the session's
/// own clock). They are left out of what the model is told, and refused if called.
pub const NOT_CALLABLE: [&str; 5] = ["run_code", "submit_plan", "ask_user", "explore", "wake_at"];

/// The rules the model reads before the tools it can call from a program.
const RULES: &str = "Run a short JavaScript program that calls tools as `await tools.name({...})`; only what it returns comes back. Use it when the work needs three or more tool calls whose order can be decided up front (several reads, a search then reads). Plain JavaScript, no type annotations. `return` what you need to see; `console.log` for notes; a failed call throws.";
/// The rules that push for programs, to measure whether a model can write them when it is told to (DOGMIND_CODE_MODE_EAGER=1): the
/// default ones above left DeepSeek writing none in six runs, as it puts calls that do not depend on each other in one answer already.
const RULES_EAGER: &str = "Run a short JavaScript program that calls tools as `await tools.name({...})`; only what it returns comes back. Prefer it to calling tools one at a time whenever the work needs two or more calls: read, search, edit and run the tests in one program where you can, and decide in the program what to do with what a call returned. Plain JavaScript, no type annotations. `return` what you need to see; `console.log` for notes; a failed call throws.";
/// The switch to [`RULES_EAGER`], read when the tools are made.
pub const EAGER_VARIABLE: &str = "DOGMIND_CODE_MODE_EAGER";
/// How much of a tool's first sentence its declaration keeps.
const ABOUT_MOST: usize = 160;

/// The names of the tools a program may call, of the tools that are offered (a list of schemas as the model is given them).
pub fn callable(offered: &Json) -> Vec<String> {
    offered.as_array().into_iter().flatten().filter_map(|tool| tool.pointer("/function/name")?.as_str()).filter(|name| !NOT_CALLABLE.contains(name)).map(str::to_string).collect()
}

/// The `run_code` tool as the model is offered it: the rules and, for each tool it can call, a line that says how (`tools.read_file({path, start_line?, limit?}) — Read a text file...`), made from
/// the tools that are offered with it, so that the web, notes and servers' tools are in it only when they are.
pub fn schema(offered: &Json) -> Json {
    schema_with(offered, std::env::var(EAGER_VARIABLE).is_ok_and(|value| value.trim() == "1"))
}

/// [`schema`], with the rules that push for programs or the default ones.
fn schema_with(offered: &Json, eager: bool) -> Json {
    let mut description = String::from(if eager { RULES_EAGER } else { RULES });
    for tool in offered.as_array().into_iter().flatten() {
        if let Some(line) = declaration(tool) {
            description.push('\n');
            description.push_str(&line);
        }
    }
    let time = format!("Its own running time in seconds, not counting the calls' waits. Default {}, at most {}.", TIME_DEFAULT.as_secs(), TIME_MOST.as_secs());
    json!({"type": "function", "function": {"name": "run_code", "description": description, "parameters": {"type": "object", "properties": {
        "code": {"type": "string", "description": "The body of an async function."},
        "description": {"type": "string", "description": "One line: what the program does."},
        "timeout_seconds": {"type": "number", "description": time}
    }, "required": ["code", "description"]}}})
}

/// One tool as a program calls it, with the first sentence of what it says it is. None for a tool a program cannot call.
fn declaration(tool: &Json) -> Option<String> {
    let function = tool.get("function")?;
    let name = function.get("name")?.as_str()?;
    if NOT_CALLABLE.contains(&name) {
        return None;
    }
    let plain = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.starts_with(|c: char| c.is_ascii_digit());
    let callee = if plain { format!("tools.{name}") } else { format!("tools[{name:?}]") };
    let arguments = function.get("parameters").map(fields).unwrap_or_default();
    let about = function.get("description").and_then(Json::as_str).map(first_sentence).unwrap_or_default();
    Some(if about.is_empty() { format!("{callee}({{{arguments}}})") } else { format!("{callee}({{{arguments}}}) — {about}") })
}

/// The fields of an object's schema as a program writes them: `path, start_line?, edits: [{path, old_string}]`. A `?` is a field that may be left out; the kind of a field is told when it
/// is not a plain value.
fn fields(schema: &Json) -> String {
    let required: Vec<&str> = schema.get("required").and_then(Json::as_array).into_iter().flatten().filter_map(Json::as_str).collect();
    let each = schema.get("properties").and_then(Json::as_object).into_iter().flatten().map(|(name, property)| {
        let mark = if required.contains(&name.as_str()) { "" } else { "?" };
        match structure(property) {
            Some(shape) => format!("{name}{mark}: {shape}"),
            None => format!("{name}{mark}"),
        }
    });
    each.collect::<Vec<_>>().join(", ")
}

/// What a value is, where a name does not say: the choices of an enumeration, the items of a list, the fields of an object.
fn structure(schema: &Json) -> Option<String> {
    if let Some(options) = schema.get("enum").and_then(Json::as_array) {
        let words: Vec<String> = options.iter().filter_map(Json::as_str).map(|word| format!("\"{word}\"")).collect();
        return (!words.is_empty()).then(|| words.join("|"));
    }
    match schema.get("type").and_then(Json::as_str)? {
        "array" => Some(format!("[{}]", structure(&schema["items"]).unwrap_or_else(|| plain_kind(&schema["items"])))),
        "object" if schema.get("properties").is_some() => Some(format!("{{{}}}", fields(schema))),
        _ => None,
    }
}

fn plain_kind(schema: &Json) -> String {
    match schema.get("type").and_then(Json::as_str) {
        Some("string") => "string",
        Some("integer" | "number") => "number",
        Some("boolean") => "boolean",
        _ => "any",
    }
    .to_string()
}

/// A description's first sentence, on one line and at most [`ABOUT_MOST`] characters.
fn first_sentence(text: &str) -> String {
    let text = text.trim();
    let end = text.char_indices().find(|(at, c)| (*c == '.' && text[at + 1..].chars().next().is_none_or(char::is_whitespace)) || *c == '。').map_or(text.len(), |(at, c)| at + c.len_utf8());
    let sentence = text[..end].split_whitespace().collect::<Vec<_>>().join(" ");
    if sentence.chars().count() <= ABOUT_MOST {
        return sentence;
    }
    let mut cut: String = sentence.chars().take(ABOUT_MOST - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn limits() -> Limits {
        Limits { memory: 16 << 20, ..Limits::default() }
    }

    fn go(code: &str, tools: &[&str], mut call: impl FnMut(&str, serde_json::Value) -> Result<String, String>) -> Outcome {
        run(code, tools, &limits(), &AtomicBool::new(false), &mut call)
    }

    fn go_alone(code: &str) -> Outcome {
        go(code, &[], |_, _| panic!("no call was expected"))
    }

    #[test]
    fn a_string_is_returned_as_it_is() {
        let outcome = go_alone("return 'hello\\nworld';");
        assert_eq!(outcome.value.as_deref(), Some("hello\nworld"));
        assert_eq!(outcome.error, None);
        assert_eq!(outcome.stopped, None);
    }

    #[test]
    fn a_number_and_an_object_are_returned_as_json() {
        assert_eq!(go_alone("return 6 * 7;").value.as_deref(), Some("42"));
        assert_eq!(go_alone("return { a: 1, b: [true, null, 'x'] };").value.as_deref(), Some(r#"{"a":1,"b":[true,null,"x"]}"#));
        assert_eq!(go_alone("return null;").value.as_deref(), Some("null"));
    }

    #[test]
    fn nothing_returned_is_no_value() {
        let outcome = go_alone("const x = 1;");
        assert_eq!(outcome.value, None);
        assert_eq!(outcome.error, None);
        assert_eq!(go_alone("return undefined;").value, None);
        assert!(render(&outcome, &limits()).starts_with("(no value returned)"));
    }

    #[test]
    fn nested_calls_reach_the_caller_in_order_with_their_arguments() {
        let mut seen: Vec<(String, serde_json::Value)> = Vec::new();
        let outcome = go(
            "const a = await tools.read_file({ path: 'a.txt', lines: [1, 2] });\nconst b = await tools.search_files({ pattern: 'x', deep: true });\nreturn a + '|' + b;",
            &["read_file", "search_files"],
            |name, args| {
                seen.push((name.to_string(), args));
                Ok(format!("<{name}>"))
            },
        );
        assert_eq!(outcome.value.as_deref(), Some("<read_file>|<search_files>"));
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], ("read_file".to_string(), json!({ "path": "a.txt", "lines": [1, 2] })));
        assert_eq!(seen[1], ("search_files".to_string(), json!({ "pattern": "x", "deep": true })));
        let names: Vec<&str> = outcome.calls.iter().map(|call| call.name.as_str()).collect();
        assert_eq!(names, ["read_file", "search_files"]);
        assert!(outcome.calls.iter().all(|call| call.ok));
    }

    #[test]
    fn two_awaited_calls_and_a_promise_all_of_two_both_work() {
        let answer = |name: &str, args: serde_json::Value| Ok(format!("{name}:{}", args["n"]));
        let outcome = go("const a = await tools.t({ n: 1 }); const b = await tools.t({ n: 2 }); return a + ' ' + b;", &["t"], answer);
        assert_eq!(outcome.value.as_deref(), Some("t:1 t:2"));
        let outcome = go("const both = await Promise.all([tools.t({ n: 1 }), tools.t({ n: 2 })]); return both.join(' ');", &["t"], answer);
        assert_eq!(outcome.value.as_deref(), Some("t:1 t:2"));
        assert_eq!(outcome.calls.len(), 2);
    }

    #[test]
    fn a_failed_call_throws_in_the_program_which_can_catch_it() {
        let outcome = go(
            "try { await tools.read_file({ path: 'nope' }); return 'no error'; } catch (error) { return 'caught: ' + error.message; }",
            &["read_file"],
            |_, _| Err("no such file: nope".to_string()),
        );
        assert_eq!(outcome.value.as_deref(), Some("caught: no such file: nope"));
        assert_eq!(outcome.error, None);
        assert_eq!(outcome.calls.len(), 1);
        assert!(!outcome.calls[0].ok);
    }

    #[test]
    fn a_failed_call_nobody_catches_is_the_outcomes_error() {
        let outcome = go("await tools.read_file({ path: 'nope' });\nreturn 'not reached';", &["read_file"], |_, _| Err("no such file: nope".to_string()));
        assert_eq!(outcome.value, None);
        let error = outcome.error.expect("the error");
        assert!(error.contains("no such file: nope"), "{error}");
        assert!(error.contains("(line 1)"), "{error}");
        assert_eq!(outcome.stopped, None);
    }

    #[test]
    fn a_tool_the_program_was_not_given_is_a_type_error_and_no_call() {
        let outcome = go_alone("return typeof tools.delete_everything;");
        assert_eq!(outcome.value.as_deref(), Some("undefined"));
        let outcome = go_alone("await tools.delete_everything({});");
        let error = outcome.error.expect("the error");
        assert!(error.starts_with("TypeError"), "{error}");
        assert!(outcome.calls.is_empty());
    }

    #[test]
    fn an_argument_that_is_not_an_object_is_an_error_in_the_program() {
        for bad in ["'a.txt'", "42", "null", "['a']", "true"] {
            let outcome = go(&format!("await tools.read_file({bad});"), &["read_file"], |_, _| panic!("a call was made with {bad}"));
            let error = outcome.error.unwrap_or_else(|| panic!("no error for {bad}"));
            assert!(error.starts_with("TypeError"), "{bad}: {error}");
        }
        // No argument at all is an empty object (a tool that takes none).
        let outcome = go("return await tools.status();", &["status"], |_, args| Ok(args.to_string()));
        assert_eq!(outcome.value.as_deref(), Some("{}"));
    }

    #[test]
    fn console_log_and_error_are_captured_in_the_log() {
        let outcome = go_alone("console.log('a', 1, { x: 2 });\nconsole.error('b');\nconsole.log([1, 'two']);\nreturn 'done';");
        assert_eq!(outcome.log, "a 1 {\"x\":2}\nb\n[1,\"two\"]");
        assert_eq!(outcome.value.as_deref(), Some("done"));
    }

    #[test]
    fn a_log_past_its_bound_is_cut_and_says_so() {
        let outcome = go_alone("const line = 'x'.repeat(1000); for (let i = 0; i < 400; i++) console.log(line); return 1;");
        assert!(outcome.log.len() < LOG_MOST + 200);
        assert!(outcome.log.ends_with("KiB)"), "{}", &outcome.log[outcome.log.len() - 60..]);
    }

    #[test]
    fn an_infinite_loop_is_stopped_by_a_short_time_limit() {
        let limits = Limits { time: Duration::from_millis(200), ..limits() };
        let began = Instant::now();
        let outcome = run("while (true) {}", &[], &limits, &AtomicBool::new(false), &mut |_, _| panic!("no call"));
        assert_eq!(outcome.stopped, Some(Stop::Time));
        assert_eq!(outcome.error, None);
        assert!(began.elapsed() < Duration::from_secs(5), "{:?}", began.elapsed());
        assert!(render(&outcome, &limits).contains("time limit"));
    }

    #[test]
    fn time_spent_inside_a_call_is_not_counted_against_the_program() {
        let limits = Limits { time: Duration::from_millis(200), ..limits() };
        let outcome = run(
            "const a = await tools.slow({}); const b = await tools.slow({}); return a + b;",
            &["slow"],
            &limits,
            &AtomicBool::new(false),
            &mut |_, _| {
                std::thread::sleep(Duration::from_millis(300));
                Ok("z".to_string())
            },
        );
        assert_eq!(outcome.stopped, None);
        assert_eq!(outcome.error, None);
        assert_eq!(outcome.value.as_deref(), Some("zz"));
        assert!(outcome.calls.iter().all(|call| call.ms >= 280), "{:?}", outcome.calls);
        assert!(outcome.ms < 200, "{} ms of its own", outcome.ms);
    }

    #[test]
    fn a_program_that_spins_after_a_slow_call_is_still_stopped_by_its_own_time() {
        let limits = Limits { time: Duration::from_millis(200), ..limits() };
        let outcome = run("await tools.slow({}); while (true) {}", &["slow"], &limits, &AtomicBool::new(false), &mut |_, _| {
            std::thread::sleep(Duration::from_millis(300));
            Ok(String::new())
        });
        assert_eq!(outcome.stopped, Some(Stop::Time));
    }

    #[test]
    fn the_memory_limit_stops_a_program_that_outgrows_it() {
        let limits = Limits { memory: 8 << 20, ..limits() };
        // (A string doubled on itself would not do: the engine joins strings lazily, and its own bound on a string's length comes before the memory's.)
        let outcome = run("const kept = []; for (;;) kept.push(new Array(10000).fill(kept.length));", &[], &limits, &AtomicBool::new(false), &mut |_, _| panic!("no call"));
        assert_eq!(outcome.stopped, Some(Stop::Memory), "{outcome:?}");
        assert_eq!(outcome.value, None);
        assert!(render(&outcome, &limits).contains("memory"));
    }

    #[test]
    fn the_calls_limit_throws_on_the_next_call_and_ends_the_run_as_stopped() {
        let limits = Limits { calls: 3, ..limits() };
        let mut made = 0;
        let outcome = run("for (let i = 0; i < 10; i++) await tools.t({ i });\nreturn 'all';", &["t"], &limits, &AtomicBool::new(false), &mut |_, _| {
            made += 1;
            Ok("ok".to_string())
        });
        assert_eq!(made, 3);
        assert_eq!(outcome.stopped, Some(Stop::Calls));
        assert_eq!(outcome.value, None);
        assert_eq!(outcome.calls.len(), 3);
        assert!(render(&outcome, &limits).contains("tool calls"));
    }

    #[test]
    fn a_program_that_catches_the_calls_limit_is_still_stopped_by_it() {
        let limits = Limits { calls: 2, ..limits() };
        let mut made = 0;
        let outcome = run(
            "let n = 0; try { for (;;) { await tools.t({}); n++; } } catch (error) { console.log('caught', error.message); }\nwhile (true) { n++; }",
            &["t"],
            &limits,
            &AtomicBool::new(false),
            &mut |_, _| {
                made += 1;
                Ok(String::new())
            },
        );
        assert_eq!(made, 2);
        assert_eq!(outcome.stopped, Some(Stop::Calls));
        assert!(outcome.log.starts_with("caught"), "{}", outcome.log);
    }

    #[test]
    fn cancel_set_during_a_call_ends_the_run_as_cancelled() {
        let cancel = AtomicBool::new(false);
        let outcome = run(
            "await tools.long({});\nconsole.log('after');\nreturn 'after';",
            &["long"],
            &limits(),
            &cancel,
            &mut |_, _| {
                cancel.store(true, Ordering::SeqCst);
                Ok("done".to_string())
            },
        );
        assert_eq!(outcome.stopped, Some(Stop::Cancelled));
        assert_eq!(outcome.value, None);
        assert_eq!(outcome.log, "");
        assert_eq!(outcome.calls.len(), 1);
    }

    #[test]
    fn cancel_set_before_the_next_call_stops_it_without_asking_the_caller() {
        let cancel = AtomicBool::new(false);
        let mut made = 0;
        let outcome = run(
            "await tools.t({});\nawait tools.t({});\nreturn 1;",
            &["t"],
            &limits(),
            &cancel,
            &mut |_, _| {
                made += 1;
                cancel.store(true, Ordering::SeqCst);
                Ok(String::new())
            },
        );
        assert_eq!(made, 1);
        assert_eq!(outcome.stopped, Some(Stop::Cancelled));
    }

    #[test]
    fn cancel_set_from_another_thread_interrupts_a_program_that_spins() {
        let cancel = Arc::new(AtomicBool::new(false));
        let setter = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                cancel.store(true, Ordering::SeqCst);
            })
        };
        let began = Instant::now();
        let outcome = run("while (true) {}", &[], &limits(), &cancel, &mut |_, _| panic!("no call"));
        setter.join().unwrap();
        assert_eq!(outcome.stopped, Some(Stop::Cancelled));
        assert!(began.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_syntax_error_that_looks_like_typescript_gets_the_hint() {
        for code in [
            "let x: number = 1;\nreturn x;",
            "interface Row { a: string }\nreturn 1;",
            "const rows = JSON.parse('[]') as Row[];\nreturn rows;",
            "function twice(n: number) { return n * 2; }\nreturn twice(2);",
            "type Row = { a: string };\nreturn 1;",
        ] {
            let outcome = go_alone(code);
            let error = outcome.error.clone().unwrap_or_else(|| panic!("no error for {code}"));
            assert!(error.starts_with("SyntaxError"), "{code}: {error}");
            assert!(render(&outcome, &limits()).contains("plain JavaScript, without type annotations"), "{code}");
        }
    }

    #[test]
    fn a_syntax_error_that_is_not_typescript_gets_no_hint() {
        let outcome = go_alone("const rows = [1, 2;\nreturn rows;");
        assert!(outcome.error.as_deref().is_some_and(|error| error.starts_with("SyntaxError")), "{:?}", outcome.error);
        assert!(!render(&outcome, &limits()).contains("type annotations"));
        // A runtime error is never given it, even in a program that is typed like TypeScript in its strings.
        let outcome = go_alone("const note = 'let x: number'; null.x; return note;");
        assert!(!render(&outcome, &limits()).contains("type annotations"));
    }

    #[test]
    fn the_error_line_is_the_bodys_own_line() {
        let outcome = go_alone("const a = 1;\nconst b = 2;\nnull.x;\nreturn a + b;");
        let error = outcome.error.expect("the error");
        assert!(error.starts_with("TypeError"), "{error}");
        assert!(error.ends_with("(line 3)"), "{error}");
        let outcome = go_alone("return 1; null.x;");
        assert!(outcome.error.is_none());
        let outcome = go_alone("null.x;");
        assert!(outcome.error.expect("the error").ends_with("(line 1)"));
        // A call's failure names the line of the call.
        let outcome = go("const a = 1;\n\nawait tools.t({});", &["t"], |_, _| Err("bad".to_string()));
        assert!(outcome.error.expect("the error").ends_with("(line 3)"));
        // So does a syntax error.
        let outcome = go_alone("const a = 1;\nconst b = ;\n");
        assert!(outcome.error.expect("the error").ends_with("(line 2)"));
    }

    #[test]
    fn render_cuts_long_output_keeping_head_and_tail() {
        let limits = Limits { output: 400, ..limits() };
        let outcome = Outcome { value: Some(format!("HEAD{}TAIL", "x".repeat(5000))), ..Outcome::default() };
        let text = render(&outcome, &limits);
        assert!(text.starts_with("HEAD"), "{text}");
        assert!(text.ends_with("TAIL"), "{text}");
        assert!(text.contains("truncated"), "{text}");
        assert!(text.len() < 400 + 100, "{} bytes", text.len());
        // At a char boundary, whatever the characters are.
        let outcome = Outcome { value: Some("あ".repeat(500)), ..Outcome::default() };
        let text = render(&outcome, &limits);
        assert!(text.contains("truncated") && text.starts_with('あ') && text.ends_with('あ'));
    }

    #[test]
    fn render_gives_the_value_then_the_log_then_the_error_and_the_stop() {
        let outcome = Outcome {
            value: Some("42".to_string()),
            log: "line one".to_string(),
            error: Some("TypeError: x (line 2)".to_string()),
            stopped: Some(Stop::Calls),
            ..Outcome::default()
        };
        let text = render(&outcome, &limits());
        let (value, log, error, stopped) = (text.find("42"), text.find("line one"), text.find("TypeError: x (line 2)"), text.find("tool calls"));
        assert!(value < log && log < error && error < stopped, "{text}");
        assert!(text.contains("limit of 50"), "{text}");
        // A short result is as it was.
        assert_eq!(render(&Outcome { value: Some("short".to_string()), ..Outcome::default() }, &limits()), "short");
    }

    #[test]
    fn the_program_has_no_file_network_process_or_clock_of_its_own() {
        let outcome = go_alone(
            "return ['require', 'std', 'os', 'process', 'fetch', 'XMLHttpRequest', 'setTimeout', 'setInterval', 'import', '__host', 'host', 'write', 'scriptArgs']\
             .map((name) => name + ':' + typeof globalThis[name]).join(' ');",
        );
        let value = outcome.value.expect("the value");
        for name in value.split(' ') {
            assert!(name.ends_with(":undefined"), "{name} is there: {value}");
        }
        let outcome = go_alone("try { await import('std'); return 'imported'; } catch (error) { return 'refused'; }");
        assert_eq!(outcome.value.as_deref(), Some("refused"), "{:?}", outcome.error);
        let outcome = go_alone("return String(typeof tools) + ' ' + String(Object.isFrozen(tools));");
        assert_eq!(outcome.value.as_deref(), Some("object true"));
    }

    #[test]
    fn a_runaway_recursion_is_an_error_and_not_a_crash() {
        let outcome = go_alone("function down(n) { return down(n + 1) + 1; }\nreturn down(0);");
        let error = outcome.error.expect("the error");
        assert!(error.contains("stack"), "{error}");
        assert_eq!(outcome.stopped, None);
    }

    #[test]
    fn a_program_that_waits_on_a_promise_nothing_settles_is_an_error() {
        let outcome = go_alone("await new Promise(() => {});\nreturn 1;");
        let error = outcome.error.expect("the error");
        assert!(error.contains("never finished"), "{error}");
    }

    #[test]
    fn a_thrown_value_that_is_not_an_error_is_shown() {
        assert_eq!(go_alone("throw 'plain words';").error.as_deref(), Some("plain words"));
        assert_eq!(go_alone("throw { code: 3 };").error.as_deref(), Some(r#"{"code":3}"#));
    }

    #[test]
    fn runs_do_not_share_an_engine() {
        let first = go_alone("globalThis.kept = 1; return 'set';");
        assert_eq!(first.value.as_deref(), Some("set"));
        let second = go_alone("return typeof globalThis.kept;");
        assert_eq!(second.value.as_deref(), Some("undefined"));
    }

    #[test]
    fn a_time_the_model_asks_for_is_held_between_a_second_and_ten_minutes() {
        let asked = |seconds: Option<f64>| Limits::default().with_timeout_seconds(seconds).time;
        assert_eq!(asked(None), Duration::from_secs(120));
        assert_eq!(asked(Some(30.0)), Duration::from_secs(30));
        assert_eq!(asked(Some(10_000.0)), Duration::from_secs(600));
        assert_eq!(asked(Some(0.0)), Duration::from_secs(1));
        assert_eq!(asked(Some(-5.0)), Duration::from_secs(1));
        assert_eq!(asked(Some(f64::NAN)), Duration::from_secs(120));
        assert_eq!(asked(Some(f64::INFINITY)), Duration::from_secs(120));
        // A time set past the most is held to it by the run itself.
        let limits = Limits { time: Duration::from_secs(5000), ..limits() };
        assert!(run("return 1;", &[], &limits, &AtomicBool::new(false), &mut |_, _| panic!("no call")).value.is_some());
    }

    fn tool(name: &str, description: Option<&str>, parameters: serde_json::Value) -> serde_json::Value {
        let mut function = json!({"name": name, "parameters": parameters});
        if let Some(description) = description {
            function["description"] = json!(description);
        }
        json!({"type": "function", "function": function})
    }

    #[test]
    fn the_rules_that_push_for_programs_differ_only_in_when_and_keep_the_same_tools() {
        let offered = json!([tool("read_file", Some("Read a text file."), json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}))]);
        let lines = |eager: bool| schema_with(&offered, eager)["function"]["description"].as_str().unwrap().lines().map(str::to_string).collect::<Vec<_>>();
        let (plain, eager) = (lines(false), lines(true));
        assert!(plain[0].contains("three or more tool calls") && eager[0].contains("two or more calls"), "{plain:?} {eager:?}");
        assert_eq!(plain[1..], eager[1..]);
        // (the default is the plain one: the switch is only read when it says 1)
        assert_eq!(schema(&offered), schema_with(&offered, std::env::var(EAGER_VARIABLE).is_ok_and(|value| value.trim() == "1")));
    }

    #[test]
    fn a_tool_is_declared_as_a_program_calls_it_with_the_first_sentence_of_what_it_is() {
        let offered = json!([
            tool("read_file", Some("Read a text file.  Then more."), json!({"type": "object", "properties": {"path": {"type": "string"}, "limit": {"type": "integer"}}, "required": ["path"]})),
            tool("todo", Some("A checklist.\nSend only what changed."), json!({"type": "object", "properties": {
                "todos": {"type": "array", "items": {"type": "object", "properties": {"id": {"type": "string"}, "status": {"type": "string", "enum": ["pending", "done"]}}, "required": ["id"]}},
                "merge": {"type": "boolean"}
            }, "required": ["todos"]})),
            tool("show_diff", None, json!({"type": "object", "properties": {"scope": {"type": "string", "enum": ["session", "staged"]}, "paths": {"type": "array", "items": {"type": "string"}}, "depth": {"type": "array", "items": {"type": "integer"}}}, "required": ["scope"]})),
            tool("mcp__a-b__c.d", Some("Version 1.2 of it is here."), json!({"type": "object", "properties": {}})),
            tool("mcp__s__t", None, json!(null)),
        ]);
        let description = schema(&offered)["function"]["description"].as_str().unwrap().to_string();
        let lines: Vec<&str> = description.lines().collect();
        assert!(lines[0].starts_with("Run a short JavaScript program"), "{description}");
        assert_eq!(lines[1], "tools.read_file({path, limit?}) — Read a text file.");
        assert_eq!(lines[2], "tools.todo({todos: [{id, status?: \"pending\"|\"done\"}], merge?}) — A checklist.");
        assert_eq!(lines[3], "tools.show_diff({scope: \"session\"|\"staged\", paths?: [string], depth?: [number]})");
        assert_eq!(lines[4], "tools[\"mcp__a-b__c.d\"]({}) — Version 1.2 of it is here.");
        assert_eq!(lines[5], "tools.mcp__s__t({})");
        assert_eq!(lines.len(), 6);
    }

    #[test]
    fn a_program_is_told_of_the_tools_it_can_call_and_not_of_those_it_cannot() {
        let offered = json!([
            tool("read_file", Some("Read."), json!({"type": "object", "properties": {}})),
            tool("ask_user", Some("Ask."), json!({"type": "object", "properties": {}})),
            tool("run_code", Some("Run."), json!({"type": "object", "properties": {}})),
            tool("explore", Some("Look."), json!({"type": "object", "properties": {}})),
            tool("wake_at", Some("Wake."), json!({"type": "object", "properties": {}})),
            tool("submit_plan", Some("Plan."), json!({"type": "object", "properties": {}})),
            tool("web_read", Some("Read a page."), json!({"type": "object", "properties": {}})),
        ]);
        assert_eq!(callable(&offered), ["read_file", "web_read"]);
        let description = schema(&offered)["function"]["description"].as_str().unwrap().to_string();
        assert_eq!(description.lines().skip(1).collect::<Vec<_>>(), ["tools.read_file({}) — Read.", "tools.web_read({}) — Read a page."]);
        assert!(callable(&json!(null)).is_empty());
    }

    #[test]
    fn a_first_sentence_is_one_line_and_not_longer_than_it_may_be() {
        assert_eq!(first_sentence("List a folder. {} is the workspace."), "List a folder.");
        assert_eq!(first_sentence("Version 1.2 is out, and more."), "Version 1.2 is out, and more.");
        assert_eq!(first_sentence("No full stop"), "No full stop");
        assert_eq!(first_sentence("フォルダーを見る。次の文。"), "フォルダーを見る。");
        assert_eq!(first_sentence(&"x ".repeat(200)).chars().count(), ABOUT_MOST);
        assert!(first_sentence(&"x ".repeat(200)).ends_with('…'));
    }
}
