//! A file's changes as rows to draw: what a unified diff says, with each line's numbers on both sides and the colours of its language.
//! The window that draws them is in `ui`; what is here can be told without one.

use crate::highlight::{self, Kind};
use crate::lang::t;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    Same,
    Added,
    Removed,
    /// Where a run of changes begins, with what the diff says it is inside, if anything.
    Hunk,
    /// What is no line of the file: that it has no newline at its end, or that it is binary.
    Note,
}

pub struct Row {
    pub kind: RowKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    /// Its text in the colours of its language.
    pub runs: Vec<(String, Kind)>,
}

impl Row {
    pub fn text(&self) -> String {
        self.runs.iter().map(|(text, _)| text.as_str()).collect()
    }
}

/// What can be told of a language from a file's name.
pub fn language_of(path: &str) -> String {
    path.rsplit(['/', '\\']).next().and_then(|name| name.rsplit_once('.')).map_or(String::new(), |(_, extension)| extension.to_lowercase())
}

/// The most of a line that is drawn: no end to it that would have to be measured for nothing.
const LONGEST: usize = 2000;
/// How much of two long lines is shown on each side of where they first differ.
const AROUND: usize = 60;

/// A line as it is drawn: tabs at four columns (an arrow and spaces, where it is a tab that is all there is to a change), and a mark where it is cut.
fn drawn(line: &str, show_tabs: bool) -> String {
    let text = line.trim_end_matches('\r').replace('\t', if show_tabs { "→   " } else { "    " });
    if text.chars().count() > LONGEST {
        format!("{}…", text.chars().take(LONGEST).collect::<String>())
    } else {
        text
    }
}

/// The lines of a diff, each without its newline and with its carriage return, if it has one: `str::lines` takes that away, and with it the difference this is about.
fn diff_lines(diff: &str) -> impl Iterator<Item = &str> {
    diff.split_inclusive('\n').map(|line| line.strip_suffix('\n').unwrap_or(line))
}

/// A line of the diff, and what is told of it by the one it is set against.
struct Prepared {
    text: String,
    /// It and the line it is paired with differ in tabs and spaces alone: the tabs are shown.
    whitespace_only: bool,
}

/// The lines of a diff, with each removed line set against the added line in its place (the n-th of a block of removals against the n-th of the additions after it): two long lines that
/// are the same as far as a line is drawn are cut where they differ and not at the end, and two that differ in tabs and spaces alone are marked.
fn prepared(diff: &str) -> Vec<Prepared> {
    let mut lines: Vec<Prepared> = diff_lines(diff).map(|line| Prepared { text: line.to_string(), whitespace_only: false }).collect();
    let is = |line: &Prepared, mark: char| line.text.starts_with(mark) && !line.text.starts_with("--- ") && !line.text.starts_with("+++ ");
    let mut at = 0;
    while at < lines.len() {
        let start = at;
        while at < lines.len() && is(&lines[at], '-') {
            at += 1;
        }
        let middle = at;
        while at < lines.len() && is(&lines[at], '+') {
            at += 1;
        }
        if middle == start || at == middle {
            at = at.max(start + 1);
            continue;
        }
        for k in 0..(middle - start).min(at - middle) {
            let (old, new) = (start + k, middle + k);
            let (a, b) = (lines[old].text[1..].trim_end_matches('\r').to_string(), lines[new].text[1..].trim_end_matches('\r').to_string());
            if a == b {
                continue;
            }
            if a.replace('\t', "    ") == b.replace('\t', "    ") {
                lines[old].whitespace_only = true;
                lines[new].whitespace_only = true;
            } else if a.chars().count() > LONGEST && b.chars().count() > LONGEST && a.chars().take(LONGEST).eq(b.chars().take(LONGEST)) {
                // (a change past the cut would not show at all: where they differ is shown instead)
                let first = a.chars().zip(b.chars()).position(|(x, y)| x != y).unwrap_or_else(|| a.chars().count().min(b.chars().count()));
                let window = |text: &str| format!("…{}…", text.chars().skip(first.saturating_sub(AROUND)).take(2 * AROUND).collect::<String>());
                lines[old].text = format!("-{}", window(&a));
                lines[new].text = format!("+{}", window(&b));
            }
        }
    }
    lines
}

