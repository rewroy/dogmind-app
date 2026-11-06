//! An Excel workbook (`.xlsx`) in the preview, as a table of its first sheet, with the names of the others. A workbook is a zip of XML: the zip
//! is read here from its central directory, each part inflated in memory and never written anywhere, and nothing of it is run or followed (no
//! formula is computed: a cell shows the value Excel saved for it). The file may be anyone's, so every count and size is bounded: a part that
//! would inflate past [`MOST_PART`] is refused, as is a zip of more than [`MOST_ENTRIES`] entries.

use std::io::Read;

/// The most a part may inflate to, and the most entries a workbook's zip may have.
const MOST_PART: usize = 64 << 20;
const MOST_ENTRIES: usize = 10_000;
/// What a cell shows at most, what all the cells shown come to at most, and what the shared strings kept come to at most (one cell's
/// string can stand in many cells: the page is bounded by what is shown, not by the parts).
const MOST_CELL: usize = 2048;
const MOST_SHOWN: usize = 1 << 20;
const MOST_STRINGS: usize = 16 << 20;

/// A zip's entries by name: where each one's data begins, its sizes and how it is stored.
struct Entry {
    name: String,
    offset: usize,
    compressed: usize,
    size: usize,
    method: u16,
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// The entries of a zip, from its end of central directory record (the last one in the file's last 64 KiB and 22 bytes).
fn entries(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    let start = bytes.len().saturating_sub(65_557);
    let end = (start..bytes.len().saturating_sub(21)).rev().find(|&at| u32_at(bytes, at) == Some(0x0605_4b50)).ok_or("not a zip")?;
    let count = u16_at(bytes, end + 10).ok_or("not a zip")? as usize;
    if count > MOST_ENTRIES {
        return Err("a zip of too many parts".into());
    }
    let mut at = u32_at(bytes, end + 16).ok_or("not a zip")? as usize;
    let mut found = Vec::with_capacity(count);
    for _ in 0..count {
        if u32_at(bytes, at) != Some(0x0201_4b50) {
            return Err("a zip whose directory is not whole".into());
        }
        let method = u16_at(bytes, at + 10).ok_or("cut short")?;
        let compressed = u32_at(bytes, at + 20).ok_or("cut short")? as usize;
        let size = u32_at(bytes, at + 24).ok_or("cut short")? as usize;
        let (name_length, extra, comment) = (u16_at(bytes, at + 28).ok_or("cut short")? as usize, u16_at(bytes, at + 30).ok_or("cut short")? as usize,
                                             u16_at(bytes, at + 32).ok_or("cut short")? as usize);
        let local = u32_at(bytes, at + 42).ok_or("cut short")? as usize;
        let name = String::from_utf8_lossy(bytes.get(at + 46..at + 46 + name_length).ok_or("cut short")?).into_owned();
        // (the data follows the local header, whose name and extra field may differ in length from the directory's)
        if u32_at(bytes, local) != Some(0x0403_4b50) {
            return Err("a zip whose parts are not where it says".into());
        }
        let data = local + 30 + u16_at(bytes, local + 26).ok_or("cut short")? as usize + u16_at(bytes, local + 28).ok_or("cut short")? as usize;
        found.push(Entry { name, offset: data, compressed, size, method });
        at += 46 + name_length + extra + comment;
    }
    Ok(found)
}

/// A part's bytes, inflated: stored or deflated, and no larger than [`MOST_PART`] whatever its header says.
fn part(bytes: &[u8], entries: &[Entry], name: &str) -> Result<Option<Vec<u8>>, String> {
    let Some(entry) = entries.iter().find(|entry| entry.name == name) else { return Ok(None) };
    if entry.size > MOST_PART {
        return Err(format!("{name} is too large to show"));
    }
    let data = bytes.get(entry.offset..entry.offset + entry.compressed).ok_or("a zip cut short")?;
    match entry.method {
        0 => Ok(Some(data.to_vec())),
        8 => {
            let mut out = Vec::with_capacity(entry.size.min(MOST_PART));
            flate2::read::DeflateDecoder::new(data).take(MOST_PART as u64 + 1).read_to_end(&mut out).map_err(|error| format!("{name}: {error}"))?;
            if out.len() > MOST_PART {
                return Err(format!("{name} is too large to show"));
            }
            Ok(Some(out))
        }
        other => Err(format!("{name} is stored in a way this cannot read ({other})")),
    }
}

/// An XML element as the reader meets it: its name without a prefix, its attributes, and whether it closes at once; or text between tags.
enum Item<'a> {
    Open(&'a str, Vec<(&'a str, String)>, bool),
    Close(&'a str),
    Text(String),
}

fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        // (an entity is a few characters: looking further for its `;` would read the rest of the text again at every `&`)
        let Some(end) = rest.as_bytes().iter().take(12).position(|byte| *byte == b';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let c = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if entity.starts_with("#x") => u32::from_str_radix(&entity[2..], 16).ok().and_then(char::from_u32),
            _ if entity.starts_with('#') => entity[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn local(name: &str) -> &str {
    name.rsplit_once(':').map_or(name, |(_, local)| local)
}

/// The items of an XML text, in order, one at a time (enough of XML for a workbook's parts: no DTD, no CDATA in them): a reader that has
/// what it wants stops, and a part's items are never all held at once.
fn items(xml: &str) -> Items<'_> {
    Items { rest: xml }
}

struct Items<'a> {
    rest: &'a str,
}

impl<'a> Iterator for Items<'a> {
    type Item = Item<'a>;

    fn next(&mut self) -> Option<Item<'a>> {
        loop {
            if self.rest.is_empty() {
                return None;
            }
            let Some(open) = self.rest.find('<') else {
                let text = unescape(self.rest);
                self.rest = "";
                return Some(Item::Text(text));
            };
            if open > 0 {
                let text = unescape(&self.rest[..open]);
                self.rest = &self.rest[open..];
                return Some(Item::Text(text));
            }
            let Some(close) = self.rest.find('>') else {
                self.rest = "";
                return None;
            };
            let tag = &self.rest[1..close];
            self.rest = &self.rest[close + 1..];
            if tag.starts_with('?') || tag.starts_with('!') {
                continue;
            }
            if let Some(name) = tag.strip_prefix('/') {
                return Some(Item::Close(local(name.trim())));
            }
            let empty = tag.ends_with('/');
            let tag = tag.trim_end_matches('/');
            let (name, mut attributes_text) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
            let mut attributes = Vec::new();
            while let Some(equals) = attributes_text.find('=') {
                let key = attributes_text[..equals].trim();
                let after = attributes_text[equals + 1..].trim_start();
                let Some(quote) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else { break };
                let Some(end) = after[1..].find(quote) else { break };
                attributes.push((local(key), unescape(&after[1..1 + end])));
                attributes_text = &after[end + 2..];
            }
            return Some(Item::Open(local(name), attributes, empty));
        }
    }
}

fn attribute<'a>(attributes: &'a [(&str, String)], name: &str) -> Option<&'a str> {
    attributes.iter().find(|(key, _)| *key == name).map(|(_, value)| value.as_str())
}

/// The shared strings: each `si`'s text, its runs joined.
fn shared_strings(xml: &str) -> Vec<String> {
    let (mut strings, mut current, mut inside, mut in_text, mut phonetic) = (Vec::new(), String::new(), false, false, false);
    let mut kept = 0usize;
    for item in items(xml) {
        match item {
            Item::Open("rPh", _, empty) => phonetic = !empty,
            Item::Close("rPh") => phonetic = false,
            Item::Open("si", _, empty) => {
                inside = true;
                current.clear();
                if empty {
                    strings.push(String::new());
                    inside = false;
                }
            }
            Item::Close("si") => {
                // (past what is kept, a string is kept empty: the cells that name it still find their place)
                kept += current.len();
                strings.push(if kept > MOST_STRINGS { String::new() } else { std::mem::take(&mut current) });
                current.clear();
                inside = false;
            }
            // (phonetic runs, a Japanese reading of the text, are not the text)
            Item::Open("t", _, false) if inside && !phonetic => in_text = true,
            Item::Close("t") => in_text = false,
            Item::Text(text) if inside && in_text && current.len() < MOST_CELL => current.push_str(&text),
            _ => {}
        }
    }
    strings
}

/// A column's number from a cell reference (`A1` is 0, `AA7` is 26).
fn column_of(reference: &str) -> usize {
    reference.bytes().take_while(u8::is_ascii_alphabetic).fold(0usize, |number, letter| number * 26 + (letter.to_ascii_uppercase() - b'A') as usize + 1).saturating_sub(1)
}

