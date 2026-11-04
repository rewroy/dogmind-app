//! Markdown as this app reads it: what the transcript needs to tell about a line as it streams, and a whole file as a page for the preview. Nothing a file
//! holds reaches the page as HTML of its own: every character is escaped, and only what is written here makes a tag.

use crate::highlight::{self, Kind};

/// What stands at a `[`, when what follows may still be coming.
#[derive(Debug, PartialEq, Eq)]
pub enum Link<'a> {
    Whole { text: &'a str, url: &'a str, length: usize },
    /// It may yet be one: more of the line tells.
    Waiting,
    No,
}

/// How much of a line a link, or an address, may take. Nothing past it is looked at: a line of a hundred thousand `[` was read to its end for each of them.
const MOST: usize = 2000;

/// The first `most` bytes of `text`, to a character's edge, and whether there was more.
fn first_bytes(text: &str, most: usize) -> (&str, bool) {
    let mut end = most.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], end < text.len())
}

/// `rest` begins at the `[`.
pub fn parse_link(rest: &str) -> Link<'_> {
    // (the limit first: each scan below is of this much at the most, so the whole of a long line is read once for all its brackets and not once for each)
    let (rest, cut) = first_bytes(rest, MOST);
    // What is not there to find: a line that goes on past the limit is no link, and one that ends may still be given more.
    let missing = |inside: &str| if inside.contains('\n') || cut { Link::No } else { Link::Waiting };
    let Some(close) = rest.find(']') else { return missing(rest) };
    if rest[..close].contains('\n') || close == 1 {
        return Link::No;
    }
    let after = &rest[close + 1..];
    if after.is_empty() {
        return missing("");
    }
    if !after.starts_with('(') {
        return Link::No;
    }
    // `(<a path with spaces>)`, as CommonMark writes an address that has them
    if let Some(inside) = after[1..].strip_prefix('<') {
        let Some(close_angle) = inside.find('>') else { return missing(inside) };
        let url = &inside[..close_angle];
        if url.is_empty() || url.contains('\n') {
            return Link::No;
        }
        let tail = &inside[close_angle + 1..];
        let Some(end) = tail.find(')') else { return missing(tail) };
        return Link::Whole { text: &rest[1..close], url, length: close + 1 + 2 + close_angle + 1 + end + 1 };
    }
    let Some(end) = after.find(')') else { return missing(after) };
    // (a title after the address is left out)
    let Some(url) = after[1..end].split_whitespace().next() else { return Link::No };
    Link::Whole { text: &rest[1..close], url, length: close + 1 + end + 1 }
}

/// What stands at a bare `http`.
#[derive(Debug, PartialEq, Eq)]
pub enum BareUrl {
    /// This many bytes of it are the address.
    Whole(usize),
    Waiting,
    No,
}

pub fn bare_url(rest: &str) -> BareUrl {
    let is_start = rest.starts_with("http://") || rest.starts_with("https://");
    if !is_start {
        return if rest.len() < 8 && ("https://".starts_with(rest) || "http://".starts_with(rest)) { BareUrl::Waiting } else { BareUrl::No };
    }
    let end = rest.find(|c: char| !c.is_ascii_graphic() || matches!(c, '<' | '>' | '"' | '`'));
    let Some(end) = end else {
        return if rest.len() > MOST { BareUrl::Whole(rest.len()) } else { BareUrl::Waiting };
    };
    let mut url = &rest[..end];
    // What ends the sentence or the bracket around it is not in the address, unless the address opened that bracket itself. The brackets are counted once and as they go, not
    // again for each character that is trimmed: a thousand `)` at the end of an address was a thousand counts of all of it.
    let count = |mark: char| url.matches(mark).count() as isize;
    let (mut parens, mut squares) = (count('(') - count(')'), count('[') - count(']'));
    loop {
        let last = url.chars().last().unwrap_or(' ');
        let unbalanced = |open_minus_close: isize, close: char| last == close && open_minus_close < 0;
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*' | '_') || unbalanced(parens, ')') || unbalanced(squares, ']') {
            match last {
                ')' => parens += 1,
                ']' => squares += 1,
                _ => {}
            }
            url = &url[..url.len() - last.len_utf8()];
        } else {
            break;
        }
    }
    if url.ends_with("://") { BareUrl::No } else { BareUrl::Whole(url.len()) }
}

/// An address to give the browser. Nothing else a model wrote is handed to the system to open.
pub fn web_address(url: &str) -> Option<String> {
    let lower = url.trim().to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://")).then(|| url.trim().to_string())
}

/// Whether a lone `*` opens emphasis here: `2 * 3` and `5*3` do not.
pub fn opens_emphasis(before: Option<char>, after: char) -> bool {
    !after.is_whitespace() && after != '*' && !before.is_some_and(|c| c.is_ascii_alphanumeric())
}

