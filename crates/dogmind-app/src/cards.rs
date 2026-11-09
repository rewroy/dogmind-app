//! The cards of the conversation that ask the person something or show something of the work: the model's question with its choices. What is on it, in each state, and what a session that is
//! opened again has of it. Decisions only, with no window in them, so that a test can ask them: `ui.rs` draws what they answer.

use std::collections::HashMap;

use dogmind_agent::agent::{self, HelperEnd, PlanOutcome};
use dogmind_agent::message::{Message, Wake, WakeCause};
use dogmind_agent::tools::FileChange;
use serde_json::Value;

use crate::lang::{t, tf};

/// Where a question stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerState {
    /// It waits for the person: its choices are buttons.
    Waiting,
    /// What the person answered, a choice's label or their own words.
    Answered(String),
    /// The turn was stopped, or the question closed, before it was answered.
    Unanswered,
}

/// Where a plan stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanState {
    /// It waits for the person: it can be carried out, or changed.
    Waiting,
    Approved,
    /// What the person asked to be changed.
    Changes(String),
    /// Stopped, or closed, before it was decided.
    Undecided,
}

/// One file of the card of an answer's changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    /// Whether there is a diff to show: a file that is binary, or too large to be read, has none.
    pub has_diff: bool,
}

/// How many of the things an `explore` helper read or searched its card shows at a time.
pub const RECENT: usize = 3;

/// Where an `explore` helper stands, as its card says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperState {
    /// At work: in which of its requests, of the most it has (none are counted before its first call); what it read or searched last, the latest last; what its context has come to in tokens;
    /// how long it had worked when the agent last said (ms); and whether the person has pressed Stop and the helper has not yet ended.
    Working { step: u32, most: u32, recent: Vec<String>, tokens: u64, ms: u64, stopping: bool },
    /// It answered.
    Done { steps: u32, files: u32, tokens: u64, ms: u64 },
    /// The person stopped it.
    Stopped { steps: u32, files: u32, tokens: u64, ms: u64 },
    /// It could not go on, and why.
    Failed { why: String, steps: u32, ms: u64 },
}

/// What a card of the transcript is: it is kept with the rest of what the transcript is written from, and written again, in whatever state it has come to, as the window changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Card {
    Question { id: u64, question: String, options: Vec<(String, String)>, state: AnswerState },
    Plan { id: u64, plan: String, state: PlanState },
    /// The files turn `turn`'s edits changed: as many as the card lists, how many more there are, and the lines added and removed in all of them.
    Files { turn: u64, rows: Vec<FileRow>, more: usize, added: usize, removed: usize },
    /// What woke the session with nobody writing (a background command ended, a line it was to wake for, a wait): the app's notice, in a row of its own and not a bubble of the person's.
    /// `at`: when it came, as the transcript says times.
    Notice { wakes: Vec<Wake>, at: String },
    /// An `explore` helper: the call it is of, its task as a line, which of the session's helper files holds its steps (none when they were not kept), and where it stands.
    Helper { call: String, task: String, number: Option<u32>, state: HelperState },
}

/// How many files a card lists: the rest are counted, and the Changes page has all of them.
pub const MOST_FILES: usize = 8;

/// The lines a notice row has for one thing that woke the session: what it was, which one, and how it went. `failed`: it went wrong (an exit code, a time out), and the line says so in a colour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeRow {
    pub title: String,
    pub what: String,
    pub how: String,
    pub failed: bool,
}

/// A length of time in the words of the window: `45 s`, `1 min 31 s`, `2 h 5 min`.
pub fn duration_words(seconds: u64) -> String {
    match seconds {
        0..=59 => tf("{0} s", &[&seconds]),
        60..=3599 => tf("{0} min {1} s", &[&(seconds / 60), &(seconds % 60)]),
        _ => tf("{0} h {1} min", &[&(seconds / 3600), &(seconds % 3600 / 60)]),
    }
}

/// What a notice row says of one thing that woke the session.
pub fn notice_row(wake: &Wake) -> NoticeRow {
    let took = duration_words(wake.seconds);
    match wake.cause {
        WakeCause::CommandEnded => {
            let (how, failed) = match (wake.exit_code, wake.ended.as_deref()) {
                (Some(code), _) => (tf("Exit code {0}  ·  ran {1}", &[&code, &took]), code != 0),
                (None, Some("timed out")) => (tf("Timed out  ·  ran {0}", &[&took]), true),
                (None, Some("cancelled")) => (tf("Cancelled  ·  ran {0}", &[&took]), true),
                (None, Some("not run")) => (t("Did not run").to_string(), true),
                (None, _) => (tf("Ended  ·  ran {0}", &[&took]), false),
            };
            NoticeRow { title: t("A background command ended").to_string(), what: format!("{}  ·  {}", wake.id, wake.what), how, failed }
        }
        WakeCause::CommandLine => NoticeRow { title: t("A background command printed the line it was watched for").to_string(), what: format!("{}  ·  {}", wake.id, wake.what), how: tf("Running for {0}, and going on", &[&took]), failed: false },
        WakeCause::Wait => NoticeRow { title: t("A wait is up").to_string(), what: wake.what.clone(), how: tf("Set for {0}", &[&took]), failed: false },
    }
}

