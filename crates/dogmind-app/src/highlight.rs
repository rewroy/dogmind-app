//! Colour for a line of code: keywords, strings, comments and numbers, by simple rules and with no knowledge of the program. It is
//! there to help the eye, so what it cannot tell stays plain, and a language it does not know has no colour at all.

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Plain,
    Keyword,
    Text,
    Comment,
    Number,
}

/// How a language writes what can be told apart.
struct Rules {
    keywords: &'static [&'static str],
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    /// Keywords are matched whatever their case.
    any_case: bool,
}

const C_LIKE: &[&str] = &[
    "abstract", "async", "await", "bool", "break", "case", "catch", "char", "class", "const", "constexpr", "continue", "default", "defer", "delete", "do", "double", "else", "enum", "export", "extends", "extern", "false", "final", "finally", "float", "fn", "for", "from", "func", "function", "go", "if", "impl", "implements", "import", "in", "include", "instanceof", "int", "interface", "let", "long", "map", "match", "mod", "move", "mut", "namespace", "new", "nil", "null", "of", "override", "package", "private", "protected", "pub", "public", "return", "self", "short", "signed", "sizeof", "static", "struct", "super", "switch", "template", "this", "throw", "trait", "true", "try", "type", "typedef", "typeof", "undefined", "union", "unsafe", "unsigned", "use", "using", "var", "virtual", "void", "where", "while", "yield",
];
const PYTHON: &[&str] = &["False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "self", "try", "while", "with", "yield"];
const SHELL: &[&str] = &["case", "do", "done", "elif", "else", "esac", "exit", "export", "fi", "for", "function", "if", "in", "local", "return", "then", "until", "while", "foreach", "param", "try", "catch", "throw"];
const SQL: &[&str] = &["add", "alter", "and", "as", "asc", "by", "create", "delete", "desc", "distinct", "drop", "from", "group", "having", "index", "insert", "into", "is", "join", "key", "left", "limit", "not", "null", "on", "or", "order", "primary", "right", "select", "set", "table", "union", "update", "values", "where"];

fn rules_of(language: &str) -> Option<Rules> {
    let c_like = || Rules { keywords: C_LIKE, line_comments: &["//"], block_comment: Some(("/*", "*/")), any_case: false };
    let plain = |comments: &'static [&'static str]| Rules { keywords: &[], line_comments: comments, block_comment: None, any_case: false };
    Some(match language {
        "rust" | "rs" | "c" | "h" | "cpp" | "c++" | "hpp" | "cc" | "cs" | "csharp" | "java" | "go" | "golang" | "kotlin" | "kt" | "swift" | "js" | "javascript" | "jsx" | "ts" | "typescript" | "tsx" | "scala" | "dart" | "php" => c_like(),
        "html" | "htm" | "xml" | "svg" => Rules { keywords: &[], line_comments: &[], block_comment: Some(("<!--", "-->")), any_case: false },
        "css" | "scss" => Rules { keywords: &[], line_comments: &[], block_comment: Some(("/*", "*/")), any_case: false },
        "python" | "py" | "python3" | "ruby" | "rb" => Rules { keywords: PYTHON, line_comments: &["#"], block_comment: None, any_case: false },
        "bash" | "sh" | "shell" | "zsh" | "powershell" | "ps1" | "pwsh" => Rules { keywords: SHELL, line_comments: &["#"], block_comment: None, any_case: true },
        "sql" => Rules { keywords: SQL, line_comments: &["--"], block_comment: Some(("/*", "*/")), any_case: true },
        "json" | "jsonc" => plain(&["//"]),
        "toml" | "yaml" | "yml" | "ini" | "conf" | "dockerfile" | "makefile" | "make" => plain(&["#"]),
        _ => return None,
    })
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The runs of a line, in order, covering all of it, and whether a block comment is still open at its end. `open`: one was at its start.
/// None for a language this does not know.
pub fn runs(line: &str, language: &str, mut open: bool) -> Option<(Vec<(String, Kind)>, bool)> {
    let rules = rules_of(language)?;
    let mut out: Vec<(String, Kind)> = Vec::new();
    let push = |out: &mut Vec<(String, Kind)>, text: &str, kind: Kind| {
        if text.is_empty() {
            return;
        }
        match out.last_mut() {
            Some((held, same)) if *same == kind => held.push_str(text),
            _ => out.push((text.to_string(), kind)),
        }
    };
    let mut rest = line;
    while !rest.is_empty() {
        if open {
            let (_, close) = rules.block_comment.unwrap_or(("", ""));
            match rest.find(close).filter(|_| !close.is_empty()) {
                Some(at) => {
                    push(&mut out, &rest[..at + close.len()], Kind::Comment);
                    rest = &rest[at + close.len()..];
                    open = false;
                    continue;
                }
                None => {
                    push(&mut out, rest, Kind::Comment);
                    return Some((out, true));
                }
            }
        }
        if rules.line_comments.iter().any(|mark| rest.starts_with(mark)) {
            push(&mut out, rest, Kind::Comment);
            break;
        }
        if let Some((start, _)) = rules.block_comment.filter(|(start, _)| rest.starts_with(start)) {
            push(&mut out, start, Kind::Comment);
            rest = &rest[start.len()..];
            open = true;
            continue;
        }
        let Some(c) = rest.chars().next() else { break };
        if matches!(c, '"' | '\'' | '`') {
            // To its closing mark on this line, past escapes. A lone ' as in Rust's lifetimes or an English word is no string.
            let mut end = None;
            let mut escaped = false;
            for (at, inner) in rest.char_indices().skip(1) {
                if escaped {
                    escaped = false;
                } else if inner == '\\' {
                    escaped = true;
                } else if inner == c {
                    end = Some(at + inner.len_utf8());
                    break;
                }
            }
            match end.filter(|at| c != '\'' || *at <= 4 || !language_has_lifetimes(language)) {
                Some(at) => {
                    push(&mut out, &rest[..at], Kind::Text);
                    rest = &rest[at..];
                }
                None if c == '\'' => {
                    push(&mut out, &rest[..1], Kind::Plain);
                    rest = &rest[1..];
                }
                None => {
                    push(&mut out, rest, Kind::Text);
                    break;
                }
            }
            continue;
        }
        if is_word(c) {
            let at = rest.find(|inner: char| !is_word(inner)).unwrap_or(rest.len());
            let word = &rest[..at];
            let kind = if word.chars().next().is_some_and(|first| first.is_ascii_digit()) {
                Kind::Number
            } else if rules.keywords.iter().any(|keyword| if rules.any_case { keyword.eq_ignore_ascii_case(word) } else { *keyword == word }) {
                Kind::Keyword
            } else {
                Kind::Plain
            };
            push(&mut out, word, kind);
            rest = &rest[at..];
            continue;
        }
        push(&mut out, &rest[..c.len_utf8()], Kind::Plain);
        rest = &rest[c.len_utf8()..];
    }
    Some((out, open))
}

fn language_has_lifetimes(language: &str) -> bool {
    matches!(language, "rust" | "rs")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &str, language: &str) -> Vec<(String, Kind)> {
        runs(line, language, false).unwrap().0
    }

    fn joined(line: &str, language: &str) -> String {
        kinds(line, language).into_iter().map(|(text, _)| text).collect()
    }

    #[test]
    fn a_language_it_does_not_know_has_no_colour() {
        assert!(runs("anything", "brainfuck", false).is_none());
        assert!(runs("anything", "", false).is_none());
    }

    #[test]
    fn nothing_of_a_line_is_lost() {
        for (line, language) in [("fn main() { let x = \"日本語\"; } // 終わり", "rust"), ("def f(a): return 'x' # 注", "python"), ("a /* b */ c", "c"), ("'unclosed", "python"), ("\"unclosed", "js"), ("", "go")] {
            assert_eq!(joined(line, language), line);
        }
    }

    #[test]
    fn keywords_strings_comments_and_numbers_are_told_apart() {
        let line = kinds("let total = 12; // sum", "rust");
        assert_eq!(line[0], ("let".to_string(), Kind::Keyword));
        assert!(line.contains(&("12".to_string(), Kind::Number)));
        assert_eq!(line.last().unwrap(), &("// sum".to_string(), Kind::Comment));
        let text = kinds("print(\"a # b\") # c", "python");
        assert!(text.contains(&("\"a # b\"".to_string(), Kind::Text)), "a mark inside a string is the string's: {text:?}");
        assert_eq!(text.last().unwrap(), &("# c".to_string(), Kind::Comment));
        assert_eq!(kinds("letter", "rust"), vec![("letter".to_string(), Kind::Plain)], "a keyword is a whole word");
        assert_eq!(kinds("SELECT a", "sql")[0], ("SELECT".to_string(), Kind::Keyword));
    }

    #[test]
    fn a_block_comment_runs_on_to_the_next_lines() {
        let (first, open) = runs("x /* a", "c", false).unwrap();
        assert!(open && first.last().unwrap().1 == Kind::Comment);
        let (second, open) = runs("b */ y", "c", true).unwrap();
        assert!(!open);
        assert_eq!(second[0], ("b */".to_string(), Kind::Comment));
    }

    #[test]
    fn a_rust_lifetime_is_no_string() {
        let line = kinds("fn f<'a>(x: &'a str) -> &'a str", "rust");
        assert!(line.iter().all(|(_, kind)| *kind != Kind::Text), "{line:?}");
        assert_eq!(kinds("let c = 'x';", "rust").iter().filter(|(_, kind)| *kind == Kind::Text).count(), 1);
    }
}