/// What a backslash can take the meaning from.
pub fn is_escapable(c: char) -> bool {
    matches!(c, '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '#' | '~' | '|' | '>' | '-' | '+' | '.' | '!')
}

/// `# a` is a heading, of this level; `#include` and `#1 の件` are not.
pub fn heading_level(line: &str) -> Option<u8> {
    let marks = line.chars().take_while(|c| *c == '#').count();
    ((1..=6).contains(&marks) && line[marks..].starts_with(' ')).then_some(marks as u8)
}

/// A line that is all rule: `---`, `***`, `___`, with spaces if it likes.
pub fn is_rule(line: &str) -> bool {
    let marks: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    marks.len() >= 3 && ["-", "*", "_"].iter().any(|mark| marks.chars().all(|c| mark.starts_with(c)))
}

/// What is quoted on a line of a quotation.
pub fn quoted(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// Whether the start of a line, with no line's end yet, cannot be told from a mark that means something: it waits for more.
pub fn undecided(head: &str) -> bool {
    let count = head.chars().count();
    if head.chars().all(|c| matches!(c, '-' | '*' | '_' | ' ')) {
        return true;
    }
    if head.chars().all(|c| c == '#') {
        return count <= 6;
    }
    if head.starts_with("- [") && count < 6 {
        return true;
    }
    count < 4 && head.chars().all(|c| c.is_ascii_digit() || matches!(c, '`' | '#' | '-' | '*' | '.' | '>' | ' '))
}

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut before = None;
    while let Some(c) = chars.next() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            // (what acts on the text and shows nothing is shown: as its mark, with the brackets escaped like any)
            hidden if is_shown_as_mark(before, hidden, chars.peek().copied()) => out.push_str(&format!("&lt;U+{:04X}&gt;", hidden as u32)),
            other => out.push(other),
        }
        before = Some(c);
    }
    out
}

/// An address fit for a page: to the web or in the file's own folder, and not `javascript:` or the like.
fn page_address(url: &str) -> String {
    let scheme = url.split_once(':').map(|(before, _)| before).filter(|before| !before.contains(['/', '?', '#', '.']));
    match scheme {
        Some(scheme) if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") => "#".to_string(),
        _ => escape(url),
    }
}

/// How deep quotations and lists nest in a page. A model's answer, or a file it wrote, is not trusted to stop: a line of a hundred thousand `>` is a hundred thousand levels, each one a
/// call, and the process ended with its stack. No real answer is near this; what is deeper is shown as the words it is, at the level that is reached.
const MOST_NESTING: usize = 32;

/// What is left, as one paragraph of its own words.
fn plain_rest(lines: &[&str], out: &mut String) {
    out.push_str("<p>");
    for (at, line) in lines.iter().enumerate() {
        if at > 0 {
            out.push('\n');
        }
        out.push_str(&escape_text(line));
    }
    out.push_str("</p>");
}

/// What the person sees of the characters that act on the text and show nothing (`is_hidden`): the mark for each, as words.
pub fn hidden_marker(c: char) -> String {
    format!("<U+{:04X}>", c as u32)
}