/// Whether the diff changes nothing but line endings: every removed line is an added line with a carriage return more or less. Which way, if so.
fn only_line_endings(diff: &str) -> Option<&'static str> {
    let (mut removed, mut added): (Vec<&str>, Vec<&str>) = (Vec::new(), Vec::new());
    for line in diff_lines(diff) {
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            continue;
        }
        match line.chars().next() {
            Some('-') => removed.push(&line[1..]),
            Some('+') => added.push(&line[1..]),
            _ => {}
        }
    }
    if removed.is_empty() || removed.len() != added.len() || !removed.iter().zip(&added).all(|(a, b)| a.trim_end_matches('\r') == b.trim_end_matches('\r')) {
        return None;
    }
    let (with, without) = (removed.iter().filter(|line| line.ends_with('\r')).count(), added.iter().filter(|line| line.ends_with('\r')).count());
    match (with, without) {
        (0, 0) => None,
        (more, fewer) if more >= fewer => Some("line endings only (CRLF → LF)"),
        _ => Some("line endings only (LF → CRLF)"),
    }
}

/// `@@ -a,b +c,d @@ what it is in`: where the two sides begin, and the words.
fn hunk_head(line: &str) -> Option<(u32, u32, &str)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, words) = rest.split_once(" @@")?;
    let first = |side: &str| side.split(',').next().and_then(|number| number.parse().ok());
    Some((first(old)?, first(new)?, words.trim()))
}

/// The rows of a diff. Past `most` it stops, saying so: a file of a hundred thousand changed lines is not read here.
pub fn rows(diff: &str, path: &str, most: usize) -> Vec<Row> {
    // A file whose every line changed by its ending alone would be a wall of lines that read the same: said once.
    if let Some(words) = only_line_endings(diff) {
        return vec![Row { kind: RowKind::Note, old: None, new: None, runs: vec![(t(words).to_string(), Kind::Comment)] }];
    }
    let language = language_of(path);
    let mut found = Vec::new();
    let (mut old, mut new) = (0, 0);
    // (each side has its own comments to be inside of)
    let (mut open_new, mut open_old) = (false, false);
    let colored = |text: String, open: &mut bool| -> Vec<(String, Kind)> {
        match highlight::runs(&text, &language, *open) {
            Some((runs, still)) => {
                *open = still;
                runs
            }
            None => vec![(text, Kind::Plain)],
        }
    };
    for prepared_line in prepared(diff) {
        let (line, tabs) = (prepared_line.text.as_str(), prepared_line.whitespace_only);
        if found.len() >= most {
            found.push(Row { kind: RowKind::Note, old: None, new: None, runs: vec![("…".to_string(), Kind::Plain)] });
            break;
        }
        if let Some((from, to, words)) = hunk_head(line) {
            (old, new) = (from, to);
            // (a new file, or a change at the very top: nothing has been passed over to mark)
            if !(found.is_empty() && from <= 1 && to <= 1) {
                found.push(Row { kind: RowKind::Hunk, old: None, new: None, runs: vec![(drawn(words, false), Kind::Comment)] });
            }
            continue;
        }
        // (what git says of the file and not of its lines: the row in the list says it is new or gone)
        const GITS_OWN: [&str; 11] = ["--- ", "+++ ", "diff --git", "index ", "new file mode", "deleted file mode", "old mode", "new mode", "similarity index", "rename from", "rename to"];
        if GITS_OWN.iter().any(|own| line.starts_with(own)) {
            continue;
        }
        match line.chars().next() {
            Some('+') => {
                let mut runs = colored(drawn(&line[1..], tabs), &mut open_new);
                if tabs {
                    runs.push((format!("   ← {}", t("tabs and spaces only")), Kind::Comment));
                }
                found.push(Row { kind: RowKind::Added, old: None, new: Some(new), runs });
                new += 1;
            }
            Some('-') => {
                found.push(Row { kind: RowKind::Removed, old: Some(old), new: None, runs: colored(drawn(&line[1..], tabs), &mut open_old) });
                old += 1;
            }
            Some(' ') => {
                let runs = colored(drawn(&line[1..], false), &mut open_new);
                open_old = open_new;
                found.push(Row { kind: RowKind::Same, old: Some(old), new: Some(new), runs });
                old += 1;
                new += 1;
            }
            None if old > 0 || new > 0 => {
                found.push(Row { kind: RowKind::Same, old: Some(old), new: Some(new), runs: Vec::new() });
                old += 1;
                new += 1;
            }
            None => {}
            _ => found.push(Row { kind: RowKind::Note, old: None, new: None, runs: vec![(drawn(line.trim_start_matches("\\ "), false), Kind::Comment)] }),
        }
    }
    found
}