impl Card {
    /// The number a card has, which says which it is among those of the transcript.
    pub fn id(&self) -> Option<u64> {
        match self {
            Card::Question { id, .. } | Card::Plan { id, .. } => Some(*id),
            Card::Files { .. } | Card::Notice { .. } | Card::Helper { .. } => None,
        }
    }

    /// The call a helper's card is of.
    pub fn helper_call(&self) -> Option<&str> {
        match self {
            Card::Helper { call, .. } => Some(call),
            _ => None,
        }
    }

    /// The card of a helper that has begun.
    pub fn helper_begun(call: &str, task: &str, number: Option<u32>) -> Card {
        Card::Helper { call: call.to_string(), task: task.to_string(), number, state: HelperState::Working { step: 0, most: 0, recent: Vec::new(), tokens: 0, ms: 0, stopping: false } }
    }

    /// The helper called a tool: the request it is on, what it is on, what its context has come to, and how long it has worked.
    pub fn helper_step(&mut self, step: u32, most: u32, tool: &str, summary: &str, tokens: u64, ms: u64) {
        let Card::Helper { state: HelperState::Working { step: now, most: of, recent, tokens: used, ms: worked, .. }, .. } = self else { return };
        (*now, *of, *used, *worked) = (step, most, tokens, ms);
        let line = if summary.trim().is_empty() { t(crate::ui::tool_name(tool)).to_string() } else { format!("{}  {}", t(crate::ui::tool_name(tool)), summary.trim()) };
        recent.push(line.lines().next().unwrap_or("").to_string());
        let extra = recent.len().saturating_sub(RECENT);
        recent.drain(..extra);
    }

    /// Stop has been pressed: the card says so until the helper ends.
    pub fn helper_stopping(&mut self) {
        if let Card::Helper { state: HelperState::Working { stopping, .. }, .. } = self {
            *stopping = true;
        }
    }

    /// The helper has ended. `number`: its file, where it is known by now.
    pub fn helper_ended(&mut self, how: &HelperEnd, steps: u32, files: u32, tokens: u64, ms: u64, number: Option<u32>) {
        let Card::Helper { number: kept, state, .. } = self else { return };
        *kept = number.or(*kept);
        *state = match how {
            HelperEnd::Answered => HelperState::Done { steps, files, tokens, ms },
            HelperEnd::Stopped => HelperState::Stopped { steps, files, tokens, ms },
            HelperEnd::Failed(why) => HelperState::Failed { why: why.clone(), steps, ms },
        };
    }

    /// A helper that is still at work when its turn has ended is not: it was stopped.
    pub fn helper_settled(&mut self) -> bool {
        let Card::Helper { state: state @ HelperState::Working { .. }, .. } = self else { return false };
        let HelperState::Working { step, tokens, ms, .. } = state.clone() else { return false };
        *state = HelperState::Stopped { steps: step, files: 0, tokens, ms };
        true
    }

    /// The card of a call of `explore` that a session kept, from the call's result: how it went comes from the record the result holds of its helper, which a result from before there was one
    /// does not have (then there is no card, and the call is a row of the calls as any is).
    pub fn of_helper_call(name: &str, call: &str, arguments: &Value, result: Option<&Message>) -> Option<Card> {
        if name != "explore" {
            return None;
        }
        let result = result?;
        let helper = result.helper.as_ref()?;
        let task = dogmind_agent::tools::summary("explore", arguments);
        let words = result.content.as_deref().unwrap_or("");
        let state = if helper.stopped {
            HelperState::Stopped { steps: helper.steps, files: helper.files, tokens: helper.tokens, ms: helper.ms }
        } else if let Some(why) = words.strip_prefix("error: ") {
            HelperState::Failed { why: why.lines().next().unwrap_or("").to_string(), steps: helper.steps, ms: helper.ms }
        } else {
            HelperState::Done { steps: helper.steps, files: helper.files, tokens: helper.tokens, ms: helper.ms }
        };
        Some(Card::Helper { call: call.to_string(), task, number: Some(helper.number), state })
    }

    /// The card of the files turn `turn`'s edits changed.
    pub fn files(turn: u64, changes: &[FileChange]) -> Card {
        let rows = changes.iter().take(MOST_FILES).map(|change| FileRow { path: change.path.clone(), added: change.added, removed: change.removed, has_diff: !change.diff.is_empty() }).collect();
        let (added, removed) = changes.iter().fold((0, 0), |(added, removed), change| (added + change.added, removed + change.removed));
        Card::Files { turn, rows, more: changes.len().saturating_sub(MOST_FILES), added, removed }
    }

    /// The card of a call of `ask_user` or `submit_plan` that a session kept. `result` is what the call returned: none for a call that has no result, which is one still waiting (the window is told of it as it
    /// happens) and is no card here. None for any other call.
    pub fn of_call(id: u64, name: &str, arguments: &Value, result: Option<&str>) -> Option<Card> {
        match name {
            "ask_user" => {
                let question = arguments.get("question").and_then(Value::as_str).map(str::trim).filter(|question| !question.is_empty())?.to_string();
                let options = options_of(arguments);
                let state = match agent::answer_in(result?) {
                    Some(answer) => AnswerState::Answered(answer.to_string()),
                    None => AnswerState::Unanswered,
                };
                Some(Card::Question { id, question, options, state })
            }
            "submit_plan" => {
                let plan = arguments.get("plan").and_then(Value::as_str).map(str::trim).filter(|plan| !plan.is_empty())?.to_string();
                let state = match agent::plan_outcome(result?) {
                    PlanOutcome::Approved => PlanState::Approved,
                    PlanOutcome::Changes(change) => PlanState::Changes(change),
                    PlanOutcome::Undecided => PlanState::Undecided,
                };
                Some(Card::Plan { id, plan, state })
            }
            _ => None,
        }
    }
}

