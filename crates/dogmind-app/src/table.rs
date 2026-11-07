//! A CSV or TSV file as a table in the preview: rows and columns as a spreadsheet shows them, with the first row as the header, rather than
//! the text with its commas. Fields are read as RFC 4180 has them (quotes around a field that holds the separator, a quote or a line end; a
//! doubled quote for a quote). Every character is escaped: the file may be anyone's. A large file is shown in part, and the page says so.

/// The rows and columns shown at most; past them the page says how many there were.
pub const MOST_ROWS: usize = 2000;
pub const MOST_COLUMNS: usize = 60;

/// The fields of `text`, row by row.
pub fn rows(text: &str, separator: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let (mut row, mut field) = (Vec::new(), String::new());
    let (mut quoted, mut chars) = (false, text.chars().peekable());
    let mut any = false;
    while let Some(c) = chars.next() {
        any = true;
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            c if c == separator => row.push(std::mem::take(&mut field)),
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                any = false;
            }
            _ => field.push(c),
        }
    }
    if any || !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The table's HTML, and a line under it when rows or columns were left out (`more`: its words, given the counts of rows and columns).
pub fn html(text: &str, separator: char, more: &dyn Fn(usize, usize) -> String) -> String {
    let rows = rows(text, separator);
    html_of(&rows, rows.len(), more)
}

/// The same of rows read already (a workbook's sheet), of `total` rows in all.
pub fn html_of(rows: &[Vec<String>], total: usize, more: &dyn Fn(usize, usize) -> String) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut out = String::from("<table class=\"data\">");
    for (at, row) in rows.iter().take(MOST_ROWS).enumerate() {
        let cell = if at == 0 { "th" } else { "td" };
        out.push_str("<tr>");
        for column in 0..columns.min(MOST_COLUMNS) {
            let value = row.get(column).map_or("", String::as_str);
            // (a number reads to the right, as a spreadsheet keeps it)
            let number = at > 0 && !value.is_empty() && value.trim().parse::<f64>().is_ok();
            out.push_str(&format!("<{cell}{}>{}</{cell}>", if number { " class=\"n\"" } else { "" }, escape(value)));
        }
        out.push_str("</tr>");
    }
    out.push_str("</table>");
    if total > MOST_ROWS.min(rows.len()) || columns > MOST_COLUMNS {
        out.push_str(&format!("<p class=\"cut\">{}</p>", escape(&more(total, columns))));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_are_read_as_rfc_4180_has_them() {
        let text = "name,note,n\r\n\"Smith, J\",\"said \"\"hi\"\"\",3\nlast,\"two\nlines\",4.5";
        assert_eq!(rows(text, ','), [vec!["name", "note", "n"], vec!["Smith, J", "said \"hi\"", "3"], vec!["last", "two\nlines", "4.5"]]);
        assert_eq!(rows("a\tb\n1\t2\n", '\t'), [vec!["a", "b"], vec!["1", "2"]]);
        assert!(rows("", ',').is_empty());
    }

    #[test]
    fn the_table_escapes_every_field_and_says_what_was_left_out() {
        let page = html("h1,h2\n<script>,7\n", ',', &|rows, columns| format!("{rows} rows, {columns} columns"));
        assert!(page.contains("<th>h1</th>") && page.contains("<td>&lt;script&gt;</td>") && page.contains("<td class=\"n\">7</td>"), "{page}");
        assert!(!page.contains("class=\"cut\""));
        let many: String = (0..MOST_ROWS + 5).map(|row| format!("{row}\n")).collect();
        assert!(html(&many, ',', &|rows, _| format!("{rows} rows")).contains(&format!("{} rows", MOST_ROWS + 5)));
    }
}