/// Where each block of changed lines begins.
pub fn change_starts(rows: &[Row]) -> Vec<usize> {
    let changed = |row: &Row| matches!(row.kind, RowKind::Added | RowKind::Removed);
    (0..rows.len()).filter(|&at| changed(&rows[at]) && (at == 0 || !changed(&rows[at - 1]))).collect()
}

/// The block of changes to go to from the row at the top of the view: the next past it or the last before it. None: there is no such one in this file.
pub fn next_change(starts: &[usize], top: usize, forward: bool) -> Option<usize> {
    if forward {
        starts.iter().copied().find(|&at| at > top)
    } else {
        starts.iter().rev().copied().find(|&at| at < top)
    }
}

/// How wide the longest line is, in half-width characters.
pub fn longest(rows: &[Row]) -> usize {
    rows.iter().map(|row| row.runs.iter().flat_map(|(text, _)| text.chars()).map(|c| if c.is_ascii() { 1 } else { 2 }).sum::<usize>()).max().unwrap_or(0)
}

/// How many digits the largest line number has, for the gutter's width.
pub fn number_width(rows: &[Row]) -> usize {
    rows.iter().flat_map(|row| [row.old, row.new]).flatten().max().unwrap_or(0).to_string().len().max(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10,3 +10,4 @@ fn main() {\n     let a = 1;\n-    let b = 2;\n+    let b = 3;\n+    let c = 4;\n     a\n@@ -40,2 +41,1 @@\n-old\n same\n\\ No newline at end of file\n";

    #[test]
    fn each_line_has_its_numbers_on_the_sides_it_is_on() {
        let rows = rows(DIFF, "src/main.rs", 100);
        let kinds: Vec<RowKind> = rows.iter().map(|row| row.kind).collect();
        assert_eq!(kinds, [RowKind::Hunk, RowKind::Same, RowKind::Removed, RowKind::Added, RowKind::Added, RowKind::Same, RowKind::Hunk, RowKind::Removed, RowKind::Same, RowKind::Note]);
        assert_eq!((rows[1].old, rows[1].new), (Some(10), Some(10)));
        assert_eq!((rows[2].old, rows[2].new), (Some(11), None));
        assert_eq!((rows[3].old, rows[3].new), (None, Some(11)));
        assert_eq!((rows[4].old, rows[4].new), (None, Some(12)));
        assert_eq!((rows[5].old, rows[5].new), (Some(12), Some(13)));
        assert_eq!((rows[7].old, rows[8].new), (Some(40), Some(41)));
        assert_eq!(rows[0].text(), "fn main() {");
        assert_eq!(rows[2].text(), "    let b = 2;");
        assert_eq!(rows[9].text(), "No newline at end of file");
    }

    #[test]
    fn a_line_comes_in_the_colours_of_its_language() {
        let rows = rows(DIFF, "src/main.rs", 100);
        assert!(rows[2].runs.iter().any(|(text, kind)| text == "let" && *kind == Kind::Keyword));
        let plain = super::rows(DIFF, "notes.xyz", 100);
        assert_eq!(plain[2].runs.len(), 1, "a language not known: one colour");
    }

    #[test]
    fn a_comment_open_on_one_side_is_not_open_on_the_other() {
        let diff = "@@ -1,2 +1,2 @@\n-/* old\n+let a = 1;\n let b = 2;\n";
        let rows = rows(diff, "a.rs", 100);
        assert!(rows[1].runs.iter().any(|(text, kind)| text == "let" && *kind == Kind::Keyword), "the new side has no comment open");
    }

    #[test]
    fn a_new_file_needs_no_mark_of_what_was_passed_over() {
        let rows = rows("@@ -0,0 +1,2 @@\n+a\n+b\n", "a.txt", 100);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].new, Some(2));
    }

    #[test]
    fn going_from_change_to_change() {
        let rows = rows(DIFF, "src/main.rs", 100);
        let starts = change_starts(&rows);
        assert_eq!(starts, [2, 7], "a removal and what replaces it are one change");
        assert_eq!(next_change(&starts, 0, true), Some(2));
        assert_eq!(next_change(&starts, 2, true), Some(7));
        assert_eq!(next_change(&starts, 7, true), None);
        assert_eq!(next_change(&starts, 7, false), Some(2));
        assert_eq!(next_change(&starts, 2, false), None);
    }

    #[test]
    fn a_very_long_diff_stops_and_says_so() {
        let long = format!("@@ -1,1 +1,1000 @@\n{}", "+x\n".repeat(1000));
        let rows = rows(&long, "a.txt", 10);
        assert_eq!(rows.len(), 11);
        assert_eq!(rows[10].kind, RowKind::Note);
    }

    #[test]
    fn what_git_says_of_the_file_is_no_row() {
        let rows = rows("diff --git a/x b/x\nnew file mode 100644\nindex 0000000..abc\n--- /dev/null\n+++ b/x\n@@ -0,0 +1 @@\n+a\n", "x", 100);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, RowKind::Added);
    }

    #[test]
    fn the_longest_line_counts_wide_characters_twice() {
        assert_eq!(longest(&rows("@@ -1,2 +1,2 @@\n abc\n 日本語\n", "x", 100)), 6);
    }

    /// A change past where a long line is cut still shows: the lines are cut where they differ, and a cut has a mark.
    #[test]
    fn a_change_past_the_cut_of_a_long_line_is_shown_where_it_is() {
        let long = |tail: &str| format!("{}{tail}", "x".repeat(LONGEST + 500));
        let diff = format!("@@ -1,1 +1,1 @@\n-{}\n+{}\n", long("OLD-END"), long("NEW-END"));
        let rows = rows(&diff, "min.js", 100);
        let (old, new) = (rows[0].text(), rows[1].text());
        assert!(old.contains("OLD-END") && new.contains("NEW-END"), "where they differ: {old} / {new}");
        assert!(old.starts_with('…') && old.ends_with('…') && old.chars().count() <= 2 * AROUND + 2, "{old}");
        // lines that differ before the cut are cut at the end, with a mark
        let diff = format!("@@ -1,1 +1,1 @@\n-a{}\n+b{}\n", "y".repeat(LONGEST + 10), "y".repeat(LONGEST + 10));
        let rows = super::rows(&diff, "min.js", 100);
        assert!(rows[0].text().ends_with('…') && rows[0].text().chars().count() == LONGEST + 1, "{}", rows[0].text().chars().count());
        // and a short one is not marked
        assert!(!super::rows("@@ -1,1 +1,1 @@\n-short\n+shorter\n", "a.txt", 100)[0].text().contains('…'));
    }

    /// Tabs changed to spaces read as two lines the same: the tabs are shown, and the pair says so.
    #[test]
    fn a_change_of_tabs_to_spaces_is_shown_as_one() {
        let rows = rows("@@ -1,2 +1,2 @@\n-\tindented\n-\tsecond\n+    indented\n+    second changed\n", "a.txt", 100);
        assert_eq!(rows[0].text(), "→   indented");
        assert!(rows[2].text().starts_with("    indented") && rows[2].text().contains("tabs and spaces only"), "{}", rows[2].text());
        assert_eq!(rows[1].text(), "    second", "a pair that is more than whitespace is not marked: its tab is drawn as it was");
        assert!(!rows[3].text().contains("tabs and spaces"), "{}", rows[3].text());
    }

    /// A file whose every line changed by its ending alone is said once.
    #[test]
    fn a_change_of_line_endings_alone_is_said_once() {
        let crlf_to_lf = "@@ -1,3 +1,3 @@\n-one\r\n-two\r\n-three\r\n+one\n+two\n+three\n";
        let rows = rows(crlf_to_lf, "a.txt", 100);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].kind, rows[0].text().as_str()), (RowKind::Note, "line endings only (CRLF → LF)"));
        let lf_to_crlf = "@@ -1,2 +1,2 @@\n-one\n-two\n+one\r\n+two\r\n";
        assert_eq!(super::rows(lf_to_crlf, "a.txt", 100)[0].text(), "line endings only (LF → CRLF)");
        // one real change among them, and it is a diff
        let with_change = "@@ -1,2 +1,2 @@\n-one\r\n-two\r\n+one\n+TWO\n";
        assert!(super::rows(with_change, "a.txt", 100).len() > 1);
        assert_eq!(only_line_endings("@@ -1,1 +1,1 @@\n-a\n+a\n"), None, "nothing differs at all: not a change of endings");
        assert_eq!(only_line_endings("@@ -1,1 +1,1 @@\n context\n"), None);
    }

    #[test]
    fn what_a_gutter_needs() {
        assert_eq!(number_width(&rows(DIFF, "a.rs", 100)), 2);
        assert_eq!(number_width(&rows("@@ -1200,1 +1200,1 @@\n a\n", "a.rs", 100)), 4);
    }
}
