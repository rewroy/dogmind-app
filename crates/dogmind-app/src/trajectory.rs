//! The trajectory page's table: its columns, and its rows for the turns of a session, each open (with its requests and its tools) or shut. Nothing here draws: the window draws what these say,
//! each cell in a column of its own of a fixed width in the characters of its monospace face, which is what makes it a table and not words lined up with tabs.

use std::collections::BTreeSet;

use dogmind_agent::trajectory_report::{RequestRow, Summary, ToolRow, TurnSummary};

use crate::lang::{t, tf};

/// A column: its title, the room it has in characters of the table's monospace face, and whether its figures stand at its right edge.
pub struct Column {
    pub title: &'static str,
    pub chars: usize,
    pub right: bool,
}

/// The columns of the table, and then what a row has to say that is not a figure, in the room that is left.
pub const COLUMNS: [Column; 6] = [
    Column { title: "Turn", chars: 22, right: false },
    Column { title: "Requests", chars: 8, right: true },
    Column { title: "Prefill s", chars: 9, right: true },
    Column { title: "Generation s", chars: 12, right: true },
    Column { title: "Tools s", chars: 7, right: true },
    Column { title: "Wall s", chars: 7, right: true },
];

/// The room at the table's left, in characters: a turn's chevron stands in it.
pub const MARGIN: usize = 3;

/// What a row is, which is what the window draws it as and what a press on it does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowKind {
    Header,
    /// A turn: pressed, it opens or shuts.
    Turn { turn: u64, open: bool },
    Request,
    Tool,
    /// All the turns added up.
    Total,
}

/// One row: a cell for each column, and a note after them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Row {
    pub kind: RowKind,
    pub cells: [String; 6],
    pub note: String,
}

/// Seconds to a tenth, without the unit (the column's title has it).
pub fn seconds(ms: u64) -> String {
    format!("{:.1}", ms as f64 / 1000.0)
}

/// The rows of the page: the header, each turn (with its requests and then its tools under it where it is open) and, with more than one turn, all of them added up.
pub fn rows(summary: &Summary, open: &BTreeSet<u64>) -> Vec<Row> {
    let mut rows = vec![Row { kind: RowKind::Header, cells: COLUMNS.map(|column| t(column.title).to_string()), note: String::new() }];
    for turn in &summary.turns {
        let is_open = open.contains(&turn.turn);
        rows.push(Row {
            kind: RowKind::Turn { turn: turn.turn, open: is_open },
            cells: [tf("Turn {0}", &[&turn.turn]), turn.requests.len().to_string(), seconds(turn.prefill_ms), seconds(turn.generation_ms), seconds(turn.tools_ms), seconds(turn.wall_ms)],
            note: turn_note(turn),
        });
        if is_open {
            rows.extend(turn.requests.iter().map(request_row));
            rows.extend(turn.tools.iter().map(tool_row));
        }
    }
    if summary.turns.len() > 1 {
        let all = summary.total();
        let requests: usize = summary.turns.iter().map(|turn| turn.requests.len()).sum();
        rows.push(Row {
            kind: RowKind::Total,
            cells: [t("All turns").to_string(), requests.to_string(), seconds(all.prefill_ms), seconds(all.generation_ms), seconds(all.tools_ms), seconds(all.wall_ms)],
            note: String::new(),
        });
    }
    rows
}

/// What a turn says besides its figures: how much was written to begin it, that it has no end, how it failed.
fn turn_note(turn: &TurnSummary) -> String {
    let mut parts = vec![if turn.woken { t("woken by background work").to_string() } else { tf("{0} characters", &[&turn.chars]) }];
    if turn.pictures > 0 {
        parts.push(tf("{0} pictures", &[&turn.pictures]));
    }
    if turn.unfinished {
        parts.push(t("no end: the run stopped here").to_string());
    }
    if let Some(kind) = &turn.failed {
        parts.push(kind.clone());
    }
    parts.join(", ")
}

