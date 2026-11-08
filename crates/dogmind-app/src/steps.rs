//! The steps of an `explore` helper as a page to read in the panel on the right: its conversation, as the agent kept it in a file beside the session, turned into the blocks that page draws,
//! and where they stand in a view. Decisions only, with no window in them, so that a test can ask them: `ui.rs` measures and draws what they answer.

use std::collections::HashMap;

use dogmind_agent::message::Message;
use dogmind_agent::prompt;

/// How many lines of what a call brought back its block shows: the rest are counted.
pub const MOST_OUTPUT_LINES: usize = 12;
/// The longest a line of that is, in characters.
const LONGEST_LINE: usize = 240;

/// A piece of what the helper wrote, as the transcript would set it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Paragraph(String),
    Heading(String),
    Item(String),
    Code(Vec<String>),
}

/// One thing in the helper's conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// What the helper was asked.
    Task(String),
    /// What the helper wrote: between its calls, and its answer.
    Said(Vec<Piece>),
    /// A call of its. `ok`: how it came out, none for a call that has no result (it was stopped before it ran). `output`: the first lines of what came back (or the error), and `cut`: how many more there were.
    Call { tool: String, summary: String, ok: Option<bool>, output: Vec<String>, cut: usize },
    /// What the app said to it, or to the person about it.
    Note(String),
}

/// The blocks of a helper's conversation, in order. Its prompt is not shown; its first message is the task; a call and its result are one block.
pub fn blocks_of(messages: &[Message]) -> Vec<Block> {
    let results: HashMap<&str, &str> = messages.iter().filter(|message| message.role == "tool").filter_map(|message| Some((message.tool_call_id.as_deref()?, message.content.as_deref().unwrap_or("")))).collect();
    let mut blocks = Vec::new();
    let mut asked = false;
    for message in messages {
        match message.role.as_str() {
            "user" => {
                let Some(text) = message.content.as_deref().map(str::trim).filter(|text| !text.is_empty()) else { continue };
                if !asked && !prompt::is_nudge(text) {
                    asked = true;
                    blocks.push(Block::Task(text.to_string()));
                } else {
                    // (after the task a user's message is the app's: the note that it is time to answer)
                    let said = text.strip_prefix(prompt::DRAFT_NOTE).or_else(|| text.strip_prefix("[From the app]")).unwrap_or(text);
                    blocks.push(Block::Note(said.trim().to_string()));
                }
            }
            "assistant" => {
                if let Some(text) = message.content.as_deref().map(str::trim).filter(|text| !text.is_empty()) {
                    let pieces = answer_pieces(text);
                    if !pieces.is_empty() {
                        blocks.push(Block::Said(pieces));
                    }
                }
                for call in message.tool_calls.iter().flatten() {
                    let arguments = serde_json::from_str(&call.function.arguments).unwrap_or(serde_json::Value::Null);
                    let summary: String = dogmind_agent::tools::summary(&call.function.name, &arguments).chars().take(160).collect();
                    let (ok, output, cut) = match results.get(call.id.as_str()) {
                        Some(result) => {
                            let (ok, text) = match result.strip_prefix("error: ") {
                                Some(error) => (false, error),
                                None => (true, *result),
                            };
                            let (output, cut) = output_lines(text);
                            (Some(ok), output, cut)
                        }
                        None => (None, Vec::new(), 0),
                    };
                    blocks.push(Block::Call { tool: call.function.name.clone(), summary, ok, output, cut });
                }
            }
            _ => {}
        }
    }
    blocks
}

/// The first lines of what a call brought back, each kept to a line's length, and how many more there were.
fn output_lines(text: &str) -> (Vec<String>, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let shown = lines.iter().take(MOST_OUTPUT_LINES).map(|line| line.trim_end().chars().take(LONGEST_LINE).collect()).collect();
    (shown, lines.len().saturating_sub(MOST_OUTPUT_LINES))
}

/// What the helper wrote as pieces: fenced code, headings, list items and paragraphs, with the marks of emphasis, code and links taken off the words.
pub fn answer_pieces(text: &str) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let (mut paragraph, mut code): (Vec<String>, Option<Vec<String>>) = (Vec::new(), None);
    let flush = |paragraph: &mut Vec<String>, pieces: &mut Vec<Piece>| {
        if !paragraph.is_empty() {
            pieces.push(Piece::Paragraph(paragraph.join("\n")));
            paragraph.clear();
        }
    };
    for line in text.lines() {
        let line = line.trim_end();
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            match code.take() {
                Some(lines) => pieces.push(Piece::Code(lines)),
                None => {
                    flush(&mut paragraph, &mut pieces);
                    code = Some(Vec::new());
                }
            }
            continue;
        }
        if let Some(lines) = &mut code {
            lines.push(line.to_string());
            continue;
        }
        if trimmed.is_empty() {
            flush(&mut paragraph, &mut pieces);
        } else if let Some(heading) = crate::markdown::heading_level(trimmed).map(|_| trimmed.trim_start_matches('#').trim()) {
            flush(&mut paragraph, &mut pieces);
            pieces.push(Piece::Heading(strip_inline(heading)));
        } else if let Some(item) = list_item(trimmed) {
            flush(&mut paragraph, &mut pieces);
            pieces.push(Piece::Item(strip_inline(item)));
        } else {
            paragraph.push(strip_inline(trimmed));
        }
    }
    // (a fence that was never closed holds what was written after it)
    if let Some(lines) = code {
        pieces.push(Piece::Code(lines));
    }
    flush(&mut paragraph, &mut pieces);
    pieces
}

