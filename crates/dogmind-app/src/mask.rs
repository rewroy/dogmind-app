//! Keys and tokens in what a tool brought back, and in the model's own reply where it says them again, hidden where the window shows it: a command that prints the environment, a config file
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

/// The model's reply as it comes in pieces, with the keys in it hidden as `hide_keys` hides them in a whole text: a key that is cut between two pieces is no less a key. What stands at the
/// end of what has come, if it begins like a key (or is one so far), is held back until what follows says whether it is; `finish` lets go of what is held when the reply ends. What is shown is
/// all this changes: the reply as the model wrote it is kept as it is, in the session and for the next request.
#[derive(Default)]
pub struct StreamMask {
    held: String,
    /// What was shown last ended in the middle of a word: what comes next goes on with it, and a key is not told by what begins here.
    after_word: bool,
}

/// The longest a word that may be a key is waited for: no key is longer, and a word as long as this is not one.
const LONGEST_WAIT: usize = 400;

impl StreamMask {
    /// `piece` of the reply, in; what may be shown now, out.
    pub fn push(&mut self, piece: &str) -> String {
        self.held.push_str(piece);
        let cut = self.held.len() - waiting_tail(&self.held);
        let ready: String = self.held.drain(..cut).collect();
        self.hidden(&ready)
    }

    /// The reply has ended: whatever was held, hidden as it should be.
    pub fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.held);
        self.hidden(&rest)
    }

    /// `text`, the next of the reply, with its keys hidden, in the word the last of it ended in if it did (a digit stands in front of it for that: no prefix begins with one).
    fn hidden(&mut self, text: &str) -> String {
        let Some(last) = text.chars().next_back() else { return String::new() };
        let shown = if self.after_word { hide_keys(&format!("0{text}"))[1..].to_string() } else { hide_keys(text).into_owned() };
        self.after_word = key_char(last);
        shown
    }
}

/// How many bytes at the end of `text` are a word that may still turn out to be a key: it begins as a key's prefix does, or is the beginning of one.
fn waiting_tail(text: &str) -> usize {
    let word = text.len() - text.trim_end_matches(key_char).len();
    let tail = &text[text.len() - word..];
    if tail.is_empty() || word > LONGEST_WAIT {
        return 0;
    }
    // (a word starts where the one before it stops: what is inside a longer word is no key's beginning)
    let starts_word = text.len() == word || !text[..text.len() - word].chars().next_back().is_some_and(key_char);
    let begins_like_a_key = KINDS.iter().map(|(prefix, ..)| *prefix).chain(std::iter::once("AKIA")).any(|prefix| tail.starts_with(prefix) || prefix.starts_with(tail));
    if starts_word && begins_like_a_key { word } else { 0 }
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

    const KEY: &str = "sk-proj-abcdEFGH1234ijklMNOP5678";

    /// The reply said in pieces of any size, cut anywhere, shows what the whole of it would, and no piece of a key is shown before it is known to be one.
    #[test]
    fn a_reply_in_pieces_hides_a_key_wherever_it_is_cut() {
        let whole = format!("The file sets OPENAI_API_KEY={KEY} and DEBUG=1. The token {KEY}, ghp_ABCDEFGHIJKLMNOPQRSTuvwxyz12 and AKIAIOSFODNN7EXAMPLE are keys; sk-learn is not.");
        let wanted = hide_keys(&whole).into_owned();
        assert!(wanted.contains("sk-proj-abcd…(hidden)") && !wanted.contains("EFGH1234"), "{wanted}");
        let characters: Vec<usize> = whole.char_indices().map(|(at, _)| at).chain([whole.len()]).collect();
        // (cut in two at every place)
        for &at in &characters {
            let mut mask = StreamMask::default();
            let mut shown = mask.push(&whole[..at]);
            shown.push_str(&mask.push(&whole[at..]));
            shown.push_str(&mask.finish());
            assert_eq!(shown, wanted, "cut at {at}");
        }
        // (cut in three at every pair of places a few apart, and one character at a time)
        for &first in characters.iter().step_by(7) {
            for &second in characters.iter().filter(|second| **second >= first).step_by(5) {
                let mut mask = StreamMask::default();
                let mut shown = String::new();
                for piece in [&whole[..first], &whole[first..second], &whole[second..]] {
                    shown.push_str(&mask.push(piece));
                }
                shown.push_str(&mask.finish());
                assert_eq!(shown, wanted, "cut at {first} and {second}");
            }
        }
        let mut mask = StreamMask::default();
        let mut shown = String::new();
        for (at, c) in whole.char_indices() {
            shown.push_str(&mask.push(&whole[at..at + c.len_utf8()]));
        }
        shown.push_str(&mask.finish());
        assert_eq!(shown, wanted);
    }

    /// A word that is no key because of what stands before its prefix (`task-...`) is none when it comes in pieces cut between them.
    #[test]
    fn a_word_that_only_ends_like_a_key_is_none_in_pieces_either() {
        for word in ["task-0123456789abcdefghijklmn", "pre_ghp_ABCDEFGHIJKLMNOPQRSTuvwxyz12", "text and xxoxb-1234567890abc"] {
            for at in 0..=word.len() {
                let mut mask = StreamMask::default();
                let mut shown = mask.push(&word[..at]);
                shown.push_str(&mask.push(&word[at..]));
                shown.push_str(&mask.finish());
                assert_eq!(shown, word, "{word} cut at {at}");
            }
        }
    }

    /// What comes before a word that may be a key is shown at once, a word that is not one is not waited for, and the one that may be is held till the next piece says.
    #[test]
    fn only_a_word_that_may_be_a_key_is_waited_for() {
        let mut mask = StreamMask::default();
        assert_eq!(mask.push("the key is sk-pro"), "the key is ");
        assert_eq!(mask.push("j-abcdEFGH1234ijklMNOP5678 and"), "sk-proj-abcd…(hidden) and");
        // (a word that is nothing like a key goes out as it comes, even cut in the middle)
        assert_eq!(mask.push(" hello wor"), " hello wor");
        assert_eq!(mask.push("ld"), "ld");
        // (so does one that began like a key and turned out not to be)
        assert_eq!(mask.push(" sk"), " ");
        assert_eq!(mask.push("y blue"), "sky blue");
        // (and at the end of the reply what is held is let go)
        assert_eq!(mask.push(" ghp_"), " ");
        assert_eq!(mask.finish(), "ghp_");
        assert_eq!(mask.finish(), "");
        // (a very long word that begins like a key is no longer waited for)
        let long = format!("sk-{}", "a".repeat(500));
        assert_eq!(StreamMask::default().push(&long), long);
    }

    /// Words that only look like a prefix are left: no digit after `sk-`, a key too short, a prefix inside a word.
    #[test]
    fn what_is_no_key_is_left_as_it_is() {
        for text in ["pip install sk-learn-extra-tools-for-everything", "ask-me sk-short1", "task-0123456789abcdefghijklmn", "AKIAIOSFODNN7EXAMPLEX", "plain text"] {
            assert!(matches!(hide_keys(text), Cow::Borrowed(_)), "{text}");
        }
    }
}
