//! A file's changes as rows to draw: what a unified diff says, with each line's numbers on both sides and the colours of its language.
//! The window that draws them is in `ui`; what is here can be told without one.

use crate::highlight::{self, Kind};

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

/// A line as it is drawn: tabs at four columns, and no end to it that would have to be measured for nothing.
fn drawn(line: &str) -> String {
    const LONGEST: usize = 2000;
    line.trim_end_matches('\r').replace('\t', "    ").chars().take(LONGEST).collect()
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
    for line in diff.lines() {
        if found.len() >= most {
            found.push(Row { kind: RowKind::Note, old: None, new: None, runs: vec![("…".to_string(), Kind::Plain)] });
            break;
        }
        if let Some((from, to, words)) = hunk_head(line) {
            (old, new) = (from, to);
            // (a new file, or a change at the very top: nothing has been passed over to mark)
            if !(found.is_empty() && from <= 1 && to <= 1) {
                found.push(Row { kind: RowKind::Hunk, old: None, new: None, runs: vec![(drawn(words), Kind::Comment)] });
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
                found.push(Row { kind: RowKind::Added, old: None, new: Some(new), runs: colored(drawn(&line[1..]), &mut open_new) });
                new += 1;
            }
            Some('-') => {
                found.push(Row { kind: RowKind::Removed, old: Some(old), new: None, runs: colored(drawn(&line[1..]), &mut open_old) });
                old += 1;
            }
            Some(' ') => {
                let runs = colored(drawn(&line[1..]), &mut open_new);
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
            _ => found.push(Row { kind: RowKind::Note, old: None, new: None, runs: vec![(drawn(line.trim_start_matches("\\ ")), Kind::Comment)] }),
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

    #[test]
    fn what_a_gutter_needs() {
        assert_eq!(number_width(&rows(DIFF, "a.rs", 100)), 2);
        assert_eq!(number_width(&rows("@@ -1200,1 +1200,1 @@\n a\n", "a.rs", 100)), 4);
    }
}
