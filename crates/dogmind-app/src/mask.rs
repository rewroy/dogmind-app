//! Keys and tokens in what a tool brought back, hidden where the window shows it: a command that prints the environment, a config file
//! read, a log, would otherwise put an API key on the screen (and in a screenshot of it). What the model is given is not changed here: it
//! may need the value, and the agent keeps reading secret files behind the person's approval. A key is told by its maker's prefix and its
//! length; its prefix and first four characters stay, so that it can still be told which key it is.

use std::borrow::Cow;

/// A maker's prefix, how many characters must follow it, and whether those must hold a digit (a word with a prefix like `sk-` is no key).
const KINDS: [(&str, usize, bool); 14] = [
    ("sk-ant-", 20, false),
    ("sk-proj-", 20, false),
    ("sk-", 20, true),
    ("github_pat_", 20, false),
    ("ghp_", 20, false),
    ("gho_", 20, false),
    ("ghu_", 20, false),
    ("ghs_", 20, false),
    ("ghr_", 20, false),
    ("glpat-", 20, false),
    ("xoxb-", 10, false),
    ("xoxp-", 10, false),
    ("hf_", 30, false),
    ("AIza", 35, false),
];

fn key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// `text` with every key in it hidden but its prefix and first four characters; borrowed when there is none.
pub fn hide_keys(text: &str) -> Cow<'_, str> {
    let mut out = String::new();
    let mut copied = 0;
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        // (a key starts a word: `task-…` or `disk-…` holds no `sk-` key)
        let starts_word = at == 0 || !text[..at].chars().next_back().is_some_and(key_char);
        let found = starts_word.then(|| {
            KINDS.iter().find_map(|(prefix, least, digit)| {
                let body = rest.strip_prefix(prefix)?;
                let length = body.find(|c: char| !key_char(c)).unwrap_or(body.len());
                let key = &body[..length];
                (length >= *least && (!digit || key.chars().any(|c| c.is_ascii_digit()))).then_some((prefix.len(), length))
            })
        }).flatten().or_else(|| aws_key(rest).filter(|_| starts_word).map(|length| (4, length - 4)));
        match found {
            Some((prefix, length)) => {
                let shown = prefix + 4;
                out.push_str(&text[copied..at + shown]);
                out.push_str("…(hidden)");
                at += prefix + length;
                copied = at;
            }
            None => at += rest.chars().next().map_or(1, char::len_utf8),
        }
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

/// An AWS access key id at the start of `text`: AKIA and sixteen capitals or digits, the word ending there. Its whole length.
fn aws_key(text: &str) -> Option<usize> {
    let body = text.strip_prefix("AKIA")?.as_bytes();
    let length = body.iter().take_while(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit()).count();
    (length == 16 && !body.get(16).is_some_and(|byte| byte.is_ascii_alphanumeric())).then_some(20)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_hidden_but_their_beginning() {
        let shown = hide_keys("OPENAI_API_KEY=sk-proj-abcdEFGH1234ijklMNOP5678 and token ghp_ABCDEFGHIJKLMNOPQRSTuvwxyz12 done");
        assert_eq!(shown, "OPENAI_API_KEY=sk-proj-abcd…(hidden) and token ghp_ABCD…(hidden) done");
        assert_eq!(hide_keys("aws AKIAIOSFODNN7EXAMPLE x"), "aws AKIAIOSF…(hidden) x");
        assert_eq!(hide_keys("key: sk-ant-api03-0123456789abcdefghijklmnop"), "key: sk-ant-api0…(hidden)");
    }

    /// Words that only look like a prefix are left: no digit after `sk-`, a key too short, a prefix inside a word.
    #[test]
    fn what_is_no_key_is_left_as_it_is() {
        for text in ["pip install sk-learn-extra-tools-for-everything", "ask-me sk-short1", "task-0123456789abcdefghijklmn", "AKIAIOSFODNN7EXAMPLEX", "plain text"] {
            assert!(matches!(hide_keys(text), Cow::Borrowed(_)), "{text}");
        }
    }
}