/// The words of a list item, if the line is one: `- `, `* `, `+ ` or `1. `.
fn list_item(line: &str) -> Option<&str> {
    for mark in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(mark) {
            return Some(rest.trim_start());
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0 && digits <= 3).then(|| line[digits..].strip_prefix(". ").map(str::trim_start)).flatten()
}

/// Words without the marks Markdown puts round them: `**strong**`, `*emphasis*`, `` `code` ``, and `[words](address)` as its words.
pub fn strip_inline(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        match c {
            '`' | '*' | '~' => at += 1,
            '_' if chars.get(at + 1) == Some(&'_') => at += 2,
            '[' => {
                // a link: its words stay, its address goes
                let close = chars[at..].iter().position(|&c| c == ']').map(|p| at + p);
                match close.filter(|&close| chars.get(close + 1) == Some(&'(')) {
                    Some(close) => {
                        let end = chars[close..].iter().position(|&c| c == ')').map(|p| close + p);
                        match end {
                            Some(end) => {
                                out.extend(&chars[at + 1..close]);
                                at = end + 1;
                            }
                            None => {
                                out.push(c);
                                at += 1;
                            }
                        }
                    }
                    None => {
                        out.push(c);
                        at += 1;
                    }
                }
            }
            _ => {
                out.push(c);
                at += 1;
            }
        }
    }
    out
}

/// The blocks as they are drawn: keys and tokens hidden but their beginning, and what acts on text and shows nothing shown as its mark, as the conversation shows what a tool brought back.
pub fn shown(blocks: Vec<Block>) -> Vec<Block> {
    let safe = |text: &str| crate::mask::hide_keys(&crate::markdown::show_hidden(text)).into_owned();
    blocks.into_iter().map(|block| match block {
        Block::Task(words) => Block::Task(safe(&words)),
        Block::Said(pieces) => Block::Said(pieces.into_iter().map(|piece| match piece {
            Piece::Paragraph(words) => Piece::Paragraph(safe(&words)),
            Piece::Heading(words) => Piece::Heading(safe(&words)),
            Piece::Item(words) => Piece::Item(safe(&words)),
            Piece::Code(lines) => Piece::Code(lines.iter().map(|line| safe(line)).collect()),
        }).collect()),
        Block::Call { tool, summary, ok, output, cut } => Block::Call { tool, summary: safe(&summary), ok, output: output.iter().map(|line| safe(line)).collect(), cut },
        Block::Note(words) => Block::Note(safe(&words)),
    }).collect()
}

/// Where a block stands in the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placed {
    pub top: i32,
    pub height: i32,
}

/// The blocks one under another, `gap` apart, each as tall as `measure` says it is: where each stands, and how tall the page is.
pub fn place(blocks: &[Block], gap: i32, mut measure: impl FnMut(&Block) -> i32) -> (Vec<Placed>, i32) {
    let mut placed = Vec::with_capacity(blocks.len());
    let mut top = 0;
    for block in blocks {
        let height = measure(block);
        placed.push(Placed { top, height });
        top += height + gap;
    }
    (placed, (top - gap).max(0))
}

/// The blocks any of which shows in a view `view` tall with `scroll` above it.
pub fn in_view(placed: &[Placed], scroll: i32, view: i32) -> std::ops::Range<usize> {
    let first = placed.partition_point(|block| block.top + block.height <= scroll);
    let end = placed.partition_point(|block| block.top < scroll + view);
    first..end.max(first)
}

/// How far a page `total` tall can be scrolled in a view `view` tall.
pub fn most_scroll(total: i32, view: i32) -> i32 {
    (total - view).max(0)
}