/// A request of an open turn: its time to the first word under Prefill, the rest of it under Generation, what it was and how it ended in the note.
fn request_row(request: &RequestRow) -> Row {
    let mut parts = Vec::new();
    if let Some(prompt) = request.prompt {
        parts.push(tf("prompt {0}", &[&prompt]));
    }
    if let Some(cached) = request.cached_percent {
        parts.push(tf("cached {0}%", &[&cached]));
    }
    if let Some(tokens) = request.tokens {
        parts.push(tf("{0} tokens", &[&tokens]));
    }
    if let Some(rate) = request.rate {
        parts.push(tf("{0} tok/s", &[&format!("{}{rate:.1}", if request.counted { "~" } else { "" })]));
    }
    parts.push(if request.calls.is_empty() { request.end.clone() } else { format!("{} [{}]", request.end, request.calls.join(", ")) });
    if let Some((of, why)) = &request.retry_of {
        parts.push(tf("repeats {0} ({1})", &[of, why]));
    }
    if let Some(file) = &request.helper_file {
        parts.push(tf("its steps are in {0}", &[file]));
    }
    let first = request.first_ms.unwrap_or(0);
    Row {
        kind: RowKind::Request,
        cells: [format!("  {} {}", request.id, request.role), String::new(), request.first_ms.map_or("-".to_string(), seconds), seconds(request.generation_ms), String::new(), seconds(first + request.generation_ms)],
        note: parts.join(", "),
    }
}

/// A tool call of an open turn: its time under Tools, and how it ended and what it did in the note.
fn tool_row(tool: &ToolRow) -> Row {
    let mut parts = vec![tool.outcome.clone()];
    parts.extend(tool.notes.iter().cloned());
    Row { kind: RowKind::Tool, cells: [format!("  {} {}", tool.id, tool.name), String::new(), String::new(), String::new(), seconds(tool.ms), seconds(tool.ms)], note: parts.join(", ") }
}