/// A sheet's rows of values: shared strings looked up, inline strings, numbers and booleans as Excel saved them, and empty cells in their place.
fn sheet_rows(xml: &str, strings: &[String], most_rows: usize, most_columns: usize) -> (Vec<Vec<String>>, usize) {
    let (mut rows, mut row, mut total): (Vec<Vec<String>>, Vec<String>, usize) = (Vec::new(), Vec::new(), 0);
    let (mut column, mut kind, mut value, mut in_value) = (0usize, String::new(), String::new(), false);
    let mut shown_bytes = 0usize;
    for item in items(xml) {
        // (what will be shown is all read: the rest of the sheet is only counted, by its rows)
        if rows.len() >= most_rows || shown_bytes >= MOST_SHOWN {
            if let Item::Close("row") = item {
                total += 1;
            }
            continue;
        }
        match item {
            Item::Open("row", _, empty) => {
                row.clear();
                if empty {
                    total += 1;
                    if rows.len() < most_rows {
                        rows.push(Vec::new());
                    }
                }
            }
            Item::Close("row") => {
                total += 1;
                if rows.len() < most_rows {
                    rows.push(std::mem::take(&mut row));
                }
            }
            Item::Open("c", attributes, empty) => {
                column = attribute(&attributes, "r").map_or(row.len(), column_of);
                kind = attribute(&attributes, "t").unwrap_or("n").to_string();
                value.clear();
                if empty && column < most_columns {
                    while row.len() <= column {
                        row.push(String::new());
                    }
                }
            }
            Item::Open("v" | "t", _, false) => in_value = true,
            Item::Close("v" | "t") => in_value = false,
            Item::Text(text) if in_value && value.len() < MOST_CELL => value.push_str(&text),
            Item::Close("c") => {
                if column < most_columns {
                    let mut shown = match kind.as_str() {
                        "s" => value.trim().parse::<usize>().ok().and_then(|at| strings.get(at)).cloned().unwrap_or_default(),
                        "b" => if value.trim() == "1" { "TRUE".to_string() } else { "FALSE".to_string() },
                        _ => std::mem::take(&mut value),
                    };
                    if shown.len() > MOST_CELL {
                        let mut cut = MOST_CELL;
                        while !shown.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        shown.truncate(cut);
                        shown.push('…');
                    }
                    shown_bytes += shown.len();
                    while row.len() < column {
                        row.push(String::new());
                    }
                    if row.len() == column {
                        row.push(shown);
                    }
                }
                value.clear();
            }
            _ => {}
        }
    }
    (rows, total)
}

/// The sheets' names and the part of each, in the workbook's order.
fn sheets(workbook: &str, relations: &str) -> Vec<(String, String)> {
    let mut targets = std::collections::HashMap::new();
    for item in items(relations) {
        if let Item::Open("Relationship", attributes, _) = item {
            if let (Some(id), Some(target)) = (attribute(&attributes, "Id"), attribute(&attributes, "Target")) {
                let target = target.trim_start_matches('/');
                let part = if target.starts_with("xl/") { target.to_string() } else { format!("xl/{target}") };
                targets.insert(id.to_string(), part);
            }
        }
    }
    items(workbook).into_iter().filter_map(|item| match item {
        Item::Open("sheet", attributes, _) => {
            let name = attribute(&attributes, "name")?.to_string();
            let part = targets.get(attribute(&attributes, "id")?)?.clone();
            Some((name, part))
        }
        _ => None,
    }).collect()
}