/// Whether this character changes how the text around it reads while showing nothing itself: the direction controls (a right-to-left override shows the rest of a line backwards, so that
/// what is shown is not what was written), the zero width characters and marks, the line and paragraph separators, and the byte order mark.
pub fn is_hidden(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{2028}' | '\u{2029}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// Whether a character is a picture a joiner may join: the emoji blocks, the symbols many emoji are in, and the selector that asks for the picture.
fn is_emoji_like(c: char) -> bool {
    matches!(c, '\u{1F000}'..='\u{1FAFF}' | '\u{2190}'..='\u{21FF}' | '\u{2300}'..='\u{23FF}' | '\u{2600}'..='\u{27BF}' | '\u{2900}'..='\u{297F}' | '\u{2B00}'..='\u{2BFF}' | '\u{3030}' | '\u{303D}' | '\u{3297}' | '\u{3299}' | '\u{FE0F}' | '\u{00A9}' | '\u{00AE}' | '\u{200D}')
}

/// Whether `c` is to be shown as a mark, between these two characters: a zero width joiner that joins two emoji is what makes them one picture, and is left alone. A joiner at the very edge
/// of what is looked at, with nothing known of what is beside it, is left alone too.
pub fn is_shown_as_mark(before: Option<char>, c: char, after: Option<char>) -> bool {
    if !is_hidden(c) {
        return false;
    }
    !(c == '\u{200D}' && (before.is_none() || after.is_none() || (before.is_some_and(is_emoji_like) && after.is_some_and(is_emoji_like))))
}

/// Whether any character of the text is to be shown as a mark.
pub fn has_marks(text: &str) -> bool {
    let mut chars = text.chars().peekable();
    let mut before = None;
    while let Some(c) = chars.next() {
        if is_shown_as_mark(before, c, chars.peek().copied()) {
            return true;
        }
        before = Some(c);
    }
    false
}

/// The text cut into what is shown as it is, and the marks that stand for its hidden characters, each with whether it is one: `("a", false), ("<U+202E>", true), ("b", false)`.
pub fn with_marks(text: &str) -> Vec<(String, bool)> {
    let mut parts: Vec<(String, bool)> = Vec::new();
    let mut chars = text.chars().peekable();
    let mut before = None;
    while let Some(c) = chars.next() {
        if is_shown_as_mark(before, c, chars.peek().copied()) {
            parts.push((hidden_marker(c), true));
        } else {
            match parts.last_mut() {
                Some((held, false)) => held.push(c),
                _ => parts.push((c.to_string(), false)),
            }
        }
        before = Some(c);
    }
    parts
}

/// The text with each hidden character written as its mark (`<U+202E>`), where a person reads plain text.
pub fn show_hidden(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.chars().any(is_hidden) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut chars = text.chars().peekable();
    let mut before = None;
    while let Some(c) = chars.next() {
        if is_shown_as_mark(before, c, chars.peek().copied()) {
            out.push_str(&hidden_marker(c));
        } else {
            out.push(c);
        }
        before = Some(c);
    }
    std::borrow::Cow::Owned(out)
}

/// `escape` for words that stand between tags: a hidden character is its mark, in the muted colour of the page.
pub fn escape_text(text: &str) -> String {
    if !text.chars().any(is_hidden) {
        return escape(text);
    }
    let mut out = String::with_capacity(text.len() + 32);
    let mut chars = text.chars().peekable();
    let mut before = None;
    let mut plain = String::new();
    while let Some(c) = chars.next() {
        if is_shown_as_mark(before, c, chars.peek().copied()) {
            out.push_str(&escape(&plain));
            plain.clear();
            out.push_str(&format!("<span class=\"u\">{}</span>", escape(&hidden_marker(c))));
        } else {
            plain.push(c);
        }
        before = Some(c);
    }
    out.push_str(&escape(&plain));
    out
}

/// A line's text as HTML: emphasis, code, links, pictures.
pub fn inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let (mut bold, mut italic, mut struck) = (false, false, false);
    let mut before: Option<char> = None;
    let mut at = 0;
    while let Some(c) = text[at..].chars().next() {
        let rest = &text[at..];
        let next = rest[c.len_utf8()..].chars().next();
        match c {
            '\\' if next.is_some_and(is_escapable) => {
                let taken = next.unwrap_or(' ');
                out.push_str(&escape(&taken.to_string()));
                at += 1 + taken.len_utf8();
                before = Some(taken);
                continue;
            }
            '`' => {
                if let Some(end) = rest[1..].find('`') {
                    out.push_str(&format!("<code>{}</code>", escape(&rest[1..1 + end])));
                    at += end + 2;
                    before = Some('`');
                    continue;
                }
            }
            '*' if next == Some('*') => {
                out.push_str(if bold { "</strong>" } else { "<strong>" });
                bold = !bold;
                at += 2;
                continue;
            }
            '*' if italic && before.is_some_and(|c| !c.is_whitespace()) => {
                out.push_str("</em>");
                italic = false;
                at += 1;
                continue;
            }
            '*' if !italic && next.is_some_and(|after| opens_emphasis(before, after)) => {
                out.push_str("<em>");
                italic = true;
                at += 1;
                continue;
            }
            '~' if next == Some('~') => {
                out.push_str(if struck { "</del>" } else { "<del>" });
                struck = !struck;
                at += 2;
                continue;
            }
            '!' if next == Some('[') => {
                if let Link::Whole { text: alt, url, length } = parse_link(&rest[1..]) {
                    out.push_str(&format!("<img src=\"{}\" alt=\"{}\">", page_address(url), escape(alt)));
                    at += 1 + length;
                    before = Some(')');
                    continue;
                }
            }
            '[' => {
                if let Link::Whole { text: words, url, length } = parse_link(rest) {
                    out.push_str(&format!("<a href=\"{}\">{}</a>", page_address(url), inline(words)));
                    at += length;
                    before = Some(')');
                    continue;
                }
            }
            'h' if !before.is_some_and(|c| c.is_ascii_alphanumeric()) => {
                // (at the end of the text there is no more to come, so what would wait is read with an end after it)
                let measured = match bare_url(rest) {
                    BareUrl::Waiting => bare_url(&format!("{rest} ")),
                    other => other,
                };
                if let BareUrl::Whole(length) = measured {
                    let url = escape(&rest[..length]);
                    out.push_str(&format!("<a href=\"{url}\">{url}</a>"));
                    at += length;
                    before = Some('/');
                    continue;
                }
            }
            _ => {}
        }
        if is_shown_as_mark(before, c, next) {
            out.push_str(&format!("<span class=\"u\">{}</span>", escape(&hidden_marker(c))));
        } else {
            out.push_str(&escape(&c.to_string()));
        }
        before = Some(c);
        at += c.len_utf8();
    }
    for (open, close) in [(struck, "</del>"), (italic, "</em>"), (bold, "</strong>")] {
        if open {
            out.push_str(close);
        }
    }
    out
}

fn class_of(kind: Kind) -> Option<&'static str> {
    match kind {
        Kind::Plain => None,
        Kind::Keyword => Some("k"),
        Kind::Text => Some("s"),
        Kind::Comment => Some("c"),
        Kind::Number => Some("n"),
    }
}

/// One line of code, in the colours of its language when it has any. Returns whether a block comment runs on past it.
fn code_line(line: &str, language: &str, open: bool, out: &mut String) -> bool {
    let Some((runs, still_open)) = highlight::runs(line, language, open) else {
        out.push_str(&escape_text(line));
        return false;
    };
    for (text, kind) in runs {
        match class_of(kind) {
            Some(class) => out.push_str(&format!("<span class=\"{class}\">{}</span>", escape_text(&text))),
            None => out.push_str(&escape_text(&text)),
        }
    }
    still_open
}