/// The left edge of each column in a row, in characters from the left of the table (after the margin, where a turn's chevron stands), and where the notes begin: the columns side by side with
/// a character between each.
pub fn column_starts() -> ([usize; 6], usize) {
    let mut starts = [0; 6];
    let mut at = MARGIN;
    for (place, column) in COLUMNS.iter().enumerate() {
        starts[place] = at;
        at += column.chars + 1;
    }
    (starts, at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dogmind_agent::trajectory_report::summary;

    fn run() -> Summary {
        let lines = [
            r#"{"t":"2026-11-05T10:00:00.000+09:00","ev":"turn_start","turn":1,"chars":40,"images":0}"#,
            r#"{"t":"2026-11-05T10:01:43.000+09:00","ev":"request","turn":1,"req":"s/1","role":"main","first_ms":2000,"ms":103000,"prompt":5000,"cached":0,"completion":8328,"tps":null,"end":"tool_calls","calls":["create_file"]}"#,
            r#"{"t":"2026-11-05T10:01:44.000+09:00","ev":"tool","turn":1,"req":"s/1","name":"create_file","ms":40,"outcome":"ok","out_bytes":10,"path":"index.html"}"#,
            r#"{"t":"2026-11-05T10:02:30.000+09:00","ev":"turn_end","turn":1,"cancelled":false,"requests":1,"tools":1,"ms":150000}"#,
            r#"{"t":"2026-11-05T10:03:00.000+09:00","ev":"turn_start","turn":2,"chars":4,"images":0}"#,
            r#"{"t":"2026-11-05T10:03:20.000+09:00","ev":"request","turn":2,"req":"s/2","role":"main","first_ms":500,"ms":20000,"prompt":9000,"cached":8100,"completion":300,"tps":31.5,"end":"stop","calls":[]}"#,
            r#"{"t":"2026-11-05T10:03:21.000+09:00","ev":"turn_end","turn":2,"cancelled":false,"requests":1,"tools":0,"ms":21000}"#,
        ];
        summary(&lines.join("\n"))
    }

    #[test]
    fn the_page_has_a_header_a_row_for_each_turn_and_all_of_them_added_up() {
        let page = rows(&run(), &BTreeSet::new());
        let cells: Vec<Vec<&str>> = page.iter().map(|row| row.cells.iter().map(String::as_str).collect()).collect();
        assert_eq!(cells[0], ["Turn", "Requests", "Prefill s", "Generation s", "Tools s", "Wall s"]);
        assert_eq!(cells[1], ["Turn 1", "1", "2.0", "101.0", "0.0", "150.0"]);
        assert_eq!(cells[2], ["Turn 2", "1", "0.5", "19.5", "0.0", "21.0"]);
        // (the sums of the turns, not their own numbers over again)
        assert_eq!(cells[3], ["All turns", "2", "2.5", "120.5", "0.0", "171.0"]);
        assert_eq!(page.len(), 4);
        assert_eq!((page[0].kind, page[1].kind, page[3].kind), (RowKind::Header, RowKind::Turn { turn: 1, open: false }, RowKind::Total));
        assert_eq!(page[1].note, "40 characters");
    }

    #[test]
    fn an_open_turn_has_its_requests_and_then_its_tools_under_it_in_the_same_columns() {
        let page = rows(&run(), &BTreeSet::from([1]));
        let kinds: Vec<RowKind> = page.iter().map(|row| row.kind).collect();
        assert_eq!(kinds, [RowKind::Header, RowKind::Turn { turn: 1, open: true }, RowKind::Request, RowKind::Tool, RowKind::Turn { turn: 2, open: false }, RowKind::Total]);
        let request = &page[2];
        assert_eq!(request.cells, ["  /1 main", "", "2.0", "101.0", "", "103.0"]);
        assert_eq!(request.note, "prompt 5000, cached 0%, 8328 tokens, ~82.5 tok/s, tool_calls [create_file]");
        let tool = &page[3];
        assert_eq!(tool.cells, ["  /1 create_file", "", "", "", "0.0", "0.0"]);
        assert_eq!(tool.note, "ok, index.html, 10 bytes out");
        // (the server's own rate is not marked as counted)
        let page = rows(&run(), &BTreeSet::from([2]));
        assert_eq!(page[3].note, "prompt 9000, cached 90%, 300 tokens, 31.5 tok/s, stop");
    }

    /// A turn nobody sent says so, where a turn of the person's says how much they wrote.
    #[test]
    fn a_turn_that_woke_itself_says_so_and_not_how_much_was_written() {
        let woken = summary(r#"{"t":"2026-11-05T10:00:00.000+09:00","ev":"turn_start","turn":1,"chars":900,"images":0,"woken":true}"#);
        let page = rows(&woken, &BTreeSet::new());
        assert_eq!(page[1].note, "woken by background work, no end: the run stopped here");
        assert!(woken.turns[0].woken);
    }

    #[test]
    fn a_session_with_one_turn_has_no_total_and_one_with_none_has_only_the_header() {
        let one = summary(r#"{"t":"2026-11-05T10:00:00.000+09:00","ev":"turn_start","turn":1,"chars":4,"images":0}"#);
        let page = rows(&one, &BTreeSet::new());
        assert_eq!(page.len(), 2);
        assert_eq!(page[1].note, "4 characters, no end: the run stopped here");
        assert_eq!(rows(&Summary::default(), &BTreeSet::new()).len(), 1);
    }

    #[test]
    fn the_columns_stand_side_by_side_each_after_the_one_before_with_a_character_between() {
        let (starts, notes) = column_starts();
        assert_eq!(starts[0], MARGIN);
        for place in 1..6 {
            assert_eq!(starts[place], starts[place - 1] + COLUMNS[place - 1].chars + 1);
        }
        assert_eq!(notes, starts[5] + COLUMNS[5].chars + 1);
        // (a figure of a tenth of a second in any of the hours a run can take fits its column)
        assert!(seconds(99_999_000).chars().count() <= COLUMNS[5].chars);
    }
}