/// The cards a session's messages hold: one for each call of `ask_user` or `submit_plan` that has a result, and each of `explore` whose result records its helper, in order, with its call's id and the number its card has (from `first_id`).
pub fn of_messages(messages: &[Message], first_id: u64) -> Vec<(String, Card)> {
    let results: HashMap<&str, &Message> = messages.iter().filter_map(|message| Some((message.tool_call_id.as_deref()?, message))).collect();
    let mut found = Vec::new();
    for call in messages.iter().filter(|message| message.role == "assistant").flat_map(|message| message.tool_calls.iter().flatten()) {
        let arguments = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
        let result = results.get(call.id.as_str()).copied();
        let card = Card::of_call(first_id + found.len() as u64, &call.function.name, &arguments, result.map(|message| message.content.as_deref().unwrap_or("")))
            .or_else(|| Card::of_helper_call(&call.function.name, &call.id, &arguments, result));
        if let Some(card) = card {
            found.push((call.id.clone(), card));
        }
    }
    found
}

/// A count of tokens in the words of the window: `850`, `14.2k`.
pub fn token_count_words(tokens: u64) -> String {
    if tokens < 1000 {
        tokens.to_string()
    } else {
        format!("{:.1}k", tokens as f64 / 1000.0)
    }
}

fn steps_words(steps: u32) -> String {
    if steps == 1 { t("1 step").to_string() } else { tf("{0} steps", &[&steps]) }
}

fn files_read_words(files: u32) -> String {
    if files == 1 { t("1 file read").to_string() } else { tf("{0} files read", &[&files]) }
}

/// What a working helper's status row says, its words and then its figures, `since` milliseconds after the agent last said how it was going: "Step 7 of 20", and "1 min 12 s  ·  14.2k tokens".
pub fn helper_status(state: &HelperState, since: u64) -> (String, String) {
    let HelperState::Working { step, most, tokens, ms, stopping, .. } = state else { return (String::new(), String::new()) };
    let time = duration_words((ms + since) / 1000);
    let figures = if *tokens > 0 { format!("{time}  ·  {}", tf("{0} tokens", &[&token_count_words(*tokens)])) } else { time };
    let left = if *stopping {
        t("Stopping…").to_string()
    } else if *step == 0 {
        t("Starting…").to_string()
    } else {
        tf("Step {0} of {1}", &[step, most])
    };
    (left, figures)
}

/// The latest things a working helper read or searched, the oldest first, in as many rows as its card keeps (the rest empty).
pub fn helper_recent(state: &HelperState) -> [String; RECENT] {
    let mut rows: [String; RECENT] = Default::default();
    if let HelperState::Working { recent, .. } = state {
        for (row, line) in rows.iter_mut().zip(recent) {
            row.clone_from(line);
        }
    }
    rows
}

/// How a helper that is over came out, in a line: what it says and whether it went well.
pub fn helper_outcome(state: &HelperState) -> Option<(String, bool)> {
    match state {
        HelperState::Working { .. } => None,
        HelperState::Done { steps, files, ms, .. } => Some((tf("Done  ·  {0}  ·  {1}  ·  {2}", &[&duration_words(ms / 1000), &steps_words(*steps), &files_read_words(*files)]), true)),
        HelperState::Stopped { steps, files, ms, .. } => Some((tf("Stopped by you  ·  {0}  ·  {1}  ·  {2}", &[&duration_words(ms / 1000), &steps_words(*steps), &files_read_words(*files)]), false)),
        HelperState::Failed { why, .. } => Some((tf("Could not finish: {0}", &[&crate::lang::notice(why)]), false)),
    }
}

/// The choices a call of `ask_user` gives, as the agent reads them: a label, and what it means.
fn options_of(arguments: &Value) -> Vec<(String, String)> {
    arguments.get("options").and_then(Value::as_array).into_iter().flatten().filter_map(|option| {
        let label = option.get("label").and_then(Value::as_str).or_else(|| option.as_str())?.trim();
        (!label.is_empty()).then(|| (label.to_string(), option.get("description").and_then(Value::as_str).unwrap_or("").trim().to_string()))
    }).take(6).collect()
}

/// Whether a tool is one whose call is a card of its own, and so has no row among the calls.
pub fn is_asking(tool: &str) -> bool {
    matches!(tool, "ask_user" | "submit_plan")
}

/// Whether what was typed while a plan waits approves it. Anything else is what to change.
pub fn approves(typed: &str) -> bool {
    matches!(typed.trim().trim_end_matches(['.', '!', '。', '！']).to_lowercase().as_str(), "yes" | "y" | "ok" | "go" | "はい" | "うん" | "承認" | "実行" | "お願い")
}