/// What a card's words say of the steps view: a line under its title.
pub fn blocks_summary(blocks: &[Block]) -> (usize, usize) {
    let calls = blocks.iter().filter(|block| matches!(block, Block::Call { .. })).count();
    let said = blocks.iter().filter(|block| matches!(block, Block::Said(_))).count();
    (calls, said)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dogmind_agent::message::{FunctionCall, ToolCall};
    use serde_json::json;

    fn calling(calls: &[(&str, &str, serde_json::Value)], words: Option<&str>) -> Message {
        let list = calls.iter().map(|(id, name, arguments)| ToolCall { id: id.to_string(), kind: "function".into(), function: FunctionCall { name: name.to_string(), arguments: arguments.to_string() } }).collect();
        Message { role: "assistant".into(), content: words.map(str::to_string), tool_calls: Some(list), ..Message::default() }
    }

    #[test]
    fn a_helpers_conversation_is_its_task_its_calls_with_what_came_of_them_and_its_answer() {
        let messages = vec![
            Message::text("system", "You look into a project for another agent."),
            Message::text("user", "Where is the table?"),
            calling(&[("a", "read_file", json!({"path": "src/table.py"})), ("b", "search_files", json!({"pattern": "put", "path": "src"})), ("c", "read_file", json!({"path": "gone.py"}))], Some("Let me look.")),
            Message::tool_result("a", "1|def put():\n2|    pass\n"),
            Message::tool_result("b", "src/table.py:1:def put"),
            Message::tool_result("c", "error: gone.py: not found"),
            Message::text("user", prompt::HELPER_ENOUGH),
            Message::text("assistant", "It is in `src/table.py`."),
        ];
        let blocks = blocks_of(&messages);
        assert_eq!(blocks.len(), 7, "{blocks:?}");
        assert_eq!(blocks[0], Block::Task("Where is the table?".into()));
        assert_eq!(blocks[1], Block::Said(vec![Piece::Paragraph("Let me look.".into())]));
        assert_eq!(blocks[2], Block::Call { tool: "read_file".into(), summary: "src/table.py".into(), ok: Some(true), output: vec!["1|def put():".into(), "2|    pass".into()], cut: 0 });
        assert_eq!(blocks[3], Block::Call { tool: "search_files".into(), summary: "put in src".into(), ok: Some(true), output: vec!["src/table.py:1:def put".into()], cut: 0 });
        assert_eq!(blocks[4], Block::Call { tool: "read_file".into(), summary: "gone.py".into(), ok: Some(false), output: vec!["gone.py: not found".into()], cut: 0 });
        assert!(matches!(&blocks[5], Block::Note(words) if words.starts_with("That is as much looking as there is room for.")), "{:?}", blocks[5]);
        assert_eq!(blocks[6], Block::Said(vec![Piece::Paragraph("It is in src/table.py.".into())]));
    }

    #[test]
    fn a_long_result_shows_its_first_lines_and_counts_the_rest_and_a_call_with_none_says_so() {
        let long: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        let messages = vec![
            Message::text("user", "Look"),
            calling(&[("a", "read_file", json!({"path": "big.txt"})), ("b", "list_files", json!({"path": "."}))], None),
            Message::tool_result("a", long),
        ];
        let blocks = blocks_of(&messages);
        let Block::Call { output, cut, .. } = &blocks[1] else { panic!("{blocks:?}") };
        assert_eq!((output.len(), *cut, output[0].as_str()), (MOST_OUTPUT_LINES, 30 - MOST_OUTPUT_LINES, "line 1"));
        assert!(matches!(&blocks[2], Block::Call { ok: None, output, .. } if output.is_empty()), "stopped before it ran");
    }

    #[test]
    fn an_answer_is_set_out_as_headings_items_code_and_paragraphs_without_the_marks() {
        let pieces = answer_pieces("**Where the work is**\n\n# Notes\n- `src/table.py` holds the [table](http://x.example)\n1. second\n\nOne line\nand another.\n\n```python\ndef put():\n    pass\n```\nafter");
        assert_eq!(pieces, vec![
            Piece::Paragraph("Where the work is".into()),
            Piece::Heading("Notes".into()),
            Piece::Item("src/table.py holds the table".into()),
            Piece::Item("second".into()),
            Piece::Paragraph("One line\nand another.".into()),
            Piece::Code(vec!["def put():".into(), "    pass".into()]),
            Piece::Paragraph("after".into()),
        ]);
        assert_eq!(answer_pieces("```\nnever closed"), vec![Piece::Code(vec!["never closed".into()])]);
        assert!(answer_pieces("  \n\n").is_empty());
        assert_eq!(strip_inline("snake_case_name and a * b"), "snake_case_name and a  b");
    }

    #[test]
    fn blocks_stand_one_under_another_and_the_view_shows_those_it_reaches() {
        let blocks = vec![Block::Task("a".into()), Block::Note("b".into()), Block::Note("c".into()), Block::Note("d".into())];
        let heights = [100, 50, 200, 30];
        let mut at = 0;
        let (placed, total) = place(&blocks, 10, |_| {
            at += 1;
            heights[at - 1]
        });
        assert_eq!(placed.iter().map(|block| block.top).collect::<Vec<_>>(), [0, 110, 170, 380]);
        assert_eq!(total, 410);
        assert_eq!(in_view(&placed, 0, 100), 0..1);
        assert_eq!(in_view(&placed, 105, 60), 1..2, "past the end of the first, the gap and the second's top");
        assert_eq!(in_view(&placed, 120, 100), 1..3);
        assert_eq!(in_view(&placed, 380, 200), 3..4);
        assert_eq!(in_view(&[], 0, 100), 0..0);
        assert_eq!((most_scroll(410, 100), most_scroll(40, 100)), (310, 0));
    }
}
