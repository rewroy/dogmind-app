//! A shaded block in the transcript: a table of one cell, written as RTF. The shading fills the block whichever the length of
//! its lines, which colours behind the text alone do not, and a table can be added to row by row as an answer streams.

/// One row, with its own small document: each is inserted on its own.
pub struct Row<'a> {
    pub lines: &'a [&'a str],
    pub face: &'a str,
    pub points: i32,
    pub color: u32,
    pub back: u32,
    /// In twips.
    pub width: i32,
    /// Against the right edge, as what the person wrote is.
    pub right: bool,
    /// The column's width, in twips, that such a row keeps to. The control's own right alignment goes by a width of its own, which is not the column's.
    pub column: i32,
    /// Space around the text, in twips: across and down.
    pub padding: (i32, i32),
}

fn channels(color: u32) -> String {
    format!("\\red{}\\green{}\\blue{}", color & 0xFF, (color >> 8) & 0xFF, (color >> 16) & 0xFF)
}

/// In RTF's own spelling: what is not plain ASCII as UTF-16 units, each a signed number with a character for old readers after it.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for unit in text.encode_utf16() {
        match unit {
            0x5C => out.push_str("\\\\"),
            0x7B => out.push_str("\\{"),
            0x7D => out.push_str("\\}"),
            0x09 => out.push_str("\\tab "),
            0x20..=0x7E => out.push(char::from(unit as u8)),
            0x00..=0x1F => {}
            _ => out.push_str(&format!("\\u{}?", unit as i16)),
        }
    }
    out
}

/// What is written in a row: what is Japanese in its own face, `\\f1` of the table every row here has, so that it does not depend on what RichEdit makes of the Latin one.
fn words(text: &str) -> String {
    let (mut out, mut run, mut japanese) = (String::new(), String::new(), false);
    let mut flush = |run: &mut String, japanese: bool| {
        if !run.is_empty() {
            let escaped = escape(run);
            if japanese {
                out.push_str(&format!("{{\\f1 {escaped}}}"));
            } else {
                out.push_str(&escaped);
            }
            run.clear();
        }
    };
    for c in text.chars() {
        let here = crate::paint::is_japanese(c);
        if here != japanese {
            flush(&mut run, japanese);
            japanese = here;
        }
        run.push(c);
    }
    flush(&mut run, japanese);
    out
}

/// RTF's `\fs`: half points, at the size the settings chose for the conversation's words.
fn half_points(points: i32) -> i32 {
    points * 2 * crate::ui::text_scale() / 100
}

/// The fonts of a row: its own, and the Japanese one.
fn font_table(face: &str) -> String {
    format!("{{\\fonttbl{{\\f0\\fnil {};}}{{\\f1\\fnil\\fcharset128 {};}}}}", escape(face), crate::paint::japanese_face())
}

pub fn row(row: &Row) -> String {
    let (across, down) = row.padding;
    let left = if row.right { (row.column - row.width).max(0) } else { 0 };
    let text: Vec<String> = row.lines.iter().map(|line| words(line)).collect();
    format!(
        "{{\\rtf1\\ansi\\deff0{fonts}{{\\colortbl ;{back};{color};}}\\trowd\\trgaph{across}\\trleft{left}\\trpaddt{down}\\trpaddb{down}\\trpaddft3\\trpaddfb3{span}\\clcfpat1\\clcbpat1\\clshdng10000{cell_span}\\cellx{end}\\pard\\intbl\\ql\\sl0\\sa0\\sb0\\f0\\fs{size}\\cf2 {body}\\cell\\row}}",
        fonts = font_table(row.face),
        back = channels(row.back),
        color = channels(row.color),
        end = left + row.width,
        // A block keeps to the column as a share of it, so that it follows the window; a card is as wide as its words.
        span = if row.right { "" } else { "\\trftsWidth2\\trwWidth5000" },
        cell_span = if row.right { "" } else { "\\clftsWidth2\\clwWidth5000" },
        size = half_points(row.points),
        body = text.join("\\line "),
    )
}

/// Room in a card for its pictures, which lie over it as windows of their own: a row of this exact height, against the right edge as the card is,
/// with one blank character in it for the window to find its place by.
pub fn spacer_row(width: i32, column: i32, height: i32, back: u32, padding: (i32, i32)) -> String {
    let (across, down) = padding;
    let left = (column - width).max(0);
    format!(
        "{{\\rtf1\\ansi\\deff0{{\\fonttbl{{\\f0\\fnil {face};}}}}{{\\colortbl ;{back};}}\\trowd\\trgaph{across}\\trleft{left}\\trpaddt{down}\\trpaddb0\\trpaddft3\\trpaddfb3\\clcfpat1\\clcbpat1\\clshdng10000\\cellx{end}\\pard\\intbl\\ql\\sl-{height}\\slmult0\\sa0\\sb0\\f0\\fs2 \\u160?\\cell\\row}}",
        face = crate::paint::UI_FACE,
        back = channels(back),
        end = left + width,
    )
}