/// A whole source file: its lines numbered, in the colours of `language` when it knows them. `language` none: plain. `marked`: the lines
/// a link named (from 1, the last included), lit, the first of them `here`, which takes the focus and so is scrolled to.
pub fn source_html(text: &str, language: &str, marked: Option<(usize, usize)>) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    out.push_str("<pre class=\"source\">");
    let mut open = false;
    for (at, line) in text.lines().enumerate() {
        let number = at + 1;
        match marked {
            Some((first, _)) if number == first => out.push_str("<span class=\"l m\" id=\"here\" tabindex=\"-1\" autofocus>"),
            Some((first, last)) if number > first && number <= last => out.push_str("<span class=\"l m\">"),
            _ => out.push_str("<span class=\"l\">"),
        }
        open = code_line(line, language, open, &mut out);
        out.push_str("</span>");
    }
    out.push_str("</pre>");
    out
}

/// The lines a link to a file names after its path, as GitHub writes them: `#L24` or `#L24-L30` (`#L24-30` too). The path, and the lines.
pub fn lines_of_link(url: &str) -> (&str, Option<(usize, usize)>) {
    let Some((path, fragment)) = url.rsplit_once("#L") else { return (url, None) };
    let (first, last) = fragment.split_once('-').map_or((fragment, None), |(first, last)| (first, Some(last.trim_start_matches('L'))));
    match (first.parse::<usize>(), last.map(str::parse::<usize>)) {
        (Ok(first), None) if first > 0 => (path, Some((first, first))),
        (Ok(first), Some(Ok(last))) if first > 0 && last >= first => (path, Some((first, last))),
        _ => (url, None),
    }
}

struct Item<'a> {
    ordered: bool,
    /// None, or whether its box is ticked.
    task: Option<bool>,
    text: &'a str,
}

fn indent_of(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

fn item_of(line: &str) -> Option<Item<'_>> {
    let trimmed = line.trim_start();
    let (ordered, text) = if let Some(text) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")).or_else(|| trimmed.strip_prefix("+ ")) {
        (false, text)
    } else {
        let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
        let rest = trimmed.get(digits..)?;
        if digits == 0 || digits > 9 {
            return None;
        }
        (true, rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") "))?)
    };
    let (task, text) = if let Some(text) = text.strip_prefix("[ ] ") {
        (Some(false), text)
    } else if let Some(text) = text.strip_prefix("[x] ").or_else(|| text.strip_prefix("[X] ")) {
        (Some(true), text)
    } else {
        (None, text)
    };
    Some(Item { ordered, task, text })
}

/// Whether a line begins something a paragraph stops for.
fn starts_block(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty() || trimmed.starts_with("```") || trimmed.starts_with("~~~") || heading_level(trimmed).is_some() || is_rule(trimmed) || trimmed.starts_with('>') || item_of(line).is_some() || trimmed.starts_with('|')
}

/// A line with up to `columns` of its leading space taken off (a tab is four).
fn without_columns(line: &str, columns: usize) -> &str {
    let mut taken = 0;
    for (at, byte) in line.bytes().enumerate() {
        if taken >= columns {
            return &line[at..];
        }
        match byte {
            b' ' => taken += 1,
            b'\t' => taken += 4,
            _ => return &line[at..],
        }
    }
    ""
}

/// A list from the line at `at`, with what is nested in it; where it ended. `depth`: how deep this list is.
fn list(lines: &[&str], mut at: usize, out: &mut String, depth: usize) -> usize {
    let indent = indent_of(lines[at]);
    let ordered = item_of(lines[at]).is_some_and(|item| item.ordered);
    out.push_str(if ordered { "<ol>" } else { "<ul>" });
    while at < lines.len() {
        let line = lines[at];
        if line.trim().is_empty() {
            let goes_on = lines.get(at + 1).is_some_and(|next| indent_of(next) > indent || (indent_of(next) == indent && item_of(next).is_some_and(|item| item.ordered == ordered)));
            if goes_on {
                at += 1;
                continue;
            }
            break;
        }
        if indent_of(line) != indent {
            break;
        }
        let Some(item) = item_of(line).filter(|item| item.ordered == ordered) else { break };
        out.push_str("<li>");
        if let Some(ticked) = item.task {
            out.push_str(if ticked { "<input type=\"checkbox\" checked disabled> " } else { "<input type=\"checkbox\" disabled> " });
        }
        out.push_str(&inline(item.text));
        at += 1;
        // What stands further in belongs to this item, blank lines among it too.
        let start = at;
        while at < lines.len() {
            if lines[at].trim().is_empty() {
                if lines.get(at + 1).is_some_and(|next| indent_of(next) > indent) {
                    at += 1;
                    continue;
                }
                break;
            }
            if indent_of(lines[at]) <= indent {
                break;
            }
            at += 1;
        }
        if start < at {
            let least = lines[start..at].iter().filter(|line| !line.trim().is_empty()).map(|line| indent_of(line)).min().unwrap_or(0);
            // (slices of the lines: a list a thousand deep copied all that was inside each of them)
            let inner: Vec<&str> = lines[start..at].iter().map(|line| without_columns(line, least)).collect();
            blocks(&inner, out, depth + 1);
        }
        out.push_str("</li>");
    }
    out.push_str(if ordered { "</ol>" } else { "</ul>" });
    at
}

