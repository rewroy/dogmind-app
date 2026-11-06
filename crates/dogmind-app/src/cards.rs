//! The cards of the conversation that ask the person something or show something of the work: the model's question with its choices. What is on it, in each state, and what a session that is
//! opened again has of it. Decisions only, with no window in them, so that a test can ask them: `ui.rs` draws what they answer.

use std::collections::HashMap;

use dogmind_agent::agent;
use dogmind_agent::message::Message;
use serde_json::Value;

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

/// What a card of the transcript is: it is kept with the rest of what the transcript is written from, and written again, in whatever state it has come to, as the window changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Card {
    Question { id: u64, question: String, options: Vec<(String, String)>, state: AnswerState },
}

impl Card {
    /// The number a card has, which says which it is among those of the transcript.
    pub fn id(&self) -> Option<u64> {
        match self {
            Card::Question { id, .. } => Some(*id),
        }
    }

    /// The card of a call of `ask_user` that a session kept. `result` is what the call returned: none for a call that has no result, which is one still waiting (the window is told of it as it
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
            _ => None,
        }
    }
}

/// The cards a session's messages hold: one for each call of `ask_user` that has a result, in order, with its call's id and the number its card has (from `first_id`).
pub fn of_messages(messages: &[Message], first_id: u64) -> Vec<(String, Card)> {
    let results: HashMap<&str, &str> = messages.iter().filter_map(|message| Some((message.tool_call_id.as_deref()?, message.content.as_deref().unwrap_or("")))).collect();
    let mut found = Vec::new();
    for call in messages.iter().filter(|message| message.role == "assistant").flat_map(|message| message.tool_calls.iter().flatten()) {
        let arguments = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
        if let Some(card) = Card::of_call(first_id + found.len() as u64, &call.function.name, &arguments, results.get(call.id.as_str()).copied()) {
            found.push((call.id.clone(), card));
        }
    }
    found
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
    matches!(tool, "ask_user")
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
    fn a_number_typed_for_a_question_picks_its_choice() {
        let labels: Vec<String> = ["hello", "hi"].iter().map(|label| label.to_string()).collect();
        assert_eq!(answer_text(&labels, "2"), "hi");
        assert_eq!(answer_text(&labels, " 1 "), "hello");
        assert_eq!(answer_text(&labels, "3"), "3", "past the choices it is a word like any other");
        assert_eq!(answer_text(&labels, "0"), "0");
        assert_eq!(answer_text(&labels, "neither, hey"), "neither, hey");
    }

    #[test]
    fn the_tool_that_asks_is_a_card_and_no_other_is() {
        assert!(is_asking("ask_user"));
        assert!(!is_asking("read_file") && !is_asking("todo"));
    }
}