/// Where the card of a turn's files goes in what a session opened again was written as. `slots`: for each of the person's messages that was shown, its place in the message list and the
/// place in what was written before it. The card goes before the words that come after the turn's own, or at `end`. None for a turn whose words are not in what was shown.
pub fn place_of_turn(slots: &[(usize, usize)], end: usize, message: usize) -> Option<usize> {
    slots.iter().any(|(at, _)| *at == message).then(|| slots.iter().find(|(at, _)| *at > message).map_or(end, |(_, place)| *place))
}

/// Where in what was written the card of turn `turn`, whose place was `slot` when none of the cards was in, goes now that those of `came` (each its own `(slot, turn)`) are in: after the
/// cards that stood at an earlier place or, at its own place, are of an earlier turn, and before the others. So that the replies can come in any order and the cards are in theirs.
pub fn card_position(came: &[(usize, u64)], slot: usize, turn: u64) -> usize {
    slot + came.iter().filter(|other| **other < (slot, turn)).count()
}

/// What `/goal` with this word after it does, the session having this goal.
#[derive(Debug, PartialEq, Eq)]
pub enum GoalAction {
    /// Make this the goal.
    Set(String),
    /// There is a goal: it goes.
    Clear,
    /// Said alone with a goal set: the goal, in full, in the composer after `/goal`, to read or to change and send.
    Edit(String),
    /// Nothing to show or to clear: the way to set one is said.
    None,
}

pub fn goal_action(word: &str, goal: Option<&str>) -> GoalAction {
    let goal = goal.map(str::trim).filter(|goal| !goal.is_empty());
    match word.trim() {
        "" => goal.map_or(GoalAction::None, |goal| GoalAction::Edit(goal.to_string())),
        "clear" | "off" | "none" | "なし" | "消す" => if goal.is_some() { GoalAction::Clear } else { GoalAction::None },
        set => GoalAction::Set(set.to_string()),
    }
}

/// How many parts a file's bar has: as many as its length in whole, however many lines it changed.
pub const BAR_PARTS: usize = 8;

/// A file's bar: how many of its parts are added lines (green) and how many removed (red), against the file of the card that changed most. A file that changed at all has a part, and
/// one that changed a tenth of the most has no more than that.
pub fn bar(added: usize, removed: usize, most: usize) -> (usize, usize) {
    let total = added + removed;
    if total == 0 || most == 0 {
        return (0, 0);
    }
    let parts = (total * BAR_PARTS).div_ceil(most).clamp(1, BAR_PARTS);
    let green = if parts == 1 {
        usize::from(added >= removed)
    } else if added > 0 && removed > 0 {
        // (lines of both kinds show both)
        ((added * parts + total / 2) / total).clamp(1, parts - 1)
    } else {
        (added * parts) / total
    };
    (green, parts - green)
}

/// The most lines one file of the rows changed.
pub fn most_changed(rows: &[FileRow]) -> usize {
    rows.iter().map(|row| row.added + row.removed).max().unwrap_or(0)
}

/// Whether a file of the card is opened with its own app. What is a program or a script is not: opened, it would run, and what the agent wrote is not for that. These are shown in Explorer.
pub fn opens_in_app(path: &str) -> bool {
    const RUNS: [&str; 30] = [
        "exe", "com", "bat", "cmd", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "msi", "msp", "scr", "lnk", "url", "reg", "hta", "cpl", "jar", "pif", "appx", "msix", "dll", "sys", "py", "pyw", "sh", "gadget",
    ];
    let extension = path.rsplit(['/', '\\']).next().and_then(|name| name.rsplit_once('.')).map_or(String::new(), |(_, extension)| extension.to_lowercase());
    !RUNS.contains(&extension.as_str())
}

/// The words of the bar under the session's title: its goal's first line. None when there is no goal, and no bar.
pub fn goal_bar(goal: Option<&str>) -> Option<String> {
    goal.and_then(|goal| goal.lines().map(str::trim).find(|line| !line.is_empty())).map(str::to_string)
}