/// One line of a block in runs of colour, as `row` writes a block's line.
pub fn colored_row(runs: &[(String, u32)], face: &str, points: i32, back: u32, width: i32, padding: (i32, i32)) -> String {
    let (across, down) = padding;
    let mut colors: Vec<u32> = Vec::new();
    let mut body = String::new();
    for (text, color) in runs {
        let at = colors.iter().position(|known| known == color).unwrap_or_else(|| {
            colors.push(*color);
            colors.len() - 1
        });
        // In the table, 1 is the background and the rest follow it.
        body.push_str(&format!("\\cf{} {}", at + 2, words(text)));
    }
    let table: String = colors.iter().map(|color| format!("{};", channels(*color))).collect();
    format!(
        "{{\\rtf1\\ansi\\deff0{fonts}{{\\colortbl ;{back};{table}}}\\trowd\\trgaph{across}\\trleft0\\trpaddt{down}\\trpaddb{down}\\trpaddft3\\trpaddfb3\\trftsWidth2\\trwWidth5000\\clcfpat1\\clcbpat1\\clshdng10000\\clftsWidth2\\clwWidth5000\\cellx{width}\\pard\\intbl\\ql\\sl0\\sa0\\sb0\\f0\\fs{size} {body}\\cell\\row}}",
        fonts = font_table(face),
        back = channels(back),
        size = half_points(points),
    )
}

/// One row of a Markdown table: a cell for each column, with a line under it.
pub struct TableRow<'a> {
    pub cells: &'a [String],
    /// Where each column ends, in twips from the left.
    pub ends: &'a [i32],
    pub face: &'a str,
    pub points: i32,
    pub color: u32,
    pub back: u32,
    pub line: u32,
    pub bold: bool,
}

pub fn table_row(row: &TableRow) -> String {
    // (a line between two rows is the upper one's foot and the lower one's head: left unsaid, the head is black and wins)
    let frames: String = row.ends.iter().map(|end| format!("\\clbrdrt\\brdrs\\brdrw10\\brdrcf3\\clbrdrb\\brdrs\\brdrw10\\brdrcf3\\clcfpat1\\clcbpat1\\clshdng10000\\cellx{end}")).collect();
    let empty = String::new();
    let cells: String = (0..row.ends.len()).map(|at| format!("\\pard\\intbl\\ql\\sl0\\sa0\\sb0\\f0\\fs{}\\cf2{} {}\\cell", half_points(row.points), if row.bold { "\\b" } else { "" }, words(row.cells.get(at).unwrap_or(&empty)))).collect();
    format!(
        "{{\\rtf1\\ansi\\deff0{fonts}{{\\colortbl ;{back};{color};{line};}}\\trowd\\trgaph120\\trleft0\\trpaddt50\\trpaddb50\\trpaddft3\\trpaddfb3{frames}{cells}\\row}}",
        fonts = font_table(row.face),
        back = channels(row.back),
        color = channels(row.color),
        line = channels(row.line),
    )
}

/// The cells of a line of a Markdown table, without their marks; none when it is not one.
pub fn table_cells(line: &str) -> Option<Vec<String>> {
    let line = line.trim();
    let inner = line.strip_prefix('|')?;
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => cell.push(chars.next().unwrap_or('|')),
            '|' => cells.push(std::mem::take(&mut cell)),
            _ => cell.push(c),
        }
    }
    cells.push(cell);
    Some(cells.iter().map(|cell| clean_cell(cell.trim())).collect())
}

/// A cell has one font: the marks that would mean another go, a link keeps its words, and a line break is a space.
fn clean_cell(cell: &str) -> String {
    let mut text = cell.replace("**", "").replace('`', "").replace("<br>", " ").replace("<br/>", " ");
    while let Some(open) = text.find('[') {
        let crate::markdown::Link::Whole { text: words, length, .. } = crate::markdown::parse_link(&text[open..]) else { break };
        let words = words.to_string();
        text.replace_range(open..open + length, &words);
    }
    text
}

/// Whether these cells are the line under a table's heading: `|---|:--:|`.
pub fn is_table_rule(cells: &[String]) -> bool {
    !cells.is_empty() && cells.iter().all(|cell| cell.contains('-') && cell.chars().all(|c| c == '-' || c == ':'))
}