fn blocks(lines: &[&str], out: &mut String, depth: usize) {
    if depth >= MOST_NESTING {
        plain_rest(lines, out);
        return;
    }
    let mut at = 0;
    while at < lines.len() {
        let line = lines[at];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            at += 1;
        } else if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let fence = &trimmed[..3];
            let language = trimmed[3..].trim().to_lowercase();
            at += 1;
            out.push_str("<pre><code>");
            let mut open = false;
            while at < lines.len() && !lines[at].trim_start().starts_with(fence) {
                open = code_line(lines[at], &language, open, out);
                out.push('\n');
                at += 1;
            }
            out.push_str("</code></pre>");
            at += 1;
        } else if let Some(level) = heading_level(trimmed) {
            out.push_str(&format!("<h{level}>{}</h{level}>", inline(trimmed[level as usize..].trim().trim_end_matches('#').trim())));
            at += 1;
        } else if is_rule(trimmed) {
            out.push_str("<hr>");
            at += 1;
        } else if trimmed.starts_with('>') {
            let mut inner = Vec::new();
            while at < lines.len() {
                let Some(text) = quoted(lines[at].trim_start()) else { break };
                inner.push(text);
                at += 1;
            }
            out.push_str("<blockquote>");
            blocks(&inner, out, depth + 1);
            out.push_str("</blockquote>");
        } else if item_of(line).is_some() {
            at = list(lines, at, out, depth);
        } else if trimmed.starts_with('|') && lines.get(at + 1).and_then(|next| crate::rtf::table_cells(next)).is_some_and(|cells| crate::rtf::is_table_rule(&cells)) {
            let rule = crate::rtf::table_cells(lines[at + 1]).unwrap_or_default();
            let aligned = |column: usize| match rule.get(column).map(String::as_str) {
                Some(cell) if cell.starts_with(':') && cell.ends_with(':') => " style=\"text-align:center\"",
                Some(cell) if cell.ends_with(':') => " style=\"text-align:right\"",
                _ => "",
            };
            out.push_str("<table><thead><tr>");
            for (column, cell) in crate::rtf::table_cells(line).unwrap_or_default().iter().enumerate() {
                out.push_str(&format!("<th{}>{}</th>", aligned(column), escape_text(cell)));
            }
            out.push_str("</tr></thead><tbody>");
            at += 2;
            while at < lines.len() && lines[at].trim_start().starts_with('|') {
                out.push_str("<tr>");
                for (column, cell) in crate::rtf::table_cells(lines[at]).unwrap_or_default().iter().enumerate() {
                    out.push_str(&format!("<td{}>{}</td>", aligned(column), escape_text(cell)));
                }
                out.push_str("</tr>");
                at += 1;
            }
            out.push_str("</tbody></table>");
        } else {
            out.push_str("<p>");
            out.push_str(&inline(trimmed));
            at += 1;
            while at < lines.len() && !starts_block(lines[at]) {
                out.push('\n');
                out.push_str(&inline(lines[at].trim()));
                at += 1;
            }
            out.push_str("</p>");
        }
    }
}