/// A workbook's first sheet as rows, the names of all its sheets, and how many rows the first has in all.
pub fn first_sheet(bytes: &[u8], most_rows: usize, most_columns: usize) -> Result<(Vec<Vec<String>>, Vec<String>, usize), String> {
    let entries = entries(bytes)?;
    let text = |name: &str| -> Result<Option<String>, String> { Ok(part(bytes, &entries, name)?.map(|bytes| String::from_utf8_lossy(&bytes).into_owned())) };
    let workbook = text("xl/workbook.xml")?.ok_or("not an Excel workbook")?;
    let relations = text("xl/_rels/workbook.xml.rels")?.unwrap_or_default();
    let sheets = sheets(&workbook, &relations);
    let first = sheets.first().map_or("xl/worksheets/sheet1.xml".to_string(), |(_, part)| part.clone());
    let strings = text("xl/sharedStrings.xml")?.map_or(Vec::new(), |xml| shared_strings(&xml));
    let sheet = text(&first)?.ok_or("the workbook's first sheet is missing")?;
    let (rows, total) = sheet_rows(&sheet, &strings, most_rows, most_columns);
    Ok((rows, sheets.into_iter().map(|(name, _)| name).collect(), total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A zip of `parts`, the first stored and the rest deflated, as Excel's are.
    fn zip(parts: &[(&str, &str)]) -> Vec<u8> {
        let (mut out, mut directory) = (Vec::new(), Vec::new());
        for (at, (name, text)) in parts.iter().enumerate() {
            let (method, data) = if at == 0 { (0u16, text.as_bytes().to_vec()) } else {
                let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(text.as_bytes()).unwrap();
                (8u16, encoder.finish().unwrap())
            };
            let offset = out.len() as u32;
            for piece in [&0x0403_4b50u32.to_le_bytes()[..], &20u16.to_le_bytes(), &0u16.to_le_bytes(), &method.to_le_bytes(), &[0; 8], &(data.len() as u32).to_le_bytes(),
                          &(text.len() as u32).to_le_bytes(), &(name.len() as u16).to_le_bytes(), &0u16.to_le_bytes(), name.as_bytes(), &data] {
                out.extend_from_slice(piece);
            }
            for piece in [&0x0201_4b50u32.to_le_bytes()[..], &20u16.to_le_bytes(), &20u16.to_le_bytes(), &0u16.to_le_bytes(), &method.to_le_bytes(), &[0; 8],
                          &(data.len() as u32).to_le_bytes(), &(text.len() as u32).to_le_bytes(), &(name.len() as u16).to_le_bytes(), &[0; 12], &offset.to_le_bytes(), name.as_bytes()] {
                directory.extend_from_slice(piece);
            }
        }
        let start = out.len() as u32;
        out.extend_from_slice(&directory);
        for piece in [&0x0605_4b50u32.to_le_bytes()[..], &[0; 4], &(parts.len() as u16).to_le_bytes(), &(parts.len() as u16).to_le_bytes(),
                      &(directory.len() as u32).to_le_bytes(), &start.to_le_bytes(), &[0; 2]] {
            out.extend_from_slice(piece);
        }
        out
    }

    #[test]
    fn the_first_sheet_reads_as_excel_saved_it() {
        let workbook = r#"<?xml version="1.0"?><workbook xmlns:r="x"><sheets><sheet name="売上" sheetId="1" r:id="rId1"/><sheet name="Notes" sheetId="2" r:id="rId2"/></sheets></workbook>"#;
        let relations = r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Target="worksheets/sheet2.xml"/></Relationships>"#;
        let strings = r#"<sst><si><t>Name</t></si><si><r><t>To</t></r><r><t>tal</t></r></si><si><t>A &amp; B</t><rPh><t>エイ</t></rPh></si></sst>"#;
        let sheet = r#"<worksheet><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="C1" t="s"><v>1</v></c></row><row r="2"><c r="A2" t="s"><v>2</v></c><c r="B2" t="b"><v>1</v></c><c r="C2"><v>12.5</v></c></row><row r="3"><c r="A3" t="inlineStr"><is><t>inline</t></is></c></row></sheetData></worksheet>"#;
        let book = zip(&[("[Content_Types].xml", "<Types/>"), ("xl/workbook.xml", workbook), ("xl/_rels/workbook.xml.rels", relations),
                         ("xl/sharedStrings.xml", strings), ("xl/worksheets/sheet1.xml", sheet)]);
        let (rows, names, total) = first_sheet(&book, 100, 10).unwrap();
        assert_eq!(names, ["売上", "Notes"]);
        assert_eq!(total, 3);
        assert_eq!(rows, [vec!["Name", "", "Total"], vec!["A & B", "TRUE", "12.5"], vec!["inline"]]);
    }

    /// One shared string named by every cell shows a cell's worth in each, and the rows past those wanted are only counted; an `&` that is
    /// no entity stays as it is, and is not looked past.
    #[test]
    fn what_is_shown_is_bounded_whatever_the_parts_hold() {
        let long = "x".repeat(MOST_CELL * 3);
        let strings = format!("<sst><si><t>{long}</t></si></sst>");
        let cells: String = (0..50).map(|row| format!("<row><c r=\"A{}\" t=\"s\"><v>0</v></c></row>", row + 1)).collect();
        let sheet = format!("<worksheet><sheetData>{cells}</sheetData></worksheet>");
        let (rows, total) = sheet_rows(&sheet, &shared_strings(&strings), 10, 5);
        assert_eq!((rows.len(), total), (10, 50));
        assert!(rows.iter().all(|row| row[0].len() <= MOST_CELL + '…'.len_utf8()));
        assert_eq!(unescape("a & b &amp; c &#x41;"), "a & b & c A");
        assert_eq!(unescape(&format!("&{}", "y".repeat(100))), format!("&{}", "y".repeat(100)));
    }

    #[test]
    fn what_is_no_workbook_is_refused() {
        assert!(first_sheet(b"plain text, no zip", 10, 10).is_err());
        assert!(first_sheet(&zip(&[("a.txt", "x")]), 10, 10).is_err());
        assert_eq!(column_of("AA7"), 26);
        assert_eq!(column_of("A1"), 0);
    }
}