/// How wide a text is, in half-width characters: what is not plain ASCII takes two.
fn columns_of(text: &str) -> i32 {
    text.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

/// Where each column of these rows ends: each as wide as it needs, and all of them narrowed in step if the column they stand in is too narrow.
pub fn table_ends(rows: &[Vec<String>], points: i32, room: i32) -> Vec<i32> {
    const GAP: i32 = 300; // the cell's own margins and a little air
    let count = rows.iter().map(Vec::len).max().unwrap_or(0);
    let half_width = points * 13; // twips: a half-width character of the type, at its widest
    let wanted: Vec<i32> = (0..count).map(|at| rows.iter().filter_map(|row| row.get(at)).map(|cell| columns_of(cell)).max().unwrap_or(0).max(2) * half_width + GAP).collect();
    let total: i32 = wanted.iter().sum();
    let mut end = 0;
    wanted.iter().map(|width| {
        end += if total > room { (i64::from(*width) * i64::from(room) / i64::from(total.max(1))) as i32 } else { *width };
        end
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_line_is_told_and_cleaned() {
        assert_eq!(table_cells("| a | **b** | `c` |"), Some(vec!["a".to_string(), "b".to_string(), "c".to_string()]));
        assert_eq!(table_cells("| no end | x"), Some(vec!["no end".to_string(), "x".to_string()]));
        assert_eq!(table_cells(r"| a\|b | c |"), Some(vec!["a|b".to_string(), "c".to_string()]));
        assert_eq!(table_cells("| [a](http://b) | x<br>y |"), Some(vec!["a".to_string(), "x y".to_string()]));
        assert_eq!(table_cells("not a table"), None);
        assert!(is_table_rule(&table_cells("|---|:--:|--:|").unwrap()));
        assert!(!is_table_rule(&table_cells("| a | b |").unwrap()));
    }

    #[test]
    fn columns_are_as_wide_as_they_need_and_fit_the_room() {
        let rows = vec![vec!["ab".to_string(), "日本語の列".to_string()], vec!["a much longer cell".to_string(), "x".to_string()]];
        let ends = table_ends(&rows, 10, 100_000);
        assert_eq!(ends, vec![18 * 130 + 300, 18 * 130 + 300 + 10 * 130 + 300]);
        let narrow = table_ends(&rows, 10, 2000);
        assert!(*narrow.last().unwrap() <= 2000 && narrow[0] > 0 && narrow[0] < narrow[1], "{narrow:?}");
    }

    #[test]
    fn a_table_row_has_a_cell_for_each_column() {
        let cells = vec!["a{".to_string(), "あ".to_string()];
        let text = table_row(&TableRow { cells: &cells, ends: &[1000, 2000], face: "F", points: 10, color: 1, back: 2, line: 3, bold: true });
        assert_eq!(text.matches("\\cellx").count(), 2);
        assert!(text.contains("a\\{") && text.contains("{\\f1 \\u12354?}") && text.contains("\\b ") && text.ends_with("\\row}"), "{text}");
    }

    #[test]
    fn what_rtf_would_read_as_its_own_is_escaped() {
        assert_eq!(escape(r"a{b}\c"), r"a\{b\}\\c");
        assert_eq!(escape("\ttab"), "\\tab tab");
        assert_eq!(escape("あ"), "\\u12354?");
        assert_eq!(escape("😀"), "\\u-10179?\\u-8704?", "both halves, as signed numbers");
        assert_eq!(escape("a\r\u{7}b"), "ab");
    }

    #[test]
    fn what_is_japanese_has_its_own_face_and_the_rest_keeps_the_rows() {
        assert_eq!(words("abc 日本 12"), "abc {\\f1 \\u26085?\\u26412?} 12");
        assert_eq!(words("plain"), "plain");
        assert_eq!(words(""), "");
        let text = row(&Row { lines: &["あ"], face: "Consolas", points: 10, color: 0, back: 0, width: 100, right: false, column: 0, padding: (0, 0) });
        assert!(text.contains(&format!("{{\\f0\\fnil Consolas;}}{{\\f1\\fnil\\fcharset128 {};}}", crate::paint::japanese_face())), "{text}");
    }

    #[test]
    fn room_for_pictures_is_a_row_of_an_exact_height_against_the_right_edge() {
        let text = spacer_row(4000, 9000, 3000, 0x332211, (200, 100));
        assert!(text.contains("\\trleft5000") && text.contains("\\cellx9000") && text.contains("\\sl-3000\\slmult0"), "{text}");
        assert!(text.contains("\\red17\\green34\\blue51;"), "{text}");
        assert!(text.starts_with("{\\rtf1") && text.ends_with("\\row}"), "{text}");
    }

    #[test]
    fn a_row_says_its_colours_as_red_green_blue() {
        let text = row(&Row { lines: &["x {y}", "二行目"], face: "Consolas", points: 10, color: 0x0000FF, back: 0x332211, width: 9000, right: false, column: 0, padding: (120, 0) });
        assert!(text.contains("\\red17\\green34\\blue51;\\red255\\green0\\blue0;"), "{text}");
        assert!(text.contains("\\cellx9000") && text.contains("\\fs20") && text.contains("\\trleft0"), "{text}");
        assert!(text.contains("x \\{y\\}\\line {\\f1 \\u20108?\\u-30644?\\u30446?}\\cell\\row"), "{text}");
        let right = row(&Row { lines: &["x"], face: "F", points: 10, color: 0, back: 0, width: 100, right: true, column: 900, padding: (0, 0) });
        assert!(right.contains("\\trleft800") && right.contains("\\cellx900"), "ends where the column ends: {right}");
    }
}