/// What goes back to the model for what was typed while a question waits: a choice's number stands for the choice, anything else is the answer as it was written.
pub fn answer_text(labels: &[String], typed: &str) -> String {
    typed.trim().parse::<usize>().ok().and_then(|number| labels.get(number.wrapping_sub(1))).cloned().unwrap_or_else(|| typed.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, arguments: Value, result: Option<&str>) -> Option<Card> {
        Card::of_call(1, name, &arguments, result)
    }

    #[test]
    fn a_question_a_session_kept_comes_back_with_its_answer() {
        let ask = json!({"question": "Which word?", "options": [{"label": "hello", "description": "the long one"}, "hi"]});
        let card = call("ask_user", ask.clone(), Some("the person answered: hello")).unwrap();
        assert_eq!(card, Card::Question { id: 1, question: "Which word?".into(), options: vec![("hello".into(), "the long one".into()), ("hi".into(), String::new())], state: AnswerState::Answered("hello".into()) });
        // (an answer in the person's own words is as good)
        let own = call("ask_user", ask.clone(), Some("the person answered: neither, hey")).unwrap();
        assert!(matches!(own, Card::Question { state: AnswerState::Answered(words), .. } if words == "neither, hey"));
        // (stopped before it was answered: the card says so)
        let stopped = call("ask_user", ask.clone(), Some("error: the user stopped the turn before answering")).unwrap();
        assert!(matches!(stopped, Card::Question { state: AnswerState::Unanswered, .. }));
        // (no result: the question is still waiting, and the window is told of it as it is asked)
        assert_eq!(call("ask_user", ask, None), None);
        assert_eq!(call("ask_user", json!({"options": ["a", "b"]}), Some("the person answered: a")), None, "no question, no card");
        assert_eq!(call("read_file", json!({"path": "a"}), Some("x")), None, "no other call is a card");
    }

    fn asking(id: &str, name: &str, arguments: Value) -> Message {
        use dogmind_agent::message::{FunctionCall, ToolCall};
        Message { role: "assistant".into(), tool_calls: Some(vec![ToolCall { id: id.into(), kind: "function".into(), function: FunctionCall { name: name.into(), arguments: arguments.to_string() } }]), ..Message::default() }
    }

    #[test]
    fn a_session_opened_again_shows_each_question_with_what_came_of_it() {
        let messages = vec![
            Message::text("user", "set up the notes"),
            asking("q1", "ask_user", json!({"question": "Which name?", "options": ["notes.md", "notes.txt"]})),
            Message::tool_result("q1", "the person answered: notes.txt"),
            asking("r1", "read_file", json!({"path": "a"})),
            Message::tool_result("r1", "text"),
            asking("q2", "ask_user", json!({"question": "Which folder?", "options": ["a", "b"]})),
            Message::tool_result("q2", "error: the user stopped the turn before answering"),
            asking("q3", "ask_user", json!({"question": "Which size?", "options": ["s", "m"]})),
            // (no result: a question still waiting is not one of the cards a file holds)
        ];
        let found = of_messages(&messages, 7);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].0, "q1");
        assert_eq!(found[0].1, Card::Question { id: 7, question: "Which name?".into(), options: vec![("notes.md".into(), String::new()), ("notes.txt".into(), String::new())], state: AnswerState::Answered("notes.txt".into()) });
        assert!(matches!(&found[1], (id, Card::Question { id: 8, state: AnswerState::Unanswered, .. }) if id == "q2"));
    }

    #[test]
    fn a_plan_a_session_kept_comes_back_with_how_it_came_out() {
        let plan = json!({"plan": "1. Add a page\n2. Link it"});
        let kept = |result: &str| call("submit_plan", plan.clone(), Some(result)).unwrap();
        let state = |card: Card| match card {
            Card::Plan { state, plan, .. } => {
                assert_eq!(plan, "1. Add a page\n2. Link it");
                state
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(state(kept(agent::PLAN_APPROVED)), PlanState::Approved);
        assert_eq!(state(kept(&format!("{}use a modal", agent::PLAN_CHANGE))), PlanState::Changes("use a modal".into()));
        assert_eq!(state(kept("error: the user stopped the turn before deciding on the plan")), PlanState::Undecided);
        assert_eq!(call("submit_plan", plan.clone(), None), None, "a plan still waiting is told as it happens");
        assert_eq!(call("submit_plan", json!({"plan": "  "}), Some(agent::PLAN_APPROVED)), None, "no plan, no card");
    }

    #[test]
    fn a_session_opened_again_shows_its_plan_beside_its_questions_in_order() {
        let messages = vec![
            Message::text("user", "add a page"),
            asking("p1", "submit_plan", json!({"plan": "1. Write it"})),
            Message::tool_result("p1", &format!("{}add a title", agent::PLAN_CHANGE)),
            asking("p2", "submit_plan", json!({"plan": "1. Write it, with a title"})),
            Message::tool_result("p2", agent::PLAN_APPROVED),
        ];
        let found = of_messages(&messages, 1);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0], ("p1".to_string(), Card::Plan { id: 1, plan: "1. Write it".into(), state: PlanState::Changes("add a title".into()) }));
        assert_eq!(found[1], ("p2".to_string(), Card::Plan { id: 2, plan: "1. Write it, with a title".into(), state: PlanState::Approved }));
    }

    #[test]
    fn a_plan_is_approved_by_a_yes_and_anything_else_asks_for_a_change() {
        for yes in ["yes", "Yes.", " OK ", "はい", "実行！", "go"] {
            assert!(approves(yes), "{yes}");
        }
        for change in ["use a modal", "no", "yes but smaller", ""] {
            assert!(!approves(change), "{change}");
        }
    }

    #[test]
    fn the_card_of_an_answers_files_lists_a_few_and_counts_the_rest() {
        let change = |path: &str, added: usize, removed: usize| FileChange { path: path.into(), added, removed, diff: if added + removed == 0 { String::new() } else { "@@\n+x\n".into() } };
        let few = Card::files(3, &[change("a.rs", 4, 1), change("logo.png", 0, 0)]);
        assert_eq!(few, Card::Files { turn: 3, rows: vec![FileRow { path: "a.rs".into(), added: 4, removed: 1, has_diff: true }, FileRow { path: "logo.png".into(), added: 0, removed: 0, has_diff: false }], more: 0, added: 4, removed: 1 });
        let many: Vec<FileChange> = (0..MOST_FILES + 5).map(|at| change(&format!("f{at}.txt"), 2, 1)).collect();
        match Card::files(4, &many) {
            Card::Files { rows, more, added, removed, .. } => assert_eq!((rows.len(), more, added, removed), (MOST_FILES, 5, 2 * (MOST_FILES + 5), MOST_FILES + 5)),
            other => panic!("{other:?}"),
        }
        assert_eq!(few.id(), None, "a card of files is told by its turn");
    }

    #[test]
    fn the_card_of_a_turns_files_goes_where_its_answer_ended_in_a_session_opened_again() {
        // the person's words at message 1, 5 and 9, written at places 0, 10 and 20 of what was shown, which ends at 30
        let slots = [(1, 0), (5, 10), (9, 20)];
        assert_eq!(place_of_turn(&slots, 30, 1), Some(10), "before the next person's words");
        assert_eq!(place_of_turn(&slots, 30, 5), Some(20));
        assert_eq!(place_of_turn(&slots, 30, 9), Some(30), "the last turn's goes at the end");
        assert_eq!(place_of_turn(&slots, 30, 3), None, "its words are not in what was shown: a compaction took them");
        assert_eq!(place_of_turn(&[], 0, 1), None);
    }

    #[test]
    fn a_file_bar_says_how_much_changed_and_of_what_kind() {
        assert_eq!(bar(0, 0, 10), (0, 0), "nothing changed, no bar");
        assert_eq!(bar(1, 0, 1), (BAR_PARTS, 0), "the file that changed most has the whole bar");
        assert_eq!(bar(5, 5, 10), (BAR_PARTS / 2, BAR_PARTS / 2));
        assert_eq!(bar(0, 40, 40), (0, BAR_PARTS), "only removed lines: all red");
        assert_eq!(bar(1, 0, 100), (1, 0), "a little change still shows");
        let (green, red) = bar(2, 1, 100);
        assert_eq!(green + red, 1, "a part is all a tenth of the most has");
        // (lines of both kinds show both when there is room for both)
        let (green, red) = bar(99, 1, 100);
        assert!(green >= 1 && red >= 1 && green + red == BAR_PARTS, "{green} {red}");
        assert_eq!(bar(3, 0, 0), (0, 0));
    }

    #[test]
    fn the_file_that_changed_most_sets_the_bars_scale() {
        let row = |added: usize, removed: usize| FileRow { path: "x".into(), added, removed, has_diff: true };
        assert_eq!(most_changed(&[row(2, 1), row(10, 4), row(1, 0)]), 14);
        assert_eq!(most_changed(&[]), 0);
    }

    #[test]
    fn a_program_the_agent_wrote_is_shown_in_explorer_and_not_run() {
        for safe in ["notes.md", "src/app.rs", "logo.PNG", "data.csv", "README", "dir\\report.pdf"] {
            assert!(opens_in_app(safe), "{safe}");
        }
        for risky in ["setup.exe", "run.BAT", "tools\\x.ps1", "a.vbs", "a.js", "a.lnk", "build.cmd", "pkg.msi", "x.dll", "run.py", "go.sh"] {
            assert!(!opens_in_app(risky), "{risky}");
        }
    }

    #[test]
    fn a_number_typed_for_a_question_picks_its_choice() {
        let labels: Vec<String> = ["hello", "hi"].iter().map(|label| label.to_string()).collect();
        assert_eq!(answer_text(&labels, "2"), "hi");
        assert_eq!(answer_text(&labels, " 1 "), "hello");
        assert_eq!(answer_text(&labels, "3"), "3", "past the choices it is a word like any other");
        assert_eq!(answer_text(&labels, "0"), "0");
        assert_eq!(answer_text(&labels, "neither, hey"), "neither, hey");
    }

    #[test]
    fn the_goal_bar_has_the_first_line_of_the_goal_and_is_not_there_without_one() {
        assert_eq!(goal_bar(Some("keep the folder to what I asked for")), Some("keep the folder to what I asked for".to_string()));
        assert_eq!(goal_bar(Some("
  first line  
second")), Some("first line".to_string()));
        assert_eq!(goal_bar(Some("  
 ")), None);
        assert_eq!(goal_bar(None), None);
    }

    #[test]
    fn a_card_goes_where_its_answer_ended_whatever_order_the_replies_come_in() {
        // (answers 1, 2 and 3 ended at places 2, 2 and 5 of what was written, and answer 4 at 0: a card each)
        let wanted = [(2usize, 1u64), (2, 2), (5, 3), (0, 4)];
        let mut orders = vec![vec![0usize, 1, 2, 3]];
        for a in 0..4 {
            for b in 0..4 {
                for c in 0..4 {
                    for d in 0..4 {
                        let order = vec![a, b, c, d];
                        if (0..4).all(|n| order.contains(&n)) {
                            orders.push(order);
                        }
                    }
                }
            }
        }
        for order in orders {
            let mut written: Vec<String> = (0..7).map(|n| format!("w{n}")).collect();
            let mut came: Vec<(usize, u64)> = Vec::new();
            for which in order.iter().copied() {
                let (slot, turn) = wanted[which];
                let at = card_position(&came, slot, turn);
                written.insert(at, format!("card{turn}"));
                came.push((slot, turn));
            }
            let text = written.join(" ");
            assert_eq!(text, "card4 w0 w1 card1 card2 w2 w3 w4 card3 w5 w6", "{order:?}");
        }
    }

    #[test]
    fn goal_with_a_word_after_it_sets_it_clears_it_or_shows_it_in_one_way_each() {
        assert_eq!(goal_action("ship the page", None), GoalAction::Set("ship the page".to_string()));
        assert_eq!(goal_action("ship the page", Some("old")), GoalAction::Set("ship the page".to_string()));
        assert_eq!(goal_action("clear", Some("old")), GoalAction::Clear);
        assert_eq!(goal_action("消す", Some("old")), GoalAction::Clear);
        // (nothing to clear, nothing to show: the way to set one is said, and nothing is sent)
        assert_eq!(goal_action("clear", None), GoalAction::None);
        assert_eq!(goal_action("", None), GoalAction::None);
        assert_eq!(goal_action("", Some("  ")), GoalAction::None);
        assert_eq!(goal_action("", Some("ship the page")), GoalAction::Edit("ship the page".to_string()));
    }

    fn woke(cause: WakeCause, exit_code: Option<i64>, ended: Option<&str>, seconds: u64) -> Wake {
        Wake { cause, id: if cause == WakeCause::Wait { "wait1".into() } else { "job2".into() }, what: if cause == WakeCause::Wait { "look at the deploy".into() } else { "cargo build --release".into() }, exit_code, ended: ended.map(str::to_string), seconds }
    }

    #[test]
    fn a_notice_row_says_what_woke_the_session_which_one_and_how_it_went() {
        let ok = notice_row(&woke(WakeCause::CommandEnded, Some(0), None, 91));
        assert_eq!((ok.title.as_str(), ok.what.as_str(), ok.how.as_str(), ok.failed), ("A background command ended", "job2  ·  cargo build --release", "Exit code 0  ·  ran 1 min 31 s", false));
        let bad = notice_row(&woke(WakeCause::CommandEnded, Some(101), None, 7));
        assert_eq!((bad.how.as_str(), bad.failed), ("Exit code 101  ·  ran 7 s", true));
        assert_eq!(notice_row(&woke(WakeCause::CommandEnded, None, Some("timed out"), 3600)).how, "Timed out  ·  ran 1 h 0 min");
        assert!(notice_row(&woke(WakeCause::CommandEnded, None, Some("cancelled"), 4)).failed);
        assert_eq!(notice_row(&woke(WakeCause::CommandEnded, None, Some("not run"), 0)).how, "Did not run");
        let line = notice_row(&woke(WakeCause::CommandLine, None, None, 12));
        assert!(line.title.contains("printed the line") && line.how == "Running for 12 s, and going on" && !line.failed, "{line:?}");
        let wait = notice_row(&woke(WakeCause::Wait, None, None, 1800));
        assert_eq!((wait.title.as_str(), wait.what.as_str(), wait.how.as_str()), ("A wait is up", "look at the deploy", "Set for 30 min 0 s"));
    }

    /// A notice is no one's words: a session that holds one opens with it as a row of its own, whatever the person said.
    #[test]
    fn a_notice_card_has_no_number_and_is_no_question() {
        let card = Card::Notice { wakes: vec![woke(WakeCause::Wait, None, None, 60)], at: "14:32".into() };
        assert_eq!(card.id(), None);
        assert!(!is_asking("wake_at"));
    }

    #[test]
    fn the_two_tools_that_ask_are_cards_and_no_other_is() {
        assert!(is_asking("ask_user") && is_asking("submit_plan"));
        assert!(!is_asking("read_file") && !is_asking("todo"));
    }

    fn working(card: &Card) -> (u32, u32, Vec<String>, u64, u64, bool) {
        match card {
            Card::Helper { state: HelperState::Working { step, most, recent, tokens, ms, stopping }, .. } => (*step, *most, recent.clone(), *tokens, *ms, *stopping),
            other => panic!("{other:?}"),
        }
    }

    /// A helper's card from its start to its end: what it reads and searches comes in a few rows, the latest last; the figures are the agent's latest.
    #[test]
    fn a_helpers_card_follows_what_the_agent_tells_of_it() {
        let mut card = Card::helper_begun("c7", "Where is the table?", Some(2));
        assert_eq!(working(&card), (0, 0, vec![], 0, 0, false));
        assert_eq!(helper_status(&HelperState::Working { step: 0, most: 0, recent: vec![], tokens: 0, ms: 0, stopping: false }, 0), ("Starting…".to_string(), "0 s".to_string()));
        for (at, (tool, summary)) in [("read_file", "src/a.py"), ("search_files", "put in src"), ("list_files", "."), ("read_file", "src/b.py")].iter().enumerate() {
            card.helper_step(at as u32 + 1, 20, tool, summary, 1500 * (at as u64 + 1), 4000 * (at as u64 + 1));
        }
        let (step, most, recent, tokens, ms, _) = working(&card);
        assert_eq!((step, most, tokens, ms), (4, 20, 6000, 16000));
        assert_eq!(recent, ["Search  put in src", "List  .", "Read  src/b.py"], "the last {RECENT}, the latest last");
        let state = match &card {
            Card::Helper { state, .. } => state.clone(),
            _ => unreachable!(),
        };
        assert_eq!(helper_status(&state, 0), ("Step 4 of 20".to_string(), "16 s  ·  6.0k tokens".to_string()));
        assert_eq!(helper_status(&state, 62_000).1, "1 min 18 s  ·  6.0k tokens", "the time runs on from the agent's last word");
        assert_eq!(helper_recent(&state).to_vec(), ["Search  put in src", "List  .", "Read  src/b.py"]);
        assert!(helper_outcome(&state).is_none(), "nothing is over yet");
        card.helper_stopping();
        assert_eq!(helper_status(&match &card { Card::Helper { state, .. } => state.clone(), _ => unreachable!() }, 0).0, "Stopping…");
        card.helper_ended(&HelperEnd::Stopped, 4, 3, 6000, 17000, None);
        assert_eq!(card, Card::Helper { call: "c7".into(), task: "Where is the table?".into(), number: Some(2), state: HelperState::Stopped { steps: 4, files: 3, tokens: 6000, ms: 17000 } });
        // (a step told after the end changes nothing: the figures of an ended helper are its own)
        card.helper_step(5, 20, "read_file", "x", 1, 1);
        assert!(matches!(card, Card::Helper { state: HelperState::Stopped { steps: 4, .. }, .. }));
    }

    #[test]
    fn a_helper_is_told_over_in_a_line_that_says_how_it_went() {
        let outcome = |state: HelperState| helper_outcome(&state).unwrap();
        assert_eq!(outcome(HelperState::Done { steps: 22, files: 14, tokens: 31000, ms: 197_000 }), ("Done  ·  3 min 17 s  ·  22 steps  ·  14 files read".to_string(), true));
        assert_eq!(outcome(HelperState::Done { steps: 1, files: 1, tokens: 0, ms: 4000 }).0, "Done  ·  4 s  ·  1 step  ·  1 file read");
        assert_eq!(outcome(HelperState::Stopped { steps: 3, files: 0, tokens: 0, ms: 61_000 }), ("Stopped by you  ·  1 min 1 s  ·  3 steps  ·  0 files read".to_string(), false));
        assert_eq!(outcome(HelperState::Failed { why: "the helper ended with nothing to say".into(), steps: 2, ms: 9000 }), ("Could not finish: the helper ended with nothing to say".to_string(), false));
        assert_eq!((token_count_words(850), token_count_words(14_250)), ("850".to_string(), "14.2k".to_string()));
    }

    /// A session that is opened again has each helper's card from the call's result alone, and a result from before there was a record of its helper has none.
    #[test]
    fn a_session_opened_again_shows_each_helper_with_how_it_went() {
        use dogmind_agent::message::HelperRef;
        let kept = |id: &str, output: &str, helper: Option<HelperRef>| Message { helper, ..Message::tool_result(id, output) };
        let reference = |number: u32, stopped: bool| Some(HelperRef { number, steps: 22, files: 14, ms: 197_000, tokens: 31_000, stopped });
        let messages = vec![
            Message::text("user", "look into the table"),
            asking("e1", "explore", json!({"task": "Where is the table?\nSecond line"})),
            kept("e1", "It is in src/table.py.", reference(1, false)),
            asking("e2", "explore", json!({"task": "Look again"})),
            kept("e2", "stopped by the person", reference(2, true)),
            asking("e3", "explore", json!({"task": "And again"})),
            kept("e3", "error: the helper's request failed: down", reference(3, false)),
            asking("e4", "explore", json!({"task": "From an older session"})),
            kept("e4", "It was here.", None),
            asking("e5", "explore", json!({"task": "Still under way"})),
        ];
        let found = of_messages(&messages, 1);
        assert_eq!(found.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["e1", "e2", "e3"], "a call with no record of its helper, and one with no result, are rows of the calls");
        let state = |at: usize| match &found[at].1 {
            Card::Helper { call, task, number, state } => (call.clone(), task.clone(), *number, state.clone()),
            other => panic!("{other:?}"),
        };
        assert_eq!(state(0), ("e1".into(), "Where is the table?".into(), Some(1), HelperState::Done { steps: 22, files: 14, tokens: 31_000, ms: 197_000 }));
        assert!(matches!(state(1), (_, _, Some(2), HelperState::Stopped { steps: 22, files: 14, .. })));
        assert!(matches!(state(2), (_, _, Some(3), HelperState::Failed { ref why, .. }) if why == "the helper's request failed: down"));
        assert_eq!(found[0].1.helper_call(), Some("e1"));
        assert_eq!(found[0].1.id(), None, "a helper's card is told by its call and takes no number");
    }

    #[test]
    fn a_helper_still_at_work_when_its_turn_ends_is_told_as_stopped() {
        let mut card = Card::helper_begun("c1", "Look", None);
        card.helper_step(3, 20, "read_file", "a", 900, 7000);
        assert!(card.helper_settled());
        assert!(matches!(card, Card::Helper { state: HelperState::Stopped { steps: 3, tokens: 900, ms: 7000, .. }, .. }));
        assert!(!card.helper_settled(), "nothing to settle twice");
    }

    #[test]
    fn what_a_helpers_card_says_has_its_japanese() {
        for words in ["Helper", "Starting…", "Stopping…", "Step {0} of {1}", "1 step", "{0} steps", "1 file read", "{0} files read", "Done  ·  {0}  ·  {1}  ·  {2}", "Stopped by you  ·  {0}  ·  {1}  ·  {2}", "Could not finish: {0}", "Stop", "Show its steps", "{0} tokens"] {
            assert!(crate::lang::has_japanese(words), "{words}");
        }
    }
}
