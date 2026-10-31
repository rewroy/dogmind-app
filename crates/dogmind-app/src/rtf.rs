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

pub fn row(row: &Row) -> String {
    let (across, down) = row.padding;
    let place = if row.right { "\\trqr" } else { "" };
    let text: Vec<String> = row.lines.iter().map(|line| escape(line)).collect();
    format!(
        "{{\\rtf1\\ansi\\deff0{{\\fonttbl{{\\f0\\fnil {face};}}}}{{\\colortbl ;{back};{color};}}\\trowd\\trgaph{across}\\trleft0{place}\\trpaddt{down}\\trpaddb{down}\\trpaddft3\\trpaddfb3{span}\\clcfpat1\\clcbpat1\\clshdng10000{cell_span}\\cellx{width}\\pard\\intbl\\ql\\sl0\\sa0\\sb0\\f0\\fs{size}\\cf2 {body}\\cell\\row}}",
        face = escape(row.face),
        back = channels(row.back),
        color = channels(row.color),
        width = row.width,
        // A block keeps to the column as a share of it, so that it follows the window; a card is as wide as its words.
        span = if row.right { "" } else { "\\trftsWidth2\\trwWidth5000" },
        cell_span = if row.right { "" } else { "\\clftsWidth2\\clwWidth5000" },
        size = row.points * 2,
        body = text.join("\\line "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_rtf_would_read_as_its_own_is_escaped() {
        assert_eq!(escape(r"a{b}\c"), r"a\{b\}\\c");
        assert_eq!(escape("\ttab"), "\\tab tab");
        assert_eq!(escape("あ"), "\\u12354?");
        assert_eq!(escape("😀"), "\\u-10179?\\u-8704?", "both halves, as signed numbers");
        assert_eq!(escape("a\r\u{7}b"), "ab");
    }

    #[test]
    fn a_row_says_its_colours_as_red_green_blue() {
        let text = row(&Row { lines: &["x {y}", "二行目"], face: "Consolas", points: 10, color: 0x0000FF, back: 0x332211, width: 9000, right: false, padding: (120, 0) });
        assert!(text.contains("\\red17\\green34\\blue51;\\red255\\green0\\blue0;"), "{text}");
        assert!(text.contains("\\cellx9000") && text.contains("\\fs20") && !text.contains("\\trqr"), "{text}");
        assert!(text.contains("x \\{y\\}\\line \\u20108?\\u-30644?\\u30446?\\cell\\row"), "{text}");
        let right = row(&Row { lines: &["x"], face: "F", points: 10, color: 0, back: 0, width: 100, right: true, padding: (0, 0) });
        assert!(right.contains("\\trqr"));
    }
}