/// A Markdown file as the body of a page.
pub fn to_html(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    blocks(&lines, &mut out, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_is_told_as_its_line_comes() {
        assert_eq!(parse_link("[a](http://b) c"), Link::Whole { text: "a", url: "http://b", length: 13 });
        assert_eq!(parse_link("[a](http://b \"title\")"), Link::Whole { text: "a", url: "http://b", length: 21 });
        for coming in ["[", "[a", "[a]", "[a](", "[a](http://b"] {
            assert_eq!(parse_link(coming), Link::Waiting, "{coming}");
        }
        for not in ["[a] b", "[a\n](b)", "[](b)", "[a]()", "[a\n", "[a](b\n"] {
            assert_eq!(parse_link(not), Link::No, "{not}");
        }
        assert_eq!(parse_link("[a](<C:\\my pics\\x.png>) c"), Link::Whole { text: "a", url: "C:\\my pics\\x.png", length: 23 });
        assert_eq!(parse_link("[a](<x y"), Link::Waiting);
        assert_eq!(parse_link("[a](<x y>"), Link::Waiting);
    }

    #[test]
    fn a_bare_address_ends_where_the_sentence_begins() {
        assert_eq!(bare_url("https://a.example/b c"), BareUrl::Whole(19));
        assert_eq!(bare_url("https://a.example/b. Next"), BareUrl::Whole(19), "the full stop");
        assert_eq!(bare_url("https://a.example/b) x"), BareUrl::Whole(19), "the bracket around it");
        assert_eq!(bare_url("https://a.example/(b) x"), BareUrl::Whole(21), "a bracket of its own");
        assert_eq!(bare_url("https://a.example/bを見て"), BareUrl::Whole(19), "Japanese after it");
        for coming in ["h", "htt", "https:/", "https://a.example"] {
            assert_eq!(bare_url(coming), BareUrl::Waiting, "{coming}");
        }
        for not in ["hello world", "http", "httpd is", "https:// x"] {
            let told = bare_url(not);
            assert!(told == BareUrl::No || (not == "http" && told == BareUrl::Waiting), "{not}: {told:?}");
        }
    }

    #[test]
    fn only_the_web_goes_to_the_browser() {
        assert_eq!(web_address(" https://a.example/b ").as_deref(), Some("https://a.example/b"));
        assert_eq!(web_address("HTTP://a.example").as_deref(), Some("HTTP://a.example"));
        for not in ["javascript:alert(1)", "file:///C:/x", "C:\\x.exe", "index.html", "ms-settings:", "\\\\host\\share"] {
            assert_eq!(web_address(not), None, "{not}");
        }
    }

    #[test]
    fn emphasis_is_told_from_arithmetic() {
        assert!(opens_emphasis(None, 'a'));
        assert!(opens_emphasis(Some(' '), 'a'));
        assert!(opens_emphasis(Some('は'), '強'), "Japanese has no space before it");
        assert!(!opens_emphasis(Some(' '), ' '), "2 * 3");
        assert!(!opens_emphasis(Some('5'), '3'), "5*3");
    }

    #[test]
    fn marks_at_the_start_of_a_line_are_told_apart() {
        assert_eq!(heading_level("# a"), Some(1));
        assert_eq!(heading_level("### a"), Some(3));
        for not in ["#include <a>", "#1 の件", "####### a", "a # b", "#"] {
            assert_eq!(heading_level(not), None, "{not}");
        }
        for rule in ["---", "***", "___", "- - -", "-----------"] {
            assert!(is_rule(rule), "{rule}");
        }
        for not in ["--", "--a", "-*-", "- a", ""] {
            assert!(!is_rule(not), "{not}");
        }
        assert_eq!(quoted("> a"), Some("a"));
        assert_eq!(quoted(">a"), Some("a"));
        assert_eq!(quoted("a"), None);
    }

    #[test]
    fn the_start_of_a_line_waits_only_while_it_could_be_a_mark() {
        for waiting in ["-", "--", "---", "--------", "*", "**", "#", "######", "1", "12.", "`", "``", ">", "- [", "- [ ]", " "] {
            assert!(undecided(waiting), "{waiting}");
        }
        for told in ["a", "1a", "#a", "- a", "1234", "#######", "**a", "- [ ] a"] {
            assert!(!undecided(told), "{told}");
        }
    }

    #[test]
    fn a_line_of_markdown_makes_its_tags_and_no_others() {
        assert_eq!(inline("a **b** *c* ~~d~~ `e*f`"), "a <strong>b</strong> <em>c</em> <del>d</del> <code>e*f</code>");
        assert_eq!(inline("2 * 3 * 4"), "2 * 3 * 4");
        assert_eq!(inline("[a](http://b)"), "<a href=\"http://b\">a</a>");
        assert_eq!(inline("see https://a.example/b."), "see <a href=\"https://a.example/b\">https://a.example/b</a>.");
        assert_eq!(inline("![p](x.png)"), "<img src=\"x.png\" alt=\"p\">");
        assert_eq!(inline("\\*a\\*"), "*a*");
        assert_eq!(inline("**open"), "<strong>open</strong>", "what stays open is closed");
    }

    #[test]
    fn nothing_in_a_file_becomes_a_tag_or_a_script() {
        let page = to_html("<script>alert(1)</script>\n\n[x](javascript:alert(1))\n\n![y](javascript:alert(2))\n\n| <b> |\n|---|\n| <i> |\n\n```\n<script>\n```\n\n<img src=x onerror=alert(3)>");
        assert!(!page.contains("<script>") && !page.contains("<b>") && !page.contains("<i>") && !page.contains("<img src=x") && !page.contains("javascript:"), "{page}");
        assert!(page.contains("&lt;script&gt;"), "{page}");
    }

    #[test]
    fn a_file_comes_out_as_its_blocks() {
        let page = to_html("# 見出し\n\n段落の\n続き\n\n- a\n  - b\n- [x] c\n\n1. d\n\n> e\n\n---\n\n| h | i |\n|:--|--:|\n| 1 | 2 |\n\n```rust\nfn a() {}\n```\n");
        for wanted in ["<h1>見出し</h1>", "<p>段落の\n続き</p>", "<ul><li>a<ul><li>b</li></ul></li><li><input type=\"checkbox\" checked disabled> c</li></ul>", "<ol><li>d</li></ol>", "<blockquote><p>e</p></blockquote>", "<hr>", "<th>h</th><th style=\"text-align:right\">i</th>", "<td style=\"text-align:right\">2</td>", "<span class=\"k\">fn</span>"] {
            assert!(page.contains(wanted), "{wanted} in {page}");
        }
    }

    #[test]
    fn a_list_and_a_quote_end_and_nothing_loops() {
        for text in ["- a\nb", "  - a\n- b", "1. a\n- b", "- a\n\n\nb", ">", "```", "|a|\n|-|", "- a\n    \n", "* * *", "- ", "|"] {
            let _ = to_html(text);
        }
        assert_eq!(to_html("- a\nb"), "<ul><li>a</li></ul><p>b</p>");
        assert_eq!(to_html("1. a\n- b"), "<ol><li>a</li></ol><ul><li>b</li></ul>");
    }

    /// `run` on a thread with a stack of this size, which proves what the test says of it: the stack of the app's own thread is far larger than this, and an answer is no reason to use it.
    fn on_a_small_stack<R: Send + 'static>(kilobytes: usize, run: impl FnOnce() -> R + Send + 'static) -> R {
        std::thread::Builder::new().stack_size(kilobytes * 1024).spawn(run).unwrap().join().expect("it did not overflow its stack")
    }

    /// The real find: a line of a hundred thousand `>` is a hundred thousand levels of quotation, and the process ended with its stack.
    #[test]
    fn a_line_of_a_hundred_thousand_quote_marks_is_a_quotation_so_deep_and_then_its_words() {
        let began = std::time::Instant::now();
        let page = on_a_small_stack(256, || to_html(&">".repeat(100_000)));
        assert_eq!(page.matches("<blockquote>").count(), MOST_NESTING, "as deep as it goes");
        assert_eq!(page.matches("</blockquote>").count(), MOST_NESTING);
        // (the rest is what it is: words, escaped, at the level that was reached)
        assert!(page.contains(&format!("<p>{}</p>", "&gt;".repeat(100_000 - MOST_NESTING))), "the rest is shown as it was written");
        eprintln!("a hundred thousand `>` on 256 KB of stack: {:?}", began.elapsed());
        assert!(began.elapsed() < std::time::Duration::from_secs(2), "{:?}", began.elapsed());
        // Quotation marks with words in them, as an answer has them, nest as far as they go and no further.
        let few = to_html("> > a\n> b");
        assert_eq!(few, "<blockquote><blockquote><p>a</p></blockquote><p>b</p></blockquote>");
    }

    #[test]
    fn lists_nested_hundreds_deep_end_at_the_same_cap() {
        let began = std::time::Instant::now();
        let text: String = (0..400).map(|level| format!("{}- item {level}\n", " ".repeat(level))).collect();
        let page = on_a_small_stack(256, move || to_html(&text));
        assert_eq!(page.matches("<ul>").count(), MOST_NESTING, "{}", &page[..200.min(page.len())]);
        assert!(page.contains("item 31") && page.contains("- item 40"), "what is deeper is its words, with its marks");
        eprintln!("lists four hundred deep on 256 KB of stack: {:?}", began.elapsed());
        assert!(began.elapsed() < std::time::Duration::from_secs(2), "{:?}", began.elapsed());
        // The same through quotations that hold lists that hold quotations.
        let mixed: String = (0..2000).map(|level| format!("{}> - x\n", "  ".repeat(level.min(40)))).collect();
        let _ = on_a_small_stack(256, move || to_html(&mixed));
        let ordinary = to_html("- a\n  - b\n    - c\n- d");
        assert_eq!(ordinary, "<ul><li>a<ul><li>b<ul><li>c</li></ul></li></ul></li><li>d</li></ul>");
    }

    /// What a line of nothing but opening brackets, closing ones or the start of a link costs: each was scanned to the end of the line from its own place.
    #[test]
    fn a_long_line_of_marks_is_read_once_and_not_once_for_each() {
        let megabyte = 1 << 20;
        let shapes: [(&str, String); 6] = [
            ("[", "[".repeat(megabyte)),
            (")", ")".repeat(megabyte)),
            ("[a](", "[a](".repeat(megabyte / 4)),
            ("[a](<", "[a](<".repeat(megabyte / 5)),
            ("an address and its closing brackets", format!("https://a.example/{}  and on", ")".repeat(megabyte))),
            ("![", "![".repeat(megabyte / 2)),
        ];
        for (name, line) in shapes {
            let began = std::time::Instant::now();
            let said = inline(&line);
            assert!(said.len() >= line.len() / 2, "{name}");
            eprintln!("inline of a megabyte of {name}: {:?}", began.elapsed());
            // (generous: this is a build with no optimization of its own code, and what is measured is that it is not a minute)
            assert!(began.elapsed() < std::time::Duration::from_secs(3), "{name}: {:?}", began.elapsed());
        }
        // Well-formed links are as many as they are, and each is a link.
        let links = "[a](http://b) ".repeat(50_000);
        let began = std::time::Instant::now();
        assert_eq!(inline(&links).matches("<a href").count(), 50_000);
        eprintln!("inline of fifty thousand links: {:?}", began.elapsed());
        assert!(began.elapsed() < std::time::Duration::from_secs(3), "{:?}", began.elapsed());
    }

    #[test]
    fn a_link_longer_than_the_limit_is_not_one_and_one_inside_it_is() {
        let long = format!("[a](http://b/{})", "x".repeat(MOST));
        assert_eq!(parse_link(&long), Link::No);
        let whole = format!("[a](http://b/{})", "x".repeat(MOST - 40));
        assert!(matches!(parse_link(&whole), Link::Whole { .. }));
        assert_eq!(parse_link(&format!("[{}", "a".repeat(MOST + 10))), Link::No);
        assert_eq!(parse_link("[あ"), Link::Waiting);
        // (a limit that falls in the middle of a character does not cut it)
        let _ = parse_link(&format!("[{}", "あ".repeat(MOST)));
    }

    #[test]
    fn the_closing_brackets_after_an_address_are_trimmed_once_and_not_counted_again_for_each() {
        assert_eq!(bare_url("https://a.example/(b) x"), BareUrl::Whole(21));
        assert_eq!(bare_url("https://a.example/b)) x"), BareUrl::Whole(19), "two brackets nothing opened");
        assert_eq!(bare_url("https://a.example/(b)) x"), BareUrl::Whole(21), "one that was opened, and one that was not");
        assert_eq!(bare_url("https://a.example/[b]] x"), BareUrl::Whole(21));
        let began = std::time::Instant::now();
        let line = format!("https://a.example/{} x", ")".repeat(1 << 20));
        assert_eq!(bare_url(&line), BareUrl::Whole("https://a.example/".len()));
        eprintln!("bare_url of an address and a million closing brackets: {:?}", began.elapsed());
        assert!(began.elapsed() < std::time::Duration::from_secs(2), "{:?}", began.elapsed());
    }

    /// Every character that acts on text and shows nothing, each by its name.
    fn hidden_characters() -> Vec<char> {
        ('\u{200B}'..='\u{200F}').chain(['\u{2028}', '\u{2029}']).chain('\u{202A}'..='\u{202E}').chain('\u{2066}'..='\u{2069}').chain(['\u{FEFF}']).collect()
    }

    #[test]
    fn what_acts_on_text_and_shows_nothing_is_shown_as_its_mark_in_every_page() {
        for c in hidden_characters() {
            let word = format!("a{c}b");
            let mark = format!("<U+{:04X}>", c as u32);
            assert_eq!(show_hidden(&word), format!("a{mark}b"), "{mark}");
            assert_eq!(escape(&word), format!("a&lt;U+{:04X}&gt;b", c as u32));
            assert_eq!(inline(&word), format!("a<span class=\"u\">&lt;U+{:04X}&gt;</span>b", c as u32));
            for page in [to_html(&word), to_html(&format!("```rust\n{word}\n```")), to_html(&format!("| {word} |\n|---|\n| {word} |")), source_html(&word, "", None), source_html(&word, "rs", None)] {
                assert!(!page.contains(c) && page.contains(&format!("U+{:04X}", c as u32)), "{mark} in {page}");
            }
        }
        // The right-to-left override in a link's words, an address and an attribute: none of it reaches the page.
        let page = to_html("[a\u{202e}b](http://x/\u{202e}y) and \u{202e}http://z.example/\u{202e}");
        assert!(!page.contains('\u{202e}'), "{page}");
    }

    #[test]
    fn japanese_emoji_and_combining_marks_are_left_as_they_are() {
        // a skin tone is a modifier and no joiner; a joiner between two emoji makes the picture of them; a combining mark is part of the letter before it
        for text in ["日本語のテキスト、「括弧」と全角！", "👍🏽 👋🏿", "👩\u{200d}💻 and 👨\u{200d}👩\u{200d}👧", "e\u{0301} が\u{3099} ❤\u{fe0f}", "ASCII only."] {
            assert_eq!(show_hidden(text), text, "{text}");
            assert_eq!(escape_text(text), escape(text));
            assert!(!to_html(text).contains("U+"), "{text}");
        }
        // A joiner with nothing known beside it is left alone, and one between letters is not a joiner.
        assert_eq!(show_hidden("\u{200d}"), "\u{200d}");
        assert_eq!(show_hidden("a\u{200d}b"), "a<U+200D>b");
        assert_eq!(show_hidden("x👩\u{200d}"), "x👩\u{200d}");
    }

    #[test]
    fn source_keeps_its_lines_and_its_colours() {
        let page = source_html("fn a() {\n  // <b>\n}\n", "rs", None);
        assert_eq!(page.matches("class=\"l\"").count(), 3);
        assert!(page.contains("<span class=\"k\">fn</span>") && page.contains("&lt;b&gt;"), "{page}");
        let plain = source_html("a<b", "", None);
        assert!(plain.contains("a&lt;b"), "{plain}");
        let marked = source_html("a\nb\nc\nd\n", "", Some((2, 3)));
        assert_eq!((marked.matches("class=\"l m\"").count(), marked.matches("id=\"here\"").count()), (2, 1), "{marked}");
        assert!(marked.find("id=\"here\"").unwrap() < marked.find(">b<").unwrap_or(marked.find("b</span>").unwrap()), "{marked}");
    }

    #[test]
    fn a_link_names_lines_as_github_does() {
        assert_eq!(lines_of_link("src/app.js#L24"), ("src/app.js", Some((24, 24))));
        assert_eq!(lines_of_link("src/app.js#L24-L30"), ("src/app.js", Some((24, 30))));
        assert_eq!(lines_of_link("src/app.js#L24-30"), ("src/app.js", Some((24, 30))));
        assert_eq!(lines_of_link("src/app.js"), ("src/app.js", None));
        assert_eq!(lines_of_link("page.html#Lunch"), ("page.html#Lunch", None), "an anchor of its own stays");
        assert_eq!(lines_of_link("a.rs#L30-L24"), ("a.rs#L30-L24", None));
    }
}
