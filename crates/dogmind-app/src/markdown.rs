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

/// `rest` begins at the `[`.
pub fn parse_link(rest: &str) -> Link<'_> {
    const MOST: usize = 2000;
    let Some(close) = rest.find(']') else {
        return if rest.contains('\n') || rest.len() > MOST { Link::No } else { Link::Waiting };
    };
    if rest[..close].contains('\n') || close == 1 {
        return Link::No;
    }
    let after = &rest[close + 1..];
    if after.is_empty() {
        return Link::Waiting;
    }
    if !after.starts_with('(') {
        return Link::No;
    }
    // `(<a path with spaces>)`, as CommonMark writes an address that has them
    if let Some(inside) = after[1..].strip_prefix('<') {
        let Some(close_angle) = inside.find('>') else {
            return if inside.contains('\n') || rest.len() > MOST { Link::No } else { Link::Waiting };
        };
        let url = &inside[..close_angle];
        if url.is_empty() || url.contains('\n') {
            return Link::No;
        }
        let tail = &inside[close_angle + 1..];
        let Some(end) = tail.find(')') else {
            return if tail.contains('\n') || rest.len() > MOST { Link::No } else { Link::Waiting };
        };
        return Link::Whole { text: &rest[1..close], url, length: close + 1 + 2 + close_angle + 1 + end + 1 };
    }
    let Some(end) = after.find(')') else {
        return if after.contains('\n') || rest.len() > MOST { Link::No } else { Link::Waiting };
    };
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
        return if rest.len() > 2000 { BareUrl::Whole(rest.len()) } else { BareUrl::Waiting };
    };
    let mut url = &rest[..end];
    // What ends the sentence or the bracket around it is not in the address, unless the address opened that bracket itself.
    loop {
        let last = url.chars().last().unwrap_or(' ');
        let unbalanced = |open: char, close: char| last == close && url.matches(close).count() > url.matches(open).count();
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*' | '_') || unbalanced('(', ')') || unbalanced('[', ']') {
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
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
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
        out.push_str(&escape(line));
    }
    out.push_str("</p>");
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
        out.push_str(&escape(&c.to_string()));
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
        out.push_str(&escape(line));
        return false;
    };
    for (text, kind) in runs {
        match class_of(kind) {
            Some(class) => out.push_str(&format!("<span class=\"{class}\">{}</span>", escape(&text))),
            None => out.push_str(&escape(&text)),
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
                out.push_str(&format!("<th{}>{}</th>", aligned(column), escape(cell)));
            }
            out.push_str("</tr></thead><tbody>");
            at += 2;
            while at < lines.len() && lines[at].trim_start().starts_with('|') {
                out.push_str("<tr>");
                for (column, cell) in crate::rtf::table_cells(lines[at]).unwrap_or_default().iter().enumerate() {
                    out.push_str(&format!("<td{}>{}</td>", aligned(column), escape(cell)));
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
        assert!(began.elapsed() < std::time::Duration::from_secs(2), "{:?}", began.elapsed());
        // The same through quotations that hold lists that hold quotations.
        let mixed: String = (0..2000).map(|level| format!("{}> - x\n", "  ".repeat(level.min(40)))).collect();
        let _ = on_a_small_stack(256, move || to_html(&mixed));
        let ordinary = to_html("- a\n  - b\n    - c\n- d");
        assert_eq!(ordinary, "<ul><li>a<ul><li>b<ul><li>c</li></ul></li></ul></li><li>d</li></ul>");
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
